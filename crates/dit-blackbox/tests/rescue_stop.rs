//! Step FC1: [S] pressed twice on the rescue console stops a session whose app does not answer,
//! records it once (INVALID, `operator_rescue`) and ends its test processes.
use dit_orchestrator::fault_ledger::CsvMeta;
use dit_orchestrator::session::{self, LayerStatus, SessionInfo, SessionStatus, SessionWriter};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn a_stop_the_app_does_not_answer_is_recorded_by_the_console() {
    let root = std::env::temp_dir().join(format!("dit-rescue-stop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let results = root.join("results");
    std::fs::create_dir_all(&results).unwrap();
    // The app ignores SIGINT and never writes a result; the layer is a shell with a child.
    let mut app = Command::new("/bin/sh")
        .args(["-c", "trap '' INT; sleep 60"])
        .spawn()
        .unwrap();
    let mut layer = Command::new("/bin/sh")
        .args(["-c", "trap '' INT; sleep 60 & wait"])
        .spawn()
        .unwrap();
    let mut writer = SessionWriter::open(
        &root,
        &SessionInfo {
            schema_version: session::SCHEMA_VERSION,
            run_id: "suite-rescue".to_owned(),
            csv: CsvMeta {
                test: "COMBINED DEEP max".to_owned(),
                ..CsvMeta::default()
            },
            app_pid: app.id(),
            started_unix_ms: session::unix_ms(),
            results_root: results.clone(),
            pending_cells: Vec::new(),
            ssd_blocks: Vec::new(),
            gpu_pcis: Vec::new(),
        },
    )
    .unwrap();
    writer.beat_now(&mut SessionStatus {
        state: "RUNNING".to_owned(),
        layers: vec![LayerStatus {
            name: "cpu-ram".to_owned(),
            watchdog_pid: Some(layer.id()),
            run_dir: root.join("suite-rescue"),
            state: "RUNNING".to_owned(),
            ..LayerStatus::default()
        }],
        ..SessionStatus::default()
    });
    let mut console = Command::new(env!("CARGO_BIN_EXE_dit-rescue"))
        .arg("--session-root")
        .arg(&root)
        .arg("--results-root")
        .arg(&results)
        .args(["--stop-wait-seconds", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    console.stdin.as_mut().unwrap().write_all(b"S").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    console.stdin.as_mut().unwrap().write_all(b"s").unwrap();
    let csv = results.join("fail-summary.csv");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !csv.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    let _ = console.kill();
    let _ = console.wait();
    let text = std::fs::read_to_string(&csv).expect("the console wrote the row");
    let row = text.lines().nth(1).unwrap();
    assert!(row.contains(",INVALID,"), "{row}");
    assert!(row.contains("INVALID[operator_rescue]"), "{row}");
    assert!(results.join("rescue/suite-rescue-result.json").is_file());
    assert_eq!(
        session::claimed_by(&results, &root, "suite-rescue").as_deref(),
        Some("rescue")
    );
    assert!(
        root.join("suite-rescue/rescue-stop").is_file(),
        "the app was asked first"
    );
    assert!(
        session::open_sessions(&root).is_empty(),
        "the session is closed"
    );
    // The test processes were ended; the app is left alone.
    let ended = layer.wait().unwrap();
    assert!(!ended.success());
    assert!(app.try_wait().unwrap().is_none());
    let _ = app.kill();
    let _ = app.wait();
    std::fs::remove_dir_all(root).unwrap();
}
