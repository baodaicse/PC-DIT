//! Sequential campaign accounting. Host-confirmed queue-empty time is a lower
//! bound for DIT starvation, not physical GPU idle (other clients may run).
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReplayBatch {
    pub version: u32,
    pub phase: String,
    pub seed: u64,
    pub generation: u64,
    pub allocation: u64,
    pub offset: u64,
    pub bytes: u64,
    pub jobs: u64,
    pub rounds: u32,
}

impl ReplayBatch {
    /// Checks the versioned batch shape before any replay dispatch.
    ///
    /// # Errors
    /// Rejects unknown versions/phases, empty ranges and invalid matrix bounds.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 && self.version != 2 {
            return Err("unsupported replay version");
        }
        match self.phase.as_str() {
            "matrix"
                if (1..=65_536).contains(&self.jobs)
                    && (if self.version == 2 {
                        (1..=4096).contains(&self.rounds)
                    } else {
                        self.rounds <= 1_000_000
                    })
                    && self.bytes == 0
                    && self.offset == 0
                    && self.allocation == 0 =>
            {
                Ok(())
            }
            "transfer" | "retention"
                if (self.version == 1 || self.version == 2)
                    && self.bytes > 0
                    && self.offset.checked_add(self.bytes).is_some()
                    && self.jobs == 0
                    && self.rounds == 0 =>
            {
                Ok(())
            }
            _ => Err("invalid replay descriptor"),
        }
    }
    /// Canonical little-endian identity; independent of JSON formatting.
    #[must_use]
    pub fn identity(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"DIT-GPU-BATCH-1\0");
        hash.update(self.version.to_le_bytes());
        hash.update(self.phase.as_bytes());
        hash.update([0]);
        for n in [
            self.seed,
            self.generation,
            self.allocation,
            self.offset,
            self.bytes,
            self.jobs,
        ] {
            hash.update(n.to_le_bytes());
        }
        hash.update(self.rounds.to_le_bytes());
        format!("{:x}", hash.finalize())
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseTotals {
    pub attempts: u64,
    pub submitted: u64,
    pub completed: u64,
    pub verified: u64,
    pub failed: u64,
    pub host_seconds: f64,
    pub submit_wait_seconds: f64,
    pub prepare_seconds: f64,
    pub compare_seconds: f64,
    pub gpu_seconds: f64,
    pub gpu_samples: u64,
    pub gpu_h2d_seconds: f64,
    pub gpu_kernel_seconds: f64,
    pub gpu_d2h_seconds: f64,
    /// CPU time summed over verifier workers (may exceed wall time). Zero where
    /// comparison runs on the calling thread and is already in `compare_seconds`.
    pub compare_core_seconds: f64,
    /// Union of device intervals when batches overlap; zero for sequential phases.
    pub gpu_busy_wall_seconds: f64,
}

