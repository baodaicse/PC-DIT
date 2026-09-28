//! Deterministic GPU-A data contract and CPU mock backend.
//!
//! This crate validates the format, split/scatter/join pipeline and independent
//! CPU oracle. It deliberately does not claim to access or validate VRAM.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Instant;
use thiserror::Error;

pub mod accounting;
pub mod balanced;
pub mod bottleneck;
pub mod control;
pub mod gemm;
pub mod lane;
pub mod plan;
pub mod pressure;
pub mod queue;
pub mod schedule;
pub mod sweep;
pub mod verifier;

pub const MODULE_ID: &str = "dit.vga";
pub const PIPELINE_VERSION: &str = "GPU-A-1";
pub const BLOCK_FORMAT: &str = "DITGPUA1";
pub const BLOCK_BYTES: usize = 4096;
const HEADER_BYTES: usize = 48;
const DIGEST_BYTES: usize = 32;
const PAYLOAD_END: usize = BLOCK_BYTES - DIGEST_BYTES;
const PAYLOAD_BYTES: usize = PAYLOAD_END - HEADER_BYTES;
pub const MATRIX_DIM: usize = 8;
pub const MATRIX_WORDS: usize = MATRIX_DIM * MATRIX_DIM;
pub const MATRIX_INPUT_WORDS: usize = MATRIX_WORDS * 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixWorkload {
    /// Per job: 64 row-major words for A followed by 64 row-major words for B.
    pub input: Vec<u32>,
    /// Per job: 64 row-major wrapping-u32 results for A x B.
    pub expected: Vec<u32>,
    pub jobs: usize,
}

/// Builds deterministic matrix input and the independent CPU oracle for GPU-C.
///
/// # Errors
///
/// Returns [`GpuAError::InvalidBlockCount`] when `jobs` is outside the locked
/// contract range.
pub fn matrix_workload(
    seed: u64,
    generation: u64,
    jobs: usize,
) -> Result<MatrixWorkload, GpuAError> {
    matrix_workload_profiled(seed, generation, jobs).map(|(workload, _)| workload)
}

/// Host preparation times, measured separately from GPU execution.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct MatrixPreparationTiming {
    pub input_generation_seconds: f64,
    pub cpu_oracle_seconds: f64,
}

/// Builds the same locked workload as [`matrix_workload`], with disjoint host timers.
///
/// # Errors
/// Returns [`GpuAError::InvalidBlockCount`] for an out-of-range job count.
pub fn matrix_workload_profiled(
    seed: u64,
    generation: u64,
    jobs: usize,
) -> Result<(MatrixWorkload, MatrixPreparationTiming), GpuAError> {
    if !(1..=65_536).contains(&jobs) {
        return Err(GpuAError::InvalidBlockCount);
    }
    let started = Instant::now();
    let mut input = Vec::with_capacity(jobs * MATRIX_INPUT_WORDS);
    for index in 0..jobs {
        let block = make_block(WorkDescriptor {
            seed,
            generation,
            block_index: u64::try_from(index).unwrap_or(u64::MAX),
        });
        let (left, right) = matrices(&block);
        input.extend_from_slice(&left);
        input.extend_from_slice(&right);
    }
    let input_generation_seconds = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let mut expected = Vec::with_capacity(jobs * MATRIX_WORDS);
    for matrices in input.as_chunks::<MATRIX_INPUT_WORDS>().0 {
        let left = std::array::from_fn(|index| matrices[index]);
        let right = std::array::from_fn(|index| matrices[MATRIX_WORDS + index]);
        expected.extend_from_slice(&matrix_oracle(&left, &right));
    }
    let cpu_oracle_seconds = started.elapsed().as_secs_f64();
    Ok((
        MatrixWorkload {
            input,
            expected,
            jobs,
        },
        MatrixPreparationTiming {
            input_generation_seconds,
            cpu_oracle_seconds,
        },
    ))
}

