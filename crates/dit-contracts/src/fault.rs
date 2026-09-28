//! Live fault records (`docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md`, step F1): an engine started
//! with `--fault-events <file>` appends one JSON line per failure the moment it finds it, so the
//! control center can count it on its component cell while the run goes on, instead of waiting
//! for the final report. The report stays the authority for the verdict; this is a live copy of
//! its failures, with the evidence the attribution rules need.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const fn one() -> u64 {
    1
}

/// One failure, or a group found together (`count`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultRecord {
    pub schema_version: u32,
    pub t_unix_ms: u64,
    /// The detector: a GPU failure phase (`compute`, `vram-sweep`, `retention`…), `ram:pattern`,
    /// `cpu:<check>`, `platform:worker_crash`, `chain:<attribution>`.
    pub source: String,
    #[serde(default = "one")]
    pub count: u64,
    /// PCI address (GPU) or namespace stable ID (SSD) of the device concerned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(default)]
    pub detail: String,
    /// Boundary-check and confirmation results (steps F3, F4), e.g. `boundary` → `d2h`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub evidence: BTreeMap<String, String>,
}

impl FaultRecord {
    #[must_use]
    pub fn new(source: impl Into<String>, count: u64, detail: impl Into<String>) -> Self {
        Self {
            schema_version: crate::SCHEMA_VERSION,
            t_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            source: source.into(),
            count: count.max(1),
            device: None,
            detail: detail.into(),
            evidence: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn device(mut self, device: impl Into<String>) -> Self {
        self.device = Some(device.into());
        self
    }

    #[must_use]
    pub fn evidence(mut self, key: &str, value: impl Into<String>) -> Self {
        self.evidence.insert(key.to_owned(), value.into());
        self
    }
}

/// Appends records to the file named by `--fault-events`; without one, it does nothing.
/// Each record is one `write` on a file opened for appending, so records of several threads or
/// processes never interleave. A failed write is reported once and never stops the test.
#[derive(Clone, Debug, Default)]
pub struct FaultWriter {
    path: Option<PathBuf>,
}

impl FaultWriter {
    #[must_use]
    pub fn new(path: Option<&Path>) -> Self {
        Self {
            path: path.map(Path::to_path_buf),
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.path.is_some()
    }

    pub fn write(&self, record: &FaultRecord) {
        let Some(path) = &self.path else {
            return;
        };
        let Ok(mut line) = serde_json::to_string(record) else {
            return;
        };
        line.push('\n');
        let written = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(line.as_bytes()));
        if let Err(error) = written {
            eprintln!("fault events {}: {error}", path.display());
        }
    }
}

/// Reads the records appended to a fault-event file since the last call (whole lines only).
#[derive(Debug, Default)]
pub struct FaultTail {
    offset: u64,
    pending: String,
}

impl FaultTail {
    pub fn read(&mut self, path: &Path) -> Vec<FaultRecord> {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut chunk = String::new();
        let Ok(read) = file.read_to_string(&mut chunk) else {
            return Vec::new();
        };
        self.offset += read as u64;
        self.pending.push_str(&chunk);
        let complete = self.pending.rfind('\n').map_or(0, |at| at + 1);
        let lines: String = self.pending.drain(..complete).collect();
        lines
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_through_a_file_and_are_read_once() {
        let dir = std::env::temp_dir().join(format!("dit-fault-events-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fault-events.jsonl");
        let _ = std::fs::remove_file(&path);
        let writer = FaultWriter::new(Some(&path));
        assert!(writer.enabled());
        writer.write(
            &FaultRecord::new("compute", 3, "matrix mismatch")
                .device("0000:08:00.0")
                .evidence("boundary", "d2h"),
        );
        let mut tail = FaultTail::default();
        let first = tail.read(&path);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].count, 3);
        assert_eq!(first[0].evidence["boundary"], "d2h");
        assert!(tail.read(&path).is_empty(), "nothing is read twice");
        writer.write(&FaultRecord::new("vram-sweep", 1, ""));
        assert_eq!(tail.read(&path).len(), 1);
        // A record without the optional fields still parses (count defaults to 1).
        std::fs::write(
            &path,
            "{\"schema_version\":1,\"t_unix_ms\":1,\"source\":\"ram:pattern\"}\n",
        )
        .unwrap();
        let mut fresh = FaultTail::default();
        assert_eq!(fresh.read(&path)[0].count, 1);
        FaultWriter::new(None).write(&FaultRecord::new("x", 1, ""));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
