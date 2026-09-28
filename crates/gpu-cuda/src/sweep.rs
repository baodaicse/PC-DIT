//! GPU-DRAM sweep on the device: runs a [`SweepPlan`] on its own stream while compute batches
//! and the transfer lane run, at DRAM bandwidth, and samples its work with CPU checkpoints.
//!
//! Like the lane, it belongs to the owner thread and is polled from the burst loop: at most one
//! launch is in flight, completion is observed through an event, and the next launch waits out
//! the duty cycle. Bytes the device verified (`device_verified_bytes`) are counted apart from
//! bytes the CPU compared (`checkpoint_cpu_verified_bytes`); a checkpoint mismatch means the
//! device's own check cannot be trusted and fails the run like any other mismatch.
use crate::{
    CudaBackend, Error, Result,
    driver::{Handle, check},
    lane::{LaneFailure, LaneFault},
    memory::{Buffer, Context, LIVE_EVENTS, LIVE_STREAMS, Pinned},
};
use gpu_engine::{
    lane::Zone,
    sweep::{
        CHECKPOINT_BYTES, DEFAULT_SPAN_BYTES, SweepPhase, SweepPlan, SweepStep, idle_after,
        sweep_pattern_into,
    },
    verifier::first_difference,
};
use serde::Serialize;
use std::{
    ffi::c_void,
    ptr,
    rc::Rc,
    sync::atomic::Ordering as AtomicOrdering,
    time::{Duration, Instant},
};

const THREADS: u32 = 256;
/// Default share of device time for the sweep. The sweep and the GEMM each fill the device on
/// their own, and on the GTX 960 they only time-share (normalised throughputs sum to 1.0-1.06
/// at every duty/share tried), so this is a policy split: the GEMM keeps ~82% of its rate and the
/// sweep runs at full DRAM bandwidth for a quarter of the time
/// (build/gpu-vram-sweep-tune-20260924-191831).
pub const DEFAULT_DUTY: f64 = 0.25;
/// Resident blocks per SM the full grid aims for (2048 threads).
const BLOCKS_PER_SM: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct SweepConfig {
    pub enabled: bool,
    /// Share of device time the sweep may hold; `None` is [`DEFAULT_DUTY`].
    pub duty: Option<f64>,
    /// Share of the device's resident threads the sweep launches (0.05..=1.0); `None` picks
    /// 1.0. A DRAM stream needs many threads in flight to reach bandwidth.
    pub share: Option<f64>,
    /// Least time between two CPU checkpoints.
    pub checkpoint_interval_ms: u64,
    /// Least time between two trades of pool halves with the transfer lane: each trade stops
    /// the lane and re-reads its pending retention checks, so it is kept rare.
    pub swap_interval_ms: u64,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duty: None,
            share: None,
            checkpoint_interval_ms: 2_000,
            swap_interval_ms: 10_000,
        }
    }
}

