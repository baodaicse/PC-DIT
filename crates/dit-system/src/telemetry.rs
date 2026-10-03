use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};

use crate::{GpuDevice, SystemDitError, SystemInventory, collect_gpu_inventory};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemperatureReading {
    pub source: String,
    pub label: String,
    pub millidegrees_c: i64,
    pub critical_millidegrees_c: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterReading {
    pub source: String,
    pub value: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerReading {
    pub source: String,
    pub label: String,
    pub milliwatts: u64,
    pub measurement: String,
    pub sample_interval_ms: Option<u64>,
}

#[allow(clippy::struct_excessive_bools)] // Availability is explicit evidence, not state logic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryAvailability {
    pub temperature_sensors: bool,
    pub cpu_power_sensors: bool,
    pub edac_counters: bool,
    pub throttle_counters: bool,
    pub cgroup_memory_events: bool,
    pub machine_check_configuration: bool,
    pub machine_check_event_log: bool,
    pub privileged_kernel_log: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub captured_unix_ms: u128,
    pub temperatures: Vec<TemperatureReading>,
    pub cpu_power: Vec<PowerReading>,
    pub edac_corrected: Vec<CounterReading>,
    pub edac_uncorrected: Vec<CounterReading>,
    pub throttle_core: Vec<CounterReading>,
    pub throttle_package: Vec<CounterReading>,
    /// The thermal-throttle counters once per core and once per package: every logical CPU of
    /// a core or package shows the same counter, so the per-CPU lists above repeat them.
    #[serde(default)]
    pub throttle_groups: Vec<ThrottleGroup>,
    pub cgroup_memory_events: BTreeMap<String, u64>,
    pub machine_check_cpu_count: u64,
    pub machine_check_bank_count: u64,
}

/// One core's or one package's thermal-throttle counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleGroup {
    /// `package<P>` or `package<P>-core<C>`.
    pub key: String,
    /// Times it started throttling since boot.
    pub count: u64,
    /// Time it spent throttled since boot, when the kernel reports it.
    pub total_time_ms: Option<u64>,
}

/// CPU thermal throttling during a run, each core and package counted once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThrottleDelta {
    /// Times a core or a package started throttling.
    pub events: u64,
    /// Longest time any one core or package spent throttled, when the kernel reports it.
    pub longest_ms: Option<u64>,
}

/// Throttling between two readings of the counters; `None` when the CPU has none (AMD CPUs, for
/// example: the counters come from the Intel thermal-throttle driver).
#[must_use]
pub fn throttle_delta(start: &[ThrottleGroup], end: &[ThrottleGroup]) -> Option<ThrottleDelta> {
    if end.is_empty() {
        return None;
    }
    let mut delta = ThrottleDelta::default();
    for group in end {
        let before = start.iter().find(|earlier| earlier.key == group.key);
        delta.events = delta.events.saturating_add(
            group
                .count
                .saturating_sub(before.map_or(group.count, |b| b.count)),
        );
        if let Some(total) = group.total_time_ms {
            let spent = total.saturating_sub(before.and_then(|b| b.total_time_ms).unwrap_or(total));
            delta.longest_ms = Some(delta.longest_ms.unwrap_or(0).max(spent));
        }
    }
    Some(delta)
}

impl TelemetrySnapshot {
    #[must_use]
    pub fn max_temperature_millicelsius(&self) -> Option<i64> {
        self.temperatures
            .iter()
            .filter(|reading| is_safety_temperature(reading))
            .map(|reading| reading.millidegrees_c)
            .max()
    }

    #[must_use]
    pub fn preferred_cpu_power(&self) -> Option<&PowerReading> {
        self.cpu_power
            .iter()
            .min_by_key(|reading| match reading.measurement.as_str() {
                "hwmon_input" => 0,
                "hwmon_average" => 1,
                "powercap_energy_delta" => 2,
                _ => 3,
            })
    }

    #[must_use]
    pub fn edac_corrected_total(&self) -> u64 {
        sum_counters(&self.edac_corrected)
    }

