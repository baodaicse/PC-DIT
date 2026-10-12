//! Live screen of the combined run: the same `UnifiedDashboard` shell as every single-module
//! test. System and plan on the left, usage bars on the right (CPU with GHz / temperature /
//! power, RAM, each GPU and its VRAM, each SSD's read/write speed and temperature, the data
//! chain), phase and per-layer progress below.
use dit_orchestrator::combined::{Intensity, Level};
use dit_system::{
    GpuDevice, GpuTelemetry, GpuTelemetryFeed, LiveSensorSampler, SlowdownCounter, ThrottleGroup,
    collect_gpu_inventory,
};
use dit_ui_core::{
    DashboardMeter, HostSampler, LiveUtilization, SystemInfo, TerminalSize, ThrottleLive,
    UnifiedDashboard, render_unified_dashboard, wrap_result_rows,
};
use nvme_inventory::SmartSnapshot;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = 1024.0 * MIB;

#[allow(clippy::cast_precision_loss)]
pub fn format_bytes(bytes: u64) -> String {
    if bytes as f64 >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB)
    }
}

pub fn format_rate(bytes_per_second: f64) -> String {
    if bytes_per_second >= GIB {
        format!("{:.2} GiB/s", bytes_per_second / GIB)
    } else {
        format!("{:.0} MiB/s", bytes_per_second / MIB)
    }
}

pub fn format_duration(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
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

/// Read/write rate of one block device from `/proc/diskstats` deltas.
pub struct DiskMeter {
    pub label: String,
    pub block_name: String,
    last: Option<(Instant, u64, u64)>,
    pub read_bps: f64,
    pub write_bps: f64,
    pub peak_bps: f64,
    pub temperature_celsius: Option<f64>,
}

impl DiskMeter {
    #[must_use]
    pub fn new(label: String, block_name: String) -> Self {
        Self {
            label,
            block_name,
            last: None,
            read_bps: 0.0,
            write_bps: 0.0,
            peak_bps: 0.0,
            temperature_celsius: None,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn sample(&mut self, diskstats: &str) {
        let Some((read, written)) = sectors(diskstats, &self.block_name) else {
            return;
        };
        let now = Instant::now();
        if let Some((then, last_read, last_written)) = self.last {
            let seconds = now.duration_since(then).as_secs_f64().max(1e-3);
            self.read_bps = read.saturating_sub(last_read) as f64 / seconds;
            self.write_bps = written.saturating_sub(last_written) as f64 / seconds;
            self.peak_bps = self.peak_bps.max(self.read_bps + self.write_bps);
        }
        self.last = Some((now, read, written));
        self.temperature_celsius = nvme_temperature(&self.block_name);
    }
}

/// Bytes read and written by `name` so far (sectors are 512 bytes in `/proc/diskstats`).
#[must_use]
pub fn sectors(diskstats: &str, name: &str) -> Option<(u64, u64)> {
    diskstats.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.get(2) == Some(&name)).then(|| {
            let read = fields.get(5)?.parse::<u64>().ok()?;
            let written = fields.get(9)?.parse::<u64>().ok()?;
            Some((read * 512, written * 512))
        })?
    })
}

/// Composite temperature of the controller behind namespace `name` (hwmon `temp1_input`).
fn nvme_temperature(name: &str) -> Option<f64> {
    let device = Path::new("/sys/block").join(name).join("device");
    std::fs::read_dir(&device)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("hwmon"))
        .find_map(|entry| {
            let text = std::fs::read_to_string(entry.path().join("temp1_input")).ok()?;
            let millidegrees = text.trim().parse::<f64>().ok()?;
            Some(millidegrees / 1000.0)
        })
}

/// Host, sensor, GPU and disk sampling at the rates each source affords.
pub struct Telemetry {
    host: HostSampler,
    sensors: LiveSensorSampler,
    cpu_temperature: Option<f64>,
    cpu_power: Option<f64>,
    last_sensor: Option<Instant>,
    pub system: SystemInfo,
    pub gpus: Vec<GpuDevice>,
    pub gpu_live: Vec<GpuTelemetry>,
    last_gpu: Option<Instant>,
    /// GPU samples taken off the screen's thread (step FC0), started with the first sample.
    gpu_feed: Option<GpuTelemetryFeed>,
    pub disks: Vec<DiskMeter>,
    pub live: LiveUtilization,
    pub mem_available_bytes: u64,
    /// Live throttle of the CPU, each tested GPU and each selected SSD since the session began.
    pub throttle: ThrottleLive,
    started: Instant,
    throttle_start: Option<Vec<ThrottleGroup>>,
    slowdowns: Vec<SlowdownCounter>,
    /// `SSD<alias>`, its namespace and its SMART when the session began.
    ssd_smart: Vec<(String, PathBuf, SmartSnapshot)>,
    last_smart: Option<Instant>,
    smart_feed: Option<SmartFeed>,
}

