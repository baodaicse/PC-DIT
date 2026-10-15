//! GPU-H1 cumulative accounting. Pure logic: no driver calls, so every identity
//! is testable without hardware. A run is a sequence of bursts; totals must be
//! summed across all of them and an empty burst must never erase earlier data.
//!
//! Naming contract: `*_core_seconds` add up CPU time across verifier workers and
//! may exceed wall time; `*_event_seconds` add up per-stream device event
//! intervals, which overlap when streams run concurrently; `*_wall_seconds`
//! are elapsed time on one clock.
use crate::balanced::Totals;
use serde::Serialize;

pub const MAX_MISMATCH_SAMPLES: usize = 16;

/// What limited a burst. A diagnostic label, never a pass/fail input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Regime {
    /// The GPU is the slowest stage: little verifier backlog and a busy device.
    GpuBound,
    /// Outputs arrive faster than verifiers can absorb them.
    VerifierBound,
    /// Copying the output takes longer than computing it.
    TransferBound,
    #[default]
    Balanced,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegimeInput {
    /// Mean verifier backlog as a fraction of the host queue during the burst.
    pub backlog_fill_mean: f64,
    /// The controller reduced load for a backlog reason during the burst, or was still holding
    /// the load down at its end (a well-paced verifier-bound run has a short queue).
    pub backlog_throttled: bool,
    pub device_busy_fraction: f64,
    /// Sum of D2H event intervals over sum of kernel event intervals.
    pub d2h_over_kernel: f64,
}

#[must_use]
pub fn classify_regime(i: &RegimeInput) -> Regime {
    if i.backlog_throttled || i.backlog_fill_mean >= 0.40 {
        Regime::VerifierBound
    } else if i.d2h_over_kernel >= 1.0 && i.device_busy_fraction >= 0.30 {
        Regime::TransferBound
    } else if i.backlog_fill_mean < 0.15 && i.device_busy_fraction >= 0.50 {
        Regime::GpuBound
    } else {
        Regime::Balanced
    }
}

/// Bursts per regime over a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RegimeCounts {
    pub gpu_bound: u64,
    pub verifier_bound: u64,
    pub transfer_bound: u64,
    pub balanced: u64,
}

impl RegimeCounts {
    fn add(&mut self, regime: Regime) {
        match regime {
            Regime::GpuBound => self.gpu_bound += 1,
            Regime::VerifierBound => self.verifier_bound += 1,
            Regime::TransferBound => self.transfer_bound += 1,
            Regime::Balanced => self.balanced += 1,
        }
    }
}
const MIB: f64 = 1_048_576.0;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MismatchSample {
    pub generation: u64,
    pub slot: usize,
    pub index: usize,
    pub expected: u32,
    pub actual: u32,
}

/// SHA-256 of the actual bytes of a failed batch, kept as evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FailureHash {
    pub generation: u64,
    pub actual_sha256: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Timing {
    /// CPU oracle time for cold corpora (single thread today).
    pub oracle_seconds: f64,
    pub input_generation_seconds: f64,
    /// Corpus build + upload wait as seen by the owner thread.
    pub corpus_prepare_wall_seconds: f64,
    pub verify_compare_core_seconds: f64,
    pub verify_hash_core_seconds: f64,
    /// Time from D2H completion being observed to a verifier starting on it.
    pub queue_wait_total_seconds: f64,
    pub queue_wait_max_seconds: f64,
    pub device_kernel_event_seconds: f64,
    pub device_d2h_event_seconds: f64,
    /// Union of per-batch device intervals: time the device had at least one batch.
    pub device_busy_wall_seconds: f64,
    pub burst_wall_seconds: f64,
    /// Time the owner waited for a prefetched corpus that was not ready yet.
    pub corpus_wait_seconds: f64,
    /// Wall time spent proving the host copy of the corpus unchanged.
    pub integrity_check_seconds: f64,
}

