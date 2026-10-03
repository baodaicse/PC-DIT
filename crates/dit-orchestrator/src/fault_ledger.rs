//! Component fault ledger (`docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md`, step F0): every FAIL
//! of every layer is counted against one component cell (CPU, RAM, the `PCIe` link of a GPU, GPU,
//! VRAM, SSD, the read/write path of an SSD, platform) with how sure the attribution is, instead
//! of against the layer that noticed it. Corrected hardware events are counted apart and never
//! mixed into the FAIL counts. The data verdict itself stays with each engine's oracle.
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

/// Version of the cell set and of the mapping rules (`rule` in each event).
pub const ATTRIBUTION_VERSION: u32 = 1;
/// Events kept in full per cell; later ones in the same cell are counted only (error storms).
const DETAIL_CAP: usize = 64;

/// A component cell. GPUs and SSDs are numbered from 1 in the order the run lists them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Component {
    Cpu,
    Ram,
    /// The `PCIe` link of GPU n (both ends and what lies between).
    PcieGpu(u8),
    Gpu(u8),
    Vram(u8),
    Ssd(u8),
    /// The path to SSD n: controller interface, its `PCIe` link, DMA.
    SsdPath(u8),
    /// Power, heat, crashes, OOM: nothing a data check can place on one part.
    Platform,
}

impl Component {
    #[must_use]
    pub fn label(self) -> String {
        match self {
            Self::Cpu => "CPU".into(),
            Self::Ram => "RAM".into(),
            Self::PcieGpu(n) => format!("PCIe-GPU{n}"),
            Self::Gpu(n) => format!("GPU{n}"),
            Self::Vram(n) => format!("VRAM{n}"),
            Self::Ssd(n) => format!("SSD{n}"),
            Self::SsdPath(n) => format!("SSD{n}-path"),
            Self::Platform => "PLATFORM".into(),
        }
    }

    /// Column of the fleet CSV, device numbers folded together.
    #[must_use]
    pub const fn column(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Ram => "ram",
            Self::PcieGpu(_) => "pcie_gpu",
            Self::Gpu(_) => "gpu",
            Self::Vram(_) => "vram",
            Self::Ssd(_) => "ssd",
            Self::SsdPath(_) => "ssd_path",
            Self::Platform => "platform",
        }
    }

    /// Cells of a machine with `gpus` GPUs and `ssds` SSDs, in display order.
    #[must_use]
    pub fn machine(gpus: u8, ssds: u8) -> Vec<Self> {
        let mut cells = vec![Self::Cpu, Self::Ram];
        for n in 1..=gpus {
            cells.extend([Self::PcieGpu(n), Self::Gpu(n), Self::Vram(n)]);
        }
        for n in 1..=ssds {
            cells.extend([Self::Ssd(n), Self::SsdPath(n)]);
        }
        cells.push(Self::Platform);
        cells
    }
}

const COLUMNS: [&str; 8] = [
    "cpu", "ram", "pcie_gpu", "gpu", "vram", "ssd", "ssd_path", "platform",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A boundary check or a confirmation step places the fault in this cell.
    Confirmed,
    /// Ranked from indirect evidence; the event lists the other candidates.
    Suspected,
    /// More than one candidate and nothing to choose between them.
    Unresolved,
}

/// Where one failure points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attribution {
    Confirmed(Component),
    /// The most likely cell and the others still possible.
    Suspected(Component, Vec<Component>),
    Unresolved(Vec<Component>),
}

impl Attribution {
    fn normalized(self) -> Self {
        match self {
            Self::Unresolved(mut cells) => {
                cells.sort();
                cells.dedup();
                if cells.len() == 1 {
                    Self::Suspected(cells[0], Vec::new())
                } else {
                    Self::Unresolved(cells)
                }
            }
            other => other,
        }
    }

    fn confidence(&self) -> Confidence {
        match self {
            Self::Confirmed(_) => Confidence::Confirmed,
            Self::Suspected(..) => Confidence::Suspected,
            Self::Unresolved(_) => Confidence::Unresolved,
        }
    }

