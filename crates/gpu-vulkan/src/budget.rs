//! Conservative sequential workspace reservation (one compute slot).
use gpu_engine::plan::SlotPlan;
use serde::Serialize;
const MIB: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
pub struct WorkspaceBudget {
    pub version: u32,
    pub device_available_bytes: u64,
    pub host_available_bytes: u64,
    pub reserve_device_bytes: u64,
    pub reserve_host_bytes: u64,
    pub matrix_jobs_requested: usize,
    pub matrix_jobs_effective: usize,
    pub matrix_device_bytes: u64,
    pub matrix_host_bytes: u64,
    pub transfer_chunk_bytes: u64,
    pub pool_bytes: u64,
    pub pool_requested_bytes: u64,
    pub backoff_steps: u64,
    /// Async plans only: effective slot counts after backoff, and the host RAM the pinned
    /// readback queue and the prefetched corpus account for inside `matrix_host_bytes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_device_slots: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub async_host_slots: Option<usize>,
    pub pinned_queue_bytes: u64,
    pub corpus_prefetch_bytes: u64,
    /// GPU-H6: pinned staging the transfer lane holds (upload + download + flag per slot),
    /// already included in `matrix_host_bytes`; 0 when the lane is off.
    pub lane_pinned_bytes: u64,
    /// v1.2 C13 G2: corpora the async pipeline keeps besides the one in use, served when the
    /// next one is not built in time, and their host RAM (inside `matrix_host_bytes`).
    pub corpus_pool: usize,
    pub corpus_pool_bytes: u64,
}

