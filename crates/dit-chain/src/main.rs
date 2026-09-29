//! dit-chain: the read-mostly data chain of the combined test (`docs/COMBINED_STRESS_CHAIN_ANALYSIS.md`
//! sections 4 and 10). Records are written to the SSD once (prefill, answers computed twice on two
//! CPU groups), then read back for the rest of the session: SSD -> RAM -> `PCIe` -> VRAM -> GPU GEMM
//! -> RAM -> CPU compare with the answers read from the SSD. The next record is chosen by a hash
//! chain over what was verified, and the path is replayed at the end.
//!
//! `run --dir` does both in one process. For the combined scheduler, `prefill` writes the records
//! and a manifest and keeps them, and `run --existing` reads them back later (once per phase),
//! so the prefill can run before the SSD baseline.
mod manifest;
mod pace;
mod prefill;
mod progress;
mod record;

use clap::{Parser, Subcommand, ValueEnum};
use gpu_cuda::{Arith, AsyncConfig, CudaBackend};
use gpu_engine::gemm::{Corpus, derive_corpus_seed};
use manifest::Manifest;
use nvme_backend_file::DirectFileBackend;
use nvme_io::StorageBackend;
use pace::{MAX_GENERATIONS, Pace, PaceReport};
use progress::{Progress, ProgressWriter};
use record::RecordHeader;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const MIB: u64 = 1024 * 1024;
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const RECOMMENDED_WORKING_SET_MIB: u64 = 16 * 1024;
/// Read sizes, one per load in turn: the CPU-SSD Deep transfer catalog (4 KiB .. 1 MiB).
const READ_SIZES: [usize; 20] = [
    4 << 10,
    64 << 10,
    128 << 10,
    1 << 20,
    8 << 10,
    12 << 10,
    16 << 10,
    24 << 10,
    32 << 10,
    48 << 10,
    80 << 10,
    96 << 10,
    160 << 10,
    192 << 10,
    256 << 10,
    320 << 10,
    384 << 10,
    512 << 10,
    640 << 10,
    768 << 10,
];

#[derive(Parser)]
#[command(
    name = "dit-chain",
    version,
    about = "DIT read-mostly SSD -> RAM -> GPU -> CPU data chain"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read records back through the GPU until the deadline: after a prefill of a new file
    /// (`--dir`), or from a file an earlier `prefill` kept (`--existing`).
    Run(RunArgs),
    /// Only write the records and their manifest; both are kept for `run --existing`.
    Prefill(PrefillArgs),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixArg {
    U32,
    Fp32,
    Int8,
    Alternate,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
enum InjectArg {
    /// Flip a byte of one record right after it was read (as a bad SSD read would).
    SsdRead,
    /// Flip a word of one corpus in host memory after its hashes were checked.
    HostRam,
    /// Corrupt one GPU result.
    Gpu,
    /// Make the two prefill answer builds of the first record disagree.
    PrefillOracle,
}

#[derive(clap::Args)]
struct ShapeArgs {
    /// Records written once at the start (the chain's whole write); recommended at least 16 GiB.
    #[arg(long, default_value_t = RECOMMENDED_WORKING_SET_MIB)]
    working_set_mib: u64,
    /// Free space the filesystem must keep beyond the working set.
    #[arg(long, default_value_t = 4096)]
    reserve_free_mib: u64,
    #[arg(long, value_enum, default_value_t = MixArg::Alternate)]
    arith: MixArg,
    #[arg(long, default_value_t = 64)]
    jobs: usize,
    #[arg(long, default_value_t = 1024)]
    inner: u32,
    /// CPU threads for each of the two prefill answer builds (0: half the allowed CPUs each).
    #[arg(long, default_value_t = 0)]
    prefill_threads: usize,
    #[arg(long)]
    seed: Option<u64>,
}

#[derive(clap::Args)]
struct OutputArgs {
    #[arg(long)]
    report: PathBuf,
    /// Heartbeat JSON replaced about once a second: phase, `ready`, declared host-memory peak,
    /// counters (for the watchdog and the combined scheduler).
    #[arg(long)]
    progress_file: Option<PathBuf>,
    /// Appends one JSON line per failure as it is found (`dit_contracts::FaultRecord`), so the
    /// control center counts it on its component cell during the run.
    #[arg(long)]
    fault_events: Option<PathBuf>,
}

#[derive(clap::Args)]
struct RunArgs {
    /// Existing directory on the SSD under test. One new file is created there (prefill first).
    #[arg(
        long,
        required_unless_present = "existing",
        conflicts_with = "existing"
    )]
    dir: Option<PathBuf>,
    /// Record file kept by `dit-chain prefill` (its manifest sits next to it). No prefill; the
    /// shape options are taken from the manifest.
    #[arg(long)]
    existing: Option<PathBuf>,
    #[command(flatten)]
    shape: ShapeArgs,
    #[arg(long, default_value_t = 900)]
    duration_seconds: u64,
    /// GPU batches run on each record read back (the starting value with `--target-read-gibps`).
    #[arg(long, default_value_t = 8)]
    generations_per_load: u64,
    /// Adjust the generations per load so the SSD reads at about this rate.
    #[arg(long)]
    target_read_gibps: Option<f64>,
    /// Reads in flight while a record is read (one read-only handle and thread each).
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    read_queue_depth: u16,
    /// Keep the record file (and write/keep its manifest) after the run (default: removed).
    #[arg(long)]
    keep_file: bool,
    #[command(flatten)]
    output: OutputArgs,
    /// GPU boundary checks (step F3 of the component fault ledger): a GPU failure carries where
    /// it went wrong (`[boundary: …]`), which places it apart from `gpu_vram_pcie`.
    #[arg(long)]
    boundary_checks: bool,
    /// Negative gate: one injected fault that must be caught and attributed.
    #[arg(long, value_enum)]
    inject: Option<InjectArg>,
}

