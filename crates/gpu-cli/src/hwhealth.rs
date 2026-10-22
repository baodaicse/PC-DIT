//! GPU-H1 hardware-health evidence: `PCIe` link and `AER` counters, kernel-log error
//! families, stress profile and the detection scorecard.
//!
//! Rules: a value that cannot be read is `None`/`unavailable`, never zero; log
//! and link data are supporting evidence and never change a data verdict; a
//! clean PASS does not prove the absence of corrected errors.
use dit_system::{GpuDevice, GpuTelemetry, SLOWDOWN_MASK, collect_gpu_telemetry};
use gpu_engine::accounting::MismatchSample;
use serde::Serialize;
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_LOG_SAMPLES: usize = 16;
const MAX_LOG_LINE: usize = 240;

// ---------------------------------------------------------------- PCIe link

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LinkSample {
    pub speed_gts: Option<f64>,
    pub width: Option<u32>,
    pub max_speed_gts: Option<f64>,
    pub max_width: Option<u32>,
}

/// `8.0 GT/s PCIe` -> `8.0`; anything else is unavailable.
fn parse_speed(text: &str) -> Option<f64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

#[must_use]
pub fn read_link(root: &Path) -> LinkSample {
    let speed = |name: &str| read_text(&root.join(name)).and_then(|t| parse_speed(&t));
    let width = |name: &str| read_text(&root.join(name)).and_then(|t| t.parse::<u32>().ok());
    LinkSample {
        speed_gts: speed("current_link_speed"),
        width: width("current_link_width"),
        max_speed_gts: speed("max_link_speed"),
        max_width: width("max_link_width"),
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AerCounters {
    pub correctable: Option<u64>,
    pub nonfatal: Option<u64>,
    pub fatal: Option<u64>,
}

fn parse_aer_total(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == key).then(|| parts.next()?.parse().ok())?
    })
}

#[must_use]
pub fn read_aer(root: &Path) -> AerCounters {
    let total =
        |file: &str, key: &str| read_text(&root.join(file)).and_then(|t| parse_aer_total(&t, key));
    AerCounters {
        correctable: total("aer_dev_correctable", "TOTAL_ERR_COR"),
        nonfatal: total("aer_dev_nonfatal", "TOTAL_ERR_NONFATAL"),
        fatal: total("aer_dev_fatal", "TOTAL_ERR_FATAL"),
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AerDelta {
    pub correctable: Option<u64>,
    pub nonfatal: Option<u64>,
    pub fatal: Option<u64>,
}

fn delta(start: Option<u64>, end: Option<u64>) -> Option<u64> {
    Some(end?.saturating_sub(start?))
}

/// Link state sampled around the run. Samples are taken between generations and
/// may land while the link is power-managed down, so `min_*` is a lower bound on
/// what the link negotiated, not proof of degradation; `max_*_seen` shows what
/// it reached.
#[derive(Clone, Debug, Serialize)]
pub struct LinkHealth {
    pub available: bool,
    pub pci_address: Option<String>,
    pub samples: u64,
    pub at_start: Option<LinkSample>,
    pub at_end: Option<LinkSample>,
    pub min_speed_seen_gts: Option<f64>,
    pub max_speed_seen_gts: Option<f64>,
    pub min_width_seen: Option<u32>,
    pub max_width_seen: Option<u32>,
    pub aer_at_start: Option<AerCounters>,
    pub aer_delta: Option<AerDelta>,
    /// The bridge directly above the GPU (root port or switch port). Link errors are often
    /// reported there rather than by the GPU function itself.
    pub upstream_port: Option<String>,
    pub upstream_aer_delta: Option<AerDelta>,
    #[serde(skip)]
    root: Option<std::path::PathBuf>,
    #[serde(skip)]
    upstream_root: Option<std::path::PathBuf>,
    #[serde(skip)]
    upstream_aer_at_start: Option<AerCounters>,
    #[serde(skip)]
    last_sample: Option<Instant>,
}

impl LinkHealth {
    #[must_use]
    pub fn begin(pci_address: Option<&str>) -> Self {
        let root = pci_address.map(|a| Path::new("/sys/bus/pci/devices").join(a));
        let upstream_root = root.as_deref().and_then(upstream_bridge);
        let sample = root.as_deref().map(read_link);
        let available = sample
            .as_ref()
            .is_some_and(|s| s.speed_gts.is_some() || s.width.is_some());
        let mut health = Self {
            available,
            pci_address: pci_address.map(str::to_owned),
            samples: 0,
            at_start: sample.clone(),
            at_end: None,
            min_speed_seen_gts: None,
            max_speed_seen_gts: None,
            min_width_seen: None,
            max_width_seen: None,
            aer_at_start: root.as_deref().map(read_aer),
            aer_delta: None,
            upstream_port: upstream_root
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned()),
            upstream_aer_delta: None,
            upstream_aer_at_start: upstream_root.as_deref().map(read_aer),
            root,
            upstream_root,
            last_sample: None,
        };
        if let Some(s) = sample {
            health.fold(&s);
        }
        health
    }

    fn fold(&mut self, s: &LinkSample) {
        self.samples += 1;
        if let Some(v) = s.speed_gts {
            self.min_speed_seen_gts = Some(self.min_speed_seen_gts.map_or(v, |m| m.min(v)));
            self.max_speed_seen_gts = Some(self.max_speed_seen_gts.map_or(v, |m| m.max(v)));
        }
        if let Some(v) = s.width {
            self.min_width_seen = Some(self.min_width_seen.map_or(v, |m| m.min(v)));
            self.max_width_seen = Some(self.max_width_seen.map_or(v, |m| m.max(v)));
        }
    }

    /// Cheap sysfs read, rate-limited so every generation can call it.
    pub fn sample(&mut self) {
        if self
            .last_sample
            .is_some_and(|t| t.elapsed() < Duration::from_millis(250))
        {
            return;
        }
        self.last_sample = Some(Instant::now());
        if let Some(root) = self.root.clone() {
            let s = read_link(&root);
            self.fold(&s);
        }
    }

    pub fn finish(&mut self) {
        if let Some(root) = self.root.clone() {
            let s = read_link(&root);
            self.fold(&s);
            self.at_end = Some(s);
            let end = read_aer(&root);
            if let Some(start) = self.aer_at_start {
                self.aer_delta = Some(aer_delta(start, end));
            }
        }
        if let (Some(root), Some(start)) = (self.upstream_root.clone(), self.upstream_aer_at_start)
        {
            self.upstream_aer_delta = Some(aer_delta(start, read_aer(&root)));
        }
    }
}

fn aer_delta(start: AerCounters, end: AerCounters) -> AerDelta {
    AerDelta {
        correctable: delta(start.correctable, end.correctable),
        nonfatal: delta(start.nonfatal, end.nonfatal),
        fatal: delta(start.fatal, end.fatal),
    }
}

/// The sysfs directory of the PCI bridge directly above `device` (its parent in the resolved
/// sysfs path), or `None` when the parent is not a PCI function (e.g. a host bridge root).
fn upstream_bridge(device: &Path) -> Option<std::path::PathBuf> {
    let parent = fs::canonicalize(device).ok()?.parent()?.to_path_buf();
    let name = parent.file_name()?.to_string_lossy().into_owned();
    is_bdf(&name).then_some(parent)
}

/// `dddd:bb:dd.f` in hex, the form sysfs and the kernel log name PCI functions by.
fn is_bdf(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 12
        && bytes.iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b':',
            10 => *b == b'.',
            _ => b.is_ascii_hexdigit(),
        })
}

