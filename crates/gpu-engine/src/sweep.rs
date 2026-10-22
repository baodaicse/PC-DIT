//! GPU-DRAM sweep schedule: the GPU-local pattern written and checked on the device at DRAM
//! bandwidth over a whole zone set, pass after pass.
//!
//! A pass writes every span of every zone with the pass key, then reads every span back and
//! compares on the device. Writing the whole zone set before reading any of it keeps the reads
//! coming from DRAM rather than from a cache that still holds what was just written (zone sets
//! are far larger than any L2). Each pass uses a new key, so every bit is expected to change
//! state with probability 1/2 between passes. The CPU checks the device's work by sampling:
//! between passes, a span is read back over `PCIe` and compared with the pattern regenerated on
//! the host ([`sweep_pattern_into`]).
//!
//! Pure schedule, no device calls: the CUDA driver side lives in `gpu-cuda`.
use crate::lane::{Zone, vram_key};

/// Largest span one kernel launch covers.
pub const DEFAULT_SPAN_BYTES: u64 = 256 * 1024 * 1024;
/// Bytes read back for one CPU checkpoint.
pub const CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
/// Every span starts and ends on this boundary (the kernels move 16-byte vectors).
pub const SWEEP_ALIGN: u64 = 4096;

/// Keeps sweep keys apart from the transfer lane's GPU-local keys for the same seed.
const SWEEP_KEY_SALT: u64 = 0x5357_4545_5044_4954;

