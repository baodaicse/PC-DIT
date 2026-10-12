//! Read-only `NVMe`, `PCIe` topology, telemetry and safety inventory.

use dit_contracts::{DeviceId, SCHEMA_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub sys_root: PathBuf,
    pub proc_root: PathBuf,
    pub dev_root: PathBuf,
    pub collect_smart: bool,
    pub nvme_program: PathBuf,
    /// Extra device nodes that must be marked protected, such as a results disk.
    pub protected_devices: Vec<PathBuf>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sys_root: PathBuf::from("/sys"),
            proc_root: PathBuf::from("/proc"),
            dev_root: PathBuf::from("/dev"),
            collect_smart: true,
            nvme_program: PathBuf::from("nvme"),
            protected_devices: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inventory {
    pub schema_version: u32,
    pub captured_unix_ms: u64,
    pub devices: Vec<NvmeDevice>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NvmeDevice {
    /// One-based alias valid only for this inventory snapshot.
    pub index: usize,
    pub stable_id: DeviceId,
    pub device_node: PathBuf,
    pub namespace: String,
    pub namespace_id: Option<u32>,
    pub controller: String,
    pub driver: String,
    pub transport: String,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub capacity_bytes: u64,
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub wwid: Option<String>,
    pub nguid: Option<String>,
    pub eui: Option<String>,
    pub pci_bdf: Option<String>,
    pub root_port_bdf: Option<String>,
    pub current_link_speed: Option<String>,
    pub current_link_width: Option<u32>,
    pub partitions: Vec<String>,
    pub partition_details: Vec<PartitionInfo>,
    pub mount_points: Vec<String>,
    pub holders: Vec<String>,
    pub smart: SmartSnapshot,
    pub aer: AerSnapshot,
    pub safety: SafetyClassification,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionInfo {
    pub name: String,
    pub stable_id: DeviceId,
    pub device_node: PathBuf,
    pub start_bytes: u64,
    pub capacity_bytes: u64,
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub mount_points: Vec<String>,
    pub holders: Vec<String>,
    pub active_swap: bool,
    pub protected: bool,
    pub write_candidate: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartSnapshot {
    pub available: bool,
    pub temperature_celsius: Option<i64>,
    pub available_spare_percent: Option<u64>,
    pub percentage_used: Option<u64>,
    pub data_units_read: Option<u64>,
    pub data_units_written: Option<u64>,
    pub controller_busy_time_minutes: Option<u64>,
    pub media_and_data_integrity_errors: Option<u64>,
    pub error_log_entries: Option<u64>,
    /// Minutes the drive has spent above its warning and its critical composite temperature.
    #[serde(default)]
    pub warning_temperature_minutes: Option<u64>,
    #[serde(default)]
    pub critical_temperature_minutes: Option<u64>,
    /// Times the drive entered thermal management level 1 (light) and 2 (heavy) throttling, and
    /// the seconds it spent there.
    #[serde(default)]
    pub thermal_level1_transitions: Option<u64>,
    #[serde(default)]
    pub thermal_level2_transitions: Option<u64>,
    #[serde(default)]
    pub thermal_level1_seconds: Option<u64>,
    #[serde(default)]
    pub thermal_level2_seconds: Option<u64>,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AerSnapshot {
    pub counters: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyClassification {
    pub read_only_allowed: bool,
    pub write_candidate: bool,
    pub protected: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionPlan {
    pub schema_version: u32,
    pub created_unix_ms: u64,
    pub inventory_captured_unix_ms: u64,
    pub devices: Vec<SelectedDevice>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedDevice {
    pub display_index: usize,
    pub stable_id: DeviceId,
    pub device_node: PathBuf,
    pub model: String,
    pub serial: String,
    pub driver: String,
    pub capacity_bytes: u64,
    pub write_candidate: bool,
    pub safety_reasons: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StorageMode {
    ReadOnly,
    BoundedWrite,
    FullDestructive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyDecision {
    pub allowed: bool,
    pub mode: StorageMode,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revalidation {
    pub valid: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Error)]
pub enum InventoryError {
    #[error("cannot read inventory path {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("no device was selected")]
    EmptySelection,
    #[error("device index {0} does not exist in this snapshot")]
    InvalidIndex(usize),
}

/// Scans `NVMe` controllers and namespaces without opening their device nodes.
///
/// # Errors
///
/// Returns [`InventoryError`] if the required sysfs class directories cannot be read.
pub fn scan(options: &ScanOptions) -> Result<Inventory, InventoryError> {
    let nvme_class = options.sys_root.join("class/nvme");
    let block_class = options.sys_root.join("class/block");
    let controllers = directory_names(&nvme_class)?
        .into_iter()
        .filter(|name| is_controller_name(name))
        .collect::<Vec<_>>();
    let block_names = directory_names(&block_class)?;
    let mount_records = read_mount_records(
        &options.proc_root.join("self/mountinfo"),
        &options.sys_root,
        &options.dev_root,
    );
    let swaps = read_swaps(&options.proc_root.join("swaps"));
    let protected = options
        .protected_devices
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();
    let mut warnings = Vec::new();
    if options.collect_smart && !program_available(&options.nvme_program) {
        warnings.push(format!(
            "nvme-cli unavailable at {}; SMART fields are N/A",
            options.nvme_program.display()
        ));
    }

    let mut devices = Vec::new();
    for controller in controllers {
        let controller_path = nvme_class.join(&controller);
        let controller_identity = ControllerIdentity::read(&controller_path, &controller);
        for namespace in block_names
            .iter()
            .filter(|name| is_namespace_for_controller(name, &controller))
        {
            devices.push(scan_namespace(
                options,
                namespace,
                &controller_identity,
                &block_names,
                &mount_records,
                &swaps,
                &protected,
            ));
        }
    }
    devices.sort_by(|left, right| left.device_node.cmp(&right.device_node));
    for (index, device) in devices.iter_mut().enumerate() {
        device.index = index + 1;
    }

    Ok(Inventory {
        schema_version: SCHEMA_VERSION,
        captured_unix_ms: unix_ms(),
        devices,
        warnings,
    })
}

/// Creates a stable-ID selection plan from one-based display indexes.
///
/// # Errors
///
/// Returns [`InventoryError::EmptySelection`] for an empty request, or
/// [`InventoryError::InvalidIndex`] when an alias is absent from the snapshot.
pub fn select_indexes(
    inventory: &Inventory,
    indexes: &[usize],
) -> Result<SelectionPlan, InventoryError> {
    if indexes.is_empty() {
        return Err(InventoryError::EmptySelection);
    }
    let unique = indexes.iter().copied().collect::<BTreeSet<_>>();
    let mut devices = Vec::with_capacity(unique.len());
    for index in unique {
        let device = inventory
            .devices
            .iter()
            .find(|device| device.index == index)
            .ok_or(InventoryError::InvalidIndex(index))?;
        devices.push(SelectedDevice::from(device));
    }
    Ok(SelectionPlan {
        schema_version: SCHEMA_VERSION,
        created_unix_ms: unix_ms(),
        inventory_captured_unix_ms: inventory.captured_unix_ms,
        devices,
    })
}

/// Creates a selection plan containing every discovered namespace.
///
/// # Errors
///
/// Returns [`InventoryError::EmptySelection`] when no `NVMe` namespaces exist.
pub fn select_all(inventory: &Inventory) -> Result<SelectionPlan, InventoryError> {
    let indexes = inventory
        .devices
        .iter()
        .map(|device| device.index)
        .collect::<Vec<_>>();
    select_indexes(inventory, &indexes)
}

/// Verifies that selected stable identities still resolve to the same device nodes.
#[must_use]
pub fn revalidate(selection: &SelectionPlan, current: &Inventory) -> Revalidation {
    let mut errors = Vec::new();
    for selected in &selection.devices {
        match current
            .devices
            .iter()
            .find(|device| device.stable_id == selected.stable_id)
        {
            None => errors.push(format!(
                "selected device disappeared: {}",
                selected.stable_id
            )),
            Some(device) if device.device_node != selected.device_node => errors.push(format!(
                "device node changed for {}: {} -> {}",
                selected.stable_id,
                selected.device_node.display(),
                device.device_node.display()
            )),
            Some(_) => {}
        }
    }
    Revalidation {
        valid: errors.is_empty(),
        errors,
    }
}

/// Validates a storage mode against a current inventory snapshot without performing I/O.
///
/// `FULL_DESTRUCTIVE` requires each selected stable ID to appear in `confirmations` exactly.
/// TASK-C will add range and write-budget validation before any write backend exists.
#[must_use]
pub fn validate_storage_mode(
    selection: &SelectionPlan,
    current: &Inventory,
    mode: StorageMode,
    confirmations: &BTreeSet<DeviceId>,
) -> SafetyDecision {
    let mut errors = revalidate(selection, current).errors;
    let mut warnings = Vec::new();
    for selected in &selection.devices {
        let Some(device) = current
            .devices
            .iter()
            .find(|device| device.stable_id == selected.stable_id)
        else {
            continue;
        };
        if mode != StorageMode::ReadOnly && !device.safety.write_candidate {
            errors.push(format!(
                "{} is not a write candidate: {}",
                device.device_node.display(),
                device.safety.reasons.join("; ")
            ));
        }
        if mode == StorageMode::FullDestructive && !confirmations.contains(&device.stable_id) {
            errors.push(format!(
                "missing exact FULL_DESTRUCTIVE confirmation for {} ({})",
                device.device_node.display(),
                device.stable_id
            ));
        }
    }
    match mode {
        StorageMode::ReadOnly => {}
        StorageMode::BoundedWrite => warnings.push(
            "TASK-C must still validate scratch range and host-write budget before opening a device"
                .to_owned(),
        ),
        StorageMode::FullDestructive => warnings.push(
            "FULL_DESTRUCTIVE erases data; TASK-C backend is not implemented or authorized"
                .to_owned(),
        ),
    }
    SafetyDecision {
        allowed: errors.is_empty(),
        mode,
        errors,
        warnings,
    }
}

impl From<&NvmeDevice> for SelectedDevice {
    fn from(device: &NvmeDevice) -> Self {
        Self {
            display_index: device.index,
            stable_id: device.stable_id.clone(),
            device_node: device.device_node.clone(),
            model: device.model.clone(),
            serial: device.serial.clone(),
            driver: device.driver.clone(),
            capacity_bytes: device.capacity_bytes,
            write_candidate: device.safety.write_candidate,
            safety_reasons: device.safety.reasons.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct ControllerIdentity {
    name: String,
    model: String,
    serial: String,
    firmware: String,
    transport: String,
    driver: String,
    pci_bdf: Option<String>,
    root_port_bdf: Option<String>,
    current_link_speed: Option<String>,
    current_link_width: Option<u32>,
    device_path: PathBuf,
}

impl ControllerIdentity {
    fn read(path: &Path, name: &str) -> Self {
        let device_path = canonical_or(path.join("device"));
        let pci_chain = device_path
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .filter(|component| looks_like_pci_bdf(component))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let pci_bdf = read_trim(path.join("address")).or_else(|| pci_chain.last().cloned());
        let root_port_bdf = if pci_chain.len() >= 2 {
            pci_chain.get(pci_chain.len() - 2).cloned()
        } else {
            None
        };
        let driver = fs::read_link(path.join("device/driver"))
            .ok()
            .and_then(|driver| {
                driver
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "unknown".to_owned());
        Self {
            name: name.to_owned(),
            model: read_trim(path.join("model")).unwrap_or_else(|| "Unknown NVMe".to_owned()),
            serial: read_trim(path.join("serial")).unwrap_or_else(|| "unknown".to_owned()),
            firmware: read_trim(path.join("firmware_rev")).unwrap_or_else(|| "unknown".to_owned()),
            transport: read_trim(path.join("transport")).unwrap_or_else(|| "unknown".to_owned()),
            driver,
            pci_bdf,
            root_port_bdf,
            current_link_speed: read_trim(path.join("device/current_link_speed")),
            current_link_width: read_trim(path.join("device/current_link_width"))
                .and_then(|width| width.parse().ok()),
            device_path,
        }
    }
}

#[derive(Clone, Debug)]
struct MountRecord {
    source: String,
    mount_point: String,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn scan_namespace(
    options: &ScanOptions,
    namespace: &str,
    controller: &ControllerIdentity,
    block_names: &[String],
    mount_records: &[MountRecord],
    swaps: &BTreeSet<String>,
    protected_devices: &BTreeSet<String>,
) -> NvmeDevice {
    let block_path = options.sys_root.join("class/block").join(namespace);
    let device_node = options.dev_root.join(namespace);
    let device_node_text = device_node.to_string_lossy().into_owned();
    let partitions = block_names
        .iter()
        .filter(|name| is_partition_of(name, namespace))
        .cloned()
        .collect::<Vec<_>>();
    let family_nodes = std::iter::once(namespace.to_owned())
        .chain(partitions.iter().cloned())
        .map(|name| options.dev_root.join(name).to_string_lossy().into_owned())
        .collect::<BTreeSet<_>>();
    let mount_points = mount_records
        .iter()
        .filter(|record| family_nodes.contains(&record.source))
        .map(|record| record.mount_point.clone())
        .collect::<Vec<_>>();
    let root_mounted = mount_records
        .iter()
        .any(|record| family_nodes.contains(&record.source) && record.mount_point == "/");
    let swap_used = swaps.iter().any(|swap| family_nodes.contains(swap));
    let holders = std::iter::once(namespace)
        .chain(partitions.iter().map(String::as_str))
        .flat_map(|name| {
            directory_names_lossy(
                &options
                    .sys_root
                    .join("class/block")
                    .join(name)
                    .join("holders"),
            )
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let explicitly_protected = family_nodes
        .iter()
        .any(|node| protected_devices.contains(node));
    let mut reasons = Vec::new();
    if root_mounted {
        reasons.push("contains the running root filesystem".to_owned());
    }
    if !mount_points.is_empty() {
        reasons.push(format!("mounted at {}", mount_points.join(", ")));
    }
    if swap_used {
        reasons.push("contains active swap".to_owned());
    }
    if !holders.is_empty() {
        reasons.push(format!("has block holders: {}", holders.join(", ")));
    }
    if !partitions.is_empty() {
        reasons.push(format!("contains partitions: {}", partitions.join(", ")));
    }
    if explicitly_protected {
        reasons.push("explicitly protected boot/results device".to_owned());
    }
    if controller.serial == "unknown" {
        reasons.push("stable serial is unavailable".to_owned());
    }
    let protected = root_mounted
        || !mount_points.is_empty()
        || swap_used
        || !holders.is_empty()
        || !partitions.is_empty()
        || explicitly_protected;
    let wwid = read_trim(block_path.join("wwid"));
    let nguid =
        read_trim(block_path.join("nguid")).or_else(|| read_trim(block_path.join("device/nguid")));
    let eui =
        read_trim(block_path.join("eui")).or_else(|| read_trim(block_path.join("device/eui")));
    let namespace_id = read_trim(block_path.join("nsid"))
        .or_else(|| read_trim(block_path.join("device/nsid")))
        .and_then(|nsid| nsid.parse().ok());
    let stable_id = build_stable_id(
        controller,
        namespace_id,
        wwid.as_deref(),
        nguid.as_deref(),
        eui.as_deref(),
    );
    let logical_block_size = read_u32(block_path.join("queue/logical_block_size")).unwrap_or(0);
    let physical_block_size = read_u32(block_path.join("queue/physical_block_size")).unwrap_or(0);
    let partition_details = partitions
        .iter()
        .map(|partition| {
            scan_partition(
                options,
                partition,
                &stable_id,
                mount_records,
                swaps,
                protected_devices,
                logical_block_size,
                physical_block_size,
            )
        })
        .collect();
    let capacity_bytes = read_u64(block_path.join("size"))
        .unwrap_or(0)
        .saturating_mul(512);
    let aer = read_aer(&controller.device_path);
    let smart = if options.collect_smart {
        collect_smart(&options.nvme_program, &device_node)
    } else {
        SmartSnapshot {
            warning: Some("SMART collection disabled".to_owned()),
            ..SmartSnapshot::default()
        }
    };

    NvmeDevice {
        index: 0,
        stable_id: DeviceId(stable_id),
        device_node: PathBuf::from(device_node_text),
        namespace: namespace.to_owned(),
        namespace_id,
        controller: controller.name.clone(),
        driver: controller.driver.clone(),
        transport: controller.transport.clone(),
        model: controller.model.clone(),
        serial: controller.serial.clone(),
        firmware: controller.firmware.clone(),
        capacity_bytes,
        logical_block_size,
        physical_block_size,
        wwid,
        nguid,
        eui,
        pci_bdf: controller.pci_bdf.clone(),
        root_port_bdf: controller.root_port_bdf.clone(),
        current_link_speed: controller.current_link_speed.clone(),
        current_link_width: controller.current_link_width,
        partitions,
        partition_details,
        mount_points,
        holders,
        smart,
        aer,
        safety: SafetyClassification {
            read_only_allowed: true,
            write_candidate: !protected && controller.serial != "unknown",
            protected,
            reasons,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_partition(
    options: &ScanOptions,
    name: &str,
    namespace_stable_id: &str,
    mount_records: &[MountRecord],
    swaps: &BTreeSet<String>,
    protected_devices: &BTreeSet<String>,
    parent_logical_block_size: u32,
    parent_physical_block_size: u32,
) -> PartitionInfo {
    let block_path = options.sys_root.join("class/block").join(name);
    let device_node = options.dev_root.join(name);
    let node = device_node.to_string_lossy().into_owned();
    let mount_points = mount_records
        .iter()
        .filter(|record| record.source == node)
        .map(|record| record.mount_point.clone())
        .collect::<Vec<_>>();
    let holders = directory_names_lossy(&block_path.join("holders"));
    let active_swap = swaps.contains(&node);
    let explicitly_protected = protected_devices.contains(&node);
    let mut reasons = Vec::new();
    if !mount_points.is_empty() {
        reasons.push(format!("mounted at {}", mount_points.join(", ")));
    }
    if active_swap {
        reasons.push("active swap".to_owned());
    }
    if !holders.is_empty() {
        reasons.push(format!("has block holders: {}", holders.join(", ")));
    }
    if explicitly_protected {
        reasons.push("explicitly protected boot/results device".to_owned());
    }
    let capacity_bytes = read_u64(block_path.join("size"))
        .unwrap_or(0)
        .saturating_mul(512);
    let start_bytes = read_u64(block_path.join("start"))
        .unwrap_or(0)
        .saturating_mul(512);
    let logical_block_size =
        read_u32(block_path.join("queue/logical_block_size")).unwrap_or(parent_logical_block_size);
    let physical_block_size = read_u32(block_path.join("queue/physical_block_size"))
        .unwrap_or(parent_physical_block_size);
    if capacity_bytes == 0 || logical_block_size == 0 {
        reasons.push("invalid or unavailable block geometry".to_owned());
    }
    let protected =
        !mount_points.is_empty() || active_swap || !holders.is_empty() || explicitly_protected;
    PartitionInfo {
        name: name.to_owned(),
        stable_id: DeviceId(format!(
            "{namespace_stable_id}:part:{}:{start_bytes}:{capacity_bytes}",
            normalized_id_part(name)
        )),
        device_node,
        start_bytes,
        capacity_bytes,
        logical_block_size,
        physical_block_size,
        mount_points,
        holders,
        active_swap,
        protected,
        write_candidate: !protected && capacity_bytes > 0 && logical_block_size > 0,
        reasons,
    }
}

fn build_stable_id(
    controller: &ControllerIdentity,
    namespace_id: Option<u32>,
    wwid: Option<&str>,
    nguid: Option<&str>,
    eui: Option<&str>,
) -> String {
    let namespace_identity = wwid
        .or(nguid)
        .or(eui)
        .map(str::to_owned)
        .or_else(|| namespace_id.map(|id| format!("nsid-{id}")))
        .unwrap_or_else(|| "namespace-unknown".to_owned());
    format!(
        "nvme:{}:{}:{}",
        normalized_id_part(&controller.serial),
        normalized_id_part(&namespace_identity),
        controller.pci_bdf.as_deref().unwrap_or("pci-unknown")
    )
}

/// Thermal management entries (levels 1 and 2) and the seconds spent there between two SMART
/// readings: the drive's own throttling. `None` when the drive does not report it.
#[must_use]
pub fn thermal_throttle_between(
    start: &SmartSnapshot,
    now: &SmartSnapshot,
) -> Option<(u64, Option<u64>)> {
    now.thermal_level1_transitions?;
    let grew = |field: fn(&SmartSnapshot) -> Option<u64>| {
        field(now)
            .unwrap_or(0)
            .saturating_sub(field(start).unwrap_or(0))
    };
    Some((
        grew(|s| s.thermal_level1_transitions) + grew(|s| s.thermal_level2_transitions),
        Some(grew(|s| s.thermal_level1_seconds) + grew(|s| s.thermal_level2_seconds)),
    ))
}

/// One SMART reading of `device` with `nvme smart-log`, for a live screen.
#[must_use]
pub fn read_smart(device: &Path) -> SmartSnapshot {
    collect_smart(Path::new("nvme"), device)
}

/// Every `nvme smart-log` of this process: an SSD that stopped answering holds one call, not the
/// caller (`docs/FAULT_CATCH_ROADMAP.md`, step FC0).
pub static NVME_SMART: dit_contracts::bounded::StuckGate = dit_contracts::bounded::StuckGate::new();
/// How long one `nvme smart-log` may take (an admin command answers in well under a second).
const SMART_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

fn collect_smart(program: &Path, device: &Path) -> SmartSnapshot {
    let mut command = Command::new(program);
    command.args(["smart-log", &device.to_string_lossy(), "-o", "json"]);
    let output = match dit_contracts::bounded::output_within(&mut command, SMART_LIMIT, &NVME_SMART)
    {
        Some(output) if output.status.success() => output,
        Some(output) => {
            return SmartSnapshot {
                warning: Some(format!("nvme smart-log failed ({})", output.status)),
                ..SmartSnapshot::default()
            };
        }
        None => {
            return SmartSnapshot {
                warning: Some(NVME_SMART.silent_seconds().map_or_else(
                    || "nvme smart-log unavailable".to_owned(),
                    |silent| format!("nvme smart-log silent {silent} s (SSD not answering)"),
                )),
                ..SmartSnapshot::default()
            };
        }
    };
    match serde_json::from_slice::<Value>(&output.stdout) {
        Ok(value) => smart_from_json(&value),
        Err(error) => SmartSnapshot {
            warning: Some(format!("invalid nvme smart-log JSON: {error}")),
            ..SmartSnapshot::default()
        },
    }
}

fn smart_from_json(value: &Value) -> SmartSnapshot {
    SmartSnapshot {
        available: true,
        temperature_celsius: json_i64(value, "temperature"),
        available_spare_percent: json_u64(value, "avail_spare"),
        percentage_used: json_u64(value, "percent_used"),
        data_units_read: json_u64(value, "data_units_read"),
        data_units_written: json_u64(value, "data_units_written"),
        controller_busy_time_minutes: json_u64(value, "controller_busy_time"),
        media_and_data_integrity_errors: json_u64(value, "media_errors"),
        error_log_entries: json_u64(value, "num_err_log_entries"),
        warning_temperature_minutes: json_u64(value, "warning_temp_time"),
        critical_temperature_minutes: json_u64(value, "critical_comp_time"),
        thermal_level1_transitions: json_u64(value, "thm_temp1_trans_count"),
        thermal_level2_transitions: json_u64(value, "thm_temp2_trans_count"),
        thermal_level1_seconds: json_u64(value, "thm_temp1_total_time"),
        thermal_level2_seconds: json_u64(value, "thm_temp2_total_time"),
        warning: None,
    }
}

fn json_u64(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|entry| {
        entry.as_u64().or_else(|| {
            entry
                .as_str()
                .map(|text| text.replace([',', ' '], ""))
                .and_then(|text| text.parse().ok())
        })
    })
}

fn json_i64(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|entry| {
        entry.as_i64().or_else(|| {
            entry
                .as_str()
                .map(|text| text.replace([',', ' '], ""))
                .and_then(|text| text.parse().ok())
        })
    })
}

fn read_aer(device_path: &Path) -> AerSnapshot {
    let mut counters = BTreeMap::new();
    for name in ["aer_dev_correctable", "aer_dev_nonfatal", "aer_dev_fatal"] {
        if let Ok(value) = fs::read_to_string(device_path.join(name)) {
            counters.insert(name.to_owned(), value.trim().to_owned());
        }
    }
    AerSnapshot { counters }
}

fn read_mount_records(path: &Path, sys_root: &Path, dev_root: &Path) -> Vec<MountRecord> {
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let separator = fields.iter().position(|field| *field == "-")?;
            let mount_point = fields.get(4)?;
            let reported_source = fields.get(separator + 2)?;
            let source = fields
                .get(2)
                .and_then(|major_minor| {
                    fs::canonicalize(sys_root.join("dev/block").join(major_minor)).ok()
                })
                .and_then(|resolved| {
                    resolved
                        .file_name()
                        .map(|name| dev_root.join(name).to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| unescape_mount_field(reported_source));
            Some(MountRecord {
                source,
                mount_point: unescape_mount_field(mount_point),
            })
        })
        .collect()
}

fn read_swaps(path: &Path) -> BTreeSet<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

fn unescape_mount_field(field: &str) -> String {
    field
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn directory_names(path: &Path) -> Result<Vec<String>, InventoryError> {
    Ok(fs::read_dir(path)
        .map_err(|source| InventoryError::Read {
            path: path.to_path_buf(),
            source,
        })?
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect())
}

fn directory_names_lossy(path: &Path) -> Vec<String> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect()
}

fn is_controller_name(name: &str) -> bool {
    name.strip_prefix("nvme").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit())
    })
}

fn is_namespace_for_controller(name: &str, controller: &str) -> bool {
    name.strip_prefix(controller)
        .and_then(|suffix| suffix.strip_prefix('n'))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit())
        })
}

fn is_partition_of(name: &str, namespace: &str) -> bool {
    name.strip_prefix(namespace)
        .and_then(|suffix| suffix.strip_prefix('p'))
        .is_some_and(|suffix| {
            !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit())
        })
}

fn looks_like_pci_bdf(component: &str) -> bool {
    let bytes = component.as_bytes();
    bytes.len() == 12
        && bytes[4] == b':'
        && bytes[7] == b':'
        && bytes[10] == b'.'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7 | 10) || byte.is_ascii_hexdigit())
}

fn canonical_or(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trim(path).and_then(|value| value.parse().ok())
}

fn read_u32(path: impl AsRef<Path>) -> Option<u32> {
    read_trim(path).and_then(|value| value.parse().ok())
}

fn normalized_id_part(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

fn program_available(program: &Path) -> bool {
    if program.components().count() > 1 {
        return program.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
    })
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
    use std::os::unix::fs::symlink;

    #[test]
    fn controller_and_namespace_names_are_strict() {
        assert!(is_controller_name("nvme0"));
        assert!(!is_controller_name("nvme0n1"));
        assert!(is_namespace_for_controller("nvme12n3", "nvme12"));
        assert!(!is_namespace_for_controller("nvme12n3p1", "nvme12"));
        assert!(is_partition_of("nvme0n1p2", "nvme0n1"));
    }

    #[test]
    fn smart_json_accepts_numbers_and_formatted_strings() {
        let value = serde_json::json!({
            "temperature": 51,
            "avail_spare": 100,
            "percent_used": "2",
            "data_units_read": "1,234",
            "data_units_written": 567,
            "controller_busy_time": 9,
            "media_errors": 0,
            "num_err_log_entries": "4",
            "warning_temp_time": 3,
            "thm_temp1_trans_count": 2,
            "thm_temp1_total_time": 40
        });
        let smart = smart_from_json(&value);
        assert!(smart.available);
        assert_eq!(smart.temperature_celsius, Some(51));
        assert_eq!(smart.data_units_read, Some(1234));
        assert_eq!(smart.error_log_entries, Some(4));
        assert_eq!(smart.warning_temperature_minutes, Some(3));
        assert_eq!(smart.thermal_level1_transitions, Some(2));
        assert_eq!(smart.thermal_level1_seconds, Some(40));
        assert_eq!(smart.thermal_level2_transitions, None);
    }

    /// The drive's throttling is the growth of its thermal management counters; a drive that
    /// does not report them reads `None`.
    #[test]
    fn thermal_throttle_is_the_growth_of_the_thermal_management_counters() {
        let smart = |transitions: Option<u64>, seconds: u64| SmartSnapshot {
            thermal_level1_transitions: transitions,
            thermal_level2_transitions: Some(1),
            thermal_level1_seconds: Some(seconds),
            thermal_level2_seconds: Some(5),
            ..SmartSnapshot::default()
        };
        assert_eq!(
            thermal_throttle_between(&smart(Some(4), 100), &smart(Some(7), 160)),
            Some((3, Some(60)))
        );
        assert_eq!(
            thermal_throttle_between(&smart(None, 0), &smart(None, 0)),
            None
        );
    }

    #[test]
    fn selection_is_deduplicated_and_revalidated_by_stable_id() {
        let first = sample_device(1, "id-a", "/dev/nvme0n1");
        let second = sample_device(2, "id-b", "/dev/nvme1n1");
        let inventory = Inventory {
            schema_version: 1,
            captured_unix_ms: 10,
            devices: vec![first.clone(), second.clone()],
            warnings: Vec::new(),
        };
        let selection = select_indexes(&inventory, &[2, 1, 2]).unwrap();
        assert_eq!(selection.devices.len(), 2);
        assert!(revalidate(&selection, &inventory).valid);

        let reordered = Inventory {
            devices: vec![
                sample_device(1, "id-b", "/dev/nvme1n1"),
                sample_device(2, "id-a", "/dev/nvme0n1"),
            ],
            ..inventory.clone()
        };
        assert!(revalidate(&selection, &reordered).valid);

        let changed_node = Inventory {
            devices: vec![sample_device(1, "id-a", "/dev/nvme9n1"), second],
            ..inventory
        };
        assert!(!revalidate(&selection, &changed_node).valid);
    }

    #[test]
    fn scan_marks_root_partition_swap_and_holder_as_protected() {
        let fixture = Fixture::new("protected");
        fixture.add_controller("nvme0", "Fixture Fast 1TB", "SER-001");
        fixture.add_namespace("nvme0n1", true);
        fixture.write("sys/class/block/nvme0n1p2/holders/dm-0/.keep", "");
        let root_source = fixture.root.join("dev/nvme0n1p2");
        fixture.write(
            "proc/self/mountinfo",
            &format!("33 2 259:2 / / rw - ext4 {} rw\n", root_source.display()),
        );
        fixture.write(
            "proc/swaps",
            &format!(
                "Filename Type Size Used Priority\n{} partition 1 0 -2\n",
                root_source.display()
            ),
        );

        let inventory = scan(&fixture.options()).unwrap();
        assert_eq!(inventory.devices.len(), 1);
        let device = &inventory.devices[0];
        assert_eq!(device.model, "Fixture Fast 1TB");
        assert_eq!(device.driver, "nvme");
        assert_eq!(device.namespace_id, Some(1));
        assert!(device.safety.protected);
        assert!(!device.safety.write_candidate);
        assert!(device.mount_points.contains(&"/".to_owned()));
        assert!(device.holders.contains(&"dm-0".to_owned()));
        assert!(
            device
                .safety
                .reasons
                .iter()
                .any(|reason| reason.contains("root filesystem"))
        );
        assert!(
            device
                .safety
                .reasons
                .iter()
                .any(|reason| reason.contains("active swap"))
        );
    }

    #[test]
    fn clean_raw_namespace_is_only_a_write_candidate() {
        let fixture = Fixture::new("candidate");
        fixture.add_controller("nvme1", "Fixture Empty 2TB", "SER-002");
        fixture.add_namespace("nvme1n1", false);
        fixture.write("proc/self/mountinfo", "");
        fixture.write("proc/swaps", "Filename Type Size Used Priority\n");
        let inventory = scan(&fixture.options()).unwrap();
        let device = &inventory.devices[0];
        assert!(device.safety.read_only_allowed);
        assert!(device.safety.write_candidate);
        assert!(!device.safety.protected);

        let mut protected_options = fixture.options();
        protected_options
            .protected_devices
            .push(fixture.root.join("dev/nvme1n1"));
        let protected = scan(&protected_options).unwrap();
        assert!(!protected.devices[0].safety.write_candidate);
    }

    #[test]
    fn mode_validation_allows_read_only_but_guards_writes() {
        let protected = sample_device(1, "id-protected", "/dev/nvme0n1");
        let mut protected = protected;
        protected.safety.write_candidate = false;
        protected.safety.protected = true;
        protected.safety.reasons = vec!["running root filesystem".into()];
        let candidate = sample_device(2, "id-candidate", "/dev/nvme1n1");
        let inventory = Inventory {
            schema_version: 1,
            captured_unix_ms: 10,
            devices: vec![protected, candidate],
            warnings: Vec::new(),
        };
        let root_selection = select_indexes(&inventory, &[1]).unwrap();
        let no_confirmations = BTreeSet::new();
        assert!(
            validate_storage_mode(
                &root_selection,
                &inventory,
                StorageMode::ReadOnly,
                &no_confirmations
            )
            .allowed
        );
        assert!(
            !validate_storage_mode(
                &root_selection,
                &inventory,
                StorageMode::BoundedWrite,
                &no_confirmations
            )
            .allowed
        );

        let candidate_selection = select_indexes(&inventory, &[2]).unwrap();
        assert!(
            !validate_storage_mode(
                &candidate_selection,
                &inventory,
                StorageMode::FullDestructive,
                &no_confirmations
            )
            .allowed
        );
        let confirmations = BTreeSet::from([DeviceId("id-candidate".into())]);
        assert!(
            validate_storage_mode(
                &candidate_selection,
                &inventory,
                StorageMode::FullDestructive,
                &confirmations
            )
            .allowed
        );
    }

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "cpu-nvme-inventory-{label}-{}-{}",
                std::process::id(),
                unix_ms()
            ));
            fs::create_dir_all(root.join("sys/class/nvme")).unwrap();
            fs::create_dir_all(root.join("sys/class/block")).unwrap();
            fs::create_dir_all(root.join("proc/self")).unwrap();
            fs::create_dir_all(root.join("dev")).unwrap();
            Self { root }
        }

        fn options(&self) -> ScanOptions {
            ScanOptions {
                sys_root: self.root.join("sys"),
                proc_root: self.root.join("proc"),
                dev_root: self.root.join("dev"),
                collect_smart: false,
                nvme_program: PathBuf::from("nvme-not-used"),
                protected_devices: Vec::new(),
            }
        }

        fn add_controller(&self, name: &str, model: &str, serial: &str) {
            let base = format!("sys/class/nvme/{name}");
            self.write(&format!("{base}/model"), model);
            self.write(&format!("{base}/serial"), serial);
            self.write(&format!("{base}/firmware_rev"), "FW1");
            self.write(&format!("{base}/transport"), "pcie");
            self.write(&format!("{base}/address"), "0000:01:00.0");
            self.write(
                &format!("{base}/device/current_link_speed"),
                "16.0 GT/s PCIe",
            );
            self.write(&format!("{base}/device/current_link_width"), "4");
            let driver = self.root.join("sys/bus/pci/drivers/nvme");
            fs::create_dir_all(&driver).unwrap();
            symlink(driver, self.root.join(format!("{base}/device/driver"))).unwrap();
        }

        fn add_namespace(&self, name: &str, with_partition: bool) {
            let base = format!("sys/class/block/{name}");
            self.write(&format!("{base}/size"), "2000000");
            self.write(&format!("{base}/queue/logical_block_size"), "512");
            self.write(&format!("{base}/queue/physical_block_size"), "4096");
            self.write(&format!("{base}/wwid"), "eui.fixture001");
            self.write(&format!("{base}/nsid"), "1");
            fs::create_dir_all(self.root.join(format!("{base}/holders"))).unwrap();
            if with_partition {
                fs::create_dir_all(self.root.join(format!("sys/class/block/{name}p2/holders")))
                    .unwrap();
            }
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    fn sample_device(index: usize, stable_id: &str, device_node: &str) -> NvmeDevice {
        NvmeDevice {
            index,
            stable_id: DeviceId(stable_id.into()),
            device_node: PathBuf::from(device_node),
            namespace: Path::new(device_node)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            namespace_id: Some(1),
            controller: "nvme0".into(),
            driver: "nvme".into(),
            transport: "pcie".into(),
            model: "Fixture NVMe".into(),
            serial: "SERIAL".into(),
            firmware: "1.0".into(),
            capacity_bytes: 1_000_000,
            logical_block_size: 512,
            physical_block_size: 4096,
            wwid: Some(stable_id.into()),
            nguid: None,
            eui: None,
            pci_bdf: Some("0000:01:00.0".into()),
            root_port_bdf: Some("0000:00:01.0".into()),
            current_link_speed: Some("16.0 GT/s PCIe".into()),
            current_link_width: Some(4),
            partitions: Vec::new(),
            partition_details: Vec::new(),
            mount_points: Vec::new(),
            holders: Vec::new(),
            smart: SmartSnapshot::default(),
            aer: AerSnapshot::default(),
            safety: SafetyClassification {
                read_only_allowed: true,
                write_candidate: true,
                protected: false,
                reasons: Vec::new(),
            },
        }
    }
}
