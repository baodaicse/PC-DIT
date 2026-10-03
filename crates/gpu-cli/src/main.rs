use clap::{Parser, Subcommand, ValueEnum};
use dit_contracts::{SCHEMA_VERSION, Verdict};
use dit_system::{
    GpuDevice, GpuTelemetry, LiveSensorSampler, TelemetrySnapshot, collect_gpu_inventory,
    collect_gpu_telemetry, format_gpu_summary,
};
use dit_ui_core::{
    DashboardMeter, HostSampler, InPlaceRenderer, LiveUtilization, SystemInfo, TerminalSize,
    ThrottleLive, UnifiedDashboard, render_unified_dashboard,
};
use gpu_engine::{
    FaultKind, MODULE_ID, run_generation,
    schedule::{CoveragePlan, CoverageScheduler},
    verifier::{HashPolicy, PatternClass, TRANSFER_PATTERN_VERSION, pattern_class},
};
use gpu_vulkan::{
    AllocationEvidence, ComputeEvidence, ComputeProfile, TransferEvidence, VulkanBackend,
    VulkanDeviceInfo,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{OpenOptions, create_dir_all};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_DURATION_SECONDS: u64 = 1_800;
const DEFAULT_BLOCKS_PER_GENERATION: usize = 32;
const DEFAULT_MATRIX_JOBS: usize = 64;
/// Jobs per batch for `--async-auto` on CUDA when `--matrix-jobs` is not given: the shape every
/// GPU-H async measurement used (H2-H6 evidence, K=1024 jobs 512), since the sequential default's
/// 64 jobs leave an async burst mostly launch overhead.
const ASYNC_AUTO_MATRIX_JOBS: usize = 512;
const DEFAULT_COMPUTE_ROUNDS: u32 = 256;
// Below this duration, the ramp would not amortize; keep the baseline so
// short smoke runs stay fast and comparable (tiered gate, like
// `mandatory_recipe_applicable` gating on total bytes rather than duration).
const COMPUTE_ROUNDS_RAMP_THRESHOLD_SECONDS: u64 = 60;
// Capped-linear ramp beyond the threshold, mirroring `CoveragePlan`'s
// `duration_seconds.div_ceil(180).clamp(1, 20)` seed-slot scaling.
const COMPUTE_ROUNDS_PER_SECOND: u32 = 1;
const MAX_AUTO_COMPUTE_ROUNDS: u32 = 1024;
const DEFAULT_SEED: u64 = 0x4750_552D_4449_542D;
const SEED_QUANTUM_SECONDS: u64 = 180;
const MIB: u64 = 1024 * 1024;
const CHUNK_SIZES: &[u64] = &[
    1024 * MIB,
    256 * MIB,
    512 * MIB,
    64 * MIB,
    32 * MIB,
    12 * MIB,
    4 * MIB,
    MIB,
    256 * 1024,
    128 * 1024,
    64 * 1024,
];
const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";
static CANCELLED: AtomicBool = AtomicBool::new(false);
static CANCEL_HANDLER: OnceLock<Result<(), String>> = OnceLock::new();

mod backend;
mod hwhealth;
mod multi;
use backend::Backend;

#[derive(Debug, Parser)]
#[command(
    name = "gpu-vram-dit",
    version,
    about = "DIT GPU-VRAM: CUDA/Vulkan transfer, VRAM coverage and verified matrix compute"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once at startup; boxing buys nothing
enum Command {
    /// Run GPU-B/C/D. Auto selects CUDA on NVIDIA, Vulkan on Intel/AMD.
    Run(RunArgs),
    /// Print PCI/sensor and CUDA/Vulkan device inventory as JSON.
    Inventory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum BackendArg {
    Auto,
    Cuda,
    Vulkan,
    Mock,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FaultArg {
    PayloadBitFlip,
    WrongPattern,
    StaleGeneration,
    WrongBlockIndex,
    MissingBlock,
    DuplicateBlock,
    MatrixOutput,
}

/// Arithmetic of the GEMM corpora (async CUDA): the integer ALU, the FP32 FMA pipes, or both in
/// turn, switching at each corpus rotation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ComputeMixArg {
    U32,
    Fp32,
    /// INT8 tensor cores only (compute capability 8.0+).
    Int8,
    Alternate,
    /// u32, FP32 and INT8 tensor in turn (INT8 skipped without tensor cores).
    All,
}

impl From<ComputeMixArg> for gpu_cuda::ArithMix {
    fn from(mix: ComputeMixArg) -> Self {
        match mix {
            ComputeMixArg::U32 => Self::U32,
            ComputeMixArg::Fp32 => Self::F32,
            ComputeMixArg::Int8 => Self::I8,
            ComputeMixArg::Alternate => Self::Alternate,
            ComputeMixArg::All => Self::All,
        }
    }
}

/// Which workloads a run exercises. `combined` is the default; the others isolate one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ProfileArg {
    Combined,
    ComputeOnly,
    TransferOnly,
}

/// GPU-H6 options: transfer lane, GPU-local VRAM workload and workload profile (async only).
#[derive(clap::Args, Clone, Debug, Default)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches, like RunArgs
struct LaneArgs {
    /// Run transfer and retention on their own streams while compute runs (CUDA), instead of the
    /// transfer pairs between bursts and the time-slice coverage floor. This is now the default
    /// for `--async-compute` (measured on the GTX 960: ~98% of compute-only throughput and ~2.4x
    /// the coverage of the old time-slice default); the flag stays for scripts that want it
    /// explicit. Passing `--coverage-floor` opts back into the time-slice default instead.
    #[arg(long)]
    transfer_overlap: bool,
    /// Force the pre-H6 default: no overlapped lane, transfer/retention time-sliced between
    /// compute bursts at `--coverage-floor` (0.25 unless given). Contradicts `--transfer-overlap`.
    #[arg(long)]
    no_transfer_overlap: bool,
    /// Add a GPU-local VRAM workload: kernels write and check a stretch of the pool on the device
    /// (no `PCIe`), and the CPU verifies it at checkpoints.
    #[arg(long)]
    vram_local: bool,
    /// Which workloads to run: `combined` (default), `compute-only` or `transfer-only`, to place a
    /// bottleneck or measure how much they slow each other.
    #[arg(long, value_enum)]
    workload_profile: Option<ProfileArg>,
    /// Lane operations in flight at once (1..=4, default 2).
    #[arg(long)]
    lane_slots: Option<usize>,
    /// Largest lane transfer in MiB (1..=64, default 8).
    #[arg(long)]
    lane_chunk_mib: Option<u64>,
    /// Cap on lane `PCIe` traffic, both directions, MiB/s (default unlimited).
    #[arg(long)]
    lane_rate_mib_s: Option<u64>,
    /// Verifier CPU weight of lane checks against compute checks (weight 3), 1..=64, default 1.
    #[arg(long)]
    lane_weight: Option<u64>,
    /// Fill and check passes per GPU-local write (1..=64, default 4).
    #[arg(long)]
    vram_passes: Option<u32>,
    /// GPU-DRAM sweep: the device writes and checks the pool at DRAM bandwidth on its own stream
    /// (16-byte accesses, a new key each pass), sampled by CPU checkpoints. It trades halves of
    /// the pool with the overlapped transfer lane, so both still cover all of it. Counted as
    /// device-verified bytes, apart from CPU-verified ones. Needs the overlapped lane.
    #[arg(long, conflicts_with = "vram_local")]
    vram_sweep: bool,
    /// Share of device time the sweep may hold (0.05..=1.0, default 0.25): it runs at full DRAM
    /// bandwidth for that share and leaves the rest to the GEMM. 1.0 favours DRAM over compute.
    #[arg(long, requires = "vram_sweep")]
    vram_sweep_duty: Option<f64>,
    /// Share of the device's resident threads the sweep launches (0.05..=1.0, default 1.0).
    #[arg(long, requires = "vram_sweep")]
    vram_sweep_share: Option<f64>,
}

impl LaneArgs {
    fn profile(&self) -> ProfileArg {
        self.workload_profile.unwrap_or(ProfileArg::Combined)
    }
    fn any(&self) -> bool {
        self.transfer_overlap
            || self.no_transfer_overlap
            || self.vram_local
            || self.vram_sweep
            || self.workload_profile.is_some()
            || self.lane_tuning()
    }
    fn lane_tuning(&self) -> bool {
        self.lane_slots.is_some()
            || self.lane_chunk_mib.is_some()
            || self.lane_rate_mib_s.is_some()
            || self.lane_weight.is_some()
            || self.vram_passes.is_some()
    }
    /// Numeric tuning only; `enabled`/`vram_local` are decided with the run's other flags by
    /// [`lane_config`], since they depend on `--async-compute`, the profile and the coverage
    /// floor, not on `LaneArgs` alone.
    fn tuning(&self) -> gpu_engine::lane::LaneConfig {
        let mut config = gpu_engine::lane::LaneConfig::default();
        if let Some(n) = self.lane_slots {
            config.slots = n;
        }
        if let Some(n) = self.lane_chunk_mib {
            config.chunk_bytes = n.saturating_mul(MIB);
        }
        if let Some(n) = self.lane_rate_mib_s {
            config.rate_bytes_per_second = n.saturating_mul(MIB);
        }
        if let Some(n) = self.lane_weight {
            config.lane_weight = n;
        }
        if let Some(n) = self.vram_passes {
            config.vram_passes = n;
        }
        config
    }
    /// Passes the options on to a per-GPU worker process.
    fn forward(&self, command: &mut std::process::Command) {
        if self.transfer_overlap {
            command.arg("--transfer-overlap");
        }
        if self.no_transfer_overlap {
            command.arg("--no-transfer-overlap");
        }
        if self.vram_local {
            command.arg("--vram-local");
        }
        if self.vram_sweep {
            command.arg("--vram-sweep");
        }
        if let Some(duty) = self.vram_sweep_duty {
            command.arg("--vram-sweep-duty").arg(duty.to_string());
        }
        if let Some(share) = self.vram_sweep_share {
            command.arg("--vram-sweep-share").arg(share.to_string());
        }
        if let Some(profile) = self.workload_profile
            && let Some(value) = profile.to_possible_value()
        {
            command.arg("--workload-profile").arg(value.get_name());
        }
        for (flag, value) in [
            ("--lane-slots", self.lane_slots.map(|n| n as u64)),
            ("--lane-chunk-mib", self.lane_chunk_mib),
            ("--lane-rate-mib-s", self.lane_rate_mib_s),
            ("--lane-weight", self.lane_weight),
            ("--vram-passes", self.vram_passes.map(u64::from)),
        ] {
            if let Some(value) = value {
                command.arg(flag).arg(value.to_string());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum VulkanFaultArg {
    Transfer,
    Matrix,
}

impl From<FaultArg> for FaultKind {
    fn from(value: FaultArg) -> Self {
        match value {
            FaultArg::PayloadBitFlip => Self::PayloadBitFlip,
            FaultArg::WrongPattern => Self::WrongPattern,
            FaultArg::StaleGeneration => Self::StaleGeneration,
            FaultArg::WrongBlockIndex => Self::WrongBlockIndex,
            FaultArg::MissingBlock => Self::MissingBlock,
            FaultArg::DuplicateBlock => Self::DuplicateBlock,
            FaultArg::MatrixOutput => Self::MatrixOutput,
        }
    }
}

#[derive(Clone, Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
struct RunArgs {
    #[arg(long, default_value_t = DEFAULT_DURATION_SECONDS)]
    duration_seconds: u64,
    /// Finite development gate; duration is authoritative when omitted.
    #[arg(long)]
    generations: Option<u64>,
    #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
    backend: BackendArg,
    /// Selected backend inventory index. Omit to select the first eligible hardware GPU.
    #[arg(long)]
    device_index: Option<usize>,
    /// Comma-separated backend indices or `all`. Interactive boots show a picker when omitted.
    #[arg(long)]
    devices: Option<String>,
    /// Percentage of currently allocatable device budget. The default is the maximum safe target.
    #[arg(long, default_value_t = 95)]
    vram_percent: u8,
    /// Exact bounded allocation for development/hardware smoke.
    #[arg(long)]
    vram_mib: Option<u64>,
    /// Hold as much VRAM as the device has: once every other buffer exists, the test pool takes
    /// what the driver reports free minus `--vram-margin-mib`, instead of a percentage with a 5%
    /// reserve on top. Discrete GPUs on CUDA; integrated GPUs (system RAM) and Vulkan keep
    /// `--vram-percent`. What the boot menu runs.
    #[arg(long, conflicts_with = "vram_mib")]
    vram_max: bool,
    /// Device memory `--vram-max` leaves free for the driver and kernel launches (32..=4096, default 128).
    #[arg(long, requires = "vram_max")]
    vram_margin_mib: Option<u64>,
    #[arg(long, default_value_t = 1024)]
    max_allocation_mib: u64,
    /// GEMM jobs per dispatch/batch (1..65536). Default 64, or 512 on devices `--async-auto`
    /// runs asynchronously.
    #[arg(long)]
    matrix_jobs: Option<usize>,
    /// GEMM inner dimension K (1..4096); omit for duration-derived sizing.
    #[arg(long = "matrix-inner", visible_alias = "compute-rounds")]
    compute_rounds: Option<u32>,
    /// GPU-H CUDA compute bursts with bounded pinned slots and CPU verifier workers.
    #[arg(long)]
    async_compute: bool,
    /// Choose the pipeline per selected device: the GPU-H async pipeline (load controller,
    /// overlapped transfer lane and the async/lane options given) on devices that run on CUDA,
    /// and the sequential path elsewhere (Vulkan has no async adapter yet), where async-only
    /// options are dropped instead of rejected. This is what the boot menu runs.
    #[arg(long, conflicts_with = "async_compute")]
    async_auto: bool,
    /// SHA-256 of passing outputs: `every-batch`, `checkpoint` or `checkpoint:<n>`. Defaults to
    /// `checkpoint` with --async-compute, `every-batch` otherwise. CUDA only. The comparison is
    /// always full and failed outputs are always hashed.
    #[arg(long)]
    hash_policy: Option<String>,
    /// Async only: rotate to a freshly derived corpus after this many batches (0 = never).
    #[arg(long)]
    corpus_rotate_batches: Option<u64>,
    /// Async only: GEMM arithmetic -- `u32` (default; integer ALU, modulo 2^32), `fp32` (IEEE
    /// single precision, one fused multiply-add per step, bit-exact against the CPU oracle),
    /// `int8` (tensor cores, exact 32-bit accumulation; compute capability 8.0+, inner dimension
    /// divisible by 32), `alternate` (u32 and fp32 in turn at each corpus rotation) or `all`
    /// (u32, fp32 and int8 in turn; int8 skipped without tensor cores).
    #[arg(long, value_enum)]
    compute_mix: Option<ComputeMixArg>,
    /// Async only: batches allowed on the GPU at once (default 3). The plan may lower it.
    #[arg(long)]
    async_device_slots: Option<usize>,
    /// Async only: pinned readback slots, i.e. the host queue (default 8; must exceed the device
    /// slots). Under memory pressure this is shed first, before device slots or jobs.
    #[arg(long)]
    async_host_slots: Option<usize>,
    /// Async only: verifier worker ceiling (default 2). The CPU share may lower it.
    #[arg(long)]
    async_workers: Option<usize>,
    /// Async only: ceiling for verifier workers when the scaler may add some (default 4, capped by
    /// the CPU share). `--async-workers` alone fixes the count.
    #[arg(long)]
    async_workers_max: Option<usize>,
    /// Async only: largest compute burst in batches (default 32).
    #[arg(long)]
    async_batches: Option<u64>,
    /// Async only: minimum fraction (0..=0.9) of run time given to transfer/retention
    /// verification. The scheduler shortens compute bursts (and adds transfer pairs) to hold it.
    /// Giving this also opts out of the default overlapped lane (H6), since the two schedule
    /// transfer/retention two different ways; pass `--transfer-overlap` explicitly to keep both
    /// (rejected unless the floor is 0).
    #[arg(long)]
    coverage_floor: Option<f64>,
    /// Async only: helper threads for corpus builds (default from the CPU share).
    #[arg(long)]
    corpus_threads: Option<usize>,
    /// Async only: failed batches held for confirmation at most (default 1).
    #[arg(long)]
    quarantine_slots: Option<usize>,
    /// Async only: append one JSON line per second to FILE classifying what is limiting compute
    /// throughput right now (verifier CPU, `PCIe` transfer, memory pressure, pacing, or none), from
    /// the same signals the load controller and coverage lane already compute. A rolled-up
    /// `bottleneck_summary` (seconds per cause) is always in the report regardless of this flag.
    #[arg(long)]
    bottleneck_log: Option<PathBuf>,
    #[command(flatten)]
    lane: LaneArgs,
    /// Diagnostic A/B: recreate matrix buffers each dispatch (Vulkan also pipeline).
    #[arg(long)]
    recreate_compute_resources: bool,
    #[arg(long, default_value_t = DEFAULT_BLOCKS_PER_GENERATION)]
    blocks_per_generation: usize,
    #[arg(long, default_value_t = DEFAULT_SEED)]
    seed: u64,
    #[arg(long)]
    output_dir: Option<PathBuf>,
    #[arg(long)]
    no_ui: bool,
    /// Stop after the first integrity mismatch. Default continues and counts failures.
    #[arg(long)]
    stop_on_error: bool,
    /// Disabled by default. Set a Celsius threshold to enable thermal abort.
    #[arg(long)]
    temp_limit_celsius: Option<f64>,
    /// GPU-A software-only negative injection; requires --backend mock.
    #[arg(long, value_enum, hide = true)]
    inject_fault: Option<FaultArg>,
    /// Hardware negative gate: CUDA kernel math or Vulkan readback corruption.
    #[arg(
        long = "inject-hardware-fault",
        alias = "inject-vulkan-fault",
        value_enum,
        hide = true
    )]
    inject_vulkan_fault: Option<VulkanFaultArg>,
    /// Internal multi-GPU worker heartbeat file.
    #[arg(long, hide = true)]
    progress_file: Option<PathBuf>,
    /// Appends one JSON line per failure record as soon as it exists (step F1 of the component
    /// fault ledger); with several GPUs each worker writes `device-N/fault-events.jsonl`.
    #[arg(long)]
    fault_events: Option<PathBuf>,
    /// Async CUDA only: GPU boundary checks (step F3 of the component fault ledger). The GPU
    /// hashes every corpus upload and checks its own output before the copy back, so a failure is
    /// placed on the `PCIe` link, the GPU, its VRAM or the host (`[boundary: …]` in its detail).
    #[arg(long)]
    boundary_checks: bool,
}

#[derive(Clone, Debug, Serialize)]
struct InventoryView {
    pci_devices: Vec<GpuDevice>,
    telemetry: Vec<GpuTelemetry>,
    vulkan_devices: Vec<VulkanDeviceInfo>,
    vulkan_error: Option<String>,
    cuda_devices: Vec<VulkanDeviceInfo>,
    cuda_error: Option<String>,
    auto_devices: Vec<VulkanDeviceInfo>,
    auto_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct FailureEvidence {
    generation: u64,
    seed: u64,
    phase: String,
    detail: String,
}

#[derive(Clone, Debug)]
struct RetentionRecord {
    allocation_index: usize,
    offset: u64,
    bytes: usize,
    seed: u64,
    generation: u64,
    ready_generation: u64,
}

/// Which workloads the run exercised and how transfers were scheduled (async runs).
#[derive(Clone, Debug, Serialize)]
struct WorkloadReport {
    profile: &'static str,
    /// `sequential` (pairs between bursts), `overlapped` (the lane) or `off`.
    transfer_mode: &'static str,
    vram_local: bool,
    compute: bool,
}

/// GPU-H7 (partial): seconds spent under each [`gpu_engine::bottleneck::BottleneckCause`],
/// sampled once a second from the load controller and coverage lane. `--bottleneck-log` also
/// writes one JSON line per sample for a full trace; this is the rolled-up total, always present.
#[derive(Clone, Debug, Default, Serialize)]
struct BottleneckSummary {
    samples: u64,
    seconds_memory_pressure: f64,
    seconds_stalled: f64,
    seconds_verifier_cpu_backlog: f64,
    seconds_verifier_worker_ceiling: f64,
    seconds_pcie_transfer: f64,
    seconds_pacing_active: f64,
    seconds_none: f64,
    last_cause: gpu_engine::bottleneck::BottleneckCause,
}

impl BottleneckSummary {
    fn record(&mut self, cause: gpu_engine::bottleneck::BottleneckCause, elapsed_seconds: f64) {
        use gpu_engine::bottleneck::BottleneckCause as C;
        self.samples += 1;
        self.last_cause = cause;
        let bucket = match cause {
            C::MemoryPressure => &mut self.seconds_memory_pressure,
            C::Stalled => &mut self.seconds_stalled,
            C::VerifierCpuBacklog => &mut self.seconds_verifier_cpu_backlog,
            C::VerifierWorkerCeiling => &mut self.seconds_verifier_worker_ceiling,
            C::PcieTransfer => &mut self.seconds_pcie_transfer,
            C::PacingActive => &mut self.seconds_pacing_active,
            C::None => &mut self.seconds_none,
        };
        *bucket += elapsed_seconds;
    }
}

/// One line of a `--bottleneck-log` trace.
#[derive(Clone, Debug, Serialize)]
struct BottleneckLogLine<'a> {
    elapsed_seconds: f64,
    cause: gpu_engine::bottleneck::BottleneckCause,
    detail: &'static str,
    regime: gpu_engine::accounting::Regime,
    controller_state: gpu_engine::control::ControlState,
    controller_reason: &'a str,
    pacing_ms: u64,
    in_flight_limit: usize,
    device_slots: usize,
    backlog_fill_peak: f64,
    workers: usize,
    workers_max: usize,
    memory_pressure_hits: u64,
    lane_fill: Option<f64>,
}

fn append_bottleneck_log(
    file: &mut std::fs::File,
    line: &BottleneckLogLine<'_>,
) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut *file, line)?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}

/// Samples the current async report for what is limiting compute throughput right now. `None`
/// when the report has nothing to classify from yet (before the first burst).
#[allow(clippy::cast_precision_loss)]
fn sample_bottleneck(
    report: &gpu_cuda::AsyncReport,
    elapsed_seconds: f64,
) -> Option<(
    gpu_engine::bottleneck::BottleneckCause,
    BottleneckLogLine<'_>,
)> {
    let regime = report.cumulative.last_regime?;
    let controller = &report.controller;
    // Peak-so-far, not live occupancy: the CLI only sees the cumulative report, not the lane's
    // live state, but a peak close to its slot count is still a useful "how saturated has the
    // lane gotten" signal.
    let lane_fill = report
        .lane
        .as_ref()
        .filter(|lane| lane.config.slots > 0)
        .map(|lane| lane.in_flight_peak as f64 / lane.config.slots as f64);
    let input = gpu_engine::bottleneck::BottleneckInput {
        regime,
        controller_state: controller.state,
        pacing_ms: controller.pacing_ms,
        workers: report.workers.current,
        workers_max: report.config.workers_max,
        // The controller's own current reason, not the cumulative hit count (which never
        // resets and would keep reporting pressure long after it cleared).
        memory_pressure: matches!(
            controller.reason,
            "host_memory_pressure" | "host_memory_pressure_sustained"
        ),
    };
    let cause = gpu_engine::bottleneck::classify(&input);
    let line = BottleneckLogLine {
        elapsed_seconds,
        cause,
        detail: cause.describe(),
        regime,
        controller_state: controller.state,
        controller_reason: controller.reason,
        pacing_ms: controller.pacing_ms,
        in_flight_limit: controller.in_flight_limit,
        device_slots: controller.config.device_slots,
        backlog_fill_peak: report.cumulative.backlog_fill_peak,
        workers: report.workers.current,
        workers_max: report.config.workers_max,
        memory_pressure_hits: report.memory_pressure_hits,
        lane_fill,
    };
    Some((cause, line))
}

#[derive(Clone, Debug, Serialize)]
struct GpuReport {
    workload: Option<WorkloadReport>,
    async_compute: Option<gpu_cuda::AsyncReport>,
    /// GPU-H1 detection scorecard; absent for mock/invalid runs and older reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    detection: Option<hwhealth::DetectionScorecard>,
    /// `clean`, `warnings`, `errors` or `unavailable`: `detection.hardware_health.status`, at the
    /// top so a reader need not know the scorecard layout. Absent without a hardware run.
    #[serde(skip_serializing_if = "Option::is_none")]
    hardware_status: Option<&'static str>,
    /// SHA-256 policy for passing outputs (comparison is always full); absent for mock runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    hash_policy: Option<String>,
    /// Async only: configured transfer/retention time floor against the share actually achieved.
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage_schedule: Option<gpu_engine::schedule::CoverageReport>,
    /// Async only: seconds spent under each bottleneck cause, sampled once a second.
    #[serde(skip_serializing_if = "Option::is_none")]
    bottleneck_summary: Option<BottleneckSummary>,
    /// CPU cores this GPU process may use for verifiers and corpus helpers (async runs).
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_share: Option<gpu_engine::plan::CpuShare>,
    workspace_budget: Option<gpu_vulkan::WorkspaceBudget>,
    campaign_profile: Option<gpu_vulkan::CampaignProfile>,
    schema_version: u32,
    module_id: String,
    run_id: String,
    verdict: Verdict,
    pipeline_version: String,
    backend: String,
    hardware_tested: bool,
    vram_hardware_test_active: bool,
    selected_device: Option<VulkanDeviceInfo>,
    shader_sha256: Option<String>,
    allocation: Option<AllocationEvidence>,
    allocation_target_percent: Option<u8>,
    /// How the pool was sized (`--vram-max` or percent/fixed); absent without a hardware run.
    #[serde(skip_serializing_if = "Option::is_none")]
    vram_fill: Option<VramFill>,
    duration_target_seconds: u64,
    active_elapsed_seconds: f64,
    base_seed: u64,
    seed_quantum_seconds: u64,
    seeds_started: u64,
    generations_completed: u64,
    transfer_operations_completed: u64,
    matrix_dispatches_completed: u64,
    matrix_dispatches_submitted: u64,
    matrix_dispatches_gpu_completed: u64,
    retention_checks_completed: u64,
    bytes_verified: u64,
    cumulative_transfer_bytes: u64,
    matrix_results_verified: u64,
    allocation_sweeps_completed: u64,
    last_transfer: Option<TransferEvidence>,
    last_compute: Option<ComputeEvidence>,
    last_compute_attempt: Option<ComputeProfile>,
    failure_count: u64,
    failures: Vec<FailureEvidence>,
    gpu_inventory: Vec<GpuDevice>,
    telemetry_start: Vec<GpuTelemetry>,
    telemetry_end: Vec<GpuTelemetry>,
    stop_on_error: bool,
    temp_limit_celsius: Option<f64>,
    completion_reason: String,
}

#[allow(clippy::struct_excessive_bools)] // independent display flags, not a state machine
struct GpuUiState {
    system: SystemInfo,
    live: LiveUtilization,
    devices: Vec<GpuDevice>,
    telemetry: Vec<GpuTelemetry>,
    duration_seconds: u64,
    exact_generations: Option<u64>,
    elapsed: Duration,
    generation: u64,
    seed_index: u64,
    seed: u64,
    backend: String,
    /// The GPU-H async pipeline drives this run (`--async-compute`, or `--async-auto` on CUDA).
    async_pipeline: bool,
    /// Hardware events graded for this GPU (corrected/uncorrected), when there are any.
    hardware_note: Option<String>,
    /// GPU-DRAM sweep progress (`--vram-sweep`), device-verified apart from CPU-verified.
    sweep_note: Option<String>,
    /// Verified rate per GEMM arithmetic (`--compute-mix`).
    mix_note: Option<String>,
    /// Live THROTTLE row of the selected GPU and its footer alert after a new slowdown.
    throttle_row: Option<String>,
    throttle_alert: Option<String>,
    hardware_active: bool,
    selected: Option<VulkanDeviceInfo>,
    allocation_bytes: u64,
    allocation_target_bytes: u64,
    chunk_bytes: u64,
    transfer_mib_s: Option<f64>,
    sweeps: u64,
    blocks_verified: u64,
    bytes_verified: u64,
    calculations_verified: u64,
    failures: u64,
    temp_limit: Option<f64>,
    aborted: bool,
    finished: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct GpuProgressSnapshot {
    device_index: usize,
    name: String,
    pci_address: Option<String>,
    uuid: String,
    state: String,
    elapsed_seconds: f64,
    duration_seconds: u64,
    generation: u64,
    seed_index: u64,
    allocation_bytes: u64,
    allocation_target_bytes: u64,
    chunk_bytes: u64,
    transfer_mib_s: Option<f64>,
    sweeps: u64,
    bytes_verified: u64,
    matrix_results_verified: u64,
    failures: u64,
    gpu_usage_percent: Option<f64>,
    vram_usage_percent: Option<f64>,
    vram_used_bytes: Option<u64>,
    vram_total_bytes: Option<u64>,
    temperature_celsius: Option<f64>,
    power_watts: Option<f64>,
    clock_mhz: Option<f64>,
    memory_clock_mhz: Option<f64>,
    telemetry_source: String,
    /// Hardware events graded for this GPU; `state` keeps its fixed values (`PASS` stays `PASS`),
    /// so a flagged PASS is read from here. Absent in progress files written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hardware_note: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
struct CpuSensorMetrics {
    temperature_celsius: Option<f64>,
    power_watts: Option<f64>,
}

struct GpuHostSampler {
    host: HostSampler,
    sensors: LiveSensorSampler,
    cached: CpuSensorMetrics,
    last_sensor_sample: Instant,
}

impl GpuHostSampler {
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

    fn system_info(&self) -> SystemInfo {
        self.host.system_info()
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
        Command::Inventory => inventory(),
        Command::Run(args) => multi::run_entry(&args),
    }
}

fn inventory() -> Result<ExitCode, Box<dyn std::error::Error>> {
    let (cuda_devices, cuda_error) = match Backend::inventory(BackendArg::Cuda) {
        Ok(devices) => (devices, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let (auto_devices, auto_error) = match Backend::inventory(BackendArg::Auto) {
        Ok(devices) => (devices, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let pci_devices = collect_gpu_inventory();
    let telemetry = collect_gpu_telemetry(&pci_devices);
    let (vulkan_devices, vulkan_error) = match VulkanBackend::inventory() {
        Ok(devices) => (devices, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    serde_json::to_writer_pretty(
        io::stdout().lock(),
        &InventoryView {
            pci_devices,
            telemetry,
            vulkan_devices,
            vulkan_error,
            cuda_devices,
            cuda_error,
            auto_devices,
            auto_error,
        },
    )?;
    println!();
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_lines)]
fn run_single(args: &RunArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    validate_args(args)?;
    let run_id = unique_id("gpu-dit");
    let output_dir = args
        .output_dir
        .clone()
        .unwrap_or_else(|| Path::new("/run/dit-suite").join(&run_id).join("gpu-vram"));
    create_dir_all(&output_dir)?;
    install_cancel_handler()?;
    let devices = collect_gpu_inventory();
    let telemetry_start = collect_gpu_telemetry(&devices);
    let mut telemetry = telemetry_start.clone();
    let mut host = GpuHostSampler::detect();
    let mut system = host.system_info();
    system.gpu = format_gpu_summary(&devices);
    let use_ui = !args.no_ui && io::stdout().is_terminal();
    let mut renderer = use_ui.then(|| InPlaceRenderer::new(io::stdout().lock(), 4));
    let started = Instant::now();
    let mut last_telemetry = Instant::now();
    let mut slowdown = dit_system::SlowdownCounter::default();
    let mut throttle = ThrottleLive::default();
    let mut last_bottleneck_sample = Instant::now();
    let mut last_log_poll = Instant::now();
    let mut hardware_note: Option<String> = None;
    let mut bottleneck_summary = BottleneckSummary::default();
    let mut bottleneck_log_file = args
        .bottleneck_log
        .as_deref()
        .map(|path| {
            OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)
        })
        .transpose()?;
    let mut generation = 0_u64;
    let mut seeds_started = 0_u64;
    let mut last_seed_index = None;
    let mut blocks_verified = 0_u64;
    let mut bytes_verified = 0_u64;
    let mut cumulative_transfer_bytes = 0_u64;
    let mut calculations_verified = 0_u64;
    let mut transfer_operations = 0_u64;
    let mut matrix_dispatches = 0_u64;
    let mut matrix_submitted = 0_u64;
    let mut matrix_gpu_completed = 0_u64;
    let mut retention_checks = 0_u64;
    let mut sweeps = 0_u64;
    let mut failures = Vec::new();
    let mut last_transfer = None;
    let mut last_compute = None;
    let mut last_compute_attempt = None;
    let mut retention_queue = VecDeque::<RetentionRecord>::new();
    let completion_reason: String;

    let mut vulkan = if args.backend == BackendArg::Mock {
        None
    } else {
        match Backend::open(args.backend, args.device_index) {
            Ok(mut value) => {
                value.set_compute_resource_reuse(!args.recreate_compute_resources);
                Some(value)
            }
            Err(error) => {
                let report = invalid_report(
                    &run_id,
                    args,
                    devices,
                    telemetry_start,
                    started.elapsed(),
                    "capability",
                    error,
                );
                let report_path = output_dir.join("report.json");
                write_new_json(&report_path, &report)?;
                println!("INVALID GPU capability; report={}", report_path.display());
                return Ok(ExitCode::from(2));
            }
        }
    };
    let selected = vulkan.as_ref().map(|backend| backend.info().clone());
    let mut allocation_target = vulkan
        .as_ref()
        .map_or(0, |backend| allocation_target_bytes(backend.info(), args));
    let workspace_result =
        (|| -> Result<Option<gpu_vulkan::WorkspaceBudget>, Box<dyn std::error::Error>> {
            if let Some(backend) = vulkan.as_mut() {
                backend.set_hash_policy(resolved_hash_policy(args)?)?;
                let info = backend.info();
                let device = info
                    .budget_bytes
                    .zip(info.usage_bytes)
                    .map_or(info.device_local_bytes, |(budget, usage)| {
                        budget.saturating_sub(usage)
                    });
                let memory = std::fs::read_to_string("/proc/meminfo")?;
                let host = memory
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("MemAvailable:")
                            .and_then(|s| s.split_whitespace().next())
                            .and_then(|s| s.parse::<u64>().ok())
                    })
                    .ok_or("MemAvailable unavailable for workspace budget")?
                    .saturating_mul(1024);
                let host = match std::env::var("DIT_GPU_HOST_BUDGET_BYTES") {
                    Ok(value) => host.min(value.parse::<u64>()?),
                    Err(std::env::VarError::NotPresent) => host,
                    Err(error) => return Err(error.into()),
                };
                let inner = resolved_compute_rounds(args);
                let plan = if args.async_compute {
                    let config = async_config(args);
                    let prefetch = config.rotate_every_batches > 0;
                    let mut plan = gpu_vulkan::WorkspaceBudget::plan_async(
                        device,
                        host,
                        allocation_target,
                        info.device_type == "INTEGRATED_GPU",
                        inner,
                        gpu_engine::plan::SlotPlan {
                            jobs: matrix_jobs(args),
                            device_slots: config.device_slots,
                            host_slots: config.host_slots,
                        },
                        prefetch,
                        config.lane.pinned_bytes(),
                    )?;
                    // The plan may have shed queue depth, overlap or jobs; configure what fits.
                    let (device_slots, host_slots) = (
                        plan.async_device_slots.unwrap_or(config.device_slots),
                        plan.async_host_slots.unwrap_or(config.host_slots),
                    );
                    backend.configure_async(gpu_cuda::AsyncConfig {
                        device_slots,
                        host_slots,
                        workers: config.workers.min(host_slots),
                        quarantine_slots: config.quarantine_slots.min(host_slots - 1),
                        ..config
                    })?;
                    let (jobs, backoffs) =
                        backend.prepare_matrix_resources(plan.matrix_jobs_effective, inner)?;
                    // Allocation backoff may go further than the estimate; report what exists.
                    let effective = backend.async_effective_config().unwrap_or(config);
                    let (matrix_device, matrix_host, pinned, prefetch_bytes) =
                        gpu_vulkan::WorkspaceBudget::async_footprint(
                            inner,
                            gpu_engine::plan::SlotPlan {
                                jobs,
                                device_slots: effective.device_slots,
                                host_slots: effective.host_slots,
                            },
                            prefetch,
                        );
                    plan.matrix_jobs_effective = jobs;
                    plan.async_device_slots = Some(effective.device_slots);
                    plan.async_host_slots = Some(effective.host_slots);
                    plan.matrix_device_bytes = matrix_device;
                    plan.matrix_host_bytes = matrix_host + effective.lane.pinned_bytes();
                    plan.pinned_queue_bytes = pinned;
                    plan.corpus_prefetch_bytes = prefetch_bytes;
                    plan.lane_pinned_bytes = effective.lane.pinned_bytes();
                    plan.backoff_steps += backoffs;
                    plan
                } else {
                    let mut plan = gpu_vulkan::WorkspaceBudget::plan_with_slots(
                        device,
                        host,
                        allocation_target,
                        matrix_jobs(args),
                        info.device_type == "INTEGRATED_GPU",
                        inner,
                        1,
                    )?;
                    let (jobs, backoffs) =
                        backend.prepare_matrix_resources(plan.matrix_jobs_effective, inner)?;
                    plan.matrix_jobs_effective = jobs;
                    let (input, output, host_workspace) =
                        gpu_engine::gemm::workspace_per_job(inner);
                    plan.matrix_device_bytes = jobs as u64 * (input + output);
                    plan.matrix_host_bytes = jobs as u64 * host_workspace;
                    plan.backoff_steps += backoffs;
                    plan
                };
                allocation_target = plan.pool_bytes;
                Ok(Some(plan))
            } else {
                Ok(None)
            }
        })();
    let mut workspace_budget = match workspace_result {
        Ok(plan) => plan,
        Err(error) => {
            let report = invalid_report(
                &run_id,
                args,
                devices,
                telemetry_start,
                started.elapsed(),
                "workspace_budget",
                error.to_string(),
            );
            let report_path = output_dir.join("report.json");
            write_new_json(&report_path, &report)?;
            println!("INVALID GPU workspace; report={}", report_path.display());
            return Ok(ExitCode::from(2));
        }
    };
    let vram_fill = vulkan.as_ref().map(|backend| {
        let fill = vram_fill_plan(
            args,
            backend.info(),
            backend.free_device_bytes(),
            allocation_target,
        );
        allocation_target = fill.target_bytes;
        fill
    });
    let allocation = if let Some(backend) = vulkan.as_mut() {
        match backend.allocate_pool(allocation_target, args.max_allocation_mib * MIB) {
            Ok(value) => Some(value),
            Err(error) => {
                let report = invalid_report(
                    &run_id,
                    args,
                    devices,
                    telemetry_start,
                    started.elapsed(),
                    "allocation",
                    error,
                );
                let report_path = output_dir.join("report.json");
                write_new_json(&report_path, &report)?;
                println!("INVALID GPU allocation; report={}", report_path.display());
                return Ok(ExitCode::from(2));
            }
        }
    } else {
        None
    };
    let allocation_sizes = vulkan
        .as_ref()
        .map(Backend::allocation_sizes)
        .unwrap_or_default();
    let mut allocation_offsets = vec![0_u64; allocation_sizes.len()];
    let mut allocation_passes = vec![0_u64; allocation_sizes.len()];
    let mut cancelled = false;
    let selected_pci = selected.as_ref().and_then(|i| i.pci_address.clone());
    let selected_nvidia = selected.as_ref().is_some_and(|i| i.vendor_id == 0x10de);
    let mut instruments = vulkan.is_some().then(|| {
        (
            hwhealth::LinkHealth::begin(selected_pci.as_deref()),
            hwhealth::KernelLogWindow::begin(),
            hwhealth::EccWindow::begin(selected_pci.as_deref(), selected_nvidia),
            hwhealth::StressSampler::start(
                devices.clone(),
                selected_pci.clone(),
                Duration::from_secs(1),
            ),
        )
    });
    let mut first_failure: Option<hwhealth::FirstFailure> = None;
    // Step F1: every failure record is also appended to `--fault-events` as soon as it exists.
    let fault_writer = dit_contracts::FaultWriter::new(args.fault_events.as_deref());
    let mut faults_emitted = 0_usize;
    // Transfer/retention operations per pattern class (schedule version 2), for the scorecard.
    let mut pattern_counts = [0_u64; 7];
    // Async only: guarantees transfer/retention a share of time (default off) and reports it.
    let profile = args.lane.profile();
    let lane_active = lane_active(args);
    let overlap = overlap_active(args);
    let compute_enabled = profile != ProfileArg::TransferOnly;
    // Transfer pairs between bursts, unless the lane does that work or the profile leaves it out.
    let sequential_transfers = profile != ProfileArg::ComputeOnly && !overlap;
    let mut coverage = (args.async_compute && profile == ProfileArg::Combined && !overlap)
        .then(|| CoverageScheduler::new(coverage_floor(args), async_config(args).batches))
        .transpose()?;
    let mut lane_now = LaneTotals::default();
    if overlap
        && args.inject_vulkan_fault == Some(VulkanFaultArg::Transfer)
        && let Some(backend) = vulkan.as_ref()
    {
        backend.inject_lane_fault(gpu_cuda::LaneFault::Transfer)?;
    }

    loop {
        if CANCELLED.load(Ordering::SeqCst) {
            completion_reason = "user_cancel".to_owned();
            cancelled = true;
            break;
        }
        if vulkan.as_ref().is_some_and(Backend::submission_stopped) {
            completion_reason = "backend_submission_failed".to_owned();
            break;
        }
        if should_stop(args, generation, started.elapsed()) {
            completion_reason = if args.generations.is_some() {
                "generation_target".to_owned()
            } else {
                "duration_reached".to_owned()
            };
            break;
        }
        let seed_index = if args.generations.is_some() {
            generation
        } else {
            started.elapsed().as_secs() / SEED_QUANTUM_SECONDS
        };
        if last_seed_index != Some(seed_index) {
            seeds_started = seeds_started.saturating_add(1);
            last_seed_index = Some(seed_index);
        }
        let seed = rotate_seed(args.seed, seed_index);
        let mut active_chunk = 0_u64;
        if let Some(backend) = vulkan.as_ref() {
            let plan = coverage.as_ref().map_or(
                CoveragePlan {
                    pairs: u32::from(sequential_transfers),
                    batches: 0,
                },
                CoverageScheduler::plan,
            );
            let pairs_started = Instant::now();
            for _pair in 0..plan.pairs {
                while retention_queue
                    .front()
                    .is_some_and(|record| record.ready_generation <= generation)
                {
                    let record = retention_queue.pop_front().expect("front was checked");
                    match backend.verify_retained(
                        record.allocation_index,
                        record.offset,
                        record.bytes,
                        record.seed,
                        record.generation,
                    ) {
                        Ok(evidence) => {
                            bytes_verified = bytes_verified.saturating_add(evidence.bytes_verified);
                            cumulative_transfer_bytes =
                                cumulative_transfer_bytes.saturating_add(evidence.bytes_verified);
                            retention_checks = retention_checks.saturating_add(1);
                            pattern_counts[pattern_class(record.generation).index()] += 1;
                        }
                        Err(error) => {
                            failures.push(failure(
                                record.generation,
                                record.seed,
                                "retention",
                                error,
                            ));
                        }
                    }
                }
                let allocation_index =
                    usize::try_from(generation).unwrap_or(0) % allocation_sizes.len();
                let offset = allocation_offsets[allocation_index];
                let remaining = allocation_sizes[allocation_index].saturating_sub(offset);
                let desired =
                    CHUNK_SIZES[usize::try_from(generation).unwrap_or(0) % CHUNK_SIZES.len()];
                active_chunk = desired
                    .min(remaining)
                    .min(
                        workspace_budget
                            .as_ref()
                            .map_or(u64::MAX, |p| p.transfer_chunk_bytes),
                    )
                    .max(1);
                let write_end = offset.saturating_add(active_chunk);
                let mut pending_index = 0_usize;
                while pending_index < retention_queue.len() {
                    let overlaps = retention_queue.get(pending_index).is_some_and(|record| {
                        record.allocation_index == allocation_index
                            && ranges_overlap(
                                offset,
                                write_end,
                                record.offset,
                                record.offset.saturating_add(
                                    u64::try_from(record.bytes).unwrap_or(u64::MAX),
                                ),
                            )
                    });
                    if !overlaps {
                        pending_index += 1;
                        continue;
                    }
                    let record = retention_queue
                        .remove(pending_index)
                        .expect("retention index was checked");
                    match backend.verify_retained(
                        record.allocation_index,
                        record.offset,
                        record.bytes,
                        record.seed,
                        record.generation,
                    ) {
                        Ok(evidence) => {
                            bytes_verified = bytes_verified.saturating_add(evidence.bytes_verified);
                            cumulative_transfer_bytes =
                                cumulative_transfer_bytes.saturating_add(evidence.bytes_verified);
                            retention_checks = retention_checks.saturating_add(1);
                            pattern_counts[pattern_class(record.generation).index()] += 1;
                        }
                        Err(error) => failures.push(failure(
                            record.generation,
                            record.seed,
                            "retention-before-overwrite",
                            error,
                        )),
                    }
                }
                let chunk_usize =
                    usize::try_from(active_chunk).map_err(|_| "chunk is too large")?;
                let transfer_result = if generation == 0
                    && args.inject_vulkan_fault == Some(VulkanFaultArg::Transfer)
                {
                    backend.verify_transfer_injected(
                        allocation_index,
                        offset,
                        chunk_usize,
                        seed,
                        generation,
                    )
                } else {
                    backend.verify_transfer(allocation_index, offset, chunk_usize, seed, generation)
                };
                match transfer_result {
                    Ok(evidence) => {
                        allocation_offsets[allocation_index] = offset.saturating_add(active_chunk);
                        if allocation_offsets[allocation_index]
                            >= allocation_sizes[allocation_index]
                        {
                            allocation_offsets[allocation_index] = 0;
                            allocation_passes[allocation_index] =
                                allocation_passes[allocation_index].saturating_add(1);
                            sweeps = completed_pool_sweeps(&allocation_passes);
                        }
                        bytes_verified = bytes_verified.saturating_add(evidence.bytes_verified);
                        cumulative_transfer_bytes = cumulative_transfer_bytes
                            .saturating_add(evidence.bytes_verified.saturating_mul(2));
                        transfer_operations = transfer_operations.saturating_add(1);
                        pattern_counts[pattern_class(generation).index()] += 1;
                        retention_queue.push_back(RetentionRecord {
                            allocation_index,
                            offset,
                            bytes: usize::try_from(active_chunk).unwrap_or(usize::MAX),
                            seed,
                            generation,
                            ready_generation: generation.saturating_add(2),
                        });
                        last_transfer = Some(evidence);
                    }
                    Err(error) => failures.push(failure(generation, seed, "transfer", error)),
                }
            }
            if let Some(scheduler) = coverage.as_mut() {
                scheduler.observe_transfers(plan.pairs, pairs_started.elapsed().as_secs_f64());
            }
            if !compute_enabled
                && lane_active
                && let Err(error) = backend.run_lane_slice(
                    seed,
                    &|| {
                        CANCELLED.load(Ordering::Relaxed)
                            || should_stop(args, generation, started.elapsed())
                    },
                    Duration::from_millis(500),
                )
            {
                failures.push(failure(generation, seed, "transfer-lane", error));
            }
            if compute_enabled {
                let burst_started = Instant::now();
                let compute_result = if args.async_compute {
                    backend.verify_matrix_async(
                        seed,
                        generation == 0 && args.inject_vulkan_fault == Some(VulkanFaultArg::Matrix),
                        &|| {
                            CANCELLED.load(Ordering::Relaxed)
                                || should_stop(args, generation, started.elapsed())
                        },
                        // With a floor the scheduler sizes the burst; without one it stays at the maximum.
                        (coverage.is_some() && coverage_floor(args) > 0.0).then_some(plan.batches),
                    )
                } else if generation == 0
                    && args.inject_vulkan_fault == Some(VulkanFaultArg::Matrix)
                {
                    backend.verify_matrix_injected(
                        seed,
                        generation,
                        workspace_budget
                            .as_ref()
                            .map_or(matrix_jobs(args), |p| p.matrix_jobs_effective),
                        resolved_compute_rounds(args),
                    )
                } else {
                    backend.verify_matrix(
                        seed,
                        generation,
                        workspace_budget
                            .as_ref()
                            .map_or(matrix_jobs(args), |p| p.matrix_jobs_effective),
                        resolved_compute_rounds(args),
                    )
                };
                // An empty async burst reports no profile; keep the last real attempt.
                let attempt = backend.last_compute_profile();
                if let Some(profile) = &attempt {
                    matrix_submitted += profile.dispatches_submitted;
                    matrix_gpu_completed += profile.dispatches_completed;
                }
                if let Some(scheduler) = coverage.as_mut() {
                    scheduler.observe_burst(
                        attempt.as_ref().map_or(0, |p| p.dispatches_submitted),
                        burst_started.elapsed().as_secs_f64(),
                    );
                }
                if attempt.is_some() {
                    last_compute_attempt = attempt;
                }
                match compute_result {
                    Ok(evidence) => {
                        calculations_verified =
                            calculations_verified.saturating_add(evidence.results_verified);
                        cumulative_transfer_bytes = cumulative_transfer_bytes
                            .saturating_add(evidence.uploaded_bytes)
                            .saturating_add(evidence.downloaded_bytes);
                        matrix_dispatches = matrix_dispatches.saturating_add(
                            evidence
                                .profile
                                .as_ref()
                                .map_or(1, |p| p.dispatches_verified),
                        );
                        if evidence.results_verified > 0 {
                            last_compute = Some(evidence);
                        }
                    }
                    Err(error) => failures.push(failure(generation, seed, "compute", error)),
                }
            }
            if lane_active {
                collect_lane_failures(backend, &mut failures);
                lane_now = LaneTotals::from_report(backend.lane_report().as_ref());
            }
        } else {
            let fault = (generation == 0)
                .then_some(args.inject_fault)
                .flatten()
                .map(Into::into);
            match run_generation(seed, generation, args.blocks_per_generation, fault) {
                Ok(evidence) => {
                    blocks_verified = blocks_verified.saturating_add(evidence.blocks_verified);
                    bytes_verified = bytes_verified.saturating_add(evidence.bytes_verified);
                    calculations_verified =
                        calculations_verified.saturating_add(evidence.matrix_results_verified);
                }
                Err(error) => failures.push(FailureEvidence {
                    generation,
                    seed,
                    phase: "gpu-a-mock".to_owned(),
                    detail: error.to_string(),
                }),
            }
        }
        generation = generation.saturating_add(1);
        if let Some((link, window, _, _)) = instruments.as_mut() {
            link.sample();
            // Grade the kernel log while the run goes on, so a corrected or fatal hardware event
            // shows on the dashboard when it happens rather than only in the final report.
            if last_log_poll.elapsed() >= LOG_POLL_INTERVAL {
                last_log_poll = Instant::now();
                window.poll();
                let findings =
                    window.findings_so_far(selected_pci.as_deref(), link.upstream_port.as_deref());
                hardware_note = live_hardware_note(&findings);
            }
        }
        if first_failure.is_none() && !failures.is_empty() {
            first_failure = Some(hwhealth::FirstFailure {
                at_seconds: started.elapsed().as_secs_f64(),
                verified_results_before: calculations_verified,
            });
        }
        if last_telemetry.elapsed() >= Duration::from_secs(1) {
            telemetry = collect_gpu_telemetry(&devices);
            last_telemetry = Instant::now();
            track_throttle(
                &mut throttle,
                &mut slowdown,
                &telemetry,
                selected.as_ref().and_then(|i| i.pci_address.as_deref()),
                started.elapsed(),
            );
            if let Some(limit) = args.temp_limit_celsius
                && telemetry
                    .iter()
                    .filter_map(|sample| sample.temperature_celsius)
                    .any(|temperature| temperature >= limit)
            {
                completion_reason = "temperature_limit".to_owned();
                failures.push(FailureEvidence {
                    generation,
                    seed,
                    phase: "thermal_policy".to_owned(),
                    detail: format!("temperature reached configured limit {limit:.1} C"),
                });
                break;
            }
        }
        if args.async_compute && last_bottleneck_sample.elapsed() >= Duration::from_secs(1) {
            let elapsed = last_bottleneck_sample.elapsed().as_secs_f64();
            last_bottleneck_sample = Instant::now();
            if let Some(report) = vulkan.as_ref().and_then(Backend::async_report)
                && let Some((cause, line)) =
                    sample_bottleneck(&report, started.elapsed().as_secs_f64())
            {
                bottleneck_summary.record(cause, elapsed);
                if let Some(file) = bottleneck_log_file.as_mut() {
                    append_bottleneck_log(file, &line)?;
                }
            }
        }
        if renderer.is_some() || args.progress_file.is_some() {
            let mut state = ui_state(
                &system,
                host.sample(),
                &devices,
                &telemetry,
                args,
                started.elapsed(),
                generation,
                seed_index,
                seed,
                selected.clone(),
                allocation.as_ref().map_or(0, |item| item.allocated_bytes),
                allocation_target,
                active_chunk,
                last_transfer
                    .as_ref()
                    .map(|item| item.h2d_d2h_mib_per_second)
                    .or_else(|| {
                        (lane_now.transfer_bytes > 0)
                            .then(|| mib_per_second(lane_now.transfer_bytes, started.elapsed()))
                    }),
                sweeps.max(lane_now.sweeps),
                blocks_verified,
                bytes_verified + lane_now.bytes_verified,
                calculations_verified,
                failures.len(),
                false,
                false,
            );
            state.hardware_note.clone_from(&hardware_note);
            state.throttle_row = throttle.row();
            state.throttle_alert = throttle.alert().map(str::to_owned);
            state.sweep_note = vulkan
                .as_ref()
                .and_then(Backend::sweep_report)
                .map(|s| sweep_note(&s, started.elapsed()));
            state.mix_note = mix_note(args, vulkan.as_ref(), started.elapsed());
            if let Some(path) = args.progress_file.as_deref() {
                write_progress(path, &progress_snapshot(&state))?;
            }
            if let Some(renderer) = renderer.as_mut() {
                renderer.render_frame(&render_gpu_ui(&state, TerminalSize::current(), true))?;
            }
        }
        emit_faults(
            &fault_writer,
            &failures,
            &mut faults_emitted,
            selected_pci.as_deref(),
            vulkan.as_ref(),
        );
        if args.stop_on_error && !failures.is_empty() {
            completion_reason = "stop_on_error".to_owned();
            break;
        }
    }

    if lane_active && let Some(backend) = vulkan.as_ref() {
        // Every region still waiting for its re-read or checkpoint is verified before the report.
        if let Err(error) = backend.flush_lane() {
            failures.push(failure(generation, 0, "retention-final", error));
        }
        collect_lane_failures(backend, &mut failures);
        lane_now = LaneTotals::from_report(backend.lane_report().as_ref());
    }
    if let Some(backend) = vulkan.as_ref() {
        while let Some(record) = retention_queue.pop_front() {
            match backend.verify_retained(
                record.allocation_index,
                record.offset,
                record.bytes,
                record.seed,
                record.generation,
            ) {
                Ok(evidence) => {
                    bytes_verified = bytes_verified.saturating_add(evidence.bytes_verified);
                    cumulative_transfer_bytes =
                        cumulative_transfer_bytes.saturating_add(evidence.bytes_verified);
                    retention_checks = retention_checks.saturating_add(1);
                    pattern_counts[pattern_class(record.generation).index()] += 1;
                }
                Err(error) => failures.push(failure(
                    record.generation,
                    record.seed,
                    "retention-final",
                    error,
                )),
            }
        }
    }

    let elapsed = started.elapsed();
    let transfer_operations = transfer_operations + lane_now.transfer_operations;
    let retention_checks = retention_checks + lane_now.retention_checks;
    let bytes_verified = bytes_verified + lane_now.bytes_verified;
    let cumulative_transfer_bytes = cumulative_transfer_bytes + lane_now.transfer_bytes;
    let sweeps = sweeps.max(lane_now.sweeps);
    for (total, lane) in pattern_counts.iter_mut().zip(lane_now.pattern_counts) {
        *total += lane;
    }
    if first_failure.is_none() && !failures.is_empty() {
        first_failure = Some(hwhealth::FirstFailure {
            at_seconds: elapsed.as_secs_f64(),
            verified_results_before: calculations_verified,
        });
    }
    let detection = instruments.take().map(|(mut link, window, ecc, sampler)| {
        link.finish();
        let upstream = link.upstream_port.clone();
        let async_report = vulkan.as_ref().and_then(Backend::async_report);
        let campaign = vulkan.as_ref().map(Backend::campaign_profile);
        build_scorecard(&ScorecardInputs {
            elapsed_seconds: elapsed.as_secs_f64(),
            compute_inner: resolved_compute_rounds(args),
            results_verified: calculations_verified,
            transfer_bytes_verified: bytes_verified,
            transfer_operations,
            retention_checks,
            pattern_counts,
            sweeps,
            allocated_bytes: allocation.as_ref().map_or(0, |a| a.allocated_bytes),
            campaign_failed_batches: campaign.as_ref().map_or(0, |c| c.matrix.failed),
            compute_failure_records: failures.iter().filter(|f| f.phase == "compute").count()
                as u64,
            async_report: async_report.as_ref(),
            first_failure: first_failure.clone(),
            link,
            kernel_log: window.finish(selected_pci.as_deref(), upstream.as_deref()),
            ecc: ecc.finish(),
            stress: sampler.finish(),
        })
    });
    let hardware_health = detection.as_ref().map(|d| d.hardware_health.clone());
    if let Some(health) = &hardware_health {
        // Uncorrectable or device-lost events fail the run as their own phase, apart from data
        // mismatches; corrected ones leave it PASS but flagged (status `warnings`).
        if health.status == hwhealth::HealthStatus::Errors {
            failures.push(failure(
                generation,
                rotate_seed(args.seed, last_seed_index.unwrap_or(0)),
                "hardware",
                health.reasons.join("; "),
            ));
        }
        hardware_note = health.summary();
    }
    emit_faults(
        &fault_writer,
        &failures,
        &mut faults_emitted,
        selected_pci.as_deref(),
        vulkan.as_ref(),
    );
    let telemetry_end = collect_gpu_telemetry(&devices);
    // A profile that leaves a workload out is judged on the workloads it runs.
    let hardware_tested = vulkan.is_some()
        && (transfer_operations > 0 || profile == ProfileArg::ComputeOnly)
        && (matrix_dispatches > 0 || !compute_enabled)
        && failures.is_empty();
    let verdict = final_verdict(cancelled, failures.len());
    if renderer.is_some() || args.progress_file.is_some() {
        let mut state = ui_state(
            &system,
            host.sample(),
            &devices,
            &telemetry_end,
            args,
            elapsed,
            generation,
            last_seed_index.unwrap_or(0),
            rotate_seed(args.seed, last_seed_index.unwrap_or(0)),
            selected.clone(),
            allocation.as_ref().map_or(0, |item| item.allocated_bytes),
            allocation_target,
            0,
            last_transfer
                .as_ref()
                .map(|item| item.h2d_d2h_mib_per_second),
            sweeps,
            blocks_verified,
            bytes_verified,
            calculations_verified,
            failures.len(),
            cancelled,
            true,
        );
        state.hardware_note.clone_from(&hardware_note);
        state.throttle_row = throttle.row();
        state.sweep_note = vulkan
            .as_ref()
            .and_then(Backend::sweep_report)
            .map(|s| sweep_note(&s, elapsed));
        state.mix_note = mix_note(args, vulkan.as_ref(), elapsed);
        if let Some(path) = args.progress_file.as_deref() {
            write_progress(path, &progress_snapshot(&state))?;
        }
        if let Some(renderer) = renderer.as_mut() {
            renderer.render_frame(&render_gpu_ui(&state, TerminalSize::current(), true))?;
            renderer.finish()?;
        }
    }
    let report = GpuReport {
        async_compute: vulkan.as_ref().and_then(Backend::async_report),
        detection,
        hardware_status: hardware_health.as_ref().map(|h| h.status.label()),
        hash_policy: vulkan.as_ref().map(Backend::hash_policy_label),
        cpu_share: args.async_compute.then(|| async_cpu_share(args)),
        coverage_schedule: coverage.as_ref().map(CoverageScheduler::report),
        bottleneck_summary: args.async_compute.then_some(bottleneck_summary),
        workload: args.async_compute.then(|| WorkloadReport {
            profile: match profile {
                ProfileArg::Combined => "combined",
                ProfileArg::ComputeOnly => "compute-only",
                ProfileArg::TransferOnly => "transfer-only",
            },
            transfer_mode: if profile == ProfileArg::ComputeOnly {
                "off"
            } else if overlap {
                "overlapped"
            } else {
                "sequential"
            },
            vram_local: lane_active && args.lane.vram_local,
            compute: compute_enabled,
        }),
        schema_version: SCHEMA_VERSION,
        module_id: MODULE_ID.to_owned(),
        run_id,
        verdict,
        pipeline_version: if args.async_compute {
            "GPU-H-CUDA-1"
        } else if vulkan.is_some() {
            "GPU-G3-1"
        } else {
            "GPU-A-1"
        }
        .to_owned(),
        backend: if let Some(backend) = vulkan.as_ref() {
            backend.label()
        } else {
            "CPU_MOCK_GPU_A"
        }
        .to_owned(),
        hardware_tested,
        vram_hardware_test_active: vulkan.is_some(),
        selected_device: selected,
        shader_sha256: vulkan
            .as_ref()
            .map(|backend| backend.kernel_hash().to_owned()),
        allocation,
        allocation_target_percent: vulkan
            .as_ref()
            .map(|backend| effective_vram_percent(backend.info(), args)),
        vram_fill,
        duration_target_seconds: args.duration_seconds,
        active_elapsed_seconds: elapsed.as_secs_f64(),
        base_seed: args.seed,
        seed_quantum_seconds: SEED_QUANTUM_SECONDS,
        seeds_started,
        generations_completed: generation,
        transfer_operations_completed: transfer_operations,
        matrix_dispatches_completed: matrix_dispatches,
        retention_checks_completed: retention_checks,
        bytes_verified,
        cumulative_transfer_bytes,
        matrix_results_verified: calculations_verified,
        allocation_sweeps_completed: sweeps,
        last_transfer,
        last_compute,
        last_compute_attempt,
        campaign_profile: vulkan.as_ref().map(Backend::campaign_profile),
        workspace_budget: workspace_budget.take(),
        matrix_dispatches_submitted: matrix_submitted,
        matrix_dispatches_gpu_completed: matrix_gpu_completed,
        failure_count: u64::try_from(failures.len()).unwrap_or(u64::MAX),
        failures,
        gpu_inventory: devices,
        telemetry_start,
        telemetry_end,
        stop_on_error: args.stop_on_error,
        temp_limit_celsius: args.temp_limit_celsius,
        completion_reason,
    };
    let report_path = output_dir.join("report.json");
    write_new_json(&report_path, &report)?;
    println!(
        "{} {} hardware_tested={} report={}",
        verdict_name(verdict),
        report.backend,
        hardware_tested,
        report_path.display()
    );
    Ok(match verdict {
        Verdict::Pass => ExitCode::SUCCESS,
        Verdict::Fail => ExitCode::from(1),
        _ => ExitCode::from(2),
    })
}

/// Tiered gate (below threshold: baseline) then a capped-linear ramp, so a
/// longer `--duration-seconds` amortizes a heavier per-dispatch workload
/// without an unbounded per-round cost regardless of how large duration is.
fn suggested_compute_rounds(duration_seconds: u64) -> u32 {
    if duration_seconds < COMPUTE_ROUNDS_RAMP_THRESHOLD_SECONDS {
        return DEFAULT_COMPUTE_ROUNDS;
    }
    let extra_seconds = duration_seconds - COMPUTE_ROUNDS_RAMP_THRESHOLD_SECONDS;
    let ramped = u64::from(DEFAULT_COMPUTE_ROUNDS)
        .saturating_add(extra_seconds.saturating_mul(u64::from(COMPUTE_ROUNDS_PER_SECOND)));
    u32::try_from(ramped)
        .unwrap_or(u32::MAX)
        .min(MAX_AUTO_COMPUTE_ROUNDS)
}

/// Explicit `--compute-rounds` always wins; otherwise derive from duration.
fn resolved_compute_rounds(args: &RunArgs) -> u32 {
    args.compute_rounds
        .unwrap_or_else(|| suggested_compute_rounds(args.duration_seconds))
}

/// Async defaults to checkpoint hashing; the sequential path keeps hashing every output so
/// its cross-backend gates stay comparable.
fn resolved_hash_policy(args: &RunArgs) -> Result<HashPolicy, String> {
    match args.hash_policy.as_deref() {
        Some(text) => HashPolicy::parse(text).ok_or_else(|| {
            "--hash-policy must be every-batch, checkpoint or checkpoint:<n>".into()
        }),
        None if args.async_compute => Ok(HashPolicy::Checkpoint {
            interval: HashPolicy::DEFAULT_INTERVAL,
        }),
        None => Ok(HashPolicy::EveryBatch),
    }
}

/// Peers running beside this GPU process (set by the multi-GPU parent), for the CPU share.
fn peer_gpu_count() -> usize {
    std::env::var("DIT_GPU_PEER_COUNT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1)
}

fn async_cpu_share(args: &RunArgs) -> gpu_engine::plan::CpuShare {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let requested = args
        .async_workers_max
        .or(args.async_workers)
        .unwrap_or(gpu_cuda::AsyncConfig::default().workers_max);
    gpu_engine::plan::cpu_share(cores, peer_gpu_count(), requested)
}

/// Requested async resources: CLI overrides on the defaults, with the CPU share as a ceiling on
/// verifier workers. Memory pressure may lower slots and jobs later, never raise anything.
fn matrix_jobs(args: &RunArgs) -> usize {
    args.matrix_jobs.unwrap_or(DEFAULT_MATRIX_JOBS)
}

/// `--async-auto` resolved for one selected device: async on CUDA (with the measured async shape
/// unless `--matrix-jobs` was given), sequential elsewhere with every async-only option dropped,
/// so a mixed NVIDIA + Intel/AMD machine runs each GPU on the best pipeline it has. Other runs
/// are returned unchanged.
fn pipeline_for_device(args: &RunArgs, device: Option<&VulkanDeviceInfo>) -> RunArgs {
    let mut resolved = args.clone();
    if !args.async_auto {
        return resolved;
    }
    resolved.async_auto = false;
    let cuda = device.is_some_and(|device| backend::runs_on_cuda(args.backend, device));
    if cuda {
        resolved.async_compute = true;
        resolved.matrix_jobs.get_or_insert(ASYNC_AUTO_MATRIX_JOBS);
    } else {
        resolved.hash_policy = None;
        resolved.corpus_rotate_batches = None;
        resolved.compute_mix = None;
        resolved.async_device_slots = None;
        resolved.async_host_slots = None;
        resolved.async_workers = None;
        resolved.async_workers_max = None;
        resolved.async_batches = None;
        resolved.coverage_floor = None;
        resolved.corpus_threads = None;
        resolved.quarantine_slots = None;
        resolved.bottleneck_log = None;
        resolved.boundary_checks = false;
        resolved.lane = LaneArgs::default();
    }
    resolved
}

fn async_config(args: &RunArgs) -> gpu_cuda::AsyncConfig {
    let mut config = gpu_cuda::AsyncConfig::default();
    if let Ok(policy) = resolved_hash_policy(args) {
        config.hash_policy = policy;
    }
    let share = async_cpu_share(args);
    // The share is the ceiling. An explicit `--async-workers` starts there and, without
    // `--async-workers-max`, stays there (no scaling).
    let initial = args
        .async_workers
        .unwrap_or(config.workers)
        .min(share.verifier_workers);
    config.workers = initial;
    config.workers_max = if args.async_workers.is_some() && args.async_workers_max.is_none() {
        initial
    } else {
        share.verifier_workers
    };
    config.corpus_threads = args.corpus_threads.unwrap_or(share.helper_threads);
    if let Some(n) = args.corpus_rotate_batches {
        config.rotate_every_batches = n;
    }
    if let Some(mix) = args.compute_mix {
        config.arith = mix.into();
    }
    if let Some(n) = args.async_device_slots {
        config.device_slots = n;
    }
    if let Some(n) = args.async_host_slots {
        config.host_slots = n;
    }
    if let Some(n) = args.async_batches {
        config.batches = n;
    }
    if let Some(n) = args.quarantine_slots {
        config.quarantine_slots = n;
    }
    config.lane = lane_config(args);
    config.sweep = gpu_cuda::SweepConfig {
        enabled: args.lane.vram_sweep,
        duty: args.lane.vram_sweep_duty,
        share: args.lane.vram_sweep_share,
        ..gpu_cuda::SweepConfig::default()
    };
    config.boundary_checks = args.boundary_checks;
    config.workers = config.workers.min(config.host_slots.max(1));
    config.workers_max = config.workers_max.max(config.workers);
    config
}

/// Share of run time reserved for transfer/retention when `--coverage-floor` is not given.
/// Measured on the GTX 960 (H5 evidence): 0.25 gives about five times the VRAM/PCIe verification
/// of an unscheduled burst loop for about a quarter less compute; `0` restores the old loop.
const DEFAULT_COVERAGE_FLOOR: f64 = 0.25;

fn coverage_floor(args: &RunArgs) -> f64 {
    args.coverage_floor.unwrap_or(DEFAULT_COVERAGE_FLOOR)
}

/// Whether the transfer/retention lane overlaps compute on its own streams. Since H6 this is the
/// default for `--async-compute` runs: explicit `--coverage-floor` opts back into the pre-H6
/// time-slice behaviour (matching what the H1-H5 hardware gates already pin), and
/// `--no-transfer-overlap` opts out regardless of the floor. `--workload-profile compute-only`
/// never has a lane at all.
fn overlap_active(args: &RunArgs) -> bool {
    if !args.async_compute || args.lane.profile() == ProfileArg::ComputeOnly {
        return false;
    }
    if args.lane.no_transfer_overlap {
        return false;
    }
    args.lane.transfer_overlap || args.coverage_floor.is_none()
}

/// Whether any GPU-H6 lane runs at all: the overlapped transfer/retention lane, the GPU-local
/// VRAM workload, or both.
fn lane_active(args: &RunArgs) -> bool {
    lane_config(args).active()
}

/// The lane configuration implied by `args`: tuning flags plus the context-dependent
/// `enabled`/`vram_local` decision (see [`overlap_active`]).
fn lane_config(args: &RunArgs) -> gpu_engine::lane::LaneConfig {
    let mut config = args.lane.tuning();
    config.enabled = overlap_active(args);
    config.vram_local = args.lane.vram_local
        && args.async_compute
        && args.lane.profile() != ProfileArg::ComputeOnly;
    config
}

/// GPU-H6 lane options: needs `--async-compute`, and its own internal contradictions
/// (overlap vs. the coverage floor, tuning without a lane, out-of-range values).
fn validate_lane_args(args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.lane.any() && !args.async_compute {
        return Err("--transfer-overlap, --no-transfer-overlap, --vram-local, --vram-sweep, --workload-profile, --lane-* and --vram-passes require --async-compute".into());
    }
    if !(args.async_compute && args.lane.any()) {
        return Ok(());
    }
    let profile = args.lane.profile();
    if args.lane.transfer_overlap && args.lane.no_transfer_overlap {
        return Err("--transfer-overlap and --no-transfer-overlap contradict each other".into());
    }
    if args.lane.transfer_overlap
        && (args.coverage_floor.is_some_and(|f| f > 0.0) || profile == ProfileArg::ComputeOnly)
    {
        return Err("--transfer-overlap replaces the coverage floor and is meaningless with --workload-profile compute-only".into());
    }
    if args.lane.vram_local && profile == ProfileArg::ComputeOnly {
        return Err("--vram-local is meaningless with --workload-profile compute-only".into());
    }
    if args.lane.vram_sweep && (!overlap_active(args) || profile != ProfileArg::Combined) {
        return Err(
            "--vram-sweep trades pool halves with the overlapped transfer lane: not with --no-transfer-overlap, --coverage-floor or a single-workload profile".into(),
        );
    }
    if [args.lane.vram_sweep_duty, args.lane.vram_sweep_share]
        .into_iter()
        .flatten()
        .any(|value| !(0.05..=1.0).contains(&value))
    {
        return Err("--vram-sweep-duty and --vram-sweep-share must be within 0.05..=1.0".into());
    }
    if !lane_active(args) && args.lane.lane_tuning() {
        return Err(
            "--lane-* and --vram-passes need a lane: --transfer-overlap, --vram-local, or the new default overlap (not --no-transfer-overlap, not --workload-profile compute-only)".into(),
        );
    }
    if lane_active(args) && lane_config(args).validate().is_err() {
        return Err(
            "lane options: 1..=4 slots, chunk 1..=64 MiB, weight 1..=64, 1..=64 vram passes".into(),
        );
    }
    if profile == ProfileArg::TransferOnly
        && args.inject_vulkan_fault == Some(VulkanFaultArg::Matrix)
    {
        return Err(
            "a matrix fault needs compute: not with --workload-profile transfer-only".into(),
        );
    }
    Ok(())
}

fn validate_args(args: &RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    resolved_hash_policy(args)?;
    if args.backend == BackendArg::Mock
        && (args.hash_policy.is_some() || args.corpus_rotate_batches.is_some())
    {
        return Err("--hash-policy and --corpus-rotate-batches need a hardware backend".into());
    }
    let async_only = args.corpus_rotate_batches.is_some()
        || args.compute_mix.is_some()
        || args.async_device_slots.is_some()
        || args.async_host_slots.is_some()
        || args.async_workers.is_some()
        || args.async_batches.is_some()
        || args.corpus_threads.is_some()
        || args.async_workers_max.is_some()
        || args.coverage_floor.is_some()
        || args.quarantine_slots.is_some()
        || args.bottleneck_log.is_some();
    if async_only && !args.async_compute {
        return Err("--corpus-rotate-batches, --compute-mix, --async-*, --corpus-threads, --quarantine-slots and --bottleneck-log require --async-compute".into());
    }
    if matches!(
        args.compute_mix,
        Some(ComputeMixArg::Alternate | ComputeMixArg::All)
    ) && args.corpus_rotate_batches == Some(0)
    {
        return Err("--compute-mix alternate switches arithmetic at corpus rotations: not with --corpus-rotate-batches 0".into());
    }
    if args.async_compute {
        let c = async_config(args);
        if !(gpu_engine::plan::MIN_DEVICE_SLOTS..=16).contains(&c.device_slots)
            || c.host_slots <= c.device_slots
            || c.host_slots > 64
            || c.quarantine_slots >= c.host_slots
            || !(1..=1024).contains(&c.batches)
            || args.async_workers == Some(0)
            || args.async_workers_max == Some(0)
            || args
                .async_workers
                .zip(args.async_workers_max)
                .is_some_and(|(initial, max)| max < initial)
            || !(0.0..=0.9).contains(&coverage_floor(args))
        {
            return Err("async options: need 2..=16 device slots, more host slots than device slots (at most 64), quarantine below the host slots, 1..=1024 batches, at least one worker (max >= initial) and a coverage floor within 0..=0.9".into());
        }
    }
    validate_lane_args(args)?;
    if args.async_compute && (args.backend == BackendArg::Mock || args.recreate_compute_resources) {
        return Err("--async-compute requires hardware with persistent compute resources".into());
    }
    if args.duration_seconds == 0 && args.generations.is_none() {
        return Err("duration must be positive when --generations is not set".into());
    }
    if args
        .vram_margin_mib
        .is_some_and(|mib| !(32..=4096).contains(&mib))
    {
        return Err("vram-margin-mib must be between 32 and 4096".into());
    }
    if !(1..=95).contains(&args.vram_percent) {
        return Err("vram-percent must be between 1 and 95".into());
    }
    if args.max_allocation_mib < 4 || !(1..=65_536).contains(&matrix_jobs(args)) {
        return Err("max-allocation-mib must be >=4 and matrix-jobs must be 1..65536".into());
    }
    if args
        .compute_rounds
        .is_some_and(|rounds| !(1..=4096).contains(&rounds))
    {
        return Err("matrix-inner (compute-rounds alias) must be 1..4096".into());
    }
    if !(1..=65_536).contains(&args.blocks_per_generation) {
        return Err("blocks-per-generation must be between 1 and 65536".into());
    }
    if args.inject_fault.is_some() && args.backend != BackendArg::Mock {
        return Err("--inject-fault requires --backend mock".into());
    }
    if args.inject_vulkan_fault.is_some() && args.backend == BackendArg::Mock {
        return Err("--inject-hardware-fault requires a hardware backend".into());
    }
    Ok(())
}

const DEFAULT_VRAM_MARGIN_MIB: u64 = 128;

/// How the test pool was sized, for the report: `max` (driver-reported free memory after every
/// other buffer, minus the margin) or `percent`/`fixed` as before.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct VramFill {
    mode: &'static str,
    target_bytes: u64,
    /// Free device memory just before the pool was allocated (`max` mode).
    free_before_pool_bytes: Option<u64>,
    margin_bytes: Option<u64>,
    note: Option<&'static str>,
}

/// The pool target once every other buffer exists. `--vram-max` asks the driver what is free now
/// (the matrix, async slots and lane are already allocated, and nothing large is allocated after
/// the pool) and leaves only the margin; otherwise the planned `planned` target stands.
fn vram_fill_plan(
    args: &RunArgs,
    info: &VulkanDeviceInfo,
    free_now: Option<u64>,
    planned: u64,
) -> VramFill {
    let kept = |mode, note| VramFill {
        mode,
        target_bytes: planned,
        free_before_pool_bytes: None,
        margin_bytes: None,
        note,
    };
    if args.vram_mib.is_some() {
        return kept("fixed", None);
    }
    if !args.vram_max {
        return kept("percent", None);
    }
    if info.device_type == "INTEGRATED_GPU" {
        return kept(
            "percent",
            Some("--vram-max ignored: integrated GPU memory is system RAM"),
        );
    }
    let Some(free) = free_now else {
        return kept(
            "percent",
            Some("--vram-max needs a live free-memory reading (CUDA); kept --vram-percent"),
        );
    };
    let margin = args.vram_margin_mib.unwrap_or(DEFAULT_VRAM_MARGIN_MIB) * MIB;
    VramFill {
        mode: "max",
        target_bytes: free.saturating_sub(margin),
        free_before_pool_bytes: Some(free),
        margin_bytes: Some(margin),
        note: None,
    }
}

fn allocation_target_bytes(info: &VulkanDeviceInfo, args: &RunArgs) -> u64 {
    if let Some(mib) = args.vram_mib {
        return mib.saturating_mul(MIB);
    }
    let available = info.budget_bytes.zip(info.usage_bytes).map_or(
        info.device_local_bytes.saturating_mul(80) / 100,
        |(budget, usage)| budget.saturating_sub(usage),
    );
    available.saturating_mul(u64::from(effective_vram_percent(info, args))) / 100
}

/// Integrated GPUs usually expose system RAM as device-local memory. Reserving
/// 85--95% of that heap can starve Linux, the CPU oracle and staging buffers.
/// An explicit --vram-mib remains the operator-controlled bounded override.
fn effective_vram_percent(info: &VulkanDeviceInfo, args: &RunArgs) -> u8 {
    if info.device_type == "INTEGRATED_GPU" && args.vram_mib.is_none() {
        args.vram_percent.min(60)
    } else {
        args.vram_percent
    }
}

fn should_stop(args: &RunArgs, generation: u64, elapsed: Duration) -> bool {
    args.generations.map_or_else(
        || elapsed >= Duration::from_secs(args.duration_seconds),
        |target| generation >= target,
    )
}

fn completed_pool_sweeps(allocation_passes: &[u64]) -> u64 {
    allocation_passes.iter().copied().min().unwrap_or(0)
}

fn ranges_overlap(left_start: u64, left_end: u64, right_start: u64, right_end: u64) -> bool {
    left_start < right_end && right_start < left_end
}

fn final_verdict(cancelled: bool, failure_count: usize) -> Verdict {
    if cancelled {
        Verdict::Aborted
    } else if failure_count == 0 {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

struct ScorecardInputs<'a> {
    elapsed_seconds: f64,
    compute_inner: u32,
    results_verified: u64,
    transfer_bytes_verified: u64,
    transfer_operations: u64,
    retention_checks: u64,
    pattern_counts: [u64; 7],
    sweeps: u64,
    allocated_bytes: u64,
    campaign_failed_batches: u64,
    compute_failure_records: u64,
    async_report: Option<&'a gpu_cuda::AsyncReport>,
    first_failure: Option<hwhealth::FirstFailure>,
    link: hwhealth::LinkHealth,
    kernel_log: hwhealth::KernelLogEvidence,
    ecc: hwhealth::EccEvidence,
    stress: Option<hwhealth::StressProfile>,
}

#[allow(clippy::cast_precision_loss)]
fn build_scorecard(i: &ScorecardInputs<'_>) -> hwhealth::DetectionScorecard {
    let seconds = i.elapsed_seconds.max(f64::EPSILON);
    // ops = 2 * jobs * N * N * K and results = jobs * N * N, so ops = results * 2K.
    let cumulative = i.async_report.map(|r| &r.cumulative);
    // The sequential path runs the u32 GEMM only; the async one counts each arithmetic.
    let (u32_results, f32_results, i8_results) =
        cumulative.map_or((i.results_verified, 0, 0), |c| {
            (
                c.u32_results_verified,
                c.f32_results_verified,
                c.i8_results_verified,
            )
        });
    let per_result = 2 * u64::from(i.compute_inner);
    let ops = u32_results.saturating_mul(per_result);
    let flops = f32_results.saturating_mul(per_result);
    let tensor_ops = i8_results.saturating_mul(per_result);
    let sweep = i.async_report.and_then(|r| r.sweep.as_ref());
    hwhealth::DetectionScorecard {
        version: 1,
        compute: hwhealth::ComputeDetection {
            verified_integer_ops: ops,
            verified_integer_ops_per_second: ops as f64 / seconds,
            verified_fp32_flops: flops,
            verified_fp32_flops_per_second: flops as f64 / seconds,
            verified_int8_tensor_ops: tensor_ops,
            verified_int8_tensor_ops_per_second: tensor_ops as f64 / seconds,
            failed_batches: i.campaign_failed_batches,
            mismatch_words: cumulative.map(|c| c.mismatch_words),
            mismatch_samples: cumulative.map(|c| c.mismatch_samples.clone()).unwrap_or_default(),
            failure_records: i.compute_failure_records,
            unfinished_batches: cumulative.map(|c| c.unfinished_batches),
            conservation: i.async_report.map(|r| r.conservation.clone()),
            integrity_checks: cumulative.map(|c| c.integrity_checks),
            integrity_failures: cumulative.map(|c| c.integrity_failures),
            failure_attribution: i.async_report.and_then(|r| r.failure_attribution.clone()),
        },
        coverage: hwhealth::CoverageDetection {
            vram_pcie_verified_bytes: i.transfer_bytes_verified,
            vram_pcie_verified_mib_per_second: i.transfer_bytes_verified as f64
                / 1_048_576.0
                / seconds,
            transfer_operations: i.transfer_operations,
            delayed_reread_checks: i.retention_checks,
            pattern_schedule_version: TRANSFER_PATTERN_VERSION,
            pattern_classes: PatternClass::ALL
                .iter()
                .zip(i.pattern_counts)
                .filter(|(_, n)| *n > 0)
                .map(|(class, n)| (class.label(), n))
                .collect(),
            allocation_sweeps: i.sweeps,
            allocated_bytes: i.allocated_bytes,
            device_verified_bytes: sweep.map(|s| s.device_verified_bytes),
            device_verified_gib_per_second: sweep
                .map(|s| s.device_verified_bytes as f64 / 1_073_741_824.0 / seconds),
            sweep_checkpoint_cpu_verified_bytes: sweep.map(|s| s.checkpoint_cpu_verified_bytes),
        },
        first_failure: i.first_failure.clone(),
        stress: i.stress.clone(),
        link: i.link.clone(),
        kernel_log: i.kernel_log.clone(),
        ecc: i.ecc.clone(),
        hardware_health: hwhealth::assess_health(&i.link, &i.kernel_log, &i.ecc),
        notes: vec![
            "delayed_reread_checks re-read after a short delay; they are not DRAM dwell-time tests"
                .into(),
            "hardware_health grades link, AER, kernel-log and ECC evidence for this GPU: errors (uncorrectable, device lost) add a `hardware` failure; warnings (corrected) keep PASS but flag it; a clean status does not prove the absence of corrected errors"
                .into(),
            "failure_records is one per failing compute call; failed_batches counts every failed batch"
                .into(),
        ],
    }
}

#[allow(clippy::cast_precision_loss)]
fn mib_per_second(bytes: u64, elapsed: Duration) -> f64 {
    bytes as f64 / MIB as f64 / elapsed.as_secs_f64().max(1e-9)
}

/// What the transfer lane has done so far, in the counters the run reports.
#[derive(Clone, Copy, Debug, Default)]
struct LaneTotals {
    transfer_operations: u64,
    retention_checks: u64,
    bytes_verified: u64,
    transfer_bytes: u64,
    pattern_counts: [u64; 7],
    sweeps: u64,
}

impl LaneTotals {
    fn from_report(report: Option<&gpu_cuda::LaneReport>) -> Self {
        let Some(report) = report else {
            return Self::default();
        };
        let (t, v) = (&report.transfer, &report.vram);
        Self {
            transfer_operations: t.write_ops + v.write_ops,
            retention_checks: t.reread_ops + v.checkpoint_ops,
            bytes_verified: t.verified_bytes + v.cpu_verified_bytes,
            transfer_bytes: t.h2d_bytes + t.d2h_bytes + v.cpu_verified_bytes,
            pattern_counts: t.pattern_counts,
            sweeps: if report.config.enabled {
                t.sweeps
            } else {
                v.sweeps
            },
        }
    }
}

/// Appends the failure records found since the last call to `--fault-events` (step F1). The
/// async pipeline's host-corpus check rides along as evidence of a compute failure.
fn emit_faults(
    writer: &dit_contracts::FaultWriter,
    failures: &[FailureEvidence],
    emitted: &mut usize,
    pci: Option<&str>,
    backend: Option<&Backend>,
) {
    if !writer.enabled() || failures.len() <= *emitted {
        *emitted = failures.len();
        return;
    }
    let host_changed = backend
        .and_then(Backend::async_report)
        .and_then(|report| report.failure_attribution)
        .is_some_and(|attribution| attribution == "host_memory_changed");
    for failure in &failures[*emitted..] {
        let mut record =
            dit_contracts::FaultRecord::new(failure.phase.clone(), 1, failure.detail.clone())
                .evidence("generation", failure.generation.to_string());
        if let Some(pci) = pci {
            record = record.device(pci);
        }
        if host_changed && failure.phase == "compute" {
            record = record.evidence("host", "host_memory_changed");
        }
        writer.write(&record);
    }
    *emitted = failures.len();
}

/// Lane verification failures become ordinary failure records (phase says which detector).
fn collect_lane_failures(backend: &Backend, failures: &mut Vec<FailureEvidence>) {
    for lane in backend.take_lane_failures() {
        failures.push(FailureEvidence {
            generation: lane.generation,
            seed: lane.seed,
            phase: lane.phase,
            detail: format!(
                "allocation {} offset {} bytes {}: {}",
                lane.allocation, lane.offset, lane.bytes, lane.detail
            ),
        });
    }
}

/// Dashboard line for the GPU-DRAM sweep: device-verified volume and rate, kept apart from the
/// CPU-verified figures on the line above.
#[allow(clippy::cast_precision_loss)]
fn sweep_note(report: &gpu_cuda::SweepReport, elapsed: Duration) -> String {
    let seconds = elapsed.as_secs_f64().max(1e-9);
    format!(
        "device-verified {} ({:.1} GiB/s) | passes {} | CPU checkpoints {} | half swaps {}",
        format_bytes(report.device_verified_bytes),
        report.device_verified_bytes as f64 / 1_073_741_824.0 / seconds,
        report.passes_completed,
        format_bytes(report.checkpoint_cpu_verified_bytes),
        report.zone_swaps
    )
}

/// Dashboard line for `--compute-mix`: verified rate of the integer and the FP32 GEMM so far.
#[allow(clippy::cast_precision_loss)]
fn mix_note(args: &RunArgs, backend: Option<&Backend>, elapsed: Duration) -> Option<String> {
    args.compute_mix?;
    let cumulative = backend.and_then(Backend::async_report)?.cumulative;
    let per_result = 2.0 * f64::from(resolved_compute_rounds(args));
    let seconds = elapsed.as_secs_f64().max(1e-9);
    Some(format!(
        "u32 {:.1} Gop/s | fp32 {:.1} GFLOP/s | int8 tensor {:.1} TOPS (verified, whole run)",
        cumulative.u32_results_verified as f64 * per_result / seconds / 1e9,
        cumulative.f32_results_verified as f64 * per_result / seconds / 1e9,
        cumulative.i8_results_verified as f64 * per_result / seconds / 1e12
    ))
}

/// How often the kernel log is read during a run for the live hardware-health line.
const LOG_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Dashboard line for kernel-log findings so far (link/ECC counters are read at the end).
fn live_hardware_note(findings: &hwhealth::LogFindings) -> Option<String> {
    (findings.errors + findings.warnings > 0).then(|| {
        format!(
            "{} error(s), {} warning(s) in kernel log: {}",
            findings.errors,
            findings.warnings,
            findings.reasons.join("; ")
        )
    })
}

fn failure(
    generation: u64,
    seed: u64,
    phase: &str,
    error: impl std::fmt::Display,
) -> FailureEvidence {
    FailureEvidence {
        generation,
        seed,
        phase: phase.to_owned(),
        detail: error.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn ui_state(
    system: &SystemInfo,
    live: LiveUtilization,
    devices: &[GpuDevice],
    telemetry: &[GpuTelemetry],
    args: &RunArgs,
    elapsed: Duration,
    generation: u64,
    seed_index: u64,
    seed: u64,
    selected: Option<VulkanDeviceInfo>,
    allocation_bytes: u64,
    allocation_target_bytes: u64,
    chunk_bytes: u64,
    transfer_mib_s: Option<f64>,
    sweeps: u64,
    blocks_verified: u64,
    bytes_verified: u64,
    calculations_verified: u64,
    failures: usize,
    aborted: bool,
    finished: bool,
) -> GpuUiState {
    GpuUiState {
        system: system.clone(),
        live,
        devices: devices.to_vec(),
        telemetry: telemetry.to_vec(),
        duration_seconds: args.duration_seconds,
        exact_generations: args.generations,
        elapsed,
        generation,
        seed_index,
        seed,
        backend: if selected
            .as_ref()
            .is_some_and(|i| i.api_version.starts_with("CUDA"))
        {
            if args.async_compute {
                "CUDA GPU-H ASYNC"
            } else {
                "CUDA GPU-G3"
            }
        } else if selected.is_some() {
            "VULKAN GPU-G3"
        } else {
            "CPU MOCK GPU-A"
        }
        .to_owned(),
        async_pipeline: args.async_compute,
        hardware_note: None,
        sweep_note: None,
        mix_note: None,
        throttle_row: None,
        throttle_alert: None,
        hardware_active: selected.is_some(),
        selected,
        allocation_bytes,
        allocation_target_bytes,
        chunk_bytes,
        transfer_mib_s,
        sweeps,
        blocks_verified,
        bytes_verified,
        calculations_verified,
        failures: u64::try_from(failures).unwrap_or(u64::MAX),
        temp_limit: args.temp_limit_celsius,
        aborted,
        finished,
    }
}

fn progress_snapshot(state: &GpuUiState) -> GpuProgressSnapshot {
    let selected = state.selected.as_ref();
    let telemetry = selected
        .and_then(|device| device.pci_address.as_deref())
        .and_then(|pci| {
            state
                .telemetry
                .iter()
                .find(|sample| sample.pci_address == pci)
        });
    let state_name = if state.aborted {
        "ABORTED"
    } else if state.finished && state.failures > 0 {
        "FAIL"
    } else if state.finished {
        "PASS"
    } else {
        "RUNNING"
    };
    GpuProgressSnapshot {
        device_index: selected.map_or(usize::MAX, |device| device.index),
        name: selected.map_or_else(|| "CPU MOCK GPU-A".to_owned(), |device| device.name.clone()),
        pci_address: selected.and_then(|device| device.pci_address.clone()),
        uuid: selected.map_or_else(String::new, |device| device.uuid.clone()),
        state: state_name.to_owned(),
        elapsed_seconds: state.elapsed.as_secs_f64(),
        duration_seconds: state.duration_seconds,
        generation: state.generation,
        seed_index: state.seed_index,
        allocation_bytes: state.allocation_bytes,
        allocation_target_bytes: state.allocation_target_bytes,
        chunk_bytes: state.chunk_bytes,
        transfer_mib_s: state.transfer_mib_s,
        sweeps: state.sweeps,
        bytes_verified: state.bytes_verified,
        matrix_results_verified: state.calculations_verified,
        failures: state.failures,
        gpu_usage_percent: telemetry.and_then(|sample| sample.utilization_percent),
        vram_usage_percent: vram_capacity_percent(
            telemetry.and_then(|sample| sample.vram_used_bytes),
            telemetry
                .and_then(|sample| sample.vram_total_bytes)
                .or_else(|| selected.map(|device| device.device_local_bytes)),
        ),
        vram_used_bytes: telemetry.and_then(|sample| sample.vram_used_bytes),
        vram_total_bytes: telemetry
            .and_then(|sample| sample.vram_total_bytes)
            .or_else(|| selected.map(|device| device.device_local_bytes)),
        temperature_celsius: telemetry.and_then(|sample| sample.temperature_celsius),
        power_watts: telemetry.and_then(|sample| sample.power_watts),
        clock_mhz: telemetry.and_then(|sample| sample.core_clock_mhz),
        memory_clock_mhz: telemetry.and_then(|sample| sample.memory_clock_mhz),
        telemetry_source: telemetry.map_or_else(
            || "N/A (no matching PCI telemetry)".to_owned(),
            |sample| sample.source.clone(),
        ),
        hardware_note: state.hardware_note.clone(),
    }
}

fn install_cancel_handler() -> Result<(), Box<dyn std::error::Error>> {
    CANCELLED.store(false, Ordering::SeqCst);
    let result = CANCEL_HANDLER.get_or_init(|| {
        ctrlc::set_handler(|| CANCELLED.store(true, Ordering::SeqCst))
            .map_err(|error| error.to_string())
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(error.clone().into()),
    }
}

fn write_progress(
    path: &Path,
    snapshot: &GpuProgressSnapshot,
) -> Result<(), Box<dyn std::error::Error>> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, snapshot)?;
    writeln!(file)?;
    file.flush()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn render_gpu_ui(state: &GpuUiState, size: TerminalSize, color: bool) -> String {
    let selected_pci = state
        .selected
        .as_ref()
        .and_then(|device| device.pci_address.as_deref());
    let selected_inventory = selected_pci
        .and_then(|pci| {
            state
                .devices
                .iter()
                .find(|device| device.pci_address == pci)
        })
        .or_else(|| state.devices.first());
    let selected_live = selected_pci
        .and_then(|pci| {
            state
                .telemetry
                .iter()
                .find(|sample| sample.pci_address == pci)
        })
        .or_else(|| state.telemetry.first());
    let ram_percent = percent(state.live.memory_used_bytes, state.live.memory_total_bytes);
    let allocation_percent = percent(state.allocation_bytes, state.allocation_target_bytes);
    let progress = state.exact_generations.map_or_else(
        || percent(state.elapsed.as_secs(), state.duration_seconds),
        |target| percent(state.generation, target),
    );
    let gpu_name = state.selected.as_ref().map_or_else(
        || selected_inventory.map_or_else(|| "N/A".to_owned(), |gpu| gpu.model.clone()),
        |gpu| gpu.name.clone(),
    );
    let gpu_driver = selected_inventory
        .and_then(|device| device.driver.as_deref())
        .unwrap_or("N/A");
    let gpu_bdf = selected_pci.unwrap_or("N/A");
    let gpu_uuid = state
        .selected
        .as_ref()
        .map_or("N/A", |device| device.uuid.as_str());
    let vram_total = selected_live
        .and_then(|sample| sample.vram_total_bytes)
        .or_else(|| selected_inventory.and_then(|device| device.vram_total_bytes))
        .or_else(|| {
            state
                .selected
                .as_ref()
                .map(|device| device.device_local_bytes)
        });
    let vram_used = selected_live.and_then(|sample| sample.vram_used_bytes);
    let vram_percent = vram_capacity_percent(vram_used, vram_total);
    let telemetry_source = selected_live.map_or("N/A", |sample| sample.source.as_str());
    let status = if state.aborted {
        "ABORTED"
    } else if state.finished && state.failures > 0 {
        "FAIL"
    } else if state.finished && state.hardware_note.is_some() {
        // Data matched, but corrected hardware events were seen: never a clean-looking PASS.
        "PASS (HW WARNINGS)"
    } else if state.finished {
        "PASS"
    } else {
        "RUNNING"
    };
    let target = state.exact_generations.map_or_else(
        || format_duration(state.duration_seconds),
        |value| format!("{value} generations"),
    );
    let hardware = if state.hardware_active {
        "ACTIVE"
    } else {
        "NOT ACTIVE (MOCK)"
    };
    let transfer = state
        .transfer_mib_s
        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} MiB/s"));
    let temp_policy = state
        .temp_limit
        .map_or_else(|| "OFF".to_owned(), |value| format!("ON {value:.1} C"));
    let cpu_detail = format!(
        "Freq {} / Temp {} / Power {}",
        state
            .live
            .cpu_frequency_ghz
            .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.2} GHz")),
        format_optional(state.live.cpu_temperature_celsius, "C"),
        format_optional(state.live.cpu_power_watts, "W")
    );
    let gpu_detail = selected_live.map_or_else(
        || "Clock N/A / Temp N/A / Power N/A".to_owned(),
        |sample| {
            format!(
                "Clock {} / Temp {} / Power {}",
                sample.core_clock_mhz.map_or_else(
                    || "N/A".to_owned(),
                    |value| format!("{:.2} GHz", value / 1000.0)
                ),
                format_optional(sample.temperature_celsius, "C"),
                format_optional(sample.power_watts, "W")
            )
        },
    );
    let vram_detail = format!(
        "Used {} / {}",
        vram_used.map_or_else(|| "N/A".to_owned(), format_bytes),
        vram_total.map_or_else(|| "N/A".to_owned(), format_bytes)
    );
    let dashboard = UnifiedDashboard {
        engine: if state.async_pipeline {
            "GPU-H-CUDA-1"
        } else {
            "GPU-G3-1"
        }
        .to_owned(),
        report_abi: SCHEMA_VERSION.to_string(),
        profile: format!(
            "GPU + VRAM / DEEP | {} | HARDWARE {hardware}",
            state.backend
        ),
        hardware_rows: vec![
            format!("CPU:     {}", state.system.cpu),
            format!("THREADS: {} logical CPU(s)", state.system.logical_threads),
            format!(
                "RAM:     {} total",
                format_bytes(state.system.memory_total_bytes)
            ),
            format!("GPU:     {gpu_name}"),
            format!(
                "VRAM:    {}",
                vram_total.map_or_else(|| "N/A".to_owned(), format_bytes)
            ),
            format!("DRIVER:  {gpu_driver}"),
            format!("BDF:     {gpu_bdf}"),
            format!("UUID:    {gpu_uuid}"),
            format!("BOARD:   {}", state.system.board),
            format!("KERNEL:  {}", state.system.kernel),
        ],
        live_meters: vec![
            DashboardMeter::new("CPU USAGE", state.live.cpu_percent).detail(cpu_detail),
            DashboardMeter::new("MEMORY", Some(ram_percent)).detail(format!(
                "Used {} / {}",
                format_bytes(state.live.memory_used_bytes),
                format_bytes(state.live.memory_total_bytes)
            )),
            DashboardMeter::new(
                "GPU USAGE",
                selected_live.and_then(|sample| sample.utilization_percent),
            )
            .detail(gpu_detail),
            DashboardMeter::new("VRAM", vram_percent).detail(vram_detail),
            DashboardMeter::new("DIT TARGET", Some(allocation_percent)).detail(format!(
                "Allocated {} / {} / Source {telemetry_source}",
                format_bytes(state.allocation_bytes),
                format_bytes(state.allocation_target_bytes)
            )),
        ],
        state: status.to_owned(),
        progress_percent: Some(progress),
        elapsed: format_duration(state.elapsed.as_secs()),
        target,
        active_test: "GPU-XFER-MATRIX-001  CPU -> RAM -> PCIe -> VRAM -> GPU -> CPU oracle"
            .to_owned(),
        process: if state.async_pipeline {
            "async u32 GEMM bursts + overlapped PCIe lane + VRAM retention + CPU verifier pool"
        } else {
            "split/scatter + VRAM retention + tiled u32 GEMM + exact readback"
        }
        .to_owned(),
        calculations_completed: state.calculations_verified,
        failures: state.failures,
        running_rows: vec![
            format!(
                "SESSION SEED  0x{:016x} | SEED INDEX {} | GENERATIONS {} | SWEEPS {}",
                state.seed, state.seed_index, state.generation, state.sweeps
            ),
            format!(
                "VRAM ALLOCATED {} / {} | CHUNK {} | H2D+D2H {transfer}",
                format_bytes(state.allocation_bytes),
                format_bytes(state.allocation_target_bytes),
                format_bytes(state.chunk_bytes)
            ),
            format!(
                "VERIFIED {} logical | BLOCKS {} | MATRIX RESULTS {}",
                format_bytes(state.bytes_verified),
                state.blocks_verified,
                state.calculations_verified
            ),
        ]
        .into_iter()
        .chain(
            state
                .mix_note
                .as_ref()
                .map(|note| format!("COMPUTE MIX   {note}")),
        )
        .chain(
            state
                .sweep_note
                .as_ref()
                .map(|note| format!("DRAM SWEEP    {note}")),
        )
        .chain(
            state
                .hardware_note
                .as_ref()
                .map(|note| format!("HARDWARE      {note}")),
        )
        .chain(state.throttle_row.clone())
        .collect(),
        footer: state.throttle_alert.clone().unwrap_or_else(|| {
            format!("TEMP LIMIT {temp_policy} | Ctrl+C: stop, drain retention and preserve report")
        }),
    };
    render_unified_dashboard(&dashboard, size, color)
}

#[allow(dead_code, clippy::too_many_lines)]
fn render_gpu_ui_legacy(state: &GpuUiState, size: TerminalSize, color: bool) -> String {
    let width = usize::from(size.cols).clamp(60, 200);
    let inner = width - 2;
    let selected_pci = state
        .selected
        .as_ref()
        .and_then(|device| device.pci_address.as_deref());
    let selected_inventory = selected_pci
        .and_then(|pci| {
            state
                .devices
                .iter()
                .find(|device| device.pci_address == pci)
        })
        .or_else(|| state.devices.first());
    let selected_live = selected_pci
        .and_then(|pci| {
            state
                .telemetry
                .iter()
                .find(|sample| sample.pci_address == pci)
        })
        .or_else(|| state.telemetry.first());
    let ram_percent = percent(state.live.memory_used_bytes, state.live.memory_total_bytes);
    let allocation_percent = percent(state.allocation_bytes, state.allocation_target_bytes);
    let progress = state.exact_generations.map_or_else(
        || percent(state.elapsed.as_secs(), state.duration_seconds),
        |target| percent(state.generation, target),
    );
    let gpu_name = state.selected.as_ref().map_or_else(
        || selected_inventory.map_or_else(|| "N/A".to_owned(), |gpu| gpu.model.clone()),
        |gpu| gpu.name.clone(),
    );
    let gpu_driver = selected_inventory
        .and_then(|device| device.driver.as_deref())
        .unwrap_or("N/A");
    let gpu_bdf = selected_pci.unwrap_or("N/A");
    let gpu_uuid = state
        .selected
        .as_ref()
        .map_or("N/A", |device| device.uuid.as_str());
    let vram_total = selected_live
        .and_then(|sample| sample.vram_total_bytes)
        .or_else(|| selected_inventory.and_then(|device| device.vram_total_bytes))
        .or_else(|| {
            state
                .selected
                .as_ref()
                .map(|device| device.device_local_bytes)
        });
    let vram_used = selected_live.and_then(|sample| sample.vram_used_bytes);
    let vram_percent = vram_capacity_percent(vram_used, vram_total);
    let telemetry_source = selected_live.map_or("N/A", |sample| sample.source.as_str());
    let status = if state.aborted {
        "ABORTED"
    } else if state.finished {
        if state.failures == 0 { "PASS" } else { "FAIL" }
    } else {
        "RUNNING"
    };
    let target = state.exact_generations.map_or_else(
        || format!("{}s", state.duration_seconds),
        |value| format!("{value} generations"),
    );
    let hardware = if state.hardware_active {
        "ACTIVE"
    } else {
        "NOT ACTIVE (MOCK)"
    };
    let transfer = state
        .transfer_mib_s
        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} MiB/s"));
    let temp_policy = state
        .temp_limit
        .map_or_else(|| "OFF".to_owned(), |value| format!("ON {value:.1} C"));
    if width < 110 || size.rows < 29 {
        return render_gpu_compact(
            state,
            size,
            color,
            selected_live,
            vram_percent,
            progress,
            status,
            &target,
            &transfer,
            &temp_policy,
        );
    }
    let gap = 2_usize;
    let left_total = (width - gap) / 2;
    let right_total = width - gap - left_total;
    let left_inner = left_total.saturating_sub(2);
    let right_inner = right_total.saturating_sub(2);
    let pair = |left: String, right: String| format!("{left}{}{right}", " ".repeat(gap));
    let cpu_detail = format!(
        "{} / Temp {} / Power {}",
        state
            .live
            .cpu_frequency_ghz
            .map_or_else(|| "GHz N/A".to_owned(), |value| format!("{value:.2} GHz")),
        format_optional(state.live.cpu_temperature_celsius, "C"),
        format_optional(state.live.cpu_power_watts, "W")
    );
    let gpu_detail = selected_live.map_or_else(
        || "GHz N/A / Temp N/A / Power N/A".to_owned(),
        |sample| {
            format!(
                "{} / Temp {} / Power {}",
                sample.core_clock_mhz.map_or_else(
                    || "GHz N/A".to_owned(),
                    |value| format!("{:.2} GHz", value / 1000.0)
                ),
                format_optional(sample.temperature_celsius, "C"),
                format_optional(sample.power_watts, "W")
            )
        },
    );
    let vram_detail = format!(
        "Used {} / {}",
        vram_used.map_or_else(|| "N/A".to_owned(), format_bytes),
        vram_total.map_or_else(|| "N/A".to_owned(), format_bytes)
    );
    let mut lines = vec![
        border("", inner, color),
        row_colored("DIT-01 / DATA INTEGRITY TEST ENGINE", inner, CYAN, color),
        row_colored(
            &format!(
                "ENGINE GPU-G3-1    REPORT ABI {SCHEMA_VERSION}    BACKEND {}    PROFILE GPU + VRAM / DEEP    HARDWARE {hardware}",
                state.backend
            ),
            inner,
            if state.hardware_active { GREEN } else { YELLOW },
            color,
        ),
        pair(
            border("SYSTEM HARDWARE", left_inner, color),
            border("LIVE UTILIZATION", right_inner, color),
        ),
        pair(
            row(&format!("CPU:     {}", state.system.cpu), left_inner),
            meter("CPU USAGE", state.live.cpu_percent, right_inner, color),
        ),
        pair(
            row(
                &format!("THREADS: {} logical CPU(s)", state.system.logical_threads),
                left_inner,
            ),
            row(&cpu_detail, right_inner),
        ),
        pair(
            row(
                &format!(
                    "RAM:     {} total",
                    format_bytes(state.system.memory_total_bytes)
                ),
                left_inner,
            ),
            meter("MEMORY", Some(ram_percent), right_inner, color),
        ),
        pair(
            row(&format!("GPU:     {gpu_name}"), left_inner),
            row(
                &format!(
                    "Used {} / {}",
                    format_bytes(state.live.memory_used_bytes),
                    format_bytes(state.live.memory_total_bytes)
                ),
                right_inner,
            ),
        ),
        pair(
            row(
                &format!(
                    "VRAM:    {}",
                    vram_total.map_or_else(|| "N/A".to_owned(), format_bytes)
                ),
                left_inner,
            ),
            meter(
                "GPU USAGE",
                selected_live.and_then(|sample| sample.utilization_percent),
                right_inner,
                color,
            ),
        ),
        pair(
            row(&format!("DRIVER:  {gpu_driver}"), left_inner),
            row(&gpu_detail, right_inner),
        ),
        pair(
            row(&format!("BDF:     {gpu_bdf}"), left_inner),
            meter("VRAM", vram_percent, right_inner, color),
        ),
        pair(
            row(&format!("UUID:    {gpu_uuid}"), left_inner),
            row(&vram_detail, right_inner),
        ),
        pair(
            row(&format!("BOARD:   {}", state.system.board), left_inner),
            meter_fixed(
                "DIT TARGET",
                Some(allocation_percent),
                right_inner,
                color,
                CYAN,
            ),
        ),
        pair(
            row(&format!("KERNEL:  {}", state.system.kernel), left_inner),
            row(
                &format!(
                    "Allocated {} / {} / Source {telemetry_source}",
                    format_bytes(state.allocation_bytes),
                    format_bytes(state.allocation_target_bytes)
                ),
                right_inner,
            ),
        ),
        pair(
            border("", left_inner, color),
            border("", right_inner, color),
        ),
        border("TESTING / PROCESS INFORMATION", inner, color),
        row_colored(
            &format!(
                "STATE: {status}       PROGRESS: {progress:.0}%       TIME: {} / {target}",
                format_duration(state.elapsed.as_secs())
            ),
            inner,
            if state.failures > 0 { RED } else { CYAN },
            color,
        ),
        meter_fixed("PROGRESS", Some(progress), inner, color, GREEN),
        row(
            "ACTIVE TEST   GPU-XFER-MATRIX-001  CPU -> RAM -> PCIe -> VRAM -> GPU -> CPU oracle",
            inner,
        ),
        row(
            "PROCESS       split/scatter + VRAM retention + tiled u32 GEMM + exact readback",
            inner,
        ),
        row_colored(
            &format!(
                "CALCULATIONS COMPLETED  {}    |    FAIL TOTAL  {}",
                state.calculations_verified, state.failures
            ),
            inner,
            if state.failures > 0 { RED } else { CYAN },
            color,
        ),
        row(
            &format!(
                "SESSION SEED  0x{:016x}    SEED INDEX {}    GENERATIONS {}    SWEEPS {}",
                state.seed, state.seed_index, state.generation, state.sweeps
            ),
            inner,
        ),
        row(
            &format!(
                "VRAM ALLOCATED  {} / {}    CHUNK {}    H2D+D2H {transfer}",
                format_bytes(state.allocation_bytes),
                format_bytes(state.allocation_target_bytes),
                format_bytes(state.chunk_bytes)
            ),
            inner,
        ),
        row(
            &format!(
                "VERIFIED  {} logical    BLOCKS {}    MATRIX RESULTS {}",
                format_bytes(state.bytes_verified),
                state.blocks_verified,
                state.calculations_verified
            ),
            inner,
        ),
        row_colored(
            &format!(
                "TEMP LIMIT: {temp_policy}   Ctrl+C: stop, drain pending retention and preserve report."
            ),
            inner,
            YELLOW,
            color,
        ),
        border("", inner, color),
    ];
    let available_rows = usize::from(size.rows).saturating_sub(1).max(1);
    lines.truncate(available_rows);
    lines.join("\n")
}

#[allow(clippy::too_many_arguments)]
fn render_gpu_compact(
    state: &GpuUiState,
    size: TerminalSize,
    color: bool,
    telemetry: Option<&GpuTelemetry>,
    vram_percent: Option<f64>,
    progress: f64,
    status: &str,
    target: &str,
    transfer: &str,
    temp_policy: &str,
) -> String {
    let width = usize::from(size.cols).clamp(60, 200);
    let inner = width - 2;
    let gpu = state
        .selected
        .as_ref()
        .map_or("N/A", |device| device.name.as_str());
    let mut lines = vec![
        border("", inner, color),
        row_colored("DIT-01 / GPU + VRAM / DEEP", inner, CYAN, color),
        row(&format!("GPU: {gpu}"), inner),
        meter("CPU USAGE", state.live.cpu_percent, inner, color),
        meter(
            "MEMORY",
            Some(percent(
                state.live.memory_used_bytes,
                state.live.memory_total_bytes,
            )),
            inner,
            color,
        ),
        meter(
            "GPU USAGE",
            telemetry.and_then(|sample| sample.utilization_percent),
            inner,
            color,
        ),
        meter("VRAM", vram_percent, inner, color),
        row(
            &format!(
                "Used {} / {}",
                telemetry
                    .and_then(|sample| sample.vram_used_bytes)
                    .map_or_else(|| "N/A".to_owned(), format_bytes),
                telemetry
                    .and_then(|sample| sample.vram_total_bytes)
                    .or_else(|| state
                        .selected
                        .as_ref()
                        .map(|device| device.device_local_bytes))
                    .map_or_else(|| "N/A".to_owned(), format_bytes)
            ),
            inner,
        ),
        row(
            &format!(
                "GPU {} / Temp {} / Power {} / Source {}",
                telemetry
                    .and_then(|sample| sample.core_clock_mhz)
                    .map_or_else(
                        || "GHz N/A".to_owned(),
                        |value| format!("{:.2} GHz", value / 1000.0)
                    ),
                format_optional(telemetry.and_then(|sample| sample.temperature_celsius), "C"),
                format_optional(telemetry.and_then(|sample| sample.power_watts), "W"),
                telemetry.map_or("N/A", |sample| sample.source.as_str())
            ),
            inner,
        ),
        border("TESTING / PROCESS INFORMATION", inner, color),
        row(
            &format!(
                "STATE {status} / PROGRESS {progress:.0}% / TIME {} / {target}",
                format_duration(state.elapsed.as_secs())
            ),
            inner,
        ),
        meter_fixed("PROGRESS", Some(progress), inner, color, GREEN),
        row(
            &format!(
                "GEN {} / SEED#{} / MATRIX {} / FAIL {}",
                state.generation, state.seed_index, state.calculations_verified, state.failures
            ),
            inner,
        ),
        row(
            &format!(
                "ALLOC {} / {} / XFER {transfer}",
                format_bytes(state.allocation_bytes),
                format_bytes(state.allocation_target_bytes)
            ),
            inner,
        ),
        row_colored(
            &format!("TEMP LIMIT {temp_policy} / Ctrl+C: preserve report"),
            inner,
            YELLOW,
            color,
        ),
        border("", inner, color),
    ];
    lines.truncate(usize::from(size.rows).saturating_sub(1).max(1));
    lines.join("\n")
}

/// The selected GPU's slowdowns since the run started, on the live THROTTLE row as `GPU1` (the
/// name the result counts it under); N/A when the driver gives no throttle reasons.
fn track_throttle(
    live: &mut ThrottleLive,
    counter: &mut dit_system::SlowdownCounter,
    telemetry: &[GpuTelemetry],
    pci: Option<&str>,
    elapsed: Duration,
) {
    let Some(sample) = pci.and_then(|pci| telemetry.iter().find(|s| s.pci_address == pci)) else {
        return;
    };
    let now = Instant::now();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let reading = sample.throttle_reasons_mask.map(|mask| {
        counter.observe(mask, now);
        (counter.episodes, Some(counter.seconds(now).round() as u64))
    });
    live.update("GPU1", reading, &format_duration(elapsed.as_secs()));
}

fn format_duration(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

fn invalid_report(
    run_id: &str,
    args: &RunArgs,
    devices: Vec<GpuDevice>,
    telemetry: Vec<GpuTelemetry>,
    elapsed: Duration,
    phase: &str,
    detail: String,
) -> GpuReport {
    GpuReport {
        workload: None,
        async_compute: None,
        detection: None,
        hardware_status: None,
        hash_policy: None,
        cpu_share: None,
        coverage_schedule: None,
        bottleneck_summary: None,
        schema_version: SCHEMA_VERSION,
        module_id: MODULE_ID.to_owned(),
        run_id: run_id.to_owned(),
        verdict: Verdict::Invalid,
        pipeline_version: "GPU-G3-1".to_owned(),
        backend: format!("{:?}_UNAVAILABLE", args.backend).to_uppercase(),
        hardware_tested: false,
        vram_hardware_test_active: false,
        selected_device: None,
        shader_sha256: None,
        allocation: None,
        allocation_target_percent: Some(args.vram_percent),
        vram_fill: None,
        duration_target_seconds: args.duration_seconds,
        active_elapsed_seconds: elapsed.as_secs_f64(),
        base_seed: args.seed,
        seed_quantum_seconds: SEED_QUANTUM_SECONDS,
        seeds_started: 0,
        generations_completed: 0,
        transfer_operations_completed: 0,
        matrix_dispatches_completed: 0,
        retention_checks_completed: 0,
        bytes_verified: 0,
        cumulative_transfer_bytes: 0,
        matrix_results_verified: 0,
        allocation_sweeps_completed: 0,
        last_transfer: None,
        last_compute: None,
        last_compute_attempt: None,
        campaign_profile: None,
        workspace_budget: None,
        matrix_dispatches_submitted: 0,
        matrix_dispatches_gpu_completed: 0,
        failure_count: 1,
        failures: vec![FailureEvidence {
            generation: 0,
            seed: args.seed,
            phase: phase.to_owned(),
            detail,
        }],
        gpu_inventory: devices,
        telemetry_start: telemetry.clone(),
        telemetry_end: telemetry,
        stop_on_error: args.stop_on_error,
        temp_limit_celsius: args.temp_limit_celsius,
        completion_reason: format!("{phase}_failed"),
    }
}

fn rotate_seed(base: u64, generation: u64) -> u64 {
    let mut value = base ^ generation.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

fn border(title: &str, inner: usize, color: bool) -> String {
    let plain = if title.is_empty() {
        format!("+{}+", "-".repeat(inner))
    } else {
        let label = sanitize_and_clip(&format!("-- {title} "), inner);
        format!("+{label}{}+", "-".repeat(inner.saturating_sub(label.len())))
    };
    if color {
        format!("{CYAN}{plain}{RESET}")
    } else {
        plain
    }
}

fn row(content: &str, inner: usize) -> String {
    let content = sanitize_and_clip(content, inner.saturating_sub(2));
    format!("| {content:<width$} |", width = inner.saturating_sub(2))
}

fn row_colored(content: &str, inner: usize, paint: &str, color: bool) -> String {
    let plain = row(content, inner);
    if color {
        format!("{paint}{plain}{RESET}")
    } else {
        plain
    }
}

#[allow(clippy::cast_precision_loss)]
fn meter(label: &str, value: Option<f64>, inner: usize, color: bool) -> String {
    meter_impl(label, value, inner, color, None)
}

fn meter_fixed(label: &str, value: Option<f64>, inner: usize, color: bool, paint: &str) -> String {
    meter_impl(label, value, inner, color, Some(paint))
}

#[allow(clippy::cast_precision_loss)]
fn meter_impl(
    label: &str,
    value: Option<f64>,
    inner: usize,
    color: bool,
    fixed_paint: Option<&str>,
) -> String {
    let value_text = value.map_or_else(|| "N/A".to_owned(), |value| format!("{value:3.0}%"));
    let label = format!("{label:<11}");
    let bar_width = inner
        .saturating_sub(2)
        .saturating_sub(label.len() + value_text.len() + 3)
        .max(4);
    let percent = value.unwrap_or(0.0).clamp(0.0, 100.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = (percent * bar_width as f64 / 100.0).round() as usize;
    let plain_bar = format!("{}{}", "#".repeat(filled), "-".repeat(bar_width - filled));
    if color {
        let paint = fixed_paint.unwrap_or(if percent >= 90.0 {
            RED
        } else if percent >= 75.0 {
            YELLOW
        } else {
            GREEN
        });
        format!(
            "| {label}[{paint}{plain_bar}{RESET}] {value_text}{} |",
            " ".repeat(
                inner
                    .saturating_sub(2)
                    .saturating_sub(label.len() + bar_width + value_text.len() + 3)
            )
        )
    } else {
        row(&format!("{label}[{plain_bar}] {value_text}"), inner)
    }
}

fn sanitize_and_clip(value: &str, width: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(width)
        .collect()
}

// Capacity occupancy, never memory-controller/bus utilization.
fn vram_capacity_percent(used: Option<u64>, total: Option<u64>) -> Option<f64> {
    Some(percent(used?, total.filter(|bytes| *bytes > 0)?))
}

#[allow(clippy::cast_precision_loss)]
fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (100.0 * value as f64 / total as f64).clamp(0.0, 100.0)
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

fn format_optional(value: Option<f64>, unit: &str) -> String {
    value.map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} {unit}"))
}

fn unique_id(prefix: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    format!("{prefix}-{stamp}-{}", std::process::id())
}

fn write_new_json(path: &Path, value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::float_cmp)]
    fn bottleneck_summary_buckets_by_cause_and_tracks_the_last_one() {
        use gpu_engine::bottleneck::BottleneckCause as C;
        let mut summary = BottleneckSummary::default();
        summary.record(C::None, 1.0);
        summary.record(C::VerifierCpuBacklog, 2.0);
        summary.record(C::VerifierCpuBacklog, 1.5);
        summary.record(C::MemoryPressure, 0.5);
        assert_eq!(summary.samples, 4);
        assert_eq!(summary.seconds_none, 1.0);
        assert_eq!(summary.seconds_verifier_cpu_backlog, 3.5);
        assert_eq!(summary.seconds_memory_pressure, 0.5);
        assert_eq!(summary.seconds_pcie_transfer, 0.0);
        assert_eq!(summary.last_cause, C::MemoryPressure);
        // A fresh summary starts empty, not with a leftover cause from some other run.
        assert_eq!(BottleneckSummary::default().last_cause, C::None);
    }

    fn parse(extra: &[&str]) -> RunArgs {
        let mut words = vec!["gpu-vram-dit", "run", "--backend", "cuda", "--no-ui"];
        words.extend(extra);
        let Command::Run(parsed) = Cli::parse_from(words).command else {
            unreachable!("only the run subcommand is parsed here");
        };
        parsed
    }

    fn fixture_device(vendor_id: u32) -> VulkanDeviceInfo {
        VulkanDeviceInfo {
            index: 0,
            name: "fixture".into(),
            device_type: "DISCRETE_GPU".into(),
            hardware_eligible: true,
            rejection_reason: None,
            vendor_id,
            device_id: 0,
            api_version: "fixture".into(),
            driver_version_raw: 0,
            uuid: "fixture".into(),
            pci_address: Some("0000:01:00.0".into()),
            compute_queue_family: None,
            device_local_bytes: 1024,
            budget_bytes: None,
            usage_bytes: None,
            memory_budget_supported: false,
        }
    }

    #[test]
    fn async_auto_runs_cuda_devices_async_and_the_others_sequential() {
        // Exactly what dit-orchestrator's boot menu passes (`gpu_run_arguments`).
        let Command::Run(menu) = Cli::parse_from([
            "gpu-vram-dit",
            "run",
            "--output-dir",
            "/run/dit-suite/x/gpu-vram",
            "--progress-file",
            "/run/dit-suite/x/gpu-vram/watchdog-heartbeat.json",
            "--async-auto",
            "--bottleneck-log",
            "/run/dit-suite/x/gpu-vram/bottleneck.jsonl",
        ])
        .command
        else {
            unreachable!("only the run subcommand is parsed here");
        };

        let nvidia = pipeline_for_device(&menu, Some(&fixture_device(0x10de)));
        assert!(nvidia.async_compute && !nvidia.async_auto);
        assert_eq!(matrix_jobs(&nvidia), ASYNC_AUTO_MATRIX_JOBS);
        assert!(nvidia.bottleneck_log.is_some() && !nvidia.lane.vram_local);
        assert!(overlap_active(&nvidia) && lane_active(&nvidia));
        validate_args(&nvidia).expect("the menu's CUDA run must validate");

        // Intel/AMD: sequential, every async-only option dropped rather than rejected.
        let mut menu_with_lane = menu.clone();
        menu_with_lane.lane.vram_local = true;
        let amd = pipeline_for_device(&menu_with_lane, Some(&fixture_device(0x1002)));
        assert!(!amd.async_compute && !amd.async_auto);
        assert!(!amd.lane.any() && amd.bottleneck_log.is_none());
        assert_eq!(matrix_jobs(&amd), DEFAULT_MATRIX_JOBS);
        validate_args(&amd).expect("the menu's Vulkan run must validate");

        // Explicit choices win: jobs, and --backend vulkan on an NVIDIA card stays sequential.
        let mut explicit = menu.clone();
        explicit.matrix_jobs = Some(128);
        assert_eq!(
            matrix_jobs(&pipeline_for_device(
                &explicit,
                Some(&fixture_device(0x10de))
            )),
            128
        );
        let mut vulkan = menu.clone();
        vulkan.backend = BackendArg::Vulkan;
        assert!(!pipeline_for_device(&vulkan, Some(&fixture_device(0x10de))).async_compute);
        // No device (mock) is sequential; runs without --async-auto are left as they are.
        assert!(!pipeline_for_device(&menu, None).async_compute);
        let plain = parse(&["--async-compute"]);
        assert!(pipeline_for_device(&plain, Some(&fixture_device(0x1002))).async_compute);
        assert!(
            Cli::try_parse_from(["gpu-vram-dit", "run", "--async-auto", "--async-compute"])
                .is_err()
        );
    }

    #[test]
    fn compute_mix_selects_the_gemm_arithmetic_for_async_runs() {
        let mix = |extra: &[&str]| async_config(&parse(extra)).arith;
        assert_eq!(mix(&["--async-compute"]), gpu_cuda::ArithMix::U32);
        assert_eq!(
            mix(&["--async-compute", "--compute-mix", "fp32"]),
            gpu_cuda::ArithMix::F32
        );
        assert_eq!(
            mix(&["--async-compute", "--compute-mix", "alternate"]),
            gpu_cuda::ArithMix::Alternate
        );
        assert!(
            validate_args(&parse(&["--compute-mix", "fp32"])).is_err(),
            "async only"
        );
        assert!(validate_args(&parse(&["--async-compute", "--compute-mix", "alternate"])).is_ok());
        // Alternating needs corpus rotations to alternate at.
        let no_rotation = parse(&[
            "--async-compute",
            "--compute-mix",
            "alternate",
            "--corpus-rotate-batches",
            "0",
        ]);
        assert!(validate_args(&no_rotation).is_err());
        assert_eq!(
            mix(&["--async-compute", "--compute-mix", "int8"]),
            gpu_cuda::ArithMix::I8
        );
        assert_eq!(
            mix(&["--async-compute", "--compute-mix", "all"]),
            gpu_cuda::ArithMix::All
        );
        let all_without_rotation = parse(&[
            "--async-compute",
            "--compute-mix",
            "all",
            "--corpus-rotate-batches",
            "0",
        ]);
        assert!(validate_args(&all_without_rotation).is_err());
    }

    #[test]
    fn vram_sweep_needs_the_overlapped_lane_and_sane_shares() {
        let ok = |extra: &[&str]| validate_args(&parse(extra)).is_ok();
        assert!(ok(&["--async-compute", "--vram-sweep"]));
        assert!(ok(&[
            "--async-compute",
            "--vram-sweep",
            "--vram-sweep-duty",
            "1.0"
        ]));
        assert!(ok(&[
            "--async-compute",
            "--vram-sweep",
            "--vram-sweep-share",
            "0.25"
        ]));
        let config = async_config(&parse(&[
            "--async-compute",
            "--vram-sweep",
            "--vram-sweep-duty",
            "0.5",
        ]));
        assert!(config.sweep.enabled && config.sweep.duty == Some(0.5));
        // It trades pool halves with the overlapped lane, so it needs that lane.
        assert!(!ok(&["--vram-sweep"]), "async only");
        assert!(!ok(&[
            "--async-compute",
            "--vram-sweep",
            "--no-transfer-overlap"
        ]));
        assert!(!ok(&[
            "--async-compute",
            "--vram-sweep",
            "--coverage-floor",
            "0.25"
        ]));
        assert!(!ok(&[
            "--async-compute",
            "--vram-sweep",
            "--workload-profile",
            "compute-only"
        ]));
        assert!(!ok(&[
            "--async-compute",
            "--vram-sweep",
            "--vram-sweep-duty",
            "0.01"
        ]));
        assert!(!ok(&[
            "--async-compute",
            "--vram-sweep",
            "--vram-sweep-share",
            "1.5"
        ]));
        assert!(
            Cli::try_parse_from(["gpu-vram-dit", "run", "--vram-sweep", "--vram-local"]).is_err()
        );
        // A Vulkan device under --async-auto drops it with the other async-only options.
        let Command::Run(menu) =
            Cli::parse_from(["gpu-vram-dit", "run", "--async-auto", "--vram-sweep"]).command
        else {
            unreachable!("only the run subcommand is parsed here");
        };
        assert!(
            !pipeline_for_device(&menu, Some(&fixture_device(0x1002)))
                .lane
                .vram_sweep
        );
        assert!(
            pipeline_for_device(&menu, Some(&fixture_device(0x10de)))
                .lane
                .vram_sweep
        );
    }

    #[test]
    fn vram_max_fills_to_the_driver_limit_minus_the_margin_on_discrete_gpus() {
        let discrete = fixture_device(0x10de);
        let mut integrated = fixture_device(0x8086);
        integrated.device_type = "INTEGRATED_GPU".into();
        let gib = 1024 * MIB;
        let max = parse(&["--vram-max"]);
        let fill = vram_fill_plan(&max, &discrete, Some(7 * gib), 5 * gib);
        assert_eq!(fill.mode, "max");
        assert_eq!(fill.target_bytes, 7 * gib - DEFAULT_VRAM_MARGIN_MIB * MIB);
        let custom = parse(&["--vram-max", "--vram-margin-mib", "512"]);
        assert_eq!(
            vram_fill_plan(&custom, &discrete, Some(7 * gib), 5 * gib).target_bytes,
            7 * gib - 512 * MIB
        );
        // No live reading (Vulkan) or an integrated GPU: the planned percentage stands.
        for (device, free) in [(&discrete, None), (&integrated, Some(7 * gib))] {
            let kept = vram_fill_plan(&max, device, free, 5 * gib);
            assert_eq!((kept.mode, kept.target_bytes), ("percent", 5 * gib));
            assert!(kept.note.is_some());
        }
        assert_eq!(
            vram_fill_plan(&parse(&[]), &discrete, Some(7 * gib), 5 * gib).mode,
            "percent"
        );
        assert!(validate_args(&parse(&["--vram-max", "--vram-margin-mib", "8"])).is_err());
        assert!(
            Cli::try_parse_from(["gpu-vram-dit", "run", "--vram-max", "--vram-mib", "64"]).is_err()
        );
        assert!(Cli::try_parse_from(["gpu-vram-dit", "run", "--vram-margin-mib", "256"]).is_err());
    }

    #[test]
    fn async_defaults_separate_device_and_host_slots_and_flags_override_them() {
        let default = async_config(&parse(&["--async-compute"]));
        assert_eq!((default.device_slots, default.host_slots), (3, 8));
        assert_eq!((default.quarantine_slots, default.batches), (1, 32));
        assert!(default.host_slots > default.device_slots);
        assert!(default.workers >= 1 && default.corpus_threads >= 1);
        let tuned = async_config(&parse(&[
            "--async-compute",
            "--async-device-slots",
            "2",
            "--async-host-slots",
            "5",
            "--async-batches",
            "8",
            "--quarantine-slots",
            "2",
            "--corpus-threads",
            "3",
        ]));
        assert_eq!((tuned.device_slots, tuned.host_slots), (2, 5));
        assert_eq!(
            (tuned.batches, tuned.quarantine_slots, tuned.corpus_threads),
            (8, 2, 3)
        );
        // Workers never exceed the host slots, whatever was asked for.
        let many = async_config(&parse(&[
            "--async-compute",
            "--async-workers",
            "64",
            "--async-host-slots",
            "3",
            "--async-device-slots",
            "2",
        ]));
        assert!(many.workers <= 3);
    }

    #[test]
    fn async_flags_are_validated_before_any_hardware_is_touched() {
        let reject = |extra: &[&str]| validate_args(&parse(extra)).is_err();
        assert!(!reject(&["--async-compute"]));
        assert!(!reject(&[
            "--async-compute",
            "--async-host-slots",
            "12",
            "--async-device-slots",
            "4"
        ]));
        // Async-only flags need --async-compute.
        assert!(reject(&["--async-host-slots", "8"]));
        assert!(reject(&["--quarantine-slots", "1"]));
        assert!(reject(&["--corpus-threads", "2"]));
        assert!(reject(&["--bottleneck-log", "trace.jsonl"]));
        assert!(!reject(&[
            "--async-compute",
            "--bottleneck-log",
            "trace.jsonl"
        ]));
        // The host queue must be deeper than the GPU, quarantine must leave a slot free.
        assert!(reject(&[
            "--async-compute",
            "--async-host-slots",
            "3",
            "--async-device-slots",
            "3"
        ]));
        assert!(reject(&["--async-compute", "--async-device-slots", "1"]));
        assert!(reject(&["--async-compute", "--async-host-slots", "65"]));
        assert!(reject(&["--async-compute", "--quarantine-slots", "8"]));
        assert!(reject(&["--async-compute", "--async-batches", "0"]));
        assert!(reject(&["--async-compute", "--async-workers", "0"]));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn lane_flags_are_validated_forwarded_and_turn_into_a_lane_config() {
        let reject = |extra: &[&str]| validate_args(&parse(extra)).is_err();
        // The lane belongs to the async path and needs a workload to run.
        assert!(reject(&["--transfer-overlap"]));
        assert!(reject(&["--vram-local"]));
        assert!(reject(&["--workload-profile", "compute-only"]));
        assert!(!reject(&["--async-compute", "--transfer-overlap"]));
        assert!(!reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--vram-local",
            "--lane-slots",
            "3",
            "--lane-chunk-mib",
            "4",
            "--lane-rate-mib-s",
            "500",
            "--lane-weight",
            "2",
            "--vram-passes",
            "8"
        ]));
        // Overlap replaces the time-slice floor and leaves nothing to do in compute-only.
        assert!(reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--coverage-floor",
            "0.3"
        ]));
        assert!(!reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--coverage-floor",
            "0"
        ]));
        assert!(reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--workload-profile",
            "compute-only"
        ]));
        assert!(reject(&[
            "--async-compute",
            "--vram-local",
            "--workload-profile",
            "compute-only"
        ]));
        assert!(!reject(&[
            "--async-compute",
            "--workload-profile",
            "compute-only"
        ]));
        // Overlap is now the default: `--transfer-overlap` and `--no-transfer-overlap` contradict
        // each other, but `--no-transfer-overlap` alone is a valid way back to the old default.
        assert!(reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--no-transfer-overlap"
        ]));
        assert!(!reject(&["--async-compute", "--no-transfer-overlap"]));
        // Tuning without any lane active, and values outside the tested ranges, are refused.
        // `--lane-slots` alone now has a lane (overlap defaults on), unlike before H6.
        assert!(!reject(&["--async-compute", "--lane-slots", "2"]));
        assert!(reject(&[
            "--async-compute",
            "--no-transfer-overlap",
            "--lane-slots",
            "2"
        ]));
        assert!(reject(&[
            "--async-compute",
            "--workload-profile",
            "compute-only",
            "--lane-slots",
            "2"
        ]));
        assert!(reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--lane-slots",
            "5"
        ]));
        assert!(reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--lane-chunk-mib",
            "0"
        ]));
        assert!(reject(&[
            "--async-compute",
            "--transfer-overlap",
            "--lane-chunk-mib",
            "65"
        ]));
        assert!(reject(&[
            "--async-compute",
            "--vram-local",
            "--vram-passes",
            "0"
        ]));
        // A matrix fault needs compute.
        assert!(reject(&[
            "--async-compute",
            "--workload-profile",
            "transfer-only",
            "--inject-hardware-fault",
            "matrix"
        ]));

        let args = parse(&[
            "--async-compute",
            "--transfer-overlap",
            "--vram-local",
            "--lane-slots",
            "3",
            "--lane-chunk-mib",
            "4",
            "--lane-rate-mib-s",
            "500",
            "--lane-weight",
            "2",
            "--vram-passes",
            "8",
        ]);
        let config = async_config(&args).lane;
        assert!(config.enabled && config.vram_local);
        assert_eq!(
            (
                config.slots,
                config.chunk_bytes,
                config.rate_bytes_per_second
            ),
            (3, 4 * MIB, 500 * MIB)
        );
        assert_eq!((config.lane_weight, config.vram_passes), (2, 8));
        // Overlap is on by default for `--async-compute`; an explicit coverage floor opts back
        // into the old time-sliced default instead, and compute-only never builds a lane.
        assert!(async_config(&parse(&["--async-compute"])).lane.active());
        assert!(lane_active(&parse(&["--async-compute"])));
        assert!(!lane_active(&parse(&[
            "--async-compute",
            "--coverage-floor",
            "0.4"
        ])));
        assert!(!lane_active(&parse(&[
            "--async-compute",
            "--no-transfer-overlap"
        ])));
        let compute_only = parse(&["--async-compute", "--workload-profile", "compute-only"]);
        assert!(!lane_active(&compute_only) && !async_config(&compute_only).lane.active());
        // Every flag reaches a per-GPU worker process.
        let mut command = std::process::Command::new("gpu-vram-dit");
        args.lane.forward(&mut command);
        let forwarded: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        for expected in [
            "--transfer-overlap",
            "--vram-local",
            "--lane-slots",
            "3",
            "--lane-chunk-mib",
            "4",
            "--lane-rate-mib-s",
            "500",
            "--lane-weight",
            "2",
            "--vram-passes",
            "8",
        ] {
            assert!(forwarded.iter().any(|a| a == expected), "{expected}");
        }
        let mut profiled = std::process::Command::new("gpu-vram-dit");
        parse(&["--async-compute", "--workload-profile", "transfer-only"])
            .lane
            .forward(&mut profiled);
        let forwarded: Vec<_> = profiled.get_args().collect();
        assert_eq!(forwarded, ["--workload-profile", "transfer-only"]);
        let mut opted_out = std::process::Command::new("gpu-vram-dit");
        parse(&["--async-compute", "--no-transfer-overlap"])
            .lane
            .forward(&mut opted_out);
        assert_eq!(
            opted_out.get_args().collect::<Vec<_>>(),
            ["--no-transfer-overlap"]
        );
    }

    #[test]
    fn lane_totals_fold_both_directions_and_never_count_touched_bytes_as_coverage() {
        assert_eq!(LaneTotals::from_report(None).bytes_verified, 0);
        let mut report = gpu_cuda::LaneReport::default();
        report.config.enabled = true;
        report.transfer.write_ops = 10;
        report.transfer.reread_ops = 9;
        report.transfer.h2d_bytes = 100;
        report.transfer.d2h_bytes = 190;
        report.transfer.verified_bytes = 190;
        report.transfer.sweeps = 3;
        report.vram.write_ops = 4;
        report.vram.checkpoint_ops = 2;
        report.vram.touched_bytes = 1_000_000;
        report.vram.cpu_verified_bytes = 50;
        let totals = LaneTotals::from_report(Some(&report));
        assert_eq!(
            (totals.transfer_operations, totals.retention_checks),
            (14, 11)
        );
        // Coverage is what the CPU compared; device-side touches are activity only.
        assert_eq!(totals.bytes_verified, 240);
        assert_eq!(totals.transfer_bytes, 340);
        assert_eq!(totals.sweeps, 3);
    }

    #[test]
    fn seed_quantum_matches_thirty_and_sixty_minute_plan() {
        assert_eq!(1_800 / SEED_QUANTUM_SECONDS, 10);
        assert_eq!(3_600 / SEED_QUANTUM_SECONDS, 20);
        assert_ne!(rotate_seed(7, 9), rotate_seed(7, 10));
    }

    #[test]
    fn allocation_uses_budget_headroom_and_hard_cap() {
        let info = VulkanDeviceInfo {
            index: 0,
            name: "test".to_owned(),
            device_type: "DISCRETE_GPU".to_owned(),
            hardware_eligible: true,
            rejection_reason: None,
            vendor_id: 1,
            device_id: 2,
            api_version: "1.1.0".to_owned(),
            driver_version_raw: 1,
            uuid: "00".repeat(16),
            pci_address: None,
            compute_queue_family: Some(0),
            device_local_bytes: 1_000,
            budget_bytes: Some(900),
            usage_bytes: Some(100),
            memory_budget_supported: true,
        };
        let mut args = RunArgs {
            duration_seconds: 1,
            generations: Some(1),
            backend: BackendArg::Vulkan,
            device_index: None,
            devices: None,
            vram_percent: 95,
            vram_mib: None,
            max_allocation_mib: 4,
            matrix_jobs: Some(1),
            async_auto: false,
            vram_max: false,
            vram_margin_mib: None,
            compute_mix: None,
            compute_rounds: Some(1),
            recreate_compute_resources: false,
            async_compute: false,
            hash_policy: None,
            corpus_rotate_batches: None,
            async_device_slots: None,
            async_host_slots: None,
            async_workers: None,
            async_workers_max: None,
            coverage_floor: None,
            bottleneck_log: None,
            lane: LaneArgs::default(),
            async_batches: None,
            corpus_threads: None,
            quarantine_slots: None,
            blocks_per_generation: 1,
            seed: 1,
            output_dir: None,
            no_ui: true,
            stop_on_error: false,
            temp_limit_celsius: None,
            inject_fault: None,
            inject_vulkan_fault: None,
            progress_file: None,
            fault_events: None,
            boundary_checks: false,
        };
        assert_eq!(allocation_target_bytes(&info, &args), 760);
        args.vram_mib = Some(64);
        assert_eq!(allocation_target_bytes(&info, &args), 64 * MIB);
    }

    #[test]
    fn integrated_gpu_uses_conservative_shared_ram_cap_unless_explicitly_bounded() {
        let info = VulkanDeviceInfo {
            index: 0,
            name: "integrated".to_owned(),
            device_type: "INTEGRATED_GPU".to_owned(),
            hardware_eligible: true,
            rejection_reason: None,
            vendor_id: 1,
            device_id: 2,
            api_version: "1.1.0".to_owned(),
            driver_version_raw: 1,
            uuid: "00".repeat(16),
            pci_address: None,
            compute_queue_family: Some(0),
            device_local_bytes: 1_000,
            budget_bytes: Some(900),
            usage_bytes: Some(100),
            memory_budget_supported: true,
        };
        let mut args = RunArgs {
            duration_seconds: 1,
            generations: Some(1),
            backend: BackendArg::Vulkan,
            device_index: None,
            devices: None,
            vram_percent: 95,
            vram_mib: None,
            max_allocation_mib: 4,
            matrix_jobs: Some(1),
            async_auto: false,
            vram_max: false,
            vram_margin_mib: None,
            compute_mix: None,
            compute_rounds: Some(1),
            recreate_compute_resources: false,
            async_compute: false,
            hash_policy: None,
            corpus_rotate_batches: None,
            async_device_slots: None,
            async_host_slots: None,
            async_workers: None,
            async_workers_max: None,
            coverage_floor: None,
            bottleneck_log: None,
            lane: LaneArgs::default(),
            async_batches: None,
            corpus_threads: None,
            quarantine_slots: None,
            blocks_per_generation: 1,
            seed: 1,
            output_dir: None,
            no_ui: true,
            stop_on_error: false,
            temp_limit_celsius: None,
            inject_fault: None,
            inject_vulkan_fault: None,
            progress_file: None,
            fault_events: None,
            boundary_checks: false,
        };
        assert_eq!(effective_vram_percent(&info, &args), 60);
        assert_eq!(allocation_target_bytes(&info, &args), 480);
        args.vram_mib = Some(64);
        assert_eq!(effective_vram_percent(&info, &args), 95);
        assert_eq!(allocation_target_bytes(&info, &args), 64 * MIB);
    }

    #[test]
    fn compute_rounds_stay_flat_below_threshold_then_ramp_and_cap() {
        assert_eq!(suggested_compute_rounds(0), DEFAULT_COMPUTE_ROUNDS);
        assert_eq!(
            suggested_compute_rounds(COMPUTE_ROUNDS_RAMP_THRESHOLD_SECONDS - 1),
            DEFAULT_COMPUTE_ROUNDS
        );
        assert_eq!(
            suggested_compute_rounds(COMPUTE_ROUNDS_RAMP_THRESHOLD_SECONDS),
            DEFAULT_COMPUTE_ROUNDS
        );
        assert!(
            suggested_compute_rounds(COMPUTE_ROUNDS_RAMP_THRESHOLD_SECONDS + 1)
                > DEFAULT_COMPUTE_ROUNDS
        );
        assert_eq!(suggested_compute_rounds(u64::MAX), MAX_AUTO_COMPUTE_ROUNDS);
    }

    #[test]
    fn explicit_compute_rounds_overrides_the_duration_derived_ramp() {
        let mut args = RunArgs {
            duration_seconds: 3_600,
            generations: None,
            backend: BackendArg::Mock,
            device_index: None,
            devices: None,
            vram_percent: 85,
            vram_mib: None,
            max_allocation_mib: 4,
            matrix_jobs: Some(1),
            async_auto: false,
            vram_max: false,
            vram_margin_mib: None,
            compute_mix: None,
            compute_rounds: None,
            recreate_compute_resources: false,
            async_compute: false,
            hash_policy: None,
            corpus_rotate_batches: None,
            async_device_slots: None,
            async_host_slots: None,
            async_workers: None,
            async_workers_max: None,
            coverage_floor: None,
            bottleneck_log: None,
            lane: LaneArgs::default(),
            async_batches: None,
            corpus_threads: None,
            quarantine_slots: None,
            blocks_per_generation: 1,
            seed: 1,
            output_dir: None,
            no_ui: true,
            stop_on_error: false,
            temp_limit_celsius: None,
            inject_fault: None,
            inject_vulkan_fault: None,
            progress_file: None,
            fault_events: None,
            boundary_checks: false,
        };
        assert_eq!(
            resolved_compute_rounds(&args),
            suggested_compute_rounds(3_600)
        );
        args.compute_rounds = Some(7);
        assert_eq!(resolved_compute_rounds(&args), 7);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn dashboard_marks_hardware_state_and_dynamic_progress() {
        let mut state = GpuUiState {
            async_pipeline: false,
            hardware_note: None,
            sweep_note: None,
            mix_note: None,
            throttle_row: None,
            throttle_alert: None,
            system: SystemInfo {
                cpu: "CPU".to_owned(),
                memory_total_bytes: 16 * 1024 * MIB,
                gpu: "GPU".to_owned(),
                board: "Board".to_owned(),
                logical_threads: 16,
                kernel: "test".to_owned(),
            },
            live: LiveUtilization::default(),
            devices: vec![GpuDevice {
                pci_address: "0000:01:00.0".to_owned(),
                model: "Test GPU".to_owned(),
                driver: Some("test-driver".to_owned()),
                kernel_modules: vec!["test-driver".to_owned()],
                vram_total_bytes: Some(8 * 1024 * MIB),
            }],
            telemetry: vec![GpuTelemetry {
                pci_address: "0000:01:00.0".to_owned(),
                utilization_percent: Some(73.0),
                memory_utilization_percent: Some(61.0),
                vram_used_bytes: Some(4 * 1024 * MIB),
                vram_total_bytes: Some(8 * 1024 * MIB),
                temperature_celsius: Some(67.5),
                power_watts: Some(175.0),
                core_clock_mhz: Some(2500.0),
                memory_clock_mhz: Some(1200.0),
                power_limit_watts: None,
                throttle_reasons_mask: None,
                source: "test-provider".to_owned(),
            }],
            duration_seconds: 60,
            exact_generations: None,
            elapsed: Duration::from_secs(30),
            generation: 4,
            seed_index: 0,
            seed: 1,
            backend: "VULKAN GPU-B/C/D".to_owned(),
            hardware_active: true,
            selected: Some(VulkanDeviceInfo {
                index: 0,
                name: "Test GPU".to_owned(),
                device_type: "DISCRETE_GPU".to_owned(),
                hardware_eligible: true,
                rejection_reason: None,
                vendor_id: 1,
                device_id: 2,
                api_version: "1.3".to_owned(),
                driver_version_raw: 1,
                uuid: "00".repeat(16),
                pci_address: Some("0000:01:00.0".to_owned()),
                compute_queue_family: Some(0),
                device_local_bytes: 8 * 1024 * MIB,
                budget_bytes: Some(7 * 1024 * MIB),
                usage_bytes: Some(1024 * MIB),
                memory_budget_supported: true,
            }),
            allocation_bytes: 8 * MIB,
            allocation_target_bytes: 8 * MIB,
            chunk_bytes: MIB,
            transfer_mib_s: Some(100.0),
            sweeps: 1,
            blocks_verified: 0,
            bytes_verified: MIB,
            calculations_verified: 64,
            failures: 0,
            temp_limit: None,
            aborted: false,
            finished: false,
        };
        let frame = render_gpu_ui(
            &state,
            TerminalSize {
                cols: 140,
                rows: 40,
            },
            false,
        );
        assert!(frame.contains("HARDWARE ACTIVE"));
        assert!(frame.contains("SYSTEM HARDWARE"));
        assert!(frame.contains("LIVE UTILIZATION"));
        assert!(frame.contains("THREADS:"));
        assert!(frame.contains("GPU USAGE"));
        assert!(frame.contains("VRAM"));
        assert!(frame.contains("Used 4.00 GiB / 8.00 GiB"));
        // Labels are padded so the bars of the panel line up: "VRAM" then spaces then "[".
        let vram_meter = frame
            .lines()
            .find(|line| {
                line.find("VRAM ")
                    .is_some_and(|at| line[at + 4..].trim_start().starts_with('['))
            })
            .unwrap();
        assert!(vram_meter.contains("50%"), "{vram_meter}");
        assert_eq!(progress_snapshot(&state).vram_usage_percent, Some(50.0));
        assert!(frame.contains("DIT TARGET"));
        assert!(frame.contains("MATRIX RESULTS"));
        assert!(frame.contains("67.5 C"));
        assert!(frame.contains("175.0 W"));
        assert!(frame.contains("test-provider"));
        assert!(frame.lines().all(|line| line.chars().count() == 140));
        for cols in [90, 140] {
            let legacy = render_gpu_ui_legacy(&state, TerminalSize { cols, rows: 40 }, false);
            assert!(legacy.contains("Used 4.00 GiB / 8.00 GiB"));
        }
        state.telemetry[0].vram_total_bytes = None;
        assert_eq!(progress_snapshot(&state).vram_usage_percent, Some(50.0));
        state.telemetry[0].vram_used_bytes = None;
        // A bus activity sample alone must not masquerade as capacity usage.
        assert_eq!(progress_snapshot(&state).vram_usage_percent, None);

        // Corrected hardware events: data matched, but the PASS must not look clean.
        state.finished = true;
        state.failures = 0;
        state.hardware_note = Some("0 error(s), 1 warning(s): NVIDIA Xid 13".to_owned());
        let flagged = render_gpu_ui(
            &state,
            TerminalSize {
                cols: 140,
                rows: 40,
            },
            false,
        );
        assert!(flagged.contains("PASS (HW WARNINGS)"), "{flagged}");
        assert!(flagged.contains("HARDWARE      0 error(s), 1 warning(s): NVIDIA Xid 13"));
        let snapshot = progress_snapshot(&state);
        assert_eq!(
            snapshot.state, "PASS",
            "the progress state keeps its fixed values"
        );
        assert!(snapshot.hardware_note.is_some());
    }

    #[test]
    fn vram_capacity_requires_used_bytes_and_nonzero_total() {
        const GIB: u64 = 1024 * MIB;
        assert_eq!(
            vram_capacity_percent(Some(7 * GIB), Some(8 * GIB)),
            Some(87.5)
        );
        assert_eq!(vram_capacity_percent(None, Some(8 * GIB)), None);
        assert_eq!(vram_capacity_percent(Some(GIB), None), None);
        assert_eq!(vram_capacity_percent(Some(GIB), Some(0)), None);
    }

    #[test]
    fn deep_scheduler_does_not_finish_early_and_sweeps_require_every_allocation() {
        let args = RunArgs {
            duration_seconds: 1_800,
            generations: None,
            backend: BackendArg::Mock,
            device_index: None,
            devices: None,
            vram_percent: 85,
            vram_mib: None,
            max_allocation_mib: 4,
            matrix_jobs: Some(1),
            async_auto: false,
            vram_max: false,
            vram_margin_mib: None,
            compute_mix: None,
            compute_rounds: Some(1),
            recreate_compute_resources: false,
            async_compute: false,
            hash_policy: None,
            corpus_rotate_batches: None,
            async_device_slots: None,
            async_host_slots: None,
            async_workers: None,
            async_workers_max: None,
            coverage_floor: None,
            bottleneck_log: None,
            lane: LaneArgs::default(),
            async_batches: None,
            corpus_threads: None,
            quarantine_slots: None,
            blocks_per_generation: 1,
            seed: 1,
            output_dir: None,
            no_ui: true,
            stop_on_error: false,
            temp_limit_celsius: None,
            inject_fault: None,
            inject_vulkan_fault: None,
            progress_file: None,
            fault_events: None,
            boundary_checks: false,
        };
        assert!(!should_stop(&args, u64::MAX, Duration::from_secs(1_799)));
        assert!(should_stop(&args, 0, Duration::from_mins(30)));
        assert_eq!(completed_pool_sweeps(&[3, 1, 2]), 1);
        assert_eq!(completed_pool_sweeps(&[3, 2, 2]), 2);
        assert_eq!(final_verdict(true, 0), Verdict::Aborted);
        assert_eq!(final_verdict(false, 1), Verdict::Fail);
        assert!(ranges_overlap(0, 8, 4, 12));
        assert!(!ranges_overlap(0, 4, 4, 8));
    }
}
