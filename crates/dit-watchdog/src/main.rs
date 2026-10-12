use clap::{Parser, Subcommand};
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA_VERSION: u32 = 2;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(
    name = "dit-watchdog",
    version,
    about = "Independent DIT test supervisor"
)]
struct Cli {
    #[command(subcommand)]
    command: CommandLine,
}

#[derive(Debug, Subcommand)]
enum CommandLine {
    /// Spawn and independently monitor one DIT test worker.
    Supervise(SuperviseArgs),
}

#[derive(Debug, clap::Args)]
struct SuperviseArgs {
    /// Human-readable module identifier (cpu-ram, cpu-nvme, gpu-vram).
    #[arg(long)]
    module: String,
    /// Stable identifier for this test run.
    #[arg(long)]
    run_id: String,
    /// Runtime directory used only for telemetry and watchdog evidence.
    #[arg(long)]
    run_dir: PathBuf,
    /// A file atomically updated by the worker while it is executing.
    #[arg(long)]
    heartbeat: Option<PathBuf>,
    /// A worker-owned append/update file used where a structured heartbeat is unavailable.
    #[arg(long = "activity-path")]
    activity_paths: Vec<PathBuf>,
    /// Do not declare a seen heartbeat/activity stale before this many seconds.
    #[arg(long, default_value_t = 90, value_parser = clap::value_parser!(u64).range(5..=86_400))]
    stale_after_seconds: u64,
    /// After a stale condition, wait this long before forcibly ending the worker.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(0..=86_400))]
    kill_after_seconds: u64,
    /// Optional durable marker on the verified result partition. It is updated at start/finalization.
    #[arg(long)]
    recovery_marker: Option<PathBuf>,
    /// Runs the worker in a memory cgroup of its own under this one (`DIT_TEST_CGROUP` when not
    /// given; `docs/FAULT_CATCH_ROADMAP.md`, FC2): the image's test cgroup, whose `memory.max`
    /// keeps the tests from starving the machine, and whose OOM count is then this worker's own.
    /// Without a usable cgroup the worker runs where the watchdog does.
    #[arg(long)]
    cgroup_parent: Option<PathBuf>,
    /// Stops the worker before the machine runs out of memory (FC2): full memory pressure at or
    /// above `--memory-guard-full-percent`, or less than `--memory-guard-min-available-mib`
    /// available, for `--memory-guard-seconds` in a row.
    #[arg(long)]
    memory_guard: bool,
    #[arg(long, default_value_t = 20.0)]
    memory_guard_full_percent: f64,
    #[arg(long, default_value_t = 256)]
    memory_guard_min_available_mib: u64,
    #[arg(long, default_value_t = 5)]
    memory_guard_seconds: u64,
    /// Worker executable and arguments. Use `--` before this portion.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    worker: Vec<OsString>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum WatchdogState {
    Starting,
    Running,
    Stalled,
    Passed,
    WorkerFailed,
    WorkerSignaled,
    OomKilled,
    StalledKilled,
    /// Stopped by the memory guard before the machine ran out of memory (FC2).
    MemoryGuard,
}

#[derive(Debug, Serialize)]
struct WatchdogRecord {
    schema_version: u32,
    module: String,
    run_id: String,
    state: WatchdogState,
    availability: dit_system::availability::AvailabilityEvidence,
    power_platform: dit_system::power_platform::PowerPlatformEvidence,
    worker_pid: Option<u32>,
    started_unix_ms: u128,
    updated_unix_ms: u128,
    heartbeat_path: Option<String>,
    activity_paths: Vec<String>,
    last_activity_unix_ms: Option<u128>,
    stale_after_seconds: u64,
    cgroup_memory_events_before: BTreeMap<String, u64>,
    cgroup_memory_events_after: BTreeMap<String, u64>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    detail: String,
    /// The worker's own memory cgroup (FC2), when it runs in one.
    #[serde(skip_serializing_if = "Option::is_none")]
    cgroup: Option<String>,
}

