use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use nix::{
    sched::{CpuSet, sched_setaffinity},
    unistd::Pid,
};
use sha2::{Digest, Sha256};
use wide::u64x4;

use crate::{CpuInventory, SystemDitError};

const REDUCTION_INITIAL: [u64; 4] = [
    0x243f_6a88_85a3_08d3,
    0x1319_8a2e_0370_7344,
    0xa409_3822_299f_31d0,
    0x082e_fa98_ec4e_6c89,
];
const REDUCTION_ADD: [u64; 4] = [
    0x9e37_79b9_7f4a_7c15,
    0xbf58_476d_1ce4_e5b9,
    0x94d0_49bb_1331_11eb,
    0xd6e8_feb8_6659_fd93,
];
const INTEGER_KAT_EXPECTED: [u64; 4] = [
    0x2474_7a9a_b7f9_28bb,
    0x131b_8816_a518_6e4c,
    0xa4f5_2e1a_8907_3e28,
    0x0836_c599_fb4e_73c1,
];
const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CoherencyResult {
    pub workers: usize,
    pub iterations_per_worker: u64,
    pub expected: u64,
    pub actual: u64,
    pub affinity_failures: u64,
}

pub(crate) fn select_worker_cpus(
    inventory: &CpuInventory,
    requested: usize,
) -> Result<Vec<usize>, SystemDitError> {
    if inventory.allowed_cpu_ids.is_empty() {
        return Err(SystemDitError::InvalidConfig(
            "no CPUs are available in the current cpuset".to_owned(),
        ));
    }
    let count = if requested == 0 {
        inventory.allowed_cpu_ids.len()
    } else {
        requested.min(inventory.allowed_cpu_ids.len())
    };
    Ok(inventory.allowed_cpu_ids[..count].to_vec())
}

pub(crate) fn pin_current_thread(cpu_id: usize) -> Result<(), String> {
    let mut cpu_set = CpuSet::new();
    cpu_set
        .set(cpu_id)
        .map_err(|error| format!("cannot build CPU set for CPU {cpu_id}: {error}"))?;
    sched_setaffinity(Pid::from_raw(0), &cpu_set)
        .map_err(|error| format!("cannot pin current thread to CPU {cpu_id}: {error}"))
}

#[must_use]
pub(crate) fn scalar_reduction(bytes: &[u8]) -> [u64; 4] {
    let mut reduction = ReductionPair::new();
    reduction.update(bytes);
    reduction.scalar
}

#[must_use]
pub(crate) fn simd_reduction(bytes: &[u8]) -> [u64; 4] {
    let mut reduction = ReductionPair::new();
    reduction.update(bytes);
    reduction.simd
}

/// The scalar and the SIMD reduction of one input, fed a block at a time (the RAM verify feeds
/// each block right after comparing it, while it is still in cache). The result is the same as
/// [`scalar_reduction`] and [`simd_reduction`] over the whole input, provided every block but
/// the last is a multiple of 32 bytes: words keep their lane (word index modulo 4).
pub(crate) struct ReductionPair {
    pub(crate) scalar: [u64; 4],
    pub(crate) simd: [u64; 4],
    words: usize,
}

impl ReductionPair {
    pub(crate) const fn new() -> Self {
        Self {
            scalar: REDUCTION_INITIAL,
            simd: REDUCTION_INITIAL,
            words: 0,
        }
    }

    pub(crate) fn update(&mut self, bytes: &[u8]) {
        debug_assert!(
            self.words.is_multiple_of(4),
            "blocks before the last are 32-byte multiples"
        );
        Self::scalar_step(&mut self.scalar, self.words, bytes);
        Self::simd_step(&mut self.simd, self.words, bytes);
        self.words += bytes.len() / 8;
    }

    fn scalar_step(accumulator: &mut [u64; 4], words_before: usize, bytes: &[u8]) {
        let (chunks, remainder) = bytes.as_chunks::<8>();
        for (index, chunk) in chunks.iter().enumerate() {
            let word = u64::from_le_bytes(*chunk);
            let lane = (words_before + index) % 4;
            accumulator[lane] = (accumulator[lane] ^ word).wrapping_add(REDUCTION_ADD[lane]);
        }
        if !remainder.is_empty() {
            let mut tail = [0_u8; 8];
            tail[..remainder.len()].copy_from_slice(remainder);
            let word = u64::from_le_bytes(tail);
            let lane = (words_before + chunks.len()) % 4;
            accumulator[lane] = (accumulator[lane] ^ word).wrapping_add(REDUCTION_ADD[lane]);
        }
    }

    fn simd_step(result: &mut [u64; 4], words_before: usize, bytes: &[u8]) {
        let mut accumulator = u64x4::new(*result);
        let addition = u64x4::new(REDUCTION_ADD);
        let (chunks, remainder) = bytes.as_chunks::<32>();
        for chunk in chunks {
            let words = std::array::from_fn(|lane| {
                let start = lane * 8;
                u64::from_le_bytes(
                    chunk[start..start + 8]
                        .try_into()
                        .expect("lane has 8 bytes"),
                )
            });
            accumulator = (accumulator ^ u64x4::new(words)) + addition;
        }
        *result = accumulator.to_array();
        let processed_words = words_before + chunks.len() * 4;
        let (tail_chunks, tail_remainder) = remainder.as_chunks::<8>();
        for (tail_index, chunk) in tail_chunks.iter().enumerate() {
            let lane = (processed_words + tail_index) % 4;
            let word = u64::from_le_bytes(*chunk);
            result[lane] = (result[lane] ^ word).wrapping_add(REDUCTION_ADD[lane]);
        }
        if !tail_remainder.is_empty() {
            let mut tail = [0_u8; 8];
            tail[..tail_remainder.len()].copy_from_slice(tail_remainder);
            let lane = (processed_words + tail_chunks.len()) % 4;
            result[lane] =
                (result[lane] ^ u64::from_le_bytes(tail)).wrapping_add(REDUCTION_ADD[lane]);
        }
    }
}

