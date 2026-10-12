//! Kernel log lines that say a device or the machine is in trouble (`docs/FAULT_CATCH_ROADMAP.md`,
//! `FC3a`, `FC3b`, FC5): what they mean, in the black box's terms. Only lines the kernel wrote
//! count (a program can write to `/dev/kmsg` too).
use crate::host::KernelLine;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KernelEvent {
    /// An `NVMe` command timed out, was aborted, or the controller was reset for it.
    NvmeTimeout { controller: String },
    /// The `NVMe` controller is down or was removed.
    NvmeDead { controller: String },
    /// A block I/O error on a device (`nvme0n1`, `sda`).
    IoError { device: String },
    /// A task waited in the kernel for minutes (`blocked for more than 120 seconds`).
    HungTask { task: String },
    /// A CPU stuck in the kernel.
    SoftLockup,
    /// A machine check (hardware error reported by the CPU).
    MachineCheck,
    /// An NVIDIA Xid; 79 is "fallen off the bus".
    Xid { code: u32, pci: String },
    /// A process died from a fault (`segfault at`, `general protection fault`, `trap invalid
    /// opcode`).
    Fault { pid: u32, name: String },
    /// The OOM killer (global or a cgroup's) ended a process.
    OomKill { pid: u32, name: String },
}

impl KernelEvent {
    /// A short line for the screen and the journal.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::NvmeTimeout { controller } => format!("{controller} I/O timeout / reset"),
            Self::NvmeDead { controller } => format!("{controller} controller down"),
            Self::IoError { device } => format!("I/O error on {device}"),
            Self::HungTask { task } => format!("task {task} blocked in the kernel"),
            Self::SoftLockup => "CPU soft lockup".to_owned(),
            Self::MachineCheck => "machine check (hardware error)".to_owned(),
            Self::Xid { code, pci } => format!("NVIDIA Xid {code} on {pci}"),
            Self::Fault { pid, name } => format!("{name} ({pid}) crashed"),
            Self::OomKill { pid, name } => format!("OOM killed {name} ({pid})"),
        }
    }

    /// The `NVMe` controller (`nvme0`) an SSD event is about.
    #[must_use]
    pub fn nvme_controller(&self) -> Option<String> {
        match self {
            Self::NvmeTimeout { controller } | Self::NvmeDead { controller } => {
                Some(controller.clone())
            }
            // `nvme1n1` (namespace 1 of controller 1) -> `nvme1`.
            Self::IoError { device } => device
                .strip_prefix("nvme")
                .and_then(|rest| rest.split_once('n'))
                .map(|(number, _)| format!("nvme{number}")),
            _ => None,
        }
    }
}

/// The word after `marker` in `text`, up to a space, comma or colon.
fn word_after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    let rest = &text[text.find(marker)? + marker.len()..];
    let end = rest.find([' ', ',', ':', ')']).unwrap_or(rest.len());
    Some(&rest[..end]).filter(|word| !word.is_empty())
}

/// `name[pid]: ...` at the start of a line.
fn name_pid(text: &str) -> Option<(String, u32)> {
    let open = text.find('[')?;
    let close = text[open..].find(']')? + open;
    let pid = text[open + 1..close].parse().ok()?;
    Some((text[..open].trim().to_owned(), pid))
}

/// `Killed process 1234 (dit-cli)`.
fn killed_process(text: &str) -> Option<(u32, String)> {
    let rest = &text[text.find("Killed process ")? + "Killed process ".len()..];
    let (pid, rest) = rest.split_once(' ')?;
    let name = rest.strip_prefix('(')?.split(')').next()?.to_owned();
    Some((pid.parse().ok()?, name))
}