// --------------------------------------------------------------- Kernel log

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct FamilyCounts {
    pub nvidia_xid: u64,
    pub device_lost: u64,
    pub pcie_aer_corrected: u64,
    pub pcie_aer_uncorrected: u64,
    pub amdgpu: u64,
    pub intel_gpu: u64,
    pub nouveau: u64,
}

impl FamilyCounts {
    #[must_use]
    pub fn total(&self) -> u64 {
        self.nvidia_xid
            + self.device_lost
            + self.pcie_aer_corrected
            + self.pcie_aer_uncorrected
            + self.amdgpu
            + self.intel_gpu
            + self.nouveau
    }
}

/// Classifies one kernel line. Returns true when it belongs to an error family.
pub fn classify_line(line: &str, counts: &mut FamilyCounts) -> bool {
    let l = line.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| l.contains(w));
    let mut hit = false;
    if l.contains("nvrm: xid") {
        counts.nvidia_xid += 1;
        hit = true;
    }
    if any(&["fallen off the bus", "gpu is lost", "device lost"]) {
        counts.device_lost += 1;
        hit = true;
    }
    if l.contains("aer:") || l.contains("pcie bus error") {
        if l.contains("uncorrected")
            || (l.contains("severity=") && !l.contains("severity=corrected"))
        {
            counts.pcie_aer_uncorrected += 1;
            hit = true;
        } else if l.contains("corrected") {
            counts.pcie_aer_corrected += 1;
            hit = true;
        }
    }
    if l.contains("amdgpu")
        && any(&[
            "ring",
            "timeout",
            "timedout",
            "gpu reset",
            "vm fault",
            "page fault",
            "hang",
            "ras ",
        ])
    {
        counts.amdgpu += 1;
        hit = true;
    }
    if (l.contains("i915") || l.contains("xe "))
        && any(&["gpu hang", "gpu reset", "hung", "engine reset", "*error*"])
    {
        counts.intel_gpu += 1;
        hit = true;
    }
    if l.contains("nouveau") && any(&["fifo", "timeout", "channel killed", "*error*", "fault"]) {
        counts.nouveau += 1;
        hit = true;
    }
    hit
}

/// NVIDIA Xid codes after which data was lost or the GPU stopped being usable: uncorrectable
/// ECC (48, 94 contained, 95 uncontained, 140 unrecovered), row-remap failure (64), `NVLink` (74),
/// fell off the bus (79), GSP firmware failure (119, 120), recovery needing a reset (154). Every
/// other code is a warning: application-level faults such as 13/31/43 would already show up as a
/// data mismatch if the hardware produced them, and 63/92 report ECC activity that was corrected.
pub const FATAL_XID: [u32; 10] = [48, 64, 74, 79, 94, 95, 119, 120, 140, 154];

/// How much one hardware event says about the device: `Warning` means it was corrected or did not
/// stop the device, `Error` means data was lost or the device became unusable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warning,
    Error,
}

/// Error-family lines of the run that concern this GPU, graded by [`Severity`]. Lines that name a
/// different PCI function are left out; lines naming none are kept, but an `Error` among them is
/// only a warning, so one card's fault never fails another card's run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LogFindings {
    pub warnings: u64,
    pub errors: u64,
    /// NVIDIA Xid code -> lines, for this GPU.
    pub xid_codes: std::collections::BTreeMap<u32, u64>,
    pub other_devices_skipped: u64,
    pub reasons: Vec<String>,
}

impl LogFindings {
    fn add(&mut self, severity: Severity, reason: String) {
        match severity {
            Severity::Warning => self.warnings += 1,
            Severity::Error => self.errors += 1,
        }
        if self.reasons.len() < MAX_LOG_SAMPLES && !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }
}

/// The code of an `NVRM: Xid (PCI:0000:01:00): 79, ...` line, from the lower-cased line.
fn xid_code(lower: &str) -> Option<u32> {
    let rest = &lower[lower.find("xid")? + 3..];
    let rest = &rest[rest.find("):")? + 2..];
    let digits: String = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// PCI addresses a line names: `dddd:bb:dd` (NVIDIA writes it without the function), with the
/// function kept when present. Lower-case hex.
fn named_devices(lower: &str) -> Vec<&str> {
    let bytes = lower.as_bytes();
    let hex = |b: u8| b.is_ascii_hexdigit();
    let mut found = Vec::new();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        let window = &bytes[i..i + 10];
        let shaped = window[4] == b':'
            && window[7] == b':'
            && window
                .iter()
                .enumerate()
                .all(|(j, b)| j == 4 || j == 7 || hex(*b))
            && (i == 0 || !hex(bytes[i - 1]));
        if shaped {
            // Keep the function (`.f`) when the line gives it, so sibling functions differ.
            let end =
                if bytes.get(i + 10) == Some(&b'.') && bytes.get(i + 11).is_some_and(|b| hex(*b)) {
                    i + 12
                } else {
                    i + 10
                };
            found.push(&lower[i..end]);
            i = end;
        } else {
            i += 1;
        }
    }
    found
}

enum Attribution {
    Ours,
    Other,
    Unnamed,
}

fn attribution(lower: &str, own: &[String]) -> Attribution {
    let named = named_devices(lower);
    if named.is_empty() {
        Attribution::Unnamed
    } else if named.iter().any(|device| {
        own.iter().any(|o| {
            if device.len() == 12 {
                o == device
            } else {
                o.starts_with(device)
            }
        })
    }) {
        Attribution::Ours
    } else {
        Attribution::Other
    }
}

