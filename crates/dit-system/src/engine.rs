use dit_contracts::{FaultRecord, FaultWriter};
use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufWriter, Write as _},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Instant, SystemTime},
};

use dit_core::{RunStatus, VerifiedSource};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    LockPolicy, MemoryTarget, SystemDitError, SystemInventory,
    compute_oracle::{self, WorkerOutcome as ComputeOracleWorkerOutcome},
    cpu::{
        CoherencyResult, coherency_test, fp_fma_known_answer_test, integer_known_answer_test,
        pin_current_thread, scalar_reduction, select_worker_cpus, sha256_known_answer_test,
        simd_reduction,
    },
    memory::{MemoryArena, plan_and_allocate},
    report::{
        ArenaEvidence, CpuEvidence, DiagnosticEvidence, ExecutionEvidence, RoundDigest,
        SystemComputeOracleEvidence, SystemConfigEvidence, SystemMismatch, SystemProgressReport,
        SystemRunReport, SystemSourceEvidence, WindowCoverageEvidence, WorkerCrashEvidence,
        WorkerEvidence,
    },
    safety::{AbortKind, RunControl, SafetyFaultInjection, SafetyMonitor, SafetyPolicy},
    window::{
        PATTERN_RECIPE_ID, WINDOW_RECIPE_ID, WindowPlan, WindowSpec, build_window_plan,
        expected_byte, expected_word, mandatory_pattern_families, mandatory_size_classes,
        window_sizes_bytes,
    },
};

const ROUND_SEED_DOMAIN: &[u8] = b"CPU-DIT-ROUND-SEED-v1";
const ROUND_SEED_DERIVATION: &str = "sha256:CPU-DIT-ROUND-SEED-v1";
const SEED_COMMITMENT_DOMAIN: &[u8] = b"CPU-DIT-SEED-COMMITMENT-v1";
const COUNTER_PROGRESS_STRIDE: u64 = 64 * 1024 * 1024;

struct RunCounters {
    calculations_completed: AtomicU64,
    failures_total: AtomicU64,
    last_emitted_bucket: AtomicU64,
    emit_progress: bool,
}

impl RunCounters {
    fn new(emit_progress: bool) -> Self {
        Self {
            calculations_completed: AtomicU64::new(0),
            failures_total: AtomicU64::new(0),
            last_emitted_bucket: AtomicU64::new(0),
            emit_progress,
        }
    }

