//! `NVMe` adapter for the shared DIT dashboard contract.

use super::{
    DashboardMeter, LayoutMode, TerminalSize, UiState, UnifiedDashboard, aggregate_state,
    calculate_layout, format_bytes, format_rate, phase_name, render_unified_dashboard,
    verdict_or_running,
};

#[allow(clippy::cast_precision_loss)]
fn percent(value: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (100.0 * value as f64 / total as f64).clamp(0.0, 100.0)
    }
}

fn cpu_sensor_row(live: &super::LiveUtilization) -> String {
    let frequency = live
        .cpu_frequency_ghz
        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.2} GHz"));
    let temperature = live
        .cpu_temperature_celsius
        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} C"));
    let power = live
        .cpu_power_watts
        .map_or_else(|| "N/A".to_owned(), |value| format!("{value:.1} W"));
    format!("Freq {frequency} / Temp {temperature} / Power {power}")
}

struct Summary {
    completed: u64,
    target: u64,
    failures: u64,
    matrix_checks: u64,
    compute_jobs: u64,
    write_operations: u64,
    read_soak_operations: u64,
    compute_workers: usize,
    matrix_rounds: u32,
    state: &'static str,
}

impl Summary {
    fn new(state: &UiState) -> Self {
        let mut summary = Self {
            completed: 0,
            target: 0,
            failures: 0,
            matrix_checks: 0,
            compute_jobs: 0,
            write_operations: 0,
            read_soak_operations: 0,
            compute_workers: 0,
            matrix_rounds: 0,
            state: aggregate_state(state.devices.values()),
        };
        for device in state.devices.values() {
            summary.completed = summary.completed.saturating_add(device.completed);
            summary.target = summary.target.saturating_add(device.target);
            summary.failures = summary.failures.saturating_add(device.failures);
            summary.matrix_checks = summary.matrix_checks.saturating_add(device.matrix_checks);
            summary.compute_jobs = summary.compute_jobs.saturating_add(device.compute_jobs);
            summary.write_operations = summary
                .write_operations
                .saturating_add(device.write_operations);
            summary.read_soak_operations = summary
                .read_soak_operations
                .saturating_add(device.read_soak_operations);
            summary.compute_workers = summary
                .compute_workers
                .saturating_add(device.compute_workers);
            summary.matrix_rounds = summary.matrix_rounds.max(device.matrix_rounds);
        }
        summary
    }