impl SweepConfig {
    pub(crate) fn validate(self) -> Result<()> {
        let out = |v: Option<f64>| v.is_some_and(|v| !(0.05..=1.0).contains(&v));
        if out(self.duty) || out(self.share) || self.checkpoint_interval_ms == 0 {
            return Err(Error::Request(
                "sweep duty and share must be within 0.05..=1.0",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SweepReport {
    pub enabled: bool,
    pub grid_blocks: u32,
    pub duty: f64,
    pub concurrent_kernels: bool,
    /// Bytes of VRAM the sweep currently works (half the pool; it trades halves with the lane).
    pub zone_bytes: u64,
    pub passes_completed: u64,
    /// Bytes written by fill kernels.
    pub fill_bytes: u64,
    /// Bytes read back and compared on the device with no mismatch: device-verified coverage.
    pub device_verified_bytes: u64,
    pub bad_words: u64,
    pub kernel_event_seconds: f64,
    pub checkpoint_ops: u64,
    /// Bytes the CPU read back over `PCIe` and found equal to the host-regenerated pattern.
    pub checkpoint_cpu_verified_bytes: u64,
    pub zone_swaps: u64,
    pub failures_total: u64,
}

enum Running {
    Step(SweepStep),
    /// A verify kernel finished; its mismatch counter is being copied back. The copy is only
    /// queued once the kernel is done: queued behind a running kernel, it would hold up every
    /// device-to-host copy after it (the transfer lane's included) until that kernel ended.
    Counter(SweepStep),
    Checkpoint {
        allocation: usize,
        offset: u64,
        bytes: u64,
        key: u32,
    },
}

pub(crate) struct Sweeper {
    context: Rc<Context>,
    stream: Handle,
    events: [Handle; 2],
    counter: Buffer,
    flag: Pinned,
    checkpoint: Pinned,
    plan: SweepPlan,
    seed: u64,
    config: SweepConfig,
    grid: u32,
    duty: f64,
    running: Option<Running>,
    not_before: Instant,
    last_checkpoint: Instant,
    report: SweepReport,
    failures: Vec<LaneFailure>,
    in_flight: bool,
    /// Negative-gate injection, consumed by the first launch it applies to.
    fault: Option<LaneFault>,
}

impl Sweeper {
    pub(crate) fn new(
        backend: &CudaBackend,
        config: SweepConfig,
        zones: Vec<Zone>,
        seed: u64,
    ) -> Result<Self> {
        config.validate()?;
        let plan = SweepPlan::new(zones, DEFAULT_SPAN_BYTES, seed).map_err(Error::Request)?;
        let context = &backend.context;
        let concurrent = backend.concurrent_kernels;
        let full = backend.sm_count * BLOCKS_PER_SM;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let grid = ((f64::from(full) * config.share.unwrap_or(1.0)).round() as u32).max(1);
        let duty = config.duty.unwrap_or(DEFAULT_DUTY);
        backend.alloc_step()?;
        let counter = Buffer::new(context, 4)?;
        backend.alloc_step()?;
        let flag = Pinned::new(context, 4)?;
        backend.alloc_step()?;
        let checkpoint = Pinned::new(
            context,
            usize::try_from(CHECKPOINT_BYTES).map_err(|_| Error::Request("checkpoint size"))?,
        )?;
        let mut sweeper = Self {
            context: Rc::clone(context),
            stream: ptr::null_mut(),
            events: [ptr::null_mut(); 2],
            counter,
            flag,
            checkpoint,
            report: SweepReport {
                enabled: true,
                grid_blocks: grid,
                duty,
                concurrent_kernels: concurrent,
                zone_bytes: plan.zone_bytes(),
                ..SweepReport::default()
            },
            plan,
            seed,
            config,
            grid,
            duty,
            running: None,
            not_before: Instant::now(),
            last_checkpoint: Instant::now(),
            failures: Vec::new(),
            in_flight: false,
            fault: None,
        };
        // Highest priority: while GEMM batches keep every SM busy with queued blocks, a
        // default-priority sweep only gets the gaps (measured on a GTX 960: 70 GiB/s alone,
        // 10 GiB/s next to the GEMM). The duty cycle bounds how much it then takes.
        let (mut least, mut greatest) = (0, 0);
        check("stream priorities", unsafe {
            (context.driver.stream_priority_range)(&raw mut least, &raw mut greatest)
        })?;
        backend.alloc_step()?;
        check("sweep stream", unsafe {
            (context.driver.stream_create_priority)(&raw mut sweeper.stream, 1, greatest)
        })?;
        LIVE_STREAMS.fetch_add(1, AtomicOrdering::SeqCst);
        for event in &mut sweeper.events {
            backend.alloc_step()?;
            check("sweep event", unsafe {
                (context.driver.event_create)(event, 0)
            })?;
            LIVE_EVENTS.fetch_add(1, AtomicOrdering::SeqCst);
        }
        Ok(sweeper)
    }

    pub(crate) fn inject(&mut self, fault: LaneFault) {
        self.fault = Some(fault);
    }

    pub(crate) fn pinned_bytes(&self) -> u64 {
        (self.flag.len + self.checkpoint.len) as u64
    }

    pub(crate) fn idle(&self) -> bool {
        self.running.is_none()
    }

    /// Idle with the last pass fully written and read: the zone set may change here.
    pub(crate) fn at_boundary(&self) -> bool {
        self.idle() && self.plan.at_pass_boundary()
    }

    /// The plan sits between passes (a launch of the next pass has not been issued).
    pub(crate) fn plan_at_boundary(&self) -> bool {
        self.plan.at_pass_boundary()
    }

    pub(crate) fn swap_interval(&self) -> Duration {
        Duration::from_millis(self.config.swap_interval_ms)
    }

    pub(crate) fn passes_completed(&self) -> u64 {
        self.plan.passes_completed()
    }

    /// Moves to another zone set (the half the lane just left). Idle, at a boundary only.
    pub(crate) fn rebind(&mut self, zones: Vec<Zone>) -> Result<()> {
        if !self.at_boundary() {
            return Err(Error::Request("sweep rebind outside a pass boundary"));
        }
        let passes = self.plan.passes_completed();
        self.report.passes_completed += passes;
        self.plan = SweepPlan::new(
            zones,
            DEFAULT_SPAN_BYTES,
            self.seed ^ passes.rotate_left(29),
        )
        .map_err(Error::Request)?;
        self.report.zone_bytes = self.plan.zone_bytes();
        self.report.zone_swaps += 1;
        Ok(())
    }

    pub(crate) fn report(&self) -> SweepReport {
        let mut report = self.report.clone();
        report.passes_completed += self.plan.passes_completed();
        report
    }

    pub(crate) fn take_failures(&mut self) -> Vec<LaneFailure> {
        std::mem::take(&mut self.failures)
    }

    /// Observes the running launch and starts the next one when allowed. `allow` is false while
    /// draining, stopping or waiting for a zone swap; a running launch is still completed.
    pub(crate) fn poll(&mut self, backend: &CudaBackend, allow: bool) -> Result<()> {
        if self.running.is_some() && !self.complete()? {
            return Ok(());
        }
        let now = Instant::now();
        if !allow || now < self.not_before {
            return Ok(());
        }
        let checkpoint_due = self.plan.at_pass_boundary()
            && self.last_checkpoint.elapsed()
                >= Duration::from_millis(self.config.checkpoint_interval_ms);
        if checkpoint_due
            && let Some((allocation, offset, bytes, key)) = self.plan.next_checkpoint()
        {
            self.last_checkpoint = now;
            return self.launch_checkpoint(backend, allocation, offset, bytes, key);
        }
        let step = self.plan.next_step();
        self.launch_step(backend, step)
    }

    /// Completes the pass under way (its remaining fills and every verify), so nothing the sweep
    /// wrote is left unchecked at the end of a run. Duty-cycle pauses are skipped.
    pub(crate) fn finish_pass(&mut self, backend: &CudaBackend) -> Result<()> {
        self.drain()?;
        while !self.plan.at_pass_boundary() {
            let step = self.plan.next_step();
            self.launch_step(backend, step)?;
            self.drain()?;
        }
        Ok(())
    }

    /// Waits (bounded) for the running launch; used before a swap and at the end of a run.
    pub(crate) fn drain(&mut self) -> Result<()> {
        let started = Instant::now();
        while self.running.is_some() {
            if self.complete()? {
                break;
            }
            if started.elapsed() > Duration::from_secs(30) {
                self.context.stopped.set(true);
                return Err(Error::Request("sweep did not finish within 30 s"));
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        Ok(())
    }

    fn device_range(
        backend: &CudaBackend,
        allocation: usize,
        offset: u64,
        bytes: u64,
    ) -> Result<u64> {
        let buffer = backend
            .pool
            .get(allocation)
            .ok_or(Error::Request("sweep allocation"))?;
        if offset + bytes > buffer.len as u64 {
            return Err(Error::Request("sweep range"));
        }
        Ok(buffer.address + offset)
    }

    fn launch_step(&mut self, backend: &CudaBackend, step: SweepStep) -> Result<()> {
        let driver = &self.context.driver;
        let mut address = Self::device_range(backend, step.allocation, step.offset, step.bytes)?;
        let mut first = step.first_word();
        let mut vectors = step.bytes / 16;
        let mut key = step.key;
        self.in_flight = true;
        check("sweep start", unsafe {
            (driver.event_record)(self.events[0], self.stream)
        })?;
        match step.phase {
            SweepPhase::Fill => {
                let mut params = [
                    (&raw mut address).cast::<c_void>(),
                    (&raw mut first).cast(),
                    (&raw mut vectors).cast(),
                    (&raw mut key).cast(),
                ];
                check("sweep fill", unsafe {
                    (driver.launch)(
                        backend.sweep_fill_function,
                        self.grid,
                        1,
                        1,
                        THREADS,
                        1,
                        1,
                        0,
                        self.stream,
                        params.as_mut_ptr(),
                        ptr::null_mut(),
                    )
                })?;
            }
            SweepPhase::Verify => {
                if self.fault == Some(LaneFault::SweepVerify) {
                    // As damaged memory would: every word differs from what the check expects.
                    key ^= 1;
                    self.fault = None;
                }
                check("sweep counter reset", unsafe {
                    (driver.fill)(self.counter.address, 0, 1, self.stream)
                })?;
                let mut counter = self.counter.address;
                let mut params = [
                    (&raw mut address).cast::<c_void>(),
                    (&raw mut first).cast(),
                    (&raw mut vectors).cast(),
                    (&raw mut key).cast(),
                    (&raw mut counter).cast(),
                ];
                check("sweep verify", unsafe {
                    (driver.launch)(
                        backend.sweep_verify_function,
                        self.grid,
                        1,
                        1,
                        THREADS,
                        1,
                        1,
                        0,
                        self.stream,
                        params.as_mut_ptr(),
                        ptr::null_mut(),
                    )
                })?;
            }
        }
        check("sweep end", unsafe {
            (driver.event_record)(self.events[1], self.stream)
        })?;
        self.running = Some(Running::Step(step));
        Ok(())
    }

    fn launch_checkpoint(
        &mut self,
        backend: &CudaBackend,
        allocation: usize,
        offset: u64,
        bytes: u64,
        key: u32,
    ) -> Result<()> {
        let driver = &self.context.driver;
        let device = Self::device_range(backend, allocation, offset, bytes)?;
        let len = usize::try_from(bytes).map_err(|_| Error::Request("checkpoint size"))?;
        self.in_flight = true;
        check("sweep start", unsafe {
            (driver.event_record)(self.events[0], self.stream)
        })?;
        check("sweep checkpoint D2H", unsafe {
            (driver.download)(self.checkpoint.address, device, len, self.stream)
        })?;
        check("sweep end", unsafe {
            (driver.event_record)(self.events[1], self.stream)
        })?;
        self.running = Some(Running::Checkpoint {
            allocation,
            offset,
            bytes,
            key,
        });
        Ok(())
    }

    /// True when the running launch finished and was accounted for.
    fn complete(&mut self) -> Result<bool> {
        let driver = &self.context.driver;
        match unsafe { (driver.event_query)(self.events[1]) } {
            0 => {}
            600 => return Ok(false), // CUDA_ERROR_NOT_READY
            status => {
                self.context.stopped.set(true);
                return Err(Error::Api {
                    call: "sweep event",
                    status,
                });
            }
        }
        self.in_flight = false;
        let mut milliseconds = 0.0_f32;
        check("sweep timing", unsafe {
            (driver.event_elapsed)(&raw mut milliseconds, self.events[0], self.events[1])
        })?;
        let busy = f64::from(milliseconds) / 1000.0;
        // The counter copy is bookkeeping, not device work: it neither counts nor delays.
        if !matches!(self.running, Some(Running::Counter(_))) {
            self.not_before = Instant::now() + Duration::from_secs_f64(idle_after(busy, self.duty));
        }
        match self.running.take() {
            Some(Running::Step(step)) => {
                self.report.kernel_event_seconds += busy;
                match step.phase {
                    SweepPhase::Fill => self.report.fill_bytes += step.bytes,
                    SweepPhase::Verify => {
                        // The kernel is done: now the counter copy runs at once.
                        self.in_flight = true;
                        check("sweep counter read", unsafe {
                            (driver.download)(
                                self.flag.address,
                                self.counter.address,
                                4,
                                self.stream,
                            )
                        })?;
                        check("sweep counter end", unsafe {
                            (driver.event_record)(self.events[1], self.stream)
                        })?;
                        self.running = Some(Running::Counter(step));
                        return Ok(false);
                    }
                }
            }
            Some(Running::Counter(step)) => self.account_verify(step),
            Some(Running::Checkpoint {
                allocation,
                offset,
                bytes,
                key,
            }) => self.account_checkpoint(allocation, offset, bytes, key)?,
            None => {}
        }
        Ok(true)
    }

    /// A verify kernel's mismatch counter has landed: device-verified bytes, or a failure.
    fn account_verify(&mut self, step: SweepStep) {
        // SAFETY: the end event completed the counter's D2H.
        let raw = unsafe { self.flag.completed_bytes(4) };
        let bad = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if bad == 0 {
            self.report.device_verified_bytes += step.bytes;
            return;
        }
        self.report.bad_words += u64::from(bad);
        self.fail(
            "vram-sweep",
            step.pass,
            step.allocation,
            step.offset,
            step.bytes,
            format!(
                "{bad} word(s) differ on the device (pass key {:#010x})",
                step.key
            ),
        );
    }

    /// A checkpoint read-back has landed: the CPU compares it with the host-regenerated pattern.
    fn account_checkpoint(
        &mut self,
        allocation: usize,
        offset: u64,
        bytes: u64,
        key: u32,
    ) -> Result<()> {
        let len = usize::try_from(bytes).map_err(|_| Error::Request("checkpoint size"))?;
        let mut expected = vec![0_u8; len];
        sweep_pattern_into(&mut expected, key, offset / 4);
        if self.fault == Some(LaneFault::SweepCheckpoint) {
            self.checkpoint.corrupt_first_byte();
            self.fault = None;
        }
        // SAFETY: the end event completed the D2H; nothing writes the buffer until the next
        // checkpoint launch, which this owner thread issues after this returns.
        let actual = unsafe { self.checkpoint.completed_bytes(len) };
        self.report.checkpoint_ops += 1;
        match first_difference(&expected, actual) {
            None => self.report.checkpoint_cpu_verified_bytes += bytes,
            Some(index) => self.fail(
                "vram-sweep-checkpoint",
                self.plan.passes_completed(),
                allocation,
                offset,
                bytes,
                format!(
                    "CPU checkpoint differs at byte {index}: expected {:#04x}, got {:#04x}",
                    expected[index], actual[index]
                ),
            ),
        }
        Ok(())
    }

    fn fail(
        &mut self,
        phase: &str,
        pass: u64,
        allocation: usize,
        offset: u64,
        bytes: u64,
        detail: String,
    ) {
        self.report.failures_total += 1;
        if self.failures.len() < 16 {
            self.failures.push(LaneFailure {
                phase: phase.to_owned(),
                generation: pass,
                seed: self.seed,
                allocation,
                offset,
                bytes,
                detail,
            });
        }
    }
}

impl Drop for Sweeper {
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
