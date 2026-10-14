//! Tier-2 compute kernels (v1.2, C5): long sessions add them to the compute oracle's bursts
//! (`compute_oracle`), one per burst in turn, each checked exactly against a reference that does
//! not come from the units it loads.
//!
//! | Kernel | Loads | Reference |
//! |---|---|---|
//! | `dgemm` | FMA units, loads/stores, L1/L2 | integer matrix product |
//! | `cache_l1`, `cache_l2`, `cache_l3` | read-modify-write passes over this thread's share of a cache level | closed form: the start value and every pass's key |
//! | `crypto` | AES-NI (AES-256-CTR) and SHA-NI over an L2-sized buffer | AES and SHA-256 in plain software (`soft_crypto`) |

use std::path::Path;

use aes::{
    Aes256,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use wide::f64x4;

use crate::soft_crypto::{SoftAes256, sha256};

/// The tier-2 kernels, in the order a worker takes them.
pub const TIER2_KERNELS: [&str; 5] = ["dgemm", "cache_l1", "cache_l2", "cache_l3", "crypto"];

/// Matrix order: three 64 x 64 matrices of `f64` are 96 KiB, beyond L1, well inside L2.
const DGEMM_N: usize = 64;
/// Matrix entries are integers below `2^20` in magnitude: a product stays below `2^40` and a sum
/// of 64 of them below `2^46`, so every intermediate value is an integer a `double` holds
/// exactly. The product is then exact however it is computed (fused or not, in any order), and
/// is compared bit for bit with an integer product.
const DGEMM_VALUE_BITS: u32 = 20;
/// Matrix products per burst.
const DGEMM_PRODUCTS: u32 = 3_000;
/// Rows of C computed together: as many accumulators as the vector registers hold.
#[cfg(target_feature = "fma")]
const DGEMM_ROWS: usize = 4;
#[cfg(not(target_feature = "fma"))]
const DGEMM_ROWS: usize = 2;

/// Bytes a cache kernel reads and writes per burst (its buffer times its passes).
const CACHE_BYTES_PER_BURST: u64 = 6 * 1024 * 1024 * 1024;
/// Buffer sizes when Linux does not report the cache: L1 data, L2, this thread's L3 share.
const FALLBACK_CACHE_BYTES: [u64; 3] = [16 * 1024, 256 * 1024, 2 * 1024 * 1024];
/// The L3 share is capped: the kernel is about the cache, not about RAM.
const MAX_L3_SHARE_BYTES: u64 = 8 * 1024 * 1024;
const PASS_KEY: u64 = 0xc2b2_ae3d_27d4_eb4f;

/// The crypto kernel's buffer: 256 KiB fits the L2 of every CPU the image supports.
const CRYPTO_BYTES: usize = 256 * 1024;
/// Encrypt-and-hash rounds per burst.
const CRYPTO_ROUNDS: u32 = 320;

/// How much each tier-2 burst does.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tier2Load {
    dgemm_products: u32,
    cache_bytes_per_burst: u64,
    crypto_rounds: u32,
}

impl Tier2Load {
    /// About 50-120 ms a burst on a desktop core (measured on a 3700X, SSE2 and AVX2 builds).
    pub(crate) const SESSION: Self = Self {
        dgemm_products: DGEMM_PRODUCTS,
        cache_bytes_per_burst: CACHE_BYTES_PER_BURST,
        crypto_rounds: CRYPTO_ROUNDS,
    };
    /// A few milliseconds even unoptimised: this crate's unit tests.
    const UNIT_TEST: Self = Self {
        dgemm_products: 2,
        cache_bytes_per_burst: 1024 * 1024,
        crypto_rounds: 2,
    };
}

/// How one tier-2 kernel fared over a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelTally {
    pub kernel: String,
    pub bursts: u64,
    pub mismatches: u64,
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// What every worker's tier-2 bursts must produce, worked out once for a session.
pub(crate) struct Tier2References {
    seed: u64,
    dgemm_a: Vec<f64>,
    dgemm_b: Vec<f64>,
    /// The integer product, as the `f64` bits the vector product must give.
    dgemm_product: Vec<u64>,
    crypto_key: [u8; 32],
    crypto_nonce: u64,
    crypto_plaintext: Vec<u8>,
    crypto_ciphertext: Vec<u8>,
    crypto_digest: [u8; 32],
}

