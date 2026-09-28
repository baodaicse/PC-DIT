use super::{
    BackendArg, CANCELLED, GpuHostSampler, GpuProgressSnapshot, MODULE_ID, RunArgs, SCHEMA_VERSION,
    VulkanDeviceInfo, border, format_bytes, format_duration, install_cancel_handler, meter,
    pipeline_for_device, row, row_colored, unique_id, verdict_name, write_new_json,
};
use dit_contracts::Verdict;
use dit_system::{GpuTelemetry, collect_gpu_inventory, collect_gpu_telemetry, format_gpu_summary};
use dit_ui_core::{
    DashboardMeter, InPlaceRenderer, PickerEntry, TerminalSize, UnifiedDashboard,
    render_device_picker, render_unified_dashboard,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::{OpenOptions, create_dir_all};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const CYAN: &str = "\x1b[36m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";

#[derive(Debug)]
struct ChildTask {
    child: Child,
    progress_path: PathBuf,
    report_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct DeviceReportView {
    verdict: Verdict,
    hardware_tested: bool,
    failure_count: u64,
}

#[derive(Debug, Serialize)]
struct MultiGpuReport {
    schema_version: u32,
    module_id: String,
    run_id: String,
    verdict: Verdict,
    backend: String,
    hardware_tested: bool,
    selected_device_count: usize,
    selected_devices: Vec<VulkanDeviceInfo>,
    duration_target_seconds: u64,
    elapsed_seconds: f64,
    failure_count: u64,
    missing_report_count: usize,
    device_reports: Vec<Value>,
    completion_reason: String,
}

pub(super) fn run_entry(args: &RunArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if args.backend == BackendArg::Mock {
        return super::run_single(&pipeline_for_device(args, None));
    }
    if args.device_index.is_some() && args.devices.is_some() {
        return Err("use either --device-index or --devices, not both".into());
    }
    let inventory = super::Backend::inventory(args.backend)?;
    let selected = if let Some(index) = args.device_index {
        // Preserve the single-device INVALID report path for an explicitly
        // requested software/unsupported device. The picker and --devices
        // still reject blocked devices before launching a multi-GPU campaign.
        inventory
            .iter()
            .any(|device| device.index == index)
            .then_some(vec![index])
            .ok_or_else(|| format!("GPU device index {index} does not exist"))?
    } else if let Some(specification) = args.devices.as_deref() {
        requested_indices(specification, &inventory)?
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() && !args.no_ui {
        interactive_selection(&inventory)?
    } else {
        inventory
            .iter()
            .find(|device| device.hardware_eligible)
            .map(|device| vec![device.index])
            .ok_or("no eligible hardware GPU")?
    };
    if selected.len() == 1 {
        let device = selected
            .first()
            .and_then(|index| inventory.iter().find(|device| device.index == *index));
        let mut single = pipeline_for_device(args, device);
        single.device_index = selected.first().copied();
        single.devices = None;
        return super::run_single(&single);
    }
    run_multi(args, &inventory, &selected)
}

fn interactive_selection(
    inventory: &[VulkanDeviceInfo],
) -> Result<Vec<usize>, Box<dyn std::error::Error>> {
    let pci_inventory = collect_gpu_inventory();
    let entries = inventory
        .iter()
        .enumerate()
        .map(|(position, device)| {
            let driver = device
                .pci_address
                .as_deref()
                .and_then(|pci| {
                    pci_inventory
                        .iter()
                        .find(|item| item.pci_address == pci)
                        .and_then(|item| item.driver.clone())
                })
                .unwrap_or_else(|| "N/A".to_owned());
            PickerEntry {
                alias: position + 1,
                device_node: device
                    .pci_address
                    .clone()
                    .unwrap_or_else(|| format!("vulkan:{}", device.index)),
                model: device.name.clone(),
                capacity: format!("{} local", format_bytes(device.device_local_bytes)),
                serial: device.uuid.clone(),
                driver,
                link: device.api_version.clone(),
                safety: if device.hardware_eligible {
                    "READY".to_owned()
                } else {
                    format!(
                        "BLOCKED:{}",
                        device.rejection_reason.as_deref().unwrap_or("unsupported")
                    )
                },
            }
        })
        .collect::<Vec<_>>();
    print!("\x1b[2J\x1b[H");
    println!(
        "{}",
        render_device_picker(
            "DIT GPU-VRAM // SELECT DEVICES",
            "Enter aliases (1,2) or A=all READY devices; selection stores UUID/BDF",
            &entries,
            &BTreeSet::new(),
            TerminalSize::current(),
        )
    );
    print!("Select GPU aliases (example 1,2) or A for all: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    picker_aliases(input.trim(), inventory)
}

fn picker_aliases(
    input: &str,
    inventory: &[VulkanDeviceInfo],
) -> Result<Vec<usize>, Box<dyn std::error::Error>> {
    if input.eq_ignore_ascii_case("a") {
        let indices = inventory
            .iter()
            .filter(|device| device.hardware_eligible)
            .map(|device| device.index)
            .collect::<Vec<_>>();
        return validate_indices(inventory, indices);
    }
    if input.is_empty() {
        return Err("GPU selection cancelled".into());
    }
    let aliases = input
        .split(',')
        .map(str::trim)
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()?;
    let indices = aliases
        .into_iter()
        .map(|alias| {
            alias
                .checked_sub(1)
                .and_then(|position| inventory.get(position))
                .map(|device| device.index)
                .ok_or_else(|| format!("GPU alias {alias} does not exist"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_indices(inventory, indices)
}

fn requested_indices(
    input: &str,
    inventory: &[VulkanDeviceInfo],
) -> Result<Vec<usize>, Box<dyn std::error::Error>> {
    if input.eq_ignore_ascii_case("all") || input.eq_ignore_ascii_case("a") {
        return validate_indices(
            inventory,
            inventory
                .iter()
                .filter(|device| device.hardware_eligible)
                .map(|device| device.index)
                .collect(),
        );
    }
    let indices = input
        .split(',')
        .map(str::trim)
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()?;
    validate_indices(inventory, indices)
}

fn validate_indices(
    inventory: &[VulkanDeviceInfo],
    indices: Vec<usize>,
) -> Result<Vec<usize>, Box<dyn std::error::Error>> {
    let mut unique = BTreeSet::new();
    for index in indices {
        let device = inventory
            .iter()
            .find(|device| device.index == index)
            .ok_or_else(|| format!("GPU device index {index} does not exist"))?;
        if !device.hardware_eligible {
            return Err(format!(
                "GPU {} is blocked: {}",
                device.name,
                device.rejection_reason.as_deref().unwrap_or("unsupported")
            )
            .into());
        }
        unique.insert(index);
    }
    if unique.is_empty() {
        return Err("no eligible GPU was selected".into());
    }
    Ok(unique.into_iter().collect())
}

#[allow(clippy::too_many_lines)]
fn run_multi(
    args: &RunArgs,
    inventory: &[VulkanDeviceInfo],
    selected_indices: &[usize],
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    install_cancel_handler()?;
    let run_id = unique_id("gpu-multi");
    let output_dir = args
        .output_dir
        .clone()
        .unwrap_or_else(|| Path::new("/run/dit-suite").join(&run_id).join("gpu-vram"));
    create_dir_all(&output_dir)?;
    let selected = selected_indices
        .iter()
        .filter_map(|index| inventory.iter().find(|device| device.index == *index))
        .cloned()
        .collect::<Vec<_>>();
    let integrated_gpu_count = selected
        .iter()
        .filter(|device| device.device_type == "INTEGRATED_GPU")
        .count();
    let executable = std::env::current_exe()?;
    let memory = std::fs::read_to_string("/proc/meminfo")?;
    let available = memory
        .lines()
        .find_map(|line| {
            line.strip_prefix("MemAvailable:")
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<u64>().ok())
        })
        .ok_or("MemAvailable unavailable for GPU worker budgets")?
        .saturating_mul(1024);
    let host_share = available / u64::try_from(selected.len().max(1))?;
    let mut tasks = Vec::new();
    for (position, info) in selected.iter().enumerate() {
        // Each worker gets the pipeline its own device supports (`--async-auto`): async on CUDA,
        // sequential on Vulkan, so one Intel/AMD GPU never makes the NVIDIA ones fall back.
        let args = &pipeline_for_device(args, Some(info));
        let device_dir = output_dir.join(format!("device-{}", position + 1));
        create_dir_all(&device_dir)?;
        let progress_path = device_dir.join("progress.json");
        let report_path = device_dir.join("report.json");
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(device_dir.join("worker.log"))?;
        let error_log = log.try_clone()?;
        let mut command = Command::new(&executable);
        command.env("DIT_GPU_HOST_BUDGET_BYTES", host_share.to_string());
        command
            .arg("run")
            .arg("--backend")
            .arg(match args.backend {
                BackendArg::Cuda => "cuda",
                BackendArg::Vulkan => "vulkan",
                _ => "auto",
            })
            .arg("--device-index")
            .arg(info.index.to_string())
            .arg("--duration-seconds")
            .arg(args.duration_seconds.to_string())
            .arg("--vram-percent")
            .arg(multi_device_vram_percent(info, args, integrated_gpu_count).to_string())
            .arg("--max-allocation-mib")
            .arg(args.max_allocation_mib.to_string())
            .arg("--blocks-per-generation")
            .arg(args.blocks_per_generation.to_string())
            .arg("--seed")
            .arg(args.seed.to_string())
            .arg("--no-ui")
            .arg("--output-dir")
            .arg(&device_dir)
            .arg("--progress-file")
            .arg(&progress_path)
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(error_log));
        if args.fault_events.is_some() {
            command
                .arg("--fault-events")
                .arg(device_dir.join("fault-events.jsonl"));
        }
        if args.async_compute {
            command.arg("--async-compute");
        }
        if let Some(jobs) = args.matrix_jobs {
            command.arg("--matrix-jobs").arg(jobs.to_string());
        }
        if let Some(policy) = &args.hash_policy {
            command.arg("--hash-policy").arg(policy);
        }
        if let Some(mix) = args.compute_mix
            && let Some(value) = clap::ValueEnum::to_possible_value(&mix)
        {
            command.arg("--compute-mix").arg(value.get_name());
        }
        if let Some(batches) = args.corpus_rotate_batches {
            command
                .arg("--corpus-rotate-batches")
                .arg(batches.to_string());
        }
        if let Some(floor) = args.coverage_floor {
            command.arg("--coverage-floor").arg(floor.to_string());
        }
        if args.bottleneck_log.is_some() {
            // Each worker is a separate process; redirect to its own file under device_dir
            // rather than forwarding the shared top-level path verbatim (would corrupt from
            // concurrent writers).
            command
                .arg("--bottleneck-log")
                .arg(device_dir.join("bottleneck.jsonl"));
        }
        args.lane.forward(&mut command);
        // Every worker sees how many peers share the machine, so each takes only its share of
        // the CPU cores for verifiers and corpus helpers.
        command.env("DIT_GPU_PEER_COUNT", selected.len().max(1).to_string());
        for (flag, value) in [
            (
                "--async-device-slots",
                args.async_device_slots.map(|n| n as u64),
            ),
            (
                "--async-host-slots",
                args.async_host_slots.map(|n| n as u64),
            ),
            ("--async-workers", args.async_workers.map(|n| n as u64)),
            (
                "--async-workers-max",
                args.async_workers_max.map(|n| n as u64),
            ),
            ("--async-batches", args.async_batches),
            ("--corpus-threads", args.corpus_threads.map(|n| n as u64)),
            (
                "--quarantine-slots",
                args.quarantine_slots.map(|n| n as u64),
            ),
        ] {
            if let Some(value) = value {
                command.arg(flag).arg(value.to_string());
            }
        }
        if let Some(generations) = args.generations {
            command.arg("--generations").arg(generations.to_string());
        }
        if let Some(vram_mib) = args.vram_mib {
            command.arg("--vram-mib").arg(vram_mib.to_string());
        }
        if args.vram_max {
            command.arg("--vram-max");
        }
        if let Some(margin) = args.vram_margin_mib {
            command.arg("--vram-margin-mib").arg(margin.to_string());
        }
        if args.stop_on_error {
            command.arg("--stop-on-error");
        }
        if let Some(limit) = args.temp_limit_celsius {
            command.arg("--temp-limit-celsius").arg(limit.to_string());
        }
        if position == 0
            && let Some(fault) = args.inject_vulkan_fault
        {
            command.arg("--inject-vulkan-fault").arg(match fault {
                super::VulkanFaultArg::Transfer => "transfer",
                super::VulkanFaultArg::Matrix => "matrix",
            });
        }
        if let Some(rounds) = args.compute_rounds {
            command.arg("--compute-rounds").arg(rounds.to_string());
        }
        if args.recreate_compute_resources {
            command.arg("--recreate-compute-resources");
        }
        tasks.push(ChildTask {
            child: command.spawn()?,
            progress_path,
            report_path,
        });
    }

    let started = Instant::now();
    let use_ui = !args.no_ui && io::stdout().is_terminal();
    let mut renderer = use_ui.then(|| InPlaceRenderer::new(io::stdout().lock(), 8));
    let mut host = GpuHostSampler::detect();
    // Inventory probing may invoke vendor utilities such as nvidia-smi. Keep
    // it out of the 8 FPS render loop so telemetry/UI refresh remains smooth.
    let pci_inventory = collect_gpu_inventory();
    let mut parent_telemetry = collect_gpu_telemetry(&pci_inventory);
    let mut last_parent_telemetry = Instant::now();
    let mut system = host.system_info();
    system.gpu = format_gpu_summary(&pci_inventory);
    let mut snapshots = selected.iter().map(initial_snapshot).collect::<Vec<_>>();
    merge_parent_telemetry(&mut snapshots, &parent_telemetry);
    let mut cancel_started = None;
    loop {
        let mut running = 0_usize;
        for (position, task) in tasks.iter_mut().enumerate() {
            if let Some(snapshot) = read_progress(&task.progress_path) {
                snapshots[position] = snapshot;
            }
            if task.child.try_wait()?.is_none() {
                running += 1;
            }
        }
        if last_parent_telemetry.elapsed() >= Duration::from_secs(1) {
            parent_telemetry = collect_gpu_telemetry(&pci_inventory);
            last_parent_telemetry = Instant::now();
        }
        merge_parent_telemetry(&mut snapshots, &parent_telemetry);
        if let Some(renderer) = renderer.as_mut() {
            renderer.render_frame(&render_multi_gpu_ui(
                &system,
                &host.sample(),
                &snapshots,
                started.elapsed(),
                args,
                TerminalSize::current(),
                true,
            ))?;
        }
        if running == 0 {
            break;
        }
        if CANCELLED.load(Ordering::SeqCst) {
            let since = cancel_started.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_secs(35) {
                for task in &mut tasks {
                    if task.child.try_wait()?.is_none() {
                        task.child.kill()?;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(125));
    }
    for task in &mut tasks {
        let _ = task.child.wait()?;
    }

    let mut report_values = Vec::new();
    let mut report_views = Vec::new();
    let mut missing = 0_usize;
    for (position, task) in tasks.iter().enumerate() {
        match std::fs::read(&task.report_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            Some(value) => {
                if let Ok(view) = serde_json::from_value::<DeviceReportView>(value.clone()) {
                    verdict_name(view.verdict).clone_into(&mut snapshots[position].state);
                    snapshots[position].failures = view.failure_count;
                    report_views.push(view);
                    report_values.push(value);
                } else {
                    missing += 1;
                }
            }
            None => missing += 1,
        }
    }
    let verdict = aggregate_verdict(&report_views, missing);
    let hardware_tested = missing == 0
        && !report_views.is_empty()
        && report_views.iter().all(|report| report.hardware_tested);
    let failure_count = report_views
        .iter()
        .map(|report| report.failure_count)
        .fold(0_u64, u64::saturating_add);
    if let Some(renderer) = renderer.as_mut() {
        renderer.render_frame(&render_multi_gpu_ui(
            &system,
            &host.sample(),
            &snapshots,
            started.elapsed(),
            args,
            TerminalSize::current(),
            true,
        ))?;
        renderer.finish()?;
    }
    let report = MultiGpuReport {
        schema_version: SCHEMA_VERSION,
        module_id: MODULE_ID.to_owned(),
        run_id,
        verdict,
        backend: "CUDA_VULKAN_MULTI_GPU".to_owned(),
        hardware_tested,
        selected_device_count: selected.len(),
        selected_devices: selected,
        duration_target_seconds: args.duration_seconds,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        failure_count,
        missing_report_count: missing,
        device_reports: report_values,
        completion_reason: if CANCELLED.load(Ordering::SeqCst) {
            "user_cancel".to_owned()
        } else {
            "all_workers_finished".to_owned()
        },
    };
    let report_path = output_dir.join("report.json");
    write_new_json(&report_path, &report)?;
    println!(
        "{} CUDA_VULKAN_MULTI_GPU devices={} hardware_tested={} report={}",
        verdict_name(verdict),
        report.selected_device_count,
        report.hardware_tested,
        report_path.display()
    );
    Ok(match verdict {
        Verdict::Pass => ExitCode::SUCCESS,
        Verdict::Fail => ExitCode::from(1),
        _ => ExitCode::from(2),
    })
}

fn multi_device_vram_percent(
    info: &VulkanDeviceInfo,
    args: &RunArgs,
    integrated_gpu_count: usize,
) -> u8 {
    if info.device_type == "INTEGRATED_GPU" && args.vram_mib.is_none() && integrated_gpu_count > 0 {
        let count = u8::try_from(integrated_gpu_count).unwrap_or(u8::MAX).max(1);
        args.vram_percent.min(60_u8.saturating_div(count).max(1))
    } else {
        args.vram_percent
    }
}

fn initial_snapshot(info: &VulkanDeviceInfo) -> GpuProgressSnapshot {
    GpuProgressSnapshot {
        device_index: info.index,
        name: info.name.clone(),
        pci_address: info.pci_address.clone(),
        uuid: info.uuid.clone(),
        state: "PREPARING".to_owned(),
        elapsed_seconds: 0.0,
        duration_seconds: 0,
        generation: 0,
        seed_index: 0,
        allocation_bytes: 0,
        allocation_target_bytes: 0,
        chunk_bytes: 0,
        transfer_mib_s: None,
        sweeps: 0,
        bytes_verified: 0,
        matrix_results_verified: 0,
        failures: 0,
        gpu_usage_percent: None,
        vram_usage_percent: None,
        vram_used_bytes: None,
        vram_total_bytes: Some(info.device_local_bytes),
        temperature_celsius: None,
        power_watts: None,
        clock_mhz: None,
        memory_clock_mhz: None,
        telemetry_source: "waiting for worker telemetry".to_owned(),
        hardware_note: None,
    }
}

fn read_progress(path: &Path) -> Option<GpuProgressSnapshot> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn merge_parent_telemetry(snapshots: &mut [GpuProgressSnapshot], telemetry: &[GpuTelemetry]) {
    for snapshot in snapshots {
        let Some(pci_address) = snapshot.pci_address.as_deref() else {
            continue;
        };
        let Some(sample) = telemetry
            .iter()
            .find(|sample| sample.pci_address == pci_address)
        else {
            continue;
        };
        snapshot.gpu_usage_percent = sample.utilization_percent.or(snapshot.gpu_usage_percent);
        snapshot.vram_used_bytes = sample.vram_used_bytes.or(snapshot.vram_used_bytes);
        snapshot.vram_total_bytes = sample.vram_total_bytes.or(snapshot.vram_total_bytes);
        snapshot.vram_usage_percent =
            super::vram_capacity_percent(snapshot.vram_used_bytes, snapshot.vram_total_bytes);
        snapshot.temperature_celsius = sample.temperature_celsius.or(snapshot.temperature_celsius);
        snapshot.power_watts = sample.power_watts.or(snapshot.power_watts);
        snapshot.clock_mhz = sample.core_clock_mhz.or(snapshot.clock_mhz);
        snapshot.memory_clock_mhz = sample.memory_clock_mhz.or(snapshot.memory_clock_mhz);
        if snapshot.telemetry_source.starts_with("N/A")
            || snapshot.telemetry_source == "waiting for worker telemetry"
        {
            snapshot.telemetry_source = sample.source.clone();
        }
    }
}

fn aggregate_verdict(reports: &[DeviceReportView], missing: usize) -> Verdict {
    if missing > 0
        || reports
            .iter()
            .any(|report| report.verdict == Verdict::Invalid)
    {
        Verdict::Invalid
    } else if reports.iter().any(|report| report.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if reports
        .iter()
        .any(|report| report.verdict == Verdict::Aborted)
    {
        Verdict::Aborted
    } else if !reports.is_empty() && reports.iter().all(|report| report.verdict == Verdict::Pass) {
        Verdict::Pass
    } else {
        Verdict::NotRun
    }
}

#[allow(clippy::too_many_lines)]
fn render_multi_gpu_ui(
    system: &dit_ui_core::SystemInfo,
    live: &dit_ui_core::LiveUtilization,
    devices: &[GpuProgressSnapshot],
    elapsed: Duration,
    args: &RunArgs,
    size: TerminalSize,
    color: bool,
) -> String {
    // Identity, two meter+detail pairs and one RUNNING row per visible GPU.
    let rows_per_gpu = 5_usize;
    let fixed_rows = 21_usize;
    let available = usize::from(size.rows)
        .saturating_sub(fixed_rows)
        .max(rows_per_gpu);
    let visible_count = (available / rows_per_gpu).max(1).min(devices.len().max(1));
    let pages = devices.len().div_ceil(visible_count).max(1);
    let page = usize::try_from(elapsed.as_secs() / 5).unwrap_or(0) % pages;
    let first = page * visible_count;
    let visible = devices
        .iter()
        .enumerate()
        .skip(first)
        .take(visible_count)
        .collect::<Vec<_>>();
    let failures = devices.iter().map(|device| device.failures).sum::<u64>();
    let state = if devices.iter().any(|device| device.state == "FAIL") {
        "FAIL"
    } else if devices.iter().any(|device| device.state == "INVALID") {
        "INVALID"
    } else if devices.iter().all(|device| device.state == "PASS") {
        "PASS"
    } else {
        "RUNNING"
    };
    let progress = if let Some(target) = args.generations {
        let complete = devices
            .iter()
            .map(|device| device.generation.min(target))
            .sum::<u64>();
        super::percent(
            complete,
            target.saturating_mul(u64::try_from(devices.len()).unwrap_or(1)),
        )
    } else {
        super::percent(elapsed.as_secs(), args.duration_seconds)
    };
    let mut hardware_rows = vec![
        format!("CPU:     {}", system.cpu),
        format!("THREADS: {} logical CPU(s)", system.logical_threads),
        format!("RAM:     {} total", format_bytes(system.memory_total_bytes)),
        format!("GPU:     {} selected", devices.len()),
        format!("BOARD:   {}", system.board),
        format!("KERNEL:  {}", system.kernel),
    ];
    hardware_rows.extend(visible.iter().map(|(position, device)| {
        format!(
            "GPU {}: {} [{}] UUID {}",
            position + 1,
            device.name,
            device.pci_address.as_deref().unwrap_or("BDF N/A"),
            device.uuid
        )
    }));
    let ram_percent = super::percent(live.memory_used_bytes, live.memory_total_bytes);
    let cpu_detail = format!(
        "Freq {} / Temp {} / Power {}",
        live.cpu_frequency_ghz
            .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.2} GHz")),
        super::format_optional(live.cpu_temperature_celsius, "C"),
        super::format_optional(live.cpu_power_watts, "W")
    );
    let mut live_meters = vec![
        DashboardMeter::new("CPU USAGE", live.cpu_percent).detail(cpu_detail),
        DashboardMeter::new("MEMORY", Some(ram_percent)).detail(format!(
            "Used {} / {}",
            format_bytes(live.memory_used_bytes),
            format_bytes(live.memory_total_bytes)
        )),
    ];
    for (position, device) in &visible {
        let label = format!("GPU {}", position + 1);
        live_meters.push(
            DashboardMeter::new(format!("{label} USAGE"), device.gpu_usage_percent).detail(
                format!(
                    "Clock {} / Temp {} / Power {}",
                    device.clock_mhz.map_or_else(
                        || "N/A".to_owned(),
                        |value| format!("{:.2} GHz", value / 1000.0)
                    ),
                    device
                        .temperature_celsius
                        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} C")),
                    device
                        .power_watts
                        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} W"))
                ),
            ),
        );
        live_meters.push(
            DashboardMeter::new(format!("{label} VRAM"), device.vram_usage_percent).detail(
                format!(
                    "Used {} / {} | TEST {} / {} | H2D+D2H {}",
                    device
                        .vram_used_bytes
                        .map_or_else(|| "N/A".to_owned(), format_bytes),
                    device
                        .vram_total_bytes
                        .map_or_else(|| "N/A".to_owned(), format_bytes),
                    format_bytes(device.allocation_bytes),
                    format_bytes(device.allocation_target_bytes),
                    device
                        .transfer_mib_s
                        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} MiB/s"))
                ),
            ),
        );
    }
    let calculations = devices
        .iter()
        .map(|device| device.matrix_results_verified)
        .sum();
    let running_rows = visible
        .iter()
        .map(|(position, device)| {
            format!(
                "GPU {} STATE {}{} | GEN {} SEED#{} SWEEP {} | CHUNK {} | VERIFIED {} | MATRIX {} | FAIL {} | {}",
                position + 1,
                device.state,
                if device.hardware_note.is_some() { " (HW WARN)" } else { "" },
                device.generation,
                device.seed_index,
                device.sweeps,
                format_bytes(device.chunk_bytes),
                format_bytes(device.bytes_verified),
                device.matrix_results_verified,
                device.failures,
                device.telemetry_source
            )
        })
        .collect();
    let target = args.generations.map_or_else(
        || format_duration(args.duration_seconds),
        |value| format!("{value} generations"),
    );
    render_unified_dashboard(
        &UnifiedDashboard {
            engine: "GPU-G3-1".to_owned(),
            report_abi: SCHEMA_VERSION.to_string(),
            profile: format!(
                "GPU + VRAM / DEEP | CUDA/VULKAN | SELECTED {} | PAGE {}/{}",
                devices.len(),
                page + 1,
                pages
            ),
            hardware_rows,
            live_meters,
            state: state.to_owned(),
            progress_percent: Some(progress),
            elapsed: format_duration(elapsed.as_secs()),
            target,
            active_test: "GPU-XFER-MATRIX-MULTI  CPU -> PCIe -> VRAM -> GPU -> CPU oracle"
                .to_owned(),
            process:
                "isolated workers; split/scatter + retention + tiled u32 GEMM + exact readback"
                    .to_owned(),
            calculations_completed: calculations,
            failures,
            running_rows,
            footer: "TEMP LIMIT default OFF | Ctrl+C drains workers and preserves all reports"
                .to_owned(),
        },
        size,
        color,
    )
}

#[allow(dead_code, clippy::too_many_lines)]
fn render_multi_gpu_ui_legacy(
    system: &dit_ui_core::SystemInfo,
    live: &dit_ui_core::LiveUtilization,
    devices: &[GpuProgressSnapshot],
    elapsed: Duration,
    args: &RunArgs,
    size: TerminalSize,
    color: bool,
) -> String {
    let width = usize::from(size.cols).clamp(60, 200);
    let inner = width - 2;
    let fixed_rows = 14_usize;
    let rows_per_gpu = 5_usize;
    let available = usize::from(size.rows)
        .saturating_sub(fixed_rows + 1)
        .max(rows_per_gpu);
    let visible_count = (available / rows_per_gpu).max(1).min(devices.len().max(1));
    let pages = devices.len().div_ceil(visible_count).max(1);
    let page = usize::try_from(elapsed.as_secs() / 5).unwrap_or(0) % pages;
    let first = page * visible_count;
    let failures = devices.iter().map(|device| device.failures).sum::<u64>();
    let state = if devices.iter().any(|device| device.state == "FAIL") {
        "FAIL"
    } else if devices.iter().any(|device| device.state == "INVALID") {
        "INVALID"
    } else if devices.iter().all(|device| device.state == "PASS") {
        "PASS"
    } else {
        "RUNNING"
    };
    let progress = if args.generations.is_some() {
        let target = args.generations.unwrap_or(1);
        let complete = devices
            .iter()
            .map(|device| device.generation.min(target))
            .sum::<u64>();
        super::percent(
            complete,
            target.saturating_mul(u64::try_from(devices.len()).unwrap_or(1)),
        )
    } else {
        super::percent(elapsed.as_secs(), args.duration_seconds)
    };
    let mut lines = vec![
        border("", inner, color),
        row_colored("DIT-01 / DATA INTEGRITY TEST ENGINE", inner, CYAN, color),
        row_colored(
            &format!(
                "ENGINE GPU-G3-1   BACKEND CUDA/VULKAN   PROFILE GPU + VRAM / DEEP   SELECTED {}   PAGE {}/{}",
                devices.len(),
                page + 1,
                pages
            ),
            inner,
            GREEN,
            color,
        ),
    ];
    if width >= 110 {
        let gap = 2_usize;
        let left_total = (width - gap) / 2;
        let right_total = width - gap - left_total;
        let left_inner = left_total.saturating_sub(2);
        let right_inner = right_total.saturating_sub(2);
        let pair = |left: String, right: String| format!("{left}{}{right}", " ".repeat(gap));
        lines.extend([
            pair(
                border("SYSTEM HARDWARE", left_inner, color),
                border("LIVE UTILIZATION", right_inner, color),
            ),
            pair(
                row(&format!("CPU:     {}", system.cpu), left_inner),
                meter("CPU USAGE", live.cpu_percent, right_inner, color),
            ),
            pair(
                row(
                    &format!("THREADS: {} logical CPU(s)", system.logical_threads),
                    left_inner,
                ),
                row(
                    &format!(
                        "{} / Temp {} / Power {}",
                        live.cpu_frequency_ghz.map_or_else(
                            || "GHz N/A".to_owned(),
                            |value| format!("{value:.2} GHz")
                        ),
                        super::format_optional(live.cpu_temperature_celsius, "C"),
                        super::format_optional(live.cpu_power_watts, "W")
                    ),
                    right_inner,
                ),
            ),
            pair(
                row(
                    &format!("RAM:     {} total", format_bytes(system.memory_total_bytes)),
                    left_inner,
                ),
                meter(
                    "MEMORY",
                    Some(super::percent(
                        live.memory_used_bytes,
                        live.memory_total_bytes,
                    )),
                    right_inner,
                    color,
                ),
            ),
            pair(
                row(&format!("GPU:     {} selected", devices.len()), left_inner),
                row(
                    &format!(
                        "Used {} / {}",
                        format_bytes(live.memory_used_bytes),
                        format_bytes(live.memory_total_bytes)
                    ),
                    right_inner,
                ),
            ),
            pair(
                row(&format!("BOARD:   {}", system.board), left_inner),
                row(&format!("KERNEL: {}", system.kernel), right_inner),
            ),
            pair(
                border("", left_inner, color),
                border("", right_inner, color),
            ),
        ]);
    } else {
        lines.extend([
            border("SYSTEM HARDWARE / LIVE UTILIZATION", inner, color),
            row(
                &format!(
                    "CPU: {} / RAM: {} / GPU: {} selected",
                    system.cpu,
                    format_bytes(system.memory_total_bytes),
                    devices.len()
                ),
                inner,
            ),
            meter("CPU USAGE", live.cpu_percent, inner, color),
            meter(
                "MEMORY",
                Some(super::percent(
                    live.memory_used_bytes,
                    live.memory_total_bytes,
                )),
                inner,
                color,
            ),
        ]);
    }
    lines.push(border("SELECTED GPU / DYNAMIC STATUS", inner, color));
    for (position, device) in devices.iter().enumerate().skip(first).take(visible_count) {
        let label = format!("GPU {}", position + 1);
        lines.push(row_colored(
            &format!(
                "{label}: {} [{}] UUID {} STATE {}{}",
                device.name,
                device.pci_address.as_deref().unwrap_or("BDF N/A"),
                device.uuid,
                device.state,
                device
                    .hardware_note
                    .as_ref()
                    .map_or_else(String::new, |note| format!(" | HARDWARE {note}"))
            ),
            inner,
            if device.failures > 0 || device.hardware_note.is_some() {
                YELLOW
            } else {
                CYAN
            },
            color,
        ));
        lines.push(meter(
            &format!("{label} USE"),
            device.gpu_usage_percent,
            inner,
            color,
        ));
        lines.push(meter(
            &format!("{label} VRAM"),
            device.vram_usage_percent,
            inner,
            color,
        ));
        lines.push(row(
            &format!(
                "{label} TEST {} / {}  VRAM {} / {}  CHUNK {}  H2D+D2H {}",
                format_bytes(device.allocation_bytes),
                format_bytes(device.allocation_target_bytes),
                device
                    .vram_used_bytes
                    .map_or_else(|| "N/A".to_owned(), format_bytes),
                device
                    .vram_total_bytes
                    .map_or_else(|| "N/A".to_owned(), format_bytes),
                format_bytes(device.chunk_bytes),
                device
                    .transfer_mib_s
                    .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} MiB/s"))
            ),
            inner,
        ));
        lines.push(row(
            &format!(
                "{label} GEN {} SEED#{} SWEEP {} VERIFIED {} MATRIX {} FAIL {} / {} / Temp {} / Power {} / {}",
                device.generation,
                device.seed_index,
                device.sweeps,
                format_bytes(device.bytes_verified),
                device.matrix_results_verified,
                device.failures,
                device
                    .clock_mhz
                    .map_or_else(|| "GHz N/A".to_owned(), |value| format!("{:.2} GHz", value / 1000.0)),
                device
                    .temperature_celsius
                    .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} C")),
                device
                    .power_watts
                    .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} W")),
                device.telemetry_source
            ),
            inner,
        ));
    }
    lines.push(border("TESTING / PROCESS INFORMATION", inner, color));
    lines.push(row(
        &format!(
            "STATE: {state}   PROGRESS: {progress:.0}%   TIME: {}s / {}s   FAIL TOTAL: {failures}",
            elapsed.as_secs(),
            args.duration_seconds
        ),
        inner,
    ));
    lines.push(super::meter_fixed(
        "PROGRESS",
        Some(progress),
        inner,
        color,
        GREEN,
    ));
    lines.push(row_colored(
        "TEMP LIMIT default OFF | Ctrl+C drains workers and preserves aggregate/per-GPU reports",
        inner,
        YELLOW,
        color,
    ));
    lines.push(border("", inner, color));
    let maximum = usize::from(size.rows).saturating_sub(1).max(1);
    lines.truncate(maximum);
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(count: usize) -> Vec<VulkanDeviceInfo> {
        (0..count)
            .map(|index| VulkanDeviceInfo {
                index,
                name: format!("GPU-{index}"),
                device_type: "DISCRETE_GPU".to_owned(),
                hardware_eligible: true,
                rejection_reason: None,
                vendor_id: 1,
                device_id: u32::try_from(index).unwrap_or(0),
                api_version: "1.3".to_owned(),
                driver_version_raw: 1,
                uuid: format!("uuid-{index}"),
                pci_address: Some(format!("0000:0{index}:00.0")),
                compute_queue_family: Some(0),
                device_local_bytes: 8 * 1024 * super::super::MIB,
                budget_bytes: Some(7 * 1024 * super::super::MIB),
                usage_bytes: Some(1024 * super::super::MIB),
                memory_budget_supported: true,
            })
            .collect()
    }

    #[test]
    fn picker_supports_aliases_all_and_rejects_software_gpu() {
        let mut inventory = devices(2);
        let mut software = devices(1).remove(0);
        software.index = 2;
        software.name = "llvmpipe".to_owned();
        software.hardware_eligible = false;
        software.rejection_reason = Some("software renderer".to_owned());
        inventory.push(software);
        assert_eq!(picker_aliases("1,2", &inventory).unwrap(), vec![0, 1]);
        assert_eq!(picker_aliases("A", &inventory).unwrap(), vec![0, 1]);
        assert!(picker_aliases("3", &inventory).is_err());
    }

    #[test]
    fn parent_telemetry_fills_missing_worker_metrics_by_pci_address() {
        let device = devices(1).remove(0);
        let mut snapshots = vec![initial_snapshot(&device)];
        let telemetry = vec![GpuTelemetry {
            pci_address: device.pci_address.clone().unwrap(),
            utilization_percent: Some(31.0),
            memory_utilization_percent: Some(26.0),
            vram_used_bytes: Some(672 * 1024 * 1024),
            vram_total_bytes: Some(2 * 1024 * 1024 * 1024),
            temperature_celsius: Some(44.0),
            power_watts: Some(18.61),
            core_clock_mhz: Some(734.0),
            memory_clock_mhz: Some(405.0),
            power_limit_watts: None,
            throttle_reasons_mask: None,
            source: "test telemetry".to_owned(),
        }];

        merge_parent_telemetry(&mut snapshots, &telemetry);

        assert_eq!(snapshots[0].gpu_usage_percent, Some(31.0));
        assert_eq!(snapshots[0].vram_usage_percent, Some(32.8125));
        assert_eq!(snapshots[0].vram_used_bytes, Some(672 * 1024 * 1024));
        assert_eq!(snapshots[0].temperature_celsius, Some(44.0));
        assert_eq!(snapshots[0].power_watts, Some(18.61));
        assert_eq!(snapshots[0].telemetry_source, "test telemetry");
    }

    #[test]
    fn aggregate_verdict_keeps_device_failure_and_missing_report() {
        let pass = DeviceReportView {
            verdict: Verdict::Pass,
            hardware_tested: true,
            failure_count: 0,
        };
        let fail = DeviceReportView {
            verdict: Verdict::Fail,
            hardware_tested: false,
            failure_count: 1,
        };
        assert_eq!(aggregate_verdict(&[pass], 0), Verdict::Pass);
        assert_eq!(aggregate_verdict(&[fail], 0), Verdict::Fail);
        assert_eq!(aggregate_verdict(&[], 1), Verdict::Invalid);
    }

    #[test]
    fn dynamic_dashboard_fits_one_two_and_four_gpu_layouts() {
        let args = RunArgs {
            duration_seconds: 1_800,
            generations: None,
            backend: BackendArg::Vulkan,
            device_index: None,
            devices: None,
            vram_percent: 85,
            vram_mib: None,
            max_allocation_mib: 256,
            matrix_jobs: Some(64),
            async_auto: false,
            vram_max: false,
            vram_margin_mib: None,
            compute_mix: None,
            compute_rounds: Some(1),
            recreate_compute_resources: false,
            async_compute: false,
            hash_policy: None,
            corpus_rotate_batches: None,
            async_device_slots: None,
            async_host_slots: None,
            async_workers: None,
            async_workers_max: None,
            coverage_floor: None,
            bottleneck_log: None,
            lane: crate::LaneArgs::default(),
            async_batches: None,
            corpus_threads: None,
            quarantine_slots: None,
            blocks_per_generation: 1,
            seed: 1,
            output_dir: None,
            no_ui: false,
            stop_on_error: false,
            temp_limit_celsius: None,
            inject_fault: None,
            inject_vulkan_fault: None,
            progress_file: None,
            fault_events: None,
        };
        for count in [1, 2, 4] {
            let snapshots = devices(count)
                .iter()
                .map(initial_snapshot)
                .collect::<Vec<_>>();
            let frame = render_multi_gpu_ui(
                &dit_ui_core::SystemInfo::default(),
                &dit_ui_core::LiveUtilization::default(),
                &snapshots,
                Duration::from_secs(10),
                &args,
                TerminalSize {
                    rows: 40,
                    cols: 140,
                },
                false,
            );
            assert!(frame.lines().all(|line| line.chars().count() == 140));
            assert!(frame.lines().count() < 40);
            assert!(frame.contains("GPU 1"));
        }
    }

    #[test]
    fn multiple_integrated_gpus_share_the_conservative_system_ram_budget() {
        let mut integrated = devices(1).remove(0);
        integrated.device_type = "INTEGRATED_GPU".to_owned();
        let mut args = RunArgs {
            duration_seconds: 1_800,
            generations: None,
            backend: BackendArg::Vulkan,
            device_index: None,
            devices: None,
            vram_percent: 95,
            vram_mib: None,
            max_allocation_mib: 256,
            matrix_jobs: Some(64),
            async_auto: false,
            vram_max: false,
            vram_margin_mib: None,
            compute_mix: None,
            compute_rounds: Some(1),
            recreate_compute_resources: false,
            async_compute: false,
            hash_policy: None,
            corpus_rotate_batches: None,
            async_device_slots: None,
            async_host_slots: None,
            async_workers: None,
            async_workers_max: None,
            coverage_floor: None,
            bottleneck_log: None,
            lane: crate::LaneArgs::default(),
            async_batches: None,
            corpus_threads: None,
            quarantine_slots: None,
            blocks_per_generation: 1,
            seed: 1,
            output_dir: None,
            no_ui: false,
            stop_on_error: false,
            temp_limit_celsius: None,
            inject_fault: None,
            inject_vulkan_fault: None,
            progress_file: None,
            fault_events: None,
        };
        assert_eq!(multi_device_vram_percent(&integrated, &args, 1), 60);
        assert_eq!(multi_device_vram_percent(&integrated, &args, 2), 30);
        let discrete = devices(1).remove(0);
        assert_eq!(multi_device_vram_percent(&discrete, &args, 2), 95);
        args.vram_mib = Some(64);
        assert_eq!(multi_device_vram_percent(&integrated, &args, 2), 95);
    }
}