/// One asynchronous compute burst, folded into the `matrix` phase.
#[derive(Clone, Debug, Default)]
pub struct AsyncBurstAccounting {
    pub submitted: u64,
    pub completed: u64,
    pub verified: u64,
    pub failed: u64,
    pub prepare_seconds: f64,
    pub compare_core_seconds: f64,
    pub gpu_kernel_seconds: f64,
    pub gpu_d2h_seconds: f64,
    pub gpu_busy_wall_seconds: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct CampaignProfile {
    pub version: u32,
    pub transfer: PhaseTotals,
    pub retention: PhaseTotals,
    pub matrix: PhaseTotals,
    pub observed_seconds: f64,
    pub dit_queue_empty_lower_bound_seconds: f64,
    pub last_batch: Option<ReplayBatch>,
    pub last_batch_id: Option<String>,
    pub first_failed_batch: Option<ReplayBatch>,
    pub last_attempt_sequence: u64,
    #[serde(skip)]
    start: f64,
    #[serde(skip)]
    idle_since: Option<f64>,
    #[serde(skip)]
    submitted_at: Option<f64>,
}

impl Default for CampaignProfile {
    fn default() -> Self {
        Self {
            version: 1,
            transfer: PhaseTotals::default(),
            retention: PhaseTotals::default(),
            matrix: PhaseTotals::default(),
            observed_seconds: 0.0,
            dit_queue_empty_lower_bound_seconds: 0.0,
            last_batch: None,
            last_batch_id: None,
            first_failed_batch: None,
            last_attempt_sequence: 0,
            start: 0.0,
            idle_since: Some(0.0),
            submitted_at: None,
        }
    }
}

impl CampaignProfile {
    pub fn phase(&mut self) -> &mut PhaseTotals {
        match self.last_batch.as_ref().map(|b| b.phase.as_str()) {
            Some("transfer") => &mut self.transfer,
            Some("retention") => &mut self.retention,
            _ => &mut self.matrix,
        }
    }
    pub fn begin(&mut self, batch: ReplayBatch, now: f64) {
        self.last_attempt_sequence += 1;
        self.last_batch_id = Some(batch.identity());
        self.last_batch = Some(batch);
        self.start = now;
        self.phase().attempts += 1;
    }
    pub fn submitted(&mut self, now: f64) {
        if let Some(idle) = self.idle_since.take() {
            self.dit_queue_empty_lower_bound_seconds += (now - idle).max(0.0);
        }
        self.submitted_at = Some(now);
        self.phase().submitted += 1;
    }
    pub fn completion(&mut self, now: f64, success: bool) {
        if let Some(start) = self.submitted_at.take() {
            self.phase().submit_wait_seconds += (now - start).max(0.0);
            if success {
                self.phase().completed += 1;
                self.idle_since = Some(now);
            }
        }
    }
    pub fn finish(&mut self, now: f64, verified: bool) {
        if !verified && self.first_failed_batch.is_none() {
            self.first_failed_batch = self.last_batch.clone();
        }
        let elapsed = (now - self.start).max(0.0);
        let phase = self.phase();
        phase.host_seconds += elapsed;
        if verified {
            phase.verified += 1;
        } else {
            phase.failed += 1;
        }
    }
    /// Folds a finished burst into the `matrix` phase. A burst that submitted
    /// nothing is not GPU work: it changes no counter and does not end an idle gap.
    pub fn record_async_burst(
        &mut self,
        start: f64,
        end: f64,
        burst: &AsyncBurstAccounting,
        last: ReplayBatch,
        first_failed: Option<ReplayBatch>,
    ) {
        if burst.submitted == 0 {
            return;
        }
        if let Some(idle) = self.idle_since.take() {
            self.dit_queue_empty_lower_bound_seconds += (start - idle).max(0.0);
        }
        self.submitted_at = None;
        if burst.completed > 0 {
            self.idle_since = Some(end);
        }
        self.last_attempt_sequence += burst.submitted;
        self.last_batch_id = Some(last.identity());
        self.last_batch = Some(last);
        if burst.failed > 0 && self.first_failed_batch.is_none() {
            self.first_failed_batch = first_failed.or_else(|| self.last_batch.clone());
        }
        let wall = (end - start).max(0.0);
        let phase = &mut self.matrix;
        phase.attempts += burst.submitted;
        phase.submitted += burst.submitted;
        phase.completed += burst.completed;
        phase.verified += burst.verified;
        phase.failed += burst.failed;
        phase.host_seconds += wall;
        phase.submit_wait_seconds += wall;
        phase.prepare_seconds += burst.prepare_seconds;
        phase.compare_core_seconds += burst.compare_core_seconds;
        phase.gpu_samples += burst.completed;
        phase.gpu_kernel_seconds += burst.gpu_kernel_seconds;
        phase.gpu_d2h_seconds += burst.gpu_d2h_seconds;
        phase.gpu_busy_wall_seconds += burst.gpu_busy_wall_seconds;
        phase.gpu_seconds += burst.gpu_busy_wall_seconds;
    }
    #[must_use]
    pub fn snapshot(&self, now: f64) -> Self {
        let mut copy = self.clone();
        copy.observed_seconds = now;
        if let Some(idle) = self.idle_since {
            copy.dit_queue_empty_lower_bound_seconds += (now - idle).max(0.0);
        }
        copy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn batch(phase: &str) -> ReplayBatch {
        ReplayBatch {
            version: 1,
            phase: phase.into(),
            seed: 7,
            generation: 2,
            allocation: 0,
            offset: 64,
            bytes: 128,
            jobs: 1,
            rounds: 4,
        }
    }
    #[test]
    fn fake_clock_mismatch_timeout_and_tail_do_not_double_count() {
        let mut p = CampaignProfile::default();
        p.begin(batch("transfer"), 1.0);
        p.submitted(2.0);
        p.completion(4.0, true);
        p.finish(5.0, true);
        p.begin(batch("retention"), 6.0);
        p.submitted(7.0);
        p.completion(8.0, true);
        p.finish(9.0, false);
        assert_eq!(p.retention.verified, 0);
        assert_eq!(p.retention.completed, 1);
        assert!((p.snapshot(10.0).dit_queue_empty_lower_bound_seconds - 7.0).abs() < 1e-9);
        p.begin(batch("matrix"), 10.0);
        p.submitted(11.0);
        p.completion(15.0, false);
        p.finish(16.0, false);
        assert_eq!(p.matrix.submitted, 1);
        assert_eq!(p.matrix.completed, 0);
        assert!((p.snapshot(20.0).dit_queue_empty_lower_bound_seconds - 8.0).abs() < 1e-9);
        assert!((p.snapshot(30.0).dit_queue_empty_lower_bound_seconds - 8.0).abs() < 1e-9);
    }
    #[test]
    fn replay_is_stable_and_sensitive_to_phase_and_range() {
        let b = batch("transfer");
        assert_eq!(b.identity(), b.clone().identity());
        let mut other = b.clone();
        other.offset += 1;
        assert_ne!(b.identity(), other.identity());
        assert_ne!(b.identity(), batch("retention").identity());
        let restored: ReplayBatch =
            serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(b, restored);
    }

    #[test]
    fn reject_unknown_version_phase_overflow_and_empty_batch() {
        let mut b = batch("transfer");
        b.jobs = 0;
        b.rounds = 0;
        assert!(b.validate().is_ok());
        assert_eq!(
            b.identity(),
            "0e9548fafb1e4299bf3b09da1302efdd6a3cc3d82ddffe95a3aa7f4cdef8c40f"
        );
        // Version 2 (systematic pattern schedule) is valid for transfer and retention.
        b.version = 2;
        assert!(b.validate().is_ok());
        assert_ne!(b.identity(), batch("transfer").identity());
        b.version = 3;
        assert!(b.validate().is_err());
        b.version = 1;
        b.bytes = 0;
        assert!(b.validate().is_err());
        b.bytes = u64::MAX;
        assert!(b.validate().is_err());
        b.phase = "unknown".into();
        assert!(b.validate().is_err());
    }

    #[test]
    fn async_burst_fills_matrix_phase_and_empty_burst_changes_nothing() {
        let mut p = CampaignProfile::default();
        let burst = AsyncBurstAccounting {
            submitted: 32,
            completed: 32,
            verified: 31,
            failed: 1,
            compare_core_seconds: 0.8,
            gpu_busy_wall_seconds: 0.3,
            gpu_kernel_seconds: 0.4,
            ..AsyncBurstAccounting::default()
        };
        p.record_async_burst(2.0, 4.0, &burst, batch("matrix"), None);
        let m = p.matrix.clone();
        assert_eq!(
            (m.attempts, m.submitted, m.completed, m.verified, m.failed),
            (32, 32, 32, 31, 1)
        );
        assert!((m.host_seconds - 2.0).abs() < 1e-9);
        assert!((m.compare_core_seconds - 0.8).abs() < 1e-9);
        assert!(
            m.compare_seconds.abs() < f64::EPSILON,
            "owner thread does not compare"
        );
        assert!(p.first_failed_batch.is_some());
        p.record_async_burst(
            5.0,
            5.0,
            &AsyncBurstAccounting::default(),
            batch("matrix"),
            None,
        );
        assert_eq!(p.matrix.submitted, 32, "an empty burst is not GPU work");
        // Idle gap before the burst (0..2) counts; the empty burst must not extend it.
        assert!((p.snapshot(4.0).dit_queue_empty_lower_bound_seconds - 2.0).abs() < 1e-9);
        assert!((p.snapshot(9.0).dit_queue_empty_lower_bound_seconds - 7.0).abs() < 1e-9);
    }

    #[test]
    fn fake_clock_cancel_before_submit_does_not_create_gpu_work() {
        let mut p = CampaignProfile::default();
        p.begin(batch("matrix"), 1.0);
        p.finish(3.0, false);
        assert_eq!(p.matrix.submitted, 0);
        assert_eq!(p.matrix.completed, 0);
        assert_eq!(p.matrix.verified, 0);
        assert!((p.snapshot(5.0).dit_queue_empty_lower_bound_seconds - 5.0).abs() < 1e-9);
    }
}
