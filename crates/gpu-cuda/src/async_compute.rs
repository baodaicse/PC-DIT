//! GPU-H first async adapter: bounded pinned slots, direct CPU comparison.
//! A slot is reused only after its verifier returns. CUDA stays on the owner thread.
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
            integrity_every_batches: 512,
            lane: LaneConfig::default(),
            sweep: SweepConfig::default(),
            arith: ArithMix::U32,
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
            || self.integrity_every_batches > 100_000_000
            || self.workers_max < self.workers
            || self.workers_max > 32
            || matches!(self.hash_policy, HashPolicy::Checkpoint { interval: 0 })
            || self.lane.validate().is_err()
            || self.sweep.validate().is_err()
            || (self.sweep.enabled && (!self.lane.active() || self.lane.vram_local))
            || (self.arith.alternates() && self.rotate_every_batches == 0)
        {
            return Err(Error::Request("async configuration"));
        }
        Ok(())
    }
    /// Counted allocation steps of one pipeline: input, upload staging, five per device slot
    /// (buffer, stream, three events), one per host slot and the origin event.
    #[must_use]
    pub fn allocation_steps(self) -> usize {
        let lane = if self.lane.active() {
            lane::STEPS_PER_SLOT * self.lane.slots
        } else {
            0
        };
        3 + 5 * self.device_slots + self.host_slots + lane
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
}

