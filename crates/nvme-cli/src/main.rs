mod auto_storage;
mod combined_storage;
mod format_disk;

use clap::{Parser, Subcommand, ValueEnum};
use dit_contracts::{DeviceId, DeviceReport, Event, EventEnvelope, Phase, RunId, Verdict};
use dit_source::{BlockContext, build_block};
use dit_system::{LiveSensorSampler, TelemetrySnapshot, collect_gpu_inventory, format_gpu_summary};
use dit_ui_core::{
    DeviceDescriptor, HostSampler, InPlaceRenderer, LiveUtilization, PickerEntry, TerminalSize,
    UiState, render_picker,
};
use nvme_backend_file::{DirectFileBackend, FileBackend, MemoryBackend};
use nvme_backend_linux::{BoundedRequest, BoundedWritePlan, open_bounded, prepare_bounded_plan};
use nvme_engine::{CoveragePlan, FaultKind, FaultPlan, RunConfig, run};
use nvme_inventory::{
    Inventory, NvmeDevice, ScanOptions, SelectionPlan, revalidate, scan, select_all, select_indexes,
};
use nvme_io::StorageBackend;
use nvme_scheduler::{DeviceJob, MultiDeviceReport, run_multi_with_limit};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, IsTerminal, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Parser)]
#[command(
    name = "cpu-nvme-dit",
    version,
    about = "DIT CPU-SSD (NVMe) Deep test and read-only inventory"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, Default)]
struct CpuSensorMetrics {
    temperature_celsius: Option<f64>,
    power_watts: Option<f64>,
}

struct CpuNvmeHostSampler {
    host: HostSampler,
    sensors: LiveSensorSampler,
    cached: CpuSensorMetrics,
    last_sensor_sample: Instant,
}

impl CpuNvmeHostSampler {
    fn detect() -> Self {
        let sensors = LiveSensorSampler::default();
        let cached = cpu_sensor_metrics(&sensors.sample());
        Self {
            host: HostSampler::detect(),
            sensors,
            cached,
            last_sensor_sample: Instant::now(),
        }
    }

    fn system_info(&self) -> dit_ui_core::SystemInfo {
        let mut system = self.host.system_info();
        system.gpu = format_gpu_summary(&collect_gpu_inventory());
        system
    }

    fn sample(&mut self) -> LiveUtilization {
        if self.last_sensor_sample.elapsed() >= Duration::from_secs(1) {
            self.cached = cpu_sensor_metrics(&self.sensors.sample());
            self.last_sensor_sample = Instant::now();
        }
        let mut live = self.host.sample();
        live.cpu_temperature_celsius = self.cached.temperature_celsius;
        live.cpu_power_watts = self.cached.power_watts;
        live
    }
}

#[allow(clippy::cast_precision_loss)]
fn cpu_sensor_metrics(snapshot: &TelemetrySnapshot) -> CpuSensorMetrics {
    CpuSensorMetrics {
        temperature_celsius: snapshot
            .max_temperature_millicelsius()
            .map(|value| value as f64 / 1000.0),
        power_watts: snapshot
            .preferred_cpu_power()
            .map(|reading| reading.milliwatts as f64 / 1000.0),
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List `NVMe` device/model/driver/topology and read-only safety status.
    Inventory(InventoryArgs),
    /// Select one, many or all `NVMe` devices and produce a stable-ID plan.
    Select(SelectArgs),
    /// Re-scan and verify that a saved selection still maps to the same device nodes.
    Revalidate(RevalidateArgs),
    /// Create a safety-checked, non-executing bounded-write plan.
    PrepareBounded(PrepareBoundedArgs),
    /// Execute an authorized bounded-write plan on exactly one block target.
    RunBounded(RunBoundedArgs),
    /// Test one selected `NVMe` through a new file without overwriting existing data.
    RunPreserving(RunPreservingArgs),
    /// Run the concurrent multi-device scheduler and dynamic UI against memory backends.
    MultiSimulate(MultiSimulateArgs),
    /// Test all selected `NVMe` filesystems concurrently without overwriting existing data.
    RunPreservingMulti(RunPreservingMultiArgs),
    /// Interactive, data-preserving CPU-SSD Deep UI for one or more selected `NVMe` devices.
    Interactive(InteractiveArgs),
    /// Preview automatic file-test volume choices. Read-only: no mounts or files created.
    AutoPlan(AutoPlanArgs),
    /// Combined run: pick SSDs, prepare their test directories (with room for the data chain)
    /// and hold them until SIGTERM, publishing the plan for the combined scheduler.
    CombinedStorage(combined_storage::CombinedStorageArgs),
    /// FORMAT SSD screen of the control center: quick-format a whole `NVMe` disk (erases it).
    FormatDisk(FormatDiskArgs),
    /// Run the integrity engine against a regular simulation file only.
    Simulate(SimulateArgs),
    /// Run the positive and full negative campaign in memory.
    Campaign(CampaignArgs),
    /// Print the canonical TASK-A golden-vector digest.
    Golden,
}

#[derive(Debug, clap::Args)]
struct InventoryFlags {
    /// Skip the optional nvme-cli SMART query.
    #[arg(long)]
    no_smart: bool,
    /// Mark an extra device node as boot/results protected. Repeat as needed.
    #[arg(long = "protected-device")]
    protected_devices: Vec<PathBuf>,
}

#[derive(Debug, clap::Args)]
struct FormatDiskArgs {
    #[command(flatten)]
    flags: InventoryFlags,
}

#[derive(Debug, clap::Args)]
struct InventoryArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// Emit the full inventory as JSON instead of the operator table.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct AutoPlanArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    #[arg(long, default_value = "all")]
    devices: String,
    /// Minimum volume/free capacity including test bytes and reserve.
    #[arg(long, default_value_t = 6144)]
    required_free_mib: u64,
}

#[derive(Debug, clap::Args)]
struct SelectArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// Comma-separated one-based aliases (for example 1,2) or "all".
    #[arg(long)]
    devices: Option<String>,
    /// Write the stable-ID selection plan to this JSON file.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Print only JSON; requires --devices to avoid an interactive prompt.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Args)]
struct RevalidateArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// Selection plan previously produced by the select command.
    #[arg(long)]
    plan: PathBuf,
}

#[derive(Debug, clap::Args)]
struct PrepareBoundedArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// Selection plan created by the select command.
    #[arg(long)]
    selection: PathBuf,
    /// One-based namespace alias stored in the selection plan.
    #[arg(long)]
    device: usize,
    /// Exact namespace or scratch-partition device node.
    #[arg(long)]
    target: PathBuf,
    #[arg(long, default_value_t = 0)]
    range_start_mib: u64,
    #[arg(long)]
    range_size_mib: u64,
    #[arg(long)]
    write_budget_mib: u64,
    #[arg(long, default_value_t = 60)]
    duration_seconds: u64,
    /// Maximum operations; defaults to 128 per requested second and is trimmed to write budget.
    #[arg(long)]
    operations: Option<u64>,
    #[arg(long, default_value_t = 0x510e_527f_ade6_82d1)]
    seed: u64,
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, clap::Args)]
struct RunBoundedArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    #[arg(long)]
    plan: PathBuf,
    /// Exact target ID printed by prepare-bounded.
    #[arg(long)]
    confirm_target_id: String,
    /// Required acknowledgement that every byte in the bounded range may be overwritten.
    #[arg(long)]
    acknowledge_data_loss: bool,
    /// Optional CPU-RAM evidence for diagnostic attribution; not required to run CPU-SSD.
    #[arg(long)]
    cpu_ram_preflight: Option<PathBuf>,
    #[arg(long)]
    events: Option<PathBuf>,
    #[arg(long)]
    report: PathBuf,
}

#[derive(Debug, clap::Args)]
struct RunPreservingArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// Selection plan created by the select command.
    #[arg(long)]
    selection: PathBuf,
    /// One-based namespace alias stored in the selection plan.
    #[arg(long)]
    device: usize,
    /// Existing directory on a directly-mounted filesystem of the selected `NVMe`.
    #[arg(long)]
    directory: PathBuf,
    /// Exact namespace stable ID printed by the selection command.
    #[arg(long)]
    confirm_device_id: String,
    /// Logical size of the new test file. Existing paths are never reused.
    #[arg(long)]
    capacity_mib: u64,
    /// Maximum bytes submitted as writes during this run.
    #[arg(long)]
    write_budget_mib: u64,
    /// Free space that must remain untouched on the filesystem.
    #[arg(long, default_value_t = 2048)]
    reserve_free_mib: u64,
    #[arg(long, default_value_t = 60)]
    duration_seconds: u64,
    #[arg(long)]
    operations: Option<u64>,
    #[arg(long, default_value_t = 0xbb67_ae85_84ca_a73b)]
    seed: u64,
    /// Optional CPU-RAM evidence for diagnostic attribution; not required to run CPU-SSD.
    #[arg(long)]
    cpu_ram_preflight: Option<PathBuf>,
    #[arg(long)]
    events: Option<PathBuf>,
    #[arg(long)]
    report: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
struct CpuRamPreflight {
    schema_version: u32,
    module_id: String,
    run_id: String,
    verdict: Verdict,
}

#[derive(Clone, Debug, Serialize)]
struct BoundedRunReport {
    schema_version: u32,
    plan: BoundedWritePlan,
    attribution: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_ram_preflight: Option<CpuRamPreflightEvidence>,
    inventory_before: nvme_inventory::NvmeDevice,
    inventory_after: Option<nvme_inventory::NvmeDevice>,
    engine: DeviceReport,
}

#[derive(Clone, Debug, Serialize)]
struct PreservingRunReport {
    schema_version: u32,
    mode: &'static str,
    attribution: &'static str,
    namespace_stable_id: DeviceId,
    filesystem_target_id: DeviceId,
    filesystem_device_node: PathBuf,
    directory: PathBuf,
    test_file_was_new: bool,
    test_file_removed: bool,
    available_bytes_before: u64,
    reserved_free_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_ram_preflight: Option<CpuRamPreflightEvidence>,
    inventory_before: NvmeDevice,
    inventory_after: Option<NvmeDevice>,
    engine: DeviceReport,
}

#[derive(Clone, Debug, Serialize)]
struct PreservingTargetEvidence {
    alias: usize,
    namespace_stable_id: DeviceId,
    filesystem_target_id: DeviceId,
    filesystem_device_node: PathBuf,
    directory: PathBuf,
    available_bytes_before: u64,
    reserved_free_bytes: u64,
    test_file_was_new: bool,
    test_file_removed: bool,
}

#[derive(Clone, Debug, Serialize)]
struct MultiPreservingRunReport {
    schema_version: u32,
    mode: &'static str,
    attribution: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_ram_preflight: Option<CpuRamPreflightEvidence>,
    targets: Vec<PreservingTargetEvidence>,
    inventory_before: Vec<NvmeDevice>,
    inventory_after: Vec<NvmeDevice>,
    scheduler: MultiDeviceReport,
}

