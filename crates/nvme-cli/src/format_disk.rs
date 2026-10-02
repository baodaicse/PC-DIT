//! Quick format of a whole SSD, from its own screen in the control center (key F, outside any
//! test): after a crash left a test volume unusable, or to give a blank drive a file system to
//! test on. It erases everything on
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
    // Mounts first: the most common reason, and the one worth naming exactly.
    let mounts: Vec<&String> = device
        .mount_points
        .iter()
        .chain(
            device
                .partition_details
                .iter()
                .flat_map(|part| &part.mount_points),
        )
        .collect();
    if let Some(mount) = mounts
        .iter()
        .find(|mount| auto_storage::held_by_running_test(mount))
    {
        return Some(format!(
            "in use by a running test ({mount}); stop that test first"
        ));
    }
    if !mounts.is_empty() {
        return Some(format!(
            "mounted on {}; unmount it first",
            mounts
                .iter()
                .map(|mount| mount.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Err(error) = auto_storage::guard_namespace(device, protected) {
        return Some(format!(
            "system, boot, results, swap or held disk, or no stable serial number ({error})"
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
        let mounted = device
            .partition_details
            .iter()
            .find(|detail| detail.device_node == part.name)
            .map(|detail| detail.mount_points.join(","))
            .filter(|mounts| !mounts.is_empty())
            .map_or_else(String::new, |mounts| format!("  mounted on {mounts}"));
        let _ = writeln!(
            text,
            "      {}  {}  {}  {:.1} GiB{mounted}",
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

/// The kernel lists exactly one partition, `partition`, over (nearly) the whole disk: the new
/// table is in use. Otherwise something still held an old partition and nothing is formatted.
fn verify_new_partition(disk: &Path, partition: &Path) -> Result<()> {
    let tree = auto_storage::block_tree(disk)?;
    let whole = tree.size;
    match tree.children.as_slice() {
        [only] if only.name == partition && only.size >= whole / 100 * 99 => Ok(()),
        _ => Err(format!(
            "the kernel still uses the old partition table of {} (a partition was busy); nothing was formatted. Restart the machine and format again",
            disk.display()
        )
        .into()),
    }
}

/// Programs the format runs; all are looked up before the disk is touched, so a missing one
/// never leaves a wiped disk without a partition table.
fn required_programs(filesystem: Filesystem) -> [&'static str; 5] {
    [
        "wipefs",
        "sfdisk",
        "partx",
        "udevadm",
        filesystem.mkfs(Path::new("/dev/null")).0,
    ]
}

/// Programs of `names` that are not on `PATH` (or `/sbin`, `/usr/sbin`).
fn missing_programs<'a>(names: &[&'a str]) -> Vec<&'a str> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut directories: Vec<PathBuf> = std::env::split_paths(&path).collect();
    directories.extend(["/sbin", "/usr/sbin", "/bin", "/usr/bin"].map(PathBuf::from));
    names
        .iter()
        .copied()
        .filter(|name| !directories.iter().any(|dir| dir.join(name).is_file()))
        .collect()
}

/// Erases `device` and leaves one freshly formatted partition on it.
fn format(
    device: &NvmeDevice,
    tree: &auto_storage::Block,
    filesystem: Filesystem,
) -> Result<PathBuf> {
    let disk = device.device_node.clone();
    let missing = missing_programs(&required_programs(filesystem));
    if !missing.is_empty() {
        return Err(format!(
            "this image lacks {} (packages fdisk / util-linux / ntfs-3g / e2fsprogs); nothing was changed. Use an image built after this fix",
            missing.join(", ")
        )
        .into());
    }
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
    // The kernel must see the new table, not keep the old partition under the same name.
    let _ = run(
        "partx",
        &["--update".into(), disk.display().to_string()],
        30,
        None,
    );
    let _ = run(
        "udevadm",
        &["settle".into(), "--timeout=15".into()],
        20,
        None,
    );
    let partition = partition_node(&disk);
    wait_for(&partition, Duration::from_secs(15))?;
    verify_new_partition(&disk, &partition)?;
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

/// The disks of the format screen: each with its partitions and whether it may be formatted.
fn render_disks(inventory: &Inventory, protected: &[PathBuf]) -> String {
    let rule = "-".repeat(78);
    let mut text = format!("{rule}\n");
    for device in &inventory.devices {
        match auto_storage::block_tree(&device.device_node) {
            Ok(tree) => {
                text.push_str(&describe(device, &tree));
                let _ = writeln!(
                    text,
                    "      -> {}",
                    refusal(device, protected, &tree).map_or_else(
                        || "can be formatted".to_owned(),
                        |reason| format!("REFUSED: {reason}")
                    )
                );
            }
            Err(error) => {
                let _ = writeln!(
                    text,
                    "  [{}] {}  {}  -> REFUSED: partitions unreadable ({error})",
                    device.index,
                    device.device_node.display(),
                    device.model.trim()
                );
            }
        }
    }
    let _ = writeln!(text, "{rule}");
    text
}

/// One pass of the screen: pick a disk, confirm, format. `Ok(false)` when the operator goes
/// back to the control center (Enter at the disk prompt).
fn format_one(inventory: &Inventory, protected: &[PathBuf]) -> io::Result<bool> {
    let answer = ask("Disk alias to format (Enter = back to the control center) > ")?;
    if answer.trim().is_empty() {
        return Ok(false);
    }
    let outcome = (|| -> io::Result<String> {
        let Ok(alias) = answer.trim().parse::<usize>() else {
            return Ok(format!(
                "'{}' is not a disk alias; nothing was changed.",
                answer.trim()
            ));
        };
        let Some(device) = inventory
            .devices
            .iter()
            .find(|device| device.index == alias)
        else {
            return Ok(format!(
                "No NVMe disk has alias {alias}; nothing was changed."
            ));
        };
        let tree = match auto_storage::block_tree(&device.device_node) {
            Ok(tree) => tree,
            Err(error) => {
                return Ok(format!(
                    "Cannot read the partitions of {}: {error}",
                    device.device_node.display()
                ));
            }
        };
        if let Some(reason) = refusal(device, protected, &tree) {
            return Ok(format!("Format refused: {reason}"));
        }
        print!("{}", describe(device, &tree));
        let Some(filesystem) = Filesystem::parse(&ask(
            "File system: N) NTFS (default; Windows reads it afterwards)  E) ext4 > ",
        )?) else {
            return Ok("Unknown file system; nothing was changed.".into());
        };
        let answer = ask(&format!(
            "ALL DATA on {} will be lost. Type the last {CONFIRM_CHARS} characters of its serial number ({}) to format it as {}, anything else cancels > ",
            device.device_node.display(),
            "*".repeat(CONFIRM_CHARS),
            filesystem.label()
        ))?;
        if confirm_code(&device.serial).chars().count() < CONFIRM_CHARS
            || !confirmed(&answer, &device.serial)
        {
            return Ok("Serial does not match; nothing was changed.".into());
        }
        Ok(match format(device, &tree, filesystem) {
            Ok(partition) => format!(
                "Format done: {} ({} {LABEL}); it can be tested now.",
                partition.display(),
                filesystem.label()
            ),
            Err(error) => format!(
                "FORMAT FAILED: {error}. The disk may hold no usable partition now; format it again."
            ),
        })
    })()?;
    println!("{outcome}");
    ask("Press Enter to continue > ")?;
    Ok(true)
}

/// `cpu-nvme-dit format-disk`: the FORMAT SSD screen of the control center (key F there). Lists
/// the `NVMe` disks, formats the chosen one, and lists them again until the operator goes back.
pub(super) fn command(flags: &super::InventoryFlags) -> Result<std::process::ExitCode> {
    use std::io::IsTerminal as _;
    if !io::stdin().is_terminal() {
        return Err("the format screen requires a terminal".into());
    }
    loop {
        let inventory = super::scan_releasing_stale(flags)?;
        print!("\x1b[H\x1b[2J");
        println!("FORMAT SSD (quick) // erases EVERYTHING on the chosen NVMe disk");
        println!(
            "One GPT partition with a new NTFS or ext4 file system labelled {LABEL} is left on it."
        );
        println!(
            "System, boot, results, swap, mounted and read-only disks are refused; the last {CONFIRM_CHARS}"
        );
        println!("characters of the disk's serial number must be typed to confirm.");
        if inventory.devices.is_empty() {
            println!("\nNo NVMe disk was found.");
            ask("Press Enter to go back > ")?;
            return Ok(std::process::ExitCode::SUCCESS);
        }
        print!(
            "{}",
            dit_ui_core::decorate(&render_disks(&inventory, &flags.protected_devices))
        );
        if !format_one(&inventory, &flags.protected_devices)? {
            return Ok(std::process::ExitCode::SUCCESS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_programs_are_found_before_the_disk_is_touched() {
        assert!(missing_programs(&["sh"]).is_empty());
        assert_eq!(
            missing_programs(&["sh", "no-such-program-dit"]),
            vec!["no-such-program-dit"]
        );
        assert!(required_programs(Filesystem::Ntfs).contains(&"sfdisk"));
        assert!(required_programs(Filesystem::Ext4).contains(&"mkfs.ext4"));
    }

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
