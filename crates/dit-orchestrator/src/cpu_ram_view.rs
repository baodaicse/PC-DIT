//! Running screen of a CPU-RAM test (Safe, Full, Compute) on the shared `UnifiedDashboard`, the
//! same shell every other test uses. `cpu-dit-ui.sh` starts the runner and then this view, which
//! follows the runner's log (`CPU_DIT_PROGRESS` / `CPU_DIT_COUNTER` lines) until the runner exits.
//! It only reads: the runner, its watchdog and the result screen stay with the UI script.
use super::combined_view::{format_bytes, format_duration};
use dit_system::LiveSensorSampler;
use dit_ui_core::{
    DashboardMeter, HostSampler, InPlaceRenderer, LiveUtilization, SystemInfo, TerminalSize,
    UnifiedDashboard, render_unified_dashboard,
};
use std::collections::BTreeMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, clap::Args)]
pub struct CpuRamViewArgs {
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
    #[arg(long, default_value = "0.1.0")]
    engine_version: String,
    #[arg(long, default_value = "9")]
    report_abi: String,
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
        let progress = line.contains("CPU_DIT_PROGRESS phase=");
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
                    "failures_total" => {
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
    loop {
        let alive = running(args.pid);
        progress.follow(&args.log);
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
        };
        let text =
            render_unified_dashboard(&dashboard(&frame, &progress), TerminalSize::current(), true);
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
            log: PathBuf::from("/nonexistent"),
            pid: 0,
            title: "FULL TEST".into(),
            duration_seconds: 1800,
            started_unix: 1000,
            temp_limit_celsius: Some(95),
            full_safe_percent: 95,
            engine_version: "0.1.0".into(),
            report_abi: "9".into(),
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
        let frame = Frame {
            args: &args,
            system: &system,
            numa: 1,
            live: &live,
            memory: (32 << 30, 16 << 30, 4 << 30),
            now_unix: 2000,
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
