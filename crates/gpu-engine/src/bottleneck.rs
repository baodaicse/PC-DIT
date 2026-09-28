//! GPU-H7 (partial): what is limiting compute throughput *right now*, from the same signals
//! the H5 controller and H6 lane already compute (regime, backlog fill, pacing, worker count,
//! memory pressure). Pure and sampled at a fixed cadence by the caller, like the controller
//! itself, so a trace is reproducible from its inputs and stays a diagnostic label, never a
//! pass/fail input — matching [`crate::accounting::Regime`]'s own contract.
use crate::accounting::Regime;
use crate::control::ControlState;
use serde::Serialize;

/// One classified cause. Ordered by priority in [`classify`]: memory pressure and a stopped
/// controller are reported over anything else, since they explain everything downstream of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BottleneckCause {
    /// Host `MemAvailable`/PSI pressure is holding load down or draining the run.
    MemoryPressure,
    /// The controller stopped submitting (sustained pressure, or another fatal reason).
    Stalled,
    /// Outputs arrive faster than the verifier workers can absorb; more workers (below the
    /// configured ceiling) would plausibly help.
    VerifierCpuBacklog,
    /// Verifier-bound, but the worker count is already at `workers_max`: more CPU budget, not
    /// more waiting, is what would help.
    VerifierWorkerCeiling,
    /// The compute output's D2H copy dominates the device timeline over the kernel.
    PcieTransfer,
    /// The controller is deliberately spacing submissions out (recovering from a recent
    /// throttle); nothing else in this sample explains it.
    PacingActive,
    /// No artificial limit found in this sample: GPU-bound or balanced, full pacing, no backlog.
    #[default]
    None,
}

impl BottleneckCause {
    /// One-line, human-readable explanation for a log or an operator screen.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::MemoryPressure => "host memory pressure is holding load down or draining the run",
            Self::Stalled => "the controller stopped submitting",
            Self::VerifierCpuBacklog => {
                "verifier CPU cannot keep up with the GPU's output; more workers may help"
            }
            Self::VerifierWorkerCeiling => {
                "verifier CPU cannot keep up and workers are already at the configured ceiling"
            }
            Self::PcieTransfer => "the compute output's D2H copy dominates the device timeline",
            Self::PacingActive => {
                "the controller is pacing submissions, recovering from a recent throttle"
            }
            Self::None => {
                "no artificial limit found: GPU-bound or balanced, running at its configured ceiling"
            }
        }
    }
}

/// The signals a sample needs; all already computed elsewhere (the H5 controller, H6 lane, the
/// per-burst regime). No driver calls, so this is exactly as testable as `classify_regime`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BottleneckInput {
    pub regime: Regime,
    pub controller_state: ControlState,
    pub pacing_ms: u64,
    pub workers: usize,
    pub workers_max: usize,
    pub memory_pressure: bool,
}