impl Tier2References {
    pub(crate) fn new(seed: u64) -> Self {
        let entry = |index: u64| {
            let raw = splitmix64(seed ^ index) % (2 << DGEMM_VALUE_BITS);
            i64::try_from(raw).unwrap_or(0) - (1_i64 << DGEMM_VALUE_BITS) + 1
        };
        let cells = (DGEMM_N * DGEMM_N) as u64;
        let a: Vec<i64> = (0..cells).map(entry).collect();
        let b: Vec<i64> = (cells..2 * cells).map(entry).collect();
        let mut product = vec![0_i64; DGEMM_N * DGEMM_N];
        for row in 0..DGEMM_N {
            for inner in 0..DGEMM_N {
                let left = a[row * DGEMM_N + inner];
                for column in 0..DGEMM_N {
                    product[row * DGEMM_N + column] += left * b[inner * DGEMM_N + column];
                }
            }
        }
        #[allow(clippy::cast_precision_loss)] // Below 2^46: exact.
        let as_f64 = |values: &[i64]| values.iter().map(|&value| value as f64).collect::<Vec<_>>();

        let crypto_key: [u8; 32] =
            std::array::from_fn(|index| splitmix64(seed ^ 0xae5 ^ index as u64).to_le_bytes()[0]);
        let crypto_nonce = splitmix64(seed ^ 0x006e_6f6e_6365);
        let crypto_plaintext: Vec<u8> = (0..CRYPTO_BYTES as u64 / 8)
            .flat_map(|word| splitmix64(seed ^ 0x7074 ^ word).to_le_bytes())
            .collect();
        let soft = SoftAes256::new(&crypto_key);
        let mut crypto_ciphertext = crypto_plaintext.clone();
        for (index, block) in crypto_ciphertext
            .as_chunks_mut::<16>()
            .0
            .iter_mut()
            .enumerate()
        {
            let mut keystream = counter_block(crypto_nonce, index as u64);
            soft.encrypt_block(&mut keystream);
            for (byte, key) in block.iter_mut().zip(keystream) {
                *byte ^= key;
            }
        }
        let crypto_digest = sha256(&crypto_ciphertext);
        Self {
            seed,
            dgemm_a: as_f64(&a),
            dgemm_b: as_f64(&b),
            dgemm_product: as_f64(&product)
                .iter()
                .map(|value| value.to_bits())
                .collect(),
            crypto_key,
            crypto_nonce,
            crypto_plaintext,
            crypto_ciphertext,
            crypto_digest,
        }
    }
}

fn counter_block(nonce: u64, index: u64) -> [u8; 16] {
    let mut block = [0_u8; 16];
    block[..8].copy_from_slice(&nonce.to_be_bytes());
    block[8..].copy_from_slice(&index.to_be_bytes());
    block
}

/// One cache level's kernel for one worker: its buffer, passes and expected final contents.
struct CacheLevel {
    buffer: Vec<u64>,
    passes: u64,
    /// Worked out at the worker's first burst of this level, in closed form.
    expected: Option<Vec<u64>>,
}

impl CacheLevel {
    fn new(bytes: u64, bytes_per_burst: u64) -> Self {
        let words = usize::try_from(bytes / 8).unwrap_or(1).max(64);
        Self {
            buffer: vec![0; words],
            passes: (bytes_per_burst / (words as u64 * 8)).max(1),
            expected: None,
        }
    }
}

fn cache_initial(seed: u64, index: u64) -> u64 {
    splitmix64(seed ^ index.wrapping_mul(PASS_KEY))
}

/// One pass's change to a word: an xor with the pass's key, the least arithmetic a pass can do,
/// so the cache level sets the pace. Measured on a 3700X (AVX2): L1 128 GB/s, L2 58, L3 55 per
/// core; with a multiply or a shift-and-add in the step every level ran at the same speed, held
/// back by the arithmetic. A wrong bit in any pass stays wrong to the end.
#[inline]
fn cache_step(value: u64, key: u64) -> u64 {
    value ^ key
}

