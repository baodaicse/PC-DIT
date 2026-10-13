//! Standalone sustained compute-oracle (ADR-0004, Phase 1).
//!
//! Runs independently of the RAM DIT route in `engine.rs`. It exists to catch
//! silent compute corruption (Raptor Lake-class instability) that a
//! memory-bandwidth-bound workload does not exercise: a dense sustained FMA
//! kernel, a branch-heavy integer kernel and a hardware AES-256 known-answer
//! check, each with an independent reference so an optimized/hardware path can
//! never validate itself.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use aes::{
    Aes256,
    cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray},
};
use serde::{Deserialize, Serialize};
use wide::f64x4;

use crate::{
    SystemDitError,
    cpu::{compute_order, pin_current_thread},
    inventory::SystemInventory,
    safety::RunControl,
};

const FMA_LANES: usize = 4;
/// Contraction factor keeps the sustained FMA recurrence bounded (no inf/NaN).
const FMA_MULTIPLIER: f64 = 0.999_999_1;
pub(crate) const BRANCH_PREDICTOR_LENGTH: u32 = 50_000;
const FMA_TOLERANCE: f64 = 1e-9;

// FIPS-197 Appendix C.3 AES-256 known-answer vector.
const AES256_KEY: [u8; 32] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
const AES256_PLAINTEXT: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];
const AES256_CIPHERTEXT: [u8; 16] = [
    0x8e, 0xa2, 0xb7, 0xca, 0x51, 0x67, 0x45, 0xbf, 0xea, 0xfc, 0x49, 0x90, 0x4b, 0x49, 0x60, 0x89,
];

fn splitmix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Deterministic bounded lanes seeded from `seed`, used as the FMA recurrence start.
fn fma_seed_lanes(seed: u64) -> ([f64; FMA_LANES], [f64; FMA_LANES]) {
    let mut acc = [0.0_f64; FMA_LANES];
    let mut add = [0.0_f64; FMA_LANES];
    let mut state = seed;
    for lane in 0..FMA_LANES {
        state = splitmix64(state);
        // Keep magnitudes small and finite across millions of iterations.
        acc[lane] = f64::from((state & 0xffff) as u32) / 65_536.0;
        state = splitmix64(state);
        add[lane] = f64::from((state & 0xffff) as u32) / 1_048_576.0;
    }
    (acc, add)
}

/// Sustained dense FMA kernel run through the safe SIMD `wide` path.
///
/// Uses hardware `vfmadd` when built with the `fma` target feature, otherwise
/// falls back to AVX multiply+add; either way it is a real dense compute pass,
/// not a one-shot check.
fn dense_fma_pass(seed: u64, iterations: u32) -> [u64; FMA_LANES] {
    let (acc_start, add_start) = fma_seed_lanes(seed);
    let mut acc = f64x4::new(acc_start);
    let multiplier = f64x4::splat(FMA_MULTIPLIER);
    let add = f64x4::new(add_start);
    for _ in 0..iterations {
        acc = acc.mul_add(multiplier, add);
    }
    acc.to_array().map(f64::to_bits)
}

/// Plain scalar reference for the same recurrence, always IEEE-754 fused
/// (`f64::mul_add` is correctly rounded regardless of target features), used
/// as an independent tolerance-checked oracle per `PRODUCT_SPEC.md` section 3.
pub(crate) fn dense_fma_scalar_reference(seed: u64, iterations: u32) -> [f64; FMA_LANES] {
    let (mut acc, add) = fma_seed_lanes(seed);
    for _ in 0..iterations {
        for lane in 0..FMA_LANES {
            acc[lane] = acc[lane].mul_add(FMA_MULTIPLIER, add[lane]);
        }
    }
    acc
}

pub(crate) const HEAVY_ACCUMULATORS: usize = 8;

/// Separate scalar recurrences are the oracle for every accumulator and lane.
pub(crate) fn heavy_fp_reference(
    seed: u64,
    iterations: u32,
) -> [[f64; FMA_LANES]; HEAVY_ACCUMULATORS] {
    std::array::from_fn(|i| dense_fma_scalar_reference(seed.wrapping_add(i as u64), iterations))
}

/// Eight independent vector dependency chains. This is a portable SIMD workload,
/// not an assertion that a particular FMA/AVX opcode or peak power was achieved.
fn heavy_fp_pass(seed: u64, iterations: u32) -> [[u64; FMA_LANES]; HEAVY_ACCUMULATORS] {
    fma_chains_pass(seed, iterations, FMA_MULTIPLIER)
}

/// `CHAINS` independent vector FMA recurrences, chain `i` seeded from `seed + i`: while one
/// chain waits for its previous result the core works on the others, so the FMA units stay busy
/// instead of idling on one dependency chain's latency.
fn fma_chains_pass<const CHAINS: usize>(
    seed: u64,
    iterations: u32,
    multiplier: f64,
) -> [[u64; FMA_LANES]; CHAINS] {
    let pairs: [_; CHAINS] = std::array::from_fn(|i| fma_seed_lanes(seed.wrapping_add(i as u64)));
    let mut acc = pairs.map(|(initial, _)| f64x4::new(initial));
    let add = pairs.map(|(_, add)| f64x4::new(add));
    let multiplier = f64x4::splat(multiplier);
    for _ in 0..iterations {
        for i in 0..CHAINS {
            acc[i] = acc[i].mul_add(multiplier, add[i]);
        }
    }
    std::hint::black_box(acc.map(|value| value.to_array().map(f64::to_bits)))
}

