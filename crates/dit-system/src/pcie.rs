//! `PCIe` link evidence shared by every module (`docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md`,
//! step F2): link speed and width and AER counters by error type at **both ends** of a device's
//! link (the device and the port above it), and the link replay counter where the driver reports
//! one (NVIDIA through `nvidia-smi -q -d PCIE`, AMD through `pcie_replay_count`).
//!
//! A value that cannot be read is `None`, never zero; deltas are only formed from two readings.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const PCI_DEVICES: &str = "/sys/bus/pci/devices";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkSample {
    pub speed_gts: Option<f64>,
    pub width: Option<u32>,
    pub max_speed_gts: Option<f64>,
    pub max_width: Option<u32>,
}

/// AER counters of one severity file (`aer_dev_correctable` …): error type → count.
pub type AerCounts = BTreeMap<String, u64>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AerSample {
    pub correctable: Option<AerCounts>,
    pub nonfatal: Option<AerCounts>,
    pub fatal: Option<AerCounts>,
}

/// One reading of a device's link: its own end, the port above it, and the replay counter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkState {
    pub bdf: String,
    pub link: LinkSample,
    pub aer: AerSample,
    pub port: Option<String>,
    pub port_aer: Option<AerSample>,
    /// Replays since reset and replay-number rollovers.
    pub replay: Option<u64>,
    pub replay_rollover: Option<u64>,
}

fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_owned())
}

/// `8.0 GT/s PCIe` → 8.0.
#[must_use]
pub fn parse_speed(text: &str) -> Option<f64> {
    text.split_whitespace().next()?.parse().ok()
}

#[must_use]
pub fn read_link(device: &Path) -> LinkSample {
    let speed = |name: &str| read_text(&device.join(name)).and_then(|t| parse_speed(&t));
    let width = |name: &str| read_text(&device.join(name)).and_then(|t| t.parse().ok());
    LinkSample {
        speed_gts: speed("current_link_speed"),
        width: width("current_link_width"),
        max_speed_gts: speed("max_link_speed"),
        max_width: width("max_link_width"),
    }
}

/// Lines `Name value` of an `aer_dev_*` / `aer_rootport_*` file.
#[must_use]
pub fn parse_aer(text: &str) -> AerCounts {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let value = parts.next()?.parse().ok()?;
            Some((name.to_owned(), value))
        })
        .collect()
}

#[must_use]
pub fn read_aer(device: &Path) -> AerSample {
    let read = |file: &str| read_text(&device.join(file)).map(|t| parse_aer(&t));
    AerSample {
        correctable: read("aer_dev_correctable"),
        nonfatal: read("aer_dev_nonfatal"),
        fatal: read("aer_dev_fatal"),
    }
}

fn is_bdf(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 12 && bytes[4] == b':' && bytes[7] == b':' && bytes[10] == b'.'
}

/// The port directly above `device` (its parent in the sysfs tree when that is a PCI function).
#[must_use]
pub fn upstream_port(device: &Path) -> Option<PathBuf> {
    let parent = fs::canonicalize(device).ok()?.parent()?.to_path_buf();
    parent
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_bdf(name))
        .map(|_| parent.clone())
}

/// PCI function (`0000:01:00.0`) of an `NVMe` namespace such as `nvme0n1`.
#[must_use]
pub fn pci_of_block(block: &str) -> Option<String> {
    // /sys/block/nvme0n1/device -> …/nvme/nvme0, whose `device` link is the PCI function.
    let controller = fs::canonicalize(Path::new("/sys/block").join(block).join("device")).ok()?;
    let function = fs::canonicalize(controller.join("device")).ok()?;
    function
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_bdf(name))
        .map(str::to_owned)
}

/// `nvidia-smi -q -d PCIE`: bus ID → (replays since reset, replay-number rollovers).
#[must_use]
pub fn parse_nvidia_pcie(text: &str) -> BTreeMap<String, (Option<u64>, Option<u64>)> {
    let mut out: BTreeMap<String, (Option<u64>, Option<u64>)> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(bus) = trimmed.strip_prefix("GPU ") {
            current = normalize_bdf(bus);
            continue;
        }
        let Some(bdf) = &current else {
            continue;
        };
        let value = || {
            trimmed
                .split(':')
                .nth(1)
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if trimmed.starts_with("Replays Since Reset") {
            out.entry(bdf.clone()).or_default().0 = value();
        } else if trimmed.starts_with("Replay Number Rollovers") {
            out.entry(bdf.clone()).or_default().1 = value();
        }
    }
    out
}

/// `00000000:08:00.0` / `08:00.0` → `0000:08:00.0`.
#[must_use]
pub fn normalize_bdf(text: &str) -> Option<String> {
    let lower = text.trim().to_ascii_lowercase();
    let mut fields: Vec<&str> = lower.split(':').collect();
    if fields.len() == 2 {
        fields.insert(0, "0000");
    }
    if fields.len() != 3 || !fields[2].contains('.') {
        return None;
    }
    let domain = fields[0];
    let domain = &domain[domain.len().saturating_sub(4)..];
    Some(format!("{domain:0>4}:{}:{}", fields[1], fields[2]))
}

