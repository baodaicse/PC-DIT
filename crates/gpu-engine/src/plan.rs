//! GPU-H3 shared planning: how the async pipeline shrinks under resource pressure
//! and how CPU cores are shared between GPUs. Pure logic, so the same rules drive the
//! budget planner, the CUDA allocation backoff and the multi-GPU parent.
use serde::Serialize;

/// Fewest device slots that still overlap a kernel with the previous batch's copy.
pub const MIN_DEVICE_SLOTS: usize = 2;

/// Shape of the async resources. Device slots hold kernel output until the D2H copy
/// completes; host slots hold pinned readback until the CPU verdict, so the host
/// queue can be deeper than the number of batches on the GPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SlotPlan {
    pub jobs: usize,
    pub device_slots: usize,
    pub host_slots: usize,
}

impl SlotPlan {
    /// One step of the fixed backoff order, or `None` at the floor. Queue depth is
    /// given up first (least visible to the workload), then GPU overlap, and only
    /// then the batch shape. Never increases anything.
    #[must_use]
    pub fn backoff_step(self) -> Option<Self> {
        let floor_host = self.device_slots + 1;
        if self.host_slots > floor_host {
            // Halve the queue depth above the floor.
            let host_slots = floor_host.max(self.host_slots.midpoint(floor_host));
            return Some(Self { host_slots, ..self });
        }
        if self.device_slots > MIN_DEVICE_SLOTS {
            let device_slots = self.device_slots - 1;
            // At this point the queue is already at its floor, which follows the device slots.
            return Some(Self {
                device_slots,
                host_slots: device_slots + 1,
                ..self
            });
        }
        if self.jobs > 1 {
            return Some(Self {
                jobs: (self.jobs / 2).max(1),
                ..self
            });
        }
        None
    }

    /// Every plan reachable by repeated backoff, starting with `self`.
    #[must_use]
    pub fn ladder(self) -> Vec<Self> {
        let mut steps = vec![self];
        while let Some(next) = steps.last().copied().and_then(Self::backoff_step) {
            steps.push(next);
        }
        steps
    }
}

/// CPU cores one GPU process may use: verifier workers plus helper threads for
/// corpus builds. The owner thread of each GPU is counted too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct CpuShare {
    pub total_cores: usize,
    pub reserved_cores: usize,
    pub gpus: usize,
    pub cores_per_gpu: usize,
    pub verifier_workers: usize,
    pub helper_threads: usize,
}

/// Splits the machine between `gpus` processes. A quarter of the cores (at least two)
/// stay free for the OS, UI, watchdog and telemetry, as long as at least one core
/// remains. `requested_workers` is a ceiling, never a target: a share below it wins.
#[must_use]
pub fn cpu_share(total_cores: usize, gpus: usize, requested_workers: usize) -> CpuShare {
    let total = total_cores.max(1);
    let gpus = gpus.max(1);
    let reserved = (total / 4).max(2).min(total - 1);
    let budget = (total - reserved).max(1);
    let per_gpu = (budget / gpus).max(1);
    let workers = requested_workers.clamp(1, per_gpu);
    // Helpers run while the verifiers and the owner thread are busy.
    let helpers = per_gpu.saturating_sub(workers + 1).clamp(1, 4);
    CpuShare {
        total_cores: total,
        reserved_cores: reserved,
        gpus,
        cores_per_gpu: per_gpu,
        verifier_workers: workers,
        helper_threads: helpers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_only_shrinks_in_the_documented_order_and_reaches_a_floor() {
        let start = SlotPlan {
            jobs: 1024,
            device_slots: 4,
            host_slots: 12,
        };
        let ladder = start.ladder();
        assert_eq!(ladder[0], start);
        for pair in ladder.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                b.jobs <= a.jobs
                    && b.device_slots <= a.device_slots
                    && b.host_slots <= a.host_slots
            );
            assert_ne!(a, b, "every step must change something");
            // Order: host depth, then device slots, then jobs; never two at once.
            let changed = [
                a.host_slots != b.host_slots,
                a.device_slots != b.device_slots,
                a.jobs != b.jobs,
            ];
            assert!(changed.iter().filter(|c| **c).count() >= 1);
            if a.host_slots > a.device_slots + 1 {
                assert_eq!((b.device_slots, b.jobs), (a.device_slots, a.jobs));
            }
            if b.jobs != a.jobs {
                assert_eq!(
                    (a.device_slots, a.host_slots),
                    (MIN_DEVICE_SLOTS, MIN_DEVICE_SLOTS + 1)
                );
            }
            assert!(b.host_slots > b.device_slots && b.device_slots >= MIN_DEVICE_SLOTS);
        }
        let floor = *ladder.last().unwrap();
        assert_eq!(
            floor,
            SlotPlan {
                jobs: 1,
                device_slots: MIN_DEVICE_SLOTS,
                host_slots: MIN_DEVICE_SLOTS + 1
            }
        );
        assert_eq!(floor.backoff_step(), None);
        // The queue is shed before the GPU overlap or the shape is touched.
        assert_eq!(ladder[1].device_slots, 4);
        assert_eq!(ladder[1].jobs, 1024);
        assert!(ladder.len() < 40, "backoff must terminate quickly");
    }

    #[test]
    fn a_plan_already_at_the_floor_never_moves() {
        let floor = SlotPlan {
            jobs: 1,
            device_slots: 2,
            host_slots: 3,
        };
        assert_eq!(floor.ladder(), vec![floor]);
    }

    #[test]
    fn cpu_share_reserves_headroom_never_starves_and_only_lowers_the_requested_workers() {
        let one = cpu_share(16, 1, 2);
        assert_eq!(
            (one.reserved_cores, one.cores_per_gpu, one.verifier_workers),
            (4, 12, 2)
        );
        assert_eq!(one.helper_threads, 4);
        let two = cpu_share(8, 2, 4);
        assert_eq!((two.reserved_cores, two.cores_per_gpu), (2, 3));
        assert_eq!(two.verifier_workers, 3, "the share caps the request");
        assert_eq!(two.helper_threads, 1);
        for (cores, gpus, want) in [(1, 1, 2), (2, 4, 4), (4, 1, 8), (64, 8, 2), (3, 3, 1)] {
            let s = cpu_share(cores, gpus, want);
            assert!(s.verifier_workers >= 1 && s.helper_threads >= 1, "{s:?}");
            assert!(s.verifier_workers <= want.max(1));
            assert!(s.reserved_cores < cores.max(2), "{s:?}");
            // Reasonable machines never exceed the budget: workers + owner within the share.
            if cores >= 8 {
                assert!(s.gpus * (s.verifier_workers + 1) <= cores, "{s:?}");
            }
        }
        // More GPUs never gives a single GPU more cores.
        assert!(cpu_share(16, 4, 4).verifier_workers <= cpu_share(16, 1, 4).verifier_workers);
    }
}
