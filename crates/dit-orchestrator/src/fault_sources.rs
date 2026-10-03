//! Reads each layer's own report and turns its failures into [`FaultEvent`]s placed on component
//! cells (step F0: no engine change, only what the reports already carry). Rules are named
//! `<layer>/<rule>` and listed in `docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md` (sections 1 and 2).
//!
//! Until the GPU pipeline has boundary checks (step F3), a GPU result that is wrong after the
//! round trip stays unresolved between its `PCIe` link, the GPU and its VRAM, instead of being
//! blamed on the GPU.
use crate::fault_ledger::{Attribution, Component, FaultEvent, HardwareEvent, Throttle};
use dit_contracts::FaultRecord;
use serde_json::Value;

/// Failures and hardware counters found in one report.
#[derive(Debug, Default)]
pub struct Findings {
    pub failures: Vec<FaultEvent>,
    pub hardware: Vec<HardwareEvent>,
    /// Each device the report measured throttling of: `None` without throttle counters.
    pub throttle: Vec<(Component, Option<Throttle>)>,
}

impl Findings {
    fn fail(&mut self, event: FaultEvent) {
        self.failures.push(event);
    }

    fn throttled(&mut self, cell: Component, reading: Option<Throttle>) {
        self.throttle.push((cell, reading));
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
        // A pattern written and read by the CPU: RAM is the likely cell unless the confirmation
        // step (F4) re-read and rewrote it from other CPUs.
        let (attribution, rule) = mismatch
            .and_then(|m| m.pointer("/confirm").and_then(Value::as_str))
            .and_then(ram_confirmed)
            .unwrap_or((
                Attribution::Suspected(Component::Ram, vec![Component::Cpu]),
                "cpu-ram/pattern",
            ));
        found.fail(FaultEvent::new(
            CPU_RAM,
            "ram:pattern",
            failing_rounds,
            attribution,
            rule,
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
    // The CPU's own thermal throttling, each core and package once; N/A without counters.
    if let Some(throttle) = report.pointer("/telemetry/throttle") {
        found.throttled(
            Component::Cpu,
            throttle.as_object().map(|_| Throttle {
                events: u64_at(throttle, "/events"),
                seconds: throttle
                    .pointer("/longest_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| ms / 1000),
            }),
        );
    }
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

/// Counts from the GPU boundary-check tag of a failure detail (step F3), e.g.
/// `[boundary: d2h=12 device=1]`: how many failed batches each check places where.
#[must_use]
pub fn boundary_counts(detail: &str) -> Vec<(String, u64)> {
    let Some(start) = detail.find("[boundary: ") else {
        return Vec::new();
    };
    let rest = &detail[start + "[boundary: ".len()..];
    let body = rest.split(']').next().unwrap_or("");
    body.split_whitespace()
        .filter_map(|pair| {
            let (kind, count) = pair.split_once('=')?;
            let count: u64 = count.parse().ok()?;
            (count > 0).then(|| (kind.to_owned(), count))
        })
        .collect()
}

/// Cell a GPU boundary check places a failed batch in (step F3).
#[must_use]
pub fn boundary_attribution(kind: &str, gpu: u8) -> Option<(Attribution, &'static str)> {
    Some(match kind {
        // Input hashed on the GPU right after the upload differed from the host's hash, and a
        // second upload into the same place was clean: the copy went wrong on the link.
        "h2d" => (
            Attribution::Confirmed(Component::PcieGpu(gpu)),
            "gpu/boundary_h2d",
        ),
        // The GPU checked its own output before the copy back and found it right; the CPU found
        // it wrong after the copy, and the host buffer read the same twice: the link.
        "d2h" => (
            Attribution::Confirmed(Component::PcieGpu(gpu)),
            "gpu/boundary_d2h",
        ),
        // The input or the answers held in VRAM changed after they were checked, or a lane region
        // was right when written and wrong on the GPU at its re-read.
        "vram-input" | "vram-answers" | "vram-rest" => (
            Attribution::Confirmed(Component::Vram(gpu)),
            "gpu/boundary_vram_at_rest",
        ),
        // Wrong already on the GPU, with its input intact: computed wrong, or written wrong.
        "device" => (
            Attribution::Suspected(Component::Gpu(gpu), vec![Component::Vram(gpu)]),
            "gpu/boundary_device",
        ),
        // The input arrived wrong twice at the same place: link or VRAM.
        "h2d-persistent" => (
            Attribution::Unresolved(vec![Component::PcieGpu(gpu), Component::Vram(gpu)]),
            "gpu/boundary_h2d_persistent",
        ),
        // Right on the GPU, wrong on the host, and the host buffer cannot hold the right word.
        "host-ram" => (
            Attribution::Confirmed(Component::Ram),
            "gpu/boundary_host_ram",
        ),
        // The host holds exactly what the GPU checked right, or the CPU's second check passed:
        // the host's own check or its copy of the answers.
        "host" => (
            Attribution::Unresolved(vec![Component::Cpu, Component::Ram]),
            "gpu/boundary_host",
        ),
        _ => return None,
    })
}

/// Events of one GPU failure record (report or live), `count` batches or regions.
#[must_use]
pub fn gpu_failure_events(
    phase: &str,
    detail: &str,
    count: u64,
    gpu: u8,
    host_changed: bool,
) -> Vec<FaultEvent> {
    const LAYER: &str = "gpu-vram";
    if phase == "gpu-a-mock" {
        return Vec::new();
    }
    if phase == "hardware" {
        return detail
            .split("; ")
            .filter(|r| !r.is_empty())
            .map(|reason| {
                FaultEvent::new(
                    LAYER,
                    "gpu:hardware",
                    1,
                    gpu_hardware_reason(reason, gpu),
                    "gpu/hardware_error",
                    reason,
                )
            })
            .collect();
    }
    let source = format!("gpu:{phase}");
    if phase == "compute" && host_changed {
        // The host copy of the corpus no longer matched its hashes after the failure.
        return vec![FaultEvent::new(
            LAYER,
            source,
            count,
            Attribution::Confirmed(Component::Ram),
            "gpu/host_corpus_changed",
            detail,
        )];
    }
    let mut events = Vec::new();
    let mut placed = 0;
    for (kind, n) in boundary_counts(detail) {
        if let Some((attribution, rule)) = boundary_attribution(&kind, gpu) {
            placed += n;
            events.push(FaultEvent::new(
                LAYER,
                source.clone(),
                n,
                attribution,
                rule,
                detail,
            ));
        }
    }
    if placed < count {
        let (attribution, rule) = gpu_phase(phase, gpu);
        events.push(FaultEvent::new(
            LAYER,
            source,
            count - placed,
            attribution,
            rule,
            detail,
        ));
    }
    events
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
            found.throttle.extend(one.throttle);
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
        let count = if phase == "compute" {
            burst_batches(detail)
        } else {
            1
        };
        found
            .failures
            .extend(gpu_failure_events(phase, detail, count, gpu, host_changed));
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
    // Slowdowns for heat or a hardware signal (not the power cap), from the stress sampler.
    if let Some(stress) = report
        .pointer("/detection/stress")
        .filter(|stress| stress.is_object())
    {
        found.throttled(
            Component::Gpu(gpu),
            stress
                .pointer("/slowdown_episodes")
                .and_then(Value::as_u64)
                .map(|events| Throttle {
                    events,
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    seconds: stress
                        .pointer("/slowdown_seconds")
                        .and_then(Value::as_f64)
                        .map(|seconds| seconds.round().max(0.0) as u64),
                }),
        );
    }
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

/// Cell of an SSD failure after the engine's immediate re-read (step F4): clean on the re-read,
/// the first read went wrong on its way; the same wrong data again, the data at rest is wrong.
#[must_use]
pub fn ssd_confirmed(confirm: &str, ssd: u8) -> Option<(Attribution, &'static str)> {
    Some(match confirm {
        "reread_clean" | "reread_differs" => (
            Attribution::Confirmed(Component::SsdPath(ssd)),
            "ssd/reread_path",
        ),
        "reread_same" => (
            Attribution::Confirmed(Component::Ssd(ssd)),
            "ssd/reread_stored",
        ),
        // The same bytes passed when checked again: the first check was computed wrong.
        "recheck_clean" => (
            Attribution::Unresolved(vec![Component::Cpu, Component::Ram]),
            "ssd/recheck_clean",
        ),
        _ => return None,
    })
}

/// Event of one SSD failure record (report or live); `None` for tool errors.
#[must_use]
pub fn ssd_failure_event(
    kind: &str,
    ssd: u8,
    detail: &str,
    confirm: Option<&str>,
) -> Option<FaultEvent> {
    let (attribution, rule) = confirm
        .and_then(|c| ssd_confirmed(c, ssd))
        .map_or_else(|| ssd_kind(kind, ssd), Some)?;
    let detail = confirm.map_or_else(|| detail.to_owned(), |c| format!("{detail} [confirm: {c}]"));
    Some(FaultEvent::new(
        "cpu-nvme",
        format!("ssd:{kind}"),
        1,
        attribution,
        rule,
        detail,
    ))
}

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
            listed += 1;
            let confirm = str_at(record, "/confirm");
            found.failures.extend(ssd_failure_event(
                str_at(record, "/kind"),
                number,
                &format!(
                    "offset {}: {}",
                    u64_at(record, "/offset_bytes"),
                    str_at(record, "/detail")
                ),
                (!confirm.is_empty()).then_some(confirm),
            ));
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
    ssd_hardware(report, &mut found, alias_of, &target_alias);
    found.in_phase(phase)
}

/// SMART and AER changes of each SSD between the inventories taken before and after the run.
fn ssd_hardware(
    report: &Value,
    found: &mut Findings,
    alias_of: &dyn Fn(&str) -> Option<u8>,
    target_alias: &dyn Fn(&str) -> Option<u8>,
) {
    let devices = |pointer: &str| {
        report
            .pointer(pointer)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let before = devices("/inventory_before");
    for after in devices("/inventory_after") {
        let id = str_at(&after, "/stable_id");
        let Some(start) = before.iter().find(|d| str_at(d, "/stable_id") == id) else {
            continue;
        };
        let Some(number) = alias_of(id).or_else(|| target_alias(id)) else {
            continue;
        };
        let grew = |pointer: &str| u64_at(&after, pointer).saturating_sub(u64_at(start, pointer));
        found.hw(
            Component::Ssd(number),
            "media_errors",
            grew("/smart/media_and_data_integrity_errors"),
        );
        found.hw(
            Component::Ssd(number),
            "error_log_entries",
            grew("/smart/error_log_entries"),
        );
        found.hw(
            Component::Ssd(number),
            "warning_temp_min",
            grew("/smart/warning_temperature_minutes"),
        );
        found.hw(
            Component::Ssd(number),
            "critical_temp_min",
            grew("/smart/critical_temperature_minutes"),
        );
        // The drive's own thermal management (levels 1 and 2); N/A when it does not report it.
        if after.pointer("/smart/available").and_then(Value::as_bool) == Some(true) {
            let reported = after
                .pointer("/smart/thermal_level1_transitions")
                .is_some_and(Value::is_u64);
            found.throttled(
                Component::Ssd(number),
                reported.then(|| Throttle {
                    events: grew("/smart/thermal_level1_transitions")
                        + grew("/smart/thermal_level2_transitions"),
                    seconds: Some(
                        grew("/smart/thermal_level1_seconds")
                            + grew("/smart/thermal_level2_seconds"),
                    ),
                }),
            );
        }
        let counters = |device: &Value| -> Vec<(String, u64)> {
            device
                .pointer("/aer/counters")
                .and_then(Value::as_object)
                .map(|map| {
                    map.iter()
                        .filter_map(|(k, v)| {
                            let n = v.as_str()?.split_whitespace().last()?.parse().ok()?;
                            Some((k.clone(), n))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let old = counters(start);
        for (name, value) in counters(&after) {
            let previous = old.iter().find(|(k, _)| *k == name).map_or(0, |(_, v)| *v);
            found.hw(
                Component::SsdPath(number),
                &format!("aer:{name}"),
                value.saturating_sub(previous),
            );
        }
    }
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

/// Cell of one chain failure: its own attribution, refined by the GPU boundary tag of a GPU-side
/// failure (step F3; the check that placed most of the step's failed batches decides).
#[must_use]
pub fn chain_cell(
    attribution: &str,
    detail: &str,
    gpu: u8,
    ssd: u8,
) -> (Attribution, &'static str) {
    if attribution == "gpu_vram_pcie"
        && let Some((attribution, rule)) = boundary_counts(detail)
            .into_iter()
            .max_by_key(|(_, count)| *count)
            .and_then(|(kind, _)| boundary_attribution(&kind, gpu))
    {
        return (attribution, rule);
    }
    chain_attribution(attribution, gpu, ssd)
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
        let (cell, rule) = chain_cell(attribution, str_at(record, "/detail"), gpu, ssd);
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

// -------------------------------------------------------------------------------- live (F1)

/// How a session numbers its devices, for live records that name one.
pub struct Numbering<'a> {
    pub gpu: &'a dyn Fn(&str) -> u8,
    pub ssd: &'a dyn Fn(&str) -> Option<u8>,
    /// GPU and SSD the data chain runs on.
    pub chain_gpu: u8,
    pub chain_ssd: u8,
}

/// Cell of a CPU-RAM pattern failure after the engine's confirmation (step F4).
#[must_use]
pub fn ram_confirmed(confirm: &str) -> Option<(Attribution, &'static str)> {
    Some(match confirm {
        // Wrong from another CPU after the caches were evicted, and wrong again at the same bit
        // after a rewrite: the cell.
        "cell_persistent" => (
            Attribution::Confirmed(Component::Ram),
            "cpu-ram/confirm_cell",
        ),
        // Right from other CPUs, wrong again from the verifier's own CPU: that core.
        "verifier_core" => (
            Attribution::Confirmed(Component::Cpu),
            "cpu-ram/confirm_core",
        ),
        // Wrong from every CPU but right after a rewrite: held wrong (written wrong or decayed).
        "rewrite_fixed" => (
            Attribution::Suspected(Component::Ram, vec![Component::Cpu]),
            "cpu-ram/confirm_rewrite_fixed",
        ),
        // Right on every re-read: a transient read.
        "transient" => (
            Attribution::Unresolved(vec![Component::Cpu, Component::Ram]),
            "cpu-ram/confirm_transient",
        ),
        _ => return None,
    })
}

/// Events of one live record written by `layer` while it runs (step F1).
#[must_use]
pub fn from_record(
    layer: &str,
    record: &FaultRecord,
    numbering: &Numbering<'_>,
) -> Vec<FaultEvent> {
    let device = record.device.as_deref().unwrap_or("");
    let evidence = |key: &str| record.evidence.get(key).map(String::as_str);
    match layer {
        "gpu-vram" => gpu_failure_events(
            &record.source,
            &record.detail,
            if record.source == "compute" {
                burst_batches(&record.detail).max(record.count)
            } else {
                record.count
            },
            (numbering.gpu)(device),
            evidence("host") == Some("host_memory_changed"),
        ),
        "cpu-nvme" => {
            let number = (numbering.ssd)(device).unwrap_or(1);
            let kind = record.source.strip_prefix("ssd:").unwrap_or(&record.source);
            ssd_failure_event(kind, number, &record.detail, evidence("confirm"))
                .into_iter()
                .collect()
        }
        "chain" => {
            let attribution = record
                .source
                .strip_prefix("chain:")
                .unwrap_or(&record.source);
            let (cell, rule) = chain_cell(
                attribution,
                &record.detail,
                numbering.chain_gpu,
                numbering.chain_ssd,
            );
            vec![FaultEvent::new(
                "chain",
                record.source.clone(),
                record.count,
                cell,
                rule,
                record.detail.clone(),
            )]
        }
        _ => {
            let (attribution, rule) = match record.source.as_str() {
                "ram:pattern" => evidence("confirm").and_then(ram_confirmed).unwrap_or((
                    Attribution::Suspected(Component::Ram, vec![Component::Cpu]),
                    "cpu-ram/pattern",
                )),
                "ram:edac_uncorrected" => {
                    (Attribution::Confirmed(Component::Ram), "cpu-ram/edac_ue")
                }
                source if source.starts_with("platform:") => (
                    Attribution::Confirmed(Component::Platform),
                    "cpu-ram/worker_crash",
                ),
                _ => (Attribution::Confirmed(Component::Cpu), "cpu-ram/cpu_check"),
            };
            let number = |key: &str| evidence(key).and_then(|v| v.parse::<u64>().ok());
            vec![
                FaultEvent::new(
                    CPU_RAM,
                    record.source.clone(),
                    record.count,
                    attribution,
                    rule,
                    record.detail.clone(),
                )
                .located(number("global_offset"), number("syndrome")),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault_ledger::{Confidence, Ledger};

    #[test]
    fn boundary_tags_place_gpu_batches_and_leave_the_rest_unresolved() {
        let events = gpu_failure_events(
            "compute",
            "matrix mismatch (burst: 5 failed batches, 320 words) [boundary: d2h=3 device=1]",
            5,
            1,
            false,
        );
        let cells: Vec<(String, u64)> = events.iter().map(|e| (e.cell.clone(), e.count)).collect();
        assert_eq!(
            cells,
            vec![
                ("PCIe-GPU1".to_owned(), 3),
                ("GPU1".to_owned(), 1),
                ("PCIe-GPU1|GPU1|VRAM1".to_owned(), 1)
            ]
        );
        assert_eq!(events[0].confidence, Confidence::Confirmed);
        assert_eq!(events[1].confidence, Confidence::Suspected);
        assert!(boundary_counts("no tag").is_empty());
        assert_eq!(
            chain_cell("gpu_vram_pcie", "x [boundary: vram-input=2]", 1, 1).0,
            Attribution::Confirmed(Component::Vram(1))
        );
    }

    /// The failure details the GTX 960 produced for each injected boundary fault (step F3,
    /// build/f3/inj-*) land on the cells of roadmap section 5.
    #[test]
    fn injected_gpu_boundary_faults_land_on_their_cells() {
        let cases: [(&str, &str, Confidence); 7] = [
            (
                "CUDA unavailable: matrix mismatch at 2: expected 0x7bde4333, actual 0x7bde43b3 (burst: 1 failed batches, 64 mismatching words) [boundary: device=1]",
                "GPU1",
                Confidence::Suspected,
            ),
            (
                "CUDA unavailable: matrix mismatch at 2: expected 0x7bde4333, actual 0x7bde4323 (burst: 1 failed batches, 1 mismatching words) [boundary: d2h=1]",
                "PCIe-GPU1",
                Confidence::Confirmed,
            ),
            (
                "CUDA unavailable: corpus upload check: the corpus arrived wrong on the GPU and right after a second upload [boundary: h2d=1]",
                "PCIe-GPU1",
                Confidence::Confirmed,
            ),
            (
                "CUDA unavailable: corpus upload check: the corpus on the GPU is still wrong after a second upload [boundary: h2d-persistent=1]",
                "PCIe-GPU1|VRAM1",
                Confidence::Unresolved,
            ),
            (
                "CUDA unavailable: matrix mismatch at 2: expected 0x7bde4333, actual 0x46d9352 (burst: 7 failed batches, 448 mismatching words) [boundary: vram-input=7]",
                "VRAM1",
                Confidence::Confirmed,
            ),
            (
                "CUDA unavailable: answers held in VRAM changed after their upload check [boundary: vram-answers=1]",
                "VRAM1",
                Confidence::Confirmed,
            ),
            (
                "matrix mismatch (burst: 2 failed batches, 2 mismatching words) [boundary: host=1 host-ram=1]",
                "CPU|RAM",
                Confidence::Unresolved,
            ),
        ];
        for (detail, cell, confidence) in cases {
            let events = gpu_failure_events("compute", detail, 1, 1, false);
            let placed = boundary_counts(detail).iter().map(|(_, n)| n).sum::<u64>();
            let total: u64 = events.iter().map(|e| e.count).sum();
            assert_eq!(total, placed.max(burst_batches(detail)).max(1), "{detail}");
            assert_eq!(
                (events[0].cell.as_str(), events[0].confidence),
                (cell, confidence),
                "{detail}"
            );
        }
        // Without the checks the same failure stays on the unresolved compute group.
        let plain = gpu_failure_events(
            "compute",
            "CUDA unavailable: matrix mismatch at 2: expected 0x7bde4333, actual 0x7bde43b3 (burst: 1 failed batches, 64 mismatching words)",
            1,
            1,
            false,
        );
        assert_eq!(plain[0].cell, "PCIe-GPU1|GPU1|VRAM1");
    }

    /// Lane failures from the GTX 960 with the boundary checks on (build/f3/lane-inj-*).
    #[test]
    fn injected_lane_boundary_faults_land_on_their_cells() {
        let cases = [
            (
                "transfer",
                "allocation 0 offset 0 bytes 65536: mismatch at byte 0: expected 0x4f, actual 0x4e [boundary: d2h=1]",
                "PCIe-GPU1",
                Confidence::Confirmed,
            ),
            (
                "transfer",
                "allocation 0 offset 0 bytes 65536: mismatch at byte 0: expected 0x4f, actual 0xef [boundary: h2d=1]",
                "PCIe-GPU1",
                Confidence::Confirmed,
            ),
            (
                "transfer",
                "allocation 0 offset 0 bytes 65536: mismatch at byte 0: expected 0x4f, actual 0xef [boundary: h2d-persistent=1]",
                "PCIe-GPU1|VRAM1",
                Confidence::Unresolved,
            ),
            (
                "retention",
                "allocation 0 offset 0 bytes 65536: mismatch at byte 0: expected 0x4f, actual 0xef [boundary: vram-rest=1]",
                "VRAM1",
                Confidence::Confirmed,
            ),
        ];
        for (phase, detail, cell, confidence) in cases {
            let events = gpu_failure_events(phase, detail, 1, 1, false);
            assert_eq!(events.len(), 1, "{detail}");
            assert_eq!(
                (events[0].cell.as_str(), events[0].confidence),
                (cell, confidence),
                "{detail}"
            );
        }
        // Without the checks a transfer failure stays on the link-or-VRAM group.
        let plain = gpu_failure_events(
            "transfer",
            "allocation 0 offset 0 bytes 65536: mismatch at byte 0: expected 0x4f, actual 0x4e",
            1,
            1,
            false,
        );
        assert_eq!(plain[0].cell, "PCIe-GPU1|VRAM1");
    }

    #[test]
    fn live_records_use_the_same_rules_and_the_confirmations() {
        let numbering = Numbering {
            gpu: &|_| 2,
            ssd: &|id| (id == "nvme:X").then_some(3),
            chain_gpu: 1,
            chain_ssd: 1,
        };
        let ram = FaultRecord::new("ram:pattern", 1, "bit 3").evidence("confirm", "verifier_core");
        assert_eq!(from_record("cpu-ram", &ram, &numbering)[0].cell, "CPU");
        let ram = FaultRecord::new("ram:pattern", 1, "bit 3");
        assert_eq!(
            from_record("cpu-ram", &ram, &numbering)[0].confidence,
            Confidence::Suspected
        );
        let ssd = FaultRecord::new("ssd:data_mismatch", 1, "offset 0")
            .device("nvme:X")
            .evidence("confirm", "reread_clean");
        let event = &from_record("cpu-nvme", &ssd, &numbering)[0];
        assert_eq!(
            (event.cell.as_str(), event.confidence),
            ("SSD3-path", Confidence::Confirmed)
        );
        let gpu = FaultRecord::new("vram-sweep", 1, "").device("0000:08:00.0");
        assert_eq!(from_record("gpu-vram", &gpu, &numbering)[0].cell, "VRAM2");
        let chain = FaultRecord::new("chain:host_ram", 1, "");
        assert_eq!(from_record("chain", &chain, &numbering)[0].cell, "RAM");
    }

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

    /// Throttling is read per device: the CPU from its deduplicated counters (N/A without
    /// them), each GPU from its slowdown episodes, each SSD from its thermal management counters.
    #[test]
    fn throttle_is_read_for_each_device() {
        let throttled = |report: Value, find: &dyn Fn(&Value) -> Findings| find(&report).throttle;
        let cpu = |throttle: Value| {
            throttled(
                serde_json::json!({"telemetry": {"throttle": throttle}}),
                &|report| cpu_ram(report, ""),
            )
        };
        assert_eq!(
            cpu(serde_json::json!({"events": 3, "longest_ms": 2500})),
            [(
                Component::Cpu,
                Some(Throttle {
                    events: 3,
                    seconds: Some(2)
                })
            )]
        );
        assert_eq!(cpu(Value::Null), [(Component::Cpu, None)]);
        let gpu = throttled(
            serde_json::json!({"selected_device": {"pci_address": ""},
                "detection": {"stress": {"slowdown_episodes": 2, "slowdown_seconds": 14.4}}}),
            &|report| gpu(report, "", &|_| 1),
        );
        assert_eq!(
            gpu,
            [(
                Component::Gpu(1),
                Some(Throttle {
                    events: 2,
                    seconds: Some(14)
                })
            )]
        );
        let smart = |transitions: u64, seconds: u64| {
            serde_json::json!({"stable_id": "ssd-a", "smart": {"available": true,
                "thermal_level1_transitions": transitions, "thermal_level2_transitions": 0,
                "thermal_level1_seconds": seconds, "thermal_level2_seconds": 0,
                "warning_temperature_minutes": transitions}})
        };
        let report = serde_json::json!({
            "inventory_before": [smart(4, 100)],
            "inventory_after": [smart(6, 142)],
        });
        let found = ssd(&report, "", &|_| Some(1));
        assert_eq!(
            found.throttle,
            [(
                Component::Ssd(1),
                Some(Throttle {
                    events: 2,
                    seconds: Some(42)
                })
            )]
        );
        assert!(
            found
                .hardware
                .iter()
                .any(|event| event.kind == "warning_temp_min" && event.delta == 2)
        );
    }
}