    fn add(&self, calculations: u64, failures: u64) {
        let completed = self
            .calculations_completed
            .fetch_add(calculations, Ordering::Relaxed)
            .saturating_add(calculations);
        let failed = self
            .failures_total
            .fetch_add(failures, Ordering::Relaxed)
            .saturating_add(failures);
        if !self.emit_progress {
            return;
        }
        let bucket = completed / COUNTER_PROGRESS_STRIDE;
        let mut previous = self.last_emitted_bucket.load(Ordering::Relaxed);
        while bucket > previous {
            match self.last_emitted_bucket.compare_exchange_weak(
                previous,
                bucket,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    eprintln!(
                        "CPU_DIT_COUNTER calculations_completed={completed} failures_total={failed}"
                    );
                    break;
                }
                Err(current) => previous = current,
            }
        }
    }

    fn snapshot(&self) -> (u64, u64) {
        (
            self.calculations_completed.load(Ordering::Relaxed),
            self.failures_total.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeedSource {
    LinuxRandom,
    OperatorReplay,
}

impl SeedSource {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LinuxRandom => "linux_random",
            Self::OperatorReplay => "operator_replay",
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // Independent run switches stay explicit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemRunConfig {
    pub test_id: String,
    pub target: MemoryTarget,
    pub safety_reserve_bytes: u64,
    pub arena_target_bytes: u64,
    pub minimum_arena_bytes: u64,
    pub lock_policy: LockPolicy,
    pub rounds: u32,
    pub minimum_duration_ms: u64,
    pub emit_progress: bool,
    /// Optional path for a best-effort JSON heartbeat, overwritten at every
    /// phase transition so a hang or crash still leaves evidence on disk.
    pub progress_report_path: Option<PathBuf>,
    pub requested_workers: usize,
    pub seed: u64,
    pub seed_source: SeedSource,
    pub coherency_iterations_per_worker: u64,
    pub fault_injection_global_offset: Option<u64>,
    pub fault_injection_simd_worker: Option<usize>,
    pub fault_injection_worker_crash: Option<usize>,
    pub safety_policy: SafetyPolicy,
    pub safety_fault_injection: SafetyFaultInjection,
    pub diagnostic_rerun: bool,
    /// Zero disables the compute-oracle (ADR-0004 Phase 2); otherwise spare
    /// logical CPUs beyond the RAM workers run it concurrently with the round loop.
    pub compute_oracle_workers: usize,
    pub compute_oracle_dense_fma_iterations: u32,
    pub compute_oracle_idle_ramp_enabled: bool,
    pub compute_oracle_idle_sleep_ms: u64,
    pub fault_injection_compute_oracle_worker: Option<usize>,
    /// Step F1 of the component fault ledger: one JSON line per failure as it is found.
    pub fault_events_path: Option<PathBuf>,
    /// Step F4: a RAM mismatch is re-read from other CPUs and rewritten to tell the verifying
    /// core, a transient read, a value held wrong and a failing cell apart.
    pub confirm_on_fail: bool,
}

impl SystemRunConfig {
    /// Validate a system-run configuration before allocation or CPU load starts.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error for empty IDs, zero rounds/arena
    /// sizes, or a fault offset outside an explicit target.
    pub fn validate(&self) -> Result<(), SystemDitError> {
        if self.test_id.is_empty() {
            return Err(SystemDitError::InvalidConfig("test_id is empty".to_owned()));
        }
        if self.rounds == 0 {
            return Err(SystemDitError::InvalidConfig(
                "rounds must be positive".to_owned(),
            ));
        }
        if self.arena_target_bytes == 0 || self.minimum_arena_bytes == 0 {
            return Err(SystemDitError::InvalidConfig(
                "arena sizes must be positive".to_owned(),
            ));
        }
        if self.coherency_iterations_per_worker == 0 {
            return Err(SystemDitError::InvalidConfig(
                "coherency iterations must be positive".to_owned(),
            ));
        }
        if self.compute_oracle_workers > 0 && self.compute_oracle_dense_fma_iterations == 0 {
            return Err(SystemDitError::InvalidConfig(
                "compute_oracle_dense_fma_iterations must be positive when workers > 0".to_owned(),
            ));
        }
        if self.minimum_duration_ms >= self.safety_policy.deadline_ms {
            return Err(SystemDitError::InvalidConfig(format!(
                "minimum duration {} ms must be lower than safety deadline {} ms",
                self.minimum_duration_ms, self.safety_policy.deadline_ms
            )));
        }
        self.safety_policy
            .validate()
            .map_err(SystemDitError::InvalidConfig)?;
        if let (MemoryTarget::Bytes(target), Some(offset)) =
            (self.target, self.fault_injection_global_offset)
            && offset >= target
        {
            return Err(SystemDitError::InvalidConfig(format!(
                "fault offset {offset} is outside explicit target {target}"
            )));
        }
        Ok(())
    }
}

struct BatchResult {
    worker_id: usize,
    cpu_id: usize,
    arenas: Vec<MemoryArena>,
    affinity_failed: bool,
    bytes: u64,
    exact_verified: u64,
    scalar_simd_comparisons: u64,
    scalar_simd_mismatches: u64,
    digest_entries: Vec<(usize, [u8; 32])>,
    mismatch: Option<SystemMismatch>,
    crash: Option<WorkerCrashEvidence>,
    execution_error: Option<String>,
}

/// Run adaptive anonymous-memory and multi-core CPU-DIT phases.
///
/// # Errors
///
/// Returns an error for invalid configuration/inventory, unsafe allocation
/// targets, mmap failure, required mlock failure, or worker panic.
#[allow(clippy::too_many_lines)] // The stage order is intentionally audit-visible in v1.
pub fn run_system(
    source: &VerifiedSource,
    config: &SystemRunConfig,
) -> Result<SystemRunReport, SystemDitError> {
    let mut primary = run_system_once(source, config)?;
    if primary.status == RunStatus::Pass {
        return Ok(primary);
    }
    if primary.status == RunStatus::Invalid {
        "skipped_incomplete_execution".clone_into(&mut primary.diagnostic.classification);
        return Ok(primary);
    }
    let primary_fingerprint = failure_fingerprint(&primary);
    if !config.diagnostic_rerun {
        "disabled".clone_into(&mut primary.diagnostic.classification);
        primary.diagnostic.primary_fingerprint = primary_fingerprint;
        return Ok(primary);
    }
    if primary
        .safety
        .stop_event
        .as_ref()
        .is_some_and(|event| event.kind == AbortKind::Temperature)
    {
        "skipped_temperature_safety_abort".clone_into(&mut primary.diagnostic.classification);
        primary.diagnostic.primary_fingerprint = primary_fingerprint;
        return Ok(primary);
    }

    // The rerun only reproduces; its failures are not new ones (no live fault records).
    let rerun_config = SystemRunConfig {
        fault_events_path: None,
        confirm_on_fail: false,
        ..config.clone()
    };
    let rerun = run_system_once(source, &rerun_config);
    primary.diagnostic = match rerun {
        Ok(rerun) => {
            let rerun_fingerprint = failure_fingerprint(&rerun);
            let reproduced = primary_fingerprint.is_some()
                && primary_fingerprint == rerun_fingerprint
                && rerun.status == RunStatus::Fail;
            let classification = if reproduced {
                "reproducible_same_route"
            } else if rerun.status == RunStatus::Pass {
                "not_reproduced_transient_or_unattributed"
            } else {
                "different_failure_unattributed"
            };
            DiagnosticEvidence {
                attempted: true,
                classification: classification.to_owned(),
                seed_hex: format!("0x{:016x}", config.seed),
                primary_fingerprint,
                rerun_status: Some(rerun.status),
                rerun_fingerprint,
                same_failure_reproduced: Some(reproduced),
                rerun_failure_reason: rerun.failure_reason,
                rerun_mismatch: rerun.mismatch,
            }
        }
        Err(error) => DiagnosticEvidence {
            attempted: true,
            classification: "rerun_invalid_unattributed".to_owned(),
            seed_hex: format!("0x{:016x}", config.seed),
            primary_fingerprint,
            rerun_status: Some(RunStatus::Invalid),
            rerun_fingerprint: None,
            same_failure_reproduced: Some(false),
            rerun_failure_reason: Some(error.to_string()),
            rerun_mismatch: None,
        },
    };
    Ok(primary)
}

#[allow(clippy::too_many_lines)] // The complete evidence gate stays audit-visible in one function.
fn run_system_once(
    source: &VerifiedSource,
    config: &SystemRunConfig,
) -> Result<SystemRunReport, SystemDitError> {
    config.validate()?;
    let started = Instant::now();
    let seed_commitment = seed_commitment(config, &source.manifest().sha256);
    let counters = Arc::new(RunCounters::new(config.emit_progress));
    emit_progress(
        config,
        &seed_commitment,
        "PREFLIGHT",
        0,
        None,
        0,
        0,
        &counters,
    );
    let inventory = SystemInventory::collect()?;
    let all_worker_cpus = select_worker_cpus(&inventory.cpu, config.requested_workers)?;
    let monitor = SafetyMonitor::start(
        config.safety_policy.clone(),
        config.safety_fault_injection.clone(),
    );
    let control = Arc::clone(&monitor.control);
    let cpu_tests_completed = !control.should_stop();
    let coherency = if cpu_tests_completed {
        coherency_test(&all_worker_cpus, config.coherency_iterations_per_worker)?
    } else {
        CoherencyResult::default()
    };
    let integer_kat = cpu_tests_completed && integer_known_answer_test();
    let sha256_kat = cpu_tests_completed && sha256_known_answer_test();
    let fp_fma_kat = cpu_tests_completed && fp_fma_known_answer_test();
    let cpu_failures = u64::from(!integer_kat)
        + u64::from(!sha256_kat)
        + u64::from(!fp_fma_kat)
        + u64::from(coherency.actual != coherency.expected);
    counters.add(4, cpu_failures);
    let faults = FaultWriter::new(config.fault_events_path.as_deref());
    let mut confirm_budget = MAX_CONFIRMATIONS;
    if cpu_tests_completed {
        for (failed, source) in [
            (!integer_kat, "cpu:integer_kat"),
            (!sha256_kat, "cpu:sha256_kat"),
            (!fp_fma_kat, "cpu:fp_fma_kat"),
            (coherency.actual != coherency.expected, "cpu:coherency"),
        ] {
            if failed {
                faults.write(&FaultRecord::new(source, 1, "CPU check failed"));
            }
        }
    }

    let allocation = plan_and_allocate(
        &inventory.memory,
        &inventory.numa_nodes,
        config.target,
        config.safety_reserve_bytes,
        config.arena_target_bytes,
        config.minimum_arena_bytes,
        config.lock_policy,
    )?;
    let mut memory_evidence = allocation.evidence;
    let mut arenas = allocation.arenas;
    let worker_count = all_worker_cpus.len().min(arenas.len());
    if worker_count == 0 {
        return Err(SystemDitError::InvalidConfig(
            "no worker can own an allocated arena".to_owned(),
        ));
    }
    if let Some(worker_id) = config.fault_injection_simd_worker
        && worker_id >= worker_count
    {
        return Err(SystemDitError::InvalidConfig(format!(
            "SIMD fault worker {worker_id} is outside active worker range 0..{worker_count}"
        )));
    }
    if let Some(worker_id) = config.fault_injection_worker_crash
        && worker_id >= worker_count
    {
        return Err(SystemDitError::InvalidConfig(format!(
            "worker crash fault {worker_id} is outside active worker range 0..{worker_count}"
        )));
    }
    let worker_cpus = all_worker_cpus[..worker_count].to_vec();
    let compute_oracle_worker_cpus: Vec<usize> = if config.compute_oracle_workers > 0 {
        inventory
            .cpu
            .allowed_cpu_ids
            .iter()
            .copied()
            .filter(|cpu_id| !worker_cpus.contains(cpu_id))
            .take(config.compute_oracle_workers)
            .collect()
    } else {
        Vec::new()
    };
    if config.compute_oracle_workers > 0 && compute_oracle_worker_cpus.is_empty() {
        return Err(SystemDitError::InvalidConfig(
            "compute_oracle_workers requested but no spare logical CPU is available beyond RAM workers"
                .to_owned(),
        ));
    }
    if let Some(worker_id) = config.fault_injection_compute_oracle_worker
        && worker_id >= compute_oracle_worker_cpus.len()
    {
        return Err(SystemDitError::InvalidConfig(format!(
            "compute-oracle fault worker {worker_id} is outside active range 0..{}",
            compute_oracle_worker_cpus.len()
        )));
    }
    let mut worker_evidence = initial_worker_evidence(&inventory, &worker_cpus);
    let source_bytes = Arc::new(source.bytes().to_vec());
    let mut round_digests = Vec::new();
    let mut rounds_completed = 0_u32;
    let mut mismatch = None;
    let mut unique_written = BTreeSet::new();
    let mut unique_verified = BTreeSet::new();
    let mut round_seeds = BTreeSet::new();
    let mut round_seeds_unique = true;
    let mut last_round_seed = None;
    let mut window_mandatory_applicable = false;
    let mut window_all_completed_rounds_met = true;
    let mut window_plans_no_gaps = true;
    let mut window_plans_no_overlaps = true;
    let mut window_tails_verified = true;
    let mut fault_applied = false;
    let mut worker_crashes = Vec::new();
    let mut worker_errors = Vec::new();
    let stress_started = Instant::now();

    emit_progress(
        config,
        &seed_commitment,
        "ALLOCATED",
        0,
        None,
        stress_started.elapsed().as_millis(),
        memory_evidence.requested_bytes,
        &counters,
    );

    // ADR-0004 Phase 2: spawn compute-oracle workers before the round loop and
    // join them only after it ends, so their sustained FMA/branch/AES bursts
    // genuinely overlap the RAM write/verify phases in wall-clock time.
    let compute_oracle_branch_reference =
        compute_oracle::branch_predictor_pass(config.seed, compute_oracle::BRANCH_PREDICTOR_LENGTH);
    let compute_oracle_dense_fma_reference = compute_oracle::dense_fma_scalar_reference(
        config.seed,
        config.compute_oracle_dense_fma_iterations,
    );
    let compute_oracle_finished = Arc::new(AtomicBool::new(false));
    let compute_oracle_handles: Vec<_> = compute_oracle_worker_cpus
        .iter()
        .enumerate()
        .map(|(worker_id, &cpu_id)| {
            let control = Arc::clone(&control);
            let finished = Arc::clone(&compute_oracle_finished);
            let seed = config.seed;
            let dense_fma_iterations = config.compute_oracle_dense_fma_iterations;
            let idle_ramp_enabled = config.compute_oracle_idle_ramp_enabled;
            let idle_sleep_ms = config.compute_oracle_idle_sleep_ms;
            let fault_worker = config.fault_injection_compute_oracle_worker;
            thread::spawn(move || {
                compute_oracle::run_worker_with_control(
                    worker_id,
                    cpu_id,
                    seed,
                    dense_fma_iterations,
                    idle_ramp_enabled,
                    idle_sleep_ms,
                    fault_worker,
                    compute_oracle_dense_fma_reference,
                    compute_oracle_branch_reference,
                    &control,
                    &finished,
                )
            })
        })
        .collect();

    loop {
        if rounds_completed >= config.rounds
            && stress_started.elapsed().as_millis() >= u128::from(config.minimum_duration_ms)
        {
            break;
        }
        if control.should_stop() {
            break;
        }
        let round = rounds_completed;
        let round_seed = derive_round_seed(config.seed, &config.test_id, round);
        if !round_seeds.insert(round_seed) {
            round_seeds_unique = false;
            break;
        }
        last_round_seed = Some(round_seed);
        let window_plan = Arc::new(build_window_plan(&arenas, round_seed));
        window_mandatory_applicable |= window_plan.mandatory_recipe_applicable;
        window_plans_no_gaps &= window_plan.no_gaps;
        window_plans_no_overlaps &= window_plan.no_overlaps;
        if !window_plan.mandatory_recipe_met || !window_plan.no_gaps || !window_plan.no_overlaps {
            window_all_completed_rounds_met = false;
            break;
        }
        emit_progress(
            config,
            &seed_commitment,
            "RAM_WRITE",
            round.saturating_add(1),
            Some(round_seed),
            stress_started.elapsed().as_millis(),
            memory_evidence.requested_bytes,
            &counters,
        );
        let write_buckets = distribute_for_write(
            arenas,
            &worker_cpus,
            &inventory,
            usize::try_from(round).unwrap_or(usize::MAX),
        );
        let write_results = run_write_phase(
            write_buckets,
            &worker_cpus,
            &source_bytes,
            round_seed,
            round,
            &window_plan,
            config.lock_policy,
            &control,
            config.fault_injection_worker_crash,
        )?;
        emit_verify_faults(&faults, &write_results, round);
        arenas = collect_phase_results(
            write_results,
            &mut worker_evidence,
            true,
            &mut unique_written,
            &mut unique_verified,
            &mut memory_evidence,
            &mut round_digests,
            &mut worker_crashes,
            &mut worker_errors,
        );

        if control.should_stop() {
            break;
        }

        if !fault_applied && let Some(global_offset) = config.fault_injection_global_offset {
            inject_memory_fault(&mut arenas, global_offset)?;
            fault_applied = true;
        }

        emit_progress(
            config,
            &seed_commitment,
            "RAM_VERIFY",
            round.saturating_add(1),
            Some(round_seed),
            stress_started.elapsed().as_millis(),
            memory_evidence.requested_bytes,
            &counters,
        );
        let read_buckets = distribute_for_verify(arenas, &worker_cpus);
        let mut read_results = run_verify_phase(
            read_buckets,
            &worker_cpus,
            &source_bytes,
            round_seed,
            round,
            &window_plan,
            config.fault_injection_simd_worker,
            &control,
            &counters,
        )?;
        if config.confirm_on_fail {
            confirm_mismatches(
                &mut read_results,
                &inventory.cpu.allowed_cpu_ids,
                &mut confirm_budget,
            );
        }
        emit_verify_faults(&faults, &read_results, round);
        let round_mismatch = read_results
            .iter()
            .find_map(|result| result.mismatch.clone());
        if mismatch.is_none() {
            mismatch.clone_from(&round_mismatch);
        }
        let round_exact_verified: u64 = read_results
            .iter()
            .map(|result| result.exact_verified)
            .sum();
        let round_fully_verified = round_mismatch.is_none()
            && !control.should_stop()
            && round_exact_verified == memory_evidence.requested_bytes;
        window_all_completed_rounds_met &= round_fully_verified && window_plan.mandatory_recipe_met;
        window_tails_verified &= !window_plan.tail_present || round_fully_verified;
        let mut round_hash = Sha256::new();
        let mut round_entries: Vec<(usize, [u8; 32])> = read_results
            .iter()
            .flat_map(|result| result.digest_entries.iter().copied())
            .collect();
        round_entries.sort_by_key(|(arena_id, _)| *arena_id);
        for (arena_id, digest) in round_entries {
            round_hash.update(u64::try_from(arena_id).unwrap_or(u64::MAX).to_le_bytes());
            round_hash.update(digest);
        }
        round_digests.push(RoundDigest {
            round,
            round_seed_hex: format!("0x{round_seed:016x}"),
            window_plan_sha256: window_plan.sha256.clone(),
            size_class_counts: window_plan.size_class_counts.clone(),
            size_class_verified_bytes: if round_fully_verified {
                window_plan.size_class_bytes.clone()
            } else {
                BTreeMap::new()
            },
            pattern_counts: window_plan.pattern_counts.clone(),
            mandatory_recipe_applicable: window_plan.mandatory_recipe_applicable,
            mandatory_recipe_met: window_plan.mandatory_recipe_met && round_fully_verified,
            no_gaps: window_plan.no_gaps,
            no_overlaps: window_plan.no_overlaps,
            tail_verified: !window_plan.tail_present || round_fully_verified,
            total_windows: window_plan.total_windows,
            sha256: hex::encode(round_hash.finalize()),
        });
        arenas = collect_phase_results(
            read_results,
            &mut worker_evidence,
            false,
            &mut unique_written,
            &mut unique_verified,
            &mut memory_evidence,
            &mut Vec::new(),
            &mut worker_crashes,
            &mut worker_errors,
        );
        if control.should_stop() {
            break;
        }
        rounds_completed += 1;
        emit_progress(
            config,
            &seed_commitment,
            "ROUND_COMPLETE",
            rounds_completed,
            Some(round_seed),
            stress_started.elapsed().as_millis(),
            memory_evidence.requested_bytes,
            &counters,
        );
    }

    // Stop compute-oracle workers now that the RAM round loop has finished
    // (normally or via abort) and join/aggregate their evidence.
    compute_oracle_finished.store(true, Ordering::Release);
    let compute_oracle_outcomes: Vec<ComputeOracleWorkerOutcome> = compute_oracle_handles
        .into_iter()
        .enumerate()
        .map(|(worker_id, handle)| {
            handle.join().unwrap_or(ComputeOracleWorkerOutcome {
                worker_id,
                cpu_id: 0,
                affinity_failed: true,
                bursts_completed: 0,
                idle_ramp_cycles: 0,
                dense_fma_final_bits: [0; 4],
                dense_fma_mismatches: 0,
                branch_predictor_final: 0,
                branch_predictor_mismatches: 0,
                aes256_mismatches: 0,
                panicked: true,
            })
        })
        .collect();
    for outcome in &compute_oracle_outcomes {
        for (mismatches, source) in [
            (outcome.dense_fma_mismatches, "cpu:oracle_fma"),
            (outcome.branch_predictor_mismatches, "cpu:oracle_branch"),
            (outcome.aes256_mismatches, "cpu:oracle_aes"),
        ] {
            if mismatches > 0 {
                faults.write(
                    &FaultRecord::new(
                        source,
                        mismatches,
                        format!(
                            "compute-oracle worker {} on CPU {}",
                            outcome.worker_id, outcome.cpu_id
                        ),
                    )
                    .evidence("cpu", outcome.cpu_id.to_string()),
                );
            }
        }
    }
    let compute_oracle_evidence = if config.compute_oracle_workers > 0 {
        let aggregate = compute_oracle::aggregate_outcomes(&compute_oracle_outcomes);
        SystemComputeOracleEvidence {
            enabled: true,
            workers_requested: config.compute_oracle_workers,
            minimum_bursts_per_worker: 1,
            coverage_met: compute_oracle::minimum_work_met(
                &compute_oracle_outcomes,
                config.compute_oracle_workers,
                config.compute_oracle_idle_ramp_enabled,
            ),
            dense_fma_iterations_per_burst: config.compute_oracle_dense_fma_iterations,
            workers_used: compute_oracle_outcomes.len(),
            worker_cpu_ids: compute_oracle_worker_cpus.clone(),
            bursts_completed: compute_oracle_outcomes
                .iter()
                .map(|outcome| outcome.bursts_completed)
                .sum(),
            idle_ramp_cycles: compute_oracle_outcomes
                .iter()
                .map(|outcome| outcome.idle_ramp_cycles)
                .sum(),
            dense_fma_cross_worker_consistent: aggregate.dense_fma_cross_worker_consistent,
            dense_fma_scalar_reference_passed: aggregate.dense_fma_scalar_reference_passed,
            branch_predictor_passed: aggregate.branch_predictor_passed,
            aes256_known_answer_passed: aggregate.aes256_known_answer_passed,
            mismatch: aggregate.mismatch,
        }
    } else {
        SystemComputeOracleEvidence::disabled()
    };

    memory_evidence.prefaulted_unique_bytes = unique_written
        .iter()
        .map(|&id| u64::try_from(arenas[id].len()).unwrap_or(u64::MAX))
        .sum();
    memory_evidence.written_unique_bytes = memory_evidence.prefaulted_unique_bytes;
    memory_evidence.read_unique_bytes = unique_verified
        .iter()
        .map(|&id| u64::try_from(arenas[id].len()).unwrap_or(u64::MAX))
        .sum();
    memory_evidence.exact_verified_unique_bytes = memory_evidence.read_unique_bytes;
    memory_evidence.locked_bytes = arenas
        .iter()
        .filter(|arena| arena.lock_succeeded)
        .map(|arena| u64::try_from(arena.len()).unwrap_or(u64::MAX))
        .sum();
    memory_evidence.coverage_target_met = memory_evidence.allocation_target_met
        && memory_evidence.exact_verified_unique_bytes == memory_evidence.requested_bytes;
    let duration_target_met =
        stress_started.elapsed().as_millis() >= u128::from(config.minimum_duration_ms);
    let minimum_rounds_met = rounds_completed >= config.rounds;
    let window_recipe_met = window_all_completed_rounds_met
        && window_plans_no_gaps
        && window_plans_no_overlaps
        && window_tails_verified;

    let scalar_simd_mismatches: u64 = worker_evidence
        .iter()
        .map(|worker| worker.scalar_simd_mismatches)
        .sum();
    let scalar_simd_passed = scalar_simd_mismatches == 0;
    let affinity_failures: u64 = worker_evidence
        .iter()
        .map(|worker| worker.affinity_failures)
        .sum::<u64>()
        + coherency.affinity_failures;
    let coherency_passed = coherency.actual == coherency.expected;
    let cpu_evidence = CpuEvidence {
        cpu_tests_completed,
        workers_requested: config.requested_workers,
        workers_used: worker_count,
        worker_cpu_ids: worker_cpus.clone(),
        cross_core_role_rotation: worker_count > 1,
        scalar_simd_differential_passed: scalar_simd_passed,
        sha256_known_answer_passed: sha256_kat,
        integer_known_answer_passed: integer_kat,
        fp_fma_known_answer_passed: fp_fma_kat,
        coherency_workers: coherency.workers,
        coherency_iterations_per_worker: coherency.iterations_per_worker,
        coherency_expected: coherency.expected,
        coherency_actual: coherency.actual,
        coherency_passed,
    };

    let required_lock_failed = config.lock_policy == LockPolicy::Required
        && arenas.iter().any(|arena| !arena.lock_succeeded);
    let (telemetry, safety) = monitor.finish();
    let execution = ExecutionEvidence {
        aborted: control.should_stop(),
        abort_event: control.event(),
        worker_crashes,
        worker_errors,
    };
    let base_passed = memory_evidence.coverage_target_met
        && minimum_rounds_met
        && duration_target_met
        && round_seeds_unique
        && window_recipe_met
        && mismatch.is_none()
        && scalar_simd_passed
        && integer_kat
        && sha256_kat
        && fp_fma_kat
        && coherency_passed
        && affinity_failures == 0
        && !required_lock_failed
        && !safety.stop_requested
        && safety.violations.is_empty()
        && execution.worker_crashes.is_empty()
        && execution.worker_errors.is_empty()
        && compute_oracle_evidence.mismatch.is_none();
    let passed = base_passed && compute_oracle_evidence.coverage_met;
    let failure_reason = (!passed).then(|| {
        if let Some(event) = &execution.abort_event {
            format!("execution aborted: {}", event.reason)
        } else if let Some(crash) = execution.worker_crashes.first() {
            format!(
                "worker {} crashed during {}: {}",
                crash.worker_id, crash.phase, crash.message
            )
        } else if let Some(error) = execution.worker_errors.first() {
            format!("worker error: {error}")
        } else if let Some(oracle_mismatch) = &compute_oracle_evidence.mismatch {
            format!(
                "compute-oracle {} mismatch: {}",
                oracle_mismatch.kernel, oracle_mismatch.detail
            )
        } else if mismatch.is_some() {
            "exact memory mismatch".to_owned()
        } else if base_passed && !compute_oracle_evidence.coverage_met {
            "compute-oracle minimum work or worker coverage not met".to_owned()
        } else if !memory_evidence.coverage_target_met {
            "memory coverage target not met".to_owned()
        } else if !round_seeds_unique {
            "round seed derivation produced a duplicate".to_owned()
        } else if !window_recipe_met {
            "window/pattern recipe coverage gate failed".to_owned()
        } else if !minimum_rounds_met {
            "minimum round target not met".to_owned()
        } else if !duration_target_met {
            "minimum duration target not met".to_owned()
        } else if affinity_failures != 0 {
            format!("{affinity_failures} CPU affinity operations failed")
        } else if !scalar_simd_passed {
            "scalar/SIMD differential mismatch".to_owned()
        } else if !integer_kat || !sha256_kat || !fp_fma_kat || !coherency_passed {
            "CPU known-answer or coherency test failed".to_owned()
        } else {
            "required memory lock failed".to_owned()
        }
    });
    let limitations = build_limitations(&inventory, config.lock_policy, &arenas);
    let arena_evidence = arenas.iter().map(arena_evidence).collect();

    emit_progress(
        config,
        &seed_commitment,
        "FINALIZING",
        rounds_completed,
        last_round_seed,
        stress_started.elapsed().as_millis(),
        memory_evidence.requested_bytes,
        &counters,
    );
    let (calculations_completed, failures_total) = counters.snapshot();
    let availability = crate::availability::AvailabilityEvidence::new(
        if !execution.worker_crashes.is_empty() || !execution.worker_errors.is_empty() {
            crate::availability::AvailabilityStatus::WorkerFailed
        } else if execution.aborted
            || !minimum_rounds_met
            || !duration_target_met
            || !compute_oracle_evidence.coverage_met
        {
            crate::availability::AvailabilityStatus::Incomplete
        } else {
            crate::availability::AvailabilityStatus::Completed
        },
        "execution completion is independent of the integrity verdict",
    );
    let power_platform = crate::power_platform::PowerPlatformEvidence::from_telemetry(&telemetry);
    Ok(SystemRunReport {
        schema_version: 10,
        availability,
        power_platform,
        test_id: config.test_id.clone(),
        status: if passed {
            RunStatus::Pass
        } else if base_passed && !compute_oracle_evidence.coverage_met {
            RunStatus::Invalid
        } else {
            RunStatus::Fail
        },
        source: SystemSourceEvidence {
            source_id: source.manifest().source_id.clone(),
            size_bytes: source.manifest().size_bytes,
            sha256: source.manifest().sha256.to_ascii_lowercase(),
        },
        config: config_evidence(config, &seed_commitment),
        inventory,
        memory: memory_evidence,
        cpu: cpu_evidence,
        compute_oracle: compute_oracle_evidence,
        arenas: arena_evidence,
        workers: worker_evidence,
        round_digests,
        window_coverage: WindowCoverageEvidence {
            window_recipe_id: WINDOW_RECIPE_ID.to_owned(),
            pattern_recipe_id: PATTERN_RECIPE_ID.to_owned(),
            mandatory_size_classes: mandatory_size_classes(),
            mandatory_pattern_families: mandatory_pattern_families(),
            mandatory_recipe_applicable: window_mandatory_applicable,
            all_completed_rounds_met: window_all_completed_rounds_met,
            plans_no_gaps: window_plans_no_gaps,
            plans_no_overlaps: window_plans_no_overlaps,
            tails_verified: window_tails_verified,
        },
        rounds_requested: config.rounds,
        rounds_completed,
        round_seeds_unique,
        duration_target_ms: config.minimum_duration_ms,
        duration_target_met,
        elapsed_ms: started.elapsed().as_millis(),
        calculations_completed,
        failures_total,
        telemetry,
        safety,
        execution,
        diagnostic: DiagnosticEvidence::not_required(config.seed),
        fault_injection_global_offset: config.fault_injection_global_offset,
        fault_injection_simd_worker: config.fault_injection_simd_worker,
        fault_injection_worker_crash: config.fault_injection_worker_crash,
        mismatch,
        failure_reason,
        limitations,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_write_phase(
    buckets: Vec<Vec<MemoryArena>>,
    worker_cpus: &[usize],
    source: &Arc<Vec<u8>>,
    seed: u64,
    round: u32,
    window_plan: &Arc<WindowPlan>,
    lock_policy: LockPolicy,
    control: &Arc<RunControl>,
    fault_injection_worker_crash: Option<usize>,
) -> Result<Vec<BatchResult>, SystemDitError> {
    let mut handles = Vec::new();
    for (worker_id, arenas) in buckets.into_iter().enumerate() {
        if arenas.is_empty() {
            continue;
        }
        let cpu_id = worker_cpus[worker_id];
        let source = Arc::clone(source);
        let control = Arc::clone(control);
        let window_plan = Arc::clone(window_plan);
        handles.push((
            worker_id,
            thread::spawn(move || {
                Ok(write_worker(
                    worker_id,
                    cpu_id,
                    arenas,
                    &source,
                    seed,
                    round,
                    &window_plan,
                    lock_policy,
                    &control,
                    fault_injection_worker_crash,
                ))
            }),
        ));
    }
    join_workers(handles)
}

#[allow(clippy::too_many_arguments)]
fn write_worker(
    worker_id: usize,
    cpu_id: usize,
    mut arenas: Vec<MemoryArena>,
    source: &[u8],
    seed: u64,
    round: u32,
    window_plan: &WindowPlan,
    lock_policy: LockPolicy,
    control: &RunControl,
    fault_injection_worker_crash: Option<usize>,
) -> BatchResult {
    let affinity_failed = pin_current_thread(cpu_id).is_err();
    let mut bytes = 0_u64;
    let mut execution_error = None;
    if fault_injection_worker_crash == Some(worker_id) {
        let message = "synthetic worker crash fault injection".to_owned();
        control.cancel(
            AbortKind::WorkerCrash,
            format!("worker {worker_id} crashed during RAM_WRITE: {message}"),
        );
        return BatchResult {
            worker_id,
            cpu_id,
            arenas,
            affinity_failed,
            bytes: 0,
            exact_verified: 0,
            scalar_simd_comparisons: 0,
            scalar_simd_mismatches: 0,
            digest_entries: Vec::new(),
            mismatch: None,
            crash: Some(WorkerCrashEvidence {
                phase: "RAM_WRITE".to_owned(),
                round,
                worker_id,
                cpu_id,
                message,
                injected: true,
            }),
            execution_error: None,
        };
    }
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        for arena in &mut arenas {
            if control.should_stop()
                || !fill_arena(
                    arena,
                    &window_plan.by_arena[arena.id],
                    source,
                    seed,
                    control,
                )
            {
                break;
            }
            arena.first_touch_cpu.get_or_insert(cpu_id);
            arena.last_writer_cpu = Some(cpu_id);
            arena.rounds_written += 1;
            bytes += u64::try_from(arena.len()).unwrap_or(u64::MAX);
            if round == 0 && lock_policy != LockPolicy::Off {
                match arena.map.lock() {
                    Ok(()) => arena.lock_succeeded = true,
                    Err(error) => {
                        arena.lock_error = Some(error.to_string());
                        if lock_policy == LockPolicy::Required {
                            return Err(format!("required mlock failed: {error}"));
                        }
                    }
                }
            }
        }
        Ok(())
    }));
    let crash = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(error)) => {
            control.cancel(AbortKind::WorkerError, error.clone());
            execution_error = Some(error);
            None
        }
        Err(payload) => {
            let message = panic_message(&payload);
            control.cancel(
                AbortKind::WorkerCrash,
                format!("worker {worker_id} crashed during RAM_WRITE: {message}"),
            );
            Some(WorkerCrashEvidence {
                phase: "RAM_WRITE".to_owned(),
                round,
                worker_id,
                cpu_id,
                message,
                injected: false,
            })
        }
    };
    BatchResult {
        worker_id,
        cpu_id,
        arenas,
        affinity_failed,
        bytes,
        exact_verified: 0,
        scalar_simd_comparisons: 0,
        scalar_simd_mismatches: 0,
        digest_entries: Vec::new(),
        mismatch: None,
        crash,
        execution_error,
    }
}

