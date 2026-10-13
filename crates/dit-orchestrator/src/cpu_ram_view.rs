//! Running screen of a CPU-RAM test (Safe, Full, Compute) on the shared `UnifiedDashboard`, the
//! same shell every other test uses. `cpu-dit-ui.sh` starts the runner and then this view, which
//! follows the runner's log (`CPU_DIT_PROGRESS` / `CPU_DIT_COUNTER` lines) until the runner exits.
//! It only reads: the runner, its watchdog and the result screen stay with the UI script.
//! `--mode power` draws the CPU availability / power-platform test (`CPU_DIT_POWER_PROGRESS`).
use super::combined_view::{format_bytes, format_duration};
use dit_system::LiveSensorSampler;
use dit_ui_core::{
    DashboardMeter, HostSampler, InPlaceRenderer, LiveUtilization, SystemInfo, TerminalSize,
    ThrottleLive, UnifiedDashboard, render_unified_dashboard,
};
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ViewMode {
    /// CPU-RAM Safe / Full / Compute (`CPU_DIT_PROGRESS`).
    CpuRam,
    /// CPU availability / power platform (`CPU_DIT_POWER_PROGRESS`).
    Power,
}

#[derive(Debug, clap::Args)]
pub struct CpuRamViewArgs {
    #[arg(long, value_enum, default_value_t = ViewMode::CpuRam)]
    mode: ViewMode,
    /// Runner log the UI script captures (stdout and stderr of the runner).
    #[arg(long)]
    log: PathBuf,
    /// Runner process; the view ends when it is gone.
    #[arg(long)]
    pid: u32,
    /// Profile title, e.g. `SAFE TEST`.
    #[arg(long)]
    title: String,
    #[arg(long)]
    duration_seconds: u64,
    /// Start of the run, seconds since the epoch.
    #[arg(long)]
    started_unix: u64,
    /// The CPU temperature limit is enforced at this many degrees (absent: telemetry only).
    #[arg(long)]
    temp_limit_celsius: Option<u32>,
    #[arg(long, default_value_t = 95)]
    full_safe_percent: u32,
    #[arg(long, default_value = dit_ui_core::DIT_VERSION)]
    engine_version: String,
    #[arg(long, default_value = "9")]
    report_abi: String,
    /// The UI session the run belongs to (`docs/FAULT_CATCH_ROADMAP.md`, `FC3a`): while it draws,
    /// the view beats the session for the UI script that ends it.
    #[arg(long)]
    session_run_id: Option<String>,
    /// The run's watchdog directory (its `watchdog-status.json`).
    #[arg(long)]
    watchdog_dir: Option<PathBuf>,
    #[arg(long, default_value = "/mnt/cpu-dit-results")]
    results_root: PathBuf,
    /// Booted with the Fault Attribution entry (the CSV test column says it).
    #[arg(long)]
    attribution: bool,
}

/// What the runner's log says so far. The failure count never goes back: the diagnostic rerun
/// after a failure prints its own counters from zero.
#[derive(Debug, Default)]
struct Progress {
    fields: BTreeMap<String, String>,
    calculations: u64,
    failures_peak: u64,
    offset: u64,
    partial: String,
}

