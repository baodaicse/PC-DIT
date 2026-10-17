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
    /// v1.2 C6: one core at a time at peak boost, everything else idle.
    RotationA,
    /// v1.2 C6: one core at a time while the GPU test runs flat out.
    RotationB,
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
            Self::RotationA => "CORE ROTATION A".into(),
            Self::RotationB => "CORE ROTATION B".into(),
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
            Self::RotationA => vec![Layer::CpuRam],
            // The overlap without CPU-RAM, then the rotation (v1.2.013).
            Self::RotationB => {
                let mut layers: Vec<Layer> = available
                    .layers()
                    .into_iter()
                    .filter(|layer| *layer != Layer::CpuRam)
                    .collect();
                layers.push(Layer::CpuRam);
                layers
            }
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

/// Rotation A of a Bounded session (v1.2.013): the two cores the firmware ranks highest, 10 s
/// each (the planner keeps the best-ranked ones when the budget is short), taken from its 5-minute
/// overlap. They are the cores most likely to fail at peak boost.
const BOUNDED_ROTATION_A_SECONDS: u64 = 20;
/// Core rotation budget of a Deep session (v1.2 C6): about 20% of the hour, taken from the
/// overlap (40 -> 28 minutes). A gets a third, B two thirds (B runs two kernels).
const DEEP_ROTATION_A_SECONDS: u64 = 240;
const DEEP_ROTATION_B_SECONDS: u64 = 480;
/// Per Soak cycle (about 10 minutes of its 50-minute overlap).
const SOAK_ROTATION_A_SECONDS: u64 = 200;
const SOAK_ROTATION_B_SECONDS: u64 = 400;

/// Adds the core rotation (v1.2 C6) to a [`schedule`]: rotation A early, while the machine is
/// cool (Bounded and Deep: after the baselines, before the chain; Soak: before every overlap),
/// rotation B right after the overlap, while it is hot (Deep and Soak). Their time comes out of
/// the overlap, so the session keeps its length. Without a GPU there is no B, its time stays with
/// the overlap. Bounded only rotates its two best-ranked cores (20 s of its 5-minute overlap).
#[must_use]
pub fn add_core_rotation(level: Level, available: Available, phases: Vec<Phase>) -> Vec<Phase> {
    let (a, b) = match level {
        Level::Bounded => (BOUNDED_ROTATION_A_SECONDS, 0),
        Level::Deep => (DEEP_ROTATION_A_SECONDS, DEEP_ROTATION_B_SECONDS),
        Level::Soak => (SOAK_ROTATION_A_SECONDS, SOAK_ROTATION_B_SECONDS),
    };
    let b = if available.gpu { b } else { 0 };
    let first_after_baselines = phases
        .iter()
        .position(|phase| !matches!(phase.kind, PhaseKind::Prefill | PhaseKind::Baseline(_)))
        .unwrap_or(phases.len());
    let mut out = Vec::with_capacity(phases.len() + 8);
    for (index, phase) in phases.into_iter().enumerate() {
        let rotation_a = Phase {
            kind: PhaseKind::RotationA,
            seconds: a,
        };
        if level != Level::Soak && index == first_after_baselines {
            out.push(rotation_a);
        }
        if phase.kind == PhaseKind::Overlap {
            if level == Level::Soak {
                out.push(rotation_a);
            }
            out.push(Phase {
                kind: PhaseKind::Overlap,
                seconds: phase.seconds.saturating_sub(a + b),
            });
            if b > 0 {
                out.push(Phase {
                    kind: PhaseKind::RotationB,
                    seconds: b,
                });
            }
        } else {
            out.push(phase);
        }
    }
    out
}

/// Whether the core rotation runs: on unless `DIT_ROTATION=0` or `dit_rotation=0` on the kernel
/// command line (the variable wins), a way back to the schedule before v1.2 on a machine where
/// the rotation misbehaves.
#[must_use]
pub fn core_rotation_enabled(env_value: Option<&str>, cmdline: &str) -> bool {
    let value = env_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            cmdline
                .split_whitespace()
                .find_map(|word| word.strip_prefix("dit_rotation="))
        });
    value != Some("0")
}

