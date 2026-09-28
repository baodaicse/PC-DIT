use std::{
    fs::{self, File},
    io::{self, BufWriter},
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use dit_core::{
    FaultInjection, RunReport, RunStatus, load_golden_vector, load_verified_source, run_reference,
};
use dit_system::{
    ComputeOracleConfig, ComputeOracleReport, ComputeOracleStatus, EdacPolicy, LockPolicy,
    MemoryTarget, SafetyFaultInjection, SafetyPolicy, SeedSource, SensorReport, SystemRunConfig,
    SystemRunReport, collect_sensor_report, format_gpu_summary, run_compute_oracle, run_system,
};

#[derive(Debug, Parser)]
#[command(name = "dit-cli", version, about = "CPU-DIT-01 reference runner")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the separate synchronized availability/power-platform campaign.
    PowerPlatformRun(PowerArgs),
    /// Print the supervisor budget for a power timing policy, without running work.
    PowerPlatformWatchdogSeconds {
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u64).range(0..=60000))]
        idle_ms: u64,
        #[arg(long, default_value_t = 5000, value_parser = clap::value_parser!(u64).range(1..=120000))]
        timeout_ms: u64,
        #[arg(long)]
        campaign: bool,
    },
    /// Recover evidence from an interrupted power campaign (worker must be stopped).
    PowerPlatformRecover {
        checkpoint: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Run the deterministic reference CPU-RAM pipeline.
    Run(RunArgs),
    /// Run adaptive mmap RAM coverage and multi-core CPU checks.
    SystemRun(SystemRunArgs),
    /// Run the standalone sustained compute-oracle (ADR-0004, Phase 1).
    ComputeOracleRun(ComputeOracleArgs),
    /// Show CPU, RAM, temperature, power and Linux driver evidence.
    Sensors(SensorArgs),
}

#[derive(Debug, Args)]
struct PowerArgs {
    /// Unified campaign: finish a complete coverage round after this minimum time.
    /// Omit to retain the legacy count-based diagnostic modes.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=86400), conflicts_with_all = ["core_sweep", "transitions"])]
    minimum_duration_seconds: Option<u64>,
    /// Worker-owned heartbeat for the independent supervisor.
    #[arg(long)]
    progress_report: Option<PathBuf>,
    #[arg(long)]
    enable_temperature_limit: bool,
    #[arg(long)]
    core_sweep: bool,
    #[arg(long)]
    run_id: String,
    #[arg(long, default_value_t = 2)]
    workers: usize,
    #[arg(long, default_value_t = 10)]
    transitions: u32,
    #[arg(long, default_value_t = 500)]
    idle_ms: u64,
    #[arg(long, default_value_t = 100)]
    burst_ms: u64,
    #[arg(long, default_value_t = 5000)]
    timeout_ms: u64,
    #[arg(long, default_value_t = 50)]
    max_start_spread_ms: u64,
    #[arg(long, default_value_t = 200_000)]
    fma_iterations: u32,
    #[arg(long, value_parser = parse_seed)]
    seed: Option<u64>,
    #[arg(long)]
    checkpoint: PathBuf,
}

fn power_platform_run(args: &PowerArgs) -> u8 {
    let mut bytes = [0_u8; 8];
    let seed = if let Some(seed) = args.seed {
        seed
    } else {
        if let Err(error) = getrandom::fill(&mut bytes) {
            eprintln!("INVALID: {error}");
            return 3;
        }
        u64::from_le_bytes(bytes)
    };
    let config = dit_system::power_platform::PowerConfig {
        minimum_duration_ms: args.minimum_duration_seconds.unwrap_or(0) * 1000,
        progress_report: args.progress_report.clone(),
        temperature_limit_enabled: args.enable_temperature_limit,
        fault: None,
        core_sweep: args.core_sweep,
        run_id: args.run_id.clone(),
        workers: args.workers,
        transitions: if args.minimum_duration_seconds.is_some() {
            1
        } else {
            args.transitions
        },
        idle_ms: args.idle_ms,
        burst_ms: args.burst_ms,
        timeout_ms: args.timeout_ms,
        max_start_spread_ms: args.max_start_spread_ms,
        fma_iterations: args.fma_iterations,
        seed,
        checkpoint: args.checkpoint.clone(),
    };
    match dit_system::power_platform::run(&config) {
        Ok(report) => {
            println!(
                "CPU_DIT_POWER_RESULT verdict={:?} availability={:?} power_platform={:?}",
                report.verdict, report.availability.status, report.power_platform.status
            );
            println!(
                "{:?}: availability={:?} power_platform={:?} transitions={} checkpoint={}",
                report.verdict,
                report.availability.status,
                report.power_platform.status,
                report.total_transitions,
                args.checkpoint.display()
            );
            match report.verdict {
                dit_system::power_platform::PowerVerdict::Pass => 0,
                dit_system::power_platform::PowerVerdict::Fail => 2,
                dit_system::power_platform::PowerVerdict::Incomplete => 3,
            }
        }
        Err(error) => {
            eprintln!("INVALID: {error}");
            3
        }
    }
}