fn pass_key(seed: u64, pass: u64) -> u64 {
    seed ^ pass.wrapping_mul(PASS_KEY)
}

/// One worker's tier-2 state: its buffers, and which kernel comes next.
pub(crate) struct Tier2Worker {
    next: usize,
    dgemm_products: u32,
    crypto_rounds: u32,
    caches: [CacheLevel; 3],
    dgemm_c: Vec<f64>,
    /// AES blocks, encrypted in place (the AES-NI path works on several at once).
    crypto_blocks: Vec<Block>,
    crypto_buffer: Vec<u8>,
    pub(crate) tallies: Vec<KernelTally>,
}

type Block = GenericArray<u8, aes::cipher::consts::U16>;

impl Tier2Worker {
    /// `cpu_id`'s caches size the buffers; workers start at different kernels.
    pub(crate) fn new(worker_id: usize, cpu_id: usize) -> Self {
        let load = if cfg!(test) {
            Tier2Load::UNIT_TEST
        } else {
            Tier2Load::SESSION
        };
        Self::with_load(worker_id, cpu_id, load)
    }

    pub(crate) fn with_load(worker_id: usize, cpu_id: usize, load: Tier2Load) -> Self {
        let sizes = cache_shares(Path::new("/sys/devices/system/cpu"), cpu_id);
        Self {
            next: worker_id % TIER2_KERNELS.len(),
            dgemm_products: load.dgemm_products,
            crypto_rounds: load.crypto_rounds,
            caches: sizes.map(|bytes| CacheLevel::new(bytes, load.cache_bytes_per_burst)),
            dgemm_c: vec![0.0; DGEMM_N * DGEMM_N],
            crypto_blocks: vec![Block::default(); CRYPTO_BYTES / 16],
            crypto_buffer: vec![0; CRYPTO_BYTES],
            tallies: TIER2_KERNELS
                .iter()
                .map(|kernel| KernelTally {
                    kernel: (*kernel).to_owned(),
                    ..KernelTally::default()
                })
                .collect(),
        }
    }

    /// Runs the next kernel once and counts it; true when its result was right.
    pub(crate) fn run_next(&mut self, references: &Tier2References) -> bool {
        let index = self.next;
        self.next = (self.next + 1) % TIER2_KERNELS.len();
        let passed = match index {
            0 => self.dgemm(references),
            1..=3 => self.cache(index - 1, references.seed),
            _ => self.crypto(references),
        };
        let tally = &mut self.tallies[index];
        tally.bursts += 1;
        tally.mismatches += u64::from(!passed);
        passed
    }

    fn dgemm(&mut self, references: &Tier2References) -> bool {
        let mut passed = true;
        for _ in 0..self.dgemm_products {
            dgemm(
                std::hint::black_box(&references.dgemm_a),
                std::hint::black_box(&references.dgemm_b),
                &mut self.dgemm_c,
            );
            passed &= self
                .dgemm_c
                .iter()
                .zip(&references.dgemm_product)
                .all(|(value, expected)| value.to_bits() == *expected);
        }
        passed
    }

    fn cache(&mut self, level: usize, seed: u64) -> bool {
        let cache = &mut self.caches[level];
        let passes = cache.passes;
        // Every pass xors every word with the same key: the end value is the start value xor
        // all the keys, worked out without going through the buffer.
        let expected = cache.expected.get_or_insert_with(|| {
            let all_keys = (0..passes).fold(0, |keys, pass| keys ^ pass_key(seed, pass));
            (0..cache.buffer.len() as u64)
                .map(|index| cache_initial(seed, index) ^ all_keys)
                .collect()
        });
        for (index, word) in cache.buffer.iter_mut().enumerate() {
            *word = cache_initial(seed, index as u64);
        }
        for pass in 0..passes {
            let key = pass_key(seed, pass);
            for word in &mut cache.buffer {
                *word = cache_step(*word, key);
            }
            // Every pass goes through the cache: the compiler may not keep words in registers
            // across passes (nor turn the loops around).
            std::hint::black_box(&mut cache.buffer);
        }
        cache.buffer == *expected
    }

