//! The black box's journal of a session (`docs/FAULT_CATCH_ROADMAP.md`, `FC3a`, `FC3b`): one JSON
//! line about every 10 s and at every change, appended and synced, never rewritten. A power loss
//! can cut the last line short; readers skip it. The next boot reads a journal that has no
//! closing line to say how the session ended (`FC3b`).
use dit_orchestrator::session::SessionInfo;
use serde_json::Value;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where journals live without a results volume.
pub const RUNTIME_DIR: &str = "/run/dit-blackbox";

pub struct Journal {
    path: PathBuf,
    file: Option<File>,
}

/// The journal of `run_id`: on the results volume when there is one.
#[must_use]
pub fn path(results_root: &Path, run_id: &str) -> PathBuf {
    let dir = if results_root.is_dir() {
        results_root.join("blackbox")
    } else {
        PathBuf::from(RUNTIME_DIR)
    };
    dir.join(format!("{run_id}.jsonl"))
}

impl Journal {
    /// Opens (appends to) the journal of the session `info`.
    #[must_use]
    pub fn open(info: &SessionInfo) -> Self {
        let path = path(&info.results_root, &info.run_id);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        Self { path, file }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `line` and syncs it to the device.
    pub fn write(&mut self, line: &Value) {
        if let Some(file) = self.file.as_mut() {
            let mut text = line.to_string();
            text.push('\n');
            let _ = file.write_all(text.as_bytes());
            let _ = file.sync_data();
        }
    }
}

/// The journal's whole lines (a last line cut short by a power loss is left out).
#[must_use]
pub fn read(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_cut_short_is_skipped() {
        let dir = std::env::temp_dir().join(format!("dit-journal-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("suite-1.jsonl");
        fs::write(
            &file,
            "{\"kind\":\"beat\",\"t\":1}\n{\"kind\":\"beat\",\"t\":2}\n{\"kind\":\"be",
        )
        .unwrap();
        let lines = read(&file);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["t"], 2);
        fs::remove_dir_all(dir).unwrap();
    }
}
