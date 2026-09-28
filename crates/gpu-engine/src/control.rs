//! GPU-H5 load controller: a deterministic state machine driven by explicit samples and a
//! caller-supplied clock, so every decision can be replayed from a trace.
//!
//! The controlled quantity is the **verifier backlog**: outputs already copied to the host
//! and waiting for, or under, a CPU verdict. Batches still running on the GPU are not a
//! backlog and never count as overload; GPU busyness is not a control input at all.
//!
//! Levers, in order of use: `in_flight_limit` (how many batches may be on the GPU) and a
//! submit *pacing* interval taken from the measured verdict rate, so that under a slow
//! verifier the batch creation rate converges on the verify rate instead of cycling between
//! full stop and full speed. The batch shape (K, jobs) is never reduced to shed load.
use serde::Serialize;
use std::collections::VecDeque;

const MAX_DECISIONS: usize = 64;
const MAX_PACING_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ControlState {
    Warmup,
    RampUp,
    Balanced,
    Throttled,
    Draining,
    Stopped,
}

/// Fraction of the *waiting room* in use: batches beyond those being verified, over the host
/// slots that are not verifier workers. A worker busy with a batch is service, not backlog.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn waiting_fill(host_backlog: usize, workers: usize, host_slots: usize) -> f64 {
    let room = host_slots.saturating_sub(workers).max(1);
    host_backlog.saturating_sub(workers) as f64 / room as f64
}

/// Thresholds are fractions of the host queue (backlog / host slots). They are the roadmap's
/// starting points, not validated defaults; hysteresis (`resume` well below `high`), the
/// cooldown and one-step changes keep the controller from oscillating around them.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct ControlConfig {
    pub device_slots: usize,
    pub host_slots: usize,
    /// Decision window. Samples inside a window are averaged before the controller acts.
    pub tick_ms: u64,
    /// Windows that must pass between two rate changes.
    pub cooldown_ticks: u32,
    pub warm_limit: usize,
    pub warm_verdicts: u64,
    /// Ramp up below this backlog fraction.
    pub low: f64,
    /// Throttle at or above this fraction.
    pub high: f64,
    /// Stop submitting at or above this fraction (or when the queue is full and half backlog).
    pub hard: f64,
    /// Recover only below this fraction.
    pub resume: f64,
    /// A rising trend throttles once the fraction is at least this.
    pub rising_floor: f64,
    /// Minimum increase between consecutive windows to count as rising.
    pub rising_step: f64,
    pub stall_ms: u64,
    pub pressure_stop_ms: u64,
    /// A recovery step needs this many consecutive calm windows, and the requirement doubles
    /// (up to `recover_hold_max_ticks`) whenever a step is followed by a new throttle soon after.
    pub recover_hold_ticks: u32,
    pub recover_hold_max_ticks: u32,
}

impl ControlConfig {
    #[must_use]
    pub fn new(device_slots: usize, host_slots: usize) -> Self {
        Self {
            device_slots: device_slots.max(1),
            host_slots: host_slots.max(2),
            tick_ms: 50,
            cooldown_ticks: 2,
            warm_limit: device_slots.clamp(1, 2),
            warm_verdicts: device_slots as u64,
            low: 0.30,
            high: 0.70,
            hard: 0.90,
            resume: 0.40,
            rising_floor: 0.50,
            rising_step: 0.03,
            stall_ms: 30_000,
            pressure_stop_ms: 30_000,
            recover_hold_ticks: 10,
            recover_hold_max_ticks: 160,
        }
    }
}

/// One observation of the pipeline. `verdicts` is cumulative (verified + failed) and must
/// not decrease within a controller's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Sample {
    pub now_ms: u64,
    /// Outputs copied to the host and not yet finished: waiting plus under verification.
    pub host_backlog: usize,
    /// Verifier workers running; the first `workers` backlog entries are service, not waiting.
    pub workers: usize,
    pub host_used: usize,
    pub device_in_flight: usize,
    pub verdicts: u64,
    /// Cumulative batches submitted; the spacing of submissions is derived from it, so a trace
    /// of samples alone reproduces every decision.
    pub submits: u64,
    pub pressure: bool,
}

