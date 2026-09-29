//! Quick format of a whole SSD from the SSD selection (key F): after a crash left a test
//! volume unusable, or to give a blank drive a filesystem to test on. It erases everything on
//! the disk, so it runs only when the operator types the last characters of the disk's serial
//! number, and never on a system, boot, results, swap or mounted disk.
//!
//! The result is one GPT partition over the whole disk with a quick-formatted NTFS (default,
//! readable by Windows afterwards) or ext4 file system labelled `SSD-TEST`.
use super::auto_storage;
use nvme_inventory::{Inventory, NvmeDevice};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Label of the formatted volume (must not start with `DIT-`, which marks boot/results media).
const LABEL: &str = "SSD-TEST";
/// Characters of the serial number the operator types to confirm.
const CONFIRM_CHARS: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filesystem {
    Ntfs,
    Ext4,
}

impl Filesystem {
    fn parse(answer: &str) -> Option<Self> {
        match answer.trim().to_ascii_lowercase().as_str() {
            "" | "n" | "ntfs" => Some(Self::Ntfs),
            "e" | "ext4" => Some(Self::Ext4),
            _ => None,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Ntfs => "NTFS",
            Self::Ext4 => "ext4",
        }
    }

    /// GPT partition type: Microsoft basic data, or Linux filesystem data.
    const fn partition_type(self) -> &'static str {
        match self {
            Self::Ntfs => "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7",
            Self::Ext4 => "0FC63DAF-8483-4772-8E79-3D69D8477DE4",
        }
    }

    /// Quick format: no zeroing, no discard pass.
    fn mkfs(self, partition: &Path) -> (&'static str, Vec<String>) {
        let node = partition.display().to_string();
        match self {
            Self::Ntfs => (
                "mkntfs",
                vec!["--quick".into(), "--label".into(), LABEL.into(), node],
            ),
            Self::Ext4 => (
                "mkfs.ext4",
                vec![
                    "-F".into(),
                    "-L".into(),
                    LABEL.into(),
                    "-E".into(),
                    "nodiscard,lazy_itable_init=1,lazy_journal_init=1".into(),
                    node,
                ],
            ),
        }
    }
}

/// `sfdisk` input: a new GPT with one partition over the whole disk (aligned by sfdisk).
fn sfdisk_script(filesystem: Filesystem) -> String {
    format!(
        "label: gpt\ntype={}, name=\"{LABEL}\"\n",
        filesystem.partition_type()
    )
}

/// First partition of `disk`: `nvme0n1` -> `nvme0n1p1`, `sda` -> `sda1`.
fn partition_node(disk: &Path) -> PathBuf {
    let name = disk.display().to_string();
    if name.ends_with(|c: char| c.is_ascii_digit()) {
        PathBuf::from(format!("{name}p1"))
    } else {
        PathBuf::from(format!("{name}1"))
    }
}

/// The characters of `serial` the operator must type.
fn confirm_code(serial: &str) -> String {
    let serial = serial.trim();
    let chars: Vec<char> = serial.chars().collect();
    chars[chars.len().saturating_sub(CONFIRM_CHARS)..]
        .iter()
        .collect()
}

fn confirmed(answer: &str, serial: &str) -> bool {
    let code = confirm_code(serial);
    !code.is_empty() && answer.trim().eq_ignore_ascii_case(&code)
}

