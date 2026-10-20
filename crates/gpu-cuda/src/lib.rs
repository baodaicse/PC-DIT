//! NVIDIA CUDA backend. Embedded PTX + dynamically loaded Driver API, no Vulkan
//! loader or CUDA toolkit required to execute. CPU oracle is shared with Vulkan.
use driver::{Driver, Handle, check};
use gpu_engine::{
    gemm::{self, Corpus},
    plan::SlotPlan,
    verifier::{
        HashPolicy, TRANSFER_PATTERN_VERSION, default_helper_threads, first_difference,
        transfer_pattern_into,
    },
};
use gpu_vulkan::{
    AllocationEvidence, CampaignProfile, ComputeEvidence, ComputeProfile, DeviceIntervals,
    ReplayBatch, TransferEvidence, VulkanDeviceInfo,
};
use memory::{Buffer, Context, Pinned};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    ffi::{CStr, c_void},
    ptr,
    rc::Rc,
    time::{Duration, Instant},
};
use thiserror::Error;
mod async_compute;
mod checkpoints;
mod driver;
mod lane;
mod memory;
mod sweep;
mod workers;
pub use async_compute::{AsyncConfig, AsyncReport};
pub use checkpoints::{BoundaryCounts, BoundaryFault, BoundaryReport};
pub use gpu_engine::gemm::{Arith, ArithMix};
pub use lane::{DmaReport, LaneFailure, LaneFault, LaneReport, VramReport};
pub use memory::{LiveResources, live_resources};
pub use sweep::{SweepConfig, SweepReport};
pub use workers::Fairness;

pub const KERNEL_SHA256: &str = "6aba864d76585de426bd5732b8736a29a4fdae10975254da041d44e6739c10cb";
const PTX: &[u8] = include_bytes!("../kernels/gemm.ptx");
/// INT8 tensor-core GEMM (`kernels/tensor.cu`), built for `compute_80`: loaded only on devices of
/// compute capability 8.0 or newer.
const TENSOR_PTX: &[u8] = include_bytes!("../kernels/tensor.ptx");
pub const TENSOR_KERNEL_SHA256: &str =
    "f25f6e2fba70c3352475f5e2b29986e01c726b9fb739c75f74cbdeb6624b13ae";