impl WorkspaceBudget {
    /// Includes conservative alignment/driver reserve and four host chunk copies.
    /// Integrated devices share the same available host budget.
    ///
    /// # Errors
    /// Rejects insufficient budgets; never substitutes host RAM for dedicated VRAM.
    pub fn plan(
        device: u64,
        host: u64,
        pool: u64,
        jobs: usize,
        integrated: bool,
        inner: u32,
    ) -> Result<Self, &'static str> {
        Self::plan_with_slots(device, host, pool, jobs, integrated, inner, 1)
    }

    /// Reserve all async device/pinned output slots before allocating the VRAM pool.
    /// # Errors
    /// Rejects invalid slot counts or insufficient host/device budgets.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_with_slots(
        device: u64,
        host: u64,
        pool: u64,
        jobs: usize,
        integrated: bool,
        inner: u32,
        slots: usize,
    ) -> Result<Self, &'static str> {
        Self::plan_with_options(device, host, pool, jobs, integrated, inner, slots, false)
    }

    /// As [`Self::plan_with_slots`]; `prefetch_corpus` also reserves the host RAM of a
    /// second corpus (input words) that is built in the background for rotation.
    ///
    /// # Errors
    /// Rejects invalid slot counts or insufficient host/device budgets.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_with_options(
        device: u64,
        host: u64,
        pool: u64,
        jobs: usize,
        integrated: bool,
        inner: u32,
        slots: usize,
        prefetch_corpus: bool,
    ) -> Result<Self, &'static str> {
        if !(1..=16).contains(&slots) {
            return Err("invalid workspace slots");
        }
        if !(1..=gpu_engine::gemm::MAX_K).contains(&inner) {
            return Err("matrix inner dimension outside contract");
        }
        if jobs == 0 || jobs > 65_536 {
            return Err("matrix jobs outside contract");
        }
        let device_reserve = (device / 20).max(32 * MIB);
        let host_reserve = (host / 10).max(128 * MIB);
        let device_free = device.saturating_sub(device_reserve);
        let host_free = host.saturating_sub(host_reserve);
        let mut effective = jobs;
        let mut backoff = 0;
        loop {
            let (input, output, host_workspace) = gpu_engine::gemm::workspace_per_job(inner);
            let matrix_device = effective as u64 * (input + output * slots as u64);
            // Staging + oracle/input + byte packing/readback/hash temporaries.
            let prefetch = if prefetch_corpus { input } else { 0 };
            let matrix_host =
                effective as u64 * (host_workspace + prefetch + 2 * output * (slots as u64 - 1));
            let local_free = device_free.saturating_sub(matrix_device);
            let host_free = host_free
                .saturating_sub(matrix_host)
                .saturating_sub(if integrated { matrix_device } else { 0 });
            let chosen_pool =
                pool.min(local_free)
                    .min(if integrated { host_free / 2 } else { u64::MAX });
            let staging_free = host_free.saturating_sub(if integrated { chosen_pool } else { 0 });
            let chunk = (staging_free / 4).min(chosen_pool).min(64 * MIB) / 4096 * 4096;
            if chosen_pool >= 4 * MIB
                && chunk >= 4096
                && matrix_device < device_free
                && matrix_host < host.saturating_sub(host_reserve)
            {
                return Ok(Self {
                    version: 1,
                    device_available_bytes: device,
                    host_available_bytes: host,
                    reserve_device_bytes: device_reserve,
                    reserve_host_bytes: host_reserve,
                    matrix_jobs_requested: jobs,
                    matrix_jobs_effective: effective,
                    matrix_device_bytes: matrix_device,
                    matrix_host_bytes: matrix_host,
                    transfer_chunk_bytes: chunk,
                    pool_bytes: chosen_pool,
                    pool_requested_bytes: pool,
                    backoff_steps: backoff,
                    async_device_slots: None,
                    async_host_slots: None,
                    pinned_queue_bytes: 0,
                    corpus_prefetch_bytes: 0,
                    corpus_pool: 0,
                    corpus_pool_bytes: 0,
                    lane_pinned_bytes: 0,
                });
            }
            if effective == 1 {
                return Err("insufficient host/device workspace reserve");
            }
            effective = (effective / 2).max(1);
            backoff += 1;
        }
    }

    /// Device bytes, host bytes, pinned queue bytes and prefetch bytes of an async plan.
    /// The host value already contains the pinned queue and the prefetch.
    #[must_use]
    pub fn async_footprint(
        inner: u32,
        plan: SlotPlan,
        prefetch_corpus: bool,
    ) -> (u64, u64, u64, u64) {
        Self::async_footprint_pooled(inner, plan, prefetch_corpus, 0)
    }

    /// Host RAM of one corpus (input and expected results) of `jobs` jobs at `inner`.
    #[must_use]
    pub fn corpus_host_bytes(inner: u32, jobs: usize) -> u64 {
        let (input, _, _) = gpu_engine::gemm::workspace_per_job(inner);
        let expected = (gpu_engine::gemm::MAX_N * gpu_engine::gemm::MAX_N * 4) as u64;
        jobs as u64 * (input + expected)
    }

    /// As [`Self::async_footprint`], with `corpus_pool` more corpora kept in host RAM; the host
    /// value contains them too.
    #[must_use]
    pub fn async_footprint_pooled(
        inner: u32,
        plan: SlotPlan,
        prefetch_corpus: bool,
        corpus_pool: usize,
    ) -> (u64, u64, u64, u64) {
        let (input, output, _) = gpu_engine::gemm::workspace_per_job(inner);
        let corpus = Self::corpus_host_bytes(inner, plan.jobs);
        let jobs = plan.jobs as u64;
        let device = jobs * (input + output * plan.device_slots as u64);
        let pinned_queue = jobs * output * plan.host_slots as u64;
        let prefetch = if prefetch_corpus { corpus } else { 0 };
        // The corpus in use plus its pinned upload staging (another input's worth).
        let host = corpus + jobs * input + pinned_queue + prefetch + corpus_pool as u64 * corpus;
        (device, host, pinned_queue, prefetch)
    }

    /// Budget for the async pipeline, with separate device and host slot pools.
    ///
    /// Host RAM is counted for what the pipeline really holds: the corpus input and the
    /// pinned upload staging (2x input), the expected results, the pinned readback queue
    /// (`host_slots` outputs) and, when rotating, the prefetched second corpus. The
    /// sequential path's temporary copies do not exist here, so the estimate is tighter
    /// and backs off less. Under pressure the [`SlotPlan`] ladder sheds queue depth first,
    /// then GPU overlap, then jobs; it never grows anything.
    ///
    /// # Errors
    /// Rejects invalid requests, or budgets that cannot hold even the smallest plan.
    #[allow(clippy::too_many_arguments)]
    pub fn plan_async(
        device: u64,
        host: u64,
        pool: u64,
        integrated: bool,
        inner: u32,
        request: SlotPlan,
        prefetch_corpus: bool,
        lane_pinned_bytes: u64,
    ) -> Result<Self, &'static str> {
        Self::plan_async_pooled(
            device,
            host,
            pool,
            integrated,
            inner,
            request,
            prefetch_corpus,
            lane_pinned_bytes,
            0,
        )
    }

    /// As [`Self::plan_async`], also keeping up to `corpus_pool` more corpora in host RAM (v1.2
    /// C13 G2), served when the next corpus is not built in time. Short of memory the pool is
    /// given up first, one corpus at a time, before the slot ladder starts.
    ///
    /// # Errors
    /// As [`Self::plan_async`].
    #[allow(clippy::too_many_arguments)]
    pub fn plan_async_pooled(
        device: u64,
        host: u64,
        pool: u64,
        integrated: bool,
        inner: u32,
        request: SlotPlan,
        prefetch_corpus: bool,
        lane_pinned_bytes: u64,
        corpus_pool: usize,
    ) -> Result<Self, &'static str> {
        if !(1..=gpu_engine::gemm::MAX_K).contains(&inner) {
            return Err("matrix inner dimension outside contract");
        }
        if request.jobs == 0 || request.jobs > 65_536 {
            return Err("matrix jobs outside contract");
        }
        if request.device_slots < gpu_engine::plan::MIN_DEVICE_SLOTS
            || request.host_slots <= request.device_slots
            || request.host_slots > 64
        {
            return Err("invalid async slot plan");
        }
        let device_reserve = (device / 20).max(32 * MIB);
        let host_reserve = (host / 10).max(128 * MIB);
        let device_free = device.saturating_sub(device_reserve);
        let host_budget = host.saturating_sub(host_reserve);
        let attempts = (1..=corpus_pool)
            .rev()
            .map(|kept| (request, kept))
            .chain(request.ladder().into_iter().map(|plan| (plan, 0)));
        for (step, (plan, kept)) in attempts.enumerate() {
            let (matrix_device, matrix_host, pinned_queue, prefetch) =
                Self::async_footprint_pooled(inner, plan, prefetch_corpus, kept);
            let matrix_host = matrix_host + lane_pinned_bytes;
            let local_free = device_free.saturating_sub(matrix_device);
            let host_free = host_budget
                .saturating_sub(matrix_host)
                .saturating_sub(if integrated { matrix_device } else { 0 });
            let chosen_pool =
                pool.min(local_free)
                    .min(if integrated { host_free / 2 } else { u64::MAX });
            let staging_free = host_free.saturating_sub(if integrated { chosen_pool } else { 0 });
            let chunk = (staging_free / 4).min(chosen_pool).min(64 * MIB) / 4096 * 4096;
            if chosen_pool >= 4 * MIB
                && chunk >= 4096
                && matrix_device < device_free
                && matrix_host < host_budget
            {
                return Ok(Self {
                    version: 1,
                    device_available_bytes: device,
                    host_available_bytes: host,
                    reserve_device_bytes: device_reserve,
                    reserve_host_bytes: host_reserve,
                    matrix_jobs_requested: request.jobs,
                    matrix_jobs_effective: plan.jobs,
                    matrix_device_bytes: matrix_device,
                    matrix_host_bytes: matrix_host,
                    transfer_chunk_bytes: chunk,
                    pool_bytes: chosen_pool,
                    pool_requested_bytes: pool,
                    backoff_steps: step as u64,
                    async_device_slots: Some(plan.device_slots),
                    async_host_slots: Some(plan.host_slots),
                    pinned_queue_bytes: pinned_queue,
                    corpus_prefetch_bytes: prefetch,
                    lane_pinned_bytes,
                    corpus_pool: kept,
                    corpus_pool_bytes: kept as u64 * Self::corpus_host_bytes(inner, plan.jobs),
                });
            }
        }
        Err("insufficient host/device workspace reserve")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pressure_reduces_jobs_without_overcommitting_workspace() {
        let b = WorkspaceBudget::plan(64 * MIB, 160 * MIB, 48 * MIB, 65_536, false, 2).unwrap();
        assert!(b.backoff_steps > 0);
        assert!(b.pool_bytes + b.matrix_device_bytes + b.reserve_device_bytes <= 64 * MIB);
        assert!(
            b.matrix_host_bytes + 4 * b.transfer_chunk_bytes + b.reserve_host_bytes <= 160 * MIB
        );
    }
    #[test]
    fn shared_memory_is_not_counted_twice_and_tiny_budgets_fail() {
        let b = WorkspaceBudget::plan(512 * MIB, 512 * MIB, 400 * MIB, 65_536, true, 2).unwrap();
        assert!(
            b.pool_bytes
                + b.matrix_device_bytes
                + b.matrix_host_bytes
                + 4 * b.transfer_chunk_bytes
                + b.reserve_host_bytes
                <= 512 * MIB
        );
        assert!(WorkspaceBudget::plan(MIB, MIB, MIB, 1, false, 2).is_err());
    }

    #[test]
    fn corpus_prefetch_reserves_a_second_corpus_of_host_memory() {
        let plain =
            WorkspaceBudget::plan_with_slots(512 * MIB, 4096 * MIB, 64 * MIB, 64, false, 256, 4)
                .unwrap();
        let rotating = WorkspaceBudget::plan_with_options(
            512 * MIB,
            4096 * MIB,
            64 * MIB,
            64,
            false,
            256,
            4,
            true,
        )
        .unwrap();
        let (input, _, _) = gpu_engine::gemm::workspace_per_job(256);
        assert_eq!(
            rotating.matrix_host_bytes - plain.matrix_host_bytes,
            64 * input
        );
        // Too little host memory for a second corpus backs jobs off instead of overcommitting.
        let tight = WorkspaceBudget::plan_with_options(
            512 * MIB,
            700 * MIB,
            64 * MIB,
            4096,
            false,
            256,
            4,
            true,
        )
        .unwrap();
        assert!(tight.matrix_jobs_effective < 4096 && tight.backoff_steps > 0);
    }

    #[test]
    fn the_corpus_pool_is_held_when_it_fits_and_given_up_first_when_not() {
        // The RTX 5080 overlap shape: 512 jobs at K 1024 (jobs x K stays as K grows).
        let corpus = WorkspaceBudget::corpus_host_bytes(1024, 512);
        let request = ask(512, 3, 8);
        let plan = |host: u64, pool: usize| {
            WorkspaceBudget::plan_async_pooled(
                16 * 1024 * MIB,
                host,
                8 * 1024 * MIB,
                false,
                1024,
                request,
                true,
                32 * MIB,
                pool,
            )
        };
        let without = plan(2048 * MIB, 0).unwrap();
        let with = plan(2048 * MIB, 3).unwrap();
        assert_eq!((with.corpus_pool, with.backoff_steps), (3, 0));
        assert_eq!(with.corpus_pool_bytes, 3 * corpus);
        assert_eq!(
            with.matrix_host_bytes,
            without.matrix_host_bytes + 3 * corpus
        );
        assert_eq!(without.corpus_pool, 0);
        // Short of host RAM: the pool shrinks, one corpus at a time, before anything else.
        let host = without.matrix_host_bytes + corpus + corpus / 2;
        let budget = host + host / 9 + MIB;
        let tight = plan(budget, 3).unwrap();
        assert_eq!(tight.corpus_pool, 1, "{tight:?}");
        assert_eq!(tight.backoff_steps, 2);
        assert_eq!(
            (tight.matrix_jobs_effective, tight.async_host_slots),
            (512, Some(8))
        );
        // Not even room for one: no pool, the slots as asked.
        let none = plan(
            without.matrix_host_bytes + without.matrix_host_bytes / 9 + MIB,
            3,
        )
        .unwrap();
        assert_eq!((none.corpus_pool, none.matrix_jobs_effective), (0, 512));
        // The old entry point keeps no pool.
        let old = WorkspaceBudget::plan_async(
            16 * 1024 * MIB,
            2048 * MIB,
            8 * 1024 * MIB,
            false,
            1024,
            request,
            true,
            32 * MIB,
        )
        .unwrap();
        assert_eq!(old.corpus_pool, 0);
        assert_eq!(old.matrix_host_bytes, without.matrix_host_bytes);
    }

    fn ask(jobs: usize, device: usize, host: usize) -> SlotPlan {
        SlotPlan {
            jobs,
            device_slots: device,
            host_slots: host,
        }
    }

    #[test]
    fn async_plan_counts_the_pinned_queue_and_a_second_corpus_and_needs_less_than_sequential() {
        let (input, output, _) = gpu_engine::gemm::workspace_per_job(256);
        let plan = WorkspaceBudget::plan_async(
            512 * MIB,
            4096 * MIB,
            64 * MIB,
            false,
            256,
            ask(64, 3, 8),
            true,
            0,
        )
        .unwrap();
        assert_eq!(
            (plan.async_device_slots, plan.async_host_slots),
            (Some(3), Some(8))
        );
        assert_eq!(plan.backoff_steps, 0);
        assert_eq!(plan.pinned_queue_bytes, 64 * output * 8);
        assert!(plan.corpus_prefetch_bytes >= 64 * input);
        assert_eq!(plan.matrix_device_bytes, 64 * (input + 3 * output));
        let quiet = WorkspaceBudget::plan_async(
            512 * MIB,
            4096 * MIB,
            64 * MIB,
            false,
            256,
            ask(64, 3, 8),
            false,
            0,
        )
        .unwrap();
        assert_eq!(quiet.corpus_prefetch_bytes, 0);
        assert!(quiet.matrix_host_bytes < plan.matrix_host_bytes);
        // The old sequential-style estimate reserved three input copies plus five outputs per job.
        let old =
            WorkspaceBudget::plan_with_slots(512 * MIB, 4096 * MIB, 64 * MIB, 64, false, 256, 3)
                .unwrap();
        assert!(quiet.matrix_host_bytes < old.matrix_host_bytes);
        assert_eq!(quiet.lane_pinned_bytes, 0);
        // The lane's pinned staging is counted in the host total and reported on its own.
        let with_lane = WorkspaceBudget::plan_async(
            512 * MIB,
            4096 * MIB,
            64 * MIB,
            false,
            256,
            ask(64, 3, 8),
            false,
            16 * MIB,
        )
        .unwrap();
        assert_eq!(with_lane.lane_pinned_bytes, 16 * MIB);
        assert_eq!(
            with_lane.matrix_host_bytes,
            quiet.matrix_host_bytes + 16 * MIB
        );
    }

    #[test]
    fn async_plan_backs_off_queue_before_overlap_before_jobs_and_never_overcommits() {
        let (_, output, _) = gpu_engine::gemm::workspace_per_job(1024);
        // Room for the shape but not for a 16-deep pinned queue.
        let host = 720 * MIB;
        let plan = WorkspaceBudget::plan_async(
            2048 * MIB,
            host,
            64 * MIB,
            false,
            1024,
            ask(512, 4, 16),
            false,
            0,
        )
        .unwrap();
        assert_eq!(plan.matrix_jobs_effective, 512, "shape kept");
        assert_eq!(plan.async_device_slots, Some(4), "overlap kept");
        assert!(
            plan.async_host_slots.unwrap() < 16,
            "queue depth was shed first"
        );
        assert!(plan.backoff_steps > 0);
        assert!(plan.matrix_host_bytes + plan.reserve_host_bytes < host);
        assert_eq!(
            plan.pinned_queue_bytes,
            512 * output * plan.async_host_slots.unwrap() as u64
        );
        // Tighter still: the plan gives up jobs only after both slot pools are at their floor.
        let tight = WorkspaceBudget::plan_async(
            2048 * MIB,
            600 * MIB,
            64 * MIB,
            false,
            1024,
            ask(512, 4, 16),
            false,
            0,
        )
        .unwrap();
        assert!(tight.matrix_jobs_effective < 512);
        assert_eq!(
            (tight.async_device_slots, tight.async_host_slots),
            (Some(2), Some(3))
        );
        assert!(tight.matrix_host_bytes + tight.reserve_host_bytes < 600 * MIB);
    }

    #[test]
    fn async_plan_reports_insufficient_resources_instead_of_growing_the_queue() {
        let error = WorkspaceBudget::plan_async(MIB, MIB, MIB, false, 256, ask(1, 2, 3), false, 0)
            .unwrap_err();
        assert_eq!(error, "insufficient host/device workspace reserve");
        for bad in [ask(0, 2, 3), ask(4, 1, 3), ask(4, 3, 3), ask(4, 2, 65)] {
            assert!(
                WorkspaceBudget::plan_async(
                    512 * MIB,
                    4096 * MIB,
                    64 * MIB,
                    false,
                    256,
                    bad,
                    false,
                    0
                )
                .is_err()
            );
        }
        // An integrated GPU shares host RAM, so the same memory buys less.
        let discrete = WorkspaceBudget::plan_async(
            512 * MIB,
            900 * MIB,
            200 * MIB,
            false,
            256,
            ask(64, 3, 8),
            false,
            0,
        )
        .unwrap();
        let shared = WorkspaceBudget::plan_async(
            512 * MIB,
            900 * MIB,
            200 * MIB,
            true,
            256,
            ask(64, 3, 8),
            false,
            0,
        )
        .unwrap();
        assert!(shared.pool_bytes <= discrete.pool_bytes);
    }
}
