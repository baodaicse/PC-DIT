//! Processes and the kernel log, read straight from `/proc` and `/dev/kmsg` (`docs/FAULT_CATCH_ROADMAP.md`,
//! FC1, `FC3a`, FC4): no program is started, so this keeps working while the machine is short of
//! memory.
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// One process as `/proc/<pid>` shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    /// Its process group (a supervised worker leads its own).
    pub pgrp: u32,
    /// `R`, `S`, `D` (waiting in the kernel, usually on I/O), `Z`...
    pub state: char,
    pub name: String,
    pub rss_bytes: u64,
    /// User + system CPU time, clock ticks.
    pub cpu_ticks: u64,
    /// Bytes it read and wrote through the block layer (`/proc/<pid>/io`), when readable.
    pub io_bytes: Option<u64>,
    pub command: String,
}

/// Parses `/proc/<pid>/stat`: the name sits in parentheses and may hold spaces.
fn parse_stat(text: &str) -> Option<(String, char, u32, u32, u64, u64)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text[open + 1..close].to_owned();
    let fields: Vec<&str> = text[close + 2..].split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    let ppid = fields.get(1)?.parse().ok()?;
    let pgrp = fields.get(2)?.parse().ok()?;
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    let rss_pages: u64 = fields.get(21)?.parse().ok()?;
    Some((name, state, ppid, pgrp, utime + stime, rss_pages))
}

/// The process `pid`, `None` when it is gone.
#[must_use]
pub fn process(proc_root: &Path, pid: u32) -> Option<Process> {
    let dir = proc_root.join(pid.to_string());
    let (name, state, parent, pgrp, cpu_ticks, rss_pages) =
        parse_stat(&fs::read_to_string(dir.join("stat")).ok()?)?;
    let io_bytes = fs::read_to_string(dir.join("io")).ok().map(|text| {
        text.lines()
            .filter_map(|line| {
                let (key, value) = line.split_once(':')?;
                matches!(key, "read_bytes" | "write_bytes")
                    .then(|| value.trim().parse::<u64>().ok())
                    .flatten()
            })
            .sum()
    });
    let command = fs::read(dir.join("cmdline"))
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .split('\0')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(Process {
        pid,
        ppid: parent,
        pgrp,
        state,
        name,
        rss_bytes: rss_pages.saturating_mul(4096),
        cpu_ticks,
        io_bytes,
        command,
    })
}

/// Every process now.
#[must_use]
pub fn processes(proc_root: &Path) -> Vec<Process> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| process(proc_root, pid))
        .collect()
}

/// `root` and all its descendants, from a process list.
#[must_use]
pub fn tree(all: &[Process], root: u32) -> Vec<Process> {
    let mut children: BTreeMap<u32, Vec<&Process>> = BTreeMap::new();
    for process in all {
        children.entry(process.ppid).or_default().push(process);
    }
    let mut found: Vec<Process> = all.iter().filter(|p| p.pid == root).cloned().collect();
    let mut index = 0;
    while index < found.len() {
        let pid = found[index].pid;
        if let Some(kids) = children.get(&pid) {
            found.extend(kids.iter().map(|kid| (*kid).clone()));
        }
        index += 1;
    }
    found
}

/// Whether `pid` runs (a zombie counts as gone).
#[must_use]
pub fn alive(proc_root: &Path, pid: u32) -> bool {
    process(proc_root, pid).is_some_and(|p| p.state != 'Z' && p.state != 'X')
}

/// One record of the kernel log.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct KernelLine {
    /// Syslog priority and facility as the record gives them (`facility * 8 + level`).
    pub priority: u32,
    pub sequence: u64,
    pub microseconds: u64,
    pub message: String,
}

impl KernelLine {
    /// Written by the kernel itself (facility 0), not by a program through `/dev/kmsg`.
    #[must_use]
    pub const fn from_kernel(&self) -> bool {
        self.priority / 8 == 0
    }
}

