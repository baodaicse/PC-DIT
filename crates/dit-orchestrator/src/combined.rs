//! Combined CPU-RAM + GPU-VRAM + SSD + data-chain run: the pure parts
//! (`docs/COMBINED_STRESS_CHAIN_ANALYSIS.md` sections 5, 6, 8 and 11). Phase schedule per level,
//! thread split per intensity, the "last allocator takes the rest" memory arbiter and the
//! verdict / grade of a whole session. The process side lives in the `dit-suite` binary.
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const COMBINED_MODULE_ID: &str = "dit.combined";
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Kernel, minimal page cache, UI and watchdogs (section 6.1 item 5). Larger than the GPU
/// pipeline's 256 MiB self-throttle threshold, so the GPU does not see a false memory shortage.
pub const SYSTEM_RESERVE_BYTES: u64 = 1536 * MIB;
/// CPU-RAM refuses to start below this arena (fail closed, section 6.1 item 6).
pub const CPU_RAM_MINIMUM_BYTES: u64 = GIB;
/// Assumed SSD engine peak when no baseline measured it.
pub const SSD_DEFAULT_PEAK_BYTES: u64 = 512 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Level {
    Bounded,
    Deep,
    Soak,
}

impl Level {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bounded => "BOUNDED",
            Self::Deep => "DEEP",
            Self::Soak => "SOAK",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intensity {
    Balanced,
    Max,
}

impl Intensity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    CpuRam,
    Gpu,
    Ssd,
    Chain,
}

impl Layer {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CpuRam => "CPU-RAM",
            Self::Gpu => "GPU-VRAM",
            Self::Ssd => "SSD",
            Self::Chain => "CHAIN",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "layer")]
pub enum PhaseKind {
    /// Chain records written once (before the SSD baseline, decision 3b).
    Prefill,
    /// One component alone: its PASS makes a later overlap failure attributable to load.
    Baseline(Layer),
    /// The chain alone over the prefilled records.
    ChainAlone,
    /// Every layer at once.
    Overlap,
    /// Everything idle (temperature and power states fall back).
    Rest,
    /// The chain re-reads its records after the rest (SSD retention).
    Retention,
}

