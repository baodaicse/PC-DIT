use std::{
    fs,
    path::Path,
    process::Command,
    sync::atomic::{AtomicU8, Ordering},
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub pci_address: String,
    pub model: String,
    pub driver: Option<String>,
    pub kernel_modules: Vec<String>,
    pub vram_total_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuTelemetry {
    pub pci_address: String,
    pub utilization_percent: Option<f64>,
    #[serde(default)]
    pub memory_utilization_percent: Option<f64>,
    pub vram_used_bytes: Option<u64>,
    pub vram_total_bytes: Option<u64>,
    pub temperature_celsius: Option<f64>,
    pub power_watts: Option<f64>,
    pub core_clock_mhz: Option<f64>,
    #[serde(default)]
    pub memory_clock_mhz: Option<f64>,
    /// Board power limit; `None` when the driver does not report it.
    #[serde(default)]
    pub power_limit_watts: Option<f64>,
    /// NVML clock-throttle bitmask; `None` means unavailable, not "no throttling".
    #[serde(default)]
    pub throttle_reasons_mask: Option<u64>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
struct NvidiaSample {
    pci_address: String,
    vram_total_bytes: Option<u64>,
    vram_used_bytes: Option<u64>,
    utilization_percent: Option<f64>,
    memory_utilization_percent: Option<f64>,
    temperature_celsius: Option<f64>,
    power_watts: Option<f64>,
    core_clock_mhz: Option<f64>,
    memory_clock_mhz: Option<f64>,
    power_limit_watts: Option<f64>,
    throttle_reasons_mask: Option<u64>,
}

#[derive(Debug, Default)]
struct SysfsSample {
    utilization_percent: Option<f64>,
    vram_used_bytes: Option<u64>,
    vram_total_bytes: Option<u64>,
    temperature_celsius: Option<f64>,
    power_watts: Option<f64>,
    core_clock_mhz: Option<f64>,
    memory_clock_mhz: Option<f64>,
}

#[must_use]
pub fn collect_gpu_inventory() -> Vec<GpuDevice> {
    let output = Command::new("lspci")
        .args(["-Dnnk"])
        .env("LC_ALL", "C")
        .output();
    let mut devices = output
        .ok()
        .filter(|output| output.status.success())
        .map_or_else(Vec::new, |output| {
            parse_lspci_gpus(&String::from_utf8_lossy(&output.stdout))
        });
    let nvidia = collect_nvidia_samples();
    for device in &mut devices {
        device.vram_total_bytes = read_vram_total(&device.pci_address);
        if device.vram_total_bytes.is_none() {
            device.vram_total_bytes = nvidia
                .iter()
                .find(|sample| sample.pci_address == device.pci_address)
                .and_then(|sample| sample.vram_total_bytes);
        }
        if device.driver.is_none() {
            device.driver = read_bound_driver(&device.pci_address);
        }
    }
    devices
}

#[must_use]
pub fn collect_gpu_telemetry(devices: &[GpuDevice]) -> Vec<GpuTelemetry> {
    let nvidia = collect_nvidia_samples();
    devices
        .iter()
        .map(|device| {
            let root = Path::new("/sys/bus/pci/devices").join(&device.pci_address);
            let sysfs = collect_sysfs_sample(&root);
            if let Some(sample) = nvidia
                .iter()
                .find(|sample| sample.pci_address == device.pci_address)
            {
                return GpuTelemetry {
                    pci_address: device.pci_address.clone(),
                    utilization_percent: sample.utilization_percent.or(sysfs.utilization_percent),
                    memory_utilization_percent: sample.memory_utilization_percent,
                    vram_used_bytes: sample.vram_used_bytes.or(sysfs.vram_used_bytes),
                    vram_total_bytes: sample
                        .vram_total_bytes
                        .or(sysfs.vram_total_bytes)
                        .or(device.vram_total_bytes),
                    temperature_celsius: sample.temperature_celsius.or(sysfs.temperature_celsius),
                    power_watts: sample.power_watts.or(sysfs.power_watts),
                    core_clock_mhz: sample.core_clock_mhz.or(sysfs.core_clock_mhz),
                    memory_clock_mhz: sample.memory_clock_mhz.or(sysfs.memory_clock_mhz),
                    power_limit_watts: sample.power_limit_watts,
                    throttle_reasons_mask: sample.throttle_reasons_mask,
                    source: "nvidia-smi + sysfs fallback".to_owned(),
                };
            }
            let has_live_metrics = sysfs.utilization_percent.is_some()
                || sysfs.vram_used_bytes.is_some()
                || sysfs.temperature_celsius.is_some()
                || sysfs.power_watts.is_some()
                || sysfs.core_clock_mhz.is_some()
                || sysfs.memory_clock_mhz.is_some();
            let driver = device.driver.as_deref().unwrap_or("unbound");
            let source = if has_live_metrics {
                format!("DRM/sysfs ({driver})")
            } else {
                format!("DRM/sysfs ({driver}; driver exposes no live counters)")
            };
            GpuTelemetry {
                pci_address: device.pci_address.clone(),
                utilization_percent: sysfs.utilization_percent,
                memory_utilization_percent: None,
                vram_used_bytes: sysfs.vram_used_bytes,
                vram_total_bytes: sysfs.vram_total_bytes.or(device.vram_total_bytes),
                temperature_celsius: sysfs.temperature_celsius,
                power_watts: sysfs.power_watts,
                core_clock_mhz: sysfs.core_clock_mhz,
                memory_clock_mhz: sysfs.memory_clock_mhz,
                power_limit_watts: None,
                throttle_reasons_mask: None,
                source,
            }
        })
        .collect()
}

#[must_use]
pub fn format_gpu_summary(devices: &[GpuDevice]) -> String {
    if devices.is_empty() {
        return "N/A (no PCI display controller)".to_owned();
    }
    devices
        .iter()
        .map(|device| {
            let driver = device.driver.as_deref().unwrap_or("UNBOUND");
            let vram = device.vram_total_bytes.map_or_else(
                || "VRAM N/A".to_owned(),
                |bytes| format!("VRAM {}", human_bytes(bytes)),
            );
            format!(
                "{} [{}; {}; {}]",
                device.model, device.pci_address, driver, vram
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

fn parse_lspci_gpus(text: &str) -> Vec<GpuDevice> {
    let mut devices = Vec::new();
    let mut current: Option<GpuDevice> = None;
    for line in text.lines() {
        if !line.starts_with(char::is_whitespace) {
            if let Some(device) = current.take() {
                devices.push(device);
            }
            if !(line.contains("[0300]") || line.contains("[0302]") || line.contains("[0380]")) {
                continue;
            }
            let Some((pci_address, description)) = line.split_once(' ') else {
                continue;
            };
            let model = description
                .split_once(": ")
                .map_or(description, |(_, model)| model)
                .trim()
                .to_owned();
            current = Some(GpuDevice {
                pci_address: pci_address.to_owned(),
                model,
                driver: None,
                kernel_modules: Vec::new(),
                vram_total_bytes: None,
            });
            continue;
        }
        let Some(device) = current.as_mut() else {
            continue;
        };
        let line = line.trim();
        if let Some(driver) = line.strip_prefix("Kernel driver in use:") {
            device.driver = Some(driver.trim().to_owned());
        } else if let Some(modules) = line.strip_prefix("Kernel modules:") {
            device.kernel_modules = modules
                .split(',')
                .map(str::trim)
                .filter(|module| !module.is_empty())
                .map(str::to_owned)
                .collect();
        }
    }
    if let Some(device) = current {
        devices.push(device);
    }
    devices
}

fn read_bound_driver(pci_address: &str) -> Option<String> {
    fs::read_link(
        Path::new("/sys/bus/pci/devices")
            .join(pci_address)
            .join("driver"),
    )
    .ok()?
    .file_name()?
    .to_str()
    .map(str::to_owned)
}

fn read_vram_total(pci_address: &str) -> Option<u64> {
    let device = Path::new("/sys/bus/pci/devices").join(pci_address);
    let direct = device.join("mem_info_vram_total");
    if let Some(value) = read_u64(direct) {
        return Some(value);
    }
    let drm = device.join("drm");
    fs::read_dir(drm).ok()?.flatten().find_map(|entry| {
        let name = entry.file_name();
        name.to_str()
            .is_some_and(|name| name.starts_with("card"))
            .then(|| read_u64(entry.path().join("device/mem_info_vram_total")))
            .flatten()
    })
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_f64(path: impl AsRef<Path>) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn collect_sysfs_sample(device: &Path) -> SysfsSample {
    let candidates = device_metric_roots(device);
    SysfsSample {
        utilization_percent: read_first_f64(&candidates, &["gpu_busy_percent", "gt_busy_percent"])
            .filter(|value| (0.0..=100.0).contains(value)),
        vram_used_bytes: read_first_u64(&candidates, &["mem_info_vram_used"]),
        vram_total_bytes: read_first_u64(&candidates, &["mem_info_vram_total"]),
        temperature_celsius: read_hwmon_value(&candidates, "temp", &["_input"])
            .map(|value| value / 1000.0)
            .filter(|value| (0.0..=200.0).contains(value)),
        power_watts: read_hwmon_value(&candidates, "power", &["_average", "_input"])
            .map(|value| value / 1_000_000.0)
            .filter(|value| (0.0..=2_000.0).contains(value)),
        core_clock_mhz: read_gpu_clock(&candidates, false),
        memory_clock_mhz: read_gpu_clock(&candidates, true),
    }
}

fn device_metric_roots(device: &Path) -> Vec<std::path::PathBuf> {
    let mut roots = vec![device.to_path_buf()];
    if let Ok(entries) = fs::read_dir(device.join("drm")) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|name| name.starts_with("card") && !name.contains('-'))
            {
                roots.push(entry.path());
                roots.push(entry.path().join("device"));
            }
        }
    }
    roots
}

fn read_first_f64(roots: &[std::path::PathBuf], names: &[&str]) -> Option<f64> {
    roots
        .iter()
        .find_map(|root| names.iter().find_map(|name| read_f64(root.join(name))))
}

fn read_first_u64(roots: &[std::path::PathBuf], names: &[&str]) -> Option<u64> {
    roots
        .iter()
        .find_map(|root| names.iter().find_map(|name| read_u64(root.join(name))))
}

fn read_hwmon_value(roots: &[std::path::PathBuf], prefix: &str, suffixes: &[&str]) -> Option<f64> {
    for root in roots {
        let Ok(hwmons) = fs::read_dir(root.join("hwmon")) else {
            continue;
        };
        for hwmon in hwmons.flatten() {
            let Ok(sensors) = fs::read_dir(hwmon.path()) else {
                continue;
            };
            for sensor in sensors.flatten() {
                let name = sensor.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                if name.starts_with(prefix)
                    && suffixes.iter().any(|suffix| name.ends_with(suffix))
                    && let Some(value) = read_f64(sensor.path())
                {
                    return Some(value);
                }
            }
        }
    }
    None
}

fn read_gpu_clock(roots: &[std::path::PathBuf], memory: bool) -> Option<f64> {
    let direct = if memory {
        &["mem_cur_freq_mhz", "memory_cur_freq_mhz"][..]
    } else {
        &[
            "gt_cur_freq_mhz",
            "rps_cur_freq_mhz",
            "gt/gt0/rps_cur_freq_mhz",
        ][..]
    };
    if let Some(value) = read_first_f64(roots, direct).filter(|value| *value > 0.0) {
        return Some(value);
    }
    let dpm = if memory { "pp_dpm_mclk" } else { "pp_dpm_sclk" };
    if let Some(value) = roots
        .iter()
        .find_map(|root| fs::read_to_string(root.join(dpm)).ok())
        .and_then(|text| parse_active_dpm_clock(&text))
    {
        return Some(value);
    }
    let hwmon_prefix = if memory { "freq2" } else { "freq1" };
    read_hwmon_value(roots, hwmon_prefix, &["_input"])
        .map(|hz| hz / 1_000_000.0)
        .filter(|mhz| (1.0..=20_000.0).contains(mhz))
}

fn parse_active_dpm_clock(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.contains('*'))?;
    line.split_whitespace().find_map(|field| {
        let normalized = field.trim_end_matches('*').trim();
        normalized
            .strip_suffix("Mhz")
            .or_else(|| normalized.strip_suffix("MHz"))
            .and_then(|value| value.parse::<f64>().ok())
    })
}

/// 0 = not yet probed, 1 = extended columns accepted, 2 = driver rejected them.
static EXTENDED_QUERY: AtomicU8 = AtomicU8::new(0);
const BASE_FIELDS: &str = "pci.bus_id,memory.total,memory.used,utilization.gpu,utilization.memory,temperature.gpu,power.draw,clocks.current.graphics,clocks.current.memory";
const EXTENDED_FIELDS: &str = "pci.bus_id,memory.total,memory.used,utilization.gpu,utilization.memory,temperature.gpu,power.draw,clocks.current.graphics,clocks.current.memory,power.limit,clocks_throttle_reasons.active";

fn run_nvidia_query(fields: &str) -> Option<String> {
    let nvidia_smi = if Path::new("/usr/bin/nvidia-smi").is_file() {
        "/usr/bin/nvidia-smi"
    } else {
        "nvidia-smi"
    };
    let query = [
        format!("--query-gpu={fields}"),
        "--format=csv,noheader,nounits".to_owned(),
    ];
    let output = Command::new(nvidia_smi)
        .args(&query)
        .env("LC_ALL", "C")
        .output()
        .or_else(|_| {
            Command::new("timeout")
                .args(["--signal=TERM", "--kill-after=1s", "2s", nvidia_smi])
                .args(&query)
                .env("LC_ALL", "C")
                .output()
        });
    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn collect_nvidia_samples() -> Vec<NvidiaSample> {
    // A driver that rejects a field fails the whole query, so probe once and fall
    // back to the base columns instead of losing all telemetry.
    if EXTENDED_QUERY.load(Ordering::Relaxed) != 2 {
        if let Some(text) = run_nvidia_query(EXTENDED_FIELDS) {
            EXTENDED_QUERY.store(1, Ordering::Relaxed);
            return parse_nvidia_samples(&text);
        }
        if EXTENDED_QUERY.load(Ordering::Relaxed) == 0 {
            EXTENDED_QUERY.store(2, Ordering::Relaxed);
        }
    }
    run_nvidia_query(BASE_FIELDS).map_or_else(Vec::new, |text| parse_nvidia_samples(&text))
}

fn parse_nvidia_samples(text: &str) -> Vec<NvidiaSample> {
    text.lines()
        .filter_map(|line| {
            let fields = line.split(',').map(str::trim).collect::<Vec<_>>();
            if fields.len() != 9 && fields.len() != 11 {
                return None;
            }
            Some(NvidiaSample {
                pci_address: normalize_pci_address(fields[0])?,
                vram_total_bytes: parse_mib(fields[1]),
                vram_used_bytes: parse_mib(fields[2]),
                utilization_percent: parse_metric(fields[3]),
                memory_utilization_percent: parse_metric(fields[4]),
                temperature_celsius: parse_metric(fields[5]),
                power_watts: parse_metric(fields[6]),
                core_clock_mhz: parse_metric(fields[7]),
                memory_clock_mhz: parse_metric(fields[8]),
                power_limit_watts: fields.get(9).and_then(|v| parse_metric(v)),
                throttle_reasons_mask: fields.get(10).and_then(|v| parse_hex_mask(v)),
            })
        })
        .collect()
}

fn parse_hex_mask(value: &str) -> Option<u64> {
    let digits = value.trim().strip_prefix("0x")?;
    u64::from_str_radix(digits, 16).ok()
}

fn normalize_pci_address(value: &str) -> Option<String> {
    let mut parts = value.trim().to_ascii_lowercase();
    let mut fields = parts.split(':').collect::<Vec<_>>();
    if fields.len() == 2 {
        fields.insert(0, "0000");
    }
    if fields.len() != 3 || !fields[2].contains('.') {
        return None;
    }
    let domain = fields[0];
    let domain = &domain[domain.len().saturating_sub(4)..];
    parts = format!("{domain:0>4}:{}:{}", fields[1], fields[2]);
    Some(parts)
}

fn parse_mib(value: &str) -> Option<u64> {
    parse_metric(value).and_then(|mib| {
        if mib.is_finite() && mib >= 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            Some((mib * 1024.0 * 1024.0).round() as u64)
        } else {
            None
        }
    })
}

fn parse_metric(value: &str) -> Option<f64> {
    value.parse::<f64>().ok().filter(|value| value.is_finite())
}

#[allow(clippy::cast_precision_loss)]
fn human_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.0} MiB", bytes as f64 / MIB as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_gpu_models_drivers_and_modules() {
        let text = "0000:00:02.0 VGA compatible controller [0300]: Intel Corporation Arc Graphics [8086:7d55]\n\tKernel driver in use: xe\n\tKernel modules: i915, xe\n0000:01:00.0 3D controller [0302]: NVIDIA Corporation Device [10de:2d04] (rev a1)\n\tKernel driver in use: nouveau\n\tKernel modules: nouveau, nvidia_drm, nvidia\n0000:02:00.0 Non-Volatile memory controller [0108]: Example NVMe\n\tKernel driver in use: nvme\n";
        let devices = parse_lspci_gpus(text);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].pci_address, "0000:00:02.0");
        assert_eq!(devices[0].driver.as_deref(), Some("xe"));
        assert_eq!(devices[0].kernel_modules, ["i915", "xe"]);
        assert_eq!(devices[1].driver.as_deref(), Some("nouveau"));
        assert!(format_gpu_summary(&devices).contains("Arc Graphics"));
    }

    #[test]
    fn empty_inventory_is_explicit() {
        assert_eq!(format_gpu_summary(&[]), "N/A (no PCI display controller)");
    }

    #[test]
    fn parses_nvidia_telemetry_and_normalizes_extended_domain() {
        let samples =
            parse_nvidia_samples("00000000:08:00.0, 2048, 672, 31, 26, 44, 18.61, 734, 405\n");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].pci_address, "0000:08:00.0");
        assert_eq!(samples[0].vram_total_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(samples[0].utilization_percent, Some(31.0));
        assert_eq!(samples[0].memory_utilization_percent, Some(26.0));
        assert_eq!(samples[0].memory_clock_mhz, Some(405.0));
    }

    #[test]
    fn parses_extended_columns_and_treats_missing_values_as_unavailable() {
        let line = "00000000:08:00.0, 2048, 672, 31, 26, 44, 18.61, 734, 405, 120.00, 0x0000000000000004\n";
        let sample = &parse_nvidia_samples(line)[0];
        assert_eq!(sample.power_limit_watts, Some(120.0));
        assert_eq!(sample.throttle_reasons_mask, Some(4));
        let na =
            "00000000:08:00.0, 2048, 672, 31, 26, 44, 18.61, 734, 405, [N/A], [Not Supported]\n";
        let sample = &parse_nvidia_samples(na)[0];
        assert_eq!(sample.power_limit_watts, None);
        assert_eq!(
            sample.throttle_reasons_mask, None,
            "unavailable must not read as 0"
        );
        assert!(parse_nvidia_samples("a, b, c\n").is_empty());
    }

    #[test]
    fn parses_active_amd_dpm_clock() {
        assert_eq!(
            parse_active_dpm_clock("0: 500Mhz\n1: 1750Mhz *\n"),
            Some(1750.0)
        );
    }

    #[test]
    fn sysfs_sample_reads_drm_usage_hwmon_temperature_power_and_dpm_clocks() {
        let root = std::env::temp_dir().join(format!(
            "dit-gpu-sysfs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let hwmon = root.join("hwmon/hwmon0");
        fs::create_dir_all(&hwmon).unwrap();
        fs::write(root.join("gpu_busy_percent"), "73\n").unwrap();
        fs::write(root.join("mem_info_vram_used"), "1048576\n").unwrap();
        fs::write(root.join("mem_info_vram_total"), "8388608\n").unwrap();
        fs::write(root.join("pp_dpm_sclk"), "0: 500Mhz\n1: 2200Mhz *\n").unwrap();
        fs::write(root.join("pp_dpm_mclk"), "0: 96Mhz\n1: 1000Mhz *\n").unwrap();
        fs::write(hwmon.join("temp1_input"), "61500\n").unwrap();
        fs::write(hwmon.join("power1_average"), "125500000\n").unwrap();

        let sample = collect_sysfs_sample(&root);
        assert_eq!(sample.utilization_percent, Some(73.0));
        assert_eq!(sample.vram_used_bytes, Some(1_048_576));
        assert_eq!(sample.temperature_celsius, Some(61.5));
        assert_eq!(sample.power_watts, Some(125.5));
        assert_eq!(sample.core_clock_mhz, Some(2200.0));
        assert_eq!(sample.memory_clock_mhz, Some(1000.0));
        fs::remove_dir_all(root).unwrap();
    }
}
