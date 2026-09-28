//! Safety-gated Linux block backend for one bounded `NVMe` target.

use dit_contracts::{DeviceId, SCHEMA_VERSION};
use memmap2::MmapMut;
use nvme_inventory::{Inventory, SelectionPlan, revalidate};
use nvme_io::{BackendError, StorageBackend, check_range};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const MIN_DIT_REGION_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Namespace,
    Partition,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundedWritePlan {
    pub schema_version: u32,
    pub created_unix_ms: u64,
    pub inventory_captured_unix_ms: u64,
    pub namespace_stable_id: DeviceId,
    pub target_stable_id: DeviceId,
    pub target_kind: TargetKind,
    pub target_node: PathBuf,
    pub model: String,
    pub serial: String,
    pub driver: String,
    pub target_capacity_bytes: u64,
    pub logical_block_size: u32,
    pub range_start_bytes: u64,
    pub range_length_bytes: u64,
    pub max_host_write_bytes: u64,
    pub duration_seconds: u64,
    pub target_operations: u64,
    pub base_seed: u64,
}

#[derive(Debug, Error)]
pub enum AuthorizationError {
    #[error("selected namespace alias {0} is absent from the selection plan")]
    SelectionAliasMissing(usize),
    #[error("selection revalidation failed: {0}")]
    Revalidation(String),
    #[error("selected namespace disappeared from current inventory")]
    NamespaceMissing,
    #[error("target {0} does not belong to the selected namespace")]
    TargetNotOwned(PathBuf),
    #[error("target is protected or not a write candidate: {0}")]
    Protected(String),
    #[error("invalid target geometry")]
    InvalidGeometry,
    #[error("bounded region is invalid: {0}")]
    InvalidRange(String),
    #[error("host-write budget is invalid: {0}")]
    InvalidBudget(String),
    #[error("duration and operation count must be greater than zero")]
    InvalidSchedule,
    #[error("explicit data-loss acknowledgement is missing")]
    MissingAcknowledgement,
    #[error("confirmation mismatch; expected exact target ID: {0}")]
    ConfirmationMismatch(DeviceId),
    #[error("backend open failed: {0}")]
    Backend(#[from] BackendError),
}

#[derive(Clone, Copy, Debug)]
pub struct BoundedRequest {
    pub range_start_bytes: u64,
    pub range_length_bytes: u64,
    pub max_host_write_bytes: u64,
    pub duration_seconds: u64,
    pub target_operations: u64,
    pub base_seed: u64,
}

/// Creates a non-executing bounded-write plan from a stable selection and current inventory.
///
/// # Errors
///
/// Returns [`AuthorizationError`] if identity, ownership, safety, alignment, range, budget or
/// schedule checks fail. This function never opens the target device node.
pub fn prepare_bounded_plan(
    inventory: &Inventory,
    selection: &SelectionPlan,
    selected_alias: usize,
    target_node: &Path,
    request: BoundedRequest,
) -> Result<BoundedWritePlan, AuthorizationError> {
    let selected = selection
        .devices
        .iter()
        .find(|device| device.display_index == selected_alias)
        .ok_or(AuthorizationError::SelectionAliasMissing(selected_alias))?;
    let revalidation = revalidate(selection, inventory);
    if !revalidation.valid {
        return Err(AuthorizationError::Revalidation(
            revalidation.errors.join("; "),
        ));
    }
    let namespace = inventory
        .devices
        .iter()
        .find(|device| device.stable_id == selected.stable_id)
        .ok_or(AuthorizationError::NamespaceMissing)?;

    let (target_stable_id, target_kind, capacity, logical_block_size, write_candidate, reasons) =
        if target_node == namespace.device_node {
            (
                namespace.stable_id.clone(),
                TargetKind::Namespace,
                namespace.capacity_bytes,
                namespace.logical_block_size,
                namespace.safety.write_candidate,
                namespace.safety.reasons.clone(),
            )
        } else if let Some(partition) = namespace
            .partition_details
            .iter()
            .find(|partition| partition.device_node == target_node)
        {
            (
                partition.stable_id.clone(),
                TargetKind::Partition,
                partition.capacity_bytes,
                partition.logical_block_size,
                partition.write_candidate,
                partition.reasons.clone(),
            )
        } else {
            return Err(AuthorizationError::TargetNotOwned(
                target_node.to_path_buf(),
            ));
        };

    if !write_candidate {
        return Err(AuthorizationError::Protected(if reasons.is_empty() {
            "no safe write classification".to_owned()
        } else {
            reasons.join("; ")
        }));
    }
    if capacity == 0 || logical_block_size == 0 {
        return Err(AuthorizationError::InvalidGeometry);
    }
    validate_request(capacity, logical_block_size, request)?;

    Ok(BoundedWritePlan {
        schema_version: SCHEMA_VERSION,
        created_unix_ms: unix_ms(),
        inventory_captured_unix_ms: inventory.captured_unix_ms,
        namespace_stable_id: namespace.stable_id.clone(),
        target_stable_id,
        target_kind,
        target_node: target_node.to_path_buf(),
        model: namespace.model.clone(),
        serial: namespace.serial.clone(),
        driver: namespace.driver.clone(),
        target_capacity_bytes: capacity,
        logical_block_size,
        range_start_bytes: request.range_start_bytes,
        range_length_bytes: request.range_length_bytes,
        max_host_write_bytes: request.max_host_write_bytes,
        duration_seconds: request.duration_seconds,
        target_operations: request.target_operations,
        base_seed: request.base_seed,
    })
}

/// Revalidates a saved plan against a new inventory snapshot.
///
/// # Errors
///
/// Returns [`AuthorizationError`] if the namespace/target identity, device node, geometry or
/// write-candidate classification changed.
pub fn revalidate_bounded_plan(
    plan: &BoundedWritePlan,
    current: &Inventory,
) -> Result<(), AuthorizationError> {
    let namespace = current
        .devices
        .iter()
        .find(|device| device.stable_id == plan.namespace_stable_id)
        .ok_or(AuthorizationError::NamespaceMissing)?;
    let (stable_id, node, capacity, logical, write_candidate, reasons) = match plan.target_kind {
        TargetKind::Namespace => (
            &namespace.stable_id,
            &namespace.device_node,
            namespace.capacity_bytes,
            namespace.logical_block_size,
            namespace.safety.write_candidate,
            &namespace.safety.reasons,
        ),
        TargetKind::Partition => {
            let partition = namespace
                .partition_details
                .iter()
                .find(|partition| partition.stable_id == plan.target_stable_id)
                .ok_or_else(|| AuthorizationError::TargetNotOwned(plan.target_node.clone()))?;
            (
                &partition.stable_id,
                &partition.device_node,
                partition.capacity_bytes,
                partition.logical_block_size,
                partition.write_candidate,
                &partition.reasons,
            )
        }
    };
    if stable_id != &plan.target_stable_id || node != &plan.target_node {
        return Err(AuthorizationError::Revalidation(
            "target stable ID or device node changed".to_owned(),
        ));
    }
    if capacity != plan.target_capacity_bytes || logical != plan.logical_block_size {
        return Err(AuthorizationError::Revalidation(
            "target capacity or logical block size changed".to_owned(),
        ));
    }
    if !write_candidate {
        return Err(AuthorizationError::Protected(reasons.join("; ")));
    }
    validate_request(
        capacity,
        logical,
        BoundedRequest {
            range_start_bytes: plan.range_start_bytes,
            range_length_bytes: plan.range_length_bytes,
            max_host_write_bytes: plan.max_host_write_bytes,
            duration_seconds: plan.duration_seconds,
            target_operations: plan.target_operations,
            base_seed: plan.base_seed,
        },
    )
}

/// Opens the exact revalidated target with `O_DIRECT | O_DSYNC | O_NOFOLLOW`.
///
/// The confirmation must exactly match `plan.target_stable_id`, and `acknowledge_data_loss`
/// must be true. The returned backend exposes only the authorized byte range.
///
/// # Errors
///
/// Returns [`AuthorizationError`] on missing confirmation, changed safety/identity, non-block
/// targets, alignment failures or OS open errors.
pub fn open_bounded(
    plan: &BoundedWritePlan,
    current: &Inventory,
    confirmation: &str,
    acknowledge_data_loss: bool,
) -> Result<LinuxBlockBackend, AuthorizationError> {
    if !acknowledge_data_loss {
        return Err(AuthorizationError::MissingAcknowledgement);
    }
    if confirmation != plan.target_stable_id.0 {
        return Err(AuthorizationError::ConfirmationMismatch(
            plan.target_stable_id.clone(),
        ));
    }
    revalidate_bounded_plan(plan, current)?;
    Ok(LinuxBlockBackend::open(plan)?)
}

#[derive(Debug)]
pub struct LinuxBlockBackend {
    file: File,
    id: String,
    path: PathBuf,
    base_offset: u64,
    capacity: u64,
    alignment: u32,
    max_host_write_bytes: u64,
    host_bytes_written: u64,
}

impl LinuxBlockBackend {
    fn open(plan: &BoundedWritePlan) -> Result<Self, BackendError> {
        let before = fs_metadata_block(&plan.target_node)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT | libc::O_DSYNC | libc::O_NOFOLLOW)
            .open(&plan.target_node)?;
        let after = file.metadata()?;
        if !after.file_type().is_block_device() || after.rdev() != before.rdev() {
            return Err(BackendError::IdentityChanged(format!(
                "{} changed while opening",
                plan.target_node.display()
            )));
        }
        Ok(Self {
            file,
            id: plan.target_stable_id.0.clone(),
            path: plan.target_node.clone(),
            base_offset: plan.range_start_bytes,
            capacity: plan.range_length_bytes,
            alignment: plan.logical_block_size,
            max_host_write_bytes: plan.max_host_write_bytes,
            host_bytes_written: 0,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn check_alignment(&self, offset: u64, length: usize) -> Result<(), BackendError> {
        let alignment = u64::from(self.alignment);
        if !offset.is_multiple_of(alignment)
            || !u64::try_from(length)
                .unwrap_or(u64::MAX)
                .is_multiple_of(alignment)
        {
            return Err(BackendError::Unaligned {
                offset,
                length,
                alignment: self.alignment,
            });
        }
        Ok(())
    }
}

impl StorageBackend for LinuxBlockBackend {
    fn id(&self) -> &str {
        &self.id
    }

    fn capacity_bytes(&self) -> u64 {
        self.capacity
    }

    fn write_all_at(&mut self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity)?;
        self.check_alignment(offset, data.len())?;
        let length = u64::try_from(data.len()).unwrap_or(u64::MAX);
        let next_total =
            reserve_write_budget(self.host_bytes_written, self.max_host_write_bytes, length)?;
        let mut aligned = MmapMut::map_anon(data.len())?;
        aligned.copy_from_slice(data);
        let written = self.file.write_at(&aligned, self.base_offset + offset)?;
        if written != data.len() {
            return Err(BackendError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("short direct write: {written}/{}", data.len()),
            )));
        }
        self.host_bytes_written = next_total;
        Ok(())
    }

