//! Progress file: the chain's heartbeat for the watchdog and the combined scheduler. A JSON
//! snapshot replaced atomically (write to a temporary, then rename), like `gpu-vram-dit
//! --progress-file`. `ready` turns true once every allocation of the read phase is done, and
//! `host_peak_bytes` declares the most host memory the process may still reach, so the arbiter
//! can hand the rest of `MemAvailable` to CPU-RAM (`docs/COMBINED_STRESS_CHAIN_ANALYSIS.md` 6.1).
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const INTERVAL: Duration = Duration::from_secs(1);

#[derive(Serialize, Clone, Debug)]
pub struct Progress {
    pub schema_version: u32,
    pub module_id: &'static str,
    /// `opening`, `prefill`, `reading`, `finishing` or `done`.
    pub phase: &'static str,
    pub ready: bool,
    pub verdict: Option<&'static str>,
    pub updated_unix_ms: u128,
    pub elapsed_seconds: f64,
    pub records: u64,
    pub records_written: u64,
    pub loads: u64,
    pub bytes_read: u64,
    /// Over the last pacing window.
    pub read_gib_per_second: f64,
    pub gpu_gops_verified: f64,
    pub generations_per_load: u64,
    pub failures: u64,
    pub host_rss_bytes: u64,
    /// Declared ceiling: the process's high-water mark plus the failure path's extra buffers.
    pub host_peak_bytes: u64,
    pub vram_bytes: u64,
}

impl Progress {
    #[must_use]
    pub fn new(records: u64) -> Self {
        Self {
            schema_version: 1,
            module_id: "dit.chain",
            phase: "opening",
            ready: false,
            verdict: None,
            updated_unix_ms: 0,
            elapsed_seconds: 0.0,
            records,
            records_written: 0,
            loads: 0,
            bytes_read: 0,
            read_gib_per_second: 0.0,
            gpu_gops_verified: 0.0,
            generations_per_load: 0,
            failures: 0,
            host_rss_bytes: 0,
            host_peak_bytes: 0,
            vram_bytes: 0,
        }
    }
}

/// `VmRSS` and `VmHWM` of this process, in bytes.
#[must_use]
pub fn host_memory() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .map_or(0, |kib| kib * 1024)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

pub struct ProgressWriter {
    path: Option<PathBuf>,
    started: Instant,
    last: Option<Instant>,
    /// Extra host memory the failure path may allocate (re-read buffers, a CPU rebuild).
    headroom_bytes: u64,
}

impl ProgressWriter {
    #[must_use]
    pub fn new(path: Option<PathBuf>, started: Instant, headroom_bytes: u64) -> Self {
        Self {
            path,
            started,
            last: None,
            headroom_bytes,
        }
    }

    /// Writes `progress` when a second has passed since the last write, or at once with `force`
    /// (phase changes, the end). A failed write is reported once per call and never stops the run:
    /// a missing heartbeat is the watchdog's evidence.
    pub fn write(&mut self, progress: &mut Progress, force: bool) {
        let Some(path) = self.path.clone() else {
            return;
        };
        if !force && self.last.is_some_and(|last| last.elapsed() < INTERVAL) {
            return;
        }
        self.last = Some(Instant::now());
        let (rss, high_water) = host_memory();
        progress.host_rss_bytes = rss;
        progress.host_peak_bytes = rss.max(high_water) + self.headroom_bytes;
        progress.elapsed_seconds = self.started.elapsed().as_secs_f64();
        progress.updated_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        if let Err(error) = write_atomic(&path, progress) {
            eprintln!("progress file {}: {error}", path.display());
        }
    }
}

fn write_atomic(path: &Path, progress: &Progress) -> std::io::Result<()> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = std::fs::File::create(&temporary)?;
    serde_json::to_writer(&mut file, progress)?;
    writeln!(file)?;
    file.flush()?;
    std::fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_are_replaced_atomically_and_declare_a_peak() {
        let dir = std::env::temp_dir().join(format!("dit-chain-progress-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("progress.json");
        let mut writer = ProgressWriter::new(Some(path.clone()), Instant::now(), 1 << 20);
        let mut progress = Progress::new(4);
        progress.phase = "reading";
        progress.ready = true;
        writer.write(&mut progress, true);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["phase"], "reading");
        assert_eq!(value["ready"], true);
        let peak = value["host_peak_bytes"].as_u64().unwrap();
        assert!(peak >= value["host_rss_bytes"].as_u64().unwrap() + (1 << 20));
        // Throttled: an unforced write within the interval leaves the file as it was.
        progress.phase = "finishing";
        writer.write(&mut progress, false);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["phase"], "reading");
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "no temporary file is left behind");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
