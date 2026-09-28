//! GPU-H6 shared verifier queue: one fair queue for every worker, so compute output checks
//! and transfer-lane checks share the CPU by weight instead of by which channel they landed in,
//! and an idle worker always takes the next item (no fixed `slot % workers` assignment).
use gpu_engine::queue::FairQueue;
use serde::Serialize;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

pub(crate) const COMPUTE: usize = 0;
pub(crate) const LANE: usize = 1;

struct State<T> {
    queue: FairQueue<T>,
    closed: bool,
    compute_queued_peak: u64,
    lane_queued_peak: u64,
}

pub(crate) struct WorkQueue<T> {
    state: Mutex<State<T>>,
    ready: Condvar,
}

/// What the queue served, by class. Bytes are the cost the classes were charged.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Fairness {
    pub compute_items: u64,
    pub compute_bytes: u64,
    pub lane_items: u64,
    pub lane_bytes: u64,
    pub compute_queued_peak: u64,
    pub lane_queued_peak: u64,
}

impl Fairness {
    pub(crate) fn add(&mut self, other: &Fairness) {
        self.compute_items += other.compute_items;
        self.compute_bytes += other.compute_bytes;
        self.lane_items += other.lane_items;
        self.lane_bytes += other.lane_bytes;
        self.compute_queued_peak = self.compute_queued_peak.max(other.compute_queued_peak);
        self.lane_queued_peak = self.lane_queued_peak.max(other.lane_queued_peak);
    }
}

impl<T> WorkQueue<T> {
    pub(crate) fn new(compute_weight: u64, lane_weight: u64, quantum: u64) -> Self {
        Self {
            state: Mutex::new(State {
                queue: FairQueue::new(&[compute_weight, lane_weight], quantum),
                closed: false,
                compute_queued_peak: 0,
                lane_queued_peak: 0,
            }),
            ready: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn push(&self, class: usize, cost: u64, item: T) {
        let mut state = self.lock();
        state.queue.push(class, cost, item);
        let queued = state.queue.queued(class) as u64;
        let peak = if class == COMPUTE {
            &mut state.compute_queued_peak
        } else {
            &mut state.lane_queued_peak
        };
        *peak = (*peak).max(queued);
        drop(state);
        self.ready.notify_one();
    }

    /// No more work will arrive; workers drain what is queued and then stop.
    pub(crate) fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    /// The next item, waiting for one; `None` once the queue is closed and empty.
    pub(crate) fn pop(&self) -> Option<T> {
        let mut state = self.lock();
        loop {
            if let Some((_, item)) = state.queue.pop() {
                return Some(item);
            }
            if state.closed {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub(crate) fn stats(&self) -> Fairness {
        let state = self.lock();
        let (compute_items, compute_bytes) = state.queue.served(COMPUTE);
        let (lane_items, lane_bytes) = state.queue.served(LANE);
        Fairness {
            compute_items,
            compute_bytes,
            lane_items,
            lane_bytes,
            compute_queued_peak: state.compute_queued_peak,
            lane_queued_peak: state.lane_queued_peak,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn workers_drain_everything_then_stop_when_closed_and_both_classes_are_served() {
        let queue = Arc::new(WorkQueue::<u64>::new(3, 1, 100));
        for i in 0..400 {
            queue.push(COMPUTE, 100, i);
            queue.push(LANE, 100, i);
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let (queue, seen) = (Arc::clone(&queue), Arc::clone(&seen));
                std::thread::spawn(move || {
                    while let Some(item) = queue.pop() {
                        seen.lock().unwrap().push(item);
                    }
                })
            })
            .collect();
        queue.close();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(seen.lock().unwrap().len(), 800);
        let stats = queue.stats();
        assert_eq!((stats.compute_items, stats.lane_items), (400, 400));
        assert_eq!(stats.compute_bytes, 40_000);
        assert!(stats.compute_queued_peak >= 1 && stats.lane_queued_peak >= 1);
    }

    #[test]
    fn a_waiting_worker_wakes_for_work_that_arrives_later() {
        let queue = Arc::new(WorkQueue::<u32>::new(1, 1, 10));
        let worker = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || queue.pop())
        };
        std::thread::sleep(std::time::Duration::from_millis(30));
        queue.push(LANE, 5, 42);
        assert_eq!(worker.join().unwrap(), Some(42));
        queue.close();
        assert_eq!(queue.pop(), None);
    }
}