    fn candidates(&self) -> Vec<Component> {
        let mut cells = match self {
            Self::Confirmed(cell) => vec![*cell],
            Self::Suspected(cell, others) => std::iter::once(*cell).chain(others.clone()).collect(),
            Self::Unresolved(cells) => cells.clone(),
        };
        cells.sort();
        cells.dedup();
        cells
    }
}

fn joined(cells: &[Component]) -> String {
    cells
        .iter()
        .map(|cell| cell.label())
        .collect::<Vec<_>>()
        .join("|")
}

/// One failure (or a group of identical ones, `count`) and the cell it is counted in.
#[derive(Clone, Debug, Serialize)]
pub struct FaultEvent {
    /// `cpu-ram`, `gpu-vram`, `cpu-nvme`, `chain` or `session`.
    pub layer: &'static str,
    /// Combined phase label, empty for a single-module run.
    pub phase: String,
    /// What reported it, e.g. `gpu:compute` or `chain:ssd_read_path_transient`.
    pub source: String,
    pub count: u64,
    pub confidence: Confidence,
    /// The cell counted: a component label, or `A|B|C` when unresolved.
    pub cell: String,
    pub candidates: Vec<String>,
    /// The mapping rule applied (`<layer>/<name>`, versioned by [`ATTRIBUTION_VERSION`]).
    pub rule: &'static str,
    pub detail: String,
    /// Seconds since the session started, for failures seen live (step F1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_seconds: Option<f64>,
    /// Where the failure was (byte offset) and which bits differed, when the source says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub syndrome: Option<u64>,
    #[serde(skip)]
    attribution: Attribution,
}

impl FaultEvent {
    #[must_use]
    pub fn new(
        layer: &'static str,
        source: impl Into<String>,
        count: u64,
        attribution: Attribution,
        rule: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        let attribution = attribution.normalized();
        let cell = match &attribution {
            Attribution::Confirmed(cell) | Attribution::Suspected(cell, _) => cell.label(),
            Attribution::Unresolved(cells) => joined(cells),
        };
        Self {
            layer,
            phase: String::new(),
            source: source.into(),
            count: count.max(1),
            confidence: attribution.confidence(),
            cell,
            candidates: attribution.candidates().iter().map(|c| c.label()).collect(),
            rule,
            detail: detail.into(),
            at_seconds: None,
            address: None,
            syndrome: None,
            attribution,
        }
    }

    #[must_use]
    pub fn in_phase(mut self, phase: &str) -> Self {
        phase.clone_into(&mut self.phase);
        self
    }

    #[must_use]
    pub fn at(mut self, seconds: f64) -> Self {
        self.at_seconds = Some(seconds);
        self
    }

    /// Byte offset and XOR syndrome given by the source's own evidence.
    #[must_use]
    pub const fn located(mut self, address: Option<u64>, syndrome: Option<u64>) -> Self {
        self.address = address;
        self.syndrome = syndrome;
        self
    }

    #[must_use]
    pub fn attribution(&self) -> &Attribution {
        &self.attribution
    }
}

/// A corrected (or otherwise non-failing) hardware event counted on a cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HardwareEvent {
    pub cell: Component,
    pub kind: String,
    pub delta: u64,
}

impl HardwareEvent {
    #[must_use]
    pub fn new(cell: Component, kind: impl Into<String>, delta: u64) -> Self {
        Self {
            cell,
            kind: kind.into(),
            delta,
        }
    }
}

/// How often a device slowed itself down for heat (or a hardware signal) during a run, and for
/// how long when it says. Counted apart from failures, like hardware events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Throttle {
    pub events: u64,
    pub seconds: Option<u64>,
}

