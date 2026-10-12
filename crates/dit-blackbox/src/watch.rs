//! The black box's decisions (`docs/FAULT_CATCH_ROADMAP.md`, `FC3a`): from what it sees of a
//! session each second, whether to wait, note something in the journal, or write the session's
//! result because its app no longer can. It never acts on the app.
//!
//! It writes a result only when the app cannot: the app is gone (10 s), its main loop stopped
//! beating (60 s, then 30 s more), or a layer stopped while the app did not record it in the 60 s
//! its watchdog and the app normally take. Each cause is INVALID unless the kernel gave hardware
//! evidence (an SSD that timed out, a crashed test process, a GPU off the bus): those are FAIL.
use crate::host::{Memory, Pressure, Process};
use crate::kernel_events::KernelEvent;
use crate::outcome::Cause;
use dit_orchestrator::session::{SessionInfo, SessionStatus};
use std::collections::{BTreeMap, BTreeSet};

/// The waits the black box gives the app, milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub app_gone: u64,
    pub app_silent: u64,
    pub app_silent_more: u64,
    /// Silence allowed while the app finishes (releasing storage, copying the report).
    pub app_finishing: u64,
    /// A layer with no progress this long is stalled (its watchdog's limit): SSD.
    pub ssd_stall: u64,
    /// Other layers.
    pub layer_stall: u64,
    /// The watchdog's grace before it ends a stalled worker.
    pub watchdog_grace: u64,
    /// What the app gets to record an event (a layer's end) on its own.
    pub app_records: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            app_gone: 10_000,
            app_silent: 60_000,
            app_silent_more: 30_000,
            app_finishing: 300_000,
            ssd_stall: 180_000,
            layer_stall: 90_000,
            watchdog_grace: 30_000,
            app_records: 60_000,
        }
    }
}

/// A test layer as the black box sees it now.
#[derive(Clone, Debug, Default)]
pub struct LayerSeen {
    pub name: String,
    /// Its watchdog's state (`RUNNING`, `STALLED`).
    pub watchdog_state: String,
    /// Last progress its watchdog saw (Unix ms; 0 before the first).
    pub last_activity_ms: u64,
    /// The worker's processes now.
    pub workers: Vec<Process>,
}

/// Everything seen of a session at one moment.
#[derive(Clone, Debug)]
pub struct Observation {
    pub now_ms: u64,
    pub app_alive: bool,
    pub status: SessionStatus,
    pub layers: Vec<LayerSeen>,
    pub memory: Memory,
    pub pressure: Option<Pressure>,
    /// Kernel events seen since the session started, with when (Unix ms).
    pub kernel: Vec<(u64, KernelEvent)>,
}

/// What to do now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Wait,
    /// Something for the journal only (a slow layer that still moves).
    Note(String),
    /// Write the session's result with this cause.
    Write(Cause),
}

#[derive(Debug, Default)]
struct LayerTrack {
    /// Worker processes ever seen (their kernel events count after they are gone).
    pids: BTreeSet<u32>,
    io_bytes: u64,
    cpu_ticks: u64,
    /// When the worker last did I/O or used CPU.
    moved_ms: u64,
    last_note_ms: u64,
}

/// What the black box remembers of a session between observations.
#[derive(Debug, Default)]
pub struct Tracker {
    gone_since: Option<u64>,
    silent_since: Option<u64>,
    layers: BTreeMap<String, LayerTrack>,
}

/// Memory so short that every program stalls: the livelock the field results pointed to.
#[must_use]
pub fn memory_starved(memory: Memory, pressure: Option<Pressure>) -> bool {
    let short = memory.total_bytes > 0 && memory.available_bytes < 256 * 1024 * 1024;
    let stalled = pressure.is_some_and(|p| p.full_avg10 >= 20.0 || p.some_avg10 >= 60.0);
    short || stalled
}

fn invalid(code: &str, text: String) -> Decision {
    Decision::Write(Cause {
        code: code.to_owned(),
        text,
        cells: Vec::new(),
        incomplete: false,
    })
}

fn fail(code: &str, text: String, cells: &[String]) -> Decision {
    Decision::Write(Cause {
        code: code.to_owned(),
        text,
        cells: cells.to_vec(),
        incomplete: false,
    })
}

/// The SSD number of an `NVMe` controller in this session (`nvme1` -> 2 when SSD 2 is `nvme1n1`).
fn ssd_number(info: &SessionInfo, controller: &str) -> u8 {
    info.ssd_blocks
        .iter()
        .find(|(_, block)| {
            block
                .strip_prefix(controller)
                .is_some_and(|rest| rest.starts_with('n'))
        })
        .map_or(1, |(alias, _)| *alias)
}

