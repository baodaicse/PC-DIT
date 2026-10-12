//! The live session the app keeps in tmpfs for the black box and the rescue console
//! (`docs/FAULT_CATCH_ROADMAP.md`, FC1 and `FC3a`), and the claim that gives a session one result.
//!
//! The app that owns a test writes `session.json` once and `session-status.json` every second
//! under `/run/dit-suite/<run-id>/`: the control center for COMBINED, GPU-VRAM and CPU-SSD; for
//! CPU-RAM the UI script, whose running screen beats for it. Whoever writes the session's result
//! first creates its claim; the others write no second result and no second CSV row.
use crate::fault_ledger::{CsvCounts, CsvMeta};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 1;
/// Where sessions live.
pub const ROOT: &str = "/run/dit-suite";
const INFO_FILE: &str = "session.json";
const STATUS_FILE: &str = "session-status.json";
/// Written by the rescue console when the operator stops the test from it.
pub const RESCUE_STOP_FILE: &str = "rescue-stop";
const BEAT_PERIOD: Duration = Duration::from_secs(1);

/// State of a session that has its result.
pub const CLOSED: &str = "CLOSED";

/// What a session is, written once when it starts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub schema_version: u32,
    pub run_id: String,
    /// The CSV row's description (test, machine, image); time, verdict and report are set when
    /// the row is written.
    pub csv: CsvMeta,
    /// The process that writes the result: its death is what the black box watches for.
    pub app_pid: u32,
    pub started_unix_ms: u64,
    /// Verified results volume; a path that is not a directory means tmpfs only.
    pub results_root: PathBuf,
    /// Cells a failure the app has not placed yet is counted on (a single-module run counts its
    /// failures from the report at the end): `["CPU", "RAM"]`.
    pub pending_cells: Vec<String>,
    /// The session's SSDs: (number, block device), e.g. `(2, "nvme1n1")`, so a kernel line about
    /// `nvme1` is counted on SSD2.
    #[serde(default)]
    pub ssd_blocks: Vec<(u8, String)>,
    /// The session's GPUs in their order (PCI addresses): GPU1 first.
    #[serde(default)]
    pub gpu_pcis: Vec<String>,
}

/// How a session is doing, rewritten every second.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionStatus {
    /// Stamped by [`SessionWriter`]; the app's main loop is alive while it moves.
    pub heartbeat_unix_ms: u64,
    /// `RUNNING`, `STOPPING`, or [`CLOSED`] once the result is written.
    pub state: String,
    /// What runs now, as the screen says it (`PHASE 6/6 OVERLAP (00:01:20 / 00:15:00)`).
    pub phase: String,
    pub elapsed_seconds: u64,
    pub layers: Vec<LayerStatus>,
    /// Every failure the app has seen, counted on a cell or not yet.
    pub failures: u64,
    /// What the failures counted so far put in the CSV row.
    pub counts: CsvCounts,
    /// The app's screen as last drawn (`UnifiedDashboard` JSON).
    pub dashboard_file: Option<PathBuf>,
    /// Set with [`CLOSED`].
    pub verdict: Option<String>,
}

/// One layer (test process) of a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayerStatus {
    /// `cpu-ram`, `gpu-vram`, `cpu-nvme`, `chain`.
    pub name: String,
    /// Its supervising `dit-watchdog`.
    pub watchdog_pid: Option<u32>,
    /// Run directory with its `watchdog-status.json`.
    pub run_dir: PathBuf,
    /// Files that change while it makes progress (heartbeat, events).
    pub progress_paths: Vec<PathBuf>,
    /// Its memory cgroup (step FC2), when it has one.
    pub cgroup: Option<PathBuf>,
    /// `RUNNING`, or how it ended (`PASS`, `STALLED_KILLED ...`).
    pub state: String,
    pub ended: bool,
}

#[must_use]
pub fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Writes `value` to `path` whole (temporary file, then rename): a reader never sees half of it.
fn write_whole(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(value).map_err(io::Error::other)?)?;
    fs::rename(tmp, path)
}

/// The app's side of a session.
pub struct SessionWriter {
    dir: PathBuf,
    last_beat: Option<Instant>,
}

impl SessionWriter {
    /// Opens the session under `root` (normally [`ROOT`]): `session.json` and a first status.
    /// # Errors
    /// The session directory or its files cannot be written.
    pub fn open(root: &Path, info: &SessionInfo) -> io::Result<Self> {
        let dir = root.join(&info.run_id);
        fs::create_dir_all(&dir)?;
        write_whole(&dir.join(INFO_FILE), info)?;
        let mut writer = Self {
            dir,
            last_beat: None,
        };
        writer.beat_now(&mut SessionStatus {
            state: "RUNNING".to_owned(),
            ..SessionStatus::default()
        });
        Ok(writer)
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Writes `status`, stamped with the heartbeat, at most once a second.
    pub fn beat(&mut self, status: &mut SessionStatus) {
        if self
            .last_beat
            .is_none_or(|last| last.elapsed() >= BEAT_PERIOD)
        {
            self.beat_now(status);
        }
    }

    pub fn beat_now(&mut self, status: &mut SessionStatus) {
        status.heartbeat_unix_ms = unix_ms();
        // tmpfs: a failed write leaves the last status, which then ages like a stopped app.
        let _ = write_whole(&self.dir.join(STATUS_FILE), status);
        self.last_beat = Some(Instant::now());
    }

    /// Marks the session closed with its result.
    pub fn close(&mut self, status: &mut SessionStatus, verdict: &str) {
        CLOSED.clone_into(&mut status.state);
        status.verdict = Some(verdict.to_owned());
        self.beat_now(status);
    }

    /// Whether the operator stopped this session from the rescue console.
    #[must_use]
    pub fn rescue_stop(&self) -> bool {
        self.dir.join(RESCUE_STOP_FILE).exists()
    }
}

/// Whether the operator stopped the session `run_id` from the rescue console.
#[must_use]
pub fn rescue_stopped(root: &Path, run_id: &str) -> bool {
    root.join(run_id).join(RESCUE_STOP_FILE).exists()
}

/// The cause a result written by the app carries, if any (`operator_rescue`).
#[must_use]
pub fn app_cause(root: &Path, run_id: &str) -> Option<&'static str> {
    rescue_stopped(root, run_id).then_some("operator_rescue")
}

