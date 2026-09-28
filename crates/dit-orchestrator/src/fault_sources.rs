//! Reads each layer's own report and turns its failures into [`FaultEvent`]s placed on component
//! cells (step F0: no engine change, only what the reports already carry). Rules are named
//! `<layer>/<rule>` and listed in `docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md` (sections 1 and 2).
//!
//! Until the GPU pipeline has boundary checks (step F3), a GPU result that is wrong after the
//! round trip stays unresolved between its `PCIe` link, the GPU and its VRAM, instead of being
//! blamed on the GPU.
use crate::fault_ledger::{Attribution, Component, FaultEvent, HardwareEvent};
use serde_json::Value;

/// Failures and hardware counters found in one report.
#[derive(Debug, Default)]
pub struct Findings {
    pub failures: Vec<FaultEvent>,
    pub hardware: Vec<HardwareEvent>,
}

impl Findings {
    fn fail(&mut self, event: FaultEvent) {
        self.failures.push(event);
    }

    fn hw(&mut self, cell: Component, kind: &str, delta: u64) {
        if delta > 0 {
            self.hardware.push(HardwareEvent::new(cell, kind, delta));
        }
    }

    fn in_phase(mut self, phase: &str) -> Self {
        self.failures = self
            .failures
            .into_iter()
            .map(|event| event.in_phase(phase))
            .collect();
        self
    }
}

fn u64_at(value: &Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

fn bool_at(value: &Value, pointer: &str) -> Option<bool> {
    value.pointer(pointer).and_then(Value::as_bool)
}

fn str_at<'a>(value: &'a Value, pointer: &str) -> &'a str {
    value.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

// ------------------------------------------------------------------------------------ CPU-RAM

const CPU_RAM: &str = "cpu-ram";

/// `dit-cli system-run` report (`system-report.json`).
#[must_use]
pub fn cpu_ram(report: &Value, phase: &str) -> Findings {
    let mut found = Findings::default();
    let counted = cpu_checks(report, &mut found);
    ram_pattern(report, &mut found, counted);
    cpu_ram_platform(report, &mut found);
    found.in_phase(phase)
}

/// Known-answer, coherency, SIMD-versus-scalar and compute-oracle checks: all computed on the CPU.
/// Returns how many of them the engine counted in `failures_total`.
fn cpu_checks(report: &Value, found: &mut Findings) -> u64 {
    let cpu_checks = [
        (
            "/cpu/integer_known_answer_passed",
            "cpu:integer_kat",
            "cpu-ram/integer_kat",
        ),
        (
            "/cpu/sha256_known_answer_passed",
            "cpu:sha256_kat",
            "cpu-ram/sha256_kat",
        ),
        (
            "/cpu/fp_fma_known_answer_passed",
            "cpu:fp_fma_kat",
            "cpu-ram/fp_fma_kat",
        ),
        (
            "/cpu/coherency_passed",
            "cpu:coherency",
            "cpu-ram/coherency",
        ),
    ];
    let completed = bool_at(report, "/cpu/cpu_tests_completed").unwrap_or(false);
    let mut counted_in_total = 0;
    for (pointer, source, rule) in cpu_checks {
        if completed && bool_at(report, pointer) == Some(false) {
            counted_in_total += 1;
            found.fail(FaultEvent::new(
                CPU_RAM,
                source,
                1,
                Attribution::Confirmed(Component::Cpu),
                rule,
                "known-answer / coherency check computed on the CPU failed",
            ));
        }
    }
    let simd: u64 = report
        .pointer("/workers")
        .and_then(Value::as_array)
        .map_or(0, |workers| {
            workers
                .iter()
                .map(|w| u64_at(w, "/scalar_simd_mismatches"))
                .sum()
        });
    if simd > 0 {
        counted_in_total += simd;
        found.fail(FaultEvent::new(
            CPU_RAM,
            "cpu:scalar_simd",
            simd,
            Attribution::Confirmed(Component::Cpu),
            "cpu-ram/scalar_simd",
            "SIMD and scalar reductions of the same arena differ",
        ));
    }
    if bool_at(report, "/compute_oracle/enabled") == Some(true) {
        for (pointer, source) in [
            (
                "/compute_oracle/dense_fma_cross_worker_consistent",
                "cpu:oracle_fma_cross_worker",
            ),
            (
                "/compute_oracle/dense_fma_scalar_reference_passed",
                "cpu:oracle_fma_reference",
            ),
            (
                "/compute_oracle/branch_predictor_passed",
                "cpu:oracle_branch",
            ),
            (
                "/compute_oracle/aes256_known_answer_passed",
                "cpu:oracle_aes",
            ),
        ] {
            if bool_at(report, pointer) == Some(false) {
                let detail = report
                    .pointer("/compute_oracle/mismatch")
                    .filter(|m| !m.is_null())
                    .map_or_else(String::new, ToString::to_string);
                found.fail(FaultEvent::new(
                    CPU_RAM,
                    source,
                    1,
                    Attribution::Confirmed(Component::Cpu),
                    "cpu-ram/compute_oracle",
                    detail,
                ));
            }
        }
    }
    counted_in_total
}

