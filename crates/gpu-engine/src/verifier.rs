//! GPU-H2 low-copy verification helpers shared by every backend: transfer
//! pattern generation into a caller buffer (optionally on a few threads), a
//! memcmp-first difference search, and the hash policy.
//!
//! Nothing here weakens a verdict: a hash is evidence, never a substitute for
//! the full byte-for-byte or word-for-word comparison.
use serde::{Serialize, Serializer};

/// Bytes above which pattern generation uses helper threads.
const PARALLEL_PATTERN_BYTES: usize = 4 * 1024 * 1024;

/// Helper threads for CPU-side preparation. Kept small so the verifier pool,
/// UI and watchdog keep headroom; callers may pass their own budget.
#[must_use]
pub fn default_helper_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| (n.get() / 4).clamp(1, 4))
}

/// When to compute the SHA-256 of an *actual* output.
///
/// When a comparison passes, the actual bytes equal the expected bytes, so the
/// hash only records evidence; skipping it never lets a wrong output pass.
/// Failing outputs are always hashed by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashPolicy {
    EveryBatch,
    /// Hash every `interval`-th ordinal, plus whatever the caller marks as last.
    Checkpoint {
        interval: u64,
    },
}

impl HashPolicy {
    pub const DEFAULT_INTERVAL: u64 = 64;

    #[must_use]
    pub fn should_hash(self, ordinal: u64) -> bool {
        match self {
            Self::EveryBatch => true,
            Self::Checkpoint { interval } => interval != 0 && ordinal.is_multiple_of(interval),
        }
    }

    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::EveryBatch => "every_batch".to_owned(),
            Self::Checkpoint { interval } => format!("checkpoint:{interval}"),
        }
    }

    /// Accepts `every-batch`, `checkpoint` and `checkpoint:<n>`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "every-batch" | "every_batch" => Some(Self::EveryBatch),
            "checkpoint" => Some(Self::Checkpoint {
                interval: Self::DEFAULT_INTERVAL,
            }),
            other => {
                let n = other.strip_prefix("checkpoint:")?.parse::<u64>().ok()?;
                (n > 0).then_some(Self::Checkpoint { interval: n })
            }
        }
    }
}

impl Serialize for HashPolicy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.label())
    }
}

/// Version of the transfer/retention pattern schedule. Version 1 was pure pseudo-random;
/// version 2 replaces every eighth generation with a systematic pattern. Replay
/// descriptors carry this version so old reports keep their meaning.
pub const TRANSFER_PATTERN_VERSION: u32 = 2;

/// Data pattern of one transfer generation. Random data has ~50% toggle activity but
/// does not guarantee the classic worst cases, so a fixed schedule adds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum PatternClass {
    Random,
    Zeros,
    Ones,
    Checkerboard,
    /// Bitwise inverse of `Checkerboard`, so every cell sees both polarities.
    CheckerboardInverse,
    /// One set bit per 64-bit word that advances with the word address and generation.
    WalkingBit,
    /// Each 64-bit word holds its own word address.
    AddressInAddress,
}

impl PatternClass {
    pub const ALL: [Self; 7] = [
        Self::Random,
        Self::Zeros,
        Self::Ones,
        Self::Checkerboard,
        Self::CheckerboardInverse,
        Self::WalkingBit,
        Self::AddressInAddress,
    ];
    const SYSTEMATIC: [Self; 6] = [
        Self::Zeros,
        Self::Ones,
        Self::Checkerboard,
        Self::CheckerboardInverse,
        Self::WalkingBit,
        Self::AddressInAddress,
    ];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Zeros => "zeros",
            Self::Ones => "ones",
            Self::Checkerboard => "checkerboard",
            Self::CheckerboardInverse => "checkerboard_inverse",
            Self::WalkingBit => "walking_bit",
            Self::AddressInAddress => "address_in_address",
        }
    }

    #[must_use]
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }
}

/// Deterministic schedule: every eighth generation is systematic, cycling through the six
/// systematic classes; the rest are random. Retention re-reads use the original
/// generation, so they see the same class as the write.
#[must_use]
pub fn pattern_class(generation: u64) -> PatternClass {
    if generation % 8 == 7 {
        let slot = usize::try_from((generation / 8) % 6).unwrap_or(0);
        PatternClass::SYSTEMATIC[slot]
    } else {
        PatternClass::Random
    }
}

#[derive(Clone, Copy)]
enum Fill {
    Random { key: u64 },
    Const(u8),
    Walking { generation: u64 },
    Address,
}

#[inline]
fn fill_random(out: &mut [u8], first_counter: u64, key: u64) {
    let mut counter = first_counter;
    let (words, tail) = out.as_chunks_mut::<8>();
    for chunk in words {
        *chunk = splitmix(key ^ counter.wrapping_mul(0xD134_2543_DE82_EF95)).to_le_bytes();
        counter = counter.wrapping_add(1);
    }
    if !tail.is_empty() {
        let value = splitmix(key ^ counter.wrapping_mul(0xD134_2543_DE82_EF95)).to_le_bytes();
        tail.copy_from_slice(&value[..tail.len()]);
    }
}

