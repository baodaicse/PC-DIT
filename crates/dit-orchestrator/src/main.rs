use clap::{Parser, Subcommand};
use dit_contracts::Verdict;
use dit_orchestrator::{
    ExecutionPlan, ModuleKind, ModuleRegistration, SuiteReport, adapt_cpu_ram_report,
    adapt_nvme_report, allocate_resources, render_module_menu,
};
use dit_ui_core::TerminalSize;
use serde::Serialize;
use std::fs::{self, OpenOptions, create_dir, create_dir_all};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

mod combined_layers;
mod combined_run;
mod combined_view;
mod cpu_ram_view;
mod fault_single;

/// Set by Ctrl+C while the menu runs. Single modules ignore it (their own watchdog handles the
/// stop); the combined run reads it to drain every layer and end the session.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(
    name = "dit-suite",
    version,
    about = "DIT shared module control center"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the shared CPU-RAM / CPU-SSD / GPU-VRAM test selection UI.
    Menu(MenuArgs),
    /// Render one selection frame without launching a test.
    RenderMenu(MenuArgs),
    /// Adapt native module reports and produce one aggregate suite report.
    Aggregate(AggregateArgs),
    /// Emit the detected module registry as JSON.
    Registry(MenuArgs),
    /// Run the combined CPU-RAM + GPU-VRAM + SSD + data-chain session without the menu.
    Combined(combined_run::CombinedCli),
    /// Running screen of a CPU-RAM test on the shared dashboard (started by the CPU-RAM UI).
    CpuRamView(cpu_ram_view::CpuRamViewArgs),
}

#[derive(Clone, Debug, clap::Args)]
struct MenuArgs {
    #[arg(long, default_value = "/usr/local/lib/cpu-dit/cpu-dit-ui.sh")]
    cpu_ui: PathBuf,
    #[arg(long, default_value = "/opt/cpu-nvme-dit/cpu-nvme-dit")]
    nvme_binary: PathBuf,
    #[arg(long, default_value = "/opt/gpu-vram-dit/gpu-vram-dit")]
    gpu_binary: PathBuf,
    /// CPU-RAM engine used directly by the combined run (its vectors sit next to it).
    #[arg(long, default_value = "/opt/cpu-dit-01/dit-cli")]
    cpu_cli: PathBuf,
    /// Read-mostly SSD -> RAM -> GPU -> CPU data chain of the combined run.
    #[arg(long, default_value = "/opt/dit-chain/dit-chain")]
    chain_binary: PathBuf,
    /// Independent worker supervisor. It must not be part of the stress workload.
    #[arg(long, default_value = "/usr/local/bin/dit-watchdog")]
    watchdog_binary: PathBuf,
    #[arg(long, default_value = "/mnt/cpu-dit-results")]
    results_root: PathBuf,
    /// Device nodes belonging to the boot/results medium. Forwarded to CPU-SSD inventory.
    #[arg(long = "protected-device")]
    protected_devices: Vec<PathBuf>,
    /// Confirmation steps that touch the data path of the tests (component fault ledger, F3
    /// GPU boundary checks and F4 confirm-on-fail). Off until validated on real machines; the
    /// boot menu turns it on with the kernel parameter `dit_attribution=1`.
    #[arg(long)]
    attribution_checks: bool,
}

