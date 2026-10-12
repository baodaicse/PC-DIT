//! Fault ledger of a single-module run (GPU-VRAM, CPU-SSD or CPU-RAM from the menu): the report
//! counted on component cells, two lines for the completion screen and one `fail-summary.csv`
//! row on the results volume, like a combined session.
use dit_orchestrator::fault_ledger::{Component, CsvMeta, Ledger, append_csv_row, csv_row};
use dit_orchestrator::fault_signatures;
use dit_orchestrator::fault_sources;
use dit_orchestrator::session::Claim;
use dit_ui_core::HostSampler;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SingleModule {
    Gpu,
    Ssd,
    /// CPU-RAM Safe / Full / Compute, run by the CPU-RAM UI (`dit-suite ledger`).
    CpuRam,
}

impl SingleModule {
    /// Its run directory's name, and its layer's name in a session.
    pub const fn directory(self) -> &'static str {
        match self {
            Self::Gpu => "gpu-vram",
            Self::Ssd => "cpu-nvme",
            Self::CpuRam => "cpu-ram",
        }
    }

    /// The report's place in the results volume, as the CSV row names it.
    fn report_path(self, run_id: &str) -> String {
        match self {
            Self::CpuRam => format!("runs/{run_id}/system-report.json"),
            _ => format!("runs/{run_id}/{}/report.json", self.directory()),
        }
    }

    /// Test column of `fail-summary.csv`, marked when the Fault Attribution entry was booted.
    fn test_label(self, attribution: bool) -> String {
        format!(
            "{}{}",
            self.test(),
            super::combined_run::attribution_suffix(attribution)
        )
    }

    const fn test(self) -> &'static str {
        match self {
            Self::Gpu => "GPU-VRAM",
            Self::Ssd => "CPU-SSD",
            Self::CpuRam => "CPU-RAM",
        }
    }
}

fn names(values: impl Iterator<Item = String>) -> String {
    values
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join("; ")
}

/// What the module's report says: its failures and hardware events, the machine's cells for
/// the FAIL BY PART line, and the GPU and SSD names for the CSV row.
fn module_findings(
    module: SingleModule,
    report: &Value,
) -> (fault_sources::Findings, Vec<Component>, String, String) {
    let str_at = |value: &Value, pointer: &str| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    match module {
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
                fault_sources::gpu_by_order(report),
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
                fault_sources::ssd(report, "", &|_| None),
                machine,
                String::new(),
                ssds,
            )
        }
        SingleModule::CpuRam => (
            fault_sources::cpu_ram(report, ""),
            vec![Component::Cpu, Component::Ram, Component::Platform],
            String::new(),
            String::new(),
        ),
    }
}

/// The CSV row's description of a single-module run on this machine; time, verdict, report and
/// the GPU and SSD names are set when the row is written.
#[must_use]
pub fn csv_meta(module: SingleModule, attribution: bool) -> CsvMeta {
    let system = HostSampler::detect().system_info();
    CsvMeta {
        image_id: super::image_id(),
        test: module.test_label(attribution),
        board: system.board,
        cpu: system.cpu,
        memory: format!("{} MiB", system.memory_total_bytes / (1024 * 1024)),
        ..CsvMeta::default()
    }
}

/// Cells a failure of `module` is counted on before its report places it (the session status).
#[must_use]
pub const fn pending_cells(module: SingleModule) -> &'static [&'static str] {
    match module {
        SingleModule::Gpu => &["PCIe-GPU1", "GPU1", "VRAM1"],
        SingleModule::Ssd => &["SSD1", "SSD1-path"],
        SingleModule::CpuRam => &["CPU", "RAM"],
    }
}

fn csv_path(results_root: &Path, run_dir: &Path) -> std::path::PathBuf {
    if results_root.is_dir() {
        results_root.join("fail-summary.csv")
    } else {
        run_dir.join("fail-summary.csv")
    }
}

/// The CSV row of a run that ended without a report (stopped, crashed, killed), so a session
/// still has its row. Its watchdog's final record (`watchdog-final.json`) says how it ended
/// (FC5): a crash signal or a stall with kernel I/O errors counts a failure and makes it a FAIL;
/// anything else leaves `verdict` and marks the cause. `claim` says whether this run writes the
/// row; `cause` marks why it was stopped (`operator_rescue`). Returns the verdict and the lines
/// for the screen.
#[allow(clippy::too_many_arguments)]
pub fn row_without_report(
    module: SingleModule,
    results_root: &Path,
    run_dir: &Path,
    verdict: &'static str,
    attribution: bool,
    claim: &Claim,
    cause: Option<&str>,
    record: Option<&Value>,
) -> (&'static str, String) {
    let ssd_cells: &[Component] = match module {
        SingleModule::Ssd => &[Component::Ssd(1), Component::SsdPath(1)],
        SingleModule::Gpu | SingleModule::CpuRam => &[],
    };
    let mut ledger = Ledger::default();
    let mut invalid = None;
    // Stopped by the operator: nothing to classify.
    if cause.is_none()
        && record.is_some_and(|r| {
            r["detail"]
                .as_str()
                .is_none_or(|d| !d.starts_with("operator requested stop"))
        })
    {
        match fault_sources::layer_ending(module.directory(), "", record, ssd_cells) {
            fault_sources::Ending::Fail(event) => ledger.add(*event),
            fault_sources::Ending::Invalid { code, text } => invalid = Some((code, text)),
        }
    }
    let verdict = if ledger.failures_total() > 0 {
        "FAIL"
    } else {
        verdict
    };
    let mut lines = ledger.cause_lines();
    if let Some((_, text)) = &invalid {
        lines.push(format!("INVALID       {text} (test infrastructure)"));
    }
    let csv = if let Claim::Taken(by) = claim {
        format!("Fail summary not written: the {by} already wrote this session's result")
    } else {
        let meta = CsvMeta {
            time: super::local_timestamp(),
            verdict: verdict.to_owned(),
            ..csv_meta(module, attribution)
        };
        let path = csv_path(results_root, run_dir);
        let mut counts = ledger.csv_counts();
        for code in invalid
            .as_ref()
            .map(|(code, _)| *code)
            .into_iter()
            .chain(cause)
        {
            counts.add_cause(code);
        }
        match append_csv_row(&path, &csv_row(&meta, &counts)) {
            Ok(()) => format!("Fail summary row added to {}", path.display()),
            Err(error) => format!("Fail summary not written ({error}): {}", path.display()),
        }
    };
    lines.push(csv);
    (verdict, lines.join("\n"))
}

