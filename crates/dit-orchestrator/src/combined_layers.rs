//! The four layers of the combined run as child processes: the command each phase starts, what
//! its progress file says while it runs, what its report says when it ends, and how much host
//! memory its process tree holds.
use super::{GpuReportView, MenuArgs};
use dit_contracts::{Event, EventEnvelope, Verdict};
use dit_orchestrator::combined::{Layer, LayerOutcome};
use dit_orchestrator::{adapt_cpu_ram_report, adapt_nvme_report};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[must_use]
pub const fn slug(layer: Layer) -> &'static str {
    match layer {
        Layer::CpuRam => "cpu-ram",
        Layer::Gpu => "gpu-vram",
        Layer::Ssd => "cpu-nvme",
        Layer::Chain => "chain",
    }
}

/// Storage prepared by `cpu-nvme-dit combined-storage` (`storage-plan.json`).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StoragePlan {
    pub state: String,
    pub selection: Option<PathBuf>,
    #[serde(default)]
    pub targets: Vec<String>,
    #[serde(default)]
    pub confirm_device_ids: Vec<String>,
    pub write_level: String,
    pub capacity_mib: u64,
    pub write_budget_mib: u64,
    pub reserve_free_mib: u64,
    pub chain_dir: Option<PathBuf>,
    pub chain_working_set_mib: u64,
    pub chain_note: Option<String>,
    #[serde(default)]
    pub devices: Vec<PlannedDevice>,
    pub holder_pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PlannedDevice {
    pub alias: usize,
    pub stable_id: String,
    pub model: String,
    pub capacity_bytes: u64,
    pub block_name: String,
    pub directory: PathBuf,
}

/// A program with arguments and environment, to be started under the watchdog.
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
    pub current_dir: Option<PathBuf>,
    /// Structured heartbeat for the watchdog, or an append-only activity file.
    pub heartbeat: Option<PathBuf>,
    pub activity: Option<PathBuf>,
}

