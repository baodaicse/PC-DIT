use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    Pass,
    Fail,
    Invalid,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoverageEvidence {
    pub requested_payload_bytes: u64,
    pub generated_bytes: u64,
    pub allocated_unique_bytes: u64,
    pub touched_unique_bytes: u64,
    pub written_unique_bytes: u64,
    pub read_unique_bytes: u64,
    pub exact_verified_unique_bytes: u64,
    pub recovered_verified_bytes: u64,
    pub total_write_bytes: u64,
    pub total_read_bytes: u64,
    pub group_count: u64,
    pub block_count: u64,
    pub coverage_target_met: bool,
    pub coverage_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageCheckpoint {
    pub stage: String,
    pub expected_sha256: String,
    pub actual_sha256: String,
    pub matched: bool,
}

impl StageCheckpoint {
    #[must_use]
    pub fn new(stage: &str, expected: &str, actual: &str) -> Self {
        Self {
            stage: stage.to_owned(),
            expected_sha256: expected.to_owned(),
            actual_sha256: actual.to_owned(),
            matched: expected.eq_ignore_ascii_case(actual),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MismatchEvidence {
    pub stage: String,
    pub group_id: u64,
    pub logical_block: u64,
    pub block_offset: u64,
    pub global_payload_offset: Option<u64>,
    pub expected: u8,
    pub actual: u8,
    pub xor_syndrome: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEvidence {
    pub source_id: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigEvidence {
    pub descriptor_version: u32,
    pub seed_hex: String,
    pub target_bytes: u64,
    pub block_sizes: Vec<u64>,
    pub reference_memory_limit_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub test_id: String,
    pub status: RunStatus,
    pub source: SourceEvidence,
    pub config: ConfigEvidence,
    pub fault_injection: Option<String>,
    pub coverage: CoverageEvidence,
    pub checkpoints: Vec<StageCheckpoint>,
    pub mismatch: Option<MismatchEvidence>,
    pub failure_reason: Option<String>,
}

impl RunReport {
    pub(crate) fn fail(&mut self, reason: impl Into<String>) {
        self.status = RunStatus::Fail;
        self.failure_reason = Some(reason.into());
        self.coverage.coverage_target_met = false;
    }

    #[must_use]
    pub fn invalid(test_id: &str, reason: impl Into<String>) -> Self {
        Self {
            schema_version: 1,
            test_id: test_id.to_owned(),
            status: RunStatus::Invalid,
            source: SourceEvidence {
                source_id: String::new(),
                size_bytes: 0,
                sha256: String::new(),
            },
            config: ConfigEvidence {
                descriptor_version: 0,
                seed_hex: String::new(),
                target_bytes: 0,
                block_sizes: Vec::new(),
                reference_memory_limit_bytes: 0,
            },
            fault_injection: None,
            coverage: CoverageEvidence {
                coverage_type: "reference_pipeline".to_owned(),
                ..CoverageEvidence::default()
            },
            checkpoints: Vec::new(),
            mismatch: None,
            failure_reason: Some(reason.into()),
        }
    }
}