struct Supervisor<'a> {
    args: &'a SuperviseArgs,
    child: Child,
    started_unix_ms: u128,
    cgroup_events_path: Option<PathBuf>,
    cgroup_events_before: BTreeMap<String, u64>,
    last_activity: Option<SystemTime>,
    stale_started: Option<SystemTime>,
    operator_stop_requested: Arc<AtomicBool>,
    cgroup: Option<PathBuf>,
    guard: Option<MemoryGuard>,
}

/// FC2: watches memory while the worker runs and says when to stop it.
struct MemoryGuard {
    full_percent: f64,
    min_available_bytes: u64,
    seconds: u64,
    /// Consecutive polls over a limit.
    strikes: u64,
}

impl MemoryGuard {
    /// One reading; `Some(detail)` once the limits have held for `seconds` polls in a row.
    fn check(&mut self, proc_root: &Path) -> Option<String> {
        let memory = dit_system::pressure::memory(proc_root);
        let full =
            dit_system::pressure::pressure(proc_root, "memory").map_or(0.0, |p| p.full_avg10);
        let short = memory.total_bytes > 0 && memory.available_bytes < self.min_available_bytes;
        if full >= self.full_percent || short {
            self.strikes += 1;
        } else {
            self.strikes = 0;
        }
        (self.strikes >= self.seconds).then(|| {
            format!(
                "memory guard: {} MiB available, full memory pressure {full:.1} % (10 s) for {} s; \
                 the worker was stopped before the machine stalled",
                memory.available_bytes / (1024 * 1024),
                self.strikes
            )
        })
    }
}

/// Creates the worker's memory cgroup under `parent` (FC2); `None` when it cannot be used.
fn layer_cgroup(parent: &Path, module: &str, run_id: &str) -> Option<PathBuf> {
    if !parent.join("cgroup.procs").is_file() {
        return None;
    }
    let name: String = format!("{module}-{run_id}-{}", std::process::id())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(120)
        .collect();
    let dir = parent.join(name);
    fs::create_dir(&dir).ok()?;
    Some(dir)
}

/// The worker under `setsid`, in its own process group (a hard stale kill then reaches shell
/// pipelines and child workers too), and in `cgroup` when given: the shell joins it before it
/// becomes the worker, so every process of the worker is counted there.
fn worker_command(args: &SuperviseArgs, cgroup: Option<&Path>) -> Result<Command, &'static str> {
    let executable = args
        .worker
        .first()
        .ok_or("watchdog requires a worker executable after --")?;
    let command = if let Some(dir) = cgroup {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(r#"{ echo 0 > "$0"; } 2>/dev/null; exec setsid --wait "$@""#)
            .arg(dir.join("cgroup.procs"))
            .arg(executable)
            .args(&args.worker[1..]);
        command
    } else {
        let mut command = Command::new("setsid");
        command
            .arg("--wait")
            .arg(executable)
            .args(&args.worker[1..]);
        command
    };
    Ok(command)
}

/// The watchdog's exit code: the worker's own when it exited (0 PASS, 2 FAIL, 3 INVALID for
/// the engines), else what ended it, so a caller tells a failed test from a test that could not
/// finish: 124 stalled and killed, 125 memory guard, 126 OOM kill, 130 operator stop, 128 + n
/// a signal n that crashed it (139 SIGSEGV, 135 SIGBUS, 132 SIGILL).
fn exit_code(
    state: WatchdogState,
    status: Option<std::process::ExitStatus>,
    operator_stop: bool,
) -> u8 {
    let own = status
        .and_then(|s| s.code())
        .and_then(|c| u8::try_from(c).ok());
    let signal = status
        .and_then(|s| s.signal())
        .and_then(|n| u8::try_from(n).ok());
    match state {
        WatchdogState::Passed => 0,
        WatchdogState::WorkerFailed => own.filter(|c| *c != 0).unwrap_or(1),
        WatchdogState::WorkerSignaled if operator_stop => 130,
        WatchdogState::WorkerSignaled => signal.map_or(130, |n| 128_u8.saturating_add(n)),
        WatchdogState::OomKilled => 126,
        WatchdogState::StalledKilled => 124,
        WatchdogState::MemoryGuard => 125,
        WatchdogState::Starting | WatchdogState::Running | WatchdogState::Stalled => 2,
    }
}

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("DIT WATCHDOG ERROR: {error}");
            ExitCode::from(2)
        }
    }
}

