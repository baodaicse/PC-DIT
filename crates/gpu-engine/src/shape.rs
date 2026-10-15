//! Adaptive GEMM shape (v1.2): when the CPU verifiers, not the GPU, set the pace, the next corpus
//! is built with twice the inner dimension K and half the jobs. Every output word then carries
//! twice the GPU work while the CPU still compares every word with the oracle; jobs × K, so the
//! corpus input in VRAM and the oracle's cost, stay the same. It only steps up: a shape that
//! keeps the verifiers busy would make them the limit again one step down.
use serde::Serialize;

use crate::accounting::Regime;

/// Bursts a decision needs at least.
pub const MIN_BURSTS: u64 = 4;
/// Share of verifier-bound bursts (numerator, denominator) that steps K up: two in three.
const STEP_UP_SHARE: (u64, u64) = (2, 3);

/// Shape of one corpus: jobs per batch and the inner dimension K of each job's GEMM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Shape {
    pub jobs: usize,
    pub inner: u32,
}

/// The shapes a run may take: level `n` is `base` with K × 2ⁿ and jobs ÷ 2ⁿ, up to `max_inner`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeLadder {
    base: Shape,
    max_inner: u32,
}

impl ShapeLadder {
    /// `None` when there is no step above `base` (K already at `max_inner`, or a single job).
    #[must_use]
    pub fn new(base: Shape, max_inner: u32) -> Option<Self> {
        let ladder = Self { base, max_inner };
        ladder.can_step(0).then_some(ladder)
    }

    #[must_use]
    pub fn shape(&self, level: u32) -> Shape {
        Shape {
            jobs: (self.base.jobs >> level).max(1),
            inner: self.base.inner << level,
        }
    }

    /// Whether `level + 1` exists: K doubled stays within `max_inner` and a job is left.
    #[must_use]
    pub fn can_step(&self, level: u32) -> bool {
        let next = level + 1;
        next < 16
            && self
                .base
                .inner
                .checked_shl(next)
                .is_some_and(|inner| inner <= self.max_inner)
            && self.base.jobs >> next >= 1
    }

    /// What the bursts since the last decision say about `level`.
    #[must_use]
    pub fn decide(&self, level: u32, window: &ShapeWindow) -> ShapeDecision {
        if window.bursts < MIN_BURSTS {
            return ShapeDecision::Wait;
        }
        let (numerator, denominator) = STEP_UP_SHARE;
        if window.verifier_bound * denominator >= window.bursts * numerator && self.can_step(level)
        {
            ShapeDecision::StepUp
        } else {
            ShapeDecision::Keep
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeDecision {
    /// Too few bursts yet: keep counting.
    Wait,
    /// The shape stays; counting starts again.
    Keep,
    /// The next corpus takes the next level.
    StepUp,
}

/// Bursts seen since the last decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShapeWindow {
    pub bursts: u64,
    pub verifier_bound: u64,
}

impl ShapeWindow {
    pub fn observe(&mut self, regime: Regime) {
        self.bursts += 1;
        self.verifier_bound += u64::from(regime == Regime::VerifierBound);
    }

    /// Share of verifier-bound bursts, percent.
    #[must_use]
    pub fn verifier_percent(&self) -> u64 {
        self.verifier_bound * 100 / self.bursts.max(1)
    }
}

/// A step the run took, for the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ShapeStep {
    /// Seconds into the run when the first corpus of the new shape was installed.
    pub at_seconds: u64,
    pub rotation: u64,
    pub jobs: usize,
    pub inner: u32,
    /// Verifier-bound bursts in the window that decided it, percent.
    pub verifier_bound_percent: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(bursts: u64, verifier_bound: u64) -> ShapeWindow {
        ShapeWindow {
            bursts,
            verifier_bound,
        }
    }

    #[test]
    fn k_doubles_and_jobs_halve_up_to_the_ceiling() {
        let ladder = ShapeLadder::new(
            Shape {
                jobs: 512,
                inner: 1024,
            },
            4096,
        )
        .unwrap();
        assert_eq!(
            ladder.shape(1),
            Shape {
                jobs: 256,
                inner: 2048
            }
        );
        assert_eq!(
            ladder.shape(2),
            Shape {
                jobs: 128,
                inner: 4096
            }
        );
        // jobs × K, the corpus input and the oracle's work, stay the same.
        for level in 0..=2 {
            let shape = ladder.shape(level);
            assert_eq!(shape.jobs as u64 * u64::from(shape.inner), 512 * 1024);
        }
        assert!(ladder.can_step(1));
        assert!(!ladder.can_step(2));
        // Nothing above: no ladder.
        let at_top = Shape {
            jobs: 128,
            inner: 4096,
        };
        assert_eq!(ShapeLadder::new(at_top, 4096), None);
        let single = Shape {
            jobs: 1,
            inner: 256,
        };
        assert_eq!(ShapeLadder::new(single, 4096), None);
    }

    #[test]
    fn it_steps_up_on_two_verifier_bound_bursts_in_three_and_waits_for_four() {
        let ladder = ShapeLadder::new(
            Shape {
                jobs: 512,
                inner: 1024,
            },
            4096,
        )
        .unwrap();
        assert_eq!(ladder.decide(0, &window(3, 3)), ShapeDecision::Wait);
        assert_eq!(ladder.decide(0, &window(6, 4)), ShapeDecision::StepUp);
        assert_eq!(ladder.decide(0, &window(6, 3)), ShapeDecision::Keep);
        // At the top it keeps whatever the bursts say.
        assert_eq!(ladder.decide(2, &window(6, 6)), ShapeDecision::Keep);
        let mut seen = ShapeWindow::default();
        for regime in [
            Regime::VerifierBound,
            Regime::GpuBound,
            Regime::VerifierBound,
            Regime::Balanced,
        ] {
            seen.observe(regime);
        }
        assert_eq!(seen, window(4, 2));
        assert_eq!(seen.verifier_percent(), 50);
    }
}
