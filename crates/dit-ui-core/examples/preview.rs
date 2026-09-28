//! Synthetic UI preview only: no inventory, preflight, filesystem or SSD I/O.
use dit_contracts::{DeviceId, Event, EventEnvelope, Phase, RunId};
use dit_ui_core::{
    DeviceDescriptor, InPlaceRenderer, LiveUtilization, SystemInfo, TerminalSize, UiState,
};
use std::io::{self, IsTerminal};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let count = args
        .first()
        .map_or(Ok(2), |value| value.parse::<usize>())?
        .clamp(1, 64);
    let frames = args
        .get(1)
        .map_or(Ok(1), |value| value.parse::<u64>())?
        .clamp(1, 480);
    let current = TerminalSize::current();
    let size = TerminalSize {
        cols: args
            .get(2)
            .map_or(Ok(current.cols), |value| value.parse())?,
        rows: args
            .get(3)
            .map_or(Ok(current.rows), |value| value.parse())?,
    };
    let descriptors = (0..count)
        .map(|index| DeviceDescriptor {
            id: DeviceId(format!("demo:{index}")),
            label: format!("/dev/nvme{index}n1"),
            model: format!("DEMO SSD {}", index + 1),
            capacity: "953.87 GiB total".into(),
            detail: "DEMO / driver nvme / PCIe 4.0 x4 / test 4 GiB / write max 2 GiB".into(),
        })
        .collect();
    let mut state = UiState::new("DIT CPU-NVMe / UI PREVIEW ONLY", descriptors);
    state.set_system_info(SystemInfo {
        cpu: "Demo CPU (no hardware test running)".into(),
        memory_total_bytes: 32 << 30,
        gpu: "Demo GPU [0000:01:00.0; amdgpu; VRAM 8.00 GiB]".into(),
        board: "Demo Board".into(),
        logical_threads: 32,
        kernel: "Linux / synthetic preview".into(),
    });
    state.set_live_utilization(LiveUtilization {
        cpu_percent: Some(65.0),
        cpu_frequency_ghz: Some(4.85),
        cpu_temperature_celsius: Some(71.5),
        cpu_power_watts: Some(122.4),
        memory_used_bytes: 4 << 30,
        memory_total_bytes: 32 << 30,
    });
    state.set_run_context(Duration::from_mins(30), 0x243f_6a88_85a3_08d3);
    let mut renderer = InPlaceRenderer::new(io::stdout(), 8);
    for frame in 0..frames {
        for index in 0..count {
            let index = u64::try_from(index)?;
            let events = [
                Event::Phase {
                    phase: Phase::Verifying,
                    operation: 40 + frame,
                },
                Event::Progress {
                    completed_operations: (40 + frame + index * 5) % 100,
                    target_operations: 100,
                    bytes_written: 1 << 30,
                    bytes_verified: 1 << 30,
                    failures: 0,
                },
                Event::Throughput {
                    current_read_bytes_per_sec: f64::from(
                        u32::try_from((frame + index * 3) % 10)? * 30 + 300,
                    ) * 1_048_576.0,
                    current_write_bytes_per_sec: 100.0 * 1_048_576.0,
                    average_read_bytes_per_sec: 300.0 * 1_048_576.0,
                    average_write_bytes_per_sec: 100.0 * 1_048_576.0,
                    peak_read_bytes_per_sec: 600.0 * 1_048_576.0,
                    peak_write_bytes_per_sec: 200.0 * 1_048_576.0,
                },
            ];
            for event in events {
                state.ingest(&EventEnvelope::new(
                    RunId("preview".into()),
                    Some(DeviceId(format!("demo:{index}"))),
                    frame,
                    frame * 125,
                    frame,
                    event,
                ));
            }
        }
        let output = state.render(size, io::stdout().is_terminal());
        if io::stdout().is_terminal() {
            renderer.render_frame(&output)?;
        } else {
            println!("{output}");
        }
        if frame + 1 < frames {
            std::thread::sleep(Duration::from_millis(125));
        }
    }
    renderer.finish()?;
    Ok(())
}