    #[must_use]
    pub fn edac_uncorrected_total(&self) -> u64 {
        sum_counters(&self.edac_uncorrected)
    }

    #[must_use]
    pub fn throttle_core_total(&self) -> u64 {
        sum_counters(&self.throttle_core)
    }

    #[must_use]
    pub fn throttle_package_total(&self) -> u64 {
        sum_counters(&self.throttle_package)
    }

    #[must_use]
    pub fn cgroup_event(&self, name: &str) -> u64 {
        self.cgroup_memory_events.get(name).copied().unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetryEvidence {
    pub availability: TelemetryAvailability,
    pub start: TelemetrySnapshot,
    pub end: TelemetrySnapshot,
    pub samples_collected: u64,
    pub peak_temperature_millicelsius: Option<i64>,
    pub edac_corrected_delta: u64,
    pub edac_uncorrected_delta: u64,
    pub throttle_core_delta: u64,
    pub throttle_package_delta: u64,
    /// Throttling with each core and package counted once; `None` without counters.
    #[serde(default)]
    pub throttle: Option<ThrottleDelta>,
    pub cgroup_oom_delta: u64,
    pub cgroup_oom_kill_delta: u64,
    pub kernel_events: KernelEventEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorReport {
    pub inventory: SystemInventory,
    pub gpus: Vec<GpuDevice>,
    pub telemetry: TelemetrySnapshot,
    pub availability: TelemetryAvailability,
    pub hwmon_chips: Vec<String>,
    pub powercap_zones: Vec<String>,
    pub loaded_modules: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KernelEventEvidence {
    pub available: bool,
    pub source: String,
    pub lines_scanned: u64,
    pub machine_check_events: u64,
    pub edac_ecc_events: u64,
    pub thermal_throttle_events: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TelemetryCollector {
    sys_root: String,
    cgroup_root: String,
    power_state: Arc<Mutex<BTreeMap<PathBuf, EnergySample>>>,
}

#[derive(Debug, Clone)]
struct EnergySample {
    energy_uj: u64,
    max_energy_range_uj: Option<u64>,
    captured: Instant,
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self {
            sys_root: "/sys".to_owned(),
            cgroup_root: "/sys/fs/cgroup".to_owned(),
            power_state: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

/// Lightweight reusable sampler for live UIs. Unlike `collect_sensor_report`,
/// this does not sleep or rebuild the complete system inventory on every tick.
#[derive(Debug, Clone, Default)]
pub struct LiveSensorSampler {
    collector: TelemetryCollector,
}

impl LiveSensorSampler {
    #[must_use]
    pub fn sample(&self) -> TelemetrySnapshot {
        self.collector.collect()
    }
}

impl TelemetryCollector {
    #[must_use]
    pub fn availability(snapshot: &TelemetrySnapshot) -> TelemetryAvailability {
        let machine_check_configuration = snapshot.machine_check_cpu_count > 0;
        let mut notes = Vec::new();
        if snapshot.temperatures.is_empty() {
            notes.push("temperature sensors are not exposed through sysfs".to_owned());
        }
        if snapshot.cpu_power.is_empty() {
            notes.push(
                "CPU package power is not exposed by hwmon/powercap or is unreadable".to_owned(),
            );
        }
        if snapshot.edac_corrected.is_empty() && snapshot.edac_uncorrected.is_empty() {
            notes.push("EDAC memory-controller counters are unavailable".to_owned());
        }
        if snapshot.throttle_core.is_empty() && snapshot.throttle_package.is_empty() {
            notes.push("CPU thermal-throttle counters are unavailable".to_owned());
        }
        TelemetryAvailability {
            temperature_sensors: !snapshot.temperatures.is_empty(),
            cpu_power_sensors: !snapshot.cpu_power.is_empty(),
            edac_counters: !snapshot.edac_corrected.is_empty()
                || !snapshot.edac_uncorrected.is_empty(),
            throttle_counters: !snapshot.throttle_core.is_empty()
                || !snapshot.throttle_package.is_empty(),
            cgroup_memory_events: !snapshot.cgroup_memory_events.is_empty(),
            machine_check_configuration,
            machine_check_event_log: false,
            privileged_kernel_log: false,
            notes,
        }
    }

    #[must_use]
    pub fn collect(&self) -> TelemetrySnapshot {
        let sys_root = Path::new(&self.sys_root);
        TelemetrySnapshot {
            captured_unix_ms: unix_ms(),
            temperatures: collect_temperatures(sys_root),
            cpu_power: collect_cpu_power(sys_root, &self.power_state),
            edac_corrected: collect_edac(sys_root, "ce_count"),
            edac_uncorrected: collect_edac(sys_root, "ue_count"),
            throttle_core: collect_throttle(sys_root, "core_throttle_count"),
            throttle_package: collect_throttle(sys_root, "package_throttle_count"),
            throttle_groups: collect_throttle_groups(sys_root),
            cgroup_memory_events: collect_memory_events(Path::new(&self.cgroup_root)),
            machine_check_cpu_count: count_machine_check_cpus(sys_root),
            machine_check_bank_count: count_machine_check_banks(sys_root),
        }
    }

    #[must_use]
    pub fn collect_kernel_events_since(start_unix_ms: u128) -> KernelEventEvidence {
        let since = format!("@{}.{:03}", start_unix_ms / 1000, start_unix_ms % 1000);
        let output = Command::new("journalctl")
            .args(["-k", "--no-pager", "-o", "cat", "--since", &since])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                parse_kernel_events(&String::from_utf8_lossy(&output.stdout))
            }
            Ok(output) => KernelEventEvidence {
                source: "journalctl:k".to_owned(),
                error: Some(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
                ..KernelEventEvidence::default()
            },
            Err(error) => KernelEventEvidence {
                source: "journalctl:k".to_owned(),
                error: Some(error.to_string()),
                ..KernelEventEvidence::default()
            },
        }
    }
}

/// Collect a two-sample sensor report so energy counters can be converted to power.
///
/// # Errors
///
/// Returns an error when the required CPU or memory inventory cannot be collected.
pub fn collect_sensor_report(sample_interval: Duration) -> Result<SensorReport, SystemDitError> {
    let collector = TelemetryCollector::default();
    let _baseline = collector.collect();
    if !sample_interval.is_zero() {
        thread::sleep(sample_interval);
    }
    let telemetry = collector.collect();
    let availability = TelemetryCollector::availability(&telemetry);
    let sys_root = Path::new("/sys");
    Ok(SensorReport {
        inventory: SystemInventory::collect()?,
        gpus: collect_gpu_inventory(),
        telemetry,
        availability,
        hwmon_chips: collect_hwmon_chips(&sys_root.join("class/hwmon")),
        powercap_zones: collect_powercap_zone_names(&sys_root.join("class/powercap")),
        loaded_modules: collect_loaded_modules(Path::new("/proc/modules")),
    })
}

#[must_use]
pub(crate) fn build_evidence(
    start: TelemetrySnapshot,
    end: TelemetrySnapshot,
    samples_collected: u64,
    peak_temperature_millicelsius: Option<i64>,
    kernel_events: KernelEventEvidence,
) -> TelemetryEvidence {
    let mut availability = TelemetryCollector::availability(&start);
    availability.cpu_power_sensors = !start.cpu_power.is_empty() || !end.cpu_power.is_empty();
    if availability.cpu_power_sensors {
        availability
            .notes
            .retain(|note| !note.starts_with("CPU package power"));
    }
    availability.privileged_kernel_log = kernel_events.available;
    if !kernel_events.available {
        availability
            .notes
            .push("kernel journal is unavailable; MCE/RAS event log was not collected".to_owned());
    }
    availability.machine_check_event_log = kernel_events.available;
    TelemetryEvidence {
        edac_corrected_delta: end
            .edac_corrected_total()
            .saturating_sub(start.edac_corrected_total()),
        edac_uncorrected_delta: end
            .edac_uncorrected_total()
            .saturating_sub(start.edac_uncorrected_total()),
        throttle_core_delta: end
            .throttle_core_total()
            .saturating_sub(start.throttle_core_total()),
        throttle_package_delta: end
            .throttle_package_total()
            .saturating_sub(start.throttle_package_total()),
        throttle: throttle_delta(&start.throttle_groups, &end.throttle_groups),
        cgroup_oom_delta: end
            .cgroup_event("oom")
            .saturating_sub(start.cgroup_event("oom")),
        cgroup_oom_kill_delta: end
            .cgroup_event("oom_kill")
            .saturating_sub(start.cgroup_event("oom_kill")),
        availability,
        start,
        end,
        samples_collected,
        peak_temperature_millicelsius,
        kernel_events,
    }
}

fn parse_kernel_events(text: &str) -> KernelEventEvidence {
    let mut evidence = KernelEventEvidence {
        available: true,
        source: "journalctl:k".to_owned(),
        ..KernelEventEvidence::default()
    };
    for line in text.lines() {
        evidence.lines_scanned = evidence.lines_scanned.saturating_add(1);
        let line = line.to_ascii_lowercase();
        if line.contains("machine check")
            || line.contains("hardware error")
            || line.contains("mce:")
        {
            evidence.machine_check_events = evidence.machine_check_events.saturating_add(1);
        }
        if line.contains("edac")
            || line.contains("ecc error")
            || line.contains("corrected error")
            || line.contains("uncorrected error")
        {
            evidence.edac_ecc_events = evidence.edac_ecc_events.saturating_add(1);
        }
        if line.contains("thermal") || line.contains("throttl") {
            evidence.thermal_throttle_events = evidence.thermal_throttle_events.saturating_add(1);
        }
    }
    evidence
}

fn collect_temperatures(sys_root: &Path) -> Vec<TemperatureReading> {
    let mut readings = collect_hwmon_temperatures(&sys_root.join("class/hwmon"));
    readings.extend(collect_thermal_zone_temperatures(
        &sys_root.join("class/thermal"),
    ));
    readings.sort_by(|left, right| left.source.cmp(&right.source));
    readings
}

fn collect_cpu_power(
    sys_root: &Path,
    power_state: &Arc<Mutex<BTreeMap<PathBuf, EnergySample>>>,
) -> Vec<PowerReading> {
    let mut readings = collect_hwmon_power(&sys_root.join("class/hwmon"));
    readings.extend(collect_powercap_power(
        &sys_root.join("class/powercap"),
        power_state,
    ));
    readings.sort_by(|left, right| {
        left.measurement
            .cmp(&right.measurement)
            .then_with(|| left.source.cmp(&right.source))
    });
    readings
}

fn collect_hwmon_power(root: &Path) -> Vec<PowerReading> {
    let mut readings = Vec::new();
    for hwmon in child_directories(root) {
        let Some(chip) = read_trimmed(hwmon.join("name")) else {
            continue;
        };
        if !is_cpu_power_chip(&chip) {
            continue;
        }
        let Ok(files) = fs::read_dir(&hwmon) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let (index, measurement) = if let Some(index) = name
                .strip_prefix("power")
                .and_then(|value| value.strip_suffix("_input"))
            {
                (index, "hwmon_input")
            } else if let Some(index) = name
                .strip_prefix("power")
                .and_then(|value| value.strip_suffix("_average"))
            {
                (index, "hwmon_average")
            } else {
                continue;
            };
            let Some(microwatts) =
                read_u64(file.path()).filter(|value| *value > 0 && *value <= 2_000_000_000)
            else {
                continue;
            };
            let label = read_trimmed(hwmon.join(format!("power{index}_label")))
                .unwrap_or_else(|| format!("power{index}"));
            readings.push(PowerReading {
                source: file.path().display().to_string(),
                label: format!("{chip}:{label}"),
                milliwatts: microwatts / 1000,
                measurement: measurement.to_owned(),
                sample_interval_ms: None,
            });
        }
    }
    readings
}

fn collect_powercap_power(
    root: &Path,
    power_state: &Arc<Mutex<BTreeMap<PathBuf, EnergySample>>>,
) -> Vec<PowerReading> {
    let mut readings = Vec::new();
    let mut seen = BTreeSet::new();
    for zone in powercap_zone_directories(root) {
        let energy_path = zone.join("energy_uj");
        let Ok(canonical_path) = fs::canonicalize(&energy_path) else {
            continue;
        };
        if !seen.insert(canonical_path.clone()) {
            continue;
        }
        let Some(label) = read_trimmed(zone.join("name")) else {
            continue;
        };
        if !is_cpu_package_zone(&label) {
            continue;
        }
        let Some(energy_uj) = read_u64(&energy_path) else {
            continue;
        };
        let max_energy_range_uj = read_u64(zone.join("max_energy_range_uj"));
        let current = EnergySample {
            energy_uj,
            max_energy_range_uj,
            captured: Instant::now(),
        };
        let Ok(mut state) = power_state.lock() else {
            continue;
        };
        let previous = state.insert(canonical_path, current.clone());
        drop(state);
        let Some(previous) = previous else {
            continue;
        };
        let elapsed_ns = current
            .captured
            .duration_since(previous.captured)
            .as_nanos();
        if elapsed_ns == 0 {
            continue;
        }
        let range = current.max_energy_range_uj.or(previous.max_energy_range_uj);
        let Some(delta_energy_uj) = energy_delta(previous.energy_uj, energy_uj, range) else {
            continue;
        };
        let milliwatts = u128::from(delta_energy_uj)
            .saturating_mul(1_000_000)
            .checked_div(elapsed_ns)
            .and_then(|value| u64::try_from(value).ok());
        let Some(milliwatts) = milliwatts.filter(|value| *value <= 2_000_000) else {
            continue;
        };
        readings.push(PowerReading {
            source: energy_path.display().to_string(),
            label,
            milliwatts,
            measurement: "powercap_energy_delta".to_owned(),
            sample_interval_ms: Some(
                u64::try_from(
                    current
                        .captured
                        .duration_since(previous.captured)
                        .as_millis(),
                )
                .unwrap_or(u64::MAX),
            ),
        });
    }
    readings
}

fn energy_delta(previous: u64, current: u64, max_range: Option<u64>) -> Option<u64> {
    if current >= previous {
        return Some(current - previous);
    }
    let range = max_range?;
    (range >= previous).then(|| range - previous + current)
}

fn powercap_zone_directories(root: &Path) -> Vec<PathBuf> {
    let mut zones = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0_u8)];
    while let Some((directory, depth)) = pending.pop() {
        if directory.join("energy_uj").is_file() && directory.join("name").is_file() {
            zones.push(directory.clone());
        }
        if depth >= 4 {
            continue;
        }
        pending.extend(
            child_directories(&directory)
                .into_iter()
                .map(|child| (child, depth + 1)),
        );
    }
    zones
}

fn collect_hwmon_chips(root: &Path) -> Vec<String> {
    let mut chips: Vec<String> = child_directories(root)
        .into_iter()
        .filter_map(|hwmon| read_trimmed(hwmon.join("name")))
        .collect();
    chips.sort();
    chips.dedup();
    chips
}

fn collect_powercap_zone_names(root: &Path) -> Vec<String> {
    let mut zones: Vec<String> = powercap_zone_directories(root)
        .into_iter()
        .filter_map(|zone| read_trimmed(zone.join("name")))
        .collect();
    zones.sort();
    zones.dedup();
    zones
}

fn collect_loaded_modules(path: &Path) -> Vec<String> {
    let mut modules: Vec<String> = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.split_whitespace().next().map(str::to_owned))
        .collect();
    modules.sort();
    modules.dedup();
    modules
}

fn is_cpu_power_chip(chip: &str) -> bool {
    matches!(
        chip,
        "k10temp"
            | "k8temp"
            | "zenpower"
            | "coretemp"
            | "cpu_thermal"
            | "x86_pkg_temp"
            | "soc_thermal"
            | "fam15h_power"
            | "amd_energy"
    )
}

fn is_cpu_package_zone(label: &str) -> bool {
    let label = label.to_ascii_lowercase();
    label == "package" || label.starts_with("package-") || label.starts_with("pkg")
}

fn collect_hwmon_temperatures(root: &Path) -> Vec<TemperatureReading> {
    let mut readings = Vec::new();
    for hwmon in child_directories(root) {
        let chip = read_trimmed(hwmon.join("name")).unwrap_or_else(|| "hwmon".to_owned());
        let Ok(files) = fs::read_dir(&hwmon) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(index) = name
                .strip_prefix("temp")
                .and_then(|value| value.strip_suffix("_input"))
            else {
                continue;
            };
            let Some(value) = read_i64(file.path()).filter(|value| sane_temperature(*value)) else {
                continue;
            };
            let label = read_trimmed(hwmon.join(format!("temp{index}_label")))
                .unwrap_or_else(|| format!("temp{index}"));
            let critical_millidegrees_c = read_i64(hwmon.join(format!("temp{index}_crit")))
                .filter(|value| sane_temperature(*value));
            readings.push(TemperatureReading {
                source: file.path().display().to_string(),
                label: format!("{chip}:{label}"),
                millidegrees_c: value,
                critical_millidegrees_c,
            });
        }
    }
    readings
}

fn collect_thermal_zone_temperatures(root: &Path) -> Vec<TemperatureReading> {
    child_directories(root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("thermal_zone"))
        })
        .filter_map(|zone| {
            let value = read_i64(zone.join("temp")).filter(|value| sane_temperature(*value))?;
            Some(TemperatureReading {
                source: zone.join("temp").display().to_string(),
                label: read_trimmed(zone.join("type")).unwrap_or_else(|| "thermal_zone".to_owned()),
                millidegrees_c: value,
                critical_millidegrees_c: collect_zone_critical(&zone),
            })
        })
        .collect()
}

