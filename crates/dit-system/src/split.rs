//! The split between RAM and compute workers, measured at the start of a session (v1.2, C2).
//!
//! A RAM worker is CPU-bound until the memory bus is full: past that point another RAM worker
//! only waits for the bus, while the same thread could load a core with the FMA kernel. The probe
//! verifies the session's own pattern in its own arenas with 2, 3, 4, 6... RAM workers, while
//! the remaining threads run the compute kernel exactly as they will during the test (a RAM
//! worker sharing a core with a compute worker is slower than one alone), and keeps the fewest
//! RAM workers that reach the bus. It measures the verify, not the write: a verifying worker is
//! slower (it generates and compares) and the read side of the bus is wider, so the verify needs
//! the most workers; the write fills its side with fewer. The compute side keeps at least a
//! quarter of the threads, as before v1.2.

use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use dit_core::ExpansionSource;
use serde::{Deserialize, Serialize};

use crate::{
    compute_oracle,
    cpu::{ReductionPair, pin_current_thread},
    memory::MemoryArena,
    safety::RunControl,
    window::{WindowExpected, WindowPlan},
};

/// Name of the probe recipe, recorded in reports.
pub(crate) const SPLIT_PROBE_ID: &str = "CPU-DIT-SPLIT-PROBE-v1";
/// RAM worker counts tried, smallest first (the largest allowed is always tried as well).
const CANDIDATES: [usize; 20] = [
    2, 3, 4, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128, 160, 192, 256,
];
/// Each step runs this long before its rate is measured (threads start, caches settle)...
const WARMUP: Duration = Duration::from_millis(100);
/// ...then is measured over this long.
const MEASURE: Duration = Duration::from_millis(300);
/// Bytes a probe worker writes between two progress counts.
const PIECE: usize = 1024 * 1024;
/// Expected bytes generated at a time by a verifying probe worker, as in the RAM verify.
const VERIFY_PIECE: usize = 4 * 1024;
/// A step gaining less than this over the one before has stopped paying: two such steps in a
/// row end the probe.
const MINIMUM_GAIN: f64 = 1.05;
/// The split keeps the fewest RAM workers whose rate is within this fraction of the best.
const KEEP_FRACTION: f64 = 0.95;
/// Arenas each probe RAM worker cycles through (larger than any cache).
const ARENAS_PER_WORKER: usize = 2;
/// Compute kernel iterations between two checks for the end of a step (well under 1 ms).
const BURN_ITERATIONS: u32 = 200_000;

/// One probe step: this many RAM workers wrote this fast while the other threads computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SplitStep {
    pub ram_workers: usize,
    pub bytes_per_second: u64,
}

/// How the threads were split between RAM and compute workers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSplitEvidence {
    /// `measured` (the probe chose the RAM workers) or `fixed` (given on the command line).
    pub mode: String,
    /// `CPU-DIT-SPLIT-PROBE-v1` when measured.
    pub probe_id: String,
    /// Threads shared between the two kinds of workers.
    pub pool_threads: usize,
    /// The compute side never gets fewer threads than this (a quarter of the pool).
    pub minimum_compute_workers: usize,
    /// The steps the probe ran, in order.
    pub steps: Vec<SplitStep>,
    pub ram_workers: usize,
    pub compute_workers: usize,
    pub probe_ms: u64,
}

impl WorkerSplitEvidence {
    pub(crate) fn fixed(ram_workers: usize, compute_workers: usize) -> Self {
        Self {
            mode: "fixed".to_owned(),
            probe_id: String::new(),
            pool_threads: ram_workers + compute_workers,
            minimum_compute_workers: 0,
            steps: Vec::new(),
            ram_workers,
            compute_workers,
            probe_ms: 0,
        }
    }
}

/// The compute side's floor: a quarter of the pool, at least one thread when there are two.
pub(crate) const fn minimum_compute(pool: usize) -> usize {
    if pool >= 2 {
        let quarter = pool / 4;
        if quarter == 0 { 1 } else { quarter }
    } else {
        0
    }
}

/// The RAM worker counts to try, up to `max_ram` (always tried itself).
fn candidates(max_ram: usize) -> Vec<usize> {
    let mut counts: Vec<usize> = CANDIDATES
        .iter()
        .copied()
        .filter(|&count| count < max_ram)
        .collect();
    if max_ram >= 1 {
        counts.push(max_ram);
    }
    counts
}

/// Whether the last steps show the bus is full: two steps in a row gaining under 5%.
fn saturated(steps: &[SplitStep]) -> bool {
    let gains: Vec<bool> = steps
        .windows(2)
        .map(|pair| rate(&pair[1]) >= rate(&pair[0]) * MINIMUM_GAIN)
        .collect();
    gains.len() >= 2 && !gains[gains.len() - 1] && !gains[gains.len() - 2]
}