#[derive(Clone, Debug, Serialize)]
struct CpuRamPreflightEvidence {
    path: PathBuf,
    schema_version: u32,
    module_id: String,
    run_id: String,
    verdict: Verdict,
    sha256: String,
}

#[derive(Debug, clap::Args)]
struct SimulateArgs {
    /// Planning horizon; changes seed and transfer-size coverage.
    #[arg(long, default_value_t = 60)]
    duration_seconds: u64,
    /// Operation budget. Default is four operations per planning second.
    #[arg(long)]
    operations: Option<u64>,
    #[arg(long, default_value_t = 64)]
    capacity_mib: u64,
    #[arg(long, default_value_t = 0x6a09_e667_f3bc_c909)]
    seed: u64,
    /// Optional regular file. A unique /tmp file is used when omitted.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Keep an automatically-created temporary simulation file.
    #[arg(long)]
    keep_file: bool,
    #[arg(long)]
    events: Option<PathBuf>,
    #[arg(long)]
    report: Option<PathBuf>,
    #[arg(long, value_enum)]
    inject: Option<FaultArg>,
    #[arg(long, default_value_t = 3)]
    inject_at: u64,
}

#[derive(Debug, clap::Args)]
struct MultiSimulateArgs {
    /// Number of independent simulated `NVMe` devices.
    #[arg(long, default_value_t = 2)]
    devices: usize,
    #[arg(long, default_value_t = 60)]
    duration_seconds: u64,
    /// Operation budget per device. Default is 128 operations per planning second.
    #[arg(long)]
    operations: Option<u64>,
    #[arg(long, default_value_t = 64)]
    capacity_mib: u64,
    /// Hard aggregate memory cap for all simulated device buffers.
    #[arg(long, default_value_t = 512)]
    memory_budget_mib: u64,
    /// Maximum simultaneous device workers; zero selects the host parallelism limit.
    #[arg(long, default_value_t = 0)]
    max_workers: usize,
    #[arg(long, default_value_t = 0x3c6e_f372_fe94_f82b)]
    seed: u64,
    /// Optional one-based device alias that receives an injected fault.
    #[arg(long)]
    inject_device: Option<usize>,
    #[arg(long, value_enum)]
    inject: Option<FaultArg>,
    #[arg(long, default_value_t = 3)]
    inject_at: u64,
    /// Disable ANSI live UI even when stdout is a terminal.
    #[arg(long)]
    no_ui: bool,
    #[arg(long, default_value_t = 4)]
    ui_fps: u32,
    #[arg(long)]
    events: Option<PathBuf>,
    #[arg(long)]
    report: PathBuf,
    /// Check a failing block again and re-read it at once, and record which of the verifying
    /// CPU, the read path or the stored data was wrong (component fault ledger, step F4).
    #[arg(long)]
    confirm_on_fail: bool,
}

#[derive(Debug, clap::Args)]
struct RunPreservingMultiArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// Selection plan containing exactly the devices to run.
    #[arg(long)]
    selection: PathBuf,
    /// Device-directory mapping, repeated as `--target ALIAS=/mounted/directory`.
    #[arg(long = "target", required = true)]
    targets: Vec<String>,
    /// Exact namespace stable ID, repeated once for every selected device.
    #[arg(long = "confirm-device-id", required = true)]
    confirm_device_ids: Vec<String>,
    #[arg(long)]
    capacity_mib: u64,
    #[arg(long)]
    write_budget_mib: u64,
    #[arg(long, default_value_t = 2048)]
    reserve_free_mib: u64,
    #[arg(long, default_value_t = 60)]
    duration_seconds: u64,
    #[arg(long)]
    operations: Option<u64>,
    #[arg(long, default_value_t = 0xa54f_f53a_5f1d_36f1)]
    seed: u64,
    /// Maximum simultaneous device workers; zero selects host parallelism.
    #[arg(long, default_value_t = 0)]
    max_workers: usize,
    #[arg(long)]
    cpu_ram_preflight: Option<PathBuf>,
    #[arg(long)]
    no_ui: bool,
    #[arg(long, default_value_t = 4)]
    ui_fps: u32,
    #[arg(long)]
    events: Option<PathBuf>,
    #[arg(long)]
    report: PathBuf,
    /// Check a failing block again and re-read it at once, and record which of the verifying
    /// CPU, the read path or the stored data was wrong (component fault ledger, step F4).
    #[arg(long)]
    confirm_on_fail: bool,
}

#[derive(Debug, clap::Args)]
struct InteractiveArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// Optional CPU-RAM evidence for diagnostic attribution. The interactive flow skips it by default.
    #[arg(long)]
    cpu_ram_preflight: Option<PathBuf>,
    /// New directory that will receive selection, events and final report.
    #[arg(long)]
    output_dir: PathBuf,
    /// Planning horizon and wall-clock stop limit shown to the operator.
    #[arg(long, default_value_t = 1800)]
    duration_seconds: u64,
    /// Sparse test-file capacity on each selected filesystem.
    #[arg(long, default_value_t = 4096)]
    capacity_mib: u64,
    /// Maximum host writes on each selected filesystem.
    #[arg(long, default_value_t = 2048)]
    write_budget_mib: u64,
    /// Free space preserved after the test allocation on each filesystem.
    #[arg(long, default_value_t = 4096)]
    reserve_free_mib: u64,
    /// Maximum simultaneous SSD workers; zero uses host/device count.
    #[arg(long, default_value_t = 0)]
    max_workers: usize,
    #[arg(long, default_value_t = 8)]
    ui_fps: u32,
    /// Confirm a failing block at once (component fault ledger, step F4).
    #[arg(long)]
    confirm_on_fail: bool,
}

#[derive(Debug, clap::Args)]
struct CampaignArgs {
    #[arg(long, default_value_t = 12)]
    operations: u64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FaultArg {
    BitFlip,
    StaleGeneration,
    SwappedLba,
    DuplicateBlock,
    ShortRead,
    ShortWrite,
    IoError,
    Timeout,
    DeviceDisappeared,
    ProducerCompute,
    ReadCompute,
    ReferenceCompute,
    JoinMismatch,
    ReadSoakBitFlip,
}

impl From<FaultArg> for FaultKind {
    fn from(value: FaultArg) -> Self {
        match value {
            FaultArg::BitFlip => Self::BitFlip,
            FaultArg::StaleGeneration => Self::StaleGeneration,
            FaultArg::SwappedLba => Self::SwappedLba,
            FaultArg::DuplicateBlock => Self::DuplicateBlock,
            FaultArg::ShortRead => Self::ShortRead,
            FaultArg::ShortWrite => Self::ShortWrite,
            FaultArg::IoError => Self::IoError,
            FaultArg::Timeout => Self::Timeout,
            FaultArg::DeviceDisappeared => Self::DeviceDisappeared,
            FaultArg::ProducerCompute => Self::ProducerCompute,
            FaultArg::ReadCompute => Self::ReadCompute,
            FaultArg::ReferenceCompute => Self::ReferenceCompute,
            FaultArg::JoinMismatch => Self::JoinMismatch,
            FaultArg::ReadSoakBitFlip => Self::ReadSoakBitFlip,
        }
    }
}

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::from(2)
        }
    }
}