/// Grades one kernel line for the GPU at `own` (its BDF and its upstream port's).
fn grade_line(line: &str, own: &[String], findings: &mut LogFindings) {
    let lower = line.to_ascii_lowercase();
    let mut family = FamilyCounts::default();
    if !classify_line(line, &mut family) {
        return;
    }
    let attribution = attribution(&lower, own);
    if matches!(attribution, Attribution::Other) {
        findings.other_devices_skipped += 1;
        return;
    }
    let code = (family.nvidia_xid > 0).then(|| xid_code(&lower)).flatten();
    if let Some(code) = code {
        *findings.xid_codes.entry(code).or_default() += 1;
    }
    let (severity, reason) = if family.device_lost > 0 {
        (Severity::Error, "GPU lost / fell off the bus".to_owned())
    } else if family.nvidia_xid > 0 {
        let fatal = code.is_some_and(|c| FATAL_XID.contains(&c));
        let label = code.map_or_else(
            || "NVIDIA Xid (unparsed)".to_owned(),
            |c| format!("NVIDIA Xid {c}"),
        );
        (
            if fatal {
                Severity::Error
            } else {
                Severity::Warning
            },
            label,
        )
    } else if family.pcie_aer_uncorrected > 0 {
        (Severity::Error, "PCIe AER uncorrected error".to_owned())
    } else if family.pcie_aer_corrected > 0 {
        (Severity::Warning, "PCIe AER corrected error".to_owned())
    } else {
        (Severity::Warning, "GPU driver error message".to_owned())
    };
    match (severity, attribution) {
        (Severity::Error, Attribution::Unnamed) => {
            findings.add(
                Severity::Warning,
                format!("{reason} (log line names no device)"),
            );
        }
        _ => findings.add(severity, reason),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct KernelLogEvidence {
    pub available: bool,
    pub source: String,
    pub lines_scanned: u64,
    /// `None` when the log could not be read; never a fake zero.
    pub families: Option<FamilyCounts>,
    /// Sum over families; one line can hit two families, so this is not a line count.
    pub family_hits_total: Option<u64>,
    pub lines_mentioning_device: Option<u64>,
    pub sample_lines: Vec<String>,
    /// Lines graded for this GPU; `None` when the log could not be read.
    pub findings: Option<LogFindings>,
    pub note: Option<String>,
}

impl KernelLogEvidence {
    fn unavailable(note: impl Into<String>) -> Self {
        Self {
            available: false,
            source: "journalctl -k".into(),
            lines_scanned: 0,
            families: None,
            family_hits_total: None,
            lines_mentioning_device: None,
            sample_lines: Vec::new(),
            findings: None,
            note: Some(note.into()),
        }
    }
}

#[must_use]
pub fn scan_kernel_text(
    text: &str,
    pci_address: Option<&str>,
    upstream_port: Option<&str>,
) -> KernelLogEvidence {
    let own: Vec<String> = [pci_address, upstream_port]
        .into_iter()
        .flatten()
        .map(str::to_ascii_lowercase)
        .collect();
    let mut findings = LogFindings::default();
    // NVIDIA Xid lines name the device as `PCI:0000:08:00` without the function
    // suffix, so match on domain:bus:device rather than the full BDF.
    let device = pci_address.map(|a| {
        let a = a.to_ascii_lowercase();
        a.rsplit_once('.')
            .map_or(a.clone(), |(head, _)| head.to_owned())
    });
    let mut counts = FamilyCounts::default();
    let mut mentions = 0_u64;
    let mut lines = 0_u64;
    let mut samples = Vec::new();
    for line in text.lines() {
        // journalctl markers such as `-- No entries --` are not kernel messages.
        if line.starts_with("-- ") {
            continue;
        }
        lines += 1;
        grade_line(line, &own, &mut findings);
        if classify_line(line, &mut counts) {
            if device
                .as_deref()
                .is_some_and(|d| line.to_ascii_lowercase().contains(d))
            {
                mentions += 1;
            }
            if samples.len() < MAX_LOG_SAMPLES {
                samples.push(line.chars().take(MAX_LOG_LINE).collect());
            }
        }
    }
    KernelLogEvidence {
        available: true,
        source: "journalctl -k".into(),
        lines_scanned: lines,
        family_hits_total: Some(counts.total()),
        families: Some(counts),
        lines_mentioning_device: device.map(|_| mentions),
        sample_lines: samples,
        findings: Some(findings),
        note: Some(
            "a clean log does not prove the absence of corrected errors; errors naming another PCI device are skipped"
                .into(),
        ),
    }
}

fn journal(args: &[&str]) -> Option<String> {
    let run = |program: &str, prefix: &[&str]| {
        Command::new(program)
            .args(prefix)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    run("timeout", &["10", "journalctl"]).or_else(|| run("journalctl", &[]))
}

/// Largest amount of kernel log kept for one run; beyond it the oldest text is dropped and the
/// evidence says so (a storm of one message must not grow memory without bound).
const MAX_LOG_BUFFER: usize = 8 * 1024 * 1024;

pub struct KernelLogWindow {
    cursor: Option<String>,
    /// Kernel lines read so far by [`Self::poll`], so the dashboard can grade them mid-run.
    buffer: String,
    truncated: bool,
}

impl KernelLogWindow {
    /// Marks the end of the journal so `finish` reads only lines from this run.
    #[must_use]
    pub fn begin() -> Self {
        let cursor = journal(&["-k", "--no-pager", "-n", "0", "--show-cursor"]).and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix("-- cursor: ").map(str::to_owned))
        });
        Self {
            cursor,
            buffer: String::new(),
            truncated: false,
        }
    }

    /// Reads kernel lines logged since the last read and keeps them. Returns false when the
    /// journal cannot be read (the run goes on; `finish` reports the log as unavailable).
    pub fn poll(&mut self) -> bool {
        let Some(cursor) = self.cursor.clone() else {
            return false;
        };
        let after = format!("--after-cursor={cursor}");
        let Some(text) = journal(&[
            "-k",
            "--no-pager",
            "-o",
            "short-monotonic",
            "--show-cursor",
            &after,
        ]) else {
            return false;
        };
        for line in text.lines() {
            if let Some(next) = line.strip_prefix("-- cursor: ") {
                self.cursor = Some(next.to_owned());
            } else if !line.starts_with("-- ") {
                self.buffer.push_str(line);
                self.buffer.push('\n');
            }
        }
        if self.buffer.len() > MAX_LOG_BUFFER {
            let cut = self.buffer.len() - MAX_LOG_BUFFER / 2;
            let cut = self.buffer[cut..].find('\n').map_or(cut, |at| cut + at + 1);
            self.buffer.drain(..cut);
            self.truncated = true;
        }
        true
    }

    /// Grades what has been read so far, for the live dashboard.
    #[must_use]
    pub fn findings_so_far(
        &self,
        pci_address: Option<&str>,
        upstream_port: Option<&str>,
    ) -> LogFindings {
        scan_kernel_text(&self.buffer, pci_address, upstream_port)
            .findings
            .unwrap_or_default()
    }

    #[must_use]
    pub fn finish(
        mut self,
        pci_address: Option<&str>,
        upstream_port: Option<&str>,
    ) -> KernelLogEvidence {
        if self.cursor.is_none() {
            return KernelLogEvidence::unavailable("journalctl -k cursor unavailable at start");
        }
        if !self.poll() {
            return KernelLogEvidence::unavailable("journalctl -k unreadable at end");
        }
        let mut evidence = scan_kernel_text(&self.buffer, pci_address, upstream_port);
        if self.truncated {
            evidence.note = Some(format!(
                "kernel log exceeded {} MiB during the run; only the newest part was graded",
                MAX_LOG_BUFFER / (1024 * 1024)
            ));
        }
        evidence
    }
}

