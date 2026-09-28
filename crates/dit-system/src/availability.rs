//! Execution evidence is independent of data correctness and power attribution.
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{self, Write},
    path::Path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AvailabilityStatus {
    Running,
    Completed,
    Incomplete,
    Interrupted,
    WorkerFailed,
    Stalled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailabilityEvidence {
    pub status: AvailabilityStatus,
    pub detail: String,
}

impl AvailabilityEvidence {
    #[must_use]
    pub fn new(status: AvailabilityStatus, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }
}

/// Persist checkpoints only at phase boundaries; do not use for hot telemetry.
///
/// # Errors
/// Returns any file, serialization or directory durability error.
pub fn write_durable(path: &Path, value: &impl Serialize) -> io::Result<()> {
    write_snapshot(path, value, true)
}

/// Atomic live progress replacement without storage flush; not crash evidence.
pub(crate) fn write_progress(path: &Path, value: &impl Serialize) -> io::Result<()> {
    write_snapshot(path, value, false)
}

fn write_snapshot(path: &Path, value: &impl Serialize, durable: bool) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut file = File::create(&tmp)?;
    serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    if durable {
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    if durable {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
