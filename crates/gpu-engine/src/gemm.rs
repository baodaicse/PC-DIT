//! GPU-G3 exact rectangular GEMM corpus, independent scalar CPU oracle.
//! Corpus is keyed by seed/jobs/K; generation tags every output without repeating
//! the cubic CPU oracle. All arithmetic is modulo 2^32.
use sha2::{Digest, Sha256};
use std::time::Instant;

pub const VERSION: &str = "G3-U32-GEMM-1";
/// FP32 corpora: IEEE-754 single precision, every product accumulated with one fused
/// multiply-add in ascending `k`, on the device and in the oracle alike.
pub const F32_VERSION: &str = "G4-F32-FMA-GEMM-1";
/// INT8 tensor-core corpora: signed bytes, 32-bit integer accumulation, `n` fixed at 64.
pub const I8_VERSION: &str = "G5-I8-MMA-GEMM-1";
/// Matrix size of INT8 corpora: tensor tiles are 16x8, so `n` must divide by 16.
pub const I8_N: usize = 64;

/// Arithmetic a corpus exercises: the integer ALU (`u32`, modulo 2^32) or the FP32 FMA pipes.
/// Both are exact to the bit: integer wrap-around is exact, and a fused multiply-add is one
/// correctly rounded IEEE operation, so the same inputs in the same order give the same bits
/// on any conforming device, however the device tiles the work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Arith {
    #[default]
    U32,
    F32,
    /// Signed 8-bit operands on the tensor cores (compute capability 8.0+), exact in 32 bits.
    I8,
}

/// Which arithmetic successive corpora use. `Alternate` switches at every corpus rotation, so a
/// run exercises the integer ALU and the FP32 FMA pipes in turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArithMix {
    #[default]
    U32,
    F32,
    /// INT8 tensor cores only (needs compute capability 8.0+).
    I8,
    /// u32 and FP32 in turn.
    Alternate,
    /// u32, FP32 and INT8 tensor in turn; without tensor cores, like `Alternate`.
    All,
}

impl ArithMix {
    /// Arithmetic of the `rotation`-th corpus of a run; `tensor` says whether the device has the
    /// INT8 tensor path (an `All` run then includes it).
    #[must_use]
    pub fn for_rotation(self, rotation: u64, tensor: bool) -> Arith {
        const TURN: [Arith; 3] = [Arith::U32, Arith::F32, Arith::I8];
        let cycle = match self {
            Self::U32 => return Arith::U32,
            Self::F32 => return Arith::F32,
            Self::I8 => return Arith::I8,
            Self::All if tensor => 3,
            Self::Alternate | Self::All => 2,
        };
        TURN[usize::try_from(rotation % cycle).unwrap_or(0)]
    }

    /// Whether this mix uses more than one arithmetic, and so needs corpus rotation.
    #[must_use]
    pub fn alternates(self) -> bool {
        matches!(self, Self::Alternate | Self::All)
    }
}

impl Arith {
    #[must_use]
    pub fn version(self) -> &'static str {
        match self {
            Self::U32 => VERSION,
            Self::F32 => F32_VERSION,
            Self::I8 => I8_VERSION,
        }
    }
}
pub const MAX_N: usize = 65;
pub const MAX_K: u32 = 4096;

/// Corpus seed for the `rotation`-th corpus of a run. Rotation 0 is the base seed
/// itself so runs that never rotate keep their original identity; later
/// rotations are a fixed function of (base, rotation), which replay can redo.
#[must_use]
pub fn derive_corpus_seed(base: u64, rotation: u64) -> u64 {
    if rotation == 0 {
        base
    } else {
        super::splitmix64(base ^ rotation.wrapping_mul(0xA24B_AED4_963E_E407))
    }
}

#[derive(Clone, Debug)]
pub struct Corpus {
    pub arith: Arith,
    pub seed: u64,
    pub jobs: usize,
    pub n: usize,
    pub k: usize,
    pub input: Vec<u32>,
    pub dot: Vec<u32>,
    pub sha256: String,
    /// SHA-256 of the expected dot products, taken when the oracle finished; with `sha256`
    /// it lets a later pass prove the host copy of the corpus has not changed.
    pub dot_sha256: String,
    /// Input generation plus the SHA-256 of the input (which runs alongside the oracle).
    pub generation_seconds: f64,
    pub oracle_seconds: f64,
    /// Elapsed time to build the whole corpus; less than the sum of the two above
    /// when helper threads overlap hashing and the oracle.
    pub build_wall_seconds: f64,
}

/// Maximum input/output device bytes and conservative peak host bytes per job.
#[must_use]
pub fn workspace_per_job(k: u32) -> (u64, u64, u64) {
    let input = 2 * MAX_N as u64 * u64::from(k) * 4;
    let output = (MAX_N * MAX_N + 4) as u64 * 4;
    (input, output, 3 * input + 5 * output)
}