/// SMART of the watched SSDs, read on a thread of its own every [`SMART_PERIOD`]: an SSD that
/// stopped answering holds that thread, not the screen (step FC0).
struct SmartFeed {
    latest: Arc<Mutex<Vec<Option<SmartSnapshot>>>>,
    stop: Arc<AtomicBool>,
}

impl SmartFeed {
    fn start(nodes: Vec<PathBuf>) -> Self {
        let latest = Arc::new(Mutex::new(vec![None; nodes.len()]));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, flag) = (Arc::clone(&latest), Arc::clone(&stop));
        let _ = std::thread::Builder::new()
            .name("ssd-smart".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    let next = Instant::now() + SMART_PERIOD;
                    while Instant::now() < next && !flag.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    for (index, node) in nodes.iter().enumerate() {
                        let snapshot = nvme_inventory::read_smart(node);
                        if let Ok(mut latest) = shared.lock() {
                            latest[index] = Some(snapshot);
                        }
                    }
                }
            });
        Self { latest, stop }
    }

    fn snapshots(&self) -> Vec<Option<SmartSnapshot>> {
        self.latest.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl Drop for SmartFeed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// SMART is an admin command: read it rarely beside the SSD layer's I/O.
const SMART_PERIOD: Duration = Duration::from_secs(30);

impl Telemetry {
    #[must_use]
    pub fn new(disks: Vec<DiskMeter>) -> Self {
        let host = HostSampler::detect();
        let gpus = collect_gpu_inventory();
        let mut system = host.system_info();
        system.gpu = dit_system::format_gpu_summary(&gpus);
        Self {
            host,
            sensors: LiveSensorSampler::default(),
            cpu_temperature: None,
            cpu_power: None,
            last_sensor: None,
            system,
            gpus,
            gpu_live: Vec::new(),
            last_gpu: None,
            gpu_feed: None,
            disks,
            live: LiveUtilization::default(),
            mem_available_bytes: 0,
            throttle: ThrottleLive::default(),
            started: Instant::now(),
            throttle_start: None,
            slowdowns: Vec::new(),
            ssd_smart: Vec::new(),
            last_smart: None,
            smart_feed: None,
        }
    }

    /// Watches the selected SSDs' own throttling: `(alias, block name)` of each.
    pub fn watch_ssds(&mut self, drives: &[(usize, String)]) {
        self.ssd_smart = drives
            .iter()
            .map(|(alias, block)| {
                let node = Path::new("/dev").join(block);
                let start = nvme_inventory::read_smart(&node);
                (format!("SSD{alias}"), node, start)
            })
            .collect();
    }

    /// Keeps only the GPUs the run tests (the engine's eligible devices).
    pub fn retain_gpus(&mut self, pci_addresses: &[String]) {
        self.gpus
            .retain(|gpu| pci_addresses.iter().any(|pci| pci == &gpu.pci_address));
        self.system.gpu = dit_system::format_gpu_summary(&self.gpus);
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn sample(&mut self) {
        if self
            .last_sensor
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            let snapshot = self.sensors.sample();
            self.cpu_temperature = snapshot
                .max_temperature_millicelsius()
                .map(|value| value as f64 / 1000.0);
            self.cpu_power = snapshot
                .preferred_cpu_power()
                .map(|reading| reading.milliwatts as f64 / 1000.0);
            self.last_sensor = Some(Instant::now());
            let start = self
                .throttle_start
                .get_or_insert_with(|| snapshot.throttle_groups.clone());
            let cpu = dit_system::throttle_delta(start, &snapshot.throttle_groups)
                .map(|delta| (delta.events, delta.longest_ms.map(|ms| ms / 1000)));
            let clock = format_duration(self.started.elapsed().as_secs());
            self.throttle.update("CPU", cpu, &clock);
        }
        // nvidia-smi is the slowest source: sampled every 2 s on its own thread.
        if self
            .last_gpu
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(2))
        {
            let feed = self.gpu_feed.get_or_insert_with(|| {
                GpuTelemetryFeed::start(self.gpus.clone(), Duration::from_secs(2))
            });
            self.gpu_live = feed.samples();
            self.last_gpu = Some(Instant::now());
            self.track_gpu_throttle();
        }
        if !self.ssd_smart.is_empty()
            && self
                .last_smart
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(5))
        {
            let feed = self.smart_feed.get_or_insert_with(|| {
                SmartFeed::start(
                    self.ssd_smart
                        .iter()
                        .map(|(_, node, _)| node.clone())
                        .collect(),
                )
            });
            let clock = format_duration(self.started.elapsed().as_secs());
            for ((label, _, start), now) in self.ssd_smart.iter().zip(feed.snapshots()) {
                if let Some(now) = now {
                    self.throttle.update(
                        label,
                        nvme_inventory::thermal_throttle_between(start, &now),
                        &clock,
                    );
                }
            }
            self.last_smart = Some(Instant::now());
        }
        let mut live = self.host.sample();
        live.cpu_temperature_celsius = self.cpu_temperature;
        live.cpu_power_watts = self.cpu_power;
        self.live = live;
        self.mem_available_bytes = mem_available().unwrap_or(0);
        let diskstats = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
        for disk in &mut self.disks {
            disk.sample(&diskstats);
        }
    }
}