#[derive(Debug, clap::Args)]
struct AggregateArgs {
    #[arg(long)]
    cpu_report: PathBuf,
    #[arg(long)]
    nvme_report: PathBuf,
    #[arg(long)]
    output: PathBuf,
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
        Command::Menu(args) => menu(&args),
        Command::RenderMenu(args) => {
            // On a terminal as the menu shows it; plain text for scripts that read it.
            let frame = render(&args);
            if io::stdout().is_terminal() {
                println!("{}", dit_ui_core::decorate(&frame));
            } else {
                println!("{frame}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Registry(args) => {
            serde_json::to_writer_pretty(io::stdout().lock(), &registry(&args))?;
            println!();
            Ok(ExitCode::SUCCESS)
        }
        Command::Aggregate(args) => aggregate(&args),
        Command::Combined(args) => combined_run::run_cli(&args),
        Command::CpuRamView(args) => {
            cpu_ram_view::command(&args)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn registry(args: &MenuArgs) -> Vec<ModuleRegistration> {
    vec![
        ModuleRegistration::detect(ModuleKind::CpuRam, args.cpu_ui.clone()),
        ModuleRegistration::detect(ModuleKind::CpuNvme, args.nvme_binary.clone()),
        ModuleRegistration::detect(ModuleKind::Vga, args.gpu_binary.clone()),
        ModuleRegistration::detect(ModuleKind::CpuPower, args.cpu_ui.clone()),
        ModuleRegistration::detect(ModuleKind::Combined, args.chain_binary.clone()),
    ]
}

fn render(args: &MenuArgs) -> String {
    let mut frame = render_module_menu(
        &registry(args),
        usize::from(TerminalSize::current().cols),
        args.attribution_checks,
    );
    if let Some(recovery) = interrupted_run_notice(&args.results_root) {
        frame.push_str("\nWATCHDOG RECOVERY: ");
        frame.push_str(&recovery);
        frame.push_str("\nRead the matching watchdog marker before starting a new test.\n");
    }
    frame
}

fn menu(args: &MenuArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if !io::stdin().is_terminal() {
        return Err("interactive menu requires a terminal".into());
    }
    // Without this, Ctrl+C's default action kills the menu process itself
    // while a module is running, orphaning its watchdog/worker instead of
    // letting the module's own graceful-stop handling finish and return here.
    ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst))?;
    // Kept for the whole menu session, so a chosen GPU duration/limit survives going back.
    let mut gpu_settings = GpuSettings::default();
    loop {
        print!(
            "\x1b[H\x1b[2J{}\nSelection > ",
            dit_ui_core::decorate(&render(args))
        );
        io::stdout().flush()?;
        let mut choice = String::new();
        io::stdin().read_line(&mut choice)?;
        if dispatch(args, &mut gpu_settings, choice.trim())? {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

/// One menu choice; `Ok(true)` when the operator quits the control center.
fn dispatch(
    args: &MenuArgs,
    gpu_settings: &mut GpuSettings,
    choice: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    {
        match choice.to_ascii_lowercase().as_str() {
            "1" => launch_cpu(args)?,
            "2" => launch_cpu_nvme(args)?,
            "3" => launch_gpu(args, gpu_settings)?,
            "4" => launch_cpu_power(args)?,
            "5" => combined_run::launch(args)?,
            "i" => {
                require_file(&args.nvme_binary, "CPU-NVMe binary")?;
                let _ = ProcessCommand::new(&args.nvme_binary)
                    .arg("inventory")
                    .status()?;
                pause("Read-only NVMe inventory completed.")?;
            }
            "f" => launch_format(args)?,
            "d" => {
                let _ = ProcessCommand::new("/bin/bash")
                    .args(["--noprofile", "--norc"])
                    .status()?;
            }
            "r" => {
                sync_filesystems();
                let _ = ProcessCommand::new("reboot").status()?;
            }
            "p" => {
                sync_filesystems();
                let _ = ProcessCommand::new("poweroff").status()?;
            }
            "q" => return Ok(true),
            _ => pause("Unknown selection; no test was started.")?,
        }
    }
    Ok(false)
}

/// FORMAT SSD: its own screen (`cpu-nvme-dit format-disk`), outside every test. The boot and
/// results media are passed as protected, like for the CPU-SSD test.
fn launch_format(args: &MenuArgs) -> Result<(), Box<dyn std::error::Error>> {
    require_file(&args.nvme_binary, "CPU-SSD binary")?;
    let mut command = ProcessCommand::new(&args.nvme_binary);
    command.arg("format-disk");
    for device in &args.protected_devices {
        command.arg("--protected-device").arg(device);
    }
    let status = command.status()?;
    if !status.success() {
        pause("The format screen ended with an error; see the message above.")?;
    }
    Ok(())
}

fn launch_cpu(args: &MenuArgs) -> Result<(), Box<dyn std::error::Error>> {
    require_file(&args.cpu_ui, "CPU-RAM UI")?;
    require_file(&args.watchdog_binary, "DIT watchdog")?;
    let _ = ProcessCommand::new(&args.cpu_ui)
        .env("CPU_DIT_SUITE_CHILD", "1")
        .env("CPU_DIT_WATCHDOG_BINARY", &args.watchdog_binary)
        .env("CPU_DIT_WATCHDOG_RESULTS_ROOT", &args.results_root)
        .status()?;
    Ok(())
}

fn launch_cpu_power(args: &MenuArgs) -> Result<(), Box<dyn std::error::Error>> {
    require_file(&args.cpu_ui, "CPU availability/power UI")?;
    require_file(&args.watchdog_binary, "DIT watchdog")?;
    let _ = power_ui_command(args).status()?;
    Ok(())
}

fn power_ui_command(args: &MenuArgs) -> ProcessCommand {
    let mut command = ProcessCommand::new(&args.cpu_ui);
    command
        .env("CPU_DIT_POWER_UI", "1")
        .env("CPU_DIT_UI_AUTOSTART_SECONDS", "0")
        .env("CPU_DIT_SUITE_CHILD", "1")
        .env("CPU_DIT_WATCHDOG_BINARY", &args.watchdog_binary)
        .env("CPU_DIT_WATCHDOG_RESULTS_ROOT", &args.results_root);
    command
}

fn launch_cpu_nvme(args: &MenuArgs) -> Result<(), Box<dyn std::error::Error>> {
    require_file(&args.nvme_binary, "CPU-SSD binary")?;
    require_file(&args.watchdog_binary, "DIT watchdog")?;
    let suite_run_id = unique_id("suite");
    let suite_dir = runtime_suite_dir(&suite_run_id);
    create_dir_all(
        suite_dir
            .parent()
            .ok_or("runtime CPU-SSD directory has no parent")?,
    )?;
    println!(
        "CPU-SSD live telemetry: {} (RAM/tmpfs)",
        suite_dir.display()
    );
    println!("USB is used only for the final report copy after the engine stops.");
    let heartbeat = suite_dir.join("events.jsonl");
    let mut command = watchdog_command(
        args,
        "cpu-nvme",
        &suite_run_id,
        &suite_dir,
        None,
        [heartbeat],
    );
    command.arg(&args.nvme_binary);
    command
        .arg("interactive")
        .arg("--output-dir")
        .arg(&suite_dir);
    if args.attribution_checks {
        command.arg("--confirm-on-fail");
    }
    for device in &args.protected_devices {
        command.arg("--protected-device").arg(device);
    }
    let nvme_status = command.status()?;
    let nvme_report = suite_dir.join("report.json");
    let completion = if nvme_report.is_file() {
        let nvme = adapt_nvme_report(&nvme_report, &std::fs::read(&nvme_report)?)?;
        let total_threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
        let available_memory = available_memory_bytes().unwrap_or(1024 * 1024 * 1024);
        let report = SuiteReport::new(
            suite_run_id.clone(),
            vec![nvme],
            allocate_resources(ExecutionPlan::CpuNvmeOnly, total_threads, available_memory)?,
        );
        write_new_json(&suite_dir.join("suite-report.json"), &report)?;
        format!(
            "CPU-SSD completed with {}.\n{}",
            verdict_name(report.aggregate_verdict),
            fault_single::summarize(
                fault_single::SingleModule::Ssd,
                &nvme_report,
                &args.results_root,
                &suite_dir,
                &suite_run_id,
                verdict_name(report.aggregate_verdict),
                args.attribution_checks,
            )
        )
    } else if !nvme_status.success() {
        "CPU-SSD was aborted or failed before a final report was written.".to_owned()
    } else {
        "CPU-SSD stopped without a final report.".to_owned()
    };
    let storage =
        match persist_run_artifacts(&suite_dir, &args.results_root, &suite_run_id, "cpu-nvme") {
            Ok(Some(path)) => format!("Final report bundle copied once to: {}", path.display()),
            Ok(None) => format!(
                "Verified result volume unavailable; artifacts remain in tmpfs: {}",
                suite_dir.display()
            ),
            Err(error) => format!(
                "Final USB report copy failed ({error}); artifacts remain in tmpfs: {}",
                suite_dir.display()
            ),
        };
    pause(&format!("{completion}\n{storage}"))?;
    Ok(())
}

/// The GPU-VRAM run the menu starts. `--async-auto` puts every CUDA GPU on the GPU-H async
/// pipeline (load controller, overlapped `PCIe` transfer lane, fair verifier queue; H1-H6) and keeps
/// Intel/AMD (Vulkan) on the sequential path, which has no async adapter yet. Not `--vram-local`:
/// on the GTX 960 it cut CPU-verified PCIe/VRAM coverage to 520 MiB/s, below the sequential path's
/// 898, while the bare async pipeline gave 1,305 MiB/s and 25x the verified compute
/// (build/gpu-async-menu-20260924-131640). The bottleneck log lands in the run directory, so it is
/// persisted with the report. Async-only options are dropped for sequential devices. `--vram-max`
/// holds VRAM up to what the driver reports free minus a small margin on discrete CUDA GPUs;
/// `--compute-mix alternate` switches the GEMM between the integer ALU and the FP32 FMA pipes at
/// each corpus rotation (both bit-exact against the CPU oracle); `--vram-sweep` adds the GPU-DRAM
/// sweep (device-verified at DRAM bandwidth for a quarter of the
/// device's time, trading pool halves with the transfer lane so CPU-verified coverage still
/// spans the whole pool).
fn gpu_run_arguments(
    profile: GpuProfile,
    settings: GpuSettings,
    suite_dir: &Path,
    heartbeat: &Path,
) -> Vec<std::ffi::OsString> {
    let mut words: Vec<std::ffi::OsString> = vec![
        "run".into(),
        "--output-dir".into(),
        suite_dir.into(),
        "--progress-file".into(),
        heartbeat.into(),
        "--async-auto".into(),
    ];
    match profile {
        GpuProfile::Full | GpuProfile::AllArithmetic => {
            let mix = if profile == GpuProfile::Full {
                "alternate"
            } else {
                "all"
            };
            words.extend(["--vram-max", "--vram-sweep", "--compute-mix", mix].map(Into::into));
            words.extend([
                "--duration-seconds".into(),
                (settings.duration_minutes * 60).to_string().into(),
            ]);
        }
        // Only the INT8 tensor-core GEMM, briefly: the check docs/gpu-vram's section 7 describes.
        GpuProfile::TensorCheck => words.extend(
            [
                "--compute-mix",
                "int8",
                "--matrix-inner",
                "1024",
                "--duration-seconds",
                "120",
            ]
            .map(Into::into),
        ),
    }
    if settings.temp_limit {
        words.extend([
            "--temp-limit-celsius".into(),
            GPU_TEMP_LIMIT_C.to_string().into(),
        ]);
    }
    words.extend([
        "--bottleneck-log".into(),
        suite_dir.join("bottleneck.jsonl").into(),
    ]);
    words
}

/// GPU thermal abort threshold when the operator turns the limit on: most discrete GPUs start
/// throttling around 83-90 C, so a reading at 90 C means cooling is not keeping up.
const GPU_TEMP_LIMIT_C: u32 = 90;

/// Operator settings on the GPU-VRAM profile screen. Like CPU-RAM, the thermal abort is off
/// unless turned on (telemetry is recorded either way).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GpuSettings {
    /// Minimum duration of profiles 1 and 3 (the tensor check always runs 2 minutes).
    duration_minutes: u64,
    temp_limit: bool,
}

impl Default for GpuSettings {
    fn default() -> Self {
        Self {
            duration_minutes: 30,
            temp_limit: false,
        }
    }
}

/// What GPU-VRAM runs when chosen in the control center.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GpuProfile {
    /// The full DIT: u32 + FP32 GEMM in turn, DRAM sweep, VRAM held to the limit.
    Full,
    /// Two minutes of the INT8 tensor-core GEMM only (compute capability 8.0+), to confirm it
    /// matches the CPU oracle on this card before it joins the full test.
    TensorCheck,
    /// The full DIT with the INT8 tensor-core GEMM in the rotation (skipped without tensor cores).
    AllArithmetic,
}

/// One answer to the profile prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfileChoice {
    Run(GpuProfile),
    Duration,
    TempLimit,
    Back,
    Unknown,
}

impl ProfileChoice {
    fn parse(choice: &str) -> Self {
        match choice.trim().to_ascii_lowercase().as_str() {
            "" | "1" => Self::Run(GpuProfile::Full),
            "2" => Self::Run(GpuProfile::TensorCheck),
            "3" => Self::Run(GpuProfile::AllArithmetic),
            "d" => Self::Duration,
            "t" => Self::TempLimit,
            "b" | "q" => Self::Back,
            _ => Self::Unknown,
        }
    }
}

fn render_gpu_profile_menu(settings: GpuSettings) -> String {
    let limit = if settings.temp_limit {
        format!("ON at {GPU_TEMP_LIMIT_C} C (GPU sensor); the run stops if it is reached")
    } else {
        "OFF (temperature is still recorded)".to_owned()
    };
    let rows = |lines: Vec<String>| lines;
    dit_ui_core::option_screen(
        "DIT GPU-VRAM // TEST PROFILE",
        &[
            rows(vec![
                " [1] Full DIT (default, Enter)  u32 + FP32 GEMM in turn, GPU DRAM sweep, VRAM held to the limit".into(),
                " [2] Tensor INT8 check          2 minutes of the INT8 tensor-core GEMM only (compute capability 8.0+,".into(),
                "                                RTX 30/40/50); confirms it matches the CPU oracle bit for bit".into(),
                " [3] Full DIT + INT8 tensor     profile 1 with the INT8 tensor GEMM in the rotation; use once".into(),
                "                                profile 2 has passed on this kind of card".into(),
            ]),
            rows(vec![
                format!(
                    " [D] DURATION                   {} min for profiles 1 and 3 (the tensor check is always 2 min)",
                    settings.duration_minutes
                ),
                format!(" [T] TEMP LIMIT                 {limit}"),
                " [B] Back to the control center".into(),
            ]),
        ],
        TerminalSize::current(),
    )
}

/// Minutes from the duration prompt: a preset key or a typed number of minutes (1..=1440).
fn parse_gpu_duration(answer: &str) -> Option<u64> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "1" => Some(10),
        "2" => Some(30),
        "3" => Some(60),
        "4" => Some(120),
        typed => typed
            .strip_suffix("m")
            .unwrap_or(typed)
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|minutes| (1..=1440).contains(minutes)),
    }
}

