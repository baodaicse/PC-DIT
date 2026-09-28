//! GPU-H6 pure planning for the transfer lane: which VRAM region to write or re-read next,
//! and a token bucket that bounds the lane's bandwidth. No driver calls, so the region
//! bookkeeping (no overlap, retention before overwrite, wrap-around) is testable on its own.
use serde::Serialize;
use std::collections::VecDeque;

/// Transfer sizes rotated through the lane, smallest to largest.
pub const CHUNK_SIZES: [u64; 8] = [
    64 * 1024,
    128 * 1024,
    256 * 1024,
    1024 * 1024,
    4 * 1024 * 1024,
    12 * 1024 * 1024,
    32 * 1024 * 1024,
    64 * 1024 * 1024,
];

/// A stretch of one pool allocation the lane may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Zone {
    pub allocation: usize,
    pub start: u64,
    pub len: u64,
}

/// A byte range of one pool allocation (absolute offsets).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Region {
    pub allocation: usize,
    pub offset: u64,
    pub bytes: u64,
}

impl Region {
    #[must_use]
    pub fn overlaps(&self, other: &Region) -> bool {
        self.allocation == other.allocation
            && self.offset < other.offset.saturating_add(other.bytes)
            && other.offset < self.offset.saturating_add(self.bytes)
    }
}

/// One unit of lane work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum LaneOp {
    /// Write the pattern for `generation` (H2D), read it back at once (D2H) and compare.
    Write { region: Region, generation: u64 },
    /// Read a region written earlier (D2H only) and compare with the pattern of `generation`.
    /// `before_overwrite` marks a re-read forced because a new write would have destroyed it.
    Reread {
        region: Region,
        generation: u64,
        before_overwrite: bool,
    },
}