/// A state or rate change together with the evidence that caused it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Decision {
    pub at_ms: u64,
    pub from: ControlState,
    pub to: ControlState,
    pub reason: &'static str,
    pub in_flight_limit: usize,
    pub pacing_ms: u64,
    /// Window-mean backlog fraction and whether it was rising.
    pub backlog_fill: f64,
    pub rising: bool,
    pub verdicts_per_second: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LoadController {
    pub config: ControlConfig,
    pub state: ControlState,
    pub reason: &'static str,
    pub in_flight_limit: usize,
    pub pacing_ms: u64,
    pub throttle_events: u64,
    pub recovery_events: u64,
    pub state_changes: u64,
    /// Milliseconds spent in each state, indexed like [`ControlState`].
    pub dwell_ms: [u64; 6],
    decisions: VecDeque<Decision>,
    #[serde(skip)]
    window: Window,
    #[serde(skip)]
    fills: VecDeque<f64>,
    #[serde(skip)]
    last_state_ms: u64,
    #[serde(skip)]
    ticks_since_action: u32,
    #[serde(skip)]
    warm_ticks: u32,
    #[serde(skip)]
    last_progress_ms: u64,
    #[serde(skip)]
    last_verdicts: u64,
    #[serde(skip)]
    last_submits: u64,
    #[serde(skip)]
    fresh: bool,
    #[serde(skip)]
    pressure_since: Option<u64>,
    #[serde(skip)]
    last_submit_ms: Option<u64>,
    #[serde(skip)]
    episode_active: bool,
    /// Smoothed verdicts per second, the basis of the pacing target.
    verdicts_per_second: f64,
    #[serde(skip)]
    calm_ticks: u32,
    #[serde(skip)]
    hold_ticks: u32,
    #[serde(skip)]
    ticks_total: u64,
    #[serde(skip)]
    last_recover_tick: Option<u64>,
    /// Smoothed spacing of actual submissions: tells whether pacing is what limits the rate.
    #[serde(skip)]
    submit_gap_ms: f64,
}

#[derive(Clone, Debug, Default)]
struct Window {
    start_ms: u64,
    sum: f64,
    count: u32,
    verdicts_at_start: u64,
    progressed: bool,
}

fn index(state: ControlState) -> usize {
    match state {
        ControlState::Warmup => 0,
        ControlState::RampUp => 1,
        ControlState::Balanced => 2,
        ControlState::Throttled => 3,
        ControlState::Draining => 4,
        ControlState::Stopped => 5,
    }
}

impl LoadController {
    #[must_use]
    pub fn new(config: ControlConfig) -> Self {
        let warm = config.warm_limit.clamp(1, config.device_slots);
        Self {
            config,
            state: ControlState::Warmup,
            reason: "warmup",
            in_flight_limit: warm,
            pacing_ms: 0,
            throttle_events: 0,
            recovery_events: 0,
            state_changes: 0,
            dwell_ms: [0; 6],
            decisions: VecDeque::new(),
            window: Window::default(),
            fills: VecDeque::new(),
            last_state_ms: 0,
            ticks_since_action: 0,
            warm_ticks: 0,
            last_progress_ms: 0,
            last_verdicts: 0,
            last_submits: 0,
            fresh: true,
            pressure_since: None,
            last_submit_ms: None,
            episode_active: false,
            verdicts_per_second: 0.0,
            calm_ticks: 0,
            hold_ticks: config.recover_hold_ticks,
            ticks_total: 0,
            last_recover_tick: None,
            submit_gap_ms: 0.0,
        }
    }

    /// Call at the start of every compute burst. Queue history, the trend and the stall clock
    /// belong to one burst: time spent in transfers between bursts is not lack of progress.
    pub fn begin_burst(&mut self, now_ms: u64) {
        self.window = Window {
            start_ms: now_ms,
            ..Window::default()
        };
        self.fills.clear();
        self.last_progress_ms = now_ms;
        self.pressure_since = None;
        self.fresh = true;
        self.account_dwell(now_ms);
    }

    fn account_dwell(&mut self, now_ms: u64) {
        let spent = now_ms.saturating_sub(self.last_state_ms);
        self.dwell_ms[index(self.state)] += spent;
        self.last_state_ms = now_ms;
    }

    /// May another batch be submitted now?
    #[must_use]
    pub fn permits(&self, device_in_flight: usize, now_ms: u64) -> bool {
        !matches!(self.state, ControlState::Draining | ControlState::Stopped)
            && device_in_flight < self.in_flight_limit
            && self
                .last_submit_ms
                .is_none_or(|t| now_ms >= t.saturating_add(self.pacing_ms))
    }

    #[allow(clippy::cast_precision_loss)]
    fn note_submit(&mut self, now_ms: u64) {
        if let Some(previous) = self.last_submit_ms {
            let gap = now_ms.saturating_sub(previous) as f64;
            // A gap across a burst boundary is not a rate; ignore anything above the pacing cap.
            if gap <= MAX_PACING_MS as f64 {
                self.submit_gap_ms = if self.submit_gap_ms == 0.0 {
                    gap
                } else {
                    0.8 * self.submit_gap_ms + 0.2 * gap
                };
            }
        }
        self.last_submit_ms = Some(now_ms);
    }