#[derive(clap::Args)]
struct PrefillArgs {
    /// Existing directory on the SSD under test; receives `dit-chain-<session>.bin` and `.json`.
    #[arg(long)]
    dir: PathBuf,
    #[command(flatten)]
    shape: ShapeArgs,
    #[command(flatten)]
    output: OutputArgs,
    /// Negative gate: only `prefill-oracle` applies here.
    #[arg(long, value_enum)]
    inject: Option<InjectArg>,
}

#[derive(Serialize, Default, Clone)]
struct PrefillReport {
    records: u64,
    bytes_written: u64,
    seconds: f64,
    oracle_groups: (Vec<usize>, Vec<usize>),
    disagreements: Vec<String>,
}

#[derive(Serialize, Default)]
struct ReadReport {
    loads: u64,
    bytes_read: u64,
    seconds: f64,
    gib_per_second: f64,
    hash_checks_passed: u64,
    /// Wall time per stage of the loop; the largest one is the chain's bottleneck.
    stage_seconds: StageSeconds,
}

#[derive(Serialize, Default)]
struct StageSeconds {
    /// `O_DIRECT` reads from the SSD.
    ssd_read: f64,
    /// Header check, rebuilding the corpus and its two SHA-256 checks.
    decode_hash: f64,
    /// Host staging and the upload to VRAM.
    upload: f64,
    /// GPU generations, readback and the CPU compare.
    gpu_verify: f64,
}

#[derive(Serialize, Default)]
struct GpuReport {
    name: String,
    pci: String,
    tensor_cores: bool,
    batches: u64,
    results_verified: u64,
    operations_verified: u64,
    vram_bytes: u64,
}

#[derive(Serialize, Clone)]
struct ChainFailure {
    phase: &'static str,
    record: u64,
    /// Where the first check that failed places the fault.
    attribution: &'static str,
    detail: String,
    at_seconds: f64,
}

#[derive(Serialize, Default)]
struct PathReport {
    /// Per-run nonce mixed into the start of the path, so each run over a kept file reads its
    /// own order; needed with the manifest to replay the path elsewhere.
    nonce: String,
    steps: u64,
    h_final: String,
    replay_matches: bool,
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    module_id: &'static str,
    /// `run`, `run-existing` or `prefill`.
    mode: &'static str,
    verdict: &'static str,
    reason: Option<String>,
    session: String,
    base_seed: u64,
    file: String,
    manifest: Option<String>,
    file_kept: bool,
    working_set_mib: u64,
    records: u64,
    stride_bytes: u64,
    generations_per_load: u64,
    read_queue_depth: u16,
    jobs: usize,
    inner: u32,
    arith: MixArg,
    inject: Option<InjectArg>,
    warnings: Vec<String>,
    prefill: PrefillReport,
    reads: ReadReport,
    gpu: GpuReport,
    pace: PaceReport,
    failure_count: u64,
    failures: Vec<ChainFailure>,
    path: PathReport,
    elapsed_seconds: f64,
}

/// One step of the read path, as logged for the replay.
#[derive(Clone, Copy)]
struct Step {
    index: u64,
    batches: u64,
    ok: bool,
}

/// What every record of a session looks like.
struct Layout {
    session: u64,
    base_seed: u64,
    jobs: usize,
    inner: u32,
    arith: MixArg,
    stride: usize,
    records: u64,
}

impl Layout {
    fn new(shape: &ShapeArgs) -> Self {
        let stride = record::stride(shape.jobs, shape.inner as usize);
        Self {
            session: random_u64(),
            base_seed: shape.seed.unwrap_or_else(random_u64),
            jobs: shape.jobs,
            inner: shape.inner,
            arith: shape.arith,
            stride,
            // Rounded up: the working set is at least what was asked for.
            records: (shape.working_set_mib * MIB).div_ceil(stride as u64).max(1),
        }
    }

    fn file_in(&self, dir: &Path) -> PathBuf {
        dir.join(format!("dit-chain-{:016x}.bin", self.session))
    }

    fn bytes(&self) -> u64 {
        self.records * self.stride as u64
    }
}

/// The record file and what each record must contain.
struct Records {
    file: DirectFileBackend,
    path: PathBuf,
    headers: Vec<RecordHeader>,
    groups: (Vec<usize>, Vec<usize>),
}

fn random_u64() -> u64 {
    let mut bytes = [0_u8; 8];
    let _ = getrandom::fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

fn arith_for(mix: MixArg, index: u64, tensor: bool) -> Arith {
    let turn = |cycle: &[Arith]| cycle[usize::try_from(index % cycle.len() as u64).unwrap_or(0)];
    match mix {
        MixArg::U32 => Arith::U32,
        MixArg::Fp32 => Arith::F32,
        MixArg::Int8 => Arith::I8,
        MixArg::All if tensor => turn(&[Arith::U32, Arith::F32, Arith::I8]),
        MixArg::Alternate | MixArg::All => turn(&[Arith::U32, Arith::F32]),
    }
}

/// `H_g = SHA-256(H_{g-1} || index || input hash || answer hash || batches || ok)`; the next record
/// is `H_g mod records`.
fn step_hash(previous: &[u8; 32], step: Step, header: &RecordHeader) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(previous);
    digest.update(step.index.to_le_bytes());
    digest.update(header.input_sha);
    digest.update(header.dot_sha);
    digest.update(step.batches.to_le_bytes());
    digest.update([u8::from(step.ok)]);
    digest.finalize().into()
}

