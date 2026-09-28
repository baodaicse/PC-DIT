//! GPU-H5 scheduling helpers: the coverage time-share and verifier worker scaling.
//! Pure logic driven by measurements the caller supplies, so both are testable with a
//! fake clock and neither touches a driver.
use serde::Serialize;

const MAX_PAIRS: u32 = 16;
const EWMA: f64 = 0.3;

fn smooth(current: f64, sample: f64) -> f64 {
    if current == 0.0 {
        sample
    } else {
        (1.0 - EWMA) * current + EWMA * sample
    }
}

/// Achieved transfer/retention time share against the configured floor.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct CoverageReport {
    pub floor: f64,
    /// Cumulative fraction of measured wall time given to transfer/retention.
    pub share: Option<f64>,
    pub iterations: u64,
    /// Transfer pairs added beyond one per iteration to hold the floor.
    pub extra_pairs: u64,
    pub transfer_seconds: f64,
    pub compute_seconds: f64,
}

/// What the next loop iteration should do: how many transfer+retention pairs, then how many
/// batches of compute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CoveragePlan {
    pub pairs: u32,
    pub batches: u64,
}

/// Guarantees transfer and retention verification a minimum share of wall time.
///
/// Async compute alone starves VRAM/PCIe verification: a burst of dozens of batches runs
/// between two transfers. The floor is the fraction of run time that must go to
/// transfer/retention work. The scheduler lengthens or shortens the compute burst (and, when
/// even one batch is longer than the share allows, adds transfer pairs) so the cumulative
/// share converges on the floor. Compute always gets at least one batch per iteration.
#[derive(Clone, Debug, Serialize)]
pub struct CoverageScheduler {
    floor: f64,
    max_batches: u64,
    transfer_seconds: f64,
    compute_seconds: f64,
    pair_seconds: f64,
    batch_seconds: f64,
    pub iterations: u64,
    pub extra_pairs: u64,
}

impl CoverageScheduler {
    /// # Errors
    /// Rejects a floor outside `0.0..=0.9` and empty burst limits.
    pub fn new(floor: f64, max_batches: u64) -> Result<Self, &'static str> {
        if !(0.0..=0.9).contains(&floor) || max_batches == 0 {
            return Err("coverage floor must be within 0..=0.9");
        }
        Ok(Self {
            floor,
            max_batches,
            transfer_seconds: 0.0,
            compute_seconds: 0.0,
            pair_seconds: 0.0,
            batch_seconds: 0.0,
            iterations: 0,
            extra_pairs: 0,
        })
    }

    /// Record `pairs` transfer+retention pairs that took `seconds` in total.
    pub fn observe_transfers(&mut self, pairs: u32, seconds: f64) {
        if pairs == 0 || !seconds.is_finite() || seconds <= 0.0 {
            return;
        }
        self.transfer_seconds += seconds;
        self.pair_seconds = smooth(self.pair_seconds, seconds / f64::from(pairs));
        self.extra_pairs += u64::from(pairs.saturating_sub(1));
    }

    /// Record one compute burst of `batches` batches that took `seconds`.
    #[allow(clippy::cast_precision_loss)]
    pub fn observe_burst(&mut self, batches: u64, seconds: f64) {
        self.iterations += 1;
        if !seconds.is_finite() || seconds < 0.0 {
            return;
        }
        self.compute_seconds += seconds;
        if batches > 0 && seconds > 0.0 {
            self.batch_seconds = smooth(self.batch_seconds, seconds / batches as f64);
        }
    }

    /// Cumulative share of measured time spent on transfer/retention.
    #[must_use]
    pub fn share(&self) -> Option<f64> {
        let total = self.transfer_seconds + self.compute_seconds;
        (total > 0.0).then(|| self.transfer_seconds / total)
    }

    #[must_use]
    pub fn floor(&self) -> f64 {
        self.floor
    }

    #[must_use]
    pub fn report(&self) -> CoverageReport {
        CoverageReport {
            floor: self.floor,
            share: self.share(),
            iterations: self.iterations,
            extra_pairs: self.extra_pairs,
            transfer_seconds: self.transfer_seconds,
            compute_seconds: self.compute_seconds,
        }
    }

    /// The next iteration. Self-correcting: it works from cumulative totals, so an earlier
    /// error in the estimates is paid back rather than accumulated.
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn plan(&self) -> CoveragePlan {
        if self.floor <= 0.0 {
            return CoveragePlan {
                pairs: 1,
                batches: self.max_batches,
            };
        }
        if self.pair_seconds == 0.0 || self.batch_seconds == 0.0 {
            // First iteration: measure both costs with the smallest burst.
            return CoveragePlan {
                pairs: 1,
                batches: 1,
            };
        }
        // Transfer time per unit of compute time that the floor demands.
        let k = self.floor / (1.0 - self.floor);
        let need = k * (self.compute_seconds + self.batch_seconds) - self.transfer_seconds;
        let pairs = (need / self.pair_seconds)
            .ceil()
            .clamp(1.0, f64::from(MAX_PAIRS)) as u32;
        if pairs > 1 {
            return CoveragePlan { pairs, batches: 1 };
        }
        let allowed = (self.transfer_seconds + self.pair_seconds) / k - self.compute_seconds;
        let batches = (allowed / self.batch_seconds)
            .round()
            .clamp(1.0, self.max_batches as f64) as u64;
        CoveragePlan { pairs: 1, batches }
    }
}

