//! GPU-H6 transfer lane: transfer, retention and GPU-local VRAM work that runs on its own
//! streams while compute batches run, instead of between bursts.
//!
//! The lane belongs to the owner thread, like every CUDA call here. CPU work (writing the
//! expected pattern, comparing the result) goes to the verifier workers as `Action`s and comes
//! back as completions; nothing on a worker touches the driver. Each slot runs one operation
//! at a time through `Idle -> Preparing -> Copying -> Comparing -> Idle`, and the planner keeps
//! two operations from touching the same bytes at once.
use crate::{
    CudaBackend, Error, Result,
    checkpoints::{self, BoundaryCounts, BoundaryFault, Kind as Boundary, LaneEvidence},
    driver::{Handle, check},
    memory::{Buffer, Context, LIVE_EVENTS, LIVE_STREAMS, Pinned},
};
use gpu_engine::{
    lane::{
        LaneConfig, LaneOp, TokenBucket, TransferPlanner, Zone, active_slot_budget,
        overlap_seconds, vram_key, vram_pattern_into,
    },
    verifier::{first_difference, pattern_class, transfer_pattern_into},
};
use serde::Serialize;
use std::{
    collections::HashMap,
    ffi::c_void,
    ptr,
    rc::Rc,
    sync::atomic::Ordering as AtomicOrdering,
    time::{Duration, Instant},
};

/// Device-side bandwidth cap of the GPU-local workload (bytes the kernels touch per second).
const VRAM_TOUCH_RATE: u64 = 4 * 1024 * 1024 * 1024;
const ALIGN: u64 = 4096;
/// A pinned buffer handed to a worker. The slot state machine guarantees exclusive access
/// while a worker holds it, so the worker may build a slice from the address.
#[derive(Clone, Copy)]
pub(crate) struct Span {
    ptr: usize,
    len: usize,
}

impl Span {
    fn of(pinned: &Pinned, len: usize) -> Self {
        assert!(len <= pinned.len);
        Self {
            ptr: pinned.address as usize,
            len,
        }
    }
    /// # Safety
    /// The caller is the only user of the bytes until it returns them (slot in `Preparing`).
    unsafe fn bytes_mut<'a>(self) -> &'a mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u8, self.len) }
    }
    /// # Safety
    /// No DMA or writer touches the bytes (slot in `Comparing`).
    unsafe fn bytes<'a>(self) -> &'a [u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Dma,
    Vram,
}

/// CPU work for a worker.
#[derive(Clone, Copy)]
pub(crate) enum Action {
    Prepare {
        slot: usize,
        span: Span,
        kind: Kind,
        seed: u64,
        generation: u64,
        offset: u64,
    },
    Compare {
        slot: usize,
        expected: Span,
        actual: Span,
        /// Boundary checks: on a mismatch, also hash both sides and compare a second time.
        evidence: bool,
    },
}

impl Action {
    /// Bytes the action costs its queue class.
    pub(crate) fn cost(&self) -> u64 {
        match self {
            Self::Prepare { span, .. } => span.len as u64,
            Self::Compare { expected, .. } => expected.len as u64,
        }
    }
}

/// Runs a `Prepare` on a worker.
pub(crate) fn run_prepare(
    kind: Kind,
    span: Span,
    seed: u64,
    generation: u64,
    offset: u64,
    threads: usize,
) {
    // SAFETY: the slot is `Preparing`; the owner does not touch the buffer until it hears back.
    let out = unsafe { span.bytes_mut() };
    match kind {
        Kind::Dma => transfer_pattern_into(out, seed, generation, offset, threads),
        Kind::Vram => vram_pattern_into(out, vram_key(seed, generation), offset / 4),
    }
}

/// First differing byte of a lane comparison as (index, expected, actual).
pub(crate) type Mismatch = (usize, u8, u8);

/// Runs a `Compare` on a worker: the first differing byte and, when asked and only on a
/// mismatch, the boundary-check evidence.
pub(crate) fn run_compare(
    expected: Span,
    actual: Span,
    evidence: bool,
) -> (Option<Mismatch>, Option<LaneEvidence>) {
    // SAFETY: the slot is `Comparing`; the D2H copy finished and nothing writes either buffer.
    let (e, a) = unsafe { (expected.bytes(), actual.bytes()) };
    let mismatch = first_difference(e, a).map(|i| {
        (
            i,
            e.get(i).copied().unwrap_or(0),
            a.get(i).copied().unwrap_or(0),
        )
    });
    let evidence = (evidence && mismatch.is_some()).then(|| checkpoints::lane_evidence(e, a));
    (mismatch, evidence)
}

/// Faults a test may inject to prove each detector fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneFault {
    /// Corrupt the read-back of the first transfer.
    Transfer,
    /// Corrupt the read-back of the first retention re-read.
    Retention,
    /// Make the first GPU-local self-check see a different key (as damaged memory would).
    VramSelfCheck,
    /// Corrupt the read-back of the first GPU-local checkpoint.
    VramCheckpoint,
    /// Make the GPU-DRAM sweep's first verify kernel expect a different key.
    SweepVerify,
    /// Corrupt the read-back of the GPU-DRAM sweep's first CPU checkpoint.
    SweepCheckpoint,
}