/// The sweep's pattern word: `sweep_word` in `gemm.cu` computes the same. Xor of the index with
/// the key, then xorshift32 -- no multiply, so the device spends its time on memory rather than
/// on the pattern. For a fixed key it is a bijection of the low 32 index bits, so every word of
/// an allocation below 16 GiB is distinct (address faults cannot pass unseen), and a new key
/// changes about half of every word's bits.
#[must_use]
pub fn sweep_word(index: u64, key: u32) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    let (low, high) = (index as u32, (index >> 32) as u32);
    let mut x = low ^ key ^ high.rotate_left(16);
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// Fills `out` with the sweep pattern for the words starting at `first_word`.
pub fn sweep_pattern_into(out: &mut [u8], key: u32, first_word: u64) {
    for (index, chunk) in out.chunks_mut(4).enumerate() {
        let word = sweep_word(first_word + index as u64, key).to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
}

/// v1.2 C10: the reversible mix of a compute pass, on one 16-byte block (four words). Every
/// step is invertible (add, xor, rotate, multiply by an odd constant), so [`unmix_block`] gives
/// the block back exactly; a single wrong bit anywhere (the ALU while mixing, a flipped bit at
/// rest in VRAM, the ALU while unmixing) spreads over the block and the result differs from the
/// pattern. `mix_block` and `unmix_block` in `gemm.cu` compute the same.
pub const MIX_MULTIPLIER: u32 = 0x2545_F491;
/// The inverse of [`MIX_MULTIPLIER`] modulo 2^32.
pub const MIX_MULTIPLIER_INVERSE: u32 = inverse_odd(MIX_MULTIPLIER);
/// Integer operations one round does on a block (4 add/sub, 5 xor, 3 rotate, 1 multiply, the
/// round key aside): the unit of `device_verified_compute_ops`.
pub const MIX_OPS_PER_ROUND: u64 = 12;
/// Rounds of a compute pass when nothing else is configured. Measured on the GTX 960
/// (2026-10-22): a forward kernel reads and writes, 28.8 ms per GiB as memory traffic alone;
/// 16 rounds take exactly that (ALU hidden), 32 take 34.5 ms (ALU and DRAM both near their
/// limit), 64 take 66.5 ms (ALU-bound, DRAM idling).
pub const DEFAULT_MIX_ROUNDS: u32 = 32;

/// Multiplicative inverse of an odd `a` modulo 2^32 (Newton's iteration doubles the correct
/// low bits each step, from 3).
#[must_use]
pub const fn inverse_odd(a: u32) -> u32 {
    let mut x = a;
    let mut step = 0;
    while step < 5 {
        x = x.wrapping_mul(2_u32.wrapping_sub(a.wrapping_mul(x)));
        step += 1;
    }
    x
}

#[allow(clippy::cast_possible_truncation)]
const fn round_key(key: u32, round: u32, block: u64) -> u32 {
    key ^ round.wrapping_mul(0x9E37_79B9)
        ^ (block as u32).wrapping_mul(0x85EB_CA6B)
        ^ ((block >> 32) as u32)
}

/// `rounds` rounds of the mix on the block `s` at block index `block` (word index / 4).
#[must_use]
#[allow(clippy::many_single_char_names)] // a, b, c, d: the block's four words, as in gemm.cu.
pub const fn mix_block(s: [u32; 4], key: u32, block: u64, rounds: u32) -> [u32; 4] {
    let [mut a, mut b, mut c, mut d] = s;
    let mut round = 0;
    while round < rounds {
        let k = round_key(key, round, block);
        a = a.wrapping_add(b ^ k);
        d ^= a;
        d = d.rotate_left(16);
        c = c.wrapping_add(d);
        b ^= c;
        b = b.rotate_left(12);
        a = a.wrapping_mul(MIX_MULTIPLIER);
        c ^= a.rotate_left(7);
        round += 1;
    }
    [a, b, c, d]
}

/// The inverse of [`mix_block`]: its steps undone in reverse order, the rounds last to first.
#[must_use]
#[allow(clippy::many_single_char_names)] // a, b, c, d: the block's four words, as in gemm.cu.
pub const fn unmix_block(s: [u32; 4], key: u32, block: u64, rounds: u32) -> [u32; 4] {
    let [mut a, mut b, mut c, mut d] = s;
    let mut round = rounds;
    while round > 0 {
        round -= 1;
        let k = round_key(key, round, block);
        c ^= a.rotate_left(7);
        a = a.wrapping_mul(MIX_MULTIPLIER_INVERSE);
        b = b.rotate_right(12);
        b ^= c;
        c = c.wrapping_sub(d);
        d = d.rotate_right(16);
        d ^= a;
        a = a.wrapping_sub(b ^ k);
    }
    [a, b, c, d]
}

/// The pattern of a compute pass after its forward kernel: the sweep pattern of the words from
/// `first_word` (a multiple of 4), mixed block by block. What a CPU checkpoint expects.
pub fn mixed_pattern_into(out: &mut [u8], key: u32, first_word: u64, rounds: u32) {
    for (index, chunk) in out.chunks_mut(16).enumerate() {
        let word = first_word + 4 * index as u64;
        let block = [
            sweep_word(word, key),
            sweep_word(word + 1, key),
            sweep_word(word + 2, key),
            sweep_word(word + 3, key),
        ];
        let mixed = mix_block(block, key, word / 4, rounds);
        for (dst, value) in chunk.chunks_mut(4).zip(mixed) {
            dst.copy_from_slice(&value.to_le_bytes()[..dst.len()]);
        }
    }
}

/// The key of sweep pass `pass`.
#[must_use]
pub fn sweep_key(seed: u64, pass: u64) -> u32 {
    vram_key(seed ^ SWEEP_KEY_SALT, pass)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepPhase {
    Fill,
    /// v1.2 C10: a compute pass mixes every block in place between fill and verify.
    Forward,
    /// Read back and compare on the device (in a compute pass, unmixed first).
    Verify,
}

/// One kernel launch: `bytes` at `offset` of `allocation`, with `key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepStep {
    pub phase: SweepPhase,
    pub pass: u64,
    pub key: u32,
    /// Mix rounds of the pass: 0 for a plain DRAM pass.
    pub rounds: u32,
    pub allocation: usize,
    pub offset: u64,
    pub bytes: u64,
}

impl SweepStep {
    /// Word index of the first word within its allocation, as the pattern is keyed.
    #[must_use]
    pub fn first_word(&self) -> u64 {
        self.offset / 4
    }
}

/// A CPU checkpoint at a pass boundary: the span to read back and what it must hold (the
/// pattern of `key`, mixed by `rounds` when the last pass was a compute pass).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub allocation: usize,
    pub offset: u64,
    pub bytes: u64,
    pub key: u32,
    pub rounds: u32,
}

#[derive(Clone, Debug)]
pub struct SweepPlan {
    zones: Vec<Zone>,
    span_bytes: u64,
    seed: u64,
    /// v1.2 C10: odd passes are compute passes of this many mix rounds (0: plain passes only).
    rounds: u32,
    pass: u64,
    phase: SweepPhase,
    zone: usize,
    cursor: u64,
    passes_completed: u64,
    checkpoint_cursor: (usize, u64),
}

