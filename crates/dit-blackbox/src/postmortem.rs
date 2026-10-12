//! The post-mortem at boot (`docs/FAULT_CATCH_ROADMAP.md`, `FC3b`): a session whose journal never
//! closed died with the whole machine (power, a frozen kernel, a restart). Its result comes from
//! the journal's last line and what this boot says: the AMD "Previous system reset reason" line
//! and what pstore kept (a panic, a machine check).
use crate::outcome::Cause;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// The reason an AMD board gives for the last reset (`x86/amd: Previous system reset reason
/// [0x...]: <text>`), from this boot's kernel log.
#[must_use]
pub fn reset_reason(kernel_lines: &[String]) -> Option<String> {
    kernel_lines.iter().find_map(|line| {
        let rest = line.split("Previous system reset reason").nth(1)?;
        Some(
            rest.split_once("]:")
                .map_or(rest, |(_, text)| text)
                .trim()
                .to_owned(),
        )
    })
}

/// What pstore kept of the previous boot's end.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Pstore {
    pub panic: bool,
    pub machine_check: bool,
    /// The files read, and their first lines that say so.
    pub records: Vec<(String, String)>,
}

/// Reads pstore records written after `since_unix_ms` from `dirs` (`/sys/fs/pstore`, and
/// `/var/lib/systemd/pstore` where systemd moves them at boot), one directory level deep.
#[must_use]
pub fn read_pstore(dirs: &[PathBuf], since_unix_ms: u64) -> Pstore {
    let mut found = Pstore::default();
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(inner) = fs::read_dir(&path) {
                    files.extend(inner.filter_map(Result::ok).map(|e| e.path()));
                }
            } else {
                files.push(path);
            }
        }
    }
    for file in files {
        let recent = fs::metadata(&file)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|age| {
                u64::try_from(age.as_millis()).unwrap_or(0) + 60_000 >= since_unix_ms
            });
        if !recent {
            continue;
        }
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        let said: Vec<&str> = text
            .lines()
            .filter(|line| {
                line.contains("Kernel panic")
                    || line.contains("Machine check")
                    || line.contains("[Hardware Error]")
                    || line.contains("Oops")
            })
            .take(5)
            .collect();
        found.panic |= text.contains("Kernel panic");
        found.machine_check |= text.contains("Machine check") || text.contains("[Hardware Error]");
        if !said.is_empty() {
            found
                .records
                .push((file.display().to_string(), said.join("\n")));
        }
    }
    found
}

fn platform(code: &str, text: String) -> Cause {
    Cause {
        code: code.to_owned(),
        text,
        cells: vec!["PLATFORM".to_owned()],
        incomplete: false,
    }
}

fn invalid(code: &str, text: String) -> Cause {
    Cause {
        code: code.to_owned(),
        text,
        cells: Vec::new(),
        incomplete: false,
    }
}

/// How a session with an open journal ended (the cause table of FC5): hardware evidence makes it
/// a PLATFORM FAIL; a test that had stopped answering, or an operator's restart, makes it
/// INVALID; nothing to go on makes it INCOMPLETE.
#[must_use]
pub fn classify(last_beat: Option<&Value>, reason: Option<&str>, pstore: &Pstore) -> Cause {
    let said = reason.unwrap_or("no reason given");
    if pstore.panic {
        return platform(
            "kernel_panic",
            "the kernel stopped (panic) during the session; pstore kept its log".into(),
        );
    }
    if pstore.machine_check {
        return platform(
            "mce",
            "a machine check (hardware error) ended the session; pstore kept its log".into(),
        );
    }
    let reason_has = |words: &[&str]| {
        reason.is_some_and(|text| {
            let text = text.to_ascii_lowercase();
            words.iter().any(|word| text.contains(word))
        })
    };
    if reason_has(&["thermal"]) {
        return platform(
            "reset_under_load",
            format!("the board cut power for heat while the test ran ({said})"),
        );
    }
    if reason_has(&["sync flood", "parity", "uncorrected", "cpu shutdown"]) {
        return platform(
            "fatal_hardware_error",
            format!("a fatal hardware error reset the machine ({said})"),
        );
    }
    if reason_has(&["watchdog"]) {
        return platform(
            "hard_hang",
            format!("the machine froze and the hardware watchdog restarted it ({said})"),
        );
    }
    if reason_has(&["shutdown_l", "shutdown pin"]) {
        return platform(
            "reset_under_load",
            format!("power was cut while the test ran ({said})"),
        );
    }
    let silent = last_beat
        .and_then(|beat| beat["heartbeat_age_s"].as_u64())
        .unwrap_or(0);
    let available = last_beat
        .and_then(|beat| beat["memory_available_mib"].as_u64())
        .unwrap_or(u64::MAX);
    let full = last_beat
        .and_then(|beat| beat["pressure"]["full_avg10"].as_f64())
        .unwrap_or(0.0);
    if silent >= 60 || available < 256 || full >= 20.0 {
        return invalid(
            "app_hang_reset",
            format!(
                "the test had stopped answering ({silent} s without a heartbeat, {} MiB available, memory pressure full {full:.0} %); the machine was then restarted ({said})",
                if available == u64::MAX { 0 } else { available }
            ),
        );
    }
    if reason_has(&[
        "power button",
        "reset_l",
        "reset pin",
        "software",
        "acpi",
        "cf9",
        "kb_rst",
        "keyboard",
    ]) {
        return invalid(
            "operator_restart",
            format!("the machine was restarted while the test ran ({said})"),
        );
    }
    Cause {
        code: "unknown_reset".to_owned(),
        text: format!(
            "the session ended without its result and this boot does not say why ({said})"
        ),
        cells: Vec::new(),
        incomplete: true,
    }
}

