//! Storage side of the combined run (`docs/COMBINED_STRESS_CHAIN_ANALYSIS.md` section 11): the
//! operator picks the SSDs and the write level here, with the same picker, safety checks and
//! automatic volume preparation as the interactive CPU-SSD flow. The prepared test directories
//! must outlive every phase (SSD baseline, overlap, data chain), so this process holds them:
//! it publishes `storage-plan.json` for the scheduler, leaves the terminal's process group (a
//! Ctrl+C meant for the tests must not unmount their directories) and releases everything only
//! on SIGTERM or when its parent disappears.
use super::{
    FS_SYNC_PERIOD, InventoryFlags, WriteLevel, auto_storage, automatic_plans, mib,
    parse_selection, parse_write_level, picker_entries, prepare_offering_repair,
    read_operator_line, render_picker, revalidate, scan, scan_options, scan_releasing_stale,
    write_selection_file, write_setup_outcome,
};
use dit_ui_core::TerminalSize;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Chain working set with the Standard/Heavy levels (decision 3: at least 16 GiB so the SSD's
/// own cache cannot serve the reads), and with Light (smaller, flagged in the chain report).
const CHAIN_WORKING_SET_MIB: u64 = 16 * 1024;
const CHAIN_LIGHT_WORKING_SET_MIB: u64 = 4 * 1024;
/// Free space `dit-chain` keeps beyond its working set.
const CHAIN_RESERVE_MIB: u64 = 4096;

#[derive(Debug, clap::Args)]
pub(super) struct CombinedStorageArgs {
    #[command(flatten)]
    flags: InventoryFlags,
    /// New directory: selection, storage plan and setup evidence.
    #[arg(long)]
    output_dir: PathBuf,
    /// Light level: sparse test-file capacity on each selected filesystem.
    #[arg(long, default_value_t = 4096)]
    capacity_mib: u64,
    /// Light level: host writes on each selected filesystem.
    #[arg(long, default_value_t = 2048)]
    write_budget_mib: u64,
    #[arg(long, default_value_t = 4096)]
    reserve_free_mib: u64,
}

#[derive(Debug, Serialize)]
struct PlannedDevice {
    alias: usize,
    stable_id: String,
    model: String,
    capacity_bytes: u64,
    /// Kernel name of the namespace (`/proc/diskstats`), e.g. `nvme0n1`.
    block_name: String,
    directory: PathBuf,
}

/// Contract read by `dit-suite` (`combined_run.rs`).
#[derive(Debug, Serialize)]
struct StoragePlan {
    schema_version: u32,
    /// `READY` (directories prepared) or `NONE` (the operator chose no SSD).
    state: &'static str,
    selection: Option<PathBuf>,
    targets: Vec<String>,
    confirm_device_ids: Vec<String>,
    write_level: &'static str,
    capacity_mib: u64,
    write_budget_mib: u64,
    reserve_free_mib: u64,
    /// Directory for the data chain (the first SSD), or `None` when it does not fit.
    chain_dir: Option<PathBuf>,
    chain_working_set_mib: u64,
    chain_note: Option<String>,
    devices: Vec<PlannedDevice>,
    holder_pid: u32,
}

fn write_plan(output: &Path, plan: &StoragePlan) -> Result<(), Box<dyn std::error::Error>> {
    // Written whole, then renamed: the scheduler never reads a partial plan.
    let temporary = output.join("storage-plan.json.tmp");
    let mut file = File::create_new(&temporary)?;
    serde_json::to_writer_pretty(&mut file, plan)?;
    file.sync_all()?;
    std::fs::rename(temporary, output.join("storage-plan.json"))?;
    Ok(())
}

fn block_name(node: &Path) -> String {
    node.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

fn prompt(text: &str) -> io::Result<String> {
    print!("{text}");
    io::stdout().flush()?;
    read_operator_line()
}

#[allow(clippy::too_many_lines)]
pub(super) fn command(args: &CombinedStorageArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let stop = stop_on_signals();
    if args.output_dir.symlink_metadata().is_ok() {
        return Err(format!(
            "output directory already exists and will not be reused: {}",
            args.output_dir.display()
        )
        .into());
    }
    if let Some(parent) = args.output_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir(&args.output_dir)?;
    let none = |note: &str| StoragePlan {
        schema_version: 1,
        state: "NONE",
        selection: None,
        targets: Vec::new(),
        confirm_device_ids: Vec::new(),
        write_level: "NONE",
        capacity_mib: 0,
        write_budget_mib: 0,
        reserve_free_mib: args.reserve_free_mib,
        chain_dir: None,
        chain_working_set_mib: 0,
        chain_note: Some(note.to_owned()),
        devices: Vec::new(),
        holder_pid: std::process::id(),
    };

    let inventory = scan_releasing_stale(&args.flags)?;
    if inventory.devices.is_empty() {
        write_plan(&args.output_dir, &none("no NVMe namespace was discovered"))?;
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "{}",
        render_picker(
            &picker_entries(&inventory),
            &BTreeSet::new(),
            TerminalSize::current()
        )
    );
    let request = prompt(
        "COMBINED: select NVMe aliases for the SSD layer and data chain (1,2 / A = all / N = no SSD): ",
    )?;
    if matches!(
        request.trim().to_ascii_lowercase().as_str(),
        "n" | "none" | ""
    ) {
        write_plan(&args.output_dir, &none("operator chose no SSD"))?;
        return Ok(ExitCode::SUCCESS);
    }
    let selection = parse_selection(&inventory, request.trim())?;
    println!(
        "Write level per SSD: L) Light {} MiB (default)  S) Standard {} MiB  H) Heavy {} MiB",
        args.write_budget_mib,
        WriteLevel::Standard.budget_for(args.write_budget_mib),
        WriteLevel::Heavy.budget_for(args.write_budget_mib)
    );
    println!(
        "  The data chain writes its records once on the first SSD: 16 GiB (Standard/Heavy) or 4 GiB (Light)."
    );
    let level = parse_write_level(&prompt("Level [L] > ")?);
    let capacity_mib = level.capacity_for(args.capacity_mib, args.write_budget_mib);
    let write_budget_mib = level.budget_for(args.write_budget_mib);
    let chain_working_set_mib = if level == WriteLevel::Light {
        CHAIN_LIGHT_WORKING_SET_MIB
    } else {
        CHAIN_WORKING_SET_MIB
    };
    let required = mib(capacity_mib.min(write_budget_mib))?
        .checked_add(mib(args.reserve_free_mib)?)
        .ok_or("free-space requirement overflow")?;
    let chain_required = required
        .checked_add(mib(chain_working_set_mib + CHAIN_RESERVE_MIB)?)
        .ok_or("free-space requirement overflow")?;
    let mut plans = automatic_plans(&inventory, &selection, &args.flags, required)?;
    // The first SSD also carries the chain: prefer a volume with room for both.
    let mut chain_note = None;
    if let Some(first) = selection.devices.first() {
        let device = inventory
            .devices
            .iter()
            .find(|device| device.stable_id == first.stable_id)
            .ok_or("selected SSD disappeared")?;
        match auto_storage::plan(device, &args.flags.protected_devices, chain_required) {
            Ok(mut plan) => {
                plan.alias = first.display_index;
                plans[0] = plan;
            }
            Err(error) => {
                chain_note = Some(format!(
                    "no volume on SSD {} has room for the chain ({chain_working_set_mib} MiB + reserve): {error}",
                    first.display_index
                ));
            }
        }
    }
    let selection_path = args.output_dir.join("selection.json");
    write_selection_file(&selection_path, &selection)?;
    let fresh = scan(&scan_options(&args.flags))?;
    if !revalidate(&selection, &fresh).valid {
        return Err("SSD selection changed; select again".into());
    }
    let mut setup_file = File::create_new(args.output_dir.join("auto-storage-plan.json"))?;
    serde_json::to_writer_pretty(&mut setup_file, &plans)?;
    setup_file.sync_all()?;
    let session = format!(
        "combined-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let mut areas = auto_storage::PreparedAreas::default();
    let mut devices = Vec::with_capacity(plans.len());
    for (position, plan) in plans.iter().enumerate() {
        println!("Preparing [{}] {} ...", plan.alias, plan.node.display());
        let need = if position == 0 && chain_note.is_none() {
            chain_required
        } else {
            required
        };
        match prepare_offering_repair(&mut areas, plan, &session, need) {
            Ok(directory) => {
                let selected = selection
                    .devices
                    .iter()
                    .find(|device| device.stable_id == plan.namespace_id)
                    .ok_or("selected SSD disappeared")?;
                devices.push(PlannedDevice {
                    alias: plan.alias,
                    stable_id: selected.stable_id.0.clone(),
                    model: selected.model.clone(),
                    capacity_bytes: selected.capacity_bytes,
                    block_name: block_name(&selected.device_node),
                    directory,
                });
            }
            Err(error) => {
                let cleanup = areas.cleanup();
                write_setup_outcome(
                    &args.output_dir,
                    "PREPARATION_ERROR",
                    Some(&error.to_string()),
                    &cleanup,
                )?;
                return Err(format!(
                    "AUTO SETUP [{}] {}: {error}",
                    plan.alias,
                    plan.node.display()
                )
                .into());
            }
        }
    }
    let chain_dir = chain_note
        .is_none()
        .then(|| devices.first().map(|device| device.directory.clone()))
        .flatten();
    let plan = StoragePlan {
        schema_version: 1,
        state: "READY",
        selection: Some(selection_path),
        targets: devices
            .iter()
            .map(|device| format!("{}={}", device.alias, device.directory.display()))
            .collect(),
        confirm_device_ids: devices.iter().map(|d| d.stable_id.clone()).collect(),
        write_level: level.label(),
        capacity_mib,
        write_budget_mib,
        reserve_free_mib: args.reserve_free_mib,
        chain_dir,
        chain_working_set_mib,
        chain_note,
        devices,
        holder_pid: std::process::id(),
    };
    // Leave the terminal's foreground group: Ctrl+C now reaches the tests, not this holder.
    let _ = nix::unistd::setpgid(nix::unistd::Pid::from_raw(0), nix::unistd::Pid::from_raw(0));
    stop.store(false, Ordering::SeqCst);
    if let Err(error) = write_plan(&args.output_dir, &plan) {
        let cleanup = areas.cleanup();
        write_setup_outcome(
            &args.output_dir,
            "PLAN_WRITE_ERROR",
            Some(&error.to_string()),
            &cleanup,
        )?;
        return Err(error);
    }
    println!("Storage ready; the combined run takes over the screen.");
    // Every layer and the chain write under these directories: keep their filesystems flushed,
    // so a crash of the machine under test leaves consistent metadata (NTFS `$MFTMirr` too).
    let keeper = auto_storage::SyncKeeper::start(
        plan.devices.iter().map(|d| d.directory.clone()).collect(),
        FS_SYNC_PERIOD,
    );
    let parent = std::os::unix::process::parent_id();
    while !stop.load(Ordering::SeqCst) && std::os::unix::process::parent_id() == parent {
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(keeper);
    // The chain's files live in the directory prepared above for this session; remove them so
    // the directory can go, even when the session ended before it could (shutdown, crash).
    if let Some(chain_dir) = &plan.chain_dir {
        remove_chain_files(chain_dir);
    }
    // A layer that was stopped at the same moment may still hold a file open for a few seconds.
    let mut cleanup = areas.cleanup();
    for _ in 0..12 {
        if cleanup.is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        cleanup = areas.cleanup();
    }
    write_setup_outcome(&args.output_dir, "RELEASED", None, &cleanup)?;
    cleanup?;
    Ok(ExitCode::SUCCESS)
}

/// SIGINT, SIGTERM and SIGHUP set the returned flag. They are blocked before any thread exists
/// and taken by one waiting thread, so a Ctrl+C during preparation cannot kill the process with
/// mounts half made. The helper programs started here (`mount`, `umount`, `blkid`, all under
/// `timeout`) inherit the block; `timeout`'s `--kill-after` still ends a hung one.
fn stop_on_signals() -> Arc<AtomicBool> {
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

/// Removes `dit-chain-<session>.bin` / `.json` from the session's own test directory.
fn remove_chain_files(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_chain =
            name.starts_with("dit-chain-") && (name.ends_with(".bin") || name.ends_with(".json"));
        if is_chain && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