/// Replay counters of every NVIDIA GPU (empty without the driver).
#[must_use]
pub fn nvidia_replay_counters() -> BTreeMap<String, (Option<u64>, Option<u64>)> {
    Command::new("timeout")
        .args([
            "--signal=TERM",
            "--kill-after=2s",
            "10s",
            "nvidia-smi",
            "-q",
            "-d",
            "PCIE",
        ])
        .env("LC_ALL", "C")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| parse_nvidia_pcie(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_default()
}

/// Reads the link of PCI function `bdf` now. `nvidia` holds this sample's `nvidia-smi` replay
/// counters (one query serves every GPU).
#[must_use]
pub fn snapshot(bdf: &str, nvidia: &BTreeMap<String, (Option<u64>, Option<u64>)>) -> LinkState {
    let device = Path::new(PCI_DEVICES).join(bdf);
    let port = upstream_port(&device);
    let amd = read_text(&device.join("pcie_replay_count")).and_then(|t| t.parse().ok());
    let (replay, rollover) = nvidia.get(bdf).copied().unwrap_or((amd, None));
    LinkState {
        bdf: bdf.to_owned(),
        link: read_link(&device),
        aer: read_aer(&device),
        port: port
            .as_deref()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .map(str::to_owned),
        port_aer: port.as_deref().map(read_aer),
        replay,
        replay_rollover: rollover,
    }
}

fn aer_deltas(out: &mut BTreeMap<String, u64>, suffix: &str, start: &AerSample, end: &AerSample) {
    for (severity, a, b) in [
        ("aer_corrected", &start.correctable, &end.correctable),
        ("aer_nonfatal", &start.nonfatal, &end.nonfatal),
        ("aer_fatal", &start.fatal, &end.fatal),
    ] {
        let (Some(a), Some(b)) = (a, b) else {
            continue;
        };
        for (name, value) in b {
            let before = a.get(name).copied().unwrap_or(0);
            let delta = value.saturating_sub(before);
            if delta == 0 {
                continue;
            }
            let key = if name.starts_with("TOTAL_") {
                format!("{severity}{suffix}")
            } else {
                format!("{severity}:{name}{suffix}")
            };
            *out.entry(key).or_default() += delta;
        }
    }
}

/// What changed on the link between two readings: AER by severity and type at the device and at
/// its port (`@port`), replays, rollovers, and a width below the width at the first reading.
#[must_use]
pub fn delta(start: &LinkState, end: &LinkState) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    aer_deltas(&mut out, "", &start.aer, &end.aer);
    if let (Some(a), Some(b)) = (&start.port_aer, &end.port_aer) {
        aer_deltas(&mut out, "@port", a, b);
    }
    for (kind, a, b) in [
        ("replay", start.replay, end.replay),
        (
            "replay_rollover",
            start.replay_rollover,
            end.replay_rollover,
        ),
    ] {
        if let (Some(a), Some(b)) = (a, b)
            && b > a
        {
            out.insert(kind.to_owned(), b - a);
        }
    }
    if let (Some(a), Some(b)) = (start.link.width, end.link.width)
        && b < a
    {
        out.insert("link_width_drop".to_owned(), 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aer_files_parse_by_type() {
        let text = "RxErr 3\nBadTLP 0\nBadDLLP 1\nRollover 0\nTimeout 0\nNonFatalErr 0\nCorrIntErr 0\nHeaderOF 0\nTOTAL_ERR_COR 4\n";
        let counts = parse_aer(text);
        assert_eq!(counts["RxErr"], 3);
        assert_eq!(counts["TOTAL_ERR_COR"], 4);
    }

    #[test]
    fn nvidia_pcie_query_gives_replays_per_gpu() {
        let text = "\n==============NVSMI LOG==============\n\nAttached GPUs : 2\nGPU 00000000:08:00.0\n    PCI\n        Bus : 0x08\n        Replays Since Reset : 17\n        Replay Number Rollovers : 1\nGPU 00000000:0A:00.0\n    PCI\n        Replays Since Reset : 0\n        Replay Number Rollovers : 0\n";
        let parsed = parse_nvidia_pcie(text);
        assert_eq!(parsed["0000:08:00.0"], (Some(17), Some(1)));
        assert_eq!(parsed["0000:0a:00.0"], (Some(0), Some(0)));
    }

    #[test]
    fn deltas_cover_both_ends_replays_and_width() {
        let aer = |rx: u64, total: u64| AerSample {
            correctable: Some(parse_aer(&format!(
                "RxErr {rx}\nBadTLP 0\nTOTAL_ERR_COR {total}\n"
            ))),
            nonfatal: Some(AerCounts::new()),
            fatal: None,
        };
        let start = LinkState {
            bdf: "0000:01:00.0".into(),
            link: LinkSample {
                width: Some(16),
                ..LinkSample::default()
            },
            aer: aer(0, 0),
            port: Some("0000:00:01.1".into()),
            port_aer: Some(aer(1, 1)),
            replay: Some(10),
            replay_rollover: Some(0),
        };
        let end = LinkState {
            link: LinkSample {
                width: Some(8),
                ..LinkSample::default()
            },
            aer: aer(2, 2),
            port_aer: Some(aer(6, 6)),
            replay: Some(42),
            ..start.clone()
        };
        let d = delta(&start, &end);
        assert_eq!(d["aer_corrected:RxErr"], 2);
        assert_eq!(d["aer_corrected"], 2);
        assert_eq!(d["aer_corrected:RxErr@port"], 5);
        assert_eq!(d["replay"], 32);
        assert_eq!(d["link_width_drop"], 1);
        assert!(!d.contains_key("replay_rollover"));
        assert_eq!(
            normalize_bdf("00000000:0A:00.0").as_deref(),
            Some("0000:0a:00.0")
        );
    }
}