fn prompt_gpu_duration(settings: &mut GpuSettings) -> io::Result<()> {
    print!(
        "\nGPU-VRAM minimum duration: 1) 10 min  2) 30 min  3) 60 min  4) 120 min  or type minutes (1-1440)\nDuration [{}] > ",
        settings.duration_minutes
    );
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if let Some(minutes) = parse_gpu_duration(&answer) {
        settings.duration_minutes = minutes;
    }
    Ok(())
}

/// Asks which GPU-VRAM profile to run (D and T change the settings in place); `None` goes back
/// without starting anything.
fn choose_gpu_profile(settings: &mut GpuSettings) -> io::Result<Option<GpuProfile>> {
    loop {
        print!(
            "\x1b[H\x1b[2J{}\nSelection [1] > ",
            dit_ui_core::decorate(&render_gpu_profile_menu(*settings))
        );
        io::stdout().flush()?;
        let mut choice = String::new();
        io::stdin().read_line(&mut choice)?;
        match ProfileChoice::parse(&choice) {
            ProfileChoice::Run(profile) => return Ok(Some(profile)),
            ProfileChoice::Duration => prompt_gpu_duration(settings)?,
            ProfileChoice::TempLimit => settings.temp_limit = !settings.temp_limit,
            ProfileChoice::Back => return Ok(None),
            ProfileChoice::Unknown => {}
        }
    }
}