    fn crypto(&mut self, references: &Tier2References) -> bool {
        let Ok(cipher) = Aes256::new_from_slice(&references.crypto_key) else {
            return false;
        };
        let mut passed = true;
        for _ in 0..self.crypto_rounds {
            for (index, block) in self.crypto_blocks.iter_mut().enumerate() {
                block.copy_from_slice(&counter_block(references.crypto_nonce, index as u64));
            }
            cipher.encrypt_blocks(&mut self.crypto_blocks);
            for ((bytes, keystream), plain) in self
                .crypto_buffer
                .as_chunks_mut::<16>()
                .0
                .iter_mut()
                .zip(&self.crypto_blocks)
                .zip(references.crypto_plaintext.as_chunks::<16>().0.iter())
            {
                for ((byte, key), plain) in bytes.iter_mut().zip(keystream).zip(plain) {
                    *byte = key ^ plain;
                }
            }
            let digest: [u8; 32] = Sha256::digest(&self.crypto_buffer).into();
            passed &= self.crypto_buffer == references.crypto_ciphertext
                && digest == references.crypto_digest;
        }
        passed
    }
}

/// C = A x B for `DGEMM_N` x `DGEMM_N` row-major matrices: `DGEMM_ROWS` rows by eight columns of
/// C at a time, held in vector accumulators, each step one broadcast of A per row and two vector
/// loads of B.
fn dgemm(a: &[f64], b: &[f64], c: &mut [f64]) {
    let load =
        |slice: &[f64], at: usize| f64x4::new(slice[at..at + 4].try_into().expect("four values"));
    for row in (0..DGEMM_N).step_by(DGEMM_ROWS) {
        for column in (0..DGEMM_N).step_by(8) {
            let mut accumulators = [[f64x4::ZERO; 2]; DGEMM_ROWS];
            for inner in 0..DGEMM_N {
                let right = [
                    load(b, inner * DGEMM_N + column),
                    load(b, inner * DGEMM_N + column + 4),
                ];
                for (offset, accumulator) in accumulators.iter_mut().enumerate() {
                    let left = f64x4::splat(a[(row + offset) * DGEMM_N + inner]);
                    accumulator[0] = left.mul_add(right[0], accumulator[0]);
                    accumulator[1] = left.mul_add(right[1], accumulator[1]);
                }
            }
            for (offset, accumulator) in accumulators.iter().enumerate() {
                let at = (row + offset) * DGEMM_N + column;
                c[at..at + 4].copy_from_slice(&accumulator[0].to_array());
                c[at + 4..at + 8].copy_from_slice(&accumulator[1].to_array());
            }
        }
    }
}

/// This thread's share of each cache level (`cpu/cpuN/cache/index*`): the size divided by the
/// threads sharing it, L3 capped at [`MAX_L3_SHARE_BYTES`]. Unreported levels fall back to
/// [`FALLBACK_CACHE_BYTES`].
fn cache_shares(root: &Path, cpu_id: usize) -> [u64; 3] {
    let mut shares = FALLBACK_CACHE_BYTES;
    let Ok(entries) = std::fs::read_dir(root.join(format!("cpu{cpu_id}/cache"))) else {
        return shares;
    };
    for entry in entries.flatten() {
        let read = |name: &str| {
            std::fs::read_to_string(entry.path().join(name))
                .map(|text| text.trim().to_owned())
                .unwrap_or_default()
        };
        let Ok(level) = read("level").parse::<usize>() else {
            continue;
        };
        if read("type") == "Instruction" || !(1..=3).contains(&level) {
            continue;
        }
        let Some(size) = parse_size(&read("size")) else {
            continue;
        };
        let threads_sharing =
            u64::try_from(crate::inventory::parse_cpu_list(&read("shared_cpu_list")).len())
                .unwrap_or(1)
                .max(1);
        let share = size / threads_sharing;
        shares[level - 1] = if level == 3 {
            share.min(MAX_L3_SHARE_BYTES)
        } else {
            share
        };
    }
    shares
}

/// `48K`, `2048K`, `32M` in bytes.
fn parse_size(text: &str) -> Option<u64> {
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((at, _)) => text.split_at(at),
        None => (text, ""),
    };
    let value: u64 = digits.parse().ok()?;
    let scale = match unit {
        "" => 1,
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        _ => return None,
    };
    value.checked_mul(scale)
}