impl SweepPlan {
    /// # Errors
    /// Rejects an empty zone set, zones off the alignment, or a span below the alignment.
    pub fn new(
        zones: Vec<Zone>,
        span_bytes: u64,
        seed: u64,
        rounds: u32,
    ) -> Result<Self, &'static str> {
        let zones: Vec<Zone> = zones.into_iter().filter(|z| z.len > 0).collect();
        if zones.is_empty() {
            return Err("sweep needs at least one zone");
        }
        if span_bytes < SWEEP_ALIGN || !span_bytes.is_multiple_of(SWEEP_ALIGN) {
            return Err("sweep span must be a positive multiple of 4 KiB");
        }
        if zones
            .iter()
            .any(|z| !z.start.is_multiple_of(SWEEP_ALIGN) || !z.len.is_multiple_of(SWEEP_ALIGN))
        {
            return Err("sweep zones must be 4 KiB aligned");
        }
        Ok(Self {
            zones,
            span_bytes,
            seed,
            rounds,
            pass: 0,
            phase: SweepPhase::Fill,
            zone: 0,
            cursor: 0,
            passes_completed: 0,
            checkpoint_cursor: (0, 0),
        })
    }

    #[must_use]
    pub fn zone_bytes(&self) -> u64 {
        self.zones.iter().map(|z| z.len).sum()
    }

    /// Passes whose verify phase has fully been issued.
    #[must_use]
    pub fn passes_completed(&self) -> u64 {
        self.passes_completed
    }

    /// True when the next step starts a new pass: the previous pass is fully written and read,
    /// so VRAM holds `last_pass_key` everywhere. Checkpoints and zone swaps happen here.
    #[must_use]
    pub fn at_pass_boundary(&self) -> bool {
        self.phase == SweepPhase::Fill && self.zone == 0 && self.cursor == 0
    }

    /// The key VRAM holds at a pass boundary (after at least one pass).
    #[must_use]
    pub fn last_pass_key(&self) -> Option<u32> {
        (self.passes_completed > 0).then(|| sweep_key(self.seed, self.pass - 1))
    }

    /// Mix rounds of pass `pass`: compute passes are the odd ones.
    #[must_use]
    pub fn rounds_of(&self, pass: u64) -> u32 {
        if pass % 2 == 1 { self.rounds } else { 0 }
    }

    /// The next kernel launch, walking fill over every zone, then verify over every zone.
    pub fn next_step(&mut self) -> SweepStep {
        let zone = self.zones[self.zone];
        let bytes = self.span_bytes.min(zone.len - self.cursor);
        let rounds = self.rounds_of(self.pass);
        let step = SweepStep {
            phase: self.phase,
            pass: self.pass,
            key: sweep_key(self.seed, self.pass),
            rounds,
            allocation: zone.allocation,
            offset: zone.start + self.cursor,
            bytes,
        };
        self.cursor += bytes;
        if self.cursor >= zone.len {
            self.cursor = 0;
            self.zone += 1;
            if self.zone == self.zones.len() {
                self.zone = 0;
                match self.phase {
                    SweepPhase::Fill if rounds > 0 => self.phase = SweepPhase::Forward,
                    SweepPhase::Fill | SweepPhase::Forward => self.phase = SweepPhase::Verify,
                    SweepPhase::Verify => {
                        self.phase = SweepPhase::Fill;
                        self.pass += 1;
                        self.passes_completed += 1;
                    }
                }
            }
        }
        step
    }

    /// The next checkpoint span at a pass boundary, rotating through every zone so repeated
    /// checkpoints eventually sample all of it. After a compute pass VRAM holds the mixed
    /// pattern (the device unmixes into registers to compare, it never writes back).
    pub fn next_checkpoint(&mut self) -> Option<Checkpoint> {
        let key = self.last_pass_key()?;
        let rounds = self.rounds_of(self.pass - 1);
        let (index, offset) = self.checkpoint_cursor;
        let zone = self.zones[index % self.zones.len()];
        let offset = offset.min(zone.len.saturating_sub(SWEEP_ALIGN));
        let bytes = CHECKPOINT_BYTES.min(zone.len - offset);
        let next = offset + bytes;
        self.checkpoint_cursor = if next >= zone.len {
            ((index + 1) % self.zones.len(), 0)
        } else {
            (index, next)
        };
        Some(Checkpoint {
            allocation: zone.allocation,
            offset: zone.start + offset,
            bytes,
            key,
            rounds,
        })
    }
}

/// Duty cycle: after a launch kept the GPU `busy_seconds`, the next may start this much later,
/// so the sweep holds at most `duty` of the device's time (1.0 = back to back).
#[must_use]
pub fn idle_after(busy_seconds: f64, duty: f64) -> f64 {
    if duty.is_nan() || duty <= 0.0 || duty >= 1.0 || !busy_seconds.is_finite() {
        return 0.0;
    }
    busy_seconds.max(0.0) * (1.0 / duty - 1.0)
}