impl PhaseKind {
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Prefill => "PREFILL".into(),
            Self::Baseline(layer) => format!("BASELINE {}", layer.label()),
            Self::ChainAlone => "CHAIN ALONE".into(),
            Self::Overlap => "OVERLAP".into(),
            Self::Rest => "REST".into(),
            Self::Retention => "RETENTION READ".into(),
        }
    }

    /// Layers this phase runs.
    #[must_use]
    pub fn layers(self, available: &Available) -> Vec<Layer> {
        match self {
            Self::Prefill | Self::ChainAlone | Self::Retention => vec![Layer::Chain],
            Self::Baseline(layer) => vec![layer],
            Self::Overlap => available.layers(),
            Self::Rest => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Phase {
    pub kind: PhaseKind,
    /// Planned length; the prefill runs to completion (0 = unbounded).
    pub seconds: u64,
}

/// Which layers this machine and the operator's choices allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Available {
    pub gpu: bool,
    pub ssd: bool,
    /// Needs a CUDA GPU and a prepared chain directory.
    pub chain: bool,
}

impl Available {
    #[must_use]
    pub fn layers(&self) -> Vec<Layer> {
        let mut layers = vec![Layer::CpuRam];
        if self.gpu {
            layers.push(Layer::Gpu);
        }
        if self.ssd {
            layers.push(Layer::Ssd);
        }
        if self.chain {
            layers.push(Layer::Chain);
        }
        layers
    }
}

const BASELINE_SECONDS: u64 = 120;

/// The schedule of section 5. BOUNDED ~15 min: baselines of ~2 min, the chain alone, a 5 min
/// overlap. DEEP ~60 min: a 40 min overlap, then rest and a retention re-read. SOAK: cycles of
/// a 50 min overlap, rest and retention until `soak_hours` are used.
#[must_use]
pub fn schedule(level: Level, available: Available, soak_hours: u64) -> Vec<Phase> {
    let mut phases = Vec::new();
    let mut push = |kind, seconds| phases.push(Phase { kind, seconds });
    if available.chain {
        push(PhaseKind::Prefill, 0);
    }
    push(PhaseKind::Baseline(Layer::CpuRam), BASELINE_SECONDS);
    if available.gpu {
        push(PhaseKind::Baseline(Layer::Gpu), BASELINE_SECONDS);
    }
    if available.ssd {
        push(PhaseKind::Baseline(Layer::Ssd), BASELINE_SECONDS);
    }
    let chain_alone = match level {
        Level::Bounded => 60,
        Level::Deep | Level::Soak => 180,
    };
    if available.chain {
        push(PhaseKind::ChainAlone, chain_alone);
    }
    match level {
        Level::Bounded => push(PhaseKind::Overlap, 300),
        Level::Deep => {
            push(PhaseKind::Overlap, 2400);
            push(PhaseKind::Rest, 120);
            if available.chain {
                push(PhaseKind::Retention, 300);
            }
        }
        Level::Soak => {
            // Cycles fill what the baselines leave of the budget; at least one always runs.
            let budget = soak_hours.clamp(1, 72) * 3600;
            let cycle = 3000 + 300 + if available.chain { 300 } else { 0 };
            let mut used = planned_seconds(&phases, 0);
            loop {
                phases.push(Phase {
                    kind: PhaseKind::Overlap,
                    seconds: 3000,
                });
                phases.push(Phase {
                    kind: PhaseKind::Rest,
                    seconds: 300,
                });
                if available.chain {
                    phases.push(Phase {
                        kind: PhaseKind::Retention,
                        seconds: 300,
                    });
                }
                used += cycle;
                if used + cycle > budget {
                    break;
                }
            }
        }
    }
    phases
}

/// Planned seconds of the whole schedule (the prefill counted at `prefill_estimate`).
#[must_use]
pub fn planned_seconds(phases: &[Phase], prefill_estimate: u64) -> u64 {
    phases
        .iter()
        .map(|phase| {
            if phase.kind == PhaseKind::Prefill {
                prefill_estimate
            } else {
                phase.seconds
            }
        })
        .sum()
}

/// CPU threads per layer during the overlap (section 6.3). `None` leaves the engine's default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPlan {
    pub cpu_ram_workers: usize,
    pub cpu_oracle_workers: usize,
    pub gpu_verifier_max: Option<usize>,
    pub ssd_workers: Option<usize>,
    pub chain_threads: usize,
    pub os_threads: usize,
}

#[must_use]
pub fn plan_threads(logical: usize, intensity: Intensity, available: Available) -> ThreadPlan {
    let n = logical.max(1);
    let chain = usize::from(available.chain);
    match intensity {
        Intensity::Max => {
            // Every engine takes its own default and the OS shares the CPUs out.
            let oracle = if n >= 2 { (n / 4).max(1) } else { 0 };
            ThreadPlan {
                cpu_ram_workers: n.saturating_sub(oracle).max(1),
                cpu_oracle_workers: oracle,
                gpu_verifier_max: None,
                ssd_workers: None,
                chain_threads: chain,
                os_threads: 0,
            }
        }
        Intensity::Balanced => {
            // 16 threads: CPU-RAM 7, oracle 2, GPU verifiers 3, SSD 2, chain 1, OS 1.
            let os = usize::from(n >= 4);
            let gpu = if available.gpu {
                (n * 7 / 32).max(1)
            } else {
                0
            };
            let ssd = if available.ssd {
                (n * 5 / 32).max(1)
            } else {
                0
            };
            let oracle = if n >= 8 { n / 8 } else { 0 };
            let cpu_ram = n.saturating_sub(os + chain + gpu + ssd + oracle).max(1);
            ThreadPlan {
                cpu_ram_workers: cpu_ram,
                cpu_oracle_workers: oracle,
                gpu_verifier_max: available.gpu.then_some(gpu),
                ssd_workers: available.ssd.then_some(ssd),
                chain_threads: chain,
                os_threads: os,
            }
        }
    }
}