impl LaneOp {
    #[must_use]
    pub fn region(&self) -> Region {
        match self {
            Self::Write { region, .. } | Self::Reread { region, .. } => *region,
        }
    }
    #[must_use]
    pub fn generation(&self) -> u64 {
        match self {
            Self::Write { generation, .. } | Self::Reread { generation, .. } => *generation,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Record {
    region: Region,
    generation: u64,
}

/// Rotates writes over the zones, re-reads each region after a delay, and never lets two
/// operations touch overlapping bytes at the same time. A region with a pending re-read is
/// verified before anything is written over it.
#[derive(Clone, Debug)]
pub struct TransferPlanner {
    zones: Vec<Zone>,
    cursors: Vec<u64>,
    passes: Vec<u64>,
    max_chunk: u64,
    retention: VecDeque<Record>,
    /// A written region becomes due for its re-read once this many further writes were issued.
    reread_after: u64,
    in_flight: Vec<Region>,
    next_generation: u64,
    pub writes_issued: u64,
    pub rereads_issued: u64,
    pub forced_rereads: u64,
}

impl TransferPlanner {
    /// # Errors
    /// Rejects an empty plan, empty zones and chunk limits below one byte.
    pub fn new(zones: Vec<Zone>, max_chunk: u64, reread_after: u64) -> Result<Self, &'static str> {
        if zones.is_empty() || zones.iter().any(|z| z.len == 0) || max_chunk == 0 {
            return Err("transfer lane needs non-empty zones and a chunk limit");
        }
        let count = zones.len();
        Ok(Self {
            zones,
            cursors: vec![0; count],
            passes: vec![0; count],
            max_chunk,
            retention: VecDeque::new(),
            reread_after,
            in_flight: Vec::new(),
            next_generation: 0,
            writes_issued: 0,
            rereads_issued: 0,
            forced_rereads: 0,
        })
    }

    /// Full sweeps of the slowest zone: coverage completed, not bytes touched.
    #[must_use]
    pub fn sweeps(&self) -> u64 {
        self.passes.iter().copied().min().unwrap_or(0)
    }

    #[must_use]
    pub fn pending_rereads(&self) -> usize {
        self.retention.len()
    }

    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    fn free(&self, region: &Region) -> bool {
        !self.in_flight.iter().any(|r| r.overlaps(region))
    }

    /// The next operation, or `None` when nothing may start now (everything runnable
    /// overlaps an operation still in flight). `flush` issues only pending re-reads.
    pub fn next(&mut self, flush: bool) -> Option<LaneOp> {
        // A due re-read comes first. Records are in write order, so the oldest is examined.
        let due = self
            .retention
            .iter()
            .position(|r| flush || r.generation + self.reread_after <= self.next_generation);
        if let Some(index) = due
            && self.free(&self.retention[index].region)
            && let Some(record) = self.retention.remove(index)
        {
            self.in_flight.push(record.region);
            self.rereads_issued += 1;
            return Some(LaneOp::Reread {
                region: record.region,
                generation: record.generation,
                before_overwrite: false,
            });
        }
        if flush {
            return None;
        }
        let zone_index = usize::try_from(self.next_generation).unwrap_or(0) % self.zones.len();
        let zone = self.zones[zone_index];
        let cursor = self.cursors[zone_index];
        let wanted =
            CHUNK_SIZES[usize::try_from(self.next_generation).unwrap_or(0) % CHUNK_SIZES.len()];
        let bytes = wanted.min(self.max_chunk).min(zone.len - cursor).max(1);
        let region = Region {
            allocation: zone.allocation,
            offset: zone.start + cursor,
            bytes,
        };
        // Never destroy data that still has to be verified.
        if let Some(index) = self
            .retention
            .iter()
            .position(|r| r.region.overlaps(&region))
        {
            if self.free(&self.retention[index].region)
                && let Some(record) = self.retention.remove(index)
            {
                self.in_flight.push(record.region);
                self.rereads_issued += 1;
                self.forced_rereads += 1;
                return Some(LaneOp::Reread {
                    region: record.region,
                    generation: record.generation,
                    before_overwrite: true,
                });
            }
            return None;
        }
        if !self.free(&region) {
            return None;
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        self.writes_issued += 1;
        self.in_flight.push(region);
        Some(LaneOp::Write { region, generation })
    }

    fn release(&mut self, region: &Region) {
        if let Some(index) = self.in_flight.iter().position(|r| r == region) {
            self.in_flight.swap_remove(index);
        }
    }

    /// An operation finished and its data compared equal. A write advances its zone and
    /// schedules the re-read; a re-read simply releases the region.
    pub fn completed(&mut self, op: &LaneOp) {
        self.release(&op.region());
        if let LaneOp::Write { region, generation } = op {
            if let Some(zone_index) = self
                .zones
                .iter()
                .position(|z| z.allocation == region.allocation)
            {
                let zone = self.zones[zone_index];
                let cursor = &mut self.cursors[zone_index];
                *cursor = region.offset + region.bytes - zone.start;
                if *cursor >= zone.len {
                    *cursor = 0;
                    self.passes[zone_index] += 1;
                }
            }
            self.retention.push_back(Record {
                region: *region,
                generation: *generation,
            });
        }
    }

    /// An operation failed. Its region is released and the cursor still advances, so a
    /// faulty range does not stall the lane; the failure is the caller's to report.
    pub fn failed(&mut self, op: &LaneOp) {
        self.release(&op.region());
        if let LaneOp::Write { region, .. } = op
            && let Some(zone_index) = self
                .zones
                .iter()
                .position(|z| z.allocation == region.allocation)
        {
            let zone = self.zones[zone_index];
            let cursor = &mut self.cursors[zone_index];
            *cursor = region.offset + region.bytes - zone.start;
            if *cursor >= zone.len {
                *cursor = 0;
                self.passes[zone_index] += 1;
            }
        }
    }
}

/// Bandwidth limiter for the lane. Time is supplied by the caller in milliseconds, so it is
/// deterministic under a fake clock. A rate of zero means unlimited.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct TokenBucket {
    bytes_per_second: u64,
    burst: u64,
    tokens: f64,
    last_ms: u64,
}

impl TokenBucket {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn new(bytes_per_second: u64, burst: u64) -> Self {
        Self {
            bytes_per_second,
            burst,
            tokens: burst as f64,
            last_ms: 0,
        }
    }

    #[allow(clippy::cast_precision_loss)]
    fn refill(&mut self, now_ms: u64) {
        let elapsed = now_ms.saturating_sub(self.last_ms) as f64 / 1000.0;
        self.last_ms = now_ms;
        self.tokens = (self.tokens + elapsed * self.bytes_per_second as f64).min(self.burst as f64);
    }

    /// Takes `cost` bytes if the bucket has them. An operation larger than the burst is
    /// admitted from a full bucket (and drives it negative) so it can never starve.
    #[allow(clippy::cast_precision_loss)]
    pub fn try_take(&mut self, now_ms: u64, cost: u64) -> bool {
        if self.bytes_per_second == 0 {
            return true;
        }
        self.refill(now_ms);
        let need = (cost as f64).min(self.burst as f64);
        if self.tokens >= need {
            self.tokens -= cost as f64;
            true
        } else {
            false
        }
    }

    /// Whether work may start now: the bucket is not in debt. Pair with [`Self::charge`] once
    /// the size of the work is known, so a limiter can gate an operation chosen afterwards.
    #[allow(clippy::cast_precision_loss)]
    pub fn admit(&mut self, now_ms: u64) -> bool {
        if self.bytes_per_second == 0 {
            return true;
        }
        self.refill(now_ms);
        self.tokens > 0.0
    }

    /// Takes `cost` bytes, possibly driving the bucket into debt that later time repays.
    #[allow(clippy::cast_precision_loss)]
    pub fn charge(&mut self, cost: u64) {
        if self.bytes_per_second != 0 {
            self.tokens -= cost as f64;
        }
    }

    #[must_use]
    pub fn limit(&self) -> u64 {
        self.bytes_per_second
    }
}

/// Everything the lane needs to know, validated once. `enabled` turns on the overlapped DMA
/// lane (transfer and retention on their own streams); `vram_local` adds the GPU-local
/// workload on a separate stretch of the pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct LaneConfig {
    pub enabled: bool,
    pub vram_local: bool,
    /// Operations in flight at once (one stream and one pair of pinned buffers each).
    pub slots: usize,
    /// Largest single transfer.
    pub chunk_bytes: u64,
    /// Bandwidth cap over both directions, bytes per second (0 = unlimited).
    pub rate_bytes_per_second: u64,
    /// A written region is re-read after this many further writes.
    pub reread_after: u64,
    /// Verifier CPU share weights when both classes of work are queued.
    pub compute_weight: u64,
    pub lane_weight: u64,
    /// Fill and verify passes a GPU-local write makes before its region waits for a checkpoint.
    pub vram_passes: u32,
}

impl Default for LaneConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            vram_local: false,
            slots: 2,
            chunk_bytes: 8 * 1024 * 1024,
            rate_bytes_per_second: 0,
            reread_after: 4,
            compute_weight: 3,
            lane_weight: 1,
            vram_passes: 4,
        }
    }
}