#[derive(Clone, Debug, Serialize)]
pub struct LaneFailure {
    pub phase: String,
    pub generation: u64,
    pub seed: u64,
    pub allocation: usize,
    pub offset: u64,
    pub bytes: u64,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct DmaReport {
    pub write_ops: u64,
    pub reread_ops: u64,
    pub forced_rereads: u64,
    pub h2d_bytes: u64,
    pub d2h_bytes: u64,
    pub h2d_ops: u64,
    pub d2h_ops: u64,
    pub h2d_event_seconds: f64,
    pub d2h_event_seconds: f64,
    /// Bytes read back and found equal to the pattern.
    pub verified_bytes: u64,
    /// Operations that verified, per pattern class (schedule version 2).
    pub pattern_counts: [u64; 7],
    pub sweeps: u64,
    pub pending_rereads: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VramReport {
    pub write_ops: u64,
    pub checkpoint_ops: u64,
    /// Bytes the kernels wrote and read on the device: activity, not coverage.
    pub touched_bytes: u64,
    /// Bytes a checkpoint read back and the CPU found equal to the pattern: coverage.
    pub cpu_verified_bytes: u64,
    pub kernel_event_seconds: f64,
    pub self_check_bad_words: u64,
    pub sweeps: u64,
    pub pending_checkpoints: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct LaneReport {
    pub version: u32,
    pub config: LaneConfig,
    pub pinned_bytes: u64,
    pub ops_started: u64,
    pub ops_completed: u64,
    pub ops_failed: u64,
    pub in_flight_peak: u64,
    pub transfer: DmaReport,
    pub vram: VramReport,
    pub prepare_core_seconds: f64,
    pub compare_core_seconds: f64,
    pub queue_wait_total_seconds: f64,
    pub queue_wait_max_seconds: f64,
    /// Union of the lane's device intervals, and how much of it ran while a compute batch did.
    pub busy_wall_seconds: f64,
    pub compute_busy_wall_seconds: f64,
    pub overlap_with_compute_seconds: f64,
    pub failures_total: u64,
    pub conservation: String,
    /// Where failed regions went wrong, when the boundary checks are on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary: Option<BoundaryCounts>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Idle,
    Preparing,
    Copying,
    Comparing,
}

struct Running {
    kind: Kind,
    op: LaneOp,
    seed: u64,
    /// The end-of-run flush, reported as `retention-final`.
    flush: bool,
    /// Boundary checks apply: a failure is placed by hashing the region on the GPU.
    hashed: bool,
}

struct Slot {
    context: Rc<Context>,
    upload: Pinned,
    download: Pinned,
    /// GPU self-check result: the device-side mismatch counter, copied here.
    flag: Pinned,
    counter: Buffer,
    stream: Handle,
    events: [Handle; 3],
    state: State,
    running: Option<Running>,
    in_flight: bool,
}

impl Slot {
    fn new(backend: &CudaBackend, bytes: usize) -> Result<Self> {
        let context = &backend.context;
        backend.alloc_step()?;
        let upload = Pinned::new(context, bytes)?;
        backend.alloc_step()?;
        let download = Pinned::new(context, bytes)?;
        backend.alloc_step()?;
        let flag = Pinned::new(context, 4)?;
        backend.alloc_step()?;
        let counter = Buffer::new(context, 4)?;
        let mut slot = Self {
            context: Rc::clone(context),
            upload,
            download,
            flag,
            counter,
            stream: ptr::null_mut(),
            events: [ptr::null_mut(); 3],
            state: State::Idle,
            running: None,
            in_flight: false,
        };
        backend.alloc_step()?;
        check("lane stream", unsafe {
            (context.driver.stream_create)(&raw mut slot.stream, 1)
        })?;
        LIVE_STREAMS.fetch_add(1, AtomicOrdering::SeqCst);
        for event in &mut slot.events {
            backend.alloc_step()?;
            check("lane event", unsafe {
                (context.driver.event_create)(event, 0)
            })?;
            LIVE_EVENTS.fetch_add(1, AtomicOrdering::SeqCst);
        }
        Ok(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.in_flight {
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

struct Source {
    kind: Kind,
    planner: TransferPlanner,
    /// Seed each written generation was made with, needed to regenerate it for the re-read.
    seeds: HashMap<u64, u64>,
}

pub(crate) struct Lane {
    config: LaneConfig,
    slots: Vec<Slot>,
    sources: Vec<Source>,
    pool_sizes: Vec<usize>,
    dma_bucket: TokenBucket,
    vram_bucket: TokenBucket,
    turn: usize,
    fault: Option<LaneFault>,
    report: LaneReport,
    failures: Vec<LaneFailure>,
    /// Device intervals of this burst, relative to the pipeline's origin event.
    intervals: Vec<(f64, f64)>,
    /// With the GPU-DRAM sweep on, the lane works one half of every allocation (0 or 1) while
    /// the sweep works the other, and they trade places; `None` is the whole pool.
    half: Option<usize>,
    /// Transfer sweeps completed on each half before the halves were last traded.
    half_sweeps: [u64; 2],
    /// Boundary checks (step F3): the GPU hashes every region before it is copied back.
    checks: bool,
    /// Test-only boundary injection (`DIT_GPU_BOUNDARY_FAULT=lane-h2d`, `lane-h2d-persistent`,
    /// `lane-rest`).
    boundary_fault: Option<BoundaryFault>,
    /// Injection: the region's second upload is damaged too (`lane-h2d-persistent`).
    corrupt_reupload: bool,
}

/// Counted allocation steps per lane slot (buffers, streams, events).
pub(crate) const STEPS_PER_SLOT: usize = 8;

impl Lane {
    pub(crate) fn new(backend: &CudaBackend, config: LaneConfig, checks: bool) -> Result<Self> {
        config.validate().map_err(Error::Request)?;
        let bytes =
            usize::try_from(config.chunk_bytes).map_err(|_| Error::Request("lane chunk"))?;
        let slots = (0..config.slots)
            .map(|_| Slot::new(backend, bytes))
            .collect::<Result<Vec<_>>>()?;
        let pinned_bytes = slots
            .iter()
            .map(|s| (s.upload.len + s.download.len + s.flag.len) as u64)
            .sum();
        Ok(Self {
            config,
            slots,
            sources: Vec::new(),
            pool_sizes: Vec::new(),
            dma_bucket: TokenBucket::new(
                config.rate_bytes_per_second,
                config.chunk_bytes.max(config.rate_bytes_per_second / 10),
            ),
            vram_bucket: TokenBucket::new(VRAM_TOUCH_RATE, VRAM_TOUCH_RATE / 10),
            turn: 0,
            fault: None,
            report: LaneReport {
                version: 1,
                config,
                pinned_bytes,
                boundary: checks.then(BoundaryCounts::default),
                ..LaneReport::default()
            },
            failures: Vec::new(),
            intervals: Vec::new(),
            half: None,
            half_sweeps: [0; 2],
            checks,
            boundary_fault: checks.then(BoundaryFault::from_env).flatten().filter(|f| {
                matches!(
                    f,
                    BoundaryFault::LaneH2d
                        | BoundaryFault::LaneH2dPersistent
                        | BoundaryFault::LaneRest
                )
            }),
            corrupt_reupload: false,
        })
    }

    /// Works only `half` of every allocation from the next bind (the sweep takes the other).
    pub(crate) fn set_half(&mut self, half: Option<usize>) {
        self.half = half.map(|h| h % 2);
        self.sources.clear();
        self.pool_sizes.clear();
    }

    pub(crate) fn half(&self) -> Option<usize> {
        self.half
    }

    /// Transfer sweeps of the zones the lane holds now (since the last trade of halves).
    pub(crate) fn current_sweeps(&self) -> u64 {
        self.sources
            .iter()
            .find(|s| s.kind == Kind::Dma)
            .map_or(0, |s| s.planner.sweeps())
    }

    /// Trades halves with the sweep: re-reads everything still awaiting its retention check
    /// (nothing written is left unchecked before the sweep overwrites it), then binds the other
    /// half. Only while idle.
    pub(crate) fn swap_half(&mut self, backend: &CudaBackend, origin: Handle) -> Result<()> {
        let Some(half) = self.half else {
            return Err(Error::Request("the lane is not split into halves"));
        };
        self.flush(backend, origin)?;
        self.half_sweeps[half] += self.current_sweeps();
        self.set_half(Some(1 - half));
        self.bind(backend)
    }

    pub(crate) fn pinned_bytes(&self) -> u64 {
        self.report.pinned_bytes
    }

    pub(crate) fn inject(&mut self, fault: LaneFault) {
        self.fault = Some(fault);
    }

    pub(crate) fn busy(&self) -> usize {
        self.slots.iter().filter(|s| s.state != State::Idle).count()
    }

    pub(crate) fn take_failures(&mut self) -> Vec<LaneFailure> {
        std::mem::take(&mut self.failures)
    }

    /// Splits the pool into the DMA and GPU-local stretches. Called before each run so a
    /// changed pool is picked up; an unchanged pool keeps its cursors and pending re-reads.
    pub(crate) fn bind(&mut self, backend: &CudaBackend) -> Result<()> {
        let sizes: Vec<usize> = backend.pool.iter().map(|b| b.len).collect();
        if !self.sources.is_empty() && sizes == self.pool_sizes {
            return Ok(());
        }
        self.sources.clear();
        let mut dma = Vec::new();
        let mut vram = Vec::new();
        if let Some(half) = self.half {
            let sizes: Vec<u64> = sizes.iter().map(|len| *len as u64).collect();
            let [first, second] = gpu_engine::sweep::halves(&sizes);
            dma = if half == 0 { first } else { second };
        }
        for (allocation, len) in sizes.iter().enumerate() {
            if self.half.is_some() {
                break;
            }
            let len = *len as u64 / ALIGN * ALIGN;
            if self.config.enabled && self.config.vram_local {
                let split = len / 4 * 3 / ALIGN * ALIGN;
                if split >= 64 * 1024 && len - split >= 64 * 1024 {
                    dma.push(Zone {
                        allocation,
                        start: 0,
                        len: split,
                    });
                    vram.push(Zone {
                        allocation,
                        start: split,
                        len: len - split,
                    });
                }
            } else if self.config.enabled && len >= 64 * 1024 {
                dma.push(Zone {
                    allocation,
                    start: 0,
                    len,
                });
            } else if self.config.vram_local && len >= 64 * 1024 {
                vram.push(Zone {
                    allocation,
                    start: 0,
                    len,
                });
            }
        }
        for (kind, zones) in [(Kind::Dma, dma), (Kind::Vram, vram)] {
            if zones.is_empty() {
                continue;
            }
            let planner =
                TransferPlanner::new(zones, self.config.chunk_bytes, self.config.reread_after)
                    .map_err(Error::Request)?;
            self.sources.push(Source {
                kind,
                planner,
                seeds: HashMap::new(),
            });
        }
        if self.sources.is_empty() {
            return Err(Error::Request("pool too small for the transfer lane"));
        }
        self.pool_sizes = sizes;
        Ok(())
    }

    fn source_mut(&mut self, kind: Kind) -> Option<&mut Source> {
        self.sources.iter_mut().find(|s| s.kind == kind)
    }

    fn failure(&mut self, slot: usize, phase: &str, detail: String) {
        let Some(running) = self.slots[slot].running.as_ref() else {
            return;
        };
        let region = running.op.region();
        self.report.failures_total += 1;
        if self.failures.len() < 16 {
            self.failures.push(LaneFailure {
                phase: phase.to_owned(),
                generation: running.op.generation(),
                seed: running.seed,
                allocation: region.allocation,
                offset: region.offset,
                bytes: region.bytes,
                detail,
            });
        }
    }

    fn phase(running: &Running) -> &'static str {
        match (running.kind, running.op, running.flush) {
            (Kind::Dma, LaneOp::Write { .. }, _) => "transfer",
            (Kind::Dma, LaneOp::Reread { .. }, true) => "retention-final",
            (
                Kind::Dma,
                LaneOp::Reread {
                    before_overwrite: true,
                    ..
                },
                _,
            ) => "retention-before-overwrite",
            (Kind::Dma, LaneOp::Reread { .. }, _) => "retention",
            (Kind::Vram, LaneOp::Write { .. }, _) => "vram-self-check",
            (Kind::Vram, LaneOp::Reread { .. }, _) => "vram-checkpoint",
        }
    }

    fn needs_cpu_pattern(running: &Running) -> bool {
        !(running.kind == Kind::Vram && matches!(running.op, LaneOp::Write { .. }))
    }

    /// Advances every slot: finishes copies, and starts new work while `allow_new`.
    /// `origin` is the pipeline's common time base for device timestamps.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn poll(
        &mut self,
        backend: &CudaBackend,
        origin: Handle,
        now_ms: u64,
        allow_new: bool,
        seed: u64,
        backlog_fill: f64,
        out: &mut Vec<Action>,
    ) -> Result<()> {
        // GPU-H6: the lane is an input-responsive consumer of the controller's own verifier
        // backlog signal. It never stops outright from this alone (that stays a job for
        // Draining/Stopped/pressure/cancel) so coverage is never lost, but it opens fewer new
        // slots while compute's own backlog is high, instead of adding to the pressure at a
        // fixed rate regardless of it.
        let budget = active_slot_budget(self.slots.len(), backlog_fill);
        for slot in 0..self.slots.len() {
            match self.slots[slot].state {
                State::Copying => self.check_copy(backend, origin, slot, out)?,
                State::Idle if allow_new && self.busy() < budget => {
                    self.try_start(backend, now_ms, seed, slot, out)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn try_start(
        &mut self,
        backend: &CudaBackend,
        now_ms: u64,
        seed: u64,
        slot: usize,
        out: &mut Vec<Action>,
    ) -> Result<()> {
        let count = self.sources.len();
        for attempt in 0..count {
            let index = (self.turn + attempt) % count;
            let kind = self.sources[index].kind;
            let admitted = match kind {
                Kind::Dma => self.dma_bucket.admit(now_ms),
                Kind::Vram => self.vram_bucket.admit(now_ms),
            };
            if !admitted {
                continue;
            }
            let Some(op) = self.sources[index].planner.next(false) else {
                continue;
            };
            self.turn = index + 1;
            let bytes = op.region().bytes;
            let passes = u64::from(self.config.vram_passes);
            match (kind, op) {
                (Kind::Dma, LaneOp::Write { .. }) => self.dma_bucket.charge(bytes * 2),
                (Kind::Dma, LaneOp::Reread { .. }) => self.dma_bucket.charge(bytes),
                (Kind::Vram, LaneOp::Write { .. }) => self.vram_bucket.charge(bytes * passes * 2),
                (Kind::Vram, LaneOp::Reread { .. }) => self.vram_bucket.charge(bytes),
            }
            let seeds = &mut self.sources[index].seeds;
            let seed = match op {
                LaneOp::Write { generation, .. } => {
                    seeds.insert(generation, seed);
                    seed
                }
                LaneOp::Reread { generation, .. } => {
                    seeds.get(&generation).copied().unwrap_or(seed)
                }
            };
            return self.begin(
                backend,
                slot,
                Running {
                    kind,
                    op,
                    seed,
                    flush: false,
                    hashed: false,
                },
                out,
            );
        }
        Ok(())
    }

    /// Takes `running` into `slot` and either asks a worker for the expected pattern or starts
    /// the device work at once.
    fn begin(
        &mut self,
        backend: &CudaBackend,
        slot: usize,
        running: Running,
        out: &mut Vec<Action>,
    ) -> Result<()> {
        self.report.ops_started += 1;
        let region = running.op.region();
        let len = usize::try_from(region.bytes).map_err(|_| Error::Request("lane region"))?;
        if len == 0
            || len > self.slots[slot].upload.len
            || (running.kind == Kind::Vram && len % 4 != 0)
        {
            return Err(Error::Request("lane region"));
        }
        let prepare = Self::needs_cpu_pattern(&running).then(|| Action::Prepare {
            slot,
            span: Span::of(&self.slots[slot].upload, len),
            kind: running.kind,
            seed: running.seed,
            generation: running.op.generation(),
            offset: region.offset,
        });
        self.slots[slot].running = Some(running);
        self.report.in_flight_peak = self
            .report
            .in_flight_peak
            .max(self.slots.iter().filter(|s| s.running.is_some()).count() as u64);
        if let Some(action) = prepare {
            self.slots[slot].state = State::Preparing;
            out.push(action);
            Ok(())
        } else {
            self.start_copy(backend, slot)
        }
    }

    /// A worker finished writing the expected pattern.
    pub(crate) fn on_prepared(
        &mut self,
        backend: &CudaBackend,
        slot: usize,
        seconds: f64,
        wait: f64,
    ) -> Result<()> {
        if self.slots[slot].state != State::Preparing {
            return Err(Error::Request("lane completion for an idle slot"));
        }
        self.report.prepare_core_seconds += seconds;
        self.note_wait(wait);
        self.start_copy(backend, slot)
    }

    fn note_wait(&mut self, wait: f64) {
        self.report.queue_wait_total_seconds += wait;
        self.report.queue_wait_max_seconds = self.report.queue_wait_max_seconds.max(wait);
    }

    #[allow(clippy::too_many_lines)]
    fn start_copy(&mut self, backend: &CudaBackend, slot: usize) -> Result<()> {
        let driver = &backend.context.driver;
        let passes = self.config.vram_passes;
        let inject_check = self.fault == Some(LaneFault::VramSelfCheck);
        let s = &mut self.slots[slot];
        let running = s
            .running
            .as_ref()
            .ok_or(Error::Request("lane slot empty"))?;
        let (kind, op, seed) = (running.kind, running.op, running.seed);
        let region = op.region();
        let buffer = backend
            .pool
            .get(region.allocation)
            .ok_or(Error::Request("lane allocation"))?;
        if region.offset + region.bytes > buffer.len as u64 {
            return Err(Error::Request("lane range"));
        }
        let device = buffer.address + region.offset;
        let len = usize::try_from(region.bytes).map_err(|_| Error::Request("lane region"))?;
        // Mark before the first queued operation: an error path must treat memory as referenced.
        s.in_flight = true;
        s.state = State::Copying;
        check("lane start", unsafe {
            (driver.event_record)(s.events[0], s.stream)
        })?;
        // Boundary checks: a region found wrong is hashed on the GPU afterwards (it stays in VRAM,
        // locked by the planner, until the operation finishes), so nothing runs on a clean one.
        let hash = self.checks && len.is_multiple_of(4);
        let corrupt = match (kind, op) {
            (Kind::Dma, LaneOp::Write { .. }) => matches!(
                self.boundary_fault,
                Some(BoundaryFault::LaneH2d | BoundaryFault::LaneH2dPersistent)
            ),
            (Kind::Dma, LaneOp::Reread { .. }) => {
                self.boundary_fault == Some(BoundaryFault::LaneRest)
            }
            _ => false,
        } && hash;
        if corrupt {
            self.corrupt_reupload = self.boundary_fault == Some(BoundaryFault::LaneH2dPersistent);
            self.boundary_fault = None;
        }
        let s = &mut self.slots[slot];
        if let Some(running) = s.running.as_mut() {
            running.hashed = hash && !(kind == Kind::Vram && matches!(op, LaneOp::Write { .. }));
        }
        match (kind, op) {
            (Kind::Dma, LaneOp::Write { .. }) => {
                check("lane H2D", unsafe {
                    (driver.upload)(device, s.upload.address, len, s.stream)
                })?;
                check("lane H2D end", unsafe {
                    (driver.event_record)(s.events[1], s.stream)
                })?;
                if corrupt {
                    corrupt_word(backend, s.stream, device)?;
                }
                check("lane D2H", unsafe {
                    (driver.download)(s.download.address, device, len, s.stream)
                })?;
            }
            (Kind::Dma | Kind::Vram, LaneOp::Reread { .. }) => {
                check("lane mark", unsafe {
                    (driver.event_record)(s.events[1], s.stream)
                })?;
                if corrupt {
                    corrupt_word(backend, s.stream, device)?;
                }
                check("lane D2H", unsafe {
                    (driver.download)(s.download.address, device, len, s.stream)
                })?;
            }
            (Kind::Vram, LaneOp::Write { generation, .. }) => {
                check("vram counter reset", unsafe {
                    (driver.fill)(s.counter.address, 0, 1, s.stream)
                })?;
                let key = vram_key(seed, generation);
                let words = (len / 4) as u64;
                let first = region.offset / 4;
                let grid = u32::try_from(words.div_ceil(256).min(2048)).unwrap_or(2048);
                for pass in 0..passes {
                    let last = pass + 1 == passes;
                    // Earlier passes write other data; the last leaves the generation's pattern
                    // in VRAM for the checkpoint.
                    let written = if last {
                        key
                    } else {
                        key.rotate_left(pass + 1) ^ 0xA55A_1973
                    };
                    let expected = if last && inject_check {
                        written ^ 1
                    } else {
                        written
                    };
                    let mut address = device;
                    let mut first_word = first;
                    let mut word_count = words;
                    let mut fill_key = written;
                    let mut fill_params = [
                        (&raw mut address).cast::<c_void>(),
                        (&raw mut first_word).cast(),
                        (&raw mut word_count).cast(),
                        (&raw mut fill_key).cast(),
                    ];
                    check("vram fill", unsafe {
                        (driver.launch)(
                            backend.fill_function,
                            grid,
                            1,
                            1,
                            256,
                            1,
                            1,
                            0,
                            s.stream,
                            fill_params.as_mut_ptr(),
                            ptr::null_mut(),
                        )
                    })?;
                    let mut check_key = expected;
                    let mut counter = s.counter.address;
                    let mut verify_params = [
                        (&raw mut address).cast::<c_void>(),
                        (&raw mut first_word).cast(),
                        (&raw mut word_count).cast(),
                        (&raw mut check_key).cast(),
                        (&raw mut counter).cast(),
                    ];
                    check("vram verify", unsafe {
                        (driver.launch)(
                            backend.verify_function,
                            grid,
                            1,
                            1,
                            256,
                            1,
                            1,
                            0,
                            s.stream,
                            verify_params.as_mut_ptr(),
                            ptr::null_mut(),
                        )
                    })?;
                }
                if inject_check {
                    self.fault = None;
                }
                let s = &mut self.slots[slot];
                check("lane kernels end", unsafe {
                    (driver.event_record)(s.events[1], s.stream)
                })?;
                check("vram counter read", unsafe {
                    (driver.download)(s.flag.address, s.counter.address, 4, s.stream)
                })?;
            }
        }
        let s = &mut self.slots[slot];
        check("lane end", unsafe {
            (driver.event_record)(s.events[2], s.stream)
        })
    }

    /// Counts a finished device copy in its direction (and GPU-local work as device activity).
    fn account_copy(&mut self, kind: Kind, op: LaneOp, first: f64, second: f64) {
        let bytes = op.region().bytes;
        match (kind, op) {
            (Kind::Dma, LaneOp::Write { .. }) => {
                let d = &mut self.report.transfer;
                d.write_ops += 1;
                d.h2d_ops += 1;
                d.d2h_ops += 1;
                d.h2d_bytes += bytes;
                d.d2h_bytes += bytes;
                d.h2d_event_seconds += first;
                d.d2h_event_seconds += second;
            }
            (
                Kind::Dma,
                LaneOp::Reread {
                    before_overwrite, ..
                },
            ) => {
                let d = &mut self.report.transfer;
                d.reread_ops += 1;
                d.forced_rereads += u64::from(before_overwrite);
                d.d2h_ops += 1;
                d.d2h_bytes += bytes;
                d.d2h_event_seconds += second;
            }
            (Kind::Vram, LaneOp::Write { .. }) => {
                let v = &mut self.report.vram;
                v.write_ops += 1;
                v.touched_bytes += bytes * u64::from(self.config.vram_passes) * 2;
                v.kernel_event_seconds += first;
            }
            (Kind::Vram, LaneOp::Reread { .. }) => {
                self.report.vram.checkpoint_ops += 1;
            }
        }
    }

    /// A GPU-local write ends with the device's own verdict: the count of words that read back
    /// different from what the kernel had just written.
    fn finish_vram_write(&mut self, slot: usize) {
        // SAFETY: the D2H of the counter finished (event observed by the caller); nothing writes it.
        let bad = u32::from_le_bytes(
            unsafe { self.slots[slot].flag.completed_bytes(4) }
                .try_into()
                .unwrap_or([0; 4]),
        );
        if bad == 0 {
            self.finish(slot, true);
        } else {
            self.report.vram.self_check_bad_words += u64::from(bad);
            self.failure(
                slot,
                "vram-self-check",
                format!(
                    "the device found {bad} words differing from the pattern it had just written"
                ),
            );
            self.finish(slot, false);
        }
    }

    fn check_copy(
        &mut self,
        backend: &CudaBackend,
        origin: Handle,
        slot: usize,
        out: &mut Vec<Action>,
    ) -> Result<()> {
        let driver = &backend.context.driver;
        let s = &self.slots[slot];
        let status = unsafe { (driver.event_query)(s.events[2]) };
        if status == 600 {
            return Ok(());
        }
        if status != 0 {
            backend.context.stopped.set(true);
            return Err(Error::Api {
                call: "lane event query",
                status,
            });
        }
        let mut timing = [0_f32; 2];
        for (i, value) in timing.iter_mut().enumerate() {
            check("lane timing", unsafe {
                (driver.event_elapsed)(value, s.events[i], s.events[i + 1])
            })?;
        }
        // Best effort: a missing stamp only drops this operation from the overlap measure.
        let mut stamps = [f32::NAN; 2];
        for (i, stamp) in stamps.iter_mut().enumerate() {
            let _ = unsafe { (driver.event_elapsed)(stamp, origin, s.events[i * 2]) };
        }
        self.intervals
            .push((f64::from(stamps[0]) / 1000.0, f64::from(stamps[1]) / 1000.0));
        let (first, second) = (f64::from(timing[0]) / 1000.0, f64::from(timing[1]) / 1000.0);
        let running = self.slots[slot]
            .running
            .as_ref()
            .ok_or(Error::Request("lane slot empty"))?;
        let (kind, op) = (running.kind, running.op);
        let bytes = op.region().bytes;
        self.slots[slot].in_flight = false;
        self.account_copy(kind, op, first, second);
        if kind == Kind::Vram && matches!(op, LaneOp::Write { .. }) {
            self.finish_vram_write(slot);
            return Ok(());
        }
        let corrupt = matches!(
            (self.fault, kind, op),
            (Some(LaneFault::Transfer), Kind::Dma, LaneOp::Write { .. })
                | (Some(LaneFault::Retention), Kind::Dma, LaneOp::Reread { .. })
                | (
                    Some(LaneFault::VramCheckpoint),
                    Kind::Vram,
                    LaneOp::Reread { .. }
                )
        );
        if corrupt {
            self.slots[slot].download.corrupt_first_byte();
            self.fault = None;
        }
        let len = usize::try_from(bytes).map_err(|_| Error::Request("lane region"))?;
        self.slots[slot].state = State::Comparing;
        let hashed = self.slots[slot].running.as_ref().is_some_and(|r| r.hashed);
        out.push(Action::Compare {
            slot,
            expected: Span::of(&self.slots[slot].upload, len),
            actual: Span::of(&self.slots[slot].download, len),
            evidence: hashed,
        });
        Ok(())
    }

    /// A worker finished comparing.
    pub(crate) fn on_compared(
        &mut self,
        backend: &CudaBackend,
        slot: usize,
        mismatch: Option<Mismatch>,
        evidence: Option<LaneEvidence>,
        seconds: f64,
        wait: f64,
    ) -> Result<()> {
        if self.slots[slot].state != State::Comparing {
            return Err(Error::Request("lane completion for an idle slot"));
        }
        self.report.compare_core_seconds += seconds;
        self.note_wait(wait);
        if let Some((index, expected, actual)) = mismatch {
            let phase = self.slots[slot]
                .running
                .as_ref()
                .map_or("transfer", Self::phase);
            let mut detail =
                format!("mismatch at byte {index}: expected {expected:#04x}, actual {actual:#04x}");
            if let Some(evidence) = evidence {
                let kind = self.place(backend, slot, evidence, index, expected)?;
                let mut counts = BoundaryCounts::default();
                counts.add_kind(kind);
                if let Some(total) = self.report.boundary.as_mut() {
                    total.add(&counts);
                }
                let tag = counts.tag();
                if !tag.is_empty() {
                    detail = format!("{detail} {tag}");
                }
            }
            self.failure(slot, phase, detail);
            self.finish(slot, false);
        } else {
            self.finish(slot, true);
        }
        Ok(())
    }

    /// Where a failed region went wrong (boundary checks). The region is still in VRAM, locked
    /// until the operation finishes, so the GPU hashes it now: still right means it changed on
    /// the way back (or on the host). Wrong is hashed again (a GPU that hashes two ways is the
    /// GPU); then a transfer's region is uploaded again (right the second time: the link; wrong
    /// again: link or VRAM), while a re-read's region was right when written (VRAM lost it at
    /// rest). Failure path only; waits on the slot's idle stream.
    fn place(
        &mut self,
        backend: &CudaBackend,
        slot: usize,
        evidence: LaneEvidence,
        index: usize,
        expected: u8,
    ) -> Result<Boundary> {
        let s = &self.slots[slot];
        let running = s
            .running
            .as_ref()
            .ok_or(Error::Request("lane slot empty"))?;
        let (op, region) = (running.op, running.op.region());
        let buffer = backend
            .pool
            .get(region.allocation)
            .ok_or(Error::Request("lane allocation"))?;
        let device = buffer.address + region.offset;
        let len = usize::try_from(region.bytes).map_err(|_| Error::Request("lane region"))?;
        let device_hash = hash_now(backend, s, device, len)?;
        match checkpoints::classify_lane(device_hash, evidence) {
            Some(Boundary::D2h) if !checkpoints::host_holds_byte(&s.download, index, expected) => {
                return Ok(Boundary::HostRam);
            }
            Some(kind) => return Ok(kind),
            None => {}
        }
        if hash_now(backend, s, device, len)? != device_hash {
            return Ok(Boundary::Device);
        }
        Ok(match op {
            LaneOp::Write { .. } => {
                check("lane re-upload", unsafe {
                    (backend.context.driver.upload)(device, s.upload.address, len, s.stream)
                })?;
                if std::mem::take(&mut self.corrupt_reupload) {
                    corrupt_word(backend, s.stream, device)?;
                }
                if hash_now(backend, s, device, len)? == evidence.expected_hash {
                    Boundary::H2d
                } else {
                    Boundary::H2dPersistent
                }
            }
            LaneOp::Reread { .. } => Boundary::VramRest,
        })
    }

    /// Releases the slot and settles the planner and the counters.
    fn finish(&mut self, slot: usize, ok: bool) {
        let s = &mut self.slots[slot];
        s.state = State::Idle;
        let Some(running) = s.running.take() else {
            return;
        };
        let bytes = running.op.region().bytes;
        let generation = running.op.generation();
        let is_write = matches!(running.op, LaneOp::Write { .. });
        if ok {
            self.report.ops_completed += 1;
        } else {
            self.report.ops_failed += 1;
        }
        if ok && !(running.kind == Kind::Vram && is_write) {
            match running.kind {
                Kind::Dma => {
                    self.report.transfer.verified_bytes += bytes;
                    self.report.transfer.pattern_counts[pattern_class(generation).index()] += 1;
                }
                Kind::Vram => self.report.vram.cpu_verified_bytes += bytes,
            }
        }
        if let Some(source) = self.source_mut(running.kind) {
            if ok {
                source.planner.completed(&running.op);
            } else {
                source.planner.failed(&running.op);
            }
            if !is_write || !ok {
                source.seeds.remove(&generation);
            }
        }
        self.refresh_progress();
    }

    fn refresh_progress(&mut self) {
        for source in &self.sources {
            let sweeps = source.planner.sweeps();
            let pending = source.planner.pending_rereads() as u64;
            match source.kind {
                Kind::Dma => {
                    // Split into halves, a pool sweep is complete once both halves were swept.
                    self.report.transfer.sweeps = match self.half {
                        Some(half) => {
                            let mut done = self.half_sweeps;
                            done[half] += sweeps;
                            done[0].min(done[1])
                        }
                        None => sweeps,
                    };
                    self.report.transfer.pending_rereads = pending;
                    self.report.transfer.forced_rereads = source.planner.forced_rereads;
                }
                Kind::Vram => {
                    self.report.vram.sweeps = sweeps;
                    self.report.vram.pending_checkpoints = pending;
                }
            }
        }
    }

    /// Closes a burst: measures how much of the lane's device time overlapped compute.
    pub(crate) fn burst_finished(&mut self, compute: &[(f64, f64)]) {
        let mut own = std::mem::take(&mut self.intervals);
        let busy = gpu_engine::accounting::interval_union(&mut own);
        let mut theirs = compute.to_vec();
        let compute_busy = gpu_engine::accounting::interval_union(&mut theirs);
        self.report.busy_wall_seconds += busy;
        self.report.compute_busy_wall_seconds += compute_busy;
        self.report.overlap_with_compute_seconds += overlap_seconds(&own, compute);
    }

    pub(crate) fn report(&self) -> LaneReport {
        let mut report = self.report.clone();
        report.conservation = if report.ops_started
            == report.ops_completed + report.ops_failed + self.busy() as u64
        {
            "ok".into()
        } else {
            format!(
                "started {} != completed {} + failed {} + busy {}",
                report.ops_started,
                report.ops_completed,
                report.ops_failed,
                self.busy()
            )
        };
        report
    }

    /// Verifies every region that still awaits its re-read or checkpoint, one at a time, with no
    /// new writes. Run once at the end so nothing written is left unchecked.
    pub(crate) fn flush(&mut self, backend: &CudaBackend, origin: Handle) -> Result<()> {
        if self.busy() != 0 {
            return Err(Error::Request("lane is busy"));
        }
        for index in 0..self.sources.len() {
            let kind = self.sources[index].kind;
            while let Some(op) = self.sources[index].planner.next(true) {
                let generation = op.generation();
                let seed = self.sources[index]
                    .seeds
                    .get(&generation)
                    .copied()
                    .unwrap_or(0);
                let mut actions = Vec::new();
                self.begin(
                    backend,
                    0,
                    Running {
                        kind,
                        op,
                        seed,
                        flush: true,
                        hashed: false,
                    },
                    &mut actions,
                )?;
                self.settle(backend, origin, 0, &mut actions)?;
            }
        }
        Ok(())
    }

    /// Drives slot `slot` to completion on this thread.
    fn settle(
        &mut self,
        backend: &CudaBackend,
        origin: Handle,
        slot: usize,
        actions: &mut Vec<Action>,
    ) -> Result<()> {
        let started = Instant::now();
        loop {
            for action in std::mem::take(actions) {
                match action {
                    Action::Prepare {
                        slot,
                        span,
                        kind,
                        seed,
                        generation,
                        offset,
                    } => {
                        let at = Instant::now();
                        run_prepare(
                            kind,
                            span,
                            seed,
                            generation,
                            offset,
                            gpu_engine::verifier::default_helper_threads(),
                        );
                        self.on_prepared(backend, slot, at.elapsed().as_secs_f64(), 0.0)?;
                    }
                    Action::Compare {
                        slot,
                        expected,
                        actual,
                        evidence,
                    } => {
                        let at = Instant::now();
                        let (mismatch, evidence) = run_compare(expected, actual, evidence);
                        self.on_compared(
                            backend,
                            slot,
                            mismatch,
                            evidence,
                            at.elapsed().as_secs_f64(),
                            0.0,
                        )?;
                    }
                }
            }
            if self.slots[slot].state == State::Idle {
                return Ok(());
            }
            if self.slots[slot].state == State::Copying {
                self.check_copy(backend, origin, slot, actions)?;
            }
            if started.elapsed() > Duration::from_secs(30) {
                backend.context.stopped.set(true);
                return Err(Error::Timeout);
            }
            if actions.is_empty() && self.slots[slot].state == State::Copying {
                std::thread::sleep(Duration::from_micros(100));
            }
        }
    }
}

/// Queues the GPU hash of `len` bytes at `device` into the slot's counter and its copy into
/// `flag` (boundary checks), on the slot's stream.
fn queue_hash(backend: &CudaBackend, s: &Slot, device: u64, len: usize) -> Result<()> {
    let driver = &backend.context.driver;
    check("lane hash clear", unsafe {
        (driver.fill)(s.counter.address, 0, 1, s.stream)
    })?;
    let mut words_arg = device;
    let mut count = (len / 4) as u64;
    let mut out = s.counter.address;
    let mut params = [
        (&raw mut words_arg).cast::<c_void>(),
        (&raw mut count).cast(),
        (&raw mut out).cast(),
    ];
    let blocks = u32::try_from((len / 4).div_ceil(256))
        .unwrap_or(u32::MAX)
        .clamp(1, backend.sm_count * 16);
    check("lane hash", unsafe {
        (driver.launch)(
            backend.hash_function,
            blocks,
            1,
            1,
            256,
            1,
            1,
            0,
            s.stream,
            params.as_mut_ptr(),
            ptr::null_mut(),
        )
    })?;
    check("lane hash readback", unsafe {
        (driver.download)(s.flag.address, s.counter.address, 4, s.stream)
    })
}

/// The GPU hash of `len` bytes at `device`, now (failure path: queued and waited on).
fn hash_now(backend: &CudaBackend, s: &Slot, device: u64, len: usize) -> Result<u32> {
    queue_hash(backend, s, device, len)?;
    let started = Instant::now();
    loop {
        let status = unsafe { (backend.context.driver.stream_query)(s.stream) };
        if status == 0 {
            break;
        }
        if status != 600 || started.elapsed() > Duration::from_secs(30) {
            backend.context.stopped.set(true);
            return Err(if status == 600 {
                Error::Timeout
            } else {
                Error::Api {
                    call: "lane confirmation",
                    status,
                }
            });
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    // SAFETY: the stream is idle, so the 4-byte copy landed; nothing else uses `flag` now.
    Ok(u32::from_le_bytes(
        unsafe { s.flag.completed_bytes(4) }
            .try_into()
            .unwrap_or([0; 4]),
    ))
}

/// Injection only: one word of the region changes on the device (stream-ordered).
fn corrupt_word(backend: &CudaBackend, stream: Handle, device: u64) -> Result<()> {
    check("lane boundary injection", unsafe {
        (backend.context.driver.fill)(device, 0xDEAD_BEEF, 1, stream)
    })
}