fn launch_gpu(
    args: &MenuArgs,
    settings: &mut GpuSettings,
) -> Result<(), Box<dyn std::error::Error>> {
    require_file(&args.gpu_binary, "GPU-VRAM binary")?;
    require_file(&args.watchdog_binary, "DIT watchdog")?;
    let Some(profile) = choose_gpu_profile(settings)? else {
        return Ok(());
    };
    let suite_run_id = unique_id("suite-gpu-vram");
    let suite_dir = runtime_gpu_dir(&suite_run_id);
    create_dir_all(&suite_dir)?;
    let heartbeat = suite_dir.join("watchdog-heartbeat.json");
    let status = watchdog_command(
        args,
        "gpu-vram",
        &suite_run_id,
        &suite_dir,
        Some(&heartbeat),
        std::iter::empty::<PathBuf>(),
    )
    .arg(&args.gpu_binary)
    .args(gpu_run_arguments(
        profile, *settings, &suite_dir, &heartbeat,
    ))
    // Step F3: GPU failures carry where they went wrong (boot option `dit_attribution=1`).
    .args(args.attribution_checks.then_some("--boundary-checks"))
    .status()?;
    let report_path = suite_dir.join("report.json");
    let completion = if report_path.is_file() {
        let report: GpuReportView = serde_json::from_slice(&std::fs::read(&report_path)?)?;
        let mut message = gpu_completion_message(&report);
        if profile == GpuProfile::TensorCheck {
            message.push('\n');
            message.push_str(&tensor_check_verdict(&report));
        }
        message.push('\n');
        message.push_str(&fault_single::summarize(
            fault_single::SingleModule::Gpu,
            &report_path,
            &args.results_root,
            &suite_dir,
            &suite_run_id,
            verdict_name(report.verdict),
            args.attribution_checks,
        ));
        message
    } else if status.success() {
        "GPU-VRAM stopped without a final report.".to_owned()
    } else {
        "GPU-VRAM was aborted or failed before a final report was written.".to_owned()
    };
    let storage =
        match persist_run_artifacts(&suite_dir, &args.results_root, &suite_run_id, "gpu-vram") {
            Ok(Some(path)) => format!("Final report bundle copied once to: {}", path.display()),
            Ok(None) => format!(
                "Verified result volume unavailable; artifacts remain in tmpfs: {}",
                suite_dir.display()
            ),
            Err(error) => format!(
                "Final report copy failed ({error}); artifacts remain in tmpfs: {}",
                suite_dir.display()
            ),
        };
    pause(&format!("{completion}\n{storage}"))?;
    Ok(())
}

