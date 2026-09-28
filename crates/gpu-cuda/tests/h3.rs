//! GPU-H3 hardware gates for the async slot pools. Ignored by default: they need a real
//! NVIDIA GPU with CUDA. Run with
//! `cargo test -p gpu-cuda --test h3 -- --ignored --nocapture --test-threads=1`.
use gpu_cuda::{AllocFailpoint, AsyncConfig, CudaBackend, Error, live_resources};
use gpu_engine::plan::SlotPlan;

const JOBS: usize = 64;
const INNER: u32 = 64;

fn open() -> CudaBackend {
    let device = CudaBackend::inventory().unwrap().remove(0);
    CudaBackend::open(device.pci_address.as_deref().unwrap()).unwrap()
}

fn requested() -> AsyncConfig {
    AsyncConfig {
        device_slots: 3,
        host_slots: 6,
        workers: 2,
        batches: 8,
        rotate_every_batches: 0,
        ..AsyncConfig::default()
    }
}

#[test]
#[ignore = "bounded real NVIDIA allocation-failure gate"]
fn h3_allocation_failure_at_every_step_frees_everything_and_backs_off_in_order() {
    let mut backend = open();
    let config = requested();
    backend.configure_async(config).unwrap();
    let baseline = live_resources();
    let steps = config.allocation_steps();
    assert_eq!(steps, 3 + 5 * 3 + 6);

    // Persistent failure: every attempt fails at `step` while its plan still has that step.
    for step in 0..steps {
        backend.set_alloc_failpoint(Some(AllocFailpoint {
            at: step,
            once: false,
        }));
        match backend.prepare_matrix_resources(JOBS, INNER) {
            Err(error) => {
                assert!(matches!(error, Error::Api { status: 2, .. }), "{error}");
                assert_eq!(
                    live_resources(),
                    baseline,
                    "leak after failure at step {step}"
                );
                assert!(backend.async_effective_config().is_none());
            }
            Ok((jobs, backoffs)) => {
                // A smaller plan that no longer has that step fit. It never grows anything.
                let effective = backend.async_effective_config().unwrap();
                assert!(backoffs >= 1, "step {step}");
                assert!(effective.device_slots <= 3 && effective.host_slots <= 6 && jobs <= JOBS);
                assert!(effective.host_slots > effective.device_slots);
                let evidence = backend
                    .verify_matrix_async(7, false, &|| false, None)
                    .unwrap();
                assert!(evidence.results_verified > 0);
                assert_eq!(backend.async_report().unwrap().conservation, "ok");
            }
        }
        backend.release_async_resources();
        assert_eq!(
            live_resources(),
            baseline,
            "resources left after step {step}"
        );
    }

    // One-shot failure: the first attempt dies, the second (one ladder step smaller) works.
    let first_step = SlotPlan {
        jobs: JOBS,
        device_slots: 3,
        host_slots: 6,
    }
    .backoff_step()
    .unwrap();
    for step in 0..steps {
        backend.set_alloc_failpoint(Some(AllocFailpoint {
            at: step,
            once: true,
        }));
        let (jobs, backoffs) = backend.prepare_matrix_resources(JOBS, INNER).unwrap();
        assert_eq!(backoffs, 1, "step {step}");
        let effective = backend.async_effective_config().unwrap();
        assert_eq!(
            (jobs, effective.device_slots, effective.host_slots),
            (
                first_step.jobs,
                first_step.device_slots,
                first_step.host_slots
            ),
            "queue depth is shed first: step {step}"
        );
        backend.release_async_resources();
        assert_eq!(live_resources(), baseline, "one-shot step {step}");
    }

    // A failure on the very first step of every plan exhausts the ladder: an error, no leak.
    backend.set_alloc_failpoint(Some(AllocFailpoint { at: 0, once: false }));
    assert!(backend.prepare_matrix_resources(JOBS, INNER).is_err());
    assert_eq!(live_resources(), baseline);

    // Recovery with no failpoint gives the full request and a working pipeline.
    backend.set_alloc_failpoint(None);
    let (jobs, backoffs) = backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    assert_eq!((jobs, backoffs), (JOBS, 0));
    let effective = backend.async_effective_config().unwrap();
    assert_eq!((effective.device_slots, effective.host_slots), (3, 6));
    backend
        .verify_matrix_async(9, false, &|| false, None)
        .unwrap();
    backend.release_async_resources();
    assert_eq!(live_resources(), baseline);
}

#[test]
#[ignore = "bounded real NVIDIA slot reuse gate"]
fn h3_slots_wrap_around_many_times_without_reuse_before_completion() {
    let mut backend = open();
    backend
        .configure_async(AsyncConfig {
            device_slots: 2,
            host_slots: 3,
            quarantine_slots: 1,
            batches: 32,
            ..requested()
        })
        .unwrap();
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    for burst in 0..5 {
        backend
            .verify_matrix_async(21, false, &|| false, None)
            .unwrap_or_else(|e| panic!("burst {burst}: {e}"));
    }
    let report = backend.async_report().unwrap();
    // 160 batches through 3 host slots and 2 device slots: every slot was reused dozens of
    // times, and each reuse was preceded by a verified completion.
    assert_eq!(report.cumulative.totals.verified, 160);
    assert_eq!(report.cumulative.totals.failed, 0);
    assert_eq!(report.conservation, "ok");
    assert!(report.cumulative.device_in_flight_peak <= 2);
    assert!(report.cumulative.host_slots_used_peak <= 3);
    assert_eq!(
        report.cumulative.device_in_flight_peak, 2,
        "the GPU did overlap"
    );
    assert!(report.pending.is_empty());
}

#[test]
#[ignore = "bounded real NVIDIA quarantine gate"]
fn h3_failed_batch_is_quarantined_then_released_and_the_slot_is_reusable() {
    let mut backend = open();
    backend.configure_async(requested()).unwrap();
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    // The injected corruption fails the first batch of the burst.
    assert!(
        backend
            .verify_matrix_async(5, true, &|| false, None)
            .is_err()
    );
    let report = backend.async_report().unwrap();
    let totals = &report.cumulative.totals;
    assert_eq!((totals.failed, totals.quarantined), (1, 1));
    assert_eq!(
        (totals.quarantine_released, totals.quarantine_expired),
        (1, 0)
    );
    assert_eq!(totals.quarantine_refused, 0);
    assert_eq!(report.failure_attribution.as_deref(), Some("corpus_intact"));
    assert_eq!(report.conservation, "ok");
    assert!(report.pending.is_empty(), "nothing is held past the burst");
    // The held slot is free again: a clean burst on the same pipeline passes in full.
    backend
        .verify_matrix_async(5, false, &|| false, None)
        .unwrap();
    let report = backend.async_report().unwrap();
    assert_eq!(report.cumulative.totals.failed, 1);
    assert!(report.cumulative.totals.verified >= 8);
    assert_eq!(report.conservation, "ok");
}