    fn read_exact_at(&mut self, offset: u64, data: &mut [u8]) -> Result<(), BackendError> {
        check_range(offset, data.len(), self.capacity)?;
        self.check_alignment(offset, data.len())?;
        let mut aligned = MmapMut::map_anon(data.len())?;
        let read = self.file.read_at(&mut aligned, self.base_offset + offset)?;
        if read != data.len() {
            return Err(BackendError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("short direct read: {read}/{}", data.len()),
            )));
        }
        data.copy_from_slice(&aligned);
        Ok(())
    }

    fn flush(&mut self) -> Result<(), BackendError> {
        self.file.sync_data()?;
        Ok(())
    }

    fn host_bytes_written(&self) -> u64 {
        self.host_bytes_written
    }
}

fn validate_request(
    capacity: u64,
    logical_block_size: u32,
    request: BoundedRequest,
) -> Result<(), AuthorizationError> {
    let alignment = u64::from(logical_block_size);
    let end = request
        .range_start_bytes
        .checked_add(request.range_length_bytes)
        .ok_or_else(|| AuthorizationError::InvalidRange("range overflow".to_owned()))?;
    if request.range_length_bytes < MIN_DIT_REGION_BYTES {
        return Err(AuthorizationError::InvalidRange(format!(
            "region must be at least {MIN_DIT_REGION_BYTES} bytes"
        )));
    }
    if end > capacity
        || !request.range_start_bytes.is_multiple_of(alignment)
        || !request.range_length_bytes.is_multiple_of(alignment)
    {
        return Err(AuthorizationError::InvalidRange(format!(
            "range must fit capacity {capacity} and align to {logical_block_size} bytes"
        )));
    }
    if request.max_host_write_bytes < 4096 || !request.max_host_write_bytes.is_multiple_of(4096) {
        return Err(AuthorizationError::InvalidBudget(
            "budget must be at least and aligned to 4096 bytes".to_owned(),
        ));
    }
    if request.duration_seconds == 0 || request.target_operations == 0 {
        return Err(AuthorizationError::InvalidSchedule);
    }
    Ok(())
}