fn watchdog_command<I>(
    args: &MenuArgs,
    module: &str,
    run_id: &str,
    suite_dir: &Path,
    heartbeat: Option<&Path>,
    activity_paths: I,
) -> ProcessCommand
where
    I: IntoIterator<Item = PathBuf>,
{
    let mut command = ProcessCommand::new(&args.watchdog_binary);
    command
        .arg("supervise")
        .arg("--module")
        .arg(module)
        .arg("--run-id")
        .arg(run_id)
        .arg("--run-dir")
        .arg(suite_dir)
        .arg("--stale-after-seconds")
        .arg(if module.ends_with("nvme") {
            "180"
        } else {
            "90"
        })
        .arg("--kill-after-seconds")
        .arg("30");
    if let Some(path) = heartbeat {
        command.arg("--heartbeat").arg(path);
    }
    for path in activity_paths {
        command.arg("--activity-path").arg(path);
    }
    if args.results_root.is_dir() {
        command.arg("--recovery-marker").arg(recovery_marker_path(
            &args.results_root,
            run_id,
            module,
        ));
    }
    command.arg("--");
    command
}

fn recovery_marker_path(results_root: &Path, run_id: &str, module: &str) -> PathBuf {
    results_root
        .join("watchdog")
        .join(format!("{run_id}-{module}.json"))
}

fn interrupted_run_notice(results_root: &Path) -> Option<String> {
    let markers = fs::read_dir(results_root.join("watchdog")).ok()?;
    markers.filter_map(Result::ok).find_map(|entry| {
        let contents = fs::read(entry.path()).ok()?;
        let marker: WatchdogMarker = serde_json::from_slice(&contents).ok()?;
        matches!(marker.state.as_str(), "STARTING" | "RUNNING" | "STALLED")
            .then(|| format!("{} {} ({})", marker.module, marker.run_id, marker.state))
    })
}

#[derive(serde::Deserialize)]
struct WatchdogMarker {
    module: String,
    run_id: String,
    state: String,
}

#[derive(serde::Deserialize)]
struct GpuReportView {
    verdict: Verdict,
    hardware_tested: bool,
    /// `clean` / `warnings` / `errors` / `unavailable`; absent in older and multi-GPU reports.
    #[serde(default)]
    hardware_status: Option<String>,
    #[serde(default)]
    failures: Vec<GpuFailureView>,
    #[serde(default)]
    detection: Option<GpuDetectionView>,
}

#[derive(serde::Deserialize)]
struct GpuFailureView {
    #[serde(default)]
    detail: String,
}

#[derive(serde::Deserialize)]
struct GpuDetectionView {
    compute: GpuComputeView,
}

#[derive(serde::Deserialize)]
struct GpuComputeView {
    #[serde(default)]
    verified_int8_tensor_ops: u64,
}

/// What a tensor check (profile 2) showed, in words an operator can act on.
fn tensor_check_verdict(report: &GpuReportView) -> String {
    let tensor_ops = report
        .detection
        .as_ref()
        .map_or(0, |d| d.compute.verified_int8_tensor_ops);
    let refused = report
        .failures
        .iter()
        .any(|f| f.detail.contains("compute capability 8.0"));
    match report.verdict {
        Verdict::Pass if tensor_ops > 0 => format!(
            "TENSOR CHECK PASSED: {tensor_ops} INT8 tensor-core operations matched the CPU oracle bit for bit. Profile 3 (Full DIT + INT8 tensor) can be used on this card."
        ),
        Verdict::Pass => "TENSOR CHECK NOT RUN: no CUDA GPU with the tensor path was tested (an Intel/AMD GPU runs without it).".to_owned(),
        _ if refused => "TENSOR CHECK NOT AVAILABLE: this GPU has no INT8 tensor-core path (compute capability below 8.0). Use profile 1.".to_owned(),
        Verdict::Fail => "TENSOR CHECK FAILED: INT8 tensor-core results differed from the CPU oracle. If every batch failed from the start, and on more than one card, suspect the tensor kernel's software rather than this GPU; keep report.json.".to_owned(),
        _ => "TENSOR CHECK INCOMPLETE: the run did not finish; see report.json.".to_owned(),
    }
}

/// What the menu prints when a GPU-VRAM run ends, hardware events included: a PASS with
/// corrected errors must not read like a clean one.
fn gpu_completion_message(report: &GpuReportView) -> String {
    let hardware = match report.hardware_status.as_deref() {
        Some("warnings") => {
            "\nHARDWARE WARNINGS: corrected hardware errors were logged during the run (data still matched). See detection.hardware_health in report.json."
        }
        Some("errors") => {
            "\nHARDWARE ERRORS: uncorrectable or device-lost events were logged during the run. See detection.hardware_health in report.json."
        }
        Some("unavailable") => {
            "\nHardware error sources (kernel log) could not be read; corrected errors cannot be ruled out."
        }
        _ => "",
    };
    format!(
        "GPU-VRAM validation completed with {}; hardware transfer/compute tested: {}.{hardware}",
        verdict_name(report.verdict),
        if report.hardware_tested { "yes" } else { "no" }
    )
}

fn runtime_suite_dir(run_id: &str) -> PathBuf {
    Path::new("/run/dit-suite").join(run_id).join("cpu-nvme")
}

fn runtime_gpu_dir(run_id: &str) -> PathBuf {
    Path::new("/run/dit-suite").join(run_id).join("gpu-vram")
}

