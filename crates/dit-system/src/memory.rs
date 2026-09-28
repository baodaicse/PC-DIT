use memmap2::{MmapMut, MmapOptions};
use serde::{Deserialize, Serialize};

use crate::{MemoryInventory, NumaNode, SystemDitError, report::MemoryPlanEvidence};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryTarget {
    Bytes(u64),
    PercentAvailable(u8),
    PercentSafe(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockPolicy {
    Off,
    BestEffort,
    Required,
}

impl LockPolicy {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::BestEffort => "best_effort",
            Self::Required => "required",
        }
    }
}

pub(crate) struct MemoryArena {
    pub id: usize,
    pub global_offset: u64,
    pub target_numa_node: Option<u32>,
    pub map: MmapMut,
    pub first_touch_cpu: Option<usize>,
    pub last_writer_cpu: Option<usize>,
    pub verify_cpu: Option<usize>,
    pub lock_succeeded: bool,
    pub lock_error: Option<String>,
    pub rounds_written: u32,
    pub rounds_verified: u32,
}

impl MemoryArena {
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

pub(crate) struct AllocationResult {
    pub arenas: Vec<MemoryArena>,
    pub evidence: MemoryPlanEvidence,
}

#[allow(clippy::too_many_lines)] // Planning and backoff are kept together for auditability.
pub(crate) fn plan_and_allocate(
    inventory: &MemoryInventory,
    nodes: &[NumaNode],
    target: MemoryTarget,
    safety_reserve_bytes: u64,
    arena_target_bytes: u64,
    minimum_arena_bytes: u64,
    lock_policy: LockPolicy,
) -> Result<AllocationResult, SystemDitError> {
    if arena_target_bytes == 0
        || minimum_arena_bytes == 0
        || minimum_arena_bytes > arena_target_bytes
    {
        return Err(SystemDitError::InvalidConfig(
            "arena sizes must be positive and minimum <= target".to_owned(),
        ));
    }
    let safe_limit_bytes = inventory
        .effective_available_bytes
        .saturating_sub(safety_reserve_bytes);
    let (target_kind, requested_bytes) = match target {
        MemoryTarget::Bytes(bytes) if bytes > 0 => ("explicit_bytes".to_owned(), bytes),
        MemoryTarget::Bytes(_) => {
            return Err(SystemDitError::InvalidConfig(
                "explicit memory target must be positive".to_owned(),
            ));
        }
        MemoryTarget::PercentAvailable(percent @ 1..=95) => (
            format!("percent_available:{percent}"),
            inventory
                .effective_available_bytes
                .saturating_mul(u64::from(percent))
                / 100,
        ),
        MemoryTarget::PercentAvailable(percent) => {
            return Err(SystemDitError::InvalidConfig(format!(
                "available percent must be 1..=95, got {percent}"
            )));
        }
        MemoryTarget::PercentSafe(percent @ 1..=100) => (
            format!("percent_safe:{percent}"),
            safe_limit_bytes.saturating_mul(u64::from(percent)) / 100,
        ),
        MemoryTarget::PercentSafe(percent) => {
            return Err(SystemDitError::InvalidConfig(format!(
                "safe percent must be 1..=100, got {percent}"
            )));
        }
    };
    if requested_bytes > safe_limit_bytes {
        return Err(SystemDitError::InvalidConfig(format!(
            "requested {requested_bytes} bytes exceeds safe limit {safe_limit_bytes}; effective available={} reserve={safety_reserve_bytes}",
            inventory.effective_available_bytes
        )));
    }

    let page_size = inventory.page_size_bytes.max(4096);
    let arena_target_bytes = align_down(arena_target_bytes, page_size).max(page_size);
    let minimum_arena_bytes = align_down(minimum_arena_bytes, page_size).max(page_size);
    let mut remaining = requested_bytes;
    let mut global_offset = 0_u64;
    let mut arenas = Vec::new();
    let mut backoffs = 0_u64;
    while remaining > 0 {
        let mut candidate = remaining.min(arena_target_bytes);
        let mut map = None;
        loop {
            let candidate_usize = usize::try_from(candidate).map_err(|_| {
                SystemDitError::InvalidConfig("arena size does not fit usize".to_owned())
            })?;
            match MmapOptions::new().len(candidate_usize).map_anon() {
                Ok(mapping) => {
                    map = Some(mapping);
                    break;
                }
                Err(_) if candidate > minimum_arena_bytes => {
                    backoffs += 1;
                    candidate = align_down((candidate / 2).max(minimum_arena_bytes), page_size);
                }
                Err(source) => {
                    if arenas.is_empty() {
                        return Err(SystemDitError::Mmap {
                            bytes: candidate_usize,
                            source,
                        });
                    }
                    break;
                }
            }
        }
        let Some(map) = map else {
            break;
        };
        let id = arenas.len();
        let target_numa_node = (!nodes.is_empty()).then(|| nodes[id % nodes.len()].node_id);
        arenas.push(MemoryArena {
            id,
            global_offset,
            target_numa_node,
            map,
            first_touch_cpu: None,
            last_writer_cpu: None,
            verify_cpu: None,
            lock_succeeded: false,
            lock_error: None,
            rounds_written: 0,
            rounds_verified: 0,
        });
        remaining -= candidate;
        global_offset += candidate;
    }

    let allocated_unique_bytes = global_offset;
    let allocation_target_met = allocated_unique_bytes == requested_bytes;
    let evidence = MemoryPlanEvidence {
        target_kind,
        effective_available_bytes: inventory.effective_available_bytes,
        safety_reserve_bytes,
        safe_limit_bytes,
        requested_bytes,
        allocated_unique_bytes,
        arena_target_bytes,
        minimum_arena_bytes,
        arena_count: arenas.len(),
        allocation_backoff_count: backoffs,
        allocation_target_met,
        lock_policy: lock_policy.as_str().to_owned(),
        locked_bytes: 0,
        prefaulted_unique_bytes: 0,
        written_unique_bytes: 0,
        read_unique_bytes: 0,
        exact_verified_unique_bytes: 0,
        total_write_bytes: 0,
        total_read_bytes: 0,
        coverage_target_met: false,
        numa_policy: "first_touch_unverified_physical_placement".to_owned(),
    };
    Ok(AllocationResult { arenas, evidence })
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value - value % alignment
}

#[cfg(test)]
mod tests {
    use super::{LockPolicy, MemoryTarget, plan_and_allocate};
    use crate::MemoryInventory;

