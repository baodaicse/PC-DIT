//! The combined run (`docs/COMBINED_STRESS_CHAIN_ANALYSIS.md` sections 5-8 and 11): storage
//! setup, the phase schedule, the four layers as supervised child processes, the live screen,
//! the arbiter at the overlap start, and one combined report.
//!
//! A layer that reports a mismatch is counted on the screen and in the report while every layer
//! runs on (decision 11.1); a layer that ends without a report (crash, OOM, watchdog kill) stops
//! the session, which is then INVALID.
use super::combined_layers::{
    self as layers, ChainWork, CpuMemory, GpuOptions, Launch, LiveLayer, SsdTally, StoragePlan,
};
use super::combined_view::{
    ChainMeter, DiskMeter, LayerLine, SessionView, Telemetry, format_bytes, format_duration,
    mem_available, render,
};
use super::{INTERRUPTED, MenuArgs, copy_new_file, pause, unique_id, watchdog_command};
use dit_orchestrator::combined::{
    self, Available, Holding, Intensity, Layer, LayerOutcome, Level, MemoryPolicy, Phase,
    PhaseKind, ThreadPlan,
};
use dit_ui_core::{InPlaceRenderer, TerminalSize};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs::{File, create_dir_all};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MIB: u64 = 1024 * 1024;
/// A layer still running this long after its phase should have ended is drained.
const OVERRUN_GRACE: Duration = Duration::from_mins(15);
const RULE: &str =
    "+--------------------------------------------------------------------------------+";