/// One evaluation window of the worker scaler.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ScalerObservation {
    pub batches: u64,
    pub verified_bytes: u64,
    pub wall_seconds: f64,
    /// The burst(s) in this observation were limited by the verifier backlog.
    pub verifier_bound: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Trial {
    previous_workers: usize,
    previous_mib_s: f64,
}

/// Chooses the verifier worker count between bursts. It grows only while the verifier is
/// the bottleneck, keeps the extra worker only if measured verified throughput really rose,
/// and never exceeds the CPU-budget ceiling.
#[derive(Clone, Debug, Serialize)]
pub struct WorkerScaler {
    initial: usize,
    ceiling: usize,
    pub current: usize,
    pub peak: usize,
    pub increases: u64,
    pub reverts: u64,
    holdoff: u32,
    calm: u32,
    trial: Option<Trial>,
    window: (u64, u64, f64, u32, u32),
}

const WINDOW_BATCHES: u64 = 32;
const WINDOW_SECONDS: f64 = 1.0;
const GAIN_TO_KEEP: f64 = 1.10;
const HOLDOFF_WINDOWS: u32 = 6;
const CALM_WINDOWS: u32 = 6;

impl WorkerScaler {
    #[must_use]
    pub fn new(initial: usize, ceiling: usize) -> Self {
        let initial = initial.max(1);
        Self {
            initial,
            ceiling: ceiling.max(initial),
            current: initial,
            peak: initial,
            increases: 0,
            reverts: 0,
            holdoff: 0,
            calm: 0,
            trial: None,
            window: (0, 0, 0.0, 0, 0),
        }
    }

