//! Versioned module registry, report adapters and resource arbitration for the DIT suite.

pub mod combined;

use dit_contracts::{SCHEMA_VERSION, Verdict};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const CPU_RAM_MODULE_ID: &str = "dit.cpu-ram";
pub const CPU_NVME_MODULE_ID: &str = "dit.cpu-nvme";
pub const VGA_MODULE_ID: &str = "dit.vga";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModuleKind {
    CpuRam,
    CpuNvme,
    Vga,
    CpuPower,
    Combined,
}

impl ModuleKind {
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::CpuRam => CPU_RAM_MODULE_ID,
            Self::CpuNvme => CPU_NVME_MODULE_ID,
            Self::Vga => VGA_MODULE_ID,
            Self::CpuPower => "dit.cpu-power-platform",
            Self::Combined => combined::COMBINED_MODULE_ID,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CpuRam => "CPU + RAM",
            Self::CpuNvme => "CPU + SSD",
            Self::Vga => "GPU + VRAM",
            Self::CpuPower => "CPU AVAIL / POWER",
            Self::Combined => "COMBINED STRESS",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleRegistration {
    pub module: ModuleKind,
    pub module_id: String,
    pub label: String,
    pub launcher: PathBuf,
    pub available: bool,
    pub schema_version: u32,
}

impl ModuleRegistration {
    #[must_use]
    pub fn detect(module: ModuleKind, launcher: PathBuf) -> Self {
        let available = launcher.is_file();
        Self {
            module,
            module_id: module.id().to_owned(),
            label: module.label().to_owned(),
            launcher,
            available,
            schema_version: SCHEMA_VERSION,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionPlan {
    CpuRamOnly,
    CpuNvmeOnly,
    /// Retained for reading/replaying older suite plans.
    CpuNvmeWithPreflight,
    CombinedConcurrent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceBudget {
    pub schema_version: u32,
    pub plan: ExecutionPlan,
    pub total_threads: usize,
    pub available_memory_bytes: u64,
    pub reserved_memory_bytes: u64,
    pub cpu_ram_threads: usize,
    pub nvme_threads: usize,
    pub cpu_ram_memory_bytes: u64,
    pub nvme_memory_bytes: u64,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OrchestratorError {
    #[error("at least one host thread is required")]
    NoThreads,
    #[error("available memory must exceed the suite reserve")]
    InsufficientMemory,
    #[error("invalid CPU-RAM report: {0}")]
    InvalidCpuRamReport(String),
    #[error("invalid CPU-NVMe report: {0}")]
    InvalidNvmeReport(String),
}

/// Produces a hard resource split for future concurrent composition.
///
/// CPU-SSD may run independently. Resource splitting also keeps a future combined runner from
/// letting modules independently claim all CPU/RAM.
///
/// # Errors
///
/// Returns an error when no CPU thread is available or memory remaining after the suite reserve
/// is too small for a valid test plan.
pub fn allocate_resources(
    plan: ExecutionPlan,
    total_threads: usize,
    available_memory_bytes: u64,
) -> Result<ResourceBudget, OrchestratorError> {
    if total_threads == 0 {
        return Err(OrchestratorError::NoThreads);
    }
    let reserve = (512_u64 * 1024 * 1024).min(available_memory_bytes / 4);
    let allocatable = available_memory_bytes.saturating_sub(reserve);
    if allocatable < 64 * 1024 * 1024 {
        return Err(OrchestratorError::InsufficientMemory);
    }
    let (cpu_ram_threads, nvme_threads, cpu_ram_memory_bytes, nvme_memory_bytes) = match plan {
        ExecutionPlan::CpuRamOnly => (total_threads, 0, allocatable, 0),
        ExecutionPlan::CpuNvmeOnly => (0, total_threads, 0, allocatable),
        ExecutionPlan::CpuNvmeWithPreflight => {
            // Sequential phases may reuse the same host threads. NVMe keeps a bounded 1/8 buffer.
            let nvme_memory = (allocatable / 8).max(32 * 1024 * 1024);
            (
                total_threads,
                total_threads,
                allocatable.saturating_sub(nvme_memory),
                nvme_memory,
            )
        }
        ExecutionPlan::CombinedConcurrent => {
            let nvme_threads = (total_threads / 4).max(1);
            let cpu_ram_threads = total_threads.saturating_sub(nvme_threads).max(1);
            let nvme_memory = allocatable / 5;
            (
                cpu_ram_threads,
                nvme_threads,
                allocatable.saturating_sub(nvme_memory),
                nvme_memory,
            )
        }
    };
    Ok(ResourceBudget {
        schema_version: SCHEMA_VERSION,
        plan,
        total_threads,
        available_memory_bytes,
        reserved_memory_bytes: reserve,
        cpu_ram_threads,
        nvme_threads,
        cpu_ram_memory_bytes,
        nvme_memory_bytes,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleResult {
    pub module_id: String,
    pub verdict: Verdict,
    pub source_schema_version: u32,
    pub source_run_id: String,
    pub evidence_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuiteReport {
    pub schema_version: u32,
    pub suite_run_id: String,
    pub aggregate_verdict: Verdict,
    pub modules: Vec<ModuleResult>,
    pub resource_budget: ResourceBudget,
}

impl SuiteReport {
    #[must_use]
    pub fn new(
        suite_run_id: String,
        modules: Vec<ModuleResult>,
        resource_budget: ResourceBudget,
    ) -> Self {
        let aggregate_verdict = aggregate_module_verdicts(&modules);
        Self {
            schema_version: SCHEMA_VERSION,
            suite_run_id,
            aggregate_verdict,
            modules,
            resource_budget,
        }
    }
}

#[derive(Deserialize)]
struct CpuRamReportView {
    schema_version: u32,
    #[serde(default)]
    module_id: Option<String>,
    #[serde(default)]
    test_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    verdict: Option<Verdict>,
}

#[derive(Deserialize)]
struct NvmeReportView {
    #[serde(default)]
    schema_version: Option<u32>,
    #[serde(default)]
    run_id: Option<dit_contracts::RunId>,
    #[serde(default)]
    aggregate_verdict: Option<Verdict>,
    #[serde(default)]
    scheduler: Option<NvmeSchedulerView>,
}

#[derive(Deserialize)]
struct NvmeSchedulerView {
    schema_version: u32,
    run_id: dit_contracts::RunId,
    aggregate_verdict: Verdict,
}

/// Converts a native CPU-RAM system report to the shared module result.
///
/// # Errors
///
/// Returns an error when JSON/schema fields or the native status are not recognized.
pub fn adapt_cpu_ram_report(path: &Path, bytes: &[u8]) -> Result<ModuleResult, OrchestratorError> {
    let report: CpuRamReportView = serde_json::from_slice(bytes)
        .map_err(|error| OrchestratorError::InvalidCpuRamReport(error.to_string()))?;
    if report
        .module_id
        .as_deref()
        .is_some_and(|module_id| module_id != CPU_RAM_MODULE_ID)
    {
        return Err(OrchestratorError::InvalidCpuRamReport(
            "module_id is not dit.cpu-ram".to_owned(),
        ));
    }
    let verdict = if let Some(verdict) = report.verdict {
        verdict
    } else {
        match report
            .status
            .as_deref()
            .unwrap_or_default()
            .to_ascii_uppercase()
            .as_str()
        {
            "PASS" => Verdict::Pass,
            "FAIL" => Verdict::Fail,
            "INVALID" => Verdict::Invalid,
            other => {
                return Err(OrchestratorError::InvalidCpuRamReport(format!(
                    "unsupported status {other}"
                )));
            }
        }
    };
    let source_run_id = report
        .run_id
        .or(report.test_id)
        .ok_or_else(|| OrchestratorError::InvalidCpuRamReport("missing run ID".to_owned()))?;
    Ok(ModuleResult {
        module_id: CPU_RAM_MODULE_ID.to_owned(),
        verdict,
        source_schema_version: report.schema_version,
        source_run_id,
        evidence_path: path.to_path_buf(),
    })
}

/// Converts a native multi-device or preserving-file `NVMe` report to a shared module result.
///
/// # Errors
///
/// Returns an error when JSON or required scheduler/result fields are missing.
pub fn adapt_nvme_report(path: &Path, bytes: &[u8]) -> Result<ModuleResult, OrchestratorError> {
    let report: NvmeReportView = serde_json::from_slice(bytes)
        .map_err(|error| OrchestratorError::InvalidNvmeReport(error.to_string()))?;
    let (schema_version, run_id, verdict) = if let Some(scheduler) = report.scheduler {
        (
            scheduler.schema_version,
            scheduler.run_id.0,
            scheduler.aggregate_verdict,
        )
    } else {
        (
            report.schema_version.ok_or_else(|| {
                OrchestratorError::InvalidNvmeReport("missing schema_version".to_owned())
            })?,
            report
                .run_id
                .ok_or_else(|| OrchestratorError::InvalidNvmeReport("missing run_id".to_owned()))?
                .0,
            report.aggregate_verdict.ok_or_else(|| {
                OrchestratorError::InvalidNvmeReport("missing aggregate_verdict".to_owned())
            })?,
        )
    };
    Ok(ModuleResult {
        module_id: CPU_NVME_MODULE_ID.to_owned(),
        verdict,
        source_schema_version: schema_version,
        source_run_id: run_id,
        evidence_path: path.to_path_buf(),
    })
}

#[must_use]
pub fn aggregate_module_verdicts(results: &[ModuleResult]) -> Verdict {
    if results.is_empty()
        || results
            .iter()
            .any(|result| result.verdict == Verdict::Invalid)
    {
        Verdict::Invalid
    } else if results.iter().any(|result| result.verdict == Verdict::Fail) {
        Verdict::Fail
    } else if results
        .iter()
        .any(|result| result.verdict == Verdict::Aborted)
    {
        Verdict::Aborted
    } else if results.iter().all(|result| result.verdict == Verdict::Pass) {
        Verdict::Pass
    } else {
        Verdict::NotRun
    }
}

#[must_use]
pub fn render_module_menu(registry: &[ModuleRegistration], width: usize) -> String {
    let width = width.clamp(60, 180);
    let inner = width - 2;
    let mut lines = vec![
        format!("+{}+", "-".repeat(inner)),
        menu_line("DIT CONTROL CENTER // SELECT TEST MODULE", inner),
        menu_line("CPU-RAM, CPU-SSD and GPU-VRAM share one boot image", inner),
        format!("+{}+", "-".repeat(inner)),
    ];
    for (index, registration) in registry.iter().enumerate() {
        let state = if registration.available {
            "READY"
        } else {
            "NOT INSTALLED"
        };
        lines.push(menu_line(
            &format!(
                " [{}] {:<20} {:<13} {}",
                index + 1,
                registration.label,
                state,
                registration.module_id
            ),
            inner,
        ));
    }
    lines.push(menu_line(" [I] NVMe inventory (read-only)", inner));
    lines.push(menu_line(" [D] Diagnostic shell", inner));
    lines.push(menu_line(" [R] Reboot          [P] Power off", inner));
    lines.push(format!("+{}+", "-".repeat(inner)));
    lines.push(menu_line(
        "CPU-SSD: selecting SSD starts safe file-mode Deep test; RAM preflight skipped",
        inner,
    ));
    lines.push(menu_line(
        "COMBINED: baselines, then every layer at once + SSD->RAM->GPU->CPU data chain",
        inner,
    ));
    lines.push(format!("+{}+", "-".repeat(inner)));
    lines.join("\n")
}

fn menu_line(content: &str, inner: usize) -> String {
    let visible = content
        .chars()
        .take(inner.saturating_sub(2))
        .collect::<String>();
    format!("| {visible:<width$} |", width = inner.saturating_sub(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(module: &str, verdict: Verdict) -> ModuleResult {
        ModuleResult {
            module_id: module.to_owned(),
            verdict,
            source_schema_version: 1,
            source_run_id: "run".to_owned(),
            evidence_path: PathBuf::from("report.json"),
        }
    }

    #[test]
    fn aggregate_preserves_module_attribution() {
        let reports = vec![
            result(CPU_RAM_MODULE_ID, Verdict::Pass),
            result(CPU_NVME_MODULE_ID, Verdict::Fail),
        ];
        assert_eq!(aggregate_module_verdicts(&reports), Verdict::Fail);
        assert_eq!(reports[0].verdict, Verdict::Pass);
        assert_eq!(reports[1].verdict, Verdict::Fail);
    }

    #[test]
    fn invalid_has_precedence_over_hardware_failure() {
        let reports = vec![
            result(CPU_RAM_MODULE_ID, Verdict::Invalid),
            result(CPU_NVME_MODULE_ID, Verdict::Fail),
        ];
        assert_eq!(aggregate_module_verdicts(&reports), Verdict::Invalid);
    }

    #[test]
    fn resource_split_never_exceeds_available_memory_or_threads() {
        let budget = allocate_resources(
            ExecutionPlan::CombinedConcurrent,
            16,
            32 * 1024 * 1024 * 1024,
        )
        .unwrap();
        assert!(budget.cpu_ram_threads + budget.nvme_threads <= budget.total_threads);
        assert!(
            budget.cpu_ram_memory_bytes + budget.nvme_memory_bytes + budget.reserved_memory_bytes
                <= budget.available_memory_bytes
        );
    }

    #[test]
    fn cpu_ssd_only_assigns_no_ram_test_resources() {
        let budget =
            allocate_resources(ExecutionPlan::CpuNvmeOnly, 16, 32 * 1024 * 1024 * 1024).unwrap();
        assert_eq!(budget.cpu_ram_threads, 0);
        assert_eq!(budget.cpu_ram_memory_bytes, 0);
        assert_eq!(budget.nvme_threads, 16);
        assert!(budget.nvme_memory_bytes > 0);
    }

    #[test]
    fn adapters_accept_native_cpu_and_wrapped_nvme_reports() {
        let cpu = adapt_cpu_ram_report(
            Path::new("cpu.json"),
            br#"{"schema_version":9,"test_id":"CPU-DIT-01-SYSTEM-v1","status":"PASS"}"#,
        )
        .unwrap();
        assert_eq!(cpu.verdict, Verdict::Pass);
        let preflight = adapt_cpu_ram_report(
            Path::new("preflight.json"),
            br#"{"schema_version":1,"module_id":"dit.cpu-ram","run_id":"cpu-run","verdict":"PASS"}"#,
        )
        .unwrap();
        assert_eq!(preflight.source_run_id, "cpu-run");
        let nvme = adapt_nvme_report(
            Path::new("nvme.json"),
            br#"{"schema_version":1,"mode":"DATA_PRESERVING_FILE","scheduler":{"schema_version":1,"run_id":"nvme-run","aggregate_verdict":"PASS"}}"#,
        )
        .unwrap();
        assert_eq!(nvme.verdict, Verdict::Pass);
        assert_eq!(nvme.source_run_id, "nvme-run");
    }

    #[test]
    fn menu_is_fixed_width_and_marks_missing_modules() {
        let registry = vec![
            ModuleRegistration {
                module: ModuleKind::CpuRam,
                module_id: CPU_RAM_MODULE_ID.to_owned(),
                label: ModuleKind::CpuRam.label().to_owned(),
                launcher: PathBuf::from("/cpu"),
                available: true,
                schema_version: 1,
            },
            ModuleRegistration {
                module: ModuleKind::CpuNvme,
                module_id: CPU_NVME_MODULE_ID.to_owned(),
                label: ModuleKind::CpuNvme.label().to_owned(),
                launcher: PathBuf::from("/nvme"),
                available: false,
                schema_version: 1,
            },
        ];
        let frame = render_module_menu(&registry, 100);
        assert!(frame.contains("CPU + RAM"));
        assert!(frame.contains("CPU + SSD"));
        assert!(frame.contains("NOT INSTALLED"));
        assert!(frame.lines().all(|line| line.chars().count() == 100));
    }
}
