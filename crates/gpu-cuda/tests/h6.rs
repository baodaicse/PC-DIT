//! GPU-H6 hardware gates for the overlapped transfer lane and the GPU-local VRAM workload.
//! Ignored by default: they need a real NVIDIA GPU with CUDA. Run with
//! `cargo test -p gpu-cuda --test h6 -- --ignored --nocapture --test-threads=1`.
#![allow(clippy::cast_precision_loss)]
use gpu_cuda::{AllocFailpoint, AsyncConfig, CudaBackend, LaneFault, SweepConfig, live_resources};
use gpu_engine::lane::LaneConfig;
use std::time::{Duration, Instant};

const JOBS: usize = 128;
const INNER: u32 = 256;
const MIB: u64 = 1024 * 1024;

fn open() -> CudaBackend {
    let device = CudaBackend::inventory().unwrap().remove(0);
    CudaBackend::open(device.pci_address.as_deref().unwrap()).unwrap()
}

fn lane(vram_local: bool) -> LaneConfig {
    LaneConfig {
        enabled: true,
        vram_local,
        ..LaneConfig::default()
    }
}

fn prepared(lane: LaneConfig, pool_mib: u64) -> CudaBackend {
    let mut backend = open();
    backend
        .configure_async(AsyncConfig {
            batches: 8,
            rotate_every_batches: 0,
            lane,
            ..AsyncConfig::default()
        })
        .unwrap();
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    backend.allocate_pool(pool_mib * MIB, 32 * MIB).unwrap();
    backend
}

fn no_stop() -> bool {
    false
}

#[test]
#[ignore = "bounded real NVIDIA lane gate"]
fn h6_lane_overlaps_compute_and_every_byte_it_moves_is_verified() {
    let backend = prepared(lane(true), 64);
    let started = Instant::now();
    let mut seed = 3;
    // Long enough for every systematic pattern class (one generation in eight) to come round.
    while started.elapsed() < Duration::from_secs(6)
        || backend
            .lane_report()
            .is_none_or(|r| r.transfer.write_ops < 56)
    {
        backend
            .verify_matrix_async(seed, false, &no_stop, None)
            .unwrap();
        seed += 1;
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "the lane is too slow"
        );
    }
    backend.flush_lane().unwrap();
    assert!(backend.take_lane_failures().is_empty());
    let report = backend.lane_report().unwrap();
    let async_report = backend.async_report().unwrap();
    let t = &report.transfer;
    let v = &report.vram;
    eprintln!("{report:#?}");
    assert_eq!(report.conservation, "ok", "{report:#?}");
    assert_eq!(report.failures_total, 0);
    assert!(t.write_ops > 20 && t.reread_ops > 20, "{t:?}");
    // Both directions are counted separately and agree with what the operations did.
    assert!(t.h2d_bytes > 0 && t.d2h_bytes >= t.h2d_bytes, "{t:?}");
    assert_eq!(t.h2d_ops, t.write_ops);
    assert_eq!(t.d2h_ops, t.write_ops + t.reread_ops);
    // Every byte moved was compared with the pattern: the flush leaves nothing pending.
    assert_eq!(t.verified_bytes, t.d2h_bytes, "{t:?}");
    assert_eq!(t.pending_rereads, 0);
    assert_eq!(
        t.pattern_counts.iter().sum::<u64>(),
        t.write_ops + t.reread_ops
    );
    assert!(
        t.pattern_counts.iter().all(|c| *c > 0),
        "{:?}",
        t.pattern_counts
    );
    // GPU-local work: touched bytes are activity, checkpoints are coverage.
    assert!(v.write_ops > 10 && v.checkpoint_ops > 5, "{v:?}");
    assert!(
        v.touched_bytes > v.cpu_verified_bytes && v.cpu_verified_bytes > 0,
        "{v:?}"
    );
    assert_eq!(v.self_check_bad_words, 0);
    assert_eq!(v.pending_checkpoints, 0);
    // The point of the lane: it runs while compute runs.
    assert!(
        report.overlap_with_compute_seconds > 0.05 * report.busy_wall_seconds,
        "overlap {:.3} s of {:.3} s lane time",
        report.overlap_with_compute_seconds,
        report.busy_wall_seconds
    );
    // Neither class was starved of verifier CPU.
    assert!(async_report.fairness.compute_items > 0 && async_report.fairness.lane_items > 0);
    assert_eq!(async_report.conservation, "ok");
}

