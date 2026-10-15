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
use std::os::unix::fs::{FileTypeExt, MetadataExt};
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

/// Whether the results volume at `results_root` can take writes now. A plain directory (a
/// development run) counts as a volume. A mount point counts only while the device behind it is
/// still there: once the USB stick is pulled its mount stays, but every write to it fails. The
/// image boots into RAM (`toram`) precisely so that the stick can come out at any time; tests
/// then go on, their results kept in tmpfs.
#[must_use]
pub fn volume_alive(results_root: &Path) -> bool {
    if !results_root.is_dir() {
        return false;
    }
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    match mount_entry(&mountinfo, results_root) {
        // The node must still be there and still be the mounted device: another stick plugged in
        // later can be given the same name.
        Some((device, source)) if source.starts_with("/dev/") => {
            fs::metadata(&source).is_ok_and(|node| {
                node.file_type().is_block_device() && device_text(node.rdev()) == device
            })
        }
        // Not a mount point (a plain directory), or a source that is not a device node.
        _ => true,
    }
}

/// The device (`major:minor`) and source of the mount at `mount_point` in `/proc/self/mountinfo`
/// text (the last one wins, as a newer mount hides an older one there).
fn mount_entry(mountinfo: &str, mount_point: &Path) -> Option<(String, String)> {
    let wanted = mount_point.to_string_lossy();
    let wanted = wanted.trim_end_matches('/');
    mountinfo
        .lines()
        .filter_map(|line| {
            let (before, after) = line.split_once(" - ")?;
            let mut fields = before.split_whitespace();
            let device = fields.nth(2)?;
            let point = fields.nth(1)?;
            let source = after.split_whitespace().nth(1)?;
            (unescape_mount(point) == wanted).then(|| (device.to_owned(), unescape_mount(source)))
        })
        .next_back()
}

/// `major:minor` of a device number, as `/proc/self/mountinfo` writes it.
fn device_text(rdev: u64) -> String {
    let major = ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff);
    let minor = (rdev & 0xff) | ((rdev >> 12) & !0xff);
    format!("{major}:{minor}")
}