fn power_platform_recover(checkpoint: &Path, output: &Path) -> u8 {
    if checkpoint == output || output.exists() {
        eprintln!("INVALID: recovery output must be a new separate file");
        return 3;
    }
    match dit_system::power_platform::recover(checkpoint) {
        Ok(report) => match dit_system::availability::write_durable(output, &report) {
            Ok(()) => {
                println!(
                    "availability={:?} power_platform={:?}",
                    report.availability.status, report.power_platform.status
                );
                0
            }
            Err(error) => {
                eprintln!("cannot persist recovery: {error}");
                4
            }
        },
        Err(error) => {
            eprintln!("INVALID: {error}");
            3
        }
    }
}

// DIT-DURATION-COMPLEXITY-RAMP-v1 (docs/DURATION_COMPLEXITY_RAMP.md): tiered
// gate below the threshold, then a capped-linear ramp so a longer
// --minimum-duration-seconds amortizes a heavier per-burst workload without
// an unbounded single-burst cost regardless of how large duration is.
const DEFAULT_DENSE_FMA_ITERATIONS: u32 = 200_000;
const DENSE_FMA_RAMP_THRESHOLD_SECONDS: u64 = 60;
const DENSE_FMA_ITERATIONS_PER_SECOND: u32 = 300_000;
const MAX_AUTO_DENSE_FMA_ITERATIONS: u32 = 500_000_000;
const MAX_MANUAL_DENSE_FMA_ITERATIONS: u32 = 2_000_000_000;

#[derive(Debug, Args)]
struct ComputeOracleArgs {
    /// Zero selects all CPUs allowed by the current cpuset.
    #[arg(long, default_value_t = 0)]
    workers: usize,

    /// Keep bursting the dense-FMA/branch-predictor/AES kernels until at least this duration has elapsed.
    #[arg(long, default_value_t = 5)]
    minimum_duration_seconds: u64,

    /// FMA/AVX iterations per burst; higher values increase sustained power draw per burst.
    /// Omit to auto-scale with --minimum-duration-seconds (tiered baseline, then a capped-linear ramp;
    /// see `docs/DURATION_COMPLEXITY_RAMP.md`, recipe DIT-DURATION-COMPLEXITY-RAMP-v1).
    #[arg(long)]
    dense_fma_iterations: Option<u32>,

    /// Alternate idle sleep and full-load bursts to exercise P-state/voltage transitions.
    #[arg(long)]
    idle_ramp: bool,

    #[arg(long, default_value_t = 50)]
    idle_sleep_ms: u64,

    /// Replay a fixed seed (decimal or 0x-prefixed hex); default uses Linux CSPRNG.
    #[arg(long, value_parser = parse_seed)]
    seed: Option<u64>,

    /// Dev/test-only: corrupt one worker's dense-FMA result to prove the cross-worker oracle FAILs.
    #[arg(long)]
    inject_worker_fault: Option<usize>,

    #[arg(long, default_value = "build/compute-oracle-report.json")]
    report: PathBuf,
}

#[derive(Debug, Args)]
struct SensorArgs {
    /// Interval used to derive Watts from cumulative energy counters.
    #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u64).range(50..=5000))]
    interval_ms: u64,

    /// Emit the complete sensor report as JSON.
    #[arg(long, conflicts_with = "compact")]
    json: bool,

    /// Emit two short lines suitable for the boot TUI.
    #[arg(long, conflicts_with = "json")]
    compact: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(long, default_value = "vectors/source-v1.dat")]
    source: PathBuf,

    #[arg(long, default_value = "vectors/source-v1.manifest.json")]
    manifest: PathBuf,

    #[arg(long, default_value = "vectors/golden-v1.json")]
    golden: PathBuf,

    #[arg(long, default_value = "build/reference-report.json")]
    report: PathBuf,

    #[arg(
        long,
        requires = "inject_byte_offset",
        conflicts_with = "inject_recovered_offset"
    )]
    inject_logical_block: Option<usize>,

    #[arg(long, requires = "inject_logical_block")]
    inject_byte_offset: Option<usize>,

    #[arg(long, conflicts_with = "inject_logical_block")]
    inject_recovered_offset: Option<u64>,

    #[arg(long, default_value_t = 1)]
    inject_xor_mask: u8,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LockChoice {
    Off,
    BestEffort,
    Required,
}