/// RAM mismatches confirmed per run, at most (each one pins and evicts; a storm stays cheap).
const MAX_CONFIRMATIONS: usize = 8;
/// Bytes of other arenas read between steps so the next read of the byte comes from DRAM.
const EVICT_BYTES: usize = 128 << 20;

/// Step F4: confirms the first mismatch of each failing worker of a verify phase.
fn confirm_mismatches(results: &mut [BatchResult], allowed: &[usize], budget: &mut usize) {
    for index in 0..results.len() {
        let Some(mismatch) = results[index].mismatch.clone() else {
            continue;
        };
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let outcome = confirm_one(results, &mismatch, allowed);
        if let Some(found) = results[index].mismatch.as_mut() {
            found.confirm = outcome.map(str::to_owned);
        }
    }
}

fn locate_arena(results: &[BatchResult], arena_id: usize) -> Option<(usize, usize)> {
    results.iter().enumerate().find_map(|(r, result)| {
        result
            .arenas
            .iter()
            .position(|arena| arena.id == arena_id)
            .map(|a| (r, a))
    })
}

/// Reads up to [`EVICT_BYTES`] of the other arenas (one byte per cache line).
fn evict_caches(results: &[BatchResult], skip: (usize, usize)) {
    let mut left = EVICT_BYTES;
    let mut sum = 0_u64;
    for (r, result) in results.iter().enumerate() {
        for (a, arena) in result.arenas.iter().enumerate() {
            if (r, a) == skip || left == 0 {
                continue;
            }
            let take = arena.map.len().min(left);
            for index in (0..take).step_by(64) {
                sum = sum.wrapping_add(u64::from(arena.map[index]));
            }
            left -= take;
        }
    }
    std::hint::black_box(sum);
}