fn execute(cli: Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cli.command {
        Command::Inventory(args) => inventory_command(&args),
        Command::AutoPlan(args) => {
            let inventory = scan(&scan_options(&args.flags))?;
            let selection = parse_selection(&inventory, &args.devices)?;
            let plans = automatic_plans(
                &inventory,
                &selection,
                &args.flags,
                mib(args.required_free_mib)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&plans)?);
            Ok(ExitCode::SUCCESS)
        }
        Command::Select(args) => select_command(&args),
        Command::Revalidate(args) => revalidate_command(&args),
        Command::PrepareBounded(args) => prepare_bounded_command(&args),
        Command::RunBounded(args) => run_bounded_command(&args),
        Command::RunPreserving(args) => run_preserving_command(&args),
        Command::MultiSimulate(args) => multi_simulate_command(&args),
        Command::RunPreservingMulti(args) => run_preserving_multi_command(&args),
        Command::Interactive(args) => interactive_command(&args),
        Command::CombinedStorage(args) => combined_storage::command(&args),
        Command::FormatDisk(args) => format_disk::command(&args.flags),
        Command::Simulate(args) => simulate(&args),
        Command::Campaign(args) => campaign(&args),
        Command::Golden => {
            print_golden();
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_preserving_command(
    args: &RunPreservingArgs,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    ensure_output_absent(&args.report)?;
    if let Some(events) = args.events.as_deref() {
        ensure_output_absent(events)?;
        if events == args.report {
            return Err("events and report must use different new paths".into());
        }
    }
    let preflight = args
        .cpu_ram_preflight
        .as_deref()
        .map(validate_cpu_ram_preflight)
        .transpose()?;
    let selection: SelectionPlan = serde_json::from_slice(&std::fs::read(&args.selection)?)?;
    if selection.devices.is_empty() {
        return Err("selection contains no devices".into());
    }
    let inventory_before = scan(&scan_options(&args.flags))?;
    let validation = revalidate(&selection, &inventory_before);
    if !validation.valid {
        return Err(format!(
            "selection revalidation failed: {}",
            validation.errors.join("; ")
        )
        .into());
    }
    let selected = selection
        .devices
        .iter()
        .find(|device| device.display_index == args.device)
        .ok_or("selected namespace alias is absent from the selection plan")?;
    if args.confirm_device_id != selected.stable_id.0 {
        return Err(format!(
            "confirmation mismatch; expected exact namespace ID: {}",
            selected.stable_id
        )
        .into());
    }
    let namespace = inventory_before
        .devices
        .iter()
        .find(|device| device.stable_id == selected.stable_id)
        .cloned()
        .ok_or("selected namespace disappeared before run")?;

    let directory = args.directory.canonicalize()?;
    let directory_metadata = directory.symlink_metadata()?;
    if !directory_metadata.is_dir() {
        return Err(format!("test directory is not a directory: {}", directory.display()).into());
    }
    let directory_device = directory_metadata.dev();
    let (filesystem_target_id, filesystem_device_node) =
        filesystem_target(&namespace, directory_device)?;

    let capacity_bytes = mib(args.capacity_mib)?;
    let write_budget_bytes = mib(args.write_budget_mib)?;
    if capacity_bytes < 1024 * 1024 || !capacity_bytes.is_multiple_of(4096) {
        return Err("test-file capacity must be at least 1 MiB and 4 KiB aligned".into());
    }
    if write_budget_bytes < 4096 || !write_budget_bytes.is_multiple_of(4096) {
        return Err("write budget must be at least 4 KiB and 4 KiB aligned".into());
    }
    let reserved_free_bytes = mib(args.reserve_free_mib)?;
    let available_bytes_before = fs2::available_space(&directory)?;
    let maximum_allocation = capacity_bytes.min(write_budget_bytes);
    let required_free = maximum_allocation
        .checked_add(reserved_free_bytes)
        .ok_or("free-space requirement overflow")?;
    if available_bytes_before < required_free {
        return Err(format!(
            "insufficient free space: available {available_bytes_before} bytes, require {required_free} bytes including reserve"
        )
        .into());
    }

    let duration_seconds = args.duration_seconds.max(1);
    let requested_operations = args
        .operations
        .unwrap_or_else(|| duration_seconds.saturating_mul(128))
        .clamp(1, 100_000_000);
    let provisional = CoveragePlan::for_duration(
        Duration::from_secs(duration_seconds),
        requested_operations,
        args.seed,
    );
    let target_operations = operations_fitting_budget(&provisional, write_budget_bytes);
    if target_operations == 0 {
        return Err("write budget cannot fit the first 4 KiB operation".into());
    }

    let run_id = new_run_id();
    let test_file = directory.join(format!(".cpu-nvme-dit-{}.test", run_id.0));
    let mut backend =
        DirectFileBackend::create_new(&test_file, capacity_bytes, write_budget_bytes)?;
    let opened_device = match backend.path().metadata() {
        Ok(metadata) => metadata.dev(),
        Err(error) => {
            drop(backend);
            let _ = std::fs::remove_file(&test_file);
            return Err(error.into());
        }
    };
    if opened_device != directory_device {
        drop(backend);
        let _ = std::fs::remove_file(&test_file);
        return Err("mount identity changed while creating the test file".into());
    }
    let coverage = CoveragePlan::for_duration(
        Duration::from_secs(duration_seconds),
        target_operations,
        args.seed,
    );
    let mut config = RunConfig::new(run_id, namespace.stable_id.clone(), coverage);
    config.wall_clock_limit = Some(Duration::from_secs(duration_seconds));
    config.sustain_until_deadline = true;
    config.compute_workers = automatic_compute_workers(1);
    let engine_result = run_with_events(&mut backend, &config, args.events.as_deref());
    drop(backend);
    let cleanup_result = std::fs::remove_file(&test_file);
    let engine_report = engine_result?;
    cleanup_result.map_err(|error| {
        format!(
            "test completed but temporary file {} could not be removed: {error}",
            test_file.display()
        )
    })?;

    let inventory_after = scan(&scan_options(&args.flags))?;
    let after_device = inventory_after
        .devices
        .into_iter()
        .find(|device| device.stable_id == namespace.stable_id);
    let report = PreservingRunReport {
        schema_version: dit_contracts::SCHEMA_VERSION,
        mode: "DATA_PRESERVING_FILE",
        attribution: attribution_name(preflight.as_ref()),
        namespace_stable_id: namespace.stable_id.clone(),
        filesystem_target_id,
        filesystem_device_node,
        directory,
        test_file_was_new: true,
        test_file_removed: true,
        available_bytes_before,
        reserved_free_bytes,
        cpu_ram_preflight: preflight,
        inventory_before: namespace,
        inventory_after: after_device,
        engine: engine_report,
    };
    write_json_file(&args.report, &report)?;
    let (matrix_checks, write_operations, read_soak_operations) = deep_counts(&report.engine);
    println!(
        "{} mode={} io={}/{} matrix_checks={} write_ops={} read_soak={} written={} read={} verified={} failures={} temporary_file_removed=true report={}",
        verdict_name(report.engine.verdict),
        report.mode,
        report.engine.completed_operations,
        report.engine.target_operations,
        matrix_checks,
        write_operations,
        read_soak_operations,
        report.engine.bytes_written,
        report.engine.bytes_read,
        report.engine.bytes_verified,
        report.engine.failure_count,
        args.report.display()
    );
    Ok(if report.engine.verdict == Verdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn filesystem_target(
    namespace: &NvmeDevice,
    directory_device: u64,
) -> Result<(DeviceId, PathBuf), Box<dyn std::error::Error>> {
    let namespace_metadata = namespace.device_node.symlink_metadata()?;
    if namespace_metadata.file_type().is_block_device()
        && namespace_metadata.rdev() == directory_device
    {
        return Ok((namespace.stable_id.clone(), namespace.device_node.clone()));
    }
    for partition in &namespace.partition_details {
        let Ok(metadata) = partition.device_node.symlink_metadata() else {
            continue;
        };
        if metadata.file_type().is_block_device() && metadata.rdev() == directory_device {
            return Ok((partition.stable_id.clone(), partition.device_node.clone()));
        }
    }
    Err(format!(
        "directory filesystem is not directly mounted from selected NVMe {} (device-mapper/LVM/encrypted stacks are not accepted yet)",
        namespace.device_node.display()
    )
    .into())
}

#[allow(clippy::too_many_lines)]
fn interactive_command(args: &InteractiveArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() {
        return Err("interactive CPU-SSD UI requires a terminal".into());
    }
    if !(1..=20).contains(&args.ui_fps) {
        return Err("--ui-fps must be between 1 and 20".into());
    }
    let _preflight = args
        .cpu_ram_preflight
        .as_deref()
        .map(validate_cpu_ram_preflight)
        .transpose()?;
    if args.output_dir.symlink_metadata().is_ok() {
        return Err(format!(
            "output directory already exists and will not be reused: {}",
            args.output_dir.display()
        )
        .into());
    }

    let inventory = scan_releasing_stale(&args.flags)?;
    if inventory.devices.is_empty() {
        return Err("no NVMe namespace was discovered".into());
    }
    print!("\x1b[H\x1b[2J");
    println!(
        "{}",
        render_picker(
            &picker_entries(&inventory),
            &BTreeSet::new(),
            TerminalSize::current()
        )
    );
    print!("Select NVMe aliases (example 1,2) or A for all: ");
    io::stdout().flush()?;
    let request = read_operator_line()?;
    let selection = parse_selection(&inventory, request.trim())?;

    let default_minutes = args.duration_seconds.max(60) / 60;
    print!(
        "Test duration: 1) 10 min  2) 30 min  3) 60 min  or type minutes (1-1440)  [{default_minutes}] > "
    );
    io::stdout().flush()?;
    let duration_seconds = parse_ssd_minutes(&read_operator_line()?, default_minutes) * 60;
    println!(
        "Write level per SSD: L) Light {} MiB (default)  S) Standard {} MiB  H) Heavy {} MiB",
        args.write_budget_mib,
        WriteLevel::Standard.budget_mib(args),
        WriteLevel::Heavy.budget_mib(args)
    );
    println!(
        "  Writes stop at the level; the test then re-reads and re-verifies what it wrote until the time is up."
    );
    println!(
        "  Each SSD needs that much free space plus {} MiB left untouched. Only new test files are used.",
        args.reserve_free_mib
    );
    print!("Level [L] > ");
    io::stdout().flush()?;
    let level = parse_write_level(&read_operator_line()?);
    let (capacity_mib, write_budget_mib) = (level.capacity_mib(args), level.budget_mib(args));

    let required = mib(capacity_mib.min(write_budget_mib))?
        .checked_add(mib(args.reserve_free_mib)?)
        .ok_or("free-space requirement overflow")?;
    let plans = automatic_plans(&inventory, &selection, &args.flags, required)?;
    println!();
    println!(
        "SSD selection accepted: starting DEEP data-preserving test for {duration_seconds}s, write level {} ({write_budget_mib} MiB per SSD).",
        level.label()
    );
    println!("Only new test files are used; system/boot/results devices remain blocked.");

    if let Some(parent) = args.output_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(&args.output_dir)?;
    let selection_path = args.output_dir.join("selection.json");
    let events_path = args.output_dir.join("events.jsonl");
    let report_path = args.output_dir.join("report.json");
    write_selection_file(&selection_path, &selection)?;
    // Re-scan immediately before side effects; neither alias reorder nor a changed
    // partition/filesystem/mount plan may silently redirect an approved test.
    let fresh = scan(&scan_options(&args.flags))?;
    let validation = revalidate(&selection, &fresh);
    if !validation.valid {
        return Err(format!("selection changed: {:?}", validation.errors).into());
    }
    if automatic_plans(&fresh, &selection, &args.flags, required)? != plans {
        return Err("automatic storage plan changed; cancel and select again".into());
    }
    let mut setup_file = File::create_new(args.output_dir.join("auto-storage-plan.json"))?;
    serde_json::to_writer_pretty(&mut setup_file, &plans)?;
    setup_file.sync_all()?;
    let session = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let mut areas = auto_storage::PreparedAreas::default();
    let mut targets = Vec::with_capacity(plans.len());
    let mut directories = Vec::with_capacity(plans.len());
    for plan in &plans {
        println!("Preparing [{}] {} ...", plan.alias, plan.node.display());
        let prepared = (|| -> Result<PathBuf, Box<dyn std::error::Error>> {
            let current = scan(&scan_options(&args.flags))?;
            if !revalidate(&selection, &current).valid {
                return Err("SSD selection changed during preparation".into());
            }
            let device = current
                .devices
                .iter()
                .find(|device| device.stable_id == plan.namespace_id)
                .ok_or("selected SSD disappeared during preparation")?;
            let mut checked = auto_storage::plan(device, &args.flags.protected_devices, required)?;
            checked.alias = plan.alias;
            if &checked != plan {
                return Err("volume plan changed during preparation; select again".into());
            }
            prepare_offering_repair(&mut areas, plan, &session, required)
        })();
        let directory = match prepared {
            Ok(directory) => directory,
            Err(error) => {
                let cleanup = areas.cleanup();
                write_setup_outcome(
                    &args.output_dir,
                    "PREPARATION_ERROR",
                    Some(&error.to_string()),
                    &cleanup,
                )?;
                return Err(format!(
                    "AUTO SETUP [{}] {}: {error}",
                    plan.alias,
                    plan.node.display()
                )
                .into());
            }
        };
        println!("  New test directory: {}", directory.display());
        targets.push(format!("{}={}", plan.alias, directory.display()));
        directories.push(directory);
    }
    let run_args = RunPreservingMultiArgs {
        flags: InventoryFlags {
            no_smart: args.flags.no_smart,
            protected_devices: args.flags.protected_devices.clone(),
        },
        selection: selection_path,
        targets,
        confirm_device_ids: selection
            .devices
            .iter()
            .map(|device| device.stable_id.0.clone())
            .collect(),
        capacity_mib,
        write_budget_mib,
        reserve_free_mib: args.reserve_free_mib,
        duration_seconds,
        operations: None,
        seed: SystemTime::now().duration_since(UNIX_EPOCH).map_or(
            0xbb67_ae85_84ca_a73b,
            |duration| {
                u64::try_from(duration.as_nanos())
                    .unwrap_or_else(|_| duration.as_secs() ^ u64::from(duration.subsec_nanos()))
            },
        ),
        max_workers: args.max_workers,
        cpu_ram_preflight: args.cpu_ram_preflight.clone(),
        no_ui: false,
        ui_fps: args.ui_fps,
        events: Some(events_path),
        report: report_path,
        confirm_on_fail: args.confirm_on_fail,
    };
    // Flushed every few seconds while the test writes, so a crash leaves consistent metadata.
    let keeper = auto_storage::SyncKeeper::start(directories, FS_SYNC_PERIOD);
    let outcome = run_preserving_multi_command(&run_args);
    drop(keeper);
    let cleanup = areas.cleanup();
    write_setup_outcome(
        &args.output_dir,
        "ENGINE_RETURNED",
        outcome.as_ref().err().map(ToString::to_string).as_deref(),
        &cleanup,
    )?;
    // A cleanup error must not be presented as successful overall completion.
    cleanup?;
    outcome
}

fn write_setup_outcome(
    output: &Path,
    phase: &str,
    error: Option<&str>,
    cleanup: &Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = File::create_new(output.join("auto-storage-outcome.json"))?;
    serde_json::to_writer_pretty(
        &mut file,
        &serde_json::json!({
            "schema_version": 1, "phase": phase, "error": error,
            "cleanup_complete": cleanup.is_ok(),
            "cleanup_error": cleanup.as_ref().err().map(ToString::to_string),
            "note": "Storage setup/cleanup evidence only; not an integrity verdict."
        }),
    )?;
    file.sync_all()?;
    Ok(())
}

fn automatic_plans(
    inventory: &Inventory,
    selection: &SelectionPlan,
    flags: &InventoryFlags,
    required: u64,
) -> Result<Vec<auto_storage::VolumePlan>, Box<dyn std::error::Error>> {
    selection
        .devices
        .iter()
        .map(|selected| {
            let device = inventory
                .devices
                .iter()
                .find(|device| device.stable_id == selected.stable_id)
                .ok_or("selected SSD disappeared")?;
            let mut plan = auto_storage::plan(device, &flags.protected_devices, required)?;
            plan.alias = selected.display_index;
            Ok(plan)
        })
        .collect()
}

/// How much each selected SSD may be written in an interactive run. `Light` is the command's own
/// `--capacity-mib`/`--write-budget-mib` (the long-standing default); the others write more of
/// the drive before the run turns to re-reading and re-verifying what it wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteLevel {
    Light,
    Standard,
    Heavy,
}

impl WriteLevel {
    fn budget_mib(self, args: &InteractiveArgs) -> u64 {
        self.budget_for(args.write_budget_mib)
    }

    /// The test file is as large as the budget, so the writes land on that many distinct bytes
    /// of the drive instead of rewriting a small file.
    fn capacity_mib(self, args: &InteractiveArgs) -> u64 {
        self.capacity_for(args.capacity_mib, args.write_budget_mib)
    }

    fn budget_for(self, light_budget_mib: u64) -> u64 {
        match self {
            Self::Light => light_budget_mib,
            Self::Standard => 16 * 1024,
            Self::Heavy => 64 * 1024,
        }
    }

    fn capacity_for(self, light_capacity_mib: u64, light_budget_mib: u64) -> u64 {
        match self {
            Self::Light => light_capacity_mib,
            Self::Standard | Self::Heavy => self.budget_for(light_budget_mib),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Light => "LIGHT",
            Self::Standard => "STANDARD",
            Self::Heavy => "HEAVY",
        }
    }
}

/// Anything but S or H keeps the light default.
fn parse_write_level(answer: &str) -> WriteLevel {
    match answer.trim().to_ascii_lowercase().as_str() {
        "s" | "standard" => WriteLevel::Standard,
        "h" | "heavy" => WriteLevel::Heavy,
        _ => WriteLevel::Light,
    }
}

/// Minutes from the duration prompt: a preset key, typed minutes (1..=1440), or the default.
fn parse_ssd_minutes(answer: &str, default_minutes: u64) -> u64 {
    match answer.trim().to_ascii_lowercase().as_str() {
        "1" => 10,
        "2" => 30,
        "3" => 60,
        typed => typed
            .strip_suffix('m')
            .unwrap_or(typed)
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|minutes| (1..=1440).contains(minutes))
            .unwrap_or(default_minutes),
    }
}

/// How often the test filesystems are flushed while a test writes to them.
const FS_SYNC_PERIOD: Duration = Duration::from_secs(10);

/// Prepares `plan`; when the NTFS safety probe refuses a volume in a state an interrupted test
/// leaves behind, offers the `ntfsfix` repair (operator's choice) and tries once more.
fn prepare_offering_repair(
    areas: &mut auto_storage::PreparedAreas,
    plan: &auto_storage::VolumePlan,
    session: &str,
    required: u64,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let error = match areas.prepare(plan, session, required) {
        Ok(directory) => return Ok(directory),
        Err(error) => error,
    };
    let Some(refused) = error
        .downcast_ref::<auto_storage::NtfsRefused>()
        .filter(|refused| refused.repairable())
    else {
        return Err(error);
    };
    println!("{refused}");
    println!(
        "  F) Repair with ntfsfix: rewrites $MFTMirr from $MFT, empties the journal and clears the dirty flag."
    );
    println!(
        "     It is not a full check (chkdsk): use it after a crash of this test, or when the drive holds"
    );
    println!("     nothing you need. Otherwise run 'chkdsk /f' on the drive in Windows.");
    print!("  F = repair and continue, Enter = leave the drive untouched > ");
    io::stdout().flush()?;
    if !read_operator_line()?.trim().eq_ignore_ascii_case("f") {
        return Err(error);
    }
    let node = refused.node.clone();
    let printed = auto_storage::repair_ntfs(&node)
        .map_err(|repair| format!("ntfsfix repair of {} failed: {repair}", node.display()))?;
    if let Some(last) = printed.lines().last() {
        println!("  ntfsfix: {last}");
    }
    println!("  Repaired; preparing {} again ...", node.display());
    areas.prepare(plan, session, required)
}

fn read_operator_line() -> io::Result<String> {
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "input closed; setup cancelled",
        ));
    }
    Ok(input)
}

