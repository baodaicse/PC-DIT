//! Step `FC3a`: the black box writes the result of a session whose app died without one (after
//! its 10 s), keeps a journal of it, and leaves alone a session the app closed itself.
use dit_orchestrator::fault_ledger::CsvMeta;
use dit_orchestrator::session::{self, Claim, SessionInfo, SessionStatus, SessionWriter};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn open(root: &Path, results: &Path, run_id: &str, app_pid: u32) -> SessionWriter {
    let mut writer = SessionWriter::open(
        root,
        &SessionInfo {
            schema_version: session::SCHEMA_VERSION,
            run_id: run_id.to_owned(),
            csv: CsvMeta {
                test: "GPU-VRAM".to_owned(),
                ..CsvMeta::default()
            },
            app_pid,
            started_unix_ms: session::unix_ms(),
            results_root: results.to_owned(),
            pending_cells: vec!["PCIe-GPU1".into(), "GPU1".into(), "VRAM1".into()],
            ssd_blocks: Vec::new(),
            gpu_pcis: Vec::new(),
        },
    )
    .unwrap();
    writer.beat_now(&mut SessionStatus {
        state: "RUNNING".to_owned(),
        phase: "GPU-VRAM FULL".to_owned(),
        ..SessionStatus::default()
    });
    writer
}

fn black_box(root: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_dit-blackbox"))
        .arg("watch")
        .arg("--session-root")
        .arg(root)
        .args(["--kmsg", "/nonexistent", "--no-switch"])
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn a_session_whose_app_died_gets_its_result_from_the_black_box() {
    let root = std::env::temp_dir().join(format!("dit-blackbox-watch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let results = root.join("results");
    std::fs::create_dir_all(&results).unwrap();
    // The app writes its own result: the black box only closes its journal.
    let mut alive = Command::new("/bin/sleep").arg("60").spawn().unwrap();
    let mut closing = open(&root, &results, "suite-closed", alive.id());
    // The app dies without a result.
    let mut gone = Command::new("/bin/sleep").arg("60").spawn().unwrap();
    let _writer = open(&root, &results, "suite-gone", gone.id());
    gone.kill().unwrap();
    gone.wait().unwrap();
    let mut watcher = black_box(&root);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        session::claim(&results, &root, "suite-closed", "app"),
        Claim::Won
    );
    closing.close(&mut SessionStatus::default(), "PASS");
    let csv = results.join("fail-summary.csv");
    let deadline = Instant::now() + Duration::from_secs(25);
    while !csv.is_file() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    std::thread::sleep(Duration::from_millis(500));
    let _ = watcher.kill();
    let _ = watcher.wait();
    let _ = alive.kill();
    let _ = alive.wait();
    let text = std::fs::read_to_string(&csv).expect("the black box wrote the row");
    assert_eq!(text.lines().count(), 2, "{text}");
    let row = text.lines().nth(1).unwrap();
    assert!(row.contains(",INVALID,"), "{row}");
    assert!(row.contains("INVALID[app_killed]"), "{row}");
    assert!(row.contains("blackbox/suite-gone-result.json"), "{row}");
    assert_eq!(
        session::claimed_by(&results, &root, "suite-gone").as_deref(),
        Some("blackbox")
    );
    let journal = dit_blackbox::journal::read(&results.join("blackbox/suite-gone.jsonl"));
    let kinds: Vec<&str> = journal.iter().filter_map(|l| l["kind"].as_str()).collect();
    assert_eq!(kinds.first(), Some(&"open"));
    assert!(kinds.contains(&"beat"));
    assert!(kinds.contains(&"result"), "{kinds:?}");
    let closed = dit_blackbox::journal::read(&results.join("blackbox/suite-closed.jsonl"));
    assert!(
        closed
            .iter()
            .any(|l| l["kind"] == "closed" && l["by"] == "app"),
        "{closed:?}"
    );
    std::fs::remove_dir_all(root).unwrap();
}
