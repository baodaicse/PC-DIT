//! GPU-H first async adapter: bounded pinned slots, direct CPU comparison.
//! A slot is reused only after its verifier returns. CUDA stays on the owner thread.
use crate::checkpoints::{
    self, BoundaryCounts, BoundaryFault, BoundaryReport, Checkpoints, GpuCheck, Kind,
};
use crate::{
    CudaBackend, Error, Result,
    driver::{Handle, check},
    lane::{self, Action, Lane, LaneFailure, LaneFault, LaneReport},
    memory::{Buffer, Context, LIVE_EVENTS, LIVE_STREAMS, Pinned},
    sweep::{SweepConfig, SweepReport, Sweeper},
    workers::{self, Fairness, WorkQueue},
};
use gpu_engine::{
    accounting::{
        BurstSummary, Cumulative, FailureHash, MAX_MISMATCH_SAMPLES, MismatchSample, Rates, Regime,
        RegimeInput, Timing, classify_regime, interval_union,
    },
    balanced::{Batch, Ledger, Pending, Stage},
    control::{ControlConfig, ControlState, LoadController, Sample, waiting_fill},
    gemm::{self, Arith, ArithMix, Corpus, MismatchSummary, derive_corpus_seed},
    lane::LaneConfig,
    pressure::{PressureConfig, PressureSampler},
    schedule::{ScalerObservation, WorkerScaler},
    shape::{Shape, ShapeDecision, ShapeLadder, ShapeStep, ShapeWindow},
    verifier::{HashPolicy, default_helper_threads},
};
use gpu_vulkan::{
    AsyncBurstAccounting, ComputeEvidence, ComputeProfile, DeviceIntervals, ReplayBatch,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::c_void,
    ptr,
    rc::Rc,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Serialize)]
pub struct AsyncConfig {
    /// Batches allowed on the GPU at once. A device slot is released as soon as its D2H copy
    /// lands, so this is independent of how many outputs may wait for a verifier.
    pub device_slots: usize,
    /// Pinned readback slots: the host queue. Must exceed `device_slots`.
    pub host_slots: usize,
    /// Verifier workers at the start; the scaler may add workers up to `workers_max`.
    pub workers: usize,
    /// CPU-budget ceiling for verifier workers (equal to `workers` disables scaling).
    pub workers_max: usize,
    /// Largest burst; the scheduler may ask for shorter ones.
    pub batches: u64,
    /// Pressure when `MemAvailable` falls below this (0 disables the check).
    pub min_available_bytes: u64,
    /// Failed batches whose host slot is held, at most, so evidence survives for confirmation.
    pub quarantine_slots: usize,
    /// A hold never outlives this age.
    pub quarantine_max_age_ms: u64,
    /// When to SHA-256 a passing output. Failing outputs are always hashed.
    pub hash_policy: HashPolicy,
    /// Rotate to a freshly derived corpus after this many batches (0 = never).
    /// Rotation happens only at burst boundaries; the next corpus is built in the
    /// background while the current one is in use.
    pub rotate_every_batches: u64,
    /// Helper threads for corpus/oracle builds (0 = automatic, at most a quarter of the cores).
    pub corpus_threads: usize,
    /// Corpora kept besides the one in use (v1.2 C13 G2). A rotation that finds the next corpus
    /// not built yet takes one of these (or keeps the one in use) instead of waiting for it; each
    /// is still verified word for word against its own oracle. 0: wait, as before.
    pub corpus_pool: usize,
    /// Re-hash the host copy of the corpus every this many batches, and always after a
    /// failed batch, to tell host-memory damage from a device fault (0 = failures only).
    pub integrity_every_batches: u64,
    /// GPU-H6: overlapped transfer/retention lane and GPU-local VRAM workload.
    pub lane: LaneConfig,
    /// GPU-DRAM sweep at device bandwidth; needs the overlapped lane, which it trades halves of
    /// the pool with.
    pub sweep: SweepConfig,
    /// Integer GEMM, FP32 FMA GEMM, or both in turn (switching at each corpus rotation).
    pub arith: ArithMix,
    /// GPU boundary checks (fault attribution step F3): the GPU hashes every corpus upload and
    /// checks its own output before the copy back, so a failed batch is placed on the link, the
    /// GPU, its VRAM or the host. Off: nothing of it runs and the report is unchanged.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub boundary_checks: bool,
    /// Adaptive shape ([`gpu_engine::shape`]): while the CPU verifiers set the pace, each next
    /// corpus may double K (halving the jobs) up to this. 0 or the starting K: K stays.
    pub inner_max: u32,
}
impl Default for AsyncConfig {
    fn default() -> Self {
        Self {
            device_slots: 3,
            host_slots: 8,
            workers: 2,
            workers_max: 4,
            batches: 32,
            min_available_bytes: 256 * 1024 * 1024,
            quarantine_slots: 1,
            quarantine_max_age_ms: 30_000,
            hash_policy: HashPolicy::Checkpoint {
                interval: HashPolicy::DEFAULT_INTERVAL,
            },
            rotate_every_batches: 256,
            corpus_threads: 0,
            corpus_pool: 3,
            integrity_every_batches: 512,
            lane: LaneConfig::default(),
            sweep: SweepConfig::default(),
            arith: ArithMix::U32,
            boundary_checks: false,
            inner_max: 0,
        }
    }
}
impl AsyncConfig {
    pub(crate) fn validate(self) -> Result<()> {
        if !(gpu_engine::plan::MIN_DEVICE_SLOTS..=16).contains(&self.device_slots)
            || self.host_slots <= self.device_slots
            || self.host_slots > 64
            || !(1..=self.host_slots).contains(&self.workers)
            || self.quarantine_slots >= self.host_slots
            || !(1..=1024).contains(&self.batches)
            || !(1..=3_600_000).contains(&self.quarantine_max_age_ms)
            || self.rotate_every_batches > 1_000_000
            || self.corpus_threads > 16
            || self.corpus_pool > 16
            || self.integrity_every_batches > 100_000_000
            || self.workers_max < self.workers
            || self.workers_max > 32
            || matches!(self.hash_policy, HashPolicy::Checkpoint { interval: 0 })
            || self.lane.validate().is_err()
            || self.sweep.validate().is_err()
            || (self.sweep.enabled && (!self.lane.active() || self.lane.vram_local))
            || (self.arith.alternates() && self.rotate_every_batches == 0)
            || self.inner_max > gemm::MAX_K
        {
            return Err(Error::Request("async configuration"));
        }
        Ok(())
    }
    /// Counted allocation steps of one pipeline: input, upload staging, five per device slot
    /// (buffer, stream, three events), one per host slot, the boundary-check buffers and the
    /// origin event.
    #[must_use]
    pub fn allocation_steps(self) -> usize {
        let lane = if self.lane.active() {
            lane::STEPS_PER_SLOT * self.lane.slots
        } else {
            0
        };
        let checks = if self.boundary_checks {
            Checkpoints::STEPS
        } else {
            0
        };
        3 + 5 * self.device_slots + self.host_slots + lane + checks
    }
    fn threads(self) -> usize {
        if self.corpus_threads == 0 {
            default_helper_threads()
        } else {
            self.corpus_threads
        }
    }
}

/// Version 2: every counter is cumulative over the whole run (all bursts).
/// `last_burst` is informational and may be empty; it never replaces totals.
#[derive(Clone, Debug, Serialize)]
pub struct AsyncReport {
    pub version: u32,
    pub backend: String,
    pub config: AsyncConfig,
    #[serde(flatten)]
    pub cumulative: Cumulative,
    pub rates: Rates,
    /// `ok` or the first violated counter identity.
    pub conservation: String,
    pub pending: Vec<Pending>,
    pub controller: LoadController,
    /// Verifier worker scaling: current, peak, increases and reverted trials.
    pub workers: WorkerScaler,
    pub memory_pressure_samples: u64,
    pub memory_pressure_hits: u64,
    pub queue_cap_bytes: u64,
    /// Upload staging plus every pinned readback slot.
    pub pinned_bytes: u64,
    pub host_queue_bytes: u64,
    pub device_output_bytes: u64,
    pub oldest_pending_ms: u64,
    pub interrupted: bool,
    /// First error of the run; sticky.
    pub error: Option<String>,
    /// Set when a failed batch triggered the host-corpus check: `corpus_intact` (the host copy
    /// matched its hashes, so the mismatch is not explained by corpus damage) or
    /// `host_memory_changed`. Neither proves which component is at fault.
    pub failure_attribution: Option<String>,
    pub result_hash_policy: String,
    /// Transfer lane and GPU-local workload counters, per direction (absent when off).
    pub lane: Option<LaneReport>,
    /// GPU-DRAM sweep counters: device-verified and CPU-checkpointed bytes (absent when off).
    pub sweep: Option<SweepReport>,
    /// CPU share the verifier queue gave compute checks and lane checks.
    pub fairness: Fairness,
    /// GPU boundary checks (absent when off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<BoundaryReport>,
}

/// One verifier verdict. Times are CPU core-seconds on the worker.
struct Verdict {
    batch: Batch,
    /// `Ok(None)`: passed, hash skipped by policy.
    result: std::result::Result<Option<String>, String>,
    /// SHA-256 of the actual bytes of a failed batch.
    failure_hash: Option<String>,
    compare_seconds: f64,
    hash_seconds: f64,
    queue_wait_seconds: f64,
    mismatch: Option<MismatchSummary>,
    /// Boundary checks on: where a failed batch went wrong.
    boundary: Option<Kind>,
    /// Boundary checks on: the GPU found wrong words in a batch the CPU passed.
    disagreement: bool,
}

/// Work for a verifier worker.
enum Job<'a> {
    Verify {
        batch: Batch,
        bytes: &'a [u8],
        ready_at: Instant,
        hash: bool,
        /// The GPU's sealed verdict on this output, when the boundary checks are on.
        gpu_check: GpuCheck,
    },
    Lane {
        action: Action,
        ready_at: Instant,
    },
}

/// What a worker reports back to the owner.
enum Done {
    Verdict(Verdict),
    Prepared {
        slot: usize,
        seconds: f64,
        wait: f64,
    },
    Compared {
        slot: usize,
        mismatch: Option<lane::Mismatch>,
        evidence: Option<checkpoints::LaneEvidence>,
        seconds: f64,
        wait: f64,
    },
    LaneFailed,
}

fn run_lane_action(action: Action, ready_at: Instant) -> Done {
    let started = Instant::now();
    let wait = started.duration_since(ready_at).as_secs_f64();
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match action {
        Action::Prepare {
            slot,
            span,
            kind,
            seed,
            generation,
            offset,
        } => {
            // One thread: the worker pool is the parallelism, and a lane job must not fan out.
            lane::run_prepare(kind, span, seed, generation, offset, 1);
            Done::Prepared {
                slot,
                seconds: started.elapsed().as_secs_f64(),
                wait,
            }
        }
        Action::Compare {
            slot,
            expected,
            actual,
            evidence,
        } => {
            let (mismatch, evidence) = lane::run_compare(expected, actual, evidence);
            Done::Compared {
                slot,
                mismatch,
                evidence,
                seconds: started.elapsed().as_secs_f64(),
                wait,
            }
        }
    }))
    .unwrap_or(Done::LaneFailed)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify_batch(
    corpus: &Corpus,
    batch: Batch,
    bytes: &[u8],
    ready_at: Instant,
    hash: bool,
    gpu_check: GpuCheck,
) -> Verdict {
    let started = Instant::now();
    let generation = batch.generation;
    let queue_wait_seconds = started.duration_since(ready_at).as_secs_f64();
    let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match corpus.verify_bytes(batch.generation, bytes) {
            Ok(()) => {
                let compared = started.elapsed().as_secs_f64();
                let hashed = Instant::now();
                let digest = hash.then(|| sha256_hex(bytes));
                (
                    Ok(digest),
                    None,
                    compared,
                    hashed.elapsed().as_secs_f64(),
                    None,
                )
            }
            Err((index, expected, actual)) => {
                // Failure path only: count every bad word, keep a bounded sample and
                // the hash of what the device actually returned.
                let summary = corpus.mismatch_summary(batch.generation, bytes, 8);
                let compared = started.elapsed().as_secs_f64();
                let hashed = Instant::now();
                let digest = sha256_hex(bytes);
                (
                    Err(format!(
                        "matrix mismatch at {index}: expected {expected:#x}, actual {actual:#x}"
                    )),
                    Some(digest),
                    compared,
                    hashed.elapsed().as_secs_f64(),
                    Some(summary),
                )
            }
        }
    }));
    let (result, failure_hash, compare_seconds, hash_seconds, mismatch) =
        checked.unwrap_or_else(|_| {
            (
                Err("verifier panic".into()),
                None,
                started.elapsed().as_secs_f64(),
                0.0,
                None,
            )
        });
    // Boundary checks: a failure is checked a second time and placed; a pass the GPU disputes
    // is noted (failure path and one flag only, nothing on a clean pass).
    let (boundary, disagreement) = match gpu_check {
        GpuCheck::Off => (None, false),
        GpuCheck::On(tail) if result.is_err() => {
            let still_wrong = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                corpus.verify_bytes(generation, bytes).is_err()
            }))
            .unwrap_or(true);
            (Some(checkpoints::classify(bytes, tail, still_wrong)), false)
        }
        GpuCheck::On(tail) => (None, tail.is_some_and(|t| t.bad > 0)),
    };
    Verdict {
        batch,
        result,
        failure_hash,
        compare_seconds,
        hash_seconds,
        queue_wait_seconds,
        mismatch,
        boundary,
        disagreement,
    }
}