/// The fewest RAM workers within [`KEEP_FRACTION`] of the best rate measured.
fn choose(steps: &[SplitStep]) -> Option<usize> {
    let best = steps.iter().map(rate).fold(0.0, f64::max);
    steps
        .iter()
        .filter(|step| rate(step) >= best * KEEP_FRACTION)
        .map(|step| step.ram_workers)
        .min()
}

#[allow(clippy::cast_precision_loss)] // Rates only compare with each other.
fn rate(step: &SplitStep) -> f64 {
    step.bytes_per_second as f64
}

/// The session's own RAM work, for the probe.
pub(crate) struct ProbeWork<'a> {
    pub(crate) plan: &'a WindowPlan,
    pub(crate) source: &'a ExpansionSource,
    pub(crate) seed: u64,
}

/// Measures how many RAM workers fill the memory bus. `order` is the pool in compute order
/// (`cpu::compute_order`): RAM workers take its back, compute workers its front. Returns the
/// evidence; `ram_workers` is the split to use.
pub(crate) fn measure(
    order: &[usize],
    arenas: &mut [MemoryArena],
    work: &ProbeWork<'_>,
    control: &RunControl,
) -> WorkerSplitEvidence {
    let started = Instant::now();
    let pool = order.len();
    let floor = minimum_compute(pool);
    let max_ram = pool.saturating_sub(floor).min(arenas.len()).max(1);
    let counts = candidates(max_ram);
    let mut steps = Vec::new();
    if counts.len() > 1 {
        let probe_arenas = arenas.len().min(max_ram * ARENAS_PER_WORKER);
        let arenas = &mut arenas[..probe_arenas];
        // Fault the probe's pages in first: a first write measures the kernel, not the bus.
        run_step(
            order,
            order.len().min(arenas.len()),
            arenas,
            work,
            control,
            None,
        );
        for &count in &counts {
            if control.should_stop() {
                break;
            }
            let bytes_per_second = run_step(order, count, arenas, work, control, Some(()));
            steps.push(SplitStep {
                ram_workers: count,
                bytes_per_second,
            });
            if saturated(&steps) {
                break;
            }
        }
    }
    let ram_workers = choose(&steps).unwrap_or(max_ram);
    WorkerSplitEvidence {
        mode: "measured".to_owned(),
        probe_id: SPLIT_PROBE_ID.to_owned(),
        pool_threads: pool,
        minimum_compute_workers: floor,
        steps,
        ram_workers,
        compute_workers: pool - ram_workers,
        probe_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

/// One step: `ram` RAM workers (from the back of `order`) verify their arenas over and over
/// while every other thread of `order` runs the compute kernel. Measured (`Some`): returns the
/// RAM workers' combined rate after [`WARMUP`] over [`MEASURE`]. Not measured: one pass writing
/// the arenas, no compute load (it faults the pages in and lays down what the steps verify).
fn run_step(
    order: &[usize],
    ram: usize,
    arenas: &mut [MemoryArena],
    work: &ProbeWork<'_>,
    control: &RunControl,
    measured: Option<()>,
) -> u64 {
    let ram = ram.clamp(1, arenas.len());
    let ram_cpus: Vec<usize> = order.iter().rev().take(ram).copied().collect();
    let compute_cpus: Vec<usize> = order
        .iter()
        .copied()
        .filter(|cpu| !ram_cpus.contains(cpu))
        .collect();
    let mut groups: Vec<Vec<&mut MemoryArena>> = (0..ram).map(|_| Vec::new()).collect();
    for (index, arena) in arenas.iter_mut().enumerate() {
        groups[index % ram].push(arena);
    }
    let written = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let once = measured.is_none();
    thread::scope(|scope| {
        let mut ram_threads = Vec::new();
        for (group, &cpu) in groups.into_iter().zip(&ram_cpus) {
            let (written, stop) = (&written, &stop);
            ram_threads.push(scope.spawn(move || {
                let _ = pin_current_thread(cpu);
                let mut group = group;
                if once {
                    for arena in &mut group {
                        write_arena(arena, work, control);
                    }
                    return;
                }
                let mut scratch = vec![0_u8; VERIFY_PIECE];
                while !stop.load(Ordering::Relaxed) && !control.should_stop() {
                    for arena in &group {
                        if !verify_arena(arena, work, &mut scratch, written, stop, control) {
                            break;
                        }
                    }
                }
            }));
        }
        if once {
            for handle in ram_threads {
                let _ = handle.join();
            }
            return;
        }
        for &cpu in &compute_cpus {
            let stop = &stop;
            scope.spawn(move || {
                let _ = pin_current_thread(cpu);
                while !stop.load(Ordering::Relaxed) && !control.should_stop() {
                    std::hint::black_box(compute_oracle::burn_fma(work.seed, BURN_ITERATIONS));
                }
            });
        }
        thread::sleep(WARMUP);
        let (first_bytes, first_time) = (written.load(Ordering::Relaxed), Instant::now());
        thread::sleep(MEASURE);
        let (last_bytes, last_time) = (written.load(Ordering::Relaxed), Instant::now());
        stop.store(true, Ordering::Relaxed);
        let seconds = last_time.duration_since(first_time).as_secs_f64();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )] // A byte rate: well within f64 and u64.
        let rate = ((last_bytes - first_bytes) as f64 / seconds) as u64;
        written.store(rate, Ordering::Relaxed);
    });
    if once {
        0
    } else {
        written.load(Ordering::Relaxed)
    }
}

