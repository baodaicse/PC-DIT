use gpu_cuda::CudaBackend;
use gpu_vulkan::VulkanBackend;

#[test]
fn embedded_kernel_identity() {
    use sha2::{Digest, Sha256};
    let ptx = include_bytes!("../kernels/gemm.ptx");
    assert_eq!(ptx.last(), Some(&0));
    assert_eq!(
        format!("{:x}", Sha256::digest(ptx)),
        gpu_cuda::KERNEL_SHA256
    );
}

#[test]
#[ignore = "bounded real NVIDIA CUDA/Vulkan cross-backend gate"]
fn g3_cross_backend() {
    let device = CudaBackend::inventory().unwrap().remove(0);
    let pci = device.pci_address.as_deref().unwrap();
    let mut cuda = CudaBackend::open(pci).unwrap();
    let vk_index = VulkanBackend::inventory()
        .unwrap()
        .iter()
        .find(|d| d.pci_address.as_deref() == Some(pci))
        .unwrap()
        .index;
    let vulkan = VulkanBackend::open(Some(vk_index)).unwrap();
    for seed in 0..3 {
        for inner in [1, 17, 64, 129] {
            let generation = (1_u64 << 40) + seed;
            let first = cuda.verify_matrix(seed, generation, 3, inner).unwrap();
            let other = vulkan.verify_matrix(seed, generation, 3, inner).unwrap();
            assert_eq!(first.result_sha256, other.result_sha256);
            let warm = cuda.verify_matrix(seed, generation, 3, inner).unwrap();
            assert_eq!(first.result_sha256, warm.result_sha256);
            let profile = warm.profile.unwrap();
            assert!(profile.input_resident && profile.corpus_reused);
            assert!(profile.cpu_oracle_seconds.abs() < f64::EPSILON);
            assert_eq!(warm.uploaded_bytes, 0);
            assert!(
                cuda.verify_matrix_injected(seed, generation, 3, inner)
                    .is_err()
            );
            assert_eq!(cuda.last_compute_profile().unwrap().dispatches_verified, 0);
            assert!(cuda.verify_matrix(seed, generation + 1, 3, inner).is_ok());
        }
    }
    assert!(cuda.verify_matrix(0, 0, 65_536, 1).is_err());
    assert!(cuda.verify_matrix(0, 0, 1, 4097).is_err());
    cuda.allocate_pool(4 * 1024 * 1024, 4 * 1024 * 1024)
        .unwrap();
    cuda.verify_transfer(0, 0, 4096, 42, 7).unwrap();
    cuda.verify_retained(0, 0, 4096, 42, 7).unwrap();
    assert!(cuda.verify_transfer_injected(0, 0, 4096, 42, 7).is_err());
    assert!(!cuda.submission_stopped());
    for fault in [
        gpu_cuda::MatrixFault::Intermediate,
        gpu_cuda::MatrixFault::WrongRow,
        gpu_cuda::MatrixFault::MissingWrite,
        gpu_cuda::MatrixFault::StaleGeneration,
        gpu_cuda::MatrixFault::GuardOverwrite,
    ] {
        assert!(matches!(
            cuda.verify_matrix_fault(42, 9, 3, 17, fault),
            Err(gpu_cuda::Error::Mismatch { .. })
        ));
        assert_eq!(cuda.last_compute_profile().unwrap().dispatches_verified, 0);
    }
    cuda.verify_matrix(42, 10, 3, 17).unwrap();
}