fn fs_metadata_block(path: &Path) -> Result<std::fs::Metadata, BackendError> {
    let metadata = path.symlink_metadata()?;
    if !metadata.file_type().is_block_device() {
        return Err(BackendError::UnsafeTarget(path.to_path_buf()));
    }
    Ok(metadata)
}

fn reserve_write_budget(current: u64, maximum: u64, attempted: u64) -> Result<u64, BackendError> {
    let remaining = maximum.saturating_sub(current);
    if attempted > remaining {
        Err(BackendError::WriteBudgetExceeded {
            attempted,
            remaining,
        })
    } else {
        Ok(current + attempted)
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nvme_inventory::{
        AerSnapshot, NvmeDevice, SafetyClassification, SmartSnapshot, select_indexes,
    };

    #[test]
    fn bounded_plan_requires_candidate_alignment_and_ownership() {
        let inventory = inventory(true);
        let selection = select_indexes(&inventory, &[1]).unwrap();
        let request = request();
        let plan = prepare_bounded_plan(
            &inventory,
            &selection,
            1,
            Path::new("/dev/nvme0n1"),
            request,
        )
        .unwrap();
        assert_eq!(plan.range_length_bytes, 8 * 1024 * 1024);
        assert_eq!(plan.max_host_write_bytes, 4 * 1024 * 1024);
        assert!(revalidate_bounded_plan(&plan, &inventory).is_ok());

        let wrong_target = prepare_bounded_plan(
            &inventory,
            &selection,
            1,
            Path::new("/dev/nvme9n1"),
            request,
        );
        assert!(matches!(
            wrong_target,
            Err(AuthorizationError::TargetNotOwned(_))
        ));

        let mut unaligned = request;
        unaligned.range_start_bytes = 1;
        assert!(matches!(
            prepare_bounded_plan(
                &inventory,
                &selection,
                1,
                Path::new("/dev/nvme0n1"),
                unaligned
            ),
            Err(AuthorizationError::InvalidRange(_))
        ));
    }

    #[test]
    fn protected_device_cannot_receive_a_bounded_plan() {
        let inventory = inventory(false);
        let selection = select_indexes(&inventory, &[1]).unwrap();
        assert!(matches!(
            prepare_bounded_plan(
                &inventory,
                &selection,
                1,
                Path::new("/dev/nvme0n1"),
                request()
            ),
            Err(AuthorizationError::Protected(_))
        ));
    }

    #[test]
    fn open_requires_acknowledgement_and_exact_stable_id() {
        let inventory = inventory(true);
        let selection = select_indexes(&inventory, &[1]).unwrap();
        let plan = prepare_bounded_plan(
            &inventory,
            &selection,
            1,
            Path::new("/dev/nvme0n1"),
            request(),
        )
        .unwrap();
        assert!(matches!(
            open_bounded(&plan, &inventory, &plan.target_stable_id.0, false),
            Err(AuthorizationError::MissingAcknowledgement)
        ));
        assert!(matches!(
            open_bounded(&plan, &inventory, "wrong-id", true),
            Err(AuthorizationError::ConfirmationMismatch(_))
        ));
    }

    #[test]
    fn budget_is_rejected_before_submission() {
        assert_eq!(reserve_write_budget(4096, 8192, 4096).unwrap(), 8192);
        assert!(matches!(
            reserve_write_budget(4096, 8192, 8192),
            Err(BackendError::WriteBudgetExceeded {
                attempted: 8192,
                remaining: 4096
            })
        ));
    }

    fn request() -> BoundedRequest {
        BoundedRequest {
            range_start_bytes: 1024 * 1024,
            range_length_bytes: 8 * 1024 * 1024,
            max_host_write_bytes: 4 * 1024 * 1024,
            duration_seconds: 60,
            target_operations: 16,
            base_seed: 42,
        }
    }

    fn inventory(candidate: bool) -> Inventory {
        Inventory {
            schema_version: 1,
            captured_unix_ms: 10,
            devices: vec![NvmeDevice {
                index: 1,
                stable_id: DeviceId("nvme:fixture:eui.fixture:0000:01:00.0".into()),
                device_node: PathBuf::from("/dev/nvme0n1"),
                namespace: "nvme0n1".into(),
                namespace_id: Some(1),
                controller: "nvme0".into(),
                driver: "nvme".into(),
                transport: "pcie".into(),
                model: "Fixture NVMe".into(),
                serial: "FIXTURE".into(),
                firmware: "1.0".into(),
                capacity_bytes: 64 * 1024 * 1024,
                logical_block_size: 512,
                physical_block_size: 4096,
                wwid: Some("eui.fixture".into()),
                nguid: None,
                eui: None,
                pci_bdf: Some("0000:01:00.0".into()),
                root_port_bdf: Some("0000:00:01.0".into()),
                current_link_speed: Some("16.0 GT/s PCIe".into()),
                current_link_width: Some(4),
                partitions: Vec::new(),
                partition_details: Vec::new(),
                mount_points: if candidate {
                    Vec::new()
                } else {
                    vec!["/".into()]
                },
                holders: Vec::new(),
                smart: SmartSnapshot::default(),
                aer: AerSnapshot::default(),
                safety: SafetyClassification {
                    read_only_allowed: true,
                    write_candidate: candidate,
                    protected: !candidate,
                    reasons: if candidate {
                        Vec::new()
                    } else {
                        vec!["running root filesystem".into()]
                    },
                },
            }],
            warnings: Vec::new(),
        }
    }
}