impl Telemetry {
    /// Each tested GPU's slowdowns, as `GPU<n>` in the order the session numbers them.
    fn track_gpu_throttle(&mut self) {
        let now = Instant::now();
        let clock = format_duration(self.started.elapsed().as_secs());
        self.slowdowns
            .resize(self.gpus.len(), SlowdownCounter::default());
        for (position, gpu) in self.gpus.iter().enumerate() {
            let Some(sample) = self
                .gpu_live
                .iter()
                .find(|sample| sample.pci_address.eq_ignore_ascii_case(&gpu.pci_address))
            else {
                continue;
            };
            let counter = &mut self.slowdowns[position];
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let reading = sample.throttle_reasons_mask.map(|mask| {
                counter.observe(mask, now);
                (counter.episodes, Some(counter.seconds(now).round() as u64))
            });
            self.throttle
                .update(&format!("GPU{}", position + 1), reading, &clock);
        }
    }
}

#[must_use]
pub fn mem_available() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemAvailable:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        })
        .map(|kib| kib * 1024)
}

/// One layer's row under the progress bar.
pub struct LayerLine {
    pub label: &'static str,
    pub state: String,
    pub detail: String,
    pub failures: u64,
}

/// What the run loop knows about the session, for one frame.
pub struct SessionView {
    pub level: Level,
    pub intensity: Intensity,
    pub gpu_temp_limit: bool,
    pub state: String,
    pub phase_title: String,
    pub phase_process: String,
    pub elapsed_seconds: u64,
    pub planned_seconds: u64,
    pub phases_row: String,
    pub plan_rows: Vec<String>,
    pub layers: Vec<LayerLine>,
    pub calculations: u64,
    pub chain: Option<ChainMeter>,
    pub held_bytes: u64,
    /// `FAIL BY PART` and `HW EVENTS` lines of the fault ledger.
    pub fault_rows: Vec<String>,
    /// Once the run is over: the verdict and where the report went, shown under the progress.
    pub result_rows: Vec<String>,
    /// Replaces the running footer once the run is over.
    pub footer: Option<String>,
}

pub struct ChainMeter {
    pub read_gibps: f64,
    pub peak_gibps: f64,
    pub generations: u64,
    pub loads: u64,
}

