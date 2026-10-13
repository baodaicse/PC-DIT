use std::{collections::BTreeSet, fs, path::Path, time::SystemTime};

use serde::{Deserialize, Serialize};

use crate::SystemDitError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInventory {
    pub installed_bytes: u64,
    pub available_at_start_bytes: u64,
    pub free_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_free_bytes: u64,
    pub hardware_corrupted_bytes: u64,
    pub page_size_bytes: u64,
    pub cgroup_limit_bytes: Option<u64>,
    pub cgroup_current_bytes: Option<u64>,
    pub effective_available_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuThread {
    pub cpu_id: usize,
    pub package_id: i32,
    pub core_id: i32,
    pub numa_node: Option<u32>,
    pub allowed: bool,
    /// Performance or efficient core of a hybrid CPU (Intel P/E cores), uniform otherwise (and in
    /// reports written before v1.2.008).
    #[serde(default)]
    pub core_kind: CoreKind,
    /// 0 for the first hardware thread of its physical core, 1 for its SMT sibling.
    #[serde(default)]
    pub smt_rank: usize,
    /// The L3 cache this thread shares (`cache/index3/id`): one per CCD/CCX on AMD.
    #[serde(default)]
    pub l3_id: Option<u32>,
}

/// The kind of core a hardware thread runs on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoreKind {
    /// Every core of the CPU is of the same kind.
    #[default]
    Uniform,
    /// A performance core of a hybrid CPU (Intel P-core, `/sys/devices/cpu_core`).
    Performance,
    /// An efficient core of a hybrid CPU (Intel E-core, `/sys/devices/cpu_atom`).
    Efficient,
}

#[allow(clippy::struct_excessive_bools)] // Capability bits are independent evidence fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuInventory {
    pub architecture: String,
    pub vendor_id: String,
    pub model_name: String,
    pub logical_cpu_count: usize,
    pub allowed_cpu_ids: Vec<usize>,
    pub physical_core_count: usize,
    pub avx2: bool,
    pub aes: bool,
    pub fma: bool,
    pub sse4_2: bool,
    pub threads: Vec<CpuThread>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NumaNode {
    pub node_id: u32,
    pub cpu_ids: Vec<usize>,
    pub mem_total_bytes: Option<u64>,
    pub mem_free_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInventory {
    pub captured_unix_ms: u128,
    pub kernel_release: String,
    pub memory: MemoryInventory,
    pub cpu: CpuInventory,
    pub numa_nodes: Vec<NumaNode>,
}

impl SystemInventory {
    /// Collect Linux memory, CPU, cpuset and NUMA information.
    ///
    /// # Errors
    ///
    /// Returns an error when required `/proc` inventory cannot be read or parsed.
    pub fn collect() -> Result<Self, SystemDitError> {
        let memory = collect_memory()?;
        let numa_nodes = collect_numa_nodes();
        let cpu = collect_cpu(&numa_nodes)?;
        let captured_unix_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_millis());
        let kernel_release = read_trimmed("/proc/sys/kernel/osrelease")?;
        Ok(Self {
            captured_unix_ms,
            kernel_release,
            memory,
            cpu,
            numa_nodes,
        })
    }
}

fn collect_memory() -> Result<MemoryInventory, SystemDitError> {
    let meminfo = read_required("/proc/meminfo")?;
    let value = |key: &str| parse_meminfo_bytes(&meminfo, key).unwrap_or(0);
    let cgroup_limit_bytes = read_optional_limit(&[
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ]);
    let cgroup_current_bytes = read_optional_u64(&[
        "/sys/fs/cgroup/memory.current",
        "/sys/fs/cgroup/memory/memory.usage_in_bytes",
    ]);
    let available_at_start_bytes = value("MemAvailable");
    let cgroup_remaining =
        cgroup_limit_bytes
            .zip(cgroup_current_bytes)
            .and_then(|(limit, current)| {
                let remaining = limit.saturating_sub(current);
                (remaining < u64::MAX / 2).then_some(remaining)
            });
    let effective_available_bytes = cgroup_remaining
        .map_or(available_at_start_bytes, |remaining| {
            available_at_start_bytes.min(remaining)
        });

    Ok(MemoryInventory {
        installed_bytes: value("MemTotal"),
        available_at_start_bytes,
        free_bytes: value("MemFree"),
        swap_total_bytes: value("SwapTotal"),
        swap_free_bytes: value("SwapFree"),
        hardware_corrupted_bytes: value("HardwareCorrupted"),
        page_size_bytes: parse_page_size().unwrap_or(4096),
        cgroup_limit_bytes,
        cgroup_current_bytes,
        effective_available_bytes,
    })
}

