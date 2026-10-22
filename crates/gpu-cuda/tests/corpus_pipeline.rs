//! Corpus pipeline gates (v1.2 C13 G3): the periodic host-corpus check runs on a helper thread and
//! still reports a change, and the next corpus is staged ahead without weakening any check.
//! Ignored by default: they need a real NVIDIA GPU with CUDA. Run with
//! `cargo test -p gpu-cuda --test corpus_pipeline -- --ignored --nocapture --test-threads=1`.
use gpu_cuda::{ArithMix, AsyncConfig, CudaBackend};

const JOBS: usize = 64;
const INNER: u32 = 1024;

fn prepared(config: AsyncConfig) -> CudaBackend {
    let device = CudaBackend::inventory().unwrap().remove(0);
    let mut backend = CudaBackend::open(device.pci_address.as_deref().unwrap()).unwrap();
    backend.configure_async(config).unwrap();
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    backend
}

#[test]
#[ignore = "bounded real NVIDIA host-corpus gate"]
fn a_host_corpus_change_is_found_by_the_background_check() {
    // One corpus (no rotation), checked every burst: the device copy stays right, so no batch
    // fails and only the host check can see the change.
    let backend = prepared(AsyncConfig {
        batches: 8,
        rotate_every_batches: 0,
        integrity_every_batches: 8,
        ..AsyncConfig::default()
    });
    backend
        .verify_matrix_async(5, false, &|| false, None)
        .unwrap();
    backend.inject_host_corpus_fault().unwrap();
    let mut found = None;
    for _ in 0..20 {
        if let Err(error) = backend.verify_matrix_async(5, false, &|| false, None) {
            found = Some(error.to_string());
            break;
        }
    }
    if let Err(error) = backend.finish_async() {
        found.get_or_insert(error.to_string());
    }
    let found = found.expect("a changed host corpus must not pass");
    assert!(found.contains("host corpus integrity"), "{found}");
    let report = backend.async_report().unwrap();
    let c = &report.cumulative;
    assert_eq!(c.totals.failed, 0, "the device copy was right");
    // Each later check finds the change again (the one started after the first finding, too).
    assert!(c.integrity_failures >= 1, "{}", c.integrity_failures);
    assert!(c.integrity_checks >= 2, "{}", c.integrity_checks);
    assert_eq!(
        report.failure_attribution.as_deref(),
        Some("host_memory_changed")
    );
    // Every check ran beside the bursts: the owner thread never waited for one.
    assert!(c.timing.integrity_check_seconds.abs() < f64::EPSILON);
    assert!(c.timing.integrity_background_seconds > 0.0);
}

#[test]
#[ignore = "bounded real NVIDIA corpus staging gate"]
fn staged_corpora_are_verified_like_any_other_and_a_fault_still_fails() {
    // A rotation every burst with a pool: the corpus each rotation takes is staged during the
    // burst before it.
    let backend = prepared(AsyncConfig {
        batches: 8,
        rotate_every_batches: 8,
        corpus_pool: 3,
        arith: ArithMix::Alternate,
        // Fixed bursts: one rotation each, as the premise says (timed bursts: the gate below).
        burst_target_ms: 0,
        ..AsyncConfig::default()
    });
    for _ in 0..16 {
        backend
            .verify_matrix_async(9, false, &|| false, None)
            .unwrap();
    }
    backend.finish_async().unwrap();
    let c = backend.async_report().unwrap().cumulative;
    assert_eq!(c.totals.failed, 0, "{:?}", c.mismatch_samples);
    assert_eq!(c.totals.verified, c.totals.submitted);
    assert!(
        c.corpus_staged_installs >= 2,
        "{}",
        c.corpus_staged_installs
    );
    assert!(
        c.u32_results_verified > 0 && c.f32_results_verified > 0,
        "u32 {} fp32 {}",
        c.u32_results_verified,
        c.f32_results_verified
    );
    assert!(
        backend
            .verify_matrix_async(9, true, &|| false, None)
            .is_err(),
        "a flipped result bit must not pass on a staged corpus"
    );
    assert_eq!(backend.async_report().unwrap().cumulative.totals.failed, 1);
}

#[test]
#[ignore = "bounded real NVIDIA corpus staging gate with timed bursts"]
fn timed_bursts_leave_a_boundary_inside_every_corpus_for_staging() {
    // v1.2 C16c/C9: bursts grow with the batch time but stay within half a corpus's life, so
    // the next corpus can be staged at a boundary in the middle of it (with one burst per
    // corpus it was never built yet there).
    let backend = prepared(AsyncConfig {
        batches: 8,
        rotate_every_batches: 64,
        corpus_pool: 3,
        arith: ArithMix::Alternate,
        ..AsyncConfig::default()
    });
    for _ in 0..24 {
        backend
            .verify_matrix_async(31, false, &|| false, None)
            .unwrap();
    }
    backend.finish_async().unwrap();
    let c = backend.async_report().unwrap().cumulative;
    assert_eq!(c.totals.failed, 0, "{:?}", c.mismatch_samples);
    let last = c.last_burst.as_ref().map_or(0, |b| b.totals.submitted);
    assert!(last <= 32, "a burst is at most half a corpus: {last}");
    assert!(
        c.corpus_staged_installs >= 2,
        "staged {} of {} rotations",
        c.corpus_staged_installs,
        c.corpus_rotations + c.corpus_pool_serves
    );
}