/// The compute kernel of CPU-RAM and of the standalone compute oracle since v1.2 (C3):
/// `FMA_CHAINS` independent vector FMA chains, short bursts, every lane checked bit for bit.
pub const FMA_KERNEL_ID: &str = "CPU-DIT-FMA-CHAINS-v1";

/// Independent chains per burst: as many as keep the FMA units busy without running out of
/// registers (measured on a Ryzen 7 3700X: 6 chains reach 31 GFLOP/s with SSE2 while 8 spill and
/// fall to 12; with AVX2+FMA 10 chains reach 64 GFLOP/s, about 95% of the core's peak).
#[cfg(target_feature = "fma")]
pub const FMA_CHAINS: usize = 10;
/// See the FMA build above.
#[cfg(not(target_feature = "fma"))]
pub const FMA_CHAINS: usize = 6;

/// Iterations per burst: about 100 ms on a desktop core (64 ms to 80 ms on the 3700X), so a
/// result is checked several times a second.
pub const FMA_ITERATIONS_PER_BURST: u32 = 64_000_000;

/// `1 - 2^-27`. The recurrence `x = m x + a` must stay bounded (it tends to `a / (1 - m)`,
/// at most about 8.4e6 here) yet forget slowly: with the multiplier of the older kernel
/// (`1 - 9e-7`), a wrong result was forgotten within about a million iterations, so only the end
/// of a burst could show it. With `1 - 2^-27` a wrong value still weighs `m^n > 0.6` after a whole
/// burst.
const CHAIN_MULTIPLIER: f64 = 0.999_999_992_549_419_4;

/// One step of a chain lane, rounded exactly as the vector path rounds it: one fused rounding
/// when the build has FMA, a multiply then an add otherwise. IEEE-754 makes either exact, so the
/// burst is checked bit for bit, not within a tolerance.
#[inline]
fn chain_step(acc: f64, add: f64) -> f64 {
    #[cfg(target_feature = "fma")]
    {
        acc.mul_add(CHAIN_MULTIPLIER, add)
    }
    #[cfg(not(target_feature = "fma"))]
    {
        acc * CHAIN_MULTIPLIER + add
    }
}

/// One burst of the C3 kernel on the vector path.
fn fma_chain_burst(seed: u64, iterations: u32) -> [[u64; FMA_LANES]; FMA_CHAINS] {
    fma_chains_pass(seed, iterations, CHAIN_MULTIPLIER)
}

/// What every burst of `(seed, iterations)` must produce: worked out once, lane by lane in plain
/// scalar code (not the vector path it checks), one thread per chain.
#[must_use]
pub fn fma_chain_reference(seed: u64, iterations: u32) -> [[u64; FMA_LANES]; FMA_CHAINS] {
    let mut reference = [[0_u64; FMA_LANES]; FMA_CHAINS];
    thread::scope(|scope| {
        for (chain, result) in reference.iter_mut().enumerate() {
            scope.spawn(move || {
                let (mut acc, add) = fma_seed_lanes(seed.wrapping_add(chain as u64));
                for _ in 0..iterations {
                    for lane in 0..FMA_LANES {
                        acc[lane] = chain_step(acc[lane], add[lane]);
                    }
                }
                *result = std::hint::black_box(acc).map(f64::to_bits);
            });
        }
    });
    reference
}

/// The chains' final bits folded to one lane set (XOR), compared across workers.
fn fold_chains(chains: &[[u64; FMA_LANES]; FMA_CHAINS]) -> [u64; FMA_LANES] {
    chains.iter().fold([0; FMA_LANES], |folded, chain| {
        std::array::from_fn(|lane| folded[lane] ^ chain[lane])
    })
}

pub(crate) fn heavy_fp_mismatches(
    seed: u64,
    iterations: u32,
    reference: &[[f64; FMA_LANES]; HEAVY_ACCUMULATORS],
    fault_accumulator: Option<usize>,
) -> u64 {
    let mut actual = heavy_fp_pass(std::hint::black_box(seed), iterations);
    if let Some(index) = fault_accumulator {
        actual[index][0] = f64::NAN.to_bits();
    }
    actual
        .iter()
        .zip(reference)
        .filter(|(actual, expected)| !dense_fma_within_tolerance(actual, expected))
        .count() as u64
}

/// Deterministic branch-heavy integer kernel (ported from the retired Python
/// `CPU-DIT` prototype's `test_branch_predictor.py`, rewritten for Rust/no
/// GIL). Result is bit-exact and reproducible for a given `(seed, length)`.
#[allow(clippy::cast_possible_truncation)] // Low 32 bits of the seed are the intended PRNG state.
pub(crate) fn branch_predictor_pass(seed: u64, length: u32) -> u64 {
    let mut state = seed;
    let mut acc: u64 = 0;
    let mut x: u32 = seed as u32;
    for i in 0..length {
        state = splitmix64(state);
        match state & 7 {
            0 => {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                acc = acc.wrapping_add(u64::from(x));
            }
            1 => {
                x ^= x >> 13;
                acc = acc.wrapping_sub(u64::from(x));
            }
            2 => {
                x = x.wrapping_mul(x).wrapping_add(12345);
                acc ^= u64::from(x);
            }
            3 => {
                x = x.wrapping_add(0xdead);
                acc |= u64::from(x & 0xff);
            }
            4 => {
                x = x.rotate_right(1);
                acc &= u64::from(x | 0xffff_0000);
            }
            5 => {
                x = x.wrapping_mul(69069).wrapping_add(1);
                acc = if x & 1 == 1 {
                    acc.wrapping_add(u64::from(i))
                } else {
                    acc.wrapping_sub(u64::from(i))
                };
            }
            6 => {
                for shift in 1..=4 {
                    x ^= x << shift;
                }
                acc = acc.wrapping_add(u64::from(x));
            }
            _ => {
                x = !x;
                acc = acc.wrapping_add(u64::from(x));
            }
        }
    }
    acc
}