fn next_index(hash: &[u8; 32], records: u64) -> u64 {
    u64::from_le_bytes(hash[..8].try_into().unwrap_or([0; 8])) % records.max(1)
}

fn start_hash(session: u64, seed: u64, nonce: u64) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"DITCHAN1-path");
    digest.update(session.to_le_bytes());
    digest.update(seed.to_le_bytes());
    digest.update(nonce.to_le_bytes());
    digest.finalize().into()
}

/// Recomputes the path from the headers and the logged steps; true when every step read the
/// record the chain says it should have, and the final hash agrees.
fn replay(start: [u8; 32], headers: &[RecordHeader], steps: &[Step], live: &[u8; 32]) -> bool {
    let records = headers.len() as u64;
    let mut hash = start;
    let mut expected = next_index(&hash, records);
    for step in steps {
        if step.index != expected {
            return false;
        }
        let Some(header) = usize::try_from(step.index)
            .ok()
            .and_then(|i| headers.get(i))
        else {
            return false;
        };
        hash = step_hash(&hash, *step, header);
        expected = next_index(&hash, records);
    }
    &hash == live
}

fn free_bytes(dir: &Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    Some(stat.blocks_available().saturating_mul(stat.fragment_size()))
}

/// Reads `buffer.len()` bytes at `offset` in chunks of `chunk` (a multiple of 4 KiB). Chunk `i`
/// goes to reader `i mod readers`, each on its own thread, so the SSD sees one synchronous
/// `O_DIRECT` read per reader in flight: queue depth = number of readers.
fn read_record(
    readers: &mut [DirectFileBackend],
    offset: u64,
    buffer: &mut [u8],
    chunk: usize,
) -> Result<(), nvme_io::BackendError> {
    let lanes = readers.len().max(1);
    let mut work: Vec<Vec<(u64, &mut [u8])>> = (0..lanes).map(|_| Vec::new()).collect();
    for (i, piece) in buffer.chunks_mut(chunk).enumerate() {
        work[i % lanes].push((offset + (i * chunk) as u64, piece));
    }
    let read_lane = |reader: &mut DirectFileBackend, pieces: Vec<(u64, &mut [u8])>| {
        pieces
            .into_iter()
            .try_for_each(|(at, piece)| reader.read_exact_at(at, piece))
    };
    if lanes == 1 {
        let pieces = work.pop().unwrap_or_default();
        return readers
            .first_mut()
            .map_or(Ok(()), |reader| read_lane(reader, pieces));
    }
    std::thread::scope(|scope| {
        let joins: Vec<_> = readers
            .iter_mut()
            .zip(work)
            .map(|(reader, pieces)| scope.spawn(move || read_lane(reader, pieces)))
            .collect();
        joins.into_iter().try_for_each(|join| {
            join.join().unwrap_or_else(|_| {
                Err(nvme_io::BackendError::Io(std::io::Error::other(
                    "read thread panicked",
                )))
            })
        })
    })
}

/// Ctrl-C, SIGTERM (watchdog stop, scheduler drain) and SIGHUP end the current step cleanly.
/// The signals are blocked here, before the first thread exists, and taken by one waiting
/// thread: no signal handler, and nothing else runs in signal context. `dit-chain` starts no
/// child process, so the inherited block reaches no other program.
fn install_stop() -> Arc<AtomicBool> {
    use nix::sys::signal::{SigSet, Signal};
    let stop = Arc::new(AtomicBool::new(false));
    let mut set = SigSet::empty();
    for signal in [Signal::SIGINT, Signal::SIGTERM, Signal::SIGHUP] {
        set.add(signal);
    }
    if set.thread_block().is_ok() {
        let flag = Arc::clone(&stop);
        std::thread::spawn(move || {
            while set.wait().is_ok() {
                flag.store(true, Ordering::SeqCst);
            }
        });
    }
    stop
}

/// State shared by every phase of one invocation: the report, the failures and the heartbeat.
struct Ctx {
    started: Instant,
    stop: Arc<AtomicBool>,
    report: Report,
    failures: Vec<ChainFailure>,
    progress: Progress,
    writer: ProgressWriter,
    faults: dit_contracts::FaultWriter,
}