/// The probe pattern, written once.
fn write_arena(arena: &mut MemoryArena, work: &ProbeWork<'_>, control: &RunControl) {
    for window in &work.plan.by_arena[arena.id] {
        let expected = WindowExpected::new(work.source, work.seed, window);
        let mut done = 0;
        while done < window.length && !control.should_stop() {
            let piece = PIECE.min(window.length - done);
            let start = window.arena_offset + done;
            expected.fill(done, &mut arena.map[start..start + piece]);
            done += piece;
        }
    }
}

/// The work of the RAM verify on one arena (expected bytes generated and compared, reductions
/// fed), counting the bytes into `verified`. A difference is not this probe's to report: the
/// rounds verify that memory again. False once stopped.
fn verify_arena(
    arena: &MemoryArena,
    work: &ProbeWork<'_>,
    scratch: &mut [u8],
    verified: &AtomicU64,
    stop: &AtomicBool,
    control: &RunControl,
) -> bool {
    let mut reductions = ReductionPair::new();
    let mut pending = 0_u64;
    for window in &work.plan.by_arena[arena.id] {
        let expected = WindowExpected::new(work.source, work.seed, window);
        let mut done = 0;
        while done < window.length {
            let piece = scratch.len().min(window.length - done);
            let start = window.arena_offset + done;
            expected.fill(done, &mut scratch[..piece]);
            let got = &arena.map[start..start + piece];
            std::hint::black_box(got == &scratch[..piece]);
            if piece == scratch.len() {
                reductions.update(got);
            }
            done += piece;
            pending += u64::try_from(piece).unwrap_or(u64::MAX);
            if pending >= PIECE as u64 {
                verified.fetch_add(pending, Ordering::Relaxed);
                pending = 0;
                if stop.load(Ordering::Relaxed) || control.should_stop() {
                    return false;
                }
            }
        }
    }
    verified.fetch_add(pending, Ordering::Relaxed);
    std::hint::black_box(reductions.scalar);
    true
}

#[cfg(test)]
mod tests {
    use super::{SplitStep, candidates, choose, minimum_compute, saturated};

    fn steps(rates: &[(usize, u64)]) -> Vec<SplitStep> {
        rates
            .iter()
            .map(|&(ram_workers, gb)| SplitStep {
                ram_workers,
                bytes_per_second: gb * 1_000_000_000,
            })
            .collect()
    }

    #[test]
    fn floor_and_candidates() {
        assert_eq!(minimum_compute(1), 0);
        assert_eq!(minimum_compute(2), 1);
        assert_eq!(minimum_compute(7), 1);
        assert_eq!(minimum_compute(32), 8);
        assert_eq!(candidates(24), [2, 3, 4, 6, 8, 10, 12, 16, 20, 24]);
        assert_eq!(candidates(9), [2, 3, 4, 6, 8, 9]);
        assert_eq!(candidates(1), [1]);
    }

    #[test]
    fn the_fewest_workers_that_fill_the_bus_are_kept() {
        // Rising until 12 workers, flat after: 12 is within 5% of the best (16).
        let measured = steps(&[
            (2, 8),
            (3, 12),
            (4, 16),
            (6, 24),
            (8, 31),
            (12, 40),
            (16, 41),
        ]);
        assert_eq!(choose(&measured), Some(12));
        // Never saturates: the largest count tried.
        let rising = steps(&[(2, 8), (3, 12), (4, 16), (6, 24)]);
        assert_eq!(choose(&rising), Some(6));
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn two_flat_steps_in_a_row_end_the_probe() {
        assert!(!saturated(&steps(&[(2, 8), (3, 12), (4, 16)])));
        assert!(!saturated(&steps(&[(8, 31), (12, 40), (16, 41)])));
        assert!(saturated(&steps(&[(8, 31), (12, 40), (16, 41), (20, 42)])));
        // One flat step between rising ones is noise, not saturation.
        assert!(!saturated(&steps(&[(8, 31), (12, 31), (16, 40)])));
    }
}