/// Closes the session `run_id` under `root` without its writer (the CPU-RAM UI script, at the
/// end of a run): the last status, marked closed.
pub fn close(root: &Path, run_id: &str, verdict: &str) {
    let dir = root.join(run_id);
    let mut status = read_status(&dir).unwrap_or_default();
    CLOSED.clone_into(&mut status.state);
    status.verdict = Some(verdict.to_owned());
    status.heartbeat_unix_ms = unix_ms();
    let _ = write_whole(&dir.join(STATUS_FILE), &status);
}

#[must_use]
pub fn read_info(dir: &Path) -> Option<SessionInfo> {
    serde_json::from_slice(&fs::read(dir.join(INFO_FILE)).ok()?).ok()
}

#[must_use]
pub fn read_status(dir: &Path) -> Option<SessionStatus> {
    serde_json::from_slice(&fs::read(dir.join(STATUS_FILE)).ok()?).ok()
}

/// Session directories under `root` that are not closed, oldest first.
#[must_use]
pub fn open_sessions(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut open: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter_map(|dir| {
            let info = read_info(&dir)?;
            let closed = read_status(&dir).is_some_and(|status| status.state == CLOSED);
            (!closed).then_some((info.started_unix_ms, dir))
        })
        .collect();
    open.sort();
    open.into_iter().map(|(_, dir)| dir).collect()
}

/// Who wrote a session's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// This caller: it writes the result.
    Won,
    /// Someone else (`app`, `blackbox`, `rescue`) already did.
    Taken(String),
}

/// Where the claim of `run_id` lives: on the results volume when there is one (it outlives the
/// session), else in the session directory.
#[must_use]
pub fn claim_path(results_root: &Path, root: &Path, run_id: &str) -> PathBuf {
    if results_root.is_dir() {
        results_root.join("claims").join(format!("{run_id}.claim"))
    } else {
        root.join(run_id).join("result.claim")
    }
}

/// Claims the result of `run_id` for `by`; the first caller wins.
#[must_use]
pub fn claim(results_root: &Path, root: &Path, run_id: &str, by: &str) -> Claim {
    let path = claim_path(results_root, root, run_id);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let _ = writeln!(file, "{by}\n{}", unix_ms());
            let _ = file.sync_all();
            if let Some(parent) = path.parent()
                && let Ok(dir) = File::open(parent)
            {
                let _ = dir.sync_all();
            }
            Claim::Won
        }
        Err(_) => Claim::Taken(claimed_by(results_root, root, run_id).unwrap_or_default()),
    }
}

/// Who claimed `run_id`'s result, if anyone.
#[must_use]
pub fn claimed_by(results_root: &Path, root: &Path, run_id: &str) -> Option<String> {
    let text = fs::read_to_string(claim_path(results_root, root, run_id)).ok()?;
    Some(text.lines().next().unwrap_or_default().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dit-session-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn info(run_id: &str, results_root: &Path) -> SessionInfo {
        SessionInfo {
            schema_version: SCHEMA_VERSION,
            run_id: run_id.to_owned(),
            csv: CsvMeta {
                test: "COMBINED DEEP max".to_owned(),
                ..CsvMeta::default()
            },
            app_pid: std::process::id(),
            started_unix_ms: unix_ms(),
            results_root: results_root.to_owned(),
            pending_cells: Vec::new(),
            ssd_blocks: Vec::new(),
            gpu_pcis: Vec::new(),
        }
    }

    #[test]
    fn a_session_is_open_until_closed_and_its_status_is_whole() {
        let root = temp("open");
        let mut writer = SessionWriter::open(&root, &info("suite-1", &root.join("none"))).unwrap();
        assert_eq!(open_sessions(&root), vec![root.join("suite-1")]);
        let mut status = SessionStatus {
            state: "RUNNING".to_owned(),
            phase: "PHASE 6/6 OVERLAP".to_owned(),
            failures: 2,
            ..SessionStatus::default()
        };
        writer.beat_now(&mut status);
        let read = read_status(writer.dir()).unwrap();
        assert_eq!(read.phase, "PHASE 6/6 OVERLAP");
        assert!(read.heartbeat_unix_ms > 0);
        writer.close(&mut status, "FAIL");
        assert!(open_sessions(&root).is_empty());
        assert_eq!(
            read_status(writer.dir()).unwrap().verdict.as_deref(),
            Some("FAIL")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_first_claim_wins_and_the_others_see_who() {
        let root = temp("claim");
        let results = root.join("results");
        fs::create_dir_all(&results).unwrap();
        assert_eq!(claim(&results, &root, "suite-2", "blackbox"), Claim::Won);
        assert_eq!(
            claim(&results, &root, "suite-2", "app"),
            Claim::Taken("blackbox".to_owned())
        );
        assert!(results.join("claims/suite-2.claim").is_file());
        // Without a results volume the claim stays with the session.
        assert_eq!(
            claim(&root.join("none"), &root, "suite-3", "app"),
            Claim::Won
        );
        assert!(root.join("suite-3/result.claim").is_file());
        fs::remove_dir_all(root).unwrap();
    }
}