fn replay_of(seed: u64, generation: u64, jobs: usize, inner: u32) -> ReplayBatch {
    ReplayBatch {
        version: 2,
        phase: "matrix".into(),
        seed,
        generation,
        allocation: 0,
        offset: 0,
        bytes: 0,
        jobs: jobs as u64,
        rounds: inner,
    }
}

/// Kernel output plus the stream and events that order it. Released when the D2H copy lands.
pub(crate) struct DeviceSlot {
    context: Rc<Context>,
    output: Buffer,
    stream: Handle,
    events: [Handle; 3],
    in_flight: Cell<bool>,
}
impl DeviceSlot {
    fn new(backend: &CudaBackend, bytes: usize) -> Result<Self> {
        let context = &backend.context;
        backend.alloc_step()?;
        let mut slot = Self {
            context: Rc::clone(context),
            output: Buffer::new(context, bytes)?,
            stream: ptr::null_mut(),
            events: [ptr::null_mut(); 3],
            in_flight: Cell::new(false),
        };
        backend.alloc_step()?;
        check("async stream", unsafe {
            (context.driver.stream_create)(&raw mut slot.stream, 1)
        })?;
        LIVE_STREAMS.fetch_add(1, AtomicOrdering::SeqCst);
        for event in &mut slot.events {
            backend.alloc_step()?;
            check("async event", unsafe {
                (context.driver.event_create)(event, 0)
            })?;
            LIVE_EVENTS.fetch_add(1, AtomicOrdering::SeqCst);
        }
        Ok(slot)
    }
}
impl Drop for DeviceSlot {
    fn drop(&mut self) {
        if self.in_flight.get() {
            self.context.stopped.set(true);
        }
        if self.context.stopped.get() || self.context.current().is_err() {
            return;
        }
        unsafe {
            for event in self.events {
                if !event.is_null() {
                    (self.context.driver.event_destroy)(event);
                    LIVE_EVENTS.fetch_sub(1, AtomicOrdering::SeqCst);
                }
            }
            if !self.stream.is_null() {
                (self.context.driver.stream_destroy)(self.stream);
                LIVE_STREAMS.fetch_sub(1, AtomicOrdering::SeqCst);
            }
        }
    }
}

/// Pinned readback: the D2H target, then read-only for the verifier until the verdict.
struct HostSlot {
    readback: Pinned,
}

#[allow(clippy::struct_excessive_bools)] // independent state flags, not one state machine
pub(crate) struct Pipeline {
    context: Rc<Context>,
    /// Common time base so events from different streams can be compared.
    origin: Handle,
    cumulative: Cumulative,
    first_error: Option<String>,
    failure_attribution: Option<String>,
    interrupted: bool,
    // Device slots drop first: one still in flight marks the context stopped, so the host
    // slots they write to are then leaked rather than freed under a running copy.
    devices: Vec<DeviceSlot>,
    hosts: Vec<HostSlot>,
    input: Buffer,
    upload: Pinned,
    corpus: Option<Arc<Corpus>>,
    /// Base seed of the run and how many corpora have been rotated through.
    base_seed: Option<u64>,
    rotation: u64,
    batches_on_corpus: u64,
    prefetch: Option<Prefetch>,
    /// Built corpora kept besides the one in use, oldest first (C13 G2); all of the current shape.
    pool: VecDeque<Arc<Corpus>>,
    /// Writing the input of the corpus a coming rotation takes into `upload` (C13 G3). The owner
    /// thread touches `upload` only after joining it.
    staging: Option<Staging>,
    /// The corpus whose input `upload` holds in full.
    staged: Option<Arc<Corpus>>,
    /// Uploads that found their corpus already in `upload` (C13 G3).
    staged_installs: u64,
    /// The periodic host-corpus check running on a helper thread (C13 G3).
    integrity_job: Option<thread::JoinHandle<IntegrityOutcome>>,
    /// Shape of the corpus in use (it changes only at a rotation, between bursts).
    jobs: usize,
    inner: u32,
    shape: AdaptiveShape,
    config: AsyncConfig,
    controller: LoadController,
    scaler: WorkerScaler,
    pressure: RefCell<PressureSampler>,
    submitted_total: u64,
    batches_since_integrity: u64,
    epoch: Instant,
    next_generation: u64,
    verified_total: u64,
    /// GPU-H6 lane; taken out for the duration of a burst.
    lane: Option<Lane>,
    /// The device runs the INT8 tensor-core GEMM (an `All` mix then includes it).
    tensor: bool,
    /// GPU-DRAM sweep, created once the pool exists; taken out for a burst like the lane.
    sweeper: Option<Sweeper>,
    /// The lane and the sweep both covered their half: trade halves once both are idle.
    swap_pending: bool,
    /// When the halves were last traded (or the sweep started).
    last_swap: Instant,
    /// A sweep fault injected before the sweep existed.
    pending_sweep_fault: Option<LaneFault>,
    /// A corpus loaded from outside (`load_external`, e.g. read back from storage by dit-chain):
    /// bursts use it as is, with no rotation or prefetch, until another one is loaded.
    external: bool,
    /// Host-corpus check of the last burst that had a failed batch: `corpus_intact` or
    /// `host_memory_changed` (unlike `failure_attribution`, reset every burst).
    burst_attribution: Option<&'static str>,
    fairness: Fairness,
    /// Boundary checks (step F3), when on.
    checks: Option<Checkpoints>,
    /// The corpus on the device is known wrong (a check found it changed): upload it again
    /// before the next burst.
    reupload: bool,
}

/// Adaptive shape of the pipeline: the ladder (none when K is fixed), the level in use, the level
/// of the next corpus, the bursts since the last decision and what that decision saw.
struct AdaptiveShape {
    ladder: Option<ShapeLadder>,
    level: u32,
    next_level: u32,
    window: ShapeWindow,
    step_percent: u64,
}

impl AdaptiveShape {
    fn new(jobs: usize, inner: u32, inner_max: u32) -> Self {
        Self {
            ladder: ShapeLadder::new(Shape { jobs, inner }, inner_max),
            level: 0,
            next_level: 0,
            window: ShapeWindow::default(),
            step_percent: 0,
        }
    }
}

/// A corpus being built on a helper thread for the next rotation.
struct Prefetch {
    seed: u64,
    arith: Arith,
    shape: Shape,
    build: PrefetchBuild,
}

enum PrefetchBuild {
    Running(thread::JoinHandle<std::result::Result<Corpus, &'static str>>),
    /// Joined once finished, so its input can be staged before its rotation (C13 G3).
    Built(std::result::Result<Arc<Corpus>, &'static str>),
}

impl Prefetch {
    fn is_built(&self) -> bool {
        match &self.build {
            PrefetchBuild::Running(handle) => handle.is_finished(),
            PrefetchBuild::Built(_) => true,
        }
    }

    /// The corpus once its build is done (joined now, without waiting); `None` while it builds
    /// or when the build failed (which [`Self::wait`] reports).
    fn built(&mut self) -> Option<&Arc<Corpus>> {
        if matches!(&self.build, PrefetchBuild::Running(handle) if handle.is_finished()) {
            let joined = PrefetchBuild::Built(Err("corpus prefetch panicked"));
            if let PrefetchBuild::Running(handle) = std::mem::replace(&mut self.build, joined) {
                self.build = PrefetchBuild::Built(match handle.join() {
                    Ok(built) => built.map(Arc::new),
                    Err(_) => Err("corpus prefetch panicked"),
                });
            }
        }
        match &self.build {
            PrefetchBuild::Built(Ok(corpus)) => Some(corpus),
            _ => None,
        }
    }

    /// Waits for the build.
    fn wait(self) -> Result<Arc<Corpus>> {
        match self.build {
            PrefetchBuild::Running(handle) => match handle.join() {
                Ok(built) => built.map(Arc::new).map_err(Error::Request),
                Err(_) => Err(Error::Request("corpus prefetch panicked")),
            },
            PrefetchBuild::Built(built) => built.map_err(Error::Request),
        }
    }
}

/// The input of the corpus a coming rotation takes, being written into the upload staging on a
/// helper thread while bursts run (C13 G3): the rotation then has only the copy to VRAM left.
struct Staging {
    corpus: Arc<Corpus>,
    handle: thread::JoinHandle<()>,
}

/// A periodic host-corpus check on a helper thread (C13 G3): what it found and how long it took.
type IntegrityOutcome = (std::result::Result<(), &'static str>, f64);

/// Lets go of a corpus no longer used, freeing it on a helper thread when this is its last
/// reference: freeing a corpus of a quarter GiB holds a thread some 25 ms (C13 G3).
fn retire(corpus: Option<Arc<Corpus>>) {
    if let Some(corpus) = corpus.filter(|c| Arc::strong_count(c) == 1) {
        // Without a thread, the closure and the corpus are dropped here.
        let _ = thread::Builder::new()
            .name("gpu-corpus-free".into())
            .spawn(move || drop(corpus));
    }
}

/// Writes `words` as little-endian bytes (as the device reads them) at `address`.
///
/// # Safety
/// `address` must be valid for `words.len() * 4` bytes of writes, and nothing else may access
/// those bytes until this returns.
unsafe fn copy_words(address: usize, words: &[u32]) {
    let target = ptr::with_exposed_provenance_mut::<u8>(address);
    #[cfg(target_endian = "little")]
    unsafe {
        ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), target, words.len() * 4);
    }
    #[cfg(not(target_endian = "little"))]
    for (index, word) in words.iter().enumerate() {
        let bytes = word.to_le_bytes();
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), target.add(index * 4), 4);
        }
    }
}