/// Host-memory ceiling handed to each GPU (`DIT_GPU_HOST_BUDGET_BYTES`), which is also its
/// declared peak: 1/16 of what is available, between 512 MiB and 2 GiB.
#[must_use]
pub fn gpu_host_budget(mem_available_bytes: u64) -> u64 {
    (mem_available_bytes / 16).clamp(512 * MIB, 2 * GIB)
}

/// Declared peak of a layer that ran a baseline alone: its measured high-water mark plus a
/// quarter and 64 MiB of margin.
#[must_use]
pub fn peak_from_baseline(high_water_bytes: u64) -> u64 {
    high_water_bytes + high_water_bytes / 4 + 64 * MIB
}

/// Memory a started layer holds now and may still add.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holding {
    pub layer: Layer,
    pub resident_bytes: u64,
    pub declared_peak_bytes: u64,
}

impl Holding {
    #[must_use]
    pub fn may_grow(&self) -> u64 {
        self.declared_peak_bytes.saturating_sub(self.resident_bytes)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArbiterError {
    #[error(
        "declared peaks {declared_mib} MiB + reserve {reserve_mib} MiB + CPU-RAM minimum {minimum_mib} MiB exceed MemAvailable {available_mib} MiB"
    )]
    Oversubscribed {
        declared_mib: u64,
        reserve_mib: u64,
        minimum_mib: u64,
        available_mib: u64,
    },
    #[error("only {remainder_mib} MiB would remain for CPU-RAM (minimum {minimum_mib} MiB)")]
    RemainderTooSmall {
        remainder_mib: u64,
        minimum_mib: u64,
    },
}

/// Memory the arbiter keeps for the system and the smallest CPU-RAM arena it accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPolicy {
    pub reserve_bytes: u64,
    pub cpu_ram_minimum_bytes: u64,
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            reserve_bytes: SYSTEM_RESERVE_BYTES,
            cpu_ram_minimum_bytes: CPU_RAM_MINIMUM_BYTES,
        }
    }
}

/// Refuses a session whose declared peaks cannot fit before anything starts.
/// # Errors
/// [`ArbiterError::Oversubscribed`].
pub fn check_fits(
    mem_available_bytes: u64,
    declared_peaks: &[u64],
    policy: MemoryPolicy,
) -> Result<(), ArbiterError> {
    let declared: u64 = declared_peaks.iter().sum();
    if declared + policy.reserve_bytes + policy.cpu_ram_minimum_bytes > mem_available_bytes {
        return Err(ArbiterError::Oversubscribed {
            declared_mib: declared / MIB,
            reserve_mib: policy.reserve_bytes / MIB,
            minimum_mib: policy.cpu_ram_minimum_bytes / MIB,
            available_mib: mem_available_bytes / MIB,
        });
    }
    Ok(())
}

/// CPU-RAM arena in MiB: `MemAvailable` measured after every other layer is ready, minus the
/// system reserve and whatever the others may still add (section 6.1 item 3).
/// # Errors
/// [`ArbiterError::RemainderTooSmall`] below the policy's CPU-RAM minimum.
pub fn cpu_ram_remainder_mib(
    mem_available_bytes: u64,
    holdings: &[Holding],
    policy: MemoryPolicy,
) -> Result<u64, ArbiterError> {
    let growth: u64 = holdings.iter().map(Holding::may_grow).sum();
    let remainder = mem_available_bytes
        .saturating_sub(policy.reserve_bytes)
        .saturating_sub(growth);
    if remainder < policy.cpu_ram_minimum_bytes {
        return Err(ArbiterError::RemainderTooSmall {
            remainder_mib: remainder / MIB,
            minimum_mib: policy.cpu_ram_minimum_bytes / MIB,
        });
    }
    Ok(remainder / MIB)
}