/// Counts the report's failures on their cells and appends the CSV row when `claim` says this
/// run writes the session's result. The returned text goes under the module's completion
/// message; an unreadable report gives an empty string.
#[allow(clippy::too_many_arguments)]
pub fn summarize(
    module: SingleModule,
    report_path: &Path,
    results_root: &Path,
    run_dir: &Path,
    run_id: &str,
    verdict: &str,
    attribution: bool,
    claim: &Claim,
    cause: Option<&str>,
) -> String {
    let Some(report) = std::fs::read(report_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return String::new();
    };
    let (findings, machine, gpus, ssds) = module_findings(module, &report);
    let mut ledger = Ledger::default();
    ledger.add_all(findings.failures);
    for event in &findings.hardware {
        ledger.add_hardware(event);
    }
    for (cell, reading) in &findings.throttle {
        ledger.add_throttle(*cell, *reading);
    }
    // Step F6 on what the report gives: addresses, bits and hardware counts (no timing).
    fault_signatures::rank(&mut ledger, &fault_signatures::Context::default());
    let meta = CsvMeta {
        time: super::local_timestamp(),
        gpus,
        ssds,
        verdict: verdict.to_owned(),
        report: module.report_path(run_id),
        ..csv_meta(module, attribution)
    };
    let csv_path = csv_path(results_root, run_dir);
    let csv = match claim {
        Claim::Taken(by) => {
            format!("Fail summary not written: the {by} already wrote this session's result")
        }
        Claim::Won => match append_csv_row(&csv_path, &{
            let mut counts = ledger.csv_counts();
            if let Some(code) = cause {
                counts.add_cause(code);
            }
            csv_row(&meta, &counts)
        }) {
            Ok(()) => format!("Fail summary row added to {}", csv_path.display()),
            Err(error) => format!("Fail summary not written ({error}): {}", csv_path.display()),
        },
    };
    format!(
        "{}\n{}\n{}\n{csv}",
        ledger.fail_line(&machine, 0),
        ledger.hardware_line(),
        ledger.throttle_line()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_without_a_report_is_a_fail_only_with_hardware_evidence() {
        let dir = std::env::temp_dir().join(format!("dit-single-ending-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let none = dir.join("no-results-volume");
        let crash = serde_json::json!({"state": "WORKER_SIGNALED", "exit_signal": 11,
            "detail": "worker ended from signal 11"});
        let (verdict, lines) = row_without_report(
            SingleModule::Gpu,
            &none,
            &dir,
            "INVALID",
            false,
            &Claim::Won,
            None,
            Some(&crash),
        );
        assert_eq!(verdict, "FAIL");
        assert!(
            lines.starts_with("PLATFORM      gpu-vram crashed"),
            "{lines}"
        );
        let stall = serde_json::json!({"state": "STALLED_KILLED", "detail": "nothing moved",
            "liveness": {"kernel_io": []}});
        let (verdict, lines) = row_without_report(
            SingleModule::Ssd,
            &none,
            &dir,
            "INVALID",
            false,
            &Claim::Won,
            None,
            Some(&stall),
        );
        assert_eq!(verdict, "INVALID");
        assert!(
            lines.starts_with("INVALID       cpu-nvme stopped making progress"),
            "{lines}"
        );
        let csv = std::fs::read_to_string(dir.join("fail-summary.csv")).unwrap();
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert!(
            rows[0].contains(",FAIL,") && rows[0].contains("PLATFORM=1(c1/s0)[worker_crash]"),
            "{}",
            rows[0]
        );
        assert!(
            rows[1].contains(",INVALID,") && rows[1].contains("INVALID[layer_stall]"),
            "{}",
            rows[1]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

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
            true,
            &Claim::Won,
            None,
        );
        // The rules leave retention failures on `PCIe-GPU1|VRAM1`; the five failures sit on two
        // byte offsets only (0 and 5701632), so the ranking (F6) moves them to VRAM1 as suspected.
        assert!(
            text.starts_with("FAIL BY PART  PCIe-GPU1 0 | GPU1 0 | VRAM1 5 (5 susp) | PLATFORM 0"),
            "{text}"
        );
        let csv = std::fs::read_to_string(dir.join("fail-summary.csv")).unwrap();
        assert_eq!(csv.lines().count(), 2);
        assert!(
            csv.lines()
                .nth(1)
                .unwrap()
                .contains("GPU-VRAM +ATTRIBUTION"),
            "a Fault Attribution run is marked in the CSV"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