impl Ctx {
    fn new(mode: &'static str, layout: &Layout, output: &OutputArgs) -> Self {
        let started = Instant::now();
        let report = Report {
            schema_version: 1,
            module_id: "dit.chain",
            mode,
            verdict: "INVALID",
            reason: None,
            session: format!("{:016x}", layout.session),
            base_seed: layout.base_seed,
            file: String::new(),
            manifest: None,
            file_kept: false,
            working_set_mib: layout.bytes() / MIB,
            records: layout.records,
            stride_bytes: layout.stride as u64,
            generations_per_load: 0,
            read_queue_depth: 0,
            jobs: layout.jobs,
            inner: layout.inner,
            arith: layout.arith,
            inject: None,
            warnings: Vec::new(),
            prefill: PrefillReport::default(),
            reads: ReadReport::default(),
            gpu: GpuReport::default(),
            pace: PaceReport::default(),
            failure_count: 0,
            failures: Vec::new(),
            path: PathReport::default(),
            elapsed_seconds: 0.0,
        };
        // The failure path may hold two more records (re-read) and a CPU rebuild of one.
        let headroom = 3 * layout.stride as u64;
        let mut ctx = Self {
            started,
            stop: install_stop(),
            report,
            failures: Vec::new(),
            progress: Progress::new(layout.records),
            writer: ProgressWriter::new(output.progress_file.clone(), started, headroom),
            faults: dit_contracts::FaultWriter::new(output.fault_events.as_deref()),
        };
        ctx.beat(true);
        ctx
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn fail(
        &mut self,
        phase: &'static str,
        record: u64,
        attribution: &'static str,
        detail: String,
    ) {
        eprintln!("CHAIN FAIL {phase} record {record}: {attribution}: {detail}");
        self.faults.write(
            &dit_contracts::FaultRecord::new(format!("chain:{attribution}"), 1, detail.clone())
                .evidence("phase", phase)
                .evidence("record", record.to_string()),
        );
        self.report.failure_count += 1;
        self.progress.failures = self.report.failure_count;
        if self.failures.len() < 64 {
            self.failures.push(ChainFailure {
                phase,
                record,
                attribution,
                detail,
                at_seconds: self.started.elapsed().as_secs_f64(),
            });
        }
    }

    fn beat(&mut self, force: bool) {
        self.writer.write(&mut self.progress, force);
    }

    fn phase(&mut self, phase: &'static str) {
        self.progress.phase = phase;
        self.progress.ready = phase == "reading";
        self.beat(true);
    }

    fn warn(&mut self, warning: String) {
        eprintln!("dit-chain warning: {warning}");
        self.report.warnings.push(warning);
    }

    /// PASS / FAIL / INVALID from what happened; `work` says whether the phase did its job.
    fn conclude(&mut self, error: Option<String>, work: Result<(), String>) {
        self.report.verdict = if let Some(reason) = error {
            self.report.reason = Some(reason);
            "INVALID"
        } else if self.report.failure_count > 0 {
            "FAIL"
        } else if self.stopped() {
            self.report.reason = Some("stopped by the operator".into());
            "INVALID"
        } else if let Err(reason) = work {
            self.report.reason = Some(reason);
            "INVALID"
        } else {
            "PASS"
        };
    }

    /// Writes the report and the last heartbeat; then removes `cleanup` (report first, so a
    /// kill during a slow unlink still leaves the evidence).
    #[allow(clippy::cast_precision_loss)]
    fn finish(mut self, report_path: &Path, cleanup: &[PathBuf]) -> ExitCode {
        self.report.failures = std::mem::take(&mut self.failures);
        self.report.elapsed_seconds = self.started.elapsed().as_secs_f64();
        let code = match self.report.verdict {
            "PASS" => 0,
            "FAIL" => 2,
            _ => 3,
        };
        match serde_json::to_vec_pretty(&self.report).map(|json| std::fs::write(report_path, json))
        {
            Ok(Ok(())) => {}
            _ => eprintln!("could not write the report to {}", report_path.display()),
        }
        self.progress.phase = "done";
        self.progress.ready = false;
        self.progress.verdict = Some(self.report.verdict);
        self.beat(true);
        for path in cleanup {
            let _ = std::fs::remove_file(path);
        }
        println!(
            "{} dit-chain {} records={} loads={} read={:.2} GiB ({:.2} GiB/s) gpu_results={} failures={} report={}",
            self.report.verdict,
            self.report.mode,
            self.report.records,
            self.report.reads.loads,
            self.report.reads.bytes_read as f64 / GIB,
            self.report.reads.gib_per_second,
            self.report.gpu.results_verified,
            self.report.failure_count,
            report_path.display()
        );
        ExitCode::from(code)
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run(args) => run(&args),
        Command::Prefill(args) => prefill_only(&args),
    }
}

fn check_space(ctx: &mut Ctx, dir: &Path, layout: &Layout, reserve_mib: u64) -> Result<(), String> {
    if layout.bytes() / MIB < RECOMMENDED_WORKING_SET_MIB {
        ctx.warn(format!(
            "working set {} MiB is below {RECOMMENDED_WORKING_SET_MIB} MiB: reads may be served by the SSD's own cache",
            layout.bytes() / MIB
        ));
    }
    let needed = layout.bytes() + reserve_mib * MIB;
    match free_bytes(dir) {
        Some(free) if free >= needed => Ok(()),
        Some(free) => Err(format!(
            "{} MiB free, {} MiB needed (working set + reserve)",
            free / MIB,
            needed / MIB
        )),
        None => Err(format!("cannot read free space of {}", dir.display())),
    }
}

/// Opens the first eligible CUDA GPU and prepares the async pipeline for `layout`'s shape
/// (jobs must not back off: that shape is every record's shape).
fn open_gpu(
    ctx: &mut Ctx,
    layout: &Layout,
    max_generations: u64,
    boundary_checks: bool,
) -> Result<CudaBackend, String> {
    ctx.phase("opening");
    let device =
        match CudaBackend::inventory().map(|all| all.into_iter().find(|d| d.hardware_eligible)) {
            Ok(Some(device)) => device,
            Ok(None) => return Err("no eligible CUDA GPU".into()),
            Err(error) => return Err(format!("CUDA: {error}")),
        };
    let pci = device.pci_address.clone().unwrap_or_default();
    let mut backend = CudaBackend::open(&pci).map_err(|error| format!("CUDA open: {error}"))?;
    ctx.report.gpu.name.clone_from(&device.name);
    ctx.report.gpu.pci.clone_from(&pci);
    ctx.report.gpu.tensor_cores = backend.tensor_available();
    if layout.arith == MixArg::Int8 && !backend.tensor_available() {
        return Err("INT8 tensor GEMM needs compute capability 8.0 or newer".into());
    }
    backend
        .configure_async(AsyncConfig {
            batches: max_generations,
            rotate_every_batches: 0,
            boundary_checks,
            ..AsyncConfig::default()
        })
        .and_then(|()| backend.prepare_matrix_resources(layout.jobs, layout.inner))
        .map_err(|error| format!("GPU prepare: {error}"))
        .and_then(|(jobs, _)| {
            (jobs == layout.jobs).then_some(()).ok_or(format!(
                "GPU could only prepare {jobs} of {} jobs",
                layout.jobs
            ))
        })?;
    Ok(backend)
}

fn oracle_groups(ctx: &mut Ctx, prefill_threads: usize) -> (Vec<usize>, Vec<usize>) {
    let cpus = prefill::allowed_cpus();
    let per_group = if prefill_threads == 0 {
        cpus.len() / 2
    } else {
        prefill_threads
    };
    let groups = prefill::split_groups(&cpus, per_group);
    if groups.0 == groups.1 {
        ctx.warn("one CPU only: both answer builds shared a core".into());
    }
    groups
}

/// Writes every record once: answers built twice on two CPU groups, written only when both agree.
/// The file is removed again on any error or a stop, since it is not a complete working set.
#[allow(clippy::cast_precision_loss)]
fn prefill_records(
    ctx: &mut Ctx,
    dir: &Path,
    layout: &Layout,
    prefill_threads: usize,
    tensor: bool,
) -> Result<Records, String> {
    ctx.phase("prefill");
    let started = Instant::now();
    let groups = oracle_groups(ctx, prefill_threads);
    ctx.report.prefill.oracle_groups = groups.clone();
    let path = layout.file_in(dir);
    ctx.report.file = path.display().to_string();
    let mut file = DirectFileBackend::create_new(&path, layout.bytes(), layout.bytes())
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    let written = write_records(ctx, &mut file, layout, &groups, tensor);
    if let Err(error) = written {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    let headers = written.unwrap_or_default();
    ctx.report.prefill.records = headers.len() as u64;
    ctx.report.prefill.bytes_written = file.host_bytes_written();
    ctx.report.prefill.seconds = started.elapsed().as_secs_f64();
    println!(
        "prefill: {} records, {:.2} GiB written in {:.1} s",
        headers.len(),
        ctx.report.prefill.bytes_written as f64 / GIB,
        ctx.report.prefill.seconds
    );
    Ok(Records {
        file,
        path,
        headers,
        groups,
    })
}

fn write_records(
    ctx: &mut Ctx,
    file: &mut DirectFileBackend,
    layout: &Layout,
    groups: &(Vec<usize>, Vec<usize>),
    tensor: bool,
) -> Result<Vec<RecordHeader>, String> {
    let inject = ctx.report.inject == Some(InjectArg::PrefillOracle);
    let mut headers = Vec::with_capacity(usize::try_from(layout.records).unwrap_or(0));
    let mut build_seed = 0_u64;
    for index in 0..layout.records {
        let arith = arith_for(layout.arith, index, tensor);
        let corpus = loop {
            if ctx.stopped() {
                return Err("stopped during prefill".into());
            }
            let seed = derive_corpus_seed(layout.base_seed, build_seed);
            build_seed += 1;
            let corrupt = inject && index == 0 && build_seed == 1;
            match prefill::build_twice(arith, seed, layout.jobs, layout.inner, groups, corrupt) {
                Ok(corpus) => break corpus,
                Err(Ok(found)) => {
                    let detail = format!(
                        "seed {:#x} {:?} {} word {}: {:#010x} vs {:#010x} (CPU groups {:?} / {:?})",
                        found.seed,
                        found.arith,
                        found.part,
                        found.word,
                        found.first,
                        found.second,
                        groups.0,
                        groups.1
                    );
                    ctx.report.prefill.disagreements.push(detail.clone());
                    ctx.fail("chain-prefill-oracle", index, "cpu_or_ram", detail);
                    // Rebuilt from the next seed; the disagreement itself is the CPU/RAM evidence.
                }
                Err(Err(error)) => return Err(format!("prefill build: {error}")),
            }
        };
        let bytes = record::encode(&corpus, layout.session, index, layout.stride)
            .map_err(|error| format!("record {index}: {error}"))?;
        file.write_all_at(index * layout.stride as u64, &bytes)
            .map_err(|error| format!("write record {index}: {error}"))?;
        headers.push(RecordHeader::for_corpus(&corpus, layout.session, index));
        ctx.progress.records_written = headers.len() as u64;
        ctx.beat(false);
    }
    file.flush().map_err(|error| format!("flush: {error}"))?;
    Ok(headers)
}

fn store_manifest(ctx: &mut Ctx, layout: &Layout, records: &Records) -> Result<PathBuf, String> {
    let (headers, headers_sha256) = Manifest::seal_headers(&records.headers);
    let manifest = Manifest {
        schema_version: 1,
        format: manifest::FORMAT.into(),
        session: format!("{:016x}", layout.session),
        base_seed: layout.base_seed,
        jobs: layout.jobs,
        inner: layout.inner,
        arith: layout.arith,
        records: layout.records,
        stride_bytes: layout.stride as u64,
        headers,
        headers_sha256,
        prefill_bytes_written: ctx.report.prefill.bytes_written,
        prefill_seconds: ctx.report.prefill.seconds,
        oracle_groups: records.groups.clone(),
        oracle_disagreements: ctx.report.prefill.disagreements.clone(),
    };
    let path = manifest::path_for(&records.path);
    manifest.store(&path)?;
    ctx.report.manifest = Some(path.display().to_string());
    Ok(path)
}

/// Layout and records of a kept file, from its manifest; the file is opened read-only.
fn open_existing(file: &Path) -> Result<(Layout, Records, Manifest), String> {
    let manifest = Manifest::load(&manifest::path_for(file))?;
    let (session, headers) = manifest.open()?;
    let stride = record::stride(manifest.jobs, manifest.inner as usize);
    if stride as u64 != manifest.stride_bytes {
        return Err("manifest stride does not match its shape".into());
    }
    let layout = Layout {
        session,
        base_seed: manifest.base_seed,
        jobs: manifest.jobs,
        inner: manifest.inner,
        arith: manifest.arith,
        stride,
        records: manifest.records,
    };
    let handle = DirectFileBackend::open_existing_read_only(file)
        .map_err(|error| format!("open {}: {error}", file.display()))?;
    if handle.capacity_bytes() != layout.bytes() {
        return Err(format!(
            "{} is {} bytes, the manifest describes {}",
            file.display(),
            handle.capacity_bytes(),
            layout.bytes()
        ));
    }
    let records = Records {
        file: handle,
        path: file.to_path_buf(),
        headers,
        groups: manifest.oracle_groups.clone(),
    };
    Ok((layout, records, manifest))
}

fn prefill_only(args: &PrefillArgs) -> ExitCode {
    let layout = Layout::new(&args.shape);
    let mut ctx = Ctx::new("prefill", &layout, &args.output);
    ctx.report.inject = args.inject;
    ctx.report.file_kept = true;
    let outcome = (|| {
        if args
            .inject
            .is_some_and(|inject| inject != InjectArg::PrefillOracle)
        {
            return Err("prefill takes only --inject prefill-oracle".to_owned());
        }
        check_space(&mut ctx, &args.dir, &layout, args.shape.reserve_free_mib)?;
        let backend = open_gpu(&mut ctx, &layout, 1, false)?;
        let tensor = backend.tensor_available();
        drop(backend);
        let records = prefill_records(
            &mut ctx,
            &args.dir,
            &layout,
            args.shape.prefill_threads,
            tensor,
        )?;
        if let Err(error) = store_manifest(&mut ctx, &layout, &records) {
            drop(records.file);
            let _ = std::fs::remove_file(&records.path);
            return Err(error);
        }
        Ok(())
    })();
    ctx.phase("finishing");
    let error = outcome.err();
    ctx.report.file_kept = error.is_none();
    ctx.conclude(error, Ok(()));
    ctx.finish(&args.output.report, &[])
}

/// Report fields of a run over a kept file: where it is, and its prefill as the manifest recorded.
fn adopt_existing(ctx: &mut Ctx, layout: &Layout, records: &Records, manifest: &Manifest) {
    ctx.report.file = records.path.display().to_string();
    ctx.report.manifest = Some(manifest::path_for(&records.path).display().to_string());
    ctx.report.prefill = PrefillReport {
        records: manifest.records,
        bytes_written: manifest.prefill_bytes_written,
        seconds: manifest.prefill_seconds,
        oracle_groups: manifest.oracle_groups.clone(),
        disagreements: manifest.oracle_disagreements.clone(),
    };
    if layout.bytes() / MIB < RECOMMENDED_WORKING_SET_MIB {
        ctx.warn(format!(
            "working set {} MiB is below {RECOMMENDED_WORKING_SET_MIB} MiB: reads may be served by the SSD's own cache",
            layout.bytes() / MIB
        ));
    }
}

fn run(args: &RunArgs) -> ExitCode {
    let existing = args.existing.as_ref().map(|file| open_existing(file));
    let (layout, opened) = match existing {
        Some(Ok((layout, records, manifest))) => (layout, Some(Ok((records, manifest)))),
        Some(Err(error)) => (Layout::new(&args.shape), Some(Err(error))),
        None => (Layout::new(&args.shape), None),
    };
    let mode = if opened.is_some() {
        "run-existing"
    } else {
        "run"
    };
    let mut ctx = Ctx::new(mode, &layout, &args.output);
    ctx.report.inject = args.inject;
    if let Some(file) = &args.existing {
        ctx.report.file = file.display().to_string();
    }
    let start_generations = args.generations_per_load.clamp(1, MAX_GENERATIONS);
    ctx.report.generations_per_load = start_generations;
    ctx.report.read_queue_depth = args.read_queue_depth;
    let max_generations = if args.target_read_gibps.is_some() {
        MAX_GENERATIONS
    } else {
        start_generations
    };
    let mut cleanup = Vec::new();
    let outcome = (|| {
        if args.generations_per_load == 0 || args.duration_seconds == 0 {
            return Err("generations per load and duration must be positive".to_owned());
        }
        if args
            .target_read_gibps
            .is_some_and(|target| target <= 0.0 || !target.is_finite())
        {
            return Err("--target-read-gibps must be positive".to_owned());
        }
        let mut records = match opened {
            Some(Ok((mut records, manifest))) => {
                if args.inject == Some(InjectArg::PrefillOracle) {
                    return Err("--inject prefill-oracle needs a prefill (use --dir)".into());
                }
                adopt_existing(&mut ctx, &layout, &records, &manifest);
                if !args.keep_file {
                    cleanup.push(records.path.clone());
                    cleanup.push(manifest::path_for(&records.path));
                }
                let backend = open_gpu(&mut ctx, &layout, max_generations, args.boundary_checks)?;
                return Ok(read_phase(&mut ctx, args, &layout, &mut records, &backend));
            }
            Some(Err(error)) => return Err(error),
            None => {
                let dir = args
                    .dir
                    .as_deref()
                    .ok_or("--dir or --existing is required")?;
                check_space(&mut ctx, dir, &layout, args.shape.reserve_free_mib)?;
                let backend = open_gpu(&mut ctx, &layout, max_generations, args.boundary_checks)?;
                let records = prefill_records(
                    &mut ctx,
                    dir,
                    &layout,
                    args.shape.prefill_threads,
                    backend.tensor_available(),
                )?;
                if !args.keep_file {
                    cleanup.push(records.path.clone());
                } else if let Err(error) = store_manifest(&mut ctx, &layout, &records) {
                    // Without a manifest the file could not be reused: remove it as well.
                    ctx.warn(format!("record file not kept: {error}"));
                    cleanup.push(records.path.clone());
                }
                (records, backend)
            }
        };
        Ok(read_phase(
            &mut ctx,
            args,
            &layout,
            &mut records.0,
            &records.1,
        ))
    })();
    ctx.phase("finishing");
    ctx.report.file_kept = cleanup.is_empty() && !ctx.report.file.is_empty();
    let (error, work) = match outcome {
        Ok(work) => (None, work),
        Err(error) => (Some(error), Ok(())),
    };
    ctx.conclude(error, work);
    ctx.finish(&args.output.report, &cleanup)
}

/// The read loop: read, check, upload, compute, compare, and choose the next record, until the
/// deadline or a stop. `Err(reason)` when it ran but did no verified work.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
fn read_phase(
    ctx: &mut Ctx,
    args: &RunArgs,
    layout: &Layout,
    records: &mut Records,
    backend: &CudaBackend,
) -> Result<(), String> {
    ctx.phase("reading");
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.duration_seconds);
    let nonce = random_u64();
    ctx.report.path.nonce = format!("{nonce:016x}");
    let start = start_hash(layout.session, layout.base_seed, nonce);
    let mut hash = start;
    let mut index = next_index(&hash, layout.records);
    let mut steps: Vec<Step> = Vec::new();
    let stride = layout.stride;
    let mut readers = (0..usize::from(args.read_queue_depth))
        .map(|_| DirectFileBackend::open_existing_read_only(&records.path))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("open readers: {error}"))?;
    let mut buffer = vec![0_u8; stride];
    let mut gpu_fail_counts: BTreeMap<u64, u32> = BTreeMap::new();
    let mut pace = Pace::new(args.target_read_gibps, ctx.report.generations_per_load);
    let mut last_print = Instant::now();
    let inject_load = 2_u64;
    let per_result = 2 * u64::from(layout.inner);
    let session = layout.session;
    while Instant::now() < deadline && !ctx.stopped() && !backend.submission_stopped() {
        let load = ctx.report.reads.loads;
        let header = records.headers[usize::try_from(index).unwrap_or(0)].clone();
        let offset = index * stride as u64;
        let chunk = READ_SIZES[usize::try_from(load % READ_SIZES.len() as u64).unwrap_or(0)];
        let generations = pace.generations();
        let mut batches = 0;
        let mut step_bytes = 0;
        let mut step_ops = 0;
        let ok = 'step: {
            let stage = Instant::now();
            let read = read_record(&mut readers, offset, &mut buffer, chunk);
            ctx.report.reads.stage_seconds.ssd_read += stage.elapsed().as_secs_f64();
            if let Err(error) = read {
                ctx.fail("chain-read", index, "ssd_io", error.to_string());
                break 'step false;
            }
            ctx.report.reads.loads += 1;
            ctx.report.reads.bytes_read += stride as u64;
            step_bytes = stride as u64;
            if args.inject == Some(InjectArg::SsdRead) && load == inject_load {
                buffer[record::HEADER_BYTES + 100] ^= 0x04;
            }
            let stage = Instant::now();
            let decoded = record::decode(&buffer, session, index).and_then(|(read, input, dot)| {
                Corpus::from_parts(read.arith, read.seed, layout.jobs, layout.inner, input, dot)
                    .map_err(|_| record::RecordError::Length)
                    .map(|corpus| (read, corpus))
            });
            ctx.report.reads.stage_seconds.decode_hash += stage.elapsed().as_secs_f64();
            let mut corpus = match decoded {
                Ok((read, corpus)) if read.matches(&corpus) && read == header => corpus,
                result => {
                    let why = match result {
                        Ok(_) => "data hash differs from the record header".to_owned(),
                        Err(error) => error.to_string(),
                    };
                    // Read again: a fault that does not repeat was on the read path (SSD
                    // controller, DMA, host RAM); one that repeats is in the stored data.
                    let mut again = vec![0_u8; stride];
                    let clean_again = read_record(&mut readers, offset, &mut again, 1 << 20)
                        .is_ok()
                        && record::decode(&again, session, index).is_ok_and(|(h, i, d)| {
                            h == header
                                && Corpus::from_parts(
                                    h.arith,
                                    h.seed,
                                    layout.jobs,
                                    layout.inner,
                                    i,
                                    d,
                                )
                                .is_ok_and(|c| h.matches(&c))
                        });
                    let attribution = if clean_again {
                        "ssd_read_path_transient"
                    } else {
                        "ssd_stored_data"
                    };
                    ctx.fail("chain-read", index, attribution, why);
                    break 'step false;
                }
            };
            ctx.report.reads.hash_checks_passed += 1;
            if args.inject == Some(InjectArg::HostRam) && load == inject_load {
                corpus.input[0] ^= 1;
            }
            let stage = Instant::now();
            let uploaded = backend.load_corpus(corpus);
            ctx.report.reads.stage_seconds.upload += stage.elapsed().as_secs_f64();
            if let Err(error) = uploaded {
                ctx.fail("chain-upload", index, "gpu_vram_pcie", error.to_string());
                break 'step false;
            }
            let inject_gpu = args.inject == Some(InjectArg::Gpu) && load == inject_load;
            let stage = Instant::now();
            let verified =
                backend.verify_matrix_async(header.seed, inject_gpu, &|| false, Some(generations));
            ctx.report.reads.stage_seconds.gpu_verify += stage.elapsed().as_secs_f64();
            match verified {
                Ok(evidence) => {
                    if let Some(profile) = evidence.profile.as_ref() {
                        batches = profile.dispatches_verified;
                        ctx.report.gpu.vram_bytes = profile.persistent_device_buffer_bytes;
                    }
                    ctx.report.gpu.batches += batches;
                    ctx.report.gpu.results_verified += evidence.results_verified;
                    step_ops = evidence.results_verified * per_result;
                    ctx.report.gpu.operations_verified += step_ops;
                    true
                }
                Err(error) => {
                    let attribution = if backend.last_burst_attribution()
                        == Some("host_memory_changed")
                    {
                        "host_ram"
                    } else {
                        let mut again = vec![0_u8; stride];
                        let same = read_record(&mut readers, offset, &mut again, 1 << 20).is_ok()
                            && again == buffer;
                        if same {
                            "gpu_vram_pcie"
                        } else {
                            "ssd_read_path_transient"
                        }
                    };
                    let count = gpu_fail_counts.entry(index).or_default();
                    *count += 1;
                    let attribution = if attribution == "gpu_vram_pcie" && *count >= 2 {
                        // Always this record: check its stored answers before blaming the GPU.
                        match prefill::build_twice(
                            header.arith,
                            header.seed,
                            layout.jobs,
                            layout.inner,
                            &records.groups,
                            false,
                        ) {
                            Ok(fresh) if !header.matches(&fresh) => "stored_answer_prefill",
                            _ => attribution,
                        }
                    } else {
                        attribution
                    };
                    // The async path reports a mismatch as `Unavailable(detail)`; keep only the detail.
                    let detail = match error {
                        gpu_cuda::Error::Unavailable(detail) => detail,
                        other => other.to_string(),
                    };
                    ctx.fail("chain-gpu", index, attribution, detail);
                    break 'step false;
                }
            }
        };
        let logged = Step { index, batches, ok };
        steps.push(logged);
        hash = step_hash(&hash, logged, &header);
        index = next_index(&hash, layout.records);
        if pace.record(step_bytes, step_ops) {
            ctx.progress.read_gib_per_second = pace.window_rates.0;
            ctx.progress.gpu_gops_verified = pace.window_rates.1;
        }
        ctx.progress.loads = ctx.report.reads.loads;
        ctx.progress.bytes_read = ctx.report.reads.bytes_read;
        ctx.progress.generations_per_load = pace.generations();
        ctx.progress.vram_bytes = ctx.report.gpu.vram_bytes;
        ctx.beat(false);
        if last_print.elapsed() >= Duration::from_secs(2) {
            last_print = Instant::now();
            let seconds = started.elapsed().as_secs_f64().max(1e-9);
            println!(
                "chain: loads {} | read {:.2} GiB/s | GPU {:.1} Gop/s verified | G {} | failures {}",
                ctx.report.reads.loads,
                ctx.report.reads.bytes_read as f64 / GIB / seconds,
                ctx.report.gpu.operations_verified as f64 / 1e9 / seconds,
                pace.generations(),
                ctx.report.failure_count
            );
        }
    }
    ctx.report.reads.seconds = started.elapsed().as_secs_f64();
    ctx.report.reads.gib_per_second =
        ctx.report.reads.bytes_read as f64 / GIB / ctx.report.reads.seconds.max(1e-9);
    if let Some(warning) = pace.shortfall() {
        ctx.warn(warning);
    }
    ctx.report.pace = pace.report;
    ctx.report.path.steps = steps.len() as u64;
    ctx.report.path.h_final = hex::encode(hash);
    ctx.report.path.replay_matches = replay(start, &records.headers, &steps, &hash);
    if !ctx.report.path.replay_matches {
        ctx.fail(
            "chain-replay",
            0,
            "end_to_end",
            "replayed read path differs from the live one".into(),
        );
    }
    if backend.submission_stopped() {
        Err("GPU submission stopped".into())
    } else if ctx.report.reads.loads == 0 {
        Err("no record was read and verified".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(count: u64) -> Vec<RecordHeader> {
        (0..count)
            .map(|index| {
                let corpus = Corpus::new_arith(Arith::U32, index, 1, 16, 1).unwrap();
                RecordHeader::for_corpus(&corpus, 5, index)
            })
            .collect()
    }

    #[test]
    fn the_read_path_replays_and_a_wrong_step_is_caught() {
        let headers = headers(7);
        let start = start_hash(5, 9, 1);
        let mut hash = start;
        let mut index = next_index(&hash, 7);
        let mut steps = Vec::new();
        for n in 0..20 {
            let step = Step {
                index,
                batches: 8,
                ok: n != 4,
            };
            steps.push(step);
            hash = step_hash(&hash, step, &headers[usize::try_from(index).unwrap()]);
            index = next_index(&hash, 7);
        }
        assert!(replay(start, &headers, &steps, &hash));
        let mut wrong = steps.clone();
        wrong[10].index = (wrong[10].index + 1) % 7;
        assert!(
            !replay(start, &headers, &wrong, &hash),
            "a different record read is caught"
        );
        let mut short = steps.clone();
        short[3].batches = 7;
        assert!(
            !replay(start, &headers, &short, &hash),
            "a different batch count is caught"
        );
        assert!(
            !replay(start_hash(5, 9, 2), &headers, &steps, &hash),
            "another run's nonce gives another path"
        );
    }

    #[test]
    fn arithmetic_turns_follow_the_mix() {
        assert_eq!(arith_for(MixArg::Alternate, 3, false), Arith::F32);
        assert_eq!(arith_for(MixArg::All, 2, true), Arith::I8);
        assert_eq!(arith_for(MixArg::All, 2, false), Arith::U32);
    }
}