/// RAM pattern mismatches, counted as failing verify rounds.
fn ram_pattern(report: &Value, found: &mut Findings, counted_in_total: u64) {
    // RAM pattern mismatches: the engine counts wrong bytes; an event is a failing verify round.
    let wrong_bytes = u64_at(report, "/failures_total").saturating_sub(counted_in_total);
    let mismatch = report.pointer("/mismatch").filter(|m| !m.is_null());
    if mismatch.is_some() || wrong_bytes > 0 {
        let failing_rounds = report
            .pointer("/round_digests")
            .and_then(Value::as_array)
            .map_or(0, |rounds| {
                rounds
                    .iter()
                    .filter(|r| {
                        r.pointer("/size_class_verified_bytes")
                            .and_then(Value::as_object)
                            .is_some_and(serde_json::Map::is_empty)
                    })
                    .count() as u64
            })
            .max(1);
        let first = mismatch.map_or_else(String::new, |m| {
            format!(
                "first at global offset {} (arena {} offset {}), expected {:#04x} actual {:#04x}, syndrome {:#04x}, writer CPU {}, verifier CPU {}",
                u64_at(m, "/global_offset"),
                u64_at(m, "/arena_id"),
                u64_at(m, "/arena_offset"),
                u64_at(m, "/expected"),
                u64_at(m, "/actual"),
                u64_at(m, "/xor_syndrome"),
                m.pointer("/writer_cpu").map_or_else(|| "N/A".into(), ToString::to_string),
                m.pointer("/verifier_cpu").map_or_else(|| "N/A".into(), ToString::to_string),
            )
        });
        found.fail(FaultEvent::new(
            CPU_RAM,
            "ram:pattern",
            failing_rounds,
            // A pattern written and read by the CPU: RAM is the likely cell until the
            // confirmation step (F4) re-reads and rewrites from another CPU.
            Attribution::Suspected(Component::Ram, vec![Component::Cpu]),
            "cpu-ram/pattern",
            format!(
                "{wrong_bytes} wrong byte(s) in {failing_rounds} failing round(s); {first}; rerun: {}",
                str_at(report, "/diagnostic/classification")
            ),
        ));
    }
}

/// Worker crashes, uncorrectable EDAC errors and the hardware counters of the run.
fn cpu_ram_platform(report: &Value, found: &mut Findings) {
    let crashes = report
        .pointer("/execution/worker_crashes")
        .and_then(Value::as_array)
        .map_or(0, Vec::len) as u64;
    if crashes > 0 {
        found.fail(FaultEvent::new(
            CPU_RAM,
            "platform:worker_crash",
            crashes,
            Attribution::Confirmed(Component::Platform),
            "cpu-ram/worker_crash",
            "a test worker thread panicked",
        ));
    }
    let uncorrected = u64_at(report, "/telemetry/edac_uncorrected_delta");
    if uncorrected > 0 {
        found.fail(FaultEvent::new(
            CPU_RAM,
            "ram:edac_uncorrected",
            uncorrected,
            Attribution::Confirmed(Component::Ram),
            "cpu-ram/edac_ue",
            "EDAC reported uncorrectable memory errors",
        ));
    }
    found.hw(
        Component::Ram,
        "edac_corrected",
        u64_at(report, "/telemetry/edac_corrected_delta"),
    );
    found.hw(
        Component::Cpu,
        "machine_check",
        u64_at(report, "/telemetry/kernel_events/machine_check_events"),
    );
    found.hw(
        Component::Platform,
        "throttle",
        u64_at(report, "/telemetry/throttle_core_delta")
            + u64_at(report, "/telemetry/throttle_package_delta"),
    );
    found.hw(
        Component::Platform,
        "oom_kill",
        u64_at(report, "/telemetry/cgroup_oom_kill_delta"),
    );
}