fn persist_run_artifacts(
    source: &Path,
    results_root: &Path,
    run_id: &str,
    module_directory: &str,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    const ARTIFACTS: [&str; 8] = [
        "selection.json",
        "auto-storage-plan.json",
        "events.jsonl",
        "report.json",
        "auto-storage-outcome.json",
        "suite-report.json",
        "watchdog-status.json",
        "watchdog-final.json",
    ];
    let Ok(results_metadata) = results_root.symlink_metadata() else {
        return Ok(None);
    };
    if !results_metadata.file_type().is_dir() {
        return Ok(None);
    }
    let files = ARTIFACTS
        .into_iter()
        .filter_map(|name| {
            let path = source.join(name);
            path.symlink_metadata()
                .ok()
                .filter(|metadata| metadata.file_type().is_file())
                .map(|_| (name, path))
        })
        .collect::<Vec<_>>();
    if files.is_empty() {
        return Ok(None);
    }
    let runs = results_root.join("runs");
    match runs.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err("results/runs is not a real directory".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => create_dir(&runs)?,
        Err(error) => return Err(error.into()),
    }
    let run_root = runs.join(run_id);
    create_dir(&run_root)?;
    if !matches!(module_directory, "cpu-nvme" | "gpu-vram") {
        return Err("unsupported result module directory".into());
    }
    let destination = run_root.join(module_directory);
    create_dir(&destination)?;
    for (name, source_path) in files {
        copy_new_file(&source_path, &destination.join(name))?;
    }
    if module_directory == "gpu-vram" {
        copy_gpu_worker_artifacts(source, &destination)?;
    }
    Ok(Some(destination))
}

fn copy_gpu_worker_artifacts(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name_text = name.to_string_lossy();
        let Some(suffix) = name_text.strip_prefix("device-") else {
            continue;
        };
        if !file_type.is_dir()
            || suffix.is_empty()
            || !suffix.chars().all(|character| character.is_ascii_digit())
        {
            continue;
        }
        let mut copied = false;
        let worker_destination = destination.join(&name);
        for artifact in ["report.json", "progress.json", "worker.log"] {
            let source_path = entry.path().join(artifact);
            if source_path
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_file())
            {
                if !copied {
                    create_dir(&worker_destination)?;
                    copied = true;
                }
                copy_new_file(&source_path, &worker_destination.join(artifact))?;
            }
        }
    }
    Ok(())
}

fn copy_new_file(source: &Path, destination: &Path) -> io::Result<()> {
    let mut input = OpenOptions::new().read(true).open(source)?;
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    io::copy(&mut input, &mut output)?;
    output.flush()?;
    output.sync_all()
}

fn aggregate(args: &AggregateArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let cpu = adapt_cpu_ram_report(&args.cpu_report, &std::fs::read(&args.cpu_report)?)?;
    let nvme = adapt_nvme_report(&args.nvme_report, &std::fs::read(&args.nvme_report)?)?;
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let memory = available_memory_bytes().unwrap_or(1024 * 1024 * 1024);
    let report = SuiteReport::new(
        unique_id("aggregate"),
        vec![cpu, nvme],
        allocate_resources(ExecutionPlan::CombinedConcurrent, threads, memory)?,
    );
    write_new_json(&args.output, &report)?;
    println!(
        "{} modules={} report={}",
        verdict_name(report.aggregate_verdict),
        report.modules.len(),
        args.output.display()
    );
    Ok(if report.aggregate_verdict == Verdict::Pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn write_new_json(path: &Path, value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}

fn require_file(path: &Path, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    if path.is_file() {
        Ok(())
    } else {
        Err(format!("{label} is unavailable: {}", path.display()).into())
    }
}

fn pause(message: &str) -> io::Result<()> {
    println!("\n{message}\nPress Enter to return to the control center.");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(())
}

fn sync_filesystems() {
    let _ = ProcessCommand::new("sync").status();
}

fn unique_id(prefix: &str) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    format!("{prefix}-{stamp}-{}", std::process::id())
}

/// Local time as `YYYY-MM-DD HH:MM:SS +ZZZZ` (the image's clock; `date` keeps the zone rules).
fn local_timestamp() -> String {
    ProcessCommand::new("date")
        .arg("+%Y-%m-%d %H:%M:%S %z")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(
            || {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
                    .to_string()
            },
            |output| String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )
}

/// The booted image's ID (`/etc/cpu-dit/image-id`), or `dev` outside the image.
fn image_id() -> String {
    fs::read_to_string("/etc/cpu-dit/image-id")
        .map(|id| id.trim().to_owned())
        .ok()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| "dev".to_owned())
}