fn execute(cli: Cli) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cli.command {
        CommandLine::Supervise(args) => supervise(&args),
    }
}

fn supervise(args: &SuperviseArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    fs::create_dir_all(&args.run_dir)?;
    let operator_stop_requested = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&operator_stop_requested);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::SeqCst))?;
    let cgroup = args
        .cgroup_parent
        .clone()
        .or_else(|| std::env::var_os("DIT_TEST_CGROUP").map(PathBuf::from))
        .and_then(|parent| layer_cgroup(&parent, &args.module, &args.run_id));
    let child = worker_command(args, cgroup.as_deref())?.spawn()?;
    let started_unix_ms = unix_ms();
    // The kernel gives a real cgroup its files; a directory without them is not one.
    let cgroup = cgroup.filter(|dir| dir.join("memory.events").is_file());
    // The worker's own cgroup counts its OOM kills only; otherwise the one it runs in.
    let cgroup_events_path = cgroup
        .as_ref()
        .map(|dir| dir.join("memory.events"))
        .or_else(|| memory_events_path(child.id()));
    let cgroup_events_before = cgroup_events_path
        .as_deref()
        .map_or_else(BTreeMap::new, read_memory_events);
    let mut supervisor = Supervisor {
        args,
        child,
        started_unix_ms,
        cgroup_events_path,
        cgroup_events_before,
        last_activity: latest_activity(args),
        stale_started: None,
        operator_stop_requested,
        cgroup,
        guard: args.memory_guard.then(|| MemoryGuard {
            full_percent: args.memory_guard_full_percent,
            min_available_bytes: args.memory_guard_min_available_mib * 1024 * 1024,
            seconds: args.memory_guard_seconds.max(1),
            strikes: 0,
        }),
    };
    supervisor.write_record(WatchdogState::Starting, None, "worker spawned")?;
    supervisor.write_record(WatchdogState::Running, None, "worker supervision active")?;
    let (state, status, detail) = supervisor.wait_for_completion()?;
    supervisor.write_record(state, status, &detail)?;
    if let Some(dir) = &supervisor.cgroup {
        // Empty once the worker is gone; a process left in it keeps it (and says so).
        let _ = fs::remove_dir(dir);
    }
    println!(
        "DIT WATCHDOG // {} // {} // run={} // {}",
        state_label(state),
        args.module,
        args.run_id,
        detail
    );
    let operator_stop = supervisor.operator_stop_requested.load(Ordering::SeqCst);
    Ok(ExitCode::from(exit_code(state, status, operator_stop)))
}

