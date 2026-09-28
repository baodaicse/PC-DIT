//! Fault ledger of a single-module run (GPU-VRAM or CPU-SSD from the menu): the module's report
//! counted on component cells, two lines for the completion screen and one `fail-summary.csv`
//! row on the results volume, like a combined session.
use dit_orchestrator::fault_ledger::{Component, CsvMeta, Ledger, append_csv};
use dit_orchestrator::fault_sources;
use dit_ui_core::HostSampler;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SingleModule {
    Gpu,
    Ssd,
}

impl SingleModule {
    const fn directory(self) -> &'static str {
        match self {
            Self::Gpu => "gpu-vram",
            Self::Ssd => "cpu-nvme",
        }
    }

    const fn test(self) -> &'static str {
        match self {
            Self::Gpu => "GPU-VRAM",
            Self::Ssd => "CPU-SSD",
        }
    }
}

fn names(values: impl Iterator<Item = String>) -> String {
    values
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Counts the report's failures on their cells and appends the CSV row. The returned text goes
/// under the module's completion message; an unreadable report gives an empty string.
pub fn summarize(
    module: SingleModule,
    report_path: &Path,
    results_root: &Path,
    run_dir: &Path,
    run_id: &str,
    verdict: &str,
) -> String {
    let Some(report) = std::fs::read(report_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return String::new();
    };
    let str_at = |value: &Value, pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let (findings, machine, gpus, ssds) = match module {
        SingleModule::Gpu => {
            let devices: Vec<Value> = report
                .pointer("/device_reports")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| vec![report.clone()]);
            let count = u8::try_from(devices.len()).unwrap_or(u8::MAX);
            let mut machine: Vec<Component> = (1..=count)
                .flat_map(|n| [Component::PcieGpu(n), Component::Gpu(n), Component::Vram(n)])
                .collect();
            machine.push(Component::Platform);
            let gpus = names(devices.iter().map(|d| str_at(d, "/selected_device/name")));
            (
                fault_sources::gpu_by_order(&report),
                machine,
                gpus,
                String::new(),
            )
        }
        SingleModule::Ssd => {
            let aliases: Vec<u8> = report
                .pointer("/targets")
                .and_then(Value::as_array)
                .map(|targets| {
                    targets
                        .iter()
                        .filter_map(|t| t.pointer("/alias").and_then(Value::as_u64))
                        .filter_map(|a| u8::try_from(a).ok())
                        .collect()
                })
                .unwrap_or_default();
            let mut machine: Vec<Component> = aliases
                .iter()
                .flat_map(|n| [Component::Ssd(*n), Component::SsdPath(*n)])
                .collect();
            machine.push(Component::Platform);
            let ssds = names(
                report
                    .pointer("/inventory_before")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|device| str_at(device, "/model")),
            );
            (
                fault_sources::ssd(&report, "", &|_| None),
                machine,
                String::new(),
                ssds,
            )
        }
    };
    let mut ledger = Ledger::default();
    ledger.add_all(findings.failures);
    for event in &findings.hardware {
        ledger.add_hardware(event);
    }
    let system = HostSampler::detect().system_info();
    let meta = CsvMeta {
        time: super::local_timestamp(),
        image_id: super::image_id(),
        test: module.test().to_owned(),
        board: system.board,
        cpu: system.cpu,
        memory: format!("{} MiB", system.memory_total_bytes / (1024 * 1024)),
        gpus,
        ssds,
        verdict: verdict.to_owned(),
        report: format!("runs/{run_id}/{}/report.json", module.directory()),
    };
    let csv_path = if results_root.is_dir() {
        results_root.join("fail-summary.csv")
    } else {
        run_dir.join("fail-summary.csv")
    };
    let csv = match append_csv(&csv_path, &ledger, &meta) {
        Ok(()) => format!("Fail summary row added to {}", csv_path.display()),
        Err(error) => format!("Fail summary not written ({error}): {}", csv_path.display()),
    };
    format!(
        "{}\n{}\n{csv}",
        ledger.fail_line(&machine, 0),
        ledger.hardware_line()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_gpu_report_gives_the_ledger_lines_and_a_csv_row() {
        let dir = std::env::temp_dir().join(format!("dit-single-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let report = dir.join("report.json");
        std::fs::write(&report, include_str!("testdata/gpu-retention-fail.json")).unwrap();
        let text = summarize(
            SingleModule::Gpu,
            &report,
            &dir.join("missing-results-root"),
            &dir,
            "suite-gpu-1",
            "FAIL",
        );
        assert!(
            text.starts_with(
                "FAIL BY PART  PCIe-GPU1 0 | GPU1 0 | VRAM1 0 | PLATFORM 0 | ? PCIe-GPU1|VRAM1 5"
            ),
            "{text}"
        );
        let csv = std::fs::read_to_string(dir.join("fail-summary.csv")).unwrap();
        assert_eq!(csv.lines().count(), 2);
        assert!(csv.lines().nth(1).unwrap().contains("GPU-VRAM"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
