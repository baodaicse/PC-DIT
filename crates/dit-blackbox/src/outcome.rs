//! A session's result written when its app cannot (`docs/FAULT_CATCH_ROADMAP.md`): by the black
//! box (`FC3a`) or the rescue console (FC1). The same files as the app's: a report and one
//! `fail-summary.csv` row, after the session's claim; FAIL when a failure was counted, else
//! INVALID, never the two mixed.
use dit_orchestrator::fault_ledger::{CsvMeta, append_csv_row, csv_row};
use dit_orchestrator::session::{self, Claim, SessionInfo, SessionStatus};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Why the session ended without its app's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Cause {
    /// `app_killed`, `app_hang`, `memory_livelock`, `operator_rescue`, `ssd_timeout`...
    pub code: String,
    /// What the screen says.
    pub text: String,
    /// Cells a failure is counted on for it (hardware evidence): one cell, or several when it
    /// cannot be placed on one. Empty: it counts no failure (test infrastructure).
    pub cells: Vec<String>,
}

/// What was written.
#[derive(Clone, Debug)]
pub struct Written {
    pub verdict: &'static str,
    pub report: PathBuf,
    /// `Fail summary row added to ...` or why not.
    pub csv: String,
}

#[derive(Serialize)]
struct ResultReport<'a> {
    schema_version: u32,
    written_by: &'a str,
    run_id: &'a str,
    verdict: &'a str,
    cause: &'a Cause,
    /// The test's own description of the session and its last status.
    session: &'a SessionInfo,
    last_status: &'a SessionStatus,
    /// What the writer saw (machine readings, layer states, kernel lines).
    evidence: &'a serde_json::Value,
    written_unix_ms: u64,
}