// ------------------------------------------------------------------- GPU ECC

/// NVIDIA ECC and row-remap state of one GPU, from `nvidia-smi`. A `None` field was not reported:
/// `GeForce` cards report ECC as N/A, and drivers before row remapping lack those fields.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct EccSample {
    pub mode: Option<String>,
    pub corrected_volatile: Option<u64>,
    pub uncorrected_volatile: Option<u64>,
    pub remapped_rows_pending: Option<bool>,
    pub remapped_rows_failure: Option<bool>,
}

const ECC_FIELDS: &str = "pci.bus_id,ecc.mode.current,ecc.errors.corrected.volatile.total,\
ecc.errors.uncorrected.volatile.total,remapped_rows.pending,remapped_rows.failure";
const ECC_FIELDS_NO_REMAP: &str = "pci.bus_id,ecc.mode.current,ecc.errors.corrected.volatile.total,\
ecc.errors.uncorrected.volatile.total";

fn reported(value: &str) -> Option<&str> {
    let value = value.trim();
    let missing = value.is_empty()
        || value.starts_with('[')
        || value.eq_ignore_ascii_case("n/a")
        || value.eq_ignore_ascii_case("not supported");
    (!missing).then_some(value)
}

/// `bus:device.function`, lower case: `nvidia-smi` writes an 8-digit domain, sysfs a 4-digit one.
fn without_domain(address: &str) -> String {
    address
        .split_once(':')
        .map_or(address, |(_, rest)| rest)
        .trim()
        .to_ascii_lowercase()
}

/// The row of `nvidia-smi --query-gpu=<ECC_FIELDS> --format=csv,noheader,nounits` for this GPU.
#[must_use]
pub fn parse_ecc_csv(text: &str, pci_address: &str) -> Option<EccSample> {
    let wanted = without_domain(pci_address);
    text.lines().find_map(|line| {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        if cols.len() < 4 || without_domain(cols[0]) != wanted {
            return None;
        }
        let number = |i: usize| {
            cols.get(i)
                .and_then(|v| reported(v))
                .and_then(|v| v.parse().ok())
        };
        let flag = |i: usize| {
            cols.get(i)
                .and_then(|v| reported(v))
                .map(|v| v.eq_ignore_ascii_case("yes"))
        };
        Some(EccSample {
            mode: reported(cols[1]).map(str::to_owned),
            corrected_volatile: number(2),
            uncorrected_volatile: number(3),
            remapped_rows_pending: flag(4),
            remapped_rows_failure: flag(5),
        })
    })
}