fn collect_cpu(numa_nodes: &[NumaNode]) -> Result<CpuInventory, SystemDitError> {
    let cpuinfo = read_required("/proc/cpuinfo")?;
    let architecture = std::env::consts::ARCH.to_owned();
    let vendor_id = first_cpuinfo_value(&cpuinfo, "vendor_id").unwrap_or_default();
    let model_name = first_cpuinfo_value(&cpuinfo, "model name").unwrap_or_default();
    let online = parse_cpu_list(&read_trimmed("/sys/devices/system/cpu/online")?);
    let allowed = parse_allowed_cpus().unwrap_or_else(|| online.clone());
    let hybrid = HybridCores::read();
    let mut physical_cores = BTreeSet::new();
    let mut threads = Vec::new();
    for cpu_id in online.iter().copied() {
        let topology_root = format!("/sys/devices/system/cpu/cpu{cpu_id}/topology");
        let package_id = read_trimmed(format!("{topology_root}/physical_package_id"))
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(-1);
        let core_id = read_trimmed(format!("{topology_root}/core_id"))
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(i32::try_from(cpu_id).unwrap_or(-1));
        physical_cores.insert((package_id, core_id));
        let numa_node = numa_nodes
            .iter()
            .find(|node| node.cpu_ids.contains(&cpu_id))
            .map(|node| node.node_id);
        let siblings = read_trimmed(format!("{topology_root}/thread_siblings_list"))
            .map_or_else(|_| vec![cpu_id], |text| parse_cpu_list(&text));
        let l3_id = read_trimmed(format!(
            "/sys/devices/system/cpu/cpu{cpu_id}/cache/index3/id"
        ))
        .ok()
        .and_then(|value| value.parse().ok());
        threads.push(CpuThread {
            cpu_id,
            package_id,
            core_id,
            numa_node,
            allowed: allowed.contains(&cpu_id),
            core_kind: hybrid.kind(cpu_id),
            smt_rank: smt_rank(cpu_id, &siblings),
            l3_id,
        });
    }

    Ok(CpuInventory {
        architecture,
        vendor_id,
        model_name,
        logical_cpu_count: threads.len(),
        allowed_cpu_ids: allowed,
        physical_core_count: physical_cores.len(),
        avx2: detect_x86_feature("avx2"),
        aes: detect_x86_feature("aes"),
        fma: detect_x86_feature("fma"),
        sse4_2: detect_x86_feature("sse4.2"),
        threads,
    })
}

/// Linux lists the cores of a hybrid Intel CPU under two PMUs: `cpu_core` (P-cores) and
/// `cpu_atom` (E-cores). Neither exists on a CPU whose cores are all alike.
struct HybridCores {
    performance: Vec<usize>,
    efficient: Vec<usize>,
}

impl HybridCores {
    fn read() -> Self {
        let list = |path: &str| {
            fs::read_to_string(path).map_or_else(|_| Vec::new(), |text| parse_cpu_list(&text))
        };
        Self {
            performance: list("/sys/devices/cpu_core/cpus"),
            efficient: list("/sys/devices/cpu_atom/cpus"),
        }
    }

    fn kind(&self, cpu_id: usize) -> CoreKind {
        if self.performance.is_empty() || self.efficient.is_empty() {
            CoreKind::Uniform
        } else if self.efficient.contains(&cpu_id) {
            CoreKind::Efficient
        } else if self.performance.contains(&cpu_id) {
            CoreKind::Performance
        } else {
            CoreKind::Uniform
        }
    }
}

/// The position of `cpu_id` among the hardware threads of its core (`thread_siblings_list`).
fn smt_rank(cpu_id: usize, siblings: &[usize]) -> usize {
    siblings.iter().filter(|&&sibling| sibling < cpu_id).count()
}

fn collect_numa_nodes() -> Vec<NumaNode> {
    let Ok(entries) = fs::read_dir("/sys/devices/system/node") else {
        return Vec::new();
    };
    let mut nodes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(node_id) = name.strip_prefix("node").and_then(|id| id.parse().ok()) else {
            continue;
        };
        let root = entry.path();
        let cpu_ids = fs::read_to_string(root.join("cpulist"))
            .map_or_else(|_| Vec::new(), |text| parse_cpu_list(&text));
        let node_meminfo = fs::read_to_string(root.join("meminfo")).unwrap_or_default();
        nodes.push(NumaNode {
            node_id,
            cpu_ids,
            mem_total_bytes: parse_node_meminfo_bytes(&node_meminfo, "MemTotal"),
            mem_free_bytes: parse_node_meminfo_bytes(&node_meminfo, "MemFree"),
        });
    }
    nodes.sort_by_key(|node| node.node_id);
    nodes
}

