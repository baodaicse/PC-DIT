//! Dedicated synchronized transitions, with separate availability and attribution.
use crate::availability::{AvailabilityEvidence, AvailabilityStatus, write_durable};
use crate::{
    SystemInventory,
    compute_oracle::{
        BRANCH_PREDICTOR_LENGTH, HEAVY_ACCUMULATORS, branch_predictor_pass,
        dense_fma_scalar_reference, heavy_fp_mismatches, heavy_fp_reference, run_burst,
    },
    cpu::pin_current_thread,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

mod pool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PowerPlatformStatus {
    NoEvidence,
    Suspected,
    HardwareEventObserved,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerPlatformEvidence {
    pub status: PowerPlatformStatus,
    pub vcore_available: bool,
    pub attribution: String,
}

impl Default for PowerPlatformEvidence {
    fn default() -> Self {
        Self {
            status: PowerPlatformStatus::NoEvidence,
            vcore_available: false,
            attribution: "unattributed; absence of evidence does not establish power stability"
                .into(),
        }
    }
}

impl PowerPlatformEvidence {
    #[must_use]
    pub fn from_telemetry(telemetry: &crate::TelemetryEvidence) -> Self {
        if telemetry.edac_corrected_delta > 0
            || telemetry.edac_uncorrected_delta > 0
            || telemetry.kernel_events.machine_check_events > 0
            || telemetry.kernel_events.edac_ecc_events > 0
        {
            Self {
                status: PowerPlatformStatus::HardwareEventObserved,
                vcore_available: false,
                attribution:
                    "hardware event observed; event alone does not identify CPU, Vcore, VRM or PSU"
                        .into(),
            }
        } else {
            Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PowerVerdict {
    Pass,
    Fail,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerFault {
    Mismatch,
    MissingParticipant,
    ZeroWork,
    WorkerPanic,
    AffinityFailure,
    WorkerTimeout,
    HeavyMismatch,
    StaleGeneration,
    DuplicateParticipant,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerKernel {
    #[default]
    MixedOracle,
    HeavyFp,
}

impl PowerKernel {
    fn id(self) -> &'static str {
        match self {
            Self::MixedOracle => "mixed-oracle-v1",
            Self::HeavyFp => "heavy-fp-v1",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerCounts {
    pub requested: usize,
    pub available: usize,
    pub started: usize,
    pub ready: usize,
    pub completed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerConfig {
    /// Zero preserves the legacy count-based diagnostic modes.
    #[serde(default)]
    pub minimum_duration_ms: u64,
    #[serde(default)]
    pub progress_report: Option<PathBuf>,
    #[serde(default)]
    pub temperature_limit_enabled: bool,
    pub fault: Option<PowerFault>,
    pub core_sweep: bool,
    pub run_id: String,
    pub workers: usize,
    pub transitions: u32,
    pub idle_ms: u64,
    pub burst_ms: u64,
    pub timeout_ms: u64,
    pub max_start_spread_ms: u64,
    pub fma_iterations: u32,
    pub seed: u64,
    pub checkpoint: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionEvidence {
    #[serde(default)]
    pub planned_idle_ms: u64,
    #[serde(default)]
    pub planned_burst_ms: u64,
    #[serde(default)]
    pub timing_class: String,
    #[serde(default)]
    pub kernel: PowerKernel,
    #[serde(default)]
    pub fp_iterations_per_chunk: u32,
    #[serde(default)]
    pub fp_lane_updates_per_worker: Vec<u64>,
    #[serde(default)]
    pub actual_burst_ms_per_worker: Vec<u128>,
    #[serde(default)]
    pub workers: Option<WorkerCounts>,
    pub transition: u32,
    pub worker_cpu_ids: Vec<usize>,
    pub bursts_per_worker: Vec<u64>,
    pub start_spread_us: u128,
    pub elapsed_ms: u128,
    pub actual_idle_ms: u128,
    pub telemetry_before: crate::TelemetrySnapshot,
    pub telemetry_after: crate::TelemetrySnapshot,
    pub mismatches: u64,
    pub coverage_met: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignStep {
    #[serde(default)]
    pub idle_ms: u64,
    #[serde(default)]
    pub burst_ms: u64,
    #[serde(default)]
    pub timing_class: String,
    #[serde(default)]
    pub kernel: PowerKernel,
    pub phase: String,
    pub group_id: usize,
    pub cpu_ids: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignProgress {
    #[serde(default)]
    pub step_verified: Vec<u64>,
    pub plan_version: String,
    pub kernel: String,
    pub steps: Vec<CampaignStep>,
    pub topology_complete: bool,
    pub completed_rounds: usize,
    pub completed_in_round: usize,
    pub active_step: usize,
    pub elapsed_ms: u128,
    pub hard_deadline_ms: u64,
    pub stop_reason: Option<String>,
}

// Roles are retained even when they use the same deduplicated CPU group.
// A mixed phase alternates singleton and full-group load, without background load.
fn campaign_steps(selected: &[usize], topology: &[crate::CpuThread]) -> Vec<CampaignStep> {
    let groups = worker_groups(selected, topology, true);
    let mut steps = Vec::new();
    let mut add = |phase: &str, cpu_ids: Vec<usize>| {
        let group_id = groups
            .iter()
            .position(|g| *g == cpu_ids)
            .expect("planned group");
        for kernel in [PowerKernel::MixedOracle, PowerKernel::HeavyFp] {
            steps.push(CampaignStep {
                idle_ms: 0,
                burst_ms: 0,
                timing_class: String::new(),
                kernel,
                phase: phase.into(),
                group_id,
                cpu_ids: cpu_ids.clone(),
            });
        }
    };
    for &cpu in selected {
        add("SINGLE_THREAD", vec![cpu]);
    }
    let mut physical = std::collections::BTreeMap::<(i32, i32), Vec<usize>>::new();
    for t in topology
        .iter()
        .filter(|t| selected.contains(&t.cpu_id) && t.package_id >= 0 && t.core_id >= 0)
    {
        physical
            .entry((t.package_id, t.core_id))
            .or_default()
            .push(t.cpu_id);
    }
    for group in physical.into_values() {
        add("CORE_SMT", group);
    }
    add("ALL_CPU", selected.to_vec());
    if selected.len() > 1 {
        for &cpu in selected {
            add("MIXED_SMALL", vec![cpu]);
            add("MIXED_ALL", selected.to_vec());
        }
    }
    steps
}

fn resolve_timing(steps: &mut [CampaignStep], config: &PowerConfig) {
    for (index, step) in steps.iter_mut().enumerate() {
        let mut random = config
            .seed
            .wrapping_add(index as u64)
            .wrapping_add(0x9e37_79b9_7f4a_7c15);
        random = (random ^ (random >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        random = (random ^ (random >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        random ^= random >> 31;
        let class = (index + (config.seed & 3) as usize) % 4;
        let duration = |base: u64, long: bool, bits: u64| {
            if long {
                base.max(2) + bits % (base.max(2) + 1)
            } else {
                base / 4 + bits % (base / 4 + 1)
            }
        };
        step.idle_ms = duration(config.idle_ms, class >= 2, random);
        step.burst_ms = duration(config.burst_ms, class % 2 == 1, random >> 32).max(1);
        step.timing_class = ["SHORT_SHORT", "SHORT_LONG", "LONG_SHORT", "LONG_LONG"][class].into();
    }
}

/// Supervisor budget covers the longest planned idle, ready/completion bounds,
/// and checkpoint overhead; heartbeat normally advances every 100..250 ms.
#[must_use]
pub fn watchdog_stale_seconds(idle_ms: u64, timeout_ms: u64, campaign: bool) -> u64 {
    let maximum_idle = if campaign {
        idle_ms.max(2).saturating_mul(2)
    } else {
        idle_ms
    };
    maximum_idle
        .saturating_add(timeout_ms.saturating_mul(2))
        .saturating_add(5000)
        .div_ceil(1000)
        .max(15)
}

fn campaign_finished(completed: usize, steps: usize, elapsed_ms: u128, minimum_ms: u64) -> bool {
    completed > 0 && completed.is_multiple_of(steps) && elapsed_ms >= u128::from(minimum_ms)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerReport {
    #[serde(default)]
    pub watchdog_stale_seconds: u64,
    #[serde(default)]
    pub total_transitions: usize,
    #[serde(default)]
    pub verified_transitions: usize,
    #[serde(default)]
    pub total_bursts: u64,
    #[serde(default)]
    pub total_mismatches: u64,
    #[serde(default)]
    pub transitions_omitted: usize,
    #[serde(default)]
    pub first_failure: Option<TransitionEvidence>,
    #[serde(default)]
    pub pool_workers_started: usize,
    #[serde(default)]
    pub checkpoint_interval_ms: u64,
    #[serde(skip)]
    last_persisted: Option<Instant>,
    #[serde(skip)]
    runtime_started: Option<Instant>,
    #[serde(default)]
    pub active_workers: Option<WorkerCounts>,
    #[serde(default)]
    pub safety: Option<crate::SafetyEvidence>,
    #[serde(default)]
    pub campaign: Option<CampaignProgress>,
    pub schema_version: u32,
    pub boot_id: String,
    pub owner_pid: u32,
    pub config: PowerConfig,
    pub verdict: PowerVerdict,
    pub availability: AvailabilityEvidence,
    pub power_platform: PowerPlatformEvidence,
    pub last_phase: String,
    pub active_transition: u32,
    pub transitions: Vec<TransitionEvidence>,
    pub telemetry: Option<crate::TelemetryEvidence>,
    pub worker_groups: Vec<Vec<usize>>,
    pub topology: Vec<crate::CpuThread>,
    pub instruction_coverage: Vec<InstructionEvidence>,
    pub limitations: Vec<String>,
}

/// Unknown opcode execution is represented as null, not inferred from utilization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstructionEvidence {
    pub instruction: String,
    pub supported_by_cpu: bool,
    pub enabled_in_binary: Option<bool>,
    pub executed: Option<bool>,
    pub verified: bool,
}

fn instruction_inventory(cpu: &crate::CpuInventory) -> Vec<InstructionEvidence> {
    [
        ("avx2", cpu.avx2, Some(cfg!(target_feature = "avx2"))),
        ("fma", cpu.fma, Some(cfg!(target_feature = "fma"))),
        ("aes-ni", cpu.aes, None),
    ]
    .into_iter()
    .map(|(name, supported, enabled)| InstructionEvidence {
        instruction: name.into(),
        supported_by_cpu: supported,
        enabled_in_binary: enabled,
        executed: None,
        verified: false,
    })
    .collect()
}

struct Outcome {
    cpu: usize,
    start: Instant,
    bursts: u64,
    mismatches: u64,
    affinity_ok: bool,
    burst_elapsed_ms: u128,
}

/// Run bounded native transitions. Checkpoint destination must be operator-authorized.
///
/// # Errors
/// Rejects invalid configuration, unavailable CPUs and checkpoint I/O failures.
#[allow(clippy::too_many_lines)] // Campaign setup, checkpoints and final evidence remain in execution order.
pub fn run(config: &PowerConfig) -> Result<PowerReport, String> {
    validate(config)?;
    if config.checkpoint.exists() {
        return Err("checkpoint already exists; recover it or select a new run path".into());
    }
    let inventory = SystemInventory::collect().map_err(|e| e.to_string())?;
    let cpus = &inventory.cpu.allowed_cpu_ids;
    if cpus.len() < config.workers {
        return Err("insufficient allowed CPUs for requested worker coverage".into());
    }
    let groups = worker_groups(
        &cpus[..config.workers],
        &inventory.cpu.threads,
        config.core_sweep || config.minimum_duration_ms > 0,
    );
    let expected_transitions = config.transitions as usize * groups.len();
    let campaign = if config.minimum_duration_ms > 0 {
        let mut steps = campaign_steps(&cpus[..config.workers], &inventory.cpu.threads);
        resolve_timing(&mut steps, config);
        if steps.len() > 4096 {
            return Err("campaign plan exceeds 4096 steps per round".into());
        }
        Some(CampaignProgress {
            plan_version: "DIT-POWER-CAMPAIGN-v3".into(),
            kernel: "mixed-oracle-v1".into(),
            topology_complete: cpus[..config.workers].iter().all(|cpu| {
                inventory
                    .cpu
                    .threads
                    .iter()
                    .any(|t| t.cpu_id == *cpu && t.package_id >= 0 && t.core_id >= 0)
            }),
            hard_deadline_ms: config
                .minimum_duration_ms
                .saturating_add(
                    (config.idle_ms.max(2) * 2 + 2 * config.timeout_ms + 1000)
                        .saturating_mul(steps.len() as u64),
                )
                .saturating_add(30_000),
            step_verified: vec![0; steps.len()],
            steps,
            completed_rounds: 0,
            completed_in_round: 0,
            active_step: 0,
            elapsed_ms: 0,
            stop_reason: None,
        })
    } else {
        None
    };
    let deadline_ms = campaign.as_ref().map_or_else(
        || {
            (config.idle_ms + config.timeout_ms)
                .saturating_mul(expected_transitions as u64)
                .saturating_add(30_000)
        },
        |p| p.hard_deadline_ms,
    );
    let mut report = PowerReport {
        schema_version: 3, campaign, active_workers: None, safety: None, config: config.clone(), verdict: PowerVerdict::Incomplete,
        total_transitions: 0, verified_transitions: 0, total_bursts: 0, total_mismatches: 0,
        transitions_omitted: 0, first_failure: None, pool_workers_started: 0,
        checkpoint_interval_ms: 5000, last_persisted: None, runtime_started: None,
        watchdog_stale_seconds: watchdog_stale_seconds(config.idle_ms, config.timeout_ms, config.minimum_duration_ms > 0),
        boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(|e| e.to_string())?.trim().into(),
        owner_pid: std::process::id(),
        availability: AvailabilityEvidence::new(AvailabilityStatus::Running, "transition campaign started"),
        power_platform: PowerPlatformEvidence::default(), last_phase: "START".into(),
        active_transition: 0, transitions: vec![], telemetry: None,
        worker_groups: groups.clone(), topology: inventory.cpu.threads.clone(),
        instruction_coverage: instruction_inventory(&inventory.cpu),
        limitations: vec!["Idle is best effort; C-state residency and Vcore droop are not measured".into(),
            "Scratch memory is 64 KiB per worker; this profile does not replace full RAM coverage".into(),
            "A process watchdog is required for kernel/worker hangs; power loss requires subsequent recovery".into()],
    };
    checkpoint(&mut report, "START")?;
    let monitor = crate::safety::SafetyMonitor::start(
        crate::SafetyPolicy {
            temperature_limit_enabled: config.temperature_limit_enabled,
            deadline_ms,
            ..crate::SafetyPolicy::default()
        },
        crate::SafetyFaultInjection::default(),
    );
    let control = Arc::clone(&monitor.control);
    let started = Instant::now();
    report.runtime_started = Some(started);
    let mut pool = pool::Pool::new(&cpus[..config.workers], &control)?;
    report.pool_workers_started = config.workers;
    let transition_limit = if report.campaign.is_some() {
        u32::MAX as usize
    } else {
        expected_transitions
    };
    for index in 0..transition_limit {
        let id = u32::try_from(index).map_err(|_| "transition count overflow")?;
        if control.should_stop() {
            break;
        }
        report.active_transition = id;
        let active_cpus = if let Some(plan) = &mut report.campaign {
            plan.active_step = index % plan.steps.len();
            plan.kernel = plan.steps[plan.active_step].kernel.id().into();
            plan.elapsed_ms = started.elapsed().as_millis();
            plan.steps[plan.active_step].cpu_ids.clone()
        } else {
            groups[index / config.transitions as usize].clone()
        };
        report.active_workers = Some(WorkerCounts {
            requested: active_cpus.len(),
            available: cpus.len(),
            ..WorkerCounts::default()
        });
        publish(&mut report, "ARM", false)?;
        let mut resolved = config.clone();
        if let Some(plan) = &report.campaign {
            resolved.idle_ms = plan.steps[plan.active_step].idle_ms;
            resolved.burst_ms = plan.steps[plan.active_step].burst_ms;
        }
        match transition(&resolved, &active_cpus, id, &mut report, &mut pool) {
            Ok(evidence) => {
                let failed = evidence.mismatches > 0;
                let covered = evidence.coverage_met;
                retain_transition(&mut report, evidence);
                if let Some(plan) = &mut report.campaign {
                    plan.elapsed_ms = started.elapsed().as_millis();
                    if covered {
                        plan.completed_rounds = report.total_transitions / plan.steps.len();
                        plan.completed_in_round = report.total_transitions % plan.steps.len();
                    }
                }
                publish(&mut report, "VERIFY", failed || !covered)?;
                if failed {
                    report.verdict = PowerVerdict::Fail;
                    break;
                }
                if !covered {
                    break;
                }
            }
            Err(detail) => {
                report.availability =
                    AvailabilityEvidence::new(AvailabilityStatus::WorkerFailed, detail);
                break;
            }
        }
        publish(&mut report, "TRANSITION_DONE", false)?;
        if report.campaign.as_ref().is_some_and(|plan| {
            campaign_finished(
                report.total_transitions,
                plan.steps.len(),
                plan.elapsed_ms,
                config.minimum_duration_ms,
            )
        }) {
            break;
        }
    }
    if let Some(plan) = &mut report.campaign {
        plan.elapsed_ms = started.elapsed().as_millis();
        if report.total_transitions == transition_limit
            && !campaign_finished(
                report.total_transitions,
                plan.steps.len(),
                plan.elapsed_ms,
                config.minimum_duration_ms,
            )
        {
            plan.stop_reason = Some("transition ID limit reached; coverage incomplete".into());
        }
    }
    drop(pool);
    let (telemetry, safety) = monitor.finish();
    report.power_platform = PowerPlatformEvidence::from_telemetry(&telemetry);
    report.telemetry = Some(telemetry);
    let covered =
        report
            .campaign
            .as_ref()
            .map_or(report.total_transitions == expected_transitions, |plan| {
                plan.topology_complete
                    && plan.step_verified.iter().all(|count| *count > 0)
                    && campaign_finished(
                        report.total_transitions,
                        plan.steps.len(),
                        plan.elapsed_ms,
                        config.minimum_duration_ms,
                    )
            })
            && report.total_transitions == report.verified_transitions
            && !safety.stop_requested;
    if !safety.violations.is_empty() {
        report.verdict = PowerVerdict::Fail;
    }
    if report.verdict != PowerVerdict::Fail && covered {
        report.verdict = PowerVerdict::Pass;
    }
    if report.availability.status == AvailabilityStatus::Running {
        report.availability = AvailabilityEvidence::new(
            if covered {
                AvailabilityStatus::Completed
            } else {
                AvailabilityStatus::Incomplete
            },
            "see transition coverage and independent correctness verdict",
        );
    }
    if let Some(plan) = &mut report.campaign {
        if !plan.topology_complete {
            plan.stop_reason =
                Some("incomplete topology; core/SMT coverage cannot be established".into());
        }
        if let Some(event) = &safety.stop_event {
            plan.stop_reason = Some(event.reason.clone());
        }
    }
    report.safety = Some(safety);
    validate_report(&report)?;
    checkpoint(&mut report, "FINAL")?;
    Ok(report)
}

/// Validate schema-3 aggregate and PASS coverage contracts. Historical reports
/// retain their original schema semantics; unknown schemas are never accepted.
///
/// # Errors
/// Rejects inconsistent retained history, timing recipes or unsupported PASS claims.
pub fn validate_report(report: &PowerReport) -> Result<(), String> {
    if !matches!(report.schema_version, 1..=3) {
        return Err("unsupported power report schema".into());
    }
    if report.schema_version < 3 {
        return Ok(());
    }
    if report.transitions.len() > 128
        || report
            .transitions
            .len()
            .checked_add(report.transitions_omitted)
            != Some(report.total_transitions)
        || report.verified_transitions > report.total_transitions
    {
        return Err("inconsistent power history/aggregate counters".into());
    }
    if let Some(plan) = &report.campaign {
        let selected = report
            .worker_groups
            .iter()
            .max_by_key(|g| g.len())
            .ok_or("missing worker groups")?;
        let mut expected = campaign_steps(selected, &report.topology);
        resolve_timing(&mut expected, &report.config);
        if plan.plan_version != "DIT-POWER-CAMPAIGN-v3"
            || plan.steps != expected
            || plan.steps.is_empty()
            || plan.step_verified.len() != plan.steps.len()
            || plan.active_step >= plan.steps.len()
        {
            return Err("invalid campaign recipe or step coverage".into());
        }
        if report.verdict == PowerVerdict::Pass
            && (!plan.topology_complete
                || !campaign_finished(
                    report.total_transitions,
                    plan.steps.len(),
                    plan.elapsed_ms,
                    report.config.minimum_duration_ms,
                )
                || plan.completed_in_round != 0
                || plan.completed_rounds != report.total_transitions / plan.steps.len()
                || plan
                    .step_verified
                    .iter()
                    .any(|count| *count != plan.completed_rounds as u64))
        {
            return Err("PASS lacks complete campaign coverage".into());
        }
    }
    if report.verdict == PowerVerdict::Pass
        && (report.availability.status != AvailabilityStatus::Completed
            || report.total_transitions == 0
            || report.total_transitions != report.verified_transitions
            || report.total_mismatches != 0
            || report.first_failure.is_some()
            || report.pool_workers_started != report.config.workers
            || report
                .transitions
                .iter()
                .any(|t| !t.coverage_met || t.mismatches > 0))
    {
        return Err("unsupported power PASS evidence".into());
    }
    Ok(())
}

fn worker_groups(
    selected: &[usize],
    topology: &[crate::CpuThread],
    sweep: bool,
) -> Vec<Vec<usize>> {
    if !sweep {
        return vec![selected.to_vec()];
    }
    let mut groups: Vec<Vec<usize>> = selected.iter().map(|&cpu| vec![cpu]).collect();
    let mut physical = std::collections::BTreeMap::<(i32, i32), Vec<usize>>::new();
    for thread in topology
        .iter()
        .filter(|t| selected.contains(&t.cpu_id) && t.package_id >= 0 && t.core_id >= 0)
    {
        physical
            .entry((thread.package_id, thread.core_id))
            .or_default()
            .push(thread.cpu_id);
    }
    for group in physical.into_values() {
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    if !groups.contains(&selected.to_vec()) {
        groups.push(selected.to_vec());
    }
    groups
}

fn validate(c: &PowerConfig) -> Result<(), String> {
    if c.run_id.is_empty()
        || c.workers == 0
        || c.transitions == 0
        || c.transitions > 100_000
        || c.idle_ms > 60_000
        || c.burst_ms == 0
        || c.burst_ms > 60_000
        || c.timeout_ms <= c.burst_ms
        || c.timeout_ms > 120_000
        || c.max_start_spread_ms == 0
        || c.fma_iterations == 0
        || c.fma_iterations > 2_000_000
        || c.minimum_duration_ms > 86_400_000
        || (c.minimum_duration_ms > 0 && (c.core_sweep || c.transitions != 1))
        || (c.minimum_duration_ms > 0
            && (c.idle_ms > 30_000 || c.burst_ms > 30_000 || c.timeout_ms <= c.burst_ms.max(2) * 2))
        || c.progress_report.as_ref() == Some(&c.checkpoint)
    {
        return Err("invalid power profile: positive workers/transitions/burst/depth required; timeout must exceed burst; bounded timing/depth limits apply".into());
    }
    Ok(())
}

fn checkpoint(report: &mut PowerReport, phase: &str) -> Result<(), String> {
    publish(report, phase, true)
}

fn retain_transition(report: &mut PowerReport, evidence: TransitionEvidence) {
    report.total_transitions += 1;
    report.total_bursts = report
        .total_bursts
        .saturating_add(evidence.bursts_per_worker.iter().sum::<u64>());
    report.total_mismatches = report.total_mismatches.saturating_add(evidence.mismatches);
    if evidence.coverage_met && evidence.mismatches == 0 {
        report.verified_transitions += 1;
        if let Some(plan) = &mut report.campaign {
            plan.step_verified[plan.active_step] += 1;
        }
    } else if report.first_failure.is_none() {
        report.first_failure = Some(evidence.clone());
    }
    if report.transitions.len() == 128 {
        report.transitions.remove(0);
        report.transitions_omitted += 1;
    }
    report.transitions.push(evidence);
}

fn heartbeat(report: &PowerReport) -> Result<(), String> {
    if let Some(path) = &report.config.progress_report {
        let step = report.campaign.as_ref().map(|p| &p.steps[p.active_step]);
        let snapshot = serde_json::json!({
            "schema_version": 1, "kind": "power-platform-progress", "run_id": report.config.run_id,
            "owner_pid": report.owner_pid, "boot_id": report.boot_id, "last_phase": report.last_phase,
            "active_transition": report.active_transition, "step": step, "availability": report.availability,
            "total_transitions": report.total_transitions, "workers": report.active_workers,
            "elapsed_ms": report.runtime_started.map(|t| t.elapsed().as_millis()),
        });
        crate::availability::write_progress(path, &snapshot).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn publish(report: &mut PowerReport, phase: &str, force: bool) -> Result<(), String> {
    report.last_phase = phase.into();
    if let (Some(start), Some(plan)) = (report.runtime_started, &mut report.campaign) {
        plan.elapsed_ms = start.elapsed().as_millis();
    }
    if force
        || report.campaign.is_none()
        || report
            .last_persisted
            .is_none_or(|t| t.elapsed().as_millis() >= u128::from(report.checkpoint_interval_ms))
    {
        write_durable(&report.config.checkpoint, report).map_err(|e| e.to_string())?;
        report.last_persisted = Some(Instant::now());
    }
    heartbeat(report)?;
    let campaign_fields = report.campaign.as_ref().map_or_else(String::new, |p| {
        let step = &p.steps[p.active_step];
        let ids = step.cpu_ids.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
        format!(" mode=campaign campaign_phase={} group={} active_cpus={} active_workers={} kernel={} rounds={} round_completed={} round_target={} elapsed_ms={} minimum_ms={} topology_complete={} idle_ms={} burst_ms={} timing={}",
            step.phase, step.group_id, ids, step.cpu_ids.len(), p.kernel, p.completed_rounds,
            p.completed_in_round, p.steps.len(), p.elapsed_ms, report.config.minimum_duration_ms, p.topology_complete, step.idle_ms, step.burst_ms, step.timing_class)
    });
    eprintln!(
        "CPU_DIT_POWER_PROGRESS phase={phase} completed={} target={} workers={} bursts={} failures={} availability={:?} power_platform={:?} seed=0x{:016x}{campaign_fields}",
        report.total_transitions,
        if report.campaign.is_some() {
            0
        } else {
            report.worker_groups.len() * report.config.transitions as usize
        },
        report.config.workers,
        report.total_bursts,
        report.total_mismatches,
        report.availability.status,
        report.power_platform.status,
        report.config.seed
    );
    Ok(())
}

#[allow(clippy::too_many_lines)] // Preparation, barrier and evidence stay in phase order.
fn transition(
    config: &PowerConfig,
    cpus: &[usize],
    id: u32,
    report: &mut PowerReport,
    pool: &mut pool::Pool,
) -> Result<TransitionEvidence, String> {
    let seed = config.seed.wrapping_add(u64::from(id));
    let kernel = report
        .campaign
        .as_ref()
        .map_or(PowerKernel::MixedOracle, |p| p.steps[p.active_step].kernel);
    let iterations = if kernel == PowerKernel::HeavyFp {
        config.fma_iterations.min(4096)
    } else {
        config.fma_iterations
    };
    let job = Arc::new(pool::Job {
        generation: id,
        seed,
        kernel,
        iterations,
        idle_ms: config.idle_ms,
        burst_ms: config.burst_ms,
        timeout_ms: config.timeout_ms,
        fault: config.fault,
        first_cpu: cpus[0],
        reference: if kernel == PowerKernel::MixedOracle {
            dense_fma_scalar_reference(seed, iterations)
        } else {
            [0.0; 4]
        },
        branch: if kernel == PowerKernel::MixedOracle {
            branch_predictor_pass(seed, BRANCH_PREDICTOR_LENGTH)
        } else {
            0
        },
        heavy_reference: if kernel == PowerKernel::HeavyFp {
            heavy_fp_reference(seed, iterations)
        } else {
            [[0.0; 4]; HEAVY_ACCUMULATORS]
        },
        gate: Arc::new((Mutex::new(false), Condvar::new())),
    });
    let started = Instant::now();
    let collector = crate::telemetry::TelemetryCollector::default();
    let telemetry_before = collector.collect();
    pool.dispatch(cpus, &job, report)?;
    pool.collect(cpus, id, true, config.timeout_ms, report)?;
    publish(report, "BURST_ARMED", id == 0)?;
    let idle_started = Instant::now();
    pool.idle(config.idle_ms, report)?;
    let actual_idle_ms = idle_started.elapsed().as_millis();
    {
        let mut open = job.gate.0.lock().map_err(|_| "gate poisoned")?;
        *open = true;
        job.gate.1.notify_all();
    }
    let mut outcomes = pool.collect(cpus, id, false, config.timeout_ms, report)?;
    outcomes.sort_by_key(|o| o.cpu);
    let first = outcomes
        .iter()
        .map(|o| o.start)
        .min()
        .ok_or("no worker evidence")?;
    let spread = outcomes
        .iter()
        .map(|o| o.start.duration_since(first).as_micros())
        .max()
        .unwrap_or(0);
    let coverage_met = outcomes.len() == cpus.len()
        && outcomes.iter().all(|o| o.affinity_ok && o.bursts > 0)
        && spread <= u128::from(config.max_start_spread_ms) * 1000;
    Ok(TransitionEvidence {
        planned_idle_ms: config.idle_ms,
        planned_burst_ms: config.burst_ms,
        timing_class: report.campaign.as_ref().map_or_else(
            || "FIXED".into(),
            |p| p.steps[p.active_step].timing_class.clone(),
        ),
        kernel,
        fp_iterations_per_chunk: iterations,
        fp_lane_updates_per_worker: outcomes
            .iter()
            .map(|o| {
                o.bursts
                    * u64::from(iterations)
                    * 4
                    * if kernel == PowerKernel::HeavyFp {
                        HEAVY_ACCUMULATORS as u64
                    } else {
                        1
                    }
            })
            .collect(),
        actual_burst_ms_per_worker: outcomes.iter().map(|o| o.burst_elapsed_ms).collect(),
        workers: report.active_workers.clone(),
        transition: id,
        worker_cpu_ids: outcomes.iter().map(|o| o.cpu).collect(),
        bursts_per_worker: outcomes.iter().map(|o| o.bursts).collect(),
        start_spread_us: spread,
        elapsed_ms: started.elapsed().as_millis(),
        actual_idle_ms,
        telemetry_before,
        telemetry_after: collector.collect(),
        mismatches: outcomes.iter().map(|o| o.mismatches).sum(),
        coverage_met,
    })
}

/// Recover a checkpoint after the operator establishes that its worker is no longer running.
/// The source checkpoint is preserved; interruption alone never attributes a power fault.
///
/// # Errors
/// Returns invalid JSON or checkpoint read errors.
pub fn recover(path: &Path) -> Result<PowerReport, String> {
    let mut report: PowerReport =
        serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if !matches!(report.schema_version, 1..=3) {
        return Err("unsupported power report schema".into());
    }
    if report.availability.status == AvailabilityStatus::Running {
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|e| e.to_string())?;
        if boot.trim() == report.boot_id
            && Path::new(&format!("/proc/{}", report.owner_pid)).exists()
        {
            return Err("owner process still exists in this boot; stop it before recovery".into());
        }
        report.verdict = if report.verdict == PowerVerdict::Fail
            || report.total_mismatches > 0
            || report.transitions.iter().any(|t| t.mismatches > 0)
        {
            PowerVerdict::Fail
        } else {
            PowerVerdict::Incomplete
        };
        report.availability = AvailabilityEvidence::new(
            AvailabilityStatus::Interrupted,
            "unclosed checkpoint recovered; cause unattributed",
        );
    }
    validate_report(&report)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn config() -> PowerConfig {
        PowerConfig {
            minimum_duration_ms: 0,
            progress_report: None,
            temperature_limit_enabled: false,
            core_sweep: false,
            run_id: "test-transition".into(),
            workers: 1,
            transitions: 2,
            idle_ms: 1,
            burst_ms: 1,
            timeout_ms: 1000,
            max_start_spread_ms: 100,
            fma_iterations: 10,
            seed: 123,
            fault: None,
            checkpoint: std::env::temp_dir().join(format!(
                "dit-power-{}-{}.json",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            )),
        }
    }

    #[test]
    fn completed_transitions_have_independent_availability_and_power_evidence() {
        let mut c = config();
        c.progress_report = Some(c.checkpoint.with_extension("heartbeat.json"));
        let report = run(&c).unwrap();
        let heartbeat: serde_json::Value =
            serde_json::from_slice(&std::fs::read(c.progress_report.as_ref().unwrap()).unwrap())
                .unwrap();
        assert_eq!(heartbeat["last_phase"], "FINAL");
        assert_eq!(heartbeat["availability"]["status"], "COMPLETED");
        assert_eq!(heartbeat["run_id"], report.config.run_id);
        assert_eq!(heartbeat["kind"], "power-platform-progress");
        assert_eq!(report.verdict, PowerVerdict::Pass);
        assert_eq!(report.availability.status, AvailabilityStatus::Completed);
        assert_eq!(
            report.power_platform.status,
            PowerPlatformStatus::NoEvidence
        );
        assert_eq!(report.transitions.len(), 2);
        assert!(
            report
                .transitions
                .iter()
                .all(|t| t.bursts_per_worker[0] > 0 && t.coverage_met)
        );
        assert!(
            run(&c).is_err(),
            "existing evidence must not be overwritten"
        );
        assert_eq!(recover(&c.checkpoint).unwrap().verdict, PowerVerdict::Pass);
        std::fs::remove_file(c.progress_report.unwrap()).unwrap();
        std::fs::remove_file(c.checkpoint).unwrap();
    }

    #[test]
    fn fault_campaign_never_passes_or_attributes_cpu_power() {
        for fault in [
            PowerFault::Mismatch,
            PowerFault::MissingParticipant,
            PowerFault::ZeroWork,
            PowerFault::WorkerPanic,
            PowerFault::AffinityFailure,
            PowerFault::WorkerTimeout,
            PowerFault::StaleGeneration,
            PowerFault::DuplicateParticipant,
        ] {
            let mut c = config();
            c.fault = Some(fault);
            let report = run(&c).unwrap();
            assert_ne!(report.verdict, PowerVerdict::Pass, "{fault:?}");
            if fault == PowerFault::Mismatch {
                assert_eq!(report.verdict, PowerVerdict::Fail);
            }
            assert_eq!(
                report.power_platform.status,
                PowerPlatformStatus::NoEvidence
            );
            std::fs::remove_file(c.checkpoint).unwrap();
        }
    }

    #[test]
    fn unclosed_checkpoint_recovers_without_power_attribution() {
        let c = config();
        let mut report = run(&c).unwrap();
        report.availability.status = AvailabilityStatus::Running;
        checkpoint(&mut report, "BURST_ARMED").unwrap();
        assert!(
            recover(&c.checkpoint).is_err(),
            "live owner must not be recovered"
        );
        report.owner_pid = u32::MAX;
        checkpoint(&mut report, "BURST_ARMED").unwrap();
        let recovered = recover(&c.checkpoint).unwrap();
        assert_eq!(recovered.verdict, PowerVerdict::Incomplete);
        assert_eq!(
            recovered.availability.status,
            AvailabilityStatus::Interrupted
        );
        assert_eq!(recovered.last_phase, "BURST_ARMED");
        assert_eq!(
            recovered.power_platform.status,
            PowerPlatformStatus::NoEvidence
        );
        let original: PowerReport =
            serde_json::from_slice(&std::fs::read(&c.checkpoint).unwrap()).unwrap();
        assert_eq!(original.availability.status, AvailabilityStatus::Running);
        report.transitions[0].mismatches = 1;
        checkpoint(&mut report, "VERIFY").unwrap();
        assert_eq!(
            recover(&c.checkpoint).unwrap().verdict,
            PowerVerdict::Fail,
            "interruption must not erase an already recorded mismatch"
        );
        std::fs::remove_file(c.checkpoint).unwrap();
    }

    #[test]
    fn invalid_configuration_rejected_before_checkpoint() {
        let mut c = config();
        c.workers = 0;
        assert!(run(&c).is_err());
        assert!(!c.checkpoint.exists());
        c.workers = 1;
        c.timeout_ms = c.burst_ms;
        assert!(run(&c).is_err());
        assert!(!c.checkpoint.exists());
    }

    #[test]
    fn campaign_plan_keeps_roles_and_respects_allowed_topology() {
        let topology = vec![
            crate::CpuThread {
                cpu_id: 3,
                package_id: 0,
                core_id: 1,
                numa_node: None,
                allowed: true,
            },
            crate::CpuThread {
                cpu_id: 7,
                package_id: 0,
                core_id: 1,
                numa_node: None,
                allowed: true,
            },
            crate::CpuThread {
                cpu_id: 9,
                package_id: 1,
                core_id: 1,
                numa_node: None,
                allowed: true,
            },
            crate::CpuThread {
                cpu_id: 12,
                package_id: 1,
                core_id: 1,
                numa_node: None,
                allowed: false,
            },
        ];
        let steps = campaign_steps(&[3, 7, 9], &topology);
        assert_eq!(steps, campaign_steps(&[3, 7, 9], &topology));
        assert_eq!(steps.len(), 24);
        assert!(
            steps
                .iter()
                .all(|s| s.cpu_ids.iter().all(|c| [3, 7, 9].contains(c)))
        );
        assert_eq!(steps[6].phase, "CORE_SMT");
        assert_eq!(steps[6].cpu_ids, vec![3, 7]);
        assert_eq!(steps[8].cpu_ids, vec![9]);
        assert_eq!(steps[10].phase, "ALL_CPU");
        assert_eq!(steps[12].phase, "MIXED_SMALL");
        assert_eq!(steps[14].phase, "MIXED_ALL");
        for pair in steps.as_chunks::<2>().0 {
            assert_eq!(pair[0].cpu_ids, pair[1].cpu_ids);
            assert_eq!(pair[0].kernel, PowerKernel::MixedOracle);
            assert_eq!(pair[1].kernel, PowerKernel::HeavyFp);
        }
        let single = campaign_steps(&[9], &topology);
        assert_eq!(single.len(), 6);
        assert!(single.iter().all(|s| s.group_id == 0));
        let missing = campaign_steps(&[3, 9], &[]);
        assert!(!missing.iter().any(|s| s.phase == "CORE_SMT"));
    }

    #[test]
    fn simulated_duration_boundary_requires_full_round() {
        assert!(!campaign_finished(0, 4, 2000, 1000));
        assert!(!campaign_finished(4, 4, 999, 1000));
        assert!(!campaign_finished(5, 4, 1000, 1000));
        assert!(!campaign_finished(7, 4, 2000, 1000));
        assert!(campaign_finished(8, 4, 2000, 1000));
    }

    #[test]
    fn timing_recipe_replays_and_covers_all_classes_with_bounded_deadlines() {
        let mut c = config();
        c.idle_ms = 500;
        c.burst_ms = 100;
        let mut first = campaign_steps(&[3, 7], &[]);
        resolve_timing(&mut first, &c);
        let mut same = campaign_steps(&[3, 7], &[]);
        resolve_timing(&mut same, &c);
        assert_eq!(first, same);
        let classes: std::collections::BTreeSet<_> =
            first.iter().map(|s| s.timing_class.as_str()).collect();
        assert_eq!(classes.len(), 4);
        assert!(
            first
                .iter()
                .all(|s| s.idle_ms <= 1000 && s.burst_ms <= 200 && s.burst_ms > 0)
        );
        c.seed += 1;
        resolve_timing(&mut same, &c);
        assert_ne!(first, same);
        assert_eq!(watchdog_stale_seconds(500, 5000, true), 16);
        assert_eq!(watchdog_stale_seconds(30_000, 120_000, true), 305);
        c.minimum_duration_ms = 10;
        c.transitions = 1;
        c.timeout_ms = 200;
        assert!(
            validate(&c).is_err(),
            "timeout must exceed maximum seeded burst"
        );
    }

    #[test]
    fn bounded_history_retains_aggregate_coverage_and_first_failure() {
        let c = config();
        let mut report = run(&c).unwrap();
        let mut evidence = report.transitions[0].clone();
        evidence.mismatches = 1;
        retain_transition(&mut report, evidence.clone());
        evidence.mismatches = 0;
        for index in 0..5000 {
            evidence.transition = index + 3;
            retain_transition(&mut report, evidence.clone());
        }
        assert_eq!(report.transitions.len(), 128);
        assert_eq!(report.total_transitions, 5003);
        assert_eq!(report.transitions_omitted, 4875);
        assert_eq!(report.verified_transitions, 5002);
        assert_eq!(report.total_mismatches, 1);
        assert_eq!(report.first_failure.as_ref().unwrap().mismatches, 1);
        let before = serde_json::to_vec(&report).unwrap().len();
        for _ in 0..5000 {
            retain_transition(&mut report, evidence.clone());
        }
        assert!(serde_json::to_vec(&report).unwrap().len() <= before + 100);
        report.availability.status = AvailabilityStatus::Running;
        report.owner_pid = u32::MAX;
        checkpoint(&mut report, "VERIFY").unwrap();
        assert_eq!(recover(&c.checkpoint).unwrap().verdict, PowerVerdict::Fail);
        std::fs::remove_file(c.checkpoint).unwrap();
    }

    #[test]
    fn pool_reuses_threads_and_idle_is_cancellable() {
        let c = config();
        let mut report = run(&c).unwrap();
        let cpus = report.transitions[0].worker_cpu_ids.clone();
        let monitor = crate::safety::SafetyMonitor::start(
            crate::SafetyPolicy::default(),
            crate::SafetyFaultInjection::default(),
        );
        let mut pool = pool::Pool::new(&cpus, &monitor.control).unwrap();
        let ids = pool.thread_ids();
        for id in 10..15 {
            let evidence = transition(&c, &cpus, id, &mut report, &mut pool).unwrap();
            assert!(evidence.coverage_met);
            assert_eq!(ids, pool.thread_ids());
        }
        monitor
            .control
            .cancel(crate::AbortKind::Deadline, "test cancellation");
        let before = Instant::now();
        assert!(pool.idle(60000, &report).is_err());
        drop(pool);
        assert!(before.elapsed() < Duration::from_secs(1));
        let _ = monitor.finish();
        std::fs::remove_file(c.checkpoint).unwrap();
    }

    #[test]
    fn progress_is_small_and_checkpoint_is_rate_limited_but_failure_is_durable() {
        let mut c = config();
        c.minimum_duration_ms = 10;
        c.transitions = 1;
        c.progress_report = Some(c.checkpoint.with_extension("progress.json"));
        let mut report = run(&c).unwrap();
        let durable = std::fs::read(&c.checkpoint).unwrap();
        publish(&mut report, "ARM", false).unwrap();
        assert_eq!(durable, std::fs::read(&c.checkpoint).unwrap());
        let progress = std::fs::read(c.progress_report.as_ref().unwrap()).unwrap();
        assert!(progress.len() < 4096);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&progress).unwrap()["last_phase"],
            "ARM"
        );
        report.verdict = PowerVerdict::Fail;
        checkpoint(&mut report, "WORKER_MISMATCH").unwrap();
        assert_eq!(recover(&c.checkpoint).unwrap().verdict, PowerVerdict::Fail);
        std::fs::remove_file(c.progress_report.unwrap()).unwrap();
        std::fs::remove_file(c.checkpoint).unwrap();
    }

    #[test]
    fn schema_three_validator_rejects_forged_pass_or_recipe() {
        let mut c = config();
        c.minimum_duration_ms = 10;
        c.transitions = 1;
        let report = run(&c).unwrap();
        assert!(validate_report(&report).is_ok());
        let mut wrong = report.clone();
        wrong.total_transitions += 1;
        assert!(validate_report(&wrong).is_err());
        let mut wrong = report.clone();
        wrong.campaign.as_mut().unwrap().step_verified[0] = 0;
        assert!(validate_report(&wrong).is_err());
        let mut wrong = report.clone();
        wrong.campaign.as_mut().unwrap().steps[0].burst_ms += 1;
        assert!(validate_report(&wrong).is_err());
        std::fs::remove_file(c.checkpoint).unwrap();
    }

    #[test]
    fn unified_campaign_completes_roles_and_preserves_recovery() {
        let mut c = config();
        c.minimum_duration_ms = 10;
        c.transitions = 1;
        let report = run(&c).unwrap();
        let plan = report.campaign.as_ref().unwrap();
        assert!(plan.completed_rounds > 0);
        assert_eq!(plan.completed_in_round, 0);
        assert!(plan.elapsed_ms >= 10);
        assert_eq!(report.verdict, PowerVerdict::Pass);
        for (index, evidence) in report.transitions.iter().enumerate() {
            assert_eq!(evidence.kernel, plan.steps[index % plan.steps.len()].kernel);
            assert!(evidence.fp_lane_updates_per_worker.iter().all(|n| *n > 0));
            assert_eq!(
                evidence.worker_cpu_ids,
                plan.steps[index % plan.steps.len()].cpu_ids
            );
        }
        assert!(recover(&c.checkpoint).unwrap().campaign.is_some());
        // Historical schema 1 reports remain readable without campaign fields.
        let mut historical = serde_json::to_value(&report).unwrap();
        historical["schema_version"] = 1.into();
        historical.as_object_mut().unwrap().remove("campaign");
        historical["config"]
            .as_object_mut()
            .unwrap()
            .remove("minimum_duration_ms");
        write_durable(&c.checkpoint, &historical).unwrap();
        assert!(recover(&c.checkpoint).unwrap().campaign.is_none());
        std::fs::remove_file(c.checkpoint).unwrap();
    }

    #[test]
    fn unified_faults_and_ambiguous_config_cannot_pass() {
        for fault in [
            PowerFault::Mismatch,
            PowerFault::ZeroWork,
            PowerFault::MissingParticipant,
            PowerFault::WorkerPanic,
            PowerFault::AffinityFailure,
            PowerFault::WorkerTimeout,
            PowerFault::HeavyMismatch,
            PowerFault::StaleGeneration,
            PowerFault::DuplicateParticipant,
        ] {
            let mut c = config();
            c.minimum_duration_ms = 10;
            assert!(
                validate(&c).is_err(),
                "count-based config cannot be silently reinterpreted"
            );
            c.transitions = 1;
            c.core_sweep = true;
            assert!(validate(&c).is_err());
            c.core_sweep = false;
            c.fault = Some(fault);
            let report = run(&c).unwrap();
            assert_ne!(report.verdict, PowerVerdict::Pass);
            if fault == PowerFault::HeavyMismatch {
                assert_eq!(report.verdict, PowerVerdict::Fail);
                assert_eq!(
                    report.transitions.last().unwrap().kernel,
                    PowerKernel::HeavyFp
                );
            }
            let counts = report.active_workers.as_ref().unwrap();
            assert_eq!(counts.requested, 1);
            assert_eq!(counts.started, 1);
            if matches!(
                fault,
                PowerFault::MissingParticipant | PowerFault::WorkerPanic
            ) {
                assert_eq!(counts.completed, 0);
            }
            assert_eq!(
                report.power_platform.status,
                PowerPlatformStatus::NoEvidence
            );
            std::fs::remove_file(c.checkpoint).unwrap();
        }
    }

    #[test]
    fn sweep_groups_follow_physical_topology_without_inventing_siblings() {
        let topology = vec![
            crate::CpuThread {
                cpu_id: 3,
                package_id: 0,
                core_id: 1,
                numa_node: None,
                allowed: true,
            },
            crate::CpuThread {
                cpu_id: 7,
                package_id: 0,
                core_id: 1,
                numa_node: None,
                allowed: true,
            },
            crate::CpuThread {
                cpu_id: 9,
                package_id: 0,
                core_id: 2,
                numa_node: None,
                allowed: true,
            },
        ];
        assert_eq!(
            worker_groups(&[3, 7, 9], &topology, true),
            vec![
                vec![3],
                vec![7],
                vec![9],
                vec![3, 7],
                vec![9],
                vec![3, 7, 9]
            ]
            .into_iter()
            .fold(Vec::<Vec<usize>>::new(), |mut out, group| {
                if !out.contains(&group) {
                    out.push(group);
                }
                out
            })
        );
        assert_eq!(
            worker_groups(&[3, 9], &[], true),
            vec![vec![3], vec![9], vec![3, 9]]
        );
    }
}
