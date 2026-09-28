//! Vulkan backend for GPU-B transfer integrity and GPU-C compute verification.
//!
//! All Vulkan FFI and resource lifetime handling is intentionally confined to
//! this crate. Callers only receive safe Rust values and explicit evidence.

use ash::{Entry, Instance, vk};
use gpu_engine::{
    gemm::{self, Corpus},
    transfer_pattern,
    verifier::TRANSFER_PATTERN_VERSION,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, CString};
use std::io::Cursor;
use std::ptr;
use std::time::Instant;
use thiserror::Error;

mod budget;
mod campaign;
pub use budget::WorkspaceBudget;
mod profiling;
pub use campaign::{AsyncBurstAccounting, CampaignProfile, ReplayBatch};
mod resources;
pub use profiling::{ComputeProfile, DeviceIntervals};
use resources::{MatrixBuffers, MatrixPipeline};

const FENCE_TIMEOUT_NS: u64 = 30_000_000_000;
const MIN_ALLOCATION_BYTES: u64 = 4 * 1024 * 1024;
pub const SHADER_SHA256: &str = "9a07718a655881ad548469ba318c885c473dcca55203f922b76bbe841778fad8";
const MATRIX_SHADER: &[u8] = include_bytes!("../shaders/matrix_u32.spv");

#[derive(Clone, Debug, Serialize)]
pub struct VulkanDeviceInfo {
    pub index: usize,
    pub name: String,
    pub device_type: String,
    pub hardware_eligible: bool,
    pub rejection_reason: Option<String>,
    pub vendor_id: u32,
    pub device_id: u32,
    pub api_version: String,
    pub driver_version_raw: u32,
    pub uuid: String,
    pub pci_address: Option<String>,
    pub compute_queue_family: Option<u32>,
    pub device_local_bytes: u64,
    pub budget_bytes: Option<u64>,
    pub usage_bytes: Option<u64>,
    pub memory_budget_supported: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct AllocationEvidence {
    pub requested_bytes: u64,
    pub allocated_bytes: u64,
    pub allocation_count: usize,
    pub largest_allocation_bytes: u64,
    pub backoff_count: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TransferEvidence {
    pub allocation_index: usize,
    pub offset_bytes: u64,
    pub bytes_verified: u64,
    pub elapsed_seconds: f64,
    pub h2d_d2h_mib_per_second: f64,
    pub result_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComputeEvidence {
    pub jobs: usize,
    pub rounds: u32,
    pub results_verified: u64,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub elapsed_seconds: f64,
    pub shader_sha256: String,
    pub result_sha256: String,
    /// Absent in legacy reports; `elapsed_seconds` retains its original meaning.
    #[serde(default)]
    pub profile: Option<ComputeProfile>,
}

#[derive(Debug, Error)]
pub enum VulkanError {
    #[error("Vulkan loader unavailable: {0}")]
    Loader(String),
    #[error("Vulkan call failed in {phase}: {code:?}")]
    Api {
        phase: &'static str,
        code: vk::Result,
    },
    #[error("no hardware Vulkan compute device is available")]
    NoHardwareDevice,
    #[error("Vulkan device index {0} does not exist")]
    DeviceIndex(usize),
    #[error("selected Vulkan device is not eligible: {0}")]
    SoftwareDevice(String),
    #[error("selected Vulkan device has no compute queue")]
    NoComputeQueue,
    #[error("no compatible Vulkan memory type for {0}")]
    NoMemoryType(&'static str),
    #[error("VRAM allocation reached only {allocated} of {requested} bytes")]
    AllocationShortfall { requested: u64, allocated: u64 },
    #[error(
        "transfer request is outside allocation {allocation}: offset={offset}, bytes={bytes}, size={size}"
    )]
    TransferRange {
        allocation: usize,
        offset: u64,
        bytes: u64,
        size: u64,
    },
    #[error("transfer mismatch at byte {byte}: expected=0x{expected:02x}, actual=0x{actual:02x}")]
    TransferMismatch { byte: u64, expected: u8, actual: u8 },
    #[error("matrix mismatch at result {result}: expected=0x{expected:08x}, actual=0x{actual:08x}")]
    MatrixMismatch {
        result: usize,
        expected: u32,
        actual: u32,
    },
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("GPU-A contract failed: {0}")]
    Contract(String),
}

fn api(phase: &'static str, code: vk::Result) -> VulkanError {
    VulkanError::Api { phase, code }
}

#[derive(Clone, Copy)]
struct Buffer {
    handle: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
    coherent: bool,
}

pub struct VulkanBackend {
    _entry: Entry,
    instance: Instance,
    device: ash::Device,
    queue: vk::Queue,
    command_pool: vk::CommandPool,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    info: VulkanDeviceInfo,
    pool: Vec<Buffer>,
    timestamp_pool: Option<vk::QueryPool>,
    timestamp_valid_bits: u32,
    timestamp_period_ns: f32,
    timestamp_unavailable_reason: Option<String>,
    matrix_buffers: RefCell<Option<MatrixBuffers>>,
    matrix_pipeline: RefCell<Option<MatrixPipeline>>,
    submission_failed: Cell<bool>,
    pending_commands: RefCell<Vec<(vk::CommandBuffer, vk::Fence)>>,
    deferred_buffers: RefCell<Vec<Buffer>>,
    last_compute_profile: RefCell<Option<ComputeProfile>>,
    reuse_compute_resources: bool,
    campaign: RefCell<CampaignProfile>,
    campaign_epoch: Instant,
    max_matrix_jobs: usize,
    max_storage_range: u64,
    corpus: RefCell<Option<Corpus>>,
    resident_corpus: RefCell<Option<String>>,
    #[cfg(test)]
    construction_failure: Cell<Option<usize>>,
    #[cfg(test)]
    construction_step: Cell<usize>,
    #[cfg(test)]
    construction_oom: Cell<bool>,
}

impl VulkanBackend {
    /// Enumerates Vulkan devices without accepting CPU/software implementations.
    ///
    /// # Errors
    ///
    /// Returns a loader or Vulkan initialization error.
    pub fn inventory() -> Result<Vec<VulkanDeviceInfo>, VulkanError> {
        let (entry, instance) = create_instance()?;
        let result = enumerate_infos(&instance);
        unsafe { instance.destroy_instance(None) };
        drop(entry);
        result
    }

