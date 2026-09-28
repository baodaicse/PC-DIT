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
    let executable = args
        .worker
        .first()
        .ok_or("watchdog requires a worker executable after --")?;
    let operator_stop_requested = Arc::new(AtomicBool::new(false));
    let signal_flag = Arc::clone(&operator_stop_requested);
    ctrlc::set_handler(move || signal_flag.store(true, Ordering::SeqCst))?;
    // `setsid` gives every worker its own process group. A hard stale kill then
    // reaches shell pipelines and child workers instead of only their launcher.
    let mut command = Command::new("setsid");
    command
        .arg("--wait")
        .arg(executable)
        .args(&args.worker[1..]);
    let child = command.spawn()?;
    let started_unix_ms = unix_ms();
    let cgroup_events_path = memory_events_path(child.id());
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
    };
    supervisor.write_record(WatchdogState::Starting, None, "worker spawned")?;
    supervisor.write_record(WatchdogState::Running, None, "worker supervision active")?;
    let (state, status, detail) = supervisor.wait_for_completion()?;
    supervisor.write_record(state, status, &detail)?;
    println!(
        "DIT WATCHDOG // {} // {} // run={} // {}",
        state_label(state),
        args.module,
        args.run_id,
        detail
    );
    Ok(if state == WatchdogState::Passed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
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
        };
        write_json_atomic(&self.args.run_dir.join("watchdog-status.json"), &record)?;
        if state != WatchdogState::Starting
            && state != WatchdogState::Running
            && state != WatchdogState::Stalled
        {
            write_json_atomic(&self.args.run_dir.join("watchdog-final.json"), &record)?;
        }
        if let Some(marker) = &self.args.recovery_marker {
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
        WatchdogState::WorkerSignaled => A::Interrupted,
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
