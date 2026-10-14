//! AES-256 and SHA-256 in plain software, byte by byte from FIPS-197 and FIPS 180-4.
//!
//! The crypto kernel (`kernels.rs`) runs AES and SHA-256 on the CPU's own units (AES-NI and
//! SHA-NI, through the `aes` and `sha2` crates, which pick them at run time). Its reference must
//! not come from those same units, so it is worked out here once, at the start of a session,
//! with nothing but table lookups, shifts and additions. Slow (tens of MB/s), and only used on
//! one buffer.

/// FIPS-197 S-box.
const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

const ROUND_CONSTANTS: [u8; 7] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40];

/// AES-256 with its 15 round keys expanded once.
pub(crate) struct SoftAes256 {
    round_keys: [[u8; 16]; 15],
}

impl SoftAes256 {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        // FIPS-197 section 5.2, Nk = 8: 60 words.
        let mut words = [[0_u8; 4]; 60];
        for (index, word) in words.iter_mut().take(8).enumerate() {
            word.copy_from_slice(&key[index * 4..index * 4 + 4]);
        }
        for index in 8..60 {
            let mut temp = words[index - 1];
            if index % 8 == 0 {
                temp = [
                    SBOX[usize::from(temp[1])] ^ ROUND_CONSTANTS[index / 8 - 1],
                    SBOX[usize::from(temp[2])],
                    SBOX[usize::from(temp[3])],
                    SBOX[usize::from(temp[0])],
                ];
            } else if index % 8 == 4 {
                temp = temp.map(|byte| SBOX[usize::from(byte)]);
            }
            words[index] = std::array::from_fn(|byte| words[index - 8][byte] ^ temp[byte]);
        }
        Self {
            round_keys: std::array::from_fn(|round| {
                std::array::from_fn(|byte| words[round * 4 + byte / 4][byte % 4])
            }),
        }
    }

    pub(crate) fn encrypt_block(&self, block: &mut [u8; 16]) {
        add_round_key(block, &self.round_keys[0]);
        for round in 1..15 {
            for byte in block.iter_mut() {
                *byte = SBOX[usize::from(*byte)];
            }
            shift_rows(block);
            if round < 14 {
                mix_columns(block);
            }
            add_round_key(block, &self.round_keys[round]);
        }
    }
}

fn add_round_key(block: &mut [u8; 16], key: &[u8; 16]) {
    for (byte, key) in block.iter_mut().zip(key) {
        *byte ^= key;
    }
}

/// The state is column-major: byte `4c + r` is row `r` of column `c`.
fn shift_rows(block: &mut [u8; 16]) {
    let state = *block;
    for column in 0..4 {
        for row in 1..4 {
            block[column * 4 + row] = state[((column + row) % 4) * 4 + row];
        }
    }
}

const fn times_two(byte: u8) -> u8 {
    (byte << 1) ^ if byte & 0x80 == 0 { 0 } else { 0x1b }
}

fn mix_columns(block: &mut [u8; 16]) {
    for column in block.as_chunks_mut::<4>().0 {
        let [a0, a1, a2, a3] = [column[0], column[1], column[2], column[3]];
        let all = a0 ^ a1 ^ a2 ^ a3;
        column[0] = a0 ^ all ^ times_two(a0 ^ a1);
        column[1] = a1 ^ all ^ times_two(a1 ^ a2);
        column[2] = a2 ^ all ^ times_two(a2 ^ a3);
        column[3] = a3 ^ all ^ times_two(a3 ^ a0);
    }
}

/// FIPS 180-4 constants.
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 of `data`, in software.
#[allow(clippy::many_single_char_names)] // a..h are the standard's working variables.
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bit_length = u64::try_from(data.len())
        .unwrap_or(u64::MAX)
        .wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_length.to_be_bytes());
    for block in padded.as_chunks::<64>().0 {
        let mut schedule = [0_u32; 64];
        for (index, word) in block.as_chunks::<4>().0.iter().enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(schedule[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (word, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *word = word.wrapping_add(value);
        }
    }
    let mut digest = [0_u8; 32];
    for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        bytes.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use aes::{
        Aes256,
        cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
    };
    use sha2::{Digest, Sha256};

    use super::{SoftAes256, sha256};

    #[test]
    fn soft_aes_matches_fips_197_and_the_hardware_path() {
        // FIPS-197 Appendix C.3.
        let key: [u8; 32] = std::array::from_fn(|index| u8::try_from(index).expect("byte"));
        let mut block: [u8; 16] =
            std::array::from_fn(|index| u8::try_from(index * 0x11).expect("byte"));
        SoftAes256::new(&key).encrypt_block(&mut block);
        assert_eq!(
            block,
            [
                0x8e, 0xa2, 0xb7, 0xca, 0x51, 0x67, 0x45, 0xbf, 0xea, 0xfc, 0x49, 0x90, 0x4b, 0x49,
                0x60, 0x89
            ]
        );
        let other_key: [u8; 32] =
            std::array::from_fn(|index| u8::try_from((index * 89 + 7) % 256).expect("byte"));
        let soft = SoftAes256::new(&other_key);
        let hardware = Aes256::new_from_slice(&other_key).expect("key");
        for seed in 0..64_u8 {
            let mut ours: [u8; 16] = std::array::from_fn(|index| {
                seed.wrapping_mul(31)
                    .wrapping_add(u8::try_from(index).expect("byte"))
            });
            let mut theirs = GenericArray::clone_from_slice(&ours);
            soft.encrypt_block(&mut ours);
            hardware.encrypt_block(&mut theirs);
            assert_eq!(ours.as_slice(), theirs.as_slice());
        }
    }

    #[test]
    fn soft_sha256_matches_fips_180_and_the_hardware_path() {
        assert_eq!(
            hex::encode(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        for length in [0_usize, 55, 56, 63, 64, 65, 1000, 4096] {
            let data: Vec<u8> = (0..length)
                .map(|index| u8::try_from(index % 251).expect("byte"))
                .collect();
            let hardware: [u8; 32] = Sha256::digest(&data).into();
            assert_eq!(sha256(&data), hardware, "{length} bytes");
        }
    }
}