impl Supervisor<'_> {
    fn wait_for_completion(
        &mut self,
    ) -> Result<(WatchdogState, Option<std::process::ExitStatus>, String), Box<dyn std::error::Error>>
    {
        loop {
            if self.operator_stop_requested.load(Ordering::SeqCst) {
                terminate_process_group(self.child.id(), "TERM")?;
                thread::sleep(Duration::from_secs(2));
                if self.child.try_wait()?.is_none() {
                    terminate_process_group(self.child.id(), "KILL")?;
                }
                let status = self.child.wait()?;
                return Ok((
                    WatchdogState::WorkerSignaled,
                    Some(status),
                    "operator requested stop; watchdog terminated the worker process group"
                        .to_owned(),
                ));
            }
            if let Some(status) = self.child.try_wait()? {
                let state =
                    classify_exit(status, &self.cgroup_events_before, &self.cgroup_events());
                let detail = exit_detail(status, state);
                return Ok((state, Some(status), detail));
            }
            if let Some(detail) = self
                .guard
                .as_mut()
                .and_then(|guard| guard.check(Path::new("/proc")))
            {
                eprintln!(
                    "DIT WATCHDOG WARNING: {} run {}: {detail}",
                    self.args.module, self.args.run_id
                );
                terminate_process_group(self.child.id(), "TERM")?;
                for _ in 0..10 {
                    if self.child.try_wait()?.is_some() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(500));
                }
                if self.child.try_wait()?.is_none() {
                    terminate_process_group(self.child.id(), "KILL")?;
                }
                let status = self.child.wait()?;
                return Ok((WatchdogState::MemoryGuard, Some(status), detail));
            }

            if let Some(activity) = latest_activity(self.args)
                && self
                    .last_activity
                    .is_none_or(|previous| activity > previous)
            {
                self.last_activity = Some(activity);
                self.stale_started = None;
                self.write_record(
                    WatchdogState::Running,
                    None,
                    "worker heartbeat/activity observed",
                )?;
            }

            // A never-created heartbeat must not kill an interactive worker before it
            // has entered its workload. Once any evidence is seen, it becomes a hard
            // liveness contract for the rest of the run.
            if let Some(last_activity) = self.last_activity
                && last_activity.elapsed().unwrap_or_default()
                    >= Duration::from_secs(self.args.stale_after_seconds)
            {
                let newly_stalled = self.stale_started.is_none();
                let first_stale = self.stale_started.get_or_insert_with(SystemTime::now);
                let elapsed = first_stale.elapsed().unwrap_or_default();
                if newly_stalled {
                    eprintln!(
                        "DIT WATCHDOG WARNING: {} run {} has stale heartbeat/activity; grace countdown started.",
                        self.args.module, self.args.run_id
                    );
                }
                self.write_record(
                    WatchdogState::Stalled,
                    None,
                    "worker heartbeat/activity is stale; grace period is active",
                )?;
                if elapsed >= Duration::from_secs(self.args.kill_after_seconds) {
                    terminate_process_group(self.child.id(), "KILL")?;
                    let status = self.child.wait()?;
                    return Ok((
                        WatchdogState::StalledKilled,
                        Some(status),
                        "worker was forcibly ended after stale heartbeat/activity evidence"
                            .to_owned(),
                    ));
                }
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn write_record(
        &self,
        state: WatchdogState,
        status: Option<std::process::ExitStatus>,
        detail: &str,
    ) -> io::Result<()> {
        let cgroup_events_after = self.cgroup_events();
        let record = WatchdogRecord {
            schema_version: SCHEMA_VERSION,
            module: self.args.module.clone(),
            run_id: self.args.run_id.clone(),
            state,
            availability: dit_system::availability::AvailabilityEvidence::new(
                availability_status(state),
                detail,
            ),
            power_platform: dit_system::power_platform::PowerPlatformEvidence::default(),
            worker_pid: Some(self.child.id()),
            started_unix_ms: self.started_unix_ms,
            updated_unix_ms: unix_ms(),
            heartbeat_path: self
                .args
                .heartbeat
                .as_ref()
                .map(|path| path.display().to_string()),
            activity_paths: self
                .args
                .activity_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            last_activity_unix_ms: self.last_activity.and_then(system_time_ms),
            stale_after_seconds: self.args.stale_after_seconds,
            cgroup_memory_events_before: self.cgroup_events_before.clone(),
            cgroup_memory_events_after: cgroup_events_after,
            exit_code: status.and_then(|value| value.code()),
            exit_signal: status.and_then(|value| value.signal()),
            detail: detail.to_owned(),
            cgroup: self.cgroup.as_ref().map(|dir| dir.display().to_string()),
        };
        write_json_atomic(&self.args.run_dir.join("watchdog-status.json"), &record)?;
        if state != WatchdogState::Starting
            && state != WatchdogState::Running
            && state != WatchdogState::Stalled
        {
            write_json_atomic(&self.args.run_dir.join("watchdog-final.json"), &record)?;
        }
        // The durable marker is written when the run starts and when it ends, not at every sign
        // of life: rewriting a file on a FAT stick every second left empty markers after a power
        // loss. The black box's append-only journal follows the run in between (FC3a).
        if let Some(marker) = &self.args.recovery_marker
            && state != WatchdogState::Running
            && state != WatchdogState::Stalled
        {
            write_json_atomic(marker, &record)?;
        }
        Ok(())
    }

    fn cgroup_events(&self) -> BTreeMap<String, u64> {
        self.cgroup_events_path
            .as_deref()
            .map_or_else(BTreeMap::new, read_memory_events)
    }
}

fn terminate_process_group(pid: u32, signal: &str) -> io::Result<()> {
    let group = format!("-{pid}");
    let status = Command::new("/bin/kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(group)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "could not signal worker process group {pid} with {signal}"
        )))
    }
}

