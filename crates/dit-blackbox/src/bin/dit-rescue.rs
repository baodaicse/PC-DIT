//! `dit-rescue`: the rescue console on tty2 (Alt+F2), `docs/FAULT_CATCH_ROADMAP.md` step FC1.
//!
//! When the test screen stops answering (the machine short of memory, a driver stuck) the
//! operator still has a way out that is recorded: switching consoles is the kernel's work, and
//! this process is locked in memory and starts no program. It shows the session, its layers and
//! the memory, and acts only on a key pressed twice: [S] stop the test with a record, [K] end the
//! test process holding the most memory, [L] save diagnostics, [R] sync and restart.
use clap::Parser;
use dit_blackbox::host::{self, KernelLog, Process};
use dit_blackbox::outcome::{self, Cause};
use dit_orchestrator::session::{self, SessionInfo, SessionStatus};
use dit_ui_core::{TerminalSize, decorate, option_screen};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(name = "dit-rescue", about = "DIT rescue console (Alt+F2)")]
struct Args {
    /// Where the app keeps its live sessions.
    #[arg(long, default_value = session::ROOT)]
    session_root: PathBuf,
    #[arg(long, default_value = "/proc")]
    proc_root: PathBuf,
    #[arg(long, default_value = "/dev/kmsg")]
    kmsg: PathBuf,
    /// Results volume for diagnostics saved while no session runs.
    #[arg(long, default_value = "/mnt/cpu-dit-results")]
    results_root: PathBuf,
    /// How long [S] waits for the app to stop and write its own result.
    #[arg(long, default_value_t = 30)]
    stop_wait_seconds: u64,
}

/// A key pressed once waits this long for the second press that confirms it.
const CONFIRM_WINDOW: Duration = Duration::from_secs(5);

/// What the console shows: the session (if one runs), the machine.
struct View {
    session: Option<(PathBuf, SessionInfo, SessionStatus)>,
    app_alive: bool,
    memory: host::Memory,
    pressure: Option<host::Pressure>,
    /// Test processes (the layers' watchdogs and everything under them).
    tests: Vec<Process>,
    layers: Vec<String>,
}

fn gib(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{value:.1} GiB")
}

fn ago(unix_ms: u64) -> String {
    if unix_ms == 0 {
        return "never".to_owned();
    }
    format!(
        "{} s ago",
        session::unix_ms().saturating_sub(unix_ms) / 1000
    )
}

impl View {
    fn read(args: &Args) -> Self {
        let session = session::open_sessions(&args.session_root)
            .pop()
            .and_then(|dir| {
                let info = session::read_info(&dir)?;
                let status = session::read_status(&dir).unwrap_or_default();
                Some((dir, info, status))
            });
        let all = host::processes(&args.proc_root);
        let mut tests = Vec::new();
        let mut layers = Vec::new();
        let app_alive = session
            .as_ref()
            .is_some_and(|(_, info, _)| host::alive(&args.proc_root, info.app_pid));
        if let Some((_, _, status)) = &session {
            for layer in &status.layers {
                if let Some(pid) = layer.watchdog_pid {
                    tests.extend(host::tree(&all, pid));
                }
                let watchdog: serde_json::Value =
                    std::fs::read(layer.run_dir.join("watchdog-status.json"))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                        .unwrap_or_default();
                layers.push(format!(
                    "{:<9} {:<15} last activity {}",
                    layer.name,
                    watchdog["state"].as_str().unwrap_or(&layer.state),
                    ago(watchdog["last_activity_unix_ms"].as_u64().unwrap_or(0))
                ));
            }
        }
        Self {
            session,
            app_alive,
            memory: host::memory(&args.proc_root),
            pressure: host::pressure(&args.proc_root, "memory"),
            tests,
            layers,
        }
    }

    /// The test process holding the most memory (not a watchdog: it keeps the record).
    fn biggest(&self) -> Option<&Process> {
        self.tests
            .iter()
            .filter(|p| p.name != "dit-watchdog" && p.state != 'Z')
            .max_by_key(|p| p.rss_bytes)
    }

