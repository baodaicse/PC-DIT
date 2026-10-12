//! Step `FC3b`: at boot, a session whose journal never closed gets its result from the journal's
//! last line and the board's reset reason; stale watchdog markers are closed; the menu is told.
use std::process::Command;

#[test]
#[allow(clippy::too_many_lines)]
fn a_session_the_machine_died_in_is_recorded_at_the_next_boot() {
    let root = std::env::temp_dir().join(format!("dit-post-mortem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let results = root.join("results");
    std::fs::create_dir_all(results.join("blackbox")).unwrap();
    std::fs::create_dir_all(results.join("watchdog")).unwrap();
    let info = serde_json::json!({
        "schema_version": 1,
        "run_id": "suite-combined-9",
        "csv": {"time": "", "image_id": "img", "test": "COMBINED DEEP max", "board": "B", "cpu": "C",
                "memory": "64 GiB", "gpus": "G", "ssds": "S", "verdict": "", "report": ""},
        "app_pid": 1234,
        "started_unix_ms": 1000,
        "results_root": "/mnt/old-mount",
        "pending_cells": [],
    });
    let journal = [
        serde_json::json!({"kind": "open", "t": 1000, "session": info}),
        serde_json::json!({"kind": "beat", "t": 2000, "phase": "PHASE 6/6 OVERLAP", "state": "RUNNING",
            "elapsed_s": 21, "heartbeat_age_s": 75, "failures": 0,
            "counts": {"columns": [0,0,0,0,0,0,0,0], "unresolved": 0, "confirmed": 0, "suspected": 0, "cells": "", "hardware": ""},
            "memory_available_mib": 180, "pressure": {"some_avg10": 98.0, "full_avg10": 64.0}}),
    ]
    .iter()
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    // The power cut the last line short.
    std::fs::write(
        results.join("blackbox/suite-combined-9.jsonl"),
        format!("{journal}\n{{\"kind\":\"be"),
    )
    .unwrap();
    std::fs::write(
        results.join("watchdog/suite-combined-9-p06-cpu-ram.json"),
        r#"{"module": "cpu-ram", "run_id": "suite-combined-9-p06", "state": "STARTING"}"#,
    )
    .unwrap();
    let kmsg = root.join("kmsg");
    std::fs::write(
        &kmsg,
        "6,1,1,-;x86/amd: Previous system reset reason [0x00000002]: power button was pressed for 4 seconds\n",
    )
    .unwrap();
    let notice = root.join("notice.txt");
    let status = Command::new(env!("CARGO_BIN_EXE_dit-blackbox"))
        .arg("post-mortem")
        .arg("--results-root")
        .arg(&results)
        .arg("--kmsg")
        .arg(&kmsg)
        .arg("--pstore")
        .arg(root.join("no-pstore"))
        .arg("--notice")
        .arg(&notice)
        .status()
        .unwrap();
    assert!(status.success());
    let csv = std::fs::read_to_string(results.join("fail-summary.csv")).unwrap();
    let row = csv.lines().nth(1).unwrap();
    assert!(row.contains(",INVALID,"), "{row}");
    assert!(row.contains("INVALID[app_hang_reset]"), "{row}");
    assert!(
        row.contains("postmortem/suite-combined-9-result.json"),
        "{row}"
    );
    let report: serde_json::Value = serde_json::from_slice(
        &std::fs::read(results.join("postmortem/suite-combined-9-result.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        report["evidence"]["reset_reason"],
        "power button was pressed for 4 seconds"
    );
    let marker =
        std::fs::read_to_string(results.join("watchdog/suite-combined-9-p06-cpu-ram.json"))
            .unwrap();
    assert!(marker.contains("CLOSED_AT_BOOT"), "{marker}");
    let told = std::fs::read_to_string(&notice).unwrap();
    assert!(
        told.contains("COMBINED DEEP max suite-combined-9: INVALID"),
        "{told}"
    );
    // A second boot finds nothing more to do.
    std::fs::remove_file(&notice).unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_dit-blackbox"))
        .arg("post-mortem")
        .arg("--results-root")
        .arg(&results)
        .arg("--kmsg")
        .arg(&kmsg)
        .arg("--notice")
        .arg(&notice)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!notice.exists());
    assert_eq!(
        std::fs::read_to_string(results.join("fail-summary.csv"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    std::fs::remove_dir_all(root).unwrap();
}