/// Re-reads the byte from another CPU after evicting the caches; right there, reads it again
/// from the verifying CPU (`verifier_core` when still wrong, else `transient`); wrong there too,
/// rewrites it and reads it back (`rewrite_fixed` or `cell_persistent`). The thread's CPU set is
/// restored afterwards; the next round rewrites every arena anyway.
fn confirm_one(
    results: &mut [BatchResult],
    mismatch: &SystemMismatch,
    allowed: &[usize],
) -> Option<&'static str> {
    let (r, a) = locate_arena(results, mismatch.arena_id)?;
    let offset = usize::try_from(mismatch.arena_offset).ok()?;
    let verifier = mismatch.verifier_cpu?;
    let other = allowed
        .iter()
        .copied()
        .find(|cpu| *cpu != verifier && Some(*cpu) != mismatch.writer_cpu)
        .or_else(|| allowed.iter().copied().find(|cpu| *cpu != verifier))?;
    let original = nix::sched::sched_getaffinity(nix::unistd::Pid::from_raw(0)).ok();
    let read = |results: &[BatchResult]| std::hint::black_box(results[r].arenas[a].map[offset]);
    let outcome = (|| {
        pin_current_thread(other).ok()?;
        evict_caches(results, (r, a));
        if read(results) == mismatch.expected {
            pin_current_thread(verifier).ok()?;
            evict_caches(results, (r, a));
            return Some(if read(results) == mismatch.expected {
                "transient"
            } else {
                "verifier_core"
            });
        }
        results[r].arenas[a].map[offset] = mismatch.expected;
        evict_caches(results, (r, a));
        Some(if read(results) == mismatch.expected {
            "rewrite_fixed"
        } else {
            "cell_persistent"
        })
    })();
    if let Some(set) = original {
        let _ = nix::sched::sched_setaffinity(nix::unistd::Pid::from_raw(0), &set);
    }
    outcome
}

