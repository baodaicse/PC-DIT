//! One pinned, prefaulted worker per selected CPU; inactive workers block on recv.
use super::{
    Arc, Condvar, Duration, HEAVY_ACCUMULATORS, Instant, Mutex, Outcome, PowerFault, PowerKernel,
    PowerReport, PowerVerdict, checkpoint, heartbeat, heavy_fp_mismatches, mpsc,
    pin_current_thread, run_burst, thread,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct Job {
    pub generation: u32,
    pub seed: u64,
    pub kernel: PowerKernel,
    pub iterations: u32,
    pub idle_ms: u64,
    pub burst_ms: u64,
    pub timeout_ms: u64,
    pub fault: Option<PowerFault>,
    pub first_cpu: usize,
    pub reference: [f64; 4],
    pub branch: u64,
    pub heavy_reference: [[f64; 4]; HEAVY_ACCUMULATORS],
    pub gate: Arc<(Mutex<bool>, Condvar)>,
}

enum Event {
    Ready {
        generation: u32,
        cpu: usize,
    },
    Done {
        generation: u32,
        outcome: Outcome,
    },
    Failed {
        generation: u32,
        cpu: usize,
        detail: String,
    },
}

pub(super) struct Pool {
    senders: BTreeMap<usize, mpsc::SyncSender<Arc<Job>>>,
    receiver: mpsc::Receiver<Event>,
    handles: Vec<thread::JoinHandle<()>>,
    cancelled: Arc<AtomicBool>,
    active_gate: Option<Arc<(Mutex<bool>, Condvar)>>,
    control: Arc<crate::safety::RunControl>,
}

impl Pool {
    #[cfg(test)]
    pub fn thread_ids(&self) -> Vec<thread::ThreadId> {
        self.handles
            .iter()
            .map(|handle| handle.thread().id())
            .collect()
    }
    pub fn new(cpus: &[usize], control: &Arc<crate::safety::RunControl>) -> Result<Self, String> {
        let (events, receiver) = mpsc::channel();
        let (initialized, initialization) = mpsc::channel();
        let mut pool = Self {
            senders: BTreeMap::new(),
            receiver,
            handles: vec![],
            cancelled: Arc::new(AtomicBool::new(false)),
            active_gate: None,
            control: Arc::clone(control),
        };
        for &cpu in cpus {
            let (sender, jobs) = mpsc::sync_channel::<Arc<Job>>(1);
            let events = events.clone();
            let initialized = initialized.clone();
            let cancelled = Arc::clone(&pool.cancelled);
            let control = Arc::clone(control);
            let handle = thread::Builder::new()
                .name(format!("power-cpu-{cpu}"))
                .spawn(move || {
                    let affinity_ok = pin_current_thread(cpu).is_ok();
                    let mut memory = vec![0_u64; 8192];
                    // Touch every word once before taking jobs; allocation is retained.
                    std::hint::black_box(&mut memory).fill(0);
                    if initialized.send(cpu).is_err() {
                        return;
                    }
                    while let Ok(job) = jobs.recv() {
                        if cancelled.load(Ordering::Acquire) || control.should_stop() {
                            break;
                        }
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            execute(
                                cpu,
                                affinity_ok,
                                &mut memory,
                                &job,
                                &events,
                                &cancelled,
                                &control,
                            )
                        }));
                        match result {
                            Ok(Ok(outcome)) => {
                                if events
                                    .send(Event::Done {
                                        generation: job.generation,
                                        outcome,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            error => {
                                let detail = match error {
                                    Ok(Err(detail)) => detail,
                                    _ => "worker panic".into(),
                                };
                                let _ = events.send(Event::Failed {
                                    generation: job.generation,
                                    cpu,
                                    detail,
                                });
                                break;
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            pool.handles.push(handle);
            pool.senders.insert(cpu, sender);
        }
        // No inactive worker may still be allocating/touching scratch when the
        // first isolation phase begins. One shared startup deadline, not per CPU.
        drop(initialized);
        let deadline = Instant::now() + Duration::from_secs(5);
        for _ in cpus {
            initialization
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|e| format!("pool initialization: {e}"))?;
        }
        Ok(pool)
    }

    pub fn dispatch(
        &mut self,
        cpus: &[usize],
        job: &Arc<Job>,
        report: &mut PowerReport,
    ) -> Result<(), String> {
        self.active_gate = Some(Arc::clone(&job.gate));
        for cpu in cpus {
            self.senders
                .get(cpu)
                .ok_or("CPU outside worker pool")?
                .try_send(Arc::clone(job))
                .map_err(|e| e.to_string())?;
            if let Some(counts) = &mut report.active_workers {
                counts.started += 1;
            }
        }
        Ok(())
    }

    pub fn collect(
        &mut self,
        cpus: &[usize],
        generation: u32,
        ready: bool,
        timeout_ms: u64,
        report: &mut PowerReport,
    ) -> Result<Vec<Outcome>, String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut seen = BTreeSet::new();
        let mut outcomes = Vec::new();
        while seen.len() < cpus.len() {
            if self.control.should_stop() {
                return Err("safety stop while waiting for workers".into());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("worker event timeout".into());
            }
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(250)))
            {
                Ok(event) => {
                    let (event_generation, cpu, outcome) = match event {
                        Event::Ready { generation, cpu } if ready => (generation, cpu, None),
                        Event::Done {
                            generation,
                            outcome,
                        } if !ready => (generation, outcome.cpu, Some(outcome)),
                        Event::Failed {
                            generation,
                            cpu,
                            detail,
                        } => {
                            return Err(format!("worker {cpu}, generation {generation}: {detail}"));
                        }
                        _ => return Err("unexpected worker phase".into()),
                    };
                    if event_generation != generation || !cpus.contains(&cpu) || !seen.insert(cpu) {
                        return Err(
                            "stale generation, unexpected CPU or duplicate participant".into()
                        );
                    }
                    if let Some(counts) = &mut report.active_workers {
                        if ready {
                            counts.ready += 1;
                        } else {
                            counts.completed += 1;
                        }
                    }
                    if let Some(outcome) = outcome {
                        // Preserve observed corruption even if another worker later times out.
                        if outcome.mismatches > 0 {
                            report.verdict = PowerVerdict::Fail;
                            checkpoint(report, "WORKER_MISMATCH")?;
                        }
                        outcomes.push(outcome);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => heartbeat(report)?,
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(outcomes)
    }

    pub fn idle(&self, duration_ms: u64, report: &PowerReport) -> Result<(), String> {
        let until = Instant::now() + Duration::from_millis(duration_ms);
        while Instant::now() < until {
            if self.control.should_stop() {
                return Err("safety stop during idle".into());
            }
            thread::sleep(
                until
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(100)),
            );
            heartbeat(report)?;
        }
        Ok(())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(gate) = &self.active_gate
            && let Ok(mut open) = gate.0.lock()
        {
            *open = true;
            gate.1.notify_all();
        }
        self.senders.clear();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn execute(
    cpu: usize,
    affinity_ok: bool,
    memory: &mut [u64],
    job: &Job,
    events: &mpsc::Sender<Event>,
    cancelled: &AtomicBool,
    control: &crate::safety::RunControl,
) -> Result<Outcome, String> {
    let first = cpu == job.first_cpu;
    if first && job.fault == Some(PowerFault::MissingParticipant) {
        return Err("injected missing participant".into());
    }
    assert!(
        !(first && job.fault == Some(PowerFault::WorkerPanic)),
        "injected worker panic"
    );
    for (index, word) in memory.iter_mut().enumerate() {
        *word = job.seed ^ index as u64;
    }
    let generation = if first && job.fault == Some(PowerFault::StaleGeneration) {
        job.generation.wrapping_sub(1)
    } else {
        job.generation
    };
    events
        .send(Event::Ready { generation, cpu })
        .map_err(|e| e.to_string())?;
    if first && job.fault == Some(PowerFault::DuplicateParticipant) {
        events
            .send(Event::Ready { generation, cpu })
            .map_err(|e| e.to_string())?;
    }
    let gate_deadline = Instant::now() + Duration::from_millis(job.idle_ms + job.timeout_ms + 5000);
    let mut open = job.gate.0.lock().map_err(|_| "gate poisoned")?;
    while !*open {
        if cancelled.load(Ordering::Acquire) || control.should_stop() {
            return Err("cancelled before release".into());
        }
        if Instant::now() >= gate_deadline {
            return Err("barrier release timeout".into());
        }
        (open, _) = job
            .gate
            .1
            .wait_timeout(open, Duration::from_millis(100))
            .map_err(|_| "gate poisoned")?;
    }
    drop(open);
    if first && job.fault == Some(PowerFault::WorkerTimeout) {
        let until = Instant::now() + Duration::from_millis(job.timeout_ms + 50);
        while Instant::now() < until && !cancelled.load(Ordering::Acquire) && !control.should_stop()
        {
            thread::sleep(Duration::from_millis(1));
        }
        return Err("injected completion timeout".into());
    }
    let start = Instant::now();
    let mut bursts = 0_u64;
    let mut mismatches = 0;
    if first && job.fault == Some(PowerFault::Mismatch) {
        memory[0] ^= 1;
    }
    loop {
        if (first && job.fault == Some(PowerFault::ZeroWork))
            || cancelled.load(Ordering::Acquire)
            || control.should_stop()
        {
            break;
        }
        if job.kernel == PowerKernel::HeavyFp {
            let fault =
                (first && bursts == 0 && job.fault == Some(PowerFault::HeavyMismatch)).then_some(0);
            mismatches +=
                heavy_fp_mismatches(job.seed, job.iterations, &job.heavy_reference, fault);
        } else {
            let result = run_burst(job.seed, job.iterations, &job.reference, job.branch);
            mismatches += u64::from(result.dense_fma_mismatch)
                + u64::from(result.branch_mismatch)
                + u64::from(result.aes_mismatch);
        }
        for (index, word) in std::hint::black_box(&mut *memory).iter_mut().enumerate() {
            mismatches += u64::from(*word != job.seed ^ index as u64 ^ bursts);
            *word = job.seed ^ index as u64 ^ (bursts + 1);
        }
        bursts += 1;
        if start.elapsed() >= Duration::from_millis(job.burst_ms) {
            break;
        }
    }
    for (index, word) in memory.iter().enumerate() {
        mismatches += u64::from(*word != job.seed ^ index as u64 ^ bursts);
    }
    Ok(Outcome {
        cpu,
        start,
        bursts,
        mismatches,
        affinity_ok: affinity_ok && job.fault != Some(PowerFault::AffinityFailure),
        burst_elapsed_ms: start.elapsed().as_millis(),
    })
}
