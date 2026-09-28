//! Persistent, bounded compute workers. At most two batch tickets may be in flight.
use super::{FaultKind, deep_block};
use dit_source::{BLOCK_SIZE, BlockContext};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Work {
    Prepare,
    Verify,
}

struct Job {
    ticket: u64,
    start: usize,
    end: usize,
    contexts: Arc<Vec<BlockContext>>,
    read_buffer: Arc<Vec<u8>>,
    work: Work,
    rounds: u32,
    fault: Option<FaultKind>,
}

#[derive(Default)]
pub struct BatchResult {
    pub buffer: Vec<u8>,
    pub verified_blocks: u64,
    pub matrix_checks: u64,
    pub cpu_ns: u64,
    pub jobs: u64,
    pub errors: Vec<(usize, String)>,
}

struct Chunk {
    ticket: u64,
    start: usize,
    result: BatchResult,
}

pub struct Ticket {
    id: u64,
    chunks: usize,
    blocks: usize,
    prepare: bool,
}

pub struct Pool {
    sender: Option<mpsc::SyncSender<Job>>,
    receiver: mpsc::Receiver<Chunk>,
    threads: Vec<JoinHandle<()>>,
    ready: BTreeMap<u64, Vec<Chunk>>,
    outstanding: usize,
    next_ticket: u64,
}

impl Pool {
    pub fn new(workers: usize) -> std::io::Result<Self> {
        let (sender, jobs) = mpsc::sync_channel::<Job>(workers * 2);
        let (results, receiver) = mpsc::channel();
        let jobs = Arc::new(Mutex::new(jobs));
        let mut pool = Self {
            sender: Some(sender),
            receiver,
            threads: Vec::new(),
            ready: BTreeMap::new(),
            outstanding: 0,
            next_ticket: 0,
        };
        for index in 0..workers {
            let queue = Arc::clone(&jobs);
            let output = results.clone();
            pool.threads.push(
                thread::Builder::new()
                    .name(format!("dit-deep-{index}"))
                    .spawn(move || {
                        loop {
                            let job = {
                                let Ok(receiver) = queue.lock() else {
                                    break;
                                };
                                let Ok(job) = receiver.recv() else {
                                    break;
                                };
                                job
                            };
                            let ticket = job.ticket;
                            let start = job.start;
                            let result =
                                std::panic::catch_unwind(|| execute(&job)).unwrap_or_else(|_| {
                                    BatchResult {
                                        errors: vec![(start, "compute worker panic".into())],
                                        ..BatchResult::default()
                                    }
                                });
                            if output
                                .send(Chunk {
                                    ticket,
                                    start,
                                    result,
                                })
                                .is_err()
                            {
                                break;
                            }
                        }
                    })?,
            );
        }
        Ok(pool)
    }

    pub fn prepare(
        &mut self,
        contexts: Vec<BlockContext>,
        rounds: u32,
        fault: Option<FaultKind>,
    ) -> Result<Ticket, String> {
        self.submit(contexts, Vec::new(), Work::Prepare, rounds, fault)
    }

    pub fn verify(
        &mut self,
        contexts: Vec<BlockContext>,
        buffer: Vec<u8>,
        rounds: u32,
        fault: Option<FaultKind>,
    ) -> Result<Ticket, String> {
        self.submit(contexts, buffer, Work::Verify, rounds, fault)
    }

    fn submit(
        &mut self,
        contexts: Vec<BlockContext>,
        buffer: Vec<u8>,
        work: Work,
        rounds: u32,
        fault: Option<FaultKind>,
    ) -> Result<Ticket, String> {
        if self.outstanding >= 2 || contexts.is_empty() {
            return Err("deep in-flight bound/empty batch".into());
        }
        let blocks = contexts.len();
        let count = blocks.min(self.threads.len());
        let chunk_size = blocks.div_ceil(count);
        let contexts = Arc::new(contexts);
        let read_buffer = Arc::new(buffer);
        let id = self.next_ticket;
        self.next_ticket += 1;
        let mut chunks = 0;
        for start in (0..blocks).step_by(chunk_size) {
            self.sender
                .as_ref()
                .ok_or("compute pool closed")?
                .send(Job {
                    ticket: id,
                    start,
                    end: (start + chunk_size).min(blocks),
                    contexts: Arc::clone(&contexts),
                    read_buffer: Arc::clone(&read_buffer),
                    work,
                    rounds,
                    fault,
                })
                .map_err(|_| "compute workers disconnected")?;
            chunks += 1;
        }
        self.outstanding += 1;
        Ok(Ticket {
            id,
            chunks,
            blocks,
            prepare: matches!(work, Work::Prepare),
        })
    }

    pub fn wait(
        &mut self,
        ticket: &Ticket,
        mut heartbeat: impl FnMut(),
    ) -> Result<BatchResult, String> {
        while self.ready.get(&ticket.id).map_or(0, Vec::len) < ticket.chunks {
            match self.receiver.recv_timeout(Duration::from_millis(125)) {
                Ok(chunk) => self.ready.entry(chunk.ticket).or_default().push(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => heartbeat(),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("compute workers disconnected".into());
                }
            }
        }
        let mut result = BatchResult::default();
        if ticket.prepare {
            result.buffer.resize(ticket.blocks * BLOCK_SIZE, 0);
        }
        for chunk in self
            .ready
            .remove(&ticket.id)
            .ok_or("missing compute result")?
        {
            if ticket.prepare && !chunk.result.buffer.is_empty() {
                let offset = chunk.start * BLOCK_SIZE;
                result.buffer[offset..offset + chunk.result.buffer.len()]
                    .copy_from_slice(&chunk.result.buffer);
            }
            result.verified_blocks += chunk.result.verified_blocks;
            result.matrix_checks += chunk.result.matrix_checks;
            result.cpu_ns = result.cpu_ns.saturating_add(chunk.result.cpu_ns);
            result.jobs += chunk.result.jobs;
            result.errors.extend(chunk.result.errors);
        }
        self.outstanding -= 1;
        Ok(result)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.sender.take();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn execute(job: &Job) -> BatchResult {
    let started = Instant::now();
    let mut result = BatchResult::default();
    for index in job.start..job.end {
        let fault = if index == 0 { job.fault } else { None };
        let checked = match job.work {
            Work::Prepare => deep_block::build(
                &job.contexts[index],
                job.rounds,
                fault == Some(FaultKind::ProducerCompute),
            )
            .map(|block| result.buffer.extend_from_slice(&block)),
            Work::Verify => deep_block::verify(
                &job.read_buffer[index * BLOCK_SIZE..(index + 1) * BLOCK_SIZE],
                &job.contexts[index],
                job.rounds,
                fault == Some(FaultKind::ReadCompute),
                fault == Some(FaultKind::ReferenceCompute),
                fault == Some(FaultKind::JoinMismatch),
            ),
        };
        match checked {
            Ok(()) => {
                result.matrix_checks += 1;
                if matches!(job.work, Work::Verify) {
                    result.verified_blocks += 1;
                }
            }
            Err(error) => {
                if matches!(job.work, Work::Prepare) {
                    result.buffer.resize(result.buffer.len() + BLOCK_SIZE, 0);
                }
                result.errors.push((index, error));
            }
        }
    }
    result.cpu_ns = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    result.jobs = 1;
    result
}