#[inline]
fn fill_counted(out: &mut [u8], first_counter: u64, word: impl Fn(u64) -> u64) {
    let mut counter = first_counter;
    let (words, tail) = out.as_chunks_mut::<8>();
    for chunk in words {
        *chunk = word(counter).to_le_bytes();
        counter = counter.wrapping_add(1);
    }
    if !tail.is_empty() {
        tail.copy_from_slice(&word(counter).to_le_bytes()[..tail.len()]);
    }
}

fn fill_part(out: &mut [u8], first_counter: u64, fill: Fill) {
    match fill {
        Fill::Random { key } => fill_random(out, first_counter, key),
        Fill::Const(byte) => out.fill(byte),
        Fill::Walking { generation } => fill_counted(out, first_counter, |c| {
            1_u64 << (c.wrapping_add(generation) % 64)
        }),
        Fill::Address => fill_counted(out, first_counter, |c| c),
    }
}

#[inline]
fn splitmix(value: u64) -> u64 {
    super::splitmix64(value)
}

/// Writes the replayable transfer pattern (version 2) for `logical_offset` into `out`.
/// Byte-identical to [`crate::transfer_pattern`]; `threads > 1` splits the buffer on
/// 8-byte boundaries (every class is a pure function of the word counter).
pub fn transfer_pattern_into(
    out: &mut [u8],
    seed: u64,
    generation: u64,
    logical_offset: u64,
    threads: usize,
) {
    let block_index = logical_offset / 4096;
    let base = logical_offset / 8;
    let fill = match pattern_class(generation) {
        PatternClass::Random => Fill::Random {
            key: seed ^ generation.rotate_left(23) ^ block_index.rotate_left(41),
        },
        PatternClass::Zeros => Fill::Const(0x00),
        PatternClass::Ones => Fill::Const(0xFF),
        PatternClass::Checkerboard => Fill::Const(0x55),
        PatternClass::CheckerboardInverse => Fill::Const(0xAA),
        PatternClass::WalkingBit => Fill::Walking { generation },
        PatternClass::AddressInAddress => Fill::Address,
    };
    let threads = if out.len() >= PARALLEL_PATTERN_BYTES {
        threads.max(1)
    } else {
        1
    };
    if threads == 1 {
        fill_part(out, base, fill);
        return;
    }
    let words = out.len().div_ceil(8);
    let per = words.div_ceil(threads) * 8;
    std::thread::scope(|scope| {
        for (index, part) in out.chunks_mut(per).enumerate() {
            let first = base.wrapping_add((index * per / 8) as u64);
            scope.spawn(move || fill_part(part, first, fill));
        }
    });
}