impl From<LockChoice> for LockPolicy {
    fn from(value: LockChoice) -> Self {
        match value {
            LockChoice::Off => Self::Off,
            LockChoice::BestEffort => Self::BestEffort,
            LockChoice::Required => Self::Required,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EdacChoice {
    Observe,
    FailUncorrected,
    FailAny,
}

impl From<EdacChoice> for EdacPolicy {
    fn from(value: EdacChoice) -> Self {
        match value {
            EdacChoice::Observe => Self::Observe,
            EdacChoice::FailUncorrected => Self::FailUncorrected,
            EdacChoice::FailAny => Self::FailAny,
        }
    }
}

#[allow(clippy::struct_excessive_bools)] // Independent CLI safety switches stay explicit.
#[derive(Debug, Args)]
struct SystemRunArgs {
    #[arg(long, default_value = "vectors/source-v1.dat")]
    source: PathBuf,

    #[arg(long, default_value = "vectors/source-v1.manifest.json")]
    manifest: PathBuf,

    #[arg(long, conflicts_with_all = ["available_percent", "safe_percent"])]
    memory_mib: Option<u64>,

    #[arg(long, conflicts_with_all = ["memory_mib", "safe_percent"])]
    available_percent: Option<u8>,

    /// Percentage of `MemAvailable` after subtracting the Linux safety reserve.
    #[arg(long, conflicts_with_all = ["memory_mib", "available_percent"])]
    safe_percent: Option<u8>,

    #[arg(long, default_value_t = 512)]
    reserve_mib: u64,

    #[arg(long, default_value_t = 16)]
    arena_mib: u64,

    #[arg(long, default_value_t = 4)]
    minimum_arena_mib: u64,

    #[arg(long, default_value_t = 2)]
    rounds: u32,

    /// Keep running complete write/read rounds until at least this duration has elapsed.
    #[arg(long, default_value_t = 0)]
    minimum_duration_seconds: u64,

    /// Emit machine-readable progress lines for a supervising UI.
    #[arg(long)]
    progress: bool,

    /// Best-effort JSON heartbeat overwritten at every phase transition, so a
    /// hang or crash still leaves a report showing the last reached phase.
    #[arg(long)]
    progress_report: Option<PathBuf>,

    /// Zero selects all CPUs allowed by the current cpuset.
    #[arg(long, default_value_t = 0)]
    workers: usize,

    #[arg(long, value_enum, default_value_t = LockChoice::BestEffort)]
    lock_policy: LockChoice,

    #[arg(long, default_value_t = 100_000)]
    coherency_iterations: u64,

    /// Replay a fixed session seed (decimal or 0x-prefixed hex); default uses Linux CSPRNG.
    #[arg(long, value_parser = parse_seed)]
    seed: Option<u64>,

    #[arg(long, group = "fault_injection")]
    inject_global_offset: Option<u64>,

    /// Corrupt the optimized reduction result for one worker to test its scalar oracle.
    #[arg(long, group = "fault_injection")]
    inject_simd_worker: Option<usize>,

    /// Trigger a caught panic in one RAM writer and require crash evidence.
    #[arg(long, group = "fault_injection")]
    inject_worker_crash: Option<usize>,

    #[arg(long, group = "fault_injection")]
    inject_timeout: bool,

    #[arg(long, group = "fault_injection")]
    inject_temperature_c: Option<u32>,

    #[arg(long, group = "fault_injection")]
    inject_edac_uncorrected: bool,

    #[arg(long, default_value_t = 300)]
    deadline_seconds: u64,

    #[arg(long, default_value_t = 250)]
    monitor_interval_ms: u64,

    /// Enable CPU temperature auto-abort; disabled by default while telemetry remains active.
    #[arg(long)]
    enable_temperature_limit: bool,

    #[arg(long, default_value_t = 95)]
    max_temperature_c: u32,

    #[arg(long)]
    require_temperature_sensor: bool,

    #[arg(long)]
    require_edac_counters: bool,

    #[arg(long, value_enum, default_value_t = EdacChoice::FailUncorrected)]
    edac_policy: EdacChoice,

    #[arg(long)]
    fail_on_throttle: bool,

    #[arg(long)]
    no_diagnostic_rerun: bool,

    /// Zero disables the compute-oracle (ADR-0004 Phase 2); otherwise spare
    /// logical CPUs beyond the RAM workers run it concurrently with the round loop.
    #[arg(long, default_value_t = 0)]
    compute_oracle_workers: usize,

    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=2_000_000_000))]
    compute_oracle_dense_fma_iterations: Option<u32>,

