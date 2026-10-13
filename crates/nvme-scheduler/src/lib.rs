//! Concurrent per-device scheduler for DIT CPU-NVMe.

use dit_contracts::{DeviceId, DeviceReport, EventEnvelope, RunId, SCHEMA_VERSION, Verdict};
use nvme_engine::{EngineError, RunConfig, run};
use nvme_io::StorageBackend;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;

#[derive(Debug)]
pub struct DeviceJob<B> {
    pub backend: B,
    pub config: RunConfig,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceOutcome {
    pub device_id: DeviceId,
    pub report: Option<DeviceReport>,
    pub worker_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MultiDeviceReport {
    pub schema_version: u32,
    /// DIT-01 version that wrote this report (absent before v1.2: v1.1).
    #[serde(default)]
    pub dit_version: String,
    pub run_id: RunId,
    pub started_unix_ms: u64,
    pub elapsed_seconds: f64,
    pub aggregate_verdict: Verdict,
    pub selected_devices: usize,
    pub max_parallel_workers: usize,
    pub devices: Vec<DeviceOutcome>,
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("at least one device job is required")]
    Empty,
    #[error("device ID occurs more than once: {0}")]
    DuplicateDevice(DeviceId),
    #[error("all device jobs must share run ID {expected}; found {actual}")]
    MixedRunId { expected: RunId, actual: RunId },
    #[error("maximum parallel worker count must be greater than zero")]
    InvalidWorkerLimit,
}

/// Runs all device jobs concurrently behind a common start barrier.
///
/// Events from workers are serialized through the calling thread. A worker engine error or panic
/// becomes an `INVALID` device outcome and does not discard reports from other devices.
///
/// # Errors
///
/// Returns [`SchedulerError`] before starting when the job set is empty, contains duplicate device
/// IDs, or mixes run IDs.
pub fn run_multi<B, F>(
    jobs: Vec<DeviceJob<B>>,
    event_sink: F,
) -> Result<MultiDeviceReport, SchedulerError>
where
    B: StorageBackend + Send + 'static,
    F: FnMut(EventEnvelope),
{
    let worker_count = jobs.len().max(1);
    run_multi_with_limit(jobs, worker_count, event_sink)
}

/// Runs device jobs with a hard cap on simultaneous worker threads.
///
/// Jobs inside a batch share a start barrier. A limit of one provides sequential execution; a
/// larger limit bounds CPU pressure while retaining concurrent multi-device coverage.
///
/// # Errors
///
/// Returns [`SchedulerError`] for an invalid job set or a zero worker limit.
pub fn run_multi_with_limit<B, F>(
    jobs: Vec<DeviceJob<B>>,
    max_parallel_workers: usize,
    mut event_sink: F,
) -> Result<MultiDeviceReport, SchedulerError>
where
    B: StorageBackend + Send + 'static,
    F: FnMut(EventEnvelope),
{
    if max_parallel_workers == 0 {
        return Err(SchedulerError::InvalidWorkerLimit);
    }
    validate_jobs(&jobs)?;
    let run_id = jobs[0].config.run_id.clone();
    let started_unix_ms = unix_ms();
    let started = Instant::now();
    let selected_devices = jobs.len();
    let effective_workers = max_parallel_workers.min(selected_devices);
    let mut pending = jobs.into_iter().enumerate().collect::<VecDeque<_>>();
    let mut outcomes = Vec::with_capacity(selected_devices);
    while !pending.is_empty() {
        let batch = (0..effective_workers)
            .filter_map(|_| pending.pop_front())
            .collect::<Vec<_>>();
        run_batch(batch, &mut event_sink, &mut outcomes);
    }
    outcomes.sort_by_key(|(index, _)| *index);
    let devices = outcomes
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect::<Vec<_>>();
    let aggregate_verdict = aggregate_verdict(&devices);
    Ok(MultiDeviceReport {
        schema_version: SCHEMA_VERSION,
        dit_version: dit_contracts::DIT_VERSION.to_owned(),
        run_id,
        started_unix_ms,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        aggregate_verdict,
        selected_devices,
        max_parallel_workers: effective_workers,
        devices,
    })
}

fn run_batch<B, F>(
    jobs: Vec<(usize, DeviceJob<B>)>,
    event_sink: &mut F,
    outcomes: &mut Vec<(usize, DeviceOutcome)>,
) where
    B: StorageBackend + Send + 'static,
    F: FnMut(EventEnvelope),
{
    let barrier = Arc::new(Barrier::new(jobs.len() + 1));
    let (event_tx, event_rx) = mpsc::channel();
    let mut handles = Vec::with_capacity(jobs.len());

    for (index, job) in jobs {
        let device_id = job.config.device_id.clone();
        let worker_barrier = Arc::clone(&barrier);
        let worker_tx = event_tx.clone();
        let handle = thread::spawn(move || {
            let DeviceJob {
                mut backend,
                config,
            } = job;
            worker_barrier.wait();
            run(&mut backend, &config, |event| {
                let _ = worker_tx.send(event);
            })
        });
        handles.push((index, device_id, handle));
    }
    drop(event_tx);
    barrier.wait();
    for event in event_rx {
        event_sink(event);
    }

    for (index, device_id, handle) in handles {
        let outcome = match handle.join() {
            Ok(Ok(report)) => DeviceOutcome {
                device_id,
                report: Some(report),
                worker_error: None,
            },
            Ok(Err(error)) => invalid_outcome(device_id, &error),
            Err(_) => DeviceOutcome {
                device_id,
                report: None,
                worker_error: Some("worker panicked".to_owned()),
            },
        };
        outcomes.push((index, outcome));
    }
}

fn validate_jobs<B>(jobs: &[DeviceJob<B>]) -> Result<(), SchedulerError> {
    let Some(first) = jobs.first() else {
        return Err(SchedulerError::Empty);
    };
    let run_id = &first.config.run_id;
    let mut devices = BTreeSet::new();
    for job in jobs {
        if &job.config.run_id != run_id {
            return Err(SchedulerError::MixedRunId {
                expected: run_id.clone(),
                actual: job.config.run_id.clone(),
            });
        }
        if !devices.insert(job.config.device_id.clone()) {
            return Err(SchedulerError::DuplicateDevice(
                job.config.device_id.clone(),
            ));
        }
    }
    Ok(())
}

fn invalid_outcome(device_id: DeviceId, error: &EngineError) -> DeviceOutcome {
    DeviceOutcome {
        device_id,
        report: None,
        worker_error: Some(error.to_string()),
    }
}

#[must_use]
pub fn aggregate_verdict(outcomes: &[DeviceOutcome]) -> Verdict {
    if outcomes.is_empty() || outcomes.iter().any(|item| item.worker_error.is_some()) {
        return Verdict::Invalid;
    }
    let verdicts = outcomes
        .iter()
        .filter_map(|outcome| outcome.report.as_ref().map(|report| report.verdict))
        .collect::<Vec<_>>();
    if verdicts.len() != outcomes.len() || verdicts.contains(&Verdict::Invalid) {
        Verdict::Invalid
    } else if verdicts.contains(&Verdict::Fail) {
        Verdict::Fail
    } else if verdicts.contains(&Verdict::Aborted) {
        Verdict::Aborted
    } else if verdicts.iter().all(|verdict| *verdict == Verdict::Pass) {
        Verdict::Pass
    } else {
        Verdict::NotRun
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dit_contracts::Event;
    use nvme_backend_file::MemoryBackend;
    use nvme_engine::{CoveragePlan, FaultKind, FaultPlan};
    use std::time::Duration;

    fn job(run_id: &RunId, index: usize, fault: Option<FaultKind>) -> DeviceJob<MemoryBackend> {
        let device_id = DeviceId(format!("memory:nvme-{index}"));
        let mut config = RunConfig::new(
            run_id.clone(),
            device_id.clone(),
            CoveragePlan::for_duration(Duration::from_secs(60), 8, index as u64 + 1),
        );
        config.fault = fault.map(|kind| FaultPlan {
            kind,
            at_operation: 3,
        });
        DeviceJob {
            backend: MemoryBackend::new(device_id.0.clone(), 2 * 1024 * 1024),
            config,
        }
    }

    #[test]
    fn one_two_and_four_devices_pass_without_losing_events() {
        for count in [1, 2, 4] {
            let run_id = RunId(format!("multi-{count}"));
            let jobs = (0..count)
                .map(|index| job(&run_id, index, None))
                .collect::<Vec<_>>();
            let mut completed = BTreeSet::new();
            let report = run_multi(jobs, |event| {
                if matches!(event.event, Event::Completed { .. }) {
                    completed.insert(event.device_id.expect("device-scoped event"));
                }
            })
            .unwrap();
            assert_eq!(report.aggregate_verdict, Verdict::Pass);
            assert_eq!(report.devices.len(), count);
            assert_eq!(completed.len(), count);
            assert!(report.devices.iter().all(|outcome| {
                outcome
                    .report
                    .as_ref()
                    .is_some_and(|item| item.verdict == Verdict::Pass)
            }));
        }
    }

    #[test]
    fn one_failed_device_does_not_change_other_device_verdicts() {
        let run_id = RunId("multi-failure".to_owned());
        let report = run_multi(
            vec![
                job(&run_id, 0, None),
                job(&run_id, 1, Some(FaultKind::BitFlip)),
                job(&run_id, 2, None),
            ],
            |_| {},
        )
        .unwrap();
        assert_eq!(report.aggregate_verdict, Verdict::Fail);
        assert_eq!(
            report.devices[0].report.as_ref().unwrap().verdict,
            Verdict::Pass
        );
        assert_eq!(
            report.devices[1].report.as_ref().unwrap().verdict,
            Verdict::Fail
        );
        assert_eq!(
            report.devices[2].report.as_ref().unwrap().verdict,
            Verdict::Pass
        );
    }

    #[test]
    fn duplicate_device_and_mixed_run_are_rejected_before_start() {
        let run_id = RunId("multi-invalid".to_owned());
        let duplicate = run_multi(vec![job(&run_id, 0, None), job(&run_id, 0, None)], |_| {});
        assert!(matches!(duplicate, Err(SchedulerError::DuplicateDevice(_))));

        let mixed = run_multi(
            vec![
                job(&RunId("run-a".to_owned()), 0, None),
                job(&RunId("run-b".to_owned()), 1, None),
            ],
            |_| {},
        );
        assert!(matches!(mixed, Err(SchedulerError::MixedRunId { .. })));
    }

    #[test]
    fn report_schema_round_trips() {
        let run_id = RunId("multi-json".to_owned());
        let report = run_multi(vec![job(&run_id, 0, None)], |_| {}).unwrap();
        let encoded = serde_json::to_vec(&report).unwrap();
        let decoded: MultiDeviceReport = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.run_id, report.run_id);
        assert_eq!(decoded.aggregate_verdict, report.aggregate_verdict);
        assert_eq!(decoded.max_parallel_workers, 1);
        assert_eq!(decoded.devices.len(), 1);
    }

    #[test]
    fn worker_limit_supports_sequential_and_bounded_parallel_execution() {
        let run_id = RunId("multi-limited".to_owned());
        for limit in [1, 2] {
            let jobs = (0..4)
                .map(|index| job(&run_id, index, None))
                .collect::<Vec<_>>();
            let report = run_multi_with_limit(jobs, limit, |_| {}).unwrap();
            assert_eq!(report.aggregate_verdict, Verdict::Pass);
            assert_eq!(report.max_parallel_workers, limit);
            assert_eq!(report.devices.len(), 4);
        }
        let invalid = run_multi_with_limit(vec![job(&run_id, 0, None)], 0, |_| {});
        assert!(matches!(invalid, Err(SchedulerError::InvalidWorkerLimit)));
    }
}