#[must_use]
pub(crate) fn integer_known_answer_test() -> bool {
    let input: Vec<u8> = (u8::MIN..=u8::MAX).collect();
    scalar_reduction(&input) == INTEGER_KAT_EXPECTED
        && simd_reduction(&input) == INTEGER_KAT_EXPECTED
}

#[must_use]
pub(crate) fn sha256_known_answer_test() -> bool {
    hex::encode(Sha256::digest(b"abc")) == SHA256_ABC
}

#[must_use]
pub(crate) fn fp_fma_known_answer_test() -> bool {
    (0_u32..100_000).all(|value| {
        let input = f64::from(value);
        input.mul_add(2.0, 1.0).to_bits() == (input * 2.0 + 1.0).to_bits()
    })
}

pub(crate) fn coherency_test(
    cpu_ids: &[usize],
    iterations_per_worker: u64,
) -> Result<CoherencyResult, SystemDitError> {
    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::with_capacity(cpu_ids.len());
    for (worker_id, &cpu_id) in cpu_ids.iter().enumerate() {
        let counter = Arc::clone(&counter);
        handles.push((
            worker_id,
            thread::spawn(move || {
                let affinity_failed = pin_current_thread(cpu_id).is_err();
                for _ in 0..iterations_per_worker {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                affinity_failed
            }),
        ));
    }
    let mut affinity_failures = 0_u64;
    for (worker_id, handle) in handles {
        let failed = handle
            .join()
            .map_err(|_| SystemDitError::WorkerPanic { worker_id })?;
        affinity_failures += u64::from(failed);
    }
    let workers = cpu_ids.len();
    let expected = iterations_per_worker
        .checked_mul(u64::try_from(workers).map_err(|_| {
            SystemDitError::InvalidConfig("coherency worker count overflow".to_owned())
        })?)
        .ok_or_else(|| SystemDitError::InvalidConfig("coherency count overflow".to_owned()))?;
    Ok(CoherencyResult {
        workers,
        iterations_per_worker,
        expected,
        actual: counter.load(Ordering::SeqCst),
        affinity_failures,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        REDUCTION_ADD, REDUCTION_INITIAL, ReductionPair, integer_known_answer_test,
        scalar_reduction, sha256_known_answer_test, simd_reduction,
    };

    /// The reductions as they were written before v1.2 (one pass over the whole input): the
    /// block-wise ones must give the same values.
    fn legacy_scalar(bytes: &[u8]) -> [u64; 4] {
        let mut accumulator = REDUCTION_INITIAL;
        let (chunks, remainder) = bytes.as_chunks::<8>();
        for (index, chunk) in chunks.iter().enumerate() {
            let lane = index % 4;
            accumulator[lane] =
                (accumulator[lane] ^ u64::from_le_bytes(*chunk)).wrapping_add(REDUCTION_ADD[lane]);
        }
        if !remainder.is_empty() {
            let mut tail = [0_u8; 8];
            tail[..remainder.len()].copy_from_slice(remainder);
            let lane = (bytes.len() / 8) % 4;
            accumulator[lane] =
                (accumulator[lane] ^ u64::from_le_bytes(tail)).wrapping_add(REDUCTION_ADD[lane]);
        }
        accumulator
    }

    #[test]
    fn block_wise_reductions_equal_the_one_pass_ones() {
        assert!(integer_known_answer_test());
        let bytes: Vec<u8> = (0_u32..300_007)
            .map(|value| u8::try_from((value * 131 + value / 7) % 256).expect("byte"))
            .collect();
        for length in [0, 1, 7, 8, 31, 32, 33, 4093, 65_536, 65_537, 300_007] {
            let input = &bytes[..length];
            let legacy = legacy_scalar(input);
            assert_eq!(scalar_reduction(input), legacy, "scalar, {length} bytes");
            assert_eq!(simd_reduction(input), legacy, "simd, {length} bytes");
            for block in [32, 96, 4096, 65_536] {
                let mut pair = ReductionPair::new();
                for piece in input.chunks(block) {
                    pair.update(piece);
                }
                assert_eq!(pair.scalar, legacy, "scalar, {length} bytes by {block}");
                assert_eq!(pair.simd, legacy, "simd, {length} bytes by {block}");
            }
        }
    }

    #[test]
    fn scalar_and_simd_reductions_match_for_tails() {
        let bytes: Vec<u8> = (0_u16..1000)
            .map(|value| u8::try_from(value % 256).expect("modulo fits u8"))
            .collect();
        for length in 0..bytes.len() {
            assert_eq!(
                scalar_reduction(&bytes[..length]),
                simd_reduction(&bytes[..length])
            );
        }
    }

    #[test]
    fn sha256_known_answer_passes() {
        assert!(sha256_known_answer_test());
    }
}