    /// Alternate idle sleep and full-load bursts to exercise P-state/voltage transitions.
    #[arg(long)]
    compute_oracle_idle_ramp: bool,

    #[arg(long, default_value_t = 50)]
    compute_oracle_idle_sleep_ms: u64,

    /// Dev/test-only: corrupt one compute-oracle worker's dense-FMA result.
    #[arg(long, group = "fault_injection")]
    inject_compute_oracle_worker: Option<usize>,

    #[arg(long, default_value = "build/system-report.json")]
    report: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::PowerPlatformRun(args) => power_platform_run(&args),
        Command::PowerPlatformWatchdogSeconds {
            idle_ms,
            timeout_ms,
            campaign,
        } => {
            println!(
                "{}",
                dit_system::power_platform::watchdog_stale_seconds(idle_ms, timeout_ms, campaign)
            );
            0
        }
        Command::PowerPlatformRecover { checkpoint, report } => {
            power_platform_recover(&checkpoint, &report)
        }
        Command::Run(args) => run(&args),
        Command::SystemRun(args) => system_run(&args),
        Command::ComputeOracleRun(args) => compute_oracle_run(&args),
        Command::Sensors(args) => sensors(&args),
    };
    ExitCode::from(code)
}

fn sensors(args: &SensorArgs) -> u8 {
    let report = match collect_sensor_report(Duration::from_millis(args.interval_ms)) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("cannot collect sensor report: {error}");
            return 4;
        }
    };
    if args.json {
        return match serde_json::to_writer_pretty(io::stdout().lock(), &report) {
            Ok(()) => {
                println!();
                0
            }
            Err(error) => {
                eprintln!("cannot serialize sensor report: {error}");
                4
            }
        };
    }
    if args.compact {
        print_compact_sensors(&report);
    } else {
        print_sensor_report(&report);
    }
    0
}

fn print_compact_sensors(report: &SensorReport) {
    let cpu = &report.inventory.cpu;
    let memory = &report.inventory.memory;
    let temperature = preferred_cpu_temperature(report).map_or_else(
        || "N/A C".to_owned(),
        |value| format!("{} C", format_milli_i64(value, 1)),
    );
    let power = report.telemetry.preferred_cpu_power().map_or_else(
        || "N/A W".to_owned(),
        |reading| format!("{} W", format_milli_u64(reading.milliwatts, 1)),
    );
    println!(
        "CPU: {} | {}C/{}T | {} | {}",
        cpu.model_name, cpu.physical_core_count, cpu.logical_cpu_count, temperature, power
    );
    println!(
        "RAM: {} GiB total | {} GiB available | NUMA {} | hwmon {}",
        format_gib(memory.installed_bytes),
        format_gib(memory.available_at_start_bytes),
        report.inventory.numa_nodes.len(),
        report.hwmon_chips.len()
    );
    println!("GPU: {}", format_gpu_summary(&report.gpus));
}

fn print_sensor_report(report: &SensorReport) {
    print_compact_sensors(report);
    println!("Kernel: {}", report.inventory.kernel_release);
    println!("hwmon chips: {}", joined_or_na(&report.hwmon_chips));
    println!("powercap zones: {}", joined_or_na(&report.powercap_zones));
    println!();
    println!("Temperature sensors:");
    if report.telemetry.temperatures.is_empty() {
        println!("  N/A — no readable hwmon/thermal-zone temperature");
    } else {
        for reading in &report.telemetry.temperatures {
            println!(
                "  {:<28} {:>7} C  [{}]",
                reading.label,
                format_milli_i64(reading.millidegrees_c, 1),
                reading.source
            );
        }
    }
    println!();
    println!("CPU package power:");
    if report.telemetry.cpu_power.is_empty() {
        let reason = if report.powercap_zones.is_empty() {
            "kernel/firmware exposes no supported CPU package counter"
        } else {
            "counter exists but is unreadable or did not produce a valid delta"
        };
        println!("  N/A — {reason}");
    } else {
        for reading in &report.telemetry.cpu_power {
            println!(
                "  {:<20} {:>8} W  {}  [{}]",
                reading.label,
                format_milli_u64(reading.milliwatts, 2),
                reading.measurement,
                reading.source
            );
        }
    }
    println!();
    println!(
        "EDAC: corrected={} uncorrected={} | throttle core={} package={}",
        report.telemetry.edac_corrected_total(),
        report.telemetry.edac_uncorrected_total(),
        report.telemetry.throttle_core_total(),
        report.telemetry.throttle_package_total()
    );
    let sensor_modules: Vec<String> = report
        .loaded_modules
        .iter()
        .filter(|module| is_sensor_module(module))
        .cloned()
        .collect();
    println!("sensor/RAS modules: {}", joined_or_na(&sensor_modules));
    for note in &report.availability.notes {
        println!("note: {note}");
    }
}