impl Throttle {
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            events: self.events.saturating_add(other.events),
            seconds: match (self.seconds, other.seconds) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (a, b) => a.or(b),
            },
        }
    }

    /// `3 (42 s)`, `3 (2 min 05 s)`, or the bare count without a time, as the live row shows it.
    fn shown(self) -> String {
        dit_ui_core::throttle_count(self.events, self.seconds)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ThrottleReport {
    pub cell: String,
    /// `false` when the device does not report throttling (no counters).
    pub available: bool,
    pub events: u64,
    pub seconds: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CellCount {
    pub confirmed: u64,
    pub suspected: u64,
}

impl CellCount {
    #[must_use]
    pub const fn total(self) -> u64 {
        self.confirmed + self.suspected
    }
}

/// Everything counted in one run or session.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    cells: BTreeMap<Component, CellCount>,
    unresolved: BTreeMap<Vec<Component>, u64>,
    hardware: BTreeMap<Component, BTreeMap<String, u64>>,
    /// Throttling per device; `None` for a device measured without throttle counters.
    throttle: BTreeMap<Component, Option<Throttle>>,
    by_phase: BTreeMap<String, BTreeMap<String, u64>>,
    events: Vec<FaultEvent>,
    kept_per_cell: BTreeMap<String, usize>,
    details_dropped: u64,
    occurrences: Vec<Occurrence>,
    rankings: Vec<Ranking>,
}

/// Every failure as the signature analysis (step F6) needs it, kept up to [`OCCURRENCE_CAP`].
#[derive(Clone, Debug)]
pub struct Occurrence {
    pub at_seconds: Option<f64>,
    pub layer: &'static str,
    pub phase: String,
    pub source: String,
    pub candidates: Vec<Component>,
    pub confidence: Confidence,
    pub count: u64,
    pub detail: String,
    pub address: Option<u64>,
    pub syndrome: Option<u64>,
}

const OCCURRENCE_CAP: usize = 8192;

/// An unresolved group moved to a suspected cell by the evidence ranking (step F6).
#[derive(Clone, Debug, Serialize)]
pub struct Ranking {
    pub weights_version: u32,
    pub group: String,
    pub suspected: String,
    pub count: u64,
    pub scores: BTreeMap<String, i64>,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CellReport {
    pub cell: String,
    pub confirmed: u64,
    pub suspected: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct HardwareReport {
    pub cell: String,
    pub kind: String,
    pub delta: u64,
}

/// The ledger as it is written into a report (`fault_ledger`).
#[derive(Clone, Debug, Serialize)]
pub struct LedgerReport {
    pub attribution_version: u32,
    pub failures_total: u64,
    pub confirmed_total: u64,
    pub suspected_total: u64,
    pub unresolved_total: u64,
    pub cells: Vec<CellReport>,
    pub unresolved: BTreeMap<String, u64>,
    pub hardware_events: Vec<HardwareReport>,
    pub throttle: Vec<ThrottleReport>,
    pub by_phase: BTreeMap<String, BTreeMap<String, u64>>,
    pub events: Vec<FaultEvent>,
    pub details_dropped: u64,
    /// Unresolved groups the evidence ranking (step F6) placed on a suspected cell.
    pub rankings: Vec<Ranking>,
}

impl Ledger {
    /// Unresolved groups and their counts.
    #[must_use]
    pub fn unresolved_groups(&self) -> Vec<(Vec<Component>, u64)> {
        self.unresolved
            .iter()
            .map(|(group, count)| (group.clone(), *count))
            .collect()
    }

    #[must_use]
    pub fn occurrences(&self) -> &[Occurrence] {
        &self.occurrences
    }

    /// Hardware counters counted on `cell`.
    #[must_use]
    pub fn hardware_of(&self, cell: Component) -> BTreeMap<String, u64> {
        self.hardware.get(&cell).cloned().unwrap_or_default()
    }

    /// Moves an unresolved group to a suspected cell (tier 2 of the attribution; it never makes
    /// anything confirmed).
    pub fn rank_unresolved(&mut self, group: &[Component], cell: Component, ranking: Ranking) {
        let Some(count) = self.unresolved.remove(group) else {
            return;
        };
        self.cells.entry(cell).or_default().suspected += count;
        self.rankings.push(Ranking { count, ..ranking });
    }

    pub fn add(&mut self, event: FaultEvent) {
        if self.occurrences.len() < OCCURRENCE_CAP {
            let (address, syndrome) = crate::fault_signatures::locate(&event.detail);
            self.occurrences.push(Occurrence {
                at_seconds: event.at_seconds,
                layer: event.layer,
                phase: event.phase.clone(),
                source: event.source.clone(),
                candidates: event.attribution().candidates(),
                confidence: event.confidence,
                count: event.count,
                detail: event.detail.clone(),
                address: event.address.or(address),
                syndrome: event.syndrome.or(syndrome),
            });
        }
        match event.attribution() {
            Attribution::Confirmed(cell) => {
                self.cells.entry(*cell).or_default().confirmed += event.count;
            }
            Attribution::Suspected(cell, _) => {
                self.cells.entry(*cell).or_default().suspected += event.count;
            }
            Attribution::Unresolved(cells) => {
                *self.unresolved.entry(cells.clone()).or_default() += event.count;
            }
        }
        let phase = if event.phase.is_empty() {
            "run".to_owned()
        } else {
            event.phase.clone()
        };
        *self
            .by_phase
            .entry(phase)
            .or_default()
            .entry(event.cell.clone())
            .or_default() += event.count;
        let kept = self.kept_per_cell.entry(event.cell.clone()).or_default();
        if *kept < DETAIL_CAP {
            *kept += 1;
            self.events.push(event);
        } else {
            self.details_dropped += 1;
        }
    }

    pub fn add_all(&mut self, events: impl IntoIterator<Item = FaultEvent>) {
        for event in events {
            self.add(event);
        }
    }

    pub fn add_hardware(&mut self, event: &HardwareEvent) {
        if event.delta > 0 {
            *self
                .hardware
                .entry(event.cell)
                .or_default()
                .entry(event.kind.clone())
                .or_default() += event.delta;
        }
    }

    /// Throttling of a device in one layer or phase: `None` when it has no throttle counters
    /// (it shows `N/A` until some reading of it is available).
    pub fn add_throttle(&mut self, cell: Component, reading: Option<Throttle>) {
        let entry = self.throttle.entry(cell).or_insert(None);
        if let Some(reading) = reading {
            *entry = Some(entry.map_or(reading, |known| known.plus(reading)));
        }
    }

    #[must_use]
    pub fn throttle_of(&self, cell: Component) -> Option<Option<Throttle>> {
        self.throttle.get(&cell).copied()
    }

    /// `THROTTLE` line: every device measured, as `FAIL BY PART` lists cells: how often it
    /// slowed itself down, `N/A` when it has no throttle counters.
    #[must_use]
    pub fn throttle_line(&self) -> String {
        if self.throttle.is_empty() {
            return "THROTTLE      none measured".to_owned();
        }
        let parts: Vec<String> = self
            .throttle
            .iter()
            .map(|(cell, reading)| {
                reading.map_or_else(
                    || format!("{} N/A", cell.label()),
                    |throttle| format!("{} {}", cell.label(), throttle.shown()),
                )
            })
            .collect();
        format!("THROTTLE      {}", parts.join(" | "))
    }

    #[must_use]
    pub fn cell(&self, cell: Component) -> CellCount {
        self.cells.get(&cell).copied().unwrap_or_default()
    }

    #[must_use]
    pub fn unresolved_total(&self) -> u64 {
        self.unresolved.values().sum()
    }

    #[must_use]
    pub fn failures_total(&self) -> u64 {
        self.cells.values().map(|c| c.total()).sum::<u64>() + self.unresolved_total()
    }

    fn column_total(&self, column: &str) -> u64 {
        self.cells
            .iter()
            .filter(|(cell, _)| cell.column() == column)
            .map(|(_, count)| count.total())
            .sum()
    }

    /// `FAIL BY PART` line: every cell of the machine (zeros included), then unresolved groups and
    /// failures of layers still running (`pending`, placed when the layer's report is read).
    #[must_use]
    pub fn fail_line(&self, machine: &[Component], pending: u64) -> String {
        let mut cells: Vec<Component> = machine.to_vec();
        cells.extend(self.cells.keys().filter(|cell| !machine.contains(cell)));
        let mut parts: Vec<String> = cells
            .iter()
            .map(|cell| {
                let count = self.cell(*cell);
                if count.suspected > 0 {
                    format!(
                        "{} {} ({} susp)",
                        cell.label(),
                        count.total(),
                        count.suspected
                    )
                } else {
                    format!("{} {}", cell.label(), count.total())
                }
            })
            .collect();
        for (group, count) in &self.unresolved {
            parts.push(format!("? {} {count}", joined(group)));
        }
        if pending > 0 {
            parts.push(format!("pending {pending}"));
        }
        format!("FAIL BY PART  {}", parts.join(" | "))
    }

    /// `HW EVENTS` line: non-zero corrected/hardware counters per cell.
    #[must_use]
    pub fn hardware_line(&self) -> String {
        let parts: Vec<String> = self
            .hardware
            .iter()
            .flat_map(|(cell, kinds)| {
                kinds
                    .iter()
                    .map(move |(kind, delta)| format!("{} {kind} +{delta}", cell.label()))
            })
            .collect();
        if parts.is_empty() {
            "HW EVENTS     none recorded".to_owned()
        } else {
            format!("HW EVENTS     {}", parts.join(" | "))
        }
    }

    #[must_use]
    pub fn report(&self) -> LedgerReport {
        let confirmed_total = self.cells.values().map(|c| c.confirmed).sum();
        let suspected_total = self.cells.values().map(|c| c.suspected).sum();
        LedgerReport {
            attribution_version: ATTRIBUTION_VERSION,
            failures_total: self.failures_total(),
            confirmed_total,
            suspected_total,
            unresolved_total: self.unresolved_total(),
            cells: self
                .cells
                .iter()
                .map(|(cell, count)| CellReport {
                    cell: cell.label(),
                    confirmed: count.confirmed,
                    suspected: count.suspected,
                })
                .collect(),
            unresolved: self
                .unresolved
                .iter()
                .map(|(group, count)| (joined(group), *count))
                .collect(),
            hardware_events: self
                .hardware
                .iter()
                .flat_map(|(cell, kinds)| {
                    kinds.iter().map(move |(kind, delta)| HardwareReport {
                        cell: cell.label(),
                        kind: kind.clone(),
                        delta: *delta,
                    })
                })
                .collect(),
            throttle: self
                .throttle
                .iter()
                .map(|(cell, reading)| ThrottleReport {
                    cell: cell.label(),
                    available: reading.is_some(),
                    events: reading.map_or(0, |throttle| throttle.events),
                    seconds: reading.and_then(|throttle| throttle.seconds),
                })
                .collect(),
            by_phase: self.by_phase.clone(),
            events: self.events.clone(),
            details_dropped: self.details_dropped,
            rankings: self.rankings.clone(),
        }
    }

    /// One `fail-summary.csv` row (columns in [`CSV_HEADER`]).
    #[must_use]
    pub fn csv_row(&self, meta: &CsvMeta) -> String {
        let report = self.report();
        let mut fields: Vec<String> = vec![
            meta.time.clone(),
            meta.image_id.clone(),
            meta.test.clone(),
            meta.board.clone(),
            meta.cpu.clone(),
            meta.memory.clone(),
            meta.gpus.clone(),
            meta.ssds.clone(),
            meta.verdict.clone(),
        ];
        fields.extend(COLUMNS.iter().map(|c| self.column_total(c).to_string()));
        fields.push(report.unresolved_total.to_string());
        fields.push(report.confirmed_total.to_string());
        fields.push(report.suspected_total.to_string());
        let mut cells = String::new();
        for cell in &report.cells {
            let _ = write!(
                cells,
                "{}={}(c{}/s{});",
                cell.cell,
                cell.confirmed + cell.suspected,
                cell.confirmed,
                cell.suspected
            );
        }
        for (group, count) in &report.unresolved {
            let _ = write!(cells, "?{group}={count};");
        }
        fields.push(cells.trim_end_matches(';').to_owned());
        // Throttling joins the hardware events column (the header stays the same).
        fields.push(
            report
                .hardware_events
                .iter()
                .map(|h| format!("{} {} +{}", h.cell, h.kind, h.delta))
                .chain(self.throttle.iter().filter_map(|(cell, reading)| {
                    reading
                        .filter(|throttle| throttle.events > 0)
                        .map(|throttle| format!("{} throttle +{}", cell.label(), throttle.shown()))
                }))
                .collect::<Vec<_>>()
                .join("; "),
        );
        fields.push(meta.report.clone());
        fields
            .iter()
            .map(|field| csv_field(field))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Header of `fail-summary.csv`.
pub const CSV_HEADER: &str = "time,image_id,test,board,cpu,memory,gpus,ssds,verdict,\
cpu_fail,ram_fail,pcie_gpu_fail,gpu_fail,vram_fail,ssd_fail,ssd_path_fail,platform_fail,\
unresolved_fail,confirmed,suspected,cells,hardware_events,report";

/// What a CSV row says about the run besides the counts.
#[derive(Clone, Debug, Default)]
pub struct CsvMeta {
    pub time: String,
    pub image_id: String,
    pub test: String,
    pub board: String,
    pub cpu: String,
    pub memory: String,
    pub gpus: String,
    pub ssds: String,
    pub verdict: String,
    pub report: String,
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// Appends one row to `path`, writing the header first when the file is new.
/// # Errors
/// File creation or write failures.
pub fn append_csv(path: &Path, ledger: &Ledger, meta: &CsvMeta) -> std::io::Result<()> {
    let new = !path.exists();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if new {
        writeln!(file, "{CSV_HEADER}")?;
    }
    writeln!(file, "{}", ledger.csv_row(meta))?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(attribution: Attribution, count: u64) -> FaultEvent {
        FaultEvent::new(
            "gpu-vram",
            "gpu:compute",
            count,
            attribution,
            "test/rule",
            "x",
        )
    }

    #[test]
    fn cells_count_by_confidence_and_unresolved_groups_stay_apart() {
        let mut ledger = Ledger::default();
        ledger.add(event(Attribution::Confirmed(Component::PcieGpu(1)), 8));
        ledger.add(event(
            Attribution::Suspected(Component::PcieGpu(1), vec![Component::Vram(1)]),
            4,
        ));
        ledger.add(event(
            Attribution::Unresolved(vec![
                Component::Vram(1),
                Component::PcieGpu(1),
                Component::Gpu(1),
            ]),
            3,
        ));
        // One candidate is not "unresolved".
        ledger.add(event(Attribution::Unresolved(vec![Component::Ram]), 1));
        assert_eq!(
            ledger.cell(Component::PcieGpu(1)),
            CellCount {
                confirmed: 8,
                suspected: 4
            }
        );
        assert_eq!(ledger.cell(Component::Ram).suspected, 1);
        assert_eq!(ledger.unresolved_total(), 3);
        assert_eq!(ledger.failures_total(), 16);
        let line = ledger.fail_line(&Component::machine(1, 1), 2);
        assert!(line.starts_with("FAIL BY PART  CPU 0 | RAM 1 (1 susp) | PCIe-GPU1 12 (4 susp) | GPU1 0 | VRAM1 0 | SSD1 0 | SSD1-path 0 | PLATFORM 0"), "{line}");
        assert!(line.contains("? PCIe-GPU1|GPU1|VRAM1 3"), "{line}");
        assert!(line.ends_with("pending 2"), "{line}");
        let report = ledger.report();
        assert_eq!(report.confirmed_total, 8);
        assert_eq!(report.suspected_total, 5);
        assert_eq!(report.by_phase["run"]["PCIe-GPU1"], 12);
    }

    #[test]
    fn a_storm_keeps_64_details_per_cell_but_every_count() {
        let mut ledger = Ledger::default();
        for _ in 0..1000 {
            ledger.add(event(Attribution::Confirmed(Component::Vram(1)), 1));
        }
        let report = ledger.report();
        assert_eq!(report.failures_total, 1000);
        assert_eq!(report.events.len(), 64);
        assert_eq!(report.details_dropped, 936);
    }

    /// Each device's throttling adds up over layers and phases; a device without counters shows
    /// N/A until a reading of it is available; the CSV carries the non-zero ones.
    #[test]
    fn throttle_is_counted_per_device() {
        let mut ledger = Ledger::default();
        assert_eq!(ledger.throttle_line(), "THROTTLE      none measured");
        ledger.add_throttle(Component::Cpu, None);
        ledger.add_throttle(
            Component::Gpu(1),
            Some(Throttle {
                events: 2,
                seconds: Some(30),
            }),
        );
        ledger.add_throttle(
            Component::Gpu(1),
            Some(Throttle {
                events: 1,
                seconds: Some(95),
            }),
        );
        ledger.add_throttle(
            Component::Ssd(1),
            Some(Throttle {
                events: 0,
                seconds: Some(0),
            }),
        );
        assert_eq!(
            ledger.throttle_line(),
            "THROTTLE      CPU N/A | GPU1 3 (2 min 05 s) | SSD1 0"
        );
        let row = ledger.csv_row(&CsvMeta::default());
        assert!(row.contains("GPU1 throttle +3 (2 min 05 s)"), "{row}");
        assert!(!row.contains("SSD1 throttle"));
        let report = ledger.report();
        assert!(!report.throttle[0].available);
        assert_eq!(report.throttle[1].events, 3);
    }

    #[test]
    fn hardware_events_are_counted_apart() {
        let mut ledger = Ledger::default();
        ledger.add_hardware(&HardwareEvent::new(
            Component::PcieGpu(1),
            "aer_corrected",
            56,
        ));
        ledger.add_hardware(&HardwareEvent::new(Component::Ram, "edac_ce", 0));
        assert_eq!(ledger.failures_total(), 0);
        assert_eq!(
            ledger.hardware_line(),
            "HW EVENTS     PCIe-GPU1 aer_corrected +56"
        );
    }

    #[test]
    fn csv_rows_fold_devices_and_quote_fields() {
        let mut ledger = Ledger::default();
        ledger.add(event(Attribution::Confirmed(Component::PcieGpu(1)), 5));
        ledger.add(event(Attribution::Confirmed(Component::PcieGpu(2)), 2));
        ledger.add(event(
            Attribution::Unresolved(vec![Component::Ssd(1), Component::SsdPath(1)]),
            1,
        ));
        let meta = CsvMeta {
            test: "COMBINED BOUNDED balanced".into(),
            gpus: "GPU A, rev 1".into(),
            verdict: "FAIL".into(),
            ..CsvMeta::default()
        };
        let row = ledger.csv_row(&meta);
        let columns = CSV_HEADER.split(',').count();
        assert!(row.contains("\"GPU A, rev 1\""), "{row}");
        assert!(row.contains(",0,0,7,0,0,0,0,0,1,7,0,"), "{row}");
        assert!(row.contains("?SSD1|SSD1-path=1"), "{row}");
        // Quoted commas aside, the row has one field per column.
        let unquoted = row.replace("\"GPU A, rev 1\"", "gpus");
        assert_eq!(unquoted.split(',').count(), columns, "{row}");
        let dir = std::env::temp_dir().join(format!("dit-ledger-csv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fail-summary.csv");
        append_csv(&path, &ledger, &meta).unwrap();
        append_csv(&path, &ledger, &meta).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "one header, two rows");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