fn parse_meminfo_bytes(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate == key).then(|| parse_kib_value(value)).flatten()
    })
}

fn parse_node_meminfo_bytes(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let marker = format!(" {key}:");
        line.find(&marker)
            .and_then(|index| parse_kib_value(&line[index + marker.len()..]))
    })
}

fn parse_kib_value(value: &str) -> Option<u64> {
    value
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

fn parse_page_size() -> Option<u64> {
    let smaps = fs::read_to_string("/proc/self/smaps").ok()?;
    parse_meminfo_bytes(&smaps, "KernelPageSize")
}

fn parse_allowed_cpus() -> Option<Vec<usize>> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Cpus_allowed_list:").map(parse_cpu_list))
}

pub(crate) fn parse_cpu_list(text: &str) -> Vec<usize> {
    let mut cpus = BTreeSet::new();
    for part in text.trim().split(',').filter(|part| !part.is_empty()) {
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (start.parse::<usize>(), end.parse::<usize>()) {
                cpus.extend(start..=end);
            }
        } else if let Ok(cpu) = part.parse() {
            cpus.insert(cpu);
        }
    }
    cpus.into_iter().collect()
}

fn first_cpuinfo_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        (candidate.trim() == key).then(|| value.trim().to_owned())
    })
}

fn read_optional_limit(paths: &[&str]) -> Option<u64> {
    paths.iter().find_map(|path| {
        let value = fs::read_to_string(path).ok()?;
        let value = value.trim();
        (value != "max").then(|| value.parse().ok()).flatten()
    })
}

fn read_optional_u64(paths: &[&str]) -> Option<u64> {
    paths
        .iter()
        .find_map(|path| fs::read_to_string(path).ok()?.trim().parse().ok())
}

fn read_required(path: impl AsRef<Path>) -> Result<String, SystemDitError> {
    let path = path.as_ref();
    fs::read_to_string(path).map_err(|source| SystemDitError::Read {
        path: path.display().to_string(),
        source,
    })
}

fn read_trimmed(path: impl AsRef<Path>) -> Result<String, SystemDitError> {
    read_required(path).map(|text| text.trim().to_owned())
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn detect_x86_feature(name: &str) -> bool {
    match name {
        "avx2" => std::arch::is_x86_feature_detected!("avx2"),
        "aes" => std::arch::is_x86_feature_detected!("aes"),
        "fma" => std::arch::is_x86_feature_detected!("fma"),
        "sse4.2" => std::arch::is_x86_feature_detected!("sse4.2"),
        _ => false,
    }
}

#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
const fn detect_x86_feature(_name: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{CoreKind, HybridCores, parse_cpu_list, parse_meminfo_bytes, smt_rank};

    #[test]
    fn core_kinds_and_smt_ranks_follow_sysfs() {
        // i9-14900K: eight P-cores with two threads each (0-15), sixteen E-cores (16-31).
        let hybrid = HybridCores {
            performance: parse_cpu_list("0-15"),
            efficient: parse_cpu_list("16-31"),
        };
        assert_eq!(hybrid.kind(3), CoreKind::Performance);
        assert_eq!(hybrid.kind(24), CoreKind::Efficient);
        let uniform = HybridCores {
            performance: Vec::new(),
            efficient: Vec::new(),
        };
        assert_eq!(uniform.kind(3), CoreKind::Uniform);
        // Siblings next to each other (Intel) or a core count apart (AMD).
        assert_eq!(smt_rank(0, &[0, 1]), 0);
        assert_eq!(smt_rank(1, &[0, 1]), 1);
        assert_eq!(smt_rank(24, &[8, 24]), 1);
        assert_eq!(smt_rank(20, &[20]), 0);
    }

    #[test]
    fn parses_cpu_ranges() {
        assert_eq!(parse_cpu_list("0-3,8,10-11"), vec![0, 1, 2, 3, 8, 10, 11]);
    }

    #[test]
    fn parses_meminfo_kib() {
        assert_eq!(
            parse_meminfo_bytes("MemTotal: 1024 kB\n", "MemTotal"),
            Some(1_048_576)
        );
    }
}
