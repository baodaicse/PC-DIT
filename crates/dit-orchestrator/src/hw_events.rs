//! Live hardware monitor of a session (`docs/COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md`, step F2):
//! every few seconds, every watched link (each GPU's and each SSD's) is read at both ends and what
//! changed becomes a timed hardware event on that device's cell. The events are counted apart
//! from FAILs; their times let step F6 test whether data failures coincide with them.
use crate::fault_ledger::{Component, HardwareEvent};
use dit_system::pcie::{self, LinkState};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Sampling period: AER is cheap sysfs; the `nvidia-smi` replay query is not.
const INTERVAL: Duration = Duration::from_secs(5);
/// Timed events kept for correlation; totals keep counting past it.
const SERIES_CAP: usize = 4096;

/// A hardware event and when it was seen (seconds since the monitor started).
#[derive(Clone, Debug, PartialEq)]
pub struct TimedEvent {
    pub at_seconds: f64,
    pub cell: Component,
    pub kind: String,
    pub delta: u64,
}

struct Watched {
    cell: Component,
    first: LinkState,
    last: LinkState,
}

pub struct HwMonitor {
    watched: Vec<Watched>,
    started: Instant,
    last_sample: Option<Instant>,
    series: Vec<TimedEvent>,
    totals: BTreeMap<(Component, String), u64>,
    nvidia: bool,
}

impl HwMonitor {
    /// `links`: the cell of each link and the PCI function at its device end, e.g.
    /// `(PcieGpu(1), "0000:08:00.0")` or `(SsdPath(2), "0000:02:00.0")`.
    #[must_use]
    pub fn new(links: Vec<(Component, String)>) -> Self {
        let nvidia = links
            .iter()
            .any(|(cell, _)| matches!(cell, Component::PcieGpu(_)));
        let counters = if nvidia {
            pcie::nvidia_replay_counters()
        } else {
            BTreeMap::new()
        };
        let watched = links
            .into_iter()
            .map(|(cell, bdf)| {
                let state = pcie::snapshot(&bdf, &counters);
                Watched {
                    cell,
                    first: state.clone(),
                    last: state,
                }
            })
            .collect();
        Self {
            watched,
            started: Instant::now(),
            last_sample: Some(Instant::now()),
            series: Vec::new(),
            totals: BTreeMap::new(),
            nvidia,
        }
    }

    /// Reads every link when the interval has passed; returns what changed since the last read.
    pub fn sample(&mut self) -> Vec<HardwareEvent> {
        if self.watched.is_empty()
            || self
                .last_sample
                .is_some_and(|last| last.elapsed() < INTERVAL)
        {
            return Vec::new();
        }
        self.last_sample = Some(Instant::now());
        let counters = if self.nvidia {
            pcie::nvidia_replay_counters()
        } else {
            BTreeMap::new()
        };
        let at_seconds = self.started.elapsed().as_secs_f64();
        let mut changed = Vec::new();
        for watched in &mut self.watched {
            let now = pcie::snapshot(&watched.last.bdf, &counters);
            for (kind, delta) in pcie::delta(&watched.last, &now) {
                *self.totals.entry((watched.cell, kind.clone())).or_default() += delta;
                if self.series.len() < SERIES_CAP {
                    self.series.push(TimedEvent {
                        at_seconds,
                        cell: watched.cell,
                        kind: kind.clone(),
                        delta,
                    });
                }
                changed.push(HardwareEvent::new(watched.cell, kind, delta));
            }
            watched.last = now;
        }
        changed
    }

    /// Everything counted since the start, per cell and kind.
    #[must_use]
    pub fn totals(&self) -> Vec<HardwareEvent> {
        self.totals
            .iter()
            .map(|((cell, kind), delta)| HardwareEvent::new(*cell, kind.clone(), *delta))
            .collect()
    }

    /// Times (seconds since the start) of the hardware events seen on `cell`.
    #[must_use]
    pub fn times(&self, cell: Component) -> Vec<f64> {
        self.series
            .iter()
            .filter(|event| event.cell == cell)
            .map(|event| event.at_seconds)
            .collect()
    }

    #[must_use]
    pub fn elapsed_seconds(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// One line per watched link for the report: cell, both ends, width and speed, replay source.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        self.watched
            .iter()
            .map(|w| {
                let first = &w.first;
                format!(
                    "{} {} <- {} | x{} of x{} | {} of {} GT/s | replay {} | AER {}",
                    w.cell.label(),
                    first.bdf,
                    first.port.as_deref().unwrap_or("N/A"),
                    first
                        .link
                        .width
                        .map_or_else(|| "?".into(), |v| v.to_string()),
                    first
                        .link
                        .max_width
                        .map_or_else(|| "?".into(), |v| v.to_string()),
                    first
                        .link
                        .speed_gts
                        .map_or_else(|| "?".into(), |v| v.to_string()),
                    first
                        .link
                        .max_speed_gts
                        .map_or_else(|| "?".into(), |v| v.to_string()),
                    if first.replay.is_some() {
                        "readable"
                    } else {
                        "unavailable"
                    },
                    if first.aer.correctable.is_some() {
                        "readable"
                    } else {
                        "unavailable"
                    },
                )
            })
            .collect()
    }
}

/// Kinds a GPU report also carries (read once at its end); the live monitor counts them in a
/// session, so the report's copy is left out there.
#[must_use]
pub fn link_kind(kind: &str) -> bool {
    kind.starts_with("aer_") || kind.starts_with("link_") || kind.starts_with("replay")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_monitor_without_links_never_samples() {
        let mut monitor = HwMonitor::new(Vec::new());
        assert!(monitor.sample().is_empty());
        assert!(monitor.totals().is_empty());
        assert!(monitor.describe().is_empty());
        assert!(link_kind("aer_corrected:RxErr@port"));
        assert!(link_kind("replay"));
        assert!(!link_kind("ecc_corrected"));
    }

    #[test]
    fn a_real_link_can_be_watched() {
        // Any PCI function of this machine: the reading must not fail, even without AER.
        let Some(bdf) = std::fs::read_dir("/sys/bus/pci/devices")
            .ok()
            .and_then(|mut entries| entries.next())
            .and_then(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
        else {
            return;
        };
        let mut monitor = HwMonitor::new(vec![(Component::SsdPath(1), bdf)]);
        monitor.last_sample = None;
        let _ = monitor.sample();
        assert_eq!(monitor.describe().len(), 1);
    }
}
