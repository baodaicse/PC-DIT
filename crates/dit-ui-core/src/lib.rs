//! Event-driven terminal UI with dynamic per-device layout.

mod dashboard;
mod unified;

pub use unified::{DashboardMeter, UnifiedDashboard, boxify, decorate, render_unified_dashboard};

/// Whether this is the Linux console (`TERM=linux`): 16 colours, where colour 3 is orange-brown.
#[must_use]
pub fn linux_console() -> bool {
    std::env::var("TERM").is_ok_and(|term| term == "linux")
}

/// Whether the terminal draws Unicode box characters: the Linux console (UTF-8 by default) and the
/// usual terminal emulators do; a serial line (`vt100`, `vt220`, `dumb`) does not. `DIT_UI_ASCII=1`
/// forces the ASCII frame. The glyphs used (box drawing, full and light-shade blocks) are in the
/// kernel's built-in console font.
#[must_use]
pub fn unicode_terminal() -> bool {
    if std::env::var_os("DIT_UI_ASCII").is_some_and(|value| !value.is_empty() && value != "0") {
        return false;
    }
    let term = std::env::var("TERM").unwrap_or_default();
    if term == "linux"
        || [
            "xterm",
            "screen",
            "tmux",
            "rxvt",
            "alacritty",
            "kitty",
            "foot",
            "wezterm",
            "vte",
            "konsole",
            "st-",
        ]
        .iter()
        .any(|prefix| term.starts_with(prefix))
    {
        return true;
    }
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .any(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("utf-8") || value.contains("utf8")
        })
}

/// Version of the fixed dashboard shell shared by all active DIT engines.
pub const UNIFIED_DASHBOARD_ABI: u32 = 1;

use dit_contracts::{DeviceId, Event, EventEnvelope, Phase, Verdict};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::time::{Duration, Instant};

