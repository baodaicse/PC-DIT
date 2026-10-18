//! FP32 FMA GEMM hardware gates: the device's fused multiply-adds must equal the CPU oracle's
//! bit for bit, an injected fault must fail, and `alternate` must run both arithmetics.
//! Ignored by default: they need a real NVIDIA GPU with CUDA. Run with
//! `cargo test -p gpu-cuda --test fp32 -- --ignored --nocapture --test-threads=1`.
use gpu_cuda::{ArithMix, AsyncConfig, CudaBackend};

const JOBS: usize = 64;
const INNER: u32 = 1024;

fn prepared(arith: ArithMix, rotate_every_batches: u64) -> CudaBackend {
    let device = CudaBackend::inventory().unwrap().remove(0);
    let mut backend = CudaBackend::open(device.pci_address.as_deref().unwrap()).unwrap();
    backend
        .configure_async(AsyncConfig {
            batches: 8,
            rotate_every_batches,
            arith,
            // Every due rotation takes a new corpus (waiting for its build): these gates count
            // rotations and arithmetics. With a pool (C13 G2) a rotation keeps a built corpus
            // when the next one is not ready, which a fast burst here always outruns.
            corpus_pool: 0,
            ..AsyncConfig::default()
        })
        .unwrap();
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    backend
}

#[test]
#[ignore = "bounded real NVIDIA FP32 gate"]
fn fp32_gemm_matches_the_cpu_fma_oracle_bit_for_bit_across_corpora() {
    // Rotating every 8 batches puts several FP32 corpora (derived seeds, N = 63..65) through the
    // check. One run seed: a new run seed restarts the rotation sequence.
    let backend = prepared(ArithMix::F32, 8);
    for _ in 0..6 {
        let evidence = backend
            .verify_matrix_async(11, false, &|| false, None)
            .unwrap();
        assert!(evidence.results_verified > 0);
    }
    let report = backend.async_report().unwrap();
    let c = &report.cumulative;
    assert_eq!(c.totals.failed, 0, "{:?}", c.mismatch_samples);
    assert!(c.f32_results_verified > 0 && c.u32_results_verified == 0);
    assert!(c.corpus_rotations >= 2, "{}", c.corpus_rotations);
}

#[test]
#[ignore = "bounded real NVIDIA FP32 negative gate"]
fn an_injected_fp32_fault_fails_the_batch() {
    let backend = prepared(ArithMix::F32, 0);
    assert!(
        backend
            .verify_matrix_async(3, true, &|| false, None)
            .is_err(),
        "a flipped bit in an FMA result must not pass"
    );
    let report = backend.async_report().unwrap();
    assert_eq!(report.cumulative.totals.failed, 1);
    assert!(report.cumulative.mismatch_words >= 1);
}

#[test]
#[ignore = "bounded real NVIDIA mixed-arithmetic gate"]
fn alternate_runs_the_integer_and_the_fp32_gemm_in_turn() {
    let backend = prepared(ArithMix::Alternate, 8);
    for _ in 0..6 {
        backend
            .verify_matrix_async(11, false, &|| false, None)
            .unwrap();
    }
    let c = backend.async_report().unwrap().cumulative;
    assert_eq!(c.totals.failed, 0);
    assert!(
        c.u32_results_verified > 0 && c.f32_results_verified > 0,
        "u32 {} fp32 {}",
        c.u32_results_verified,
        c.f32_results_verified
    );
}