#[must_use]
pub fn classify(i: &BottleneckInput) -> BottleneckCause {
    if i.memory_pressure {
        return BottleneckCause::MemoryPressure;
    }
    if i.controller_state == ControlState::Stopped {
        return BottleneckCause::Stalled;
    }
    match i.regime {
        Regime::VerifierBound if i.workers >= i.workers_max => {
            BottleneckCause::VerifierWorkerCeiling
        }
        Regime::VerifierBound => BottleneckCause::VerifierCpuBacklog,
        Regime::TransferBound => BottleneckCause::PcieTransfer,
        Regime::GpuBound | Regime::Balanced if i.pacing_ms > 0 => BottleneckCause::PacingActive,
        Regime::GpuBound | Regime::Balanced => BottleneckCause::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BottleneckInput {
        BottleneckInput {
            regime: Regime::GpuBound,
            controller_state: ControlState::Balanced,
            pacing_ms: 0,
            workers: 2,
            workers_max: 4,
            memory_pressure: false,
        }
    }

    #[test]
    fn memory_pressure_and_stalled_win_over_every_regime() {
        for regime in [
            Regime::GpuBound,
            Regime::VerifierBound,
            Regime::TransferBound,
            Regime::Balanced,
        ] {
            let pressured = BottleneckInput {
                regime,
                memory_pressure: true,
                ..base()
            };
            assert_eq!(
                classify(&pressured),
                BottleneckCause::MemoryPressure,
                "{regime:?}"
            );
            let stalled = BottleneckInput {
                regime,
                controller_state: ControlState::Stopped,
                ..base()
            };
            assert_eq!(classify(&stalled), BottleneckCause::Stalled, "{regime:?}");
        }
        // Pressure wins even over a stopped controller (it usually caused the stop).
        let both = BottleneckInput {
            controller_state: ControlState::Stopped,
            memory_pressure: true,
            ..base()
        };
        assert_eq!(classify(&both), BottleneckCause::MemoryPressure);
    }

    #[test]
    fn verifier_bound_distinguishes_room_to_add_workers_from_the_ceiling() {
        let room = BottleneckInput {
            regime: Regime::VerifierBound,
            workers: 2,
            workers_max: 4,
            ..base()
        };
        assert_eq!(classify(&room), BottleneckCause::VerifierCpuBacklog);
        let ceiling = BottleneckInput {
            regime: Regime::VerifierBound,
            workers: 4,
            workers_max: 4,
            ..base()
        };
        assert_eq!(classify(&ceiling), BottleneckCause::VerifierWorkerCeiling);
        // A single fixed worker count (no scaler) is workers == workers_max: still the ceiling.
        let fixed = BottleneckInput {
            regime: Regime::VerifierBound,
            workers: 1,
            workers_max: 1,
            ..base()
        };
        assert_eq!(classify(&fixed), BottleneckCause::VerifierWorkerCeiling);
    }

    #[test]
    fn transfer_bound_is_reported_regardless_of_pacing_or_workers() {
        let i = BottleneckInput {
            regime: Regime::TransferBound,
            pacing_ms: 50,
            workers: 4,
            workers_max: 4,
            ..base()
        };
        assert_eq!(classify(&i), BottleneckCause::PcieTransfer);
    }

    #[test]
    fn pacing_is_reported_only_when_nothing_more_specific_explains_it() {
        let gpu_bound_pacing = BottleneckInput {
            regime: Regime::GpuBound,
            pacing_ms: 5,
            ..base()
        };
        assert_eq!(classify(&gpu_bound_pacing), BottleneckCause::PacingActive);
        let balanced_pacing = BottleneckInput {
            regime: Regime::Balanced,
            pacing_ms: 5,
            ..base()
        };
        assert_eq!(classify(&balanced_pacing), BottleneckCause::PacingActive);
        // Verifier-bound already explains pacing; it must not be masked by it.
        let verifier_pacing = BottleneckInput {
            regime: Regime::VerifierBound,
            pacing_ms: 5,
            ..base()
        };
        assert!(matches!(
            classify(&verifier_pacing),
            BottleneckCause::VerifierCpuBacklog | BottleneckCause::VerifierWorkerCeiling
        ));
    }

    #[test]
    fn a_clean_run_reports_none() {
        assert_eq!(classify(&base()), BottleneckCause::None);
        let balanced = BottleneckInput {
            regime: Regime::Balanced,
            ..base()
        };
        assert_eq!(classify(&balanced), BottleneckCause::None);
    }

    #[test]
    fn every_cause_has_a_non_empty_description() {
        for cause in [
            BottleneckCause::MemoryPressure,
            BottleneckCause::Stalled,
            BottleneckCause::VerifierCpuBacklog,
            BottleneckCause::VerifierWorkerCeiling,
            BottleneckCause::PcieTransfer,
            BottleneckCause::PacingActive,
            BottleneckCause::None,
        ] {
            assert!(!cause.describe().is_empty());
        }
    }
}