/// What `prepare` did, for the burst's accounting.
#[allow(clippy::struct_excessive_bools)] // Independent facts about one corpus choice, not a state.
struct Prepared {
    reused: bool,
    /// Seed of the corpus actually in use (derived from the run's base seed).
    seed: u64,
    rotated: bool,
    prefetch_hit: bool,
    wait_seconds: f64,
    /// Uploading a corpus to VRAM (a new one, or the one in use again after a boundary check).
    install_seconds: f64,
    /// A rotation was due and no new corpus was ready: one from the pool, or the one in use.
    pool_served: bool,
    /// The corpus was built for this burst (its oracle and input time belong to it).
    fresh: bool,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // The staging thread writes into `upload`: it ends before the field frees it.
        self.finish_staging();
        if let Some(job) = self.integrity_job.take() {
            let _ = job.join();
        }
        if self.context.stopped.get() || self.context.current().is_err() || self.origin.is_null() {
            return;
        }
        unsafe {
            (self.context.driver.event_destroy)(self.origin);
        }
        LIVE_EVENTS.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

/// Host readback and device bytes of one output slot. With the checks on, the GPU's sealed
/// verdict rides behind every output; on the device the check's partials and counter follow it.
fn slot_sizes(config: AsyncConfig, output: usize, jobs: usize) -> (usize, usize) {
    if config.boundary_checks {
        let readback = output + checkpoints::TAIL_RESERVE;
        (readback, readback + checkpoints::partials_bytes(jobs))
    } else {
        (output, output)
    }
}

/// The fused check's completion counter resets itself after every batch; it starts at zero here,
/// once per slot.
pub(crate) fn clear_check_counters(backend: &CudaBackend, devices: &[DeviceSlot]) -> Result<()> {
    for slot in devices {
        check("boundary counter clear", unsafe {
            (backend.context.driver.fill)(
                checkpoints::counter_address(slot.output.address, slot.output.len),
                0,
                4,
                backend.stream,
            )
        })?;
    }
    wait_stream(backend, "boundary counter clear")
}

/// The load controller's configuration. The boundary checks add two short kernels to every
/// batch's stream; one more batch on the device from the start hides them (the controller still
/// lowers the limit on verifier backlog or memory pressure).
fn control_config(config: AsyncConfig) -> ControlConfig {
    let mut control = ControlConfig::new(config.device_slots, config.host_slots);
    if config.boundary_checks {
        control.warm_limit = (control.warm_limit + 1).min(control.device_slots);
    }
    control
}

/// Waits for the owner stream to go idle; bounded, marks the context stopped on timeout.
pub(crate) fn wait_stream(backend: &CudaBackend, call: &'static str) -> Result<()> {
    let start = Instant::now();
    loop {
        let status = unsafe { (backend.context.driver.stream_query)(backend.stream) };
        if status == 0 {
            return Ok(());
        }
        if status != 600 || start.elapsed() > Duration::from_secs(30) {
            backend.context.stopped.set(true);
            return if status == 600 {
                Err(Error::Timeout)
            } else {
                Err(Error::Api { call, status })
            };
        }
        thread::sleep(Duration::from_micros(100));
    }
}

impl Pipeline {
    #[allow(clippy::too_many_lines)] // Mostly the initializer, one field per line.
    pub(crate) fn new(
        backend: &CudaBackend,
        jobs: usize,
        inner: u32,
        config: AsyncConfig,
    ) -> Result<Self> {
        config.validate()?;
        let tensor = backend.tensor_available();
        if config.arith == ArithMix::I8 && !tensor {
            return Err(Error::Request(
                "INT8 tensor GEMM needs compute capability 8.0 or newer",
            ));
        }
        if (config.arith == ArithMix::I8 || (config.arith == ArithMix::All && tensor))
            && !inner.is_multiple_of(32)
        {
            return Err(Error::Request(
                "INT8 tensor GEMM needs an inner dimension divisible by 32",
            ));
        }
        backend.alloc_counter.set(0);
        let (input, output, _) = gemm::workspace_per_job(inner);
        let input = usize::try_from(input).map_err(|_| Error::Request("input size"))? * jobs;
        let output = usize::try_from(output).map_err(|_| Error::Request("output size"))? * jobs;
        let (output, device_output) = slot_sizes(config, output, jobs);
        // Every resource is an owner (RAII) until the pipeline exists, so a failure at any
        // step frees what was already allocated. The raw origin event is created last.
        backend.alloc_step()?;
        let input = Buffer::new(&backend.context, input)?;
        backend.alloc_step()?;
        let upload = Pinned::new(&backend.context, input.len)?;
        let devices = (0..config.device_slots)
            .map(|_| DeviceSlot::new(backend, device_output))
            .collect::<Result<Vec<_>>>()?;
        let hosts = (0..config.host_slots)
            .map(|_| {
                backend.alloc_step()?;
                Ok(HostSlot {
                    readback: Pinned::new(&backend.context, output)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let checks = config
            .boundary_checks
            .then(|| Checkpoints::new(backend, jobs, &devices))
            .transpose()?;
        let mut lane = config
            .lane
            .active()
            .then(|| Lane::new(backend, config.lane, config.boundary_checks))
            .transpose()?;
        if config.sweep.enabled
            && let Some(lane) = lane.as_mut()
        {
            lane.set_half(Some(0));
        }
        backend.alloc_step()?;
        let mut origin = ptr::null_mut();
        check("async origin event", unsafe {
            (backend.context.driver.event_create)(&raw mut origin, 0)
        })?;
        LIVE_EVENTS.fetch_add(1, AtomicOrdering::SeqCst);
        Ok(Self {
            context: Rc::clone(&backend.context),
            origin,
            cumulative: Cumulative::default(),
            first_error: None,
            failure_attribution: None,
            interrupted: false,
            devices,
            hosts,
            input,
            upload,
            corpus: None,
            base_seed: None,
            rotation: 0,
            batches_on_corpus: 0,
            prefetch: None,
            pool: VecDeque::new(),
            staging: None,
            staged: None,
            staged_installs: 0,
            integrity_job: None,
            jobs,
            inner,
            shape: AdaptiveShape::new(jobs, inner, config.inner_max),
            config,
            controller: LoadController::new(control_config(config)),
            scaler: WorkerScaler::new(config.workers, config.workers_max),
            pressure: RefCell::new(PressureSampler::new(PressureConfig {
                min_available_bytes: config.min_available_bytes,
                // Test hook: the file's `1`/`0` overrides the real memory readings.
                override_file: std::env::var_os("DIT_GPU_PRESSURE_FILE").map(Into::into),
                ..PressureConfig::default()
            })),
            submitted_total: 0,
            batches_since_integrity: 0,
            epoch: Instant::now(),
            next_generation: 0,
            verified_total: 0,
            lane,
            tensor,
            sweeper: None,
            swap_pending: false,
            last_swap: Instant::now(),
            pending_sweep_fault: None,
            external: false,
            burst_attribution: None,
            fairness: Fairness::default(),
            checks,
            reupload: false,
        })
    }

    pub(crate) fn lane_report(&self) -> Option<LaneReport> {
        self.lane.as_ref().map(Lane::report)
    }

    pub(crate) fn sweep_report(&self) -> Option<SweepReport> {
        self.sweeper.as_ref().map(Sweeper::report)
    }

    pub(crate) fn take_lane_failures(&mut self) -> Vec<LaneFailure> {
        let mut failures = self
            .lane
            .as_mut()
            .map(Lane::take_failures)
            .unwrap_or_default();
        if let Some(sweeper) = self.sweeper.as_mut() {
            failures.extend(sweeper.take_failures());
        }
        failures
    }

    pub(crate) fn inject_lane_fault(&mut self, fault: LaneFault) -> Result<()> {
        if matches!(fault, LaneFault::SweepVerify | LaneFault::SweepCheckpoint) {
            if !self.config.sweep.enabled {
                return Err(Error::Request("the GPU-DRAM sweep is off"));
            }
            // The sweep starts with the first burst; hold the fault until then.
            match self.sweeper.as_mut() {
                Some(sweeper) => sweeper.inject(fault),
                None => self.pending_sweep_fault = Some(fault),
            }
            return Ok(());
        }
        self.lane
            .as_mut()
            .ok_or(Error::Request("the transfer lane is off"))?
            .inject(fault);
        Ok(())
    }

    /// Verifies every region still awaiting its re-read or checkpoint.
    pub(crate) fn flush_lane(&mut self, backend: &CudaBackend) -> Result<()> {
        let origin = self.origin;
        if let Some(sweeper) = self.sweeper.as_mut() {
            sweeper.finish_pass(backend)?;
        }
        if let Some(lane) = self.lane.as_mut() {
            lane.bind(backend)?;
            lane.flush(backend, origin)?;
        }
        Ok(())
    }

    pub(crate) fn config(&self) -> AsyncConfig {
        self.config
    }

    /// Makes `corpus` (built elsewhere, e.g. read back from storage) the corpus of the next
    /// bursts: uploaded now, then used as is, with no rotation, until another one is loaded.
    pub(crate) fn load_external(&mut self, backend: &CudaBackend, corpus: Corpus) -> Result<()> {
        if corpus.jobs != self.jobs || corpus.k != self.inner as usize || corpus.n > gemm::MAX_N {
            return Err(Error::Request(
                "external corpus does not match the prepared shape",
            ));
        }
        if corpus.arith == Arith::I8 && !self.tensor {
            return Err(Error::Request(
                "INT8 tensor GEMM needs compute capability 8.0 or newer",
            ));
        }
        if corpus.input.len() * 4 > self.input.len {
            return Err(Error::Request("external corpus exceeds the input buffer"));
        }
        self.prefetch = None;
        self.pool.drain(..).for_each(|kept| retire(Some(kept)));
        retire(self.corpus.take());
        let corpus = Arc::new(corpus);
        self.base_seed = Some(corpus.seed);
        self.batches_on_corpus = 0;
        self.external = true;
        self.install(backend, &corpus)
    }

    /// Uploads `corpus` as the one in use and, with the boundary checks on, checks that the
    /// device received it (c2). A copy found wrong fails with the boundary tag, after leaving the
    /// corpus right on the device when a second upload could.
    fn install(&mut self, backend: &CudaBackend, corpus: &Arc<Corpus>) -> Result<()> {
        self.finish_staging();
        if self.staged.as_ref().is_some_and(|s| Arc::ptr_eq(s, corpus)) {
            self.staged_installs += 1;
        } else {
            retire(self.staged.take());
            self.upload.write_u32(&corpus.input);
            self.staged = Some(Arc::clone(corpus));
        }
        check("async corpus upload", unsafe {
            (backend.context.driver.upload)(
                self.input.address,
                self.upload.address,
                corpus.input.len() * 4,
                backend.stream,
            )
        })?;
        wait_stream(backend, "corpus upload")?;
        retire(self.corpus.replace(Arc::clone(corpus)));
        self.reupload = false;
        let threads = self.config.threads();
        let Some(checks) = self.checks.as_mut() else {
            return Ok(());
        };
        checks.prepare(corpus, threads);
        let outcomes = checks.check_upload(backend, &self.input, &self.upload)?;
        let (counts, broken) = checkpoints::upload_counts(&outcomes);
        if counts.is_empty() {
            return Ok(());
        }
        checks.report.counts.add(&counts);
        self.reupload = broken;
        Err(Error::Unavailable(format!(
            "corpus upload check: {} {}",
            if broken {
                "the corpus on the GPU is still wrong after a second upload"
            } else {
                "the corpus arrived wrong on the GPU and right after a second upload"
            },
            counts.tag()
        )))
    }

    pub(crate) fn burst_attribution(&self) -> Option<&'static str> {
        self.burst_attribution
    }

    fn start_prefetch(&mut self, base: u64) {
        if self.external || self.config.rotate_every_batches == 0 || self.prefetch.is_some() {
            return;
        }
        self.decide_shape();
        let seed = derive_corpus_seed(base, self.rotation + 1);
        let arith = self
            .config
            .arith
            .for_rotation(self.rotation + 1, self.tensor);
        let shape = self.next_shape();
        let (jobs, inner, threads) = (shape.jobs, shape.inner, self.config.threads());
        self.prefetch = thread::Builder::new()
            .name("gpu-corpus-prefetch".into())
            .spawn(move || Corpus::new_arith(arith, seed, jobs, inner, threads))
            .ok()
            .map(|handle| Prefetch {
                seed,
                arith,
                shape,
                build: PrefetchBuild::Running(handle),
            });
    }

    /// Builds a corpus of the shape in use here, waiting for it (no prefetch to take).
    fn build_now(&self, arith: Arith, seed: u64) -> Result<Arc<Corpus>> {
        Corpus::new_arith(arith, seed, self.jobs, self.inner, self.config.threads())
            .map(Arc::new)
            .map_err(Error::Request)
    }

    /// Joins the staging thread (started a burst or more ago, so normally done): `upload` is the
    /// owner thread's again.
    fn finish_staging(&mut self) {
        if let Some(staging) = self.staging.take() {
            // The copy cannot fail; a thread that did not finish leaves `upload` unknown.
            let staged = staging.handle.join().is_ok().then_some(staging.corpus);
            retire(std::mem::replace(&mut self.staged, staged));
        }
    }

    /// Frees the staged corpus now (when nothing else holds it), so a cold build's peak is one
    /// corpus plus staging.
    fn release_staged(&mut self) {
        self.finish_staging();
        self.staged = None;
    }

    /// C13 G3: writes the input of the corpus the coming rotation takes into `upload` on a helper
    /// thread while the bursts run: the new corpus once built, else (with a pool, from two bursts
    /// before the rotation) the pool's oldest, which that rotation takes then.
    fn stage_next(&mut self) {
        let every = self.config.rotate_every_batches;
        if self.external || every == 0 {
            return;
        }
        let near = self.batches_on_corpus + 2 * self.config.batches >= every;
        let next = if let Some(prefetch) = self.prefetch.as_mut().filter(|p| p.is_built()) {
            prefetch.built().cloned()
        } else if self.config.corpus_pool > 0 && near {
            self.pool.front().cloned()
        } else {
            None
        };
        let Some(next) = next.filter(|next| next.input.len() * 4 <= self.upload.len) else {
            return;
        };
        let holds = |corpus: Option<&Arc<Corpus>>| corpus.is_some_and(|c| Arc::ptr_eq(c, &next));
        if holds(self.staged.as_ref()) || holds(self.staging.as_ref().map(|s| &s.corpus)) {
            return;
        }
        self.finish_staging();
        retire(self.staged.take());
        let address = self.upload.address.expose_provenance();
        let corpus = Arc::clone(&next);
        self.staging = thread::Builder::new()
            .name("gpu-corpus-stage".into())
            // SAFETY: `address` is the pinned upload staging, checked above to hold the input; the
            // owner thread neither reads, uploads from nor frees it before joining this thread
            // (`finish_staging`, in `install` and on drop).
            .spawn(move || unsafe { copy_words(address, &corpus.input) })
            .ok()
            .map(|handle| Staging {
                corpus: next,
                handle,
            });
    }

    /// Adaptive shape: the bursts since the last decision choose the shape of the next corpus,
    /// whose build starts now.
    fn decide_shape(&mut self) {
        let shape = &mut self.shape;
        let Some(ladder) = shape.ladder else {
            return;
        };
        if shape.next_level != shape.level {
            return;
        }
        match ladder.decide(shape.level, &shape.window) {
            ShapeDecision::Wait => {}
            ShapeDecision::Keep => shape.window = ShapeWindow::default(),
            ShapeDecision::StepUp => {
                shape.next_level = shape.level + 1;
                shape.step_percent = shape.window.verifier_percent();
                shape.window = ShapeWindow::default();
            }
        }
    }

    fn next_shape(&self) -> Shape {
        self.shape.ladder.map_or(
            Shape {
                jobs: self.jobs,
                inner: self.inner,
            },
            |ladder| ladder.shape(self.shape.next_level),
        )
    }

    /// At a rotation: the shape decided for this corpus takes over, nothing being in flight.
    fn apply_shape(&mut self) {
        if self.shape.next_level == self.shape.level {
            return;
        }
        self.shape.level = self.shape.next_level;
        let shape = self.next_shape();
        self.jobs = shape.jobs;
        self.inner = shape.inner;
        self.cumulative.shape_steps.push(ShapeStep {
            at_seconds: self.epoch.elapsed().as_secs(),
            rotation: self.rotation,
            jobs: shape.jobs,
            inner: shape.inner,
            verifier_bound_percent: self.shape.step_percent,
        });
    }

    /// Chooses the corpus for the next burst: reuse, a prefetched rotation, or a
    /// cold build. Only ever called with no batch outstanding.
    fn prepare(&mut self, backend: &CudaBackend, base: u64) -> Result<Prepared> {
        let mut install_seconds = self.reinstall_if_broken(backend)?;
        if self.external {
            let seed = self
                .corpus
                .as_ref()
                .ok_or(Error::Request("external corpus missing"))?
                .seed;
            return Ok(Prepared {
                reused: true,
                seed,
                rotated: false,
                prefetch_hit: false,
                wait_seconds: 0.0,
                install_seconds,
                pool_served: false,
                fresh: false,
            });
        }
        if self.config.corpus_pool > 0 {
            return self.prepare_pooled(backend, base, install_seconds);
        }
        if self.base_seed != Some(base) {
            // A new run seed restarts the rotation sequence; a stale prefetch is dropped.
            self.base_seed = Some(base);
            self.rotation = 0;
            self.batches_on_corpus = 0;
            self.prefetch = None;
        }
        let mut rotated = false;
        let every = self.config.rotate_every_batches;
        if every > 0 && self.batches_on_corpus >= every {
            self.rotation += 1;
            self.batches_on_corpus = 0;
            rotated = true;
            self.apply_shape();
        }
        let seed = derive_corpus_seed(base, self.rotation);
        let arith = self.config.arith.for_rotation(self.rotation, self.tensor);
        if self
            .corpus
            .as_ref()
            .is_some_and(|c| c.matches_arith(arith, seed, self.jobs, self.inner))
        {
            self.start_prefetch(base);
            return Ok(Prepared {
                reused: true,
                seed,
                rotated: false,
                prefetch_hit: false,
                wait_seconds: 0.0,
                install_seconds,
                pool_served: false,
                fresh: false,
            });
        }
        let waited = Instant::now();
        let (corpus, prefetch_hit) = match self.prefetch.take() {
            Some(p)
                if p.seed == seed
                    && p.arith == arith
                    && p.shape
                        == (Shape {
                            jobs: self.jobs,
                            inner: self.inner,
                        }) =>
            {
                (p.wait()?, true)
            }
            _ => {
                // A cold build starts from nothing: free the old corpus first so the peak is one
                // corpus plus staging, not two.
                self.corpus.take();
                self.release_staged();
                (self.build_now(arith, seed)?, false)
            }
        };
        let wait_seconds = waited.elapsed().as_secs_f64();
        // No outstanding batches cross this boundary. Drop old corpus before replacement.
        retire(self.corpus.take());
        let installing = Instant::now();
        let installed = self.install(backend, &corpus);
        install_seconds += installing.elapsed().as_secs_f64();
        self.start_prefetch(base);
        installed?;
        Ok(Prepared {
            reused: false,
            seed,
            rotated,
            prefetch_hit,
            wait_seconds,
            install_seconds,
            pool_served: false,
            fresh: true,
        })
    }

    /// C13 G2: the corpus of the next burst, never waiting for one being built but the very first
    /// (and the first of a new shape, which nothing else fits). A due rotation takes the new
    /// corpus when its build is done, else the oldest one in the pool (the one in use goes to the
    /// back), else keeps the one in use. Each still has its own oracle and is verified word for
    /// word against it. Only ever called with no batch outstanding.
    fn prepare_pooled(
        &mut self,
        backend: &CudaBackend,
        base: u64,
        mut install_seconds: f64,
    ) -> Result<Prepared> {
        if self.base_seed != Some(base) {
            // A new run seed starts a new line of corpora; what is built or kept stays valid.
            self.base_seed = Some(base);
            self.rotation = 0;
        }
        let current_fits = self
            .corpus
            .as_ref()
            .is_some_and(|c| c.jobs == self.jobs && c.k == self.inner as usize);
        let every = self.config.rotate_every_batches;
        let due = every > 0 && self.batches_on_corpus >= every;
        let in_use_seed = |pipeline: &Self| pipeline.corpus.as_ref().map_or(0, |c| c.seed);
        if current_fits && !due {
            self.start_prefetch(base);
            return Ok(Prepared {
                reused: true,
                seed: in_use_seed(self),
                rotated: false,
                prefetch_hit: false,
                wait_seconds: 0.0,
                install_seconds,
                pool_served: false,
                fresh: false,
            });
        }
        let built = self.prefetch.as_ref().is_some_and(Prefetch::is_built);
        if current_fits && !built {
            return self.serve_from_pool(backend, base, install_seconds);
        }
        // A new corpus: its build is done, or nothing fits to run on (the first corpus).
        let waited = Instant::now();
        let shape_now = |pipeline: &Self| Shape {
            jobs: pipeline.jobs,
            inner: pipeline.inner,
        };
        let mut prefetched = None;
        if let Some(prefetch) = self.prefetch.take() {
            if prefetch.shape != shape_now(self) {
                // The adaptive shape stepped: the new corpus is of the next shape, which nothing
                // kept fits.
                self.apply_shape();
                self.pool.drain(..).for_each(|kept| retire(Some(kept)));
                retire(self.corpus.take());
                self.finish_staging();
                retire(self.staged.take());
            }
            let shape = prefetch.shape;
            let built = prefetch.wait()?;
            // Never upload a corpus of another shape than the kernels will run.
            if shape == shape_now(self) {
                prefetched = Some(built);
            }
        }
        let prefetch_hit = prefetched.is_some();
        let corpus = if let Some(corpus) = prefetched {
            corpus
        } else {
            let arith = self.config.arith.for_rotation(self.rotation, self.tensor);
            self.build_now(arith, derive_corpus_seed(base, self.rotation))?
        };
        let wait_seconds = waited.elapsed().as_secs_f64();
        let rotated = prefetch_hit && self.corpus.is_some();
        if prefetch_hit {
            self.rotation += 1;
        }
        // The one in use joins the pool; the oldest beyond the pool's size goes.
        if let Some(current) = self.corpus.take()
            && current.jobs == self.jobs
            && current.k == self.inner as usize
        {
            self.pool.push_back(current);
        }
        while self.pool.len() > self.config.corpus_pool {
            retire(self.pool.pop_front());
        }
        self.batches_on_corpus = 0;
        let seed = corpus.seed;
        let installing = Instant::now();
        let installed = self.install(backend, &corpus);
        install_seconds += installing.elapsed().as_secs_f64();
        self.start_prefetch(base);
        installed?;
        Ok(Prepared {
            reused: false,
            seed,
            rotated,
            prefetch_hit,
            wait_seconds,
            install_seconds,
            pool_served: false,
            fresh: true,
        })
    }

    /// The corpus on the device is known wrong (a check found it changed): upload it again.
    /// Returns the seconds the upload took.
    fn reinstall_if_broken(&mut self, backend: &CudaBackend) -> Result<f64> {
        if !self.reupload {
            return Ok(0.0);
        }
        let Some(corpus) = self.corpus.clone() else {
            return Ok(0.0);
        };
        let installing = Instant::now();
        self.install(backend, &corpus)?;
        Ok(installing.elapsed().as_secs_f64())
    }

    /// A due rotation with the next corpus not built yet (C13 G2): the pool's oldest corpus
    /// (the one in use goes to the back), or the one in use again when the pool is empty.
    fn serve_from_pool(
        &mut self,
        backend: &CudaBackend,
        base: u64,
        mut install_seconds: f64,
    ) -> Result<Prepared> {
        self.batches_on_corpus = 0;
        let mut reused = true;
        if let Some(next) = self.pool.pop_front() {
            if let Some(current) = self.corpus.take() {
                self.pool.push_back(current);
            }
            let installing = Instant::now();
            let installed = self.install(backend, &next);
            install_seconds += installing.elapsed().as_secs_f64();
            installed?;
            reused = false;
        }
        self.start_prefetch(base);
        Ok(Prepared {
            reused,
            seed: self.corpus.as_ref().map_or(0, |c| c.seed),
            rotated: false,
            prefetch_hit: false,
            wait_seconds: 0.0,
            install_seconds,
            pool_served: true,
            fresh: false,
        })
    }

    /// Starts the periodic host-corpus check on a helper thread (C13 G3); false when no thread
    /// could be started (the check then runs here).
    fn start_integrity(&mut self, corpus: &Arc<Corpus>) -> bool {
        let corpus = Arc::clone(corpus);
        self.integrity_job = thread::Builder::new()
            .name("gpu-corpus-integrity".into())
            .spawn(move || {
                let checked = Instant::now();
                let outcome = corpus.integrity_check();
                (outcome, checked.elapsed().as_secs_f64())
            })
            .ok();
        self.integrity_job.is_some()
    }

    /// The background host-corpus check once it has finished, or waited for when `wait`.
    fn take_integrity(&mut self, wait: bool) -> Option<IntegrityOutcome> {
        if !wait && !self.integrity_job.as_ref()?.is_finished() {
            return None;
        }
        let job = self.integrity_job.take()?;
        // A check that did not finish proved nothing intact.
        Some(
            job.join()
                .unwrap_or((Err("host corpus check did not finish"), 0.0)),
        )
    }

    /// A host-corpus check found the host copy changed: host memory, not the GPU, is suspect.
    fn host_memory_changed(&mut self, failure: &mut Option<String>, reason: &str) {
        self.burst_attribution = Some("host_memory_changed");
        self.failure_attribution
            .get_or_insert_with(|| "host_memory_changed".into());
        failure.get_or_insert_with(|| format!("host corpus integrity: {reason}"));
    }

    /// End of the run (C13 G3): waits for the host-corpus check still on its helper thread and
    /// counts it. Returns what it found wrong.
    pub(crate) fn finish(&mut self) -> Option<String> {
        let (outcome, seconds) = self.take_integrity(true)?;
        self.cumulative.integrity_checks += 1;
        self.cumulative.timing.integrity_background_seconds += seconds;
        let reason = outcome.err()?;
        self.cumulative.integrity_failures += 1;
        let mut failure = None;
        self.host_memory_changed(&mut failure, reason);
        if let Some(error) = &failure {
            self.first_error.get_or_insert_with(|| error.clone());
        }
        failure
    }

    /// Negative gate: the host copy of the corpus in use changes (one input bit) after its
    /// upload, as damaged host memory would change it; the device copy stays right.
    pub(crate) fn inject_host_corpus_fault(&mut self) -> Result<()> {
        let corpus = self
            .corpus
            .as_ref()
            .ok_or(Error::Request("corpus missing"))?;
        let mut changed = Corpus::clone(corpus);
        changed.input[0] ^= 1;
        self.corpus = Some(Arc::new(changed));
        Ok(())
    }

    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    fn launch(
        &self,
        backend: &CudaBackend,
        batch: &Batch,
        bytes: usize,
        fault: u32,
        seal_fault: u32,
    ) -> Result<()> {
        let slot = &self.devices[batch.device_slot];
        let host = &self.hosts[batch.slot];
        // Mark before the first queued operation; error paths retain referenced memory.
        slot.in_flight.set(true);
        let driver = &backend.context.driver;
        check("output guards", unsafe {
            (driver.fill)(slot.output.address, 0xA55A_1973, bytes / 4, slot.stream)
        })?;
        let tail = slot.output.address + checkpoints::tail_offset(bytes) as u64;
        check("compute start", unsafe {
            (driver.event_record)(slot.events[0], slot.stream)
        })?;
        let mut input = self.input.address;
        let mut output = slot.output.address;
        let mut jobs = self.jobs as u32;
        let mut inner = self.inner;
        let corpus = self
            .corpus
            .as_ref()
            .ok_or(Error::Request("corpus missing"))?;
        let mut dimension = corpus.n as u32;
        let arith = corpus.arith;
        // With the checks on, the u32 and FP32 GEMMs check their own output in the same kernel
        // (no extra launch); the INT8 tensor GEMM is followed by the separate check kernels.
        let fused = self.checks.is_some() && arith != Arith::I8;
        let function = match (arith, fused) {
            (Arith::U32, false) => backend.function,
            (Arith::U32, true) => backend.gemm_checked_function,
            (Arith::F32, false) => backend.function_f32,
            (Arith::F32, true) => backend.gemm_f32_checked_function,
            (Arith::I8, _) => backend.function_i8,
        };
        // Tiled GEMMs: one 16x16 block per output tile. Tensor: 4 warps of 16x8 tiles per block.
        let (grid_x, grid_y, block_x, block_y) = if arith == Arith::I8 {
            (((dimension / 16) * (dimension / 8)).div_ceil(4), 1, 128, 1)
        } else {
            (dimension.div_ceil(16), dimension.div_ceil(16), 16, 16)
        };
        let mut lo = batch.generation as u32;
        let mut hi = (batch.generation >> 32) as u32;
        let mut fault = fault;
        let mut answers = self.checks.as_ref().map_or(0, Checkpoints::answers_address);
        let mut tail_arg = tail;
        let mut partials =
            checkpoints::partials_address(slot.output.address, slot.output.len, self.jobs);
        let mut counter = checkpoints::counter_address(slot.output.address, slot.output.len);
        let mut seal = seal_fault;
        let mut all_params = [
            (&raw mut input).cast::<c_void>(),
            (&raw mut output).cast(),
            (&raw mut jobs).cast(),
            (&raw mut inner).cast(),
            (&raw mut dimension).cast(),
            (&raw mut lo).cast(),
            (&raw mut hi).cast(),
            (&raw mut fault).cast(),
            (&raw mut answers).cast(),
            (&raw mut tail_arg).cast(),
            (&raw mut partials).cast(),
            (&raw mut counter).cast(),
            (&raw mut seal).cast(),
        ];
        // The plain GEMMs take the first eight parameters; the extra ones are then ignored.
        let params = &mut all_params;
        check("async GEMM", unsafe {
            (driver.launch)(
                function,
                grid_x,
                grid_y,
                jobs,
                block_x,
                block_y,
                1,
                0,
                slot.stream,
                params.as_mut_ptr(),
                ptr::null_mut(),
            )
        })?;
        let readback = match &self.checks {
            Some(checks) => {
                if !fused {
                    self.launch_check(
                        backend,
                        slot,
                        checks.answers_address(),
                        tail,
                        lo,
                        hi,
                        seal_fault,
                    )?;
                }
                checkpoints::readback_len(bytes)
            }
            None => bytes,
        };
        check("compute end", unsafe {
            (driver.event_record)(slot.events[1], slot.stream)
        })?;
        check("async readback", unsafe {
            (driver.download)(
                host.readback.address,
                slot.output.address,
                readback,
                slot.stream,
            )
        })?;
        check("readback end", unsafe {
            (driver.event_record)(slot.events[2], slot.stream)
        })?;
        Ok(())
    }

    /// c3: the GPU checks the output of the batch against the answers and hashes it (one block
    /// per job, each writing its partial behind the tail), then one block sums the partials and
    /// seals the tail. Both are queued on the batch's stream, before the copy back.
    #[allow(clippy::too_many_arguments, clippy::cast_possible_truncation)]
    fn launch_check(
        &self,
        backend: &CudaBackend,
        slot: &DeviceSlot,
        answers: u64,
        tail: u64,
        lo: u32,
        hi: u32,
        seal_fault: u32,
    ) -> Result<()> {
        let driver = &backend.context.driver;
        let corpus = self
            .corpus
            .as_ref()
            .ok_or(Error::Request("corpus missing"))?;
        let mut output = slot.output.address;
        let mut answers = answers;
        let mut jobs = self.jobs as u32;
        let mut n = corpus.n as u32;
        let mut partials =
            checkpoints::partials_address(slot.output.address, slot.output.len, self.jobs);
        let (mut lo, mut hi, mut tail, mut seal_fault) = (lo, hi, tail, seal_fault);
        let mut params = [
            (&raw mut output).cast::<c_void>(),
            (&raw mut answers).cast(),
            (&raw mut jobs).cast(),
            (&raw mut n).cast(),
            (&raw mut lo).cast(),
            (&raw mut hi).cast(),
            (&raw mut partials).cast(),
        ];
        check("boundary output check", unsafe {
            (driver.launch)(
                backend.check_function,
                jobs,
                1,
                1,
                128,
                1,
                1,
                0,
                slot.stream,
                params.as_mut_ptr(),
                ptr::null_mut(),
            )
        })?;
        let mut seal = [
            (&raw mut output).cast::<c_void>(),
            (&raw mut lo).cast(),
            (&raw mut tail).cast(),
            (&raw mut partials).cast(),
            (&raw mut jobs).cast(),
            (&raw mut seal_fault).cast(),
        ];
        check("boundary seal", unsafe {
            (driver.launch)(
                backend.seal_function,
                1,
                1,
                1,
                256,
                1,
                1,
                0,
                slot.stream,
                seal.as_mut_ptr(),
                ptr::null_mut(),
            )
        })
    }

    #[allow(
        clippy::too_many_lines,
        clippy::cast_precision_loss,
        clippy::similar_names
    )]
    pub(crate) fn run(
        &mut self,
        backend: &CudaBackend,
        seed: u64,
        inject: bool,
        stop: &dyn Fn() -> bool,
        batches: Option<u64>,
        lane_slice: Option<Duration>,
    ) -> Result<ComputeEvidence> {
        let started = Instant::now();
        let start_seconds = backend.epoch.elapsed().as_secs_f64();
        backend.ready()?;
        self.burst_attribution = None;
        let prepare_started = Instant::now();
        let staged_before = self.staged_installs;
        let prepared = self.prepare(backend, seed)?;
        let staged_installs = self.staged_installs - staged_before;
        self.stage_next();
        let reused = prepared.reused;
        // Everything below refers to the corpus actually in use, not the run's base seed.
        let lane_seed = seed;
        let seed = prepared.seed;
        let hash_policy = self.config.hash_policy;
        let prepare_wall = if reused {
            0.0
        } else {
            prepare_started.elapsed().as_secs_f64()
        };
        let corpus = Arc::clone(
            self.corpus
                .as_ref()
                .ok_or(Error::Request("corpus missing"))?,
        );
        // Stream events share one device clock; stamp a common origin while idle.
        let syncing = Instant::now();
        check("async origin", unsafe {
            (backend.context.driver.event_record)(self.origin, backend.stream)
        })?;
        wait_stream(backend, "async origin")?;
        let origin_sync_seconds = syncing.elapsed().as_secs_f64();
        let origin = self.origin;
        let setting_up = Instant::now();
        if let Some(lane) = self.lane.as_mut() {
            lane.bind(backend)?;
        }
        if self.config.sweep.enabled && self.sweeper.is_none() {
            // The half the lane does not hold (validation guarantees the lane is on).
            let half = self.lane.as_ref().and_then(Lane::half).unwrap_or(0);
            let zones = pool_halves(backend)[1 - half].clone();
            let mut sweeper = Sweeper::new(backend, self.config.sweep, zones, seed)?;
            if let Some(fault) = self.pending_sweep_fault.take() {
                sweeper.inject(fault);
            }
            self.sweeper = Some(sweeper);
            self.last_swap = Instant::now();
        }
        let setup_seconds = setting_up.elapsed().as_secs_f64();
        let loop_started = Instant::now();
        let prepare_seconds = loop_started.duration_since(started).as_secs_f64();
        let output_bytes = (corpus.n * corpus.n + 4) * self.jobs * 4;
        // The byte cap is the host queue: one pinned slot per host slot.
        let queue_cap = output_bytes as u64 * self.config.host_slots as u64;
        let mut ledger = Ledger::new(
            self.config.device_slots,
            self.config.host_slots,
            queue_cap,
            self.config.quarantine_slots,
        )
        .map_err(Error::Request)?;
        let mut compare_core = 0.0;
        let mut hash_core = 0.0;
        let mut wait_total = 0.0;
        let mut wait_max = 0.0_f64;
        let mut kernel_event = 0.0;
        let mut d2h_event = 0.0;
        let mut intervals: Vec<(f64, f64)> = Vec::new();
        let mut mismatch_words = 0_u64;
        let mut mismatch_samples: Vec<MismatchSample> = Vec::new();
        let mut limit_min = usize::MAX;
        let mut limit_max = 0_usize;
        let mut oldest_peak = 0_u64;
        let mut last_hash = String::new();
        let mut last_hash_generation: Option<u64> = None;
        let mut last_generation = 0;
        let mut last_passing: Option<(u64, usize)> = None;
        let mut slot_generation: Vec<Option<u64>> = vec![None; self.config.host_slots];
        let mut failure_hashes: Vec<FailureHash> = Vec::new();
        let mut hashes_computed = 0_u64;
        let mut failure: Option<String> = None;
        let mut interrupted = false;
        let mut drain_started = None;
        let start_generation = self.next_generation;
        // A lane slice runs the lane alone: no compute batch, only a deadline.
        let target = if lane_slice.is_some() {
            start_generation
        } else {
            start_generation
                .checked_add(
                    batches
                        .unwrap_or(self.config.batches)
                        .clamp(1, self.config.batches),
                )
                .ok_or(Error::Request("batch identity overflow"))?
        };
        let lane_deadline = lane_slice.map(|d| Instant::now() + d);
        let mut lane = self.lane.take();
        let mut sweeper = self.sweeper.take();
        let mut swap_pending = self.swap_pending;
        let mut last_swap = self.last_swap;
        let mut lane_actions: Vec<Action> = Vec::new();
        let queue: WorkQueue<Job<'_>> = WorkQueue::new(
            self.config.lane.compute_weight,
            self.config.lane.lane_weight,
            output_bytes as u64,
        );
        let mut controller = self.controller.clone();
        let workers = self.scaler.current.clamp(1, self.config.host_slots);
        let (mut fill_sum, mut fill_samples, mut fill_peak) = (0.0_f64, 0_u64, 0.0_f64);
        let burst_start_ms = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        controller.begin_burst(burst_start_ms);
        let verified_before = self.verified_total;
        let epoch = self.epoch;
        let mut next_generation = start_generation;
        // Boundary checks (F3): what each failed batch showed, and the one-shot injections.
        let checks_on = self.checks.is_some();
        let mut burst_counts = BoundaryCounts::default();
        let mut disagreements = 0_u64;
        let mut seal_fault_pending = self
            .checks
            .as_mut()
            .is_some_and(|c| c.fire(BoundaryFault::AfterCheck));
        let mut host_fault_pending = self
            .checks
            .as_mut()
            .is_some_and(|c| c.fire(BoundaryFault::HostBuffer));
        // Scoped workers borrow pinned bytes. The owner cannot free them before scope joins.
        // Slot reuse is additionally guarded by the verifier completion ledger.
        thread::scope(|scope| {
            let (done_tx, done_rx) = mpsc::channel::<Done>();
            for worker in 0..workers {
                let done = done_tx.clone();
                let corpus = Arc::clone(&corpus);
                let queue = &queue;
                let spawn = thread::Builder::new()
                    .name(format!("gpu-verify-{worker}"))
                    .spawn_scoped(scope, move || {
                        while let Some(job) = queue.pop() {
                            let result = match job {
                                Job::Verify {
                                    batch,
                                    bytes,
                                    ready_at,
                                    hash,
                                    gpu_check,
                                } => Done::Verdict(verify_batch(
                                    &corpus, batch, bytes, ready_at, hash, gpu_check,
                                )),
                                Job::Lane { action, ready_at } => run_lane_action(action, ready_at),
                            };
                            if done.send(result).is_err() {
                                break;
                            }
                        }
                    });
                if let Err(error) = spawn {
                    failure = Some(format!("verifier spawn: {error}"));
                    break;
                }
            }
            drop(done_tx);
            loop {
                let now_ms = u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
                oldest_peak = oldest_peak.max(ledger.oldest_ms(now_ms));
                // A quarantine hold never outlives its deadline.
                let _ = ledger.expire_quarantine(now_ms, self.config.quarantine_max_age_ms);
                while let Ok(done) = done_rx.try_recv() {
                    let verdict = match done {
                        Done::Verdict(verdict) => verdict,
                        other => {
                            let handled = match (lane.as_mut(), other) {
                                (
                                    Some(lane),
                                    Done::Prepared {
                                        slot,
                                        seconds,
                                        wait,
                                    },
                                ) => lane.on_prepared(backend, slot, seconds, wait),
                                (
                                    Some(lane),
                                    Done::Compared {
                                        slot,
                                        mismatch,
                                        evidence,
                                        seconds,
                                        wait,
                                    },
                                ) => lane
                                    .on_compared(backend, slot, mismatch, evidence, seconds, wait),
                                _ => Err(Error::Request("lane worker failed")),
                            };
                            if let Err(error) = handled {
                                backend.context.stopped.set(true);
                                failure.get_or_insert_with(|| error.to_string());
                            }
                            continue;
                        }
                    };
                    compare_core += verdict.compare_seconds;
                    hash_core += verdict.hash_seconds;
                    wait_total += verdict.queue_wait_seconds;
                    wait_max = wait_max.max(verdict.queue_wait_seconds);
                    let batch = verdict.batch;
                    if let Some(mut kind) = verdict.boundary {
                        // Right on the GPU, wrong on the host: a host cell that cannot hold the
                        // right word is RAM, not the link (the slot is idle until finish()).
                        if kind == Kind::D2h
                            && let Some(&(index, expected, _)) =
                                verdict.mismatch.as_ref().and_then(|m| m.samples.first())
                            && !checkpoints::host_holds(
                                &self.hosts[batch.slot].readback,
                                index,
                                expected,
                            )
                        {
                            kind = Kind::HostRam;
                        }
                        burst_counts.add_kind(kind);
                    }
                    disagreements += u64::from(verdict.disagreement);
                    if let Err(error) = ledger.finish(
                        &batch,
                        verdict.result.is_ok(),
                        corpus.integer_operations(),
                        now_ms,
                    ) {
                        failure.get_or_insert_with(|| error.into());
                    }
                    if let Some(summary) = verdict.mismatch {
                        mismatch_words += summary.words;
                        for (index, expected, actual) in summary.samples {
                            if mismatch_samples.len() < MAX_MISMATCH_SAMPLES {
                                mismatch_samples.push(MismatchSample {
                                    generation: batch.generation,
                                    slot: batch.slot,
                                    index,
                                    expected,
                                    actual,
                                });
                            }
                        }
                    }
                    if let Some(hash) = verdict.failure_hash {
                        hashes_computed += 1;
                        if failure_hashes.len() < MAX_MISMATCH_SAMPLES {
                            failure_hashes.push(FailureHash {
                                generation: batch.generation,
                                actual_sha256: hash,
                            });
                        }
                    }
                    match verdict.result {
                        Ok(hash) => {
                            if last_passing.is_none_or(|(g, _)| batch.generation >= g) {
                                last_passing = Some((batch.generation, batch.slot));
                                last_generation = batch.generation;
                            }
                            if let Some(hash) = hash {
                                hashes_computed += 1;
                                if last_hash_generation.is_none_or(|g| batch.generation >= g) {
                                    last_hash_generation = Some(batch.generation);
                                    last_hash = hash;
                                }
                            }
                        }
                        Err(error) => {
                            failure.get_or_insert(error);
                        }
                    }
                }
                if stop() {
                    interrupted = true;
                }
                let lane_time_left = lane.is_some()
                    && lane_deadline.map_or(next_generation < target, |t| Instant::now() < t);
                if failure.is_some()
                    || interrupted
                    || (next_generation >= target && !lane_time_left)
                {
                    drain_started.get_or_insert_with(Instant::now);
                }
                if drain_started.is_some_and(|t| t.elapsed() >= Duration::from_secs(30)) {
                    failure.get_or_insert_with(|| "bounded drain timeout".into());
                    break;
                }
                // Computed once per tick, ahead of the lane: the lane throttles its own new
                // work off the same backlog signal the controller acts on (GPU-H6).
                let backlog = ledger.host_backlog();
                let fill = waiting_fill(backlog, workers, self.config.host_slots);
                for pending in ledger.pending() {
                    if !pending.stage.on_device() {
                        continue;
                    }
                    let slot = &self.devices[pending.batch.device_slot];
                    let host = &self.hosts[pending.batch.slot];
                    if pending.stage == Stage::GpuWriting
                        && unsafe { (backend.context.driver.event_query)(slot.events[1]) } == 0
                    {
                        let _ = ledger.copying(&pending.batch);
                    }
                    let status = unsafe { (backend.context.driver.event_query)(slot.events[2]) };
                    if status == 600 {
                        continue;
                    }
                    if status != 0 {
                        failure.get_or_insert_with(|| format!("CUDA event query: {status}"));
                        backend.context.stopped.set(true);
                        break;
                    }
                    slot.in_flight.set(false);
                    let mut timing = [0_f32; 2];
                    for (i, value) in timing.iter_mut().enumerate() {
                        if let Err(error) = check("async event timing", unsafe {
                            (backend.context.driver.event_elapsed)(
                                value,
                                slot.events[i],
                                slot.events[i + 1],
                            )
                        }) {
                            failure.get_or_insert_with(|| error.to_string());
                        }
                    }
                    kernel_event += f64::from(timing[0]) / 1000.0;
                    d2h_event += f64::from(timing[1]) / 1000.0;
                    // Best effort: a missing stamp only drops this batch from the busy union.
                    let mut stamps = [f32::NAN; 3];
                    for (i, stamp) in stamps.iter_mut().enumerate() {
                        let _ = unsafe {
                            (backend.context.driver.event_elapsed)(stamp, origin, slot.events[i])
                        };
                    }
                    intervals.push((f64::from(stamps[0]) / 1000.0, f64::from(stamps[2]) / 1000.0));
                    if let Err(error) = ledger
                        .ready(&pending.batch)
                        .and_then(|()| ledger.verifying(&pending.batch))
                    {
                        failure.get_or_insert_with(|| error.into());
                        break;
                    }
                    if host_fault_pending {
                        // Injection: one payload byte changes on the host after the copy landed.
                        host_fault_pending = false;
                        // SAFETY: the copy has landed and no worker has the slot yet.
                        unsafe { *host.readback.address.cast::<u8>().add(8) ^= 0x10 };
                    }
                    // SAFETY: event above completed D2H; no submission may reuse this slot
                    // until finish() consumes the verifier response. Scope keeps allocation alive.
                    let (bytes, gpu_check) = if checks_on {
                        let all = unsafe {
                            host.readback
                                .completed_bytes(checkpoints::readback_len(output_bytes))
                        };
                        (
                            &all[..output_bytes],
                            GpuCheck::On(checkpoints::read_tail(
                                all,
                                output_bytes,
                                pending.batch.generation,
                            )),
                        )
                    } else {
                        (
                            unsafe { host.readback.completed_bytes(output_bytes) },
                            GpuCheck::Off,
                        )
                    };
                    queue.push(
                        workers::COMPUTE,
                        output_bytes as u64,
                        Job::Verify {
                            batch: pending.batch.clone(),
                            bytes,
                            ready_at: Instant::now(),
                            hash: hash_policy.should_hash(pending.batch.generation),
                            gpu_check,
                        },
                    );
                }
                if let Some(lane) = lane.as_mut() {
                    let allow = failure.is_none()
                        && !interrupted
                        && lane_time_left
                        && !swap_pending
                        && !matches!(
                            controller.state,
                            ControlState::Draining | ControlState::Stopped
                        )
                        && !backend.submission_stopped();
                    if let Err(error) = lane.poll(
                        backend,
                        origin,
                        now_ms,
                        allow,
                        lane_seed,
                        fill,
                        &mut lane_actions,
                    ) {
                        backend.context.stopped.set(true);
                        failure.get_or_insert_with(|| error.to_string());
                    }
                    for action in lane_actions.drain(..) {
                        queue.push(
                            workers::LANE,
                            action.cost(),
                            Job::Lane {
                                action,
                                ready_at: Instant::now(),
                            },
                        );
                    }
                }
                if let Some(sweeper) = sweeper.as_mut() {
                    // A pending trade lets the running pass finish but starts no new one.
                    let held_for_trade = swap_pending && sweeper.plan_at_boundary();
                    let allow = failure.is_none()
                        && !interrupted
                        && !matches!(
                            controller.state,
                            ControlState::Draining | ControlState::Stopped
                        )
                        && !backend.submission_stopped()
                        && !held_for_trade;
                    if let Err(error) = sweeper.poll(backend, allow) {
                        backend.context.stopped.set(true);
                        failure.get_or_insert_with(|| error.to_string());
                    }
                    if let Some(lane) = lane.as_mut() {
                        swap_pending |= lane.current_sweeps() >= 1
                            && sweeper.passes_completed() >= 1
                            && last_swap.elapsed() >= sweeper.swap_interval();
                        if swap_pending
                            && failure.is_none()
                            && lane.busy() == 0
                            && sweeper.at_boundary()
                        {
                            let traded = lane.swap_half(backend, origin).and_then(|()| {
                                let half = lane.half().unwrap_or(0);
                                sweeper.rebind(pool_halves(backend)[1 - half].clone())
                            });
                            match traded {
                                Ok(()) => {
                                    swap_pending = false;
                                    last_swap = Instant::now();
                                }
                                Err(error) => {
                                    backend.context.stopped.set(true);
                                    failure.get_or_insert_with(|| error.to_string());
                                }
                            }
                        }
                    }
                }
                fill_sum += fill;
                fill_samples += 1;
                fill_peak = fill_peak.max(fill);
                controller.observe(&Sample {
                    now_ms,
                    host_backlog: backlog,
                    workers,
                    host_used: ledger.host_used(),
                    device_in_flight: ledger.device_in_flight(),
                    verdicts: verified_before + ledger.totals.verified + ledger.totals.failed,
                    submits: self.submitted_total + ledger.totals.submitted,
                    pressure: self.pressure.borrow_mut().check(),
                });
                if controller.state == ControlState::Stopped {
                    failure.get_or_insert_with(|| controller.reason.to_owned());
                }
                if backend.submission_stopped() {
                    break;
                }
                if failure.is_none()
                    && !interrupted
                    && next_generation < target
                    && controller.permits(ledger.device_in_flight(), now_ms)
                    && let Some((device_slot, slot)) = ledger.free_slots()
                {
                    let batch = Batch {
                        version: 1,
                        device: backend.info.uuid.clone(),
                        corpus: corpus.sha256.clone(),
                        seed,
                        generation: next_generation,
                        jobs: self.jobs,
                        inner: self.inner,
                        slot,
                        epoch: 0,
                        device_slot,
                        device_epoch: 0,
                    };
                    match ledger.reserve(batch, output_bytes as u64, now_ms) {
                        Ok(batch) => {
                            let fault = u32::from(inject && next_generation == start_generation);
                            let seal_fault = u32::from(std::mem::take(&mut seal_fault_pending));
                            next_generation += 1;
                            limit_min = limit_min.min(controller.in_flight_limit);
                            limit_max = limit_max.max(controller.in_flight_limit);
                            slot_generation[batch.slot] = Some(batch.generation);
                            if let Err(error) =
                                self.launch(backend, &batch, output_bytes, fault, seal_fault)
                            {
                                backend.context.stopped.set(true);
                                failure.get_or_insert_with(|| error.to_string());
                            }
                        }
                        Err(error) => {
                            failure.get_or_insert_with(|| error.into());
                        }
                    }
                }
                let lane_time_left = lane.is_some()
                    && lane_deadline.map_or(next_generation < target, |t| Instant::now() < t);
                if ledger.count() == 0
                    && lane.as_ref().is_none_or(|l| l.busy() == 0)
                    && (failure.is_some()
                        || interrupted
                        || (next_generation >= target && !lane_time_left))
                {
                    break;
                }
                thread::sleep(Duration::from_micros(50));
            }
            // Workers do bounded in-memory comparison only, no driver calls or external waits.
            queue.close();
        });
        let loop_ended = Instant::now();
        let loop_seconds = loop_ended.duration_since(loop_started).as_secs_f64();
        let served = queue.stats();
        drop(queue);
        self.fairness.add(&served);
        if let Some(lane) = lane.as_mut() {
            lane.burst_finished(&intervals);
        }
        // A lane operation still on the device when the burst gave up leaves the context stopped
        // (its slot marks that when dropped); the lane goes back either way.
        let lane_report = lane.as_ref().map(Lane::report);
        self.lane = lane;
        // The sweep's launch in flight finishes before the burst's accounting closes.
        if let Some(sweeper) = sweeper.as_mut()
            && let Err(error) = sweeper.drain()
        {
            self.first_error.get_or_insert_with(|| error.to_string());
        }
        let sweep_report = sweeper.as_ref().map(Sweeper::report);
        self.sweeper = sweeper;
        self.swap_pending = swap_pending;
        self.last_swap = last_swap;
        // Under a checkpoint policy the last passing output of the burst is hashed once, here,
        // from its slot, provided nothing has reused that slot since.
        if let Some((generation, slot)) = last_passing
            && last_hash_generation != Some(generation)
            && slot_generation[slot] == Some(generation)
            && !ledger.pending().iter().any(|p| p.batch.slot == slot)
            && !backend.submission_stopped()
        {
            let started = Instant::now();
            // SAFETY: the batch was verified and its D2H finished; the slot is idle and the
            // verifier scope has joined, so nothing else touches the buffer.
            last_hash =
                sha256_hex(unsafe { self.hosts[slot].readback.completed_bytes(output_bytes) });
            hash_core += started.elapsed().as_secs_f64();
            hashes_computed += 1;
        }
        self.next_generation = next_generation;
        self.batches_on_corpus += ledger.totals.submitted;
        self.submitted_total += ledger.totals.submitted;
        self.verified_total += ledger.totals.verified + ledger.totals.failed;
        self.controller = controller.clone();
        if self.devices.iter().any(|s| s.in_flight.get()) {
            backend.context.stopped.set(true);
        }
        // Boundary checks: after a batch the GPU found wrong (or disputed), check whether the
        // input or the answers changed in VRAM since their upload check.
        if let Some(checks) = self.checks.as_mut() {
            if (burst_counts.device > 0 || disagreements > 0) && !backend.submission_stopped() {
                match checks.changed_at_rest(backend, &self.input) {
                    Ok((input_changed, answers_changed)) => {
                        if input_changed {
                            burst_counts.vram_input += burst_counts.device;
                            burst_counts.device = 0;
                        }
                        if answers_changed {
                            burst_counts.vram_answers += 1;
                            // The GPU's verdicts rested on wrong answers: they place nothing.
                            if !input_changed {
                                burst_counts.device = 0;
                            }
                        }
                        self.reupload |= input_changed || answers_changed;
                    }
                    Err(error) => {
                        failure.get_or_insert_with(|| error.to_string());
                    }
                }
            }
            checks.report.batches_checked += ledger.totals.submitted;
            checks.report.device_disagreements += disagreements;
            checks.report.counts.add(&burst_counts);
            if burst_counts.vram_answers > 0 && ledger.totals.failed == 0 {
                failure.get_or_insert_with(|| {
                    "answers held in VRAM changed after their upload check".into()
                });
            }
        }
        // Prove the host copy of the corpus unchanged after any failed batch (here, before the
        // burst's verdict) and periodically: damaged host memory would otherwise be reported as a
        // GPU fault. C13 G3: the periodic check runs on a helper thread beside the next bursts and
        // reports at a later burst boundary (or in `finish`); with an external corpus, replaced
        // every burst, it stays here.
        let (mut integrity_checks, mut integrity_failures, mut integrity_seconds) = (0, 0, 0.0);
        let mut integrity_background_seconds = 0.0;
        if let Some((outcome, seconds)) = self.take_integrity(false) {
            integrity_checks += 1;
            integrity_background_seconds = seconds;
            if let Err(reason) = outcome {
                integrity_failures += 1;
                self.host_memory_changed(&mut failure, reason);
            }
        }
        let every = self.config.integrity_every_batches;
        self.batches_since_integrity += ledger.totals.submitted;
        let periodic = every > 0 && self.batches_since_integrity >= every;
        let failed = ledger.totals.failed > 0;
        if ledger.totals.submitted > 0 && (failed || periodic) && !backend.submission_stopped() {
            let background = !failed && !self.external;
            if background && self.integrity_job.is_some() {
                // The last one still runs: this one starts at a later burst boundary.
            } else if background && self.start_integrity(&corpus) {
                self.batches_since_integrity = 0;
            } else {
                self.batches_since_integrity = 0;
                let checked = Instant::now();
                integrity_checks += 1;
                match corpus.integrity_check() {
                    Ok(()) => {
                        if failed {
                            self.burst_attribution = Some("corpus_intact");
                            self.failure_attribution
                                .get_or_insert_with(|| "corpus_intact".into());
                        }
                    }
                    Err(reason) => {
                        integrity_failures += 1;
                        self.host_memory_changed(&mut failure, reason);
                    }
                }
                integrity_seconds = checked.elapsed().as_secs_f64();
            }
        }
        // Confirmation (the host-corpus check above) is done: hold nothing past the burst.
        for held in ledger
            .pending()
            .into_iter()
            .filter(|p| p.stage == Stage::Quarantined)
        {
            if ledger.release_quarantine(&held.batch).is_err() {
                failure.get_or_insert_with(|| "quarantine release failed".into());
            }
        }
        let end_seconds = backend.epoch.elapsed().as_secs_f64();
        let after_loop_seconds = loop_ended.elapsed().as_secs_f64();
        let busy_wall = interval_union(&mut intervals);
        let burst_wall = loop_started.elapsed().as_secs_f64();
        #[allow(clippy::cast_precision_loss)]
        let backlog_fill_mean = if fill_samples == 0 {
            0.0
        } else {
            fill_sum / fill_samples as f64
        };
        let backlog_throttled = controller.is_holding_load()
            || controller.decisions().iter().any(|d| {
                d.at_ms >= burst_start_ms
                    && matches!(
                        d.reason,
                        "verifier_backlog" | "backlog_rising" | "queue_hard_cap"
                    )
            });
        let regime = classify_regime(&RegimeInput {
            backlog_fill_mean,
            backlog_throttled,
            device_busy_fraction: if burst_wall > 0.0 {
                busy_wall / burst_wall
            } else {
                0.0
            },
            d2h_over_kernel: if kernel_event > 0.0 {
                d2h_event / kernel_event
            } else {
                0.0
            },
        });
        let burst = BurstSummary {
            totals: ledger.totals.clone(),
            timing: Timing {
                oracle_seconds: if prepared.fresh {
                    corpus.oracle_seconds
                } else {
                    0.0
                },
                input_generation_seconds: if prepared.fresh {
                    corpus.generation_seconds
                } else {
                    0.0
                },
                corpus_prepare_wall_seconds: prepare_wall,
                verify_compare_core_seconds: compare_core,
                verify_hash_core_seconds: hash_core,
                queue_wait_total_seconds: wait_total,
                queue_wait_max_seconds: wait_max,
                device_kernel_event_seconds: kernel_event,
                device_d2h_event_seconds: d2h_event,
                device_busy_wall_seconds: busy_wall,
                burst_wall_seconds: burst_wall,
                corpus_wait_seconds: prepared.wait_seconds,
                integrity_check_seconds: integrity_seconds,
                integrity_background_seconds,
                prepare_seconds,
                corpus_install_seconds: prepared.install_seconds,
                origin_sync_seconds,
                setup_seconds,
                loop_seconds,
                after_loop_seconds,
            },
            mismatch_words,
            mismatch_samples,
            failure_hashes,
            corpus_rotations: u64::from(prepared.rotated),
            corpus_prefetch_hits: u64::from(prepared.prefetch_hit),
            corpus_cold_builds: u64::from(prepared.fresh && !prepared.prefetch_hit),
            corpus_pool_serves: u64::from(prepared.pool_served),
            corpus_staged_installs: staged_installs,
            device_in_flight_peak: ledger.peak_device_in_flight(),
            host_slots_used_peak: ledger.peak_host_used(),
            hashes_computed,
            integrity_checks,
            integrity_failures,
            pending_at_end: ledger.count() as u64,
            in_flight_limit_min: if limit_max == 0 { 0 } else { limit_min },
            in_flight_limit_max: limit_max,
            oldest_pending_ms_peak: oldest_peak,
            regime,
            backlog_fill_mean,
            backlog_fill_peak: fill_peak,
            workers,
        };
        if let Err(error) = ledger.check_conservation() {
            failure.get_or_insert_with(|| format!("ledger conservation: {error}"));
        }
        if burst.totals.failed > 0 {
            let detail = format!(
                "burst: {} failed batches, {} mismatching words",
                burst.totals.failed, burst.mismatch_words
            );
            failure = Some(failure.map_or(detail.clone(), |e| format!("{e} ({detail})")));
        }
        let tag = burst_counts.tag();
        if !tag.is_empty() {
            failure = failure.map(|e| format!("{e} {tag}"));
        }
        self.cumulative.add_burst(burst.clone());
        self.shape.window.observe(regime);
        let cells = burst.totals.verified * (corpus.jobs * corpus.n * corpus.n) as u64;
        // Each cell is 2K operations, K of this burst's corpus (the adaptive shape changes K).
        let operations = cells.saturating_mul(2 * corpus.k as u64);
        let c = &mut self.cumulative;
        c.inner_now = u32::try_from(corpus.k).unwrap_or(u32::MAX);
        match corpus.arith {
            Arith::U32 => {
                c.u32_results_verified += cells;
                c.u32_ops_verified += operations;
            }
            Arith::F32 => {
                c.f32_results_verified += cells;
                c.f32_flops_verified += operations;
            }
            Arith::I8 => {
                c.i8_results_verified += cells;
                c.i8_ops_verified += operations;
            }
        }
        // The next burst may use more verifier workers, but only while verifiers are the limit
        // and only if the last extra worker measurably helped.
        let workers_before = self.scaler.current;
        self.scaler.observe(&ScalerObservation {
            batches: burst.totals.submitted,
            verified_bytes: burst.totals.verified_bytes,
            wall_seconds: burst_wall,
            verifier_bound: regime == Regime::VerifierBound,
        });
        if self.scaler.current > workers_before {
            self.controller
                .capacity_increased(workers_before, self.scaler.current);
        }
        if let Err(error) = self.cumulative.check_conservation() {
            failure.get_or_insert_with(|| format!("cumulative conservation: {error}"));
        }
        if let Some(error) = &failure {
            self.first_error.get_or_insert_with(|| error.clone());
        }
        self.interrupted = interrupted;
        let last = replay_of(
            seed,
            next_generation.saturating_sub(1),
            self.jobs,
            self.inner,
        );
        backend.campaign.borrow_mut().record_async_burst(
            start_seconds,
            end_seconds,
            &AsyncBurstAccounting {
                submitted: burst.totals.submitted,
                completed: burst.totals.completed,
                verified: burst.totals.verified,
                failed: burst.totals.failed,
                prepare_seconds: prepare_wall,
                compare_core_seconds: compare_core + hash_core,
                gpu_kernel_seconds: kernel_event,
                gpu_d2h_seconds: d2h_event,
                gpu_busy_wall_seconds: burst.timing.device_busy_wall_seconds,
            },
            last,
            burst
                .totals
                .first_failure
                .as_ref()
                .map(|b| replay_of(b.seed, b.generation, b.jobs, b.inner)),
        );
        let report = AsyncReport {
            version: 2,
            backend: "CUDA_ASYNC_PINNED".into(),
            config: self.config,
            rates: self.cumulative.rates(),
            conservation: self
                .cumulative
                .check_conservation()
                .err()
                .unwrap_or_else(|| "ok".into()),
            cumulative: self.cumulative.clone(),
            pending: ledger.pending(),
            controller,
            workers: self.scaler.clone(),
            memory_pressure_samples: self.pressure.borrow().samples,
            memory_pressure_hits: self.pressure.borrow().pressured_samples,
            queue_cap_bytes: queue_cap,
            pinned_bytes: (self.upload.len
                + self.hosts.iter().map(|s| s.readback.len).sum::<usize>())
                as u64
                + self.lane.as_ref().map_or(0, Lane::pinned_bytes)
                + self.sweeper.as_ref().map_or(0, Sweeper::pinned_bytes)
                + self.checks.as_ref().map_or(0, Checkpoints::pinned_bytes),
            host_queue_bytes: self.hosts.iter().map(|s| s.readback.len as u64).sum(),
            device_output_bytes: self.devices.iter().map(|s| s.output.len as u64).sum(),
            oldest_pending_ms: ledger
                .oldest_ms(u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX)),
            interrupted,
            error: self.first_error.clone(),
            failure_attribution: self.failure_attribution.clone(),
            result_hash_policy: hash_policy.label(),
            lane: lane_report,
            sweep: sweep_report,
            fairness: self.fairness,
            boundary: self.checks.as_ref().map(|c| c.report),
        };
        // An empty burst (deadline reached before any submit) must not look like work.
        *backend.profile.borrow_mut() = (!burst.is_empty()).then(|| ComputeProfile {
            version: 1,
            seed,
            generation: last_generation,
            workload: "GPU-H-CUDA-BURST-1".into(),
            corpus_sha256: corpus.sha256.clone(),
            corpus_reused: reused,
            input_resident: reused,
            matrix_dimension: corpus.n,
            integer_operations: burst.totals.verified_integer_ops,
            input_generation_seconds: burst.timing.input_generation_seconds,
            cpu_oracle_seconds: burst.timing.oracle_seconds,
            buffers_reused: true,
            pipeline_reused: true,
            persistent_device_buffer_bytes: (self.input.len
                + self.devices.iter().map(|s| s.output.len).sum::<usize>())
                as u64
                + self.checks.as_ref().map_or(0, Checkpoints::device_bytes),
            persistent_host_buffer_bytes: report.pinned_bytes,
            host_upload_seconds: prepare_wall,
            submit_wait_seconds: burst.timing.burst_wall_seconds,
            cpu_compare_seconds: compare_core,
            result_hash_seconds: hash_core,
            end_to_end_seconds: started.elapsed().as_secs_f64(),
            dispatches_submitted: burst.totals.submitted,
            dispatches_completed: burst.totals.completed,
            dispatches_verified: burst.totals.verified,
            gpu: Some(DeviceIntervals {
                h2d_seconds: 0.0,
                kernel_seconds: kernel_event,
                d2h_seconds: d2h_event,
            }),
            ..ComputeProfile::default()
        });
        let profile = backend.profile.borrow().clone();
        *backend.balanced_report.borrow_mut() = Some(report);
        if let Some(error) = failure {
            return Err(Error::Unavailable(error));
        }
        if ledger.count() != 0 {
            return Err(Error::Request("unverified async batches"));
        }
        Ok(ComputeEvidence {
            jobs: self.jobs,
            rounds: self.inner,
            results_verified: burst.totals.verified * (self.jobs * corpus.n * corpus.n) as u64,
            uploaded_bytes: if reused {
                0
            } else {
                (corpus.input.len() * 4) as u64
            },
            downloaded_bytes: burst.totals.readback_bytes,
            elapsed_seconds: started.elapsed().as_secs_f64(),
            shader_sha256: crate::KERNEL_SHA256.into(),
            result_sha256: last_hash,
            profile,
        })
    }
}