/// Work for a verifier worker.
enum Job<'a> {
    Verify {
        batch: Batch,
        bytes: &'a [u8],
        ready_at: Instant,
        hash: bool,
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
        mismatch: Option<(usize, u8, u8)>,
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
        } => {
            let mismatch = lane::run_compare(expected, actual);
            Done::Compared {
                slot,
                mismatch,
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
) -> Verdict {
    let started = Instant::now();
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
    Verdict {
        batch,
        result,
        failure_hash,
        compare_seconds,
        hash_seconds,
        queue_wait_seconds,
        mismatch,
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
struct DeviceSlot {
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
    jobs: usize,
    inner: u32,
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
}

/// A corpus being built on a helper thread for the next rotation.
struct Prefetch {
    seed: u64,
    arith: Arith,
    handle: thread::JoinHandle<std::result::Result<Corpus, &'static str>>,
}

/// What `prepare` did, for the burst's accounting.
struct Prepared {
    reused: bool,
    /// Seed of the corpus actually in use (derived from the run's base seed).
    seed: u64,
    rotated: bool,
    prefetch_hit: bool,
    wait_seconds: f64,
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if self.context.stopped.get() || self.context.current().is_err() || self.origin.is_null() {
            return;
        }
        unsafe {
            (self.context.driver.event_destroy)(self.origin);
        }
        LIVE_EVENTS.fetch_sub(1, AtomicOrdering::SeqCst);
    }
}

/// Waits for the owner stream to go idle; bounded, marks the context stopped on timeout.
fn wait_stream(backend: &CudaBackend, call: &'static str) -> Result<()> {
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
        // Every resource is an owner (RAII) until the pipeline exists, so a failure at any
        // step frees what was already allocated. The raw origin event is created last.
        backend.alloc_step()?;
        let input = Buffer::new(&backend.context, input)?;
        backend.alloc_step()?;
        let upload = Pinned::new(&backend.context, input.len)?;
        let devices = (0..config.device_slots)
            .map(|_| DeviceSlot::new(backend, output))
            .collect::<Result<Vec<_>>>()?;
        let hosts = (0..config.host_slots)
            .map(|_| {
                backend.alloc_step()?;
                Ok(HostSlot {
                    readback: Pinned::new(&backend.context, output)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut lane = config
            .lane
            .active()
            .then(|| Lane::new(backend, config.lane))
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
            jobs,
            inner,
            config,
            controller: LoadController::new(ControlConfig::new(
                config.device_slots,
                config.host_slots,
            )),
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
        self.corpus.take();
        let corpus = Arc::new(corpus);
        self.upload.write_u32(&corpus.input);
        check("external corpus upload", unsafe {
            (backend.context.driver.upload)(
                self.input.address,
                self.upload.address,
                corpus.input.len() * 4,
                backend.stream,
            )
        })?;
        wait_stream(backend, "external corpus upload")?;
        self.base_seed = Some(corpus.seed);
        self.batches_on_corpus = 0;
        self.corpus = Some(corpus);
        self.external = true;
        Ok(())
    }

    pub(crate) fn burst_attribution(&self) -> Option<&'static str> {
        self.burst_attribution
    }

    fn start_prefetch(&mut self, base: u64) {
        if self.external || self.config.rotate_every_batches == 0 || self.prefetch.is_some() {
            return;
        }
        let seed = derive_corpus_seed(base, self.rotation + 1);
        let arith = self
            .config
            .arith
            .for_rotation(self.rotation + 1, self.tensor);
        let (jobs, inner, threads) = (self.jobs, self.inner, self.config.threads());
        self.prefetch = thread::Builder::new()
            .name("gpu-corpus-prefetch".into())
            .spawn(move || Corpus::new_arith(arith, seed, jobs, inner, threads))
            .ok()
            .map(|handle| Prefetch {
                seed,
                arith,
                handle,
            });
    }

    /// Chooses the corpus for the next burst: reuse, a prefetched rotation, or a
    /// cold build. Only ever called with no batch outstanding.
    fn prepare(&mut self, backend: &CudaBackend, base: u64) -> Result<Prepared> {
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
            });
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
            });
        }
        let waited = Instant::now();
        let (corpus, prefetch_hit) = match self.prefetch.take() {
            Some(p) if p.seed == seed && p.arith == arith => match p.handle.join() {
                Ok(built) => (built.map_err(Error::Request)?, true),
                Err(_) => return Err(Error::Request("corpus prefetch panicked")),
            },
            _ => {
                // A cold build starts from nothing: free the old corpus first so the peak is one
                // corpus plus staging, not two.
                self.corpus.take();
                (
                    Corpus::new_arith(arith, seed, self.jobs, self.inner, self.config.threads())
                        .map_err(Error::Request)?,
                    false,
                )
            }
        };
        let wait_seconds = waited.elapsed().as_secs_f64();
        // No outstanding batches cross this boundary. Drop old corpus before replacement.
        self.corpus.take();
        let corpus = Arc::new(corpus);
        self.upload.write_u32(&corpus.input);
        check("async corpus upload", unsafe {
            (backend.context.driver.upload)(
                self.input.address,
                self.upload.address,
                corpus.input.len() * 4,
                backend.stream,
            )
        })?;
        wait_stream(backend, "corpus upload")?;
        self.corpus = Some(corpus);
        self.start_prefetch(base);
        Ok(Prepared {
            reused: false,
            seed,
            rotated,
            prefetch_hit,
            wait_seconds,
        })
    }

    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    fn launch(&self, backend: &CudaBackend, batch: &Batch, bytes: usize, fault: u32) -> Result<()> {
        let slot = &self.devices[batch.device_slot];
        let host = &self.hosts[batch.slot];
        // Mark before the first queued operation; error paths retain referenced memory.
        slot.in_flight.set(true);
        let driver = &backend.context.driver;
        check("output guards", unsafe {
            (driver.fill)(slot.output.address, 0xA55A_1973, bytes / 4, slot.stream)
        })?;
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
        let function = match arith {
            Arith::U32 => backend.function,
            Arith::F32 => backend.function_f32,
            Arith::I8 => backend.function_i8,
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
        let mut params = [
            (&raw mut input).cast::<c_void>(),
            (&raw mut output).cast(),
            (&raw mut jobs).cast(),
            (&raw mut inner).cast(),
            (&raw mut dimension).cast(),
            (&raw mut lo).cast(),
            (&raw mut hi).cast(),
            (&raw mut fault).cast(),
        ];
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
        check("compute end", unsafe {
            (driver.event_record)(slot.events[1], slot.stream)
        })?;
        check("async readback", unsafe {
            (driver.download)(
                host.readback.address,
                slot.output.address,
                bytes,
                slot.stream,
            )
        })?;
        check("readback end", unsafe {
            (driver.event_record)(slot.events[2], slot.stream)
        })?;
        Ok(())
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
        let prepared = self.prepare(backend, seed)?;
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
        check("async origin", unsafe {
            (backend.context.driver.event_record)(self.origin, backend.stream)
        })?;
        wait_stream(backend, "async origin")?;
        let origin = self.origin;
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
        let loop_started = Instant::now();
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
                                } => Done::Verdict(verify_batch(
                                    &corpus, batch, bytes, ready_at, hash,
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
                                        seconds,
                                        wait,
                                    },
                                ) => lane.on_compared(slot, mismatch, seconds, wait),
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
                    // SAFETY: event above completed D2H; no submission may reuse this slot
                    // until finish() consumes the verifier response. Scope keeps allocation alive.
                    let bytes = unsafe { host.readback.completed_bytes(output_bytes) };
                    queue.push(
                        workers::COMPUTE,
                        output_bytes as u64,
                        Job::Verify {
                            batch: pending.batch.clone(),
                            bytes,
                            ready_at: Instant::now(),
                            hash: hash_policy.should_hash(pending.batch.generation),
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
                            next_generation += 1;
                            limit_min = limit_min.min(controller.in_flight_limit);
                            limit_max = limit_max.max(controller.in_flight_limit);
                            slot_generation[batch.slot] = Some(batch.generation);
                            if let Err(error) = self.launch(backend, &batch, output_bytes, fault) {
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
        // Prove the host copy of the corpus unchanged after any failed batch and periodically:
        // damaged host memory would otherwise be reported as a GPU fault.
        let (mut integrity_checks, mut integrity_failures, mut integrity_seconds) = (0, 0, 0.0);
        let every = self.config.integrity_every_batches;
        self.batches_since_integrity += ledger.totals.submitted;
        let periodic = every > 0 && self.batches_since_integrity >= every;
        if periodic {
            self.batches_since_integrity = 0;
        }
        if ledger.totals.submitted > 0
            && (ledger.totals.failed > 0 || periodic)
            && !backend.submission_stopped()
        {
            let checked = Instant::now();
            integrity_checks = 1;
            match corpus.integrity_check() {
                Ok(()) => {
                    if ledger.totals.failed > 0 {
                        self.burst_attribution = Some("corpus_intact");
                        self.failure_attribution
                            .get_or_insert_with(|| "corpus_intact".into());
                    }
                }
                Err(reason) => {
                    integrity_failures = 1;
                    self.burst_attribution = Some("host_memory_changed");
                    self.failure_attribution
                        .get_or_insert_with(|| "host_memory_changed".into());
                    failure.get_or_insert_with(|| format!("host corpus integrity: {reason}"));
                }
            }
            integrity_seconds = checked.elapsed().as_secs_f64();
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
                oracle_seconds: if reused { 0.0 } else { corpus.oracle_seconds },
                input_generation_seconds: if reused {
                    0.0
                } else {
                    corpus.generation_seconds
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
            },
            mismatch_words,
            mismatch_samples,
            failure_hashes,
            corpus_rotations: u64::from(prepared.rotated),
            corpus_prefetch_hits: u64::from(prepared.prefetch_hit),
            corpus_cold_builds: u64::from(!reused && !prepared.prefetch_hit),
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
        self.cumulative.add_burst(burst.clone());
        let cells = burst.totals.verified * (self.jobs * corpus.n * corpus.n) as u64;
        match corpus.arith {
            Arith::U32 => self.cumulative.u32_results_verified += cells,
            Arith::F32 => self.cumulative.f32_results_verified += cells,
            Arith::I8 => self.cumulative.i8_results_verified += cells,
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
                + self.sweeper.as_ref().map_or(0, Sweeper::pinned_bytes),
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
                as u64,
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