    /// True while the controller is holding the load down: paced or throttled because of the
    /// verifier backlog. Used to label a burst as verifier-bound even when its queue is short,
    /// which is exactly what a well-paced verifier-bound run looks like.
    #[must_use]
    pub fn is_holding_load(&self) -> bool {
        self.state == ControlState::Throttled
            && (self.pacing_ms > 0 || self.in_flight_limit < self.config.device_slots)
    }

    /// The verifiers gained capacity (`old_workers` -> `new_workers`). The pacing is relaxed in
    /// proportion to the capacity actually added, at once instead of waiting for a calm window,
    /// and earlier premature recoveries are forgotten so the gain can show in the throughput.
    pub fn capacity_increased(&mut self, old_workers: usize, new_workers: usize) {
        if new_workers > old_workers && old_workers > 0 {
            self.pacing_ms = self.pacing_ms * old_workers as u64 / new_workers as u64;
        }
        self.hold_ticks = self.config.recover_hold_ticks;
        self.calm_ticks = 0;
        self.last_recover_tick = None;
    }

    /// Most recent decisions, oldest first (bounded).
    #[must_use]
    pub fn decisions(&self) -> Vec<Decision> {
        self.decisions.iter().cloned().collect()
    }

    #[allow(clippy::cast_precision_loss)]
    fn backlog_fill(&self, s: &Sample) -> f64 {
        waiting_fill(s.host_backlog, s.workers, self.config.host_slots)
    }

    #[allow(clippy::cast_precision_loss)]
    fn transition(
        &mut self,
        now_ms: u64,
        to: ControlState,
        reason: &'static str,
        fill: f64,
        rising: bool,
        rate: f64,
    ) {
        let from = self.state;
        self.account_dwell(now_ms);
        if from != to {
            self.state_changes += 1;
        }
        self.state = to;
        self.reason = reason;
        self.ticks_since_action = 0;
        if self.decisions.len() == MAX_DECISIONS {
            self.decisions.pop_front();
        }
        self.decisions.push_back(Decision {
            at_ms: now_ms,
            from,
            to,
            reason,
            in_flight_limit: self.in_flight_limit,
            pacing_ms: self.pacing_ms,
            backlog_fill: fill,
            rising,
            verdicts_per_second: rate,
        });
    }