/// Why `device` may not be formatted, if anything forbids it.
fn refusal(
    device: &NvmeDevice,
    protected: &[PathBuf],
    tree: &auto_storage::Block,
) -> Option<String> {
    if let Err(error) = auto_storage::guard_namespace(device, protected) {
        return Some(error.to_string());
    }
    if !device.mount_points.is_empty()
        || device
            .partition_details
            .iter()
            .any(|part| !part.mount_points.is_empty())
    {
        return Some(format!(
            "{} has a mounted file system; unmount it first",
            device.device_node.display()
        ));
    }
    if device
        .partition_details
        .iter()
        .any(|part| part.active_swap || !part.holders.is_empty() || part.protected)
    {
        return Some(
            "a partition is swap, protected or held by another device (RAID/LVM/encryption)".into(),
        );
    }
    if tree.ro {
        return Some("the disk is read-only".into());
    }
    if std::iter::once(tree).chain(&tree.children).any(|block| {
        block.label.as_deref().is_some_and(|label| {
            let label = label.to_ascii_uppercase();
            label.starts_with("DIT-") || label.starts_with("CPU-DIT")
        })
    }) {
        return Some("the disk holds DIT boot or results media".into());
    }
    None
}

#[allow(clippy::cast_precision_loss)] // sizes shown to one decimal of a GiB
fn describe(device: &NvmeDevice, tree: &auto_storage::Block) -> String {
    let gib = |bytes: u64| bytes as f64 / f64::from(1_u32 << 30);
    let mut text = format!(
        "  [{}] {}  {}  {:.1} GiB  SN {}\n",
        device.index,
        device.device_node.display(),
        device.model.trim(),
        gib(tree.size),
        device.serial.trim()
    );
    if tree.children.is_empty() {
        let _ = writeln!(
            text,
            "      whole disk: {}",
            tree.fstype.as_deref().unwrap_or("no file system")
        );
    }
    for part in &tree.children {
        let _ = writeln!(
            text,
            "      {}  {}  {}  {:.1} GiB",
            part.name.display(),
            part.fstype.as_deref().unwrap_or("-"),
            part.label.as_deref().unwrap_or("-"),
            gib(part.size)
        );
    }
    text
}

fn ask(text: &str) -> io::Result<String> {
    print!("{text}");
    io::stdout().flush()?;
    super::read_operator_line()
}

