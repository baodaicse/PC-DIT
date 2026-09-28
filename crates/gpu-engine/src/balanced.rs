//! GPU-H bounded ownership ledger and deterministic feedback controller.
//!
//! Two slot pools with separate lifecycles (GPU-H3):
//! - device slot: `FREE -> GPU_WRITING -> COPYING -> FREE`, freed when the D2H event completes;
//! - host slot: `FREE -> DMA_WRITING -> READY -> VERIFYING -> FREE`, freed by the CPU verdict.
//!
//! A batch reserves both before submission. Because the device slot is released as soon as
//! the copy lands, the GPU can start the next batch while earlier ones wait for a verifier,
//! and the host queue may be deeper than the number of batches on the GPU. A failed batch
//! keeps its host slot in `QUARANTINE`, bounded in count and age, so its bytes stay intact
//! for confirmation instead of being overwritten by the next batch.
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Stage {
    /// Kernel queued or running; the host slot is reserved as the DMA target.
    GpuWriting,
    /// Kernel finished, D2H copy in flight.
    Copying,
    /// D2H acknowledged; the CPU may read the host slot.
    Ready,
    Verifying,
    /// Failed batch held (bounded) so its bytes survive for confirmation.
    Quarantined,
}

impl Stage {
    #[must_use]
    pub fn on_device(self) -> bool {
        matches!(self, Self::GpuWriting | Self::Copying)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Batch {
    pub version: u32,
    pub device: String,
    pub corpus: String,
    pub seed: u64,
    pub generation: u64,
    pub jobs: usize,
    pub inner: u32,
    /// Host slot (pinned readback) and its epoch.
    pub slot: usize,
    pub epoch: u64,
    pub device_slot: usize,
    pub device_epoch: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Pending {
    pub batch: Batch,
    pub stage: Stage,
    pub bytes: u64,
    pub submitted_ms: u64,
    pub quarantined_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Totals {
    pub submitted: u64,
    pub completed: u64,
    pub readback_ready: u64,
    pub verified: u64,
    pub failed: u64,
    pub verified_bytes: u64,
    pub verified_integer_ops: u64,
    /// Bytes whose D2H completion was observed, verified or not.
    pub readback_bytes: u64,
    pub failed_bytes: u64,
    pub peak_queue_bytes: u64,
    /// Failed batches whose host slot was held, and how each hold ended.
    pub quarantined: u64,
    pub quarantine_released: u64,
    pub quarantine_expired: u64,
    /// Failed batches that could not be held because the quarantine was full.
    pub quarantine_refused: u64,
    pub first_failure: Option<Batch>,
}

impl Totals {
    /// Adds a finished burst. Peaks take the maximum; the earliest failure is kept.
    pub fn accumulate(&mut self, other: &Totals) {
        self.submitted += other.submitted;
        self.completed += other.completed;
        self.readback_ready += other.readback_ready;
        self.verified += other.verified;
        self.failed += other.failed;
        self.verified_bytes += other.verified_bytes;
        self.verified_integer_ops += other.verified_integer_ops;
        self.readback_bytes += other.readback_bytes;
        self.failed_bytes += other.failed_bytes;
        self.quarantined += other.quarantined;
        self.quarantine_released += other.quarantine_released;
        self.quarantine_expired += other.quarantine_expired;
        self.quarantine_refused += other.quarantine_refused;
        self.peak_queue_bytes = self.peak_queue_bytes.max(other.peak_queue_bytes);
        if self.first_failure.is_none() {
            self.first_failure.clone_from(&other.first_failure);
        }
    }
}

pub struct Ledger {
    host: Vec<Option<Pending>>,
    host_epochs: Vec<u64>,
    /// Host slot currently written by the batch that owns each device slot.
    device: Vec<Option<usize>>,
    device_epochs: Vec<u64>,
    cap: u64,
    used: u64,
    quarantine_slots: usize,
    peak_device_in_flight: usize,
    peak_host_used: usize,
    pub totals: Totals,
}
impl Ledger {
    /// # Errors
    /// Rejects slot counts outside 1..=16 device / device..=64 host, an empty byte cap, or a
    /// quarantine that could hold every host slot.
    pub fn new(
        device_slots: usize,
        host_slots: usize,
        bytes: u64,
        quarantine_slots: usize,
    ) -> Result<Self, &'static str> {
        if !(1..=16).contains(&device_slots)
            || !(2..=64).contains(&host_slots)
            || host_slots < device_slots
            || bytes == 0
            || quarantine_slots >= host_slots
        {
            return Err("invalid queue capacity");
        }
        Ok(Self {
            host: vec![None; host_slots],
            host_epochs: vec![0; host_slots],
            device: vec![None; device_slots],
            device_epochs: vec![0; device_slots],
            cap: bytes,
            used: 0,
            quarantine_slots,
            peak_device_in_flight: 0,
            peak_host_used: 0,
            totals: Totals::default(),
        })
    }
    /// Reserve both slots before GPU submission. A failed submission stays pending until finalized.
    /// # Errors
    /// Rejects invalid identity, occupied slots, byte cap and epoch overflow.
    pub fn reserve(
        &mut self,
        mut batch: Batch,
        bytes: u64,
        now_ms: u64,
    ) -> Result<Batch, &'static str> {
        if batch.version != 1
            || batch.device.is_empty()
            || batch.corpus.is_empty()
            || batch.slot >= self.host.len()
            || batch.device_slot >= self.device.len()
            || bytes == 0
            || !(1..=65_535).contains(&batch.jobs)
            || !(1..=4096).contains(&batch.inner)
        {
            return Err("invalid batch");
        }
        if self.host[batch.slot].is_some()
            || self.device[batch.device_slot].is_some()
            || bytes > self.cap.saturating_sub(self.used)
        {
            return Err("queue full");
        }
        let epoch = self.host_epochs[batch.slot]
            .checked_add(1)
            .ok_or("slot epoch overflow")?;
        let device_epoch = self.device_epochs[batch.device_slot]
            .checked_add(1)
            .ok_or("slot epoch overflow")?;
        self.host_epochs[batch.slot] = epoch;
        self.device_epochs[batch.device_slot] = device_epoch;
        batch.epoch = epoch;
        batch.device_epoch = device_epoch;
        self.device[batch.device_slot] = Some(batch.slot);
        self.host[batch.slot] = Some(Pending {
            batch: batch.clone(),
            stage: Stage::GpuWriting,
            bytes,
            submitted_ms: now_ms,
            quarantined_ms: None,
        });
        self.used += bytes;
        self.totals.submitted += 1;
        self.totals.peak_queue_bytes = self.totals.peak_queue_bytes.max(self.used);
        self.peak_device_in_flight = self.peak_device_in_flight.max(self.device_in_flight());
        self.peak_host_used = self.peak_host_used.max(self.host_used());
        Ok(batch)
    }
    fn entry(&mut self, batch: &Batch, allowed: &[Stage]) -> Result<&mut Pending, &'static str> {
        let p = self
            .host
            .get_mut(batch.slot)
            .and_then(Option::as_mut)
            .ok_or("unknown completion")?;
        if p.batch != *batch || !allowed.contains(&p.stage) {
            return Err("stale, duplicate or out-of-order completion");
        }
        Ok(p)
    }
    /// Kernel finished, copy in flight. Optional: `ready` also accepts `GpuWriting`.
    /// # Errors
    /// Rejects stale, duplicate or out-of-order transitions.
    pub fn copying(&mut self, batch: &Batch) -> Result<(), &'static str> {
        self.entry(batch, &[Stage::GpuWriting])?.stage = Stage::Copying;
        Ok(())
    }
    /// D2H acknowledged: the device slot is released and the CPU may read the host slot.
    /// # Errors
    /// Rejects stale/duplicate completion or a device slot the batch does not own.
    pub fn ready(&mut self, batch: &Batch) -> Result<(), &'static str> {
        if self.device.get(batch.device_slot).copied().flatten() != Some(batch.slot)
            || self.device_epochs.get(batch.device_slot) != Some(&batch.device_epoch)
        {
            return Err("stale, duplicate or out-of-order completion");
        }
        let pending = self.entry(batch, &[Stage::GpuWriting, Stage::Copying])?;
        pending.stage = Stage::Ready;
        let bytes = pending.bytes;
        self.device[batch.device_slot] = None;
        self.totals.completed += 1;
        self.totals.readback_ready += 1;
        self.totals.readback_bytes += bytes;
        Ok(())
    }
    /// # Errors
    /// Rejects verification before DMA completion or duplicate ownership transfer.
    pub fn verifying(&mut self, batch: &Batch) -> Result<(), &'static str> {
        self.entry(batch, &[Stage::Ready])?.stage = Stage::Verifying;
        Ok(())
    }
    /// # Errors
    /// Rejects wrong identity/state. Only full comparison may set `passed`. A failed batch
    /// enters quarantine while there is room, otherwise its slot is freed at once.
    pub fn finish(
        &mut self,
        batch: &Batch,
        passed: bool,
        ops: u64,
        now_ms: u64,
    ) -> Result<(), &'static str> {
        let bytes = self.entry(batch, &[Stage::Verifying])?.bytes;
        if passed {
            self.host[batch.slot] = None;
            self.used -= bytes;
            self.totals.verified += 1;
            self.totals.verified_bytes += bytes;
            self.totals.verified_integer_ops += ops;
            return Ok(());
        }
        self.totals.failed += 1;
        self.totals.failed_bytes += bytes;
        self.totals
            .first_failure
            .get_or_insert_with(|| batch.clone());
        let room = self.quarantined_count() < self.quarantine_slots;
        if let Some(p) = self.host[batch.slot].as_mut().filter(|_| room) {
            p.stage = Stage::Quarantined;
            p.quarantined_ms = Some(now_ms);
            self.totals.quarantined += 1;
        } else {
            self.host[batch.slot] = None;
            self.used -= bytes;
            self.totals.quarantine_refused += 1;
        }
        Ok(())
    }
    /// Ends a hold once confirmation is done.
    /// # Errors
    /// Rejects a batch that is not quarantined.
    pub fn release_quarantine(&mut self, batch: &Batch) -> Result<(), &'static str> {
        let bytes = self.entry(batch, &[Stage::Quarantined])?.bytes;
        self.host[batch.slot] = None;
        self.used -= bytes;
        self.totals.quarantine_released += 1;
        Ok(())
    }
    /// Frees holds older than `max_age_ms`; a hold never outlives its deadline.
    pub fn expire_quarantine(&mut self, now_ms: u64, max_age_ms: u64) -> Vec<Batch> {
        let mut expired = Vec::new();
        for slot in 0..self.host.len() {
            let due = self.host[slot].as_ref().is_some_and(|p| {
                p.stage == Stage::Quarantined
                    && now_ms.saturating_sub(p.quarantined_ms.unwrap_or(now_ms)) >= max_age_ms
            });
            if due && let Some(p) = self.host[slot].take() {
                self.used -= p.bytes;
                self.totals.quarantine_expired += 1;
                expired.push(p.batch);
            }
        }
        expired
    }
    /// Every batch the ledger still holds, quarantined ones included.
    #[must_use]
    pub fn pending(&self) -> Vec<Pending> {
        self.host.iter().flatten().cloned().collect()
    }
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.used
    }
    /// Batches that have not reached a verdict (quarantined ones already have one).
    #[must_use]
    pub fn count(&self) -> usize {
        self.host
            .iter()
            .flatten()
            .filter(|p| p.stage != Stage::Quarantined)
            .count()
    }
    #[must_use]
    pub fn device_in_flight(&self) -> usize {
        self.device.iter().flatten().count()
    }
    /// Occupied host slots: DMA targets, queued outputs, verifying and quarantined.
    #[must_use]
    pub fn host_used(&self) -> usize {
        self.host.iter().flatten().count()
    }
    /// Outputs waiting for or under verification: the verifier backlog.
    #[must_use]
    pub fn host_backlog(&self) -> usize {
        self.host
            .iter()
            .flatten()
            .filter(|p| matches!(p.stage, Stage::Ready | Stage::Verifying))
            .count()
    }
    #[must_use]
    pub fn quarantined_count(&self) -> usize {
        self.host
            .iter()
            .flatten()
            .filter(|p| p.stage == Stage::Quarantined)
            .count()
    }
    #[must_use]
    pub fn peak_device_in_flight(&self) -> usize {
        self.peak_device_in_flight
    }
    #[must_use]
    pub fn peak_host_used(&self) -> usize {
        self.peak_host_used
    }
    #[must_use]
    pub fn device_slot_count(&self) -> usize {
        self.device.len()
    }
    #[must_use]
    pub fn host_slot_count(&self) -> usize {
        self.host.len()
    }
    /// First free device slot and first free host slot, if both exist.
    #[must_use]
    pub fn free_slots(&self) -> Option<(usize, usize)> {
        let device = self.device.iter().position(Option::is_none)?;
        let host = self.host.iter().position(Option::is_none)?;
        Some((device, host))
    }
    #[must_use]
    pub fn oldest_ms(&self, now_ms: u64) -> u64 {
        self.host
            .iter()
            .flatten()
            .filter(|p| p.stage != Stage::Quarantined)
            .map(|p| now_ms.saturating_sub(p.submitted_ms))
            .max()
            .unwrap_or(0)
    }
    /// Verifies that no batch was counted twice or lost and that both slot pools agree with
    /// the batches. Cheap enough to call every burst.
    /// # Errors
    /// Names the first violated identity.
    pub fn check_conservation(&self) -> Result<(), String> {
        let t = &self.totals;
        let all: Vec<&Pending> = self.host.iter().flatten().collect();
        let held_slots = all.iter().filter(|p| p.stage == Stage::Quarantined).count();
        let held = held_slots as u64;
        let active = all.len() as u64 - held;
        let on_device = all.iter().filter(|p| p.stage.on_device()).count();
        let host_active = all
            .iter()
            .filter(|p| matches!(p.stage, Stage::Ready | Stage::Verifying));
        let (host_count, host_bytes) =
            host_active.fold((0_u64, 0_u64), |(n, b), p| (n + 1, b + p.bytes));
        let total_bytes: u64 = all.iter().map(|p| p.bytes).sum();
        if t.submitted != t.verified + t.failed + active {
            return Err("submitted != verified + failed + pending".into());
        }
        if t.completed != t.readback_ready {
            return Err("completed != readback_ready".into());
        }
        if t.completed != t.verified + t.failed + host_count {
            return Err("completed != verified + failed + host pending".into());
        }
        if t.readback_bytes != t.verified_bytes + t.failed_bytes + host_bytes {
            return Err("readback bytes are not conserved".into());
        }
        if self.used != total_bytes {
            return Err("queue byte accounting drifted".into());
        }
        if t.failed != t.quarantined + t.quarantine_refused {
            return Err("failed batches are neither quarantined nor refused".into());
        }
        if t.quarantined != t.quarantine_released + t.quarantine_expired + held {
            return Err("quarantine holds are not conserved".into());
        }
        if held_slots > self.quarantine_slots {
            return Err("quarantine exceeds its capacity".into());
        }
        if self.device.iter().flatten().count() != on_device {
            return Err("device slots disagree with batches on the device".into());
        }
        for (index, owner) in self.device.iter().enumerate() {
            if let Some(host) = owner {
                let ok = self.host[*host].as_ref().is_some_and(|p| {
                    p.stage.on_device()
                        && p.batch.device_slot == index
                        && p.batch.device_epoch == self.device_epochs[index]
                });
                if !ok {
                    return Err("device slot owner does not match its host slot".into());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn batch(device_slot: usize, slot: usize) -> Batch {
        Batch {
            version: 1,
            device: "pci:test".into(),
            corpus: "hash".into(),
            seed: 7,
            generation: 9,
            jobs: 2,
            inner: 17,
            slot,
            epoch: 0,
            device_slot,
            device_epoch: 0,
        }
    }
    fn to_verifying(q: &mut Ledger, b: &Batch) {
        q.ready(b).unwrap();
        q.verifying(b).unwrap();
    }

    #[test]
    fn ledger_rejects_stale_duplicate_and_early_verification() {
        let mut q = Ledger::new(2, 3, 192, 1).unwrap();
        let a = q.reserve(batch(0, 0), 64, 0).unwrap();
        let b = q.reserve(batch(1, 1), 64, 1).unwrap();
        assert!(
            q.verifying(&a).is_err(),
            "verification before D2H completion"
        );
        assert!(
            q.reserve(batch(0, 2), 1, 2).is_err(),
            "device slot still owned"
        );
        assert!(
            q.reserve(batch(1, 0), 1, 2).is_err(),
            "host slot still owned"
        );
        to_verifying(&mut q, &b);
        q.finish(&b, true, 10, 3).unwrap();
        assert!(q.finish(&b, true, 10, 3).is_err(), "duplicate verdict");
        assert!(q.ready(&b).is_err(), "completion of a finished batch");
        let c = q.reserve(batch(1, 1), 64, 4).unwrap();
        assert_eq!((c.epoch, c.device_epoch), (2, 2));
        assert!(
            q.ready(&b).is_err(),
            "old epoch must not complete the reused slots"
        );
        to_verifying(&mut q, &a);
        q.finish(&a, true, 10, 5).unwrap();
        q.check_conservation().unwrap();
        assert_eq!(
            q.totals.submitted,
            q.totals.verified + q.totals.failed + q.count() as u64
        );
        assert_eq!(q.used_bytes(), 64);
        assert_eq!(q.oldest_ms(20), 16);
    }

    #[test]
    fn device_slot_frees_at_copy_completion_so_the_gpu_can_run_ahead_of_the_verifier() {
        let mut q = Ledger::new(2, 6, 6 * 64, 1).unwrap();
        let first = q.reserve(batch(0, 0), 64, 0).unwrap();
        let second = q.reserve(batch(1, 1), 64, 1).unwrap();
        assert_eq!(q.free_slots(), None, "both device slots are busy");
        q.copying(&first).unwrap();
        assert!(q.copying(&first).is_err(), "copying twice");
        q.ready(&first).unwrap();
        // The first output is queued for a verifier, yet its device slot is already reusable.
        assert_eq!(
            (q.device_in_flight(), q.host_backlog(), q.host_used()),
            (1, 1, 2)
        );
        let (device, host) = q.free_slots().unwrap();
        assert_eq!(
            (device, host),
            (0, 2),
            "device slot reused while host slot 0 is still queued"
        );
        let third = q.reserve(batch(device, host), 64, 2).unwrap();
        q.check_conservation().unwrap();
        assert_eq!(q.peak_device_in_flight(), 2);
        assert_eq!(q.peak_host_used(), 3);
        // Host slots are only returned by the verdict, not by the copy.
        to_verifying(&mut q, &second);
        q.finish(&second, true, 1, 3).unwrap();
        q.verifying(&first).unwrap(); // already ready
        q.finish(&first, true, 1, 3).unwrap();
        to_verifying(&mut q, &third);
        q.finish(&third, true, 1, 4).unwrap();
        assert_eq!((q.count(), q.host_used(), q.used_bytes()), (0, 0, 0));
        q.check_conservation().unwrap();
    }

    #[test]
    fn a_failed_batch_is_quarantined_bounded_and_released_or_expired() {
        let mut q = Ledger::new(2, 4, 4 * 64, 1).unwrap();
        let a = q.reserve(batch(0, 0), 64, 0).unwrap();
        let b = q.reserve(batch(1, 1), 64, 0).unwrap();
        to_verifying(&mut q, &a);
        to_verifying(&mut q, &b);
        q.finish(&a, false, 0, 100).unwrap();
        assert_eq!(q.quarantined_count(), 1);
        assert_eq!(q.count(), 1, "a quarantined batch already has its verdict");
        q.check_conservation().unwrap();
        // The hold keeps the host slot and its bytes: the next batch cannot land on it.
        assert!(q.reserve(batch(0, 0), 64, 101).is_err());
        assert_eq!(q.used_bytes(), 128);
        // Quarantine is full, so a second failure is not held and its slot is freed at once.
        q.finish(&b, false, 0, 102).unwrap();
        assert_eq!(
            (
                q.totals.quarantined,
                q.totals.quarantine_refused,
                q.totals.failed
            ),
            (1, 1, 2)
        );
        assert_eq!(q.used_bytes(), 64);
        q.check_conservation().unwrap();
        // A hold expires by age and never outlives its deadline.
        assert!(q.expire_quarantine(150, 100).is_empty());
        let expired = q.expire_quarantine(200, 100);
        assert_eq!(expired.len(), 1);
        assert_eq!((q.totals.quarantine_expired, q.used_bytes()), (1, 0));
        assert!(q.release_quarantine(&a).is_err(), "already expired");
        q.check_conservation().unwrap();
        assert_eq!(q.totals.first_failure, Some(a));
        // Explicit release path.
        let c = q.reserve(batch(0, 0), 64, 300).unwrap();
        to_verifying(&mut q, &c);
        q.finish(&c, false, 0, 301).unwrap();
        q.release_quarantine(&c).unwrap();
        assert_eq!((q.totals.quarantine_released, q.used_bytes()), (1, 0));
        q.check_conservation().unwrap();
    }

    #[test]
    fn invalid_capacities_are_rejected() {
        assert!(Ledger::new(0, 4, 1, 1).is_err());
        assert!(
            Ledger::new(3, 2, 1, 0).is_err(),
            "fewer host slots than device slots"
        );
        assert!(Ledger::new(2, 4, 0, 1).is_err());
        assert!(
            Ledger::new(2, 4, 1, 4).is_err(),
            "quarantine may not hold every host slot"
        );
        assert!(Ledger::new(17, 20, 1, 1).is_err());
        assert!(Ledger::new(2, 65, 1, 1).is_err());
        assert!(Ledger::new(2, 2, 1, 1).is_ok());
    }

    #[test]
    fn hard_byte_cap_rejects_a_reservation_that_does_not_fit() {
        let mut q = Ledger::new(2, 3, 100, 1).unwrap();
        q.reserve(batch(0, 0), 80, 0).unwrap();
        assert!(q.reserve(batch(1, 1), 21, 1).is_err());
    }
}