impl CudaBackend {
    /// Configure before resource preparation. Sequential remains available for A/B.
    /// # Errors
    /// Rejects invalid limits or reconfiguration of an initialized pool.
    pub fn configure_async(&mut self, config: AsyncConfig) -> Result<()> {
        config.validate()?;
        if self.balanced.borrow().is_some() {
            return Err(Error::Request("async already prepared"));
        }
        self.async_config = Some(config);
        Ok(())
    }
    #[must_use]
    pub fn async_report(&self) -> Option<AsyncReport> {
        self.balanced_report.borrow().clone()
    }
    /// Loads a corpus built elsewhere (e.g. read back from storage) for the next bursts, which
    /// then run on it as is (no rotation, no prefetch) until another one is loaded.
    /// # Errors
    /// Rejects a corpus whose shape does not match the prepared resources, INT8 without tensor
    /// cores, or a failed upload.
    pub fn load_corpus(&self, corpus: Corpus) -> Result<()> {
        self.balanced
            .borrow_mut()
            .as_mut()
            .ok_or(Error::Request("async resources not prepared"))?
            .load_external(self, corpus)
    }
    /// Host-corpus check of the last burst that failed a batch: `corpus_intact` (the host copy
    /// still matches its hashes) or `host_memory_changed`; `None` when the last burst passed.
    #[must_use]
    pub fn last_burst_attribution(&self) -> Option<&'static str> {
        self.balanced
            .borrow()
            .as_ref()
            .and_then(Pipeline::burst_attribution)
    }
    /// Submit a bounded burst while CPU workers verify completed pinned outputs.
    /// # Errors
    /// Reports preparation, device, verifier or full-comparison failure.
    pub fn verify_matrix_async(
        &self,
        seed: u64,
        inject: bool,
        stop: &dyn Fn() -> bool,
        batches: Option<u64>,
    ) -> Result<ComputeEvidence> {
        self.profile.borrow_mut().take();
        let mut pipeline = self.balanced.borrow_mut();
        pipeline
            .as_mut()
            .ok_or(Error::Request("async resources not prepared"))?
            .run(self, seed, inject, stop, batches, None)
    }
    /// Runs the transfer lane alone for `slice` (no compute batch): the transfer-only profile.
    /// # Errors
    /// Reports preparation or device failures; lane verification failures are in
    /// [`Self::take_lane_failures`].
    pub fn run_lane_slice(
        &self,
        seed: u64,
        stop: &dyn Fn() -> bool,
        slice: Duration,
    ) -> Result<ComputeEvidence> {
        self.profile.borrow_mut().take();
        let mut pipeline = self.balanced.borrow_mut();
        pipeline
            .as_mut()
            .ok_or(Error::Request("async resources not prepared"))?
            .run(self, seed, false, stop, None, Some(slice))
    }
    /// The lane's counters, per direction (absent when the lane is off).
    #[must_use]
    pub fn lane_report(&self) -> Option<LaneReport> {
        self.balanced
            .borrow()
            .as_ref()
            .and_then(Pipeline::lane_report)
    }
    /// GPU-DRAM sweep counters so far (between bursts), when the sweep is on.
    #[must_use]
    pub fn sweep_report(&self) -> Option<SweepReport> {
        self.balanced
            .borrow()
            .as_ref()
            .and_then(Pipeline::sweep_report)
    }
    /// Verification failures found by the lane since the last call.
    #[must_use]
    pub fn take_lane_failures(&self) -> Vec<LaneFailure> {
        self.balanced
            .borrow_mut()
            .as_mut()
            .map(Pipeline::take_lane_failures)
            .unwrap_or_default()
    }
    /// Arms a one-shot lane fault (negative gates).
    /// # Errors
    /// Fails when the lane is off.
    pub fn inject_lane_fault(&self, fault: LaneFault) -> Result<()> {
        self.balanced
            .borrow_mut()
            .as_mut()
            .ok_or(Error::Request("async resources not prepared"))?
            .inject_lane_fault(fault)
    }
    /// Verifies every region still awaiting its re-read or checkpoint. Call once, at the end.
    /// # Errors
    /// Reports driver failures; mismatches are in [`Self::take_lane_failures`].
    pub fn flush_lane(&self) -> Result<()> {
        let mut pipeline = self.balanced.borrow_mut();
        let pipeline = pipeline
            .as_mut()
            .ok_or(Error::Request("async resources not prepared"))?;
        pipeline.flush_lane(self)?;
        if let Some(report) = self.balanced_report.borrow_mut().as_mut() {
            report.lane = pipeline.lane_report();
        }
        Ok(())
    }
    /// Call once, at the end of an async run: waits for the periodic host-corpus check still on
    /// its helper thread (C13 G3) and adds it to the report.
    /// # Errors
    /// The check found the host copy of the corpus changed.
    pub fn finish_async(&self) -> Result<()> {
        let mut pipeline = self.balanced.borrow_mut();
        let Some(pipeline) = pipeline.as_mut() else {
            return Ok(());
        };
        let failure = pipeline.finish();
        if let Some(report) = self.balanced_report.borrow_mut().as_mut() {
            report.cumulative = pipeline.cumulative.clone();
            report.rates = pipeline.cumulative.rates();
            report.error.clone_from(&pipeline.first_error);
            report
                .failure_attribution
                .clone_from(&pipeline.failure_attribution);
        }
        failure.map_or(Ok(()), |error| Err(Error::Unavailable(error)))
    }
    /// Negative gate: changes one bit of the host copy of the corpus in use (not the device's).
    /// # Errors
    /// Fails before the async resources and their first corpus exist.
    pub fn inject_host_corpus_fault(&self) -> Result<()> {
        self.balanced
            .borrow_mut()
            .as_mut()
            .ok_or(Error::Request("async resources not prepared"))?
            .inject_host_corpus_fault()
    }
}

