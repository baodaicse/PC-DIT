//! Versioned contracts shared by standalone DIT modules and the future orchestrator.

use serde::{Deserialize, Serialize};
use std::fmt;

pub mod bounded;
pub mod fault;
pub use fault::{FaultRecord, FaultTail, FaultWriter};

pub const SCHEMA_VERSION: u32 = 1;
pub const NVME_MODULE_ID: &str = "dit.cpu-nvme";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunId(pub String);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub String);

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Pass,
    Fail,
    Aborted,
    Invalid,
    NotRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Preparing,
    Writing,
    Reading,
    Verifying,
    Finalizing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    DataMismatch,
    ShortRead,
    ShortWrite,
    IoError,
    Timeout,
    DeviceDisappeared,
    WriteBudgetExceeded,
    UnalignedIo,
    Internal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReason {
    OperationTarget,
    DurationReached,
    Failure,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PerformanceSummary {
    pub elapsed_seconds: f64,
    pub average_read_bytes_per_sec: f64,
    pub average_write_bytes_per_sec: f64,
    pub peak_read_bytes_per_sec: f64,
    pub peak_write_bytes_per_sec: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    DeepProgress {
        stage: String,
        evidence: DeepEvidence,
    },
    Hello {
        engine_version: String,
    },
    Inventory {
        model: String,
        capacity_bytes: u64,
        backend: String,
    },
    Selection {
        selected: Vec<DeviceId>,
    },
    Phase {
        phase: Phase,
        operation: u64,
    },
    Progress {
        completed_operations: u64,
        target_operations: u64,
        bytes_written: u64,
        bytes_verified: u64,
        failures: u64,
    },
    Throughput {
        current_read_bytes_per_sec: f64,
        current_write_bytes_per_sec: f64,
        average_read_bytes_per_sec: f64,
        average_write_bytes_per_sec: f64,
        peak_read_bytes_per_sec: f64,
        peak_write_bytes_per_sec: f64,
    },
    Failure {
        kind: FailureKind,
        operation: u64,
        offset_bytes: u64,
        detail: String,
        /// Result of the immediate confirmation (re-check, re-read), when it ran.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        confirm: Option<String>,
    },
    Warning {
        code: String,
        detail: String,
    },
    Completed {
        verdict: Verdict,
        completed_operations: u64,
        failures: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub schema_version: u32,
    pub run_id: RunId,
    pub module_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<DeviceId>,
    pub sequence: u64,
    pub monotonic_ns: u64,
    pub wall_time_unix_ms: u64,
    #[serde(flatten)]
    pub event: Event,
}

impl EventEnvelope {
    #[must_use]
    pub fn new(
        run_id: RunId,
        device_id: Option<DeviceId>,
        sequence: u64,
        monotonic_ns: u64,
        wall_time_unix_ms: u64,
        event: Event,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            run_id,
            module_id: NVME_MODULE_ID.to_owned(),
            device_id,
            sequence,
            monotonic_ns,
            wall_time_unix_ms,
            event,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    pub kind: FailureKind,
    pub operation: u64,
    pub offset_bytes: u64,
    pub detail: String,
    /// Result of the immediate confirmation (re-check, re-read), when it ran: `recheck_clean`,
    /// `reread_clean`, `reread_same` or `reread_differs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceReport {
    pub schema_version: u32,
    pub run_id: RunId,
    pub module_id: String,
    pub device_id: DeviceId,
    pub verdict: Verdict,
    pub target_operations: u64,
    pub completed_operations: u64,
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub bytes_verified: u64,
    pub seed_slots: u32,
    pub generations: u64,
    pub failure_count: u64,
    pub failures: Vec<FailureRecord>,
    pub completion_reason: CompletionReason,
    pub performance: PerformanceSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep: Option<DeepEvidence>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepEvidence {
    pub pipeline_version: String,
    pub block_format: String,
    pub compute_workers: usize,
    pub matrix_rounds: u32,
    pub max_inflight_batches: usize,
    pub compute_jobs_completed: u64,
    pub matrix_checks_completed: u64,
    pub write_operations_completed: u64,
    pub read_soak_operations: u64,
    pub prepare_cpu_ns: u64,
    pub verify_cpu_ns: u64,
    pub write_io_ns: u64,
    pub read_io_ns: u64,
    pub compute_wait_ns: u64,
    pub duration_target_ms: u64,
    pub duration_target_met: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_schema_round_trips() {
        let event = EventEnvelope::new(
            RunId("run-1".into()),
            Some(DeviceId("file:test".into())),
            7,
            12,
            34,
            Event::Progress {
                completed_operations: 3,
                target_operations: 10,
                bytes_written: 4096,
                bytes_verified: 4096,
                failures: 0,
            },
        );
        let json = serde_json::to_string(&event).expect("serialize");
        let decoded: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, event);
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"type\":\"progress\""));
    }

    #[test]
    fn verdict_is_explicitly_serialized() {
        assert_eq!(serde_json::to_string(&Verdict::Pass).unwrap(), "\"PASS\"");
        assert_eq!(
            serde_json::to_string(&Verdict::Invalid).unwrap(),
            "\"INVALID\""
        );
    }

    #[test]
    fn deep_progress_schema_round_trips() {
        let event = EventEnvelope::new(
            RunId("deep-run".into()),
            Some(DeviceId("nvme:serial".into())),
            9,
            100,
            200,
            Event::DeepProgress {
                stage: "READ_SOAK_COMPUTE".into(),
                evidence: DeepEvidence {
                    pipeline_version: "deep-v1".into(),
                    block_format: "DITDEEP2".into(),
                    compute_workers: 8,
                    matrix_rounds: 4,
                    matrix_checks_completed: 123,
                    read_soak_operations: 7,
                    duration_target_met: true,
                    ..DeepEvidence::default()
                },
            },
        );
        let json = serde_json::to_string(&event).expect("serialize deep progress");
        let decoded: EventEnvelope =
            serde_json::from_str(&json).expect("deserialize deep progress");
        assert_eq!(decoded, event);
        assert!(json.contains("\"type\":\"deep_progress\""));
        assert!(json.contains("\"matrix_checks_completed\":123"));
    }
}