impl LaneConfig {
    /// # Errors
    /// Rejects values outside the ranges the lane is built and tested for.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=4).contains(&self.slots)
            || !(64 * 1024..=64 * 1024 * 1024).contains(&self.chunk_bytes)
            || self.reread_after > 1024
            || !(1..=64).contains(&self.compute_weight)
            || !(1..=64).contains(&self.lane_weight)
            || !(1..=64).contains(&self.vram_passes)
        {
            return Err("lane configuration out of range");
        }
        Ok(())
    }

    #[must_use]
    pub fn active(&self) -> bool {
        self.enabled || self.vram_local
    }

    /// Pinned host bytes every lane slot holds (upload + download + a 4-byte GPU-local flag),
    /// 0 when the lane is off. Belongs in the host RAM budget alongside the compute pipeline's
    /// own pinned queue.
    #[must_use]
    pub fn pinned_bytes(&self) -> u64 {
        if !self.active() {
            return 0;
        }
        self.slots as u64 * (2 * self.chunk_bytes + 4)
    }
}

/// Seconds during which both interval sets are active. Overlapping intervals inside one set are
/// merged first; invalid intervals are ignored.
#[must_use]
pub fn overlap_seconds(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    fn merged(set: &[(f64, f64)]) -> Vec<(f64, f64)> {
        let mut sorted: Vec<(f64, f64)> = set
            .iter()
            .copied()
            .filter(|(s, e)| s.is_finite() && e.is_finite() && e >= s)
            .collect();
        sorted.sort_by(|x, y| x.0.total_cmp(&y.0));
        let mut out: Vec<(f64, f64)> = Vec::new();
        for (s, e) in sorted {
            match out.last_mut() {
                Some(last) if s <= last.1 => last.1 = last.1.max(e),
                _ => out.push((s, e)),
            }
        }
        out
    }
    let (a, b) = (merged(a), merged(b));
    let (mut i, mut j, mut total) = (0, 0, 0.0);
    while i < a.len() && j < b.len() {
        let (start, end) = (a[i].0.max(b[j].0), a[i].1.min(b[j].1));
        if end > start {
            total += end - start;
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    total
}

/// GPU-H6: how many lane slots may start new work right now, given the controller's verifier
/// backlog fill (0 = empty queue, 1 = at the hard cap; it can exceed 1 briefly). At least one
/// slot always stays available, so a busy verifier never fully starves the lane — coverage must
/// not be lost under throttle — but a lane that is itself adding to the pressure backs off
/// smoothly instead of submitting at full rate regardless of the controller's state.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]
pub fn active_slot_budget(total_slots: usize, backlog_fill: f64) -> usize {
    let total_slots = total_slots.max(1);
    if total_slots == 1 || backlog_fill.is_nan() {
        return total_slots;
    }
    // Full budget below 30% fill; shrinks linearly to one slot by 90% fill and stays there.
    let fraction = (1.0 - (backlog_fill - 0.3) / 0.6).clamp(0.0, 1.0);
    let budget = (total_slots as f64 * fraction).ceil() as usize;
    budget.clamp(1, total_slots)
}

/// The GPU-local workload's word for `index` (position in the allocation, in 32-bit words):
/// a full-avalanche mix of the index and the key, so equal data never repeats at two addresses.
/// The CUDA kernels compute the same function.
#[must_use]
pub fn vram_word(index: u64, key: u32) -> u32 {
    #[allow(clippy::cast_possible_truncation)]
    let mut x = (index as u32).wrapping_mul(0x9E37_79B1)
        ^ key.wrapping_mul(0x85EB_CA6B)
        ^ ((index >> 32) as u32).wrapping_mul(0xC2B2_AE35);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^ (x >> 16)
}

/// Key of one GPU-local write, a pure function of the run seed and the write's generation.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn vram_key(seed: u64, generation: u64) -> u32 {
    crate::splitmix64(seed ^ generation.rotate_left(17) ^ 0x5652_414D) as u32
}

/// Fills `out` with the GPU-local pattern for the words starting at `first_word`.
pub fn vram_pattern_into(out: &mut [u8], key: u32, first_word: u64) {
    for (index, chunk) in out.chunks_mut(4).enumerate() {
        let word = vram_word(first_word + index as u64, key).to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::float_cmp)]
mod tests {
    use super::*;

    fn zones(sizes: &[u64]) -> Vec<Zone> {
        sizes
            .iter()
            .enumerate()
            .map(|(allocation, len)| Zone {
                allocation,
                start: 0,
                len: *len,
            })
            .collect()
    }

    /// Runs the planner with `depth` operations in flight, finishing them in a scrambled order,
    /// and checks the invariants after every step. Returns the planner and every op issued.
    fn run(
        mut p: TransferPlanner,
        depth: usize,
        steps: usize,
        seed: u64,
    ) -> (TransferPlanner, Vec<LaneOp>) {
        let mut state = seed | 1;
        let mut rng = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut live: Vec<LaneOp> = Vec::new();
        let mut issued = Vec::new();
        for _ in 0..steps {
            while live.len() < depth {
                let Some(op) = p.next(false) else { break };
                for other in &live {
                    assert!(
                        !op.region().overlaps(&other.region()),
                        "{op:?} overlaps {other:?}"
                    );
                }
                live.push(op);
                issued.push(op);
            }
            if live.is_empty() {
                break;
            }
            let i = usize::try_from(rng()).unwrap() % live.len();
            let op = live.swap_remove(i);
            p.completed(&op);
            assert_eq!(p.in_flight(), live.len());
        }
        for op in live {
            p.completed(&op);
        }
        (p, issued)
    }

    #[test]
    fn regions_never_overlap_in_flight_and_every_write_is_reread_before_it_is_overwritten() {
        let sizes = [40 * 1024 * 1024, 9 * 1024 * 1024];
        for depth in [1, 2, 3] {
            let p = TransferPlanner::new(zones(&sizes), 32 * 1024 * 1024, 2).unwrap();
            let (mut p, issued) = run(p, depth, 4_000, 7 + depth as u64);
            assert!(p.sweeps() >= 3, "depth {depth}: {} sweeps", p.sweeps());
            // Replay the issued order: a write over a region that still had an unread earlier
            // write is only allowed after that write was re-read.
            let mut unread: Vec<(Region, u64)> = Vec::new();
            for op in &issued {
                match op {
                    LaneOp::Write { region, generation } => {
                        assert!(
                            !unread.iter().any(|(r, _)| r.overlaps(region)),
                            "depth {depth}: write {generation} destroys an unverified region"
                        );
                        unread.push((*region, *generation));
                    }
                    LaneOp::Reread {
                        region, generation, ..
                    } => {
                        let at = unread
                            .iter()
                            .position(|(r, g)| r == region && g == generation)
                            .expect("a re-read follows its write");
                        unread.swap_remove(at);
                    }
                }
            }
            // Flushing verifies whatever is still pending, and only that.
            let mut flushed = 0;
            while let Some(op) = p.next(true) {
                assert!(matches!(op, LaneOp::Reread { .. }));
                p.completed(&op);
                flushed += 1;
            }
            assert_eq!(flushed, unread.len(), "depth {depth}");
            assert_eq!(p.pending_rereads(), 0);
        }
    }

    #[test]
    fn writes_use_the_rotating_chunk_sizes_inside_the_limit_and_never_leave_their_zone() {
        let z = zones(&[100 * 1024 * 1024]);
        let p = TransferPlanner::new(z.clone(), 8 * 1024 * 1024, 2).unwrap();
        let (_, issued) = run(p, 1, 500, 3);
        let mut sizes = std::collections::BTreeSet::new();
        for op in &issued {
            let r = op.region();
            assert!(r.offset + r.bytes <= z[0].start + z[0].len);
            assert!(r.bytes <= 8 * 1024 * 1024 && r.bytes >= 1);
            sizes.insert(r.bytes);
        }
        assert!(sizes.len() >= 5, "chunk sizes rotate: {sizes:?}");
    }

    #[test]
    fn zones_with_an_offset_keep_every_operation_inside_their_stretch() {
        let z = vec![Zone {
            allocation: 0,
            start: 32 * 1024 * 1024,
            len: 32 * 1024 * 1024,
        }];
        let p = TransferPlanner::new(z, 64 * 1024 * 1024, 2).unwrap();
        let (p, issued) = run(p, 2, 600, 5);
        assert!(!issued.is_empty() && p.sweeps() >= 1);
        for op in &issued {
            let r = op.region();
            assert!(
                r.offset >= 32 * 1024 * 1024 && r.offset + r.bytes <= 64 * 1024 * 1024,
                "{r:?}"
            );
        }
    }

    #[test]
    fn a_failed_write_releases_its_region_and_the_lane_moves_on() {
        let mut p = TransferPlanner::new(zones(&[16 * 1024 * 1024]), 4 * 1024 * 1024, 2).unwrap();
        let first = p.next(false).unwrap();
        let LaneOp::Write { region, .. } = first else {
            panic!("first op is a write")
        };
        p.failed(&first);
        assert_eq!(p.in_flight(), 0);
        assert_eq!(
            p.pending_rereads(),
            0,
            "nothing to re-read after a failed write"
        );
        let second = p.next(false).unwrap();
        assert!(
            second.region().offset >= region.offset + region.bytes,
            "cursor advanced past the fault"
        );
    }

    #[test]
    fn a_blocked_plan_waits_instead_of_touching_data_in_flight() {
        // One tiny zone: after the first write nothing else fits until it completes.
        let mut p = TransferPlanner::new(zones(&[64 * 1024]), 64 * 1024, 0).unwrap();
        let a = p.next(false).unwrap();
        assert!(p.next(false).is_none(), "the only region is in flight");
        p.completed(&a);
        // With a zero delay the re-read is due at once and must precede the next write.
        let b = p.next(false).unwrap();
        assert!(matches!(b, LaneOp::Reread { .. }));
        assert!(p.next(false).is_none());
        p.completed(&b);
        assert!(matches!(p.next(false), Some(LaneOp::Write { .. })));
        assert!(TransferPlanner::new(vec![], 1, 1).is_err());
        assert!(TransferPlanner::new(zones(&[0]), 1, 1).is_err());
    }

    #[test]
    fn admit_and_charge_gate_work_chosen_afterwards_and_repay_debt_over_time() {
        let mut b = TokenBucket::new(10 * 1024 * 1024, 4 * 1024 * 1024);
        assert!(b.admit(0));
        b.charge(64 * 1024 * 1024);
        assert!(!b.admit(100), "still in debt after 100 ms");
        assert!(
            b.admit(7_000),
            "debt of 64 MiB repaid at 10 MiB/s within 7 s"
        );
        assert!(TokenBucket::new(0, 1).admit(0));
    }

    #[test]
    fn overlap_is_the_intersection_of_the_merged_sets() {
        let a = [(0.0, 2.0), (1.5, 4.0), (10.0, 11.0)];
        let b = [(3.0, 10.5), (f64::NAN, 1.0), (20.0, 19.0)];
        // a merges to [0,4] and [10,11]; b is [3,10.5]: overlap 1.0 + 0.5.
        assert!((overlap_seconds(&a, &b) - 1.5).abs() < 1e-12);
        assert_eq!(overlap_seconds(&a, &[]), 0.0);
        assert_eq!(overlap_seconds(&[(0.0, 1.0)], &[(1.0, 2.0)]), 0.0);
    }

    #[test]
    fn config_validation_rejects_out_of_range_values() {
        assert!(LaneConfig::default().validate().is_ok());
        for bad in [
            LaneConfig {
                slots: 0,
                ..LaneConfig::default()
            },
            LaneConfig {
                slots: 5,
                ..LaneConfig::default()
            },
            LaneConfig {
                chunk_bytes: 1,
                ..LaneConfig::default()
            },
            LaneConfig {
                lane_weight: 0,
                ..LaneConfig::default()
            },
            LaneConfig {
                vram_passes: 0,
                ..LaneConfig::default()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        assert!(!LaneConfig::default().active());
        assert!(
            LaneConfig {
                vram_local: true,
                ..LaneConfig::default()
            }
            .active()
        );
    }

    #[test]
    fn active_slot_budget_stays_full_below_the_knee_shrinks_to_one_above_it_and_never_zero() {
        for slots in [1, 2, 3, 4] {
            assert_eq!(active_slot_budget(slots, 0.0), slots);
            assert_eq!(
                active_slot_budget(slots, 0.3),
                slots,
                "full budget up to the knee"
            );
            assert_eq!(
                active_slot_budget(slots, 1.5),
                1,
                "sustained overload: one slot, never zero"
            );
            assert_eq!(
                active_slot_budget(slots, f64::NAN),
                slots,
                "a bad sample never blocks the lane"
            );
            assert_eq!(active_slot_budget(slots, f64::INFINITY), 1);
        }
        // Monotonic: never grows as fill rises.
        let mut previous = active_slot_budget(4, 0.0);
        for tenth in 1..=15 {
            let budget = active_slot_budget(4, f64::from(tenth) / 10.0);
            assert!(
                budget <= previous,
                "fill {:.1}: {budget} > {previous}",
                f64::from(tenth) / 10.0
            );
            previous = budget;
        }
        assert_eq!(active_slot_budget(4, 0.9), 1);
        assert_eq!(
            active_slot_budget(4, 0.6),
            2,
            "halfway between the knee and the floor"
        );
    }

    #[test]
    fn pinned_bytes_is_zero_when_off_and_scales_with_slots_and_chunk() {
        assert_eq!(LaneConfig::default().pinned_bytes(), 0);
        let on = LaneConfig {
            enabled: true,
            slots: 2,
            chunk_bytes: 8 * 1024 * 1024,
            ..LaneConfig::default()
        };
        assert_eq!(on.pinned_bytes(), 2 * (2 * 8 * 1024 * 1024 + 4));
        let vram_only = LaneConfig {
            vram_local: true,
            ..on
        };
        assert_eq!(
            vram_only.pinned_bytes(),
            on.pinned_bytes(),
            "either workload uses the same slots"
        );
    }

    #[test]
    fn the_vram_pattern_is_address_dependent_key_dependent_and_replayable() {
        let mut a = vec![0_u8; 1 << 16];
        let mut b = vec![0_u8; 1 << 16];
        vram_pattern_into(&mut a, 7, 0);
        vram_pattern_into(&mut b, 7, 0);
        assert_eq!(a, b);
        vram_pattern_into(&mut b, 8, 0);
        assert_ne!(a, b, "another key gives other data");
        vram_pattern_into(&mut b, 7, 1);
        assert_ne!(a, b, "the same key at another address gives other data");
        // No word repeats inside a region, so an address-line fault cannot go unnoticed.
        let mut words: Vec<u32> = a
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        words.sort_unstable();
        words.dedup();
        assert!(
            words.len() >= (1 << 14) - 2,
            "{} distinct words",
            words.len()
        );
        // A tail shorter than a word is the prefix of the same pattern.
        let mut tail = [0_u8; 6];
        vram_pattern_into(&mut tail, 7, 0);
        assert_eq!(&tail[..], &a[..6]);
        assert_ne!(vram_key(1, 1), vram_key(1, 2));
        assert_eq!(vram_key(1, 1), vram_key(1, 1));
    }

    #[test]
    fn the_token_bucket_limits_the_rate_admits_oversize_work_and_zero_means_unlimited() {
        let mut unlimited = TokenBucket::new(0, 1);
        assert!(unlimited.try_take(0, u64::MAX));
        let mut bucket = TokenBucket::new(100 * 1024 * 1024, 64 * 1024 * 1024);
        let mut taken = 0_u64;
        for ms in 0..10_000 {
            if bucket.try_take(ms, 8 * 1024 * 1024) {
                taken += 8 * 1024 * 1024;
            }
        }
        // About 100 MiB/s over 10 s plus the initial burst.
        let rate = taken as f64 / 1_048_576.0 / 10.0;
        assert!((95.0..=112.0).contains(&rate), "{rate:.1} MiB/s");
        // An operation bigger than the whole burst is still admitted from a full bucket.
        let mut small = TokenBucket::new(1024, 1024);
        assert!(small.try_take(0, 1 << 20));
        assert!(!small.try_take(1, 1 << 20), "the debt must be repaid first");
        assert!(small.try_take(2_000_000, 1 << 20));
    }
}