fn preferred_cpu_temperature(report: &SensorReport) -> Option<i64> {
    let cpu_readings = report.telemetry.temperatures.iter().filter(|reading| {
        let label = reading.label.to_ascii_lowercase();
        [
            "k10temp", "k8temp", "coretemp", "zenpower", "cpu", "package", "tctl", "tccd",
        ]
        .iter()
        .any(|marker| label.contains(marker))
    });
    cpu_readings
        .map(|reading| reading.millidegrees_c)
        .max()
        .or_else(|| report.telemetry.max_temperature_millicelsius())
}

fn joined_or_na(values: &[String]) -> String {
    if values.is_empty() {
        "N/A".to_owned()
    } else {
        values.join(", ")
    }
}

fn is_sensor_module(module: &str) -> bool {
    [
        "temp", "hwmon", "sensor", "edac", "mce", "rapl", "power", "i2c", "ipmi",
    ]
    .iter()
    .any(|marker| module.contains(marker))
}

fn format_milli_u64(value: u64, decimals: u32) -> String {
    let divisor = 10_u64.pow(3_u32.saturating_sub(decimals.min(3)));
    let fraction = (value % 1000) / divisor;
    format!(
        "{}.{:0width$}",
        value / 1000,
        fraction,
        width = usize::try_from(decimals.min(3)).unwrap_or(3)
    )
}

fn format_milli_i64(value: i64, decimals: u32) -> String {
    let sign = if value < 0 { "-" } else { "" };
    format!("{sign}{}", format_milli_u64(value.unsigned_abs(), decimals))
}

fn format_gib(bytes: u64) -> String {
    let hundredths = u128::from(bytes).saturating_mul(100) / 1_073_741_824_u128;
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

#[allow(clippy::too_many_lines)] // Explicit CLI-to-config wiring keeps flags auditable.
fn system_run(args: &SystemRunArgs) -> u8 {
    let source = match load_verified_source(&args.source, &args.manifest) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("INVALID: {error}");
            return 3;
        }
    };
    let target = match (args.memory_mib, args.available_percent, args.safe_percent) {
        (Some(mib), None, None) => {
            let Some(bytes) = mib_to_bytes(mib) else {
                eprintln!("INVALID: memory MiB value overflow");
                return 3;
            };
            MemoryTarget::Bytes(bytes)
        }
        (None, Some(percent), None) => MemoryTarget::PercentAvailable(percent),
        (None, None, Some(percent)) => MemoryTarget::PercentSafe(percent),
        (None, None, None) => MemoryTarget::Bytes(256 * 1024 * 1024),
        _ => unreachable!("clap enforces conflicts"),
    };
    let Some(safety_reserve_bytes) = mib_to_bytes(args.reserve_mib) else {
        eprintln!("INVALID: reserve MiB value overflow");
        return 3;
    };
    let Some(arena_target_bytes) = mib_to_bytes(args.arena_mib) else {
        eprintln!("INVALID: arena MiB value overflow");
        return 3;
    };
    let Some(minimum_arena_bytes) = mib_to_bytes(args.minimum_arena_mib) else {
        eprintln!("INVALID: minimum arena MiB value overflow");
        return 3;
    };
    let Some(deadline_ms) = args.deadline_seconds.checked_mul(1000) else {
        eprintln!("INVALID: deadline seconds value overflow");
        return 3;
    };
    let Some(minimum_duration_ms) = args.minimum_duration_seconds.checked_mul(1000) else {
        eprintln!("INVALID: minimum duration seconds value overflow");
        return 3;
    };
    let max_temperature_millicelsius = i64::from(args.max_temperature_c) * 1000;
    let (session_seed, seed_source) = if let Some(seed) = args.seed {
        (seed, SeedSource::OperatorReplay)
    } else {
        let mut bytes = [0_u8; 8];
        if let Err(error) = getrandom::fill(&mut bytes) {
            eprintln!("INVALID: Linux CSPRNG could not provide a session seed: {error}");
            return 3;
        }
        (u64::from_le_bytes(bytes), SeedSource::LinuxRandom)
    };
    let config = SystemRunConfig {
        test_id: "CPU-DIT-01-SYSTEM-v1".to_owned(),
        target,
        safety_reserve_bytes,
        arena_target_bytes,
        minimum_arena_bytes,
        lock_policy: args.lock_policy.into(),
        rounds: args.rounds,
        minimum_duration_ms,
        emit_progress: args.progress,
        progress_report_path: args.progress_report.clone(),
        requested_workers: args.workers,
        seed: session_seed,
        seed_source,
        coherency_iterations_per_worker: args.coherency_iterations,
        fault_injection_global_offset: args.inject_global_offset,
        fault_injection_simd_worker: args.inject_simd_worker,
        fault_injection_worker_crash: args.inject_worker_crash,
        safety_policy: SafetyPolicy {
            deadline_ms,
            monitor_interval_ms: args.monitor_interval_ms,
            temperature_limit_enabled: args.enable_temperature_limit,
            max_temperature_millicelsius,
            require_temperature_sensor: args.require_temperature_sensor,
            require_edac_counters: args.require_edac_counters,
            edac_policy: args.edac_policy.into(),
            fail_on_throttle_delta: args.fail_on_throttle,
            ..SafetyPolicy::default()
        },
        safety_fault_injection: SafetyFaultInjection {
            timeout: args.inject_timeout,
            temperature_millicelsius: args
                .inject_temperature_c
                .map(|value| i64::from(value) * 1000),
            edac_uncorrected_delta: u64::from(args.inject_edac_uncorrected),
            ..SafetyFaultInjection::default()
        },
        diagnostic_rerun: !args.no_diagnostic_rerun,
        compute_oracle_workers: args.compute_oracle_workers,
        compute_oracle_dense_fma_iterations: args
            .compute_oracle_dense_fma_iterations
            .unwrap_or_else(|| suggested_dense_fma_iterations(args.minimum_duration_seconds)),
        compute_oracle_idle_ramp_enabled: args.compute_oracle_idle_ramp,
        compute_oracle_idle_sleep_ms: args.compute_oracle_idle_sleep_ms,
        fault_injection_compute_oracle_worker: args.inject_compute_oracle_worker,
    };
    let report = match run_system(&source, &config) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("INVALID: {error}");
            return 3;
        }
    };
    finish_system(&args.report, &report)
}