impl Corpus {
    /// # Errors
    /// Rejects empty/excessive jobs or inner dimension.
    pub fn new(seed: u64, jobs: usize, k: u32) -> Result<Self, &'static str> {
        Self::new_with_threads(seed, jobs, k, 1)
    }

    /// Same corpus for every `threads` value: input words and jobs are pure
    /// functions of their index, so splitting work never changes a result.
    /// # Errors
    /// Rejects empty/excessive jobs or inner dimension.
    pub fn new_with_threads(
        seed: u64,
        jobs: usize,
        k: u32,
        threads: usize,
    ) -> Result<Self, &'static str> {
        Self::new_arith(Arith::U32, seed, jobs, k, threads)
    }

    /// A corpus of the given arithmetic; `new_with_threads` is the `u32` one.
    /// # Errors
    /// Rejects empty/excessive jobs or inner dimension.
    #[allow(clippy::cast_possible_truncation)]
    pub fn new_arith(
        arith: Arith,
        seed: u64,
        jobs: usize,
        k: u32,
        threads: usize,
    ) -> Result<Self, &'static str> {
        if !(1..=65_536).contains(&jobs) || !(1..=MAX_K).contains(&k) {
            return Err("G3 jobs must be 1..65536 and matrix inner dimension 1..4096");
        }
        if arith == Arith::I8 && !k.is_multiple_of(32) {
            return Err("INT8 tensor corpora need an inner dimension divisible by 32");
        }
        let threads = threads.clamp(1, 16);
        let start = Instant::now();
        let n = Self::dimension(arith, seed);
        let k = k as usize;
        // INT8 packs four operands to a word: A (n x k) and B^T (n x k) bytes.
        let per_job = Self::input_words_per_job(arith, n, k);
        let words = jobs.checked_mul(per_job).ok_or("G3 input overflow")?;
        let mut input = vec![0_u32; words];
        fill_input(&mut input, seed, threads);
        if arith == Arith::F32 {
            for word in &mut input {
                *word = f32_operand(*word);
            }
        }
        let filled = start.elapsed().as_secs_f64();
        let oracle_start = Instant::now();
        let mut dot = vec![0_u32; jobs * n * n];
        let hashed = std::thread::scope(|scope| {
            let input = &input;
            let hasher = scope.spawn(move || {
                let started = Instant::now();
                (
                    hash_words_with(Some(arith.version().as_bytes()), input),
                    started.elapsed().as_secs_f64(),
                )
            });
            match arith {
                Arith::U32 => oracle(input, &mut dot, n, k, threads),
                Arith::F32 => oracle_f32(input, &mut dot, n, k, threads),
                Arith::I8 => oracle_i8(input, &mut dot, n, k, threads),
            }
            hasher.join()
        });
        let (sha256, hash_seconds) = hashed.map_err(|_| "G3 input hash thread failed")?;
        let dot_sha256 = hash_words(&dot);
        let oracle_seconds = oracle_start.elapsed().as_secs_f64();
        Ok(Self {
            arith,
            seed,
            jobs,
            n,
            k,
            input,
            dot,
            sha256,
            dot_sha256,
            generation_seconds: filled + hash_seconds,
            oracle_seconds,
            build_wall_seconds: start.elapsed().as_secs_f64(),
        })
    }
    /// A corpus from parts built elsewhere (for example read back from a storage record): the
    /// input words and the expected dot products as they were computed at build time. `n` follows
    /// from the arithmetic and seed exactly as in [`Self::new_arith`]; both hashes are computed
    /// here from the words given, so the caller compares them with the hashes it stored.
    /// # Errors
    /// Rejects sizes that do not match (jobs, n, k, arithmetic).
    pub fn from_parts(
        arith: Arith,
        seed: u64,
        jobs: usize,
        k: u32,
        input: Vec<u32>,
        dot: Vec<u32>,
    ) -> Result<Self, &'static str> {
        if !(1..=65_536).contains(&jobs) || !(1..=MAX_K).contains(&k) {
            return Err("G3 jobs must be 1..65536 and matrix inner dimension 1..4096");
        }
        let n = Self::dimension(arith, seed);
        let k = k as usize;
        if input.len() != jobs * Self::input_words_per_job(arith, n, k) || dot.len() != jobs * n * n
        {
            return Err("corpus parts do not match jobs, n and k");
        }
        let started = Instant::now();
        let sha256 = hash_words_with(Some(arith.version().as_bytes()), &input);
        let dot_sha256 = hash_words(&dot);
        Ok(Self {
            arith,
            seed,
            jobs,
            n,
            k,
            input,
            dot,
            sha256,
            dot_sha256,
            generation_seconds: started.elapsed().as_secs_f64(),
            oracle_seconds: 0.0,
            build_wall_seconds: started.elapsed().as_secs_f64(),
        })
    }

    /// Matrix size of a corpus: `I8_N` for INT8 (tensor tiles), otherwise 63..=65 by seed.
    #[must_use]
    pub fn dimension(arith: Arith, seed: u64) -> usize {
        if arith == Arith::I8 {
            I8_N
        } else {
            63 + usize::try_from(seed % 3).unwrap_or(0)
        }
    }

    /// Input words of one job: A and B as u32/f32 words, or packed four bytes to a word for INT8.
    #[must_use]
    pub fn input_words_per_job(arith: Arith, n: usize, k: usize) -> usize {
        if arith == Arith::I8 {
            n * k / 2
        } else {
            2 * n * k
        }
    }

    /// Re-hashes the host copy of the input and the expected dot products (two threads) and
    /// compares with the hashes taken at build time. A change means host memory or a stray
    /// write altered the corpus, so a mismatch seen meanwhile must not be blamed on the GPU.
    /// # Errors
    /// Names which part changed.
    pub fn integrity_check(&self) -> Result<(), &'static str> {
        let (input_ok, dot_ok) = std::thread::scope(|scope| {
            let dot = scope.spawn(|| hash_words(&self.dot) == self.dot_sha256);
            let input_ok =
                hash_words_with(Some(self.arith.version().as_bytes()), &self.input) == self.sha256;
            (input_ok, dot.join().unwrap_or(false))
        });
        match (input_ok, dot_ok) {
            (true, true) => Ok(()),
            (false, true) => Err("corpus input changed in host memory"),
            (true, false) => Err("expected results changed in host memory"),
            (false, false) => Err("corpus input and expected results changed in host memory"),
        }
    }
    #[must_use]
    pub fn matches(&self, seed: u64, jobs: usize, k: u32) -> bool {
        self.matches_arith(Arith::U32, seed, jobs, k)
    }
    #[must_use]
    pub fn matches_arith(&self, arith: Arith, seed: u64, jobs: usize, k: u32) -> bool {
        self.arith == arith && self.seed == seed && self.jobs == jobs && self.k == k as usize
    }
    /// Operations one batch verifies: a multiply and an add (integer) or one fused
    /// multiply-add counted as two floating-point operations, per inner step and output cell.
    #[must_use]
    pub fn operations(&self) -> u64 {
        self.integer_operations()
    }
    /// Includes generation headers and guard words per job.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn expected(&self, generation: u64) -> Vec<u32> {
        let mut result = Vec::with_capacity(self.jobs * (self.n * self.n + 4));
        for job in 0..self.jobs {
            result.extend([generation as u32, (generation >> 32) as u32]);
            for cell in 0..self.n * self.n {
                result.push(tagged(
                    self.dot[job * self.n * self.n + cell],
                    generation,
                    job as u32,
                    cell as u32,
                ));
            }
            result.extend([0xA55A_1973, 0xA55A_1973]);
        }
        result
    }
    #[must_use]
    pub fn integer_operations(&self) -> u64 {
        2 * self.jobs as u64 * (self.n * self.n * self.k) as u64
    }

    #[allow(clippy::cast_possible_truncation)]
    fn expected_word(&self, stride: usize, index: usize, generation: u64) -> u32 {
        let job = index / stride;
        let cell = index % stride;
        match cell {
            0 => generation as u32,
            1 => (generation >> 32) as u32,
            c if c >= stride - 2 => 0xA55A_1973,
            c => tagged(
                self.dot[job * self.n * self.n + c - 2],
                generation,
                job as u32,
                (c - 2) as u32,
            ),
        }
    }

    /// Full readback verification without allocating expected or copying output.
    ///
    /// Works job by job on 64-word blocks with a branch-free OR of differences, so
    /// the hot loop has no per-word division or early exit and vectorizes. A block
    /// with any difference is rescanned to report the exact first mismatch, which
    /// keeps the result identical to the word-by-word definition.
    /// # Errors
    /// Returns the first differing word, including generation headers and guards.
    #[allow(clippy::cast_possible_truncation)]
    pub fn verify_bytes(&self, generation: u64, bytes: &[u8]) -> Result<(), (usize, u32, u32)> {
        const BLOCK: usize = 64;
        const GUARD: u32 = 0xA55A_1973;
        let cells = self.n * self.n;
        let stride = cells + 4;
        if bytes.len() != self.jobs * stride * 4 {
            return Err((usize::MAX, 0, 0));
        }
        let lo = generation as u32;
        let hi = (generation >> 32) as u32;
        let epoch = lo ^ hi.rotate_left(13);
        for (job, chunk) in bytes.chunks_exact(stride * 4).enumerate() {
            let words = chunk.as_chunks::<4>().0;
            let base = job * stride;
            for (index, expected) in [(0, lo), (1, hi)] {
                let actual = u32::from_le_bytes(words[index]);
                if actual != expected {
                    return Err((base + index, expected, actual));
                }
            }
            let dots = &self.dot[job * cells..(job + 1) * cells];
            let job_tag = (job as u32).wrapping_mul(0x9E37_79B9);
            let expected_at =
                |cell: usize, dot: u32| (dot ^ epoch).rotate_left(7) ^ job_tag ^ cell as u32;
            let payload = &words[2..2 + cells];
            for (block, (got, dot)) in payload.chunks(BLOCK).zip(dots.chunks(BLOCK)).enumerate() {
                let first = block * BLOCK;
                let mut diff = 0_u32;
                for (i, (word, &d)) in got.iter().zip(dot).enumerate() {
                    diff |= expected_at(first + i, d) ^ u32::from_le_bytes(*word);
                }
                if diff != 0 {
                    for (i, (word, &d)) in got.iter().zip(dot).enumerate() {
                        let expected = expected_at(first + i, d);
                        let actual = u32::from_le_bytes(*word);
                        if expected != actual {
                            return Err((base + 2 + first + i, expected, actual));
                        }
                    }
                }
            }
            for index in [2 + cells, 3 + cells] {
                let actual = u32::from_le_bytes(words[index]);
                if actual != GUARD {
                    return Err((base + index, GUARD, actual));
                }
            }
        }
        Ok(())
    }

    /// Counts every differing word of a batch already known to be bad. Only used
    /// on the failure path, so the extra full pass never taxes healthy batches.
    #[must_use]
    pub fn mismatch_summary(
        &self,
        generation: u64,
        bytes: &[u8],
        max_samples: usize,
    ) -> MismatchSummary {
        let stride = self.n * self.n + 4;
        let expected_words = self.jobs * stride;
        let words = bytes.as_chunks::<4>().0;
        let mut summary = MismatchSummary {
            length_mismatch: words.len() != expected_words || !bytes.len().is_multiple_of(4),
            ..MismatchSummary::default()
        };
        for (index, word) in words.iter().take(expected_words).enumerate() {
            let expected = self.expected_word(stride, index, generation);
            let actual = u32::from_le_bytes(*word);
            if expected != actual {
                summary.words += 1;
                if summary.samples.len() < max_samples {
                    summary.samples.push((index, expected, actual));
                }
            }
        }
        // Missing or extra words are differences too, without inventing sample values.
        summary.words += words.len().abs_diff(expected_words) as u64;
        summary
    }
}

