//! Memory and memory pressure as `/proc` says them (`docs/FAULT_CATCH_ROADMAP.md`, FC2 and
//! `FC3a`): read straight from the files, so it works while the machine is short of memory.
use std::fs;
use std::path::Path;

/// `/proc/meminfo`: total and available memory, bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[must_use]
pub fn memory(proc_root: &Path) -> Memory {
    let text = fs::read_to_string(proc_root.join("meminfo")).unwrap_or_default();
    let field = |name: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .map_or(0, |kib| kib.saturating_mul(1024))
    };
    Memory {
        total_bytes: field("MemTotal:"),
        available_bytes: field("MemAvailable:"),
    }
}

/// Pressure stall information of one resource (`/proc/pressure/<resource>`): the share of time
/// some / all non-idle tasks waited on it over the last 10 s, percent.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Pressure {
    pub some_avg10: f64,
    pub full_avg10: f64,
}

#[must_use]
pub fn pressure(proc_root: &Path, resource: &str) -> Option<Pressure> {
    let text = fs::read_to_string(proc_root.join("pressure").join(resource)).ok()?;
    let avg10 = |kind: &str| {
        text.lines()
            .find(|line| line.starts_with(kind))
            .and_then(|line| {
                line.split_whitespace()
                    .find_map(|field| field.strip_prefix("avg10="))
            })
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    Some(Pressure {
        some_avg10: avg10("some"),
        full_avg10: avg10("full"),
    })
}