// ---------------------------------------------------------------------------------- GPU-VRAM

/// Cell of a GPU failure record by the detector (`phase`) that raised it.
#[must_use]
pub fn gpu_phase(phase: &str, gpu: u8) -> (Attribution, &'static str) {
    let link_vram = || Attribution::Unresolved(vec![Component::PcieGpu(gpu), Component::Vram(gpu)]);
    match phase {
        // The GEMM result is wrong after upload, compute and read-back: no boundary check yet.
        "compute" => (
            Attribution::Unresolved(vec![
                Component::PcieGpu(gpu),
                Component::Gpu(gpu),
                Component::Vram(gpu),
            ]),
            "gpu/compute_round_trip",
        ),
        // Written, verified and re-read entirely on the GPU: nothing crossed the link.
        "vram-sweep" | "vram-self-check" => (
            Attribution::Confirmed(Component::Vram(gpu)),
            "gpu/device_local_vram",
        ),
        // Data that went over the link and was held in VRAM, checked by the CPU.
        "transfer"
        | "transfer-lane"
        | "retention"
        | "retention-before-overwrite"
        | "retention-final"
        | "vram-checkpoint"
        | "vram-sweep-checkpoint" => (link_vram(), "gpu/transfer_round_trip"),
        "thermal_policy" => (
            Attribution::Confirmed(Component::Platform),
            "gpu/thermal_policy",
        ),
        _ => (
            Attribution::Unresolved(vec![
                Component::PcieGpu(gpu),
                Component::Gpu(gpu),
                Component::Vram(gpu),
            ]),
            "gpu/unknown_detector",
        ),
    }
}

/// Cell of a hardware-health reason (kernel log): NVIDIA Xid codes, `PCIe` AER, device lost.
#[must_use]
pub fn gpu_hardware_reason(reason: &str, gpu: u8) -> Attribution {
    let lower = reason.to_ascii_lowercase();
    if let Some(code) = lower
        .split("xid")
        .nth(1)
        .and_then(|rest| rest.trim().split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse::<u32>().ok())
    {
        return match code {
            // ECC double-bit / row remap / uncontained memory errors.
            48 | 63 | 64 | 92 | 94 | 95 => Attribution::Confirmed(Component::Vram(gpu)),
            // Fallen off the bus.
            79 => Attribution::Unresolved(vec![Component::PcieGpu(gpu), Component::Platform]),
            _ => Attribution::Suspected(
                Component::Gpu(gpu),
                vec![Component::Vram(gpu), Component::PcieGpu(gpu)],
            ),
        };
    }
    if lower.contains("aer") || lower.contains("pcie") {
        return Attribution::Confirmed(Component::PcieGpu(gpu));
    }
    if lower.contains("lost") || lower.contains("fallen") {
        return Attribution::Unresolved(vec![Component::PcieGpu(gpu), Component::Platform]);
    }
    Attribution::Suspected(Component::Gpu(gpu), vec![Component::Vram(gpu)])
}