/// Outcome of one layer in one phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LayerOutcome {
    Pass,
    /// Data matched, but corrected hardware events were reported.
    PassHwWarnings,
    Fail,
    Invalid,
    /// Ended without a report (crash, OOM, watchdog kill).
    Crashed,
    NotRun,
}

impl LayerOutcome {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::PassHwWarnings => "PASS (HW WARNINGS)",
            Self::Fail => "FAIL",
            Self::Invalid => "INVALID",
            Self::Crashed => "CRASHED",
            Self::NotRun => "NOT RUN",
        }
    }
}

/// Session verdict (section 8): any FAIL is a FAIL; any crash, INVALID layer, stop or missing
/// phase makes the session INVALID; otherwise PASS, flagged when hardware warnings were seen.
#[must_use]
pub fn session_verdict(outcomes: &[LayerOutcome], completed: bool) -> &'static str {
    if outcomes.contains(&LayerOutcome::Fail) {
        "FAIL"
    } else if !completed
        || outcomes
            .iter()
            .any(|outcome| matches!(outcome, LayerOutcome::Invalid | LayerOutcome::Crashed))
    {
        "INVALID"
    } else if outcomes.contains(&LayerOutcome::PassHwWarnings) {
        "PASS (HW WARNINGS)"
    } else {
        "PASS"
    }
}