/// Live fault records of one phase's worker results (step F1): the first RAM mismatch of each
/// failing worker, SIMD/scalar disagreements and worker crashes.
fn emit_verify_faults(faults: &FaultWriter, results: &[BatchResult], round: u32) {
    if !faults.enabled() {
        return;
    }
    for result in results {
        if let Some(m) = &result.mismatch {
            faults.write(
                &FaultRecord::new(
                    "ram:pattern",
                    1,
                    format!(
                        "round {round} worker {}: global offset {} (arena {} offset {}), expected {:#04x} actual {:#04x}, syndrome {:#04x}",
                        result.worker_id,
                        m.global_offset,
                        m.arena_id,
                        m.arena_offset,
                        m.expected,
                        m.actual,
                        m.xor_syndrome
                    ),
                )
                .evidence("global_offset", m.global_offset.to_string())
                .evidence("syndrome", m.xor_syndrome.to_string())
                .evidence("confirm", m.confirm.clone().unwrap_or_default())
                .evidence("verifier_cpu", m.verifier_cpu.map_or_else(String::new, |c| c.to_string()))
                .evidence("writer_cpu", m.writer_cpu.map_or_else(String::new, |c| c.to_string())),
            );
        }
        if result.scalar_simd_mismatches > 0 {
            faults.write(
                &FaultRecord::new(
                    "cpu:scalar_simd",
                    result.scalar_simd_mismatches,
                    format!(
                        "round {round}: SIMD and scalar reductions differ on CPU {}",
                        result.cpu_id
                    ),
                )
                .evidence("cpu", result.cpu_id.to_string()),
            );
        }
        if let Some(crash) = &result.crash {
            faults.write(&FaultRecord::new(
                "platform:worker_crash",
                1,
                format!(
                    "worker {} on CPU {} in {}: {}",
                    crash.worker_id, crash.cpu_id, crash.phase, crash.message
                ),
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)] // Explicit worker inputs keep write/verify orchestration auditable.
fn run_verify_phase(
    buckets: Vec<Vec<MemoryArena>>,
    worker_cpus: &[usize],
    source: &Arc<Vec<u8>>,
    seed: u64,
    round: u32,
    window_plan: &Arc<WindowPlan>,
    fault_injection_simd_worker: Option<usize>,
    control: &Arc<RunControl>,
    counters: &Arc<RunCounters>,
) -> Result<Vec<BatchResult>, SystemDitError> {
    let mut handles = Vec::new();
    for (worker_id, arenas) in buckets.into_iter().enumerate() {
        if arenas.is_empty() {
            continue;
        }
        let cpu_id = worker_cpus[worker_id];
        let source = Arc::clone(source);
        let control = Arc::clone(control);
        let counters = Arc::clone(counters);
        let window_plan = Arc::clone(window_plan);
        handles.push((
            worker_id,
            thread::spawn(move || {
                Ok(verify_worker(
                    worker_id,
                    cpu_id,
                    arenas,
                    &source,
                    seed,
                    round,
                    &window_plan,
                    fault_injection_simd_worker,
                    &control,
                    &counters,
                ))
            }),
        ));
    }
    join_workers(handles)
}

#[allow(clippy::too_many_arguments)]
fn verify_worker(
    worker_id: usize,
    cpu_id: usize,
    mut arenas: Vec<MemoryArena>,
    source: &[u8],
    seed: u64,
    round: u32,
    window_plan: &WindowPlan,
    fault_injection_simd_worker: Option<usize>,
    control: &RunControl,
    counters: &RunCounters,
) -> BatchResult {
    let affinity_failed = pin_current_thread(cpu_id).is_err();
    let mut bytes = 0_u64;
    let mut exact_verified = 0_u64;
    let mut comparisons = 0_u64;
    let mut simd_mismatches = 0_u64;
    let mut digest_entries = Vec::new();
    let mut first_mismatch = None;
    let mut simd_fault_injected = false;
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        for arena in &mut arenas {
            if control.should_stop() {
                break;
            }
            arena.verify_cpu = Some(cpu_id);
            let (verified, mismatch, cancelled) = verify_arena(
                arena,
                &window_plan.by_arena[arena.id],
                source,
                seed,
                round,
                cpu_id,
                control,
                counters,
            );
            let arena_matched = mismatch.is_none() && !cancelled;
            exact_verified += verified;
            if first_mismatch.is_none() {
                first_mismatch = mismatch;
            }
            if cancelled {
                break;
            }
            bytes += u64::try_from(arena.len()).unwrap_or(u64::MAX);
            let scalar = scalar_reduction(&arena.map);
            let mut simd = simd_reduction(&arena.map);
            if round == 0 && fault_injection_simd_worker == Some(worker_id) && !simd_fault_injected
            {
                simd[0] ^= 1;
                simd_fault_injected = true;
            }
            comparisons += 1;
            if scalar != simd {
                simd_mismatches += 1;
            }
            counters.add(1, u64::from(scalar != simd));
            let digest: [u8; 32] = Sha256::digest(&arena.map).into();
            digest_entries.push((arena.id, digest));
            if arena_matched {
                arena.rounds_verified += 1;
            }
        }
    }));
    let crash = outcome.err().map(|payload| {
        let message = panic_message(&payload);
        control.cancel(
            AbortKind::WorkerCrash,
            format!("worker {worker_id} crashed during RAM_VERIFY: {message}"),
        );
        WorkerCrashEvidence {
            phase: "RAM_VERIFY".to_owned(),
            round,
            worker_id,
            cpu_id,
            message,
            injected: false,
        }
    });
    BatchResult {
        worker_id,
        cpu_id,
        arenas,
        affinity_failed,
        bytes,
        exact_verified,
        scalar_simd_comparisons: comparisons,
        scalar_simd_mismatches: simd_mismatches,
        digest_entries,
        mismatch: first_mismatch,
        crash,
        execution_error: None,
    }
}

