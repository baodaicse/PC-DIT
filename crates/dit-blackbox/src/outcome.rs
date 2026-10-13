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
    /// Nothing says what happened (a reset without a reason): the session is INCOMPLETE, neither
    /// FAIL nor INVALID.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub incomplete: bool,
}

/// What was written.
#[derive(Clone, Debug)]
pub struct Written {
    pub verdict: &'static str,
    pub report: PathBuf,
    /// `Fail summary row added to ...` or why not.
    pub csv: String,
    /// The `FAIL BY PART` line of what was counted.
    pub fail_line: String,
}

/// `FAIL BY PART` from a CSV row's cells (`RAM=3(c2/s1);?SSD1|SSD1-path=1[ssd_timeout]`): the
/// failed parts only, as the result screens show them.
#[must_use]
pub fn fail_line(cells: &str) -> String {
    let parts: Vec<String> = cells
        .split(';')
        .filter_map(|entry| {
            let entry = entry.split('[').next()?.trim();
            let (cell, rest) = entry.split_once('=')?;
            let count = rest.split('(').next()?.trim();
            let suspected = rest
                .split("/s")
                .nth(1)
                .and_then(|s| s.trim_end_matches(')').parse::<u64>().ok())
                .unwrap_or(0);
            Some(if let Some(group) = cell.strip_prefix('?') {
                format!("? {group} {count}")
            } else if suspected > 0 {
                format!("{cell} {count} ({suspected} susp)")
            } else {
                format!("{cell} {count}")
            })
        })
        .collect();
    if parts.is_empty() {
        "FAIL BY PART  none".to_owned()
    } else {
        format!("FAIL BY PART  {}", parts.join(" | "))
    }
}

#[derive(Serialize)]
struct ResultReport<'a> {
    schema_version: u32,
    /// DIT-01 version that wrote this report (absent before v1.2: v1.1).
    dit_version: &'static str,
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
    write_for(root, &info, &status, dir, by, cause, evidence)
}

/// [`write_result`] from a session's description and last status as read elsewhere (the
/// post-mortem reads them from the journal); `dir` holds the files without a results volume.
/// # Errors
/// The result was claimed already (by whom).
pub fn write_for(
    root: &Path,
    info: &SessionInfo,
    status: &SessionStatus,
    dir: &Path,
    by: &str,
    cause: &Cause,
    evidence: &serde_json::Value,
) -> Result<Written, String> {
    if let Claim::Taken(who) = session::claim(&info.results_root, root, &info.run_id, by) {
        return Err(format!("the {who} already wrote this session's result"));
    }
    let mut counts = status.counts.clone();
    // Failures the app saw but had not placed yet (a single run counts them from its report).
    counts.add_pending(
        &info.pending_cells,
        status.failures.saturating_sub(counts.failures()),
    );
    if !cause.cells.is_empty() {
        let cells: Vec<&str> = cause.cells.iter().map(String::as_str).collect();
        counts.add_failure(&cells, &cause.code);
    } else if cause.incomplete && counts.failures() == 0 {
        counts.add_marked("INCOMPLETE", &cause.code);
    } else {
        counts.add_cause(&cause.code);
    }
    let verdict = if counts.failures() > 0 {
        "FAIL"
    } else if cause.incomplete {
        "INCOMPLETE"
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
        // v2: dit_version.
        schema_version: 2,
        dit_version: dit_contracts::DIT_VERSION,
        written_by: by,
        run_id: &info.run_id,
        verdict,
        cause,
        session: info,
        last_status: status,
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
        fail_line: fail_line(&counts.cells),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dit_orchestrator::fault_ledger::CsvCounts;
    use dit_orchestrator::session::{SessionWriter, unix_ms};

    #[test]
    fn the_fail_line_lists_the_failed_parts_of_the_cells() {
        assert_eq!(
            fail_line(
                "RAM=3(c2/s1);PLATFORM=1(c1/s0)[worker_crash];?SSD1|SSD1-path=1[ssd_timeout];INVALID[x]"
            ),
            "FAIL BY PART  RAM 3 (1 susp) | PLATFORM 1 | ? SSD1|SSD1-path 1"
        );
        assert_eq!(fail_line("INVALID[app_hang]"), "FAIL BY PART  none");
    }

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
                ssd_blocks: Vec::new(),
                gpu_pcis: Vec::new(),
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
            incomplete: false,
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
            incomplete: false,
        };
        let written = write_result(&root, &dir, "rescue", &cause, &serde_json::json!({})).unwrap();
        assert_eq!(written.verdict, "FAIL");
        let csv = std::fs::read_to_string(dir.join("fail-summary.csv")).unwrap();
        assert!(csv.contains("?CPU|RAM=2;NOTE[operator_rescue]"), "{csv}");
        std::fs::remove_dir_all(root).unwrap();
    }
}
