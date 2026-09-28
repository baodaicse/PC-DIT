use std::path::PathBuf;

use dit_core::{RunStatus, load_verified_source};
use dit_system::{
    LockPolicy, MemoryTarget, SafetyFaultInjection, SafetyPolicy, SeedSource, SystemRunConfig,
    run_system,
};

fn verified_source() -> dit_core::VerifiedSource {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    load_verified_source(
        root.join("vectors/source-v1.dat"),
        root.join("vectors/source-v1.manifest.json"),
    )
    .expect("verified source")
}

fn config(fault: Option<u64>) -> SystemRunConfig {
    SystemRunConfig {
        test_id: "CPU-DIT-01-SYSTEM-TEST-v1".to_owned(),
        target: MemoryTarget::Bytes(4 * 1024 * 1024),
        safety_reserve_bytes: 0,
        arena_target_bytes: 1024 * 1024,
        minimum_arena_bytes: 1024 * 1024,
        lock_policy: LockPolicy::Off,
        rounds: 2,
        minimum_duration_ms: 0,
        emit_progress: false,
        progress_report_path: None,
        requested_workers: 2,
        seed: 0x0123_4567_89ab_cdef,
        seed_source: SeedSource::OperatorReplay,
        coherency_iterations_per_worker: 10_000,
        fault_injection_global_offset: fault,
        fault_injection_simd_worker: None,
        fault_injection_worker_crash: None,
        safety_policy: SafetyPolicy::default(),
        safety_fault_injection: SafetyFaultInjection::default(),
        diagnostic_rerun: false,
        compute_oracle_workers: 0,
        compute_oracle_dense_fma_iterations: 1_000,
        compute_oracle_idle_ramp_enabled: false,
        compute_oracle_idle_sleep_ms: 0,
        fault_injection_compute_oracle_worker: None,
        fault_events_path: None,
    }
}

#[test]
fn adaptive_multi_core_pipeline_passes() {
    let report = run_system(&verified_source(), &config(None)).expect("system run");
    assert_eq!(report.status, RunStatus::Pass);
    assert_eq!(report.schema_version, 10);
    assert!(!report.compute_oracle.enabled);
    assert!(report.memory.allocation_target_met);
    assert!(report.memory.coverage_target_met);
    assert_eq!(report.memory.exact_verified_unique_bytes, 4 * 1024 * 1024);
    assert_eq!(report.rounds_completed, 2);
    assert!(report.duration_target_met);
    assert!(report.cpu.cross_core_role_rotation);
    assert!(report.cpu.scalar_simd_differential_passed);
    assert!(report.cpu.integer_known_answer_passed);
    assert!(report.cpu.sha256_known_answer_passed);
    assert!(report.cpu.fp_fma_known_answer_passed);
    assert!(report.cpu.coherency_passed);
    assert!(report.round_seeds_unique);
    assert_eq!(report.config.seed_source, "operator_replay");
    assert_eq!(report.round_digests.len(), 2);
    assert_ne!(
        report.round_digests[0].round_seed_hex,
        report.round_digests[1].round_seed_hex
    );
    assert!(report.window_coverage.plans_no_gaps);
    assert!(report.window_coverage.plans_no_overlaps);
    assert!(report.calculations_completed >= 8 * 1024 * 1024);
    assert_eq!(report.failures_total, 0);
}

#[test]
fn release_sized_round_covers_full_window_and_pattern_recipe() {
    let mut config = config(None);
    config.target = MemoryTarget::Bytes(64 * 1024 * 1024);
    config.arena_target_bytes = 32 * 1024 * 1024;
    config.minimum_arena_bytes = 4 * 1024 * 1024;
    config.rounds = 1;

    let report = run_system(&verified_source(), &config).expect("window recipe system run");
    assert_eq!(report.status, RunStatus::Pass);
    assert!(report.window_coverage.mandatory_recipe_applicable);
    assert!(report.window_coverage.all_completed_rounds_met);
    let round = &report.round_digests[0];
    assert!(round.mandatory_recipe_met);
    assert!(round.no_gaps);
    assert!(round.no_overlaps);
    assert_eq!(
        round.size_class_verified_bytes.values().sum::<u64>(),
        64 * 1024 * 1024
    );
    assert_eq!(round.pattern_counts.len(), 8);
}

#[test]
fn replay_seed_reproduces_round_seeds_and_digests() {
    let mut replay = config(None);
    replay.target = MemoryTarget::Bytes(64 * 1024);
    replay.arena_target_bytes = 64 * 1024;
    replay.minimum_arena_bytes = 64 * 1024;
    replay.requested_workers = 1;

    let first = run_system(&verified_source(), &replay).expect("first replay run");
    let second = run_system(&verified_source(), &replay).expect("second replay run");
    assert_eq!(first.status, RunStatus::Pass);
    assert_eq!(second.status, RunStatus::Pass);
    assert_eq!(first.config.seed_hex, second.config.seed_hex);
    assert_eq!(
        first.config.seed_commitment_sha256,
        second.config.seed_commitment_sha256
    );
    assert_eq!(first.round_digests, second.round_digests);
}