    /// Pacing that matches the smoothed verdict rate (submit no faster than verdicts arrive).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn pacing_target(&self) -> Option<u64> {
        (self.verdicts_per_second > 0.0).then(|| {
            (((1000.0 / self.verdicts_per_second) * 1.05).ceil() as u64).clamp(1, MAX_PACING_MS)
        })
    }

    /// A throttle or hard cap soon after a recovery step means the step was premature.
    fn note_if_premature(&mut self) {
        if self
            .last_recover_tick
            .is_some_and(|t| self.ticks_total.saturating_sub(t) <= u64::from(self.hold_ticks) * 4)
        {
            self.hold_ticks = (self.hold_ticks * 2).min(self.config.recover_hold_max_ticks);
        }
    }

    /// One load-reducing step. Acts (and counts as a throttle event) only when it changes
    /// something: the in-flight limit, or the pacing by more than a tenth.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn reduce_load(&mut self, now_ms: u64, reason: &'static str, fill: f64, rising: bool) {
        let rate = self.verdicts_per_second;
        let target = self.pacing_target().unwrap_or(self.pacing_ms);
        let new_limit = self.in_flight_limit.saturating_sub(1).max(1);
        // Pacing moves toward the target by at most 25% per step.
        let new_pacing = if self.pacing_ms == 0 {
            target
        } else {
            let lo = self.pacing_ms - self.pacing_ms / 4;
            let hi = self.pacing_ms + (self.pacing_ms / 4).max(1);
            target.clamp(lo, hi)
        };
        let pacing_moves = new_pacing.abs_diff(self.pacing_ms) * 10 > self.pacing_ms.max(1);
        let changes = new_limit != self.in_flight_limit || pacing_moves;
        if self.state == ControlState::Throttled && !changes {
            return;
        }
        self.throttle_events += 1;
        self.note_if_premature();
        self.episode_active = true;
        self.calm_ticks = 0;
        self.in_flight_limit = new_limit;
        self.pacing_ms = new_pacing;
        self.transition(now_ms, ControlState::Throttled, reason, fill, rising, rate);
    }

    fn stop(&mut self, now_ms: u64, reason: &'static str, fill: f64) {
        self.transition(now_ms, ControlState::Stopped, reason, fill, false, 0.0);
    }

    /// Feed one sample. Returns the decision it caused, if any. Hard-cap, memory-pressure and
    /// stall rules act on the sample itself; every other rule acts once per closed window.
    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    pub fn observe(&mut self, s: &Sample) -> Option<Decision> {
        if self.state == ControlState::Stopped {
            return None;
        }
        let before = self.decisions.len();
        let fill = self.backlog_fill(s);
        if self.fresh {
            self.fresh = false;
            self.last_verdicts = s.verdicts;
            self.last_submits = s.submits;
            self.window.verdicts_at_start = s.verdicts;
            self.last_progress_ms = s.now_ms;
        }
        if s.submits > self.last_submits {
            self.note_submit(s.now_ms);
            self.last_submits = s.submits;
        }
        if s.verdicts > self.last_verdicts {
            self.last_progress_ms = s.now_ms;
            self.last_verdicts = s.verdicts;
            self.window.progressed = true;
        }
        let busy = s.device_in_flight + s.host_backlog > 0;
        if busy && s.now_ms.saturating_sub(self.last_progress_ms) >= self.config.stall_ms {
            self.stop(s.now_ms, "stalled", fill);
            return self.decisions.back().cloned();
        }
        if s.pressure {
            let since = *self.pressure_since.get_or_insert(s.now_ms);
            if s.now_ms.saturating_sub(since) >= self.config.pressure_stop_ms {
                self.stop(s.now_ms, "host_memory_pressure_sustained", fill);
                return self.decisions.back().cloned();
            }
        } else {
            self.pressure_since = None;
        }
        let hard =
            fill >= self.config.hard || (s.host_used >= self.config.host_slots && fill >= 0.5);
        if (s.pressure || hard) && self.state != ControlState::Draining {
            self.throttle_events += 1;
            self.episode_active = true;
            self.calm_ticks = 0;
            self.note_if_premature();
            self.in_flight_limit = 1;
            let reason = if s.pressure {
                "host_memory_pressure"
            } else {
                "queue_hard_cap"
            };
            self.transition(s.now_ms, ControlState::Draining, reason, fill, false, 0.0);
        }
        // Accumulate the window and act once per closed window.
        if self.window.count == 0 && self.window.start_ms == 0 && s.now_ms > 0 {
            self.window.start_ms = s.now_ms;
        }
        self.window.sum += fill;
        self.window.count += 1;
        if s.now_ms.saturating_sub(self.window.start_ms) >= self.config.tick_ms {
            self.close_window(s);
        }
        (self.decisions.len() > before)
            .then(|| self.decisions.back().cloned())
            .flatten()
    }

    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    fn close_window(&mut self, s: &Sample) {
        let w = std::mem::take(&mut self.window);
        let elapsed = s.now_ms.saturating_sub(w.start_ms).max(1);
        let mean = if w.count == 0 {
            0.0
        } else {
            w.sum / f64::from(w.count)
        };
        let rate = s.verdicts.saturating_sub(w.verdicts_at_start) as f64 * 1000.0 / elapsed as f64;
        self.window = Window {
            start_ms: s.now_ms,
            verdicts_at_start: s.verdicts,
            ..Window::default()
        };
        if rate > 0.0 {
            self.verdicts_per_second = if self.verdicts_per_second == 0.0 {
                rate
            } else {
                0.7 * self.verdicts_per_second + 0.3 * rate
            };
        }
        self.fills.push_back(mean);
        if self.fills.len() > 4 {
            self.fills.pop_front();
        }
        self.ticks_since_action = self.ticks_since_action.saturating_add(1);
        self.ticks_total += 1;
        let step = self.config.rising_step;
        let rising = self.fills.len() >= 3 && {
            let n = self.fills.len();
            self.fills[n - 1] - self.fills[n - 2] >= step
                && self.fills[n - 2] - self.fills[n - 3] >= step
        };
        // Recovery needs verdicts arriving (or nothing outstanding) and no memory pressure.
        // Progress is judged over a horizon of many windows: batches often arrive further apart
        // than one window, so a single window without a verdict says nothing.
        let horizon = (self.config.tick_ms * 10).max(500);
        let progressing = w.progressed
            || s.now_ms.saturating_sub(self.last_progress_ms) < horizon
            || (s.device_in_flight + s.host_backlog == 0);
        if mean < self.config.resume && !rising && progressing && !s.pressure {
            self.calm_ticks = self.calm_ticks.saturating_add(1);
        } else {
            self.calm_ticks = 0;
        }
        let cooled = self.ticks_since_action >= self.config.cooldown_ticks;
        let cfg = self.config;
        let now = s.now_ms;
        match self.state {
            ControlState::Stopped => {}
            ControlState::Warmup => {
                self.warm_ticks += 1;
                if mean >= cfg.high {
                    self.reduce_load(now, "verifier_backlog", mean, rising);
                } else if s.verdicts >= cfg.warm_verdicts || self.warm_ticks >= 4 {
                    self.in_flight_limit = self.in_flight_limit.max(cfg.warm_limit);
                    let next = if self.in_flight_limit < cfg.device_slots {
                        ControlState::RampUp
                    } else {
                        ControlState::Balanced
                    };
                    self.transition(now, next, "warmup_done", mean, rising, rate);
                }
            }
            ControlState::Draining => {
                if !s.pressure && mean < cfg.resume && progressing {
                    // Leave the full stop gently: one batch at a time, paced.
                    self.in_flight_limit = 1;
                    if let Some(target) = self.pacing_target() {
                        // Restart no faster than the verifiers were seen to deliver.
                        self.pacing_ms = target;
                    }
                    self.last_recover_tick = Some(self.ticks_total);
                    self.transition(
                        now,
                        ControlState::Throttled,
                        "recovering",
                        mean,
                        rising,
                        rate,
                    );
                }
            }
            ControlState::Throttled => {
                if s.pressure {
                    // Stay throttled; the sample-level rule already drained if needed.
                } else if cooled && (mean >= cfg.high || (rising && mean >= cfg.rising_floor)) {
                    let reason = if mean >= cfg.high {
                        "verifier_backlog"
                    } else {
                        "backlog_rising"
                    };
                    self.reduce_load(now, reason, mean, rising);
                } else if cooled && self.calm_ticks >= self.hold_ticks {
                    self.calm_ticks = 0;
                    self.recover_step(now, mean, rising, rate);
                }
            }
            ControlState::RampUp | ControlState::Balanced => {
                if cooled && (mean >= cfg.high || (rising && mean >= cfg.rising_floor)) {
                    let reason = if mean >= cfg.high {
                        "verifier_backlog"
                    } else {
                        "backlog_rising"
                    };
                    self.reduce_load(now, reason, mean, rising);
                } else if cooled
                    && mean < cfg.low
                    && !rising
                    && self.in_flight_limit < cfg.device_slots
                {
                    self.in_flight_limit += 1;
                    let next = if self.in_flight_limit < cfg.device_slots {
                        ControlState::RampUp
                    } else {
                        ControlState::Balanced
                    };
                    self.transition(now, next, "capacity_available", mean, rising, rate);
                }
            }
        }
    }

    /// One recovery step: pacing relaxes first, then the in-flight limit grows; the episode
    /// ends (and counts as a recovery) only when neither lever is restricted any more.
    #[allow(clippy::cast_precision_loss)]
    fn recover_step(&mut self, now: u64, mean: f64, rising: bool, rate: f64) {
        // A step that stayed calm for well beyond the hold earns a shorter hold next time, so an
        // overload that has really passed is not paid for with a long recovery.
        let stable = self
            .last_recover_tick
            .is_none_or(|t| self.ticks_total.saturating_sub(t) > u64::from(self.hold_ticks) * 4);
        if stable {
            self.hold_ticks = (self.hold_ticks / 2).max(self.config.recover_hold_ticks);
        }
        if self.pacing_ms > 0 {
            // If submissions are already spaced wider than the pacing (the GPU is the limit),
            // the pacing does nothing and is dropped at once. Otherwise it is what holds the
            // load down, so it is relaxed one gentle step at a time to probe for capacity.
            // Only a clear gap (submissions at least twice as far apart as the pacing) proves that
            // the pacing is idle; anything closer means it is what holds the rate down.
            let binding = self.pacing_ms as f64 >= 0.5 * self.submit_gap_ms;
            self.pacing_ms = if !binding || self.pacing_ms < 3 {
                0
            } else {
                self.pacing_ms * 3 / 4
            };
        } else if self.in_flight_limit < self.config.device_slots {
            self.in_flight_limit += 1;
        }
        self.last_recover_tick = Some(self.ticks_total);
        let restored = self.pacing_ms == 0 && self.in_flight_limit >= self.config.device_slots;
        if restored {
            self.hold_ticks = self.config.recover_hold_ticks;
            if self.episode_active {
                self.recovery_events += 1;
                self.episode_active = false;
            }
            self.transition(now, ControlState::Balanced, "recovered", mean, rising, rate);
        } else {
            self.transition(
                now,
                ControlState::Throttled,
                "recovering",
                mean,
                rising,
                rate,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ControlConfig {
        ControlConfig::new(3, 8)
    }

    fn sample(now_ms: u64, backlog: usize, in_flight: usize, verdicts: u64) -> Sample {
        Sample {
            now_ms,
            host_backlog: backlog,
            workers: 1,
            host_used: backlog + in_flight,
            device_in_flight: in_flight,
            verdicts,
            submits: 0,
            pressure: false,
        }
    }

    /// A closed-loop model: the GPU makes a batch every `produce_ms` while the controller
    /// permits it, and `workers` verifiers each take `verify_ms` per batch.
    struct Sim {
        controller: LoadController,
        now: u64,
        backlog: usize,
        in_flight: Vec<u64>,
        verifying: Vec<u64>,
        verdicts: u64,
        submits: u64,
        produce_ms: u64,
        verify_ms: u64,
        workers: usize,
        max_backlog: usize,
        samples: Vec<Sample>,
        cap_events: u64,
    }

    impl Sim {
        fn new(produce_ms: u64, verify_ms: u64, workers: usize) -> Self {
            let mut controller = LoadController::new(config());
            controller.begin_burst(1);
            Self {
                controller,
                now: 1,
                backlog: 0,
                in_flight: Vec::new(),
                verifying: Vec::new(),
                verdicts: 0,
                submits: 0,
                produce_ms,
                verify_ms,
                workers,
                max_backlog: 0,
                samples: Vec::new(),
                cap_events: 0,
            }
        }

        fn step(&mut self) {
            self.now += 1;
            // GPU batches finish their kernel + copy after `produce_ms`.
            let due: Vec<u64> = self
                .in_flight
                .iter()
                .copied()
                .filter(|t| *t <= self.now)
                .collect();
            self.in_flight.retain(|t| *t > self.now);
            self.backlog += due.len();
            // Verifiers take from the backlog whenever a worker is free.
            self.verifying.retain(|t| {
                if *t <= self.now {
                    self.verdicts += 1;
                    false
                } else {
                    true
                }
            });
            while self.verifying.len() < self.workers && self.backlog > 0 {
                self.backlog -= 1;
                self.verifying.push(self.now + self.verify_ms);
            }
            let used = self.backlog + self.verifying.len() + self.in_flight.len();
            if used < 8 && self.controller.permits(self.in_flight.len(), self.now) {
                self.in_flight.push(self.now + self.produce_ms);
                self.submits += 1;
            }
            let sample = Sample {
                now_ms: self.now,
                host_backlog: self.backlog + self.verifying.len(),
                workers: self.workers,
                host_used: self.backlog + self.verifying.len() + self.in_flight.len(),
                device_in_flight: self.in_flight.len(),
                verdicts: self.verdicts,
                submits: self.submits,
                pressure: false,
            };
            self.max_backlog = self.max_backlog.max(sample.host_backlog);
            self.samples.push(sample);
            if let Some(d) = self.controller.observe(&sample)
                && d.reason == "queue_hard_cap"
            {
                self.cap_events += 1;
            }
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms {
                self.step();
            }
        }
    }

    #[test]
    fn gpu_bound_trace_never_throttles_and_ramps_to_full_overlap() {
        // Verifiers are 4x faster than the GPU: the backlog stays near zero.
        let mut sim = Sim::new(25, 5, 2);
        sim.run(10_000);
        let c = &sim.controller;
        assert_eq!(c.throttle_events, 0, "{:?}", c.decisions());
        assert_eq!(c.state, ControlState::Balanced);
        assert_eq!(c.in_flight_limit, 3);
        assert_eq!(c.pacing_ms, 0);
        assert!(sim.max_backlog <= 3);
        let states: Vec<_> = c.decisions().iter().map(|d| d.to).collect();
        assert_eq!(states, [ControlState::RampUp, ControlState::Balanced]);
    }

    #[test]
    fn warm_start_keeps_overlap_from_the_first_batch() {
        let c = LoadController::new(config());
        assert_eq!((c.state, c.in_flight_limit), (ControlState::Warmup, 2));
        assert!(c.permits(1, 0), "a second batch may overlap right away");
        assert!(!c.permits(2, 0));
        let single = LoadController::new(ControlConfig::new(1, 4));
        assert_eq!(single.in_flight_limit, 1);
    }

    #[test]
    fn verifier_bound_trace_throttles_settles_and_never_hits_the_hard_cap() {
        // The GPU could deliver a batch every 10 ms; one worker verifies one per 30 ms.
        let mut sim = Sim::new(10, 30, 1);
        sim.run(30_000);
        let c = &sim.controller;
        assert!(c.throttle_events >= 1);
        assert!(sim.max_backlog <= 8, "the queue is never overrun");
        assert!(
            c.state_changes <= 14,
            "no flapping: {} changes",
            c.state_changes
        );
        // Pacing converges on the verify interval instead of duty-cycling full stop / full speed.
        assert!((25..=45).contains(&c.pacing_ms), "pacing {}", c.pacing_ms);
        let tail = &sim.samples[sim.samples.len() - 5_000..];
        let (lo, hi) = tail.iter().fold((usize::MAX, 0), |(lo, hi), s| {
            (lo.min(s.host_backlog), hi.max(s.host_backlog))
        });
        assert!(
            hi <= 7 && hi - lo <= 6,
            "backlog stays inside the queue in the tail: {lo}..{hi}"
        );
        // The verifier still ran flat out: no output was dropped or skipped.
        assert!(sim.verdicts >= 30_000 / 30 - 3);
        assert!(sim.cap_events <= 1);
    }

    #[test]
    fn short_overload_recovers_and_counts_one_episode() {
        // Fast production for 3 s against a slow verifier, then the load disappears.
        let mut sim = Sim::new(5, 40, 1);
        sim.run(3_000);
        assert!(sim.controller.throttle_events >= 1);
        sim.produce_ms = 60; // the GPU now produces less than the verifier can absorb
        sim.verify_ms = 20;
        sim.run(30_000);
        let c = &sim.controller;
        assert_eq!(
            c.state,
            ControlState::Balanced,
            "{:?}",
            c.decisions().last()
        );
        assert_eq!(c.recovery_events, 1);
        assert_eq!((c.in_flight_limit, c.pacing_ms), (3, 0));
    }

    #[test]
    fn hard_cap_acts_on_the_sample_without_waiting_for_a_window() {
        let mut c = LoadController::new(config());
        c.begin_burst(10);
        assert!(c.observe(&sample(11, 1, 1, 0)).is_none());
        let d = c.observe(&sample(12, 8, 0, 0)).expect("immediate");
        assert_eq!((d.to, d.reason), (ControlState::Draining, "queue_hard_cap"));
        assert!(!c.permits(0, 13));
        // Leaves the full stop only once the backlog is below the resume level for a window.
        let mut t = 12;
        for backlog in [7, 6, 5, 4, 4] {
            t += 60;
            c.observe(&sample(t, backlog, 0, (8 - backlog as u64) * 3));
        }
        assert_eq!(
            c.state,
            ControlState::Draining,
            "still above the resume fraction"
        );
        for _ in 0..3 {
            t += 60;
            c.observe(&sample(t, 1, 0, 40 + t));
        }
        assert_eq!(c.state, ControlState::Throttled);
        assert_eq!(c.reason, "recovering");
        assert_eq!(c.in_flight_limit, 1, "restarts one batch at a time");
    }

    #[test]
    fn stall_is_measured_inside_the_burst_not_across_idle_time() {
        let mut c = LoadController::new(config());
        c.begin_burst(0);
        c.observe(&sample(10, 0, 1, 0));
        // Minutes pass in transfers with nothing outstanding: not a stall.
        c.begin_burst(300_000);
        c.observe(&sample(300_010, 0, 1, 0));
        assert_ne!(c.state, ControlState::Stopped);
        // A real stall inside a burst: work outstanding, no verdict for the deadline.
        for t in (300_100..=330_100).step_by(500) {
            c.observe(&sample(t, 2, 1, 0));
        }
        assert_eq!((c.state, c.reason), (ControlState::Stopped, "stalled"));
        assert!(!c.permits(0, 400_000));
        // Sticky: nothing revives it.
        assert!(c.observe(&sample(400_000, 0, 0, 99)).is_none());
    }

    #[test]
    fn memory_pressure_drains_recovers_and_sustained_pressure_stops() {
        let mut c = LoadController::new(config());
        c.begin_burst(0);
        let mut verdicts = 0;
        let mut t = 0;
        let mut feed = |c: &mut LoadController, pressure: bool, backlog: usize, ms: u64| {
            for _ in 0..ms / 10 {
                t += 10;
                verdicts += 1;
                c.observe(&Sample {
                    pressure,
                    ..sample(t, backlog, 0, verdicts)
                });
            }
        };
        feed(&mut c, false, 0, 500);
        feed(&mut c, true, 1, 200);
        assert_eq!(
            (c.state, c.reason),
            (ControlState::Draining, "host_memory_pressure")
        );
        feed(&mut c, false, 0, 500);
        assert!(
            c.decisions()
                .iter()
                .any(|d| d.to == ControlState::Throttled && d.reason == "recovering"),
            "pressure gone: a gentle restart through Throttled, not straight to full speed"
        );
        feed(&mut c, false, 0, 12_000);
        assert_eq!(c.state, ControlState::Balanced);
        assert_eq!(c.recovery_events, 1);
        // Pressure that never lifts is an error, not an endless throttle.
        feed(&mut c, true, 1, 31_000);
        assert_eq!(
            (c.state, c.reason),
            (ControlState::Stopped, "host_memory_pressure_sustained")
        );
    }

    #[test]
    fn noise_around_the_threshold_does_not_flap_the_controller() {
        let mut c = LoadController::new(config());
        c.begin_burst(0);
        let mut seed = 0x9E37_79B9_u64;
        let mut verdicts = 0;
        for t in (1..=60_000).step_by(5) {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            // Backlog jitters between 5 and 6 of 8 (62%..75%), straddling the throttle line.
            let backlog = 5 + usize::try_from((seed >> 33) % 2).unwrap();
            verdicts += 1;
            c.observe(&sample(t, backlog, 1, verdicts));
        }
        assert!(c.state_changes <= 8, "{} changes in 60 s", c.state_changes);
        assert!(
            c.throttle_events <= 6,
            "{} throttle events",
            c.throttle_events
        );
        assert!(c.in_flight_limit >= 1 && c.in_flight_limit <= 3);
        assert!(c.pacing_ms <= MAX_PACING_MS);
    }

    #[test]
    fn decisions_replay_identically_from_the_recorded_samples() {
        let mut sim = Sim::new(8, 25, 1);
        sim.run(8_000);
        sim.produce_ms = 40;
        sim.run(8_000);
        let mut replay = LoadController::new(config());
        replay.begin_burst(1);
        for s in &sim.samples {
            replay.observe(s);
        }
        // The samples carry the submit count, so the trace alone reproduces every decision.
        assert_eq!(replay.decisions(), sim.controller.decisions());
        assert_eq!(replay.throttle_events, sim.controller.throttle_events);
        assert_eq!(replay.recovery_events, sim.controller.recovery_events);
        assert_eq!(replay.state, sim.controller.state);
    }

    #[test]
    fn extra_verifier_capacity_relaxes_the_pacing_at_once() {
        let mut sim = Sim::new(10, 30, 1);
        sim.run(6_000);
        assert!(sim.controller.is_holding_load());
        let before = sim.controller.pacing_ms;
        assert!(before >= 20);
        sim.controller.capacity_increased(1, 12);
        assert_eq!(sim.controller.pacing_ms, before / 12);
        // With three batches in flight the GPU can make 300 batches/s; twelve verifiers (400/s)
        // outrun it, so the load is released and stays released.
        sim.workers = 12;
        sim.run(120_000);
        assert_eq!(
            sim.controller.state,
            ControlState::Balanced,
            "{:?}",
            sim.controller.decisions().last()
        );
        assert!(!sim.controller.is_holding_load());
    }

    #[test]
    fn pacing_close_to_the_natural_submit_gap_is_relaxed_gently_not_dropped() {
        // Seen on hardware: one batch takes ~5 ms on the GPU, one verifier needs 10 ms. Dropping the
        // pacing to zero once it looked "idle" refilled the queue within milliseconds, every time.
        let mut sim = Sim::new(4, 10, 1);
        sim.run(60_000);
        let c = &sim.controller;
        assert!(
            sim.cap_events <= 2,
            "{} hard-cap events in 60 s",
            sim.cap_events
        );
        assert!(
            c.state_changes <= 16,
            "{} state changes in 60 s",
            c.state_changes
        );
        assert!(
            c.pacing_ms >= 5,
            "pacing stays near the verify interval: {}",
            c.pacing_ms
        );
        assert!(
            sim.verdicts >= 6_000 * 98 / 100,
            "the verifier stays saturated: {}",
            sim.verdicts
        );
    }

    #[test]
    fn a_busy_verifier_is_service_not_backlog() {
        // Two batches under verification on two workers, nothing waiting: not overload.
        assert!(waiting_fill(2, 2, 8).abs() < f64::EPSILON);
        assert!(waiting_fill(0, 4, 8).abs() < f64::EPSILON);
        // Waiting batches beyond the workers count against the room left over.
        assert!((waiting_fill(5, 2, 8) - 0.5).abs() < 1e-12);
        assert!((waiting_fill(8, 2, 8) - 1.0).abs() < 1e-12);
        // A fleet that is busy but keeping up never throttles, however many slots it occupies.
        let mut c = LoadController::new(config());
        c.begin_burst(0);
        for t in 1..=10_000_u64 {
            let s = Sample {
                workers: 4,
                ..sample(t, 4, 3, t / 10)
            };
            c.observe(&s);
        }
        assert_eq!(c.throttle_events, 0, "{:?}", c.decisions());
    }

    #[test]
    fn levers_never_grow_past_their_bounds_and_dwell_time_is_accounted() {
        let mut sim = Sim::new(3, 60, 1);
        sim.run(20_000);
        let c = &sim.controller;
        assert!(c.in_flight_limit >= 1 && c.in_flight_limit <= 3);
        assert!(c.pacing_ms <= MAX_PACING_MS);
        let tracked: u64 = c.dwell_ms.iter().sum();
        assert!((1..=20_001).contains(&tracked), "dwell {tracked}");
        assert!(c.decisions().len() <= MAX_DECISIONS);
    }
}
