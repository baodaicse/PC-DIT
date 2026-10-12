//! Commands that must not hold up their caller (`docs/FAULT_CATCH_ROADMAP.md`, step FC0). A
//! vendor tool asking a hung device (`nvidia-smi` on a stuck GPU driver, `nvme smart-log` on an
//! SSD that stopped answering) can wait in the kernel for minutes. Such a command is killed at a
//! deadline; one that does not die even then is left behind, and while it lives every later
//! call through the same [`StuckGate`] returns at once instead of starting another one.
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL: Duration = Duration::from_millis(10);
/// How long a killed command gets to go away before it is left behind.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// Commands of one tool left behind, and since when the tool has not answered.
pub struct StuckGate {
    stuck: Mutex<Vec<Child>>,
    /// Unix milliseconds of the first call without an answer; 0 while the tool answers.
    silent_since_ms: AtomicU64,
}

impl Default for StuckGate {
    fn default() -> Self {
        Self::new()
    }
}

impl StuckGate {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stuck: Mutex::new(Vec::new()),
            silent_since_ms: AtomicU64::new(0),
        }
    }

    /// Whether a command left behind earlier is still alive (the ones that ended are reaped).
    #[must_use]
    pub fn blocked(&self) -> bool {
        let Ok(mut stuck) = self.stuck.lock() else {
            return false;
        };
        stuck.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        !stuck.is_empty()
    }

    /// Seconds since the tool last answered, when its last call did not end in time (`None`
    /// while it answers).
    #[must_use]
    pub fn silent_seconds(&self) -> Option<u64> {
        match self.silent_since_ms.load(Ordering::Relaxed) {
            0 => None,
            since => Some(unix_ms().saturating_sub(since) / 1000),
        }
    }

    fn answered(&self) {
        self.silent_since_ms.store(0, Ordering::Relaxed);
    }

    fn unanswered(&self) {
        let _ = self.silent_since_ms.compare_exchange(
            0,
            unix_ms().max(1),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    fn leave(&self, child: Child) {
        if let Ok(mut stuck) = self.stuck.lock() {
            stuck.push(child);
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Runs `command` (stdin closed, stderr dropped) and returns its output when it ends within
/// `limit`. `None` when it cannot start, is still running at the deadline (it is killed), or a
/// command of the same `gate` left behind earlier is still alive.
pub fn output_within(command: &mut Command, limit: Duration, gate: &StuckGate) -> Option<Output> {
    if gate.blocked() {
        gate.unanswered();
        return None;
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read on a thread: a large output must not fill the pipe while this one waits.
    let (sender, receiver) = mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        let reader = thread::Builder::new()
            .name("bounded-output".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                let _ = stdout.read_to_end(&mut bytes);
                let _ = sender.send(bytes);
            });
        if reader.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    }
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                gate.answered();
                // The pipe closes with the process, unless it passed it on to a child of its own.
                let stdout = receiver.recv_timeout(KILL_GRACE).unwrap_or_default();
                return Some(Output {
                    status,
                    stdout,
                    stderr: Vec::new(),
                });
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(POLL),
            _ => break,
        }
    }
    gate.unanswered();
    let _ = child.kill();
    let grace = Instant::now() + KILL_GRACE;
    while Instant::now() < grace {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return None;
        }
        thread::sleep(POLL);
    }
    // Waiting in the kernel: it ends when the device answers or never. Not waited for.
    gate.leave(child);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_comes_back_when_the_command_ends_in_time() {
        let gate = StuckGate::new();
        let output = output_within(
            Command::new("/bin/sh").args(["-c", "echo hello"]),
            Duration::from_secs(5),
            &gate,
        )
        .expect("output");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello\n");
        assert_eq!(gate.silent_seconds(), None);
    }

    #[test]
    fn a_command_past_its_deadline_is_killed_and_does_not_hold_the_caller() {
        let gate = StuckGate::new();
        let started = Instant::now();
        let output = output_within(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(300),
            &gate,
        );
        assert!(output.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(gate.silent_seconds(), Some(0));
        // `sleep` dies from the kill: nothing is left behind and the next call runs.
        assert!(!gate.blocked());
        assert!(
            output_within(
                Command::new("/bin/true").arg(""),
                Duration::from_secs(5),
                &gate
            )
            .is_some()
        );
        assert_eq!(gate.silent_seconds(), None);
    }

    #[test]
    fn a_command_left_behind_blocks_the_gate_until_it_ends() {
        let gate = StuckGate::new();
        let child = Command::new("/bin/sleep").arg("1").spawn().expect("sleep");
        gate.leave(child);
        let started = Instant::now();
        assert!(
            output_within(
                Command::new("/bin/true").arg(""),
                Duration::from_secs(5),
                &gate
            )
            .is_none()
        );
        assert!(started.elapsed() < Duration::from_millis(200));
        thread::sleep(Duration::from_millis(1200));
        assert!(!gate.blocked(), "the ended command is reaped");
    }

    #[test]
    fn a_missing_program_is_none() {
        let gate = StuckGate::new();
        assert!(
            output_within(
                &mut Command::new("/nonexistent/tool"),
                Duration::from_secs(1),
                &gate
            )
            .is_none()
        );
    }
}