fn collect_zone_critical(zone: &Path) -> Option<i64> {
    let files = fs::read_dir(zone).ok()?;
    files
        .flatten()
        .filter_map(|file| {
            let name = file.file_name();
            let name = name.to_str()?;
            (name.starts_with("trip_point_") && name.ends_with("_temp"))
                .then(|| read_i64(file.path()))
                .flatten()
        })
        .filter(|value| sane_temperature(*value))
        .min()
}

fn collect_edac(sys_root: &Path, counter_name: &str) -> Vec<CounterReading> {
    let root = sys_root.join("devices/system/edac/mc");
    child_directories(&root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("mc"))
        })
        .filter_map(|controller| {
            let path = controller.join(counter_name);
            Some(CounterReading {
                source: path.display().to_string(),
                value: read_u64(path)?,
            })
        })
        .collect()
}

fn collect_throttle(sys_root: &Path, counter_name: &str) -> Vec<CounterReading> {
    let root = sys_root.join("devices/system/cpu");
    child_directories(&root)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.strip_prefix("cpu")
                        .is_some_and(|id| id.parse::<u32>().is_ok())
                })
        })
        .filter_map(|cpu| {
            let path = cpu.join("thermal_throttle").join(counter_name);
            Some(CounterReading {
                source: path.display().to_string(),
                value: read_u64(path)?,
            })
        })
        .collect()
}