/// `(burst: N failed batches` in an async compute failure: N batches failed at once.
fn burst_batches(detail: &str) -> u64 {
    detail
        .split("burst: ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(1)
}

/// `gpu-vram-dit run` report (`report.json`). A multi-GPU report is read device by device;
/// `index_of` numbers a GPU from its PCI address (1 when unknown). [`gpu_by_order`] numbers them
/// in the report's own order instead.
#[must_use]
pub fn gpu(report: &Value, phase: &str, index_of: &dyn Fn(&str) -> u8) -> Findings {
    if let Some(devices) = report.pointer("/device_reports").and_then(Value::as_array) {
        let mut found = Findings::default();
        for (position, device) in devices.iter().enumerate() {
            let pci = str_at(device, "/selected_device/pci_address");
            let fallback = u8::try_from(position + 1).unwrap_or(u8::MAX);
            let number = if pci.is_empty() {
                fallback
            } else {
                index_of(pci)
            };
            let one = gpu_device(device, number);
            found.failures.extend(one.failures);
            found.hardware.extend(one.hardware);
        }
        return found.in_phase(phase);
    }
    let pci = str_at(report, "/selected_device/pci_address");
    let number = if pci.is_empty() { 1 } else { index_of(pci) };
    gpu_device(report, number).in_phase(phase)
}

/// [`gpu`] for a single-module run: GPUs numbered 1, 2, … in the order the report lists them.
#[must_use]
pub fn gpu_by_order(report: &Value) -> Findings {
    let order: Vec<String> = report
        .pointer("/device_reports")
        .and_then(Value::as_array)
        .map(|devices| {
            devices
                .iter()
                .map(|d| str_at(d, "/selected_device/pci_address").to_owned())
                .collect()
        })
        .unwrap_or_default();
    gpu(report, "", &|pci| {
        order
            .iter()
            .position(|p| p == pci)
            .and_then(|i| u8::try_from(i + 1).ok())
            .unwrap_or(1)
    })
}

fn gpu_device(report: &Value, gpu: u8) -> Findings {
    const LAYER: &str = "gpu-vram";
    let mut found = Findings::default();
    let host_changed =
        str_at(report, "/async_compute/failure_attribution") == "host_memory_changed";
    let records = report
        .pointer("/failures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for record in &records {
        let phase = str_at(record, "/phase");
        let detail = str_at(record, "/detail");
        if phase == "gpu-a-mock" {
            continue;
        }
        if phase == "hardware" {
            for reason in detail.split("; ").filter(|r| !r.is_empty()) {
                found.fail(FaultEvent::new(
                    LAYER,
                    "gpu:hardware",
                    1,
                    gpu_hardware_reason(reason, gpu),
                    "gpu/hardware_error",
                    reason,
                ));
            }
            continue;
        }
        let (attribution, rule) = if phase == "compute" && host_changed {
            // The host copy of the corpus no longer matched its hashes after the failure.
            (
                Attribution::Confirmed(Component::Ram),
                "gpu/host_corpus_changed",
            )
        } else {
            gpu_phase(phase, gpu)
        };
        let count = if phase == "compute" {
            burst_batches(detail)
        } else {
            1
        };
        found.fail(FaultEvent::new(
            LAYER,
            format!("gpu:{phase}"),
            count,
            attribution,
            rule,
            detail,
        ));
    }
    // Records the report counted but did not list.
    let unlisted = u64_at(report, "/failure_count").saturating_sub(records.len() as u64);
    if unlisted > 0 {
        found.fail(FaultEvent::new(
            LAYER,
            "gpu:unlisted",
            unlisted,
            Attribution::Unresolved(vec![
                Component::PcieGpu(gpu),
                Component::Gpu(gpu),
                Component::Vram(gpu),
            ]),
            "gpu/unlisted_records",
            "failure records beyond the report's list",
        ));
    }
    gpu_hardware(report, gpu, &mut found);
    found
}

/// Link AER and width, ECC and kernel-log warnings: corrected events, counted apart.
fn gpu_hardware(report: &Value, gpu: u8, found: &mut Findings) {
    let link = Component::PcieGpu(gpu);
    for (pointer, kind) in [
        ("/detection/link/aer_delta/correctable", "aer_corrected"),
        ("/detection/link/aer_delta/nonfatal", "aer_nonfatal"),
        ("/detection/link/aer_delta/fatal", "aer_fatal"),
        (
            "/detection/link/upstream_aer_delta/correctable",
            "aer_corrected_upstream",
        ),
        (
            "/detection/link/upstream_aer_delta/nonfatal",
            "aer_nonfatal_upstream",
        ),
        (
            "/detection/link/upstream_aer_delta/fatal",
            "aer_fatal_upstream",
        ),
    ] {
        found.hw(link, kind, u64_at(report, pointer));
    }
    let max_width = u64_at(report, "/detection/link/at_start/max_width");
    let min_width = u64_at(report, "/detection/link/min_width_seen");
    if max_width > 0 && min_width > 0 && min_width < max_width {
        found.hw(link, "link_width_drop", 1);
    }
    found.hw(
        Component::Vram(gpu),
        "ecc_corrected",
        u64_at(report, "/detection/ecc/corrected_delta"),
    );
    if str_at(report, "/detection/hardware_health/status") == "warnings" {
        for reason in report
            .pointer("/detection/hardware_health/reasons")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let cell = match gpu_hardware_reason(reason, gpu) {
                Attribution::Confirmed(cell) | Attribution::Suspected(cell, _) => cell,
                Attribution::Unresolved(_) => Component::PcieGpu(gpu),
            };
            found.hw(cell, "kernel_warning", 1);
        }
    }
}

