//! `dit-blackbox`: the black box (`docs/FAULT_CATCH_ROADMAP.md`, `FC3a` and `FC3b`).
//!
//! `watch` runs beside the app for the whole boot (its own service, locked in memory, starting no
//! program but `chvt` once): it keeps a journal of every session on the results volume and, when
//! the app can no longer write a session's result (gone, frozen, or not taking a stopped layer
//! off), writes it and switches to it on tty3. It only watches and records: it never stops,
//! kills or frees anything. `post-mortem` runs at boot, before the menu: a session whose
//! journal never closed (the whole machine died) gets its result from what the journal and this
//! boot say.
use clap::{Parser, Subcommand};
use dit_blackbox::host::{self, KernelLog, Process};
use dit_blackbox::journal::Journal;
use dit_blackbox::kernel_events::{self, KernelEvent};
use dit_blackbox::outcome::{self, Cause, Written};
use dit_blackbox::watch::{Decision, LayerSeen, Limits, Observation, Tracker};
use dit_contracts::bounded::{StuckGate, output_within};
use dit_orchestrator::session::{self, SessionInfo, SessionStatus};
use dit_ui_core::{TerminalSize, UnifiedDashboard, decorate, option_screen, render_result_frame};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

#[derive(Debug, Parser)]
#[command(
    name = "dit-blackbox",
    about = "DIT black box: watches test sessions and records them"
)]
struct Cli {
    #[command(subcommand)]
    command: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Watch every session for the whole boot (`dit-blackbox.service`, tty3).
    Watch(WatchArgs),
    /// At boot, before the menu: results for the sessions the previous boot left without one.
    PostMortem(PostMortemArgs),
}

#[derive(Debug, clap::Args)]
struct PostMortemArgs {
    #[arg(long, default_value = "/mnt/cpu-dit-results")]
    results_root: PathBuf,
    #[arg(long, default_value = "/dev/kmsg")]
    kmsg: PathBuf,
    /// Where pstore records are: the kernel's, and where systemd moves them at boot.
    #[arg(long = "pstore", default_values = ["/sys/fs/pstore", "/var/lib/systemd/pstore"])]
    pstore: Vec<PathBuf>,
    /// The notice the control center shows above its menu this boot.
    #[arg(long, default_value = "/run/dit-suite/postmortem-notice.txt")]
    notice: PathBuf,
}

#[derive(Debug, clap::Args)]
struct WatchArgs {
    #[arg(long, default_value = session::ROOT)]
    session_root: PathBuf,
    #[arg(long, default_value = "/proc")]
    proc_root: PathBuf,
    #[arg(long, default_value = "/dev/kmsg")]
    kmsg: PathBuf,
    /// The console the black box draws on; it switches to it when it writes a result.
    #[arg(long, default_value_t = 3)]
    console: u8,
    /// Do not switch consoles (tests).
    #[arg(long)]
    no_switch: bool,
}

/// A journal line at least this often while nothing changes.
const LINE_EVERY: Duration = Duration::from_secs(10);

/// A session the black box follows.
struct Watched {
    dir: PathBuf,
    info: SessionInfo,
    tracker: Tracker,
    journal: Journal,
    /// Kernel events since it started, with when they were read (Unix ms).
    kernel: Vec<(u64, KernelEvent)>,
    /// Kernel events not in the journal yet.
    unjournaled: Vec<KernelEvent>,
    last_line: Option<Instant>,
    last_digest: String,
    /// The last beat line, for the watch screen.
    last_beat: Option<Value>,
}