/// Failure-path detail: how many words differ and where the first ones are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MismatchSummary {
    pub words: u64,
    pub samples: Vec<(usize, u32, u32)>,
    pub length_mismatch: bool,
}

#[allow(clippy::cast_possible_truncation)]
fn fill_input(input: &mut [u32], seed: u64, threads: usize) {
    let fill = |part: &mut [u32], first: usize| {
        for (offset, word) in part.iter_mut().enumerate() {
            *word = super::splitmix64(
                seed ^ ((first + offset) as u64).wrapping_mul(0xD134_2543_DE82_EF95),
            ) as u32;
        }
    };
    if threads == 1 || input.len() < 1 << 16 {
        fill(input, 0);
        return;
    }
    let per = input.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for (index, part) in input.chunks_mut(per).enumerate() {
            scope.spawn(move || fill(part, index * per));
        }
    });
}

fn hash_words(words: &[u32]) -> String {
    hash_words_with(None, words)
}

fn hash_words_with(prefix: Option<&[u8]>, input: &[u32]) -> String {
    let mut digest = Sha256::new();
    if let Some(prefix) = prefix {
        digest.update(prefix);
    }
    let mut block = vec![0_u8; 1 << 16];
    for words in input.chunks(block.len() / 4) {
        for (dst, word) in block.as_chunks_mut::<4>().0.iter_mut().zip(words) {
            dst.copy_from_slice(&word.to_le_bytes());
        }
        digest.update(&block[..words.len() * 4]);
    }
    format!("{:x}", digest.finalize())
}