#[cfg(test)]
mod tests {
    use super::{
        CacheLevel, DGEMM_N, TIER2_KERNELS, Tier2References, Tier2Worker, cache_shares, parse_size,
    };

    #[test]
    fn sizes_and_shares_come_from_sysfs() {
        assert_eq!(parse_size("48K"), Some(48 * 1024));
        assert_eq!(parse_size("32M"), Some(32 * 1024 * 1024));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size("1T"), None);
        let root = std::env::temp_dir().join(format!("dit-cache-{}", std::process::id()));
        for (index, level, kind, size, shared) in [
            (0, "1", "Data", "48K", "0,16"),
            (1, "1", "Instruction", "32K", "0,16"),
            (2, "2", "Unified", "2048K", "0,16"),
            (3, "3", "Unified", "36864K", "0-31"),
        ] {
            let dir = root.join(format!("cpu0/cache/index{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            for (name, value) in [
                ("level", level),
                ("type", kind),
                ("size", size),
                ("shared_cpu_list", shared),
            ] {
                std::fs::write(dir.join(name), value).unwrap();
            }
        }
        // i9-14900K P-core: 48K L1d and 2 MiB L2 per core (two threads), 36 MiB L3 for 32.
        assert_eq!(
            cache_shares(&root, 0),
            [24 * 1024, 1024 * 1024, 36 * 1024 * 1024 / 32]
        );
        assert_eq!(cache_shares(&root, 7), super::FALLBACK_CACHE_BYTES);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_kernel_matches_its_reference_and_a_wrong_reference_is_caught() {
        let references = Tier2References::new(0x5eed);
        // The integer product's entries are exact in f64.
        assert_eq!(references.dgemm_product.len(), DGEMM_N * DGEMM_N);
        let mut worker = Tier2Worker::new(0, 0);
        // Small caches keep the test short.
        worker.caches =
            [4 * 1024, 16 * 1024, 64 * 1024].map(|bytes| CacheLevel::new(bytes, 3 * bytes));
        for _ in 0..TIER2_KERNELS.len() {
            assert!(worker.run_next(&references), "{:?}", worker.tallies);
        }
        assert!(
            worker
                .tallies
                .iter()
                .all(|tally| tally.bursts == 1 && tally.mismatches == 0)
        );

        let mut wrong = Tier2References::new(0x5eed);
        wrong.dgemm_product[17] ^= 1;
        wrong.crypto_digest[0] ^= 1;
        assert!(!worker.run_next(&wrong), "dgemm");
        worker.caches[0].expected.as_mut().unwrap()[5] ^= 1 << 40;
        assert!(!worker.run_next(&references), "cache_l1");
        worker.next = TIER2_KERNELS.len() - 1;
        assert!(!worker.run_next(&wrong), "crypto");
        assert_eq!(worker.tallies[0].mismatches, 1);
        assert_eq!(worker.tallies[1].mismatches, 1);
        assert_eq!(worker.tallies[4].mismatches, 1);
    }

    /// Burst time of each kernel on this machine's caches (`cargo test --release -p dit-system
    /// --lib tier2_speed -- --ignored --nocapture`).
    #[test]
    #[ignore = "measurement, run by hand in release"]
    fn tier2_speed() {
        let started = std::time::Instant::now();
        let references = Tier2References::new(1);
        println!(
            "references       {:7.1} ms",
            started.elapsed().as_secs_f64() * 1e3
        );
        let mut worker = Tier2Worker::with_load(0, 0, super::Tier2Load::SESSION);
        for level in &worker.caches {
            println!(
                "cache buffer {:9} bytes, {:6} passes",
                level.buffer.len() * 8,
                level.passes
            );
        }
        for round in 0..2 {
            for kernel in TIER2_KERNELS {
                let started = std::time::Instant::now();
                assert!(worker.run_next(&references));
                println!(
                    "{kernel:12} round {round} {:7.1} ms",
                    started.elapsed().as_secs_f64() * 1e3
                );
            }
        }
    }
}