/// Generates replayable transfer bytes without relying on a stored payload.
#[must_use]
pub fn transfer_pattern(seed: u64, generation: u64, logical_offset: u64, bytes: usize) -> Vec<u8> {
    let mut output = vec![0_u8; bytes];
    verifier::transfer_pattern_into(
        &mut output,
        seed,
        generation,
        logical_offset,
        verifier::default_helper_threads(),
    );
    output
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkDescriptor {
    pub seed: u64,
    pub generation: u64,
    pub block_index: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    PayloadBitFlip,
    WrongPattern,
    StaleGeneration,
    WrongBlockIndex,
    MissingBlock,
    DuplicateBlock,
    MatrixOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationEvidence {
    pub pipeline_version: String,
    pub block_format: String,
    pub seed: u64,
    pub generation: u64,
    pub blocks_verified: u64,
    pub bytes_verified: u64,
    pub matrix_results_verified: u64,
    pub result_sha256: String,
    pub hardware_tested: bool,
    pub backend: String,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GpuAError {
    #[error("block count must be between 1 and 65536")]
    InvalidBlockCount,
    #[error("block {block}: digest mismatch")]
    DigestMismatch { block: u64 },
    #[error("block {block}: header mismatch in {field}")]
    HeaderMismatch { block: u64, field: &'static str },
    #[error("block {block}: payload mismatch at byte {byte}")]
    PayloadMismatch { block: u64, byte: usize },
    #[error("duplicate block index {block}")]
    DuplicateBlock { block: u64 },
    #[error("missing block index {block}")]
    MissingBlock { block: u64 },
    #[error("matrix mismatch in block {block}, cell {cell}")]
    MatrixMismatch { block: u64, cell: usize },
}

#[derive(Clone)]
struct Block {
    bytes: Vec<u8>,
}

/// Runs one deterministic GPU-A generation through a CPU-backed mock transport.
///
/// # Errors
///
/// Returns a checkpoint-specific error when the serialized data, join order or
/// matrix output differs from the independent expectation.
///
/// # Panics
///
/// Panics only if an internal fixed-size block/layout invariant is broken by a
/// future code change. Caller-provided block counts and fault modes are checked.
pub fn run_generation(
    seed: u64,
    generation: u64,
    block_count: usize,
    fault: Option<FaultKind>,
) -> Result<GenerationEvidence, GpuAError> {
    if !(1..=65_536).contains(&block_count) {
        return Err(GpuAError::InvalidBlockCount);
    }
    let mut blocks = (0..block_count)
        .map(|index| {
            make_block(WorkDescriptor {
                seed,
                generation,
                block_index: u64::try_from(index).expect("block index fits u64"),
            })
        })
        .collect::<Vec<_>>();

    apply_fault(&mut blocks, seed, generation, block_count, fault)?;

    let order = permutation(blocks.len(), seed ^ generation.rotate_left(17));
    let transported = order
        .into_iter()
        .map(|index| blocks[index].clone())
        .collect::<Vec<_>>();
    let mut joined = vec![None; block_count];
    for block in transported {
        let descriptor = read_descriptor(&block)?;
        let index = usize::try_from(descriptor.block_index).unwrap_or(usize::MAX);
        if index >= block_count {
            return Err(GpuAError::HeaderMismatch {
                block: descriptor.block_index,
                field: "block_index",
            });
        }
        if joined[index].replace(block).is_some() {
            return Err(GpuAError::DuplicateBlock {
                block: descriptor.block_index,
            });
        }
    }

    let mut result_digest = Sha256::new();
    for (index, slot) in joined.into_iter().enumerate() {
        let block_index = u64::try_from(index).expect("block index fits u64");
        let block = slot.ok_or(GpuAError::MissingBlock { block: block_index })?;
        let expected = WorkDescriptor {
            seed,
            generation,
            block_index,
        };
        verify_block(&block, expected)?;
        let (left, right) = matrices(&block);
        let mut actual = matrix_kernel(&left, &right);
        if fault == Some(FaultKind::MatrixOutput) && index == 0 {
            actual[0] ^= 1;
        }
        let expected_matrix = matrix_oracle(&left, &right);
        for (cell, (&actual, &expected)) in actual.iter().zip(&expected_matrix).enumerate() {
            if actual != expected {
                return Err(GpuAError::MatrixMismatch {
                    block: block_index,
                    cell,
                });
            }
            result_digest.update(actual.to_le_bytes());
        }
    }

    Ok(GenerationEvidence {
        pipeline_version: PIPELINE_VERSION.to_owned(),
        block_format: BLOCK_FORMAT.to_owned(),
        seed,
        generation,
        blocks_verified: u64::try_from(block_count).expect("block count fits u64"),
        bytes_verified: u64::try_from(block_count * BLOCK_BYTES).expect("byte count fits u64"),
        matrix_results_verified: u64::try_from(block_count * MATRIX_WORDS)
            .expect("matrix result count fits u64"),
        result_sha256: format!("{:x}", result_digest.finalize()),
        hardware_tested: false,
        backend: "CPU_MOCK_GPU_A".to_owned(),
    })
}

fn apply_fault(
    blocks: &mut Vec<Block>,
    seed: u64,
    generation: u64,
    block_count: usize,
    fault: Option<FaultKind>,
) -> Result<(), GpuAError> {
    match fault {
        Some(FaultKind::PayloadBitFlip) => blocks[0].bytes[HEADER_BYTES] ^= 1,
        Some(FaultKind::WrongPattern) => {
            blocks[0].bytes[HEADER_BYTES] ^= 1;
            seal_block(&mut blocks[0]);
        }
        Some(FaultKind::StaleGeneration) => {
            blocks[0] = make_block(WorkDescriptor {
                seed,
                generation: generation.wrapping_sub(1),
                block_index: 0,
            });
        }
        Some(FaultKind::WrongBlockIndex) => {
            blocks[0].bytes[32..40]
                .copy_from_slice(&u64::try_from(block_count).unwrap_or(u64::MAX).to_le_bytes());
            seal_block(&mut blocks[0]);
        }
        Some(FaultKind::MissingBlock) => {
            blocks.pop();
        }
        Some(FaultKind::DuplicateBlock) if block_count > 1 => {
            blocks[block_count - 1] = blocks[0].clone();
        }
        Some(FaultKind::DuplicateBlock) => {
            return Err(GpuAError::DuplicateBlock { block: 0 });
        }
        _ => {}
    }
    Ok(())
}

fn make_block(descriptor: WorkDescriptor) -> Block {
    let mut bytes = vec![0_u8; BLOCK_BYTES];
    bytes[..8].copy_from_slice(BLOCK_FORMAT.as_bytes());
    bytes[8..12].copy_from_slice(&1_u32.to_le_bytes());
    bytes[16..24].copy_from_slice(&descriptor.seed.to_le_bytes());
    bytes[24..32].copy_from_slice(&descriptor.generation.to_le_bytes());
    bytes[32..40].copy_from_slice(&descriptor.block_index.to_le_bytes());
    bytes[40..44].copy_from_slice(
        &u32::try_from(PAYLOAD_BYTES)
            .expect("payload length fits u32")
            .to_le_bytes(),
    );
    fill_expected_payload(descriptor, &mut bytes[HEADER_BYTES..PAYLOAD_END]);
    let mut block = Block { bytes };
    seal_block(&mut block);
    block
}

fn seal_block(block: &mut Block) {
    let digest = Sha256::digest(&block.bytes[..PAYLOAD_END]);
    block.bytes[PAYLOAD_END..].copy_from_slice(&digest);
}

fn read_descriptor(block: &Block) -> Result<WorkDescriptor, GpuAError> {
    let block_hint = read_u64(&block.bytes, 32);
    if block.bytes.len() != BLOCK_BYTES || block.bytes[..8] != *BLOCK_FORMAT.as_bytes() {
        return Err(GpuAError::HeaderMismatch {
            block: block_hint,
            field: "magic/length",
        });
    }
    if read_u32(&block.bytes, 8) != 1 {
        return Err(GpuAError::HeaderMismatch {
            block: block_hint,
            field: "version",
        });
    }
    if usize::try_from(read_u32(&block.bytes, 40)).ok() != Some(PAYLOAD_BYTES) {
        return Err(GpuAError::HeaderMismatch {
            block: block_hint,
            field: "payload_length",
        });
    }
    let expected_digest = Sha256::digest(&block.bytes[..PAYLOAD_END]);
    if block.bytes[PAYLOAD_END..] != expected_digest[..] {
        return Err(GpuAError::DigestMismatch { block: block_hint });
    }
    Ok(WorkDescriptor {
        seed: read_u64(&block.bytes, 16),
        generation: read_u64(&block.bytes, 24),
        block_index: block_hint,
    })
}

fn verify_block(block: &Block, expected: WorkDescriptor) -> Result<(), GpuAError> {
    let actual = read_descriptor(block)?;
    for (field, matches) in [
        ("seed", actual.seed == expected.seed),
        ("generation", actual.generation == expected.generation),
        ("block_index", actual.block_index == expected.block_index),
    ] {
        if !matches {
            return Err(GpuAError::HeaderMismatch {
                block: expected.block_index,
                field,
            });
        }
    }
    let mut expected_payload = vec![0_u8; PAYLOAD_BYTES];
    fill_expected_payload(expected, &mut expected_payload);
    if let Some(byte) = block.bytes[HEADER_BYTES..PAYLOAD_END]
        .iter()
        .zip(expected_payload)
        .position(|(actual, expected)| *actual != expected)
    {
        return Err(GpuAError::PayloadMismatch {
            block: expected.block_index,
            byte,
        });
    }
    Ok(())
}

fn fill_expected_payload(descriptor: WorkDescriptor, output: &mut [u8]) {
    for (word_index, chunk) in output.chunks_mut(8).enumerate() {
        let counter = u64::try_from(word_index).expect("payload word index fits u64");
        let value = splitmix64(
            descriptor.seed
                ^ descriptor.generation.rotate_left(23)
                ^ descriptor.block_index.rotate_left(41)
                ^ counter.wrapping_mul(0xD134_2543_DE82_EF95),
        )
        .to_le_bytes();
        chunk.copy_from_slice(&value[..chunk.len()]);
    }
}

fn permutation(count: usize, seed: u64) -> Vec<usize> {
    let mut values = (0..count).collect::<Vec<_>>();
    let mut state = seed;
    for upper in (1..count).rev() {
        state = splitmix64(state);
        let index = usize::try_from(state % u64::try_from(upper + 1).expect("upper fits u64"))
            .expect("permutation index fits usize");
        values.swap(upper, index);
    }
    values
}

fn matrices(block: &Block) -> ([u32; MATRIX_WORDS], [u32; MATRIX_WORDS]) {
    let mut left = [0_u32; MATRIX_WORDS];
    let mut right = [0_u32; MATRIX_WORDS];
    let payload = &block.bytes[HEADER_BYTES..PAYLOAD_END];
    for (index, word) in left.iter_mut().enumerate() {
        *word = read_u32(payload, index * 4);
    }
    for (index, word) in right.iter_mut().enumerate() {
        *word = read_u32(payload, (MATRIX_WORDS + index) * 4);
    }
    (left, right)
}

fn matrix_kernel(left: &[u32; MATRIX_WORDS], right: &[u32; MATRIX_WORDS]) -> [u32; MATRIX_WORDS] {
    let mut output = [0_u32; MATRIX_WORDS];
    for row in 0..MATRIX_DIM {
        for column in 0..MATRIX_DIM {
            let mut sum = 0_u32;
            for inner in 0..MATRIX_DIM {
                sum = sum.wrapping_add(
                    left[row * MATRIX_DIM + inner].wrapping_mul(right[inner * MATRIX_DIM + column]),
                );
            }
            output[row * MATRIX_DIM + column] = sum;
        }
    }
    output
}

fn matrix_oracle(left: &[u32; MATRIX_WORDS], right: &[u32; MATRIX_WORDS]) -> [u32; MATRIX_WORDS] {
    std::array::from_fn(|cell| {
        let row = cell / MATRIX_DIM;
        let column = cell % MATRIX_DIM;
        (0..MATRIX_DIM).fold(0_u32, |total, inner| {
            total.wrapping_add(
                right[column + inner * MATRIX_DIM].wrapping_mul(left[inner + row * MATRIX_DIM]),
            )
        })
    })
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed u32"))
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("fixed u64"))
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_is_deterministic_and_cpu_mock_is_explicit() {
        let first = run_generation(0x1234, 7, 8, None).unwrap();
        let second = run_generation(0x1234, 7, 8, None).unwrap();
        assert_eq!(first, second);
        assert!(!first.hardware_tested);
        assert_eq!(first.backend, "CPU_MOCK_GPU_A");
    }

    #[test]
    fn faults_are_detected_at_distinct_checkpoints() {
        assert!(matches!(
            run_generation(1, 2, 4, Some(FaultKind::PayloadBitFlip)),
            Err(GpuAError::DigestMismatch { .. })
        ));
        assert!(matches!(
            run_generation(1, 2, 4, Some(FaultKind::WrongPattern)),
            Err(GpuAError::PayloadMismatch { .. })
        ));
        assert!(matches!(
            run_generation(1, 2, 4, Some(FaultKind::StaleGeneration)),
            Err(GpuAError::HeaderMismatch {
                field: "generation",
                ..
            })
        ));
        assert!(matches!(
            run_generation(1, 2, 4, Some(FaultKind::WrongBlockIndex)),
            Err(GpuAError::HeaderMismatch {
                field: "block_index",
                ..
            })
        ));
        assert!(matches!(
            run_generation(1, 2, 4, Some(FaultKind::MissingBlock)),
            Err(GpuAError::MissingBlock { .. })
        ));
        assert!(matches!(
            run_generation(1, 2, 4, Some(FaultKind::DuplicateBlock)),
            Err(GpuAError::DuplicateBlock { .. })
        ));
        assert!(matches!(
            run_generation(1, 2, 1, Some(FaultKind::DuplicateBlock)),
            Err(GpuAError::DuplicateBlock { .. })
        ));
        assert!(matches!(
            run_generation(1, 2, 4, Some(FaultKind::MatrixOutput)),
            Err(GpuAError::MatrixMismatch { .. })
        ));
    }

    #[test]
    fn rejects_empty_generation() {
        assert_eq!(
            run_generation(1, 1, 0, None),
            Err(GpuAError::InvalidBlockCount)
        );
    }

    #[test]
    fn golden_generation_digest_is_locked() {
        let evidence = run_generation(0x0123_4567_89AB_CDEF, 3, 4, None).unwrap();
        assert_eq!(
            evidence.result_sha256,
            "947c732fe0f088959e81d0b84b7a21238a186156758c11f1e13c249b77c679ae"
        );
    }

    #[test]
    fn exported_matrix_workload_is_stable() {
        let workload = matrix_workload(0x1234, 9, 3).unwrap();
        assert_eq!(workload.input.len(), 3 * MATRIX_INPUT_WORDS);
        assert_eq!(workload.expected.len(), 3 * MATRIX_WORDS);
        assert_eq!(workload, matrix_workload(0x1234, 9, 3).unwrap());
        let (profiled, timing) = matrix_workload_profiled(0x1234, 9, 3).unwrap();
        assert_eq!(workload, profiled);
        assert!(timing.input_generation_seconds >= 0.0);
        assert!(timing.cpu_oracle_seconds >= 0.0);
        assert!(matrix_workload_profiled(1, 0, 0).is_err());
        assert!(matrix_workload_profiled(1, 0, 65_537).is_err());
    }

    #[test]
    fn transfer_pattern_uses_seed_generation_and_offset() {
        let base = transfer_pattern(1, 2, 0, 4096);
        assert_eq!(base, transfer_pattern(1, 2, 0, 4096));
        assert_ne!(base, transfer_pattern(2, 2, 0, 4096));
        assert_ne!(base, transfer_pattern(1, 3, 0, 4096));
        assert_ne!(base, transfer_pattern(1, 2, 4096, 4096));
    }
}