/// True when every dense-FMA lane's SIMD result stays within `FMA_TOLERANCE`
/// of the independent scalar (non-vectorized) reference for the same seed.
fn dense_fma_within_tolerance(actual_bits: &[u64; FMA_LANES], expected: &[f64; FMA_LANES]) -> bool {
    actual_bits
        .iter()
        .zip(expected.iter())
        .all(|(&actual_bits, &expected)| {
            let actual = f64::from_bits(actual_bits);
            let rel_error = if expected == 0.0 {
                actual.abs()
            } else {
                ((actual - expected) / expected).abs()
            };
            rel_error <= FMA_TOLERANCE
        })
}

/// AES-256 known-answer test against the platform's hardware path (the `aes`
/// crate autodetects AES-NI on `x86_64`); ported from the retired prototype's
/// planned "Hardware crypto correctness" idea.
fn aes256_known_answer() -> bool {
    let Ok(cipher) = Aes256::new_from_slice(&AES256_KEY) else {
        return false;
    };
    let mut block = *GenericArray::from_slice(&AES256_PLAINTEXT);
    cipher.encrypt_block(&mut block);
    block.as_slice() == AES256_CIPHERTEXT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputeOracleStatus {
    Pass,
    Fail,
    Incomplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeOracleConfig {
    pub test_id: String,
    pub requested_workers: usize,
    pub seed: u64,
    pub minimum_duration_ms: u64,
    pub dense_fma_iterations_per_burst: u32,
    pub idle_ramp_enabled: bool,
    pub idle_sleep_ms: u64,
    /// Dev/test-only: force one worker's dense-FMA result to be wrong, to
    /// prove the cross-worker oracle cannot be fooled (`TEST_AND_ACCEPTANCE.md`).
    pub fault_injection_worker: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeOracleWorkerEvidence {
    pub worker_id: usize,
    pub cpu_id: usize,
    pub affinity_failed: bool,
    pub bursts_completed: u64,
    pub dense_fma_final_bits: [u64; FMA_LANES],
    pub dense_fma_mismatches: u64,
    pub branch_predictor_final: u64,
    pub branch_predictor_mismatches: u64,
    pub aes256_mismatches: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeOracleMismatch {
    pub kernel: String,
    pub worker_id: Option<usize>,
    pub cpu_id: Option<usize>,
    pub detail: String,
}

#[allow(clippy::struct_excessive_bools)] // Independent oracle results stay explicit in JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeOracleReport {
    pub schema_version: u32,
    /// DIT-01 version that wrote this report (absent before v1.2: v1.1).
    #[serde(default)]
    pub dit_version: String,
    pub test_id: String,
    pub status: ComputeOracleStatus,
    pub availability: crate::availability::AvailabilityEvidence,
    pub workers_requested: usize,
    pub coverage_met: bool,
    pub minimum_bursts_per_worker: u64,
    pub seed_hex: String,
    pub workers_used: usize,
    pub worker_cpu_ids: Vec<usize>,
    pub elapsed_ms: u128,
    /// The FMA kernel the bursts ran (`CPU-DIT-FMA-CHAINS-v1` since v1.2, checked bit for bit);
    /// empty in older reports: one chain, checked within a tolerance.
    #[serde(default)]
    pub fma_kernel: String,
    /// Independent FMA chains per burst (1 in older reports).
    #[serde(default = "crate::report::one_chain")]
    pub fma_chains: usize,
    pub idle_ramp_cycles: u64,
    /// Resolved value actually used (explicit override or duration-derived
    /// ramp); replay only needs this field, never the original duration.
    pub dense_fma_iterations_per_burst: u32,
    pub dense_fma_cross_worker_consistent: bool,
    pub dense_fma_scalar_reference_passed: bool,
    pub branch_predictor_passed: bool,
    pub aes256_known_answer_passed: bool,
    pub workers: Vec<ComputeOracleWorkerEvidence>,
    pub mismatch: Option<ComputeOracleMismatch>,
}

pub(crate) struct WorkerOutcome {
    pub(crate) worker_id: usize,
    pub(crate) cpu_id: usize,
    pub(crate) affinity_failed: bool,
    pub(crate) bursts_completed: u64,
    pub(crate) idle_ramp_cycles: u64,
    pub(crate) dense_fma_final_bits: [u64; FMA_LANES],
    pub(crate) dense_fma_mismatches: u64,
    pub(crate) branch_predictor_final: u64,
    pub(crate) branch_predictor_mismatches: u64,
    pub(crate) aes256_mismatches: u64,
    pub(crate) panicked: bool,
}

pub(crate) fn minimum_work_met(outcomes: &[WorkerOutcome], requested: usize, idle: bool) -> bool {
    requested > 0
        && outcomes.len() == requested
        && outcomes.iter().all(|o| {
            o.bursts_completed > 0
                && !o.affinity_failed
                && !o.panicked
                && (!idle || o.idle_ramp_cycles > 0)
        })
}

pub(crate) struct BurstOutcome {
    dense_fma_final_bits: [u64; FMA_LANES],
    pub(crate) dense_fma_mismatch: bool,
    branch_predictor_final: u64,
    pub(crate) branch_mismatch: bool,
    pub(crate) aes_mismatch: bool,
}

/// Run one burst of all three kernels; shared by the standalone deadline loop
/// and the `RunControl`-aware loop used when coordinated with the RAM route.
/// Every burst is checked against its independent reference here (not just
/// the last one), so a transient mid-session mismatch is never missed.
pub(crate) fn run_burst(
    seed: u64,
    dense_fma_iterations: u32,
    dense_fma_scalar_reference: &[f64; FMA_LANES],
    branch_predictor_reference: u64,
) -> BurstOutcome {
    let dense_fma_final_bits = dense_fma_pass(seed, dense_fma_iterations);
    let branch_predictor_final = branch_predictor_pass(seed, BRANCH_PREDICTOR_LENGTH);
    BurstOutcome {
        dense_fma_final_bits,
        dense_fma_mismatch: !dense_fma_within_tolerance(
            &dense_fma_final_bits,
            dense_fma_scalar_reference,
        ),
        branch_predictor_final,
        branch_mismatch: branch_predictor_final != branch_predictor_reference,
        aes_mismatch: !aes256_known_answer(),
    }
}

/// One burst of the CPU-RAM and standalone compute oracle (C3 kernel, branch kernel, AES
/// known answer), every result checked against its reference.
pub(crate) fn run_compute_burst(
    seed: u64,
    fma_iterations: u32,
    fma_reference: &[[u64; FMA_LANES]; FMA_CHAINS],
    branch_predictor_reference: u64,
) -> BurstOutcome {
    let chains = fma_chain_burst(std::hint::black_box(seed), fma_iterations);
    let branch_predictor_final = branch_predictor_pass(seed, BRANCH_PREDICTOR_LENGTH);
    BurstOutcome {
        dense_fma_final_bits: fold_chains(&chains),
        dense_fma_mismatch: chains != *fma_reference,
        branch_predictor_final,
        branch_mismatch: branch_predictor_final != branch_predictor_reference,
        aes_mismatch: !aes256_known_answer(),
    }
}

fn worker_loop(
    worker_id: usize,
    cpu_id: usize,
    config: &ComputeOracleConfig,
    fma_reference: &[[u64; FMA_LANES]; FMA_CHAINS],
    branch_predictor_reference: u64,
    deadline: Instant,
) -> WorkerOutcome {
    let affinity_failed = pin_current_thread(cpu_id).is_err();
    let mut bursts_completed = 0_u64;
    let mut idle_ramp_cycles = 0_u64;
    let mut dense_fma_mismatches = 0_u64;
    let mut branch_predictor_mismatches = 0_u64;
    let mut aes256_mismatches = 0_u64;

    let (mut dense_fma_final_bits, mut branch_predictor_final);
    loop {
        if config.idle_ramp_enabled {
            thread::sleep(Duration::from_millis(config.idle_sleep_ms));
            idle_ramp_cycles += 1;
        }
        let burst = run_compute_burst(
            config.seed,
            config.dense_fma_iterations_per_burst,
            fma_reference,
            branch_predictor_reference,
        );
        dense_fma_final_bits = burst.dense_fma_final_bits;
        branch_predictor_final = burst.branch_predictor_final;
        dense_fma_mismatches += u64::from(burst.dense_fma_mismatch);
        branch_predictor_mismatches += u64::from(burst.branch_mismatch);
        aes256_mismatches += u64::from(burst.aes_mismatch);
        bursts_completed += 1;
        if Instant::now() >= deadline {
            break;
        }
    }

    if config.fault_injection_worker == Some(worker_id) {
        dense_fma_final_bits[0] ^= 1;
    }

    WorkerOutcome {
        worker_id,
        cpu_id,
        affinity_failed,
        bursts_completed,
        idle_ramp_cycles,
        dense_fma_final_bits,
        dense_fma_mismatches,
        branch_predictor_final,
        branch_predictor_mismatches,
        aes256_mismatches,
        panicked: false,
    }
}

/// Phase 2 (ADR-0004): run one compute-oracle worker for as long as the RAM
/// route's round loop is active, sharing its cooperative stop token so a
/// thermal/EDAC/deadline abort stops both routes together. `finished` is set
/// by the caller once the RAM round loop completes normally.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_worker_with_control(
    worker_id: usize,
    cpu_id: usize,
    seed: u64,
    dense_fma_iterations: u32,
    idle_ramp_enabled: bool,
    idle_sleep_ms: u64,
    fault_injection_worker: Option<usize>,
    fma_reference: [[u64; FMA_LANES]; FMA_CHAINS],
    branch_predictor_reference: u64,
    control: &RunControl,
    finished: &AtomicBool,
) -> WorkerOutcome {
    let affinity_failed = pin_current_thread(cpu_id).is_err();
    let mut bursts_completed = 0_u64;
    let mut idle_ramp_cycles = 0_u64;
    let mut dense_fma_mismatches = 0_u64;
    let mut branch_predictor_mismatches = 0_u64;
    let mut aes256_mismatches = 0_u64;
    let mut dense_fma_final_bits = [0_u64; FMA_LANES];
    let mut branch_predictor_final = 0_u64;

    while !control.should_stop() && !finished.load(Ordering::Acquire) {
        if idle_ramp_enabled {
            thread::sleep(Duration::from_millis(idle_sleep_ms));
            idle_ramp_cycles += 1;
        }
        let burst = run_compute_burst(
            seed,
            dense_fma_iterations,
            &fma_reference,
            branch_predictor_reference,
        );
        dense_fma_final_bits = burst.dense_fma_final_bits;
        branch_predictor_final = burst.branch_predictor_final;
        dense_fma_mismatches += u64::from(burst.dense_fma_mismatch);
        branch_predictor_mismatches += u64::from(burst.branch_mismatch);
        aes256_mismatches += u64::from(burst.aes_mismatch);
        bursts_completed += 1;
    }

    if fault_injection_worker == Some(worker_id) {
        dense_fma_final_bits[0] ^= 1;
    }

    WorkerOutcome {
        worker_id,
        cpu_id,
        affinity_failed,
        bursts_completed,
        idle_ramp_cycles,
        dense_fma_final_bits,
        dense_fma_mismatches,
        branch_predictor_final,
        branch_predictor_mismatches,
        aes256_mismatches,
        panicked: false,
    }
}

/// Run the standalone sustained compute-oracle described in ADR-0004.
///
/// # Errors
///
/// Returns an error for invalid configuration (zero workers/iterations) or if
/// a worker thread panics.
#[allow(clippy::too_many_lines)] // Keeping oracle aggregation stages together makes audit review direct.
pub fn run_compute_oracle(
    config: &ComputeOracleConfig,
) -> Result<ComputeOracleReport, SystemDitError> {
    if config.dense_fma_iterations_per_burst == 0 {
        return Err(SystemDitError::InvalidConfig(
            "dense_fma_iterations_per_burst must be positive".to_owned(),
        ));
    }

    let inventory = SystemInventory::collect()?;
    // Every worker computes: they take the threads that suit compute best, whole performance
    // cores first (`cpu::compute_order`); zero requested takes every allowed thread.
    let order = compute_order(&inventory.cpu);
    let count = if config.requested_workers == 0 {
        order.len()
    } else {
        config.requested_workers
    };
    let worker_cpus: Vec<usize> = order.into_iter().take(count).collect();
    if worker_cpus.is_empty() {
        return Err(SystemDitError::InvalidConfig(
            "no CPUs are available in the current cpuset".to_owned(),
        ));
    }
    if let Some(worker_id) = config.fault_injection_worker
        && worker_id >= worker_cpus.len()
    {
        return Err(SystemDitError::InvalidConfig(format!(
            "fault injection worker {worker_id} is outside active worker range 0..{}",
            worker_cpus.len()
        )));
    }

    // Independent references computed once, before workers start, mirroring
    // the retired prototype's "compute_reference_results" pattern. The scalar
    // dense-FMA reference is a pure function of (seed, iterations), so every
    // burst is checked against it below instead of only the last one.
    let branch_predictor_reference = branch_predictor_pass(config.seed, BRANCH_PREDICTOR_LENGTH);
    let fma_reference = fma_chain_reference(config.seed, config.dense_fma_iterations_per_burst);

    let started = Instant::now();
    let deadline = started + Duration::from_millis(config.minimum_duration_ms);

    let mut handles = Vec::with_capacity(worker_cpus.len());
    for (worker_id, &cpu_id) in worker_cpus.iter().enumerate() {
        let config = config.clone();
        handles.push(thread::spawn(move || {
            catch_unwind(AssertUnwindSafe(|| {
                worker_loop(
                    worker_id,
                    cpu_id,
                    &config,
                    &fma_reference,
                    branch_predictor_reference,
                    deadline,
                )
            }))
        }));
    }

    let mut outcomes = Vec::with_capacity(handles.len());
    for handle in handles {
        match handle.join() {
            Ok(Ok(outcome)) => outcomes.push(outcome),
            Ok(Err(_)) | Err(_) => outcomes.push(WorkerOutcome {
                worker_id: outcomes.len(),
                cpu_id: 0,
                affinity_failed: true,
                bursts_completed: 0,
                idle_ramp_cycles: 0,
                dense_fma_final_bits: [0; FMA_LANES],
                dense_fma_mismatches: 0,
                branch_predictor_final: 0,
                branch_predictor_mismatches: 0,
                aes256_mismatches: 0,
                panicked: true,
            }),
        }
    }
    outcomes.sort_by_key(|outcome| outcome.worker_id);

    let aggregate = aggregate_outcomes(&outcomes);
    let requested = if config.requested_workers == 0 {
        worker_cpus.len()
    } else {
        config.requested_workers
    };
    let coverage_met = minimum_work_met(&outcomes, requested, config.idle_ramp_enabled);
    let status = if aggregate.mismatch.is_some() {
        ComputeOracleStatus::Fail
    } else if !coverage_met {
        ComputeOracleStatus::Incomplete
    } else {
        ComputeOracleStatus::Pass
    };

    let idle_ramp_cycles = outcomes
        .iter()
        .map(|outcome| outcome.idle_ramp_cycles)
        .sum();
    let workers = outcomes
        .iter()
        .map(|outcome| ComputeOracleWorkerEvidence {
            worker_id: outcome.worker_id,
            cpu_id: outcome.cpu_id,
            affinity_failed: outcome.affinity_failed,
            bursts_completed: outcome.bursts_completed,
            dense_fma_final_bits: outcome.dense_fma_final_bits,
            dense_fma_mismatches: outcome.dense_fma_mismatches,
            branch_predictor_final: outcome.branch_predictor_final,
            branch_predictor_mismatches: outcome.branch_predictor_mismatches,
            aes256_mismatches: outcome.aes256_mismatches,
        })
        .collect();

    Ok(ComputeOracleReport {
        // v3: records the resolved dense_fma_iterations_per_burst (DIT-
        // DURATION-COMPLEXITY-RAMP-v1) alongside the v2 per-burst mismatch fix.
        // v5: dit_version. v6: multi-chain FMA kernel (fma_kernel, fma_chains).
        schema_version: 6,
        dit_version: dit_contracts::DIT_VERSION.to_owned(),
        availability: crate::availability::AvailabilityEvidence::new(
            if coverage_met {
                crate::availability::AvailabilityStatus::Completed
            } else {
                crate::availability::AvailabilityStatus::Incomplete
            },
            "minimum one verified burst per requested worker",
        ),
        workers_requested: config.requested_workers,
        coverage_met,
        minimum_bursts_per_worker: 1,
        test_id: config.test_id.clone(),
        status,
        seed_hex: format!("0x{:016x}", config.seed),
        workers_used: worker_cpus.len(),
        worker_cpu_ids: worker_cpus,
        elapsed_ms: started.elapsed().as_millis(),
        fma_kernel: FMA_KERNEL_ID.to_owned(),
        fma_chains: FMA_CHAINS,
        idle_ramp_cycles,
        dense_fma_iterations_per_burst: config.dense_fma_iterations_per_burst,
        dense_fma_cross_worker_consistent: aggregate.dense_fma_cross_worker_consistent,
        dense_fma_scalar_reference_passed: aggregate.dense_fma_scalar_reference_passed,
        branch_predictor_passed: aggregate.branch_predictor_passed,
        aes256_known_answer_passed: aggregate.aes256_known_answer_passed,
        workers,
        mismatch: aggregate.mismatch,
    })
}

/// Aggregate result shared by the standalone report and the Phase 2 system
/// integration; keeps oracle attribution identical in both code paths.
#[allow(clippy::struct_excessive_bools)] // Independent oracle results stay explicit.
pub(crate) struct ComputeOracleAggregate {
    pub(crate) dense_fma_cross_worker_consistent: bool,
    pub(crate) dense_fma_scalar_reference_passed: bool,
    pub(crate) branch_predictor_passed: bool,
    pub(crate) aes256_known_answer_passed: bool,
    pub(crate) mismatch: Option<ComputeOracleMismatch>,
}

pub(crate) fn aggregate_outcomes(outcomes: &[WorkerOutcome]) -> ComputeOracleAggregate {
    let mut mismatch = None;
    let panicked_worker = outcomes.iter().find(|outcome| outcome.panicked);
    if let Some(outcome) = panicked_worker {
        mismatch = Some(ComputeOracleMismatch {
            kernel: "worker_panic".to_owned(),
            worker_id: Some(outcome.worker_id),
            cpu_id: None,
            detail: "compute-oracle worker thread panicked".to_owned(),
        });
    }

    let completed: Vec<_> = outcomes.iter().filter(|o| o.bursts_completed > 0).collect();
    let reference_bits = completed
        .first()
        .map(|outcome| outcome.dense_fma_final_bits);
    let mut dense_fma_cross_worker_consistent = true;
    if let Some(reference_bits) = reference_bits {
        for outcome in &completed[1..] {
            if outcome.dense_fma_final_bits != reference_bits {
                dense_fma_cross_worker_consistent = false;
                if mismatch.is_none() {
                    mismatch = Some(ComputeOracleMismatch {
                        kernel: "dense_fma_cross_worker".to_owned(),
                        worker_id: Some(outcome.worker_id),
                        cpu_id: Some(outcome.cpu_id),
                        detail: format!(
                            "worker {} dense-FMA result diverged from worker 0",
                            outcome.worker_id
                        ),
                    });
                }
            }
        }
    }

    // Checked on every burst of every worker (not just the last one), so a
    // transient mid-session divergence from the independent scalar path is
    // never masked by a later, correctly computed final burst.
    let total_dense_fma_mismatches: u64 = outcomes
        .iter()
        .map(|outcome| outcome.dense_fma_mismatches)
        .sum();
    let dense_fma_scalar_reference_passed = total_dense_fma_mismatches == 0;
    if !dense_fma_scalar_reference_passed && mismatch.is_none() {
        mismatch = Some(ComputeOracleMismatch {
            kernel: "dense_fma_scalar_reference".to_owned(),
            worker_id: None,
            cpu_id: None,
            detail: format!(
                "{total_dense_fma_mismatches} dense-FMA per-burst mismatches against scalar reference"
            ),
        });
    }

    let total_branch_mismatches: u64 = outcomes
        .iter()
        .map(|outcome| outcome.branch_predictor_mismatches)
        .sum();
    let branch_predictor_passed = total_branch_mismatches == 0;
    if !branch_predictor_passed && mismatch.is_none() {
        mismatch = Some(ComputeOracleMismatch {
            kernel: "branch_predictor".to_owned(),
            worker_id: None,
            cpu_id: None,
            detail: format!("{total_branch_mismatches} branch-predictor result mismatches"),
        });
    }

    let total_aes_mismatches: u64 = outcomes
        .iter()
        .map(|outcome| outcome.aes256_mismatches)
        .sum();
    let aes256_known_answer_passed = total_aes_mismatches == 0;
    if !aes256_known_answer_passed && mismatch.is_none() {
        mismatch = Some(ComputeOracleMismatch {
            kernel: "aes256_known_answer".to_owned(),
            worker_id: None,
            cpu_id: None,
            detail: format!("{total_aes_mismatches} AES-256 known-answer mismatches"),
        });
    }

    let any_affinity_failure = outcomes.iter().any(|outcome| outcome.affinity_failed);
    if any_affinity_failure && mismatch.is_none() {
        mismatch = Some(ComputeOracleMismatch {
            kernel: "affinity".to_owned(),
            worker_id: outcomes
                .iter()
                .find(|outcome| outcome.affinity_failed)
                .map(|outcome| outcome.worker_id),
            cpu_id: None,
            detail: "a compute-oracle worker could not pin its requested CPU".to_owned(),
        });
    }

    ComputeOracleAggregate {
        dense_fma_cross_worker_consistent,
        dense_fma_scalar_reference_passed,
        branch_predictor_passed,
        aes256_known_answer_passed,
        mismatch,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BRANCH_PREDICTOR_LENGTH, CHAIN_MULTIPLIER, ComputeOracleConfig, ComputeOracleStatus,
        FMA_CHAINS, aes256_known_answer, branch_predictor_pass, dense_fma_pass,
        dense_fma_scalar_reference, fma_chain_burst, fma_chain_reference, run_compute_burst,
        run_compute_oracle,
    };

    #[test]
    fn chain_kernel_matches_its_scalar_reference_bit_for_bit() {
        assert_eq!(
            CHAIN_MULTIPLIER.to_bits(),
            (1.0 - 1.0 / f64::from(1_u32 << 27)).to_bits()
        );
        for seed in [0, 1, 0x0123_4567_89ab_cdef, u64::MAX] {
            for iterations in [0, 1, 7, 1_000, 250_000] {
                assert_eq!(
                    fma_chain_burst(seed, iterations),
                    fma_chain_reference(seed, iterations),
                    "seed {seed:#x}, {iterations} iterations"
                );
            }
        }
        // Chains are independent: different seeds per chain, different results.
        let chains = fma_chain_reference(5, 1_000);
        for (index, chain) in chains.iter().enumerate() {
            assert!(chains[index + 1..].iter().all(|other| other != chain));
            assert!(chain.iter().all(|bits| f64::from_bits(*bits).is_finite()));
        }
        assert_eq!(chains.len(), FMA_CHAINS);
    }

    #[test]
    fn chain_burst_reports_a_single_wrong_bit() {
        let seed = 0x5eed;
        let iterations = 10_000;
        let branch = branch_predictor_pass(seed, BRANCH_PREDICTOR_LENGTH);
        let reference = fma_chain_reference(seed, iterations);
        assert!(!run_compute_burst(seed, iterations, &reference, branch).dense_fma_mismatch);
        let mut wrong = reference;
        wrong[FMA_CHAINS - 1][3] ^= 1;
        assert!(run_compute_burst(seed, iterations, &wrong, branch).dense_fma_mismatch);
    }

    #[test]
    fn dense_fma_is_deterministic_for_same_seed() {
        assert_eq!(dense_fma_pass(42, 1_000), dense_fma_pass(42, 1_000));
        assert_ne!(dense_fma_pass(42, 1_000), dense_fma_pass(43, 1_000));
    }

    #[test]
    fn heavy_fp_checks_every_accumulator_and_lane_against_scalar() {
        let reference = super::heavy_fp_reference(123, 1000);
        assert_eq!(super::heavy_fp_mismatches(123, 1000, &reference, None), 0);
        for accumulator in 0..super::HEAVY_ACCUMULATORS {
            assert_eq!(
                super::heavy_fp_mismatches(123, 1000, &reference, Some(accumulator)),
                1
            );
            for lane in 0..4 {
                let mut wrong = reference;
                wrong[accumulator][lane] += 1.0;
                assert_eq!(super::heavy_fp_mismatches(123, 1000, &wrong, None), 1);
            }
        }
        // A later healthy chunk cannot erase a transient mismatch.
        let mismatches = super::heavy_fp_mismatches(123, 1000, &reference, Some(0))
            + super::heavy_fp_mismatches(123, 1000, &reference, None);
        assert_eq!(mismatches, 1);
    }

    #[test]
    fn dense_fma_matches_scalar_reference_within_tolerance() {
        let bits = dense_fma_pass(7, 10_000);
        let scalar = dense_fma_scalar_reference(7, 10_000);
        for (actual_bits, expected) in bits.iter().zip(scalar.iter()) {
            let actual = f64::from_bits(*actual_bits);
            assert!((actual - expected).abs() <= expected.abs().max(1.0) * 1e-6);
        }
    }

    #[test]
    fn branch_predictor_is_deterministic() {
        assert_eq!(
            branch_predictor_pass(99, 5_000),
            branch_predictor_pass(99, 5_000)
        );
    }

    #[test]
    fn aes256_known_answer_passes() {
        assert!(aes256_known_answer());
    }

    fn base_config() -> ComputeOracleConfig {
        ComputeOracleConfig {
            test_id: "CPU-DIT-01-COMPUTE-ORACLE-TEST-v1".to_owned(),
            requested_workers: 2,
            seed: 0x0123_4567_89ab_cdef,
            minimum_duration_ms: 10,
            dense_fma_iterations_per_burst: 500,
            idle_ramp_enabled: false,
            idle_sleep_ms: 0,
            fault_injection_worker: None,
        }
    }

    #[test]
    fn healthy_run_passes() {
        let report = run_compute_oracle(&base_config()).expect("compute-oracle run");
        assert_eq!(report.status, ComputeOracleStatus::Pass);
        assert!(report.dense_fma_cross_worker_consistent);
        assert!(report.dense_fma_scalar_reference_passed);
        assert!(report.branch_predictor_passed);
        assert!(report.aes256_known_answer_passed);
        assert!(report.mismatch.is_none());
    }

    #[test]
    fn minimum_work_rejects_zero_partial_and_missing_idle_coverage() {
        assert!(!super::minimum_work_met(&[], 1, false));
        let mut outcomes = vec![super::WorkerOutcome {
            worker_id: 0,
            cpu_id: 0,
            affinity_failed: false,
            bursts_completed: 0,
            idle_ramp_cycles: 0,
            dense_fma_final_bits: [0; 4],
            dense_fma_mismatches: 0,
            branch_predictor_final: 0,
            branch_predictor_mismatches: 0,
            aes256_mismatches: 0,
            panicked: false,
        }];
        assert!(!super::minimum_work_met(&outcomes, 1, false));
        outcomes[0].bursts_completed = 1;
        assert!(super::minimum_work_met(&outcomes, 1, false));
        assert!(!super::minimum_work_met(&outcomes, 2, false));
        assert!(!super::minimum_work_met(&outcomes, 1, true));
        outcomes[0].idle_ramp_cycles = 1;
        assert!(super::minimum_work_met(&outcomes, 1, true));
        outcomes[0].panicked = true;
        assert!(!super::minimum_work_met(&outcomes, 1, true));
    }

    #[test]
    fn injected_worker_fault_cannot_pass() {
        let mut config = base_config();
        config.fault_injection_worker = Some(1);
        let report = run_compute_oracle(&config).expect("compute-oracle run");
        assert_eq!(report.status, ComputeOracleStatus::Fail);
        assert!(!report.dense_fma_cross_worker_consistent);
        let mismatch = report.mismatch.expect("mismatch evidence");
        assert_eq!(mismatch.kernel, "dense_fma_cross_worker");
        assert_eq!(mismatch.worker_id, Some(1));
    }

    #[test]
    fn idle_ramp_cycles_are_counted_when_enabled() {
        let mut config = base_config();
        config.idle_ramp_enabled = true;
        config.idle_sleep_ms = 1;
        let report = run_compute_oracle(&config).expect("compute-oracle run");
        assert_eq!(report.status, ComputeOracleStatus::Pass);
        assert!(report.idle_ramp_cycles >= report.workers_used as u64);
    }

    #[test]
    fn dense_fma_within_tolerance_accepts_matching_scalar_reference() {
        let bits = dense_fma_pass(7, 10_000);
        let scalar = dense_fma_scalar_reference(7, 10_000);
        assert!(super::dense_fma_within_tolerance(&bits, &scalar));
    }

    #[test]
    fn dense_fma_within_tolerance_rejects_wrong_reference() {
        let bits = dense_fma_pass(7, 10_000);
        let wrong_reference = dense_fma_scalar_reference(9, 10_000);
        assert!(!super::dense_fma_within_tolerance(&bits, &wrong_reference));
    }

    #[test]
    fn run_burst_flags_a_mismatch_on_every_burst_not_only_the_last() {
        // A stale/wrong scalar reference (as if only the last burst of a long
        // session were ever checked) must still be caught on this one burst,
        // proving the per-burst check does not rely on session-final timing.
        let branch_reference = branch_predictor_pass(11, super::BRANCH_PREDICTOR_LENGTH);
        let wrong_scalar_reference = dense_fma_scalar_reference(999, 1_000);
        let burst = super::run_burst(11, 1_000, &wrong_scalar_reference, branch_reference);
        assert!(burst.dense_fma_mismatch);

        let correct_scalar_reference = dense_fma_scalar_reference(11, 1_000);
        let healthy_burst =
            super::run_burst(11, 1_000, &correct_scalar_reference, branch_reference);
        assert!(!healthy_burst.dense_fma_mismatch);
    }

    /// FMA throughput by chain count (`cargo test --release -p dit-system --lib
    /// fma_chain_speed -- --ignored --nocapture`).
    #[test]
    #[ignore = "measurement, run by hand in release"]
    fn fma_chain_speed() {
        fn measure(label: &str, chains: u32, run: impl Fn(u32)) {
            let iterations = 20_000_000_u32 / chains.max(1);
            run(iterations / 10);
            let started = std::time::Instant::now();
            run(iterations);
            let seconds = started.elapsed().as_secs_f64();
            // One fused multiply-add per lane: 2 flops, 4 lanes per chain.
            let flops = 2.0 * 4.0 * f64::from(chains) * f64::from(iterations);
            println!(
                "{label:10} {:6.2} GFLOP/s  {:7.2} ns/iteration",
                flops / seconds / 1e9,
                seconds * 1e9 / f64::from(iterations)
            );
        }
        measure("old kernel", 1, |n| {
            std::hint::black_box(super::dense_fma_pass(1, n));
        });
        measure("4 chains", 4, |n| {
            std::hint::black_box(super::fma_chains_pass::<4>(1, n, super::CHAIN_MULTIPLIER));
        });
        measure("6 chains", 6, |n| {
            std::hint::black_box(super::fma_chains_pass::<6>(1, n, super::CHAIN_MULTIPLIER));
        });
        measure("8 chains", 8, |n| {
            std::hint::black_box(super::fma_chains_pass::<8>(1, n, super::CHAIN_MULTIPLIER));
        });
        measure("10 chains", 10, |n| {
            std::hint::black_box(super::fma_chains_pass::<10>(1, n, super::CHAIN_MULTIPLIER));
        });
        let chains = u32::try_from(super::FMA_CHAINS).expect("chains");
        measure("C3 kernel", chains, |n| {
            std::hint::black_box(super::fma_chain_burst(1, n));
        });
    }
}