/// Tiered gate (below threshold: baseline) then a capped-linear ramp; see
/// `suggested_compute_rounds` in `crates/gpu-cli/src/main.rs` for the
/// reference implementation of the same recipe.
fn suggested_dense_fma_iterations(duration_seconds: u64) -> u32 {
    if duration_seconds < DENSE_FMA_RAMP_THRESHOLD_SECONDS {
        return DEFAULT_DENSE_FMA_ITERATIONS;
    }
    let extra_seconds = duration_seconds - DENSE_FMA_RAMP_THRESHOLD_SECONDS;
    let ramped = u64::from(DEFAULT_DENSE_FMA_ITERATIONS)
        .saturating_add(extra_seconds.saturating_mul(u64::from(DENSE_FMA_ITERATIONS_PER_SECOND)));
    u32::try_from(ramped)
        .unwrap_or(u32::MAX)
        .min(MAX_AUTO_DENSE_FMA_ITERATIONS)
}

/// Explicit `--dense-fma-iterations` always wins; otherwise derive from duration.
fn resolved_dense_fma_iterations(args: &ComputeOracleArgs) -> u32 {
    args.dense_fma_iterations
        .unwrap_or_else(|| suggested_dense_fma_iterations(args.minimum_duration_seconds))
}

fn compute_oracle_run(args: &ComputeOracleArgs) -> u8 {
    if args
        .dense_fma_iterations
        .is_some_and(|value| value > MAX_MANUAL_DENSE_FMA_ITERATIONS)
    {
        eprintln!(
            "INVALID: dense-fma-iterations must be at most {MAX_MANUAL_DENSE_FMA_ITERATIONS}"
        );
        return 3;
    }
    let seed = if let Some(seed) = args.seed {
        seed
    } else {
        let mut bytes = [0_u8; 8];
        if let Err(error) = getrandom::fill(&mut bytes) {
            eprintln!("INVALID: Linux CSPRNG could not provide a seed: {error}");
            return 3;
        }
        u64::from_le_bytes(bytes)
    };
    let Some(minimum_duration_ms) = args.minimum_duration_seconds.checked_mul(1000) else {
        eprintln!("INVALID: minimum duration seconds value overflow");
        return 3;
    };
    let config = ComputeOracleConfig {
        test_id: "CPU-DIT-01-COMPUTE-ORACLE-v1".to_owned(),
        requested_workers: args.workers,
        seed,
        minimum_duration_ms,
        dense_fma_iterations_per_burst: resolved_dense_fma_iterations(args),
        idle_ramp_enabled: args.idle_ramp,
        idle_sleep_ms: args.idle_sleep_ms,
        fault_injection_worker: args.inject_worker_fault,
    };
    let report = match run_compute_oracle(&config) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("INVALID: {error}");
            return 3;
        }
    };
    finish_compute_oracle(&args.report, &report)
}

