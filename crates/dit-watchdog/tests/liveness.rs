//! Step FC4 (`docs/FAULT_CATCH_ROADMAP.md`): before ending a worker with no progress the watchdog
//! looks at its processes. Nothing moving: stalled. Still working (the SSD test's CPU time
//! counts, as the kernel works in its name): slow, ended only at the slow I/O limit. No sign of
//! life by the startup deadline: ended as such.
use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn supervise(dir: &Path, module: &str, extra: &[&str], worker: &str) -> (i32, Value) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    let heartbeat = dir.join("heartbeat");
    let status = Command::new(env!("CARGO_BIN_EXE_dit-watchdog"))
        .args([
            "supervise",
            "--module",
            module,
            "--run-id",
            "t",
            "--run-dir",
        ])
        .arg(dir)
        .arg("--heartbeat")
        .arg(&heartbeat)
        .args(["--stale-after-seconds", "5", "--kill-after-seconds", "0"])
        .args(extra)
        .args(["--", "/bin/sh", "-c"])
        .arg(worker.replace("HB", &heartbeat.display().to_string()))
        .status()
        .unwrap();
    let record =
        serde_json::from_slice(&std::fs::read(dir.join("watchdog-final.json")).unwrap()).unwrap();
    (status.code().unwrap_or(-1), record)
}

#[test]
fn a_worker_doing_nothing_is_stalled_one_still_working_is_slow() {
    let base = std::env::temp_dir().join(format!("dit-liveness-{}", std::process::id()));
    // Beats once, then neither progresses nor does anything: stalled after 5 s.
    let (code, record) = supervise(
        &base.join("idle"),
        "cpu-ram",
        &[],
        "touch HB; exec sleep 60",
    );
    assert_eq!(code, 124);
    assert_eq!(record["state"], "STALLED_KILLED");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains("nothing moved"), "{detail}");
    assert!(detail.contains("no kernel I/O error"), "{detail}");
    // The SSD test busy in its own name: slow, not stuck, until the slow I/O limit (8 s).
    let (code, record) = supervise(
        &base.join("slow"),
        "cpu-nvme",
        &["--slow-io-limit-seconds", "8"],
        "touch HB; while :; do :; done",
    );
    assert_eq!(code, 124);
    assert_eq!(record["state"], "STALLED_KILLED");
    let detail = record["detail"].as_str().unwrap();
    assert!(detail.contains("kept doing I/O"), "{detail}");
    assert!(record["liveness"]["cpu_ticks"].as_u64().unwrap() > 0);
    // Never a sign of life: ended at the startup deadline.
    let (code, record) = supervise(
        &base.join("never"),
        "cpu-ram",
        &["--startup-deadline-seconds", "2"],
        "exec sleep 60",
    );
    assert_eq!(code, 123);
    assert_eq!(record["state"], "STARTUP_STALLED");
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn a_pulled_results_stick_does_not_end_the_test() {
    let base = std::env::temp_dir().join(format!("dit-marker-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // The marker's folder cannot be made, as on a stick pulled out under its mount point.
    std::fs::write(base.join("gone"), "").unwrap();
    let marker = base.join("gone/watchdog/t-cpu-ram.json");
    let (code, record) = supervise(
        &base.join("run"),
        "cpu-ram",
        &["--recovery-marker", marker.to_str().unwrap()],
        "touch HB; exit 0",
    );
    assert_eq!(code, 0);
    assert_eq!(record["state"], "PASSED");
    std::fs::remove_dir_all(base).unwrap();
}