    fn screen(&self, message: &str, pending: Option<char>) -> String {
        let mut state = Vec::new();
        match &self.session {
            Some((_, info, status)) => {
                state.push(format!(" SESSION  {}  {}", info.csv.test, info.run_id));
                state.push(format!(
                    " NOW      {}  ({} s in, failures counted {})",
                    status.phase, status.elapsed_seconds, status.failures
                ));
                state.push(format!(
                    " APP      pid {} {}  last heartbeat {}",
                    info.app_pid,
                    if self.app_alive { "alive" } else { "GONE" },
                    ago(status.heartbeat_unix_ms)
                ));
                for layer in &self.layers {
                    state.push(format!(" LAYER    {layer}"));
                }
            }
            None => state.push(" SESSION  no test is running".to_owned()),
        }
        state.push(format!(
            " MEMORY   {} available of {}   pressure some {:.1} % / full {:.1} % (10 s)",
            gib(self.memory.available_bytes),
            gib(self.memory.total_bytes),
            self.pressure.map_or(0.0, |p| p.some_avg10),
            self.pressure.map_or(0.0, |p| p.full_avg10)
        ));
        let biggest = self.biggest().map_or_else(
            || "no test process".to_owned(),
            |p| format!("pid {} {} holds {}", p.pid, p.name, gib(p.rss_bytes)),
        );
        let confirm = |key: char| {
            if pending == Some(key) {
                "   <- press again to confirm"
            } else {
                ""
            }
        };
        let keys = vec![
            format!(
                " [S] STOP the test and record it (INVALID, operator_rescue; FAIL if it failed){}",
                confirm('s')
            ),
            format!(
                " [K] END the test process using the most memory: {biggest}{}",
                confirm('k')
            ),
            format!(
                " [L] SAVE diagnostics to the USB (kernel log, memory, processes, watchdogs){}",
                confirm('l')
            ),
            format!(" [R] SYNC the USB and RESTART the machine{}", confirm('r')),
            " Each key acts on its second press. Alt+F1 returns to the test screen.".to_owned(),
        ];
        let mut sections = vec![state, keys];
        if !message.is_empty() {
            sections.push(vec![format!(" {message}")]);
        }
        option_screen(
            "DIT RESCUE CONSOLE // ALT+F1 RETURNS TO THE TEST",
            &sections,
            TerminalSize::current(),
        )
    }
}

fn signal(pid: u32, which: Signal) {
    if let Ok(raw) = i32::try_from(pid) {
        let _ = kill(Pid::from_raw(raw), which);
    }
}

fn draw(text: &str) {
    let mut out = io::stdout().lock();
    let _ = write!(out, "\x1b[H\x1b[2J{}", decorate(text));
    let _ = out.flush();
}

/// [S]: asks the app to stop (as Ctrl+C would, and with the rescue-stop file so it records the
/// reason); when it does not write its result in time, records the session here and ends the
/// test processes.
fn stop(args: &Args, view: &View) -> String {
    let Some((dir, info, status)) = &view.session else {
        return "No test is running.".to_owned();
    };
    let _ = std::fs::write(
        dir.join(session::RESCUE_STOP_FILE),
        session::unix_ms().to_string(),
    );
    signal(info.app_pid, Signal::SIGINT);
    for pid in status.layers.iter().filter_map(|layer| layer.watchdog_pid) {
        signal(pid, Signal::SIGINT);
    }
    let deadline = Instant::now() + Duration::from_secs(args.stop_wait_seconds);
    while Instant::now() < deadline {
        if let Some(by) = session::claimed_by(&info.results_root, &args.session_root, &info.run_id)
        {
            return format!("The test stopped; the {by} wrote its result. Alt+F1 shows it.");
        }
        draw(&View::read(args).screen(
            &format!(
                "Stopping: waiting for the test to write its result ({} s left)...",
                deadline.saturating_duration_since(Instant::now()).as_secs()
            ),
            None,
        ));
        std::thread::sleep(Duration::from_millis(500));
    }
    let cause = Cause {
        code: "operator_rescue".to_owned(),
        text: "stopped by the operator from the rescue console; the app did not answer".to_owned(),
        cells: Vec::new(),
        incomplete: false,
    };
    let evidence = serde_json::json!({
        "memory": view.memory,
        "pressure": view.pressure,
        "test_processes": view.tests,
    });
    let written = outcome::write_result(&args.session_root, dir, "rescue", &cause, &evidence);
    for process in &view.tests {
        signal(process.pid, Signal::SIGKILL);
    }
    match written {
        Ok(written) => format!(
            "Recorded {} (operator_rescue): {}. Test processes ended. [R] restarts the machine.",
            written.verdict, written.csv
        ),
        Err(error) => format!("Not recorded here: {error}. Test processes ended."),
    }
}

/// [K]: ends the test process holding the most memory and notes it in the session.
fn end_biggest(view: &View) -> String {
    let Some(process) = view.biggest() else {
        return "No test process to end.".to_owned();
    };
    signal(process.pid, Signal::SIGKILL);
    if let Some((dir, _, _)) = &view.session {
        let line = serde_json::json!({
            "at_unix_ms": session::unix_ms(),
            "action": "kill",
            "pid": process.pid,
            "name": process.name,
            "rss_bytes": process.rss_bytes,
        });
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("rescue-events.jsonl"))
        {
            let _ = writeln!(file, "{line}");
        }
    }
    format!(
        "Ended pid {} {} ({} freed); the test that ran it fails or ends without a report.",
        process.pid,
        process.name,
        gib(process.rss_bytes)
    )
}