// --------------------------------------------------------------------------------------- SSD

/// Cell of an SSD failure kind; `None` for tool errors that are no component's fault.
#[must_use]
pub fn ssd_kind(kind: &str, ssd: u8) -> Option<(Attribution, &'static str)> {
    let either = || Attribution::Unresolved(vec![Component::Ssd(ssd), Component::SsdPath(ssd)]);
    match kind {
        // Until the engine re-reads on a mismatch (F4), stored data and read path are one group.
        "data_mismatch" => Some((either(), "ssd/data_mismatch")),
        "short_read" | "short_write" | "io_error" | "timeout" => Some((either(), "ssd/io")),
        "device_disappeared" => Some((either(), "ssd/device_disappeared")),
        _ => None,
    }
}

/// `cpu-nvme-dit run-preserving-multi` report (`report.json`, the scheduler inside) or a bare
/// scheduler report. `alias_of` maps a namespace stable ID to the SSD number.
#[must_use]
pub fn ssd(report: &Value, phase: &str, alias_of: &dyn Fn(&str) -> Option<u8>) -> Findings {
    const LAYER: &str = "cpu-nvme";
    let mut found = Findings::default();
    let scheduler = report.pointer("/scheduler").unwrap_or(report);
    let targets = report
        .pointer("/targets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let target_alias = |id: &str| {
        targets.iter().find_map(|t| {
            (str_at(t, "/namespace_stable_id") == id)
                .then(|| u8::try_from(u64_at(t, "/alias")).ok())
                .flatten()
        })
    };
    let devices = scheduler
        .pointer("/devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (position, device) in devices.iter().enumerate() {
        let id = str_at(device, "/device_id");
        let number = alias_of(id)
            .or_else(|| target_alias(id))
            .unwrap_or_else(|| u8::try_from(position + 1).unwrap_or(u8::MAX));
        let Some(engine) = device.pointer("/report").filter(|r| !r.is_null()) else {
            let error = str_at(device, "/worker_error");
            if !error.is_empty() {
                found.fail(FaultEvent::new(
                    LAYER,
                    "ssd:worker_error",
                    1,
                    Attribution::Unresolved(vec![
                        Component::Ssd(number),
                        Component::SsdPath(number),
                    ]),
                    "ssd/worker_error",
                    error,
                ));
            }
            continue;
        };
        let records = engine
            .pointer("/failures")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut listed = 0_u64;
        for record in &records {
            let kind = str_at(record, "/kind");
            listed += 1;
            if let Some((attribution, rule)) = ssd_kind(kind, number) {
                found.fail(FaultEvent::new(
                    LAYER,
                    format!("ssd:{kind}"),
                    1,
                    attribution,
                    rule,
                    format!(
                        "offset {}: {}",
                        u64_at(record, "/offset_bytes"),
                        str_at(record, "/detail")
                    ),
                ));
            }
        }
        let unlisted = u64_at(engine, "/failure_count").saturating_sub(listed);
        if unlisted > 0 {
            found.fail(FaultEvent::new(
                LAYER,
                "ssd:unlisted",
                unlisted,
                Attribution::Unresolved(vec![Component::Ssd(number), Component::SsdPath(number)]),
                "ssd/unlisted_records",
                "failures beyond the report's list",
            ));
        }
    }
    found.in_phase(phase)
}

// ------------------------------------------------------------------------------------- chain