/// The SSD write budget of the overlap at `overlap_index`, shared with the rotation B right after
/// it in proportion to their lengths (v1.2.013): `(overlap, B)` MiB. The SSD runs in both, so a
/// session writes what it wrote with the overlap alone; without a rotation B after it the overlap
/// keeps it all.
#[must_use]
pub fn ssd_budget_split(phases: &[Phase], overlap_index: usize, budget_mib: u64) -> (u64, u64) {
    let overlap = phases.get(overlap_index).map_or(0, |phase| phase.seconds);
    let rotation = phases
        .get(overlap_index + 1)
        .filter(|phase| phase.kind == PhaseKind::RotationB)
        .map_or(0, |phase| phase.seconds);
    if rotation == 0 {
        return (budget_mib, 0);
    }
    let share = budget_mib * rotation / (overlap + rotation);
    (budget_mib - share, share)
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
    /// Niceness of the CPU-RAM layer in the overlap (`max`: 10). Its workers fill every thread
    /// there, and since v1.2 C1 they keep the memory bus busy: at the same priority the GPU
    /// test's CPU verifiers fell behind and the GPU waited (on a 3700X + GTX 960: busy 60%,
    /// 66 W; at niceness 10: 79%, 86 W, CPU-RAM 6% fewer rounds). It still takes every cycle the
    /// others leave, so the CPU stays fully loaded. `balanced` gives each layer its own threads.
    #[serde(default)]
    pub cpu_ram_nice: i32,
    /// How the CPU-RAM threads were split between RAM workers and the compute oracle (v1.2
    /// C12); absent before v1.2.021.
    #[serde(default)]
    pub ram_split: Option<RamSplit>,
}

/// [`ThreadPlan::cpu_ram_nice`] at `max`.
pub const MAX_CPU_RAM_NICE: i32 = 10;

/// v1.2 C12: in the overlap the RAM workers aim at this share of the memory bus the CPU-RAM
/// baseline measured (numerator, denominator); every other CPU-RAM thread runs the compute
/// oracle. Since C1 a RAM worker verifies 6–7 times faster and about ten fill the bus of a
/// 9950X3D: the 24 that `n − n/4` gave kept it full, and the GPU test's CPU side lost it (RTX
/// 5060 Ti: verified operations 46 % of its baseline, CPU-verified VRAM lane 15 %) while the
/// oracle had 8 threads. A third of the bus is about v1.1's RAM rate.
pub const OVERLAP_RAM_BUS_SHARE: (u64, u64) = (1, 3);
/// Before the baseline's probe, or without one: one RAM worker for this many threads (a C1
/// worker against 6–7 of v1.1's, so v1.1's rate again).
const THREADS_PER_RAM_WORKER: usize = 8;

/// How the overlap's RAM workers were chosen (v1.2 C12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RamSplit {
    pub source: RamSplitSource,
    /// Best verify rate of the baseline's split probe, bytes/s (0 without the probe).
    pub peak_bytes_per_second: u64,
    /// The [`OVERLAP_RAM_BUS_SHARE`] of it.
    pub target_bytes_per_second: u64,
    /// What the probe measured with the RAM workers chosen.
    pub chosen_bytes_per_second: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RamSplitSource {
    /// One RAM worker per [`THREADS_PER_RAM_WORKER`] threads: the plan before the baseline,
    /// kept when the baseline left no probe (a crash, a fixed split).
    PerThreads,
    /// The step of the CPU-RAM baseline's probe (`worker_split.steps` of its report) nearest
    /// the target.
    Probe,
}

/// RAM workers among `pool` CPU-RAM threads before any probe.
fn ram_workers_per_threads(logical: usize, pool: usize) -> usize {
    (logical / THREADS_PER_RAM_WORKER).clamp(1, pool.max(1))
}

const PER_THREADS: RamSplit = RamSplit {
    source: RamSplitSource::PerThreads,
    peak_bytes_per_second: 0,
    target_bytes_per_second: 0,
    chosen_bytes_per_second: 0,
};

