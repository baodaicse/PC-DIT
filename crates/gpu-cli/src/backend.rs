//! Vendor-aware selection with stable PCI/UUID mapping. Explicit requests never fall back.
use super::BackendArg;
use gpu_cuda::CudaBackend;
use gpu_vulkan::{
    AllocationEvidence, CampaignProfile, ComputeEvidence, ComputeProfile, TransferEvidence,
    VulkanBackend, VulkanDeviceInfo,
};
type Result<T> = std::result::Result<T, String>;
enum Engine {
    Vulkan(Box<VulkanBackend>),
    Cuda(Box<CudaBackend>),
}
pub struct Backend {
    engine: Engine,
    info: VulkanDeviceInfo,
}

/// Whether `device` runs on the CUDA engine under `mode`: an explicit CUDA request, or auto on an
/// NVIDIA device (auto never drives NVIDIA through Vulkan). The one rule for both opening a device
/// and choosing its pipeline (`--async-auto`).
pub fn runs_on_cuda(mode: BackendArg, device: &VulkanDeviceInfo) -> bool {
    mode == BackendArg::Cuda || (mode == BackendArg::Auto && device.vendor_id == 0x10de)
}

fn merge(mut vulkan: Vec<VulkanDeviceInfo>, cuda: Vec<VulkanDeviceInfo>) -> Vec<VulkanDeviceInfo> {
    vulkan.retain(|v| {
        !cuda
            .iter()
            .any(|c| c.pci_address.is_some() && c.pci_address == v.pci_address)
    });
    for v in &mut vulkan {
        if v.vendor_id == 0x10de {
            v.hardware_eligible = false;
            v.rejection_reason = Some(
                "NVIDIA auto requires CUDA; use --backend vulkan only for explicit fallback".into(),
            );
        }
    }
    vulkan.extend(cuda);
    vulkan.sort_by(|a, b| {
        a.pci_address
            .as_deref()
            .unwrap_or("~")
            .cmp(b.pci_address.as_deref().unwrap_or("~"))
            .then(a.uuid.cmp(&b.uuid))
    });
    for (i, v) in vulkan.iter_mut().enumerate() {
        v.index = i;
    }
    vulkan
}
macro_rules! forward_result {
    ($name:ident($($arg:ident:$ty:ty),*)->$result:ty) => {
        pub fn $name(&self,$($arg:$ty),*) -> Result<$result> { match &self.engine {
            Engine::Vulkan(b)=>b.$name($($arg),*).map_err(|e|e.to_string()),
            Engine::Cuda(b)=>b.$name($($arg),*).map_err(|e|e.to_string()),
        } }
    }
}
impl Backend {
    pub fn configure_async(&mut self, config: gpu_cuda::AsyncConfig) -> Result<()> {
        match &mut self.engine {
            Engine::Cuda(b) => b.configure_async(config).map_err(|e| e.to_string()),
            Engine::Vulkan(_) => Err("GPU-H async adapter currently requires CUDA; omit --async-compute for explicit Vulkan sequential mode".into()),
        }
    }
    /// Vulkan always hashes every output; a lighter policy is refused, not ignored.
    pub fn set_hash_policy(&mut self, policy: gpu_engine::verifier::HashPolicy) -> Result<()> {
        match &mut self.engine {
            Engine::Cuda(b) => {
                b.set_hash_policy(policy);
                Ok(())
            }
            Engine::Vulkan(_) if policy == gpu_engine::verifier::HashPolicy::EveryBatch => Ok(()),
            Engine::Vulkan(_) => Err(
                "--hash-policy checkpoint requires the CUDA backend; Vulkan hashes every output"
                    .into(),
            ),
        }
    }
    pub fn hash_policy_label(&self) -> String {
        match &self.engine {
            Engine::Cuda(b) => b.hash_policy().label(),
            Engine::Vulkan(_) => gpu_engine::verifier::HashPolicy::EveryBatch.label(),
        }
    }
    /// Async configuration in force after allocation backoff.
    pub fn async_effective_config(&self) -> Option<gpu_cuda::AsyncConfig> {
        match &self.engine {
            Engine::Cuda(b) => b.async_effective_config(),
            Engine::Vulkan(_) => None,
        }
    }
    pub fn async_report(&self) -> Option<gpu_cuda::AsyncReport> {
        match &self.engine {
            Engine::Cuda(b) => b.async_report(),
            Engine::Vulkan(_) => None,
        }
    }
    pub fn verify_matrix_async(
        &self,
        seed: u64,
        inject: bool,
        stop: &dyn Fn() -> bool,
        batches: Option<u64>,
    ) -> Result<ComputeEvidence> {
        match &self.engine {
            Engine::Cuda(b) => b
                .verify_matrix_async(seed, inject, stop, batches)
                .map_err(|e| e.to_string()),
            Engine::Vulkan(_) => Err("Vulkan async adapter unavailable".into()),
        }
    }
    /// Transfer lane alone for `slice` (the transfer-only profile).
    pub fn run_lane_slice(
        &self,
        seed: u64,
        stop: &dyn Fn() -> bool,
        slice: std::time::Duration,
    ) -> Result<()> {
        match &self.engine {
            Engine::Cuda(b) => b
                .run_lane_slice(seed, stop, slice)
                .map(|_| ())
                .map_err(|e| e.to_string()),
            Engine::Vulkan(_) => Err("the transfer lane needs the CUDA backend".into()),
        }
    }
    pub fn sweep_report(&self) -> Option<gpu_cuda::SweepReport> {
        match &self.engine {
            Engine::Cuda(b) => b.sweep_report(),
            Engine::Vulkan(_) => None,
        }
    }
    pub fn lane_report(&self) -> Option<gpu_cuda::LaneReport> {
        match &self.engine {
            Engine::Cuda(b) => b.lane_report(),
            Engine::Vulkan(_) => None,
        }
    }
    pub fn take_lane_failures(&self) -> Vec<gpu_cuda::LaneFailure> {
        match &self.engine {
            Engine::Cuda(b) => b.take_lane_failures(),
            Engine::Vulkan(_) => Vec::new(),
        }
    }
    pub fn flush_lane(&self) -> Result<()> {
        match &self.engine {
            Engine::Cuda(b) => b.flush_lane().map_err(|e| e.to_string()),
            Engine::Vulkan(_) => Ok(()),
        }
    }
    /// Once, at the end: the periodic host-corpus check still on its helper thread reports.
    pub fn finish_async(&self) -> Result<()> {
        match &self.engine {
            Engine::Cuda(b) => b.finish_async().map_err(|e| e.to_string()),
            Engine::Vulkan(_) => Ok(()),
        }
    }
    pub fn inject_lane_fault(&self, fault: gpu_cuda::LaneFault) -> Result<()> {
        match &self.engine {
            Engine::Cuda(b) => b.inject_lane_fault(fault).map_err(|e| e.to_string()),
            Engine::Vulkan(_) => Err("the transfer lane needs the CUDA backend".into()),
        }
    }
    pub fn inventory(mode: BackendArg) -> Result<Vec<VulkanDeviceInfo>> {
        match mode {
            BackendArg::Cuda => CudaBackend::inventory().map_err(|e| e.to_string()),
            BackendArg::Vulkan => VulkanBackend::inventory().map_err(|e| e.to_string()),
            BackendArg::Auto => {
                let v = VulkanBackend::inventory();
                let c = CudaBackend::inventory();
                if let (Err(ve), Err(ce)) = (&v, &c) {
                    return Err(format!("Vulkan: {ve}; CUDA: {ce}"));
                }
                Ok(merge(v.unwrap_or_default(), c.unwrap_or_default()))
            }
            BackendArg::Mock => Err("mock has no hardware inventory".into()),
        }
    }
    pub fn open(mode: BackendArg, index: Option<usize>) -> Result<Self> {
        let inventory = Self::inventory(mode)?;
        let selected = if let Some(index) = index {
            inventory.iter().find(|i| i.index == index)
        } else {
            inventory.iter().find(|i| i.hardware_eligible)
        }
        .ok_or("no selected eligible hardware GPU")?;
        if !selected.hardware_eligible {
            return Err(selected
                .rejection_reason
                .clone()
                .unwrap_or("unsupported GPU".into()));
        }
        let engine = if runs_on_cuda(mode, selected) {
            Engine::Cuda(Box::new(
                CudaBackend::open(
                    selected
                        .pci_address
                        .as_deref()
                        .ok_or("CUDA PCI identity missing")?,
                )
                .map_err(|e| e.to_string())?,
            ))
        } else {
            let raw = VulkanBackend::inventory()
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|i| i.uuid == selected.uuid && i.pci_address == selected.pci_address)
                .ok_or("Vulkan identity changed before open")?;
            Engine::Vulkan(Box::new(
                VulkanBackend::open(Some(raw.index)).map_err(|e| e.to_string())?,
            ))
        };
        let mut info = match &engine {
            Engine::Vulkan(b) => b.info().clone(),
            Engine::Cuda(b) => b.info().clone(),
        };
        info.index = selected.index;
        Ok(Self { engine, info })
    }
    pub fn info(&self) -> &VulkanDeviceInfo {
        &self.info
    }
    pub fn label(&self) -> &'static str {
        match self.engine {
            Engine::Vulkan(_) => "VULKAN_GPU_G3",
            Engine::Cuda(_) => "CUDA_GPU_G3",
        }
    }
    pub fn kernel_hash(&self) -> &'static str {
        match self.engine {
            Engine::Vulkan(_) => gpu_vulkan::SHADER_SHA256,
            Engine::Cuda(_) => gpu_cuda::KERNEL_SHA256,
        }
    }
    pub fn set_compute_resource_reuse(&mut self, value: bool) {
        match &mut self.engine {
            Engine::Vulkan(b) => b.set_compute_resource_reuse(value),
            Engine::Cuda(b) => b.set_compute_resource_reuse(value),
        }
    }
    pub fn allocation_sizes(&self) -> Vec<u64> {
        match &self.engine {
            Engine::Vulkan(b) => b.allocation_sizes(),
            Engine::Cuda(b) => b.allocation_sizes(),
        }
    }
    pub fn submission_stopped(&self) -> bool {
        match &self.engine {
            Engine::Vulkan(b) => b.submission_stopped(),
            Engine::Cuda(b) => b.submission_stopped(),
        }
    }
    pub fn last_compute_profile(&self) -> Option<ComputeProfile> {
        match &self.engine {
            Engine::Vulkan(b) => b.last_compute_profile(),
            Engine::Cuda(b) => b.last_compute_profile(),
        }
    }
    pub fn campaign_profile(&self) -> CampaignProfile {
        match &self.engine {
            Engine::Vulkan(b) => b.campaign_profile(),
            Engine::Cuda(b) => b.campaign_profile(),
        }
    }
    /// Device memory the driver reports free right now, where the backend can ask (CUDA);
    /// `None` on Vulkan, whose budget extension is only read at inventory time.
    pub fn free_device_bytes(&self) -> Option<u64> {
        match &self.engine {
            Engine::Cuda(b) => b.free_device_bytes().ok(),
            Engine::Vulkan(_) => None,
        }
    }
    pub fn allocate_pool(&mut self, bytes: u64, maximum: u64) -> Result<AllocationEvidence> {
        match &mut self.engine {
            Engine::Vulkan(b) => b.allocate_pool(bytes, maximum).map_err(|e| e.to_string()),
            Engine::Cuda(b) => b.allocate_pool(bytes, maximum).map_err(|e| e.to_string()),
        }
    }
    forward_result!(prepare_matrix_resources(jobs:usize,inner:u32)->(usize,u64));
    forward_result!(verify_transfer(a:usize,o:u64,b:usize,s:u64,g:u64)->TransferEvidence);
    forward_result!(verify_transfer_injected(a:usize,o:u64,b:usize,s:u64,g:u64)->TransferEvidence);
    forward_result!(verify_retained(a:usize,o:u64,b:usize,s:u64,g:u64)->TransferEvidence);
    forward_result!(verify_matrix(s:u64,g:u64,j:usize,k:u32)->ComputeEvidence);
    forward_result!(verify_matrix_injected(s:u64,g:u64,j:usize,k:u32)->ComputeEvidence);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device(vendor: u32, pci: &str, index: usize) -> VulkanDeviceInfo {
        VulkanDeviceInfo {
            index,
            name: "fixture".into(),
            device_type: "DISCRETE_GPU".into(),
            hardware_eligible: true,
            rejection_reason: None,
            vendor_id: vendor,
            device_id: 0,
            api_version: "fixture".into(),
            driver_version_raw: 0,
            uuid: pci.into(),
            pci_address: Some(pci.into()),
            compute_queue_family: None,
            device_local_bytes: 1024,
            budget_bytes: None,
            usage_bytes: None,
            memory_budget_supported: false,
        }
    }
    #[test]
    fn auto_prefers_cuda_by_pci_not_enumeration() {
        let v = vec![
            device(0x10de, "0000:08:00.0", 0),
            device(0x1002, "0000:03:00.0", 1),
        ];
        let mut c = device(0x10de, "0000:08:00.0", 7);
        c.api_version = "CUDA".into();
        let merged = merge(v, vec![c]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].vendor_id, 0x1002);
        assert_eq!(merged[1].api_version, "CUDA");
        assert_eq!(merged[1].index, 1);
        let rejected = merge(vec![device(0x10de, "0000:08:00.0", 0)], vec![]);
        assert!(!rejected[0].hardware_eligible);
    }
}