#[test]
fn minimum_duration_repeats_complete_rounds() {
    let mut config = config(None);
    config.target = MemoryTarget::Bytes(4096);
    config.arena_target_bytes = 4096;
    config.minimum_arena_bytes = 4096;
    config.requested_workers = 1;
    config.rounds = 1;
    config.minimum_duration_ms = 50;

    let report = run_system(&verified_source(), &config).expect("duration system run");
    assert_eq!(report.status, RunStatus::Pass);
    assert!(report.duration_target_met);
    assert!(report.elapsed_ms >= 50);
    assert!(report.rounds_completed > 1);
}

#[test]
fn adaptive_pipeline_fault_cannot_pass() {
    let report = run_system(&verified_source(), &config(Some(12_345))).expect("system run");
    assert_eq!(report.status, RunStatus::Fail);
    let mismatch = report.mismatch.expect("mismatch evidence");
    assert_eq!(mismatch.stage, "RAM_EXACT_VERIFY");
    assert_eq!(mismatch.global_offset, 12_345);
    assert_eq!(mismatch.xor_syndrome, 1);
    assert!(!mismatch.window_class.is_empty());
    assert!(!mismatch.pattern_id.is_empty());
    assert_eq!(report.rounds_completed, 2);
    assert!(report.calculations_completed >= 8 * 1024 * 1024);
    assert_eq!(report.failures_total, 1);
}

#[test]
fn adaptive_pipeline_verifies_non_page_aligned_tail() {
    let mut config = config(None);
    config.target = MemoryTarget::Bytes(4 * 1024 * 1024 + 17);

    let report = run_system(&verified_source(), &config).expect("tail system run");
    assert_eq!(report.status, RunStatus::Pass);
    assert_eq!(report.memory.arena_count, 5);
    assert_eq!(report.arenas.last().expect("tail arena").length_bytes, 17);
    assert_eq!(
        report.memory.exact_verified_unique_bytes,
        4 * 1024 * 1024 + 17
    );
    assert!(report.memory.coverage_target_met);
}

#[test]
fn simd_worker_fault_cannot_pass_scalar_oracle() {
    let mut config = config(None);
    config.fault_injection_simd_worker = Some(0);

    let report = run_system(&verified_source(), &config).expect("SIMD fault run");
    assert_eq!(report.status, RunStatus::Fail);
    assert!(!report.cpu.scalar_simd_differential_passed);
    assert!(
        report
            .workers
            .iter()
            .any(|worker| worker.scalar_simd_mismatches > 0)
    );
    assert_eq!(
        report.failure_reason.as_deref(),
        Some("scalar/SIMD differential mismatch")
    );
    assert_eq!(report.failures_total, 1);
}

#[test]
fn compute_oracle_runs_concurrently_with_ram_route() {
    let mut config = config(None);
    config.compute_oracle_workers = 1;
    config.compute_oracle_dense_fma_iterations = 2_000;

    let report = run_system(&verified_source(), &config).expect("compute-oracle system run");
    assert_eq!(report.status, RunStatus::Pass);
    assert!(report.compute_oracle.enabled);
    assert_eq!(report.compute_oracle.workers_used, 1);
    assert!(report.compute_oracle.bursts_completed > 0);
    assert!(report.compute_oracle.dense_fma_cross_worker_consistent);
    assert!(report.compute_oracle.dense_fma_scalar_reference_passed);
    assert!(report.compute_oracle.branch_predictor_passed);
    assert!(report.compute_oracle.aes256_known_answer_passed);
    assert!(report.compute_oracle.mismatch.is_none());
    // Compute-oracle CPUs must not overlap the RAM route's worker CPUs.
    assert!(
        report
            .compute_oracle
            .worker_cpu_ids
            .iter()
            .all(|cpu| !report.cpu.worker_cpu_ids.contains(cpu))
    );
}

#[test]
fn compute_oracle_fault_injection_cannot_pass() {
    let mut config = config(None);
    config.compute_oracle_workers = 2;
    config.compute_oracle_dense_fma_iterations = 2_000;
    config.fault_injection_compute_oracle_worker = Some(1);

    let report = run_system(&verified_source(), &config).expect("compute-oracle fault run");
    assert_eq!(report.status, RunStatus::Fail);
    assert!(!report.compute_oracle.dense_fma_cross_worker_consistent);
    let mismatch = report.compute_oracle.mismatch.expect("mismatch evidence");
    assert_eq!(mismatch.kernel, "dense_fma_cross_worker");
    assert!(
        report
            .failure_reason
            .as_deref()
            .unwrap_or_default()
            .contains("compute-oracle")
    );
}