const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const GRAY: &str = "\x1b[90m";
const RESET: &str = "\x1b[0m";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl TerminalSize {
    #[must_use]
    pub fn current() -> Self {
        terminal_size::terminal_size().map_or(
            Self {
                rows: 30,
                cols: 120,
            },
            |(terminal_size::Width(cols), terminal_size::Height(rows))| Self { rows, cols },
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutMode {
    Expanded,
    Compact,
    Paged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayoutPlan {
    pub mode: LayoutMode,
    pub first_device: usize,
    pub visible_devices: usize,
    pub rows_per_device: usize,
}

#[derive(Clone, Debug)]
pub struct SystemInfo {
    pub cpu: String,
    pub memory_total_bytes: u64,
    pub gpu: String,
    pub board: String,
    pub logical_threads: usize,
    pub kernel: String,
}

impl Default for SystemInfo {
    fn default() -> Self {
        Self {
            cpu: "N/A".to_owned(),
            memory_total_bytes: 0,
            gpu: "N/A".to_owned(),
            board: "N/A".to_owned(),
            logical_threads: 0,
            kernel: "N/A".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LiveUtilization {
    pub cpu_percent: Option<f64>,
    pub cpu_frequency_ghz: Option<f64>,
    pub cpu_temperature_celsius: Option<f64>,
    pub cpu_power_watts: Option<f64>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
}

#[derive(Debug)]
pub struct HostSampler {
    system: SystemInfo,
    previous_cpu: Option<(u64, u64)>,
}

impl HostSampler {
    #[must_use]
    pub fn detect() -> Self {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let cpu = cpuinfo
            .lines()
            .find_map(|line| line.strip_prefix("model name\t: "))
            .unwrap_or("N/A")
            .to_owned();
        let memory_total_bytes = parse_meminfo(&meminfo).0;
        let board = fs::read_to_string("/sys/class/dmi/id/board_name")
            .map_or_else(|_| "N/A".to_owned(), |value| value.trim().to_owned());
        Self {
            system: SystemInfo {
                cpu,
                memory_total_bytes,
                gpu: "N/A".to_owned(),
                board,
                logical_threads: std::thread::available_parallelism()
                    .map_or(0, std::num::NonZero::get),
                kernel: fs::read_to_string("/proc/sys/kernel/osrelease")
                    .map_or_else(|_| "N/A".to_owned(), |value| value.trim().to_owned()),
            },
            previous_cpu: None,
        }
    }

    #[must_use]
    pub fn system_info(&self) -> SystemInfo {
        self.system.clone()
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn sample(&mut self) -> LiveUtilization {
        let cpu_stat = fs::read_to_string("/proc/stat").unwrap_or_default();
        let current_cpu = parse_cpu_stat(&cpu_stat);
        let cpu_percent = self.previous_cpu.zip(current_cpu).and_then(
            |((previous_total, previous_idle), (current_total, current_idle))| {
                let total_delta = current_total.saturating_sub(previous_total);
                let idle_delta = current_idle.saturating_sub(previous_idle);
                (total_delta > 0).then(|| {
                    100.0 * total_delta.saturating_sub(idle_delta) as f64 / total_delta as f64
                })
            },
        );
        self.previous_cpu = current_cpu;
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let (memory_total_bytes, memory_available_bytes) = parse_meminfo(&meminfo);
        LiveUtilization {
            cpu_percent,
            cpu_frequency_ghz: current_cpu_frequency_ghz(),
            memory_used_bytes: memory_total_bytes.saturating_sub(memory_available_bytes),
            memory_total_bytes,
            ..LiveUtilization::default()
        }
    }
}

fn current_cpu_frequency_ghz() -> Option<f64> {
    let khz = fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq")
        .ok()
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| (100_000.0..=10_000_000.0).contains(value));
    if let Some(khz) = khz {
        return Some(khz / 1_000_000.0);
    }
    fs::read_to_string("/proc/cpuinfo")
        .ok()?
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == "cpu MHz")
                .then(|| value.trim().parse::<f64>().ok())
                .flatten()
        })
        .filter(|value| (100.0..=10_000.0).contains(value))
        .map(|mhz| mhz / 1000.0)
}

/// Computes layout without fixed SSD row coordinates.
#[must_use]
pub fn calculate_layout(size: TerminalSize, device_count: usize, page: usize) -> LayoutPlan {
    let dashboard = size.cols >= 110 && size.rows >= 30;
    let fixed_rows = if dashboard {
        25
    } else if size.rows >= 18 {
        15
    } else {
        14
    };
    // Leave the last terminal row unused to avoid bottom-right auto-scroll.
    let available = usize::from(size.rows).saturating_sub(1 + fixed_rows);
    let visible = (available / 2).max(1).min(device_count);
    let pages = device_count.div_ceil(visible.max(1)).max(1);
    let first = (page % pages) * visible;
    LayoutPlan {
        mode: if pages > 1 {
            LayoutMode::Paged
        } else if dashboard {
            LayoutMode::Expanded
        } else {
            LayoutMode::Compact
        },
        first_device: first,
        visible_devices: visible.min(device_count.saturating_sub(first)),
        rows_per_device: 2,
    }
}

#[derive(Clone, Debug)]
pub struct DeviceDescriptor {
    pub id: DeviceId,
    pub label: String,
    pub model: String,
    /// Physical namespace capacity, kept separate from RAM and test-file limits.
    pub capacity: String,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub struct PickerEntry {
    pub alias: usize,
    pub device_node: String,
    pub model: String,
    pub capacity: String,
    pub serial: String,
    pub driver: String,
    pub link: String,
    pub safety: String,
}

#[derive(Clone, Debug)]
struct DeviceView {
    descriptor: DeviceDescriptor,
    phase: Phase,
    completed: u64,
    target: u64,
    bytes_verified: u64,
    failures: u64,
    current_read: f64,
    current_write: f64,
    average_read: f64,
    average_write: f64,
    peak_read: f64,
    peak_write: f64,
    peak_total: f64,
    deep_stage: String,
    matrix_checks: u64,
    compute_jobs: u64,
    write_operations: u64,
    read_soak_operations: u64,
    compute_workers: usize,
    matrix_rounds: u32,
    duration_target_met: bool,
    verdict: Option<Verdict>,
}

impl DeviceView {
    fn new(descriptor: DeviceDescriptor) -> Self {
        Self {
            descriptor,
            phase: Phase::Preparing,
            completed: 0,
            target: 0,
            bytes_verified: 0,
            failures: 0,
            current_read: 0.0,
            current_write: 0.0,
            average_read: 0.0,
            average_write: 0.0,
            peak_read: 0.0,
            peak_write: 0.0,
            peak_total: 0.0,
            deep_stage: "PREPARE".to_owned(),
            matrix_checks: 0,
            compute_jobs: 0,
            write_operations: 0,
            read_soak_operations: 0,
            compute_workers: 0,
            matrix_rounds: 0,
            duration_target_met: false,
            verdict: None,
        }
    }
}

#[derive(Debug)]
pub struct UiState {
    title: String,
    devices: BTreeMap<DeviceId, DeviceView>,
    order: Vec<DeviceId>,
    started: Instant,
    page: usize,
    system: SystemInfo,
    live: LiveUtilization,
    duration_limit: Option<Duration>,
    session_seed: Option<u64>,
}

impl UiState {
    #[must_use]
    pub fn new(title: impl Into<String>, descriptors: Vec<DeviceDescriptor>) -> Self {
        let mut devices = BTreeMap::new();
        let mut order = Vec::new();
        for descriptor in descriptors {
            if devices.contains_key(&descriptor.id) {
                continue;
            }
            order.push(descriptor.id.clone());
            devices.insert(descriptor.id.clone(), DeviceView::new(descriptor));
        }
        Self {
            title: title.into(),
            devices,
            order,
            started: Instant::now(),
            page: 0,
            system: SystemInfo::default(),
            live: LiveUtilization::default(),
            duration_limit: None,
            session_seed: None,
        }
    }

    pub fn set_system_info(&mut self, system: SystemInfo) {
        self.system = system;
    }

    pub fn set_live_utilization(&mut self, live: LiveUtilization) {
        self.live = live;
    }

    pub fn set_run_context(&mut self, duration_limit: Duration, seed: u64) {
        self.duration_limit = Some(duration_limit);
        self.session_seed = Some(seed);
    }

    pub fn ingest(&mut self, envelope: &EventEnvelope) {
        let Some(device_id) = envelope.device_id.as_ref() else {
            return;
        };
        let Some(device) = self.devices.get_mut(device_id) else {
            return;
        };
        match &envelope.event {
            Event::Inventory { model, .. } if device.descriptor.model.is_empty() => {
                device.descriptor.model.clone_from(model);
            }
            Event::Phase { phase, .. } => device.phase = *phase,
            Event::Progress {
                completed_operations,
                target_operations,
                bytes_verified,
                failures,
                ..
            } => {
                device.completed = *completed_operations;
                device.target = *target_operations;
                device.bytes_verified = *bytes_verified;
                device.failures = *failures;
            }
            Event::Throughput {
                current_read_bytes_per_sec,
                current_write_bytes_per_sec,
                average_read_bytes_per_sec,
                average_write_bytes_per_sec,
                peak_read_bytes_per_sec,
                peak_write_bytes_per_sec,
            } => {
                device.current_read = *current_read_bytes_per_sec;
                device.current_write = *current_write_bytes_per_sec;
                device.peak_total = device
                    .peak_total
                    .max(device.current_read + device.current_write);
                device.average_read = *average_read_bytes_per_sec;
                device.average_write = *average_write_bytes_per_sec;
                device.peak_read = *peak_read_bytes_per_sec;
                device.peak_write = *peak_write_bytes_per_sec;
            }
            Event::DeepProgress { stage, evidence } => {
                device.deep_stage.clone_from(stage);
                device.matrix_checks = evidence.matrix_checks_completed;
                device.compute_jobs = evidence.compute_jobs_completed;
                device.write_operations = evidence.write_operations_completed;
                device.read_soak_operations = evidence.read_soak_operations;
                device.compute_workers = evidence.compute_workers;
                device.matrix_rounds = evidence.matrix_rounds;
                device.duration_target_met = evidence.duration_target_met;
            }
            Event::Failure { .. } => device.failures = device.failures.saturating_add(1),
            Event::Completed {
                verdict,
                completed_operations,
                failures,
            } => {
                device.verdict = Some(*verdict);
                device.completed = *completed_operations;
                device.failures = *failures;
                device.current_read = 0.0;
                device.current_write = 0.0;
                device.phase = Phase::Finalizing;
            }
            _ => {}
        }
    }

    pub fn next_page(&mut self) {
        self.page = self.page.saturating_add(1);
    }

    #[must_use]
    pub fn render(&self, size: TerminalSize, color: bool) -> String {
        dashboard::render(self, size, color)
    }
}

/// Renders the selection list used before a multi-device run.
#[must_use]
pub fn render_picker(
    entries: &[PickerEntry],
    selected: &BTreeSet<usize>,
    size: TerminalSize,
) -> String {
    render_device_picker(
        "DIT CPU-SSD // SELECT NVMe DEVICES",
        "Enter aliases (1,2) or A=all; plans store stable IDs",
        entries,
        selected,
        size,
    )
}

/// Renders a module-specific picker with the same box geometry as CPU-SSD.
#[must_use]
pub fn render_device_picker(
    title: &str,
    prompt: &str,
    entries: &[PickerEntry],
    selected: &BTreeSet<usize>,
    size: TerminalSize,
) -> String {
    let width = usize::from(size.cols).clamp(50, 240);
    let inner = width - 2;
    let mut lines = vec![
        format!("+{}+", "-".repeat(inner)),
        box_line(title, inner),
        format!("+{}+", "-".repeat(inner)),
    ];
    for entry in entries {
        let mark = if selected.contains(&entry.alias) {
            'x'
        } else {
            ' '
        };
        lines.push(box_line(
            &format!(
                "[{mark}] {}  {}  {}  {}  SN:{}  driver:{}  {}  {}",
                entry.alias,
                entry.device_node,
                entry.model,
                entry.capacity,
                entry.serial,
                entry.driver,
                entry.link,
                entry.safety
            ),
            inner,
        ));
    }
    lines.push(format!("+{}+", "-".repeat(inner)));
    lines.push(box_line(prompt, inner));
    lines.push(format!("+{}+", "-".repeat(inner)));
    lines.join("\n")
}

pub struct InPlaceRenderer<W: Write> {
    writer: W,
    previous_lines: Vec<String>,
    last_frame: Option<Instant>,
    frame_interval: Duration,
    cursor_hidden: bool,
}

impl<W: Write> InPlaceRenderer<W> {
    #[must_use]
    pub fn new(writer: W, frames_per_second: u32) -> Self {
        let fps = frames_per_second.clamp(1, 20);
        Self {
            writer,
            previous_lines: Vec::new(),
            last_frame: None,
            frame_interval: Duration::from_millis(1000 / u64::from(fps)),
            cursor_hidden: false,
        }
    }

    /// Updates the terminal only when the configured frame interval has elapsed.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when writing or flushing the terminal fails.
    pub fn render_if_due(&mut self, state: &UiState, force: bool) -> io::Result<bool> {
        if !force
            && self
                .last_frame
                .is_some_and(|last| last.elapsed() < self.frame_interval)
        {
            return Ok(false);
        }
        let frame = state.render(TerminalSize::current(), true);
        self.render_frame(&frame)?;
        self.last_frame = Some(Instant::now());
        Ok(true)
    }

    /// Repaints the previous frame in place without clearing the whole screen.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when writing or flushing the terminal fails.
    pub fn render_frame(&mut self, frame: &str) -> io::Result<()> {
        let mut update = Vec::new();
        if !self.cursor_hidden {
            write!(update, "\x1b[?25l\x1b[H")?;
            self.cursor_hidden = true;
        }
        let lines = frame.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            if self.previous_lines.get(index).map(String::as_str) != Some(*line) {
                write!(update, "\x1b[{};1H{line}", index + 1)?;
            }
        }
        for index in lines.len()..self.previous_lines.len() {
            if index >= usize::from(TerminalSize::current().rows) {
                break;
            }
            write!(update, "\x1b[{};1H\x1b[2K", index + 1)?;
        }
        self.writer.write_all(&update)?;
        self.writer.flush()?;
        self.previous_lines = lines.into_iter().map(str::to_owned).collect();
        Ok(())
    }

    /// Restores the cursor after live rendering.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when writing or flushing the terminal fails.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.cursor_hidden {
            write!(
                self.writer,
                "\x1b[{};1H\x1b[0m\x1b[?25h",
                self.previous_lines.len() + 1
            )?;
            self.writer.flush()?;
            self.cursor_hidden = false;
        }
        Ok(())
    }
}

impl<W: Write> Drop for InPlaceRenderer<W> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn aggregate_state<'a>(devices: impl Iterator<Item = &'a DeviceView>) -> &'static str {
    let verdicts = devices.map(|device| device.verdict).collect::<Vec<_>>();
    if verdicts.is_empty() {
        "NOT_RUN"
    } else if verdicts.iter().any(Option::is_none) {
        "RUNNING"
    } else if verdicts.contains(&Some(Verdict::Invalid)) {
        "INVALID"
    } else if verdicts.contains(&Some(Verdict::Fail)) {
        "FAIL"
    } else if verdicts.contains(&Some(Verdict::Aborted)) {
        "ABORTED"
    } else if verdicts.iter().all(|value| *value == Some(Verdict::Pass)) {
        "PASS"
    } else {
        "NOT_RUN"
    }
}