fn join_workers(
    handles: Vec<(usize, thread::JoinHandle<Result<BatchResult, String>>)>,
) -> Result<Vec<BatchResult>, SystemDitError> {
    let mut results = Vec::new();
    for (worker_id, handle) in handles {
        let result = handle
            .join()
            .map_err(|_| SystemDitError::WorkerPanic { worker_id })?
            .map_err(|reason| SystemDitError::Worker { worker_id, reason })?;
        results.push(result);
    }
    Ok(results)
}

fn fill_arena(
    arena: &mut MemoryArena,
    windows: &[WindowSpec],
    source: &[u8],
    seed: u64,
    control: &RunControl,
) -> bool {
    let mut processed_words = 0_usize;
    for window in windows {
        let end = window.arena_offset + window.length;
        let (chunks, remainder) = arena.map[window.arena_offset..end].as_chunks_mut::<8>();
        for (word_index, chunk) in chunks.iter_mut().enumerate() {
            if processed_words.is_multiple_of(131_072) && control.should_stop() {
                return false;
            }
            let offset = window.global_offset + u64::try_from(word_index * 8).unwrap_or(u64::MAX);
            chunk.copy_from_slice(&expected_word(source, seed, window, offset));
            processed_words += 1;
        }
        if !remainder.is_empty() {
            if control.should_stop() {
                return false;
            }
            let base = window.global_offset
                + u64::try_from(window.length - remainder.len()).unwrap_or(u64::MAX);
            for (index, byte) in remainder.iter_mut().enumerate() {
                *byte = expected_byte(
                    source,
                    seed,
                    window,
                    base + u64::try_from(index).unwrap_or(u64::MAX),
                );
            }
        }
    }
    true
}

#[allow(clippy::too_many_arguments)] // Explicit verify inputs keep the byte-exact oracle auditable.
fn verify_arena(
    arena: &MemoryArena,
    windows: &[WindowSpec],
    source: &[u8],
    seed: u64,
    round: u32,
    verifier_cpu: usize,
    control: &RunControl,
    counters: &RunCounters,
) -> (u64, Option<SystemMismatch>, bool) {
    let mut verified = 0_u64;
    let mut processed_words = 0_usize;
    let mut pending_calculations = 0_u64;
    let mut pending_failures = 0_u64;
    let mut first_mismatch = None;
    for window in windows {
        let end = window.arena_offset + window.length;
        let (chunks, remainder) = arena.map[window.arena_offset..end].as_chunks::<8>();
        for (word_index, chunk) in chunks.iter().enumerate() {
            let window_offset = word_index * 8;
            let arena_offset = window.arena_offset + window_offset;
            let global_offset =
                window.global_offset + u64::try_from(window_offset).unwrap_or(u64::MAX);
            let expected = expected_word(source, seed, window, global_offset);
            let word_failures = chunk
                .iter()
                .zip(expected.iter())
                .filter(|(actual, expected)| actual != expected)
                .count();
            if word_failures > 0 && first_mismatch.is_none() {
                let lane = chunk
                    .iter()
                    .zip(expected.iter())
                    .position(|(actual, expected)| actual != expected)
                    .unwrap_or(0);
                let actual = chunk[lane];
                let expected = expected[lane];
                first_mismatch = Some(window_mismatch(
                    arena,
                    window,
                    round,
                    arena_offset + lane,
                    global_offset + u64::try_from(lane).unwrap_or(u64::MAX),
                    expected,
                    actual,
                    verifier_cpu,
                ));
            }
            let word_failures = u64::try_from(word_failures).unwrap_or(8);
            verified += 8_u64.saturating_sub(word_failures);
            pending_calculations += 8;
            pending_failures += word_failures;
            processed_words += 1;
            if processed_words.is_multiple_of(131_072) {
                counters.add(pending_calculations, pending_failures);
                pending_calculations = 0;
                pending_failures = 0;
                if control.should_stop() {
                    return (verified, first_mismatch, true);
                }
            }
        }
        let base_offset = window.length - remainder.len();
        for (index, &actual) in remainder.iter().enumerate() {
            if control.should_stop() {
                counters.add(pending_calculations, pending_failures);
                return (verified, first_mismatch, true);
            }
            let arena_offset = window.arena_offset + base_offset + index;
            let global_offset =
                window.global_offset + u64::try_from(base_offset + index).unwrap_or(u64::MAX);
            let expected = expected_byte(source, seed, window, global_offset);
            if actual == expected {
                verified += 1;
            } else {
                pending_failures += 1;
                if first_mismatch.is_none() {
                    first_mismatch = Some(window_mismatch(
                        arena,
                        window,
                        round,
                        arena_offset,
                        global_offset,
                        expected,
                        actual,
                        verifier_cpu,
                    ));
                }
            }
            pending_calculations += 1;
        }
    }
    counters.add(pending_calculations, pending_failures);
    (verified, first_mismatch, false)
}