/// Index of the first differing byte, or `None` when the slices are identical
/// (including length). Compares in 4 KiB slices so the common all-equal case
/// runs at memory-compare speed and the slow byte scan only touches one slice.
#[must_use]
pub fn first_difference(expected: &[u8], actual: &[u8]) -> Option<usize> {
    const SLICE: usize = 4096;
    let common = expected.len().min(actual.len());
    let mut start = 0;
    while start < common {
        let end = (start + SLICE).min(common);
        if expected[start..end] != actual[start..end] {
            let offset = expected[start..end]
                .iter()
                .zip(&actual[start..end])
                .position(|(a, b)| a != b)?;
            return Some(start + offset);
        }
        start = end;
    }
    (expected.len() != actual.len()).then_some(common)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_pattern(seed: u64, generation: u64, offset: u64, bytes: usize) -> Vec<u8> {
        // The original algorithm, kept verbatim as the differential oracle.
        let block_index = offset / 4096;
        let mut output = vec![0_u8; bytes];
        for (word_index, chunk) in output.chunks_mut(8).enumerate() {
            let counter = offset / 8 + u64::try_from(word_index).unwrap();
            let value = splitmix(
                seed ^ generation.rotate_left(23)
                    ^ block_index.rotate_left(41)
                    ^ counter.wrapping_mul(0xD134_2543_DE82_EF95),
            )
            .to_le_bytes();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
        output
    }

    #[test]
    fn random_generations_match_the_original_algorithm_for_every_thread_count_and_odd_size() {
        for &(seed, generation, offset, bytes) in &[
            (1_u64, 2_u64, 0_u64, 4096_usize),
            (42, 6, 4096 * 5 + 3, 8 * 1024 * 1024 + 5),
            (u64::MAX, 1 << 40, 8, 4 * 1024 * 1024),
            (9, 9, 12_345, 7),
            (3, 3, 0, 1),
        ] {
            assert_eq!(pattern_class(generation), PatternClass::Random);
            let want = reference_pattern(seed, generation, offset, bytes);
            for threads in [1, 2, 3, 4, 7] {
                let mut got = vec![0xEE_u8; bytes];
                transfer_pattern_into(&mut got, seed, generation, offset, threads);
                assert!(got == want, "threads={threads} bytes={bytes}");
            }
        }
    }

    #[test]
    fn first_difference_finds_every_position_and_length_mismatch() {
        let a: Vec<u8> = (0..20_000_u32)
            .map(|i| u8::try_from(i * 7 % 256).unwrap())
            .collect();
        assert_eq!(first_difference(&a, &a), None);
        for position in [0, 1, 4095, 4096, 4097, 8191, 19_999] {
            let mut b = a.clone();
            b[position] ^= 0x80;
            assert_eq!(first_difference(&a, &b), Some(position));
            // Only the first of several differences is reported.
            b[19_999] ^= 1;
            assert_eq!(first_difference(&a, &b), Some(position.min(19_999)));
        }
        assert_eq!(first_difference(&a, &a[..19_000]), Some(19_000));
        assert_eq!(first_difference(&a[..10], &a), Some(10));
        assert_eq!(first_difference(&[], &[]), None);
    }

    #[test]
    fn hash_policy_parses_and_selects_checkpoints_only() {
        assert_eq!(
            HashPolicy::parse("every-batch"),
            Some(HashPolicy::EveryBatch)
        );
        assert_eq!(
            HashPolicy::parse("checkpoint"),
            Some(HashPolicy::Checkpoint { interval: 64 })
        );
        assert_eq!(
            HashPolicy::parse("checkpoint:8"),
            Some(HashPolicy::Checkpoint { interval: 8 })
        );
        for bad in ["", "checkpoint:0", "checkpoint:x", "never", "checkpoint:-1"] {
            assert_eq!(HashPolicy::parse(bad), None, "{bad}");
        }
        let policy = HashPolicy::Checkpoint { interval: 4 };
        let hashed: Vec<u64> = (0..12).filter(|n| policy.should_hash(*n)).collect();
        assert_eq!(hashed, [0, 4, 8]);
        assert!((0..100).all(|n| HashPolicy::EveryBatch.should_hash(n)));
        assert!(!HashPolicy::Checkpoint { interval: 0 }.should_hash(0));
        assert_eq!(serde_json::to_string(&policy).unwrap(), "\"checkpoint:4\"");
    }

    #[test]
    fn schedule_is_deterministic_covers_every_class_and_keeps_most_generations_random() {
        assert_eq!(pattern_class(6), PatternClass::Random);
        assert_eq!(pattern_class(7), PatternClass::Zeros);
        assert_eq!(pattern_class(15), PatternClass::Ones);
        let mut counts = [0_usize; 7];
        for g in 0..480 {
            counts[pattern_class(g).index()] += 1;
            assert_eq!(pattern_class(g), pattern_class(g));
        }
        assert_eq!(
            counts[0], 420,
            "seven of every eight generations stay random"
        );
        assert!(counts[1..].iter().all(|&n| n == 10), "{counts:?}");
        let labels: std::collections::BTreeSet<_> =
            PatternClass::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(labels.len(), 7);
        // Checkerboard and its inverse alternate so every cell sees both polarities.
        let (a, b) = (pattern_class(8 * 2 + 7), pattern_class(8 * 3 + 7));
        assert_eq!(
            (a, b),
            (
                PatternClass::Checkerboard,
                PatternClass::CheckerboardInverse
            )
        );
    }

    #[test]
    fn systematic_classes_have_the_documented_content_for_every_thread_count() {
        let bytes = 4 * 1024 * 1024 + 11;
        let offset = 4096 * 3;
        for (generation, check) in [
            (7_u64, 0_usize),
            (15, 1),
            (23, 2),
            (31, 3),
            (39, 4),
            (47, 5),
        ] {
            let mut serial = vec![0_u8; bytes];
            transfer_pattern_into(&mut serial, 5, generation, offset, 1);
            for threads in [2, 3, 5] {
                let mut par = vec![0_u8; bytes];
                transfer_pattern_into(&mut par, 5, generation, offset, threads);
                assert!(par == serial, "class {check} threads {threads}");
            }
            let words = serial.as_chunks::<8>().0;
            let base = offset / 8;
            match check {
                0 => assert!(serial.iter().all(|&b| b == 0x00)),
                1 => assert!(serial.iter().all(|&b| b == 0xFF)),
                2 => assert!(serial.iter().all(|&b| b == 0x55)),
                3 => assert!(serial.iter().all(|&b| b == 0xAA)),
                4 => {
                    for (i, w) in words.iter().enumerate().take(200) {
                        let want = 1_u64 << ((base + i as u64 + generation) % 64);
                        assert_eq!(u64::from_le_bytes(*w), want);
                    }
                    assert!(
                        words
                            .iter()
                            .all(|w| u64::from_le_bytes(*w).is_power_of_two())
                    );
                }
                _ => {
                    for i in [0_usize, 1, 999, words.len() - 1] {
                        assert_eq!(u64::from_le_bytes(words[i]), base + i as u64);
                    }
                }
            }
        }
    }

    #[test]
    fn every_class_is_caught_when_a_single_bit_is_wrong() {
        for generation in [6_u64, 7, 15, 23, 31, 39, 47] {
            let mut expected = vec![0_u8; 8192];
            transfer_pattern_into(&mut expected, 1, generation, 0, 1);
            for position in [0, 4095, 4096, 8191] {
                let mut actual = expected.clone();
                actual[position] ^= 0x10;
                assert_eq!(first_difference(&expected, &actual), Some(position));
            }
        }
    }
}