fn words(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

/// The CPU-RAM source vectors: next to the binary in the image, `vectors/` in a checkout.
fn cpu_vectors(cpu_cli: &Path) -> (PathBuf, PathBuf) {
    let beside = cpu_cli
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("vectors");
    let base = if beside.join("source-v1.dat").is_file() {
        beside
    } else {
        PathBuf::from("vectors")
    };
    (
        base.join("source-v1.dat"),
        base.join("source-v1.manifest.json"),
    )
}

/// CPU-RAM memory: all but a reserve when alone, the arbiter's remainder in the overlap.
pub enum CpuMemory {
    Alone,
    Arena { mib: u64 },
}

pub fn cpu_ram(
    menu: &MenuArgs,
    dir: &Path,
    seconds: u64,
    workers: usize,
    oracle: usize,
    memory: &CpuMemory,
    reserve_mib: u64,
) -> Launch {
    let (source, manifest) = cpu_vectors(&menu.cpu_cli);
    let mut args = vec![
        OsString::from("system-run"),
        "--source".into(),
        source.into(),
    ];
    args.extend(["--manifest".into(), manifest.into()]);
    args.extend(words(&[
        "--arena-mib",
        "32",
        "--minimum-arena-mib",
        "4",
        "--lock-policy",
        "best-effort",
        "--monitor-interval-ms",
        "250",
        "--rounds",
        "1",
    ]));
    args.extend([
        "--progress-report".into(),
        dir.join("progress.json").into(),
        "--report".into(),
        dir.join("system-report.json").into(),
        "--workers".into(),
        workers.to_string().into(),
        "--compute-oracle-workers".into(),
        oracle.to_string().into(),
        "--minimum-duration-seconds".into(),
        seconds.to_string().into(),
        "--deadline-seconds".into(),
        (seconds + 900).to_string().into(),
    ]);
    match memory {
        CpuMemory::Alone => {
            args.extend(words(&["--safe-percent", "90"]));
            args.extend(["--reserve-mib".into(), reserve_mib.to_string().into()]);
        }
        CpuMemory::Arena { mib } => {
            // The arena already leaves the arbiter's reserve free; the engine's own check keeps
            // half of it, so memory moving between the measurement and the start is absorbed.
            args.extend(["--memory-mib".into(), mib.to_string().into()]);
            let engine_reserve = (reserve_mib / 2).min(512);
            args.extend(["--reserve-mib".into(), engine_reserve.to_string().into()]);
        }
    }
    Launch {
        program: menu.cpu_cli.clone(),
        args,
        env: Vec::new(),
        current_dir: None,
        heartbeat: Some(dir.join("progress.json")),
        activity: None,
    }
}

pub struct GpuOptions {
    pub temp_limit: bool,
    pub verifier_max: Option<usize>,
    pub host_budget_bytes: u64,
}

pub fn gpu(menu: &MenuArgs, dir: &Path, seconds: u64, options: &GpuOptions) -> Launch {
    let mut args = vec![
        OsString::from("run"),
        "--output-dir".into(),
        dir.into(),
        "--progress-file".into(),
        dir.join("progress.json").into(),
        "--bottleneck-log".into(),
        dir.join("bottleneck.jsonl").into(),
        "--duration-seconds".into(),
        seconds.to_string().into(),
    ];
    // The menu's Full profile, on every eligible GPU.
    args.extend(words(&[
        "--no-ui",
        "--devices",
        "all",
        "--async-auto",
        "--vram-max",
        "--vram-sweep",
        "--compute-mix",
        "alternate",
    ]));
    if options.temp_limit {
        args.extend(words(&["--temp-limit-celsius", "90"]));
    }
    if let Some(workers) = options.verifier_max {
        args.extend(["--async-workers-max".into(), workers.to_string().into()]);
    }
    Launch {
        program: menu.gpu_binary.clone(),
        args,
        env: vec![(
            "DIT_GPU_HOST_BUDGET_BYTES".to_owned(),
            options.host_budget_bytes.to_string(),
        )],
        current_dir: None,
        heartbeat: Some(dir.join("progress.json")),
        activity: None,
    }
}

pub fn ssd(
    menu: &MenuArgs,
    dir: &Path,
    seconds: u64,
    storage: &StoragePlan,
    write_budget_mib: u64,
    workers: Option<usize>,
    seed: u64,
) -> Launch {
    let mut args = vec![OsString::from("run-preserving-multi")];
    if let Some(selection) = &storage.selection {
        args.extend(["--selection".into(), selection.into()]);
    }
    for target in &storage.targets {
        args.extend(["--target".into(), target.into()]);
    }
    for id in &storage.confirm_device_ids {
        args.extend(["--confirm-device-id".into(), id.into()]);
    }
    for device in &menu.protected_devices {
        args.extend(["--protected-device".into(), device.into()]);
    }
    args.extend([
        "--capacity-mib".into(),
        storage.capacity_mib.to_string().into(),
        "--write-budget-mib".into(),
        write_budget_mib.to_string().into(),
        "--reserve-free-mib".into(),
        storage.reserve_free_mib.to_string().into(),
        "--duration-seconds".into(),
        seconds.to_string().into(),
        "--seed".into(),
        seed.to_string().into(),
        "--no-ui".into(),
        "--events".into(),
        dir.join("events.jsonl").into(),
        "--report".into(),
        dir.join("report.json").into(),
    ]);
    if let Some(workers) = workers {
        args.extend(["--max-workers".into(), workers.to_string().into()]);
    }
    Launch {
        program: menu.nvme_binary.clone(),
        args,
        env: Vec::new(),
        current_dir: None,
        heartbeat: None,
        activity: Some(dir.join("events.jsonl")),
    }
}

pub enum ChainWork<'a> {
    Prefill {
        dir: &'a Path,
        working_set_mib: u64,
    },
    Read {
        file: &'a Path,
        seconds: u64,
        generations: u64,
    },
}