/// [`ThrottleGroup`]s: each logical CPU's counters filed under its core and its package
/// (`topology/core_id`, `topology/physical_package_id`), so siblings count once.
fn collect_throttle_groups(sys_root: &Path) -> Vec<ThrottleGroup> {
    let mut groups: BTreeMap<String, ThrottleGroup> = BTreeMap::new();
    for cpu in child_directories(&sys_root.join("devices/system/cpu")) {
        let is_cpu = cpu
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("cpu"))
            .is_some_and(|id| id.parse::<u32>().is_ok());
        let counters = cpu.join("thermal_throttle");
        if !is_cpu || !counters.is_dir() {
            continue;
        }
        let package = read_u64(cpu.join("topology/physical_package_id")).unwrap_or(0);
        let core = read_u64(cpu.join("topology/core_id"));
        for (scope, key) in [
            (
                "core",
                core.map(|core| format!("package{package}-core{core}")),
            ),
            ("package", Some(format!("package{package}"))),
        ] {
            let (Some(key), Some(count)) = (
                key,
                read_u64(counters.join(format!("{scope}_throttle_count"))),
            ) else {
                continue;
            };
            let time = read_u64(counters.join(format!("{scope}_throttle_total_time_ms")));
            let group = groups.entry(key.clone()).or_insert(ThrottleGroup {
                key,
                count: 0,
                total_time_ms: None,
            });
            group.count = group.count.max(count);
            group.total_time_ms = match (group.total_time_ms, time) {
                (Some(known), Some(read)) => Some(known.max(read)),
                (known, read) => known.or(read),
            };
        }
    }
    groups.into_values().collect()
}