/// The GPU number of a PCI address an Xid names (`0000:01:00`).
fn gpu_number(info: &SessionInfo, pci: &str) -> u8 {
    info.gpu_pcis
        .iter()
        .position(|address| !pci.is_empty() && address.starts_with(pci))
        .and_then(|i| u8::try_from(i + 1).ok())
        .unwrap_or(1)
}

impl Tracker {
    /// One step: what the black box does about the session now.
    #[allow(clippy::too_many_lines)]
    pub fn step(&mut self, info: &SessionInfo, seen: &Observation, limits: &Limits) -> Decision {
        let now = seen.now_ms;
        let since = |start: &mut Option<u64>| now.saturating_sub(*start.get_or_insert(now));
        // The app is gone without its result.
        if seen.app_alive {
            self.gone_since = None;
        } else {
            if since(&mut self.gone_since) < limits.app_gone {
                return Decision::Wait;
            }
            let oom = seen.kernel.iter().any(|(_, event)| {
                matches!(event, KernelEvent::OomKill { pid, .. } if *pid == info.app_pid)
            });
            return if oom {
                invalid(
                    "oom",
                    "the kernel ended the app for memory (OOM) before it wrote its result".into(),
                )
            } else {
                invalid(
                    "app_killed",
                    "the app ended without writing its result".into(),
                )
            };
        }
        // The app's main loop stopped beating.
        let silent = now.saturating_sub(seen.status.heartbeat_unix_ms);
        let allowed = if seen.status.state == "FINISHING" {
            limits.app_finishing
        } else {
            limits.app_silent
        };
        if silent >= allowed {
            if since(&mut self.silent_since) < limits.app_silent_more {
                return Decision::Wait;
            }
            return if memory_starved(seen.memory, seen.pressure) {
                invalid(
                    "memory_livelock",
                    format!(
                        "the machine ran out of memory and every program stalled: {} MiB available, memory pressure full {:.0} % (10 s); the app stopped answering for {} s",
                        seen.memory.available_bytes / (1024 * 1024),
                        seen.pressure.map_or(0.0, |p| p.full_avg10),
                        silent / 1000
                    ),
                )
            } else {
                invalid(
                    "app_hang",
                    format!("the app stopped answering for {} s", silent / 1000),
                )
            };
        }
        self.silent_since = None;
        // A layer the app still lists as running.
        let mut note = None;
        for layer in &seen.layers {
            let track = self.layers.entry(layer.name.clone()).or_default();
            track.pids.extend(layer.workers.iter().map(|p| p.pid));
            let io: u64 = layer.workers.iter().filter_map(|p| p.io_bytes).sum();
            let cpu: u64 = layer.workers.iter().map(|p| p.cpu_ticks).sum();
            if io != track.io_bytes || cpu != track.cpu_ticks || track.moved_ms == 0 {
                track.io_bytes = io;
                track.cpu_ticks = cpu;
                track.moved_ms = now;
            }
            let pids = &track.pids;
            let after_app_had_time = |at: u64| now.saturating_sub(at) >= limits.app_records;
            // Kernel evidence about this layer's processes the app has not acted on.
            for (at, event) in &seen.kernel {
                if !after_app_had_time(*at) {
                    continue;
                }
                match event {
                    KernelEvent::OomKill { pid, name } if pids.contains(pid) => {
                        return invalid(
                            "oom",
                            format!(
                                "the kernel ended {} ({name}) for memory (OOM) and the app did not record it",
                                layer.name
                            ),
                        );
                    }
                    KernelEvent::Fault { pid, name } if pids.contains(pid) => {
                        return fail(
                            "worker_crash",
                            format!(
                                "{} crashed ({name}, {pid}) and the app did not record it",
                                layer.name
                            ),
                            &["PLATFORM".to_owned()],
                        );
                    }
                    KernelEvent::Xid { code: 79, pci } if layer.name == "gpu-vram" => {
                        let n = gpu_number(info, pci);
                        return fail(
                            "gpu_off_bus",
                            format!(
                                "GPU{n} fell off the bus (Xid 79) and the app did not record it"
                            ),
                            &[format!("PCIe-GPU{n}"), "PLATFORM".to_owned()],
                        );
                    }
                    _ => {}
                }
            }
            if layer.last_activity_ms == 0 {
                continue;
            }
            let stall = if layer.name == "cpu-nvme" {
                limits.ssd_stall
            } else {
                limits.layer_stall
            };
            let quiet = now.saturating_sub(layer.last_activity_ms);
            if quiet < stall + limits.watchdog_grace + limits.app_records {
                continue;
            }
            // Its watchdog could not end it, or the app did not take it off: decide here.
            let ssd = seen.kernel.iter().find_map(|(_, event)| {
                event
                    .nvme_controller()
                    .map(|controller| (ssd_number(info, &controller), event.describe()))
            });
            if let Some((n, said)) =
                ssd.filter(|_| layer.name == "cpu-nvme" || layer.name == "chain")
            {
                return fail(
                    "ssd_timeout",
                    format!(
                        "SSD{n} not responding for {} s; kernel: {said}",
                        quiet / 1000
                    ),
                    &[format!("SSD{n}"), format!("SSD{n}-path")],
                );
            }
            let moving = now.saturating_sub(track.moved_ms) < 10_000;
            if moving {
                if now.saturating_sub(track.last_note_ms) >= 60_000 {
                    track.last_note_ms = now;
                    note = Some(format!(
                        "slow_io: {} made no progress for {} s, but its processes still do I/O or use CPU",
                        layer.name,
                        quiet / 1000
                    ));
                }
                continue;
            }
            let waiting = layer.workers.iter().any(|p| p.state == 'D');
            return invalid(
                "layer_stall",
                format!(
                    "{} made no progress for {} s{} and the kernel reported no I/O error",
                    layer.name,
                    quiet / 1000,
                    if waiting {
                        ", stuck waiting in the kernel,"
                    } else {
                        ""
                    }
                ),
            );
        }
        note.map_or(Decision::Wait, Decision::Note)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dit_orchestrator::fault_ledger::CsvMeta;

    const S: u64 = 1000;

    fn info() -> SessionInfo {
        SessionInfo {
            schema_version: 1,
            run_id: "suite-1".to_owned(),
            csv: CsvMeta::default(),
            app_pid: 4000,
            started_unix_ms: 0,
            results_root: "/nonexistent".into(),
            pending_cells: Vec::new(),
            ssd_blocks: vec![(1, "nvme0n1".to_owned()), (2, "nvme1n1".to_owned())],
            gpu_pcis: vec!["0000:01:00.0".to_owned()],
        }
    }

    fn seen(now_s: u64, beat_s: u64) -> Observation {
        Observation {
            now_ms: now_s * S,
            app_alive: true,
            status: SessionStatus {
                heartbeat_unix_ms: beat_s * S,
                state: "RUNNING".to_owned(),
                ..SessionStatus::default()
            },
            layers: Vec::new(),
            memory: Memory {
                total_bytes: 64 << 30,
                available_bytes: 8 << 30,
            },
            pressure: None,
            kernel: Vec::new(),
        }
    }

    fn code(decision: &Decision) -> &str {
        match decision {
            Decision::Write(cause) => &cause.code,
            Decision::Note(_) => "note",
            Decision::Wait => "wait",
        }
    }

    #[test]
    fn a_gone_app_gets_ten_seconds_then_its_session_is_recorded() {
        let mut tracker = Tracker::default();
        let limits = Limits::default();
        let mut at = seen(100, 100);
        at.app_alive = false;
        assert_eq!(code(&tracker.step(&info(), &at, &limits)), "wait");
        at.now_ms = 109 * S;
        assert_eq!(code(&tracker.step(&info(), &at, &limits)), "wait");
        at.now_ms = 110 * S;
        assert_eq!(code(&tracker.step(&info(), &at, &limits)), "app_killed");
        // OOM killed: said so.
        let mut tracker = Tracker::default();
        at.kernel = vec![(
            100 * S,
            KernelEvent::OomKill {
                pid: 4000,
                name: "dit-suite".into(),
            },
        )];
        at.now_ms = 100 * S;
        let _ = tracker.step(&info(), &at, &limits);
        at.now_ms = 111 * S;
        assert_eq!(code(&tracker.step(&info(), &at, &limits)), "oom");
    }

    #[test]
    fn a_silent_app_gets_ninety_seconds_and_memory_decides_the_cause() {
        let limits = Limits::default();
        let mut tracker = Tracker::default();
        assert_eq!(
            code(&tracker.step(&info(), &seen(159, 100), &limits)),
            "wait"
        );
        assert_eq!(
            code(&tracker.step(&info(), &seen(160, 100), &limits)),
            "wait"
        );
        assert_eq!(
            code(&tracker.step(&info(), &seen(189, 100), &limits)),
            "wait"
        );
        assert_eq!(
            code(&tracker.step(&info(), &seen(190, 100), &limits)),
            "app_hang"
        );
        let mut tracker = Tracker::default();
        let mut starved = seen(160, 100);
        starved.pressure = Some(Pressure {
            some_avg10: 90.0,
            full_avg10: 55.0,
        });
        let _ = tracker.step(&info(), &starved, &limits);
        starved.now_ms = 190 * S;
        assert_eq!(
            code(&tracker.step(&info(), &starved, &limits)),
            "memory_livelock"
        );
        // Finishing (storage released, report copied) is allowed longer.
        let mut tracker = Tracker::default();
        let mut finishing = seen(200, 100);
        finishing.status.state = "FINISHING".to_owned();
        assert_eq!(code(&tracker.step(&info(), &finishing, &limits)), "wait");
        finishing.now_ms = 240 * S;
        assert_eq!(code(&tracker.step(&info(), &finishing, &limits)), "wait");
    }

    fn stuck_ssd(now_s: u64) -> Observation {
        let mut at = seen(now_s, now_s);
        at.layers = vec![LayerSeen {
            name: "cpu-nvme".to_owned(),
            watchdog_state: "STALLED".to_owned(),
            last_activity_ms: 1000 * S,
            workers: vec![Process {
                pid: 5000,
                state: 'D',
                io_bytes: Some(4096),
                cpu_ticks: 10,
                ..Process::default()
            }],
        }];
        at
    }

    #[test]
    fn a_stuck_ssd_is_a_fail_with_kernel_evidence_and_invalid_without() {
        let limits = Limits::default();
        // 180 s stall + 30 s grace + 60 s for the app = 270 s.
        let mut tracker = Tracker::default();
        let _ = tracker.step(&info(), &stuck_ssd(1200), &limits);
        assert_eq!(
            code(&tracker.step(&info(), &stuck_ssd(1269), &limits)),
            "wait"
        );
        let mut at = stuck_ssd(1270);
        at.kernel = vec![(
            1100 * S,
            KernelEvent::NvmeTimeout {
                controller: "nvme1".into(),
            },
        )];
        match tracker.step(&info(), &at, &limits) {
            Decision::Write(cause) => {
                assert_eq!(cause.code, "ssd_timeout");
                assert_eq!(cause.cells, vec!["SSD2".to_owned(), "SSD2-path".to_owned()]);
                assert!(
                    cause.text.starts_with("SSD2 not responding for 270 s"),
                    "{}",
                    cause.text
                );
            }
            other => panic!("{other:?}"),
        }
        let mut tracker = Tracker::default();
        let _ = tracker.step(&info(), &stuck_ssd(1200), &limits);
        assert_eq!(
            code(&tracker.step(&info(), &stuck_ssd(1270), &limits)),
            "layer_stall"
        );
    }

    #[test]
    fn a_slow_layer_that_still_moves_is_only_noted() {
        let limits = Limits::default();
        let mut tracker = Tracker::default();
        let mut at = stuck_ssd(1270);
        assert_eq!(code(&tracker.step(&info(), &at, &limits)), "note");
        at.now_ms = 1275 * S;
        at.layers[0].workers[0].io_bytes = Some(1 << 20);
        assert_eq!(code(&tracker.step(&info(), &at, &limits)), "wait");
    }

    #[test]
    fn a_crash_or_an_oom_the_app_does_not_record_is_written_after_its_minute() {
        let limits = Limits::default();
        let mut tracker = Tracker::default();
        let mut at = stuck_ssd(500);
        at.layers[0].last_activity_ms = 499 * S;
        let _ = tracker.step(&info(), &at, &limits);
        at.layers[0].workers.clear();
        at.kernel = vec![(
            500 * S,
            KernelEvent::Fault {
                pid: 5000,
                name: "cpu-nvme-dit".into(),
            },
        )];
        at.now_ms = 559 * S;
        at.status.heartbeat_unix_ms = 559 * S;
        assert_eq!(code(&tracker.step(&info(), &at, &limits)), "wait");
        at.now_ms = 560 * S;
        at.status.heartbeat_unix_ms = 560 * S;
        match tracker.step(&info(), &at, &limits) {
            Decision::Write(cause) => {
                assert_eq!(cause.code, "worker_crash");
                assert_eq!(cause.cells, vec!["PLATFORM".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
    }
}