fn resolve_test_directory(path: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if !path.is_absolute() {
        return Err("enter an absolute path to an existing directory on the mounted SSD".into());
    }
    let directory = path.canonicalize().map_err(|error| {
        format!(
            "cannot access test directory {}: {error}. The directory must already exist on the selected SSD's mounted filesystem. Creating an empty mount-point directory alone does not mount an SSD",
            path.display()
        )
    })?;
    if !directory.is_dir() {
        return Err(format!("test path is not a directory: {}", directory.display()).into());
    }
    Ok(directory)
}

fn reject_critical_ui_directory(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.is_absolute() {
        return Err("test directory must be an absolute path".into());
    }
    if path == Path::new("/")
        || path.starts_with("/boot")
        || path.starts_with("/home")
        || path.starts_with("/usr")
        || path.starts_with("/var")
        || path.starts_with("/mnt/cpu-dit-results")
    {
        return Err(format!(
            "the interactive UI refuses critical/results path {}; mount the selected SSD under a dedicated /mnt path",
            path.display()
        )
        .into());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn multi_simulate_command(
    args: &MultiSimulateArgs,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !(1..=64).contains(&args.devices) {
        return Err("--devices must be between 1 and 64".into());
    }
    if !(1..=20).contains(&args.ui_fps) {
        return Err("--ui-fps must be between 1 and 20".into());
    }
    ensure_output_absent(&args.report)?;
    if let Some(events) = args.events.as_deref() {
        ensure_output_absent(events)?;
        if events == args.report {
            return Err("events and report must use different new paths".into());
        }
    }
    if args.inject_device.is_some() && args.inject.is_none() {
        return Err("--inject-device requires --inject".into());
    }
    let fault_device = args.inject_device.unwrap_or(1);
    if args.inject.is_some() && !(1..=args.devices).contains(&fault_device) {
        return Err(format!("injected device alias must be 1..={}", args.devices).into());
    }
    let capacity_bytes = mib(args.capacity_mib)?;
    let aggregate_bytes = capacity_bytes
        .checked_mul(u64::try_from(args.devices)?)
        .ok_or("aggregate simulator memory overflow")?;
    let memory_budget_bytes = mib(args.memory_budget_mib)?;
    if aggregate_bytes > memory_budget_bytes {
        return Err(format!(
            "simulator memory request {aggregate_bytes} bytes exceeds budget {memory_budget_bytes} bytes"
        )
        .into());
    }
    let capacity = usize::try_from(capacity_bytes)?;
    let duration_seconds = args.duration_seconds.max(1);
    let operations = args
        .operations
        .unwrap_or_else(|| duration_seconds.saturating_mul(128))
        .max(1);
    let run_id = new_run_id();
    let max_workers = device_worker_limit(args.max_workers, args.devices);
    let compute_workers = automatic_compute_workers(max_workers);
    let mut descriptors = Vec::with_capacity(args.devices);
    let mut jobs = Vec::with_capacity(args.devices);
    for index in 1..=args.devices {
        let device_id = DeviceId(format!("memory:nvme-{index}"));
        descriptors.push(DeviceDescriptor {
            id: device_id.clone(),
            label: format!("SSD {index}"),
            model: format!("Simulated NVMe Device {index}"),
            capacity: human_bytes(capacity_bytes),
            detail: format!(
                "memory backend / worker {index} / test {} MiB",
                args.capacity_mib
            ),
        });
        let device_seed = args.seed.wrapping_add(
            u64::try_from(index)
                .unwrap_or(u64::MAX)
                .wrapping_mul(0x9e37_79b9_7f4a_7c15),
        );
        let plan = CoveragePlan::for_duration(
            Duration::from_secs(duration_seconds),
            operations,
            device_seed,
        );
        let mut config = RunConfig::new(run_id.clone(), device_id.clone(), plan);
        config.confirm_on_fail = args.confirm_on_fail;
        config.wall_clock_limit = Some(Duration::from_secs(duration_seconds));
        config.sustain_until_deadline = args.operations.is_none();
        config.compute_workers = compute_workers;
        if index == fault_device {
            config.fault = args.inject.map(|kind| FaultPlan {
                kind: kind.into(),
                at_operation: args.inject_at,
            });
        }
        jobs.push(DeviceJob {
            backend: MemoryBackend::new(device_id.0.clone(), capacity),
            config,
        });
    }

    let mut event_journal = EventJournal::new(args.events.as_deref())?;
    let mut event_error: Option<io::Error> = None;
    let mut ui_state = UiState::new("DIT CPU-SSD", descriptors);
    ui_state.set_run_context(Duration::from_secs(duration_seconds), args.seed);
    let use_ui = !args.no_ui && io::stdout().is_terminal();
    let mut host_sampler = use_ui.then(CpuNvmeHostSampler::detect);
    if let Some(sampler) = host_sampler.as_mut() {
        ui_state.set_system_info(sampler.system_info());
        ui_state.set_live_utilization(sampler.sample());
    }
    let mut last_host_sample = Instant::now();
    let mut renderer = use_ui.then(|| InPlaceRenderer::new(io::stdout(), args.ui_fps));
    if let Some(active) = renderer.as_mut() {
        active.render_if_due(&ui_state, true)?;
    }
    let report: MultiDeviceReport = run_multi_with_limit(jobs, max_workers, |event| {
        if event_error.is_none()
            && let Err(error) = event_journal.record(&event)
        {
            event_error = Some(error);
        }
        ui_state.ingest(&event);
        if last_host_sample.elapsed() >= Duration::from_millis(250)
            && let Some(sampler) = host_sampler.as_mut()
        {
            ui_state.set_live_utilization(sampler.sample());
            last_host_sample = Instant::now();
        }
        if event_error.is_none()
            && let Some(active) = renderer.as_mut()
            && let Err(error) = active.render_if_due(&ui_state, false)
        {
            event_error = Some(error);
        }
    })?;
    if let Some(active) = renderer.as_mut() {
        active.render_if_due(&ui_state, true)?;
        active.finish()?;
        println!();
    }
    if let Some(error) = event_error {
        return Err(error.into());
    }
    event_journal.flush()?;
    write_json_file(&args.report, &report)?;
    println!(
        "{} devices={} elapsed={:.3}s report={}",
        verdict_name(report.aggregate_verdict),
        report.selected_devices,
        report.elapsed_seconds,
        args.report.display()
    );
    for outcome in &report.devices {
        if let Some(device_report) = outcome.report.as_ref() {
            println!(
                "  {} {} operations={}/{} failures={}",
                outcome.device_id,
                verdict_name(device_report.verdict),
                device_report.completed_operations,
                device_report.target_operations,
                device_report.failure_count
            );
        } else {
            println!(
                "  {} INVALID worker_error={}",
                outcome.device_id,
                outcome.worker_error.as_deref().unwrap_or("unknown")
            );
        }
    }
    Ok(if report.aggregate_verdict == Verdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

#[allow(clippy::too_many_lines)]
fn run_preserving_multi_command(
    args: &RunPreservingMultiArgs,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !(1..=20).contains(&args.ui_fps) {
        return Err("--ui-fps must be between 1 and 20".into());
    }
    ensure_output_absent(&args.report)?;
    if let Some(events) = args.events.as_deref() {
        ensure_output_absent(events)?;
        if events == args.report {
            return Err("events and report must use different new paths".into());
        }
    }
    let preflight = args
        .cpu_ram_preflight
        .as_deref()
        .map(validate_cpu_ram_preflight)
        .transpose()?;
    let selection: SelectionPlan = serde_json::from_slice(&std::fs::read(&args.selection)?)?;
    if selection.devices.is_empty() {
        return Err("selection contains no devices".into());
    }
    let inventory = scan(&scan_options(&args.flags))?;
    let validation = revalidate(&selection, &inventory);
    if !validation.valid {
        return Err(format!(
            "selection revalidation failed: {}",
            validation.errors.join("; ")
        )
        .into());
    }
    let target_map = parse_target_map(&args.targets)?;
    let selected_aliases = selection
        .devices
        .iter()
        .map(|device| device.display_index)
        .collect::<BTreeSet<_>>();
    if target_map.keys().copied().collect::<BTreeSet<_>>() != selected_aliases {
        return Err("--target mappings must cover every selected alias exactly once".into());
    }
    let confirmed = args
        .confirm_device_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let expected = selection
        .devices
        .iter()
        .map(|device| device.stable_id.0.clone())
        .collect::<BTreeSet<_>>();
    if confirmed.len() != args.confirm_device_ids.len() || confirmed != expected {
        return Err("confirmed stable IDs must exactly match all selected namespaces".into());
    }

    let capacity_bytes = mib(args.capacity_mib)?;
    let write_budget_bytes = mib(args.write_budget_mib)?;
    if capacity_bytes < 1024 * 1024 || !capacity_bytes.is_multiple_of(4096) {
        return Err("test-file capacity must be at least 1 MiB and 4 KiB aligned".into());
    }
    if write_budget_bytes < 4096 || !write_budget_bytes.is_multiple_of(4096) {
        return Err("write budget must be at least 4 KiB and 4 KiB aligned".into());
    }
    let reserved_free_bytes = mib(args.reserve_free_mib)?;
    let required_free = capacity_bytes
        .min(write_budget_bytes)
        .checked_add(reserved_free_bytes)
        .ok_or("free-space requirement overflow")?;
    let duration_seconds = args.duration_seconds.max(1);
    let requested_operations = args
        .operations
        .unwrap_or_else(|| duration_seconds.saturating_mul(128))
        .clamp(1, 100_000_000);
    let provisional = CoveragePlan::for_duration(
        Duration::from_secs(duration_seconds),
        requested_operations,
        args.seed,
    );
    let target_operations = operations_fitting_budget(&provisional, write_budget_bytes);
    if target_operations == 0 {
        return Err("write budget cannot fit the first 4 KiB operation".into());
    }

    let max_workers = device_worker_limit(args.max_workers, selection.devices.len());
    let compute_workers = automatic_compute_workers(max_workers);
    let mut event_journal = EventJournal::new(args.events.as_deref())?;
    let run_id = new_run_id();
    let mut jobs = Vec::with_capacity(selection.devices.len());
    let mut descriptors = Vec::with_capacity(selection.devices.len());
    let mut target_evidence = Vec::with_capacity(selection.devices.len());
    let mut test_files = Vec::with_capacity(selection.devices.len());
    let preparation = (|| -> Result<(), Box<dyn std::error::Error>> {
        for selected in &selection.devices {
            let namespace = inventory
                .devices
                .iter()
                .find(|device| device.stable_id == selected.stable_id)
                .ok_or("selected namespace disappeared during preparation")?;
            let directory = resolve_test_directory(
                target_map
                    .get(&selected.display_index)
                    .ok_or("missing target directory")?,
            )?;
            let metadata = directory.symlink_metadata()?;
            if !metadata.is_dir() {
                return Err(format!("not a directory: {}", directory.display()).into());
            }
            let directory_device = metadata.dev();
            let (filesystem_target_id, filesystem_device_node) =
                filesystem_target(namespace, directory_device)?;
            let available_bytes_before = fs2::available_space(&directory)?;
            if available_bytes_before < required_free {
                return Err(format!(
                    "insufficient free space on {}: available {available_bytes_before}, require {required_free}",
                    directory.display()
                )
                .into());
            }
            let test_file = directory.join(format!(
                ".cpu-nvme-dit-{}-{}.test",
                run_id.0, selected.display_index
            ));
            let backend =
                DirectFileBackend::create_new(&test_file, capacity_bytes, write_budget_bytes)?;
            test_files.push(test_file);
            if backend.path().metadata()?.dev() != directory_device {
                drop(backend);
                return Err("mount identity changed while creating a test file".into());
            }
            let device_seed = args.seed.wrapping_add(
                u64::try_from(selected.display_index)
                    .unwrap_or(u64::MAX)
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15),
            );
            let coverage = CoveragePlan::for_duration(
                Duration::from_secs(duration_seconds),
                target_operations,
                device_seed,
            );
            let mut config = RunConfig::new(run_id.clone(), namespace.stable_id.clone(), coverage);
            config.confirm_on_fail = args.confirm_on_fail;
            config.wall_clock_limit = Some(Duration::from_secs(duration_seconds));
            config.sustain_until_deadline = true;
            config.compute_workers = compute_workers;
            descriptors.push(DeviceDescriptor {
                id: namespace.stable_id.clone(),
                label: format!("SSD {}", selected.display_index),
                model: namespace.model.clone(),
                capacity: human_bytes(namespace.capacity_bytes),
                detail: format!(
                    "{} / {} / test {} MiB / write max {} MiB",
                    filesystem_device_node.display(),
                    namespace.driver,
                    args.capacity_mib,
                    args.write_budget_mib
                ),
            });
            target_evidence.push(PreservingTargetEvidence {
                alias: selected.display_index,
                namespace_stable_id: namespace.stable_id.clone(),
                filesystem_target_id,
                filesystem_device_node,
                directory,
                available_bytes_before,
                reserved_free_bytes,
                test_file_was_new: true,
                test_file_removed: false,
            });
            jobs.push(DeviceJob { backend, config });
        }
        Ok(())
    })();
    if let Err(error) = preparation {
        drop(jobs);
        remove_test_files_best_effort(&test_files);
        return Err(error);
    }

    let mut event_error: Option<io::Error> = None;
    let mut ui_state = UiState::new("DIT CPU-SSD", descriptors);
    ui_state.set_run_context(Duration::from_secs(args.duration_seconds), args.seed);
    let use_ui = !args.no_ui && io::stdout().is_terminal();
    let mut host_sampler = use_ui.then(CpuNvmeHostSampler::detect);
    if let Some(sampler) = host_sampler.as_mut() {
        ui_state.set_system_info(sampler.system_info());
        ui_state.set_live_utilization(sampler.sample());
    }
    let mut last_host_sample = Instant::now();
    let mut renderer = use_ui.then(|| InPlaceRenderer::new(io::stdout(), args.ui_fps));
    if let Some(active) = renderer.as_mut()
        && let Err(error) = active.render_if_due(&ui_state, true)
    {
        drop(jobs);
        remove_test_files_best_effort(&test_files);
        return Err(error.into());
    }
    let scheduler_result = run_multi_with_limit(jobs, max_workers, |event| {
        if event_error.is_none()
            && let Err(error) = event_journal.record(&event)
        {
            event_error = Some(error);
        }
        ui_state.ingest(&event);
        if last_host_sample.elapsed() >= Duration::from_millis(250)
            && let Some(sampler) = host_sampler.as_mut()
        {
            ui_state.set_live_utilization(sampler.sample());
            last_host_sample = Instant::now();
        }
        if event_error.is_none()
            && let Some(active) = renderer.as_mut()
            && let Err(error) = active.render_if_due(&ui_state, false)
        {
            event_error = Some(error);
        }
    });
    let cleanup_result = remove_test_files(&test_files);
    for target in &mut target_evidence {
        target.test_file_removed = cleanup_result.is_ok();
    }
    let scheduler = scheduler_result?;
    cleanup_result?;
    if let Some(active) = renderer.as_mut() {
        active.render_if_due(&ui_state, true)?;
        active.finish()?;
        println!();
    }
    if let Some(error) = event_error {
        return Err(error.into());
    }
    event_journal.flush()?;
    let inventory_after = scan(&scan_options(&args.flags))?;
    let selected_ids = selection
        .devices
        .iter()
        .map(|device| device.stable_id.clone())
        .collect::<BTreeSet<_>>();
    let report = MultiPreservingRunReport {
        schema_version: dit_contracts::SCHEMA_VERSION,
        mode: "DATA_PRESERVING_FILE_MULTI",
        attribution: attribution_name(preflight.as_ref()),
        cpu_ram_preflight: preflight,
        targets: target_evidence,
        inventory_before: inventory
            .devices
            .into_iter()
            .filter(|device| selected_ids.contains(&device.stable_id))
            .collect(),
        inventory_after: inventory_after
            .devices
            .into_iter()
            .filter(|device| selected_ids.contains(&device.stable_id))
            .collect(),
        scheduler,
    };
    write_json_file(&args.report, &report)?;
    let (matrix_checks, write_operations, read_soak_operations) = report
        .scheduler
        .devices
        .iter()
        .filter_map(|outcome| outcome.report.as_ref())
        .map(deep_counts)
        .fold((0_u64, 0_u64, 0_u64), |total, value| {
            (
                total.0.saturating_add(value.0),
                total.1.saturating_add(value.1),
                total.2.saturating_add(value.2),
            )
        });
    println!(
        "{} mode={} devices={} workers={} matrix_checks={} write_ops={} read_soak={} temporary_files_removed=true report={}",
        verdict_name(report.scheduler.aggregate_verdict),
        report.mode,
        report.scheduler.selected_devices,
        report.scheduler.max_parallel_workers,
        matrix_checks,
        write_operations,
        read_soak_operations,
        args.report.display()
    );
    Ok(if report.scheduler.aggregate_verdict == Verdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn parse_target_map(
    values: &[String],
) -> Result<BTreeMap<usize, PathBuf>, Box<dyn std::error::Error>> {
    let mut targets = BTreeMap::new();
    for value in values {
        let (alias, directory) = value
            .split_once('=')
            .ok_or("target must use ALIAS=/mounted/directory format")?;
        let alias = alias.parse::<usize>()?;
        if alias == 0 || directory.is_empty() {
            return Err("target alias and directory must be non-empty".into());
        }
        if targets.insert(alias, PathBuf::from(directory)).is_some() {
            return Err(format!("target alias {alias} occurs more than once").into());
        }
    }
    Ok(targets)
}

fn remove_test_files(paths: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let mut failures = Vec::new();
    for path in paths {
        if let Err(error) = std::fs::remove_file(path) {
            failures.push(format!("{}: {error}", path.display()));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("temporary-file cleanup failed: {}", failures.join("; ")).into())
    }
}

fn remove_test_files_best_effort(paths: &[PathBuf]) {
    for path in paths {
        let _ = std::fs::remove_file(path);
    }
}

fn revalidate_command(args: &RevalidateArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let plan: SelectionPlan = serde_json::from_slice(&std::fs::read(&args.plan)?)?;
    let inventory = scan(&scan_options(&args.flags))?;
    let result = revalidate(&plan, &inventory);
    if result.valid {
        println!(
            "PASS selection revalidated: {} device(s), stable identity and device node unchanged",
            plan.devices.len()
        );
        Ok(ExitCode::SUCCESS)
    } else {
        println!("FAIL selection revalidation");
        for error in result.errors {
            println!("  {error}");
        }
        Ok(ExitCode::from(1))
    }
}

fn prepare_bounded_command(
    args: &PrepareBoundedArgs,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let selection: SelectionPlan = serde_json::from_slice(&std::fs::read(&args.selection)?)?;
    let inventory = scan(&scan_options(&args.flags))?;
    let duration_seconds = args.duration_seconds.max(1);
    let requested_operations = args
        .operations
        .unwrap_or_else(|| duration_seconds.saturating_mul(128))
        .clamp(1, 100_000_000);
    let write_budget_bytes = mib(args.write_budget_mib)?;
    let provisional = CoveragePlan::for_duration(
        Duration::from_secs(duration_seconds),
        requested_operations,
        args.seed,
    );
    let target_operations = operations_fitting_budget(&provisional, write_budget_bytes);
    if target_operations == 0 {
        return Err("write budget cannot fit the first 4 KiB operation".into());
    }
    let request = BoundedRequest {
        range_start_bytes: mib(args.range_start_mib)?,
        range_length_bytes: mib(args.range_size_mib)?,
        max_host_write_bytes: write_budget_bytes,
        duration_seconds,
        target_operations,
        base_seed: args.seed,
    };
    let plan = prepare_bounded_plan(&inventory, &selection, args.device, &args.target, request)?;
    write_json_file(&args.output, &plan)?;
    println!("BOUNDED PLAN CREATED (no device was opened)");
    println!("Target: {} | {}", plan.target_node.display(), plan.model);
    println!(
        "Range: {} + {} bytes | host-write budget: {} bytes",
        plan.range_start_bytes, plan.range_length_bytes, plan.max_host_write_bytes
    );
    println!(
        "Duration: {}s | operations cap: {}",
        plan.duration_seconds, plan.target_operations
    );
    println!("Confirmation target ID:");
    println!("{}", plan.target_stable_id);
    println!("Plan: {}", args.output.display());
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_lines)]
fn run_bounded_command(args: &RunBoundedArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let preflight = args
        .cpu_ram_preflight
        .as_deref()
        .map(validate_cpu_ram_preflight)
        .transpose()?;
    let plan: BoundedWritePlan = serde_json::from_slice(&std::fs::read(&args.plan)?)?;
    let inventory_before = scan(&scan_options(&args.flags))?;
    let before_device = inventory_before
        .devices
        .iter()
        .find(|device| device.stable_id == plan.namespace_stable_id)
        .cloned()
        .ok_or("selected namespace missing before run")?;
    let mut backend = open_bounded(
        &plan,
        &inventory_before,
        &args.confirm_target_id,
        args.acknowledge_data_loss,
    )?;
    let coverage = CoveragePlan::for_duration(
        Duration::from_secs(plan.duration_seconds),
        plan.target_operations,
        plan.base_seed,
    );
    let mut config = RunConfig::new(new_run_id(), plan.target_stable_id.clone(), coverage);
    config.wall_clock_limit = Some(Duration::from_secs(plan.duration_seconds));
    config.sustain_until_deadline = true;
    config.compute_workers = automatic_compute_workers(1);
    let engine_report = run_with_events(&mut backend, &config, args.events.as_deref())?;
    drop(backend);
    let inventory_after = scan(&scan_options(&args.flags))?;
    let after_device = inventory_after
        .devices
        .into_iter()
        .find(|device| device.stable_id == plan.namespace_stable_id);
    let report = BoundedRunReport {
        schema_version: dit_contracts::SCHEMA_VERSION,
        plan,
        attribution: attribution_name(preflight.as_ref()),
        cpu_ram_preflight: preflight,
        inventory_before: before_device,
        inventory_after: after_device,
        engine: engine_report,
    };
    write_json_file(&args.report, &report)?;
    let (matrix_checks, write_operations, read_soak_operations) = deep_counts(&report.engine);
    println!(
        "{} io={}/{} matrix_checks={} write_ops={} read_soak={} written={} read={} verified={} failures={} report={}",
        verdict_name(report.engine.verdict),
        report.engine.completed_operations,
        report.engine.target_operations,
        matrix_checks,
        write_operations,
        read_soak_operations,
        report.engine.bytes_written,
        report.engine.bytes_read,
        report.engine.bytes_verified,
        report.engine.failure_count,
        args.report.display()
    );
    Ok(if report.engine.verdict == Verdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn validate_cpu_ram_preflight(
    path: &Path,
) -> Result<CpuRamPreflightEvidence, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(path)?;
    let preflight: CpuRamPreflight = serde_json::from_slice(&bytes)?;
    if preflight.module_id != "dit.cpu-ram" || preflight.verdict != Verdict::Pass {
        return Err(format!(
            "CPU-RAM preflight must have module_id=dit.cpu-ram and verdict=PASS; got {} {:?}",
            preflight.module_id, preflight.verdict
        )
        .into());
    }
    Ok(CpuRamPreflightEvidence {
        path: path.to_path_buf(),
        schema_version: preflight.schema_version,
        module_id: preflight.module_id,
        run_id: preflight.run_id,
        verdict: preflight.verdict,
        sha256: hex(&Sha256::digest(&bytes)),
    })
}

fn attribution_name(preflight: Option<&CpuRamPreflightEvidence>) -> &'static str {
    if preflight.is_some() {
        "SSD_AFTER_CPU_RAM_PREFLIGHT"
    } else {
        "CPU_SSD_PATH_UNATTRIBUTED"
    }
}

fn operations_fitting_budget(plan: &CoveragePlan, budget: u64) -> u64 {
    let mut used = 0_u64;
    let mut operations = 0_u64;
    let size_count = u64::try_from(plan.transfer_sizes.len()).unwrap_or(1).max(1);
    for index in 0..plan.target_operations {
        let size_index = usize::try_from(index % size_count).unwrap_or(0);
        let size = plan
            .transfer_sizes
            .get(size_index)
            .and_then(|size| u64::try_from(*size).ok())
            .unwrap_or(u64::MAX);
        let Some(next) = used.checked_add(size) else {
            break;
        };
        if next > budget {
            break;
        }
        used = next;
        operations += 1;
    }
    operations
}

fn mib(value: u64) -> Result<u64, Box<dyn std::error::Error>> {
    value
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "MiB value overflow".into())
}

/// Limits concurrent SSD coordinators independently from their per-device CPU pools.
fn device_worker_limit(requested: usize, devices: usize) -> usize {
    let devices = devices.max(1);
    let logical = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    if requested == 0 {
        logical.min(devices)
    } else {
        requested.min(devices).max(1)
    }
}

/// Leaves one logical CPU for Linux/UI and splits the rest across active SSDs.
fn automatic_compute_workers(parallel_devices: usize) -> usize {
    let logical = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    logical
        .saturating_sub(1)
        .max(1)
        .checked_div(parallel_devices.max(1))
        .unwrap_or(1)
        .clamp(1, 64)
}

fn inventory_command(args: &InventoryArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let inventory = scan(&scan_options(&args.flags))?;
    if args.json {
        serde_json::to_writer_pretty(io::stdout().lock(), &inventory)?;
        println!();
    } else {
        print_inventory(&inventory);
    }
    Ok(ExitCode::SUCCESS)
}

fn select_command(args: &SelectArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let inventory = scan(&scan_options(&args.flags))?;
    if inventory.devices.is_empty() {
        return Err("no NVMe namespace was discovered".into());
    }
    if !args.json {
        if args.devices.is_none() && io::stdin().is_terminal() {
            println!(
                "{}",
                render_picker(
                    &picker_entries(&inventory),
                    &std::collections::BTreeSet::new(),
                    TerminalSize::current()
                )
            );
        } else {
            print_inventory(&inventory);
        }
    }
    let request = match args.devices.as_deref() {
        Some(request) => request.to_owned(),
        None if args.json || !io::stdin().is_terminal() => {
            return Err("--devices is required when stdin is not interactive".into());
        }
        None => {
            print!("Select device indexes (example 1,2) or A for all: ");
            io::stdout().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            input.trim().to_owned()
        }
    };
    let selection = parse_selection(&inventory, &request)?;
    if let Some(path) = args.output.as_deref() {
        write_selection_file(path, &selection)?;
    }
    if args.json {
        serde_json::to_writer_pretty(io::stdout().lock(), &selection)?;
        println!();
    } else {
        print_selection(&selection);
        if let Some(path) = args.output.as_deref() {
            println!("Selection plan: {}", path.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn picker_entries(inventory: &Inventory) -> Vec<PickerEntry> {
    inventory
        .devices
        .iter()
        .map(|device| {
            let width = device
                .current_link_width
                .map_or_else(|| "x?".to_owned(), |value| format!("x{value}"));
            let speed = device.current_link_speed.as_deref().unwrap_or("link N/A");
            PickerEntry {
                alias: device.index,
                device_node: device.device_node.display().to_string(),
                model: device.model.clone(),
                capacity: human_bytes(device.capacity_bytes),
                serial: device.serial.clone(),
                driver: device.driver.clone(),
                link: format!("{speed} {width}"),
                safety: if device.safety.write_candidate {
                    "preserving/raw-scratch candidate".to_owned()
                } else {
                    "data-preserving file only".to_owned()
                },
            }
        })
        .collect()
}

/// Scans the `NVMe` disks after releasing the test mounts a dead test left behind (a crashed
/// layer or a failed cleanup keeps its volume mounted under `/run`, which blocks the next test
/// and FORMAT SSD alike). What was released is printed.
fn scan_releasing_stale(flags: &InventoryFlags) -> Result<Inventory, Box<dyn std::error::Error>> {
    let inventory = scan(&scan_options(flags))?;
    let released: Vec<String> = inventory
        .devices
        .iter()
        .flat_map(auto_storage::release_stale_mounts)
        .collect();
    if released.is_empty() {
        return Ok(inventory);
    }
    for line in &released {
        println!("Left by an interrupted test: {line}");
    }
    Ok(scan(&scan_options(flags))?)
}

fn scan_options(flags: &InventoryFlags) -> ScanOptions {
    ScanOptions {
        collect_smart: !flags.no_smart,
        protected_devices: flags.protected_devices.clone(),
        ..ScanOptions::default()
    }
}

fn parse_selection(
    inventory: &Inventory,
    request: &str,
) -> Result<SelectionPlan, Box<dyn std::error::Error>> {
    if request.eq_ignore_ascii_case("all") || request.eq_ignore_ascii_case("a") {
        return Ok(select_all(inventory)?);
    }
    let indexes = request
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(select_indexes(inventory, &indexes)?)
}

fn print_inventory(inventory: &Inventory) {
    println!("DIT CPU-NVMe // READ-ONLY DEVICE INVENTORY");
    println!("Detected namespaces: {}", inventory.devices.len());
    for warning in &inventory.warnings {
        println!("WARNING: {warning}");
    }
    if inventory.devices.is_empty() {
        println!("No NVMe namespace detected.");
        return;
    }
    for device in &inventory.devices {
        print_device(device);
    }
    println!("Aliases [1], [2], ... apply only to this screen; plans store stable IDs.");
}

fn print_device(device: &NvmeDevice) {
    let pcie = device.pci_bdf.as_deref().unwrap_or("N/A");
    let root_port = device.root_port_bdf.as_deref().unwrap_or("N/A");
    let link_speed = device.current_link_speed.as_deref().unwrap_or("N/A");
    let link_width = device
        .current_link_width
        .map_or_else(|| "N/A".to_owned(), |width| format!("x{width}"));
    let safety = if device.safety.write_candidate {
        "WRITE CANDIDATE (still requires TASK-C confirmation)".to_owned()
    } else {
        format!(
            "PROTECTED / READ-ONLY: {}",
            device.safety.reasons.join("; ")
        )
    };
    let smart = if device.smart.available {
        format!(
            "temp={}C used={}% media_errors={} error_log={}",
            optional_i64(device.smart.temperature_celsius),
            optional_u64(device.smart.percentage_used),
            optional_u64(device.smart.media_and_data_integrity_errors),
            optional_u64(device.smart.error_log_entries)
        )
    } else {
        device
            .smart
            .warning
            .clone()
            .unwrap_or_else(|| "N/A".to_owned())
    };
    println!();
    println!(
        "[{}] {}  {}  {}",
        device.index,
        device.device_node.display(),
        device.model,
        human_bytes(device.capacity_bytes)
    );
    println!(
        "    Serial: {} | Firmware: {} | Namespace: {}",
        device.serial, device.firmware, device.namespace
    );
    println!(
        "    Driver: {} | Controller: {} | Transport: {}",
        device.driver, device.controller, device.transport
    );
    println!("    PCIe: {pcie} via {root_port} | Link: {link_speed} {link_width}");
    println!(
        "    Blocks: logical {} B / physical {} B | SMART: {smart}",
        device.logical_block_size, device.physical_block_size
    );
    println!("    Safety: {safety}");
    println!("    Stable ID: {}", device.stable_id);
    for partition in &device.partition_details {
        let partition_safety = if partition.write_candidate {
            "unmounted write candidate; explicit data-loss confirmation still required".to_owned()
        } else {
            format!("protected: {}", partition.reasons.join("; "))
        };
        println!(
            "    Partition: {} | {} | {}",
            partition.device_node.display(),
            human_bytes(partition.capacity_bytes),
            partition_safety
        );
        println!("      Target ID: {}", partition.stable_id);
    }
}

fn print_selection(selection: &SelectionPlan) {
    println!();
    println!("SELECTED DEVICES: {}", selection.devices.len());
    for selected in &selection.devices {
        let safety = if selected.write_candidate {
            "write candidate"
        } else {
            "protected/read-only"
        };
        println!(
            "  [{}] {} | {} | serial={} | driver={} | {}",
            selected.display_index,
            selected.device_node.display(),
            selected.model,
            selected.serial,
            selected.driver,
            safety
        );
    }
    println!("This command only creates a selection plan; it performs no storage I/O.");
}

fn optional_u64(value: Option<u64>) -> String {
    value.map_or_else(|| "N/A".to_owned(), |value| value.to_string())
}

fn optional_i64(value: Option<i64>) -> String {
    value.map_or_else(|| "N/A".to_owned(), |value| value.to_string())
}

fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    let whole = bytes / GIB;
    let hundredths = (bytes % GIB).saturating_mul(100) / GIB;
    format!("{whole}.{hundredths:02} GiB")
}

fn write_selection_file(
    path: &Path,
    value: &SelectionPlan,
) -> Result<(), Box<dyn std::error::Error>> {
    write_json_file(path, value)
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = create_output(path)?;
    serde_json::to_writer_pretty(&mut writer, value)?;
    writeln!(writer)?;
    writer.flush()?;
    Ok(())
}

fn simulate(args: &SimulateArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let duration_seconds = args.duration_seconds.max(1);
    let operations = args
        .operations
        .unwrap_or_else(|| duration_seconds.saturating_mul(4))
        .max(1);
    let capacity_bytes = args
        .capacity_mib
        .checked_mul(1024 * 1024)
        .ok_or("capacity overflow")?;
    let temporary = args.file.is_none();
    let path = args.file.clone().unwrap_or_else(temporary_path);
    let mut backend = FileBackend::create(&path, capacity_bytes)?;
    let device_id = DeviceId(backend.id().to_owned());
    let plan =
        CoveragePlan::for_duration(Duration::from_secs(duration_seconds), operations, args.seed);
    let mut config = RunConfig::new(new_run_id(), device_id, plan);
    config.compute_workers = automatic_compute_workers(1);
    if args.operations.is_none() {
        config.wall_clock_limit = Some(Duration::from_secs(duration_seconds));
        config.sustain_until_deadline = true;
    }
    config.fault = args.inject.map(|kind| FaultPlan {
        kind: kind.into(),
        at_operation: args.inject_at,
    });

    let report = run_with_events(&mut backend, &config, args.events.as_deref())?;
    if let Some(path) = args.report.as_deref() {
        let mut writer = create_output(path)?;
        serde_json::to_writer_pretty(&mut writer, &report)?;
        writeln!(writer)?;
        writer.flush()?;
    }

    let (matrix_checks, write_operations, read_soak_operations) = deep_counts(&report);
    println!(
        "{} io={}/{} matrix_checks={} write_ops={} read_soak={} written={} verified={} failures={} seeds={} generations={} file={}",
        verdict_name(report.verdict),
        report.completed_operations,
        report.target_operations,
        matrix_checks,
        write_operations,
        read_soak_operations,
        report.bytes_written,
        report.bytes_verified,
        report.failure_count,
        report.seed_slots,
        report.generations,
        path.display()
    );
    drop(backend);
    if temporary && !args.keep_file {
        std::fs::remove_file(&path)?;
    }
    Ok(if report.verdict == Verdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn campaign(args: &CampaignArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let faults = [
        FaultKind::BitFlip,
        FaultKind::StaleGeneration,
        FaultKind::SwappedLba,
        FaultKind::DuplicateBlock,
        FaultKind::ShortRead,
        FaultKind::ShortWrite,
        FaultKind::IoError,
        FaultKind::Timeout,
        FaultKind::DeviceDisappeared,
        FaultKind::ProducerCompute,
        FaultKind::ReadCompute,
        FaultKind::ReferenceCompute,
        FaultKind::JoinMismatch,
    ];
    let plan = CoveragePlan::for_duration(Duration::from_secs(60), args.operations.max(4), 0x1234);
    let clean_config = RunConfig::new(
        RunId("campaign-clean".into()),
        DeviceId("memory:campaign".into()),
        plan.clone(),
    );
    let mut clean_backend = MemoryBackend::new("memory:campaign", 4 * 1024 * 1024);
    let clean = run(&mut clean_backend, &clean_config, |_| {})?;
    if clean.verdict != Verdict::Pass {
        return Err("positive campaign did not PASS".into());
    }
    println!("PASS positive-control");

    for (index, kind) in faults.into_iter().enumerate() {
        let mut config = RunConfig::new(
            RunId(format!("campaign-negative-{index}")),
            DeviceId("memory:campaign".into()),
            plan.clone(),
        );
        config.fault = Some(FaultPlan {
            kind,
            at_operation: 3,
        });
        let mut backend = MemoryBackend::new("memory:campaign", 4 * 1024 * 1024);
        let report = run(&mut backend, &config, |_| {})?;
        if report.verdict != Verdict::Fail || report.failures.is_empty() {
            return Err(format!("negative campaign missed {kind:?}").into());
        }
        println!("PASS detected {kind:?}");
    }

    let mut soak_config = RunConfig::new(
        RunId("campaign-negative-read-soak".into()),
        DeviceId("memory:campaign".into()),
        CoveragePlan::for_duration(Duration::from_secs(60), 1, 0x1234),
    );
    soak_config.matrix_rounds = 1;
    soak_config.wall_clock_limit = Some(Duration::from_millis(25));
    soak_config.sustain_until_deadline = true;
    soak_config.fault = Some(FaultPlan {
        kind: FaultKind::ReadSoakBitFlip,
        at_operation: 0,
    });
    let mut soak_backend = MemoryBackend::new("memory:campaign", 4 * 1024 * 1024);
    let soak = run(&mut soak_backend, &soak_config, |_| {})?;
    if soak.verdict != Verdict::Fail || soak.failures.is_empty() {
        return Err("negative campaign missed ReadSoakBitFlip".into());
    }
    println!("PASS detected ReadSoakBitFlip");
    println!(
        "PASS campaign: 1 positive + {} negative cases",
        faults.len() + 1
    );
    Ok(ExitCode::SUCCESS)
}

fn print_golden() {
    let block = build_block(&BlockContext {
        session_seed: 0x0123_4567_89ab_cdef,
        generation: 7,
        device_id: "file:golden-device".into(),
        lba: 42,
        stripe_id: 2,
        stripe_index: 10,
    });
    let digest = Sha256::digest(block);
    println!("{}", hex(&digest));
}

fn write_event(writer: &mut BufWriter<File>, event: &EventEnvelope) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, event).map_err(io::Error::other)?;
    writeln!(writer)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum JournalClass {
    Phase,
    Progress,
    Throughput,
    DeepProgress,
}

struct EventJournal {
    writer: Option<BufWriter<File>>,
    last_sample: BTreeMap<(Option<DeviceId>, JournalClass), Instant>,
}

impl EventJournal {
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

    fn new(path: Option<&Path>) -> io::Result<Self> {
        Ok(Self {
            writer: path.map(create_output).transpose()?,
            last_sample: BTreeMap::new(),
        })
    }

    fn record(&mut self, event: &EventEnvelope) -> io::Result<()> {
        let Some(writer) = self.writer.as_mut() else {
            return Ok(());
        };
        let (class, force) = match &event.event {
            Event::Hello { .. }
            | Event::Inventory { .. }
            | Event::Selection { .. }
            | Event::Failure { .. }
            | Event::Warning { .. }
            | Event::Completed { .. } => (None, true),
            Event::Phase { phase, .. } => (
                Some(JournalClass::Phase),
                matches!(phase, Phase::Finalizing),
            ),
            Event::Progress { failures, .. } => (Some(JournalClass::Progress), *failures > 0),
            Event::Throughput {
                current_read_bytes_per_sec,
                current_write_bytes_per_sec,
                ..
            } => (
                Some(JournalClass::Throughput),
                *current_read_bytes_per_sec == 0.0 && *current_write_bytes_per_sec == 0.0,
            ),
            Event::DeepProgress { stage, .. } => {
                (Some(JournalClass::DeepProgress), stage == "COMPLETE")
            }
        };
        if let Some(class) = class {
            let key = (event.device_id.clone(), class);
            if !force
                && self
                    .last_sample
                    .get(&key)
                    .is_some_and(|last| last.elapsed() < Self::SAMPLE_INTERVAL)
            {
                return Ok(());
            }
            self.last_sample.insert(key, Instant::now());
        }
        write_event(writer, event)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }
}

fn run_with_events<B: StorageBackend>(
    backend: &mut B,
    config: &RunConfig,
    events_path: Option<&Path>,
) -> Result<DeviceReport, Box<dyn std::error::Error>> {
    let mut event_journal = EventJournal::new(events_path)?;
    let mut event_error: Option<io::Error> = None;
    let report = run(backend, config, |event| {
        if event_error.is_none()
            && let Err(error) = event_journal.record(&event)
        {
            event_error = Some(error);
        }
    })?;
    if let Some(error) = event_error {
        return Err(error.into());
    }
    event_journal.flush()?;
    Ok(report)
}

fn create_output(path: &Path) -> io::Result<BufWriter<File>> {
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    Ok(BufWriter::new(file))
}

fn ensure_output_absent(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.symlink_metadata().is_ok() {
        return Err(format!(
            "output path already exists and will not be overwritten: {}",
            path.display()
        )
        .into());
    }
    Ok(())
}

fn temporary_path() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    std::env::temp_dir().join(format!(
        "cpu-nvme-dit-{}-{stamp}.dit-sim",
        std::process::id()
    ))
}

fn new_run_id() -> RunId {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    RunId(format!("sim-{stamp}-{}", std::process::id()))
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Pass => "PASS",
        Verdict::Fail => "FAIL",
        Verdict::Aborted => "ABORTED",
        Verdict::Invalid => "INVALID",
        Verdict::NotRun => "NOT_RUN",
    }
}

fn deep_counts(report: &DeviceReport) -> (u64, u64, u64) {
    report.deep.as_ref().map_or((0, 0, 0), |deep| {
        (
            deep.matrix_checks_completed,
            deep.write_operations_completed,
            deep.read_soak_operations,
        )
    })
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_test_directory_reports_path_without_creating_it() {
        let path = temporary_path();
        let error = resolve_test_directory(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()));
        assert!(error.contains("mounted filesystem"));
        assert!(!path.exists());
        assert!(resolve_test_directory(Path::new("relative/path")).is_err());
    }

    #[test]
    fn test_directory_rejects_files_and_resolves_protected_aliases() {
        let path = temporary_path();
        File::create_new(&path).unwrap();
        let error = resolve_test_directory(&path).unwrap_err().to_string();
        assert!(error.contains("not a directory"));
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("/", &path).unwrap();
        let resolved = resolve_test_directory(&path).unwrap();
        assert!(reject_critical_ui_directory(&resolved).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn operation_budget_never_exceeds_host_write_budget() {
        let plan = CoveragePlan::for_duration(Duration::from_mins(30), 10_000, 1);
        let budget = 2 * 1024 * 1024;
        let operations = operations_fitting_budget(&plan, budget);
        assert!(operations > 0);
        let used = (0..operations)
            .map(|index| plan.operation(index, 64 * 1024 * 1024).transfer_size as u64)
            .sum::<u64>();
        let next = plan.operation(operations, 64 * 1024 * 1024).transfer_size as u64;
        assert!(used <= budget);
        assert!(used + next > budget);
    }

    #[test]
    fn multi_target_mapping_requires_unique_aliases_and_preserves_path_text() {
        let values = vec!["1=/mnt/ssd-one".to_owned(), "2=/mnt/ssd=two".to_owned()];
        let targets = parse_target_map(&values).unwrap();
        assert_eq!(targets.get(&1).unwrap(), Path::new("/mnt/ssd-one"));
        assert_eq!(targets.get(&2).unwrap(), Path::new("/mnt/ssd=two"));
        assert!(parse_target_map(&["missing-separator".to_owned()]).is_err());
        assert!(parse_target_map(&["1=/mnt/a".to_owned(), "1=/mnt/b".to_owned()]).is_err());
    }

    #[test]
    fn automatic_worker_split_is_bounded_and_never_zero() {
        let logical = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        assert_eq!(device_worker_limit(1, 4), 1);
        assert_eq!(device_worker_limit(99, 2), 2);
        assert_eq!(device_worker_limit(0, 4), logical.min(4));
        for devices in [1, 2, 4, 64, usize::MAX] {
            assert!((1..=64).contains(&automatic_compute_workers(devices)));
        }
    }

    #[test]
    fn cpu_nvme_sensor_mapping_uses_cpu_ram_telemetry_units() {
        let mut snapshot = TelemetrySnapshot::default();
        snapshot.temperatures.push(dit_system::TemperatureReading {
            source: "/sys/class/hwmon/hwmon0/temp1_input".to_owned(),
            label: "k10temp:Tctl".to_owned(),
            millidegrees_c: 72_500,
            critical_millidegrees_c: Some(95_000),
        });
        snapshot.cpu_power.push(dit_system::PowerReading {
            source: "/sys/class/hwmon/hwmon1/power1_average".to_owned(),
            label: "amd_energy:package".to_owned(),
            milliwatts: 105_300,
            measurement: "hwmon_average".to_owned(),
            sample_interval_ms: None,
        });
        let metrics = cpu_sensor_metrics(&snapshot);
        assert_eq!(metrics.temperature_celsius, Some(72.5));
        assert_eq!(metrics.power_watts, Some(105.3));
    }

    #[test]
    fn event_journal_throttles_hot_progress_but_keeps_completion() {
        let path = temporary_path();
        let progress = |sequence| {
            EventEnvelope::new(
                RunId("journal-test".to_owned()),
                Some(DeviceId("nvme:test".to_owned())),
                sequence,
                sequence,
                sequence,
                Event::Progress {
                    completed_operations: sequence,
                    target_operations: 100,
                    bytes_written: sequence * 4096,
                    bytes_verified: sequence * 4096,
                    failures: 0,
                },
            )
        };
        let mut journal = EventJournal::new(Some(&path)).unwrap();
        journal.record(&progress(1)).unwrap();
        journal.record(&progress(2)).unwrap();
        journal
            .record(&EventEnvelope::new(
                RunId("journal-test".to_owned()),
                Some(DeviceId("nvme:test".to_owned())),
                3,
                3,
                3,
                Event::Completed {
                    verdict: Verdict::Pass,
                    completed_operations: 2,
                    failures: 0,
                },
            ))
            .unwrap();
        journal.flush().unwrap();
        drop(journal);
        let lines = std::fs::read_to_string(&path).unwrap();
        assert_eq!(lines.lines().count(), 2);
        assert!(lines.contains("\"type\":\"progress\""));
        assert!(lines.contains("\"type\":\"completed\""));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn interactive_duration_and_write_level_prompts() {
        let cli = Cli::parse_from(["cpu-nvme-dit", "interactive", "--output-dir", "/tmp/x"]);
        let Command::Interactive(args) = cli.command else {
            unreachable!("interactive parsed");
        };
        assert_eq!(parse_ssd_minutes("\n", 30), 30);
        assert_eq!(parse_ssd_minutes("1", 30), 10);
        assert_eq!(parse_ssd_minutes("3", 30), 60);
        assert_eq!(parse_ssd_minutes("90", 30), 90);
        assert_eq!(
            parse_ssd_minutes("5000", 30),
            30,
            "out of range keeps the default"
        );
        assert_eq!(parse_write_level(""), WriteLevel::Light);
        assert_eq!(parse_write_level("S"), WriteLevel::Standard);
        assert_eq!(parse_write_level("h"), WriteLevel::Heavy);
        // Light is exactly the long-standing default; heavier levels write a file as large as
        // their budget.
        assert_eq!(
            (
                WriteLevel::Light.capacity_mib(&args),
                WriteLevel::Light.budget_mib(&args)
            ),
            (args.capacity_mib, args.write_budget_mib)
        );
        assert_eq!(
            (
                WriteLevel::Heavy.capacity_mib(&args),
                WriteLevel::Heavy.budget_mib(&args)
            ),
            (65_536, 65_536)
        );
    }

    #[test]
    fn interactive_cpu_ssd_does_not_require_ram_preflight() {
        let cli = Cli::try_parse_from([
            "cpu-nvme-dit",
            "interactive",
            "--output-dir",
            "/tmp/new-cpu-ssd-run",
        ])
        .unwrap();
        let Command::Interactive(args) = cli.command else {
            panic!("expected interactive command");
        };
        assert!(args.cpu_ram_preflight.is_none());
        assert_eq!(args.duration_seconds, 1800);
        assert_eq!(attribution_name(None), "CPU_SSD_PATH_UNATTRIBUTED");
    }
}