impl Progress {
    fn absorb_line(&mut self, line: &str) {
        let progress = line.contains("CPU_DIT_PROGRESS phase=")
            || line.contains("CPU_DIT_POWER_PROGRESS phase=");
        let counter = line.contains("CPU_DIT_COUNTER calculations_completed=");
        if !progress && !counter {
            return;
        }
        for token in line.split_whitespace() {
            if let Some((key, value)) = token.split_once('=') {
                if progress {
                    self.fields.insert(key.to_owned(), value.to_owned());
                }
                match key {
                    "failures_total" | "failures" => {
                        if let Ok(count) = value.parse::<u64>() {
                            self.failures_peak = self.failures_peak.max(count);
                        }
                    }
                    "calculations_completed" => {
                        if let Ok(count) = value.parse::<u64>() {
                            self.calculations = count;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Reads what the log gained since the last call.
    fn follow(&mut self, path: &Path) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        let length = file.metadata().map_or(0, |m| m.len());
        if length < self.offset {
            // Truncated (a new run reused the file): start over.
            *self = Self::default();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return;
        }
        self.offset += bytes.len() as u64;
        let text = std::mem::take(&mut self.partial) + &String::from_utf8_lossy(&bytes);
        let mut lines: Vec<&str> = text.split('\n').collect();
        let tail = lines.pop().unwrap_or_default().to_owned();
        for line in lines {
            self.absorb_line(line);
        }
        self.partial = tail;
    }

    fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    fn number(&self, key: &str) -> Option<u64> {
        self.field(key).and_then(|value| value.parse().ok())
    }
}

fn phase_description(phase: &str) -> &'static str {
    match phase {
        "PREFLIGHT" => "source verify + inventory + sensor/driver preflight",
        "ALLOCATED" => "high-water RAM arenas allocated and prefaulted",
        "RAM_WRITE" => "CPU pattern generation -> scattered RAM writes",
        "RAM_VERIFY" => "cross-core RAM readback -> exact comparison",
        "ROUND_COMPLETE" => "exact round committed; next complete round starts",
        "FINALIZING" => "telemetry, safety gates and report finalization",
        _ => "deterministic CPU/RAM data integrity pipeline",
    }
}

fn phase_state(phase: &str) -> &'static str {
    match phase {
        "PREFLIGHT" | "ALLOCATED" => "RAMPING",
        "FINALIZING" => "FINALIZING",
        _ => "RUNNING",
    }
}

/// `calculations_completed` counts compared bytes: the raw count and its size.
#[allow(clippy::cast_precision_loss)]
fn format_calculations(value: u64) -> String {
    let (unit, label) = match value {
        v if v >= 1 << 30 => (1_u64 << 30, "GiB"),
        v if v >= 1 << 20 => (1 << 20, "MiB"),
        v if v >= 1 << 10 => (1 << 10, "KiB"),
        _ => return value.to_string(),
    };
    format!("{value} ({:.2} {label})", value as f64 / unit as f64)
}

/// Memory figures of `/proc/meminfo`, bytes: total, available, cache (buffers, page cache and
/// reclaimable slab, as the UI showed before).
fn meminfo() -> (u64, u64, u64) {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let value = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    (
        value("MemTotal:"),
        value("MemAvailable:"),
        value("Buffers:") + value("Cached:") + value("SReclaimable:"),
    )
}

fn numa_nodes() -> usize {
    let Ok(entries) = std::fs::read_dir("/sys/devices/system/node") else {
        return 1;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix("node")
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        })
        .count()
        .max(1)
}

fn optional(value: Option<f64>, unit: &str, digits: usize) -> String {
    value.map_or_else(
        || "N/A".to_owned(),
        |value| format!("{value:.digits$} {unit}"),
    )
}

#[allow(clippy::cast_precision_loss)]
fn percent(value: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| (100.0 * value as f64 / total as f64).clamp(0.0, 100.0))
}

/// Everything one frame needs besides the log.
struct Frame<'a> {
    args: &'a CpuRamViewArgs,
    system: &'a SystemInfo,
    numa: usize,
    live: &'a LiveUtilization,
    memory: (u64, u64, u64),
    now_unix: u64,
    /// The CPU's thermal throttling since the view started, and its footer alert.
    throttle: &'a ThrottleLive,
}

