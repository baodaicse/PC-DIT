use sha2::{Digest, Sha256};

use crate::{
    DitError, GoldenVector, VerifiedSource,
    gf256::{MATRIX_WIDTH, apply, invert, reference_matrix},
    pattern::{expanded_byte, expansion_multiplier, splitmix64},
    report::{
        ConfigEvidence, CoverageEvidence, MismatchEvidence, RunReport, RunStatus, SourceEvidence,
        StageCheckpoint,
    },
};

const MAX_REFERENCE_BYTES: u64 = 64 * 1024 * 1024;
const SCATTER_DOMAIN: u64 = 0x5343_4154_5445_5231;

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub test_id: String,
    pub descriptor_version: u32,
    pub target_bytes: u64,
    pub seed: u64,
    pub block_sizes: Vec<u64>,
}

impl RunConfig {
    /// Build a bounded deterministic reference configuration.
    ///
    /// # Errors
    ///
    /// Returns an invalid-configuration error when an identifier/version is
    /// invalid, the target exceeds the safety limit, or a block size is zero.
    pub fn new(
        test_id: String,
        descriptor_version: u32,
        target_bytes: u64,
        seed: u64,
        block_sizes: Vec<u64>,
    ) -> Result<Self, DitError> {
        if test_id.is_empty() {
            return Err(DitError::InvalidConfig("test_id is empty".to_owned()));
        }
        if descriptor_version == 0 {
            return Err(DitError::InvalidConfig(
                "descriptor_version must be positive".to_owned(),
            ));
        }
        if target_bytes == 0 || target_bytes > MAX_REFERENCE_BYTES {
            return Err(DitError::InvalidConfig(format!(
                "target_bytes must be between 1 and {MAX_REFERENCE_BYTES} for the reference pipeline"
            )));
        }
        if block_sizes.is_empty() || block_sizes.contains(&0) {
            return Err(DitError::InvalidConfig(
                "block_sizes must contain only positive values".to_owned(),
            ));
        }
        Ok(Self {
            test_id,
            descriptor_version,
            target_bytes,
            seed,
            block_sizes,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultInjection {
    EncodeByte {
        logical_block: usize,
        byte_offset: usize,
        xor_mask: u8,
    },
    ReadbackByte {
        logical_block: usize,
        byte_offset: usize,
        xor_mask: u8,
    },
    RecoveredByte {
        global_offset: u64,
        xor_mask: u8,
    },
}

impl FaultInjection {
    fn description(self) -> String {
        match self {
            Self::EncodeByte {
                logical_block,
                byte_offset,
                xor_mask,
            } => format!(
                "encode-byte:logical-block={logical_block},offset={byte_offset},xor=0x{xor_mask:02x}"
            ),
            Self::ReadbackByte {
                logical_block,
                byte_offset,
                xor_mask,
            } => format!(
                "readback-byte:logical-block={logical_block},offset={byte_offset},xor=0x{xor_mask:02x}"
            ),
            Self::RecoveredByte {
                global_offset,
                xor_mask,
            } => format!("recovered-byte:global-offset={global_offset},xor=0x{xor_mask:02x}"),
        }
    }
}

#[derive(Debug, Clone)]
struct GroupDescriptor {
    group_id: usize,
    global_offset: u64,
    block_size: usize,
    valid_bytes: usize,
}

/// Run the complete deterministic reference data route.
///
/// # Errors
///
/// Returns an error for invalid source/golden/config relationships, allocation
/// constraints, invalid fault injection, arithmetic overflow, or matrix failure.
/// Data mismatches are returned in a report with `FAIL` status.
#[allow(clippy::too_many_lines)] // Keeping v1 stage order together makes audit review direct.
pub fn run_reference(
    source: &VerifiedSource,
    config: &RunConfig,
    golden: &GoldenVector,
    fault: Option<FaultInjection>,
) -> Result<RunReport, DitError> {
    validate_golden(source, config, golden)?;
    validate_fault(fault)?;

    let groups = build_groups(config)?;
    let block_count = groups
        .len()
        .checked_mul(MATRIX_WIDTH)
        .ok_or_else(|| DitError::InvalidConfig("block count overflow".to_owned()))?;
    validate_fault_location(fault, &groups, config.target_bytes)?;
    let logical_to_slot = scatter_permutation(block_count, config.seed);
    let mut slots: Vec<Vec<u8>> = (0..block_count).map(|_| Vec::new()).collect();
    let matrix = reference_matrix();
    let inverse_matrix = invert(&matrix)?;

    let mut report = initial_report(source, config, fault, &groups)?;
    let mut generated_hasher = Sha256::new();
    let mut encoded_hasher = Sha256::new();

    for group in &groups {
        let data = generate_group(source.bytes(), config.seed, group)?;
        update_valid_hash(&mut generated_hasher, &data, group.valid_bytes);
        let encoded = apply(&matrix, &data)?;
        for (shard, mut block) in encoded.into_iter().enumerate() {
            let logical_block = group.group_id * MATRIX_WIDTH + shard;
            if let Some((block_offset, expected, actual)) =
                inject_encode_fault(fault, logical_block, &mut block)
            {
                report.mismatch = Some(mismatch(
                    "MATRIX_ENCODE",
                    group,
                    logical_block,
                    shard,
                    block_offset,
                    expected,
                    actual,
                ));
            }
            encoded_hasher.update(&block);
            let slot = logical_to_slot[logical_block];
            report.coverage.written_unique_bytes = report
                .coverage
                .written_unique_bytes
                .saturating_add(u64::try_from(block.len()).unwrap_or(u64::MAX));
            report.coverage.total_write_bytes = report
                .coverage
                .total_write_bytes
                .saturating_add(u64::try_from(block.len()).unwrap_or(u64::MAX));
            slots[slot] = block;
        }
    }

    report.coverage.generated_bytes = config.target_bytes;
    report.coverage.allocated_unique_bytes = report.coverage.written_unique_bytes;
    report.coverage.touched_unique_bytes = report.coverage.written_unique_bytes;

    let generated_sha256 = hex::encode(generated_hasher.finalize());
    let encoded_sha256 = hex::encode(encoded_hasher.finalize());
    report.checkpoints.push(StageCheckpoint::new(
        "GENERATE",
        &golden.expected_generated_sha256,
        &generated_sha256,
    ));
    report.checkpoints.push(StageCheckpoint::new(
        "MATRIX_ENCODE",
        &golden.expected_encoded_sha256,
        &encoded_sha256,
    ));
    if !generated_sha256.eq_ignore_ascii_case(&golden.expected_generated_sha256) {
        report.fail("generated payload differs from independent golden vector");
        return Ok(report);
    }
    if !encoded_sha256.eq_ignore_ascii_case(&golden.expected_encoded_sha256) {
        report.fail("matrix encoded data differs from independent golden vector");
        return Ok(report);
    }

    inject_readback_fault(fault, &logical_to_slot, &mut slots)?;

    let mut readback_hasher = Sha256::new();
    for group in &groups {
        let data = generate_group(source.bytes(), config.seed, group)?;
        let expected_encoded = apply(&matrix, &data)?;
        for (shard, expected_block) in expected_encoded.iter().enumerate() {
            let logical_block = group.group_id * MATRIX_WIDTH + shard;
            let slot = logical_to_slot[logical_block];
            let actual_block = &slots[slot];
            if actual_block.len() != expected_block.len() {
                report.fail("readback block size changed");
                return Ok(report);
            }
            for (block_offset, (&expected, &actual)) in
                expected_block.iter().zip(actual_block).enumerate()
            {
                report.coverage.read_unique_bytes += 1;
                report.coverage.total_read_bytes += 1;
                if expected != actual {
                    report.mismatch = Some(mismatch(
                        "READBACK",
                        group,
                        logical_block,
                        shard,
                        block_offset,
                        expected,
                        actual,
                    ));
                    report.fail("exact readback mismatch");
                    return Ok(report);
                }
                report.coverage.exact_verified_unique_bytes += 1;
            }
            readback_hasher.update(actual_block);
        }
    }

    let readback_sha256 = hex::encode(readback_hasher.finalize());
    report.checkpoints.push(StageCheckpoint::new(
        "READBACK",
        &golden.expected_encoded_sha256,
        &readback_sha256,
    ));

    let mut recovered_hasher = Sha256::new();
    for group in &groups {
        let encoded: [Vec<u8>; MATRIX_WIDTH] = std::array::from_fn(|shard| {
            let logical_block = group.group_id * MATRIX_WIDTH + shard;
            slots[logical_to_slot[logical_block]].clone()
        });
        let recovered = apply(&inverse_matrix, &encoded)?;
        let mut remaining = group.valid_bytes;
        for (shard, block) in recovered.iter().enumerate() {
            let valid_in_block = remaining.min(group.block_size);
            for (block_offset, &stored_actual) in block[..valid_in_block].iter().enumerate() {
                let global_offset = group
                    .global_offset
                    .checked_add(
                        u64::try_from(shard * group.block_size + block_offset)
                            .map_err(|_| DitError::InvalidConfig("offset overflow".to_owned()))?,
                    )
                    .ok_or_else(|| DitError::InvalidConfig("offset overflow".to_owned()))?;
                let expected = expanded_byte(source.bytes(), config.seed, global_offset);
                let actual = apply_recovered_fault(fault, global_offset, stored_actual);
                if expected != actual {
                    report.mismatch = Some(MismatchEvidence {
                        stage: "RECOVER_JOIN".to_owned(),
                        group_id: u64::try_from(group.group_id).unwrap_or(u64::MAX),
                        logical_block: u64::try_from(group.group_id * MATRIX_WIDTH + shard)
                            .unwrap_or(u64::MAX),
                        block_offset: u64::try_from(block_offset).unwrap_or(u64::MAX),
                        global_payload_offset: Some(global_offset),
                        expected,
                        actual,
                        xor_syndrome: expected ^ actual,
                    });
                    report.fail("recovered/joined payload mismatch");
                    return Ok(report);
                }
                recovered_hasher.update([actual]);
                report.coverage.recovered_verified_bytes += 1;
            }
            remaining -= valid_in_block;
        }
    }

    let recovered_sha256_bytes = recovered_hasher.finalize();
    let recovered_sha256 = hex::encode(recovered_sha256_bytes);
    report.checkpoints.push(StageCheckpoint::new(
        "RECOVER_JOIN",
        &golden.expected_generated_sha256,
        &recovered_sha256,
    ));
    if !recovered_sha256.eq_ignore_ascii_case(&golden.expected_generated_sha256) {
        report.fail("recovered digest differs from independent golden vector");
        return Ok(report);
    }

    let final_sha256 = final_digest(
        config,
        &source.manifest().sha256,
        recovered_sha256_bytes.as_slice(),
        &hex::decode(&readback_sha256).map_err(|error| {
            DitError::InvalidConfig(format!("internal readback digest error: {error}"))
        })?,
    );
    report.checkpoints.push(StageCheckpoint::new(
        "FINAL",
        &golden.expected_final_sha256,
        &final_sha256,
    ));
    if !final_sha256.eq_ignore_ascii_case(&golden.expected_final_sha256) {
        report.fail("final digest differs from independent golden vector");
        return Ok(report);
    }

    report.coverage.coverage_target_met = report.coverage.generated_bytes
        == report.coverage.requested_payload_bytes
        && report.coverage.exact_verified_unique_bytes == report.coverage.allocated_unique_bytes
        && report.coverage.recovered_verified_bytes == report.coverage.requested_payload_bytes;
    if !report.coverage.coverage_target_met {
        report.fail("reference coverage target was not met");
        return Ok(report);
    }
    report.status = RunStatus::Pass;
    Ok(report)
}

fn validate_golden(
    source: &VerifiedSource,
    config: &RunConfig,
    golden: &GoldenVector,
) -> Result<(), DitError> {
    golden.validate_hashes()?;
    if !source
        .manifest()
        .sha256
        .eq_ignore_ascii_case(&golden.source_sha256)
    {
        return Err(DitError::InvalidConfig(
            "golden vector source SHA-256 does not match verified source".to_owned(),
        ));
    }
    let golden_config = golden.to_config()?;
    if config.test_id != golden_config.test_id
        || config.descriptor_version != golden_config.descriptor_version
        || config.target_bytes != golden_config.target_bytes
        || config.seed != golden_config.seed
        || config.block_sizes != golden_config.block_sizes
    {
        return Err(DitError::InvalidConfig(
            "runtime configuration does not match golden vector".to_owned(),
        ));
    }
    Ok(())
}

fn validate_fault(fault: Option<FaultInjection>) -> Result<(), DitError> {
    match fault {
        Some(
            FaultInjection::EncodeByte { xor_mask: 0, .. }
            | FaultInjection::ReadbackByte { xor_mask: 0, .. }
            | FaultInjection::RecoveredByte { xor_mask: 0, .. },
        ) => Err(DitError::InvalidConfig(
            "fault xor_mask must be non-zero".to_owned(),
        )),
        _ => Ok(()),
    }
}

fn validate_fault_location(
    fault: Option<FaultInjection>,
    groups: &[GroupDescriptor],
    target_bytes: u64,
) -> Result<(), DitError> {
    match fault {
        Some(
            FaultInjection::EncodeByte {
                logical_block,
                byte_offset,
                ..
            }
            | FaultInjection::ReadbackByte {
                logical_block,
                byte_offset,
                ..
            },
        ) => {
            let group = groups.get(logical_block / MATRIX_WIDTH).ok_or_else(|| {
                DitError::InvalidConfig(format!(
                    "fault logical block {logical_block} is out of range"
                ))
            })?;
            if byte_offset >= group.block_size {
                return Err(DitError::InvalidConfig(format!(
                    "fault byte offset {byte_offset} is out of range"
                )));
            }
            Ok(())
        }
        Some(FaultInjection::RecoveredByte { global_offset, .. })
            if global_offset >= target_bytes =>
        {
            Err(DitError::InvalidConfig(format!(
                "fault recovered offset {global_offset} is out of range"
            )))
        }
        _ => Ok(()),
    }
}

fn initial_report(
    source: &VerifiedSource,
    config: &RunConfig,
    fault: Option<FaultInjection>,
    groups: &[GroupDescriptor],
) -> Result<RunReport, DitError> {
    let allocated = groups.iter().try_fold(0_u64, |total, group| {
        let group_bytes = u64::try_from(group.block_size)
            .ok()
            .and_then(|size| size.checked_mul(u64::try_from(MATRIX_WIDTH).ok()?))
            .ok_or_else(|| DitError::InvalidConfig("allocated size overflow".to_owned()))?;
        total
            .checked_add(group_bytes)
            .ok_or_else(|| DitError::InvalidConfig("allocated size overflow".to_owned()))
    })?;

    Ok(RunReport {
        schema_version: 1,
        test_id: config.test_id.clone(),
        status: RunStatus::Fail,
        source: SourceEvidence {
            source_id: source.manifest().source_id.clone(),
            size_bytes: source.manifest().size_bytes,
            sha256: source.manifest().sha256.to_ascii_lowercase(),
        },
        config: ConfigEvidence {
            descriptor_version: config.descriptor_version,
            seed_hex: format!("0x{:016x}", config.seed),
            target_bytes: config.target_bytes,
            block_sizes: config.block_sizes.clone(),
            reference_memory_limit_bytes: MAX_REFERENCE_BYTES,
        },
        fault_injection: fault.map(FaultInjection::description),
        coverage: CoverageEvidence {
            requested_payload_bytes: config.target_bytes,
            allocated_unique_bytes: allocated,
            group_count: u64::try_from(groups.len()).unwrap_or(u64::MAX),
            block_count: u64::try_from(groups.len() * MATRIX_WIDTH).unwrap_or(u64::MAX),
            coverage_type: "reference_pipeline".to_owned(),
            ..CoverageEvidence::default()
        },
        checkpoints: Vec::new(),
        mismatch: None,
        failure_reason: None,
    })
}

fn build_groups(config: &RunConfig) -> Result<Vec<GroupDescriptor>, DitError> {
    let mut groups = Vec::new();
    let mut global_offset = 0_u64;
    let width = u64::try_from(MATRIX_WIDTH).expect("matrix width fits u64");
    while global_offset < config.target_bytes {
        let group_id = groups.len();
        let requested_block = config.block_sizes[group_id % config.block_sizes.len()];
        let remaining = config.target_bytes - global_offset;
        let full_group_bytes = requested_block
            .checked_mul(width)
            .ok_or_else(|| DitError::InvalidConfig("group size overflow".to_owned()))?;
        let block_size_u64 = if full_group_bytes > remaining {
            remaining.div_ceil(width)
        } else {
            requested_block
        };
        let valid_bytes_u64 = remaining.min(
            block_size_u64
                .checked_mul(width)
                .ok_or_else(|| DitError::InvalidConfig("group size overflow".to_owned()))?,
        );
        let block_size = usize::try_from(block_size_u64).map_err(|_| {
            DitError::InvalidConfig("block size does not fit platform usize".to_owned())
        })?;
        let valid_bytes = usize::try_from(valid_bytes_u64).map_err(|_| {
            DitError::InvalidConfig("valid byte count does not fit platform usize".to_owned())
        })?;
        groups.push(GroupDescriptor {
            group_id,
            global_offset,
            block_size,
            valid_bytes,
        });
        global_offset = global_offset
            .checked_add(valid_bytes_u64)
            .ok_or_else(|| DitError::InvalidConfig("global offset overflow".to_owned()))?;
    }
    Ok(groups)
}

fn generate_group(
    source: &[u8],
    seed: u64,
    group: &GroupDescriptor,
) -> Result<[Vec<u8>; MATRIX_WIDTH], DitError> {
    let mut blocks = std::array::from_fn(|_| vec![0_u8; group.block_size]);
    let mut remaining = group.valid_bytes;
    for (shard, block) in blocks.iter_mut().enumerate() {
        let valid_in_block = remaining.min(group.block_size);
        let shard_base = shard
            .checked_mul(group.block_size)
            .ok_or_else(|| DitError::InvalidConfig("shard offset overflow".to_owned()))?;
        for (offset, byte) in block[..valid_in_block].iter_mut().enumerate() {
            let local_offset = shard_base
                .checked_add(offset)
                .ok_or_else(|| DitError::InvalidConfig("local offset overflow".to_owned()))?;
            let global_offset = group
                .global_offset
                .checked_add(u64::try_from(local_offset).map_err(|_| {
                    DitError::InvalidConfig("global offset does not fit u64".to_owned())
                })?)
                .ok_or_else(|| DitError::InvalidConfig("global offset overflow".to_owned()))?;
            *byte = expanded_byte(source, seed, global_offset);
        }
        remaining -= valid_in_block;
    }
    Ok(blocks)
}

fn scatter_permutation(block_count: usize, seed: u64) -> Vec<usize> {
    let mut permutation: Vec<usize> = (0..block_count).collect();
    let mut state = seed ^ SCATTER_DOMAIN;
    for upper in (1..block_count).rev() {
        state = state.wrapping_add(expansion_multiplier());
        let random = splitmix64(state);
        let range = u64::try_from(upper + 1).expect("permutation range fits u64");
        let selected = usize::try_from(random % range).expect("selected slot fits usize");
        permutation.swap(upper, selected);
    }
    permutation
}

fn inject_readback_fault(
    fault: Option<FaultInjection>,
    logical_to_slot: &[usize],
    slots: &mut [Vec<u8>],
) -> Result<(), DitError> {
    let Some(FaultInjection::ReadbackByte {
        logical_block,
        byte_offset,
        xor_mask,
    }) = fault
    else {
        return Ok(());
    };
    let &slot = logical_to_slot.get(logical_block).ok_or_else(|| {
        DitError::InvalidConfig(format!(
            "fault logical block {logical_block} is out of range"
        ))
    })?;
    let block = &mut slots[slot];
    let byte = block.get_mut(byte_offset).ok_or_else(|| {
        DitError::InvalidConfig(format!("fault byte offset {byte_offset} is out of range"))
    })?;
    *byte ^= xor_mask;
    Ok(())
}

fn inject_encode_fault(
    fault: Option<FaultInjection>,
    logical_block: usize,
    block: &mut [u8],
) -> Option<(usize, u8, u8)> {
    let Some(FaultInjection::EncodeByte {
        logical_block: injected_block,
        byte_offset,
        xor_mask,
    }) = fault
    else {
        return None;
    };
    if injected_block != logical_block {
        return None;
    }
    let byte = &mut block[byte_offset];
    let expected = *byte;
    *byte ^= xor_mask;
    Some((byte_offset, expected, *byte))
}

fn apply_recovered_fault(fault: Option<FaultInjection>, global_offset: u64, actual: u8) -> u8 {
    match fault {
        Some(FaultInjection::RecoveredByte {
            global_offset: injected_offset,
            xor_mask,
        }) if injected_offset == global_offset => actual ^ xor_mask,
        _ => actual,
    }
}

fn update_valid_hash(hasher: &mut Sha256, blocks: &[Vec<u8>; MATRIX_WIDTH], mut remaining: usize) {
    for block in blocks {
        let valid = remaining.min(block.len());
        hasher.update(&block[..valid]);
        remaining -= valid;
    }
}

fn mismatch(
    stage: &str,
    group: &GroupDescriptor,
    logical_block: usize,
    shard: usize,
    block_offset: usize,
    expected: u8,
    actual: u8,
) -> MismatchEvidence {
    let local_offset = shard
        .checked_mul(group.block_size)
        .and_then(|base| base.checked_add(block_offset));
    let global_payload_offset = local_offset
        .filter(|&offset| offset < group.valid_bytes)
        .and_then(|offset| u64::try_from(offset).ok())
        .and_then(|offset| group.global_offset.checked_add(offset));
    MismatchEvidence {
        stage: stage.to_owned(),
        group_id: u64::try_from(group.group_id).unwrap_or(u64::MAX),
        logical_block: u64::try_from(logical_block).unwrap_or(u64::MAX),
        block_offset: u64::try_from(block_offset).unwrap_or(u64::MAX),
        global_payload_offset,
        expected,
        actual,
        xor_syndrome: expected ^ actual,
    }
}

fn final_digest(
    config: &RunConfig,
    source_sha256: &str,
    recovered_sha256: &[u8],
    readback_sha256: &[u8],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"CPU-DIT-01-FINAL-v1\0");
    hasher.update(config.test_id.as_bytes());
    hasher.update([0]);
    hasher.update(source_sha256.to_ascii_lowercase().as_bytes());
    hasher.update([0]);
    hasher.update(config.seed.to_le_bytes());
    hasher.update(config.target_bytes.to_le_bytes());
    hasher.update(recovered_sha256);
    hasher.update(readback_sha256);
    for block_size in &config.block_sizes {
        hasher.update(block_size.to_le_bytes());
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::{RunConfig, build_groups};

    #[test]
    fn tail_group_covers_every_requested_byte() {
        let config =
            RunConfig::new("test".to_owned(), 1, 1_000_003, 7, vec![65_536]).expect("valid config");
        let groups = build_groups(&config).expect("groups");
        let covered: usize = groups.iter().map(|group| group.valid_bytes).sum();
        assert_eq!(covered, 1_000_003);
        assert!(groups.last().expect("tail").valid_bytes > 0);
    }
}