    fn progress(&self, state: &UiState) -> f64 {
        state.duration_limit.map_or_else(
            || percent(self.completed, self.target),
            |limit| {
                let total_ms = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX);
                let elapsed_ms =
                    u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX);
                percent(elapsed_ms, total_ms)
            },
        )
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn render(state: &UiState, size: TerminalSize, color: bool) -> String {
    let page = state
        .page
        .saturating_add(usize::try_from(state.started.elapsed().as_secs() / 5).unwrap_or(0));
    let layout = calculate_layout(size, state.order.len(), page);
    let visible = state
        .order
        .iter()
        .enumerate()
        .skip(layout.first_device)
        .take(layout.visible_devices)
        .filter_map(|(index, id)| state.devices.get(id).map(|device| (index + 1, device)))
        .collect::<Vec<_>>();
    let summary = Summary::new(state);
    let progress = summary.progress(state);
    let mut hardware_rows = vec![
        format!("CPU:     {}", state.system.cpu),
        format!("THREADS: {} logical CPU(s)", state.system.logical_threads),
        format!(
            "RAM:     {} total system memory",
            format_bytes(state.system.memory_total_bytes)
        ),
        format!("GPU:     {}", state.system.gpu),
        format!("BOARD:   {}", state.system.board),
        format!("KERNEL:  {}", state.system.kernel),
    ];
    hardware_rows.extend(visible.iter().map(|(index, device)| {
        format!(
            "SSD {index}: {} | {} | {}",
            device.descriptor.model, device.descriptor.capacity, device.descriptor.detail
        )
    }));

    let memory_percent = (state.live.memory_total_bytes > 0)
        .then(|| percent(state.live.memory_used_bytes, state.live.memory_total_bytes));
    let mut live_meters = vec![
        DashboardMeter::new("CPU USAGE", state.live.cpu_percent)
            .detail(cpu_sensor_row(&state.live)),
        DashboardMeter::new("RAM USAGE", memory_percent).detail(format!(
            "RAM used {} / {} total | available {}",
            format_bytes(state.live.memory_used_bytes),
            format_bytes(state.live.memory_total_bytes),
            format_bytes(
                state
                    .live
                    .memory_total_bytes
                    .saturating_sub(state.live.memory_used_bytes)
            )
        )),
    ];
    live_meters.extend(visible.iter().map(|(index, device)| {
        let speed = device.current_read + device.current_write;
        let peak = device.peak_total.max(speed);
        let speed_percent = (peak > 0.0).then(|| 100.0 * speed / peak);
        DashboardMeter::new(
            format!("SSD {index} {}", device.descriptor.capacity),
            speed_percent,
        )
        .suffix(format!(
            "R {} W {}",
            format_rate(device.current_read),
            format_rate(device.current_write)
        ))
    }));

    let active = visible.iter().find(|(_, device)| device.verdict.is_none());
    let active = active.map_or_else(
        || "No active worker on this page".to_owned(),
        |(index, device)| {
            format!(
                "CPU-NVME-EXACT / SSD {index} {} / {} / {}",
                device.deep_stage,
                phase_name(device.phase),
                device.descriptor.detail
            )
        },
    );
    let seed = state
        .session_seed
        .map_or_else(|| "N/A".to_owned(), |seed| format!("0x{seed:016x}"));
    let mut running_rows = vec![
        format!(
            "MATRIX CHECKS COMPLETED  {} | I/O COMPLETED {} | COMPUTE JOBS {}",
            summary.matrix_checks, summary.completed, summary.compute_jobs
        ),
        format!(
            "SESSION SEED {seed} | CPU WORKERS {} | ROUNDS {} | WRITE OPS {} | READ-SOAK {}",
            summary.compute_workers,
            summary.matrix_rounds,
            summary.write_operations,
            summary.read_soak_operations
        ),
    ];
    running_rows.extend(visible.iter().map(|(index, device)| {
        format!(
            "SSD {index} [{}] {:3.0}% I/O {}/{} MATRIX {} SOAK {} FAIL {} {}",
            progress_bar(percent(device.completed, device.target), 12),
            percent(device.completed, device.target),
            device.completed,
            device.target,
            device.matrix_checks,
            device.read_soak_operations,
            device.failures,
            verdict_or_running(device.verdict)
        )
    }));
    let page_note = if layout.mode == LayoutMode::Paged {
        let visible_count = calculate_layout(size, state.order.len(), 0)
            .visible_devices
            .max(1);
        format!(
            "PAGE {}/{} (auto 5s) | ",
            layout.first_device / visible_count + 1,
            state.order.len().div_ceil(visible_count)
        )
    } else {
        String::new()
    };
    // Same clock as every other test: hh:mm:ss.
    let clock = |seconds: u64| {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    };
    let target = state
        .duration_limit
        .map_or_else(|| "N/A".to_owned(), |limit| clock(limit.as_secs()));
    let dashboard = UnifiedDashboard {
        engine: format!("{}-{}", state.title, crate::DIT_VERSION),
        report_abi: dit_contracts::SCHEMA_VERSION.to_string(),
        profile: "CPU + SSD / NVMe DEEP".to_owned(),
        hardware_rows,
        live_meters,
        state: summary.state.to_owned(),
        progress_percent: Some(progress),
        elapsed: clock(state.started.elapsed().as_secs()),
        target,
        active_test: active,
        process: "4K matrix -> direct write -> readback -> independent exact recompute".to_owned(),
        calculations_completed: summary.matrix_checks,
        failures: summary.failures,
        running_rows: {
            running_rows.extend(state.throttle.row());
            running_rows
        },
        footer: state.throttle.alert().map_or_else(
            || format!(
                "{page_note}TEMP LIMIT OFF | SPEED=R+W/observed peak | main progress = duration | Ctrl+C preserves report"
            ),
            str::to_owned,
        ),
    };
    render_unified_dashboard(&dashboard, size, color)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn progress_bar(value: f64, width: usize) -> String {
    let filled = (value.clamp(0.0, 100.0) * width as f64 / 100.0).round() as usize;
    format!("{}{}", "#".repeat(filled), "-".repeat(width - filled))
}