/// What a kernel log line says, if it is one of the lines the black box watches for.
#[must_use]
pub fn classify(line: &KernelLine) -> Option<KernelEvent> {
    if !line.from_kernel() {
        return None;
    }
    let text = line.message.as_str();
    if let Some(controller) = text
        .strip_prefix("nvme ")
        .and_then(|rest| rest.split(':').next())
    {
        let controller = controller.to_owned();
        if text.contains("timeout")
            || text.contains("aborting")
            || text.contains("resetting controller")
        {
            return Some(KernelEvent::NvmeTimeout { controller });
        }
        if text.contains("controller is down")
            || text.contains("Removing after probe failure")
            || text.contains("Device not ready")
            || text.contains("Disabling device after reset failure")
        {
            return Some(KernelEvent::NvmeDead { controller });
        }
    }
    if text.contains("I/O error, dev ") {
        return word_after(text, "dev ").map(|device| KernelEvent::IoError {
            device: device.to_owned(),
        });
    }
    if text.contains("blocked for more than") {
        return word_after(text, "task ").map(|task| KernelEvent::HungTask {
            task: task.to_owned(),
        });
    }
    if text.contains("soft lockup") || text.contains("hard LOCKUP") {
        return Some(KernelEvent::SoftLockup);
    }
    if text.contains("[Hardware Error]") || text.starts_with("mce: ") {
        return Some(KernelEvent::MachineCheck);
    }
    if text.starts_with("NVRM: Xid ") {
        let pci = text
            .split("(PCI:")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap_or("")
            .to_owned();
        let code = text
            .split("): ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .and_then(|code| code.trim().parse().ok())?;
        return Some(KernelEvent::Xid { code, pci });
    }
    if text.contains("Killed process ")
        && (text.contains("Out of memory") || text.contains("out of memory"))
    {
        return killed_process(text).map(|(pid, name)| KernelEvent::OomKill { pid, name });
    }
    if text.contains("segfault at ")
        || text.contains("general protection fault")
        || text.contains("trap invalid opcode")
        || text.contains("bus error")
    {
        return name_pid(text).map(|(name, pid)| KernelEvent::Fault { pid, name });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::parse_kmsg;

    fn event(message: &str) -> Option<KernelEvent> {
        classify(&parse_kmsg(&format!("3,1,1,-;{message}")).unwrap())
    }

    #[test]
    fn device_and_machine_troubles_are_recognised() {
        assert_eq!(
            event("nvme nvme0: I/O 12 (Write) QID 3 timeout, aborting"),
            Some(KernelEvent::NvmeTimeout {
                controller: "nvme0".into()
            })
        );
        assert_eq!(
            event("nvme nvme1: controller is down; will reset: CSTS=0xffffffff"),
            Some(KernelEvent::NvmeDead {
                controller: "nvme1".into()
            })
        );
        assert_eq!(
            event("I/O error, dev nvme0n1, sector 2048 op 0x1:(WRITE) flags 0x8800"),
            Some(KernelEvent::IoError {
                device: "nvme0n1".into()
            })
        );
        assert_eq!(
            event("INFO: task cpu-nvme-dit:4242 blocked for more than 122 seconds."),
            Some(KernelEvent::HungTask {
                task: "cpu-nvme-dit".into()
            })
        );
        assert_eq!(
            event("NVRM: Xid (PCI:0000:01:00): 79, pid=1234, GPU has fallen off the bus."),
            Some(KernelEvent::Xid {
                code: 79,
                pci: "0000:01:00".into()
            })
        );
        assert_eq!(
            event("Memory cgroup out of memory: Killed process 4321 (dit-cli) total-vm:1kB"),
            Some(KernelEvent::OomKill {
                pid: 4321,
                name: "dit-cli".into()
            })
        );
        assert_eq!(
            event("dit-cli[777]: segfault at 0 ip 000055 sp 00007f error 4 in dit-cli"),
            Some(KernelEvent::Fault {
                pid: 777,
                name: "dit-cli".into()
            })
        );
        assert_eq!(
            event("mce: [Hardware Error]: Machine check events logged"),
            Some(KernelEvent::MachineCheck)
        );
        assert_eq!(event("usb 1-1: new high-speed USB device"), None);
        // Written by a program, not the kernel: not evidence.
        let forged = parse_kmsg("12,2,2,-;nvme nvme0: I/O 1 QID 1 timeout, aborting").unwrap();
        assert_eq!(classify(&forged), None);
    }

    #[test]
    fn ssd_events_name_their_controller() {
        let io = KernelEvent::IoError {
            device: "nvme1n1".into(),
        };
        assert_eq!(io.nvme_controller().as_deref(), Some("nvme1"));
        let sata = KernelEvent::IoError {
            device: "sda".into(),
        };
        assert_eq!(sata.nvme_controller(), None);
    }
}