/// Grade actually reached: the chosen level only when every planned phase ran to its end.
#[must_use]
pub fn grade(level: Level, completed_phases: usize, planned_phases: usize) -> Option<Level> {
    (planned_phases > 0 && completed_phases >= planned_phases).then_some(level)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Available = Available {
        gpu: true,
        ssd: true,
        chain: true,
    };

    #[test]
    fn schedules_follow_the_levels() {
        let bounded = schedule(Level::Bounded, ALL, 0);
        let kinds: Vec<PhaseKind> = bounded.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            vec![
                PhaseKind::Prefill,
                PhaseKind::Baseline(Layer::CpuRam),
                PhaseKind::Baseline(Layer::Gpu),
                PhaseKind::Baseline(Layer::Ssd),
                PhaseKind::ChainAlone,
                PhaseKind::Overlap,
            ]
        );
        // ~15 minutes with a ~90 s prefill.
        assert_eq!(planned_seconds(&bounded, 90), 90 + 360 + 60 + 300);
        let deep = schedule(Level::Deep, ALL, 0);
        assert_eq!(
            planned_seconds(&deep, 90),
            90 + 360 + 180 + 2400 + 120 + 300
        );
        let soak = schedule(Level::Soak, ALL, 4);
        let overlaps = soak.iter().filter(|p| p.kind == PhaseKind::Overlap).count();
        assert_eq!(
            overlaps, 3,
            "three 60-minute cycles fit in 4 h after the baselines"
        );
        assert!(planned_seconds(&soak, 90) <= 4 * 3600 + 90 + 540);
        // Without GPU and SSD: CPU-RAM baseline, then the overlap of CPU-RAM alone.
        let cpu_only = Available {
            gpu: false,
            ssd: false,
            chain: false,
        };
        let phases = schedule(Level::Bounded, cpu_only, 0);
        assert_eq!(phases.len(), 2);
        assert_eq!(PhaseKind::Overlap.layers(&cpu_only), vec![Layer::CpuRam]);
        assert_eq!(PhaseKind::Retention.layers(&ALL), vec![Layer::Chain]);
    }

    #[test]
    fn balanced_threads_match_section_6_3_and_never_starve_cpu_ram() {
        let plan = plan_threads(16, Intensity::Balanced, ALL);
        assert_eq!(
            (
                plan.cpu_ram_workers,
                plan.cpu_oracle_workers,
                plan.gpu_verifier_max,
                plan.ssd_workers,
                plan.chain_threads,
                plan.os_threads
            ),
            (7, 2, Some(3), Some(2), 1, 1)
        );
        for n in [1, 2, 4, 8, 32, 64, 128] {
            let plan = plan_threads(n, Intensity::Balanced, ALL);
            assert!(plan.cpu_ram_workers >= 1, "{n} threads");
            let used = plan.cpu_ram_workers
                + plan.cpu_oracle_workers
                + plan.gpu_verifier_max.unwrap_or(0)
                + plan.ssd_workers.unwrap_or(0)
                + plan.chain_threads
                + plan.os_threads;
            if n >= 8 {
                assert_eq!(used, n, "{n} threads are shared out exactly");
            }
        }
        let max = plan_threads(16, Intensity::Max, ALL);
        assert_eq!((max.cpu_ram_workers, max.cpu_oracle_workers), (12, 4));
        assert_eq!(max.gpu_verifier_max, None);
    }

    #[test]
    fn the_arbiter_gives_cpu_ram_the_rest_or_refuses() {
        // The section 6.2 example: 32 GB machine, ~28 GiB available at the start.
        let start = 28 * GIB;
        let gpu = gpu_host_budget(start);
        assert_eq!(gpu, 28 * GIB / 16);
        let chain = 400 * MIB;
        let ssd = SSD_DEFAULT_PEAK_BYTES;
        check_fits(start, &[gpu, chain, ssd], MemoryPolicy::default()).unwrap();
        // After the others are up: they hold most of their peak already.
        let holdings = [
            Holding {
                layer: Layer::Gpu,
                resident_bytes: gpu - 200 * MIB,
                declared_peak_bytes: gpu,
            },
            Holding {
                layer: Layer::Chain,
                resident_bytes: 100 * MIB,
                declared_peak_bytes: chain,
            },
            Holding {
                layer: Layer::Ssd,
                resident_bytes: 300 * MIB,
                declared_peak_bytes: ssd,
            },
        ];
        let now = start - (gpu - 200 * MIB) - 100 * MIB - 300 * MIB;
        let arena = cpu_ram_remainder_mib(now, &holdings, MemoryPolicy::default()).unwrap();
        let expected = (now - SYSTEM_RESERVE_BYTES - (200 + 300 + 212) * MIB) / MIB;
        assert_eq!(arena, expected);
        let held = arena * MIB + gpu + chain + ssd;
        assert!(
            held * 100 / start >= 90,
            "tests hold >= 90% of MemAvailable"
        );

        for total_gib in [8, 16, 32, 64, 128] {
            let available = total_gib * GIB * 9 / 10;
            let gpu = gpu_host_budget(available);
            assert!(
                check_fits(
                    available,
                    &[gpu, 400 * MIB, SSD_DEFAULT_PEAK_BYTES],
                    MemoryPolicy::default()
                )
                .is_ok()
            );
        }
        assert!(matches!(
            check_fits(
                3 * GIB,
                &[2 * GIB, SSD_DEFAULT_PEAK_BYTES],
                MemoryPolicy::default()
            ),
            Err(ArbiterError::Oversubscribed { .. })
        ));
        assert!(matches!(
            cpu_ram_remainder_mib(2 * GIB, &[], MemoryPolicy::default()),
            Err(ArbiterError::RemainderTooSmall { .. })
        ));
        assert_eq!(peak_from_baseline(400 * MIB), 564 * MIB);
    }

    #[test]
    fn verdicts_and_grades() {
        use LayerOutcome::{Crashed, Fail, Invalid, Pass, PassHwWarnings};
        assert_eq!(session_verdict(&[Pass, Pass], true), "PASS");
        assert_eq!(
            session_verdict(&[Pass, PassHwWarnings], true),
            "PASS (HW WARNINGS)"
        );
        assert_eq!(session_verdict(&[Pass, Fail, Crashed], true), "FAIL");
        assert_eq!(session_verdict(&[Pass, Crashed], true), "INVALID");
        assert_eq!(session_verdict(&[Pass, Invalid], true), "INVALID");
        assert_eq!(session_verdict(&[Pass], false), "INVALID");
        assert_eq!(grade(Level::Deep, 7, 7), Some(Level::Deep));
        assert_eq!(grade(Level::Deep, 6, 7), None);
    }
}