fn gpu_meters(telemetry: &Telemetry) -> Vec<DashboardMeter> {
    let mut meters = Vec::new();
    for (position, device) in telemetry.gpus.iter().enumerate() {
        let live = telemetry
            .gpu_live
            .iter()
            .find(|sample| sample.pci_address == device.pci_address);
        let label = format!("GPU {}", position + 1);
        meters.push(
            DashboardMeter::new(
                format!("{label} USAGE"),
                live.and_then(|l| l.utilization_percent),
            )
            .detail(
                // A driver that stopped answering is said, not shown as frozen numbers.
                match dit_system::nvidia_silent_seconds()
                    .filter(|_| device.driver.as_deref() == Some("nvidia"))
                {
                    Some(silent) => {
                        format!("nvidia-smi silent {silent} s (driver not answering)")
                    }
                    None => format!(
                        "Clock {} / Temp {} / Power {}",
                        optional(
                            live.and_then(|l| l.core_clock_mhz).map(|v| v / 1000.0),
                            "GHz",
                            2
                        ),
                        optional(live.and_then(|l| l.temperature_celsius), "C", 0),
                        optional(live.and_then(|l| l.power_watts), "W", 0)
                    ),
                },
            ),
        );
        let used = live.and_then(|l| l.vram_used_bytes);
        let total = live
            .and_then(|l| l.vram_total_bytes)
            .or(device.vram_total_bytes);
        meters.push(
            DashboardMeter::new(
                format!("{label} VRAM"),
                used.zip(total)
                    .and_then(|(used, total)| percent(used, total)),
            )
            .detail(format!(
                "Used {} / {} / Mem clock {}",
                used.map_or_else(|| "N/A".to_owned(), format_bytes),
                total.map_or_else(|| "N/A".to_owned(), format_bytes),
                optional(live.and_then(|l| l.memory_clock_mhz), "MHz", 0)
            )),
        );
    }
    meters
}

fn meters(view: &SessionView, telemetry: &Telemetry) -> Vec<DashboardMeter> {
    let live = &telemetry.live;
    let mut meters = vec![
        DashboardMeter::new("CPU USAGE", live.cpu_percent).detail(format!(
            "Freq {} / Temp {} / Power {}",
            optional(live.cpu_frequency_ghz, "GHz", 2),
            optional(live.cpu_temperature_celsius, "C", 0),
            optional(live.cpu_power_watts, "W", 0)
        )),
        DashboardMeter::new(
            "RAM USAGE",
            percent(live.memory_used_bytes, live.memory_total_bytes),
        )
        .detail(format!(
            "Used {} / {} / Tests hold {} / Available {}",
            format_bytes(live.memory_used_bytes),
            format_bytes(live.memory_total_bytes),
            format_bytes(view.held_bytes),
            format_bytes(telemetry.mem_available_bytes)
        )),
    ];
    meters.extend(gpu_meters(telemetry));
    for (position, disk) in telemetry.disks.iter().enumerate() {
        let speed = disk.read_bps + disk.write_bps;
        meters.push(
            DashboardMeter::new(
                format!("SSD {} SPEED", position + 1),
                (disk.peak_bps > 0.0).then(|| 100.0 * speed / disk.peak_bps),
            )
            .suffix(format!(
                "R {} W {}",
                format_rate(disk.read_bps),
                format_rate(disk.write_bps)
            ))
            .detail(format!(
                "{} / Temp {} / Peak {}",
                disk.label,
                optional(disk.temperature_celsius, "C", 0),
                format_rate(disk.peak_bps)
            )),
        );
    }
    if let Some(chain) = &view.chain {
        meters.push(
            DashboardMeter::new(
                "CHAIN READ",
                (chain.peak_gibps > 0.0).then(|| 100.0 * chain.read_gibps / chain.peak_gibps),
            )
            .suffix(format!("{:.2} GiB/s", chain.read_gibps))
            .detail(format!(
                "SSD -> RAM -> PCIe -> VRAM -> GPU -> CPU / G {} / loads {}",
                chain.generations, chain.loads
            )),
        );
    }
    meters
}