/// UTC time as `YYYY-MM-DD HH:MM:SS +0000` without asking another program.
#[must_use]
pub fn utc_timestamp(unix_seconds: u64) -> String {
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(0);
    let rest = unix_seconds % 86_400;
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} +0000",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Writes the result of the session in `dir` (under `root`) for `by` (`blackbox`, `rescue`):
/// claims it, writes `<results>/<by>/<run-id>-result.json` and the CSV row, closes the session.
/// # Errors
/// The result was claimed already (by whom), or the session cannot be read.
pub fn write_result(
    root: &Path,
    dir: &Path,
    by: &str,
    cause: &Cause,
    evidence: &serde_json::Value,
) -> Result<Written, String> {
    let info = session::read_info(dir).ok_or("the session cannot be read")?;
    let status = session::read_status(dir).unwrap_or_default();
    if let Claim::Taken(who) = session::claim(&info.results_root, root, &info.run_id, by) {
        return Err(format!("the {who} already wrote this session's result"));
    }
    let mut counts = status.counts.clone();
    // Failures the app saw but had not placed yet (a single run counts them from its report).
    counts.add_pending(
        &info.pending_cells,
        status.failures.saturating_sub(counts.failures()),
    );
    if cause.cells.is_empty() {
        counts.add_cause(&cause.code);
    } else {
        let cells: Vec<&str> = cause.cells.iter().map(String::as_str).collect();
        counts.add_failure(&cells, &cause.code);
    }
    let verdict = if counts.failures() > 0 {
        "FAIL"
    } else {
        "INVALID"
    };
    let volume = info.results_root.is_dir();
    let report = if volume {
        info.results_root
            .join(by)
            .join(format!("{}-result.json", info.run_id))
    } else {
        dir.join(format!("{by}-result.json"))
    };
    let now = session::unix_ms();
    let body = ResultReport {
        schema_version: 1,
        written_by: by,
        run_id: &info.run_id,
        verdict,
        cause,
        session: &info,
        last_status: &status,
        evidence,
        written_unix_ms: now,
    };
    if let Some(parent) = report.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = serde_json::to_vec_pretty(&body)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&report, bytes));
    let meta = CsvMeta {
        time: utc_timestamp(now / 1000),
        verdict: verdict.to_owned(),
        report: if volume {
            format!("{by}/{}-result.json", info.run_id)
        } else {
            report.display().to_string()
        },
        ..info.csv.clone()
    };
    let csv_path = if volume {
        info.results_root.join("fail-summary.csv")
    } else {
        dir.join("fail-summary.csv")
    };
    let csv = match append_csv_row(&csv_path, &csv_row(&meta, &counts)) {
        Ok(()) => format!("Fail summary row added to {}", csv_path.display()),
        Err(error) => format!("Fail summary not written ({error}): {}", csv_path.display()),
    };
    nix::unistd::sync();
    session::close(root, &info.run_id, verdict);
    Ok(Written {
        verdict,
        report,
        csv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dit_orchestrator::fault_ledger::CsvCounts;
    use dit_orchestrator::session::{SessionWriter, unix_ms};

    #[test]
    fn utc_dates_come_out_right() {
        assert_eq!(utc_timestamp(0), "1970-01-01 00:00:00 +0000");
        assert_eq!(utc_timestamp(951_782_400), "2000-02-29 00:00:00 +0000");
        assert_eq!(utc_timestamp(1_791_811_845), "2026-10-12 13:30:45 +0000");
    }

    fn open_session(root: &Path, results: &Path, failures: u64, counts: CsvCounts) -> PathBuf {
        let mut writer = SessionWriter::open(
            root,
            &SessionInfo {
                schema_version: session::SCHEMA_VERSION,
                run_id: "suite-x".to_owned(),
                csv: CsvMeta {
                    test: "CPU-RAM".to_owned(),
                    ..CsvMeta::default()
                },
                app_pid: 1,
                started_unix_ms: unix_ms(),
                results_root: results.to_owned(),
                pending_cells: vec!["CPU".to_owned(), "RAM".to_owned()],
            },
        )
        .unwrap();
        writer.beat_now(&mut SessionStatus {
            state: "RUNNING".to_owned(),
            failures,
            counts,
            ..SessionStatus::default()
        });
        writer.dir().to_owned()
    }

    #[test]
    fn a_hang_without_failures_is_invalid_and_written_once() {
        let root = std::env::temp_dir().join(format!("dit-outcome-a-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let results = root.join("results");
        std::fs::create_dir_all(&results).unwrap();
        let dir = open_session(&root, &results, 0, CsvCounts::default());
        let cause = Cause {
            code: "app_hang".to_owned(),
            text: "the app stopped answering".to_owned(),
            cells: Vec::new(),
        };
        let written =
            write_result(&root, &dir, "blackbox", &cause, &serde_json::json!({})).unwrap();
        assert_eq!(written.verdict, "INVALID");
        assert!(results.join("blackbox/suite-x-result.json").is_file());
        let csv = std::fs::read_to_string(results.join("fail-summary.csv")).unwrap();
        let row = csv.lines().nth(1).unwrap();
        assert!(row.contains(",INVALID,"), "{row}");
        assert!(row.contains("INVALID[app_hang]"), "{row}");
        assert!(row.contains("blackbox/suite-x-result.json"), "{row}");
        assert_eq!(session::read_status(&dir).unwrap().state, session::CLOSED);
        // The rescue console comes second: no second result.
        assert!(write_result(&root, &dir, "rescue", &cause, &serde_json::json!({})).is_err());
        assert_eq!(
            std::fs::read_to_string(results.join("fail-summary.csv"))
                .unwrap()
                .lines()
                .count(),
            2
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failures_seen_before_the_hang_make_it_a_fail() {
        let root = std::env::temp_dir().join(format!("dit-outcome-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = open_session(&root, &root.join("no-volume"), 2, CsvCounts::default());
        let cause = Cause {
            code: "operator_rescue".to_owned(),
            text: "stopped from the rescue console".to_owned(),
            cells: Vec::new(),
        };
        let written = write_result(&root, &dir, "rescue", &cause, &serde_json::json!({})).unwrap();
        assert_eq!(written.verdict, "FAIL");
        let csv = std::fs::read_to_string(dir.join("fail-summary.csv")).unwrap();
        assert!(csv.contains("?CPU|RAM=2;NOTE[operator_rescue]"), "{csv}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
