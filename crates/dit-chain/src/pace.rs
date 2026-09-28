//! Read pacing: how many GPU generations run on each record read back (`G`). With a target read
//! rate, `G` follows what the GPU really does: while the GPU is the limit the read rate scales as
//! `1 / G`, so `G × measured / target` is the `G` that meets the target. Each window moves `G` by at
//! most a factor of two to ride out noisy windows. Every step logs its own batch count, so a
//! changing `G` does not disturb the path replay.
use serde::Serialize;
use std::time::{Duration, Instant};

/// Largest burst the pipeline is configured for.
pub const MAX_GENERATIONS: u64 = 64;
const WINDOW: Duration = Duration::from_secs(2);

/// `G` for the next window.
#[must_use]
pub fn next_generations(current: u64, measured_gibps: f64, target_gibps: f64) -> u64 {
    if measured_gibps <= 0.0 || target_gibps <= 0.0 {
        return current;
    }
    let low = (current / 2).max(1);
    let high = (current * 2).min(MAX_GENERATIONS);
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let wanted = (current as f64 * measured_gibps / target_gibps).round() as u64;
    wanted.clamp(low, high).clamp(1, MAX_GENERATIONS)
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct PaceReport {
    pub target_read_gibps: Option<f64>,
    pub start: u64,
    pub min: u64,
    pub max: u64,
    pub last: u64,
    pub adjustments: u64,
    /// Read rate of the last complete window.
    pub last_window_gibps: f64,
}

pub struct Pace {
    target: Option<f64>,
    current: u64,
    window_started: Instant,
    window_bytes: u64,
    window_ops: u64,
    pub report: PaceReport,
    /// Rates of the last complete window: GiB/s read and G op/s verified.
    pub window_rates: (f64, f64),
}

impl Pace {
    #[must_use]
    pub fn new(target: Option<f64>, start: u64) -> Self {
        let start = start.clamp(1, MAX_GENERATIONS);
        Self {
            target,
            current: start,
            window_started: Instant::now(),
            window_bytes: 0,
            window_ops: 0,
            report: PaceReport {
                target_read_gibps: target,
                start,
                min: start,
                max: start,
                last: start,
                adjustments: 0,
                last_window_gibps: 0.0,
            },
            window_rates: (0.0, 0.0),
        }
    }

    #[must_use]
    pub fn generations(&self) -> u64 {
        self.current
    }

    /// Accounts one load; at the end of a window, updates the rates and, with a target, `G`.
    /// Returns true when a window closed.
    #[allow(clippy::cast_precision_loss)]
    pub fn record(&mut self, bytes: u64, operations: u64) -> bool {
        self.window_bytes += bytes;
        self.window_ops += operations;
        let seconds = self.window_started.elapsed().as_secs_f64();
        if seconds < WINDOW.as_secs_f64() {
            return false;
        }
        let gibps = self.window_bytes as f64 / f64::from(1 << 30) / seconds;
        self.window_rates = (gibps, self.window_ops as f64 / 1e9 / seconds);
        self.report.last_window_gibps = gibps;
        if let Some(target) = self.target {
            let next = next_generations(self.current, gibps, target);
            if next != self.current {
                self.report.adjustments += 1;
                self.current = next;
                self.report.min = self.report.min.min(next);
                self.report.max = self.report.max.max(next);
            }
        }
        self.report.last = self.current;
        self.window_started = Instant::now();
        self.window_bytes = 0;
        self.window_ops = 0;
        true
    }

    /// A target the chain could not reach even with one generation per load (the read path or
    /// the CPU checks are the limit, not the GPU).
    #[must_use]
    pub fn shortfall(&self) -> Option<String> {
        let target = self.target?;
        (self.current == 1 && self.report.last_window_gibps < 0.9 * target).then(|| {
            format!(
                "read target {target:.2} GiB/s not reached at 1 generation per load ({:.2} GiB/s): the read path or CPU checks are the limit",
                self.report.last_window_gibps
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_follow_the_target_within_a_factor_of_two() {
        // GPU-bound at 0.25 GiB/s with G = 8: G = 2 would give 1 GiB/s.
        assert_eq!(next_generations(8, 0.25, 1.0), 4);
        assert_eq!(next_generations(4, 0.5, 1.0), 2);
        // Faster than wanted: more GPU work per load.
        assert_eq!(next_generations(8, 2.0, 1.0), 16);
        assert_eq!(next_generations(8, 1.0, 1.0), 8);
        assert_eq!(next_generations(1, 0.1, 1.0), 1);
        assert_eq!(next_generations(64, 9.0, 1.0), MAX_GENERATIONS);
        assert_eq!(next_generations(8, 0.0, 1.0), 8);
    }

    #[test]
    fn without_a_target_generations_stay_put() {
        let mut pace = Pace::new(None, 8);
        pace.window_started = Instant::now().checked_sub(WINDOW).unwrap();
        assert!(pace.record(1 << 30, 1_000));
        assert_eq!(pace.generations(), 8);
        assert!(pace.window_rates.0 > 0.0);
        assert!(pace.shortfall().is_none());
    }
}