fn verdict_or_running(verdict: Option<Verdict>) -> &'static str {
    match verdict {
        Some(Verdict::Pass) => "PASS",
        Some(Verdict::Fail) => "FAIL",
        Some(Verdict::Aborted) => "ABORTED",
        Some(Verdict::Invalid) => "INVALID",
        Some(Verdict::NotRun) => "NOT_RUN",
        None => "RUNNING",
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Preparing => "PREPARING",
        Phase::Writing => "WRITING",
        Phase::Reading => "READING",
        Phase::Verifying => "VERIFYING",
        Phase::Finalizing => "FINALIZING",
    }
}

#[allow(clippy::cast_precision_loss)]
fn bar(value: f64, ceiling: f64, width: usize) -> String {
    let ratio = if ceiling > 0.0 {
        (value / ceiling).clamp(0.0, 1.0)
    } else {
        0.0
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let filled = (ratio * width as f64).round() as usize;
    format!("{}{}", "#".repeat(filled), "-".repeat(width - filled))
}

fn format_rate(bytes_per_second: f64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * MIB;
    if bytes_per_second >= GIB {
        format!("{:.2} GiB/s", bytes_per_second / GIB)
    } else {
        format!("{:.1} MiB/s", bytes_per_second / MIB)
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    }
}

fn box_line(content: &str, inner: usize) -> String {
    let visible = sanitize_and_clip(content, inner.saturating_sub(2));
    format!("| {visible:<width$} |", width = inner.saturating_sub(2))
}

fn sanitize_and_clip(content: &str, maximum: usize) -> String {
    content
        .chars()
        .filter(|character| !character.is_control())
        .map(|character| if character.is_ascii() { character } else { '?' })
        .take(maximum)
        .collect()
}

fn parse_cpu_stat(content: &str) -> Option<(u64, u64)> {
    let fields = content
        .lines()
        .next()?
        .split_whitespace()
        .collect::<Vec<_>>();
    if fields.first().copied() != Some("cpu") || fields.len() < 5 {
        return None;
    }
    let values = fields[1..]
        .iter()
        .map(|field| field.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    let total = values.iter().copied().fold(0_u64, u64::saturating_add);
    let idle = values
        .get(3)
        .copied()
        .unwrap_or(0)
        .saturating_add(values.get(4).copied().unwrap_or(0));
    Some((total, idle))
}

fn parse_meminfo(content: &str) -> (u64, u64) {
    let mut total_kib = 0_u64;
    let mut available_kib = 0_u64;
    for line in content.lines() {
        if let Some(value) = line.strip_prefix("MemTotal:") {
            total_kib = first_u64(value).unwrap_or(0);
        } else if let Some(value) = line.strip_prefix("MemAvailable:") {
            available_kib = first_u64(value).unwrap_or(0);
        }
    }
    (
        total_kib.saturating_mul(1024),
        available_kib.saturating_mul(1024),
    )
}

fn first_u64(value: &str) -> Option<u64> {
    value.split_whitespace().next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dit_contracts::{EventEnvelope, RunId};

    fn descriptors(count: usize) -> Vec<DeviceDescriptor> {
        (0..count)
            .map(|index| DeviceDescriptor {
                id: DeviceId(format!("nvme:{index}")),
                label: format!("SSD {}", index + 1),
                model: format!("Model-{index}"),
                capacity: "953.87 GiB total".to_owned(),
                detail: format!("Gen4 x4 / id-{index}"),
            })
            .collect()
    }

    fn event(device: usize, sequence: u64, event: Event) -> EventEnvelope {
        EventEnvelope::new(
            RunId("ui-test".to_owned()),
            Some(DeviceId(format!("nvme:{device}"))),
            sequence,
            sequence,
            sequence,
            event,
        )
    }

    #[test]
    fn layout_expands_compacts_and_pages_by_device_count_and_height() {
        assert_eq!(
            calculate_layout(
                TerminalSize {
                    rows: 40,
                    cols: 120
                },
                4,
                0
            )
            .mode,
            LayoutMode::Expanded
        );
        assert_eq!(
            calculate_layout(
                TerminalSize {
                    rows: 24,
                    cols: 100
                },
                4,
                0
            )
            .mode,
            LayoutMode::Compact
        );
        let paged = calculate_layout(TerminalSize { rows: 10, cols: 80 }, 4, 1);
        assert_eq!(paged.mode, LayoutMode::Paged);
        assert_eq!(paged.visible_devices, 1);
        assert_eq!(paged.first_device, 1);
    }

    #[test]
    fn snapshots_cover_one_two_and_four_devices_without_overflow() {
        for count in [1, 2, 4] {
            let state = UiState::new("DIT CPU-NVMe", descriptors(count));
            let frame = state.render(
                TerminalSize {
                    rows: 30,
                    cols: 100,
                },
                false,
            );
            assert_eq!(frame.lines().next().unwrap().chars().count(), 100);
            assert!(frame.lines().all(|line| line.chars().count() == 100));
            for index in 1..=count {
                assert!(frame.contains(&format!("SSD {index}")));
            }
        }
    }

    #[test]
    fn events_update_independent_speed_progress_and_failure_rows() {
        let mut state = UiState::new("DIT CPU-NVMe", descriptors(2));
        state.ingest(&event(
            0,
            1,
            Event::Progress {
                completed_operations: 7,
                target_operations: 10,
                bytes_written: 4096,
                bytes_verified: 4096,
                failures: 0,
            },
        ));
        state.ingest(&event(
            0,
            2,
            Event::Throughput {
                current_read_bytes_per_sec: 1024.0 * 1024.0 * 300.0,
                current_write_bytes_per_sec: 1024.0 * 1024.0 * 100.0,
                average_read_bytes_per_sec: 1.0,
                average_write_bytes_per_sec: 1.0,
                peak_read_bytes_per_sec: 1024.0 * 1024.0 * 400.0,
                peak_write_bytes_per_sec: 1024.0 * 1024.0 * 100.0,
            },
        ));
        state.ingest(&event(
            1,
            1,
            Event::Completed {
                verdict: Verdict::Fail,
                completed_operations: 3,
                failures: 2,
            },
        ));
        let frame = state.render(
            TerminalSize {
                rows: 30,
                cols: 120,
            },
            false,
        );
        assert!(frame.contains("300.0 MiB/s"));
        assert!(frame.contains("I/O 7/10 MATRIX 0 SOAK 0 FAIL 0"));
        assert!(frame.contains("FAIL"));
    }

    #[test]
    fn in_place_renderer_never_clears_the_whole_screen() {
        let state = UiState::new("DIT CPU-NVMe", descriptors(1));
        let mut output = Vec::new();
        {
            let mut renderer = InPlaceRenderer::new(&mut output, 4);
            renderer
                .render_frame(&state.render(TerminalSize { rows: 20, cols: 80 }, false))
                .unwrap();
            renderer.finish().unwrap();
        }
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("\x1b[H"));
        assert!(!output.contains("\x1b[2K"));
        assert!(!output.contains('\n'));
        assert!(!output.contains("\x1b[2J"));
        assert!(output.ends_with("\x1b[?25h"));
    }

    #[test]
    fn picker_marks_exact_selected_aliases() {
        let entries = vec![PickerEntry {
            alias: 1,
            device_node: "/dev/nvme0n1".to_owned(),
            model: "Example".to_owned(),
            capacity: "1.00 TiB".to_owned(),
            serial: "ABC".to_owned(),
            driver: "nvme".to_owned(),
            link: "Gen4 x4".to_owned(),
            safety: "data-preserving".to_owned(),
        }];
        let frame = render_picker(
            &entries,
            &BTreeSet::from([1]),
            TerminalSize {
                rows: 20,
                cols: 100,
            },
        );
        assert!(frame.contains("[x] 1"));
        assert!(frame.contains("/dev/nvme0n1"));
    }

    #[test]
    fn proc_parsers_produce_cpu_and_memory_counters() {
        assert_eq!(
            parse_cpu_stat("cpu  10 2 3 40 5 6 7 8\ncpu0 1 2 3 4"),
            Some((81, 45))
        );
        assert_eq!(
            parse_meminfo("MemTotal:       1000 kB\nMemAvailable:    400 kB\n"),
            (1_024_000, 409_600)
        );
    }

    #[test]
    fn system_and_live_rows_follow_the_cpu_ram_ui_language() {
        let mut state = UiState::new("DIT CPU-NVMe", descriptors(1));
        state.set_system_info(SystemInfo {
            cpu: "Example CPU".to_owned(),
            memory_total_bytes: 16 * 1024 * 1024 * 1024,
            gpu: "Example GPU [0000:01:00.0; amdgpu; VRAM 8.00 GiB]".to_owned(),
            board: "Example Board".to_owned(),
            ..SystemInfo::default()
        });
        state.set_live_utilization(LiveUtilization {
            cpu_percent: Some(25.0),
            cpu_frequency_ghz: Some(4.25),
            cpu_temperature_celsius: Some(72.5),
            cpu_power_watts: Some(105.3),
            memory_used_bytes: 8 * 1024 * 1024 * 1024,
            memory_total_bytes: 16 * 1024 * 1024 * 1024,
        });
        let frame = state.render(
            TerminalSize {
                rows: 30,
                cols: 120,
            },
            false,
        );
        assert!(frame.contains("SYSTEM HARDWARE"));
        assert!(frame.contains("CPU:     Example CPU"));
        assert!(frame.contains("GPU:     Example GPU"));
        assert!(frame.contains("LIVE UTILIZATION"));
        // Labels are padded so the bars of a panel line up.
        let row = |label: &str| {
            frame
                .lines()
                .find(|line| line.contains(label) && line.contains('['))
                .unwrap_or_else(|| panic!("no {label} meter"))
                .to_owned()
        };
        assert!(row("CPU USAGE").contains(" 25%"));
        assert!(frame.contains(" 25%"));
        assert!(frame.contains("Freq 4.25 GHz / Temp 72.5 C / Power 105.3 W"));
        assert!(row("RAM USAGE").contains(" 50%"));
        let bar_column = |label: &str| {
            let line = row(label);
            let at = line.find(label).unwrap();
            line[at..].find('[').map(|offset| at + offset)
        };
        assert_eq!(
            bar_column("CPU USAGE"),
            bar_column("RAM USAGE"),
            "bars start in one column"
        );
        assert!(frame.contains(" 50%"));
        assert!(frame.contains("RAM used 8.00 GiB / 16.00 GiB total"));
        assert!(frame.contains("953.87 GiB total"));
    }

    #[test]
    fn each_ssd_adds_exactly_one_live_row_and_moves_process_panel_down() {
        let mut previous_process_row = None;
        for count in 1..=4 {
            let state = UiState::new("DIT CPU-NVMe", descriptors(count));
            let frame = state.render(
                TerminalSize {
                    rows: 40,
                    cols: 160,
                },
                false,
            );
            let speed_rows = frame
                .lines()
                .filter(|line| line.contains(" R ") && line.contains(" W "))
                .count();
            assert_eq!(speed_rows, count);
            let process_row = frame
                .lines()
                .position(|line| line.contains("TESTING / PROCESS INFORMATION"))
                .unwrap();
            // One more meter row per SSD, plus a blank row before it when the screen has room.
            if let Some(previous) = previous_process_row {
                assert!(
                    process_row == previous + 1 || process_row == previous + 2,
                    "{previous} -> {process_row}"
                );
            }
            previous_process_row = Some(process_row);
            assert!(
                frame
                    .lines()
                    .any(|line| line.contains("SYSTEM HARDWARE")
                        && line.contains("LIVE UTILIZATION"))
            );
        }
    }

    #[test]
    fn all_layouts_fit_terminal_including_paged_and_empty_states() {
        for count in [0, 1, 2, 4, 8, 32] {
            let state = UiState::new("DIT CPU-NVMe", descriptors(count));
            for rows in [17, 24, 30, 32, 40, 60] {
                for cols in [40, 80, 100, 110, 120, 180, 240] {
                    let frame = state.render(TerminalSize { rows, cols }, false);
                    assert!(
                        frame.lines().count() < usize::from(rows),
                        "{count} SSD / {cols}x{rows}"
                    );
                    assert!(
                        frame
                            .lines()
                            .all(|line| line.chars().count() == usize::from(cols))
                    );
                }
            }
        }
    }

    #[test]
    fn paging_reaches_last_device_and_keeps_total_failure_count() {
        let mut state = UiState::new("DIT CPU-NVMe", descriptors(5));
        state.ingest(&event(
            4,
            1,
            Event::Completed {
                verdict: Verdict::Fail,
                completed_operations: 1,
                failures: 3,
            },
        ));
        let size = TerminalSize {
            cols: 120,
            rows: 30,
        };
        let first = state.render(size, false);
        assert!(first.contains("PAGE 1/3"));
        assert!(first.contains("FAIL TOTAL  3"));
        state.next_page();
        state.next_page();
        let last = state.render(size, false);
        assert!(last.contains("PAGE 3/3"));
        assert!(last.contains("SSD 5"));
        assert!(!last.contains("SSD 1:"));
    }

    #[test]
    fn renderer_skips_identical_frames_and_updates_only_changed_line() {
        let mut renderer = InPlaceRenderer::new(Vec::new(), 8);
        renderer.render_frame("first\nsecond").unwrap();
        let first_length = renderer.writer.len();
        renderer.render_frame("first\nsecond").unwrap();
        assert_eq!(renderer.writer.len(), first_length);
        renderer.render_frame("first\nchange").unwrap();
        assert_eq!(&renderer.writer[first_length..], b"\x1b[2;1Hchange");
        renderer.render_frame("first").unwrap();
        assert!(renderer.writer.ends_with(b"\x1b[2;1H\x1b[2K"));
    }

    #[test]
    fn missing_metrics_stay_na_and_seed_duration_are_real_context() {
        let mut state = UiState::new("DIT CPU-NVMe", descriptors(1));
        state.set_run_context(Duration::from_mins(30), 123);
        let frame = state.render(
            TerminalSize {
                rows: 40,
                cols: 160,
            },
            false,
        );
        assert!(
            frame
                .lines()
                .find(|line| line.contains("CPU USAGE"))
                .unwrap()
                .contains("N/A")
        );
        assert!(frame.contains("1800s stop limit"));
        assert!(frame.contains("0x000000000000007b"));
        assert!(frame.contains("main progress = duration"));
    }

    #[test]
    fn deep_progress_exposes_matrix_workers_and_read_soak() {
        let mut state = UiState::new("DIT CPU-NVMe", descriptors(1));
        state.set_run_context(Duration::from_secs(60), 123);
        state.ingest(&event(
            0,
            1,
            Event::DeepProgress {
                stage: "READ_SOAK_COMPUTE".into(),
                evidence: dit_contracts::DeepEvidence {
                    compute_workers: 15,
                    matrix_rounds: 4,
                    compute_jobs_completed: 91,
                    matrix_checks_completed: 4096,
                    write_operations_completed: 17,
                    read_soak_operations: 23,
                    ..dit_contracts::DeepEvidence::default()
                },
            },
        ));
        let frame = state.render(
            TerminalSize {
                rows: 40,
                cols: 180,
            },
            false,
        );
        assert!(frame.contains("READ_SOAK_COMPUTE"));
        assert!(frame.contains("MATRIX CHECKS COMPLETED  4096"));
        assert!(frame.contains("CPU WORKERS 15"));
        assert!(frame.contains("WRITE OPS 17"));
        assert!(frame.contains("READ-SOAK 23"));
    }

    #[test]
    fn colored_frame_has_same_visible_geometry_and_green_bars() {
        let mut state = UiState::new("DIT CPU-NVMe", descriptors(2));
        state.set_live_utilization(LiveUtilization {
            cpu_percent: Some(50.0),
            ..LiveUtilization::default()
        });
        let size = TerminalSize {
            rows: 40,
            cols: 160,
        };
        let colored = state.render(size, true);
        // Bars are `#` on an ASCII terminal, full blocks on a Unicode one.
        // Bar fills: the console's plain green, or a softer green elsewhere.
        assert!(colored.contains("\x1b[38;5;71m") || colored.contains(&format!("{GREEN}#")));
        assert!(colored.contains(CYAN));
        let mut stripped = String::new();
        let mut escape = false;
        for character in colored.chars() {
            if character == '\x1b' {
                escape = true;
            } else if escape {
                if character == 'm' {
                    escape = false;
                }
            } else {
                stripped.push(character);
            }
        }
        let plain = state.render(size, false);
        if unicode_terminal() {
            // Same geometry: the same characters per line, the frame drawn with box characters.
            let lines: Vec<String> = plain.lines().map(str::to_owned).collect();
            let boxed: Vec<String> = boxify(&lines)
                .into_iter()
                .map(|row| row.into_iter().collect())
                .collect();
            assert_eq!(stripped, boxed.join("\n"));
            for (a, b) in stripped.lines().zip(plain.lines()) {
                assert_eq!(a.chars().count(), b.chars().count());
            }
        } else {
            assert_eq!(stripped, plain);
        }
    }

    #[test]
    fn frames_turn_into_box_drawing_without_touching_text() {
        let lines: Vec<String> = [
            "+-- SYSTEM HARDWARE --+  +-- LIVE --+",
            "| CPU-RAM [3/8+] a-b  |  | [##--] x |",
            "| CPU + RAM           |  | y        |",
            "| GPU + VRAM          |  | z        |",
            "+---------------------+  +----------+",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        let boxed: Vec<String> = boxify(&lines)
            .into_iter()
            .map(|row| row.into_iter().collect())
            .collect();
        assert_eq!(
            boxed[0],
            "\u{250c}\u{2500}\u{2500} SYSTEM HARDWARE \u{2500}\u{2500}\u{2510}  \u{250c}\u{2500}\u{2500} LIVE \u{2500}\u{2500}\u{2510}"
        );
        assert_eq!(
            boxed[1],
            "\u{2502} CPU-RAM [3/8+] a-b  \u{2502}  \u{2502} [\u{25a0}\u{25a0}\u{2500}\u{2500}] x \u{2502}"
        );
        assert!(
            boxed[2].contains("CPU + RAM") && boxed[3].contains("GPU + VRAM"),
            "text pluses stay"
        );
        assert!(boxed[4].starts_with("\u{2514}\u{2500}") && boxed[4].ends_with("\u{2500}\u{2518}"));
        for (a, b) in boxed.iter().zip(&lines) {
            assert_eq!(
                a.chars().count(),
                b.chars().count(),
                "one cell per character"
            );
        }
    }

    #[test]
    fn generic_picker_reuses_cpu_ssd_geometry_for_gpu_module() {
        let entries = vec![PickerEntry {
            alias: 1,
            device_node: "0000:01:00.0".to_owned(),
            model: "Example GPU".to_owned(),
            capacity: "8.00 GiB".to_owned(),
            serial: "uuid".to_owned(),
            driver: "amdgpu".to_owned(),
            link: "Vulkan 1.3".to_owned(),
            safety: "READY".to_owned(),
        }];
        let frame = render_device_picker(
            "DIT GPU-VRAM // SELECT DEVICES",
            "Select aliases or A=all",
            &entries,
            &BTreeSet::new(),
            TerminalSize {
                rows: 20,
                cols: 100,
            },
        );
        assert!(frame.contains("DIT GPU-VRAM // SELECT DEVICES"));
        assert!(frame.contains("Example GPU"));
        assert!(frame.lines().all(|line| line.chars().count() == 100));
    }
}