#[test]
#[ignore = "bounded real NVIDIA negative gate"]
fn h6_each_injected_lane_fault_is_reported_once_with_its_phase() {
    for (fault, phase) in [
        (LaneFault::Transfer, "transfer"),
        (LaneFault::Retention, "retention"),
        (LaneFault::VramSelfCheck, "vram-self-check"),
        (LaneFault::VramCheckpoint, "vram-checkpoint"),
    ] {
        let backend = prepared(lane(true), 64);
        backend.inject_lane_fault(fault).unwrap();
        for seed in 1..=4 {
            backend
                .verify_matrix_async(seed, false, &no_stop, None)
                .unwrap();
        }
        backend.flush_lane().unwrap();
        let failures = backend.take_lane_failures();
        assert_eq!(failures.len(), 1, "{fault:?}: {failures:?}");
        assert_eq!(failures[0].phase, phase, "{fault:?}");
        let report = backend.lane_report().unwrap();
        assert_eq!(report.failures_total, 1, "{fault:?}");
        assert_eq!(report.ops_failed, 1, "{fault:?}");
        assert_eq!(report.conservation, "ok");
        // The lane keeps working after a failure.
        assert!(report.ops_completed > 10, "{fault:?}: {report:?}");
    }
}

#[test]
#[ignore = "bounded real NVIDIA transfer-only gate"]
fn h6_transfer_only_slice_runs_the_lane_without_compute_and_the_rate_cap_holds() {
    let mut cfg = lane(false);
    cfg.rate_bytes_per_second = 200 * MIB;
    let backend = prepared(cfg, 64);
    let started = Instant::now();
    let evidence = backend
        .run_lane_slice(5, &no_stop, Duration::from_secs(4))
        .unwrap();
    let wall = started.elapsed().as_secs_f64();
    assert_eq!(evidence.results_verified, 0, "no compute ran");
    backend.flush_lane().unwrap();
    assert!(backend.take_lane_failures().is_empty());
    let t = backend.lane_report().unwrap().transfer;
    let moved = (t.h2d_bytes + t.d2h_bytes) as f64 / MIB as f64;
    let rate = moved / wall;
    // 200 MiB/s cap over both directions, plus the burst allowance and the final flush.
    assert!(
        (100.0..=260.0).contains(&rate),
        "{rate:.0} MiB/s ({moved:.0} MiB in {wall:.1} s)"
    );
    assert!(t.write_ops > 5);
}

#[test]
#[ignore = "bounded real NVIDIA allocation-failure gate"]
fn h6_lane_allocation_failure_at_every_lane_step_leaks_nothing() {
    let mut backend = open();
    let config = AsyncConfig {
        batches: 4,
        rotate_every_batches: 0,
        lane: lane(true),
        ..AsyncConfig::default()
    };
    backend.configure_async(config).unwrap();
    let baseline = live_resources();
    let steps = config.allocation_steps();
    let without_lane = AsyncConfig {
        lane: LaneConfig::default(),
        ..config
    }
    .allocation_steps();
    assert!(steps > without_lane);
    for step in without_lane - 1..steps {
        backend.set_alloc_failpoint(Some(AllocFailpoint {
            at: step,
            once: true,
        }));
        // The plan backs off and still ends up with a working pipeline, or reports the error;
        // either way nothing is left behind once it is released.
        let _ = backend.prepare_matrix_resources(JOBS, INNER);
        backend.release_async_resources();
        assert_eq!(
            live_resources(),
            baseline,
            "leak after failure at step {step}"
        );
    }
}

/// The overlapped lane plus the GPU-DRAM sweep, with short intervals so a bounded test sees
/// several checkpoints and trades of pool halves.
fn prepared_sweep(pool_mib: u64) -> CudaBackend {
    let mut backend = open();
    backend
        .configure_async(AsyncConfig {
            batches: 8,
            rotate_every_batches: 0,
            lane: lane(false),
            sweep: SweepConfig {
                enabled: true,
                duty: Some(0.5),
                share: None,
                checkpoint_interval_ms: 100,
                swap_interval_ms: 300,
                ..SweepConfig::default()
            },
            ..AsyncConfig::default()
        })
        .unwrap();
    backend.prepare_matrix_resources(JOBS, INNER).unwrap();
    backend.allocate_pool(pool_mib * MIB, 32 * MIB).unwrap();
    backend
}

