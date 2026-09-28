//! INT8 tensor-core GEMM hardware gates. On compute capability 8.0+ the device's
//! `mma.sync.m16n8k32` results must equal the CPU oracle bit for bit and an injected fault must
//! fail; below 8.0 an INT8 request must be refused and `all` must run without it.
//! Ignored by default: they need a real NVIDIA GPU with CUDA. Run with
//! `cargo test -p gpu-cuda --test tensor -- --ignored --nocapture --test-threads=1`.
use gpu_cuda::{ArithMix, AsyncConfig, CudaBackend};

const JOBS: usize = 64;
const INNER: u32 = 1024;

fn open() -> CudaBackend {
    let device = CudaBackend::inventory().unwrap().remove(0);
    CudaBackend::open(device.pci_address.as_deref().unwrap()).unwrap()
}

fn configure(backend: &mut CudaBackend, arith: ArithMix, rotate_every_batches: u64) {
    backend
        .configure_async(AsyncConfig {
            batches: 8,
            rotate_every_batches,
            arith,
            ..AsyncConfig::default()
        })
        .unwrap();
}

#[test]
#[ignore = "bounded real NVIDIA tensor-core gate (compute capability 8.0+)"]
fn int8_tensor_gemm_matches_the_oracle_and_catches_a_fault() {
    let mut backend = open();
    if !backend.tensor_available() {
        eprintln!("no tensor cores on this device (compute capability below 8.0): not exercised");
        return;
    }
    configure(&mut backend, ArithMix::I8, 8);
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    for _ in 0..6 {
        backend
            .verify_matrix_async(21, false, &|| false, None)
            .unwrap();
    }
    let c = backend.async_report().unwrap().cumulative;
    assert_eq!(c.totals.failed, 0, "{:?}", c.mismatch_samples);
    assert!(c.i8_results_verified > 0 && c.corpus_rotations >= 2);

    let mut faulty = open();
    configure(&mut faulty, ArithMix::I8, 0);
    faulty.prepare_matrix_resources(JOBS, INNER).unwrap();
    assert!(
        faulty
            .verify_matrix_async(22, true, &|| false, None)
            .is_err()
    );
    assert_eq!(faulty.async_report().unwrap().cumulative.totals.failed, 1);
}

#[test]
#[ignore = "bounded real NVIDIA gate for devices without tensor cores"]
fn without_tensor_cores_int8_is_refused_and_all_runs_the_rest() {
    let mut backend = open();
    if backend.tensor_available() {
        eprintln!("this device has tensor cores: the no-tensor path is not exercised here");
        return;
    }
    configure(&mut backend, ArithMix::I8, 8);
    let refused = backend.prepare_matrix_resources(JOBS, INNER).unwrap_err();
    assert!(refused.to_string().contains("8.0"), "{refused}");

    let mut all = open();
    configure(&mut all, ArithMix::All, 8);
    all.prepare_matrix_resources(JOBS, INNER).unwrap();
    for _ in 0..6 {
        all.verify_matrix_async(23, false, &|| false, None).unwrap();
    }
    let c = all.async_report().unwrap().cumulative;
    assert_eq!(c.totals.failed, 0);
    assert!(c.u32_results_verified > 0 && c.f32_results_verified > 0);
    assert_eq!(c.i8_results_verified, 0);
}