pub fn chain(menu: &MenuArgs, dir: &Path, work: &ChainWork<'_>) -> Launch {
    let mut args = match work {
        ChainWork::Prefill {
            dir: target,
            working_set_mib,
        } => vec![
            OsString::from("prefill"),
            "--dir".into(),
            (*target).into(),
            "--working-set-mib".into(),
            working_set_mib.to_string().into(),
        ],
        ChainWork::Read {
            file,
            seconds,
            generations,
        } => vec![
            OsString::from("run"),
            "--existing".into(),
            (*file).into(),
            "--keep-file".into(),
            "--duration-seconds".into(),
            seconds.to_string().into(),
            "--generations-per-load".into(),
            generations.to_string().into(),
        ],
    };
    args.extend([
        "--report".into(),
        dir.join("report.json").into(),
        "--progress-file".into(),
        dir.join("progress.json").into(),
    ]);
    Launch {
        program: menu.chain_binary.clone(),
        args,
        env: Vec::new(),
        current_dir: None,
        heartbeat: Some(dir.join("progress.json")),
        activity: None,
    }
}

/// GPUs the GPU engine itself will test (`hardware_eligible` in its inventory; a BMC VGA or a
/// software renderer is not), and whether one of them runs CUDA (needed by the data chain).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuEligibility {
    pub pci_addresses: Vec<String>,
    pub cuda: bool,
}

#[must_use]
pub fn parse_gpu_inventory(json: &serde_json::Value) -> GpuEligibility {
    let eligible = |key: &str| -> Vec<String> {
        json.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|devices| {
                devices
                    .iter()
                    .filter(|d| d.get("hardware_eligible") == Some(&serde_json::Value::Bool(true)))
                    .map(|d| str_at(d, "pci_address").to_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    GpuEligibility {
        pci_addresses: eligible("auto_devices"),
        cuda: !eligible("cuda_devices").is_empty(),
    }
}

#[must_use]
pub fn gpu_eligibility(binary: &Path) -> GpuEligibility {
    std::process::Command::new(binary)
        .arg("inventory")
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<serde_json::Value>(&output.stdout).ok())
        .map(|json| parse_gpu_inventory(&json))
        .unwrap_or_default()
}

/// What a running layer reports right now.
#[derive(Clone, Debug, Default)]
pub struct LiveLayer {
    pub state: String,
    pub detail: String,
    pub failures: u64,
    pub calculations: u64,
    /// Every allocation of the phase is done (overlap start order waits on it).
    pub ready: bool,
    /// Chain only: its declared host peak and read rate.
    pub declared_peak_bytes: Option<u64>,
    pub read_gibps: Option<f64>,
    pub generations: Option<u64>,
    pub loads: Option<u64>,
    pub hardware_note: Option<String>,
}

fn json(path: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn u64_at(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn str_at<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

#[allow(clippy::cast_precision_loss)]
fn cpu_live(dir: &Path) -> LiveLayer {
    let Some(progress) = json(&dir.join("progress.json")) else {
        return LiveLayer {
            state: "STARTING".into(),
            ..LiveLayer::default()
        };
    };
    LiveLayer {
        state: str_at(&progress, "phase").to_ascii_uppercase(),
        detail: format!(
            "round {} | arena {:.1} GiB | {} calc",
            u64_at(&progress, "round"),
            u64_at(&progress, "target_bytes") as f64 / f64::from(1 << 30),
            u64_at(&progress, "calculations_completed")
        ),
        failures: u64_at(&progress, "failures_total"),
        calculations: u64_at(&progress, "calculations_completed"),
        ready: true,
        ..LiveLayer::default()
    }
}

/// GPU progress snapshots: one file for one GPU, one per `device-N/` with several.
fn gpu_snapshots(dir: &Path) -> Vec<serde_json::Value> {
    if let Some(single) = json(&dir.join("progress.json")) {
        return vec![single];
    }
    let mut devices: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path().join("progress.json"))
                .filter(|path| {
                    path.parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name.to_string_lossy().starts_with("device-"))
                })
                .collect()
        })
        .unwrap_or_default();
    devices.sort();
    devices.iter().filter_map(|path| json(path)).collect()
}