fn collect_memory_events(cgroup_root: &Path) -> BTreeMap<String, u64> {
    let Some(text) = read_trimmed(cgroup_root.join("memory.events")) else {
        return BTreeMap::new();
    };
    parse_key_value_counters(&text)
}

fn count_machine_check_cpus(sys_root: &Path) -> u64 {
    u64::try_from(
        child_directories(&sys_root.join("devices/system/machinecheck"))
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("machinecheck"))
            })
            .count(),
    )
    .unwrap_or(u64::MAX)
}

fn count_machine_check_banks(sys_root: &Path) -> u64 {
    child_directories(&sys_root.join("devices/system/machinecheck"))
        .into_iter()
        .map(|cpu| {
            fs::read_dir(cpu).map_or(0, |entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry.file_name().to_str().is_some_and(|name| {
                            name.strip_prefix("bank")
                                .is_some_and(|id| id.parse::<u32>().is_ok())
                        })
                    })
                    .count()
            })
        })
        .map(|count| u64::try_from(count).unwrap_or(u64::MAX))
        .sum()
}

fn child_directories(root: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

fn parse_key_value_counters(text: &str) -> BTreeMap<String, u64> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.to_owned(), fields.next()?.parse().ok()?))
        })
        .collect()
}

fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

fn read_i64(path: impl AsRef<Path>) -> Option<i64> {
    read_trimmed(path)?.parse().ok()
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

const fn sane_temperature(value: i64) -> bool {
    value >= -100_000 && value <= 300_000
}

pub(crate) fn is_safety_temperature(reading: &TemperatureReading) -> bool {
    if reading.source == "synthetic:fault_injection" {
        return true;
    }
    let label = reading.label.to_ascii_lowercase();
    label.starts_with("coretemp:")
        || label.starts_with("k10temp:")
        || label.starts_with("k8temp:")
        || label.starts_with("zenpower:")
        || label.starts_with("cpu_thermal:")
        || label.starts_with("soc_thermal:")
        || matches!(
            label.as_str(),
            "x86_pkg_temp" | "cpu-thermal" | "cpu_thermal" | "soc_thermal"
        )
}

fn sum_counters(readings: &[CounterReading]) -> u64 {
    readings
        .iter()
        .fold(0_u64, |total, reading| total.saturating_add(reading.value))
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::{
        CounterReading, TelemetrySnapshot, TemperatureReading, ThrottleDelta,
        collect_throttle_groups, energy_delta, parse_kernel_events, parse_key_value_counters,
        sane_temperature, throttle_delta,
    };
    use std::fs;

    #[test]
    fn parses_cgroup_memory_events() {
        let events = parse_key_value_counters("low 2\noom 3\noom_kill 1\n");
        assert_eq!(events.get("oom"), Some(&3));
        assert_eq!(events.get("oom_kill"), Some(&1));
    }

    /// Every logical CPU of a core or package shows the same counter: each core and package is
    /// counted once, and the longest throttled time is the one reported.
    #[test]
    fn throttle_counts_each_core_and_package_once() {
        let root = std::env::temp_dir().join(format!("dit-throttle-{}", std::process::id()));
        let write = |cpu: u32, core: u64, core_count: u64, package_count: u64, package_ms: u64| {
            let dir = root.join(format!("devices/system/cpu/cpu{cpu}"));
            fs::create_dir_all(dir.join("thermal_throttle")).unwrap();
            fs::create_dir_all(dir.join("topology")).unwrap();
            fs::write(dir.join("topology/physical_package_id"), "0\n").unwrap();
            fs::write(dir.join("topology/core_id"), format!("{core}\n")).unwrap();
            let counters = dir.join("thermal_throttle");
            fs::write(
                counters.join("core_throttle_count"),
                format!("{core_count}\n"),
            )
            .unwrap();
            fs::write(
                counters.join("package_throttle_count"),
                format!("{package_count}\n"),
            )
            .unwrap();
            fs::write(
                counters.join("package_throttle_total_time_ms"),
                format!("{package_ms}\n"),
            )
            .unwrap();
        };
        // Two cores with two threads each, one package.
        for (cpu, core) in [(0, 0), (1, 0), (2, 1), (3, 1)] {
            write(cpu, core, 5, 10, 1000);
        }
        let start = collect_throttle_groups(&root);
        assert_eq!(start.len(), 3);
        // Core 1 throttled twice more, the package once more for 1.5 s; every thread shows it.
        for (cpu, core, core_count) in [(0, 0, 5), (1, 0, 5), (2, 1, 7), (3, 1, 7)] {
            write(cpu, core, core_count, 11, 2500);
        }
        let end = collect_throttle_groups(&root);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            throttle_delta(&start, &end),
            Some(ThrottleDelta {
                events: 3,
                longest_ms: Some(1500)
            })
        );
        assert_eq!(throttle_delta(&start, &[]), None);
    }

    #[test]
    fn sums_edac_counters_without_overflow() {
        let snapshot = TelemetrySnapshot {
            edac_corrected: vec![
                CounterReading {
                    source: "mc0".to_owned(),
                    value: u64::MAX,
                },
                CounterReading {
                    source: "mc1".to_owned(),
                    value: 1,
                },
            ],
            ..TelemetrySnapshot::default()
        };
        assert_eq!(snapshot.edac_corrected_total(), u64::MAX);
    }

    #[test]
    fn rejects_implausible_temperature() {
        assert!(sane_temperature(95_000));
        assert!(!sane_temperature(400_000));
    }

    #[test]
    fn safety_temperature_excludes_unconnected_super_io_aux_channels() {
        let mut snapshot = TelemetrySnapshot::default();
        snapshot.temperatures.push(TemperatureReading {
            source: "/sys/class/hwmon/hwmon4/temp5_input".to_owned(),
            label: "nct6798:AUXTIN2".to_owned(),
            millidegrees_c: 127_000,
            critical_millidegrees_c: Some(125_000),
        });
        snapshot.temperatures.push(TemperatureReading {
            source: "/sys/class/hwmon/hwmon2/temp1_input".to_owned(),
            label: "coretemp:Package id 0".to_owned(),
            millidegrees_c: 84_000,
            critical_millidegrees_c: Some(100_000),
        });
        assert_eq!(snapshot.max_temperature_millicelsius(), Some(84_000));
    }

    #[test]
    fn calculates_energy_delta_with_counter_wrap() {
        assert_eq!(energy_delta(900, 50, Some(1000)), Some(150));
        assert_eq!(energy_delta(100, 250, Some(1000)), Some(150));
        assert_eq!(energy_delta(900, 50, None), None);
    }

    #[test]
    fn classifies_kernel_hardware_events() {
        let evidence = parse_kernel_events(
            "mce: machine check\nEDAC MC0: corrected error\nCPU thermal throttling\n",
        );
        assert_eq!(evidence.machine_check_events, 1);
        assert_eq!(evidence.edac_ecc_events, 1);
        assert_eq!(evidence.thermal_throttle_events, 1);
    }
}