type Result<T> = std::result::Result<T, Error>;
/// Explicit diagnostics executed on device, never enabled in normal runs.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum MatrixFault {
    Intermediate = 1,
    WrongRow = 2,
    MissingWrite = 3,
    StaleGeneration = 4,
    GuardOverwrite = 5,
}
#[derive(Debug, Error)]
pub enum Error {
    #[error("CUDA unavailable: {0}")]
    Unavailable(String),
    #[error("CUDA {call}: status {status}{}", api_name(*status))]
    Api { call: &'static str, status: i32 },
    #[error("CUDA request: {0}")]
    Request(&'static str),
    #[error("CUDA timeout; context retained until process exit")]
    Timeout,
    #[error("{phase} mismatch at {index}: expected {expected:#x}, actual {actual:#x}")]
    Mismatch {
        phase: &'static str,
        index: usize,
        expected: u32,
        actual: u32,
    },
}
impl Error {
    /// v1.2 C15a: the driver ran out of memory (`CUDA_ERROR_OUT_OF_MEMORY`). No evidence about
    /// the hardware under test: the caller ends INVALID, never FAIL.
    #[must_use]
    pub fn is_out_of_memory(&self) -> bool {
        matches!(self, Self::Api { status: 2, .. })
    }
}

/// The driver's name of a common `CUresult`, so a report says which failure it was (v1.2 C15a:
/// running out of memory is told apart from a fault of the hardware under test).
fn api_name(status: i32) -> &'static str {
    match status {
        1 => " (CUDA_ERROR_INVALID_VALUE)",
        2 => " (CUDA_ERROR_OUT_OF_MEMORY)",
        3 => " (CUDA_ERROR_NOT_INITIALIZED)",
        4 => " (CUDA_ERROR_DEINITIALIZED)",
        100 => " (CUDA_ERROR_NO_DEVICE)",
        101 => " (CUDA_ERROR_INVALID_DEVICE)",
        201 => " (CUDA_ERROR_INVALID_CONTEXT)",
        400 => " (CUDA_ERROR_INVALID_HANDLE)",
        700 => " (CUDA_ERROR_ILLEGAL_ADDRESS)",
        701 => " (CUDA_ERROR_LAUNCH_OUT_OF_RESOURCES)",
        702 => " (CUDA_ERROR_LAUNCH_TIMEOUT)",
        719 => " (CUDA_ERROR_LAUNCH_FAILED)",
        999 => " (CUDA_ERROR_UNKNOWN)",
        _ => "",
    }
}

struct MatrixSlot {
    input: Buffer,
    output: Buffer,
    upload: Pinned,
    download: Pinned,
    resident: Option<String>,
}
struct TransferSlot {
    upload: Pinned,
    download: Pinned,
}
pub struct CudaBackend {
    context: Rc<Context>,
    info: VulkanDeviceInfo,
    stream: Handle,
    module: Handle,
    function: Handle,
    /// FP32 FMA GEMM (`dit_gemm_f32`) for FP32 corpora.
    function_f32: Handle,
    /// INT8 tensor-core module and GEMM (`dit_gemm_i8`); null below compute capability 8.0.
    tensor_module: Handle,
    function_i8: Handle,
    /// GPU-local VRAM workload kernels (H6).
    fill_function: Handle,
    verify_function: Handle,
    /// GPU-DRAM sweep kernels (16-byte accesses).
    sweep_fill_function: Handle,
    sweep_verify_function: Handle,
    /// Boundary-check kernels (step F3): hash of a word array, the GPU's check of its own output
    /// and the seal of that check.
    hash_function: Handle,
    check_function: Handle,
    seal_function: Handle,
    /// The u32 and FP32 GEMMs with the boundary check fused in (no extra launch per batch).
    gemm_checked_function: Handle,
    gemm_f32_checked_function: Handle,
    /// Streaming multiprocessors, to size the sweep grid.
    sm_count: u32,
    /// Whether kernels from different streams can run at once (`CONCURRENT_KERNELS`); without it
    /// the sweep time-slices with the GEMM, so it defaults to a smaller share.
    concurrent_kernels: bool,
    events: [Handle; 4],
    pool: Vec<Buffer>,
    matrix: RefCell<Option<MatrixSlot>>,
    transfer: RefCell<Option<TransferSlot>>,
    corpus: RefCell<Option<Corpus>>,
    profile: RefCell<Option<ComputeProfile>>,
    campaign: RefCell<CampaignProfile>,
    epoch: Instant,
    reuse: bool,
    hash_policy: Cell<HashPolicy>,
    async_config: Option<AsyncConfig>,
    balanced: RefCell<Option<async_compute::Pipeline>>,
    balanced_report: RefCell<Option<AsyncReport>>,
    /// Test hook: fail an async allocation step as if the device were out of memory.
    failpoint: Cell<Option<AllocFailpoint>>,
    alloc_counter: Cell<usize>,
}

/// Simulated out-of-memory at allocation step `at` of every pipeline attempt (`once` = only
/// the first attempt). Steps count buffers, streams, events and pinned slots in order.
#[derive(Clone, Copy, Debug)]
pub struct AllocFailpoint {
    pub at: usize,
    pub once: bool,
}

fn infos(driver: &Driver) -> Result<Vec<VulkanDeviceInfo>> {
    let mut count = 0;
    let mut version = 0;
    check("cuDeviceGetCount", unsafe {
        (driver.count)(&raw mut count)
    })?;
    check("cuDriverGetVersion", unsafe {
        (driver.version)(&raw mut version)
    })?;
    let mut result = Vec::new();
    for ordinal in 0..count {
        let mut device = 0;
        check("cuDeviceGet", unsafe {
            (driver.device)(&raw mut device, ordinal)
        })?;
        let mut name = [0_i8; 256];
        let mut pci = [0_i8; 32];
        let mut uuid = [0_u8; 16];
        let mut total = 0_usize;
        let mut major = 0;
        let mut minor = 0;
        check("cuDeviceGetName", unsafe {
            (driver.name)(name.as_mut_ptr(), 256, device)
        })?;
        check("cuDeviceGetPCIBusId", unsafe {
            (driver.pci)(pci.as_mut_ptr(), 32, device)
        })?;
        check("cuDeviceGetUuid", unsafe {
            (driver.uuid)(&raw mut uuid, device)
        })?;
        check("cuDeviceTotalMem", unsafe {
            (driver.total)(&raw mut total, device)
        })?;
        check("CC major", unsafe {
            (driver.attribute)(&raw mut major, 75, device)
        })?;
        check("CC minor", unsafe {
            (driver.attribute)(&raw mut minor, 76, device)
        })?;
        let mut limits_ok = true;
        // Driver API attributes: block threads/dimensions, grid dimensions, shared memory.
        for (attribute, minimum) in [
            (1, 256),
            (2, 16),
            (3, 16),
            (5, 5),
            (6, 5),
            (7, 65_535),
            (8, 2048),
        ] {
            let mut limit = 0;
            check("GEMM capability", unsafe {
                (driver.attribute)(&raw mut limit, attribute, device)
            })?;
            limits_ok &= limit >= minimum;
        }
        let mut integrated = 0;
        check("integrated memory", unsafe {
            (driver.attribute)(&raw mut integrated, 18, device)
        })?;
        let eligible = (major > 5 || (major == 5 && minor >= 2)) && limits_ok;
        result.push(VulkanDeviceInfo {
            index: usize::try_from(ordinal)
                .map_err(|_| Error::Request("negative device ordinal"))?,
            name: unsafe { CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
            device_type: if integrated == 0 {
                "DISCRETE_GPU"
            } else {
                "INTEGRATED_GPU"
            }
            .into(),
            hardware_eligible: eligible,
            rejection_reason: (!eligible).then(|| {
                "CUDA GEMM requires CC >= 5.2 and 16x16 tile/grid/shared-memory limits".into()
            }),
            vendor_id: 0x10de,
            device_id: 0,
            api_version: format!("CUDA {version} / CC {major}.{minor}"),
            driver_version_raw: u32::try_from(version).unwrap_or(0),
            uuid: hex::encode(uuid),
            pci_address: Some(
                unsafe { CStr::from_ptr(pci.as_ptr()) }
                    .to_string_lossy()
                    .to_lowercase(),
            ),
            compute_queue_family: None,
            device_local_bytes: total as u64,
            budget_bytes: None,
            usage_bytes: None,
            memory_budget_supported: true,
        });
    }
    Ok(result)
}

impl CudaBackend {
    /// # Errors
    /// Returns loader/driver errors; never creates a mock GPU.
    pub fn inventory() -> Result<Vec<VulkanDeviceInfo>> {
        infos(&Driver::load()?)
    }

    /// Opens by stable PCI identity, not by Vulkan/CUDA enumeration order.
    /// # Errors
    /// Rejects missing/unsupported devices and initialization errors.
    pub fn open(pci: &str) -> Result<Self> {
        let driver = Driver::load()?;
        let info = infos(&driver)?
            .into_iter()
            .find(|i| i.pci_address.as_deref() == Some(pci))
            .ok_or(Error::Request("CUDA PCI device not found"))?;
        if !info.hardware_eligible {
            return Err(Error::Request("unsupported CUDA architecture"));
        }
        let mut device = 0;
        check("cuDeviceGet", unsafe {
            (driver.device)(
                &raw mut device,
                i32::try_from(info.index).map_err(|_| Error::Request("device index"))?,
            )
        })?;
        let (mut sm_count, mut concurrent, mut major) = (0, 0, 0);
        check("CC major", unsafe {
            (driver.attribute)(&raw mut major, 75, device)
        })?;
        check("SM count", unsafe {
            (driver.attribute)(&raw mut sm_count, 16, device)
        })?;
        check("concurrent kernels", unsafe {
            (driver.attribute)(&raw mut concurrent, 31, device)
        })?;
        let mut raw_context = ptr::null_mut();
        check("cuCtxCreate", unsafe {
            (driver.context_create)(&raw mut raw_context, 0, device)
        })?;
        let context = Rc::new(Context {
            driver,
            handle: raw_context,
            stopped: std::cell::Cell::new(false),
        });
        let mut backend = Self {
            context,
            info,
            stream: ptr::null_mut(),
            module: ptr::null_mut(),
            function: ptr::null_mut(),
            function_f32: ptr::null_mut(),
            tensor_module: ptr::null_mut(),
            function_i8: ptr::null_mut(),
            fill_function: ptr::null_mut(),
            verify_function: ptr::null_mut(),
            sweep_fill_function: ptr::null_mut(),
            sweep_verify_function: ptr::null_mut(),
            hash_function: ptr::null_mut(),
            check_function: ptr::null_mut(),
            seal_function: ptr::null_mut(),
            gemm_checked_function: ptr::null_mut(),
            gemm_f32_checked_function: ptr::null_mut(),
            sm_count: 1,
            concurrent_kernels: false,
            events: [ptr::null_mut(); 4],
            pool: Vec::new(),
            matrix: RefCell::new(None),
            transfer: RefCell::new(None),
            corpus: RefCell::new(None),
            profile: RefCell::new(None),
            campaign: RefCell::new(CampaignProfile::default()),
            epoch: Instant::now(),
            reuse: true,
            hash_policy: Cell::new(HashPolicy::EveryBatch),
            async_config: None,
            balanced: RefCell::new(None),
            balanced_report: RefCell::new(None),
            failpoint: Cell::new(None),
            alloc_counter: Cell::new(0),
        };
        backend.sm_count = u32::try_from(sm_count).unwrap_or(1).max(1);
        backend.concurrent_kernels = concurrent != 0;
        let d = &backend.context.driver;
        check("cuStreamCreate", unsafe {
            (d.stream_create)(&raw mut backend.stream, 1)
        })?;
        check("cuModuleLoadData", unsafe {
            (d.module_load)(&raw mut backend.module, PTX.as_ptr().cast())
        })?;
        backend.load_functions()?;
        if major >= 8 {
            backend.load_tensor()?;
        }
        let d = &backend.context.driver;
        for event in &mut backend.events {
            check("cuEventCreate", unsafe { (d.event_create)(event, 0) })?;
        }
        let mut free = 0;
        let mut total = 0;
        check("cuMemGetInfo", unsafe {
            (d.memory_info)(&raw mut free, &raw mut total)
        })?;
        backend.info.budget_bytes = Some(total as u64);
        backend.info.usage_bytes = Some((total - free) as u64);
        Ok(backend)
    }
    /// The INT8 tensor-core module (compute capability 8.0+ only).
    fn load_tensor(&mut self) -> Result<()> {
        let context = Rc::clone(&self.context);
        let d = &context.driver;
        check("tensor cuModuleLoadData", unsafe {
            (d.module_load)(&raw mut self.tensor_module, TENSOR_PTX.as_ptr().cast())
        })?;
        check("tensor cuModuleGetFunction", unsafe {
            (d.function)(
                &raw mut self.function_i8,
                self.tensor_module,
                c"dit_gemm_i8".as_ptr(),
            )
        })
    }

    /// Whether this device runs the INT8 tensor-core GEMM (compute capability 8.0 or newer).
    #[must_use]
    pub fn tensor_available(&self) -> bool {
        !self.function_i8.is_null()
    }

    /// Every kernel of the module: the u32 and FP32 GEMM, the GPU-local, sweep and boundary kernels.
    fn load_functions(&mut self) -> Result<()> {
        let context = Rc::clone(&self.context);
        let d = &context.driver;
        for (handle, name) in [
            (&raw mut self.function, c"dit_gemm"),
            (&raw mut self.function_f32, c"dit_gemm_f32"),
            (&raw mut self.fill_function, c"dit_vram_fill"),
            (&raw mut self.verify_function, c"dit_vram_verify"),
            (&raw mut self.sweep_fill_function, c"dit_vram_sweep_fill"),
            (
                &raw mut self.sweep_verify_function,
                c"dit_vram_sweep_verify",
            ),
            (&raw mut self.hash_function, c"dit_hash_words"),
            (&raw mut self.check_function, c"dit_check_output"),
            (&raw mut self.seal_function, c"dit_check_seal"),
            (&raw mut self.gemm_checked_function, c"dit_gemm_checked"),
            (
                &raw mut self.gemm_f32_checked_function,
                c"dit_gemm_f32_checked",
            ),
        ] {
            check("cuModuleGetFunction", unsafe {
                (d.function)(handle, self.module, name.as_ptr())
            })?;
        }
        Ok(())
    }
    #[must_use]
    pub fn info(&self) -> &VulkanDeviceInfo {
        &self.info
    }
    #[must_use]
    pub fn allocated_bytes(&self) -> u64 {
        self.pool.iter().map(|b| b.len as u64).sum()
    }
    #[must_use]
    pub fn allocation_sizes(&self) -> Vec<u64> {
        self.pool.iter().map(|b| b.len as u64).collect()
    }
    #[must_use]
    pub fn submission_stopped(&self) -> bool {
        self.context.stopped.get()
    }
    #[must_use]
    pub fn last_compute_profile(&self) -> Option<ComputeProfile> {
        self.profile.borrow().clone()
    }
    #[must_use]
    pub fn campaign_profile(&self) -> CampaignProfile {
        self.campaign
            .borrow()
            .snapshot(self.epoch.elapsed().as_secs_f64())
    }
    pub fn set_compute_resource_reuse(&mut self, reuse: bool) {
        self.reuse = reuse;
    }
    /// Arms the out-of-memory test hook for the next async resource preparation.
    pub fn set_alloc_failpoint(&self, failpoint: Option<AllocFailpoint>) {
        self.failpoint.set(failpoint);
    }
    /// One counted allocation step; fails when the test hook says the device is out of memory.
    pub(crate) fn alloc_step(&self) -> Result<()> {
        let step = self.alloc_counter.get();
        self.alloc_counter.set(step + 1);
        if let Some(hook) = self.failpoint.get()
            && hook.at == step
        {
            if hook.once {
                self.failpoint.set(None);
            }
            return Err(Error::Api {
                call: "injected allocation failure",
                status: 2,
            });
        }
        Ok(())
    }
    /// Free device memory as the driver reports it now; used to prove rollback frees everything.
    /// # Errors
    /// Returns the driver error.
    pub fn free_device_bytes(&self) -> Result<u64> {
        self.ready()?;
        let (mut free, mut total) = (0, 0);
        check("cuMemGetInfo", unsafe {
            (self.context.driver.memory_info)(&raw mut free, &raw mut total)
        })?;
        Ok(free as u64)
    }
    /// Releases the async pipeline (if any) and everything it owns.
    pub fn release_async_resources(&self) {
        self.balanced.borrow_mut().take();
    }
    /// The async configuration in force after allocation backoff.
    #[must_use]
    pub fn async_effective_config(&self) -> Option<AsyncConfig> {
        self.balanced
            .borrow()
            .as_ref()
            .map(async_compute::Pipeline::config)
    }
    /// Applies to sequential transfer, retention and matrix hashes. A skipped hash
    /// leaves `result_sha256` empty; the comparison itself is never skipped.
    pub fn set_hash_policy(&self, policy: HashPolicy) {
        self.hash_policy.set(policy);
    }
    #[must_use]
    pub fn hash_policy(&self) -> HashPolicy {
        self.hash_policy.get()
    }
    fn ready(&self) -> Result<()> {
        if self.submission_stopped() {
            return Err(Error::Request("CUDA context stopped"));
        }
        self.context.current()
    }
    fn batch<T>(&self, descriptor: ReplayBatch, run: impl FnOnce() -> Result<T>) -> Result<T> {
        descriptor.validate().map_err(Error::Request)?;
        self.campaign
            .borrow_mut()
            .begin(descriptor, self.epoch.elapsed().as_secs_f64());
        let result = self.ready().and_then(|()| run());
        if matches!(&result, Err(Error::Timeout | Error::Api { .. })) {
            self.context.stopped.set(true);
        }
        self.campaign
            .borrow_mut()
            .finish(self.epoch.elapsed().as_secs_f64(), result.is_ok());
        result
    }
    fn stamp(&self, index: usize) -> Result<()> {
        check("cuEventRecord", unsafe {
            (self.context.driver.event_record)(self.events[index], self.stream)
        })?;
        if index == 0 {
            self.campaign
                .borrow_mut()
                .submitted(self.epoch.elapsed().as_secs_f64());
        }
        Ok(())
    }
    fn wait(&self) -> Result<DeviceIntervals> {
        let start = Instant::now();
        loop {
            let status = unsafe { (self.context.driver.stream_query)(self.stream) };
            if status == 0 {
                break;
            }
            if status != 600 || start.elapsed() > Duration::from_secs(30) {
                self.context.stopped.set(true);
                self.campaign
                    .borrow_mut()
                    .completion(self.epoch.elapsed().as_secs_f64(), false);
                return if status == 600 {
                    Err(Error::Timeout)
                } else {
                    Err(Error::Api {
                        call: "cuStreamQuery",
                        status,
                    })
                };
            }
            std::thread::sleep(Duration::from_micros(100));
        }
        self.campaign
            .borrow_mut()
            .completion(self.epoch.elapsed().as_secs_f64(), true);
        let mut elapsed = [0_f32; 3];
        for (i, value) in elapsed.iter_mut().enumerate() {
            check("cuEventElapsedTime", unsafe {
                (self.context.driver.event_elapsed)(value, self.events[i], self.events[i + 1])
            })?;
        }
        let result = DeviceIntervals {
            h2d_seconds: f64::from(elapsed[0]) / 1000.0,
            kernel_seconds: f64::from(elapsed[1]) / 1000.0,
            d2h_seconds: f64::from(elapsed[2]) / 1000.0,
        };
        let mut campaign = self.campaign.borrow_mut();
        let phase = campaign.phase();
        phase.gpu_samples += 1;
        phase.gpu_h2d_seconds += result.h2d_seconds;
        phase.gpu_kernel_seconds += result.kernel_seconds;
        phase.gpu_d2h_seconds += result.d2h_seconds;
        phase.gpu_seconds += result.h2d_seconds + result.kernel_seconds + result.d2h_seconds;
        Ok(result)
    }
    /// # Errors
    /// Returns allocation/API errors after bounded pressure backoff.
    pub fn allocate_pool(&mut self, requested: u64, maximum: u64) -> Result<AllocationEvidence> {
        self.ready()?;
        self.pool.clear();
        if requested < 4 * 1024 * 1024 || maximum < 4 * 1024 * 1024 {
            return Err(Error::Request("pool minimum is 4 MiB"));
        }
        let mut remaining = requested;
        let mut backoffs = 0;
        while remaining >= 4 * 1024 * 1024 {
            let mut bytes = remaining.min(maximum) / 4096 * 4096;
            loop {
                match Buffer::new(
                    &self.context,
                    usize::try_from(bytes).map_err(|_| Error::Request("allocation size"))?,
                ) {
                    Ok(buffer) => {
                        self.pool.push(buffer);
                        remaining -= bytes;
                        break;
                    }
                    Err(Error::Api { status: 2, .. }) if bytes >= 8 * 1024 * 1024 => {
                        bytes = bytes / 2 / 4096 * 4096;
                        backoffs += 1;
                    }
                    Err(error @ Error::Api { status: 2, .. }) => {
                        if self.pool.is_empty() {
                            return Err(error);
                        }
                        remaining = 0;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(AllocationEvidence {
            requested_bytes: requested,
            allocated_bytes: self.allocated_bytes(),
            allocation_count: self.pool.len(),
            largest_allocation_bytes: self.pool.iter().map(|b| b.len as u64).max().unwrap_or(0),
            backoff_count: backoffs,
        })
    }
    fn matrix_slot(&self, jobs: usize, inner: u32) -> Result<MatrixSlot> {
        let (input, output, _) = gemm::workspace_per_job(inner);
        let input = usize::try_from(input).map_err(|_| Error::Request("matrix size"))? * jobs;
        let output = usize::try_from(output).map_err(|_| Error::Request("matrix size"))? * jobs;
        Ok(MatrixSlot {
            input: Buffer::new(&self.context, input)?,
            output: Buffer::new(&self.context, output)?,
            upload: Pinned::new(&self.context, input)?,
            download: Pinned::new(&self.context, output)?,
            resident: None,
        })
    }
    /// # Errors
    /// Returns invalid-size or allocation errors; backoff changes jobs explicitly.
    pub fn prepare_matrix_resources(&self, requested: usize, inner: u32) -> Result<(usize, u64)> {
        self.ready()?;
        if !(1..=65_536).contains(&requested) || !(1..=gemm::MAX_K).contains(&inner) {
            return Err(Error::Request("matrix dimensions"));
        }
        self.matrix.borrow_mut().take();
        self.balanced.borrow_mut().take();
        let mut jobs = requested.min(65_535);
        let mut backoffs = 0;
        if let Some(config) = self.async_config {
            // Out of memory sheds queue depth first, then GPU overlap, then jobs. Every failed
            // attempt has already released what it allocated (owners drop on the error path).
            let mut plan = SlotPlan {
                jobs,
                device_slots: config.device_slots,
                host_slots: config.host_slots,
            };
            loop {
                let effective = AsyncConfig {
                    device_slots: plan.device_slots,
                    host_slots: plan.host_slots,
                    workers: config.workers.min(plan.host_slots),
                    quarantine_slots: config.quarantine_slots.min(plan.host_slots - 1),
                    ..config
                };
                match async_compute::Pipeline::new(self, plan.jobs, inner, effective) {
                    Ok(pipeline) => {
                        *self.balanced.borrow_mut() = Some(pipeline);
                        return Ok((plan.jobs, backoffs));
                    }
                    Err(error @ Error::Api { status: 2, .. }) => {
                        let Some(next) = plan.backoff_step() else {
                            return Err(error);
                        };
                        plan = next;
                        backoffs += 1;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        loop {
            match self.matrix_slot(jobs, inner) {
                Ok(slot) => {
                    *self.matrix.borrow_mut() = Some(slot);
                    return Ok((jobs, backoffs));
                }
                Err(Error::Api { status: 2, .. }) if jobs > 1 => {
                    jobs = (jobs / 2).max(1);
                    backoffs += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }
    /// # Errors
    /// Returns transfer/driver/mismatch errors.
    pub fn verify_transfer(
        &self,
        allocation: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
    ) -> Result<TransferEvidence> {
        self.transfer_run(allocation, offset, bytes, seed, generation, false, false)
    }
    /// # Errors
    /// Returns an injected mismatch or earlier driver error.
    pub fn verify_transfer_injected(
        &self,
        allocation: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
    ) -> Result<TransferEvidence> {
        self.transfer_run(allocation, offset, bytes, seed, generation, false, true)
    }
    /// # Errors
    /// Returns retention/driver/mismatch errors.
    pub fn verify_retained(
        &self,
        allocation: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
    ) -> Result<TransferEvidence> {
        self.transfer_run(allocation, offset, bytes, seed, generation, true, false)
    }
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        clippy::cast_precision_loss
    )]
    fn transfer_run(
        &self,
        allocation: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
        retention: bool,
        inject: bool,
    ) -> Result<TransferEvidence> {
        let name = if retention { "retention" } else { "transfer" };
        self.batch(
            ReplayBatch {
                version: TRANSFER_PATTERN_VERSION,
                phase: name.into(),
                seed,
                generation,
                allocation: allocation as u64,
                offset,
                bytes: bytes as u64,
                jobs: 0,
                rounds: 0,
            },
            || {
                let started = Instant::now();
                let buffer = self
                    .pool
                    .get(allocation)
                    .ok_or(Error::Request("allocation index"))?;
                if bytes == 0
                    || offset
                        .checked_add(bytes as u64)
                        .is_none_or(|end| end > buffer.len as u64)
                {
                    return Err(Error::Request("transfer range"));
                }
                let mut cache = self.transfer.borrow_mut();
                if cache.as_ref().is_none_or(|s| s.upload.len < bytes) {
                    cache.take();
                    *cache = Some(TransferSlot {
                        upload: Pinned::new(&self.context, bytes)?,
                        download: Pinned::new(&self.context, bytes)?,
                    });
                }
                let slot = cache.as_mut().expect("staging initialized");
                // The expected pattern is generated straight into the pinned upload
                // buffer: no intermediate Vec and no second copy. H2D reads it, and the
                // comparison reads it again after the download has landed.
                let pattern = Instant::now();
                // SAFETY: no DMA touches the upload buffer before the submissions below.
                transfer_pattern_into(
                    unsafe { slot.upload.bytes_mut(bytes) },
                    seed,
                    generation,
                    offset,
                    default_helper_threads(),
                );
                self.campaign.borrow_mut().phase().prepare_seconds +=
                    pattern.elapsed().as_secs_f64();
                self.stamp(0)?;
                if !retention {
                    check("cuMemcpyHtoDAsync", unsafe {
                        (self.context.driver.upload)(
                            buffer.address + offset,
                            slot.upload.address,
                            bytes,
                            self.stream,
                        )
                    })?;
                }
                self.stamp(1)?;
                self.stamp(2)?;
                check("cuMemcpyDtoHAsync", unsafe {
                    (self.context.driver.download)(
                        slot.download.address,
                        buffer.address + offset,
                        bytes,
                        self.stream,
                    )
                })?;
                self.stamp(3)?;
                self.wait()?;
                if inject {
                    slot.download.corrupt_first_byte();
                }
                // SAFETY: wait() proved the D2H completed; neither buffer is written
                // again before this closure returns.
                let (expected, actual) = unsafe {
                    (
                        slot.upload.completed_bytes(bytes),
                        slot.download.completed_bytes(bytes),
                    )
                };
                let compare = Instant::now();
                let mismatch = first_difference(expected, actual);
                self.campaign.borrow_mut().phase().compare_seconds +=
                    compare.elapsed().as_secs_f64();
                if let Some(index) = mismatch {
                    return Err(Error::Mismatch {
                        phase: name,
                        index,
                        expected: u32::from(expected[index]),
                        actual: u32::from(actual[index]),
                    });
                }
                let hash = if self.hash_policy.get().should_hash(generation) {
                    format!("{:x}", Sha256::digest(actual))
                } else {
                    String::new()
                };
                let elapsed = started.elapsed().as_secs_f64();
                Ok(TransferEvidence {
                    allocation_index: allocation,
                    offset_bytes: offset,
                    bytes_verified: bytes as u64,
                    elapsed_seconds: elapsed,
                    h2d_d2h_mib_per_second: bytes as f64 * if retention { 1.0 } else { 2.0 }
                        / 1_048_576.0
                        / elapsed.max(1e-9),
                    result_sha256: hash,
                })
            },
        )
    }
    /// # Errors
    /// Returns driver, corpus or independent CPU comparison errors.
    pub fn verify_matrix(&self, s: u64, g: u64, j: usize, k: u32) -> Result<ComputeEvidence> {
        self.matrix_run(s, g, j, k, 0)
    }
    /// # Errors
    /// Returns injected mismatch or earlier execution error.
    pub fn verify_matrix_injected(
        &self,
        s: u64,
        g: u64,
        j: usize,
        k: u32,
    ) -> Result<ComputeEvidence> {
        self.matrix_run(s, g, j, k, MatrixFault::Intermediate as u32)
    }
    /// Run an explicitly corrupted device batch through the production oracle.
    /// # Errors
    /// Returns a mismatch or earlier capability/driver error.
    pub fn verify_matrix_fault(
        &self,
        seed: u64,
        generation: u64,
        jobs: usize,
        inner: u32,
        fault: MatrixFault,
    ) -> Result<ComputeEvidence> {
        self.matrix_run(seed, generation, jobs, inner, fault as u32)
    }
    #[allow(clippy::too_many_lines, clippy::cast_possible_truncation)]
    fn matrix_run(
        &self,
        seed: u64,
        generation: u64,
        jobs: usize,
        inner: u32,
        fault: u32,
    ) -> Result<ComputeEvidence> {
        self.profile.borrow_mut().take();
        if jobs > 65_535 {
            return Err(Error::Request("matrix dispatch limit"));
        }
        self.batch(
            ReplayBatch {
                version: 2,
                phase: "matrix".into(),
                seed,
                generation,
                allocation: 0,
                offset: 0,
                bytes: 0,
                jobs: jobs as u64,
                rounds: inner,
            },
            || {
                let started = Instant::now();
                let mut corpus_cache = self.corpus.borrow_mut();
                let reused = corpus_cache
                    .as_ref()
                    .is_some_and(|c| c.matches(seed, jobs, inner));
                if !reused {
                    *corpus_cache = Some(
                        Corpus::new_with_threads(seed, jobs, inner, default_helper_threads())
                            .map_err(Error::Request)?,
                    );
                }
                let corpus = corpus_cache.as_ref().expect("corpus initialized");
                let output_bytes = jobs * (corpus.n * corpus.n + 4) * 4;
                let input_bytes = corpus.input.len() * 4;
                let mut profile = ComputeProfile {
                    version: 1,
                    seed,
                    generation,
                    workload: gemm::VERSION.into(),
                    corpus_sha256: corpus.sha256.clone(),
                    corpus_reused: reused,
                    matrix_dimension: corpus.n,
                    integer_operations: corpus.integer_operations(),
                    input_generation_seconds: if reused {
                        0.0
                    } else {
                        corpus.generation_seconds
                    },
                    cpu_oracle_seconds: if reused { 0.0 } else { corpus.oracle_seconds },
                    pipeline_reused: true,
                    ..ComputeProfile::default()
                };
                let setup = Instant::now();
                let mut cache = self.matrix.borrow_mut();
                profile.buffers_reused = cache
                    .as_ref()
                    .is_some_and(|s| s.input.len >= input_bytes && s.output.len >= output_bytes);
                if !profile.buffers_reused {
                    cache.take();
                    *cache = Some(self.matrix_slot(jobs, inner)?);
                }
                let slot = cache.as_mut().expect("slot initialized");
                profile.input_resident = slot.resident.as_ref() == Some(&corpus.sha256);
                profile.persistent_device_buffer_bytes = (slot.input.len + slot.output.len) as u64;
                profile.persistent_host_buffer_bytes = (slot.upload.len + slot.download.len) as u64;
                profile.buffer_setup_seconds = setup.elapsed().as_secs_f64();
                let upload = Instant::now();
                if !profile.input_resident {
                    slot.upload.write_u32(&corpus.input);
                }
                profile.host_upload_seconds = upload.elapsed().as_secs_f64();
                let outcome = (|| {
                    self.stamp(0)?;
                    if !profile.input_resident {
                        check("matrix upload", unsafe {
                            (self.context.driver.upload)(
                                slot.input.address,
                                slot.upload.address,
                                input_bytes,
                                self.stream,
                            )
                        })?;
                    }
                    check("matrix guards", unsafe {
                        (self.context.driver.fill)(
                            slot.output.address,
                            0xA55A_1973,
                            slot.output.len / 4,
                            self.stream,
                        )
                    })?;
                    self.stamp(1)?;
                    let mut input_address = slot.input.address;
                    let mut output_address = slot.output.address;
                    let mut job_count = jobs as u32;
                    let mut k = inner;
                    let mut n = corpus.n as u32;
                    let mut lo = generation as u32;
                    let mut hi = (generation >> 32) as u32;
                    let mut fault_mode = fault;
                    let mut params = [
                        (&raw mut input_address).cast::<c_void>(),
                        (&raw mut output_address).cast(),
                        (&raw mut job_count).cast(),
                        (&raw mut k).cast(),
                        (&raw mut n).cast(),
                        (&raw mut lo).cast(),
                        (&raw mut hi).cast(),
                        (&raw mut fault_mode).cast(),
                    ];
                    check("cuLaunchKernel", unsafe {
                        (self.context.driver.launch)(
                            self.function,
                            n.div_ceil(16),
                            n.div_ceil(16),
                            job_count,
                            16,
                            16,
                            1,
                            0,
                            self.stream,
                            params.as_mut_ptr(),
                            ptr::null_mut(),
                        )
                    })?;
                    profile.dispatches_submitted = 1;
                    self.stamp(2)?;
                    check("matrix download", unsafe {
                        (self.context.driver.download)(
                            slot.download.address,
                            slot.output.address,
                            output_bytes,
                            self.stream,
                        )
                    })?;
                    self.stamp(3)?;
                    let wait = Instant::now();
                    let gpu = self.wait();
                    profile.submit_wait_seconds = wait.elapsed().as_secs_f64();
                    profile.gpu = Some(gpu?);
                    profile.dispatches_completed = 1;
                    // SAFETY: wait() above proved the D2H completed; nothing writes the
                    // pinned buffer again before this closure returns.
                    let bytes = unsafe { slot.download.completed_bytes(output_bytes) };
                    let compare = Instant::now();
                    let verdict = corpus.verify_bytes(generation, bytes);
                    profile.cpu_compare_seconds = compare.elapsed().as_secs_f64();
                    if let Err((index, expected, actual)) = verdict {
                        return Err(Error::Mismatch {
                            phase: "matrix",
                            index,
                            expected,
                            actual,
                        });
                    }
                    profile.dispatches_verified = 1;
                    slot.resident = Some(corpus.sha256.clone());
                    // Hash inside the closure: the pinned buffer may be released right after.
                    let hash_started = Instant::now();
                    let digest = self
                        .hash_policy
                        .get()
                        .should_hash(generation)
                        .then(|| format!("{:x}", Sha256::digest(bytes)));
                    if digest.is_some() {
                        profile.result_hash_seconds = hash_started.elapsed().as_secs_f64();
                    }
                    Ok(digest.unwrap_or_default())
                })();
                profile.end_to_end_seconds = started.elapsed().as_secs_f64();
                {
                    let mut c = self.campaign.borrow_mut();
                    let p = c.phase();
                    p.prepare_seconds +=
                        profile.input_generation_seconds + profile.cpu_oracle_seconds;
                    p.compare_seconds += profile.cpu_compare_seconds;
                }
                *self.profile.borrow_mut() = Some(profile.clone());
                // Keep referenced resources alive after any device execution error.
                if matches!(&outcome, Err(Error::Api { .. } | Error::Timeout)) {
                    self.context.stopped.set(true);
                }
                if !self.reuse && !self.submission_stopped() {
                    cache.take();
                }
                let hash = outcome?;
                profile.end_to_end_seconds = started.elapsed().as_secs_f64();
                *self.profile.borrow_mut() = Some(profile.clone());
                Ok(ComputeEvidence {
                    jobs,
                    rounds: inner,
                    results_verified: (jobs * corpus.n * corpus.n) as u64,
                    uploaded_bytes: if profile.input_resident {
                        0
                    } else {
                        input_bytes as u64
                    },
                    downloaded_bytes: output_bytes as u64,
                    elapsed_seconds: profile.end_to_end_seconds,
                    shader_sha256: KERNEL_SHA256.into(),
                    result_sha256: hash,
                    profile: Some(profile),
                })
            },
        )
    }
}
impl Drop for CudaBackend {
    fn drop(&mut self) {
        self.balanced.get_mut().take();
        if self.context.stopped.get() || self.context.current().is_err() {
            return;
        }
        unsafe {
            for event in self.events {
                if !event.is_null() {
                    (self.context.driver.event_destroy)(event);
                }
            }
            if !self.stream.is_null() {
                (self.context.driver.stream_destroy)(self.stream);
            }
            if !self.module.is_null() {
                (self.context.driver.module_unload)(self.module);
            }
            if !self.tensor_module.is_null() {
                (self.context.driver.module_unload)(self.tensor_module);
            }
        }
    }
}
