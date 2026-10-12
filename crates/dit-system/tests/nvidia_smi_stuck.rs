//! Step FC0 (`docs/FAULT_CATCH_ROADMAP.md`): an `nvidia-smi` that never answers is killed at its
//! deadline, the caller is told the driver is silent, and a screen reading the telemetry feed is
//! never held up by it.
use dit_system::{GpuDevice, GpuTelemetryFeed, collect_gpu_telemetry, nvidia_silent_seconds};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::time::{Duration, Instant};

fn nvidia_device() -> GpuDevice {
    GpuDevice {
        pci_address: "0000:99:00.0".to_owned(),
        model: "Fake NVIDIA GPU".to_owned(),
        driver: Some("nvidia".to_owned()),
        kernel_modules: vec!["nvidia".to_owned()],
        vram_total_bytes: None,
    }
}

/// The checks, run in a child of this test binary that has `DIT_NVIDIA_SMI` set.
fn silent_driver_checks() {
    let started = Instant::now();
    let samples = collect_gpu_telemetry(&[nvidia_device()]);
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(6),
        "a stuck query held the caller {took:?}"
    );
    assert_eq!(samples.len(), 1);
    assert!(
        samples[0].source.starts_with("nvidia-smi silent"),
        "{}",
        samples[0].source
    );
    assert!(nvidia_silent_seconds().is_some());

    // A screen reads the feed while its thread waits on the stuck query: no wait at all.
    let feed = GpuTelemetryFeed::start(vec![nvidia_device()], Duration::from_millis(200));
    for _ in 0..20 {
        let read = Instant::now();
        let _ = feed.samples();
        assert!(read.elapsed() < Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_silent_nvidia_smi_does_not_hold_the_caller() {
    if std::env::var_os("DIT_NVIDIA_SMI").is_some() {
        silent_driver_checks();
        return;
    }
    let dir = std::env::temp_dir().join(format!("dit-fake-nvidia-smi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("nvidia-smi");
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .env("DIT_NVIDIA_SMI", &fake)
        .args([
            "a_silent_nvidia_smi_does_not_hold_the_caller",
            "--exact",
            "--nocapture",
        ])
        .status()
        .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(status.success());
}