/// What the black box read of a layer's watchdog.
fn layer_seen(status_layer: &session::LayerStatus, all: &[Process]) -> LayerSeen {
    let record: Value = std::fs::read(status_layer.run_dir.join("watchdog-status.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let worker = record["worker_pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok());
    LayerSeen {
        name: status_layer.name.clone(),
        watchdog_state: record["state"].as_str().unwrap_or("").to_owned(),
        last_activity_ms: record["last_activity_unix_ms"].as_u64().unwrap_or(0),
        workers: worker.map_or_else(Vec::new, |pid| host::tree(all, pid)),
    }
}

fn layer_json(layer: &LayerSeen, now_ms: u64) -> Value {
    json!({
        "name": layer.name,
        "watchdog": layer.watchdog_state,
        "activity_age_s": (layer.last_activity_ms > 0)
            .then(|| now_ms.saturating_sub(layer.last_activity_ms) / 1000),
        "processes": layer.workers.len(),
        "waiting_in_kernel": layer.workers.iter().filter(|p| p.state == 'D').count(),
        "io_bytes": layer.workers.iter().filter_map(|p| p.io_bytes).sum::<u64>(),
        "cpu_ticks": layer.workers.iter().map(|p| p.cpu_ticks).sum::<u64>(),
        "rss_mib": layer.workers.iter().map(|p| p.rss_bytes).sum::<u64>() / (1024 * 1024),
    })
}

/// The live readings of the app's screen (temperatures, power, usage), when it has one.
fn meters(status: &SessionStatus) -> Value {
    status
        .dashboard_file
        .as_deref()
        .and_then(dit_ui_core::load_final_frame)
        .map_or(Value::Null, |screen| {
            Value::Array(
                screen
                    .live_meters
                    .iter()
                    .map(|meter| {
                        json!({
                            "label": meter.label,
                            "percent": meter.percent,
                            "detail": meter.detail,
                        })
                    })
                    .collect(),
            )
        })
}

/// A screen for the session when the app left none: its machine and test.
fn bare_screen(info: &SessionInfo, status: &SessionStatus) -> UnifiedDashboard {
    UnifiedDashboard {
        engine: "DIT-BLACKBOX".to_owned(),
        report_abi: "1".to_owned(),
        profile: info.csv.test.clone(),
        hardware_rows: vec![
            format!("CPU:     {}", info.csv.cpu),
            format!("RAM:     {}", info.csv.memory),
            format!("BOARD:   {}", info.csv.board),
        ],
        live_meters: Vec::new(),
        state: "DONE".to_owned(),
        progress_percent: None,
        elapsed: format!("{} s", status.elapsed_seconds),
        target: String::new(),
        active_test: status.phase.clone(),
        process: String::new(),
        calculations_completed: 0,
        failures: status.failures,
        running_rows: Vec::new(),
        footer: String::new(),
    }
}

fn mib(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// The result on the black box's console: the session's last screen, the verdict in big
/// letters, the failed parts, what happened and where the record went.
fn draw_result(
    info: &SessionInfo,
    status: &SessionStatus,
    seen: &Observation,
    cause: &Cause,
    written: &Written,
) {
    let mut screen = status
        .dashboard_file
        .as_deref()
        .and_then(dit_ui_core::load_final_frame)
        .unwrap_or_else(|| bare_screen(info, status));
    screen
        .running_rows
        .retain(|row| !row.starts_with("FAIL BY PART"));
    let mut rows = vec![
        format!(
            "RESULT: {}    written by the black box: the test could not write it",
            written.verdict
        ),
        format!("CAUSE    {}", cause.text),
        format!(
            "LAST     {} ({} s in); {} MiB available; memory pressure full {:.0} %",
            status.phase,
            status.elapsed_seconds,
            mib(seen.memory.available_bytes),
            seen.pressure.map_or(0.0, |p| p.full_avg10)
        ),
    ];
    let still: Vec<&str> = seen
        .layers
        .iter()
        .filter(|layer| !layer.workers.is_empty())
        .map(|layer| layer.name.as_str())
        .collect();
    if !still.is_empty() {
        rows.push(format!("STILL RUNNING  {}", still.join(", ")));
    }
    rows.push(written.fail_line.clone());
    rows.push(format!("REPORT   {}", written.report.display()));
    rows.push(format!("CSV      {}", written.csv));
    let frame = render_result_frame(
        &screen,
        &rows,
        "Written by the black box | Alt+F1: the test screen | Alt+F2: rescue console",
        TerminalSize::current(),
        true,
    );
    let mut out = io::stdout().lock();
    let _ = write!(out, "\x1b[H\x1b[2J{frame}");
    let _ = out.flush();
}

/// What the black box's console shows while it watches.
fn draw_watching(watched: &BTreeMap<String, Watched>, last: &BTreeMap<String, Value>) {
    let mut rows = Vec::new();
    if watched.is_empty() {
        rows.push(" No test is running.".to_owned());
    }
    for (run_id, session) in watched {
        rows.push(format!(" SESSION  {}  {run_id}", session.info.csv.test));
        if let Some(line) = last.get(run_id) {
            rows.push(format!(
                " NOW      {}  app heartbeat {} s ago  failures {}",
                line["phase"].as_str().unwrap_or(""),
                line["heartbeat_age_s"].as_u64().unwrap_or(0),
                line["failures"].as_u64().unwrap_or(0)
            ));
        }
        rows.push(format!(" JOURNAL  {}", session.journal.path().display()));
    }
    let text = option_screen(
        "DIT BLACK BOX // WATCHING (ALT+F1: THE TEST SCREEN)",
        &[
            rows,
            vec![
                " The black box keeps a journal of each test session. If the test cannot write its"
                    .to_owned(),
                " result (it was killed, froze, or a part stopped answering), the black box writes"
                    .to_owned(),
                " it and shows it here. It never stops or changes the test.".to_owned(),
            ],
        ],
        TerminalSize::current(),
    );
    let mut out = io::stdout().lock();
    let _ = write!(out, "\x1b[H\x1b[2J{}", decorate(&text));
    let _ = out.flush();
}

/// Brings the black box's console to the screen (once, when it writes a result): `chvt`, or
/// busybox's where the `kbd` package is not installed (the image has busybox, not `kbd`).
fn switch_console(console: u8) {
    static CHVT: StuckGate = StuckGate::new();
    let console = console.to_string();
    let switched = |program: &str, args: &[&str]| {
        output_within(
            Command::new(program).args(args),
            Duration::from_secs(3),
            &CHVT,
        )
        .is_some_and(|output| output.status.success())
    };
    if !switched("chvt", &[console.as_str()]) {
        let _ = switched("busybox", &["chvt", console.as_str()]);
    }
}

/// What one second showed of every session.
struct Moment<'a> {
    now: u64,
    all: &'a [Process],
    memory: host::Memory,
    pressure: Option<host::Pressure>,
    fresh: &'a [KernelEvent],
}

/// What became of a session after a second.
enum Followed {
    Watching,
    /// Its app wrote its result (or someone else did).
    Closed,
    /// The black box wrote it.
    Written,
}

/// One second of a session: its journal, and its result when the app cannot write it.
#[allow(clippy::too_many_lines)]
fn follow(
    args: &WatchArgs,
    limits: &Limits,
    run_id: &str,
    session: &mut Watched,
    moment: &Moment<'_>,
) -> Followed {
    let (now, memory, pressure) = (moment.now, moment.memory, moment.pressure);
    session
        .kernel
        .extend(moment.fresh.iter().cloned().map(|event| (now, event)));
    session.unjournaled.extend(moment.fresh.iter().cloned());
    let status = session::read_status(&session.dir).unwrap_or_default();
    let claimed = session::claimed_by(&session.info.results_root, &args.session_root, run_id);
    if status.state == session::CLOSED || claimed.is_some() {
        session.journal.write(&json!({
            "kind": "closed",
            "t": now,
            "by": claimed,
            "verdict": status.verdict,
        }));
        return Followed::Closed;
    }
    let seen = Observation {
        now_ms: moment.now,
        app_alive: host::alive(&args.proc_root, session.info.app_pid),
        layers: status
            .layers
            .iter()
            .map(|layer| layer_seen(layer, moment.all))
            .collect(),
        status: status.clone(),
        memory: moment.memory,
        pressure: moment.pressure,
        kernel: session.kernel.clone(),
    };
    let decision = session.tracker.step(&session.info, &seen, limits);
    let line = json!({
        "kind": "beat",
        "t": now,
        "phase": status.phase,
        "state": status.state,
        "elapsed_s": status.elapsed_seconds,
        "heartbeat_age_s": now.saturating_sub(status.heartbeat_unix_ms) / 1000,
        "app_alive": seen.app_alive,
        "failures": status.failures,
        "counts": status.counts,
        "layers": seen.layers.iter().map(|l| layer_json(l, now)).collect::<Vec<_>>(),
        "memory_available_mib": mib(memory.available_bytes),
        "memory_total_mib": mib(memory.total_bytes),
        "pressure": pressure,
        "kernel": session.unjournaled.iter().map(|e| json!({"event": e, "said": e.describe()})).collect::<Vec<_>>(),
        "meters": meters(&status),
    });
    // A line on every change, else every 10 s.
    // The phase without its running clock (`PHASE 6/6 OVERLAP (00:01:20 / ...)`): a line a
    // second would wear the stick for nothing.
    let digest = format!(
        "{}|{}|{}|{}|{:?}",
        status.phase.split(" (").next().unwrap_or(""),
        status.state,
        status.failures,
        seen.app_alive,
        seen.layers
            .iter()
            .map(|l| (l.name.clone(), l.watchdog_state.clone()))
            .collect::<Vec<_>>()
    );
    if digest != session.last_digest
        || !session.unjournaled.is_empty()
        || session
            .last_line
            .is_none_or(|at| at.elapsed() >= LINE_EVERY)
    {
        session.journal.write(&line);
        session.unjournaled.clear();
        session.last_line = Some(Instant::now());
        session.last_digest = digest;
    }
    session.last_beat = Some(line);
    match decision {
        Decision::Wait => {}
        Decision::Note(text) => session.journal.write(&json!({
            "kind": "note",
            "t": now,
            "text": text,
        })),
        Decision::Write(cause) => {
            let evidence = json!({
                "memory_available_mib": mib(memory.available_bytes),
                "pressure": pressure,
                "app_alive": seen.app_alive,
                "heartbeat_age_s": now.saturating_sub(status.heartbeat_unix_ms) / 1000,
                "layers": seen.layers.iter().map(|l| layer_json(l, now)).collect::<Vec<_>>(),
                "kernel": seen.kernel.iter().map(|(t, e)| json!({"t": t, "event": e, "said": e.describe()})).collect::<Vec<_>>(),
                "journal": session.journal.path(),
            });
            match outcome::write_result(
                &args.session_root,
                &session.dir,
                "blackbox",
                &cause,
                &evidence,
            ) {
                Ok(written) => {
                    session.journal.write(&json!({
                        "kind": "result",
                        "t": now,
                        "verdict": written.verdict,
                        "cause": cause,
                        "report": written.report,
                    }));
                    draw_result(&session.info, &status, &seen, &cause, &written);
                    if !args.no_switch {
                        switch_console(args.console);
                    }
                }
                Err(error) => session.journal.write(&json!({
                    "kind": "note",
                    "t": now,
                    "text": format!("result not written: {error}"),
                })),
            }
            return Followed::Written;
        }
    }
    Followed::Watching
}

fn watch(args: &WatchArgs) -> ExitCode {
    // It has to keep running while the machine is short of memory: every page stays in RAM.
    let _ = nix::sys::mman::mlockall(
        nix::sys::mman::MlockAllFlags::MCL_CURRENT | nix::sys::mman::MlockAllFlags::MCL_FUTURE,
    );
    let mut kernel = KernelLog::open(&args.kmsg);
    // The boot's own log is not about any session.
    let _ = kernel.read_new();
    let limits = Limits::default();
    let mut watched: BTreeMap<String, Watched> = BTreeMap::new();
    let mut last_lines: BTreeMap<String, Value> = BTreeMap::new();
    let mut showing_result = false;
    let mut last_draw: Option<Instant> = None;
    loop {
        let tick = Instant::now();
        let now = session::unix_ms();
        let fresh: Vec<KernelEvent> = kernel
            .read_new()
            .iter()
            .filter_map(kernel_events::classify)
            .collect();
        for dir in session::open_sessions(&args.session_root) {
            let Some(info) = session::read_info(&dir) else {
                continue;
            };
            watched.entry(info.run_id.clone()).or_insert_with(|| {
                // A new test: the console shows the watch again.
                showing_result = false;
                let mut journal = Journal::open(&info);
                journal.write(&json!({
                    "kind": "open",
                    "t": now,
                    "run_id": info.run_id,
                    "test": info.csv.test,
                    "app_pid": info.app_pid,
                    "session": info,
                }));
                Watched {
                    dir: dir.clone(),
                    info,
                    tracker: Tracker::default(),
                    journal,
                    kernel: Vec::new(),
                    unjournaled: Vec::new(),
                    last_line: None,
                    last_digest: String::new(),
                    last_beat: None,
                }
            });
        }
        let all = host::processes(&args.proc_root);
        let memory = host::memory(&args.proc_root);
        let pressure = host::pressure(&args.proc_root, "memory");
        let mut finished = Vec::new();
        let moment = Moment {
            now,
            all: &all,
            memory,
            pressure,
            fresh: &fresh,
        };
        for (run_id, session) in &mut watched {
            match follow(args, &limits, run_id, session, &moment) {
                Followed::Watching => {}
                Followed::Closed => finished.push(run_id.clone()),
                Followed::Written => {
                    finished.push(run_id.clone());
                    showing_result = true;
                }
            }
            if let Some(line) = session.last_beat.take() {
                last_lines.insert(run_id.clone(), line);
            }
        }
        for run_id in finished {
            watched.remove(&run_id);
            last_lines.remove(&run_id);
        }
        if !showing_result && last_draw.is_none_or(|at| at.elapsed() >= Duration::from_secs(5)) {
            draw_watching(&watched, &last_lines);
            last_draw = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_secs(1).saturating_sub(tick.elapsed()));
    }
}

/// The last beat's status, as the post-mortem gives it to the result.
fn status_of(beat: Option<&Value>) -> SessionStatus {
    let Some(beat) = beat else {
        return SessionStatus::default();
    };
    SessionStatus {
        heartbeat_unix_ms: beat["t"].as_u64().unwrap_or(0),
        state: beat["state"].as_str().unwrap_or("").to_owned(),
        phase: beat["phase"].as_str().unwrap_or("").to_owned(),
        elapsed_seconds: beat["elapsed_s"].as_u64().unwrap_or(0),
        failures: beat["failures"].as_u64().unwrap_or(0),
        counts: serde_json::from_value(beat["counts"].clone()).unwrap_or_default(),
        ..SessionStatus::default()
    }
}

/// Watchdog markers still saying a run is going on (`STARTING`, `RUNNING`, `STALLED`): closed
/// now, so the menu stops warning about them; the ones no journal covered are noted.
fn close_markers(results_root: &std::path::Path, handled: &[String], notices: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(results_root.join("watchdog")) else {
        return;
    };
    for path in entries.filter_map(Result::ok).map(|e| e.path()) {
        let Some(mut marker) = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        else {
            continue;
        };
        if !matches!(
            marker["state"].as_str(),
            Some("STARTING" | "RUNNING" | "STALLED")
        ) {
            continue;
        }
        let run_id = marker["run_id"].as_str().unwrap_or("").to_owned();
        if !handled.iter().any(|id| run_id.starts_with(id.as_str())) {
            notices.push(format!(
                "{} {run_id}: ended without a result and without a black box journal (watchdog marker {})",
                marker["module"].as_str().unwrap_or("test"),
                path.display()
            ));
        }
        marker["state"] = json!("CLOSED_AT_BOOT");
        marker["closed_by"] = json!("dit-blackbox post-mortem");
        marker["closed_unix_ms"] = json!(session::unix_ms());
        let tmp = path.with_extension("tmp");
        if serde_json::to_vec_pretty(&marker).is_ok_and(|bytes| std::fs::write(&tmp, bytes).is_ok())
        {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// `FC3b`: each session the previous boot left with an open journal gets its result from the
/// journal's last line, the board's reset reason and pstore.
fn post_mortem(args: &PostMortemArgs) -> ExitCode {
    if !args.results_root.is_dir() {
        return ExitCode::SUCCESS;
    }
    let kernel: Vec<String> = KernelLog::open(&args.kmsg)
        .read_new()
        .into_iter()
        .filter(host::KernelLine::from_kernel)
        .map(|line| line.message)
        .collect();
    let reason = dit_blackbox::postmortem::reset_reason(&kernel);
    let mut notices = Vec::new();
    let mut handled = Vec::new();
    for path in dit_blackbox::postmortem::open_journals(&args.results_root) {
        let lines = dit_blackbox::journal::read(&path);
        let Some(mut info) = lines
            .iter()
            .find(|line| line["kind"] == "open")
            .and_then(|line| serde_json::from_value::<SessionInfo>(line["session"].clone()).ok())
        else {
            continue;
        };
        // This boot's mount of the same volume.
        info.results_root.clone_from(&args.results_root);
        let beat = lines.iter().rev().find(|line| line["kind"] == "beat");
        let pstore = dit_blackbox::postmortem::read_pstore(&args.pstore, info.started_unix_ms);
        let cause = dit_blackbox::postmortem::classify(beat, reason.as_deref(), &pstore);
        let status = status_of(beat);
        let evidence = json!({
            "reset_reason": reason,
            "pstore": pstore,
            "last_beat": beat,
            "journal": path,
        });
        let written = outcome::write_for(
            std::path::Path::new(session::ROOT),
            &info,
            &status,
            &args.results_root,
            "postmortem",
            &cause,
            &evidence,
        );
        let mut journal = Journal::open(&info);
        journal.write(&json!({
            "kind": "postmortem",
            "t": session::unix_ms(),
            "verdict": written.as_ref().map(|w| w.verdict).ok(),
            "cause": cause,
            "reset_reason": reason,
        }));
        // Recorded: the NVRAM space goes back to the firmware.
        for (file, _) in &pstore.records {
            if file.starts_with("/sys/fs/pstore") {
                let _ = std::fs::remove_file(file);
            }
        }
        if let Ok(written) = written {
            notices.push(format!(
                "{} {}: {} - {}; last seen {} ({} s in). {}",
                info.csv.test,
                info.run_id,
                written.verdict,
                cause.text,
                status.phase,
                status.elapsed_seconds,
                written.csv
            ));
        }
        handled.push(info.run_id.clone());
    }
    close_markers(&args.results_root, &handled, &mut notices);
    nix::unistd::sync();
    if !notices.is_empty() {
        for notice in &notices {
            println!("PREVIOUS SESSION: {notice}");
        }
        if let Some(parent) = args.notice.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&args.notice, notices.join("\n") + "\n");
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Mode::Watch(args) => watch(&args),
        Mode::PostMortem(args) => post_mortem(&args),
    }
}