/// Splits each allocation into two halves at a 4 KiB boundary: the transfer lane works one half
/// while the sweep works the other, and they trade places so both cover the whole pool.
#[must_use]
pub fn halves(sizes: &[u64]) -> [Vec<Zone>; 2] {
    let mut out = [Vec::new(), Vec::new()];
    for (allocation, len) in sizes.iter().enumerate() {
        let len = len / SWEEP_ALIGN * SWEEP_ALIGN;
        let mid = len / 2 / SWEEP_ALIGN * SWEEP_ALIGN;
        if mid == 0 || len - mid == 0 {
            continue;
        }
        out[0].push(Zone {
            allocation,
            start: 0,
            len: mid,
        });
        out[1].push(Zone {
            allocation,
            start: mid,
            len: len - mid,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn zone(allocation: usize, start: u64, len: u64) -> Zone {
        Zone {
            allocation,
            start,
            len,
        }
    }

    #[test]
    fn a_pass_fills_every_zone_before_verifying_any_and_then_changes_key() {
        let mut plan = SweepPlan::new(
            vec![zone(0, 0, 3 * MIB), zone(1, 4 * MIB, MIB)],
            2 * MIB,
            7,
            0,
        )
        .unwrap();
        assert!(plan.at_pass_boundary() && plan.last_pass_key().is_none());
        let steps: Vec<SweepStep> = (0..6).map(|_| plan.next_step()).collect();
        let shape: Vec<(SweepPhase, usize, u64, u64)> = steps
            .iter()
            .map(|s| (s.phase, s.allocation, s.offset, s.bytes))
            .collect();
        assert_eq!(
            shape,
            vec![
                (SweepPhase::Fill, 0, 0, 2 * MIB),
                (SweepPhase::Fill, 0, 2 * MIB, MIB),
                (SweepPhase::Fill, 1, 4 * MIB, MIB),
                (SweepPhase::Verify, 0, 0, 2 * MIB),
                (SweepPhase::Verify, 0, 2 * MIB, MIB),
                (SweepPhase::Verify, 1, 4 * MIB, MIB),
            ]
        );
        assert!(steps.iter().all(|s| s.key == steps[0].key && s.pass == 0));
        assert!(plan.at_pass_boundary());
        assert_eq!(plan.passes_completed(), 1);
        assert_eq!(plan.last_pass_key(), Some(steps[0].key));
        let next = plan.next_step();
        assert_eq!((next.pass, next.phase), (1, SweepPhase::Fill));
        assert_ne!(next.key, steps[0].key, "each pass writes different data");
        assert_eq!(
            steps[2].first_word(),
            4 * MIB / 4,
            "pattern is keyed by allocation word"
        );
    }

    #[test]
    fn checkpoints_rotate_through_every_zone_with_the_key_vram_holds() {
        let mut plan = SweepPlan::new(
            vec![zone(0, 0, 32 * MIB), zone(1, 0, 8 * MIB)],
            16 * MIB,
            3,
            0,
        )
        .unwrap();
        assert!(plan.next_checkpoint().is_none(), "nothing written yet");
        for _ in 0..6 {
            plan.next_step();
        }
        let key = plan.last_pass_key().unwrap();
        let seen: Vec<(usize, u64, u64)> = (0..4)
            .map(|_| {
                let c = plan.next_checkpoint().unwrap();
                assert_eq!((c.key, c.rounds), (key, 0));
                (c.allocation, c.offset, c.bytes)
            })
            .collect();
        assert_eq!(
            seen,
            vec![
                (0, 0, CHECKPOINT_BYTES),
                (0, CHECKPOINT_BYTES, CHECKPOINT_BYTES),
                (1, 0, 8 * MIB),
                (0, 0, CHECKPOINT_BYTES),
            ]
        );
    }

    #[test]
    fn sweep_words_are_distinct_per_address_and_change_with_the_key() {
        let key = sweep_key(1, 0);
        let mut seen = std::collections::HashSet::new();
        for index in 0..(1 << 16) {
            assert!(
                seen.insert(sweep_word(index, key)),
                "address {index} repeats a word"
            );
        }
        let other = sweep_key(1, 1);
        let flipped: u32 = (0..4096)
            .map(|i| (sweep_word(i, key) ^ sweep_word(i, other)).count_ones())
            .sum();
        let share = f64::from(flipped) / (4096.0 * 32.0);
        assert!(
            (0.4..0.6).contains(&share),
            "a new pass flips {share:.2} of the bits"
        );
        let mut bytes = [0_u8; 12];
        sweep_pattern_into(&mut bytes, key, 7);
        assert_eq!(bytes[4..8], sweep_word(8, key).to_le_bytes());
    }

    #[test]
    fn duty_cycle_and_halves() {
        assert!((idle_after(0.010, 0.25) - 0.030).abs() < 1e-12);
        assert!(idle_after(0.010, 1.0).abs() < f64::EPSILON);
        assert!(idle_after(f64::NAN, 0.5).abs() < f64::EPSILON);
        assert!(
            idle_after(0.010, 0.0).abs() < f64::EPSILON,
            "invalid duty is not a sleep"
        );
        assert!(idle_after(0.010, f64::NAN).abs() < f64::EPSILON);
        let [a, b] = halves(&[10 * MIB + 123, 4096]);
        assert_eq!(a, vec![zone(0, 0, 5 * MIB)]);
        assert_eq!(b, vec![zone(0, 5 * MIB, 5 * MIB)]);
        assert!(SweepPlan::new(vec![zone(0, 100, MIB)], MIB, 0, 0).is_err());
        assert!(SweepPlan::new(vec![], MIB, 0, 0).is_err());
    }

    #[test]
    fn the_mix_is_undone_exactly_and_one_bit_spreads_over_the_block() {
        assert_eq!(MIX_MULTIPLIER.wrapping_mul(MIX_MULTIPLIER_INVERSE), 1);
        let block = [0x1234_5678, 0x9ABC_DEF0, 0x0F1E_2D3C, 0x4B5A_6978];
        for rounds in [1, 7, DEFAULT_MIX_ROUNDS] {
            let mixed = mix_block(block, 0xC0FF_EE11, 99, rounds);
            assert_ne!(mixed, block);
            assert_eq!(unmix_block(mixed, 0xC0FF_EE11, 99, rounds), block);
            // A wrong key or block index while unmixing does not give the block back.
            assert_ne!(unmix_block(mixed, 0xC0FF_EE10, 99, rounds), block);
            assert_ne!(unmix_block(mixed, 0xC0FF_EE11, 98, rounds), block);
        }
        // One flipped bit at rest, between mix and unmix, changes every word of the block.
        let mixed = mix_block(block, 5, 3, DEFAULT_MIX_ROUNDS);
        for word in 0..4 {
            for bit in [0, 13, 31] {
                let mut bad = mixed;
                bad[word] ^= 1 << bit;
                let back = unmix_block(bad, 5, 3, DEFAULT_MIX_ROUNDS);
                assert!(
                    back.iter().zip(block).all(|(x, y)| *x != y),
                    "word {word} bit {bit}: {back:x?}"
                );
            }
        }
    }

    #[test]
    fn compute_passes_alternate_with_plain_ones_and_checkpoints_expect_the_mixed_pattern() {
        let mut plan = SweepPlan::new(vec![zone(0, 0, 2 * MIB)], 2 * MIB, 4, 16).unwrap();
        let phases: Vec<(u64, SweepPhase, u32)> = (0..5)
            .map(|_| {
                let s = plan.next_step();
                (s.pass, s.phase, s.rounds)
            })
            .collect();
        assert_eq!(
            phases,
            vec![
                (0, SweepPhase::Fill, 0),
                (0, SweepPhase::Verify, 0),
                (1, SweepPhase::Fill, 16),
                (1, SweepPhase::Forward, 16),
                (1, SweepPhase::Verify, 16),
            ]
        );
        assert!(plan.at_pass_boundary());
        let checkpoint = plan.next_checkpoint().unwrap();
        assert_eq!(checkpoint.rounds, 16, "VRAM holds the mixed pattern");
        // The expected bytes are the pattern mixed block by block, starting on a block.
        let mut mixed = vec![0_u8; 64];
        mixed_pattern_into(&mut mixed, checkpoint.key, 8, 16);
        let block = [8_u64, 9, 10, 11].map(|w| sweep_word(w, checkpoint.key));
        let expected = mix_block(block, checkpoint.key, 2, 16);
        assert_eq!(mixed[..4], expected[0].to_le_bytes());
        assert_eq!(mixed[12..16], expected[3].to_le_bytes());
        // Without rounds every pass is plain, as before v1.2 C10.
        let mut no_compute = SweepPlan::new(vec![zone(0, 0, 2 * MIB)], 2 * MIB, 4, 0).unwrap();
        assert!((0..8).all(|_| no_compute.next_step().phase != SweepPhase::Forward));
    }
}