#[must_use]
pub fn plan_threads(logical: usize, intensity: Intensity, available: Available) -> ThreadPlan {
    let n = logical.max(1);
    let chain = usize::from(available.chain);
    match intensity {
        Intensity::Max => {
            // Every engine takes its own default and the OS shares the CPUs out; CPU-RAM's
            // threads are RAM workers and the compute oracle (C12).
            let ram = ram_workers_per_threads(n, n);
            ThreadPlan {
                cpu_ram_workers: ram,
                cpu_oracle_workers: n - ram,
                gpu_verifier_max: None,
                ssd_workers: None,
                chain_threads: chain,
                os_threads: 0,
                cpu_ram_nice: MAX_CPU_RAM_NICE,
                ram_split: Some(PER_THREADS),
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
            let pool = n.saturating_sub(os + chain + gpu + ssd).max(1);
            let ram = ram_workers_per_threads(n, pool);
            ThreadPlan {
                cpu_ram_workers: ram,
                cpu_oracle_workers: pool - ram,
                gpu_verifier_max: available.gpu.then_some(gpu),
                ssd_workers: available.ssd.then_some(ssd),
                chain_threads: chain,
                os_threads: os,
                cpu_ram_nice: 0,
                ram_split: Some(PER_THREADS),
            }
        }
    }
}

/// v1.2 C12: the overlap's RAM workers from the CPU-RAM baseline's split probe, `steps` being
/// `(RAM workers, verify bytes/s)`: the step whose rate is nearest [`OVERLAP_RAM_BUS_SHARE`]
/// of the best rate, the fewer workers on a tie. Nearest rather than the fewest reaching it:
/// a step measures 300 ms and the counts jump 4 → 6, so on a 3700X four workers at 8.5 GB/s
/// against a target of 8.6 would have given six (51 % of the bus) in one session and four in
/// the next. CPU-RAM keeps its threads; the ones the RAM side gives up run the compute oracle.
/// No steps (no probe): the plan stays as it is.
pub fn split_overlap_ram(plan: &mut ThreadPlan, steps: &[(usize, u64)]) {
    let peak = steps.iter().map(|&(_, rate)| rate).max().unwrap_or(0);
    if peak == 0 {
        return;
    }
    let (numerator, denominator) = OVERLAP_RAM_BUS_SHARE;
    let target = peak / denominator * numerator;
    let Some(&(workers, rate)) = steps
        .iter()
        .filter(|&&(workers, _)| workers > 0)
        .min_by_key(|&&(workers, rate)| (rate.abs_diff(target), workers))
    else {
        return;
    };
    let pool = (plan.cpu_ram_workers + plan.cpu_oracle_workers).max(1);
    let ram = workers.min(pool);
    plan.cpu_ram_workers = ram;
    plan.cpu_oracle_workers = pool - ram;
    plan.ram_split = Some(RamSplit {
        source: RamSplitSource::Probe,
        peak_bytes_per_second: peak,
        target_bytes_per_second: target,
        chosen_bytes_per_second: rate,
    });
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
    fn the_ssd_writes_what_it_wrote_before_rotation_b() {
        let deep = add_core_rotation(Level::Deep, ALL, schedule(Level::Deep, ALL, 0));
        let overlap = deep
            .iter()
            .position(|p| p.kind == PhaseKind::Overlap)
            .unwrap();
        // Standard: 16 GiB over 28 + 8 minutes.
        let (main, rotation) = ssd_budget_split(&deep, overlap, 16_384);
        assert_eq!(main + rotation, 16_384);
        assert_eq!(rotation, 16_384 * 480 / 2160);
        // No rotation B after it (Bounded, or rotation off): the overlap keeps it all.
        let bounded = add_core_rotation(Level::Bounded, ALL, schedule(Level::Bounded, ALL, 0));
        let overlap = bounded
            .iter()
            .position(|p| p.kind == PhaseKind::Overlap)
            .unwrap();
        assert_eq!(ssd_budget_split(&bounded, overlap, 2048), (2048, 0));
    }

    #[test]
    fn core_rotation_can_be_turned_off() {
        assert!(core_rotation_enabled(None, "boot=live quiet"));
        assert!(!core_rotation_enabled(None, "boot=live dit_rotation=0"));
        assert!(core_rotation_enabled(None, "dit_rotation=1"));
        assert!(!core_rotation_enabled(Some("0"), "dit_rotation=1"));
        assert!(core_rotation_enabled(Some("1"), "dit_rotation=0"));
    }

    #[test]
    fn core_rotation_comes_out_of_the_overlap_and_keeps_the_length() {
        use PhaseKind::{
            Baseline, ChainAlone, Overlap, Prefill, Rest, Retention, RotationA, RotationB,
        };
        let deep = schedule(Level::Deep, ALL, 0);
        let rotated = add_core_rotation(Level::Deep, ALL, deep.clone());
        let kinds: Vec<PhaseKind> = rotated.iter().map(|p| p.kind).collect();
        // Baseline -> [A] -> Chain -> Overlap -> [B] -> Rest -> Retention.
        assert_eq!(
            kinds,
            vec![
                Prefill,
                Baseline(Layer::CpuRam),
                Baseline(Layer::Gpu),
                Baseline(Layer::Ssd),
                RotationA,
                ChainAlone,
                Overlap,
                RotationB,
                Rest,
                Retention,
            ]
        );
        assert_eq!(planned_seconds(&rotated, 90), planned_seconds(&deep, 90));
        let overlap = rotated.iter().find(|p| p.kind == Overlap).unwrap();
        assert_eq!(overlap.seconds, 2400 - 720, "40 -> 28 minutes");

        // Soak: one A+B per cycle, every cycle still 60 minutes.
        let soak = schedule(Level::Soak, ALL, 4);
        let rotated = add_core_rotation(Level::Soak, ALL, soak.clone());
        assert_eq!(planned_seconds(&rotated, 90), planned_seconds(&soak, 90));
        let count = |kind| rotated.iter().filter(|p| p.kind == kind).count();
        assert_eq!(
            (count(RotationA), count(RotationB), count(Overlap)),
            (3, 3, 3)
        );
        for window in rotated.windows(3) {
            if window[1].kind == Overlap {
                assert_eq!((window[0].kind, window[2].kind), (RotationA, RotationB));
                assert_eq!(window[1].seconds, 3000 - 600);
            }
        }

        // Bounded: A on the two best-ranked cores (20 s) out of the 5-minute overlap, no B.
        let bounded = schedule(Level::Bounded, ALL, 0);
        let rotated = add_core_rotation(Level::Bounded, ALL, bounded.clone());
        let kinds: Vec<PhaseKind> = rotated.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            vec![
                Prefill,
                Baseline(Layer::CpuRam),
                Baseline(Layer::Gpu),
                Baseline(Layer::Ssd),
                RotationA,
                ChainAlone,
                Overlap,
            ]
        );
        assert_eq!(planned_seconds(&rotated, 90), planned_seconds(&bounded, 90));
        assert_eq!(
            rotated.iter().find(|p| p.kind == Overlap).unwrap().seconds,
            280
        );
        // Without a GPU: no B, the overlap keeps its share.
        let no_gpu = Available { gpu: false, ..ALL };
        let rotated = add_core_rotation(Level::Deep, no_gpu, schedule(Level::Deep, no_gpu, 0));
        assert_eq!(rotated.iter().filter(|p| p.kind == RotationB).count(), 0);
        assert_eq!(
            rotated.iter().find(|p| p.kind == Overlap).unwrap().seconds,
            2400 - 240
        );
        // B: the overlap's layers but CPU-RAM, then the rotation (a CPU-RAM layer).
        assert_eq!(
            RotationB.layers(&ALL),
            vec![Layer::Gpu, Layer::Ssd, Layer::Chain, Layer::CpuRam]
        );
        assert_eq!(RotationA.layers(&ALL), vec![Layer::CpuRam]);
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
            (2, 7, Some(3), Some(2), 1, 1)
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
        assert_eq!((max.cpu_ram_workers, max.cpu_oracle_workers), (2, 14));
        assert_eq!(max.gpu_verifier_max, None);
        // At max CPU-RAM gives way to the GPU, SSD and chain feeders; balanced shares threads.
        assert_eq!(max.cpu_ram_nice, MAX_CPU_RAM_NICE);
        assert_eq!(plan.cpu_ram_nice, 0);
    }

    #[test]
    fn the_overlap_keeps_a_third_of_the_bus_and_the_oracle_takes_the_rest() {
        // Before the probe: one RAM worker per eight threads, every other one computes.
        for (n, ram) in [(1, 1), (2, 1), (8, 1), (16, 2), (32, 4), (64, 8)] {
            let plan = plan_threads(n, Intensity::Max, ALL);
            assert_eq!(plan.cpu_ram_workers, ram, "{n} threads");
            assert_eq!(plan.cpu_ram_workers + plan.cpu_oracle_workers, n);
            assert_eq!(plan.ram_split.unwrap().source, RamSplitSource::PerThreads);
        }
        // The probe of the CPU-RAM baseline on the MAG X870E 9950X3D (2026-10-16), MB/s.
        let probe: Vec<(usize, u64)> = [
            (2, 10_800),
            (3, 15_400),
            (4, 20_800),
            (6, 29_900),
            (8, 37_700),
            (10, 40_300),
            (12, 40_000),
            (16, 39_100),
        ]
        .iter()
        .map(|&(workers, mb)| (workers, mb * 1_000_000))
        .collect();
        let mut max = plan_threads(32, Intensity::Max, ALL);
        split_overlap_ram(&mut max, &probe);
        // A third of 40.3 GB/s is 13.4: three workers (15.4) are nearest, two (10.8) further.
        assert_eq!((max.cpu_ram_workers, max.cpu_oracle_workers), (3, 29));
        let split = max.ram_split.unwrap();
        assert_eq!(split.source, RamSplitSource::Probe);
        assert_eq!(split.peak_bytes_per_second, 40_300_000_000);
        assert_eq!(split.target_bytes_per_second, 13_433_333_333);
        assert_eq!(split.chosen_bytes_per_second, 15_400_000_000);
        assert_eq!(max.cpu_ram_nice, MAX_CPU_RAM_NICE);
        // Balanced: the same RAM workers, CPU-RAM's other threads to the oracle, the GPU, SSD,
        // chain and OS threads untouched.
        let before = plan_threads(32, Intensity::Balanced, ALL);
        let mut balanced = before;
        split_overlap_ram(&mut balanced, &probe);
        assert_eq!(balanced.cpu_ram_workers, 3);
        assert_eq!(
            balanced.cpu_ram_workers + balanced.cpu_oracle_workers,
            before.cpu_ram_workers + before.cpu_oracle_workers
        );
        assert_eq!(
            (
                balanced.gpu_verifier_max,
                balanced.ssd_workers,
                balanced.os_threads
            ),
            (
                before.gpu_verifier_max,
                before.ssd_workers,
                before.os_threads
            )
        );
        // A small CPU-RAM pool caps the RAM workers.
        let mut small = plan_threads(4, Intensity::Balanced, ALL);
        split_overlap_ram(&mut small, &probe);
        assert_eq!((small.cpu_ram_workers, small.cpu_oracle_workers), (1, 0));
        // A probe with noise at the target (3700X, 2026-10-17, MB/s): four workers just under a
        // third of 25.7 GB/s are nearer than six well over it.
        let noisy: Vec<(usize, u64)> = [
            (2, 4_400),
            (3, 6_400),
            (4, 8_500),
            (6, 13_200),
            (8, 16_800),
            (10, 21_100),
            (12, 25_700),
        ]
        .iter()
        .map(|&(workers, mb)| (workers, mb * 1_000_000))
        .collect();
        let mut dev = plan_threads(16, Intensity::Max, ALL);
        split_overlap_ram(&mut dev, &noisy);
        assert_eq!((dev.cpu_ram_workers, dev.cpu_oracle_workers), (4, 12));
        // On a tie the fewer RAM workers: more threads for the oracle.
        let mut tie = plan_threads(16, Intensity::Max, ALL);
        // Target 4 (a third of 12): two workers at 3 and three at 5 are both 1 away.
        split_overlap_ram(&mut tie, &[(2, 3), (3, 5), (6, 12)]);
        assert_eq!(tie.cpu_ram_workers, 2);
        // No probe (a fixed split, no report): the plan stays.
        let mut unprobed = plan_threads(32, Intensity::Max, ALL);
        split_overlap_ram(&mut unprobed, &[]);
        assert_eq!(unprobed, plan_threads(32, Intensity::Max, ALL));
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