/// `prio,seq,usec,flags;message` (`/dev/kmsg` record), continuation lines dropped.
#[must_use]
pub fn parse_kmsg(record: &str) -> Option<KernelLine> {
    let (header, message) = record.split_once(';')?;
    let mut fields = header.split(',');
    let priority = fields.next()?.parse().ok()?;
    let sequence = fields.next()?.parse().ok()?;
    let microseconds = fields.next()?.parse().ok()?;
    Some(KernelLine {
        priority,
        sequence,
        microseconds,
        message: message.lines().next().unwrap_or_default().to_owned(),
    })
}

/// Reads the kernel log record by record without blocking: what is there now, then later only
/// what is new.
pub struct KernelLog {
    file: Option<fs::File>,
}

impl KernelLog {
    /// Opens `path` (normally `/dev/kmsg`), at its first record still kept.
    #[must_use]
    pub fn open(path: &Path) -> Self {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::fcntl::OFlag::O_NONBLOCK.bits())
            .open(path)
            .ok();
        Self { file }
    }

    /// The records written since the last call.
    pub fn read_new(&mut self) -> Vec<KernelLine> {
        let Some(file) = self.file.as_mut() else {
            return Vec::new();
        };
        let mut lines = Vec::new();
        let mut buffer = vec![0_u8; 8192];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if let Some(line) = parse_kmsg(&String::from_utf8_lossy(&buffer[..read])) {
                        lines.push(line);
                    }
                }
                // EPIPE: records were overwritten before they were read; go on with the next.
                Err(error) if error.raw_os_error() == Some(nix::libc::EPIPE) => {}
                Err(_) => break,
            }
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pressure::{memory, pressure};

    #[test]
    fn stat_with_a_spaced_name_parses() {
        let text = "1234 (dit cli (x)) D 1000 1234 1234 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9 0 777 1000000 2560 18446744073709551615";
        let (name, state, ppid, pgrp, ticks, rss) = parse_stat(text).unwrap();
        assert_eq!(pgrp, 1234);
        assert_eq!(name, "dit cli (x)");
        assert_eq!(state, 'D');
        assert_eq!(ppid, 1000);
        assert_eq!(ticks, 300);
        assert_eq!(rss, 2560);
    }

    #[test]
    fn kernel_records_say_who_wrote_them() {
        let line = parse_kmsg(
            "3,1022,123456789,-;nvme nvme0: I/O 12 QID 3 timeout, aborting\n SUBSYSTEM=nvme",
        )
        .unwrap();
        assert_eq!(line.sequence, 1022);
        assert!(line.from_kernel());
        assert_eq!(line.message, "nvme nvme0: I/O 12 QID 3 timeout, aborting");
        // A program writing to /dev/kmsg gets the user facility (1): not the kernel's word.
        assert!(
            !parse_kmsg("12,5,1,-;NVRM: Xid 79 (fake)")
                .unwrap()
                .from_kernel()
        );
    }

    #[test]
    fn pressure_and_memory_are_read_from_their_files() {
        let root = std::env::temp_dir().join(format!("dit-host-{}", std::process::id()));
        fs::create_dir_all(root.join("pressure")).unwrap();
        fs::write(
            root.join("pressure/memory"),
            "some avg10=12.50 avg60=3.00 avg300=1.00 total=123\nfull avg10=4.25 avg60=1.00 avg300=0.10 total=45\n",
        )
        .unwrap();
        fs::write(
            root.join("meminfo"),
            "MemTotal:       65536000 kB\nMemFree:  100 kB\nMemAvailable:    1048576 kB\n",
        )
        .unwrap();
        let pressure = pressure(&root, "memory").unwrap();
        assert!((pressure.some_avg10 - 12.5).abs() < 1e-9);
        assert!((pressure.full_avg10 - 4.25).abs() < 1e-9);
        let memory = memory(&root);
        assert_eq!(memory.available_bytes, 1_073_741_824);
        assert_eq!(memory.total_bytes, 65_536_000 * 1024);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_tree_holds_the_root_and_its_descendants_only() {
        let p = |pid, ppid| Process {
            pid,
            ppid,
            ..Process::default()
        };
        let all = vec![p(1, 0), p(10, 1), p(11, 10), p(12, 11), p(20, 1)];
        let mut pids: Vec<u32> = tree(&all, 10).iter().map(|p| p.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, vec![10, 11, 12]);
    }
}
