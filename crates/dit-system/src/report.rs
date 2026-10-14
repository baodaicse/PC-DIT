use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use dit_core::RunStatus;

use crate::{
    SystemInventory,
    compute_oracle::ComputeOracleMismatch,
    safety::{AbortEvent, SafetyEvidence},
    telemetry::TelemetryEvidence,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemSourceEvidence {
    pub source_id: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfigEvidence {
    pub seed_hex: String,
    pub seed_source: String,
    pub seed_derivation: String,
    pub seed_commitment_sha256: String,
    pub window_recipe_id: String,
    pub window_sizes_bytes: Vec<u64>,
    pub pattern_recipe_id: String,
    pub pattern_families: Vec<String>,
    pub target: String,
    pub safety_reserve_bytes: u64,
    pub arena_target_bytes: u64,
    pub minimum_arena_bytes: u64,
    pub rounds: u32,
    pub minimum_duration_ms: u64,
    pub requested_workers: usize,
    pub lock_policy: String,
    pub coherency_iterations_per_worker: u64,
    /// RAM contents are hashed (SHA-256) in round 0 and every this many rounds after; 1 in
    /// reports written before v1.2.006, which hashed every round.
    #[serde(default = "every_round")]
    pub content_hash_every_rounds: u32,
    /// How workers were placed on CPUs (`CPU-DIT-PLACEMENT-v1`: compute on whole performance
    /// cores, RAM on the rest); empty before v1.2.008: RAM on the first CPUs by number, compute
    /// on the next ones.
    #[serde(default)]
    pub worker_placement: String,
}

pub(crate) const fn one_chain() -> usize {
    1
}

const fn every_round() -> u32 {
    1
}

const fn hashed() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPlanEvidence {
    pub target_kind: String,
    pub effective_available_bytes: u64,
    pub safety_reserve_bytes: u64,
    pub safe_limit_bytes: u64,
    pub requested_bytes: u64,
    pub allocated_unique_bytes: u64,
    pub arena_target_bytes: u64,
    pub minimum_arena_bytes: u64,
    pub arena_count: usize,
    pub allocation_backoff_count: u64,
    pub allocation_target_met: bool,
    pub lock_policy: String,
    pub locked_bytes: u64,
    pub prefaulted_unique_bytes: u64,
    pub written_unique_bytes: u64,
    pub read_unique_bytes: u64,
    pub exact_verified_unique_bytes: u64,
    pub total_write_bytes: u64,
    pub total_read_bytes: u64,
    pub coverage_target_met: bool,
    pub numa_policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArenaEvidence {
    pub arena_id: usize,
    pub global_offset: u64,
    pub length_bytes: u64,
    pub target_numa_node: Option<u32>,
    pub first_touch_cpu: Option<usize>,
    pub verify_cpu: Option<usize>,
    pub lock_succeeded: bool,
    pub lock_error: Option<String>,
    pub rounds_written: u32,
    pub rounds_verified: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerEvidence {
    pub worker_id: usize,
    pub cpu_id: usize,
    pub numa_node: Option<u32>,
    pub affinity_attempts: u64,
    pub affinity_failures: u64,
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub exact_verified_bytes: u64,
    pub scalar_simd_comparisons: u64,
    pub scalar_simd_mismatches: u64,
    /// SHA-256 of the arenas this worker verified in the last round that hashed RAM contents.
    pub sha256_digest: String,
}

#[allow(clippy::struct_excessive_bools)] // Independent CPU oracle results stay explicit in JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuEvidence {
    pub cpu_tests_completed: bool,
    pub workers_requested: usize,
    pub workers_used: usize,
    pub worker_cpu_ids: Vec<usize>,
    pub cross_core_role_rotation: bool,
    pub scalar_simd_differential_passed: bool,
    pub sha256_known_answer_passed: bool,
    pub integer_known_answer_passed: bool,
    pub fp_fma_known_answer_passed: bool,
    pub coherency_workers: usize,
    pub coherency_iterations_per_worker: u64,
    pub coherency_expected: u64,
    pub coherency_actual: u64,
    pub coherency_passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemMismatch {
    pub stage: String,
    pub round: u32,
    pub window_id: u64,
    pub window_class: String,
    pub pattern_id: String,
    pub arena_id: usize,
    pub arena_offset: u64,
    pub global_offset: u64,
    pub expected: u8,
    pub actual: u8,
    pub xor_syndrome: u8,
    pub writer_cpu: Option<usize>,
    pub verifier_cpu: Option<usize>,
    /// Confirmation of this mismatch (component fault ledger, step F4): `verifier_core`,
    /// `transient`, `rewrite_fixed` or `cell_persistent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Independent coverage gates remain explicit in evidence.
pub struct RoundDigest {
    pub round: u32,
    pub round_seed_hex: String,
    pub window_plan_sha256: String,
    pub size_class_counts: BTreeMap<String, u64>,
    pub size_class_verified_bytes: BTreeMap<String, u64>,
    pub pattern_counts: BTreeMap<String, u64>,
    pub mandatory_recipe_applicable: bool,
    pub mandatory_recipe_met: bool,
    pub no_gaps: bool,
    pub no_overlaps: bool,
    pub tail_verified: bool,
    pub total_windows: u64,
    /// Whether this round hashed the RAM contents (`config.content_hash_every_rounds`); every
    /// round did before v1.2.006. The bytes are compared in every round either way.
    #[serde(default = "hashed")]
    pub content_hashed: bool,
    /// SHA-256 over the arenas' digests; empty in a round that did not hash.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Independent release gates remain explicit in evidence.
pub struct WindowCoverageEvidence {
    pub window_recipe_id: String,
    pub pattern_recipe_id: String,
    pub mandatory_size_classes: Vec<String>,
    pub mandatory_pattern_families: Vec<String>,
    pub mandatory_recipe_applicable: bool,
    pub all_completed_rounds_met: bool,
    pub plans_no_gaps: bool,
    pub plans_no_overlaps: bool,
    pub tails_verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerCrashEvidence {
    pub phase: String,
    pub round: u32,
    pub worker_id: usize,
    pub cpu_id: usize,
    pub message: String,
    pub injected: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionEvidence {
    pub aborted: bool,
    pub abort_event: Option<AbortEvent>,
    pub worker_crashes: Vec<WorkerCrashEvidence>,
    pub worker_errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvidence {
    pub attempted: bool,
    pub classification: String,
    pub seed_hex: String,
    pub primary_fingerprint: Option<String>,
    pub rerun_status: Option<RunStatus>,
    pub rerun_fingerprint: Option<String>,
    pub same_failure_reproduced: Option<bool>,
    pub rerun_failure_reason: Option<String>,
    pub rerun_mismatch: Option<SystemMismatch>,
}

impl DiagnosticEvidence {
    #[must_use]
    pub fn not_required(seed: u64) -> Self {
        Self {
            attempted: false,
            classification: "not_required".to_owned(),
            seed_hex: format!("0x{seed:016x}"),
            primary_fingerprint: None,
            rerun_status: None,
            rerun_fingerprint: None,
            same_failure_reproduced: None,
            rerun_failure_reason: None,
            rerun_mismatch: None,
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // Independent oracle results stay explicit in JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemComputeOracleEvidence {
    pub enabled: bool,
    pub workers_requested: usize,
    pub minimum_bursts_per_worker: u64,
    pub coverage_met: bool,
    /// The FMA kernel the bursts ran (`CPU-DIT-FMA-CHAINS-v1` since v1.2, checked bit for bit);
    /// empty in older reports: one chain, checked within a tolerance.
    #[serde(default)]
    pub fma_kernel: String,
    /// Independent FMA chains per burst (1 in older reports).
    #[serde(default = "one_chain")]
    pub fma_chains: usize,
    pub dense_fma_iterations_per_burst: u32,
    pub workers_used: usize,
    pub worker_cpu_ids: Vec<usize>,
    pub bursts_completed: u64,
    pub idle_ramp_cycles: u64,
    pub dense_fma_cross_worker_consistent: bool,
    pub dense_fma_scalar_reference_passed: bool,
    pub branch_predictor_passed: bool,
    pub aes256_known_answer_passed: bool,
    pub mismatch: Option<ComputeOracleMismatch>,
}

impl SystemComputeOracleEvidence {
    #[must_use]
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            workers_requested: 0,
            minimum_bursts_per_worker: 0,
            coverage_met: true,
            fma_kernel: String::new(),
            fma_chains: 0,
            dense_fma_iterations_per_burst: 0,
            workers_used: 0,
            worker_cpu_ids: Vec::new(),
            bursts_completed: 0,
            idle_ramp_cycles: 0,
            dense_fma_cross_worker_consistent: true,
            dense_fma_scalar_reference_passed: true,
            branch_predictor_passed: true,
            aes256_known_answer_passed: true,
            mismatch: None,
        }
    }
}

/// Best-effort heartbeat snapshot written to disk while a system-run is still
/// in progress, so a hang or crash leaves evidence of the last reached phase
/// instead of no report at all. Overwritten at every phase transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemProgressReport {
    pub test_id: String,
    pub status: &'static str,
    pub phase: String,
    pub round: u32,
    pub rounds_requested: u32,
    pub minimum_duration_ms: u64,
    pub elapsed_ms: u128,
    pub target_bytes: u64,
    pub calculations_completed: u64,
    pub failures_total: u64,
    pub session_seed_hex: String,
    pub seed_source: String,
    pub round_seed_hex: Option<String>,
    pub seed_commitment_sha256: String,
    pub temperature_limit_enabled: bool,
    pub updated_unix_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemRunReport {
    pub schema_version: u32,
    /// DIT-01 version that wrote this report (absent before v1.2: v1.1).
    #[serde(default)]
    pub dit_version: String,
    pub test_id: String,
    pub status: RunStatus,
    pub availability: crate::availability::AvailabilityEvidence,
    pub power_platform: crate::power_platform::PowerPlatformEvidence,
    pub source: SystemSourceEvidence,
    pub config: SystemConfigEvidence,
    pub inventory: SystemInventory,
    pub memory: MemoryPlanEvidence,
    pub cpu: CpuEvidence,
    pub compute_oracle: SystemComputeOracleEvidence,
    pub arenas: Vec<ArenaEvidence>,
    pub workers: Vec<WorkerEvidence>,
    pub round_digests: Vec<RoundDigest>,
    /// How the threads were split between RAM and compute workers (v1.2.009 on; absent before).
    #[serde(default)]
    pub worker_split: Option<crate::split::WorkerSplitEvidence>,
    /// The instruction-set level the RAM and compute kernels ran at (v1.2.010 on; absent
    /// before: baseline).
    #[serde(default)]
    pub cpu_isa: Option<crate::isa::IsaEvidence>,
    pub window_coverage: WindowCoverageEvidence,
    pub rounds_requested: u32,
    pub rounds_completed: u32,
    pub round_seeds_unique: bool,
    pub duration_target_ms: u64,
    pub duration_target_met: bool,
    pub elapsed_ms: u128,
    pub calculations_completed: u64,
    pub failures_total: u64,
    pub telemetry: TelemetryEvidence,
    pub safety: SafetyEvidence,
    pub execution: ExecutionEvidence,
    pub diagnostic: DiagnosticEvidence,
    pub fault_injection_global_offset: Option<u64>,
    pub fault_injection_simd_worker: Option<usize>,
    pub fault_injection_worker_crash: Option<usize>,
    pub mismatch: Option<SystemMismatch>,
    pub failure_reason: Option<String>,
    pub limitations: Vec<String>,
}