#[test]
fn diagnostic_rerun_reproduces_ram_fault_route() {
    let mut config = config(Some(12_345));
    config.diagnostic_rerun = true;

    let report = run_system(&verified_source(), &config).expect("diagnostic run");
    assert_eq!(report.status, RunStatus::Fail);
    assert!(report.diagnostic.attempted);
    assert_eq!(report.diagnostic.classification, "reproducible_same_route");
    assert_eq!(report.diagnostic.same_failure_reproduced, Some(true));
    assert_eq!(
        report
            .diagnostic
            .rerun_mismatch
            .expect("rerun mismatch")
            .global_offset,
        12_345
    );
}

#[test]
fn synthetic_temperature_safety_stop_cannot_pass() {
    let mut config = config(None);
    config.safety_policy.temperature_limit_enabled = true;
    config.safety_fault_injection.temperature_millicelsius = Some(100_000);

    let report = run_system(&verified_source(), &config).expect("temperature fault run");
    assert_eq!(report.status, RunStatus::Fail);
    assert!(report.safety.stop_requested);
    assert!(report.telemetry.peak_temperature_millicelsius >= Some(100_000));
}

#[test]
fn temperature_limit_defaults_off_and_does_not_abort() {
    let mut config = config(None);
    config.safety_fault_injection.temperature_millicelsius = Some(127_000);

    let report = run_system(&verified_source(), &config).expect("telemetry-only run");
    assert_eq!(report.status, RunStatus::Pass);
    assert!(!report.safety.policy.temperature_limit_enabled);
    assert!(!report.safety.stop_requested);
    assert_eq!(
        report.telemetry.peak_temperature_millicelsius,
        Some(127_000)
    );
}

#[test]
fn temperature_abort_skips_automatic_diagnostic_rerun() {
    let mut config = config(None);
    config.safety_policy.temperature_limit_enabled = true;
    config.safety_fault_injection.temperature_millicelsius = Some(100_000);
    config.diagnostic_rerun = true;

    let report = run_system(&verified_source(), &config).expect("temperature fault run");
    assert_eq!(report.status, RunStatus::Fail);
    assert!(!report.diagnostic.attempted);
    assert_eq!(
        report.diagnostic.classification,
        "skipped_temperature_safety_abort"
    );
}

#[test]
fn caught_worker_crash_has_evidence_and_cannot_pass() {
    let mut config = config(None);
    config.fault_injection_worker_crash = Some(0);

    let report = run_system(&verified_source(), &config).expect("worker crash run");
    assert_eq!(report.status, RunStatus::Fail);
    assert_eq!(report.execution.worker_crashes.len(), 1);
    assert!(report.execution.worker_crashes[0].injected);
}

#[test]
fn synthetic_uncorrected_edac_event_cannot_pass() {
    let mut config = config(None);
    config.safety_fault_injection.edac_uncorrected_delta = 1;

    let report = run_system(&verified_source(), &config).expect("EDAC fault run");
    assert_eq!(report.status, RunStatus::Fail);
    assert_eq!(report.telemetry.edac_uncorrected_delta, 1);
    assert!(!report.safety.violations.is_empty());
}

#[test]
fn real_deadline_monitor_aborts_longer_run() {
    let mut config = config(None);
    config.safety_policy.deadline_ms = 1;
    config.safety_policy.monitor_interval_ms = 1;

    let report = run_system(&verified_source(), &config).expect("deadline run");
    assert_eq!(report.status, RunStatus::Fail);
    assert!(report.safety.stop_requested);
    assert_eq!(
        report.execution.abort_event.expect("deadline abort").kind,
        dit_system::AbortKind::Deadline
    );
}

#[test]
fn progress_report_heartbeat_reflects_last_phase() {
    let mut config = config(None);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("cpu-dit-progress-test-{unique}.json"));
    config.progress_report_path = Some(path.clone());

    let report = run_system(&verified_source(), &config).expect("system run");
    assert_eq!(report.status, RunStatus::Pass);

    let contents = std::fs::read_to_string(&path).expect("heartbeat file written");
    let value: serde_json::Value = serde_json::from_str(&contents).expect("valid JSON heartbeat");
    assert_eq!(value["status"], "running");
    assert_eq!(value["phase"], "FINALIZING");
    assert_eq!(value["test_id"], "CPU-DIT-01-SYSTEM-TEST-v1");
    assert!(value["calculations_completed"].as_u64().expect("count") > 0);
    assert!(!path.with_extension("json.tmp").exists());

    std::fs::remove_file(&path).ok();
}