fn dashboard(frame: &Frame<'_>, progress: &Progress) -> UnifiedDashboard {
    let args = frame.args;
    let (total, available, cache) = frame.memory;
    let live = frame.live;
    let phase = progress.field("phase").unwrap_or("PREFLIGHT");
    let elapsed = progress.number("elapsed_ms").map_or_else(
        || frame.now_unix.saturating_sub(args.started_unix),
        |ms| ms / 1000,
    );
    #[allow(clippy::cast_precision_loss)]
    let progress_percent = (args.duration_seconds > 0)
        .then(|| (100.0 * elapsed as f64 / args.duration_seconds as f64).min(99.0));
    let target = progress.number("target_bytes");
    let target_text = target.map_or_else(
        || "planning".to_owned(),
        |bytes| format!("{} MiB", bytes >> 20),
    );
    let text = |key: &str, default: &'static str| progress.field(key).unwrap_or(default).to_owned();
    let used = total.saturating_sub(available);
    UnifiedDashboard {
        engine: format!("CPU-RAM {}", args.engine_version),
        report_abi: args.report_abi.clone(),
        profile: format!(
            "{} / CPU + RAM",
            args.title.strip_suffix(" TEST").unwrap_or(&args.title)
        ),
        hardware_rows: vec![
            format!("CPU:     {}", frame.system.cpu),
            format!("THREADS: {} logical CPU(s)", frame.system.logical_threads),
            format!("RAM:     {} total system memory", format_bytes(total)),
            format!("NUMA:    {} node(s)", frame.numa),
            format!("GPU:     {}", frame.system.gpu),
            format!("BOARD:   {}", frame.system.board),
            format!("KERNEL:  {}", frame.system.kernel),
        ],
        live_meters: vec![
            DashboardMeter::new("CPU USAGE", live.cpu_percent).detail(format!(
                "Freq {} / Temp {} / Power {}",
                optional(live.cpu_frequency_ghz, "GHz", 2),
                optional(live.cpu_temperature_celsius, "C", 0),
                optional(live.cpu_power_watts, "W", 0)
            )),
            DashboardMeter::new("RAM USAGE", percent(used, total)).detail(format!(
                "Used {} / {} / Available {} / Cache {}",
                format_bytes(used),
                format_bytes(total),
                format_bytes(available),
                format_bytes(cache)
            )),
            DashboardMeter::new("DIT TARGET", target.and_then(|bytes| percent(bytes, total)))
                .detail(format!(
                    "Exact test buffer {target_text} / Full policy {}% safe allocatable",
                    args.full_safe_percent
                )),
        ],
        state: phase_state(phase).to_owned(),
        progress_percent,
        elapsed: format_duration(elapsed),
        target: format_duration(args.duration_seconds),
        active_test: format!(
            "[{}/{}+] CPU-RAM-EXACT-001  {}",
            progress.number("round").unwrap_or(0),
            progress.number("minimum_rounds").unwrap_or(0),
            phase.replace('_', " ")
        ),
        process: phase_description(phase).to_owned(),
        calculations_completed: progress.calculations,
        failures: progress.failures_peak,
        running_rows: vec![
            format!(
                "CALCULATIONS  {}",
                format_calculations(progress.calculations)
            ),
            format!(
                "SESSION SEED  {}    SOURCE  {}",
                text("session_seed", "planning"),
                text("seed_source", "pending")
            ),
            format!("ROUND SEED    {}", text("round_seed", "N/A")),
            format!("WORKERS       auto topology    TEST BUFFER  {target_text}"),
            format!(
                "RECIPE        {} | {}",
                text("window_recipe", "planning"),
                text("pattern_recipe", "planning")
            ),
            format!(
                "WINDOW        {}    PATTERN  {}    exact full-target write + readback",
                text("active_window", "planning"),
                text("active_pattern", "planning")
            ),
        ],
        footer: args.temp_limit_celsius.map_or_else(
            || "TEMP LIMIT OFF - TELEMETRY ONLY    |    Ctrl+C: stop + preserve report".to_owned(),
            |limit| {
                format!(
                    "TEMP LIMIT ON ({limit} C, TRUSTED CPU SENSORS)    |    Ctrl+C: stop + preserve report"
                )
            },
        ),
    }
}