/// CPU i-k-j traversal (differs from the GPU shared-tile reduction). Jobs are
/// independent, so they are split across `threads` without changing any value.
fn oracle(input: &[u32], dot: &mut [u32], n: usize, k: usize, threads: usize) {
    let jobs = dot.len() / (n * n);
    let run = |part: &mut [u32], first_job: usize| {
        for (local, out) in part.chunks_exact_mut(n * n).enumerate() {
            let base = (first_job + local) * 2 * n * k;
            for row in 0..n {
                for inner in 0..k {
                    let left = input[base + row * k + inner];
                    let right = &input[base + n * k + inner * n..base + n * k + inner * n + n];
                    for (cell, &r) in out[row * n..row * n + n].iter_mut().zip(right) {
                        *cell = cell.wrapping_add(left.wrapping_mul(r));
                    }
                }
            }
        }
    };
    let threads = threads.min(jobs).max(1);
    if threads == 1 {
        run(dot, 0);
        return;
    }
    let per_job = jobs.div_ceil(threads);
    std::thread::scope(|scope| {
        for (index, part) in dot.chunks_mut(per_job * n * n).enumerate() {
            scope.spawn(move || run(part, index * per_job));
        }
    });
}

/// INT8 dot products: `dot[row][col] = sum over k of a[row][k] * b^T[col][k]`, signed bytes in
/// 32-bit integers. Every word of the input packs four operands, low byte first.
fn oracle_i8(input: &[u32], dot: &mut [u32], n: usize, k: usize, threads: usize) {
    let jobs = dot.len() / (n * n);
    let words = k / 4;
    let run = |part: &mut [u32], first_job: usize| {
        let mut a = vec![0_i32; n * k];
        let mut bt = vec![0_i32; n * k];
        for (local, out) in part.chunks_exact_mut(n * n).enumerate() {
            let base = (first_job + local) * 2 * n * words;
            let unpack = |dst: &mut [i32], src: &[u32]| {
                for (four, word) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src) {
                    for (value, byte) in four.iter_mut().zip(word.to_le_bytes()) {
                        *value = i32::from(byte.cast_signed());
                    }
                }
            };
            unpack(&mut a, &input[base..base + n * words]);
            unpack(&mut bt, &input[base + n * words..base + 2 * n * words]);
            for row in 0..n {
                let left = &a[row * k..(row + 1) * k];
                for (col, cell) in out[row * n..(row + 1) * n].iter_mut().enumerate() {
                    let right = &bt[col * k..(col + 1) * k];
                    let sum: i32 = left.iter().zip(right).map(|(x, y)| x * y).sum();
                    *cell = sum.cast_unsigned();
                }
            }
        }
    };
    let threads = threads.min(jobs).max(1);
    if threads == 1 {
        run(dot, 0);
        return;
    }
    let per_job = jobs.div_ceil(threads);
    std::thread::scope(|scope| {
        for (index, part) in dot.chunks_mut(per_job * n * n).enumerate() {
            scope.spawn(move || run(part, index * per_job));
        }
    });
}