/// Runs `program` under `timeout`, feeding `input` on stdin when given.
fn run(program: &str, args: &[String], seconds: u64, input: Option<&str>) -> Result<String> {
    let mut child = Command::new("timeout")
        .args([
            "--signal=TERM",
            "--kill-after=5s",
            &format!("{seconds}s"),
            program,
        ])
        .args(args)
        .env("LC_ALL", "C")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn wait_for(node: &Path, limit: Duration) -> Result<()> {
    let started = Instant::now();
    while !node.exists() {
        if started.elapsed() > limit {
            return Err(format!("{} did not appear after partitioning", node.display()).into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Erases `device` and leaves one freshly formatted partition on it.
fn format(
    device: &NvmeDevice,
    tree: &auto_storage::Block,
    filesystem: Filesystem,
) -> Result<PathBuf> {
    let disk = device.device_node.clone();
    println!("  Clearing old signatures ...");
    // Best effort per partition (wipefs may decline a nested signature); the whole-disk wipe and
    // sfdisk's own wipe of the new partition below are what count.
    for part in &tree.children {
        let _ = run(
            "wipefs",
            &["--all".into(), part.name.display().to_string()],
            30,
            None,
        );
    }
    run(
        "wipefs",
        &["--all".into(), disk.display().to_string()],
        30,
        None,
    )?;
    println!("  Writing a new GPT with one partition ...");
    run(
        "sfdisk",
        &[
            "--wipe".into(),
            "always".into(),
            "--wipe-partitions".into(),
            "always".into(),
            disk.display().to_string(),
        ],
        60,
        Some(&sfdisk_script(filesystem)),
    )?;
    let _ = run(
        "udevadm",
        &["settle".into(), "--timeout=15".into()],
        20,
        None,
    );
    let partition = partition_node(&disk);
    wait_for(&partition, Duration::from_secs(15))?;
    println!(
        "  Quick format {} as {} ...",
        partition.display(),
        filesystem.label()
    );
    let (program, args) = filesystem.mkfs(&partition);
    run(program, &args, 300, None)?;
    let _ = run(
        "udevadm",
        &["settle".into(), "--timeout=15".into()],
        20,
        None,
    );
    let _ = run("sync", &[], 60, None);
    Ok(partition)
}

/// The F key of the SSD selection: pick a disk, confirm, format. Errors are printed, never
/// fatal to the caller, which rescans afterwards.
pub(super) fn interactive(inventory: &Inventory, protected: &[PathBuf]) -> io::Result<()> {
    println!();
    println!("FORMAT DISK (quick): erases EVERYTHING on the chosen NVMe disk and leaves one");
    println!("partition with a new file system labelled {LABEL}. System, boot, results, swap and");
    println!("mounted disks are refused.");
    let answer = ask("Disk alias to format (Enter = cancel) > ")?;
    let Ok(alias) = answer.trim().parse::<usize>() else {
        println!("Format cancelled.");
        return Ok(());
    };
    let Some(device) = inventory
        .devices
        .iter()
        .find(|device| device.index == alias)
    else {
        println!("No NVMe disk has alias {alias}; format cancelled.");
        return Ok(());
    };
    let tree = match auto_storage::block_tree(&device.device_node) {
        Ok(tree) => tree,
        Err(error) => {
            println!(
                "Cannot read the partitions of {}: {error}",
                device.device_node.display()
            );
            return Ok(());
        }
    };
    if let Some(reason) = refusal(device, protected, &tree) {
        println!("Format refused: {reason}");
        return Ok(());
    }
    print!("{}", describe(device, &tree));
    let Some(filesystem) = Filesystem::parse(&ask(
        "File system: N) NTFS (default; Windows reads it afterwards)  E) ext4 > ",
    )?) else {
        println!("Unknown file system; format cancelled.");
        return Ok(());
    };
    let code = confirm_code(&device.serial);
    let answer = ask(&format!(
        "ALL DATA on {} will be lost. Type the last {CONFIRM_CHARS} characters of its serial number ({}) to format it as {}, anything else cancels > ",
        device.device_node.display(),
        "*".repeat(CONFIRM_CHARS),
        filesystem.label()
    ))?;
    if !confirmed(&answer, &device.serial) || code.len() < CONFIRM_CHARS {
        println!("Serial does not match; nothing was changed.");
        return Ok(());
    }
    match format(device, &tree, filesystem) {
        Ok(partition) => println!(
            "Format done: {} ({} {LABEL}). The disk is listed again below.",
            partition.display(),
            filesystem.label()
        ),
        Err(error) => println!(
            "FORMAT FAILED: {error}. The disk may hold no usable partition now; run F again."
        ),
    }
    ask("Press Enter to continue > ")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_scripts_and_confirmation() {
        assert_eq!(
            partition_node(Path::new("/dev/nvme0n1")),
            PathBuf::from("/dev/nvme0n1p1")
        );
        assert_eq!(
            partition_node(Path::new("/dev/sdb")),
            PathBuf::from("/dev/sdb1")
        );
        assert_eq!(
            sfdisk_script(Filesystem::Ntfs),
            "label: gpt\ntype=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name=\"SSD-TEST\"\n"
        );
        assert_eq!(Filesystem::parse(""), Some(Filesystem::Ntfs));
        assert_eq!(Filesystem::parse("E"), Some(Filesystem::Ext4));
        assert_eq!(Filesystem::parse("x"), None);
        assert_eq!(confirm_code(" P300PDCB250912013972 "), "3972");
        assert!(confirmed("3972", "P300PDCB250912013972"));
        assert!(!confirmed("yes", "P300PDCB250912013972"));
        assert!(!confirmed("", ""));
        let (program, args) = Filesystem::Ntfs.mkfs(Path::new("/dev/nvme0n1p1"));
        assert_eq!(program, "mkntfs");
        assert!(args.contains(&"--quick".to_owned()) && args.contains(&LABEL.to_owned()));
        assert!(
            !LABEL.starts_with("DIT-"),
            "the test volume must stay selectable"
        );
    }
}