/// The CPU availability / power-platform test: transitions or, in the campaign, minimum time and
/// coverage rounds, with the availability and power evidence kept apart from FAIL.
#[allow(clippy::too_many_lines)]
fn power_dashboard(frame: &Frame<'_>, progress: &Progress) -> UnifiedDashboard {
    let args = frame.args;
    let (total, available, cache) = frame.memory;
    let live = frame.live;
    let text = |key: &str, default: &str| progress.field(key).unwrap_or(default).to_owned();
    let campaign = progress.field("mode") == Some("campaign");
    let phase = text("phase", "PREFLIGHT");
    let rounds = progress.number("rounds").unwrap_or(0);
    let (mut completed, target, workers) = if campaign {
        (
            progress.number("round_completed"),
            progress.number("round_target"),
            text("active_workers", "planning"),
        )
    } else {
        (
            progress.number("completed"),
            progress.number("target"),
            text("workers", "planning"),
        )
    };
    if campaign
        && matches!(phase.as_str(), "FINAL" | "TRANSITION_DONE")
        && completed == Some(0)
        && rounds > 0
    {
        completed = target;
    }
    let minimum_ms = progress.number("minimum_ms").filter(|ms| *ms > 0);
    let elapsed = match (campaign, progress.number("elapsed_ms")) {
        (true, Some(ms)) => ms / 1000,
        _ => frame.now_unix.saturating_sub(args.started_unix),
    };
    #[allow(clippy::cast_precision_loss)]
    let progress_percent = match (campaign, minimum_ms, completed, target) {
        (true, Some(minimum), ..) => {
            Some((100.0 * (elapsed * 1000) as f64 / minimum as f64).min(100.0))
        }
        (false, _, Some(done), Some(goal)) if goal > 0 => {
            Some((100.0 * done as f64 / goal as f64).min(100.0))
        }
        _ => Some(0.0),
    };
    let availability = text("availability", "Starting");
    let detail = if !campaign {
        "Progress counts completed transitions; completion does not prove power stability."
            .to_owned()
    } else if progress.field("topology_complete") == Some("false") {
        "INCOMPLETE TOPOLOGY: core/SMT coverage unavailable; this run cannot PASS.".to_owned()
    } else if progress_percent.is_some_and(|p| p >= 100.0) && availability == "Running" {
        "Finishing current coverage round; minimum time reached, test is not complete.".to_owned()
    } else {
        format!("Completed rounds {rounds}; full coverage round required. Not power validation.")
    };
    let load = if campaign {
        format!(
            "Idle {} ms / burst {} ms / {}",
            text("idle_ms", "?"),
            text("burst_ms", "?"),
            text("timing", "planning")
        )
    } else {
        "Idle 500 ms / burst 100 ms".to_owned()
    };
    let shown = |value: Option<u64>| value.map_or_else(|| "planning".to_owned(), |v| v.to_string());
    let used = total.saturating_sub(available);
    let mut running_rows = vec![
        format!(
            "{}  {} / {}    WORKERS {workers}    BURSTS {}",
            if campaign {
                "ROUND COVERAGE"
            } else {
                "TRANSITIONS   "
            },
            completed.unwrap_or(0),
            shown(target),
            progress.number("bursts").unwrap_or(0)
        ),
        format!(
            "AVAILABILITY  {availability}    POWER {}    VCORE unavailable",
            text("power_platform", "NoEvidence")
        ),
        format!("SEED          {}", text("seed", "planning")),
    ];
    if campaign {
        running_rows.push(format!(
            "KERNEL        {}    ROUNDS {rounds}    GROUP {}    ACTIVE CPUs {}",
            text("kernel", "planning"),
            text("group", "-"),
            text("active_cpus", "-")
        ));
    }
    running_rows.push(detail);
    running_rows
        .push("Scratch RAM 64 KiB/worker; full RAM coverage is a separate test.".to_owned());
    UnifiedDashboard {
        engine: format!("CPU-POWER {}", args.engine_version),
        report_abi: args.report_abi.clone(),
        profile: format!("{} / CPU AVAILABILITY + POWER PLATFORM", args.title),
        hardware_rows: vec![
            format!("CPU:     {}", frame.system.cpu),
            format!("THREADS: {} logical CPU(s)", frame.system.logical_threads),
            format!("RAM:     {} total system memory", format_bytes(total)),
            format!("NUMA:    {} node(s)", frame.numa),
            format!("GPU:     {}", frame.system.gpu),
            format!("BOARD:   {}", frame.system.board),
            format!("KERNEL:  {}", frame.system.kernel),
        ],
        live_meters: vec![
            DashboardMeter::new("CPU USAGE", live.cpu_percent).detail(format!(
                "Freq {} / Temp {} / Power {}",
                optional(live.cpu_frequency_ghz, "GHz", 2),
                optional(live.cpu_temperature_celsius, "C", 0),
                optional(live.cpu_power_watts, "W", 0)
            )),
            DashboardMeter::new("RAM USAGE", percent(used, total)).detail(format!(
                "Used {} / {} / Available {} / Cache {}",
                format_bytes(used),
                format_bytes(total),
                format_bytes(available),
                format_bytes(cache)
            )),
        ],
        state: availability.to_ascii_uppercase(),
        progress_percent,
        elapsed: format_duration(elapsed),
        target: if campaign {
            minimum_ms.map_or_else(|| "planning".to_owned(), |ms| format_duration(ms / 1000))
        } else {
            format!("{} transitions", shown(target))
        },
        active_test: if campaign {
            format!(
                "{}  PHASE {} / {phase}",
                args.title,
                text("campaign_phase", "planning")
            )
        } else {
            format!("{}  PHASE {phase}", args.title)
        },
        process: load,
        calculations_completed: progress.number("bursts").unwrap_or(0),
        failures: progress.failures_peak,
        running_rows,
        footer: args.temp_limit_celsius.map_or_else(
            || "TEMP LIMIT OFF - TELEMETRY ONLY    |    Ctrl+C: stop + preserve report".to_owned(),
            |limit| {
                format!(
                    "TEMP LIMIT ON ({limit} C, TRUSTED CPU SENSORS)    |    Ctrl+C: stop + preserve report"
                )
            },
        ),
    }
}