fn query_ecc(pci_address: &str) -> Option<EccSample> {
    let run = |fields: &str| {
        let mut command = Command::new(dit_system::nvidia_smi_program());
        command
            .arg(format!("--query-gpu={fields}"))
            .arg("--format=csv,noheader,nounits")
            .env("LC_ALL", "C");
        dit_contracts::bounded::output_within(
            &mut command,
            std::time::Duration::from_secs(10),
            &dit_system::NVIDIA_SMI,
        )
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    // An older driver rejects the whole query for the row-remap fields; ask again without them.
    run(ECC_FIELDS)
        .or_else(|| run(ECC_FIELDS_NO_REMAP))
        .and_then(|text| parse_ecc_csv(&text, pci_address))
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EccEvidence {
    /// At least one ECC or row-remap value was reported at both ends of the run.
    pub available: bool,
    pub at_start: Option<EccSample>,
    pub at_end: Option<EccSample>,
    pub corrected_delta: Option<u64>,
    pub uncorrected_delta: Option<u64>,
    pub note: Option<String>,
}

pub struct EccWindow {
    pci_address: Option<String>,
    at_start: Option<EccSample>,
}

impl EccWindow {
    /// Only NVIDIA devices are queried; `nvidia-smi` does not know the others.
    #[must_use]
    pub fn begin(pci_address: Option<&str>, nvidia: bool) -> Self {
        let pci_address = pci_address.filter(|_| nvidia).map(str::to_owned);
        let at_start = pci_address.as_deref().and_then(query_ecc);
        Self {
            pci_address,
            at_start,
        }
    }

    #[must_use]
    pub fn finish(self) -> EccEvidence {
        let at_end = self.pci_address.as_deref().and_then(query_ecc);
        let (start, end) = (self.at_start.clone(), at_end.clone());
        let reported = |s: &Option<EccSample>| {
            s.as_ref().is_some_and(|s| {
                s.corrected_volatile.is_some()
                    || s.uncorrected_volatile.is_some()
                    || s.remapped_rows_failure.is_some()
            })
        };
        let available = reported(&start) && reported(&end);
        let pair = |f: fn(&EccSample) -> Option<u64>| {
            delta(start.as_ref().and_then(f), end.as_ref().and_then(f))
        };
        EccEvidence {
            available,
            corrected_delta: pair(|s| s.corrected_volatile),
            uncorrected_delta: pair(|s| s.uncorrected_volatile),
            at_start: self.at_start,
            at_end,
            note: (!available).then(|| {
                if self.pci_address.is_none() {
                    "not an NVIDIA device; no ECC source".to_owned()
                } else {
                    "ECC not reported by this card/driver (GeForce reports N/A; GDDR on-die ECC corrects silently)".to_owned()
                }
            }),
        }
    }
}

// ----------------------------------------------------------- Hardware health

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// The kernel log was read and nothing concerning this GPU was found.
    Clean,
    /// Corrected or non-fatal events: data still matched, the run stays PASS but is flagged.
    Warnings,
    /// Uncorrectable or device-lost events: the run gets a `hardware` failure (FAIL).
    Errors,
    /// Nothing was found, but the kernel log could not be read, so "clean" cannot be claimed.
    Unavailable,
}

impl HealthStatus {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Warnings => "warnings",
            Self::Errors => "errors",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Every hardware-error source of the run for one GPU, graded. Warnings/errors add up signals
/// from several sources (the same corrected AER event can appear as a counter and a log line).
#[derive(Clone, Debug, Serialize)]
pub struct HardwareHealth {
    pub status: HealthStatus,
    pub warnings: u64,
    pub errors: u64,
    pub reasons: Vec<String>,
    pub sources_unavailable: Vec<String>,
}

impl HardwareHealth {
    /// One line for the dashboard when events were found; `None` for clean and for unavailable
    /// (the report says `unavailable`; the dashboard only flags what was actually seen).
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        match self.status {
            HealthStatus::Clean | HealthStatus::Unavailable => None,
            HealthStatus::Warnings | HealthStatus::Errors => Some(format!(
                "{} error(s), {} warning(s): {}",
                self.errors,
                self.warnings,
                self.reasons.join("; ")
            )),
        }
    }
}

fn add_aer(findings: &mut LogFindings, delta: Option<&AerDelta>, place: &str) {
    let Some(delta) = delta else { return };
    if let Some(n) = delta.correctable.filter(|n| *n > 0) {
        findings.warnings += n;
        findings
            .reasons
            .push(format!("PCIe AER corrected +{n} ({place})"));
    }
    for (label, value) in [("non-fatal", delta.nonfatal), ("fatal", delta.fatal)] {
        if let Some(n) = value.filter(|n| *n > 0) {
            findings.errors += n;
            findings
                .reasons
                .push(format!("PCIe AER uncorrected {label} +{n} ({place})"));
        }
    }
}

/// Grades the run's link, kernel-log and ECC evidence for one GPU.
#[must_use]
pub fn assess_health(
    link: &LinkHealth,
    log: &KernelLogEvidence,
    ecc: &EccEvidence,
) -> HardwareHealth {
    let mut findings = log.findings.clone().unwrap_or_default();
    let mut unavailable = Vec::new();
    if !log.available {
        unavailable.push("kernel log".to_owned());
    }
    if link.aer_delta.is_none() {
        unavailable.push("PCIe AER counters".to_owned());
    }
    add_aer(&mut findings, link.aer_delta.as_ref(), "GPU");
    let upstream = link.upstream_port.as_deref().map_or_else(
        || "upstream port".to_owned(),
        |p| format!("upstream port {p}"),
    );
    add_aer(&mut findings, link.upstream_aer_delta.as_ref(), &upstream);
    if ecc.available {
        if let Some(n) = ecc.corrected_delta.filter(|n| *n > 0) {
            findings.warnings += n;
            findings.reasons.push(format!("GPU ECC corrected +{n}"));
        }
        if let Some(n) = ecc.uncorrected_delta.filter(|n| *n > 0) {
            findings.errors += n;
            findings.reasons.push(format!("GPU ECC uncorrected +{n}"));
        }
        let end = ecc.at_end.as_ref();
        if end.and_then(|e| e.remapped_rows_failure) == Some(true) {
            findings.errors += 1;
            findings.reasons.push("GPU row remapping failed".to_owned());
        } else if end.and_then(|e| e.remapped_rows_pending) == Some(true) {
            findings.warnings += 1;
            findings
                .reasons
                .push("GPU row remap pending (needs a reset)".to_owned());
        }
    } else {
        unavailable.push("GPU ECC counters".to_owned());
    }
    let status = if findings.errors > 0 {
        HealthStatus::Errors
    } else if findings.warnings > 0 {
        HealthStatus::Warnings
    } else if log.available {
        HealthStatus::Clean
    } else {
        HealthStatus::Unavailable
    };
    HardwareHealth {
        status,
        warnings: findings.warnings,
        errors: findings.errors,
        reasons: findings.reasons,
        sources_unavailable: unavailable,
    }
}

// ------------------------------------------------------------ Stress profile

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stat {
    pub min: f64,
    pub mean: f64,
    pub max: f64,
}

#[derive(Clone, Debug, Default)]
struct Running {
    n: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Running {
    fn add(&mut self, v: Option<f64>) {
        let Some(v) = v.filter(|v| v.is_finite()) else {
            return;
        };
        if self.n == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.n += 1;
        self.sum += v;
    }
    #[allow(clippy::cast_precision_loss)]
    fn stat(&self) -> Option<Stat> {
        (self.n > 0).then(|| Stat {
            min: self.min,
            mean: self.sum / self.n as f64,
            max: self.max,
        })
    }
}

const THROTTLE_NAMES: [(u64, &str); 9] = [
    (0x1, "GpuIdle"),
    (0x2, "ApplicationsClocksSetting"),
    (0x4, "SwPowerCap"),
    (0x8, "HwSlowdown"),
    (0x10, "SyncBoost"),
    (0x20, "SwThermalSlowdown"),
    (0x40, "HwThermalSlowdown"),
    (0x80, "HwPowerBrakeSlowdown"),
    (0x100, "DisplayClockSetting"),
];

#[must_use]
pub fn decode_throttle(mask: u64) -> Vec<&'static str> {
    THROTTLE_NAMES
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct StressAccumulator {
    power: Running,
    temperature: Running,
    core_clock: Running,
    memory_clock: Running,
    utilization: Running,
    power_limit: Option<f64>,
    throttle_seen: Option<u64>,
    throttle_samples: u64,
    slowdown_samples: u64,
    slowdown_episodes: u64,
    slowing_down: bool,
    first_temperature: Option<f64>,
    last_temperature: Option<f64>,
    samples: u64,
}

impl StressAccumulator {
    pub fn observe(&mut self, t: &GpuTelemetry) {
        self.samples += 1;
        self.power.add(t.power_watts);
        self.temperature.add(t.temperature_celsius);
        self.core_clock.add(t.core_clock_mhz);
        self.memory_clock.add(t.memory_clock_mhz);
        self.utilization.add(t.utilization_percent);
        if let Some(limit) = t.power_limit_watts {
            self.power_limit = Some(limit);
        }
        if self.first_temperature.is_none() {
            self.first_temperature = t.temperature_celsius;
        }
        if t.temperature_celsius.is_some() {
            self.last_temperature = t.temperature_celsius;
        }
        if let Some(mask) = t.throttle_reasons_mask {
            self.throttle_seen = Some(self.throttle_seen.unwrap_or(0) | mask);
            // GpuIdle alone is not a limit on a loaded device; count any other bit.
            if mask & !0x1 != 0 {
                self.throttle_samples += 1;
            }
            let slowing = mask & SLOWDOWN_MASK != 0;
            if slowing {
                self.slowdown_samples += 1;
                if !self.slowing_down {
                    self.slowdown_episodes += 1;
                }
            }
            self.slowing_down = slowing;
        }
    }

    #[must_use]
    pub fn finish(&self, duration_seconds: f64) -> StressProfile {
        let power = self.power.stat();
        StressProfile {
            version: 1,
            samples: self.samples,
            duration_seconds,
            sampling: "background, about 1 Hz; short spikes between samples are not seen".into(),
            power_watts: power.clone(),
            power_limit_watts: self.power_limit,
            power_max_percent_of_limit: power
                .as_ref()
                .zip(self.power_limit)
                .filter(|(_, limit)| *limit > 0.0)
                .map(|(p, limit)| p.max / limit * 100.0),
            temperature_celsius: self.temperature.stat(),
            temperature_first_celsius: self.first_temperature,
            temperature_last_celsius: self.last_temperature,
            core_clock_mhz: self.core_clock.stat(),
            memory_clock_mhz: self.memory_clock.stat(),
            utilization_percent: self.utilization.stat(),
            throttle_reasons_available: self.throttle_seen.is_some(),
            throttle_reasons_seen_mask: self.throttle_seen,
            throttle_reasons_seen: self.throttle_seen.map(decode_throttle).unwrap_or_default(),
            throttle_limited_samples: self.throttle_seen.map(|_| self.throttle_samples),
            slowdown_episodes: self.throttle_seen.map(|_| self.slowdown_episodes),
            #[allow(clippy::cast_precision_loss)]
            slowdown_seconds: self.throttle_seen.map(|_| {
                duration_seconds * self.slowdown_samples as f64 / self.samples.max(1) as f64
            }),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StressProfile {
    pub version: u32,
    pub samples: u64,
    pub duration_seconds: f64,
    pub sampling: String,
    pub power_watts: Option<Stat>,
    pub power_limit_watts: Option<f64>,
    /// Transient readings can exceed 100%; this is an observation, not a verdict input.
    pub power_max_percent_of_limit: Option<f64>,
    pub temperature_celsius: Option<Stat>,
    /// First and last readings distinguish warm-up from steady state.
    pub temperature_first_celsius: Option<f64>,
    pub temperature_last_celsius: Option<f64>,
    pub core_clock_mhz: Option<Stat>,
    pub memory_clock_mhz: Option<Stat>,
    pub utilization_percent: Option<Stat>,
    pub throttle_reasons_available: bool,
    pub throttle_reasons_seen_mask: Option<u64>,
    pub throttle_reasons_seen: Vec<&'static str>,
    pub throttle_limited_samples: Option<u64>,
    /// Times the GPU entered a slowdown ([`SLOWDOWN_MASK`]) and about how long it stayed there
    /// (its share of the samples times the duration); `None` without throttle reasons.
    pub slowdown_episodes: Option<u64>,
    pub slowdown_seconds: Option<f64>,
}

/// Samples telemetry on its own thread so readings are not tied to the
/// generation boundaries where the GPU is often momentarily idle.
pub struct StressSampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<StressAccumulator>>,
    started: Instant,
}

impl StressSampler {
    #[must_use]
    pub fn start(devices: Vec<GpuDevice>, pci_address: Option<String>, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("gpu-stress-sampler".into())
            .spawn(move || {
                let mut acc = StressAccumulator::default();
                let mut next = Instant::now();
                while !flag.load(Ordering::Relaxed) {
                    if Instant::now() >= next {
                        for sample in collect_gpu_telemetry(&devices) {
                            if pci_address
                                .as_deref()
                                .is_none_or(|a| a == sample.pci_address)
                            {
                                acc.observe(&sample);
                            }
                        }
                        next = Instant::now() + interval;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                acc
            })
            .ok();
        Self {
            stop,
            handle,
            started: Instant::now(),
        }
    }

    #[must_use]
    pub fn finish(mut self) -> Option<StressProfile> {
        self.stop.store(true, Ordering::Relaxed);
        let acc = self.handle.take()?.join().ok()?;
        Some(acc.finish(self.started.elapsed().as_secs_f64()))
    }
}

impl Drop for StressSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------- Scorecard

#[derive(Clone, Debug, Default, Serialize)]
pub struct ComputeDetection {
    /// `u32` GEMM operations verified (multiply + add per inner step and cell).
    pub verified_integer_ops: u64,
    pub verified_integer_ops_per_second: f64,
    /// FP32 FMA GEMM operations verified (one fused multiply-add counted as two flops).
    pub verified_fp32_flops: u64,
    pub verified_fp32_flops_per_second: f64,
    /// INT8 tensor-core GEMM operations verified (multiply + add per step and cell).
    pub verified_int8_tensor_ops: u64,
    pub verified_int8_tensor_ops_per_second: f64,
    /// v1.2 C9: u32 GEMM chain operations verified (every layer and the projection).
    pub verified_chain_ops: u64,
    pub verified_chain_ops_per_second: f64,
    pub failed_batches: u64,
    /// Async path only: every differing word of every failed batch.
    pub mismatch_words: Option<u64>,
    pub mismatch_samples: Vec<MismatchSample>,
    /// One record per failing compute call (burst), so it can be lower than `failed_batches`.
    pub failure_records: u64,
    pub unfinished_batches: Option<u64>,
    pub conservation: Option<String>,
    /// Host-corpus integrity passes (async only): re-hash of the host copy of the corpus.
    pub integrity_checks: Option<u64>,
    pub integrity_failures: Option<u64>,
    /// `corpus_intact` / `host_memory_changed` after a failed batch; not proof of the culprit.
    pub failure_attribution: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CoverageDetection {
    /// Transfer plus delayed re-read bytes compared against the independent pattern.
    pub vram_pcie_verified_bytes: u64,
    pub vram_pcie_verified_mib_per_second: f64,
    pub transfer_operations: u64,
    /// "Retention" checks re-read after a short delay; not a DRAM dwell-time test.
    pub delayed_reread_checks: u64,
    /// Version of the transfer pattern schedule and how many operations used each class.
    pub pattern_schedule_version: u32,
    pub pattern_classes: std::collections::BTreeMap<&'static str, u64>,
    pub allocation_sweeps: u64,
    pub allocated_bytes: u64,
    /// GPU-DRAM sweep (`--vram-sweep`): bytes the device read back and compared with no
    /// mismatch. Device-verified, not CPU-verified; `None` when the sweep was off.
    pub device_verified_bytes: Option<u64>,
    pub device_verified_gib_per_second: Option<f64>,
    /// Sweep bytes the CPU re-checked over `PCIe` (checkpoints proving the device's own check).
    pub sweep_checkpoint_cpu_verified_bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct FirstFailure {
    pub at_seconds: f64,
    pub verified_results_before: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DetectionScorecard {
    pub version: u32,
    pub compute: ComputeDetection,
    pub coverage: CoverageDetection,
    pub first_failure: Option<FirstFailure>,
    pub stress: Option<StressProfile>,
    pub link: LinkHealth,
    pub kernel_log: KernelLogEvidence,
    pub ecc: EccEvidence,
    /// Link, kernel-log and ECC evidence graded for this GPU; `errors` makes the run FAIL.
    pub hardware_health: HardwareHealth,
    pub notes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_and_aer_parsers_treat_bad_input_as_unavailable() {
        assert_eq!(parse_speed("8.0 GT/s PCIe"), Some(8.0));
        assert_eq!(parse_speed("Unknown"), None);
        assert_eq!(parse_speed(""), None);
        let text = "RxErr 0\nBadTLP 2\nTOTAL_ERR_COR 7\n";
        assert_eq!(parse_aer_total(text, "TOTAL_ERR_COR"), Some(7));
        assert_eq!(parse_aer_total(text, "TOTAL_ERR_FATAL"), None);
        assert_eq!(delta(Some(3), Some(10)), Some(7));
        assert_eq!(
            delta(None, Some(1)),
            None,
            "missing start is unavailable, not zero"
        );
    }

    #[test]
    fn link_health_reads_a_fake_sysfs_and_reports_aer_delta() {
        let root = std::env::temp_dir().join(format!("dit-link-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let w = |n: &str, v: &str| fs::write(root.join(n), v).unwrap();
        w("current_link_speed", "8.0 GT/s PCIe\n");
        w("current_link_width", "16\n");
        w("max_link_speed", "8.0 GT/s PCIe\n");
        w("max_link_width", "16\n");
        w("aer_dev_correctable", "RxErr 0\nTOTAL_ERR_COR 1\n");
        w("aer_dev_nonfatal", "TOTAL_ERR_NONFATAL 0\n");
        w("aer_dev_fatal", "TOTAL_ERR_FATAL 0\n");
        let mut health = LinkHealth {
            root: Some(root.clone()),
            ..LinkHealth::begin(None)
        };
        health.aer_at_start = Some(read_aer(&root));
        health.sample();
        w("current_link_speed", "2.5 GT/s PCIe\n");
        w("current_link_width", "8\n");
        w("aer_dev_correctable", "TOTAL_ERR_COR 5\n");
        health.last_sample = None;
        health.sample();
        health.finish();
        assert_eq!(health.min_speed_seen_gts, Some(2.5));
        assert_eq!(health.max_speed_seen_gts, Some(8.0));
        assert_eq!(health.min_width_seen, Some(8));
        let d = health.aer_delta.unwrap();
        assert_eq!(
            (d.correctable, d.nonfatal, d.fatal),
            (Some(4), Some(0), Some(0))
        );
        fs::remove_dir_all(root).unwrap();
        let missing = LinkHealth::begin(Some("0000:ff:00.0"));
        assert!(!missing.available && missing.aer_at_start.unwrap() == AerCounters::default());
    }

    const MIXED_LOG: &str = "\
[  10.1] NVRM: Xid (PCI:0000:08:00): 79, pid=1, GPU has fallen off the bus.
[  10.2] pcieport 0000:00:01.1: AER: Corrected error message received from 0000:08:00.0
[  10.3] pcieport 0000:00:01.1: AER: Uncorrected (Non-Fatal) error received: 0000:08:00.0
[  10.4] pcieport 0000:00:01.1: PCIe Bus Error: severity=Corrected, type=Physical Layer
[  10.5] amdgpu 0000:03:00.0: amdgpu: ring gfx_0.0.0 timeout, signaled seq=10
[  10.6] i915 0000:00:02.0: [drm] GPU HANG: ecode 9:1:85dffffb
[  10.7] nouveau 0000:01:00.0: fifo: channel 3 killed!
[  10.8] audit: type=1400 apparmor=\"DENIED\" operation=\"symlink\"
";

    #[test]
    fn log_lines_are_graded_for_this_gpu_and_its_port_only() {
        let nvidia = scan_kernel_text(MIXED_LOG, Some("0000:08:00.0"), Some("0000:00:01.1"))
            .findings
            .unwrap();
        // Xid 79 (fell off the bus) and the uncorrected AER are errors; the two corrected AER
        // lines (one names only the upstream port) are warnings; other cards' lines are skipped.
        assert_eq!((nvidia.errors, nvidia.warnings), (2, 2), "{nvidia:?}");
        assert_eq!(nvidia.other_devices_skipped, 3);
        assert_eq!(nvidia.xid_codes.get(&79), Some(&1));

        // The AMD card in the same machine sees only its own ring timeout, as a warning.
        let amd = scan_kernel_text(MIXED_LOG, Some("0000:03:00.0"), None)
            .findings
            .unwrap();
        assert_eq!((amd.errors, amd.warnings), (0, 1), "{amd:?}");

        // A sibling function of the upstream port is another port, not ours.
        let sibling = scan_kernel_text(
            "pcieport 0000:00:01.0: AER: Uncorrected (Fatal) error received: 0000:00:01.0\n",
            Some("0000:08:00.0"),
            Some("0000:00:01.1"),
        )
        .findings
        .unwrap();
        assert_eq!((sibling.errors, sibling.other_devices_skipped), (0, 1));
    }

    #[test]
    fn xid_codes_set_severity_and_unnamed_errors_only_warn() {
        let grade = |line: &str| {
            scan_kernel_text(line, Some("0000:01:00.0"), None)
                .findings
                .unwrap()
        };
        let app = grade("NVRM: Xid (PCI:0000:01:00): 13, pid=77, Graphics Exception\n");
        assert_eq!((app.errors, app.warnings), (0, 1));
        let dbe = grade("NVRM: Xid (PCI:0000:01:00): 48, pid=77, DBE (double bit error)\n");
        assert_eq!((dbe.errors, dbe.warnings), (1, 0));
        assert_eq!(dbe.xid_codes.get(&48), Some(&1));
        let old_format = grade("NVRM: Xid (0000:01:00): 95, uncontained ECC error\n");
        assert_eq!(old_format.errors, 1);
        // A device-lost line that names no device cannot be pinned on this GPU.
        let unnamed = grade("NVRM: GPU is lost\n");
        assert_eq!((unnamed.errors, unnamed.warnings), (0, 1));
        assert!(unnamed.reasons[0].contains("names no device"));
        assert_eq!(
            xid_code("nvrm: xid (pci:0000:01:00): 154, gpu recovery"),
            Some(154)
        );
        assert_eq!(xid_code("nvrm: xid without a code"), None);
    }

    #[test]
    fn ecc_rows_parse_for_this_gpu_and_na_is_unavailable() {
        let text = "00000000:01:00.0, Enabled, 3, 0, No, No\n00000000:08:00.0, [N/A], [N/A], [N/A], [N/A], [N/A]\n";
        let pro = parse_ecc_csv(text, "0000:01:00.0").unwrap();
        assert_eq!(pro.corrected_volatile, Some(3));
        assert_eq!(pro.uncorrected_volatile, Some(0));
        assert_eq!(pro.remapped_rows_failure, Some(false));
        let geforce = parse_ecc_csv(text, "0000:08:00.0").unwrap();
        assert_eq!(geforce, EccSample::default(), "N/A is not zero");
        assert!(parse_ecc_csv(text, "0000:09:00.0").is_none());
    }

    #[test]
    fn hardware_health_status_follows_the_worst_evidence() {
        let log = |text: &str| scan_kernel_text(text, Some("0000:08:00.0"), None);
        let mut link = LinkHealth::begin(None);
        let no_ecc = EccEvidence::default();
        let clean = assess_health(&link, &log("[1.0] usb 1-1: new device\n"), &no_ecc);
        assert_eq!(clean.status, HealthStatus::Clean);
        assert!(
            clean
                .sources_unavailable
                .contains(&"GPU ECC counters".to_owned())
        );
        assert!(clean.summary().is_none());

        link.aer_delta = Some(AerDelta {
            correctable: Some(5),
            nonfatal: Some(0),
            fatal: Some(0),
        });
        let corrected = assess_health(&link, &log(""), &no_ecc);
        assert_eq!(
            (corrected.status, corrected.warnings),
            (HealthStatus::Warnings, 5)
        );
        assert!(
            corrected
                .summary()
                .unwrap()
                .contains("PCIe AER corrected +5 (GPU)")
        );

        let ecc = EccEvidence {
            available: true,
            uncorrected_delta: Some(1),
            ..EccEvidence::default()
        };
        let uncorrected = assess_health(&link, &log(""), &ecc);
        assert_eq!(uncorrected.status, HealthStatus::Errors);

        let unreadable = assess_health(
            &LinkHealth::begin(None),
            &KernelLogEvidence::unavailable("test"),
            &no_ecc,
        );
        assert_eq!(unreadable.status, HealthStatus::Unavailable);
        assert!(
            unreadable.summary().is_none(),
            "unavailable is reported, not flagged"
        );
    }

    #[test]
    fn kernel_log_families_are_counted_from_fixtures_and_noise_is_ignored() {
        let text = "\
[  10.1] NVRM: Xid (PCI:0000:08:00): 79, pid=1, GPU has fallen off the bus.
[  10.2] pcieport 0000:00:01.1: AER: Corrected error message received from 0000:08:00.0
[  10.3] pcieport 0000:00:01.1: AER: Uncorrected (Non-Fatal) error received: 0000:08:00.0
[  10.4] pcieport 0000:00:01.1: PCIe Bus Error: severity=Corrected, type=Physical Layer
[  10.5] amdgpu 0000:03:00.0: amdgpu: ring gfx_0.0.0 timeout, signaled seq=10
[  10.6] i915 0000:00:02.0: [drm] GPU HANG: ecode 9:1:85dffffb
[  10.7] nouveau 0000:01:00.0: fifo: channel 3 killed!
[  10.8] audit: type=1400 apparmor=\"DENIED\" operation=\"symlink\"
[  10.9] nvidia 0000:08:00.0: vgaarb: deactivate vga console
";
        let e = scan_kernel_text(text, Some("0000:08:00.0"), None);
        let f = e.families.unwrap();
        assert_eq!(f.nvidia_xid, 1);
        assert_eq!(f.device_lost, 1);
        assert_eq!(f.pcie_aer_corrected, 2);
        assert_eq!(f.pcie_aer_uncorrected, 1);
        assert_eq!((f.amdgpu, f.intel_gpu, f.nouveau), (1, 1, 1));
        assert_eq!(e.lines_scanned, 9);
        assert_eq!(
            e.lines_mentioning_device,
            Some(3),
            "Xid line names the device without its function"
        );
        assert_eq!(e.sample_lines.len(), 7, "noise lines are not evidence");
        let clean = scan_kernel_text("-- No entries --\n[1.0] usb 1-1: new device\n", None, None);
        assert_eq!(
            clean.lines_scanned, 1,
            "journalctl markers are not kernel lines"
        );
        assert_eq!(clean.families.unwrap().total(), 0);
        assert_eq!(clean.lines_mentioning_device, None);
    }

    #[test]
    fn unreadable_log_is_unavailable_never_zero() {
        let e = KernelLogEvidence::unavailable("no journal");
        assert!(!e.available && e.families.is_none());
        let window = KernelLogWindow {
            cursor: None,
            buffer: String::new(),
            truncated: false,
        };
        assert!(window.finish(None, None).families.is_none());
    }

    fn telemetry(power: f64, temp: f64, mask: Option<u64>) -> GpuTelemetry {
        GpuTelemetry {
            pci_address: "0000:08:00.0".into(),
            utilization_percent: Some(50.0),
            memory_utilization_percent: None,
            vram_used_bytes: None,
            vram_total_bytes: None,
            temperature_celsius: Some(temp),
            power_watts: Some(power),
            core_clock_mhz: Some(1200.0),
            memory_clock_mhz: Some(3505.0),
            power_limit_watts: Some(120.0),
            throttle_reasons_mask: mask,
            source: "test".into(),
        }
    }

    #[test]
    fn stress_profile_tracks_peak_limit_share_and_throttle_bits() {
        let mut acc = StressAccumulator::default();
        assert!(acc.finish(1.0).power_watts.is_none());
        acc.observe(&telemetry(30.0, 40.0, Some(0x1)));
        acc.observe(&telemetry(90.0, 60.0, Some(0x1 | 0x4)));
        acc.observe(&telemetry(126.0, 65.0, Some(0x1)));
        let p = acc.finish(3.0);
        let power = p.power_watts.unwrap();
        assert!((power.mean - 82.0).abs() < 1e-9 && (power.max - 126.0).abs() < 1e-9);
        assert!((p.power_max_percent_of_limit.unwrap() - 105.0).abs() < 1e-9);
        assert_eq!(p.temperature_first_celsius, Some(40.0));
        assert_eq!(p.temperature_last_celsius, Some(65.0));
        assert_eq!(p.throttle_reasons_seen, ["GpuIdle", "SwPowerCap"]);
        assert_eq!(
            p.throttle_limited_samples,
            Some(1),
            "idle bit alone is not a limit"
        );
    }

    /// Each entry into a thermal or hardware slowdown is one episode; the power cap is not one.
    #[test]
    fn slowdown_episodes_count_entries_into_a_heat_or_hardware_slowdown() {
        let mut acc = StressAccumulator::default();
        for mask in [0x1, 0x4, 0x20, 0x20, 0x4, 0x40 | 0x4, 0x1, 0x8] {
            acc.observe(&telemetry(100.0, 80.0, Some(mask)));
        }
        let p = acc.finish(8.0);
        assert_eq!(p.slowdown_episodes, Some(3));
        assert!((p.slowdown_seconds.unwrap() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn missing_throttle_data_is_unavailable_not_no_throttling() {
        let mut acc = StressAccumulator::default();
        acc.observe(&telemetry(50.0, 50.0, None));
        let p = acc.finish(1.0);
        assert!(!p.throttle_reasons_available);
        assert_eq!(p.throttle_reasons_seen_mask, None);
        assert_eq!(p.throttle_limited_samples, None);
        assert_eq!(p.slowdown_episodes, None);
    }
}