/// Cell of a `dit-chain` attribution (its confirm-on-fail tree, `dit-chain` section 10).
#[must_use]
pub fn chain_attribution(attribution: &str, gpu: u8, ssd: u8) -> (Attribution, &'static str) {
    match attribution {
        // The re-read was clean: the first read went wrong on its way.
        "ssd_read_path_transient" => (
            Attribution::Confirmed(Component::SsdPath(ssd)),
            "chain/read_path_transient",
        ),
        // Read twice, wrong twice: the data at rest is wrong.
        "ssd_stored_data" => (
            Attribution::Confirmed(Component::Ssd(ssd)),
            "chain/stored_data",
        ),
        "ssd_io" => (
            Attribution::Unresolved(vec![Component::Ssd(ssd), Component::SsdPath(ssd)]),
            "chain/ssd_io",
        ),
        // The host copy changed after its hashes were checked.
        "host_ram" => (Attribution::Confirmed(Component::Ram), "chain/host_ram"),
        "gpu_vram_pcie" => (
            Attribution::Unresolved(vec![
                Component::PcieGpu(gpu),
                Component::Gpu(gpu),
                Component::Vram(gpu),
            ]),
            "chain/gpu_round_trip",
        ),
        "stored_answer_prefill" | "cpu_or_ram" | "end_to_end" => (
            Attribution::Unresolved(vec![Component::Cpu, Component::Ram]),
            "chain/cpu_ram",
        ),
        _ => (
            Attribution::Unresolved(vec![Component::Cpu, Component::Ram]),
            "chain/unknown",
        ),
    }
}