#[allow(clippy::too_many_arguments)]
fn window_mismatch(
    arena: &MemoryArena,
    window: &WindowSpec,
    round: u32,
    arena_offset: usize,
    global_offset: u64,
    expected: u8,
    actual: u8,
    verifier_cpu: usize,
) -> SystemMismatch {
    SystemMismatch {
        stage: "RAM_EXACT_VERIFY".to_owned(),
        round,
        window_id: window.id,
        window_class: window.class_id.to_owned(),
        pattern_id: window.pattern.as_str().to_owned(),
        arena_id: arena.id,
        arena_offset: u64::try_from(arena_offset).unwrap_or(u64::MAX),
        global_offset,
        expected,
        actual,
        xor_syndrome: expected ^ actual,
        writer_cpu: arena.last_writer_cpu,
        verifier_cpu: Some(verifier_cpu),
        confirm: None,
    }
}

fn distribute_for_write(
    arenas: Vec<MemoryArena>,
    worker_cpus: &[usize],
    inventory: &SystemInventory,
    round: usize,
) -> Vec<Vec<MemoryArena>> {
    let mut buckets: Vec<Vec<MemoryArena>> = (0..worker_cpus.len()).map(|_| Vec::new()).collect();
    for arena in arenas {
        let candidates: Vec<usize> = worker_cpus
            .iter()
            .enumerate()
            .filter(|(_, cpu)| {
                arena.target_numa_node.is_none()
                    || cpu_node(inventory, **cpu) == arena.target_numa_node
            })
            .map(|(worker, _)| worker)
            .collect();
        let worker = if candidates.is_empty() {
            (arena.id + round) % worker_cpus.len()
        } else {
            candidates[(arena.id + round) % candidates.len()]
        };
        buckets[worker].push(arena);
    }
    buckets
}

fn distribute_for_verify(arenas: Vec<MemoryArena>, worker_cpus: &[usize]) -> Vec<Vec<MemoryArena>> {
    let mut buckets: Vec<Vec<MemoryArena>> = (0..worker_cpus.len()).map(|_| Vec::new()).collect();
    for arena in arenas {
        let writer_index = arena
            .last_writer_cpu
            .and_then(|cpu| worker_cpus.iter().position(|&candidate| candidate == cpu))
            .unwrap_or(arena.id % worker_cpus.len());
        let reader_index = if worker_cpus.len() > 1 {
            (writer_index + 1) % worker_cpus.len()
        } else {
            writer_index
        };
        buckets[reader_index].push(arena);
    }
    buckets
}

#[allow(clippy::too_many_arguments)]
fn collect_phase_results(
    results: Vec<BatchResult>,
    workers: &mut [WorkerEvidence],
    write_phase: bool,
    unique_written: &mut BTreeSet<usize>,
    unique_verified: &mut BTreeSet<usize>,
    memory: &mut crate::report::MemoryPlanEvidence,
    _round_digests: &mut Vec<RoundDigest>,
    worker_crashes: &mut Vec<WorkerCrashEvidence>,
    worker_errors: &mut Vec<String>,
) -> Vec<MemoryArena> {
    let mut arenas = Vec::new();
    for result in results {
        let worker = &mut workers[result.worker_id];
        worker.affinity_attempts += 1;
        worker.affinity_failures += u64::from(result.affinity_failed);
        if write_phase {
            worker.bytes_written += result.bytes;
            memory.total_write_bytes += result.bytes;
            unique_written.extend(
                result
                    .arenas
                    .iter()
                    .filter(|arena| arena.rounds_written > 0)
                    .map(|arena| arena.id),
            );
        } else {
            worker.bytes_read += result.bytes;
            worker.exact_verified_bytes += result.exact_verified;
            worker.scalar_simd_comparisons += result.scalar_simd_comparisons;
            worker.scalar_simd_mismatches += result.scalar_simd_mismatches;
            memory.total_read_bytes += result.bytes;
            unique_verified.extend(
                result
                    .arenas
                    .iter()
                    .filter(|arena| arena.rounds_verified > 0)
                    .map(|arena| arena.id),
            );
            let mut digest = Sha256::new();
            for (_, entry) in &result.digest_entries {
                digest.update(entry);
            }
            worker.sha256_digest = hex::encode(digest.finalize());
        }
        if let Some(crash) = result.crash {
            worker_crashes.push(crash);
        }
        if let Some(error) = result.execution_error {
            worker_errors.push(error);
        }
        debug_assert_eq!(worker.cpu_id, result.cpu_id);
        arenas.extend(result.arenas);
    }
    arenas.sort_by_key(|arena| arena.id);
    arenas
}

fn initial_worker_evidence(
    inventory: &SystemInventory,
    worker_cpus: &[usize],
) -> Vec<WorkerEvidence> {
    worker_cpus
        .iter()
        .enumerate()
        .map(|(worker_id, &cpu_id)| WorkerEvidence {
            worker_id,
            cpu_id,
            numa_node: cpu_node(inventory, cpu_id),
            ..WorkerEvidence::default()
        })
        .collect()
}

fn cpu_node(inventory: &SystemInventory, cpu_id: usize) -> Option<u32> {
    inventory
        .cpu
        .threads
        .iter()
        .find(|thread| thread.cpu_id == cpu_id)
        .and_then(|thread| thread.numa_node)
}

fn inject_memory_fault(
    arenas: &mut [MemoryArena],
    global_offset: u64,
) -> Result<(), SystemDitError> {
    let arena = arenas
        .iter_mut()
        .find(|arena| {
            let end = arena
                .global_offset
                .saturating_add(u64::try_from(arena.len()).unwrap_or(u64::MAX));
            global_offset >= arena.global_offset && global_offset < end
        })
        .ok_or_else(|| {
            SystemDitError::InvalidConfig(format!(
                "fault offset {global_offset} is outside allocated arenas"
            ))
        })?;
    let offset = usize::try_from(global_offset - arena.global_offset)
        .map_err(|_| SystemDitError::InvalidConfig("fault offset overflow".to_owned()))?;
    arena.map[offset] ^= 1;
    Ok(())
}

fn arena_evidence(arena: &MemoryArena) -> ArenaEvidence {
    ArenaEvidence {
        arena_id: arena.id,
        global_offset: arena.global_offset,
        length_bytes: u64::try_from(arena.len()).unwrap_or(u64::MAX),
        target_numa_node: arena.target_numa_node,
        first_touch_cpu: arena.first_touch_cpu,
        verify_cpu: arena.verify_cpu,
        lock_succeeded: arena.lock_succeeded,
        lock_error: arena.lock_error.clone(),
        rounds_written: arena.rounds_written,
        rounds_verified: arena.rounds_verified,
    }
}

fn build_limitations(
    inventory: &SystemInventory,
    lock_policy: LockPolicy,
    arenas: &[MemoryArena],
) -> Vec<String> {
    let mut limitations = vec![
        "coverage is anonymous allocatable virtual memory, not 100% physical RAM".to_owned(),
        "NUMA placement uses first-touch inference; physical PFN/node placement is not proven"
            .to_owned(),
    ];
    if lock_policy == LockPolicy::Off {
        limitations.push("memory was not mlock-protected from swap".to_owned());
    } else if arenas.iter().any(|arena| !arena.lock_succeeded) {
        limitations.push("one or more arenas could not be mlock-protected".to_owned());
    }
    if inventory.numa_nodes.len() <= 1 {
        limitations.push("cross-NUMA route was not applicable on this device".to_owned());
    }
    limitations
}