fn finish_compute_oracle(path: &Path, report: &ComputeOracleReport) -> u8 {
    if let Err(error) = write_json(path, report) {
        eprintln!("cannot write report {}: {error}", path.display());
        return 4;
    }
    let status_name = match report.status {
        ComputeOracleStatus::Pass => "PASS",
        ComputeOracleStatus::Fail => "FAIL",
        ComputeOracleStatus::Incomplete => "INCOMPLETE",
    };
    println!(
        "{status_name}: test_id={}, workers={}, elapsed_ms={}, dense_fma_cross_worker_consistent={}, branch_predictor_passed={}, aes256_known_answer_passed={}, report={}",
        report.test_id,
        report.workers_used,
        report.elapsed_ms,
        report.dense_fma_cross_worker_consistent,
        report.branch_predictor_passed,
        report.aes256_known_answer_passed,
        path.display()
    );
    if let Some(mismatch) = &report.mismatch {
        println!(
            "reason: kernel={} detail={}",
            mismatch.kernel, mismatch.detail
        );
    }
    match report.status {
        ComputeOracleStatus::Pass => 0,
        ComputeOracleStatus::Fail => 2,
        ComputeOracleStatus::Incomplete => 3,
    }
}

fn run(args: &RunArgs) -> u8 {
    let golden = match load_golden_vector(&args.golden) {
        Ok(golden) => golden,
        Err(error) => {
            eprintln!("INVALID: {error}");
            return 3;
        }
    };
    let config = match golden.to_config() {
        Ok(config) => config,
        Err(error) => {
            let report = RunReport::invalid(&golden.test_id, error.to_string());
            return finish(&args.report, &report);
        }
    };
    let source = match load_verified_source(&args.source, &args.manifest) {
        Ok(source) => source,
        Err(error) => {
            let report = RunReport::invalid(&golden.test_id, error.to_string());
            return finish(&args.report, &report);
        }
    };

    let fault = args
        .inject_logical_block
        .map(|logical_block| FaultInjection::ReadbackByte {
            logical_block,
            byte_offset: args
                .inject_byte_offset
                .expect("clap requires inject_byte_offset"),
            xor_mask: args.inject_xor_mask,
        });
    let fault = fault.or_else(|| {
        args.inject_recovered_offset
            .map(|global_offset| FaultInjection::RecoveredByte {
                global_offset,
                xor_mask: args.inject_xor_mask,
            })
    });

    let report = match run_reference(&source, &config, &golden, fault) {
        Ok(report) => report,
        Err(error) => RunReport::invalid(&golden.test_id, error.to_string()),
    };
    finish(&args.report, &report)
}

fn finish(path: &Path, report: &RunReport) -> u8 {
    if let Err(error) = write_report(path, report) {
        eprintln!("cannot write report {}: {error}", path.display());
        return 4;
    }

    println!(
        "{}: test_id={}, generated={} bytes, exact_verified={} bytes, report={}",
        status_name(report.status),
        report.test_id,
        report.coverage.generated_bytes,
        report.coverage.exact_verified_unique_bytes,
        path.display()
    );
    if let Some(reason) = &report.failure_reason {
        println!("reason: {reason}");
    }
    match report.status {
        RunStatus::Pass => 0,
        RunStatus::Fail => 2,
        RunStatus::Invalid => 3,
    }
}

fn write_report(path: &Path, report: &RunReport) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, report).map_err(io::Error::other)
}

fn finish_system(path: &Path, report: &SystemRunReport) -> u8 {
    println!(
        "availability={:?}, power_platform={:?}",
        report.availability.status, report.power_platform.status
    );
    if let Err(error) = write_json(path, report) {
        eprintln!("cannot write report {}: {error}", path.display());
        return 4;
    }
    println!(
        "{}: test_id={}, allocated={} bytes, exact_verified={} bytes, workers={}, rounds={}/{}, report={}",
        status_name(report.status),
        report.test_id,
        report.memory.allocated_unique_bytes,
        report.memory.exact_verified_unique_bytes,
        report.cpu.workers_used,
        report.rounds_completed,
        report.rounds_requested,
        path.display()
    );
    if let Some(reason) = &report.failure_reason {
        println!("reason: {reason}");
    }
    println!(
        "calculations_completed={}, failures_total={}",
        report.calculations_completed, report.failures_total
    );
    println!(
        "telemetry: temperature={}, cpu_power={}, edac={}, throttle={}, peak_mC={}; safety_stop={}",
        report.telemetry.availability.temperature_sensors,
        report.telemetry.availability.cpu_power_sensors,
        report.telemetry.availability.edac_counters,
        report.telemetry.availability.throttle_counters,
        report
            .telemetry
            .peak_temperature_millicelsius
            .map_or_else(|| "unavailable".to_owned(), |value| value.to_string()),
        report.safety.stop_requested
    );
    if report.diagnostic.attempted {
        println!("diagnostic: {}", report.diagnostic.classification);
    }
    match report.status {
        RunStatus::Pass => 0,
        RunStatus::Fail => 2,
        RunStatus::Invalid => 3,
    }
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value).map_err(io::Error::other)
}