    /// Workers for the next burst. Returns the current count until a full window has accrued.
    #[allow(clippy::cast_precision_loss)]
    pub fn observe(&mut self, o: &ScalerObservation) -> usize {
        self.window.0 += o.batches;
        self.window.1 += o.verified_bytes;
        self.window.2 += o.wall_seconds.max(0.0);
        self.window.3 += 1;
        self.window.4 += u32::from(o.verifier_bound);
        if self.window.0 < WINDOW_BATCHES && self.window.2 < WINDOW_SECONDS {
            return self.current;
        }
        let (bytes, seconds, bursts, bound) =
            (self.window.1, self.window.2, self.window.3, self.window.4);
        self.window = (0, 0, 0.0, 0, 0);
        if seconds <= 0.0 || bytes == 0 {
            return self.current;
        }
        let mib_s = bytes as f64 / 1_048_576.0 / seconds;
        let verifier_bound = bound * 2 >= bursts.max(1);
        if let Some(trial) = self.trial.take() {
            if mib_s >= trial.previous_mib_s * GAIN_TO_KEEP {
                self.peak = self.peak.max(self.current);
            } else {
                // The extra worker bought nothing measurable: give it back and stop trying for a while.
                self.current = trial.previous_workers;
                self.reverts += 1;
                self.holdoff = HOLDOFF_WINDOWS;
            }
            return self.current;
        }
        if self.holdoff > 0 {
            self.holdoff -= 1;
            return self.current;
        }
        if verifier_bound {
            self.calm = 0;
            if self.current < self.ceiling {
                self.trial = Some(Trial {
                    previous_workers: self.current,
                    previous_mib_s: mib_s,
                });
                self.current += 1;
                self.increases += 1;
            }
        } else {
            self.calm += 1;
            if self.calm >= CALM_WINDOWS && self.current > self.initial {
                self.current -= 1;
                self.calm = 0;
            }
        }
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the scheduler against a fixed cost model for `seconds` of wall time and returns
    /// (share, iterations, min/max batches, max pairs).
    #[allow(clippy::cast_precision_loss)]
    fn drive(
        floor: f64,
        pair_s: f64,
        batch_s: f64,
        burst_overhead_s: f64,
        seconds: f64,
    ) -> (f64, u64, u64, u64, u32) {
        let mut sched = CoverageScheduler::new(floor, 32).unwrap();
        let mut clock = 0.0;
        let (mut lo, mut hi, mut max_pairs) = (u64::MAX, 0, 0);
        while clock < seconds {
            let plan = sched.plan();
            lo = lo.min(plan.batches);
            hi = hi.max(plan.batches);
            max_pairs = max_pairs.max(plan.pairs);
            let transfer = pair_s * f64::from(plan.pairs);
            sched.observe_transfers(plan.pairs, transfer);
            let burst = burst_overhead_s + batch_s * plan.batches as f64;
            sched.observe_burst(plan.batches, burst);
            clock += transfer + burst;
        }
        (sched.share().unwrap(), sched.iterations, lo, hi, max_pairs)
    }

    #[test]
    fn floor_zero_keeps_the_full_burst_and_one_pair() {
        let mut s = CoverageScheduler::new(0.0, 32).unwrap();
        assert_eq!(
            s.plan(),
            CoveragePlan {
                pairs: 1,
                batches: 32
            }
        );
        s.observe_transfers(1, 0.024);
        s.observe_burst(32, 0.9);
        assert_eq!(
            s.plan(),
            CoveragePlan {
                pairs: 1,
                batches: 32
            }
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn the_share_converges_on_the_floor_for_a_range_of_floors_and_costs() {
        for floor in [0.05, 0.1, 0.25, 0.4, 0.5] {
            for (pair, batch) in [(0.024, 0.025), (0.024, 0.008), (0.03, 0.1)] {
                let (share, iterations, lo, hi, _) = drive(floor, pair, batch, 0.002, 120.0);
                // The floor is a minimum: when even the longest allowed burst leaves more than the
                // floor to transfers, that larger share is what the loop achieves.
                let at_max_burst = pair / (pair + 32.0 * batch + 0.002);
                let expected = floor.max(at_max_burst);
                assert!(
                    share >= floor - 0.03,
                    "below the floor: {share:.3} < {floor}"
                );
                assert!(
                    (share - expected).abs() <= 0.03,
                    "floor {floor} pair {pair} batch {batch}: share {share:.3}, expected {expected:.3}"
                );
                assert!(iterations > 20 && lo >= 1 && hi <= 32);
            }
        }
    }

    #[test]
    fn a_batch_longer_than_the_share_allows_adds_transfer_pairs_instead_of_starving_them() {
        // One 200 ms batch against a 24 ms pair cannot reach a 25% share with one pair per iteration.
        let (share, _, lo, _, max_pairs) = drive(0.25, 0.024, 0.2, 0.002, 200.0);
        assert!(max_pairs > 1, "extra pairs are needed");
        assert_eq!(lo, 1, "compute still gets a batch every iteration");
        assert!((share - 0.25).abs() <= 0.03, "share {share:.3}");
        assert!(max_pairs <= MAX_PAIRS);
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn compute_is_never_starved_by_the_floor() {
        for floor in [0.1, 0.5, 0.9] {
            let mut s = CoverageScheduler::new(floor, 32).unwrap();
            for _ in 0..200 {
                let plan = s.plan();
                assert!(plan.batches >= 1 && plan.pairs >= 1 && plan.pairs <= MAX_PAIRS);
                s.observe_transfers(plan.pairs, 0.02 * f64::from(plan.pairs));
                s.observe_burst(plan.batches, 0.03 * plan.batches as f64);
            }
        }
        assert!(CoverageScheduler::new(0.95, 32).is_err());
        assert!(CoverageScheduler::new(-0.1, 32).is_err());
        assert!(CoverageScheduler::new(0.2, 0).is_err());
    }

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn observe(sc: &mut WorkerScaler, mib_s: f64, bound: bool) -> usize {
        // One second of wall time carrying `mib_s` of verified output.
        sc.observe(&ScalerObservation {
            batches: 40,
            verified_bytes: (mib_s * 1_048_576.0) as u64,
            wall_seconds: 1.0,
            verifier_bound: bound,
        })
    }

    #[test]
    fn workers_grow_only_while_verifier_bound_and_only_if_throughput_really_rises() {
        let mut sc = WorkerScaler::new(1, 4);
        assert_eq!(
            observe(&mut sc, 500.0, false),
            1,
            "GPU-bound: no extra worker"
        );
        assert_eq!(
            observe(&mut sc, 500.0, true),
            2,
            "verifier-bound: try one more"
        );
        assert_eq!(
            observe(&mut sc, 900.0, true),
            2,
            "throughput rose 80%: keep it"
        );
        assert_eq!(observe(&mut sc, 900.0, true), 3, "still bound: try another");
        assert_eq!(observe(&mut sc, 930.0, true), 2, "only +3%: give it back");
        assert_eq!((sc.increases, sc.reverts), (2, 1));
        // A failed trial holds off further attempts for a while.
        for _ in 0..HOLDOFF_WINDOWS {
            assert_eq!(observe(&mut sc, 900.0, true), 2);
        }
        assert_eq!(observe(&mut sc, 900.0, true), 3, "holdoff over: try again");
    }

    #[test]
    fn workers_never_exceed_the_ceiling_and_decay_when_the_load_passes() {
        let mut sc = WorkerScaler::new(1, 2);
        let mut rate = 400.0;
        for _ in 0..20 {
            observe(&mut sc, rate, true);
            rate *= 1.5;
        }
        assert_eq!(sc.current, 2);
        assert!(sc.peak <= 2);
        for _ in 0..CALM_WINDOWS {
            observe(&mut sc, rate, false);
        }
        assert_eq!(sc.current, 1, "decays back to the initial count once calm");
        // Short bursts accumulate into a window rather than acting on noise.
        let mut quick = WorkerScaler::new(1, 4);
        let a = ScalerObservation {
            batches: 3,
            verified_bytes: 1 << 20,
            wall_seconds: 0.05,
            verifier_bound: true,
        };
        for _ in 0..5 {
            assert_eq!(quick.observe(&a), 1);
        }
    }
}
