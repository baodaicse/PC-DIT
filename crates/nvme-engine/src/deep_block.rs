//! Versioned integer-matrix wire blocks. No v1 generator is used as an oracle.
use dit_source::{BLOCK_SIZE, BlockContext};
use sha2::{Digest, Sha256};

pub const DIM: usize = 16;
const WORDS: usize = DIM * DIM;
const HEADER: usize = 128;
const DATA_END: usize = HEADER + WORDS * 3 * 4;
type Matrix = [u32; WORDS];

fn header(context: &BlockContext, rounds: u32) -> [u8; HEADER] {
    let mut result = [0; HEADER];
    result[..8].copy_from_slice(b"DITDEEP2");
    result[8..12].copy_from_slice(&2_u32.to_le_bytes());
    result[12..16].copy_from_slice(&rounds.to_le_bytes());
    for (offset, value) in [
        (16, context.session_seed),
        (24, context.generation),
        (64, context.lba),
        (72, context.stripe_id),
        (80, u64::from(context.stripe_index)),
    ] {
        result[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }
    result[32..64].copy_from_slice(&Sha256::digest(context.device_id.as_bytes()));
    result[88..92].copy_from_slice(&16_u32.to_le_bytes());
    result
}

fn key(context: &BlockContext, rounds: u32) -> u32 {
    let digest = Sha256::digest(header(context, rounds));
    u32::from_le_bytes(digest[..4].try_into().expect("four bytes"))
}

fn inputs_fast(seed: u32) -> (Matrix, Matrix) {
    let mut state = seed;
    let mut a = [0; WORDS];
    let mut b = [0; WORDS];
    for word in a.iter_mut().chain(b.iter_mut()) {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *word = state;
    }
    (a, b)
}

fn inputs_reference(seed: u32) -> (Matrix, Matrix) {
    let mut state = u64::from(seed);
    let mut words = [0; WORDS * 2];
    for value in &mut words {
        state = (state * 1_664_525 + 1_013_904_223) % (1_u64 << 32);
        *value = u32::try_from(state).expect("reduced to 32 bits");
    }
    (
        words[..WORDS].try_into().expect("matrix A"),
        words[WORDS..].try_into().expect("matrix B"),
    )
}

/// Cache-friendly i-k-j multiplication with wrapping-u32 arithmetic.
fn optimized(a: &Matrix, b: &Matrix, rounds: u32) -> Matrix {
    let mut current = *a;
    for round in 0..rounds {
        let mut next = [0_u32; WORDS];
        for i in 0..DIM {
            for k in 0..DIM {
                let lhs = current[i * DIM + k];
                for j in 0..DIM {
                    let index = i * DIM + j;
                    next[index] = next[index].wrapping_add(lhs.wrapping_mul(b[k * DIM + j]));
                }
            }
        }
        for (index, value) in next.iter_mut().enumerate() {
            *value ^= a[(index + round as usize) % WORDS].rotate_left(round % 32);
        }
        current = next;
    }
    current
}

/// Separate i-j-k dot products accumulated without overflow in u128.
#[allow(clippy::manual_rotate)] // Deliberately differs from the optimized kernel's rotate helper.
fn reference(a: &Matrix, b: &Matrix, rounds: u32) -> Matrix {
    let mut source = *a;
    for pass in 0..rounds {
        let mut destination = [0_u32; WORDS];
        for row in 0..DIM {
            for column in 0..DIM {
                let sum: u128 = (0..DIM)
                    .map(|inner| {
                        u128::from(source[row * DIM + inner]) * u128::from(b[inner * DIM + column])
                    })
                    .sum();
                let position = row * DIM + column;
                let low = u32::try_from(sum % (1_u128 << 32)).expect("modulo 2^32");
                let rotation = pass % 32;
                let original = a[(position + pass as usize) % WORDS];
                let mixed = if rotation == 0 {
                    original
                } else {
                    (original << rotation) | (original >> (32 - rotation))
                };
                destination[position] = low ^ mixed;
            }
        }
        source = destination;
    }
    source
}

fn wire_index(index: usize, seed: u32) -> usize {
    // gcd(5, 768) = 1: every A/B/C word has one unique wire position.
    (index * 5 + seed as usize % (WORDS * 3)) % (WORDS * 3)
}

fn digest(block: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(&block[..96]);
    hash.update([0_u8; 32]);
    hash.update(&block[HEADER..]);
    hash.finalize().into()
}

pub fn build(
    context: &BlockContext,
    rounds: u32,
    inject_compute: bool,
) -> Result<[u8; BLOCK_SIZE], String> {
    let seed = key(context, rounds);
    let (a, b) = inputs_fast(seed);
    let mut c = optimized(&a, &b, rounds);
    if inject_compute {
        c[17] ^= 1;
    }
    let (expected_a, expected_b) = inputs_reference(seed);
    if a != expected_a || b != expected_b || c != reference(&expected_a, &expected_b, rounds) {
        return Err(
            "producer matrix/input differs from independent reference; no write authorized".into(),
        );
    }
    let mut block = [0; BLOCK_SIZE];
    block[..HEADER].copy_from_slice(&header(context, rounds));
    for (index, word) in a.iter().chain(&b).chain(&c).enumerate() {
        let offset = HEADER + wire_index(index, seed) * 4;
        block[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    for (index, value) in block[DATA_END..].iter_mut().enumerate() {
        *value = seed.to_le_bytes()[index % 4] ^ u8::try_from(index % 256).expect("byte");
    }
    let tag = digest(&block);
    block[96..128].copy_from_slice(&tag);
    Ok(block)
}

pub fn verify(
    block: &[u8],
    context: &BlockContext,
    rounds: u32,
    inject_compute: bool,
    inject_reference: bool,
    inject_join: bool,
) -> Result<(), String> {
    if block.len() != BLOCK_SIZE {
        return Err("deep block length mismatch".into());
    }
    if block[..96] != header(context, rounds)[..96] {
        return Err(
            "deep identity/header mismatch (device, generation, LBA, seed or rounds)".into(),
        );
    }
    if block[96..128] != digest(block) {
        return Err("deep block SHA-256 mismatch".into());
    }
    let seed = key(context, rounds);
    // Decode ONLY the supplied backend read buffer, never the producer buffer.
    let mut words = [0; WORDS * 3];
    for (logical, word) in words.iter_mut().enumerate() {
        let offset = HEADER + wire_index(logical, seed) * 4;
        *word = u32::from_le_bytes(block[offset..offset + 4].try_into().expect("word"));
    }
    if inject_join {
        words.swap(0, 1);
    }
    let actual_a: Matrix = words[..WORDS].try_into().expect("A");
    let actual_b: Matrix = words[WORDS..WORDS * 2].try_into().expect("B");
    let stored_c: Matrix = words[WORDS * 2..].try_into().expect("C");
    let (expected_a, expected_b) = inputs_reference(seed);
    let mut expected_c = reference(&expected_a, &expected_b, rounds);
    if inject_reference {
        expected_c[17] ^= 1;
    }
    if actual_a != expected_a || actual_b != expected_b {
        return Err("readback A/B or split/join differs from reference input".into());
    }
    let mut computed_c = optimized(&actual_a, &actual_b, rounds);
    if inject_compute {
        computed_c[17] ^= 1;
    }
    if computed_c != expected_c || stored_c != expected_c {
        return Err("readback matrix computation/stored C differs from reference C".into());
    }
    for (index, actual) in block[DATA_END..].iter().enumerate() {
        let expected = seed.to_le_bytes()[index % 4] ^ u8::try_from(index % 256).expect("byte");
        if *actual != expected {
            return Err("deep padding mismatch".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> BlockContext {
        BlockContext {
            session_seed: 123,
            generation: 9,
            device_id: "fixture".into(),
            lba: 17,
            stripe_id: 3,
            stripe_index: 1,
        }
    }

    #[test]
    fn matrix_known_answer_and_overflow_agree() {
        let zero = [0; WORDS];
        let ones = [1; WORDS];
        assert_eq!(optimized(&ones, &zero, 1), ones);
        assert_eq!(optimized(&ones, &ones, 1), [17; WORDS]);
        for seed in [0, 1, 123, u32::MAX] {
            let (a, b) = inputs_fast(seed);
            assert_eq!((a, b), inputs_reference(seed));
            for rounds in [1, 4, 8] {
                assert_eq!(optimized(&a, &b, rounds), reference(&a, &b, rounds));
            }
        }
    }

    #[test]
    fn wire_roundtrip_and_each_fault_is_detected() {
        let ctx = context();
        let block = build(&ctx, 4, false).unwrap();
        verify(&block, &ctx, 4, false, false, false).unwrap();
        assert!(build(&ctx, 4, true).is_err());
        for flags in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert!(verify(&block, &ctx, 4, flags.0, flags.1, flags.2).is_err());
        }
        let mut wrong = block;
        wrong[HEADER + 7] ^= 1;
        // Even a coherently rehashed corruption must fail reference checks.
        let tag = digest(&wrong);
        wrong[96..128].copy_from_slice(&tag);
        assert!(verify(&wrong, &ctx, 4, false, false, false).is_err());
        let mut stale = ctx.clone();
        stale.generation += 1;
        assert!(verify(&block, &stale, 4, false, false, false).is_err());
    }

    #[test]
    fn serialized_block_has_stable_golden_digest() {
        let block = build(&context(), 4, false).unwrap();
        let actual: [u8; 32] = Sha256::digest(block).into();
        assert_eq!(
            actual,
            [
                0x05, 0x5f, 0x7f, 0xb5, 0x9f, 0x57, 0x16, 0xe3, 0xa1, 0xc1, 0x26, 0x9a, 0xea, 0x6b,
                0x9c, 0x5f, 0x68, 0xdc, 0x65, 0x39, 0x38, 0xf2, 0x5f, 0xab, 0xec, 0x15, 0x40, 0xd2,
                0xff, 0x68, 0xa5, 0x3c,
            ]
        );
    }
}