#[must_use]
pub fn render(
    view: &SessionView,
    telemetry: &Telemetry,
    size: TerminalSize,
    color: bool,
) -> String {
    let system = &telemetry.system;
    let mut hardware_rows = vec![
        format!("CPU:     {}", system.cpu),
        format!("THREADS: {} logical CPU(s)", system.logical_threads),
        format!("RAM:     {} total", format_bytes(system.memory_total_bytes)),
    ];
    for (position, gpu) in telemetry.gpus.iter().enumerate() {
        hardware_rows.push(format!(
            "GPU {}:   {} [{}]",
            position + 1,
            gpu.model,
            gpu.pci_address
        ));
    }
    for (position, disk) in telemetry.disks.iter().enumerate() {
        hardware_rows.push(format!("SSD {}:   {}", position + 1, disk.label));
    }
    hardware_rows.extend(view.plan_rows.iter().cloned());
    hardware_rows.push(format!("BOARD:   {}", system.board));
    hardware_rows.push(format!("KERNEL:  {}", system.kernel));
    let failures = view.layers.iter().map(|layer| layer.failures).sum();
    let mut running_rows: Vec<String> = view
        .layers
        .iter()
        .map(|layer| {
            format!(
                "{:<8} {:<18} FAIL {:<4} {}",
                layer.label, layer.state, layer.failures, layer.detail
            )
        })
        .collect();
    running_rows.extend(view.fault_rows.iter().cloned());
    running_rows.push(view.phases_row.clone());
    if !view.result_rows.is_empty() {
        running_rows.push(String::new());
        running_rows.extend(wrap_result_rows(&view.result_rows, size));
    }
    // A pass means the planned work is complete, whatever the last running estimate showed.
    let passed = view
        .result_rows
        .first()
        .is_some_and(|row| row.starts_with("RESULT: PASS"));
    #[allow(clippy::cast_precision_loss)]
    let progress = if passed {
        Some(100.0)
    } else {
        (view.planned_seconds > 0)
            .then(|| 100.0 * view.elapsed_seconds as f64 / view.planned_seconds as f64)
    };
    let dashboard = UnifiedDashboard {
        engine: "DIT-COMBINED-1".to_owned(),
        report_abi: dit_contracts::SCHEMA_VERSION.to_string(),
        profile: format!(
            "COMBINED / {} / {} | GPU TEMP LIMIT {}",
            view.level.label(),
            view.intensity.label(),
            if view.gpu_temp_limit { "ON 90 C" } else { "OFF" }
        ),
        hardware_rows,
        live_meters: meters(view, telemetry),
        state: view.state.clone(),
        progress_percent: progress,
        elapsed: format_duration(view.elapsed_seconds),
        target: format_duration(view.planned_seconds),
        active_test: view.phase_title.clone(),
        process: view.phase_process.clone(),
        calculations_completed: view.calculations,
        failures,
        running_rows,
        footer: view.footer.clone().unwrap_or_else(|| {
            "A layer FAIL is counted, the run continues | Ctrl+C: stop every layer and keep the reports".to_owned()
        }),
    };
    render_unified_dashboard(&dashboard, size, color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diskstats_rows_give_bytes() {
        let stats = " 259       0 nvme0n1 1000 0 2048 10 500 0 4096 20 0 0 0\n 259       1 nvme0n1p1 1 0 8 0 0 0 0 0 0 0 0\n";
        assert_eq!(sectors(stats, "nvme0n1"), Some((2048 * 512, 4096 * 512)));
        assert_eq!(sectors(stats, "nvme1n1"), None);
    }

    #[test]
    fn a_frame_has_every_meter_and_layer_row() {
        let mut telemetry = Telemetry::new(vec![DiskMeter::new(
            "Test SSD 1 TB (nvme0n1)".into(),
            "nvme0n1".into(),
        )]);
        telemetry.sample();
        let view = SessionView {
            level: Level::Bounded,
            intensity: Intensity::Balanced,
            gpu_temp_limit: true,
            state: "RUNNING".into(),
            phase_title: "PHASE 6/6 OVERLAP".into(),
            phase_process: "CPU-RAM + GPU-VRAM + SSD + CHAIN".into(),
            elapsed_seconds: 450,
            planned_seconds: 900,
            phases_row: "PHASES [OK] PREFILL [>>] OVERLAP".into(),
            plan_rows: vec!["PLAN:    CPU-RAM 7 thr / 20 GiB".into()],
            layers: vec![LayerLine {
                label: "CPU-RAM",
                state: "RUNNING".into(),
                detail: "round 2".into(),
                failures: 1,
            }],
            calculations: 42,
            chain: Some(ChainMeter {
                read_gibps: 0.3,
                peak_gibps: 0.4,
                generations: 4,
                loads: 10,
            }),
            held_bytes: 1 << 30,
            fault_rows: vec![
                "FAIL BY PART  CPU 0 | RAM 0 | ? PCIe-GPU1|GPU1|VRAM1 1".into(),
                "HW EVENTS     none recorded".into(),
            ],
            result_rows: Vec::new(),
            footer: None,
        };
        let frame = render(
            &view,
            &telemetry,
            TerminalSize {
                rows: 40,
                cols: 140,
            },
            false,
        );
        for needle in [
            "LIVE UTILIZATION",
            "CPU USAGE",
            "RAM USAGE",
            "SSD 1 SPEED",
            "CHAIN READ",
            "CPU-RAM",
            "FAIL TOTAL  1",
            "PHASE 6/6 OVERLAP",
            "COMBINED / BOUNDED / balanced",
            "FAIL BY PART",
            "HW EVENTS",
        ] {
            assert!(frame.contains(needle), "{needle} missing:\n{frame}");
        }
    }
}