    fn inventory() -> MemoryInventory {
        MemoryInventory {
            installed_bytes: 8 << 30,
            available_at_start_bytes: 4 << 30,
            free_bytes: 4 << 30,
            swap_total_bytes: 0,
            swap_free_bytes: 0,
            hardware_corrupted_bytes: 0,
            page_size_bytes: 4096,
            cgroup_limit_bytes: None,
            cgroup_current_bytes: None,
            effective_available_bytes: 4 << 30,
        }
    }

    #[test]
    fn rejects_target_beyond_reserve() {
        let result = plan_and_allocate(
            &inventory(),
            &[],
            MemoryTarget::Bytes(4 << 30),
            512 << 20,
            64 << 20,
            4 << 20,
            LockPolicy::Off,
        );
        assert!(result.is_err());
    }

    #[test]
    fn allocates_multiple_anonymous_arenas() {
        let result = plan_and_allocate(
            &inventory(),
            &[],
            MemoryTarget::Bytes(9 << 20),
            512 << 20,
            4 << 20,
            1 << 20,
            LockPolicy::Off,
        )
        .expect("allocation");
        assert_eq!(result.arenas.len(), 3);
        assert_eq!(result.evidence.allocated_unique_bytes, 9 << 20);
        assert!(result.evidence.allocation_target_met);
    }

    #[test]
    fn percentage_target_uses_effective_available_memory() {
        let mut inventory = inventory();
        inventory.effective_available_bytes = 5_000_003;
        let result = plan_and_allocate(
            &inventory,
            &[],
            MemoryTarget::PercentAvailable(10),
            0,
            1024 * 1024,
            4096,
            LockPolicy::Off,
        )
        .expect("percentage allocation");

        assert_eq!(result.evidence.requested_bytes, 500_000);
        assert_eq!(result.evidence.allocated_unique_bytes, 500_000);
        assert_eq!(result.arenas.len(), 1);
        assert_eq!(result.arenas[0].len(), 500_000);
    }

    #[test]
    fn safe_percentage_preserves_reserve_before_applying_target() {
        let mut inventory = inventory();
        inventory.installed_bytes = 16 << 20;
        inventory.available_at_start_bytes = 12 << 20;
        inventory.free_bytes = 12 << 20;
        inventory.effective_available_bytes = 12 << 20;
        let reserve = 2 << 20;
        let result = plan_and_allocate(
            &inventory,
            &[],
            MemoryTarget::PercentSafe(95),
            reserve,
            1024 * 1024,
            1024 * 1024,
            LockPolicy::Off,
        )
        .expect("safe percentage allocation");
        assert_eq!(result.evidence.safe_limit_bytes, 10 << 20);
        assert_eq!(result.evidence.requested_bytes, (10 << 20) * 95 / 100);
        assert_eq!(result.evidence.target_kind, "percent_safe:95");
    }
}
