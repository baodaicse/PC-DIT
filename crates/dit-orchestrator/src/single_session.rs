//! A single-module test (GPU-VRAM, CPU-SSD, CPU-RAM) as a live session for the black box and the
//! rescue console (`docs/FAULT_CATCH_ROADMAP.md`, FC1 and `FC3a`): its status every second while
//! it runs, its result claimed before the CSV row is written.
use super::fault_single::SingleModule;
use dit_orchestrator::session::{
    self, Claim, LayerStatus, SessionInfo, SessionStatus, SessionWriter,
};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

pub struct SingleSession {
    writer: Option<SessionWriter>,
    results_root: PathBuf,
    run_id: String,
    layer: LayerStatus,
    /// The test's screen file (`DIT_UI_FINAL_FRAME`), kept current while it runs.
    frame: PathBuf,
    started: Instant,
    /// Failures the test's progress showed (the screen file says it).
    failures: u64,
}

impl SingleSession {
    /// Opens the session `run_id`: the one layer of `module`, run in `run_dir`, its screen in
    /// `frame`. A session that cannot be written leaves the test as it was, without one.
    #[must_use]
    pub fn open(
        run_id: &str,
        module: SingleModule,
        attribution: bool,
        app_pid: u32,
        results_root: &Path,
        run_dir: &Path,
        frame: &Path,
    ) -> Self {
        let writer = SessionWriter::open(
            Path::new(session::ROOT),
            &SessionInfo {
                schema_version: session::SCHEMA_VERSION,
                run_id: run_id.to_owned(),
                csv: super::fault_single::csv_meta(module, attribution),
                app_pid,
                started_unix_ms: session::unix_ms(),
                results_root: results_root.to_owned(),
                pending_cells: super::fault_single::pending_cells(module)
                    .iter()
                    .map(|cell| (*cell).to_owned())
                    .collect(),
            },
        )
        .ok();
        Self {
            writer,
            results_root: results_root.to_owned(),
            run_id: run_id.to_owned(),
            layer: LayerStatus {
                name: module.directory().to_owned(),
                watchdog_pid: None,
                run_dir: run_dir.to_owned(),
                progress_paths: Vec::new(),
                cgroup: None,
                state: "RUNNING".to_owned(),
                ended: false,
            },
            frame: frame.to_owned(),
            started: Instant::now(),
            failures: 0,
        }
    }

    fn status(&mut self, phase: Option<String>, failures: Option<u64>) -> SessionStatus {
        let screen = dit_ui_core::load_final_frame(&self.frame);
        self.failures = self
            .failures
            .max(failures.unwrap_or(0))
            .max(screen.as_ref().map_or(0, |screen| screen.failures));
        SessionStatus {
            heartbeat_unix_ms: 0,
            state: "RUNNING".to_owned(),
            phase: phase
                .or_else(|| screen.map(|screen| screen.active_test))
                .unwrap_or_default(),
            elapsed_seconds: self.started.elapsed().as_secs(),
            layers: vec![self.layer.clone()],
            failures: self.failures,
            counts: dit_orchestrator::fault_ledger::CsvCounts::default(),
            dashboard_file: Some(self.frame.clone()),
            verdict: None,
        }
    }

    /// The status, at most once a second. `phase` and `failures` when the caller knows them
    /// better than the screen file.
    pub fn beat(&mut self, watchdog_pid: u32, phase: Option<String>, failures: Option<u64>) {
        if self.writer.is_none() {
            return;
        }
        self.layer.watchdog_pid = Some(watchdog_pid);
        let mut status = self.status(phase, failures);
        if let Some(writer) = self.writer.as_mut() {
            writer.beat(&mut status);
        }
    }

    /// Whether the operator stopped the test from the rescue console.
    #[must_use]
    pub fn rescue_stop(&self) -> bool {
        self.writer.as_ref().is_some_and(SessionWriter::rescue_stop)
    }

    /// Claims the session's result for the app.
    #[must_use]
    pub fn claim(&self) -> Claim {
        session::claim(
            &self.results_root,
            Path::new(session::ROOT),
            &self.run_id,
            "app",
        )
    }

    pub fn close(&mut self, verdict: &str) {
        let mut status = self.status(None, None);
        self.layer.ended = true;
        if let Some(writer) = self.writer.as_mut() {
            writer.close(&mut status, verdict);
        }
    }
}

/// Runs a supervised single test (its `dit-watchdog`) while beating the session; a stop asked
/// from the rescue console is passed to the watchdog as the operator's Ctrl+C would be.
/// # Errors
/// The watchdog cannot be started or waited for.
pub fn run_watched(command: &mut Command, session: &mut SingleSession) -> io::Result<ExitStatus> {
    let mut child = command.spawn()?;
    let mut stop_sent = false;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        session.beat(child.id(), None, None);
        if !stop_sent && session.rescue_stop() {
            let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap_or(0));
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT);
            stop_sent = true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
