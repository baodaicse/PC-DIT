//! Deterministic CPU–NVMe Deep integrity engine for memory, file and bounded backends.

mod deep_block;
mod deep_pool;
mod deep_run;

use dit_contracts::{
    CompletionReason, DeviceId, DeviceReport, Event, EventEnvelope, FailureKind, FailureRecord,
    PerformanceSummary, Phase, RunId, SCHEMA_VERSION, Verdict,
};
use dit_source::{BLOCK_SIZE, BlockContext};
use nvme_io::{BackendError, StorageBackend};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

const TRANSFER_SIZE_CATALOG: [usize; 20] = [
    4 * 1024,
    64 * 1024,
    128 * 1024,
    1024 * 1024,
    8 * 1024,
    12 * 1024,
    16 * 1024,
    24 * 1024,
    32 * 1024,
    48 * 1024,
    80 * 1024,
    96 * 1024,
    160 * 1024,
    192 * 1024,
    256 * 1024,
    320 * 1024,
    384 * 1024,
    512 * 1024,
    640 * 1024,
    768 * 1024,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoveragePlan {
    pub duration_seconds: u64,
    pub target_operations: u64,
    pub base_seed: u64,
    pub seed_slots: u32,
    pub transfer_sizes: Vec<usize>,
}

impl CoveragePlan {
    #[must_use]
    pub fn for_duration(duration: Duration, target_operations: u64, base_seed: u64) -> Self {
        let duration_seconds = duration.as_secs().max(1);
        let seed_slots_u64 = duration_seconds.div_ceil(180).clamp(1, 20);
        let seed_slots = u32::try_from(seed_slots_u64).unwrap_or(20);
        let transfer_count = usize::try_from(seed_slots).unwrap_or(20).max(4);
        let transfer_sizes = TRANSFER_SIZE_CATALOG[..transfer_count].to_vec();
        Self {
            duration_seconds,
            target_operations: target_operations.max(1),
            base_seed,
            seed_slots,
            transfer_sizes,
        }
    }

    #[must_use]
    pub fn operation(&self, index: u64, capacity_bytes: u64) -> OperationSpec {
        let slot_count = self.seed_slots.max(1);
        let seed_slot = u32::try_from(index % u64::from(slot_count)).unwrap_or(0);
        let size_count = u64::try_from(self.transfer_sizes.len()).unwrap_or(1).max(1);
        let size_index = usize::try_from(index % size_count).unwrap_or(0);
        let transfer_size = self
            .transfer_sizes
            .get(size_index)
            .copied()
            .unwrap_or(BLOCK_SIZE);
        let transfer_blocks = u64::try_from(transfer_size / BLOCK_SIZE).unwrap_or(1);
        let capacity_blocks = capacity_bytes / BLOCK_SIZE as u64;
        let possible_starts = capacity_blocks
            .saturating_sub(transfer_blocks)
            .saturating_add(1);
        let mixed = splitmix64(
            self.base_seed ^ index.rotate_left(17) ^ u64::from(seed_slot).rotate_left(41),
        );
        let mut lba = if possible_starts == 0 {
            0
        } else {
            mixed % possible_starts
        };
        lba -= lba % transfer_blocks.max(1);
        OperationSpec {
            index,
            session_seed: splitmix64(self.base_seed ^ u64::from(seed_slot)),
            seed_slot,
            generation: index / u64::from(self.seed_slots),
            lba,
            transfer_size,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationSpec {
    pub index: u64,
    pub session_seed: u64,
    pub seed_slot: u32,
    pub generation: u64,
    pub lba: u64,
    pub transfer_size: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FaultKind {
    BitFlip,
    StaleGeneration,
    SwappedLba,
    DuplicateBlock,
    ShortRead,
    ShortWrite,
    IoError,
    Timeout,
    DeviceDisappeared,
    ProducerCompute,
    ReadCompute,
    ReferenceCompute,
    JoinMismatch,
    ReadSoakBitFlip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultPlan {
    pub kind: FaultKind,
    pub at_operation: u64,
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub run_id: RunId,
    pub device_id: DeviceId,
    pub plan: CoveragePlan,
    pub fault: Option<FaultPlan>,
    pub max_failure_records: usize,
    /// Optional real wall-clock stop for finite or sustained runs.
    pub wall_clock_limit: Option<Duration>,
    /// Sustained mode rereads verified extents after the finite write phase.
    pub sustain_until_deadline: bool,
    pub compute_workers: usize,
    pub matrix_rounds: u32,
}

impl RunConfig {
    #[must_use]
    pub fn new(run_id: RunId, device_id: DeviceId, plan: CoveragePlan) -> Self {
        Self {
            run_id,
            device_id,
            plan,
            fault: None,
            max_failure_records: 32,
            wall_clock_limit: None,
            sustain_until_deadline: false,
            compute_workers: 1,
            matrix_rounds: 4,
        }
    }
}

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("invalid deep configuration: {0}")]
    Configuration(String),
    #[error("backend capacity {actual} is smaller than largest transfer {required}")]
    CapacityTooSmall { actual: u64, required: usize },
    #[error("backend error before campaign could produce a verdict: {0}")]
    Backend(#[from] BackendError),
}

/// Runs one complete deterministic Deep campaign.
///
/// The backend may be memory, a newly-created direct-I/O file or an authorized bounded target.
/// Events are emitted in sequence as work progresses and a final per-device report is returned.
///
/// # Errors
///
/// Returns [`EngineError::CapacityTooSmall`] when the backend cannot fit the largest transfer.
/// A backend error after the campaign starts is captured as a FAIL record instead of escaping.
pub fn run<B, F>(
    backend: &mut B,
    config: &RunConfig,
    event_sink: F,
) -> Result<DeviceReport, EngineError>
where
    B: StorageBackend,
    F: FnMut(EventEnvelope),
{
    deep_run::run(backend, config, event_sink)
}

fn backend_failure_kind(error: &BackendError) -> FailureKind {
    match error {
        BackendError::WriteBudgetExceeded { .. } => FailureKind::WriteBudgetExceeded,
        BackendError::Unaligned { .. } => FailureKind::UnalignedIo,
        BackendError::OutOfRange { .. }
        | BackendError::Exists(_)
        | BackendError::UnsafeTarget(_)
        | BackendError::IdentityChanged(_)
        | BackendError::Io(_) => FailureKind::IoError,
    }
}

#[allow(clippy::cast_precision_loss)]
fn bytes_per_second(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / elapsed.as_secs_f64().max(f64::EPSILON)
}

fn operation_contexts(operation: &OperationSpec, device_id: &DeviceId) -> Vec<BlockContext> {
    let block_count = operation.transfer_size / BLOCK_SIZE;
    (0..block_count)
        .map(|index| {
            let stripe_group = u64::try_from(index / 16).unwrap_or(0);
            BlockContext {
                session_seed: operation.session_seed,
                generation: operation.generation,
                device_id: device_id.0.clone(),
                lba: operation.lba + index as u64,
                stripe_id: (operation.index << 32) | stripe_group,
                stripe_index: u32::try_from(index % 16).unwrap_or(0),
            }
        })
        .collect()
}

fn apply_deep_data_fault(
    kind: FaultKind,
    contexts: &[BlockContext],
    buffer: &mut [u8],
    rounds: u32,
) {
    match kind {
        FaultKind::BitFlip => buffer[dit_source::HEADER_SIZE + 17] ^= 0x40,
        FaultKind::StaleGeneration => {
            let mut stale = contexts[0].clone();
            stale.generation = stale.generation.wrapping_sub(1);
            if let Ok(block) = deep_block::build(&stale, rounds, false) {
                buffer[..BLOCK_SIZE].copy_from_slice(&block);
            }
        }
        FaultKind::SwappedLba => {
            if contexts.len() > 1 {
                let (first, rest) = buffer.split_at_mut(BLOCK_SIZE);
                first.swap_with_slice(&mut rest[..BLOCK_SIZE]);
            } else {
                let mut wrong = contexts[0].clone();
                wrong.lba = wrong.lba.wrapping_add(1);
                if let Ok(block) = deep_block::build(&wrong, rounds, false) {
                    buffer[..BLOCK_SIZE].copy_from_slice(&block);
                }
            }
        }
        FaultKind::DuplicateBlock => {
            if contexts.len() > 1 {
                let first = buffer[..BLOCK_SIZE].to_vec();
                buffer[BLOCK_SIZE..BLOCK_SIZE * 2].copy_from_slice(&first);
            } else {
                buffer[dit_source::HEADER_SIZE + 3] ^= 1;
            }
        }
        FaultKind::ShortRead
        | FaultKind::ShortWrite
        | FaultKind::IoError
        | FaultKind::Timeout
        | FaultKind::DeviceDisappeared
        | FaultKind::ProducerCompute
        | FaultKind::ReadCompute
        | FaultKind::ReferenceCompute
        | FaultKind::JoinMismatch
        | FaultKind::ReadSoakBitFlip => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn record_failure<F>(
    failures: &mut Vec<FailureRecord>,
    failure_count: &mut u64,
    max_records: usize,
    kind: FailureKind,
    operation: u64,
    offset_bytes: u64,
    detail: &str,
    emit: &mut F,
) where
    F: FnMut(Event),
{
    *failure_count = failure_count.saturating_add(1);
    emit(Event::Failure {
        kind,
        operation,
        offset_bytes,
        detail: detail.to_owned(),
    });
    if failures.len() < max_records {
        failures.push(FailureRecord {
            kind,
            operation,
            offset_bytes,
            detail: detail.to_owned(),
        });
    }
}

const fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

fn wall_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvme_backend_file::MemoryBackend;

    fn config(operations: u64) -> RunConfig {
        RunConfig::new(
            RunId("test-run".into()),
            DeviceId("memory:test-device".into()),
            CoveragePlan::for_duration(Duration::from_secs(60), operations, 0x1234),
        )
    }

    #[test]
    fn duration_scales_seed_and_transfer_rotation() {
        let short = CoveragePlan::for_duration(Duration::from_secs(60), 100, 1);
        let thirty = CoveragePlan::for_duration(Duration::from_mins(30), 100, 1);
        let sixty = CoveragePlan::for_duration(Duration::from_mins(60), 100, 1);
        assert_eq!(short.seed_slots, 1);
        assert_eq!(
            short.transfer_sizes,
            vec![4 * 1024, 64 * 1024, 128 * 1024, 1024 * 1024]
        );
        assert_eq!(thirty.seed_slots, 10);
        assert_eq!(thirty.transfer_sizes.len(), 10);
        assert_eq!(sixty.seed_slots, 20);
        assert_eq!(sixty.transfer_sizes.len(), 20);
        assert!(sixty.transfer_sizes.contains(&(1024 * 1024)));
    }

    #[test]
    fn clean_campaign_passes_and_events_are_ordered() {
        let mut backend = MemoryBackend::new("memory:test", 2 * 1024 * 1024);
        let mut events = Vec::new();
        let report = run(&mut backend, &config(40), |event| events.push(event)).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
        assert_eq!(report.completed_operations, 40);
        assert!(report.bytes_verified > 0);
        assert!(
            events
                .windows(2)
                .all(|pair| pair[1].sequence == pair[0].sequence + 1)
        );
        assert!(matches!(
            events.last().unwrap().event,
            Event::Completed { .. }
        ));
    }

    #[test]
    fn every_fault_class_is_detected() {
        let faults = [
            FaultKind::BitFlip,
            FaultKind::StaleGeneration,
            FaultKind::SwappedLba,
            FaultKind::DuplicateBlock,
            FaultKind::ShortRead,
            FaultKind::ShortWrite,
            FaultKind::IoError,
            FaultKind::Timeout,
            FaultKind::DeviceDisappeared,
            FaultKind::ProducerCompute,
            FaultKind::ReadCompute,
            FaultKind::ReferenceCompute,
            FaultKind::JoinMismatch,
        ];
        for kind in faults {
            let mut backend = MemoryBackend::new("memory:test", 2 * 1024 * 1024);
            let mut run_config = config(12);
            run_config.fault = Some(FaultPlan {
                kind,
                at_operation: 3,
            });
            let report = run(&mut backend, &run_config, |_| {}).unwrap();
            assert_eq!(report.verdict, Verdict::Fail, "fault {kind:?}");
            assert!(!report.failures.is_empty(), "fault {kind:?}");
        }
    }

    #[test]
    fn report_json_round_trips() {
        let mut backend = MemoryBackend::new("memory:test", 2 * 1024 * 1024);
        let report = run(&mut backend, &config(2), |_| {}).unwrap();
        let json = serde_json::to_string(&report).unwrap();
        let decoded: DeviceReport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.run_id, report.run_id);
        assert_eq!(decoded.device_id, report.device_id);
        assert_eq!(decoded.verdict, report.verdict);
        assert_eq!(decoded.completed_operations, report.completed_operations);
        assert_eq!(decoded.bytes_written, report.bytes_written);
        assert_eq!(decoded.bytes_read, report.bytes_read);
        assert_eq!(decoded.bytes_verified, report.bytes_verified);
        assert_eq!(decoded.failure_count, report.failure_count);
        assert!(
            (decoded.performance.peak_write_bytes_per_sec
                - report.performance.peak_write_bytes_per_sec)
                .abs()
                <= report.performance.peak_write_bytes_per_sec * f64::EPSILON
        );
    }

    #[test]
    fn wall_clock_limit_finishes_with_duration_reason() {
        let mut backend = MemoryBackend::new("memory:test", 2 * 1024 * 1024);
        let mut run_config = config(100);
        run_config.wall_clock_limit = Some(Duration::ZERO);
        let mut events = Vec::new();
        let report = run(&mut backend, &run_config, |event| events.push(event)).unwrap();
        assert_eq!(report.verdict, Verdict::Pass);
        assert_eq!(report.completion_reason, CompletionReason::DurationReached);
        assert_eq!(report.completed_operations, 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event.event, Event::Throughput { .. }))
        );
    }

    #[test]
    fn sustained_deep_mode_uses_read_soak_without_more_host_writes() {
        let mut backend = MemoryBackend::new("memory:test", 2 * 1024 * 1024);
        let mut run_config = config(1);
        run_config.matrix_rounds = 1;
        run_config.compute_workers = 2;
        run_config.wall_clock_limit = Some(Duration::from_millis(40));
        run_config.sustain_until_deadline = true;
        let report = run(&mut backend, &run_config, |_| {}).unwrap();
        let deep = report.deep.as_ref().expect("deep evidence");
        assert_eq!(report.verdict, Verdict::Pass);
        assert_eq!(report.completion_reason, CompletionReason::DurationReached);
        assert_eq!(report.bytes_written, BLOCK_SIZE as u64);
        assert!(report.bytes_read > report.bytes_written);
        assert!(deep.read_soak_operations > 0);
        assert!(deep.matrix_checks_completed > deep.write_operations_completed);
        assert_eq!(deep.compute_workers, 2);
        assert!(deep.duration_target_met);
    }

    #[test]
    fn read_soak_fault_is_detected_after_write_budget_is_spent() {
        let mut backend = MemoryBackend::new("memory:test", 2 * 1024 * 1024);
        let mut run_config = config(1);
        run_config.matrix_rounds = 1;
        run_config.wall_clock_limit = Some(Duration::from_millis(25));
        run_config.sustain_until_deadline = true;
        run_config.fault = Some(FaultPlan {
            kind: FaultKind::ReadSoakBitFlip,
            at_operation: 0,
        });
        let report = run(&mut backend, &run_config, |_| {}).unwrap();
        assert_eq!(report.verdict, Verdict::Fail);
        assert_eq!(report.bytes_written, BLOCK_SIZE as u64);
        assert!(report.failure_count > 0);
        assert!(
            report
                .failures
                .iter()
                .any(|failure| failure.kind == FailureKind::DataMismatch)
        );
    }
}