/// [L]: kernel log, memory, pressure, processes and the session's files, on the results volume.
fn save_diagnostics(args: &Args, view: &View) -> String {
    let results = view.session.as_ref().map_or_else(
        || args.results_root.clone(),
        |(_, info, _)| info.results_root.clone(),
    );
    let stamp = outcome::utc_timestamp(session::unix_ms() / 1000)
        .replace([' ', ':'], "-")
        .replace("-+0000", "Z");
    let dir = if results.is_dir() {
        results.join("rescue").join(stamp)
    } else {
        Path::new("/run/dit-rescue").join(stamp)
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return format!("Diagnostics not saved ({error}): {}", dir.display());
    }
    let kernel: Vec<String> = KernelLog::open(&args.kmsg)
        .read_new()
        .iter()
        .map(|line| {
            format!(
                "[{:>12}] <{}> {}",
                line.microseconds, line.priority, line.message
            )
        })
        .collect();
    let _ = std::fs::write(dir.join("kernel-log.txt"), kernel.join("\n"));
    for name in ["meminfo", "pressure/memory", "pressure/cpu", "pressure/io"] {
        if let Ok(text) = std::fs::read_to_string(args.proc_root.join(name)) {
            let _ = std::fs::write(dir.join(name.replace('/', "-")), text);
        }
    }
    let mut processes = host::processes(&args.proc_root);
    processes.sort_by_key(|p| std::cmp::Reverse(p.rss_bytes));
    let table: Vec<String> = processes
        .iter()
        .map(|p| {
            format!(
                "{:>7} {:>7} {} {:>12} {:>10} {}",
                p.pid, p.ppid, p.state, p.rss_bytes, p.cpu_ticks, p.command
            )
        })
        .collect();
    let _ = std::fs::write(
        dir.join("processes.txt"),
        format!(
            "    PID    PPID S    RSS_BYTES  CPU_TICKS COMMAND\n{}",
            table.join("\n")
        ),
    );
    if let Some((session_dir, _, status)) = &view.session {
        for name in ["session.json", "session-status.json", "rescue-events.jsonl"] {
            let _ = std::fs::copy(session_dir.join(name), dir.join(name));
        }
        for layer in &status.layers {
            let _ = std::fs::copy(
                layer.run_dir.join("watchdog-status.json"),
                dir.join(format!("{}-watchdog-status.json", layer.name)),
            );
        }
    }
    nix::unistd::sync();
    format!("Diagnostics saved to {}", dir.display())
}

fn restart() -> String {
    nix::unistd::sync();
    match nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT) {
        Err(error) => format!("Restart refused ({error}); hold the power button instead."),
        Ok(never) => match never {},
    }
}

/// Keys come one by one, without echo and without Ctrl+C ending the console.
fn raw_terminal() {
    use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};
    let stdin = io::stdin();
    if let Ok(mut settings) = tcgetattr(stdin.as_fd()) {
        settings
            .local_flags
            .remove(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG);
        let _ = tcsetattr(stdin.as_fd(), SetArg::TCSANOW, &settings);
    }
}

/// The key pressed within `wait`, lower case.
fn key(wait: Duration) -> Option<char> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    let stdin = io::stdin();
    let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
    let timeout = PollTimeout::try_from(wait).unwrap_or(PollTimeout::MAX);
    if poll(&mut fds, timeout).ok()? == 0 {
        return None;
    }
    let mut byte = [0_u8; 1];
    if let Ok(1) = io::stdin().lock().read(&mut byte) {
        Some(char::from(byte[0]).to_ascii_lowercase())
    } else {
        // No terminal (end of input): do not spin.
        std::thread::sleep(wait);
        None
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    // It must answer while the machine is short of memory: every page of it stays in RAM.
    let _ = nix::sys::mman::mlockall(
        nix::sys::mman::MlockAllFlags::MCL_CURRENT | nix::sys::mman::MlockAllFlags::MCL_FUTURE,
    );
    raw_terminal();
    let mut message = String::new();
    let mut pending: Option<(char, Instant)> = None;
    loop {
        if pending.is_some_and(|(_, at)| at.elapsed() > CONFIRM_WINDOW) {
            pending = None;
        }
        let view = View::read(&args);
        draw(&view.screen(&message, pending.map(|(k, _)| k)));
        let Some(pressed) = key(Duration::from_secs(1)) else {
            continue;
        };
        if !matches!(pressed, 's' | 'k' | 'l' | 'r') {
            pending = None;
            continue;
        }
        if pending.map(|(k, _)| k) != Some(pressed) {
            pending = Some((pressed, Instant::now()));
            message.clear();
            continue;
        }
        pending = None;
        message = match pressed {
            's' => stop(&args, &view),
            'k' => end_biggest(&view),
            'l' => save_diagnostics(&args, &view),
            _ => restart(),
        };
    }
}