/// The screen of the view's mode.
fn model(frame: &Frame<'_>, progress: &Progress) -> UnifiedDashboard {
    let mut model = match frame.args.mode {
        ViewMode::CpuRam => dashboard(frame, progress),
        ViewMode::Power => power_dashboard(frame, progress),
    };
    if let Some(row) = frame.throttle.row() {
        model.running_rows.push(row);
    }
    if let Some(alert) = frame.throttle.alert() {
        alert.clone_into(&mut model.footer);
    }
    model
}

/// Whether `pid` still runs (a zombie waiting for its parent counts as gone).
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next())
            .is_some_and(|state| state != 'Z' && state != 'X')
    })
}

/// `dit-suite cpu-ram-view`: draws until the runner exits (or Ctrl+C ends the view; the UI
/// script then stops the runner and starts the view again to show the stop).
///
/// # Errors
///
/// Fails when the terminal cannot be written.
pub fn command(args: &CpuRamViewArgs) -> io::Result<()> {
    let mut host = HostSampler::detect();
    let mut system = host.system_info();
    system.gpu = dit_system::format_gpu_summary(&dit_system::collect_gpu_inventory());
    let numa = numa_nodes();
    let sensors = LiveSensorSampler::default();
    let mut renderer = InPlaceRenderer::new(io::stdout(), 2);
    let mut progress = Progress::default();
    let mut live = LiveUtilization::default();
    let mut last_sensor: Option<Instant> = None;
    let (mut temperature, mut power) = (None, None);
    let mut throttle = ThrottleLive::default();
    let mut throttle_start = None;
    let module = super::fault_single::SingleModule::CpuRam;
    let mut session = args
        .session_run_id
        .as_deref()
        .filter(|_| args.mode == ViewMode::CpuRam)
        .map(|run_id| {
            let run_dir = args.watchdog_dir.clone().unwrap_or_default();
            let frame = std::env::var_os(dit_ui_core::FINAL_FRAME_ENV)
                .map_or_else(|| run_dir.join("dashboard-final.json"), PathBuf::from);
            super::single_session::SingleSession::open(
                run_id,
                module,
                args.attribution, // The UI script writes the result: it is the session's app.
                std::os::unix::process::parent_id(),
                &args.results_root,
                &run_dir,
                &frame,
            )
        });
    loop {
        let alive = running(args.pid);
        progress.follow(&args.log);
        if let Some(session) = session.as_mut() {
            session.beat(
                args.pid,
                progress.fields.get("phase").cloned(),
                Some(progress.failures_peak),
            );
        }
        if last_sensor.is_none_or(|last| last.elapsed() >= Duration::from_secs(1)) {
            let snapshot = sensors.sample();
            #[allow(clippy::cast_precision_loss)]
            {
                temperature = snapshot
                    .max_temperature_millicelsius()
                    .map(|value| value as f64 / 1000.0);
                power = snapshot
                    .preferred_cpu_power()
                    .map(|reading| reading.milliwatts as f64 / 1000.0);
            }
            let start = throttle_start.get_or_insert_with(|| snapshot.throttle_groups.clone());
            let since_start = dit_system::throttle_delta(start, &snapshot.throttle_groups)
                .map(|delta| (delta.events, delta.longest_ms.map(|ms| ms / 1000)));
            let clock = format_duration(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs())
                    .saturating_sub(args.started_unix),
            );
            throttle.update("CPU", since_start, &clock);
            last_sensor = Some(Instant::now());
        }
        live = {
            let mut sample = host.sample();
            sample.cpu_temperature_celsius = temperature;
            sample.cpu_power_watts = power;
            if sample.cpu_percent.is_none() {
                sample.cpu_percent = live.cpu_percent;
            }
            sample
        };
        let frame = Frame {
            args,
            system: &system,
            numa,
            live: &live,
            memory: meminfo(),
            now_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            throttle: &throttle,
        };
        let text =
            render_unified_dashboard(&model(&frame, &progress), TerminalSize::current(), true);
        renderer.render_frame(&text)?;
        if !alive {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    renderer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> CpuRamViewArgs {
        CpuRamViewArgs {
            mode: ViewMode::CpuRam,
            log: PathBuf::from("/nonexistent"),
            pid: 0,
            title: "FULL TEST".into(),
            duration_seconds: 1800,
            started_unix: 1000,
            temp_limit_celsius: Some(95),
            full_safe_percent: 95,
            engine_version: dit_ui_core::DIT_VERSION.into(),
            report_abi: "9".into(),
            session_run_id: None,
            watchdog_dir: None,
            results_root: PathBuf::from("/nonexistent"),
            attribution: false,
        }
    }

    #[test]
    fn the_log_gives_the_fields_and_a_failure_count_that_never_drops() {
        let mut progress = Progress::default();
        progress.absorb_line("noise line");
        progress.absorb_line(
            "CPU_DIT_PROGRESS phase=RAM_VERIFY round=3 minimum_rounds=8 elapsed_ms=125000 target_bytes=8589934592 calculations_completed=42 failures_total=1 session_seed=0xabc round_seed=0xdef",
        );
        progress.absorb_line("CPU_DIT_COUNTER calculations_completed=2147483648 failures_total=1");
        // The diagnostic rerun starts its counters again.
        progress.absorb_line(
            "CPU_DIT_PROGRESS phase=RAM_WRITE round=1 minimum_rounds=8 failures_total=0",
        );
        assert_eq!(progress.failures_peak, 1);
        assert_eq!(progress.field("phase"), Some("RAM_WRITE"));
        assert_eq!(progress.calculations, 2_147_483_648);
        assert_eq!(format_calculations(2_147_483_648), "2147483648 (2.00 GiB)");
    }

    #[test]
    fn the_power_campaign_screen_shows_minimum_time_coverage_and_evidence() {
        let mut progress = Progress::default();
        progress.absorb_line(
            "CPU_DIT_POWER_PROGRESS phase=BURST_ARMED mode=campaign completed=3 target=10 round_completed=5 round_target=24 rounds=1 minimum_ms=120000 elapsed_ms=60000 active_workers=4 bursts=25 failures=1 availability=Running power_platform=NoEvidence seed=0x1234 campaign_phase=SINGLE kernel=heavy-fp-v1 group=0 active_cpus=0-3 idle_ms=200 burst_ms=50 timing=seeded",
        );
        let mut args = args();
        args.mode = ViewMode::Power;
        args.title = "UNIFIED POWER CAMPAIGN".into();
        let system = SystemInfo::default();
        let live = LiveUtilization::default();
        let throttle = ThrottleLive::default();
        let frame = Frame {
            args: &args,
            system: &system,
            numa: 1,
            live: &live,
            memory: (32 << 30, 16 << 30, 4 << 30),
            now_unix: 2000,
            throttle: &throttle,
        };
        let text = render_unified_dashboard(
            &model(&frame, &progress),
            TerminalSize {
                cols: 160,
                rows: 48,
            },
            false,
        );
        for expected in [
            "PROFILE UNIFIED POWER CAMPAIGN / CPU AVAILABILITY + POWER PLATFORM",
            "STATE: RUNNING",
            "PROGRESS: 50%",
            "00:01:00 / 00:02:00",
            "PHASE SINGLE / BURST_ARMED",
            "ROUND COVERAGE  5 / 24",
            "FAIL TOTAL  1",
            "AVAILABILITY  Running    POWER NoEvidence    VCORE unavailable",
            "KERNEL        heavy-fp-v1",
            "Completed rounds 1; full coverage round required",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
    }

    /// The live THROTTLE row closes the running rows; a new throttle event takes the footer.
    #[test]
    fn the_cpu_throttle_shows_live_with_a_footer_alert() {
        let progress = Progress::default();
        let args = args();
        let system = SystemInfo::default();
        let live = LiveUtilization::default();
        let screen = |throttle: &ThrottleLive| {
            model(
                &Frame {
                    args: &args,
                    system: &system,
                    numa: 1,
                    live: &live,
                    memory: (32 << 30, 16 << 30, 4 << 30),
                    now_unix: 2000,
                    throttle,
                },
                &progress,
            )
        };
        let mut throttle = ThrottleLive::default();
        throttle.update("CPU", Some((0, Some(0))), "00:00:01");
        let quiet = screen(&throttle);
        assert_eq!(quiet.running_rows.last().unwrap(), "THROTTLE      CPU 0");
        assert!(quiet.footer.starts_with("TEMP LIMIT"));
        throttle.update("CPU", Some((1, Some(3))), "00:01:10");
        let alerted = screen(&throttle);
        assert_eq!(
            alerted.running_rows.last().unwrap(),
            "THROTTLE      CPU 1 (3 s)"
        );
        assert!(alerted.footer.starts_with("! THROTTLE CPU +1 at 00:01:10"));
    }

    #[test]
    fn the_running_screen_is_the_shared_dashboard_with_the_cpu_ram_rows() {
        let mut progress = Progress::default();
        progress.absorb_line(
            "CPU_DIT_PROGRESS phase=RAM_VERIFY round=3 minimum_rounds=8 elapsed_ms=125000 target_bytes=8589934592 failures_total=2 session_seed=0xabc seed_source=linux-random round_seed=0xdef window_recipe=W1 pattern_recipe=P1 active_window=MIXED_PARALLEL active_pattern=MULTI_FAMILY",
        );
        let args = args();
        let system = SystemInfo {
            cpu: "Test CPU".into(),
            logical_threads: 16,
            ..SystemInfo::default()
        };
        let live = LiveUtilization::default();
        let throttle = ThrottleLive::default();
        let frame = Frame {
            args: &args,
            system: &system,
            numa: 1,
            live: &live,
            memory: (32 << 30, 16 << 30, 4 << 30),
            now_unix: 2000,
            throttle: &throttle,
        };
        let text = render_unified_dashboard(
            &dashboard(&frame, &progress),
            TerminalSize {
                cols: 160,
                rows: 48,
            },
            false,
        );
        for expected in [
            "DIT-01 / DATA INTEGRITY TEST ENGINE",
            "PROFILE FULL / CPU + RAM",
            "SYSTEM HARDWARE",
            "LIVE UTILIZATION",
            "DIT TARGET",
            "TESTING / PROCESS INFORMATION",
            "[3/8+] CPU-RAM-EXACT-001  RAM VERIFY",
            "cross-core RAM readback -> exact comparison",
            "FAIL TOTAL  2",
            "SESSION SEED  0xabc    SOURCE  linux-random",
            "WINDOW        MIXED_PARALLEL",
            "TEMP LIMIT ON (95 C",
            "00:02:05 / 00:30:00",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in\n{text}");
        }
        assert!(!running(u32::MAX), "no such process");
        assert!(running(std::process::id()));
    }
}