const READY_WAIT: Duration = Duration::from_mins(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LevelArg {
    Bounded,
    Deep,
    Soak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum IntensityArg {
    Balanced,
    Max,
}

#[derive(Debug, clap::Args)]
pub struct CombinedCli {
    #[command(flatten)]
    menu: MenuArgs,
    #[arg(long, value_enum, default_value_t = LevelArg::Bounded)]
    level: LevelArg,
    #[arg(long, value_enum, default_value_t = IntensityArg::Balanced)]
    intensity: IntensityArg,
    /// SOAK length in hours.
    #[arg(long, default_value_t = 4)]
    soak_hours: u64,
    /// GPU thermal abort at 90 C (always on with `--intensity max`).
    #[arg(long)]
    gpu_temp_limit: bool,
    /// A storage plan to use instead of the interactive SSD picker (automation, validation).
    #[arg(long)]
    storage_plan: Option<PathBuf>,
    /// Phase lengths as a percentage of the level's schedule (validation runs only).
    #[arg(long, default_value_t = 100, hide = true)]
    time_scale_percent: u64,
    /// System reserve kept by the arbiter (validation on small machines only).
    #[arg(long, default_value_t = combined::SYSTEM_RESERVE_BYTES / MIB, hide = true)]
    reserve_mib: u64,
    /// Smallest CPU-RAM arena the arbiter accepts (validation on small machines only).
    #[arg(long, default_value_t = combined::CPU_RAM_MINIMUM_BYTES / MIB, hide = true)]
    cpu_ram_minimum_mib: u64,
    /// Negative gate: one injected fault in this layer during the overlap.
    #[arg(long, value_enum, hide = true)]
    inject_overlap: Option<InjectLayerArg>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum InjectLayerArg {
    CpuRam,
    Chain,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Settings {
    level: Level,
    intensity: Intensity,
    soak_hours: u64,
    gpu_temp_limit: bool,
    time_scale_percent: u64,
    memory: MemoryPolicy,
    inject_overlap: Option<Layer>,
}

/// Menu entry: choose level, intensity and the GPU limit, then run.
pub fn launch(menu: &MenuArgs) -> Result<(), Box<dyn std::error::Error>> {
    let Some(settings) = choose_settings()? else {
        return Ok(());
    };
    let (summary, _) = run_session(menu, settings, None)?;
    pause(&summary)?;
    Ok(())
}

/// `dit-suite combined`: the same session without the menu.
pub fn run_cli(args: &CombinedCli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // Ctrl+C / SIGINT drains every layer and still writes the report.
    ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst))?;
    let intensity = match args.intensity {
        IntensityArg::Balanced => Intensity::Balanced,
        IntensityArg::Max => Intensity::Max,
    };
    let settings = Settings {
        level: match args.level {
            LevelArg::Bounded => Level::Bounded,
            LevelArg::Deep => Level::Deep,
            LevelArg::Soak => Level::Soak,
        },
        intensity,
        soak_hours: args.soak_hours.clamp(1, 72),
        gpu_temp_limit: args.gpu_temp_limit || intensity == Intensity::Max,
        time_scale_percent: args.time_scale_percent.clamp(1, 100),
        memory: MemoryPolicy {
            reserve_bytes: args.reserve_mib * MIB,
            cpu_ram_minimum_bytes: args.cpu_ram_minimum_mib * MIB,
        },
        inject_overlap: args.inject_overlap.map(|layer| match layer {
            InjectLayerArg::CpuRam => Layer::CpuRam,
            InjectLayerArg::Chain => Layer::Chain,
        }),
    };
    let (summary, verdict) = run_session(&args.menu, settings, args.storage_plan.as_deref())?;
    println!("{summary}");
    Ok(match verdict {
        "PASS" | "PASS (HW WARNINGS)" => ExitCode::SUCCESS,
        "FAIL" => ExitCode::from(2),
        _ => ExitCode::from(3),
    })
}

fn ask(question: &str) -> io::Result<String> {
    print!("{question}");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_ascii_lowercase())
}

fn choose_settings() -> io::Result<Option<Settings>> {
    print!("\x1b[H\x1b[2J");
    println!("+-- COMBINED STRESS: CPU-RAM + GPU-VRAM + SSD + DATA CHAIN ----------------------+");
    for line in [
        "Level  1) BOUNDED ~15 min  quick check; a stability verdict needs DEEP or SOAK",
        "       2) DEEP    ~60 min  40 min overlap, rest, retention re-read",
        "       3) SOAK    hours    50 min overlap cycles with rest and retention",
        "Intensity  B) balanced (default): each layer gets its own CPU share",
        "           M) max: every layer at full speed; GPU temp limit forced ON",
        "Baselines run first, so a failure seen only in the overlap points to load,",
        "power or heat. A layer FAIL is counted and the run goes on.",
    ] {
        println!("| {line:<78} |");
    }
    println!("{RULE}");
    let level = match ask("Level [1], Q to go back > ")?.as_str() {
        "q" => return Ok(None),
        "2" => Level::Deep,
        "3" => Level::Soak,
        _ => Level::Bounded,
    };
    let soak_hours = if level == Level::Soak {
        ask("SOAK hours (1-72) [4] > ")?
            .parse::<u64>()
            .ok()
            .filter(|hours| (1..=72).contains(hours))
            .unwrap_or(4)
    } else {
        0
    };
    let intensity = if ask("Intensity [B] > ")? == "m" {
        Intensity::Max
    } else {
        Intensity::Balanced
    };
    let gpu_temp_limit = intensity == Intensity::Max
        || matches!(
            ask("GPU temperature limit 90 C? (y/N) > ")?.as_str(),
            "y" | "yes"
        );
    Ok(Some(Settings {
        level,
        intensity,
        soak_hours,
        gpu_temp_limit,
        time_scale_percent: 100,
        memory: MemoryPolicy::default(),
        inject_overlap: None,
    }))
}

/// A layer running under its watchdog.
struct Running {
    layer: Layer,
    dir: PathBuf,
    child: Child,
    peak_bytes: u64,
    resident_bytes: u64,
    tally: Option<SsdTally>,
    live: LiveLayer,
    drained: bool,
}

#[derive(Clone, Debug, Serialize)]
struct LayerRecord {
    layer: Layer,
    outcome: LayerOutcome,
    failures: u64,
    calculations: u64,
    peak_host_bytes: u64,
    directory: String,
    detail: String,
}

#[derive(Clone, Debug, Serialize)]
struct PhaseRecord {
    index: usize,
    phase: PhaseKind,
    label: String,
    planned_seconds: u64,
    started_unix_ms: u128,
    seconds: f64,
    completed: bool,
    layers: Vec<LayerRecord>,
    note: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct MemorySample {
    at_seconds: u64,
    mem_available_mib: u64,
    held_by_tests_mib: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct Totals {
    failures: u64,
    calculations: u64,
}

#[derive(Serialize)]
struct CombinedReport<'a> {
    schema_version: u32,
    module_id: &'static str,
    run_id: &'a str,
    verdict: &'static str,
    grade: Option<Level>,
    completed: bool,
    stopped_by_operator: bool,
    abort_reason: Option<String>,
    settings: Settings,
    available: Available,
    threads: ThreadPlan,
    gpu_host_budget_bytes: u64,
    cpu_ram_arena_mib: Option<u64>,
    storage: Option<&'a StoragePlan>,
    phases: &'a [PhaseRecord],
    layer_failures: BTreeMap<&'static str, u64>,
    failures_total: u64,
    calculations_total: u64,
    memory_timeline: &'a [MemorySample],
    elapsed_seconds: f64,
}

struct Session<'a> {
    menu: &'a MenuArgs,
    settings: Settings,
    run_id: String,
    dir: PathBuf,
    started: Instant,
    telemetry: Telemetry,
    storage: Option<StoragePlan>,
    available: Available,
    threads: ThreadPlan,
    gpu_budget: u64,
    gpu_count: usize,
    phases: Vec<Phase>,
    planned_seconds: u64,
    current: usize,
    records: Vec<PhaseRecord>,
    peaks: BTreeMap<Layer, u64>,
    totals: BTreeMap<Layer, Totals>,
    chain_file: Option<PathBuf>,
    chain_peak_gibps: f64,
    timeline: Vec<MemorySample>,
    last_sample: Option<Instant>,
    arena_mib: Option<u64>,
    abort: Option<String>,
    renderer: Option<InPlaceRenderer<io::Stdout>>,
    last_frame: Option<Instant>,
    last_line: Option<Instant>,
    phase_started: Instant,
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn random_seed() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    #[allow(clippy::cast_possible_truncation)]
    let low = nanos as u64;
    low ^ u64::from(std::process::id()).rotate_left(32)
}

/// What the SSD setup produced.
enum Storage {
    /// A plan (READY or NONE) and, when READY, the process holding the directories.
    Plan(Box<StoragePlan>, Option<Child>),
    /// No plan: the combined run goes on without the SSD layer and the data chain.
    Without,
    /// The operator went back to the control center.
    Back,
}

/// Starts the interactive storage holder and waits for its plan. When the setup fails (for
/// example a Windows volume left hibernated), the operator retries the selection, goes on
/// without the SSD layer and the chain, or goes back to the menu.
fn obtain_storage(menu: &MenuArgs, dir: &Path) -> Result<Storage, Box<dyn std::error::Error>> {
    if !menu.nvme_binary.is_file() || !io::stdin().is_terminal() {
        return Ok(Storage::Without);
    }
    for attempt in 1.. {
        let storage_dir = dir.join(format!("storage-{attempt}"));
        let mut command = std::process::Command::new(&menu.nvme_binary);
        command
            .arg("combined-storage")
            .arg("--output-dir")
            .arg(&storage_dir);
        for device in &menu.protected_devices {
            command.arg("--protected-device").arg(device);
        }
        let mut holder = command.spawn()?;
        let plan_path = storage_dir.join("storage-plan.json");
        let status = loop {
            if plan_path.is_file() {
                let plan: StoragePlan = serde_json::from_slice(&std::fs::read(&plan_path)?)?;
                let holder = (plan.state == "READY").then_some(holder);
                return Ok(Storage::Plan(Box::new(plan), holder));
            }
            if let Some(status) = holder.try_wait()? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        if plan_path.is_file() {
            continue;
        }
        println!(
            "\nSSD setup ended ({status}) without a storage plan (the reason is printed above)."
        );
        match ask(
            "R) retry the SSD selection  C) continue without the SSD layer and the data chain  Q) back to the menu [R] > ",
        )?
        .as_str()
        {
            "c" => return Ok(Storage::Without),
            "q" => return Ok(Storage::Back),
            _ => {}
        }
    }
    Ok(Storage::Without)
}

fn release_storage(holder: Option<Child>) -> String {
    let Some(mut holder) = holder else {
        return String::new();
    };
    let pid = nix::unistd::Pid::from_raw(i32::try_from(holder.id()).unwrap_or(0));
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = holder.try_wait() {
            return if status.success() {
                "SSD test directories released.".to_owned()
            } else {
                format!(
                    "SSD directory release reported {status}; see storage/auto-storage-outcome.json."
                )
            };
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    "SSD directory release did not finish within 120 s; check the mounts before reuse.".to_owned()
}

#[allow(clippy::too_many_lines)]
fn run_session(
    menu: &MenuArgs,
    settings: Settings,
    storage_plan: Option<&Path>,
) -> Result<(String, &'static str), Box<dyn std::error::Error>> {
    INTERRUPTED.store(false, Ordering::SeqCst);
    let run_id = unique_id("suite-combined");
    let dir = Path::new("/run/dit-suite").join(&run_id).join("combined");
    let dir = if create_dir_all(&dir).is_ok() {
        dir
    } else {
        let fallback = std::env::temp_dir().join(&run_id).join("combined");
        create_dir_all(&fallback)?;
        fallback
    };
    let (storage, holder) = match storage_plan {
        Some(path) => (Some(serde_json::from_slice(&std::fs::read(path)?)?), None),
        None => match obtain_storage(menu, &dir)? {
            Storage::Plan(plan, holder) => (Some(*plan), holder),
            Storage::Without => (None, None),
            Storage::Back => {
                return Ok((
                    "COMBINED cancelled at the SSD setup; no test was started.".to_owned(),
                    "NOT RUN",
                ));
            }
        },
    };
    let disks = storage.as_ref().map_or_else(Vec::new, |plan| {
        plan.devices
            .iter()
            .map(|device| {
                DiskMeter::new(
                    format!(
                        "{} {} GB ({})",
                        device.model,
                        device.capacity_bytes / 1_000_000_000,
                        device.block_name
                    ),
                    device.block_name.clone(),
                )
            })
            .collect()
    });
    let mut telemetry = Telemetry::new(disks);
    let eligible = if menu.gpu_binary.is_file() {
        layers::gpu_eligibility(&menu.gpu_binary)
    } else {
        layers::GpuEligibility::default()
    };
    telemetry.retain_gpus(&eligible.pci_addresses);
    let gpu_count = eligible.pci_addresses.len();
    let ssd_ready = storage
        .as_ref()
        .is_some_and(|plan| plan.state == "READY" && !plan.targets.is_empty());
    let available = Available {
        gpu: gpu_count > 0,
        ssd: ssd_ready && menu.nvme_binary.is_file(),
        chain: eligible.cuda
            && menu.chain_binary.is_file()
            && storage
                .as_ref()
                .is_some_and(|plan| plan.chain_dir.is_some()),
    };
    let logical = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let threads = combined::plan_threads(logical, settings.intensity, available);
    let start_available = mem_available().unwrap_or(0);
    let gpu_budget = combined::gpu_host_budget(start_available);
    let mut phases = combined::schedule(settings.level, available, settings.soak_hours);
    for phase in &mut phases {
        if phase.seconds > 0 {
            phase.seconds = (phase.seconds * settings.time_scale_percent / 100).max(20);
        }
    }
    let prefill_estimate = storage
        .as_ref()
        .map_or(0, |plan| plan.chain_working_set_mib / 1024 * 6);
    let planned_seconds = combined::planned_seconds(&phases, prefill_estimate);
    let mut declared = Vec::new();
    if available.gpu {
        declared.push(gpu_budget * u64::try_from(gpu_count).unwrap_or(1));
    }
    if available.chain {
        declared.push(512 * MIB);
    }
    if available.ssd {
        declared.push(combined::SSD_DEFAULT_PEAK_BYTES);
    }
    if let Err(error) = combined::check_fits(start_available, &declared, settings.memory) {
        let release = release_storage(holder);
        return Ok((
            format!("COMBINED refused before start (fail closed): {error}\n{release}"),
            "INVALID",
        ));
    }
    let renderer = io::stdout()
        .is_terminal()
        .then(|| InPlaceRenderer::new(io::stdout(), 2));
    if renderer.is_some() {
        print!("\x1b[H\x1b[2J");
    }
    let mut session = Session {
        menu,
        settings,
        run_id,
        dir,
        started: Instant::now(),
        telemetry,
        storage,
        available,
        threads,
        gpu_budget,
        gpu_count,
        phases,
        planned_seconds,
        current: 0,
        records: Vec::new(),
        peaks: BTreeMap::new(),
        totals: BTreeMap::new(),
        chain_file: None,
        chain_peak_gibps: 0.0,
        timeline: Vec::new(),
        last_sample: None,
        arena_mib: None,
        abort: None,
        renderer,
        last_frame: None,
        last_line: None,
        phase_started: Instant::now(),
    };
    for index in 0..session.phases.len() {
        if session.abort.is_some() || INTERRUPTED.load(Ordering::SeqCst) {
            break;
        }
        session.current = index;
        session.run_phase(index);
    }
    if let Some(renderer) = session.renderer.as_mut() {
        let _ = renderer.finish();
    }
    let cleanup = session.remove_chain_file();
    let release = release_storage(holder);
    let (verdict, report_path) = session.write_report()?;
    let stored = session.persist();
    let failures: u64 = session.totals.values().map(|t| t.failures).sum();
    let summary = format!(
        "COMBINED {} {} completed with {verdict}; failures counted {failures}; depth reached {}.\n{}\nReport: {}\n{stored}\n{cleanup}{release}",
        session.settings.level.label(),
        session.settings.intensity.label(),
        combined::grade(
            session.settings.level,
            session.records.iter().filter(|r| r.completed).count(),
            session.phases.len()
        )
        .map_or("none (not every phase completed)", Level::label),
        session.layer_summary(),
        report_path.display()
    );
    Ok((summary, verdict))
}

impl Session<'_> {
    fn layer_summary(&self) -> String {
        let mut worst: BTreeMap<Layer, LayerOutcome> = BTreeMap::new();
        for record in self.records.iter().flat_map(|phase| &phase.layers) {
            let rank = |outcome: LayerOutcome| match outcome {
                LayerOutcome::Fail => 5,
                LayerOutcome::Crashed => 4,
                LayerOutcome::Invalid => 3,
                LayerOutcome::PassHwWarnings => 2,
                LayerOutcome::Pass => 1,
                LayerOutcome::NotRun => 0,
            };
            let entry = worst.entry(record.layer).or_insert(LayerOutcome::NotRun);
            if rank(record.outcome) > rank(*entry) {
                *entry = record.outcome;
            }
        }
        worst
            .iter()
            .map(|(layer, outcome)| {
                format!(
                    "{} {} (fail {})",
                    layer.label(),
                    outcome.label(),
                    self.totals.get(layer).map_or(0, |t| t.failures)
                )
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    fn phase_dir(&self, index: usize, layer: Layer) -> PathBuf {
        self.dir
            .join(format!("p{:02}-{}", index + 1, layers::slug(layer)))
    }

    fn spawn(&self, index: usize, layer: Layer, launch: Launch) -> Result<Running, String> {
        let dir = self.phase_dir(index, layer);
        create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        let log = File::create(dir.join("console.log")).map_err(|error| error.to_string())?;
        let error_log = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = watchdog_command(
            self.menu,
            layers::slug(layer),
            &format!("{}-p{:02}", self.run_id, index + 1),
            &dir,
            launch.heartbeat.as_deref(),
            launch.activity,
        );
        command
            .arg(&launch.program)
            .args(&launch.args)
            .envs(launch.env)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log));
        if let Some(current) = launch.current_dir {
            command.current_dir(current);
        }
        let child = command
            .spawn()
            .map_err(|error| format!("start {}: {error}", launch.program.display()))?;
        Ok(Running {
            layer,
            dir,
            child,
            peak_bytes: 0,
            resident_bytes: 0,
            tally: (layer == Layer::Ssd).then(SsdTally::default),
            live: LiveLayer::default(),
            drained: false,
        })
    }

    fn launch_layer(
        &self,
        index: usize,
        layer: Layer,
        seconds: u64,
        overlap: bool,
    ) -> Result<Running, String> {
        let dir = self.phase_dir(index, layer);
        let launch = match layer {
            Layer::CpuRam => {
                let memory = self
                    .arena_mib
                    .filter(|_| overlap)
                    .map_or(CpuMemory::Alone, |mib| CpuMemory::Arena { mib });
                let (workers, oracle) = if overlap {
                    (
                        self.threads.cpu_ram_workers,
                        self.threads.cpu_oracle_workers,
                    )
                } else {
                    let logical =
                        std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
                    let oracle = if logical >= 2 {
                        (logical / 4).max(1)
                    } else {
                        0
                    };
                    (logical - oracle, oracle)
                };
                layers::cpu_ram(
                    self.menu,
                    &dir,
                    seconds,
                    workers,
                    oracle,
                    &memory,
                    self.settings.memory.reserve_bytes / MIB,
                )
            }
            Layer::Gpu => layers::gpu(
                self.menu,
                &dir,
                seconds,
                &GpuOptions {
                    temp_limit: self.settings.gpu_temp_limit,
                    verifier_max: self.threads.gpu_verifier_max.filter(|_| overlap),
                    host_budget_bytes: self.gpu_budget,
                },
            ),
            Layer::Ssd => {
                let storage = self.storage.as_ref().ok_or("no storage plan")?;
                // The baseline writes at most the Light budget; the overlap the chosen level.
                let budget = if overlap {
                    storage.write_budget_mib
                } else {
                    storage.write_budget_mib.min(2048)
                };
                layers::ssd(
                    self.menu,
                    &dir,
                    seconds,
                    storage,
                    budget,
                    self.threads.ssd_workers.filter(|_| overlap),
                    random_seed(),
                )
            }
            Layer::Chain => {
                let file = self.chain_file.as_deref().ok_or("chain records missing")?;
                let generations = match self.settings.intensity {
                    Intensity::Balanced => 4,
                    Intensity::Max => 1,
                };
                layers::chain(
                    self.menu,
                    &dir,
                    &ChainWork::Read {
                        file,
                        seconds,
                        generations,
                    },
                )
            }
        };
        let mut launch = launch;
        if overlap && self.settings.inject_overlap == Some(layer) {
            match layer {
                Layer::CpuRam => launch
                    .args
                    .extend(["--inject-simd-worker".into(), "0".into()]),
                Layer::Chain => launch.args.extend(["--inject".into(), "gpu".into()]),
                Layer::Gpu | Layer::Ssd => {}
            }
        }
        self.spawn(index, layer, launch)
    }

    fn run_phase(&mut self, index: usize) {
        let phase = self.phases[index];
        self.phase_started = Instant::now();
        let mut record = PhaseRecord {
            index: index + 1,
            phase: phase.kind,
            label: phase.kind.label(),
            planned_seconds: phase.seconds,
            started_unix_ms: unix_ms(),
            seconds: 0.0,
            completed: false,
            layers: Vec::new(),
            note: None,
        };
        let seconds = phase.seconds;
        let result = match phase.kind {
            PhaseKind::Prefill => self.prefill(index, &mut record),
            PhaseKind::Baseline(layer) => self
                .launch_layer(index, layer, seconds, false)
                .map(|running| self.supervise(vec![running], seconds, &mut record)),
            PhaseKind::ChainAlone | PhaseKind::Retention => {
                if self.chain_file.is_some() {
                    self.launch_layer(index, Layer::Chain, seconds, false)
                        .map(|running| self.supervise(vec![running], seconds, &mut record))
                } else {
                    Err("chain records are unavailable (prefill did not complete)".into())
                }
            }
            PhaseKind::Rest => {
                self.supervise(Vec::new(), seconds, &mut record);
                Ok(())
            }
            PhaseKind::Overlap => {
                self.overlap(index, seconds, &mut record);
                Ok(())
            }
        };
        if let Err(error) = result {
            record.note = Some(error);
        }
        record.seconds = self.phase_started.elapsed().as_secs_f64();
        record.completed = self.abort.is_none()
            && !INTERRUPTED.load(Ordering::SeqCst)
            && record.note.is_none()
            && record
                .layers
                .iter()
                .all(|layer| layer.outcome != LayerOutcome::Crashed);
        self.records.push(record);
    }

    fn prefill(&mut self, index: usize, record: &mut PhaseRecord) -> Result<(), String> {
        let storage = self.storage.as_ref().ok_or("no storage plan")?;
        let target = storage.chain_dir.clone().ok_or("no chain directory")?;
        let dir = self.phase_dir(index, Layer::Chain);
        let launch = layers::chain(
            self.menu,
            &dir,
            &ChainWork::Prefill {
                dir: &target,
                working_set_mib: storage.chain_working_set_mib,
            },
        );
        let running = self.spawn(index, Layer::Chain, launch)?;
        self.supervise(vec![running], 0, record);
        let report: Option<serde_json::Value> = std::fs::read(dir.join("report.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        self.chain_file = report
            .as_ref()
            .filter(|report| report["file_kept"].as_bool() == Some(true))
            .and_then(|report| report["file"].as_str())
            .map(PathBuf::from);
        if self.chain_file.is_none() {
            return Err("prefill kept no record file; chain phases are skipped".into());
        }
        Ok(())
    }

    /// Starts the overlap in the order of section 6.1 (chain, GPU, SSD, each once ready), then
    /// gives CPU-RAM what `MemAvailable` leaves after the others' declared growth.
    fn overlap(&mut self, index: usize, seconds: u64, record: &mut PhaseRecord) {
        let end = Instant::now() + Duration::from_secs(seconds);
        let remaining = || {
            end.saturating_duration_since(Instant::now())
                .as_secs()
                .max(60)
        };
        let mut running: Vec<Running> = Vec::new();
        let mut order = Vec::new();
        if self.available.chain && self.chain_file.is_some() {
            order.push(Layer::Chain);
        }
        if self.available.gpu {
            order.push(Layer::Gpu);
        }
        if self.available.ssd {
            order.push(Layer::Ssd);
        }
        for layer in order {
            match self.launch_layer(index, layer, remaining(), true) {
                Ok(started) => {
                    running.push(started);
                    self.wait_ready(&mut running, record);
                }
                Err(error) => record.layers.push(self.not_started(index, layer, error)),
            }
            if self.abort.is_some() || INTERRUPTED.load(Ordering::SeqCst) {
                break;
            }
        }
        if self.abort.is_none() && !INTERRUPTED.load(Ordering::SeqCst) {
            match self.arbitrate(&running) {
                Ok(mib) => {
                    self.arena_mib = Some(mib);
                    match self.launch_layer(index, Layer::CpuRam, remaining(), true) {
                        Ok(started) => running.push(started),
                        Err(error) => {
                            record
                                .layers
                                .push(self.not_started(index, Layer::CpuRam, error));
                        }
                    }
                }
                Err(error) => {
                    record.layers.push(self.not_started(
                        index,
                        Layer::CpuRam,
                        format!("arbiter refused CPU-RAM (fail closed): {error}"),
                    ));
                }
            }
        }
        self.supervise(running, remaining(), record);
    }

    fn not_started(&self, index: usize, layer: Layer, detail: String) -> LayerRecord {
        LayerRecord {
            layer,
            outcome: LayerOutcome::Invalid,
            failures: 0,
            calculations: 0,
            peak_host_bytes: 0,
            directory: self.phase_dir(index, layer).display().to_string(),
            detail,
        }
    }

    fn arbitrate(&self, running: &[Running]) -> Result<u64, combined::ArbiterError> {
        let gpus = u64::try_from(self.gpu_count.max(1)).unwrap_or(1);
        let holdings: Vec<Holding> = running
            .iter()
            .map(|layer| {
                let measured = self.peaks.get(&layer.layer).copied();
                let declared = match layer.layer {
                    Layer::Gpu => measured
                        .map_or(0, combined::peak_from_baseline)
                        .max(self.gpu_budget * gpus),
                    Layer::Ssd => measured.map_or(
                        combined::SSD_DEFAULT_PEAK_BYTES,
                        combined::peak_from_baseline,
                    ),
                    Layer::Chain => layer
                        .live
                        .declared_peak_bytes
                        .unwrap_or(512 * MIB)
                        .max(layer.resident_bytes),
                    Layer::CpuRam => layer.resident_bytes,
                };
                Holding {
                    layer: layer.layer,
                    resident_bytes: layer.resident_bytes,
                    declared_peak_bytes: declared,
                }
            })
            .collect();
        combined::cpu_ram_remainder_mib(
            mem_available().unwrap_or(0),
            &holdings,
            self.settings.memory,
        )
    }

    /// Ticks until the newest layer reports ready (or the wait times out, noted in the phase).
    fn wait_ready(&mut self, running: &mut Vec<Running>, record: &mut PhaseRecord) {
        let since = Instant::now();
        while since.elapsed() < READY_WAIT {
            self.tick(running, record);
            let Some(newest) = running.last() else {
                return;
            };
            if newest.live.ready || self.abort.is_some() || INTERRUPTED.load(Ordering::SeqCst) {
                return;
            }
            if running.iter().all(|layer| layer.layer != newest.layer) {
                return;
            }
        }
        if let Some(newest) = running.last() {
            record.note = Some(format!(
                "{} not ready after {} s; started the next layer anyway",
                newest.layer.label(),
                READY_WAIT.as_secs()
            ));
        }
    }

    /// Runs until every layer has ended (drained after the grace period, or at a stop or
    /// a crash elsewhere); a phase without layers just waits `seconds`.
    fn supervise(&mut self, mut running: Vec<Running>, seconds: u64, record: &mut PhaseRecord) {
        let started = Instant::now();
        let planned = Duration::from_secs(seconds);
        loop {
            self.tick(&mut running, record);
            let stop = self.abort.is_some() || INTERRUPTED.load(Ordering::SeqCst);
            if running.is_empty() && (stop || started.elapsed() >= planned) {
                return;
            }
            let overran = seconds > 0 && started.elapsed() > planned + OVERRUN_GRACE;
            if stop || overran {
                for layer in running.iter_mut().filter(|layer| !layer.drained) {
                    drain(layer);
                    if overran && !stop {
                        record.note = Some(format!(
                            "{} overran its phase by {} s and was drained",
                            layer.layer.label(),
                            OVERRUN_GRACE.as_secs()
                        ));
                    }
                }
            }
        }
    }

    /// One pass: telemetry, progress of every layer, ended layers, memory timeline, screen.
    fn tick(&mut self, running: &mut Vec<Running>, record: &mut PhaseRecord) {
        std::thread::sleep(Duration::from_millis(250));
        self.telemetry.sample();
        let mut held = 0;
        for layer in running.iter_mut() {
            if let Some(tally) = layer.tally.as_mut() {
                tally.update(&layer.dir.join("events.jsonl"));
            }
            layer.live = layers::live(layer.layer, &layer.dir, layer.tally.as_ref());
            if let Some(read) = layer.live.read_gibps {
                self.chain_peak_gibps = self.chain_peak_gibps.max(read);
            }
            let (rss, hwm) = layers::tree_memory(layer.child.id());
            layer.resident_bytes = rss;
            layer.peak_bytes = layer.peak_bytes.max(hwm);
            held += rss;
        }
        let mut index = 0;
        while index < running.len() {
            if matches!(running[index].child.try_wait(), Ok(Some(_))) {
                let ended = running.remove(index);
                self.finish(ended, record);
            } else {
                index += 1;
            }
        }
        if self
            .last_sample
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
        {
            self.last_sample = Some(Instant::now());
            self.timeline.push(MemorySample {
                at_seconds: self.started.elapsed().as_secs(),
                mem_available_mib: self.telemetry.mem_available_bytes / MIB,
                held_by_tests_mib: held / MIB,
            });
        }
        self.draw(running, held);
    }

    fn finish(&mut self, mut ended: Running, record: &mut PhaseRecord) {
        if let Some(tally) = ended.tally.as_mut() {
            tally.update(&ended.dir.join("events.jsonl"));
        }
        let live = layers::live(ended.layer, &ended.dir, ended.tally.as_ref());
        let (mut outcome, mut detail) = layers::outcome(ended.layer, &ended.dir);
        // A layer the session stopped (drain, Ctrl+C) did not crash, even without a report.
        let stopped = ended.drained || INTERRUPTED.load(Ordering::SeqCst);
        if outcome == LayerOutcome::Crashed && stopped {
            outcome = LayerOutcome::Invalid;
            detail = format!("stopped by the session before its report: {detail}");
        }
        let failures = live.failures.max(ended.live.failures);
        let calculations = live.calculations.max(ended.live.calculations);
        let totals = self.totals.entry(ended.layer).or_default();
        totals.failures += failures;
        totals.calculations += calculations;
        if matches!(record.phase, PhaseKind::Baseline(_)) {
            self.peaks.insert(ended.layer, ended.peak_bytes);
        }
        if outcome == LayerOutcome::Crashed && self.abort.is_none() {
            self.abort = Some(format!(
                "{} ended without a report in phase {}: {detail}",
                ended.layer.label(),
                record.label
            ));
        }
        record.layers.push(LayerRecord {
            layer: ended.layer,
            outcome,
            failures,
            calculations,
            peak_host_bytes: ended.peak_bytes,
            directory: ended.dir.display().to_string(),
            detail,
        });
    }

    fn phases_row(&self) -> String {
        let marks: Vec<String> = self
            .phases
            .iter()
            .enumerate()
            .map(|(index, phase)| {
                let mark = match index.cmp(&self.current) {
                    std::cmp::Ordering::Less
                        if self.records.get(index).is_some_and(|r| r.completed) =>
                    {
                        "OK"
                    }
                    std::cmp::Ordering::Less => "!!",
                    std::cmp::Ordering::Equal => ">>",
                    std::cmp::Ordering::Greater => "--",
                };
                format!("[{mark}] {}", phase.kind.label())
            })
            .collect();
        format!("PHASES  {}", marks.join("  "))
    }

    fn plan_rows(&self) -> Vec<String> {
        let t = &self.threads;
        let mut rows = vec![format!(
            "THREADS: RAM {} / ORACLE {} / GPU {} / SSD {} / CHAIN {} / OS {}",
            t.cpu_ram_workers,
            t.cpu_oracle_workers,
            t.gpu_verifier_max
                .map_or_else(|| "auto".to_owned(), |v| v.to_string()),
            t.ssd_workers
                .map_or_else(|| "auto".to_owned(), |v| v.to_string()),
            t.chain_threads,
            t.os_threads
        )];
        rows.push(format!(
            "MEMORY:  CPU-RAM {} / GPU host {} x{} / reserve {}",
            self.arena_mib
                .map_or_else(|| "at overlap".to_owned(), |mib| format_bytes(mib * MIB)),
            format_bytes(self.gpu_budget),
            self.gpu_count,
            format_bytes(self.settings.memory.reserve_bytes)
        ));
        if let Some(storage) = &self.storage {
            rows.push(format!(
                "STORAGE: {} / write {} MiB / chain {}",
                storage.write_level,
                storage.write_budget_mib,
                if self.available.chain {
                    format!("{} MiB", storage.chain_working_set_mib)
                } else {
                    storage
                        .chain_note
                        .clone()
                        .unwrap_or_else(|| "off".to_owned())
                }
            ));
        }
        rows
    }

    fn draw(&mut self, running: &[Running], held: u64) {
        let now = Instant::now();
        let interactive = self.renderer.is_some();
        let due = if interactive {
            self.last_frame
                .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(500))
        } else {
            self.last_line
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(10))
        };
        if !due {
            return;
        }
        let view = self.view(running, held);
        if let Some(renderer) = self.renderer.as_mut() {
            let frame = render(&view, &self.telemetry, TerminalSize::current(), true);
            let _ = renderer.render_frame(&frame);
            self.last_frame = Some(now);
        } else {
            let layers: Vec<String> = view
                .layers
                .iter()
                .map(|l| format!("{} {} fail {}", l.label, l.state, l.failures))
                .collect();
            println!(
                "[{}] {} | {} | {}",
                format_duration(view.elapsed_seconds),
                view.phase_title,
                layers.join(" | "),
                view.phases_row
            );
            self.last_line = Some(now);
        }
    }

    fn layer_lines(&self, running: &[Running]) -> Vec<LayerLine> {
        [Layer::CpuRam, Layer::Gpu, Layer::Ssd, Layer::Chain]
            .into_iter()
            .filter(|layer| {
                *layer == Layer::CpuRam
                    || match layer {
                        Layer::Gpu => self.available.gpu,
                        Layer::Ssd => self.available.ssd,
                        _ => self.available.chain,
                    }
            })
            .map(|layer| {
                let total = self.totals.get(&layer).copied().unwrap_or_default();
                let active = running.iter().find(|r| r.layer == layer);
                LayerLine {
                    label: layer.label(),
                    state: active.map_or_else(
                        || "IDLE".to_owned(),
                        |r| {
                            if r.live.state.is_empty() {
                                "RUNNING".to_owned()
                            } else {
                                r.live.state.clone()
                            }
                        },
                    ),
                    detail: active.map_or_else(String::new, |r| {
                        r.live.hardware_note.as_ref().map_or_else(
                            || r.live.detail.clone(),
                            |note| format!("{} | HW {note}", r.live.detail),
                        )
                    }),
                    failures: total.failures + active.map_or(0, |r| r.live.failures),
                }
            })
            .collect()
    }

    fn view(&self, running: &[Running], held: u64) -> SessionView {
        let phase = self.phases.get(self.current).copied();
        let layers = self.layer_lines(running);
        let calculations = [Layer::CpuRam, Layer::Gpu, Layer::Ssd, Layer::Chain]
            .iter()
            .map(|layer| {
                self.totals.get(layer).map_or(0, |t| t.calculations)
                    + running
                        .iter()
                        .find(|r| r.layer == *layer)
                        .map_or(0, |r| r.live.calculations)
            })
            .sum();
        let chain = running
            .iter()
            .find(|r| r.layer == Layer::Chain && r.live.read_gibps.is_some())
            .map(|r| ChainMeter {
                read_gibps: r.live.read_gibps.unwrap_or(0.0),
                peak_gibps: self.chain_peak_gibps,
                generations: r.live.generations.unwrap_or(0),
                loads: r.live.loads.unwrap_or(0),
            });
        let stopping = INTERRUPTED.load(Ordering::SeqCst) || self.abort.is_some();
        SessionView {
            level: self.settings.level,
            intensity: self.settings.intensity,
            gpu_temp_limit: self.settings.gpu_temp_limit,
            state: if stopping { "STOPPING" } else { "RUNNING" }.to_owned(),
            phase_title: phase.map_or_else(String::new, |phase| {
                format!(
                    "PHASE {}/{} {} ({} / {})",
                    self.current + 1,
                    self.phases.len(),
                    phase.kind.label(),
                    format_duration(self.phase_started.elapsed().as_secs()),
                    if phase.seconds == 0 {
                        "until done".to_owned()
                    } else {
                        format_duration(phase.seconds)
                    }
                )
            }),
            phase_process: phase.map_or_else(String::new, |phase| {
                let names: Vec<&str> = phase
                    .kind
                    .layers(&self.available)
                    .iter()
                    .map(|layer| layer.label())
                    .collect();
                if names.is_empty() {
                    "all layers idle: temperatures and power states settle".to_owned()
                } else {
                    format!(
                        "{} | {}",
                        names.join(" + "),
                        self.settings.intensity.label()
                    )
                }
            }),
            elapsed_seconds: self.started.elapsed().as_secs(),
            planned_seconds: self.planned_seconds,
            phases_row: self.phases_row(),
            plan_rows: self.plan_rows(),
            layers,
            calculations,
            chain,
            held_bytes: held,
        }
    }

    fn remove_chain_file(&self) -> String {
        let Some(file) = &self.chain_file else {
            return String::new();
        };
        let manifest = file.with_extension("json");
        let removed = std::fs::remove_file(file).is_ok() && std::fs::remove_file(&manifest).is_ok();
        if removed {
            "Chain record file removed.\n".to_owned()
        } else {
            format!(
                "Chain record file could not be removed: {}\n",
                file.display()
            )
        }
    }

    fn write_report(&self) -> Result<(&'static str, PathBuf), Box<dyn std::error::Error>> {
        let outcomes: Vec<LayerOutcome> = self
            .records
            .iter()
            .flat_map(|phase| phase.layers.iter().map(|layer| layer.outcome))
            .filter(|outcome| *outcome != LayerOutcome::NotRun)
            .collect();
        let stopped = INTERRUPTED.load(Ordering::SeqCst);
        let completed = !stopped
            && self.abort.is_none()
            && self.records.len() == self.phases.len()
            && self.records.iter().all(|record| record.completed);
        let verdict = combined::session_verdict(&outcomes, completed);
        let report = CombinedReport {
            schema_version: 1,
            module_id: combined::COMBINED_MODULE_ID,
            run_id: &self.run_id,
            verdict,
            grade: combined::grade(
                self.settings.level,
                self.records.iter().filter(|r| r.completed).count(),
                self.phases.len(),
            ),
            completed,
            stopped_by_operator: stopped,
            abort_reason: self.abort.clone(),
            settings: self.settings,
            available: self.available,
            threads: self.threads,
            gpu_host_budget_bytes: self.gpu_budget,
            cpu_ram_arena_mib: self.arena_mib,
            storage: self.storage.as_ref(),
            phases: &self.records,
            layer_failures: self
                .totals
                .iter()
                .map(|(layer, totals)| (layer.label(), totals.failures))
                .collect(),
            failures_total: self.totals.values().map(|t| t.failures).sum(),
            calculations_total: self.totals.values().map(|t| t.calculations).sum(),
            memory_timeline: &self.timeline,
            elapsed_seconds: self.started.elapsed().as_secs_f64(),
        };
        let path = self.dir.join("combined-report.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
        Ok((verdict, path))
    }

    /// Copies the session directory once to the results volume (`runs/<id>/combined`).
    fn persist(&self) -> String {
        let root = &self.menu.results_root;
        if !root.is_dir() {
            return format!(
                "Verified result volume unavailable; artifacts remain in {}",
                self.dir.display()
            );
        }
        let destination = root.join("runs").join(&self.run_id).join("combined");
        match copy_tree(&self.dir, &destination) {
            Ok(files) => format!(
                "Final report bundle ({files} files) copied once to: {}",
                destination.display()
            ),
            Err(error) => format!(
                "Final report copy failed ({error}); artifacts remain in {}",
                self.dir.display()
            ),
        }
    }
}

/// Stops a layer the way the operator's Ctrl+C would: SIGINT to its watchdog, which stops the
/// worker's process group and records the stop.
fn drain(layer: &mut Running) {
    let pid = nix::unistd::Pid::from_raw(i32::try_from(layer.child.id()).unwrap_or(0));
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT);
    layer.drained = true;
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<usize> {
    create_dir_all(destination)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copied += copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            copy_new_file(&entry.path(), &target)?;
            copied += 1;
        }
    }
    Ok(copied)
}