#[allow(clippy::cast_precision_loss)]
fn gpu_live(dir: &Path) -> LiveLayer {
    let snapshots = gpu_snapshots(dir);
    if snapshots.is_empty() {
        return LiveLayer {
            state: "PREPARING".into(),
            ..LiveLayer::default()
        };
    }
    let generation = snapshots
        .iter()
        .map(|s| u64_at(s, "generation"))
        .min()
        .unwrap_or(0);
    let allocated: u64 = snapshots
        .iter()
        .map(|s| u64_at(s, "allocation_bytes"))
        .sum();
    let transfer: f64 = snapshots
        .iter()
        .filter_map(|s| s.get("transfer_mib_s").and_then(serde_json::Value::as_f64))
        .map(|rate| rate.max(0.0))
        .sum();
    LiveLayer {
        state: str_at(&snapshots[0], "state").to_owned(),
        detail: format!(
            "{} GPU | gen {generation} | VRAM test {:.1} GiB | H2D+D2H {transfer:.0} MiB/s",
            snapshots.len(),
            allocated as f64 / f64::from(1 << 30)
        ),
        failures: snapshots.iter().map(|s| u64_at(s, "failures")).sum(),
        calculations: snapshots
            .iter()
            .map(|s| u64_at(s, "matrix_results_verified"))
            .sum(),
        ready: generation >= 1,
        hardware_note: snapshots.iter().find_map(|s| {
            s.get("hardware_note")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }),
        ..LiveLayer::default()
    }
}

#[allow(clippy::cast_precision_loss)]
fn chain_live(dir: &Path) -> LiveLayer {
    let Some(progress) = json(&dir.join("progress.json")) else {
        return LiveLayer {
            state: "STARTING".into(),
            ..LiveLayer::default()
        };
    };
    let phase = str_at(&progress, "phase").to_ascii_uppercase();
    let read = progress
        .get("read_gib_per_second")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let detail = if phase == "PREFILL" {
        format!(
            "records {}/{} written, answers computed twice",
            u64_at(&progress, "records_written"),
            u64_at(&progress, "records")
        )
    } else {
        format!(
            "loads {} | read {read:.2} GiB/s | GPU {:.1} Gop/s | G {}",
            u64_at(&progress, "loads"),
            progress
                .get("gpu_gops_verified")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0),
            u64_at(&progress, "generations_per_load")
        )
    };
    LiveLayer {
        state: phase,
        detail,
        failures: u64_at(&progress, "failures"),
        calculations: u64_at(&progress, "loads"),
        ready: progress
            .get("ready")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        declared_peak_bytes: Some(u64_at(&progress, "host_peak_bytes")),
        read_gibps: Some(read),
        generations: Some(u64_at(&progress, "generations_per_load")),
        loads: Some(u64_at(&progress, "loads")),
        hardware_note: None,
    }
}

/// Incremental tally of the SSD engine's event journal (read from where the last call stopped).
#[derive(Debug, Default)]
pub struct SsdTally {
    offset: u64,
    pending: String,
    devices: BTreeMap<String, SsdDevice>,
    pub events: u64,
    /// Failure events read since the caller last took them (step F1), as fault records.
    pub records: Vec<dit_contracts::FaultRecord>,
}

#[derive(Debug, Default, Clone)]
struct SsdDevice {
    completed: u64,
    target: u64,
    /// Cumulative count from progress events, and the failure events seen; the larger counts.
    failures: u64,
    failure_events: u64,
    matrix_checks: u64,
    stage: String,
}