/// The pool's allocations split in halves, for the lane/sweep trade.
fn pool_halves(backend: &CudaBackend) -> [Vec<gpu_engine::lane::Zone>; 2] {
    let sizes: Vec<u64> = backend.pool.iter().map(|b| b.len as u64).collect();
    gpu_engine::sweep::halves(&sizes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lane_options_are_validated_with_the_rest_of_the_async_config_and_counted_as_allocations() {
        let off = AsyncConfig::default();
        assert!(off.validate().is_ok() && !off.lane.active());
        let on = AsyncConfig {
            lane: LaneConfig {
                enabled: true,
                vram_local: true,
                slots: 3,
                ..LaneConfig::default()
            },
            ..off
        };
        assert!(on.validate().is_ok());
        // Every lane slot adds its buffers, stream and events to the counted allocation steps.
        assert_eq!(
            on.allocation_steps(),
            off.allocation_steps() + 3 * lane::STEPS_PER_SLOT
        );
        for lane in [
            LaneConfig {
                slots: 0,
                ..on.lane
            },
            LaneConfig {
                chunk_bytes: 1,
                ..on.lane
            },
            LaneConfig {
                lane_weight: 0,
                ..on.lane
            },
        ] {
            assert!(AsyncConfig { lane, ..on }.validate().is_err());
        }
    }

    /// The GPU-local kernels and `gpu_engine::lane::vram_word` must be the same function; the
    /// hardware gate proves it on the device, this catches an edit to only one side early.
    #[test]
    fn the_vram_kernel_source_uses_the_engine_pattern_constants() {
        let source = include_str!("../kernels/gemm.cu");
        for constant in [
            "0x9E3779B1u",
            "0x85EBCA6Bu",
            "0xC2B2AE35u",
            "0x7FEB352Du",
            "0x846CA68Bu",
        ] {
            assert!(source.contains(constant), "{constant}");
        }
        assert!(source.contains("x ^= x >> 16") && source.contains("x ^= x >> 15"));
        assert!(source.contains("dit_vram_fill") && source.contains("dit_vram_verify"));
        // The sweep word must stay gpu_engine::sweep::sweep_word (multiply-free xorshift32).
        assert!(source.contains("dit_vram_sweep_fill") && source.contains("dit_vram_sweep_verify"));
        assert!(source.contains("x ^= x << 13; x ^= x >> 17; x ^= x << 5;"));
        assert!(source.contains("((hi << 16) | (hi >> 16))"));
    }
}