fn config_evidence(config: &SystemRunConfig, seed_commitment: &str) -> SystemConfigEvidence {
    let target = match config.target {
        MemoryTarget::Bytes(bytes) => format!("bytes:{bytes}"),
        MemoryTarget::PercentAvailable(percent) => format!("percent_available:{percent}"),
        MemoryTarget::PercentSafe(percent) => format!("percent_safe:{percent}"),
    };
    SystemConfigEvidence {
        seed_hex: format!("0x{:016x}", config.seed),
        seed_source: config.seed_source.as_str().to_owned(),
        seed_derivation: ROUND_SEED_DERIVATION.to_owned(),
        seed_commitment_sha256: seed_commitment.to_owned(),
        window_recipe_id: WINDOW_RECIPE_ID.to_owned(),
        window_sizes_bytes: window_sizes_bytes(),
        pattern_recipe_id: PATTERN_RECIPE_ID.to_owned(),
        pattern_families: mandatory_pattern_families(),
        target,
        safety_reserve_bytes: config.safety_reserve_bytes,
        arena_target_bytes: config.arena_target_bytes,
        minimum_arena_bytes: config.minimum_arena_bytes,
        rounds: config.rounds,
        minimum_duration_ms: config.minimum_duration_ms,
        requested_workers: config.requested_workers,
        lock_policy: config.lock_policy.as_str().to_owned(),
        coherency_iterations_per_worker: config.coherency_iterations_per_worker,
    }
}

#[allow(clippy::too_many_arguments)] // Explicit progress fields keep the live UI event auditable.
fn emit_progress(
    config: &SystemRunConfig,
    seed_commitment: &str,
    phase: &str,
    round: u32,
    round_seed: Option<u64>,
    elapsed_ms: u128,
    target_bytes: u64,
    counters: &RunCounters,
) {
    if !config.emit_progress && config.progress_report_path.is_none() {
        return;
    }
    let (calculations_completed, failures_total) = counters.snapshot();
    let temperature_limit = if config.safety_policy.temperature_limit_enabled {
        "on"
    } else {
        "off"
    };
    if config.emit_progress {
        let round_seed_text =
            round_seed.map_or_else(|| "N/A".to_owned(), |seed| format!("0x{seed:016x}"));
        eprintln!(
            "CPU_DIT_PROGRESS phase={phase} round={round} minimum_rounds={} elapsed_ms={elapsed_ms} duration_target_ms={} target_bytes={target_bytes} calculations_completed={calculations_completed} failures_total={failures_total} session_seed=0x{:016x} round_seed={round_seed_text} seed_source={} seed_commitment={seed_commitment} window_recipe={WINDOW_RECIPE_ID} pattern_recipe={PATTERN_RECIPE_ID} active_window=MIXED_PARALLEL active_pattern=MULTI_FAMILY temperature_limit={temperature_limit}",
            config.rounds,
            config.minimum_duration_ms,
            config.seed,
            config.seed_source.as_str(),
        );
    }
    if let Some(path) = &config.progress_report_path {
        write_progress_report(
            path,
            &SystemProgressReport {
                test_id: config.test_id.clone(),
                status: "running",
                phase: phase.to_owned(),
                round,
                rounds_requested: config.rounds,
                minimum_duration_ms: config.minimum_duration_ms,
                elapsed_ms,
                target_bytes,
                calculations_completed,
                failures_total,
                session_seed_hex: format!("0x{:016x}", config.seed),
                seed_source: config.seed_source.as_str().to_owned(),
                round_seed_hex: round_seed.map(|seed| format!("0x{seed:016x}")),
                seed_commitment_sha256: seed_commitment.to_owned(),
                temperature_limit_enabled: config.safety_policy.temperature_limit_enabled,
                updated_unix_ms: unix_ms(),
            },
        );
    }
}

/// Writes the heartbeat via a same-directory temp file + rename so a reader
/// never observes a half-written file; all I/O errors are swallowed on
/// purpose because a failed heartbeat write must never abort the real test.
fn write_progress_report(path: &Path, report: &SystemProgressReport) {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let tmp_path = path.with_extension("json.tmp");
    let Ok(file) = fs::File::create(&tmp_path) else {
        return;
    };
    let mut writer = BufWriter::new(file);
    if serde_json::to_writer_pretty(&mut writer, report).is_err() {
        return;
    }
    if writer.flush().is_err() {
        return;
    }
    if writer.get_ref().sync_all().is_err() {
        return;
    }
    let _ = fs::rename(&tmp_path, path);
    if let Some(parent) = path.parent()
        && let Ok(directory) = fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn derive_round_seed(session_seed: u64, test_id: &str, round: u32) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(ROUND_SEED_DOMAIN);
    hasher.update([0]);
    hasher.update(test_id.as_bytes());
    hasher.update([0]);
    hasher.update(session_seed.to_le_bytes());
    hasher.update(round.to_le_bytes());
    let digest = hasher.finalize();
    u64::from_le_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 has at least 8 bytes"),
    )
}

fn seed_commitment(config: &SystemRunConfig, source_sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(SEED_COMMITMENT_DOMAIN);
    hasher.update([0]);
    hasher.update(config.test_id.as_bytes());
    hasher.update([0]);
    hasher.update(source_sha256.to_ascii_lowercase().as_bytes());
    hasher.update(config.seed.to_le_bytes());
    hex::encode(hasher.finalize())
}

fn failure_fingerprint(report: &SystemRunReport) -> Option<String> {
    if let Some(mismatch) = &report.mismatch {
        return Some(format!(
            "mismatch:{}:{}:{}:{}:{}:{}",
            mismatch.stage,
            mismatch.round,
            mismatch.global_offset,
            mismatch.expected,
            mismatch.actual,
            mismatch.xor_syndrome
        ));
    }
    if !report.cpu.scalar_simd_differential_passed && report.cpu.cpu_tests_completed {
        return Some("cpu:scalar_simd_differential".to_owned());
    }
    if let Some(crash) = report.execution.worker_crashes.first() {
        return Some(format!(
            "worker_crash:{}:{}:{}",
            crash.phase, crash.worker_id, crash.message
        ));
    }
    if let Some(event) = &report.execution.abort_event {
        return Some(format!("abort:{:?}:{}", event.kind, event.reason));
    }
    report
        .failure_reason
        .as_ref()
        .map(|reason| format!("failure:{reason}"))
}

fn panic_message(payload: &Box<dyn Any + Send>) -> String {
    payload.downcast_ref::<&str>().map_or_else(
        || {
            payload
                .downcast_ref::<String>()
                .cloned()
                .unwrap_or_else(|| "non-string panic payload".to_owned())
        },
        |message| (*message).to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::{SeedSource, SystemRunConfig, derive_round_seed};
    use crate::{LockPolicy, MemoryTarget, SafetyFaultInjection, SafetyPolicy};

    #[test]
    fn rejects_fault_outside_explicit_target() {
        let config = SystemRunConfig {
            test_id: "test".to_owned(),
            target: MemoryTarget::Bytes(1024),
            safety_reserve_bytes: 0,
            arena_target_bytes: 4096,
            minimum_arena_bytes: 4096,
            lock_policy: LockPolicy::Off,
            rounds: 1,
            minimum_duration_ms: 0,
            emit_progress: false,
            progress_report_path: None,
            requested_workers: 1,
            seed: 1,
            seed_source: SeedSource::OperatorReplay,
            coherency_iterations_per_worker: 1,
            fault_injection_global_offset: Some(1024),
            fault_injection_simd_worker: None,
            fault_injection_worker_crash: None,
            safety_policy: SafetyPolicy::default(),
            safety_fault_injection: SafetyFaultInjection::default(),
            diagnostic_rerun: false,
            compute_oracle_workers: 0,
            compute_oracle_dense_fma_iterations: 1,
            compute_oracle_idle_ramp_enabled: false,
            compute_oracle_idle_sleep_ms: 0,
            fault_injection_compute_oracle_worker: None,
            fault_events_path: None,
            confirm_on_fail: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn round_seed_derivation_is_deterministic_and_domain_separated() {
        let first = derive_round_seed(0x0123_4567_89ab_cdef, "CPU-DIT-TEST", 0);
        assert_eq!(
            first,
            derive_round_seed(0x0123_4567_89ab_cdef, "CPU-DIT-TEST", 0)
        );
        assert_ne!(
            first,
            derive_round_seed(0x0123_4567_89ab_cdef, "CPU-DIT-TEST", 1)
        );
        assert_ne!(
            first,
            derive_round_seed(0x0123_4567_89ab_cdef, "OTHER-TEST", 0)
        );
        assert_eq!(
            derive_round_seed(0x7bc0_239b_31d7_34ed, "CPU-DIT-01-SYSTEM-v1", 0),
            0x9ea0_8511_6157_a55f
        );
    }
}