fn availability_status(state: WatchdogState) -> dit_system::availability::AvailabilityStatus {
    use dit_system::availability::AvailabilityStatus as A;
    match state {
        WatchdogState::Starting | WatchdogState::Running => A::Running,
        WatchdogState::Passed => A::Completed,
        WatchdogState::Stalled | WatchdogState::StalledKilled => A::Stalled,
        WatchdogState::WorkerSignaled | WatchdogState::MemoryGuard => A::Interrupted,
        WatchdogState::WorkerFailed | WatchdogState::OomKilled => A::WorkerFailed,
    }
}

fn latest_activity(args: &SuperviseArgs) -> Option<SystemTime> {
    args.heartbeat
        .iter()
        .chain(args.activity_paths.iter())
        .filter_map(|path| path.metadata().ok()?.modified().ok())
        .max()
}

fn memory_events_path(pid: u32) -> Option<PathBuf> {
    let Ok(cgroup) = fs::read_to_string(format!("/proc/{pid}/cgroup")) else {
        return None;
    };
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    Some(
        Path::new("/sys/fs/cgroup")
            .join(path.trim_start_matches('/'))
            .join("memory.events"),
    )
}

fn read_memory_events(events_path: &Path) -> BTreeMap<String, u64> {
    let Ok(contents) = fs::read_to_string(events_path) else {
        return BTreeMap::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.to_owned(), fields.next()?.parse().ok()?))
        })
        .collect()
}

fn classify_exit(
    status: std::process::ExitStatus,
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
) -> WatchdogState {
    if counter_increased(before, after, "oom_kill") {
        WatchdogState::OomKilled
    } else if status.success() {
        WatchdogState::Passed
    } else if status.signal().is_some() {
        WatchdogState::WorkerSignaled
    } else {
        WatchdogState::WorkerFailed
    }
}

fn counter_increased(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
    key: &str,
) -> bool {
    after.get(key).copied().unwrap_or(0) > before.get(key).copied().unwrap_or(0)
}