/// Runs bursts until `done` holds for the sweep report (bounded).
fn run_sweep_until(backend: &CudaBackend, done: impl Fn(&gpu_cuda::SweepReport) -> bool) {
    let started = Instant::now();
    let mut seed = 1;
    while backend.sweep_report().is_none_or(|s| !done(&s)) {
        assert!(
            started.elapsed() < Duration::from_secs(90),
            "sweep did not get there: {:?}",
            backend.sweep_report()
        );
        backend
            .verify_matrix_async(seed, false, &no_stop, None)
            .unwrap();
        seed += 1;
    }
}

#[test]
#[ignore = "bounded real NVIDIA GPU-DRAM sweep gate"]
fn h6_sweep_verifies_on_the_device_trades_halves_and_its_checkpoints_agree() {
    let backend = prepared_sweep(128);
    run_sweep_until(&backend, |s| {
        s.zone_swaps >= 2
            && s.checkpoint_ops >= 2
            && s.passes_completed >= 4
            && s.compute_checkpoint_cpu_verified_bytes > 0
    });
    backend.flush_lane().unwrap();
    assert!(backend.take_lane_failures().is_empty());
    let s = backend.sweep_report().unwrap();
    assert_eq!((s.bad_words, s.failures_total), (0, 0), "{s:?}");
    // Every pass reads back all it wrote; the device-verified volume is whole passes.
    assert!(s.device_verified_bytes >= 4 * s.zone_bytes, "{s:?}");
    assert_eq!(s.fill_bytes, s.device_verified_bytes, "{s:?}");
    assert!(s.checkpoint_cpu_verified_bytes > 0, "{s:?}");
    // v1.2 C10: every other pass is a compute pass, unmixed and compared on the device, its mix
    // checked by the CPU at a checkpoint; device-verified only.
    assert!(
        s.mix_rounds > 0 && s.compute_device_verified_bytes >= s.zone_bytes,
        "{s:?}"
    );
    assert!(
        s.device_verified_compute_ops > 0 && s.forward_event_seconds > 0.0,
        "{s:?}"
    );
    assert_eq!(s.compute_bad_words, 0, "{s:?}");
    // The lane kept verifying through the trades and has covered both halves.
    let lane = backend.lane_report().unwrap();
    assert!(lane.transfer.sweeps >= 1, "{:?}", lane.transfer);
    assert_eq!(lane.conservation, "ok");
    assert_eq!(lane.transfer.pending_rereads, 0);
}

#[test]
#[ignore = "bounded real NVIDIA GPU-DRAM sweep negative gate"]
fn h6_each_injected_sweep_fault_is_reported_once_with_its_phase() {
    for (fault, phase) in [
        (LaneFault::SweepVerify, "vram-sweep"),
        (LaneFault::SweepCheckpoint, "vram-sweep-checkpoint"),
        // v1.2 C10 compute passes: a block left unmixed, a word changed at rest, the unmix
        // with another key, a corrupted read-back of the mixed state.
        (LaneFault::ComputeSweepSkip, "compute-sweep"),
        (LaneFault::ComputeSweepState, "compute-sweep"),
        (LaneFault::ComputeSweepKey, "compute-sweep"),
        (
            LaneFault::ComputeSweepCheckpoint,
            "compute-sweep-checkpoint",
        ),
    ] {
        let backend = prepared_sweep(64);
        backend.inject_lane_fault(fault).unwrap();
        run_sweep_until(&backend, |s| {
            s.passes_completed >= 4 && s.checkpoint_ops >= 2 && s.failures_total >= 1
        });
        backend.flush_lane().unwrap();
        let failures = backend.take_lane_failures();
        assert_eq!(failures.len(), 1, "{fault:?}: {failures:?}");
        assert_eq!(failures[0].phase, phase, "{fault:?}");
        let s = backend.sweep_report().unwrap();
        assert_eq!(s.failures_total, 1, "{fault:?}: {s:?}");
        // The sweep carries on after a failure.
        assert!(s.device_verified_bytes > 0, "{fault:?}: {s:?}");
    }
}