impl Timing {
    fn add(&mut self, o: &Timing) {
        self.oracle_seconds += o.oracle_seconds;
        self.input_generation_seconds += o.input_generation_seconds;
        self.corpus_prepare_wall_seconds += o.corpus_prepare_wall_seconds;
        self.verify_compare_core_seconds += o.verify_compare_core_seconds;
        self.verify_hash_core_seconds += o.verify_hash_core_seconds;
        self.queue_wait_total_seconds += o.queue_wait_total_seconds;
        self.queue_wait_max_seconds = self.queue_wait_max_seconds.max(o.queue_wait_max_seconds);
        self.device_kernel_event_seconds += o.device_kernel_event_seconds;
        self.device_d2h_event_seconds += o.device_d2h_event_seconds;
        self.device_busy_wall_seconds += o.device_busy_wall_seconds;
        self.burst_wall_seconds += o.burst_wall_seconds;
        self.corpus_wait_seconds += o.corpus_wait_seconds;
        self.integrity_check_seconds += o.integrity_check_seconds;
    }
    #[must_use]
    pub fn verify_core_seconds(&self) -> f64 {
        self.verify_compare_core_seconds + self.verify_hash_core_seconds
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct BurstSummary {
    pub totals: Totals,
    pub timing: Timing,
    pub mismatch_words: u64,
    pub mismatch_samples: Vec<MismatchSample>,
    pub failure_hashes: Vec<FailureHash>,
    /// Corpus events at the start of this burst.
    pub corpus_rotations: u64,
    pub corpus_prefetch_hits: u64,
    pub corpus_cold_builds: u64,
    /// SHA-256 computations of actual outputs (policy, last-batch and failure hashes).
    pub hashes_computed: u64,
    pub integrity_checks: u64,
    pub integrity_failures: u64,
    /// Batches still owned by the ledger when the burst ended (stalled/aborted).
    pub pending_at_end: u64,
    pub in_flight_limit_min: usize,
    pub in_flight_limit_max: usize,
    pub oldest_pending_ms_peak: u64,
    pub regime: Regime,
    pub backlog_fill_mean: f64,
    pub backlog_fill_peak: f64,
    /// Verifier workers used in this burst.
    pub workers: usize,
    /// High-water marks of the two slot pools during the burst.
    pub device_in_flight_peak: usize,
    pub host_slots_used_peak: usize,
}

impl BurstSummary {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.totals.submitted == 0
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Cumulative {
    /// Output cells verified per arithmetic (each is 2K operations: integer ops or FP32 flops).
    pub u32_results_verified: u64,
    pub f32_results_verified: u64,
    pub i8_results_verified: u64,
    /// Operations behind those cells: 2K each, K of the cell's own corpus (the adaptive shape
    /// changes K between corpora, [`crate::shape`]).
    pub u32_ops_verified: u64,
    pub f32_flops_verified: u64,
    pub i8_ops_verified: u64,
    /// K of the corpus in use.
    pub inner_now: u32,
    /// Steps the adaptive shape took (none when K was given or could not grow).
    pub shape_steps: Vec<crate::shape::ShapeStep>,
    pub bursts: u64,
    pub empty_bursts: u64,
    pub totals: Totals,
    pub timing: Timing,
    pub mismatch_words: u64,
    pub mismatch_samples: Vec<MismatchSample>,
    pub failure_hashes: Vec<FailureHash>,
    pub corpus_rotations: u64,
    pub corpus_prefetch_hits: u64,
    pub corpus_cold_builds: u64,
    pub hashes_computed: u64,
    pub integrity_checks: u64,
    pub integrity_failures: u64,
    /// Submitted batches that never reached a verdict (sum of `pending_at_end`).
    pub unfinished_batches: u64,
    pub in_flight_limit_min: Option<usize>,
    pub in_flight_limit_max: Option<usize>,
    pub oldest_pending_ms_peak: u64,
    pub device_in_flight_peak: usize,
    pub host_slots_used_peak: usize,
    pub regimes: RegimeCounts,
    pub last_regime: Option<Regime>,
    pub backlog_fill_peak: f64,
    pub workers_peak: usize,
    pub last_burst: Option<BurstSummary>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Rates {
    /// Completed D2H bytes over burst wall time: how fast the GPU side produces output.
    pub output_production_mib_s: Option<f64>,
    /// Verified bytes over worker core time: per-core verifier speed.
    pub verifier_mib_s_per_core: Option<f64>,
    pub verified_ops_per_second: Option<f64>,
    /// Device busy time over burst wall time; a diagnostic, not a target.
    pub device_busy_fraction: Option<f64>,
    /// `>1` means output arrives faster than one core can verify it.
    pub output_over_verifier_per_core: Option<f64>,
}

impl Cumulative {
    pub fn add_burst(&mut self, burst: BurstSummary) {
        self.bursts += 1;
        if burst.is_empty() {
            self.empty_bursts += 1;
        } else {
            self.in_flight_limit_min = Some(
                self.in_flight_limit_min
                    .map_or(burst.in_flight_limit_min, |v| {
                        v.min(burst.in_flight_limit_min)
                    }),
            );
            self.in_flight_limit_max = Some(
                self.in_flight_limit_max
                    .map_or(burst.in_flight_limit_max, |v| {
                        v.max(burst.in_flight_limit_max)
                    }),
            );
        }
        self.totals.accumulate(&burst.totals);
        self.timing.add(&burst.timing);
        self.mismatch_words += burst.mismatch_words;
        for sample in &burst.mismatch_samples {
            if self.mismatch_samples.len() < MAX_MISMATCH_SAMPLES {
                self.mismatch_samples.push(sample.clone());
            }
        }
        for hash in &burst.failure_hashes {
            if self.failure_hashes.len() < MAX_MISMATCH_SAMPLES {
                self.failure_hashes.push(hash.clone());
            }
        }
        self.corpus_rotations += burst.corpus_rotations;
        self.corpus_prefetch_hits += burst.corpus_prefetch_hits;
        self.corpus_cold_builds += burst.corpus_cold_builds;
        self.hashes_computed += burst.hashes_computed;
        self.integrity_checks += burst.integrity_checks;
        self.integrity_failures += burst.integrity_failures;
        self.unfinished_batches += burst.pending_at_end;
        self.oldest_pending_ms_peak = self
            .oldest_pending_ms_peak
            .max(burst.oldest_pending_ms_peak);
        self.device_in_flight_peak = self.device_in_flight_peak.max(burst.device_in_flight_peak);
        self.host_slots_used_peak = self.host_slots_used_peak.max(burst.host_slots_used_peak);
        if !burst.is_empty() {
            self.regimes.add(burst.regime);
            self.last_regime = Some(burst.regime);
        }
        self.backlog_fill_peak = self.backlog_fill_peak.max(burst.backlog_fill_peak);
        self.workers_peak = self.workers_peak.max(burst.workers);
        self.last_burst = Some(burst);
    }

    /// # Errors
    /// Names the violated identity when a batch was lost or counted twice.
    pub fn check_conservation(&self) -> Result<(), String> {
        let t = &self.totals;
        if t.submitted != t.verified + t.failed + self.unfinished_batches {
            return Err("submitted != verified + failed + unfinished".into());
        }
        if self.empty_bursts > self.bursts {
            return Err("empty bursts exceed bursts".into());
        }
        Ok(())
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn rates(&self) -> Rates {
        let per = |numerator: f64, denominator: f64| {
            (denominator > 0.0 && numerator.is_finite()).then(|| numerator / denominator)
        };
        let t = &self.timing;
        let output = per(
            self.totals.readback_bytes as f64 / MIB,
            t.burst_wall_seconds,
        );
        let verifier = per(
            self.totals.verified_bytes as f64 / MIB,
            t.verify_core_seconds(),
        );
        Rates {
            output_production_mib_s: output,
            verifier_mib_s_per_core: verifier,
            verified_ops_per_second: per(
                self.totals.verified_integer_ops as f64,
                t.burst_wall_seconds,
            ),
            device_busy_fraction: per(t.device_busy_wall_seconds, t.burst_wall_seconds),
            output_over_verifier_per_core: output.zip(verifier).and_then(|(o, v)| per(o, v)),
        }
    }
}

/// Length of the union of `[start, end]` intervals. Invalid intervals are ignored.
#[must_use]
pub fn interval_union(intervals: &mut [(f64, f64)]) -> f64 {
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut total = 0.0;
    let mut current: Option<(f64, f64)> = None;
    for &(start, end) in intervals.iter() {
        if !(start.is_finite() && end.is_finite()) || end < start {
            continue;
        }
        match current {
            Some((s, e)) if start <= e => current = Some((s, e.max(end))),
            Some((s, e)) => {
                total += e - s;
                current = Some((start, end));
            }
            None => current = Some((start, end)),
        }
    }
    if let Some((s, e)) = current {
        total += e - s;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::balanced::{Batch, Ledger};

    fn burst(submitted: u64, verified: u64, failed: u64, pending: u64) -> BurstSummary {
        BurstSummary {
            totals: Totals {
                submitted,
                completed: verified + failed,
                readback_ready: verified + failed,
                verified,
                failed,
                verified_bytes: verified * 100,
                failed_bytes: failed * 100,
                readback_bytes: (verified + failed) * 100,
                verified_integer_ops: verified * 1000,
                peak_queue_bytes: 300,
                ..Totals::default()
            },
            timing: Timing {
                burst_wall_seconds: 2.0,
                verify_compare_core_seconds: 1.0,
                verify_hash_core_seconds: 1.0,
                device_busy_wall_seconds: 1.0,
                queue_wait_max_seconds: 0.5,
                ..Timing::default()
            },
            mismatch_words: failed * 3,
            failure_hashes: (0..failed)
                .map(|g| FailureHash {
                    generation: g,
                    actual_sha256: "h".into(),
                })
                .collect(),
            corpus_rotations: 1,
            corpus_cold_builds: u64::from(submitted == 32 && failed == 1),
            hashes_computed: 2,
            integrity_checks: 1,
            integrity_failures: u64::from(failed > 0),
            pending_at_end: pending,
            in_flight_limit_min: 1,
            in_flight_limit_max: 3,
            oldest_pending_ms_peak: 40,
            ..BurstSummary::default()
        }
    }

    #[test]
    fn totals_accumulate_across_bursts_and_an_empty_burst_erases_nothing() {
        let mut c = Cumulative::default();
        c.add_burst(burst(32, 31, 1, 0));
        c.add_burst(burst(32, 32, 0, 0));
        c.add_burst(BurstSummary::default());
        assert_eq!((c.bursts, c.empty_bursts), (3, 1));
        assert_eq!(c.totals.submitted, 64);
        assert_eq!(c.totals.verified, 63);
        assert_eq!(c.totals.failed, 1);
        assert_eq!(c.totals.verified_bytes, 6300);
        assert_eq!(c.mismatch_words, 3);
        assert_eq!(c.in_flight_limit_max, Some(3));
        assert_eq!(
            (c.corpus_rotations, c.corpus_cold_builds, c.hashes_computed),
            (2, 1, 4)
        );
        assert_eq!((c.integrity_checks, c.integrity_failures), (2, 1));
        assert_eq!(
            c.failure_hashes.len(),
            1,
            "only the burst with a failed batch adds a hash"
        );
        // The report must still show the earlier work, not the empty final burst.
        assert!(c.last_burst.as_ref().unwrap().is_empty());
        assert!(c.check_conservation().is_ok());
    }

    #[test]
    fn stalled_burst_keeps_conservation_through_unfinished_batches() {
        let mut c = Cumulative::default();
        c.add_burst(burst(10, 6, 1, 3));
        assert_eq!(c.unfinished_batches, 3);
        assert!(c.check_conservation().is_ok());
        c.totals.verified += 1; // a duplicate verdict must be caught
        assert!(c.check_conservation().is_err());
    }

    #[test]
    fn rates_separate_core_time_from_wall_time_and_reject_zero_denominators() {
        let mut c = Cumulative::default();
        assert_eq!(c.rates(), Rates::default());
        c.add_burst(burst(32, 32, 0, 0));
        let r = c.rates();
        let bytes_mib = 3200.0 / MIB;
        assert!((r.output_production_mib_s.unwrap() - bytes_mib / 2.0).abs() < 1e-9);
        // 2 core-seconds for the same bytes: per-core speed is half the wall rate.
        assert!((r.verifier_mib_s_per_core.unwrap() - bytes_mib / 2.0).abs() < 1e-9);
        assert!((r.device_busy_fraction.unwrap() - 0.5).abs() < 1e-12);
        assert!((r.output_over_verifier_per_core.unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn interval_union_merges_overlap_and_ignores_bad_intervals() {
        let mut v = [
            (5.0, 6.0),
            (0.0, 2.0),
            (1.0, 3.0),
            (f64::NAN, 9.0),
            (8.0, 7.0),
        ];
        assert!((interval_union(&mut v) - 4.0).abs() < 1e-12);
        assert!(interval_union(&mut []).abs() < f64::EPSILON);
        let mut nested = [(0.0, 10.0), (2.0, 3.0)];
        assert!((interval_union(&mut nested) - 10.0).abs() < 1e-12);
    }

    fn batch(device_slot: usize, slot: usize) -> Batch {
        Batch {
            version: 1,
            device: "d".into(),
            corpus: "c".into(),
            seed: 1,
            generation: 0,
            jobs: 1,
            inner: 1,
            slot,
            epoch: 0,
            device_slot,
            device_epoch: 0,
        }
    }

    /// Deterministic random walk over every ledger transition of both slot pools,
    /// including illegal ones: rejected calls must not mutate counters and identities
    /// always hold. Slots wrap around thousands of times.
    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::similar_names)]
    fn conservation_holds_under_random_legal_and_illegal_transitions() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut ledger = Ledger::new(3, 7, 7 * 64, 2).unwrap();
        let mut live: Vec<Batch> = Vec::new();
        let mut held: Vec<Batch> = Vec::new();
        let mut finished: Vec<Batch> = Vec::new();
        for step in 0..30_000_u64 {
            let before = ledger.totals.clone();
            match next() % 8 {
                0 | 1 => {
                    if let Some((device, host)) = ledger.free_slots()
                        && let Ok(b) = ledger.reserve(batch(device, host), 64, step)
                    {
                        live.push(b);
                    }
                }
                2 if !live.is_empty() => {
                    let i = next() as usize % live.len();
                    let _ = ledger.copying(&live[i]);
                }
                3 if !live.is_empty() => {
                    let i = next() as usize % live.len();
                    let _ = ledger.ready(&live[i]);
                }
                4 if !live.is_empty() => {
                    let i = next() as usize % live.len();
                    let _ = ledger.verifying(&live[i]);
                }
                5 if !live.is_empty() => {
                    let i = next() as usize % live.len();
                    let ok = next() % 3 != 0;
                    if ledger.finish(&live[i], ok, 10, step).is_ok() {
                        let done = live.swap_remove(i);
                        if !ok && ledger.pending().iter().any(|p| p.batch == done) {
                            held.push(done);
                        } else {
                            finished.push(done);
                        }
                    }
                }
                6 => {
                    if !held.is_empty() && next() % 2 == 0 {
                        let i = next() as usize % held.len();
                        if ledger.release_quarantine(&held[i]).is_ok() {
                            finished.push(held.swap_remove(i));
                        }
                    } else {
                        for expired in ledger.expire_quarantine(step, 40) {
                            held.retain(|b| *b != expired);
                            finished.push(expired);
                        }
                    }
                }
                _ if !finished.is_empty() => {
                    // Replays of finished batches must always be rejected unchanged.
                    let i = next() as usize % finished.len();
                    let snapshot = ledger.totals.clone();
                    assert!(ledger.ready(&finished[i]).is_err());
                    assert!(ledger.copying(&finished[i]).is_err());
                    assert!(ledger.verifying(&finished[i]).is_err());
                    assert!(ledger.finish(&finished[i], true, 1, step).is_err());
                    assert!(ledger.release_quarantine(&finished[i]).is_err());
                    assert_eq!(snapshot.verified, ledger.totals.verified);
                    assert_eq!(snapshot.failed, ledger.totals.failed);
                }
                _ => {}
            }
            assert!(
                ledger.totals.submitted >= before.submitted
                    && ledger.totals.verified >= before.verified
                    && ledger.totals.failed >= before.failed
            );
            ledger.check_conservation().unwrap();
            assert!(ledger.device_in_flight() <= 3 && ledger.host_used() <= 7);
            assert!(ledger.quarantined_count() <= 2);
        }
        assert!(ledger.totals.submitted > 500 && ledger.totals.failed > 50);
        assert!(ledger.totals.quarantined > 10 && ledger.totals.quarantine_refused > 0);
        assert_eq!(ledger.peak_device_in_flight(), 3);
        assert!(
            ledger.peak_host_used() > 3,
            "the host queue must run deeper than the GPU"
        );
    }

    #[test]
    fn lost_completion_is_visible_as_pending_not_verified() {
        let mut ledger = Ledger::new(2, 3, 192, 1).unwrap();
        let a = ledger.reserve(batch(0, 0), 64, 0).unwrap();
        let b = ledger.reserve(batch(1, 1), 64, 0).unwrap();
        ledger.ready(&a).unwrap();
        ledger.verifying(&a).unwrap();
        ledger.finish(&a, true, 5, 1).unwrap();
        // `b` never completes: it stays pending and is never counted as verified.
        ledger.check_conservation().unwrap();
        assert_eq!(ledger.totals.verified, 1);
        assert_eq!(ledger.pending().len(), 1);
        assert!(ledger.verifying(&b).is_err());
        assert_eq!(
            ledger.device_in_flight(),
            1,
            "its device slot is still owned"
        );
    }

    #[test]
    fn regimes_follow_the_backlog_the_copy_share_and_the_device_load() {
        let base = RegimeInput {
            backlog_fill_mean: 0.05,
            backlog_throttled: false,
            device_busy_fraction: 0.8,
            d2h_over_kernel: 0.1,
        };
        assert_eq!(classify_regime(&base), Regime::GpuBound);
        assert_eq!(
            classify_regime(&RegimeInput {
                backlog_fill_mean: 0.55,
                ..base
            }),
            Regime::VerifierBound
        );
        assert_eq!(
            classify_regime(&RegimeInput {
                backlog_throttled: true,
                ..base
            }),
            Regime::VerifierBound,
            "a backlog throttle wins over an empty mean"
        );
        assert_eq!(
            classify_regime(&RegimeInput {
                d2h_over_kernel: 1.4,
                ..base
            }),
            Regime::TransferBound
        );
        // Idle device and idle verifiers: nothing is the bottleneck.
        assert_eq!(
            classify_regime(&RegimeInput {
                device_busy_fraction: 0.1,
                ..base
            }),
            Regime::Balanced
        );
        assert_eq!(
            classify_regime(&RegimeInput {
                backlog_fill_mean: 0.25,
                ..base
            }),
            Regime::Balanced
        );
    }

    #[test]
    fn regime_counts_ignore_empty_bursts_and_track_peaks() {
        let mut c = Cumulative::default();
        let mut b = burst(32, 32, 0, 0);
        b.regime = Regime::VerifierBound;
        b.backlog_fill_peak = 0.75;
        b.workers = 3;
        c.add_burst(b);
        c.add_burst(BurstSummary::default());
        assert_eq!(
            c.regimes,
            RegimeCounts {
                verifier_bound: 1,
                ..RegimeCounts::default()
            }
        );
        assert_eq!(c.last_regime, Some(Regime::VerifierBound));
        assert!((c.backlog_fill_peak - 0.75).abs() < 1e-12);
        assert_eq!(c.workers_peak, 3);
    }
}
