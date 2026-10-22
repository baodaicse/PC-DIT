//! u32 GEMM chain gates (v1.2 C9): every word of `A·M_1·…·M_4·B` must equal the CPU oracle bit
//! for bit across corpora, every injected fault (in the output, in an intermediate layer, a
//! skipped layer, a wrong matrix) must fail its batch, and the chain must be a turn of `all`.
//! Ignored by default: they need a real NVIDIA GPU with CUDA. Run with
//! `cargo test -p gpu-cuda --test chain -- --ignored --nocapture --test-threads=1`.
use gpu_cuda::{ArithMix, AsyncConfig, CudaBackend, MatrixFault};

const JOBS: usize = 16;
const INNER: u32 = 256;

fn prepared(arith: ArithMix, rotate_every_batches: u64) -> CudaBackend {
    let device = CudaBackend::inventory().unwrap().remove(0);
    let mut backend = CudaBackend::open(device.pci_address.as_deref().unwrap()).unwrap();
    backend
        .configure_async(AsyncConfig {
            batches: 8,
            rotate_every_batches,
            arith,
            // Fixed bursts and no pool: the gates count rotations and batches.
            burst_target_ms: 0,
            corpus_pool: 0,
            ..AsyncConfig::default()
        })
        .unwrap();
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    backend
}

#[test]
#[ignore = "bounded real NVIDIA GEMM chain gate"]
fn the_gemm_chain_matches_its_cpu_oracle_bit_for_bit_across_corpora() {
    let backend = prepared(ArithMix::Chain, 8);
    for _ in 0..4 {
        let evidence = backend
            .verify_matrix_async(17, false, &|| false, None)
            .unwrap();
        assert!(evidence.results_verified > 0);
    }
    let c = backend.async_report().unwrap().cumulative;
    assert_eq!(c.totals.failed, 0, "{:?}", c.mismatch_samples);
    assert_eq!(c.totals.verified, 32);
    assert!(c.corpus_rotations >= 2, "{}", c.corpus_rotations);
    assert!(c.chain_results_verified > 0);
    assert_eq!(c.u32_results_verified, 0);
    // Four layers of n x 256 by 256 x 256 and the projection, 2 ops per step, per job.
    let n_min = 63_u64;
    let per_batch_min = 2 * JOBS as u64 * (4 * n_min * 256 * 256 + n_min * n_min * 256);
    assert!(
        c.chain_ops_verified >= 32 * per_batch_min,
        "{}",
        c.chain_ops_verified
    );
}

#[test]
#[ignore = "bounded real NVIDIA GEMM chain negative gate"]
fn every_injected_chain_fault_fails_its_batch() {
    let backend = prepared(ArithMix::Chain, 0);
    // A clean burst first: a skipped layer then reads what a real earlier batch left.
    backend
        .verify_matrix_async(23, false, &|| false, None)
        .unwrap();
    let faults = [
        ("flipped output bit", MatrixFault::Intermediate),
        ("wrong row", MatrixFault::WrongRow),
        ("missing write", MatrixFault::MissingWrite),
        ("stale generation", MatrixFault::StaleGeneration),
        ("guard overwrite", MatrixFault::GuardOverwrite),
        (
            "bit 31 of a layer-1 intermediate",
            MatrixFault::ChainBitFlip,
        ),
        ("layer 2 skipped", MatrixFault::ChainSkippedLayer),
        (
            "layer 3 with the matrix of layer 1",
            MatrixFault::ChainWrongMatrix,
        ),
    ];
    for (count, (name, fault)) in faults.into_iter().enumerate() {
        backend.arm_matrix_fault(fault).unwrap();
        assert!(
            backend
                .verify_matrix_async(23, false, &|| false, None)
                .is_err(),
            "{name} must not pass"
        );
        let c = backend.async_report().unwrap().cumulative;
        assert_eq!(c.totals.failed, count as u64 + 1, "{name}");
    }
}

#[test]
#[ignore = "bounded real NVIDIA mixed-arithmetic gate with the chain"]
fn the_chain_is_a_turn_of_all() {
    let backend = prepared(ArithMix::All, 8);
    for _ in 0..8 {
        backend
            .verify_matrix_async(29, false, &|| false, None)
            .unwrap();
    }
    let c = backend.async_report().unwrap().cumulative;
    assert_eq!(c.totals.failed, 0, "{:?}", c.mismatch_samples);
    assert!(
        c.u32_results_verified > 0 && c.f32_results_verified > 0 && c.chain_results_verified > 0,
        "u32 {} fp32 {} chain {}",
        c.u32_results_verified,
        c.f32_results_verified,
        c.chain_results_verified
    );
}