/// `/proc/self/mountinfo` writes spaces and the like as `\040` octal escapes.
fn unescape_mount(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&text[index + 1..index + 4], 8)
        {
            out.push(value);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The session's own claim, in its tmpfs directory: there for as long as the machine runs,
/// whatever happens to the results volume.
fn session_claim_path(root: &Path, run_id: &str) -> PathBuf {
    root.join(run_id).join("result.claim")
}

/// The durable copy of the claim on the results volume: it outlives the session, so the next
/// boot's post-mortem knows the session got its result.
fn volume_claim_path(results_root: &Path, run_id: &str) -> PathBuf {
    results_root.join("claims").join(format!("{run_id}.claim"))
}

/// Claims the result of `run_id` for `by`; the first caller wins. The decision is the session's
/// tmpfs claim, so a pulled USB stick cannot make every caller lose (a failing write to the stick
/// once read as "someone else wrote it", and nobody wrote the result). A live volume also gets a
/// durable copy; a claim already there (written before this boot) wins over this one. A claim
/// that cannot be written anywhere lets the caller write: a row twice beats no row.
#[must_use]
pub fn claim(results_root: &Path, root: &Path, run_id: &str, by: &str) -> Claim {
    let local = session_claim_path(root, run_id);
    if let Some(parent) = local.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(false) = create_claim(&local, by) {
        return Claim::Taken(claimed_by(results_root, root, run_id).unwrap_or_default());
    }
    if volume_alive(results_root) {
        let durable = volume_claim_path(results_root, run_id);
        if let Some(parent) = durable.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(false) = create_claim(&durable, by) {
            let who = fs::read_to_string(&durable)
                .ok()
                .map(|text| text.lines().next().unwrap_or_default().to_owned())
                .unwrap_or_default();
            return Claim::Taken(who);
        }
        // Any other error: the volume is failing; the session's claim stands.
    }
    Claim::Won
}

/// Creates the claim file `path` for `by`: `Ok(true)` created, `Ok(false)` it already exists,
/// `Err` it could not be written.
fn create_claim(path: &Path, by: &str) -> io::Result<bool> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            let _ = writeln!(file, "{by}\n{}", unix_ms());
            let _ = file.sync_all();
            if let Some(parent) = path.parent()
                && let Ok(dir) = File::open(parent)
            {
                let _ = dir.sync_all();
            }
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// Who claimed `run_id`'s result, if anyone: the session's claim, else the volume's (a session
/// of an earlier boot).
#[must_use]
pub fn claimed_by(results_root: &Path, root: &Path, run_id: &str) -> Option<String> {
    let text = fs::read_to_string(session_claim_path(root, run_id))
        .ok()
        .or_else(|| {
            volume_alive(results_root)
                .then(|| fs::read_to_string(volume_claim_path(results_root, run_id)).ok())
                .flatten()
        })?;
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
        // The session's own claim decides; without a results volume there is no durable copy.
        assert_eq!(
            claim(&root.join("none"), &root, "suite-3", "app"),
            Claim::Won
        );
        assert!(root.join("suite-3/result.claim").is_file());
        // A claim the volume kept from an earlier boot wins over a fresh one (the post-mortem).
        fs::write(results.join("claims/suite-4.claim"), "app\n1\n").unwrap();
        assert_eq!(
            claim(&results, &root, "suite-4", "blackbox"),
            Claim::Taken("app".to_owned())
        );
        // No session claim can be made (a post-mortem in a test): the volume's decides.
        let blocked = root.join("blocked");
        fs::write(&blocked, "").unwrap();
        assert_eq!(claim(&results, &blocked, "suite-5", "app"), Claim::Won);
        assert_eq!(
            claim(&results, &blocked, "suite-5", "blackbox"),
            Claim::Taken("app".to_owned())
        );
        // Nowhere to write a claim: the result is still written.
        assert_eq!(
            claim(&root.join("none"), &blocked, "suite-6", "app"),
            Claim::Won
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_pulled_stick_is_told_by_its_mount_source() {
        let mountinfo = "\
22 1 0:21 / /run rw,nosuid shared:5 - tmpfs tmpfs rw
96 30 8:3 / /mnt/cpu-dit-results rw,nosuid shared:50 - vfat /dev/sdb3 rw,fmask=0077
97 30 8:4 / /mnt/with\\040space rw shared:51 - vfat /dev/sdb4 rw";
        let entry = |point: &str| mount_entry(mountinfo, Path::new(point));
        let sdb = |device: &str, source: &str| Some((device.to_owned(), source.to_owned()));
        assert_eq!(entry("/mnt/cpu-dit-results"), sdb("8:3", "/dev/sdb3"));
        assert_eq!(entry("/mnt/cpu-dit-results/"), sdb("8:3", "/dev/sdb3"));
        assert_eq!(entry("/mnt/with space"), sdb("8:4", "/dev/sdb4"));
        assert_eq!(entry("/mnt/other"), None);
        // sdb3 is 8:19 and the 259 block major (NVMe partitions) goes past 8 bits.
        assert_eq!(device_text(0x813), "8:19");
        assert_eq!(device_text((259 << 8) | 5), "259:5");
        // A mounted device of this machine is alive.
        let here = fs::read_to_string("/proc/self/mountinfo").unwrap();
        if let Some(point) = here.lines().find_map(|line| {
            let point = line.split_whitespace().nth(4)?;
            let source = line.split_once(" - ")?.1.split_whitespace().nth(1)?;
            (source.starts_with("/dev/") && Path::new(source).exists())
                .then(|| unescape_mount(point))
        }) {
            assert!(volume_alive(Path::new(&point)), "{point}");
        }
        // A plain directory is a volume (development runs); a missing one is not.
        let dir = temp("volume");
        assert!(volume_alive(&dir));
        assert!(!volume_alive(&dir.join("absent")));
        fs::remove_dir_all(dir).unwrap();
    }
}