impl SsdTally {
    pub fn update(&mut self, path: &Path) {
        let Ok(mut file) = std::fs::File::open(path) else {
            return;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut chunk = String::new();
        let Ok(read) = file.read_to_string(&mut chunk) else {
            return;
        };
        self.offset += read as u64;
        self.pending.push_str(&chunk);
        let complete = self.pending.rfind('\n').map_or(0, |at| at + 1);
        let lines: String = self.pending.drain(..complete).collect();
        for line in lines.lines() {
            let Ok(envelope) = serde_json::from_str::<EventEnvelope>(line) else {
                continue;
            };
            self.events += 1;
            let Some(id) = envelope.device_id.as_ref() else {
                continue;
            };
            let stable_id = id.0.clone();
            let device = self.devices.entry(stable_id.clone()).or_default();
            match envelope.event {
                Event::Progress {
                    completed_operations,
                    target_operations,
                    failures,
                    ..
                } => {
                    device.completed = completed_operations;
                    device.target = target_operations;
                    device.failures = device.failures.max(failures);
                }
                Event::DeepProgress { stage, evidence } => {
                    device.stage = stage;
                    device.matrix_checks = evidence.matrix_checks_completed;
                }
                Event::Failure {
                    kind,
                    offset_bytes,
                    detail,
                    confirm,
                    ..
                } => {
                    device.failure_events += 1;
                    let kind = serde_json::to_value(kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    self.records.push(
                        dit_contracts::FaultRecord::new(
                            format!("ssd:{kind}"),
                            1,
                            format!("offset {offset_bytes}: {detail}"),
                        )
                        .device(stable_id),
                    );
                    if let (Some(confirm), Some(record)) = (confirm, self.records.last_mut()) {
                        record.evidence.insert("confirm".to_owned(), confirm);
                    }
                }
                Event::Completed { failures, .. } => {
                    device.failures = device.failures.max(failures);
                }
                _ => {}
            }
        }
    }

    fn live(&self) -> LiveLayer {
        let failures = self
            .devices
            .values()
            .map(|d| d.failures.max(d.failure_events))
            .sum();
        let checks = self.devices.values().map(|d| d.matrix_checks).sum();
        let completed: u64 = self.devices.values().map(|d| d.completed).sum();
        let stage = self
            .devices
            .values()
            .map(|d| d.stage.as_str())
            .find(|stage| !stage.is_empty())
            .unwrap_or("PREPARE");
        LiveLayer {
            state: stage.to_owned(),
            detail: format!(
                "{} SSD | I/O {completed} | matrix checks {checks}",
                self.devices.len()
            ),
            failures,
            calculations: checks,
            ready: self.events > 0,
            ..LiveLayer::default()
        }
    }
}

#[must_use]
pub fn live(layer: Layer, dir: &Path, tally: Option<&SsdTally>) -> LiveLayer {
    match layer {
        Layer::CpuRam => cpu_live(dir),
        Layer::Gpu => gpu_live(dir),
        Layer::Chain => chain_live(dir),
        Layer::Ssd => tally.map(SsdTally::live).unwrap_or_default(),
    }
}

/// The layer's own report, parsed; `None` when it wrote none (or it is unreadable).
#[must_use]
pub fn report_json(layer: Layer, dir: &Path) -> Option<serde_json::Value> {
    let name = match layer {
        Layer::CpuRam => "system-report.json",
        Layer::Gpu | Layer::Ssd | Layer::Chain => "report.json",
    };
    json(&dir.join(name))
}

/// Final outcome of a layer from its report; `Crashed` when there is none.
#[must_use]
pub fn outcome(layer: Layer, dir: &Path) -> (LayerOutcome, String) {
    let from_verdict = |verdict: Verdict| match verdict {
        Verdict::Pass => LayerOutcome::Pass,
        Verdict::Fail => LayerOutcome::Fail,
        Verdict::Aborted | Verdict::Invalid => LayerOutcome::Invalid,
        Verdict::NotRun => LayerOutcome::NotRun,
    };
    let report = match layer {
        Layer::CpuRam => dir.join("system-report.json"),
        Layer::Gpu | Layer::Ssd | Layer::Chain => dir.join("report.json"),
    };
    let Ok(bytes) = std::fs::read(&report) else {
        return missing_report(dir);
    };
    match layer {
        Layer::CpuRam => adapt_cpu_ram_report(&report, &bytes).map_or_else(
            |error| (LayerOutcome::Invalid, error.to_string()),
            |result| (from_verdict(result.verdict), String::new()),
        ),
        Layer::Ssd => adapt_nvme_report(&report, &bytes).map_or_else(
            |error| (LayerOutcome::Invalid, error.to_string()),
            |result| (from_verdict(result.verdict), String::new()),
        ),
        Layer::Gpu => serde_json::from_slice::<GpuReportView>(&bytes).map_or_else(
            |error| (LayerOutcome::Invalid, error.to_string()),
            |view| {
                let outcome = from_verdict(view.verdict);
                match (outcome, view.hardware_status.as_deref()) {
                    (LayerOutcome::Pass, Some("warnings")) => (
                        LayerOutcome::PassHwWarnings,
                        "corrected hardware errors".into(),
                    ),
                    (outcome, status) => (outcome, status.unwrap_or_default().to_owned()),
                }
            },
        ),
        Layer::Chain => serde_json::from_slice::<serde_json::Value>(&bytes).map_or_else(
            |error| (LayerOutcome::Invalid, error.to_string()),
            |value| {
                let outcome = match str_at(&value, "verdict") {
                    "PASS" => LayerOutcome::Pass,
                    "FAIL" => LayerOutcome::Fail,
                    _ => LayerOutcome::Invalid,
                };
                (outcome, str_at(&value, "reason").to_owned())
            },
        ),
    }
}

/// A layer without a report: an engine that refused its configuration exits with the INVALID
/// code (3) on its own, anything else (signal, OOM, stall kill, another code) crashed.
fn missing_report(dir: &Path) -> (LayerOutcome, String) {
    let Some(record) = json(&dir.join("watchdog-final.json")) else {
        return (
            LayerOutcome::Crashed,
            "no report and no watchdog record".to_owned(),
        );
    };
    let state = str_at(&record, "state");
    if state == "WORKER_FAILED"
        && record.get("exit_code").and_then(serde_json::Value::as_i64) == Some(3)
    {
        let reason = std::fs::read_to_string(dir.join("console.log"))
            .unwrap_or_default()
            .lines()
            .find(|line| line.contains("INVALID"))
            .unwrap_or("engine refused to start")
            .to_owned();
        return (
            LayerOutcome::Invalid,
            format!("refused, no report: {reason}"),
        );
    }
    (
        LayerOutcome::Crashed,
        format!("no report; watchdog {state}: {}", str_at(&record, "detail")),
    )
}

/// `(VmRSS, VmHWM)` summed over `root` and every descendant process.
#[must_use]
pub fn tree_memory(root: u32) -> (u64, u64) {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (0, 0);
    };
    let mut parent: BTreeMap<u32, u32> = BTreeMap::new();
    for entry in entries.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        // `pid (comm) state ppid ...`; comm may hold spaces, so split after the last ')'.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let parent_of = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|value| value.parse::<u32>().ok());
        if let Some(parent_of) = parent_of {
            parent.insert(pid, parent_of);
        }
    }
    let mut members = vec![root];
    let mut index = 0;
    while index < members.len() {
        let current = members[index];
        members.extend(
            parent
                .iter()
                .filter(|(_, ppid)| **ppid == current)
                .map(|(pid, _)| *pid),
        );
        index += 1;
    }
    members.iter().fold((0, 0), |(rss, hwm), pid| {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|kib| kib.parse::<u64>().ok())
                .map_or(0, |kib| kib * 1024)
        };
        (rss + field("VmRSS:"), hwm + field("VmHWM:"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_invalid_and_a_signal_is_a_crash() {
        let dir = std::env::temp_dir().join(format!("dit-combined-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("watchdog-final.json"),
            r#"{"state":"WORKER_FAILED","detail":"worker exited with code 3","exit_code":3}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("console.log"),
            "INVALID: invalid system-run configuration: requested too much\n",
        )
        .unwrap();
        let (outcome, detail) = outcome(Layer::CpuRam, &dir);
        assert_eq!(outcome, LayerOutcome::Invalid);
        assert!(detail.contains("requested too much"), "{detail}");
        std::fs::write(
            dir.join("watchdog-final.json"),
            r#"{"state":"WORKER_SIGNALED","detail":"signal 9","exit_code":null}"#,
        )
        .unwrap();
        assert_eq!(outcome_of(Layer::Chain, &dir), LayerOutcome::Crashed);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn outcome_of(layer: Layer, dir: &Path) -> LayerOutcome {
        outcome(layer, dir).0
    }

    #[test]
    fn only_hardware_eligible_gpus_count() {
        let inventory = serde_json::json!({
            "auto_devices": [
                {"pci_address": "0000:08:00.0", "hardware_eligible": true},
                {"pci_address": null, "hardware_eligible": false},
                {"pci_address": "0000:03:00.0", "hardware_eligible": false}
            ],
            "cuda_devices": [{"pci_address": "0000:08:00.0", "hardware_eligible": true}]
        });
        assert_eq!(
            parse_gpu_inventory(&inventory),
            GpuEligibility {
                pci_addresses: vec!["0000:08:00.0".into()],
                cuda: true
            }
        );
        let bmc_only = serde_json::json!({
            "auto_devices": [{"pci_address": "0000:03:00.0", "hardware_eligible": false}],
            "cuda_error": "no driver"
        });
        assert_eq!(parse_gpu_inventory(&bmc_only), GpuEligibility::default());
    }

    #[test]
    fn this_process_tree_has_resident_memory() {
        let (rss, hwm) = tree_memory(std::process::id());
        assert!(rss > 0 && hwm >= rss);
    }

    /// A journal written by the SSD engine itself (`cpu-nvme-dit multi-simulate --devices 2
    /// --operations 40 --inject-device 1 --inject bit-flip`): 59 events, one failure.
    #[test]
    fn a_real_engine_journal_gives_the_engine_failure_count() {
        let dir = std::env::temp_dir().join(format!("dit-combined-real-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        std::fs::write(&path, include_str!("testdata/ssd-events-bitflip.jsonl")).unwrap();
        let mut tally = SsdTally::default();
        tally.update(&path);
        let live = tally.live();
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(tally.events, 59, "every line parses");
        assert_eq!(live.failures, 1);
        assert!(live.ready);
        assert!(live.detail.starts_with("2 SSD"), "{}", live.detail);
    }

    #[test]
    fn ssd_events_are_tallied_incrementally() {
        use dit_contracts::{DeviceId, FailureKind, RunId};
        let dir = std::env::temp_dir().join(format!("dit-combined-ssd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let envelope = |sequence, event| {
            let mut line = serde_json::to_string(&EventEnvelope {
                schema_version: 1,
                run_id: RunId("r".into()),
                module_id: "dit.cpu-nvme".into(),
                device_id: Some(DeviceId("ssd-1".into())),
                sequence,
                monotonic_ns: 1,
                wall_time_unix_ms: 1,
                event,
            })
            .unwrap();
            line.push('\n');
            line
        };
        let progress = envelope(
            1,
            Event::Progress {
                completed_operations: 10,
                target_operations: 100,
                bytes_written: 0,
                bytes_verified: 0,
                failures: 0,
            },
        );
        let failure = envelope(
            2,
            Event::Failure {
                kind: FailureKind::DataMismatch,
                operation: 3,
                offset_bytes: 0,
                detail: "x".into(),
                confirm: None,
            },
        );
        // The second line arrives in two pieces.
        let (head, tail) = failure.split_at(20);
        std::fs::write(&path, format!("{progress}{head}")).unwrap();
        let mut tally = SsdTally::default();
        tally.update(&path);
        assert_eq!((tally.events, tally.live().failures), (1, 0));
        assert!(tally.live().ready);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, tail.as_bytes()).unwrap();
        tally.update(&path);
        tally.update(&path);
        assert_eq!((tally.events, tally.live().failures), (2, 1));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