/// `dit-chain` report (`report.json`); the chain runs on GPU `gpu` and SSD `ssd`.
#[must_use]
pub fn chain(report: &Value, phase: &str, gpu: u8, ssd: u8) -> Findings {
    const LAYER: &str = "chain";
    let mut found = Findings::default();
    let records = report
        .pointer("/failures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut last = None;
    for record in &records {
        let attribution = str_at(record, "/attribution");
        let (cell, rule) = chain_attribution(attribution, gpu, ssd);
        last = Some((cell.clone(), rule));
        found.fail(FaultEvent::new(
            LAYER,
            format!("chain:{attribution}"),
            1,
            cell,
            rule,
            format!(
                "{} record {}: {}",
                str_at(record, "/phase"),
                u64_at(record, "/record"),
                str_at(record, "/detail")
            ),
        ));
    }
    let unlisted = u64_at(report, "/failure_count").saturating_sub(records.len() as u64);
    if let (true, Some((cell, rule))) = (unlisted > 0, last) {
        found.fail(FaultEvent::new(
            LAYER,
            "chain:unlisted",
            unlisted,
            cell,
            rule,
            "failures beyond the report's list, counted like the last listed one",
        ));
    }
    found.in_phase(phase)
}

/// A layer that ended without a report (crash, OOM, watchdog kill).
#[must_use]
pub fn crash(layer: &'static str, phase: &str, detail: &str) -> FaultEvent {
    FaultEvent::new(
        layer,
        "platform:no_report",
        1,
        Attribution::Confirmed(Component::Platform),
        "session/no_report",
        detail,
    )
    .in_phase(phase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault_ledger::{Confidence, Ledger};

    fn one(_: &str) -> u8 {
        1
    }

    #[test]
    fn a_gpu_round_trip_mismatch_is_unresolved_and_counts_its_batches() {
        let report: Value =
            serde_json::from_str(include_str!("testdata/gpu-compute-fail.json")).unwrap();
        let found = gpu(&report, "OVERLAP", &one);
        assert_eq!(found.failures.len(), 1);
        let event = &found.failures[0];
        assert_eq!(event.confidence, Confidence::Unresolved);
        assert_eq!(event.cell, "PCIe-GPU1|GPU1|VRAM1");
        assert_eq!(event.phase, "OVERLAP");
        assert_eq!(event.count, 1, "burst: 1 failed batches");
        let mut ledger = Ledger::default();
        ledger.add_all(found.failures);
        assert_eq!(ledger.unresolved_total(), 1);
    }

    #[test]
    fn gpu_detectors_map_to_their_cells() {
        assert_eq!(
            gpu_phase("vram-sweep", 2).0,
            Attribution::Confirmed(Component::Vram(2))
        );
        assert_eq!(
            gpu_phase("retention", 1).0,
            Attribution::Unresolved(vec![Component::PcieGpu(1), Component::Vram(1)])
        );
        assert_eq!(
            gpu_hardware_reason("NVIDIA Xid 48", 1),
            Attribution::Confirmed(Component::Vram(1))
        );
        assert_eq!(
            gpu_hardware_reason("NVIDIA Xid 79", 1),
            Attribution::Unresolved(vec![Component::PcieGpu(1), Component::Platform])
        );
        assert_eq!(
            gpu_hardware_reason("PCIe AER uncorrected error", 1),
            Attribution::Confirmed(Component::PcieGpu(1))
        );
        assert_eq!(
            burst_batches("mismatch (burst: 12 failed batches, 768 words)"),
            12
        );
    }

    #[test]
    fn an_xid_48_hardware_failure_lands_on_vram() {
        let report: Value = serde_json::from_str(include_str!("testdata/gpu-xid48.json")).unwrap();
        let found = gpu(&report, "", &one);
        assert_eq!(found.failures.len(), 1);
        assert_eq!(found.failures[0].cell, "VRAM1");
        assert_eq!(found.failures[0].confidence, Confidence::Confirmed);
    }

    #[test]
    fn retention_records_of_a_sync_gpu_run_are_counted_one_by_one() {
        let report: Value =
            serde_json::from_str(include_str!("testdata/gpu-retention-fail.json")).unwrap();
        let found = gpu(&report, "", &one);
        let total: u64 = found.failures.iter().map(|e| e.count).sum();
        assert_eq!(total, 5);
        assert!(found.failures.iter().all(|e| e.cell == "PCIe-GPU1|VRAM1"));
    }

    #[test]
    fn chain_attributions_follow_its_confirm_tree() {
        let report: Value =
            serde_json::from_str(include_str!("testdata/chain-gpu-fail.json")).unwrap();
        let found = chain(&report, "OVERLAP", 1, 2);
        assert_eq!(found.failures.len(), 1);
        assert_eq!(found.failures[0].cell, "PCIe-GPU1|GPU1|VRAM1");
        assert_eq!(
            chain_attribution("ssd_read_path_transient", 1, 2).0,
            Attribution::Confirmed(Component::SsdPath(2))
        );
        assert_eq!(
            chain_attribution("host_ram", 1, 2).0,
            Attribution::Confirmed(Component::Ram)
        );
    }

    #[test]
    fn ssd_failures_are_unresolved_between_the_drive_and_its_path() {
        // Scheduler report of the engine's own simulator run with one injected bit flip.
        let report: Value =
            serde_json::from_str(include_str!("testdata/ssd-bitflip-report.json")).unwrap();
        let found = ssd(&report, "", &|_| None);
        let total: u64 = found.failures.iter().map(|e| e.count).sum();
        assert_eq!(total, 1);
        assert_eq!(found.failures[0].cell, "SSD1|SSD1-path");
        assert!(ssd_kind("write_budget_exceeded", 1).is_none());
    }

    #[test]
    fn cpu_ram_splits_cpu_checks_from_ram_patterns() {
        let report = serde_json::json!({
            "cpu": {"cpu_tests_completed": true, "integer_known_answer_passed": true,
                    "sha256_known_answer_passed": true, "fp_fma_known_answer_passed": false,
                    "coherency_passed": true},
            "workers": [{"scalar_simd_mismatches": 2}, {"scalar_simd_mismatches": 0}],
            "compute_oracle": {"enabled": true, "dense_fma_cross_worker_consistent": true,
                    "dense_fma_scalar_reference_passed": true, "branch_predictor_passed": false,
                    "aes256_known_answer_passed": true, "mismatch": null},
            "failures_total": 3 + 1000,
            "mismatch": {"global_offset": 4096, "arena_id": 1, "arena_offset": 0, "expected": 170,
                         "actual": 171, "xor_syndrome": 1, "writer_cpu": 3, "verifier_cpu": 5},
            "round_digests": [{"size_class_verified_bytes": {}}, {"size_class_verified_bytes": {"4k": 1}}],
            "diagnostic": {"classification": "reproducible_same_route"},
            "execution": {"worker_crashes": []},
            "telemetry": {"edac_corrected_delta": 2, "edac_uncorrected_delta": 0,
                          "throttle_core_delta": 0, "throttle_package_delta": 0,
                          "cgroup_oom_kill_delta": 0, "kernel_events": {"machine_check_events": 0}}
        });
        let found = cpu_ram(&report, "BASELINE CPU-RAM");
        let mut ledger = Ledger::default();
        ledger.add_all(found.failures);
        for event in &found.hardware {
            ledger.add_hardware(event);
        }
        // FMA KAT + 2 SIMD + branch oracle on the CPU; one failing round on RAM (suspected).
        assert_eq!(ledger.cell(Component::Cpu).confirmed, 4);
        assert_eq!(ledger.cell(Component::Ram).suspected, 1);
        assert_eq!(
            ledger.hardware_line(),
            "HW EVENTS     RAM edac_corrected +2"
        );
    }
}