    /// Opens one hardware Vulkan compute device. `None` selects the first eligible GPU.
    ///
    /// # Errors
    ///
    /// Returns an explicit capability or Vulkan API error.
    #[allow(clippy::too_many_lines)]
    pub fn open(selected_index: Option<usize>) -> Result<Self, VulkanError> {
        let (entry, instance) = create_instance()?;
        let physicals = unsafe { instance.enumerate_physical_devices() }
            .map_err(|code| api("enumerate_physical_devices", code))?;
        let infos = enumerate_infos_for(&instance, &physicals)?;
        let index = if let Some(index) = selected_index {
            if index >= infos.len() {
                unsafe { instance.destroy_instance(None) };
                return Err(VulkanError::DeviceIndex(index));
            }
            if !infos[index].hardware_eligible {
                let reason = infos[index]
                    .rejection_reason
                    .clone()
                    .unwrap_or_else(|| "not a hardware compute GPU".to_owned());
                unsafe { instance.destroy_instance(None) };
                return Err(VulkanError::SoftwareDevice(reason));
            }
            index
        } else {
            infos
                .iter()
                .position(|info| info.hardware_eligible)
                .ok_or(VulkanError::NoHardwareDevice)?
        };
        let info = infos[index].clone();
        let physical = physicals[index];
        let limits = unsafe { instance.get_physical_device_properties(physical) }.limits;
        if limits.max_compute_work_group_invocations < 256
            || limits.max_compute_work_group_size[0] < 16
            || limits.max_compute_work_group_size[1] < 16
            || limits.max_compute_shared_memory_size < 2048
            || limits.max_compute_work_group_count[0] < 5
            || limits.max_compute_work_group_count[1] < 5
        {
            unsafe {
                instance.destroy_instance(None);
            }
            return Err(VulkanError::InvalidRequest("G3 tile exceeds device limits"));
        }
        let queue_family = info
            .compute_queue_family
            .ok_or(VulkanError::NoComputeQueue)?;
        let priorities = [1.0_f32];
        let queue_info = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&priorities)];
        let device_info = vk::DeviceCreateInfo::default().queue_create_infos(&queue_info);
        let device = unsafe { instance.create_device(physical, &device_info, None) }
            .map_err(|code| api("create_device", code))?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let pool_info = vk::CommandPoolCreateInfo::default()
            .queue_family_index(queue_family)
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        let command_pool = match unsafe { device.create_command_pool(&pool_info, None) } {
            Ok(value) => value,
            Err(code) => {
                unsafe {
                    device.destroy_device(None);
                    instance.destroy_instance(None);
                }
                return Err(api("create_command_pool", code));
            }
        };
        let memory_properties = unsafe { instance.get_physical_device_memory_properties(physical) };
        let properties = unsafe { instance.get_physical_device_properties(physical) };
        let queue_properties =
            unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let timestamp_valid_bits = queue_properties[queue_family as usize].timestamp_valid_bits;
        let (timestamp_pool, timestamp_unavailable_reason) = if timestamp_valid_bits == 0 {
            (
                None,
                Some("queue family has no timestamp support".to_owned()),
            )
        } else {
            let query_info = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::TIMESTAMP)
                .query_count(4);
            match unsafe { device.create_query_pool(&query_info, None) } {
                Ok(pool) => (Some(pool), None),
                Err(code) => (None, Some(format!("timestamp query pool: {code:?}"))),
            }
        };
        Ok(Self {
            _entry: entry,
            instance,
            device,
            queue,
            command_pool,
            memory_properties,
            info,
            pool: Vec::new(),
            timestamp_pool,
            timestamp_valid_bits,
            timestamp_period_ns: properties.limits.timestamp_period,
            timestamp_unavailable_reason,
            matrix_buffers: RefCell::new(None),
            matrix_pipeline: RefCell::new(None),
            submission_failed: Cell::new(false),
            pending_commands: RefCell::new(Vec::new()),
            deferred_buffers: RefCell::new(Vec::new()),
            last_compute_profile: RefCell::new(None),
            reuse_compute_resources: true,
            campaign: RefCell::new(CampaignProfile::default()),
            campaign_epoch: Instant::now(),
            max_matrix_jobs: (properties.limits.max_compute_work_group_count[2] as usize)
                .min(properties.limits.max_storage_buffer_range as usize / 512),
            max_storage_range: u64::from(properties.limits.max_storage_buffer_range),
            corpus: RefCell::new(None),
            resident_corpus: RefCell::new(None),
            #[cfg(test)]
            construction_failure: Cell::new(None),
            #[cfg(test)]
            construction_step: Cell::new(0),
            #[cfg(test)]
            construction_oom: Cell::new(false),
        })
    }

    #[must_use]
    pub fn info(&self) -> &VulkanDeviceInfo {
        &self.info
    }

    #[must_use]
    pub fn allocated_bytes(&self) -> u64 {
        self.pool.iter().map(|buffer| buffer.size).sum()
    }

    #[must_use]
    pub fn allocation_sizes(&self) -> Vec<u64> {
        self.pool.iter().map(|buffer| buffer.size).collect()
    }

    /// Allocates a chunked device-local working set with bounded backoff.
    ///
    /// # Errors
    ///
    /// Returns an allocation shortfall if the backend cannot reach at least one
    /// minimum chunk. A partial allocation is retained and reported otherwise.
    pub fn allocate_pool(
        &mut self,
        requested_bytes: u64,
        maximum_chunk_bytes: u64,
    ) -> Result<AllocationEvidence, VulkanError> {
        self.ensure_ready()?;
        if requested_bytes < MIN_ALLOCATION_BYTES || maximum_chunk_bytes < MIN_ALLOCATION_BYTES {
            return Err(VulkanError::InvalidRequest(
                "allocation and max chunk must be at least 4 MiB",
            ));
        }
        self.destroy_pool();
        let mut remaining = requested_bytes;
        let mut backoff_count = 0_u64;
        while remaining >= MIN_ALLOCATION_BYTES {
            let mut attempt = remaining.min(maximum_chunk_bytes) / 4096 * 4096;
            let mut allocated = None;
            while attempt >= MIN_ALLOCATION_BYTES {
                match self.create_buffer(
                    attempt,
                    vk::BufferUsageFlags::TRANSFER_SRC
                        | vk::BufferUsageFlags::TRANSFER_DST
                        | vk::BufferUsageFlags::STORAGE_BUFFER,
                    vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    vk::MemoryPropertyFlags::DEVICE_LOCAL,
                    "device-local",
                ) {
                    Ok(buffer) => {
                        allocated = Some(buffer);
                        break;
                    }
                    Err(VulkanError::Api {
                        code:
                            vk::Result::ERROR_OUT_OF_DEVICE_MEMORY
                            | vk::Result::ERROR_OUT_OF_HOST_MEMORY,
                        ..
                    }) => {
                        attempt = (attempt / 2) / 4096 * 4096;
                        backoff_count = backoff_count.saturating_add(1);
                    }
                    Err(error) => return Err(error),
                }
            }
            let Some(buffer) = allocated else { break };
            remaining = remaining.saturating_sub(buffer.size);
            self.pool.push(buffer);
        }
        let allocated_bytes = self.allocated_bytes();
        if allocated_bytes < MIN_ALLOCATION_BYTES {
            return Err(VulkanError::AllocationShortfall {
                requested: requested_bytes,
                allocated: allocated_bytes,
            });
        }
        Ok(AllocationEvidence {
            requested_bytes,
            allocated_bytes,
            allocation_count: self.pool.len(),
            largest_allocation_bytes: self.pool.iter().map(|item| item.size).max().unwrap_or(0),
            backoff_count,
        })
    }

    /// Uploads deterministic bytes to a selected VRAM range, downloads them,
    /// and performs a full CPU byte comparison.
    ///
    /// # Errors
    ///
    /// Returns range, Vulkan, timeout, or first-mismatch evidence.
    pub fn verify_transfer(
        &self,
        allocation_index: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
    ) -> Result<TransferEvidence, VulkanError> {
        self.profile_batch(
            ReplayBatch {
                version: TRANSFER_PATTERN_VERSION,
                phase: "transfer".into(),
                seed,
                generation,
                allocation: allocation_index as u64,
                offset,
                bytes: bytes as u64,
                jobs: 0,
                rounds: 0,
            },
            || self.verify_transfer_inner(allocation_index, offset, bytes, seed, generation, false),
        )
    }

    /// Test-only negative path that corrupts readback before the same full
    /// comparator used by production.
    ///
    /// # Errors
    ///
    /// Returns the expected mismatch or an earlier Vulkan error.
    pub fn verify_transfer_injected(
        &self,
        allocation_index: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
    ) -> Result<TransferEvidence, VulkanError> {
        self.profile_batch(
            ReplayBatch {
                version: TRANSFER_PATTERN_VERSION,
                phase: "transfer".into(),
                seed,
                generation,
                allocation: allocation_index as u64,
                offset,
                bytes: bytes as u64,
                jobs: 0,
                rounds: 0,
            },
            || self.verify_transfer_inner(allocation_index, offset, bytes, seed, generation, true),
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn verify_transfer_inner(
        &self,
        allocation_index: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
        inject_corruption: bool,
    ) -> Result<TransferEvidence, VulkanError> {
        let device_buffer = self
            .pool
            .get(allocation_index)
            .ok_or(VulkanError::TransferRange {
                allocation: allocation_index,
                offset,
                bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
                size: 0,
            })?;
        let byte_count = u64::try_from(bytes).map_err(|_| VulkanError::InvalidRequest("size"))?;
        if bytes == 0 || offset.saturating_add(byte_count) > device_buffer.size {
            return Err(VulkanError::TransferRange {
                allocation: allocation_index,
                offset,
                bytes: byte_count,
                size: device_buffer.size,
            });
        }
        let prepare_started = Instant::now();
        let input = transfer_pattern(seed, generation, offset, bytes);
        self.campaign.borrow_mut().phase().prepare_seconds +=
            prepare_started.elapsed().as_secs_f64();
        let upload = self.create_staging(byte_count, "upload staging")?;
        let download = match self.create_staging(byte_count, "download staging") {
            Ok(value) => value,
            Err(error) => {
                self.destroy_buffer(upload);
                return Err(error);
            }
        };
        let started = Instant::now();
        let result = (|| {
            self.write_buffer(upload, &input)?;
            let command = self.begin_commands()?;
            self.write_timestamp(command, 0);
            let upload_copy = [vk::BufferCopy {
                src_offset: 0,
                dst_offset: offset,
                size: byte_count,
            }];
            unsafe {
                self.device.cmd_copy_buffer(
                    command,
                    upload.handle,
                    device_buffer.handle,
                    &upload_copy,
                );
            }
            let barrier = [vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(device_buffer.handle)
                .offset(offset)
                .size(byte_count)];
            unsafe {
                self.device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &barrier,
                    &[],
                );
            }
            self.write_timestamp(command, 1);
            self.write_timestamp(command, 2);
            let download_copy = [vk::BufferCopy {
                src_offset: offset,
                dst_offset: 0,
                size: byte_count,
            }];
            unsafe {
                self.device.cmd_copy_buffer(
                    command,
                    device_buffer.handle,
                    download.handle,
                    &download_copy,
                );
            }
            self.write_timestamp(command, 3);
            self.submit_and_wait(command)?;
            let mut actual = self.read_buffer(download, bytes)?;
            if inject_corruption && !actual.is_empty() {
                actual[0] ^= 1;
            }
            let compare_started = Instant::now();
            let mismatch = input
                .iter()
                .zip(&actual)
                .enumerate()
                .find(|(_, pair)| pair.0 != pair.1);
            self.campaign.borrow_mut().phase().compare_seconds +=
                compare_started.elapsed().as_secs_f64();
            if let Some((index, (&expected, &actual))) = mismatch {
                return Err(VulkanError::TransferMismatch {
                    byte: offset.saturating_add(u64::try_from(index).unwrap_or(u64::MAX)),
                    expected,
                    actual,
                });
            }
            Ok(format!("{:x}", Sha256::digest(&actual)))
        })();
        self.destroy_buffer(download);
        self.destroy_buffer(upload);
        let digest = result?;
        let elapsed = started.elapsed().as_secs_f64();
        #[allow(clippy::cast_precision_loss)]
        let throughput = (2.0 * byte_count as f64 / (1024.0 * 1024.0)) / elapsed.max(0.000_001);
        Ok(TransferEvidence {
            allocation_index,
            offset_bytes: offset,
            bytes_verified: byte_count,
            elapsed_seconds: elapsed,
            h2d_d2h_mib_per_second: throughput,
            result_sha256: digest,
        })
    }

    /// Downloads a previously written VRAM range without rewriting it first.
    /// This is the delayed-retention checkpoint used by GPU-D.
    ///
    /// # Errors
    ///
    /// Returns range, Vulkan, timeout, or first-mismatch evidence.
    #[allow(clippy::too_many_lines)]
    pub fn verify_retained(
        &self,
        allocation_index: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
    ) -> Result<TransferEvidence, VulkanError> {
        self.profile_batch(
            ReplayBatch {
                version: TRANSFER_PATTERN_VERSION,
                phase: "retention".into(),
                seed,
                generation,
                allocation: allocation_index as u64,
                offset,
                bytes: bytes as u64,
                jobs: 0,
                rounds: 0,
            },
            || self.verify_retained_inner(allocation_index, offset, bytes, seed, generation),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn verify_retained_inner(
        &self,
        allocation_index: usize,
        offset: u64,
        bytes: usize,
        seed: u64,
        generation: u64,
    ) -> Result<TransferEvidence, VulkanError> {
        let device_buffer = self
            .pool
            .get(allocation_index)
            .ok_or(VulkanError::TransferRange {
                allocation: allocation_index,
                offset,
                bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
                size: 0,
            })?;
        let byte_count = u64::try_from(bytes).map_err(|_| VulkanError::InvalidRequest("size"))?;
        if bytes == 0 || offset.saturating_add(byte_count) > device_buffer.size {
            return Err(VulkanError::TransferRange {
                allocation: allocation_index,
                offset,
                bytes: byte_count,
                size: device_buffer.size,
            });
        }
        let prepare_started = Instant::now();
        let expected_bytes = transfer_pattern(seed, generation, offset, bytes);
        self.campaign.borrow_mut().phase().prepare_seconds +=
            prepare_started.elapsed().as_secs_f64();
        let download = self.create_staging(byte_count, "retention download")?;
        let started = Instant::now();
        let result = (|| {
            let command = self.begin_commands()?;
            self.write_timestamp(command, 0);
            let barrier = [vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(device_buffer.handle)
                .offset(offset)
                .size(byte_count)];
            unsafe {
                self.device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &barrier,
                    &[],
                );
                self.write_timestamp(command, 1);
                self.write_timestamp(command, 2);
                let region = [vk::BufferCopy {
                    src_offset: offset,
                    dst_offset: 0,
                    size: byte_count,
                }];
                self.device.cmd_copy_buffer(
                    command,
                    device_buffer.handle,
                    download.handle,
                    &region,
                );
            }
            self.write_timestamp(command, 3);
            self.submit_and_wait(command)?;
            let actual = self.read_buffer(download, bytes)?;
            let compare_started = Instant::now();
            let mismatch = expected_bytes
                .iter()
                .zip(&actual)
                .enumerate()
                .find(|(_, pair)| pair.0 != pair.1);
            self.campaign.borrow_mut().phase().compare_seconds +=
                compare_started.elapsed().as_secs_f64();
            if let Some((index, (&expected, &actual))) = mismatch {
                return Err(VulkanError::TransferMismatch {
                    byte: offset.saturating_add(u64::try_from(index).unwrap_or(u64::MAX)),
                    expected,
                    actual,
                });
            }
            Ok(format!("{:x}", Sha256::digest(&actual)))
        })();
        self.destroy_buffer(download);
        let digest = result?;
        let elapsed = started.elapsed().as_secs_f64();
        #[allow(clippy::cast_precision_loss)]
        let throughput = (byte_count as f64 / (1024.0 * 1024.0)) / elapsed.max(0.000_001);
        Ok(TransferEvidence {
            allocation_index,
            offset_bytes: offset,
            bytes_verified: byte_count,
            elapsed_seconds: elapsed,
            h2d_d2h_mib_per_second: throughput,
            result_sha256: digest,
        })
    }

    /// Dispatches the GPU-C 8x8 wrapping-u32 matrix shader and compares every
    /// output word against the independent CPU oracle.
    ///
    /// # Errors
    ///
    /// Returns contract, Vulkan, timeout, or first-result-mismatch evidence.
    #[allow(clippy::too_many_lines)]
    pub fn verify_matrix(
        &self,
        seed: u64,
        generation: u64,
        jobs: usize,
        rounds: u32,
    ) -> Result<ComputeEvidence, VulkanError> {
        self.profile_batch(
            ReplayBatch {
                version: 2,
                phase: "matrix".into(),
                seed,
                generation,
                allocation: 0,
                offset: 0,
                bytes: 0,
                jobs: jobs as u64,
                rounds,
            },
            || self.verify_matrix_inner(seed, generation, jobs, rounds, false),
        )
    }

    /// Test-only negative path that corrupts one compute result before the
    /// production CPU-oracle comparison.
    ///
    /// # Errors
    ///
    /// Returns the expected matrix mismatch or an earlier Vulkan error.
    pub fn verify_matrix_injected(
        &self,
        seed: u64,
        generation: u64,
        jobs: usize,
        rounds: u32,
    ) -> Result<ComputeEvidence, VulkanError> {
        self.profile_batch(
            ReplayBatch {
                version: 2,
                phase: "matrix".into(),
                seed,
                generation,
                allocation: 0,
                offset: 0,
                bytes: 0,
                jobs: jobs as u64,
                rounds,
            },
            || self.verify_matrix_inner(seed, generation, jobs, rounds, true),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn verify_matrix_inner(
        &self,
        seed: u64,
        generation: u64,
        jobs: usize,
        rounds: u32,
        inject_corruption: bool,
    ) -> Result<ComputeEvidence, VulkanError> {
        self.last_compute_profile.borrow_mut().take();
        self.ensure_ready()?;
        let end_to_end = Instant::now();
        if jobs > self.max_matrix_jobs {
            return Err(VulkanError::InvalidRequest("matrix dispatch limit"));
        }
        let (input_limit, output_limit, _) = gemm::workspace_per_job(rounds);
        if (jobs as u64).saturating_mul(input_limit.max(output_limit)) > self.max_storage_range {
            return Err(VulkanError::InvalidRequest("matrix storage range limit"));
        }
        let mut corpus_cache = self.corpus.borrow_mut();
        let corpus_reused = corpus_cache
            .as_ref()
            .is_some_and(|c| c.matches(seed, jobs, rounds));
        if !corpus_reused {
            *corpus_cache =
                Some(Corpus::new(seed, jobs, rounds).map_err(VulkanError::InvalidRequest)?);
        }
        let workload = corpus_cache.as_ref().expect("corpus initialized");
        let expected = workload.expected(generation);
        let mut profile = ComputeProfile {
            version: 1,
            seed,
            generation,
            workload: gemm::VERSION.to_owned(),
            corpus_sha256: workload.sha256.clone(),
            corpus_reused,
            matrix_dimension: workload.n,
            integer_operations: workload.integer_operations(),
            input_generation_seconds: if corpus_reused {
                0.0
            } else {
                workload.generation_seconds
            },
            cpu_oracle_seconds: if corpus_reused {
                0.0
            } else {
                workload.oracle_seconds
            },
            gpu_unavailable_reason: self.timestamp_unavailable_reason.clone(),
            ..ComputeProfile::default()
        };
        let setup_started = Instant::now();
        let input_bytes = words_as_bytes(&workload.input);
        let output_bytes = expected.len() * size_of::<u32>();
        let input_size = u64::try_from(input_bytes.len())
            .map_err(|_| VulkanError::InvalidRequest("matrix input"))?;
        let output_size = u64::try_from(output_bytes)
            .map_err(|_| VulkanError::InvalidRequest("matrix output"))?;
        let mut cache = self.matrix_buffers.borrow_mut();
        let matches = cache.as_ref().is_some_and(|resources| {
            resources.buffers[0].size >= input_size && resources.buffers[1].size >= output_size
        });
        profile.buffers_reused = matches;
        profile.input_resident =
            matches && self.resident_corpus.borrow().as_ref() == Some(&workload.sha256);
        if !matches {
            // The previous sequential dispatch has completed before this boundary.
            // Release the old buffers first to avoid a transient double allocation.
            cache.take();
            *cache = Some(self.create_matrix_buffers(input_size, output_size)?);
        }
        let resources = cache.as_ref().expect("matrix buffers were initialized");
        let [upload, download, device_input, device_output] = [
            resources.buffers[0],
            resources.buffers[1],
            resources.buffers[2],
            resources.buffers[3],
        ];
        profile.persistent_device_buffer_bytes = input_size + output_size;
        profile.persistent_host_buffer_bytes = input_size + output_size;
        let started = Instant::now();
        profile.buffer_setup_seconds = setup_started.elapsed().as_secs_f64();
        let result = self.matrix_inner(
            upload,
            download,
            device_input,
            device_output,
            &input_bytes,
            &expected,
            jobs,
            rounds,
            u32::try_from(workload.n).map_err(|_| VulkanError::InvalidRequest("dimension"))?,
            generation,
            inject_corruption,
            &mut profile,
        );
        let cleanup_started = Instant::now();
        drop(cache);
        if !self.reuse_compute_resources && !self.submission_failed.get() {
            self.matrix_pipeline.borrow_mut().take();
            self.matrix_buffers.borrow_mut().take();
            self.resident_corpus.borrow_mut().take();
        } else if result.is_ok() {
            *self.resident_corpus.borrow_mut() = Some(workload.sha256.clone());
        }
        profile.resource_cleanup_seconds += cleanup_started.elapsed().as_secs_f64();
        {
            let mut campaign = self.campaign.borrow_mut();
            let phase = campaign.phase();
            phase.prepare_seconds += profile.input_generation_seconds + profile.cpu_oracle_seconds;
            phase.compare_seconds += profile.cpu_compare_seconds;
        }
        profile.end_to_end_seconds = end_to_end.elapsed().as_secs_f64();
        *self.last_compute_profile.borrow_mut() = Some(profile.clone());
        let actual = result?;
        let elapsed = started.elapsed().as_secs_f64();
        let hash_started = Instant::now();
        let result_sha256 = format!("{:x}", Sha256::digest(words_as_bytes(&actual)));
        profile.result_hash_seconds = hash_started.elapsed().as_secs_f64();
        profile.end_to_end_seconds = end_to_end.elapsed().as_secs_f64();
        *self.last_compute_profile.borrow_mut() = Some(profile.clone());
        Ok(ComputeEvidence {
            jobs,
            rounds,
            results_verified: (jobs * workload.n * workload.n) as u64,
            uploaded_bytes: if profile.input_resident {
                0
            } else {
                input_size
            },
            downloaded_bytes: output_size,
            elapsed_seconds: elapsed,
            shader_sha256: SHADER_SHA256.to_owned(),
            result_sha256,
            profile: Some(profile),
        })
    }

    /// Returns timing for the latest matrix attempt, including compare/submit failure.
    /// Preparation failures before dispatch return no profile, never stale evidence.
    #[must_use]
    pub fn last_compute_profile(&self) -> Option<ComputeProfile> {
        self.last_compute_profile.borrow().clone()
    }

    /// Selects the sequential resource-recreation diagnostic for reproducible A/B runs.
    pub fn set_compute_resource_reuse(&mut self, enabled: bool) {
        self.reuse_compute_resources = enabled;
    }

    /// A failed submission/wait is terminal; a data mismatch alone is not.
    #[must_use]
    pub fn submission_stopped(&self) -> bool {
        self.submission_failed.get()
    }

    /// Reserves the compute slot before allocating the test pool. Only allocation
    /// pressure triggers halving; capability/API failures remain explicit.
    ///
    /// # Errors
    /// Returns the final allocation error or a non-memory Vulkan error.
    pub fn prepare_matrix_resources(
        &self,
        requested_jobs: usize,
        inner: u32,
    ) -> Result<(usize, u64), VulkanError> {
        if !(1..=65_536).contains(&requested_jobs) {
            return Err(VulkanError::InvalidRequest("matrix jobs"));
        }
        self.ensure_ready()?;
        if !(1..=gemm::MAX_K).contains(&inner) {
            return Err(VulkanError::InvalidRequest("matrix inner dimension"));
        }
        let (input_per_job, output_per_job, _) = gemm::workspace_per_job(inner);
        let mut jobs = requested_jobs.min(self.max_matrix_jobs).min(
            usize::try_from(self.max_storage_range / input_per_job.max(output_per_job))
                .unwrap_or(usize::MAX),
        );
        self.resident_corpus.borrow_mut().take();
        self.matrix_pipeline.borrow_mut().take();
        self.matrix_buffers.borrow_mut().take();
        if jobs == 0 {
            return Err(VulkanError::InvalidRequest("matrix exceeds device limits"));
        }
        let mut steps = 0;
        loop {
            let attempt = (|| {
                let buffers = self.create_matrix_buffers(
                    jobs as u64 * input_per_job,
                    jobs as u64 * output_per_job,
                )?;
                let pipeline = self.create_matrix_pipeline()?;
                Ok((buffers, pipeline))
            })();
            match attempt {
                Ok((buffers, pipeline)) => {
                    *self.matrix_buffers.borrow_mut() = Some(buffers);
                    *self.matrix_pipeline.borrow_mut() = Some(pipeline);
                    return Ok((jobs, steps));
                }
                Err(VulkanError::Api {
                    code:
                        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY | vk::Result::ERROR_OUT_OF_HOST_MEMORY,
                    ..
                }) if jobs > 1 => {
                    jobs = (jobs / 2).max(1);
                    steps += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn construction_checkpoint(&self) -> Result<(), VulkanError> {
        #[cfg(test)]
        {
            let step = self.construction_step.get() + 1;
            self.construction_step.set(step);
            if self.construction_failure.get() == Some(step) {
                if self.construction_oom.get() {
                    return Err(api(
                        "injected allocation pressure",
                        vk::Result::ERROR_OUT_OF_DEVICE_MEMORY,
                    ));
                }
                return Err(VulkanError::InvalidRequest(
                    "injected partial initialization failure",
                ));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn matrix_inner(
        &self,
        upload: Buffer,
        download: Buffer,
        device_input: Buffer,
        device_output: Buffer,
        input: &[u8],
        expected: &[u32],
        jobs: usize,
        rounds: u32,
        dimension: u32,
        generation: u64,
        inject_corruption: bool,
        profile: &mut ComputeProfile,
    ) -> Result<Vec<u32>, VulkanError> {
        let upload_started = Instant::now();
        if !profile.input_resident {
            self.write_buffer(upload, input)?;
        }
        profile.host_upload_seconds = upload_started.elapsed().as_secs_f64();
        let pipeline_started = Instant::now();
        let mut cache = self.matrix_pipeline.borrow_mut();
        profile.pipeline_reused = cache.is_some();
        if cache.is_none() {
            *cache = Some(self.create_matrix_pipeline()?);
        }
        let resources = cache.as_ref().expect("pipeline was initialized");
        let pipeline = resources.pipeline;
        let pipeline_layout = resources.pipeline_layout;
        let set = resources.set;
        let input_info = [vk::DescriptorBufferInfo::default()
            .buffer(device_input.handle)
            .offset(0)
            .range(device_input.size)];
        let output_info = [vk::DescriptorBufferInfo::default()
            .buffer(device_output.handle)
            .offset(0)
            .range(device_output.size)];
        let writes = [
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&input_info),
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(&output_info),
        ];
        unsafe { self.device.update_descriptor_sets(&writes, &[]) };
        profile.pipeline_setup_seconds = pipeline_started.elapsed().as_secs_f64();
        (|| {
            let record_started = Instant::now();
            let command = self.begin_commands()?;
            if let Some(pool) = self.timestamp_pool {
                unsafe { self.device.cmd_reset_query_pool(command, pool, 0, 4) };
            }
            self.write_timestamp(command, 0);
            let upload_region = [vk::BufferCopy::default().size(input.len() as u64)];
            if !profile.input_resident {
                unsafe {
                    self.device.cmd_copy_buffer(
                        command,
                        upload.handle,
                        device_input.handle,
                        &upload_region,
                    );
                }
            }
            unsafe {
                self.device.cmd_fill_buffer(
                    command,
                    device_output.handle,
                    0,
                    device_output.size,
                    0xA55A_1973,
                );
            };
            let before_compute = [
                vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .buffer(device_input.handle)
                    .offset(0)
                    .size(device_input.size),
                vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                    .dst_access_mask(vk::AccessFlags::SHADER_WRITE)
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .buffer(device_output.handle)
                    .offset(0)
                    .size(device_output.size),
            ];
            unsafe {
                self.device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &before_compute,
                    &[],
                );
                self.write_timestamp(command, 1);
                self.device
                    .cmd_bind_pipeline(command, vk::PipelineBindPoint::COMPUTE, pipeline);
                self.device.cmd_bind_descriptor_sets(
                    command,
                    vk::PipelineBindPoint::COMPUTE,
                    pipeline_layout,
                    0,
                    &[set],
                    &[],
                );
                let job_count =
                    u32::try_from(jobs).map_err(|_| VulkanError::InvalidRequest("jobs"))?;
                let mut push_constants = [0_u8; 20];
                let epoch = generation.to_le_bytes();
                for (chunk, value) in push_constants[..12]
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip([job_count, rounds, dimension])
                {
                    chunk.copy_from_slice(&value.to_le_bytes());
                }
                push_constants[12..].copy_from_slice(&epoch);
                self.device.cmd_push_constants(
                    command,
                    pipeline_layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    &push_constants,
                );
                self.device.cmd_dispatch(
                    command,
                    dimension.div_ceil(16),
                    dimension.div_ceil(16),
                    job_count,
                );
            }
            self.write_timestamp(command, 2);
            let after_compute = [vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(device_output.handle)
                .offset(0)
                .size(device_output.size)];
            unsafe {
                self.device.cmd_pipeline_barrier(
                    command,
                    vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &after_compute,
                    &[],
                );
                let download_region =
                    [vk::BufferCopy::default().size(size_of_val(expected) as u64)];
                self.device.cmd_copy_buffer(
                    command,
                    device_output.handle,
                    download.handle,
                    &download_region,
                );
            }
            self.write_timestamp(command, 3);
            profile.command_record_seconds = record_started.elapsed().as_secs_f64();
            let wait_started = Instant::now();
            let submission = self.submit_and_wait_profiled(command, Some(profile));
            profile.submit_wait_seconds = wait_started.elapsed().as_secs_f64();
            submission?;
            if let Some(pool) = self.timestamp_pool {
                let mut stamps = [0_u64; 4];
                match unsafe {
                    self.device.get_query_pool_results(
                        pool,
                        0,
                        &mut stamps,
                        vk::QueryResultFlags::TYPE_64,
                    )
                } {
                    Ok(()) => {
                        profile.gpu = profiling::timestamp_intervals(
                            stamps,
                            self.timestamp_valid_bits,
                            self.timestamp_period_ns,
                            profile.submit_wait_seconds,
                        );
                        if profile.gpu.is_none() {
                            profile.gpu_unavailable_reason =
                                Some("invalid or wrap-ambiguous timestamps".to_owned());
                        }
                    }
                    Err(code) => {
                        profile.gpu_unavailable_reason = Some(format!("timestamp read: {code:?}"));
                    }
                }
            }
            let readback_started = Instant::now();
            let bytes = self.read_buffer(download, size_of_val(expected))?;
            let mut actual = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|chunk| u32::from_le_bytes(*chunk))
                .collect::<Vec<_>>();
            profile.host_readback_seconds = readback_started.elapsed().as_secs_f64();
            if inject_corruption && !actual.is_empty() {
                actual[0] ^= 1;
            }
            let compare_started = Instant::now();
            let mismatch = expected
                .iter()
                .zip(&actual)
                .enumerate()
                .find(|(_, pair)| pair.0 != pair.1);
            profile.cpu_compare_seconds = compare_started.elapsed().as_secs_f64();
            if let Some((result, (&expected, &actual))) = mismatch {
                return Err(VulkanError::MatrixMismatch {
                    result,
                    expected,
                    actual,
                });
            }
            profile.dispatches_verified = 1;
            Ok(actual)
        })()
    }

    #[allow(clippy::too_many_lines)]
    fn create_matrix_pipeline(&self) -> Result<MatrixPipeline, VulkanError> {
        let mut resources = MatrixPipeline::new(self.device.clone());
        let descriptor_bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let set_layout_info =
            vk::DescriptorSetLayoutCreateInfo::default().bindings(&descriptor_bindings);
        let set_layout = unsafe {
            self.device
                .create_descriptor_set_layout(&set_layout_info, None)
        }
        .map_err(|code| api("create_descriptor_set_layout", code))?;
        resources.set_layout = set_layout;
        self.construction_checkpoint()?;
        let push_range = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(20)];
        let set_layouts = [set_layout];
        let pipeline_layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_range);
        let pipeline_layout = unsafe {
            self.device
                .create_pipeline_layout(&pipeline_layout_info, None)
        }
        .map_err(|code| api("create_pipeline_layout", code))?;
        resources.pipeline_layout = pipeline_layout;
        self.construction_checkpoint()?;
        let mut cursor = Cursor::new(MATRIX_SHADER);
        let shader_words = ash::util::read_spv(&mut cursor)
            .map_err(|_| VulkanError::InvalidRequest("embedded SPIR-V"))?;
        let shader_info = vk::ShaderModuleCreateInfo::default().code(&shader_words);
        let shader = unsafe { self.device.create_shader_module(&shader_info, None) }
            .map_err(|code| api("create_shader_module", code))?;
        resources.shader = shader;
        self.construction_checkpoint()?;
        let entry = CString::new("main").expect("static shader entry has no NUL");
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(shader)
            .name(&entry);
        let pipeline_info = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(pipeline_layout)];
        let pipeline = match unsafe {
            self.device
                .create_compute_pipelines(vk::PipelineCache::null(), &pipeline_info, None)
        } {
            Ok(values) => values[0],
            Err((partial, code)) => {
                for handle in partial {
                    unsafe { self.device.destroy_pipeline(handle, None) };
                }
                return Err(api("create_compute_pipeline", code));
            }
        };
        resources.pipeline = pipeline;
        self.construction_checkpoint()?;
        let pool_size = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::STORAGE_BUFFER)
            .descriptor_count(2)];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_size);
        let descriptor_pool = unsafe { self.device.create_descriptor_pool(&pool_info, None) }
            .map_err(|code| api("create_descriptor_pool", code))?;
        resources.descriptor_pool = descriptor_pool;
        self.construction_checkpoint()?;
        let alloc_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(descriptor_pool)
            .set_layouts(&set_layouts);
        let set = unsafe { self.device.allocate_descriptor_sets(&alloc_info) }
            .map_err(|code| api("allocate_descriptor_sets", code))?[0];
        resources.set = set;
        self.construction_checkpoint()?;
        Ok(resources)
    }

    fn create_matrix_buffers(
        &self,
        input_size: u64,
        output_size: u64,
    ) -> Result<MatrixBuffers, VulkanError> {
        let mut resources = MatrixBuffers {
            device: self.device.clone(),
            buffers: Vec::with_capacity(4),
        };
        let upload = self.create_staging(input_size, "matrix upload")?;
        resources.buffers.push(upload);
        self.construction_checkpoint()?;
        let download = self.create_staging(output_size, "matrix download")?;
        resources.buffers.push(download);
        self.construction_checkpoint()?;
        let device_input = self.create_buffer(
            input_size,
            vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            "matrix device input",
        )?;
        resources.buffers.push(device_input);
        self.construction_checkpoint()?;
        let device_output = self.create_buffer(
            output_size,
            vk::BufferUsageFlags::TRANSFER_SRC
                | vk::BufferUsageFlags::TRANSFER_DST
                | vk::BufferUsageFlags::STORAGE_BUFFER,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
            "matrix device output",
        )?;
        resources.buffers.push(device_output);
        self.construction_checkpoint()?;
        Ok(resources)
    }

    fn write_timestamp(&self, command: vk::CommandBuffer, index: u32) {
        if let Some(pool) = self.timestamp_pool {
            // Bottom-of-pipe intervals include boundary/barrier overhead. They
            // are GPU timeline measurements, never CPU wall-time estimates.
            unsafe {
                self.device.cmd_write_timestamp(
                    command,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    pool,
                    index,
                );
            };
        }
    }

    fn create_staging(&self, size: u64, label: &'static str) -> Result<Buffer, VulkanError> {
        self.create_buffer(
            size,
            vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST,
            vk::MemoryPropertyFlags::HOST_VISIBLE,
            vk::MemoryPropertyFlags::HOST_COHERENT | vk::MemoryPropertyFlags::HOST_CACHED,
            label,
        )
    }

    fn create_buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
        label: &'static str,
    ) -> Result<Buffer, VulkanError> {
        self.ensure_ready()?;
        let info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let handle = unsafe { self.device.create_buffer(&info, None) }
            .map_err(|code| api("create_buffer", code))?;
        let requirements = unsafe { self.device.get_buffer_memory_requirements(handle) };
        if let Err(error) = self.construction_checkpoint() {
            unsafe { self.device.destroy_buffer(handle, None) };
            return Err(error);
        }
        let memory_type = self
            .find_memory_type(requirements.memory_type_bits, required, preferred)
            .ok_or(VulkanError::NoMemoryType(label));
        let memory_type = match memory_type {
            Ok(value) => value,
            Err(error) => {
                unsafe { self.device.destroy_buffer(handle, None) };
                return Err(error);
            }
        };
        let allocation_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type);
        let memory = match unsafe { self.device.allocate_memory(&allocation_info, None) } {
            Ok(value) => value,
            Err(code) => {
                unsafe { self.device.destroy_buffer(handle, None) };
                return Err(api("allocate_memory", code));
            }
        };
        if let Err(code) = unsafe { self.device.bind_buffer_memory(handle, memory, 0) } {
            unsafe {
                self.device.free_memory(memory, None);
                self.device.destroy_buffer(handle, None);
            }
            return Err(api("bind_buffer_memory", code));
        }
        if let Err(error) = self.construction_checkpoint() {
            unsafe {
                self.device.destroy_buffer(handle, None);
                self.device.free_memory(memory, None);
            }
            return Err(error);
        }
        let properties = self.memory_properties.memory_types
            [usize::try_from(memory_type).expect("memory type index fits usize")]
        .property_flags;
        Ok(Buffer {
            handle,
            memory,
            size,
            coherent: properties.contains(vk::MemoryPropertyFlags::HOST_COHERENT),
        })
    }

    fn find_memory_type(
        &self,
        bits: u32,
        required: vk::MemoryPropertyFlags,
        preferred: vk::MemoryPropertyFlags,
    ) -> Option<u32> {
        let count = usize::try_from(self.memory_properties.memory_type_count).ok()?;
        let mut fallback = None;
        for index in 0..count {
            let bit = 1_u32.checked_shl(u32::try_from(index).ok()?)?;
            let flags = self.memory_properties.memory_types[index].property_flags;
            if bits & bit != 0 && flags.contains(required) {
                let value = u32::try_from(index).ok()?;
                if flags.contains(preferred) {
                    return Some(value);
                }
                fallback.get_or_insert(value);
            }
        }
        fallback
    }

    fn write_buffer(&self, buffer: Buffer, bytes: &[u8]) -> Result<(), VulkanError> {
        if bytes.len() as u64 > buffer.size {
            return Err(VulkanError::InvalidRequest("host write exceeds buffer"));
        }
        let mapped = unsafe {
            self.device.map_memory(
                buffer.memory,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            )
        }
        .map_err(|code| api("map_memory_write", code))?;
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast(), bytes.len()) };
        if !buffer.coherent {
            let ranges = [vk::MappedMemoryRange::default()
                .memory(buffer.memory)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            if let Err(code) = unsafe { self.device.flush_mapped_memory_ranges(&ranges) } {
                unsafe { self.device.unmap_memory(buffer.memory) };
                return Err(api("flush_mapped_memory_ranges", code));
            }
        }
        unsafe { self.device.unmap_memory(buffer.memory) };
        Ok(())
    }

    fn read_buffer(&self, buffer: Buffer, bytes: usize) -> Result<Vec<u8>, VulkanError> {
        if bytes as u64 > buffer.size {
            return Err(VulkanError::InvalidRequest("host read exceeds buffer"));
        }
        let mapped = unsafe {
            self.device.map_memory(
                buffer.memory,
                0,
                vk::WHOLE_SIZE,
                vk::MemoryMapFlags::empty(),
            )
        }
        .map_err(|code| api("map_memory_read", code))?;
        if !buffer.coherent {
            let ranges = [vk::MappedMemoryRange::default()
                .memory(buffer.memory)
                .offset(0)
                .size(vk::WHOLE_SIZE)];
            if let Err(code) = unsafe { self.device.invalidate_mapped_memory_ranges(&ranges) } {
                unsafe { self.device.unmap_memory(buffer.memory) };
                return Err(api("invalidate_mapped_memory_ranges", code));
            }
        }
        let mut output = vec![0_u8; bytes];
        unsafe { ptr::copy_nonoverlapping(mapped.cast(), output.as_mut_ptr(), bytes) };
        unsafe { self.device.unmap_memory(buffer.memory) };
        Ok(output)
    }

    fn begin_commands(&self) -> Result<vk::CommandBuffer, VulkanError> {
        self.ensure_ready()?;
        let allocate_info = vk::CommandBufferAllocateInfo::default()
            .command_pool(self.command_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        let command = unsafe { self.device.allocate_command_buffers(&allocate_info) }
            .map_err(|code| api("allocate_command_buffer", code))?[0];
        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        if let Err(code) = unsafe { self.device.begin_command_buffer(command, &begin) } {
            unsafe {
                self.device
                    .free_command_buffers(self.command_pool, &[command]);
            }
            return Err(api("begin_command_buffer", code));
        }
        if let Some(pool) = self.timestamp_pool {
            unsafe { self.device.cmd_reset_query_pool(command, pool, 0, 4) };
        }
        Ok(command)
    }

    fn submit_and_wait(&self, command: vk::CommandBuffer) -> Result<(), VulkanError> {
        self.submit_and_wait_profiled(command, None)
    }

    #[allow(clippy::too_many_lines)]
    fn submit_and_wait_profiled(
        &self,
        command: vk::CommandBuffer,
        mut profile: Option<&mut ComputeProfile>,
    ) -> Result<(), VulkanError> {
        let commands = [command];
        if let Err(code) = unsafe { self.device.end_command_buffer(command) } {
            unsafe {
                self.device
                    .free_command_buffers(self.command_pool, &commands);
            }
            return Err(api("end_command_buffer", code));
        }
        let submit = [vk::SubmitInfo::default().command_buffers(&commands)];
        let fence = match unsafe {
            self.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        } {
            Ok(fence) => fence,
            Err(code) => {
                unsafe {
                    self.device
                        .free_command_buffers(self.command_pool, &commands);
                };
                return Err(api("create_fence", code));
            }
        };
        let submit_at = self.campaign_epoch.elapsed().as_secs_f64();
        let submit_result = unsafe { self.device.queue_submit(self.queue, &submit, fence) };
        if let Err(code) = submit_result {
            self.submission_failed.set(true);
            unsafe {
                self.device.destroy_fence(fence, None);
                self.device
                    .free_command_buffers(self.command_pool, &commands);
            }
            return Err(api("queue_submit", code));
        }
        if let Some(profile) = profile.as_mut() {
            profile.dispatches_submitted = 1;
        }
        self.campaign.borrow_mut().submitted(submit_at);
        let wait_result = unsafe {
            self.device
                .wait_for_fences(&[fence], true, FENCE_TIMEOUT_NS)
        };
        self.campaign.borrow_mut().completion(
            self.campaign_epoch.elapsed().as_secs_f64(),
            wait_result.is_ok(),
        );
        if let Err(code) = wait_result {
            // A timeout is not completion. Retain commands, fence and referenced
            // buffers until backend teardown waits for idle; reject further work.
            self.submission_failed.set(true);
            self.pending_commands.borrow_mut().push((command, fence));
            return Err(api("wait_for_fence", code));
        }
        if let Some(profile) = profile {
            profile.dispatches_completed = 1;
        }
        if let Some(pool) = self.timestamp_pool {
            let mut stamps = [0_u64; 4];
            if unsafe {
                self.device.get_query_pool_results(
                    pool,
                    0,
                    &mut stamps,
                    vk::QueryResultFlags::TYPE_64,
                )
            }
            .is_ok()
                && let Some(intervals) = profiling::timestamp_intervals(
                    stamps,
                    self.timestamp_valid_bits,
                    self.timestamp_period_ns,
                    self.campaign_epoch.elapsed().as_secs_f64() - submit_at,
                )
            {
                let mut campaign = self.campaign.borrow_mut();
                let name = campaign
                    .last_batch
                    .as_ref()
                    .map(|b| b.phase.clone())
                    .unwrap_or_default();
                let phase = campaign.phase();
                phase.gpu_seconds +=
                    intervals.h2d_seconds + intervals.kernel_seconds + intervals.d2h_seconds;
                phase.gpu_samples += 1;
                match name.as_str() {
                    "retention" => {
                        phase.gpu_d2h_seconds += intervals.h2d_seconds
                            + intervals.kernel_seconds
                            + intervals.d2h_seconds;
                    }
                    "transfer" => {
                        phase.gpu_h2d_seconds += intervals.h2d_seconds;
                        phase.gpu_d2h_seconds += intervals.kernel_seconds + intervals.d2h_seconds;
                    }
                    _ => {
                        phase.gpu_h2d_seconds += intervals.h2d_seconds;
                        phase.gpu_kernel_seconds += intervals.kernel_seconds;
                        phase.gpu_d2h_seconds += intervals.d2h_seconds;
                    }
                }
            }
        }
        unsafe {
            self.device.destroy_fence(fence, None);
            self.device
                .free_command_buffers(self.command_pool, &commands);
        }
        Ok(())
    }

    fn ensure_ready(&self) -> Result<(), VulkanError> {
        if self.submission_failed.get() {
            return Err(VulkanError::InvalidRequest(
                "backend stopped after submission failure",
            ));
        }
        Ok(())
    }

    fn profile_batch<T>(
        &self,
        batch: ReplayBatch,
        run: impl FnOnce() -> Result<T, VulkanError>,
    ) -> Result<T, VulkanError> {
        batch.validate().map_err(VulkanError::InvalidRequest)?;
        self.campaign
            .borrow_mut()
            .begin(batch, self.campaign_epoch.elapsed().as_secs_f64());
        let result = run();
        self.campaign
            .borrow_mut()
            .finish(self.campaign_epoch.elapsed().as_secs_f64(), result.is_ok());
        result
    }

    /// Snapshot includes final host verification/drain time, not physical GPU idle.
    #[must_use]
    pub fn campaign_profile(&self) -> CampaignProfile {
        self.campaign
            .borrow()
            .snapshot(self.campaign_epoch.elapsed().as_secs_f64())
    }

    fn destroy_buffer(&self, buffer: Buffer) {
        if self.submission_failed.get() {
            self.deferred_buffers.borrow_mut().push(buffer);
            return;
        }
        unsafe {
            self.device.destroy_buffer(buffer.handle, None);
            self.device.free_memory(buffer.memory, None);
        }
    }

    fn destroy_pool(&mut self) {
        for buffer in self.pool.drain(..) {
            unsafe {
                self.device.destroy_buffer(buffer.handle, None);
                self.device.free_memory(buffer.memory, None);
            }
        }
    }
}