fn available_memory_bytes() -> Option<u64> {
    let content = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = content.lines().find_map(|line| {
        line.strip_prefix("MemAvailable:")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })?;
    kib.checked_mul(1024)
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
    fn power_menu_is_fourth_and_launcher_sets_separate_profile_without_autostart() {
        let cli = Cli::try_parse_from(["dit-suite", "render-menu"]).unwrap();
        let Command::RenderMenu(args) = cli.command else {
            panic!("wrong command")
        };
        let modules = registry(&args);
        assert_eq!(modules[3].module, ModuleKind::CpuPower);
        assert!(render(&args).contains("[4] CPU AVAIL / POWER"));
        let command = power_ui_command(&args);
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("CPU_DIT_POWER_UI")],
            Some(std::ffi::OsStr::new("1"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("CPU_DIT_UI_AUTOSTART_SECONDS")],
            Some(std::ffi::OsStr::new("0"))
        );
        assert_eq!(
            env[std::ffi::OsStr::new("CPU_DIT_WATCHDOG_RESULTS_ROOT")],
            Some(args.results_root.as_os_str())
        );
    }

    #[test]
    fn cpu_ssd_artifacts_are_staged_then_copied_once() {
        let root = std::env::temp_dir().join(unique_id("dit-orchestrator-copy-test"));
        let source = root.join("runtime");
        let results = root.join("results");
        create_dir_all(&source).unwrap();
        create_dir_all(&results).unwrap();
        std::fs::write(source.join("report.json"), b"report").unwrap();
        std::fs::write(source.join("events.jsonl"), b"event\n").unwrap();
        std::fs::write(source.join("watchdog-final.json"), b"watchdog").unwrap();
        std::fs::write(source.join("not-an-artifact"), b"ignore").unwrap();

        let destination = persist_run_artifacts(&source, &results, "run-1", "cpu-nvme")
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(destination.join("report.json")).unwrap(),
            b"report"
        );
        assert_eq!(
            std::fs::read(destination.join("events.jsonl")).unwrap(),
            b"event\n"
        );
        assert_eq!(
            std::fs::read(destination.join("watchdog-final.json")).unwrap(),
            b"watchdog"
        );
        assert!(!destination.join("not-an-artifact").exists());
        assert!(source.join("report.json").is_file());
        assert!(persist_run_artifacts(&source, &results, "run-1", "cpu-nvme").is_err());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cpu_ssd_artifacts_stay_in_runtime_without_results_volume() {
        let missing = std::env::temp_dir().join(unique_id("dit-orchestrator-missing-results"));
        assert!(
            persist_run_artifacts(Path::new("/run/unused"), &missing, "run-1", "cpu-nvme")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cpu_ssd_runtime_is_never_the_usb_results_tree() {
        assert_eq!(
            runtime_suite_dir("run-1"),
            PathBuf::from("/run/dit-suite/run-1/cpu-nvme")
        );
    }

    #[test]
    fn gpu_runtime_is_staged_in_tmpfs_tree() {
        assert_eq!(
            runtime_gpu_dir("run-1"),
            PathBuf::from("/run/dit-suite/run-1/gpu-vram")
        );
    }

    #[test]
    fn gpu_menu_runs_the_async_pipeline_and_keeps_its_bottleneck_log_with_the_report() {
        let suite = Path::new("/run/dit-suite/run-1/gpu-vram");
        let arguments = gpu_run_arguments(
            GpuProfile::Full,
            GpuSettings::default(),
            suite,
            &suite.join("watchdog-heartbeat.json"),
        );
        let words: Vec<&str> = arguments.iter().filter_map(|a| a.to_str()).collect();
        assert_eq!(words.first(), Some(&"run"));
        assert!(words.contains(&"--async-auto"));
        assert!(
            words.contains(&"--vram-max"),
            "the menu holds VRAM up to the driver limit"
        );
        assert!(
            words.contains(&"--vram-sweep"),
            "the menu runs the GPU-DRAM sweep"
        );
        let mix = words.iter().position(|w| *w == "--compute-mix");
        assert_eq!(
            mix.and_then(|at| words.get(at + 1)),
            Some(&"alternate"),
            "the menu runs the integer and the FP32 GEMM in turn"
        );
        // Never the CUDA-only flag: an Intel/AMD GPU would be refused instead of run sequentially.
        assert!(!words.contains(&"--async-compute"));
        // The GPU-local workload costs verified coverage below the sequential path (measured).
        assert!(!words.contains(&"--vram-local"));
        let log = words
            .iter()
            .position(|word| *word == "--bottleneck-log")
            .and_then(|at| words.get(at + 1))
            .expect("bottleneck log path");
        assert_eq!(*log, "/run/dit-suite/run-1/gpu-vram/bottleneck.jsonl");
    }

    #[test]
    fn gpu_profiles_choose_their_workloads_and_the_tensor_check_is_explained() {
        let suite = Path::new("/run/dit-suite/run-2/gpu-vram");
        let words = |profile| {
            gpu_run_arguments(
                profile,
                GpuSettings::default(),
                suite,
                &suite.join("hb.json"),
            )
            .into_iter()
            .filter_map(|w| w.into_string().ok())
            .collect::<Vec<_>>()
        };
        let mix = |w: &[String]| {
            let at = w.iter().position(|x| x == "--compute-mix").unwrap();
            w[at + 1].clone()
        };
        let tensor = words(GpuProfile::TensorCheck);
        assert_eq!(mix(&tensor), "int8");
        assert!(tensor.contains(&"--async-auto".to_owned()));
        assert!(
            !tensor.contains(&"--vram-sweep".to_owned()),
            "the check runs the GEMM alone"
        );
        assert_eq!(mix(&words(GpuProfile::AllArithmetic)), "all");
        assert_eq!(mix(&words(GpuProfile::Full)), "alternate");
        assert_eq!(
            ProfileChoice::parse("\n"),
            ProfileChoice::Run(GpuProfile::Full)
        );
        assert_eq!(
            ProfileChoice::parse("2"),
            ProfileChoice::Run(GpuProfile::TensorCheck)
        );
        assert_eq!(ProfileChoice::parse("b"), ProfileChoice::Back);
        assert_eq!(ProfileChoice::parse("9"), ProfileChoice::Unknown);

        let report =
            |json: serde_json::Value| serde_json::from_value::<GpuReportView>(json).unwrap();
        let passed = report(
            serde_json::json!({"verdict": "PASS", "hardware_tested": true,
            "detection": {"compute": {"verified_int8_tensor_ops": 5}}}),
        );
        assert!(tensor_check_verdict(&passed).starts_with("TENSOR CHECK PASSED"));
        let refused = report(
            serde_json::json!({"verdict": "INVALID", "hardware_tested": false,
            "failures": [{"detail": "INT8 tensor GEMM needs compute capability 8.0 or newer"}]}),
        );
        assert!(tensor_check_verdict(&refused).starts_with("TENSOR CHECK NOT AVAILABLE"));
        let failed = report(serde_json::json!({"verdict": "FAIL", "hardware_tested": false}));
        assert!(tensor_check_verdict(&failed).starts_with("TENSOR CHECK FAILED"));
    }

    #[test]
    fn gpu_duration_and_temperature_limit_reach_the_run() {
        let suite = Path::new("/run/dit-suite/run-3/gpu-vram");
        let run = |profile, settings| {
            gpu_run_arguments(profile, settings, suite, &suite.join("hb.json"))
                .into_iter()
                .filter_map(|w| w.into_string().ok())
                .collect::<Vec<_>>()
        };
        let value = |w: &[String], flag: &str| {
            w.iter()
                .position(|x| x == flag)
                .and_then(|at| w.get(at + 1).cloned())
        };
        let defaults = run(GpuProfile::Full, GpuSettings::default());
        assert_eq!(
            value(&defaults, "--duration-seconds").as_deref(),
            Some("1800")
        );
        assert_eq!(
            value(&defaults, "--temp-limit-celsius"),
            None,
            "off by default"
        );
        let chosen = GpuSettings {
            duration_minutes: 10,
            temp_limit: true,
        };
        let full = run(GpuProfile::AllArithmetic, chosen);
        assert_eq!(value(&full, "--duration-seconds").as_deref(), Some("600"));
        assert_eq!(value(&full, "--temp-limit-celsius").as_deref(), Some("90"));
        let tensor = run(GpuProfile::TensorCheck, chosen);
        assert_eq!(value(&tensor, "--duration-seconds").as_deref(), Some("120"));
        assert_eq!(
            value(&tensor, "--temp-limit-celsius").as_deref(),
            Some("90")
        );

        assert_eq!(parse_gpu_duration("1"), Some(10));
        assert_eq!(parse_gpu_duration("4"), Some(120));
        assert_eq!(parse_gpu_duration(" 45m "), Some(45));
        assert_eq!(parse_gpu_duration("0"), None);
        assert_eq!(parse_gpu_duration("2000"), None);
        assert_eq!(ProfileChoice::parse("D"), ProfileChoice::Duration);
        assert_eq!(ProfileChoice::parse("t"), ProfileChoice::TempLimit);
        let screen = render_gpu_profile_menu(chosen);
        assert!(screen.contains("10 min") && screen.contains("ON at 90 C"));
    }

    #[test]
    fn gpu_completion_names_hardware_warnings_and_errors() {
        let view = |status: Option<&str>| {
            let mut json = serde_json::json!({"verdict": "PASS", "hardware_tested": true});
            if let Some(status) = status {
                json["hardware_status"] = status.into();
            }
            serde_json::from_value::<GpuReportView>(json).unwrap()
        };
        assert!(!gpu_completion_message(&view(None)).contains("HARDWARE"));
        assert!(!gpu_completion_message(&view(Some("clean"))).contains("HARDWARE"));
        assert!(gpu_completion_message(&view(Some("warnings"))).contains("HARDWARE WARNINGS"));
        assert!(gpu_completion_message(&view(Some("errors"))).contains("HARDWARE ERRORS"));
        assert!(gpu_completion_message(&view(Some("unavailable"))).contains("cannot be ruled out"));
    }

    #[test]
    fn gpu_artifacts_are_copied_under_gpu_vram_directory() {
        let root = std::env::temp_dir().join(unique_id("dit-orchestrator-gpu-copy-test"));
        let source = root.join("runtime");
        let results = root.join("results");
        create_dir_all(&source).unwrap();
        create_dir_all(&results).unwrap();
        std::fs::write(source.join("report.json"), b"gpu-report").unwrap();
        let destination = persist_run_artifacts(&source, &results, "run-gpu", "gpu-vram")
            .unwrap()
            .unwrap();
        assert_eq!(destination, results.join("runs/run-gpu/gpu-vram"));
        assert_eq!(
            std::fs::read(destination.join("report.json")).unwrap(),
            b"gpu-report"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gpu_multi_worker_reports_progress_and_logs_are_preserved() {
        let root = std::env::temp_dir().join(unique_id("dit-orchestrator-gpu-workers"));
        let source = root.join("runtime");
        let results = root.join("results");
        let worker = source.join("device-1");
        create_dir_all(&worker).unwrap();
        create_dir_all(&results).unwrap();
        std::fs::write(source.join("report.json"), b"aggregate").unwrap();
        std::fs::write(worker.join("report.json"), b"device report").unwrap();
        std::fs::write(worker.join("progress.json"), b"progress").unwrap();
        std::fs::write(worker.join("worker.log"), b"log").unwrap();
        std::fs::write(worker.join("not-packaged"), b"ignore").unwrap();

        let destination = persist_run_artifacts(&source, &results, "run-gpu", "gpu-vram")
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read(destination.join("device-1/report.json")).unwrap(),
            b"device report"
        );
        assert!(destination.join("device-1/progress.json").is_file());
        assert!(destination.join("device-1/worker.log").is_file());
        assert!(!destination.join("device-1/not-packaged").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_notice_only_shows_a_nonterminal_watchdog_marker() {
        let root = std::env::temp_dir().join(unique_id("dit-watchdog-recovery"));
        let watchdog = root.join("watchdog");
        create_dir_all(&watchdog).unwrap();
        let marker = watchdog.join("run-cpu-ram.json");
        std::fs::write(
            &marker,
            r#"{"module":"cpu-ram","run_id":"run","state":"RUNNING"}"#,
        )
        .unwrap();
        assert_eq!(
            interrupted_run_notice(&root).as_deref(),
            Some("cpu-ram run (RUNNING)")
        );
        std::fs::write(
            &marker,
            r#"{"module":"cpu-ram","run_id":"run","state":"PASSED"}"#,
        )
        .unwrap();
        assert!(interrupted_run_notice(&root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