fn exit_detail(status: std::process::ExitStatus, state: WatchdogState) -> String {
    match state {
        WatchdogState::Passed => "worker exited successfully".to_owned(),
        WatchdogState::OomKilled => {
            "cgroup memory.events recorded oom_kill during this run".to_owned()
        }
        WatchdogState::WorkerSignaled => format!(
            "worker ended from signal {} (no cgroup OOM evidence)",
            status.signal().unwrap_or_default()
        ),
        WatchdogState::WorkerFailed => format!(
            "worker exited with code {}",
            status.code().unwrap_or_default()
        ),
        _ => "worker stopped".to_owned(),
    }
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("watchdog path has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut file = File::create(&tmp)?;
    serde_json::to_writer_pretty(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(tmp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

fn system_time_ms(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

fn state_label(state: WatchdogState) -> &'static str {
    match state {
        WatchdogState::Starting => "STARTING",
        WatchdogState::Running => "RUNNING",
        WatchdogState::Stalled => "STALLED",
        WatchdogState::Passed => "PASS",
        WatchdogState::WorkerFailed => "WORKER_FAILED",
        WatchdogState::WorkerSignaled => "WORKER_SIGNALED",
        WatchdogState::OomKilled => "OOM_KILLED",
        WatchdogState::StalledKilled => "STALLED_KILLED",
        WatchdogState::MemoryGuard => "MEMORY_GUARD",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_classifier_prefers_cgroup_oom_evidence() {
        let status = Command::new("/bin/sh")
            .args(["-c", "exit 1"])
            .status()
            .unwrap();
        let before = BTreeMap::from([("oom_kill".to_owned(), 4)]);
        let after = BTreeMap::from([("oom_kill".to_owned(), 5)]);
        assert_eq!(
            classify_exit(status, &before, &after),
            WatchdogState::OomKilled
        );
    }

    #[test]
    fn the_memory_guard_trips_only_after_its_seconds_in_a_row() {
        let root = std::env::temp_dir().join(format!("dit-guard-{}", std::process::id()));
        fs::create_dir_all(root.join("pressure")).unwrap();
        let readings = |full: f64, available_kib: u64| {
            fs::write(
                root.join("pressure/memory"),
                format!("some avg10=50.00 avg60=0 avg300=0 total=1\nfull avg10={full:.2} avg60=0 avg300=0 total=1\n"),
            )
            .unwrap();
            fs::write(
                root.join("meminfo"),
                format!("MemTotal: 16000000 kB\nMemAvailable: {available_kib} kB\n"),
            )
            .unwrap();
        };
        let mut guard = MemoryGuard {
            full_percent: 20.0,
            min_available_bytes: 256 * 1024 * 1024,
            seconds: 3,
            strikes: 0,
        };
        readings(35.0, 8_000_000);
        assert!(guard.check(&root).is_none());
        assert!(guard.check(&root).is_none());
        // One calm reading starts the count again.
        readings(1.0, 8_000_000);
        assert!(guard.check(&root).is_none());
        readings(1.0, 100_000);
        assert!(guard.check(&root).is_none());
        assert!(guard.check(&root).is_none());
        let detail = guard.check(&root).expect("tripped");
        assert!(
            detail.starts_with("memory guard: 97 MiB available"),
            "{detail}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_exit_code_tells_a_failed_test_from_one_that_could_not_finish() {
        let code = |shell: &str| {
            Command::new("/bin/sh")
                .args(["-c", shell])
                .status()
                .unwrap()
        };
        let exited = |n: u8| {
            exit_code(
                WatchdogState::WorkerFailed,
                Some(code(&format!("exit {n}"))),
                false,
            )
        };
        assert_eq!(
            exit_code(WatchdogState::Passed, Some(code("exit 0")), false),
            0
        );
        assert_eq!(exited(2), 2);
        assert_eq!(exited(3), 3);
        assert_eq!(exit_code(WatchdogState::StalledKilled, None, false), 124);
        assert_eq!(exit_code(WatchdogState::MemoryGuard, None, false), 125);
        assert_eq!(exit_code(WatchdogState::OomKilled, None, false), 126);
        let segfault = code("kill -SEGV $$");
        assert_eq!(
            exit_code(WatchdogState::WorkerSignaled, Some(segfault), false),
            139
        );
        assert_eq!(
            exit_code(WatchdogState::WorkerSignaled, Some(segfault), true),
            130
        );
    }

    #[test]
    fn a_worker_joins_its_cgroup_before_it_starts() {
        // A directory standing in for a cgroup: the shell writes `0` to its cgroup.procs.
        let parent = std::env::temp_dir().join(format!("dit-cgroup-{}", std::process::id()));
        fs::create_dir_all(&parent).unwrap();
        fs::write(parent.join("cgroup.procs"), "").unwrap();
        let dir = layer_cgroup(&parent, "cpu-ram", "suite/1").expect("created");
        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("cpu-ram-suite_1-")
        );
        // The kernel creates cgroup.procs in a real cgroup; here the test does.
        fs::write(dir.join("cgroup.procs"), "").unwrap();
        let Cli {
            command: CommandLine::Supervise(args),
        } = Cli::parse_from([
            "dit-watchdog",
            "supervise",
            "--module",
            "m",
            "--run-id",
            "r",
            "--run-dir",
            "/tmp",
            "--",
            "/bin/true",
        ]);
        let status = worker_command(&args, Some(&dir)).unwrap().status().unwrap();
        assert!(status.success());
        assert_eq!(fs::read_to_string(dir.join("cgroup.procs")).unwrap(), "0\n");
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn exit_classifier_keeps_normal_failure_distinct() {
        let status = Command::new("/bin/sh")
            .args(["-c", "exit 9"])
            .status()
            .unwrap();
        assert_eq!(
            classify_exit(status, &BTreeMap::new(), &BTreeMap::new()),
            WatchdogState::WorkerFailed
        );
    }
}