/// An FP32 operand from 32 random bits: random sign and full 23-bit mantissa, exponent picked
/// so the magnitude is in [0.5, 4). Sums of up to 4096 such products stay far from overflow and
/// underflow, so every accumulation step is an ordinary normal-range rounding.
#[must_use]
pub fn f32_operand(random: u32) -> u32 {
    let sign = random & 0x8000_0000;
    let exponent = 126 + ((random >> 23) & 0xFF) % 3;
    sign | (exponent << 23) | (random & 0x007F_FFFF)
}

/// FP32 dot products: for every cell, `acc = fma(a[row][k], b[k][col], acc)` for k ascending,
/// starting from +0.0 -- the order the tiled kernel accumulates in.
fn oracle_f32(input: &[u32], dot: &mut [u32], n: usize, k: usize, threads: usize) {
    let jobs = dot.len() / (n * n);
    let run = |part: &mut [u32], first_job: usize| {
        let mut acc = vec![0.0_f32; n * n];
        for (local, out) in part.chunks_exact_mut(n * n).enumerate() {
            let base = (first_job + local) * 2 * n * k;
            acc.fill(0.0);
            f32_job(&input[base..base + 2 * n * k], &mut acc, n, k);
            for (word, value) in out.iter_mut().zip(&acc) {
                *word = value.to_bits();
            }
        }
    };
    let threads = threads.min(jobs).max(1);
    if threads == 1 {
        run(dot, 0);
        return;
    }
    let per_job = jobs.div_ceil(threads);
    std::thread::scope(|scope| {
        for (index, part) in dot.chunks_mut(per_job * n * n).enumerate() {
            scope.spawn(move || run(part, index * per_job));
        }
    });
}

/// One job, i-k-j: each cell still sees its products in ascending `k`. `mul_add` is the
/// correctly rounded IEEE fused multiply-add (the hardware instruction where the C library
/// selects it at run time), so the result does not depend on how it is carried out.
fn f32_job(job: &[u32], acc: &mut [f32], n: usize, k: usize) {
    for row in 0..n {
        for inner in 0..k {
            let left = f32::from_bits(job[row * k + inner]);
            let right = &job[n * k + inner * n..n * k + inner * n + n];
            for (cell, &r) in acc[row * n..row * n + n].iter_mut().zip(right) {
                *cell = left.mul_add(f32::from_bits(r), *cell);
            }
        }
    }
}