/// Journals on the results volume whose session has no result (no closing line, no claim).
#[must_use]
pub fn open_journals(results_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(results_root.join("blackbox")) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter(|path| {
            let lines = crate::journal::read(path);
            let run_id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ended = lines.iter().any(|line| {
                matches!(
                    line["kind"].as_str(),
                    Some("closed" | "result" | "postmortem")
                )
            });
            !ended
                && !results_root
                    .join("claims")
                    .join(format!("{run_id}.claim"))
                    .exists()
        })
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_reset_reason_is_read_from_the_kernel_line() {
        let lines = vec![
            "Linux version 7.0.0".to_owned(),
            "x86/amd: Previous system reset reason [0x00200800]: ACPI power state transition occurred".to_owned(),
        ];
        assert_eq!(
            reset_reason(&lines).as_deref(),
            Some("ACPI power state transition occurred")
        );
        assert_eq!(reset_reason(&lines[..1]), None);
    }

    #[test]
    fn the_ending_follows_the_cause_table() {
        let calm = json!({"heartbeat_age_s": 1, "memory_available_mib": 8000, "pressure": {"full_avg10": 0.0}});
        let frozen = json!({"heartbeat_age_s": 75, "memory_available_mib": 180, "pressure": {"full_avg10": 64.0}});
        let none = Pstore::default();
        let code = |beat: &Value, reason: Option<&str>, pstore: &Pstore| {
            let cause = classify(Some(beat), reason, pstore);
            (cause.code, cause.cells, cause.incomplete)
        };
        assert_eq!(
            code(&calm, Some("internal CPU thermal limit was tripped"), &none),
            ("reset_under_load".into(), vec!["PLATFORM".into()], false)
        );
        assert_eq!(
            code(&calm, Some("hardware watchdog timer expired"), &none).0,
            "hard_hang"
        );
        assert_eq!(
            code(
                &calm,
                Some("an uncorrected error caused a data fabric sync flood event"),
                &none
            )
            .0,
            "fatal_hardware_error"
        );
        // A frozen test then the power button: the app's fault, not the hardware's.
        assert_eq!(
            code(
                &frozen,
                Some("power button was pressed for 4 seconds"),
                &none
            ),
            ("app_hang_reset".into(), vec![], false)
        );
        assert_eq!(
            code(&calm, Some("power button was pressed for 4 seconds"), &none).0,
            "operator_restart"
        );
        // Intel: no reason line.
        assert_eq!(
            code(&calm, None, &none),
            ("unknown_reset".into(), vec![], true)
        );
        let panic = Pstore {
            panic: true,
            ..Pstore::default()
        };
        assert_eq!(code(&frozen, None, &panic).0, "kernel_panic");
    }

    #[test]
    fn only_journals_without_an_ending_are_open() {
        let root = std::env::temp_dir().join(format!("dit-postmortem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("blackbox")).unwrap();
        fs::create_dir_all(root.join("claims")).unwrap();
        fs::write(
            root.join("blackbox/a.jsonl"),
            "{\"kind\":\"open\"}\n{\"kind\":\"beat\"}\n",
        )
        .unwrap();
        fs::write(
            root.join("blackbox/b.jsonl"),
            "{\"kind\":\"open\"}\n{\"kind\":\"closed\"}\n",
        )
        .unwrap();
        fs::write(root.join("blackbox/c.jsonl"), "{\"kind\":\"open\"}\n").unwrap();
        fs::write(root.join("claims/c.claim"), "app\n1\n").unwrap();
        assert_eq!(open_journals(&root), vec![root.join("blackbox/a.jsonl")]);
        fs::remove_dir_all(root).unwrap();
    }
}