fn parse_seed(value: &str) -> Result<u64, String> {
    let trimmed = value.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16)
            .map_err(|error| format!("invalid hexadecimal seed '{value}': {error}"));
    }
    trimmed
        .parse::<u64>()
        .map_err(|error| format!("invalid decimal seed '{value}': {error}"))
}

const fn mib_to_bytes(mib: u64) -> Option<u64> {
    mib.checked_mul(1024 * 1024)
}

const fn status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Pass => "PASS",
        RunStatus::Fail => "FAIL",
        RunStatus::Invalid => "INVALID",
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ComputeOracleArgs, DEFAULT_DENSE_FMA_ITERATIONS, DENSE_FMA_RAMP_THRESHOLD_SECONDS,
        MAX_AUTO_DENSE_FMA_ITERATIONS, parse_seed, resolved_dense_fma_iterations,
        suggested_dense_fma_iterations,
    };
    use clap::Parser;

    #[test]
    fn unified_power_cli_preserves_diagnostic_modes() {
        let base = [
            "dit-cli",
            "power-platform-run",
            "--run-id",
            "test",
            "--checkpoint",
            "/tmp/not-executed.json",
        ];
        let parsed = super::Cli::try_parse_from(
            base.into_iter()
                .chain(["--minimum-duration-seconds", "600"]),
        )
        .unwrap();
        let super::Command::PowerPlatformRun(args) = parsed.command else {
            panic!("wrong command")
        };
        assert_eq!(args.minimum_duration_seconds, Some(600));
        for extra in [
            vec!["--minimum-duration-seconds", "0"],
            vec!["--minimum-duration-seconds", "86401"],
            vec!["--minimum-duration-seconds", "10", "--core-sweep"],
            vec!["--minimum-duration-seconds", "10", "--transitions", "1"],
        ] {
            assert!(super::Cli::try_parse_from(base.into_iter().chain(extra)).is_err());
        }
        assert!(
            super::Cli::try_parse_from(base.into_iter().chain([
                "--core-sweep",
                "--transitions",
                "10"
            ]))
            .is_ok()
        );
    }

    #[test]
    fn parses_decimal_and_hex_replay_seeds() {
        assert_eq!(parse_seed("81985529216486895"), Ok(0x0123_4567_89ab_cdef));
        assert_eq!(parse_seed("0x0123456789abcdef"), Ok(0x0123_4567_89ab_cdef));
        assert!(parse_seed("0xnot-a-seed").is_err());
    }

    #[test]
    fn dense_fma_iterations_stay_flat_below_threshold_then_ramp_and_cap() {
        assert_eq!(
            suggested_dense_fma_iterations(0),
            DEFAULT_DENSE_FMA_ITERATIONS
        );
        assert_eq!(
            suggested_dense_fma_iterations(DENSE_FMA_RAMP_THRESHOLD_SECONDS),
            DEFAULT_DENSE_FMA_ITERATIONS
        );
        assert!(
            suggested_dense_fma_iterations(DENSE_FMA_RAMP_THRESHOLD_SECONDS + 1)
                > DEFAULT_DENSE_FMA_ITERATIONS
        );
        assert_eq!(
            suggested_dense_fma_iterations(u64::MAX),
            MAX_AUTO_DENSE_FMA_ITERATIONS
        );
    }

    #[test]
    fn explicit_dense_fma_iterations_overrides_the_duration_derived_ramp() {
        let mut args = ComputeOracleArgs {
            workers: 1,
            minimum_duration_seconds: 3_600,
            dense_fma_iterations: None,
            idle_ramp: false,
            idle_sleep_ms: 50,
            seed: None,
            inject_worker_fault: None,
            report: "build/compute-oracle-report.json".into(),
        };
        assert_eq!(
            resolved_dense_fma_iterations(&args),
            suggested_dense_fma_iterations(3_600)
        );
        args.dense_fma_iterations = Some(7);
        assert_eq!(resolved_dense_fma_iterations(&args), 7);
    }
}