#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn tagged(dot: u32, generation: u64, job: u32, cell: u32) -> u32 {
    let epoch = (generation as u32) ^ ((generation >> 32) as u32).rotate_left(13);
    (dot ^ epoch).rotate_left(7) ^ job.wrapping_mul(0x9E37_79B9) ^ cell
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_corpora_are_deterministic_fused_and_in_range() {
        let a = Corpus::new_arith(Arith::F32, 5, 3, 33, 1).unwrap();
        let b = Corpus::new_arith(Arith::F32, 5, 3, 33, 3).unwrap();
        assert_eq!(
            (a.dot.clone(), a.sha256.clone()),
            (b.dot, b.sha256),
            "threads change nothing"
        );
        let int = Corpus::new(5, 3, 33).unwrap();
        assert_ne!(
            a.sha256, int.sha256,
            "the arithmetic is part of the corpus identity"
        );
        assert!(!a.matches(5, 3, 33) && a.matches_arith(Arith::F32, 5, 3, 33));
        for word in &a.input {
            let x = f32::from_bits(*word).abs();
            assert!((0.5..4.0).contains(&x), "{x}");
        }
        // Recompute job 0 without fusing: some cell must differ, or the oracle is not an fma.
        let (n, k) = (a.n, a.k);
        let job = &a.input[..2 * n * k];
        let mut unfused = vec![0.0_f32; n * n];
        let mut fused = vec![0.0_f32; n * n];
        for row in 0..n {
            for inner in 0..k {
                let left = f32::from_bits(job[row * k + inner]);
                for col in 0..n {
                    let right = f32::from_bits(job[n * k + inner * n + col]);
                    unfused[row * n + col] += left * right;
                    fused[row * n + col] = left.mul_add(right, fused[row * n + col]);
                }
            }
        }
        let oracle: Vec<u32> = a.dot[..n * n].to_vec();
        assert_eq!(
            oracle,
            fused.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
        assert!(
            unfused
                .iter()
                .zip(&fused)
                .any(|(u, f)| u.to_bits() != f.to_bits())
        );
        assert!(a.integrity_check().is_ok());
    }

    #[test]
    fn a_corpus_rebuilt_from_its_parts_has_the_same_identity() {
        for arith in [Arith::U32, Arith::F32, Arith::I8] {
            let built = Corpus::new_arith(arith, 12, 3, 64, 1).unwrap();
            let again =
                Corpus::from_parts(arith, 12, 3, 64, built.input.clone(), built.dot.clone())
                    .unwrap();
            assert_eq!(
                (again.sha256, again.dot_sha256),
                (built.sha256.clone(), built.dot_sha256.clone())
            );
            assert_eq!(again.n, built.n);
            let mut short = built.input.clone();
            short.pop();
            assert!(Corpus::from_parts(arith, 12, 3, 64, short, built.dot.clone()).is_err());
        }
    }

    #[test]
    fn i8_corpora_match_an_independent_i64_reference() {
        let c = Corpus::new_arith(Arith::I8, 9, 2, 64, 2).unwrap();
        assert_eq!((c.n, c.input.len()), (I8_N, 2 * I8_N * 64 / 2));
        let byte = |word: u32, i: usize| i64::from(word.to_le_bytes()[i].cast_signed());
        let words = c.k / 4;
        for job in 0..c.jobs {
            let base = job * 2 * c.n * words;
            for row in 0..c.n {
                for col in 0..c.n {
                    let mut sum = 0_i64;
                    for kk in 0..c.k {
                        let a = byte(c.input[base + row * words + kk / 4], kk % 4);
                        let b = byte(c.input[base + c.n * words + col * words + kk / 4], kk % 4);
                        sum += a * b;
                    }
                    let expected = i32::try_from(sum).unwrap().cast_unsigned();
                    assert_eq!(
                        c.dot[(job * c.n + row) * c.n + col],
                        expected,
                        "{job} {row} {col}"
                    );
                }
            }
        }
        assert!(
            Corpus::new_arith(Arith::I8, 9, 2, 48, 1).is_err(),
            "k must divide by 32"
        );
        assert_eq!(ArithMix::All.for_rotation(2, true), Arith::I8);
        assert_eq!(
            ArithMix::All.for_rotation(2, false),
            Arith::U32,
            "no tensor: skip INT8"
        );
        assert_eq!(ArithMix::Alternate.for_rotation(1, true), Arith::F32);
    }

    #[test]
    #[ignore = "timing of a menu-sized FP32 corpus build"]
    fn f32_corpus_build_time_at_menu_size() {
        let started = std::time::Instant::now();
        let c = Corpus::new_arith(Arith::F32, 7, 512, 1024, 4).unwrap();
        println!(
            "F32 corpus 512 jobs, K=1024, 4 threads: {:.2} s (oracle {:.2} s)",
            started.elapsed().as_secs_f64(),
            c.oracle_seconds
        );
        let started = std::time::Instant::now();
        let c = Corpus::new_arith(Arith::U32, 7, 512, 1024, 4).unwrap();
        println!(
            "U32 corpus same size: {:.2} s (oracle {:.2} s)",
            started.elapsed().as_secs_f64(),
            c.oracle_seconds
        );
    }
    #[test]
    fn independent_python_golden() {
        // Python arbitrary-precision dot products, reduce modulo 2^32 at end;
        // fixture seed=2, N=65, K=17, jobs=1, generation=(1<<40)+7.
        let corpus = Corpus::new(2, 1, 17).unwrap();
        assert_eq!(
            corpus.sha256,
            "83e8038ea47ec025453d1837347fad1c4f651f5b2389add13e87a70966f30bdb"
        );
        let bytes: Vec<u8> = corpus
            .expected((1_u64 << 40) + 7)
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        assert_eq!(
            format!("{:x}", Sha256::digest(bytes)),
            "639ac80b42278826e1b68242808d4bce86d46672cd993ab33fcaa6b8d51d86de"
        );
    }

    #[test]
    fn direct_verifier_checks_all_output_regions_without_expected_allocation() {
        let corpus = Corpus::new(2, 3, 17).unwrap();
        let generation = (1_u64 << 40) + 7;
        let clean: Vec<u8> = corpus
            .expected(generation)
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        assert!(corpus.verify_bytes(generation, &clean).is_ok());
        for word in [0, 1, 2, 65 * 65 + 2, clean.len() / 4 - 1] {
            let mut bad = clean.clone();
            bad[word * 4] ^= 1;
            assert_eq!(corpus.verify_bytes(generation, &bad).unwrap_err().0, word);
        }
        assert!(corpus.verify_bytes(generation + 1, &clean).is_err());
        assert!(
            corpus
                .verify_bytes(generation, &clean[..clean.len() - 1])
                .is_err()
        );
    }
    #[test]
    fn scalar_oracle_tail_overflow_and_generation_tags() {
        for seed in 0..3 {
            let c = Corpus::new(seed, 1, 17).unwrap();
            for (row, col) in [(0, 0), (c.n - 1, c.n - 1), (7, 13)] {
                let expected = (0..c.k).fold(0_u32, |sum, k| {
                    sum.wrapping_add(
                        c.input[row * c.k + k].wrapping_mul(c.input[c.n * c.k + k * c.n + col]),
                    )
                });
                assert_eq!(c.dot[row * c.n + col], expected);
            }
            assert_ne!(c.expected(0), c.expected(1));
            assert_ne!(c.expected(0), c.expected(1_u64 << 32));
            let mut wrong = c.expected(0);
            wrong[2] ^= 1;
            assert_ne!(wrong, c.expected(0));
            assert_eq!(c.sha256, Corpus::new(seed, 1, 17).unwrap().sha256);
        }
        assert!(Corpus::new(0, 0, 1).is_err());
        assert!(Corpus::new(0, 1, 0).is_err());
    }

    #[test]
    fn mismatch_summary_counts_every_bad_word_and_agrees_with_first_mismatch() {
        let corpus = Corpus::new(2, 3, 17).unwrap();
        let generation = 9;
        let clean: Vec<u8> = corpus
            .expected(generation)
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect();
        assert_eq!(
            corpus.mismatch_summary(generation, &clean, 4),
            MismatchSummary::default()
        );
        let mut bad = clean.clone();
        let hits = [
            1_usize,
            2,
            65 * 65 + 3,
            clean.len() / 4 - 1,
            clean.len() / 4 - 2,
        ];
        for &word in &hits {
            bad[word * 4 + 1] ^= 0x40;
        }
        let summary = corpus.mismatch_summary(generation, &bad, 3);
        assert_eq!(summary.words, hits.len() as u64);
        assert_eq!(summary.samples.len(), 3, "samples are capped");
        let first = corpus.verify_bytes(generation, &bad).unwrap_err();
        assert_eq!(summary.samples[0], first);
        assert!(!summary.length_mismatch);
        // A truncated buffer is reported, with the missing tail counted as differing.
        let short = &clean[..clean.len() - 8];
        let truncated = corpus.mismatch_summary(generation, short, 4);
        assert!(truncated.length_mismatch);
        assert_eq!(truncated.words, 2);
        // Wrong generation: every header word differs, never silently zero.
        assert!(corpus.mismatch_summary(generation + 1, &clean, 1).words > 0);
    }

    /// The pre-H2 word-by-word definition, kept as the differential oracle.
    fn reference_verify(
        c: &Corpus,
        generation: u64,
        bytes: &[u8],
    ) -> Result<(), (usize, u32, u32)> {
        let stride = c.n * c.n + 4;
        if bytes.len() != c.jobs * stride * 4 {
            return Err((usize::MAX, 0, 0));
        }
        for (index, word) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let expected = c.expected_word(stride, index, generation);
            let actual = u32::from_le_bytes(*word);
            if expected != actual {
                return Err((index, expected, actual));
            }
        }
        Ok(())
    }

    #[test]
    #[allow(clippy::cast_possible_truncation)]
    fn fast_verifier_agrees_with_the_word_by_word_definition_on_random_faults() {
        let mut state = 0x243F_6A88_85A3_08D3_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for seed in 0..3_u64 {
            let c = Corpus::new(seed, 5, 9).unwrap();
            for generation in [0_u64, 7, (1 << 40) + 3, u64::MAX] {
                let clean: Vec<u8> = c
                    .expected(generation)
                    .iter()
                    .flat_map(|w| w.to_le_bytes())
                    .collect();
                assert_eq!(c.verify_bytes(generation, &clean), Ok(()));
                for round in 0..400 {
                    let mut bad = clean.clone();
                    // 1..=4 corruptions anywhere: headers, payload, block edges, guards.
                    let hits = 1 + next() % 4;
                    for _ in 0..hits {
                        let word = match next() % 4 {
                            0 => (next() as usize % 5) * (c.n * c.n + 4) + (next() as usize % 2),
                            1 => {
                                (next() as usize % 5 + 1) * (c.n * c.n + 4)
                                    - 1
                                    - (next() as usize % 2)
                            }
                            _ => next() as usize % (clean.len() / 4),
                        };
                        bad[word * 4 + (next() as usize % 4)] ^= 1 << (next() % 8);
                    }
                    assert_eq!(
                        c.verify_bytes(generation, &bad),
                        reference_verify(&c, generation, &bad),
                        "seed={seed} generation={generation} round={round}"
                    );
                }
                assert_eq!(
                    c.verify_bytes(generation, &clean[..clean.len() - 4]),
                    reference_verify(&c, generation, &clean[..clean.len() - 4])
                );
            }
        }
    }

    #[test]
    fn helper_threads_never_change_the_corpus() {
        for (seed, jobs, k) in [(0, 1, 1), (1, 5, 17), (2, 7, 40), (5, 33, 3)] {
            let serial = Corpus::new(seed, jobs, k).unwrap();
            for threads in [2, 3, 8] {
                let parallel = Corpus::new_with_threads(seed, jobs, k, threads).unwrap();
                assert_eq!(serial.sha256, parallel.sha256, "threads={threads}");
                assert!(serial.input == parallel.input && serial.dot == parallel.dot);
            }
        }
        // Large enough to take the threaded input-fill path.
        let serial = Corpus::new(4, 24, 64).unwrap();
        let parallel = Corpus::new_with_threads(4, 24, 64, 5).unwrap();
        assert!(serial.input.len() >= 1 << 16);
        assert!(serial.sha256 == parallel.sha256 && serial.dot == parallel.dot);
    }

    /// `cargo test -p gpu-engine --release -- --ignored --nocapture verifier_speed`
    #[test]
    #[ignore = "timing measurement, not a correctness gate"]
    #[allow(clippy::cast_precision_loss)]
    fn verifier_speed() {
        let c = Corpus::new(2, 1024, 1).unwrap();
        let bytes: Vec<u8> = c.expected(5).iter().flat_map(|w| w.to_le_bytes()).collect();
        let mib = bytes.len() as f64 / 1e6;
        let reps = 40;
        let time = |f: &dyn Fn()| {
            let t = Instant::now();
            for _ in 0..reps {
                f();
            }
            t.elapsed().as_secs_f64() / f64::from(reps)
        };
        let fast = time(&|| c.verify_bytes(5, &bytes).unwrap());
        let slow = time(&|| reference_verify(&c, 5, &bytes).unwrap());
        println!("buffer {mib:.1} MB");
        println!(
            "fast verify_bytes : {:.2} ms ({:.0} MB/s)",
            fast * 1e3,
            mib / fast
        );
        println!(
            "word-by-word ref  : {:.2} ms ({:.0} MB/s)",
            slow * 1e3,
            mib / slow
        );
        println!("speedup {:.1}x", slow / fast);
        let cold_serial = Instant::now();
        let a = Corpus::new(1, 256, 512).unwrap();
        let serial = cold_serial.elapsed().as_secs_f64();
        let cold_par = Instant::now();
        let b = Corpus::new_with_threads(1, 256, 512, 4).unwrap();
        let parallel = cold_par.elapsed().as_secs_f64();
        assert_eq!(a.sha256, b.sha256);
        println!(
            "corpus 256 jobs K=512: serial {serial:.2}s, 4 threads {parallel:.2}s ({:.1}x)",
            serial / parallel
        );
    }

    #[test]
    fn rotated_corpus_seeds_are_replayable_distinct_and_vary_the_dimension() {
        assert_eq!(derive_corpus_seed(7, 0), 7);
        let seeds: Vec<u64> = (0..64).map(|r| derive_corpus_seed(7, r)).collect();
        assert_eq!(
            seeds,
            (0..64)
                .map(|r| derive_corpus_seed(7, r))
                .collect::<Vec<_>>()
        );
        let mut unique = seeds.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 64);
        let dims: std::collections::BTreeSet<u64> = seeds.iter().map(|s| 63 + s % 3).collect();
        assert_eq!(dims.len(), 3, "rotation must reach N = 63, 64 and 65");
        assert_ne!(derive_corpus_seed(7, 5), derive_corpus_seed(8, 5));
    }

    #[test]
    fn integrity_check_detects_a_flipped_bit_in_input_or_expected_results() {
        let mut c = Corpus::new(1, 3, 9).unwrap();
        assert_eq!(c.integrity_check(), Ok(()));
        let clean_dot = c.dot[5];
        c.dot[5] ^= 1 << 17;
        assert_eq!(
            c.integrity_check(),
            Err("expected results changed in host memory")
        );
        c.dot[5] = clean_dot;
        assert_eq!(c.integrity_check(), Ok(()));
        let last = c.input.len() - 1;
        c.input[last] ^= 1;
        assert_eq!(
            c.integrity_check(),
            Err("corpus input changed in host memory")
        );
        c.dot[0] ^= 1;
        assert_eq!(
            c.integrity_check(),
            Err("corpus input and expected results changed in host memory")
        );
        // A threaded build records the same hashes as a serial one.
        let a = Corpus::new(2, 6, 11).unwrap();
        let b = Corpus::new_with_threads(2, 6, 11, 4).unwrap();
        assert_eq!((a.sha256, a.dot_sha256), (b.sha256, b.dot_sha256));
    }
}