impl Drop for VulkanBackend {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
        }
        self.submission_failed.set(false);
        let deferred = std::mem::take(self.deferred_buffers.get_mut());
        for buffer in deferred {
            self.destroy_buffer(buffer);
        }
        for (_, fence) in self.pending_commands.get_mut().drain(..) {
            unsafe { self.device.destroy_fence(fence, None) };
        }
        self.matrix_pipeline.get_mut().take();
        self.matrix_buffers.get_mut().take();
        self.destroy_pool();
        unsafe {
            if let Some(pool) = self.timestamp_pool {
                self.device.destroy_query_pool(pool, None);
            }
            self.device.destroy_command_pool(self.command_pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

fn create_instance() -> Result<(Entry, Instance), VulkanError> {
    let entry = unsafe { Entry::load() }.map_err(|error| VulkanError::Loader(error.to_string()))?;
    let name = CString::new("DIT-01 GPU-VRAM").expect("static application name has no NUL");
    let application = vk::ApplicationInfo::default()
        .application_name(&name)
        .application_version(vk::make_api_version(0, 0, 1, 0))
        .engine_name(&name)
        .engine_version(vk::make_api_version(0, 0, 1, 0))
        .api_version(vk::API_VERSION_1_1);
    let info = vk::InstanceCreateInfo::default().application_info(&application);
    let instance = unsafe { entry.create_instance(&info, None) }
        .map_err(|code| api("create_instance", code))?;
    Ok((entry, instance))
}

fn enumerate_infos(instance: &Instance) -> Result<Vec<VulkanDeviceInfo>, VulkanError> {
    let physicals = unsafe { instance.enumerate_physical_devices() }
        .map_err(|code| api("enumerate_physical_devices", code))?;
    enumerate_infos_for(instance, &physicals)
}

fn enumerate_infos_for(
    instance: &Instance,
    physicals: &[vk::PhysicalDevice],
) -> Result<Vec<VulkanDeviceInfo>, VulkanError> {
    physicals
        .iter()
        .copied()
        .enumerate()
        .map(|(index, physical)| device_info(instance, physical, index))
        .collect()
}

fn device_info(
    instance: &Instance,
    physical: vk::PhysicalDevice,
    index: usize,
) -> Result<VulkanDeviceInfo, VulkanError> {
    let properties = unsafe { instance.get_physical_device_properties(physical) };
    let memory = unsafe { instance.get_physical_device_memory_properties(physical) };
    let queues = unsafe { instance.get_physical_device_queue_family_properties(physical) };
    let extensions = unsafe { instance.enumerate_device_extension_properties(physical) }
        .map_err(|code| api("enumerate_device_extensions", code))?;
    let extension_names = extensions
        .iter()
        .map(|item| {
            unsafe { CStr::from_ptr(item.extension_name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    let budget_supported = extension_names
        .iter()
        .any(|name| name == "VK_EXT_memory_budget");
    let pci_supported = extension_names
        .iter()
        .any(|name| name == "VK_EXT_pci_bus_info");
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut properties2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
    unsafe { instance.get_physical_device_properties2(physical, &mut properties2) };
    let pci_address = if pci_supported {
        let mut pci = vk::PhysicalDevicePCIBusInfoPropertiesEXT::default();
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut pci);
        unsafe { instance.get_physical_device_properties2(physical, &mut props) };
        Some(format!(
            "{:04x}:{:02x}:{:02x}.{}",
            pci.pci_domain, pci.pci_bus, pci.pci_device, pci.pci_function
        ))
    } else {
        None
    };
    let mut device_local_bytes = 0_u64;
    let mut local_heap_index = None;
    for index in 0..usize::try_from(memory.memory_heap_count).unwrap_or(0) {
        let heap = memory.memory_heaps[index];
        if heap.flags.contains(vk::MemoryHeapFlags::DEVICE_LOCAL) && heap.size > device_local_bytes
        {
            device_local_bytes = heap.size;
            local_heap_index = Some(index);
        }
    }
    let (budget_bytes, usage_bytes) = if budget_supported {
        let mut budget = vk::PhysicalDeviceMemoryBudgetPropertiesEXT::default();
        let mut memory2 = vk::PhysicalDeviceMemoryProperties2::default().push_next(&mut budget);
        unsafe { instance.get_physical_device_memory_properties2(physical, &mut memory2) };
        local_heap_index.map_or((None, None), |heap| {
            (
                Some(budget.heap_budget[heap]),
                Some(budget.heap_usage[heap]),
            )
        })
    } else {
        (None, None)
    };
    let compute_queue_family = queues
        .iter()
        .position(|queue| {
            queue.queue_count > 0 && queue.queue_flags.contains(vk::QueueFlags::COMPUTE)
        })
        .and_then(|value| u32::try_from(value).ok());
    let device_type = format!("{:?}", properties.device_type);
    let software = properties.device_type == vk::PhysicalDeviceType::CPU;
    let rejection_reason = if software {
        Some("software/CPU Vulkan renderer is forbidden for hardware validation".to_owned())
    } else if compute_queue_family.is_none() {
        Some("no Vulkan compute queue".to_owned())
    } else {
        None
    };
    let hardware_eligible = rejection_reason.is_none();
    Ok(VulkanDeviceInfo {
        index,
        name: unsafe { CStr::from_ptr(properties.device_name.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
        device_type,
        hardware_eligible,
        rejection_reason,
        vendor_id: properties.vendor_id,
        device_id: properties.device_id,
        api_version: format!(
            "{}.{}.{}",
            vk::api_version_major(properties.api_version),
            vk::api_version_minor(properties.api_version),
            vk::api_version_patch(properties.api_version)
        ),
        driver_version_raw: properties.driver_version,
        uuid: hex_bytes(&id.device_uuid),
        pci_address,
        compute_queue_family,
        device_local_bytes,
        budget_bytes,
        usage_bytes,
        memory_budget_supported: budget_supported,
    })
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn words_as_bytes(words: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(size_of_val(words));
    for word in words {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_compute_evidence_deserializes_without_profile() {
        let legacy = r#"{"jobs":1,"rounds":1,"results_verified":64,
            "uploaded_bytes":512,"downloaded_bytes":256,"elapsed_seconds":0.5,
            "shader_sha256":"old","result_sha256":"old"}"#;
        let evidence: ComputeEvidence = serde_json::from_str(legacy).unwrap();
        assert!(evidence.profile.is_none());
        assert_eq!(evidence.results_verified, 64);
        let mut current = evidence;
        current.profile = Some(ComputeProfile {
            version: 1,
            seed: 7,
            generation: 2,
            ..ComputeProfile::default()
        });
        let serialized = serde_json::to_string(&current).unwrap();
        let decoded: ComputeEvidence = serde_json::from_str(&serialized).unwrap();
        assert_eq!(decoded.profile.unwrap().seed, 7);
    }

    #[test]
    #[ignore = "explicit bounded smoke requires a real Vulkan GPU"]
    fn persistent_matrix_hardware_smoke() {
        let backend = VulkanBackend::open(None).unwrap();
        let first = backend.verify_matrix(7, 0, 32, 2).unwrap();
        let profile = first.profile.as_ref().unwrap();
        assert!(!profile.buffers_reused);
        assert!(!profile.pipeline_reused);
        assert!(profile.end_to_end_seconds >= first.elapsed_seconds);
        assert!(profile.gpu.is_some() || profile.gpu_unavailable_reason.is_some());
        let second = backend.verify_matrix(7, 0, 32, 2).unwrap();
        let profile = second.profile.unwrap();
        assert!(profile.buffers_reused && profile.pipeline_reused);
        assert_eq!(first.result_sha256, second.result_sha256);
        assert_eq!(profile.dispatches_verified, 1);
        let resized = backend.verify_matrix(7, 1, 17, 2).unwrap();
        let profile = resized.profile.unwrap();
        assert!(profile.buffers_reused && profile.pipeline_reused);
        assert_eq!(resized.results_verified, 17 * 64 * 64);
        assert!(matches!(
            backend.verify_matrix_injected(7, 2, 17, 2),
            Err(VulkanError::MatrixMismatch { .. })
        ));
        let failed = backend.last_compute_profile().unwrap();
        assert_eq!(failed.dispatches_submitted, 1);
        assert_eq!(failed.dispatches_completed, 1);
        assert_eq!(failed.dispatches_verified, 0);
        assert!(backend.verify_matrix(7, 3, 17, 2).is_ok());
        // Simulate the terminal state without inducing a GPU hang.
        backend.submission_failed.set(true);
        assert!(backend.submission_stopped());
        assert!(backend.verify_matrix(7, 4, 32, 2).is_err());
    }

    #[test]
    #[ignore = "explicit hardware partial-construction fault campaign"]
    fn partial_initialization_hardware_smoke() {
        let backend = VulkanBackend::open(None).unwrap();
        backend.prepare_matrix_resources(8, 2).unwrap();
        let steps = backend.construction_step.get();
        assert_eq!(steps, 18);
        for step in 1..=steps {
            backend.construction_step.set(0);
            backend.construction_failure.set(Some(step));
            assert!(
                backend.prepare_matrix_resources(8, 2).is_err(),
                "step {step}"
            );
            assert!(backend.matrix_buffers.borrow().is_none());
            assert!(backend.matrix_pipeline.borrow().is_none());
            backend.construction_failure.set(None);
            backend.prepare_matrix_resources(8, 2).unwrap();
            backend.verify_matrix(7, step as u64, 8, 2).unwrap();
        }
        for step in [1, 14] {
            backend.construction_step.set(0);
            backend.construction_failure.set(Some(step));
            backend.construction_oom.set(true);
            let (jobs, backoffs) = backend.prepare_matrix_resources(8, 2).unwrap();
            assert_eq!((jobs, backoffs), (4, 1));
            backend.verify_matrix(7, 99, jobs, 2).unwrap();
        }
    }

    #[test]
    fn shader_identity_matches_embedded_bytes() {
        assert_eq!(
            format!("{:x}", Sha256::digest(MATRIX_SHADER)),
            SHADER_SHA256
        );
        assert_eq!(&MATRIX_SHADER[..4], &[0x03, 0x02, 0x23, 0x07]);
    }

    #[test]
    fn software_device_policy_is_explicit_in_schema() {
        let fixture = VulkanDeviceInfo {
            index: 1,
            name: "llvmpipe".to_owned(),
            device_type: "CPU".to_owned(),
            hardware_eligible: false,
            rejection_reason: Some("software renderer".to_owned()),
            vendor_id: 0,
            device_id: 0,
            api_version: "1.3.0".to_owned(),
            driver_version_raw: 0,
            uuid: "00".repeat(16),
            pci_address: None,
            compute_queue_family: Some(0),
            device_local_bytes: 0,
            budget_bytes: None,
            usage_bytes: None,
            memory_budget_supported: false,
        };
        assert!(!fixture.hardware_eligible);
        assert!(fixture.rejection_reason.is_some());
    }

    #[test]
    #[ignore = "explicit transfer/retention timing and negative hardware smoke"]
    fn campaign_profile_hardware_smoke() {
        let mut backend = VulkanBackend::open(None).unwrap();
        backend
            .allocate_pool(4 * 1024 * 1024 + 3, 4 * 1024 * 1024 + 3)
            .unwrap();
        assert!(backend.allocated_bytes().is_multiple_of(4096));
        backend.verify_transfer(0, 0, 4096, 7, 0).unwrap();
        backend.verify_retained(0, 0, 4096, 7, 0).unwrap();
        assert!(backend.verify_retained(0, 0, 4096, 8, 0).is_err());
        assert!(backend.verify_transfer_injected(0, 0, 4096, 9, 0).is_err());
        let p = backend.campaign_profile();
        assert_eq!(
            (p.transfer.completed, p.transfer.verified, p.transfer.failed),
            (2, 1, 1)
        );
        assert_eq!(
            (
                p.retention.completed,
                p.retention.verified,
                p.retention.failed
            ),
            (2, 1, 1)
        );
        assert_eq!(p.first_failed_batch.unwrap().phase, "retention");
        assert!(p.dit_queue_empty_lower_bound_seconds <= p.observed_seconds);
    }
}
