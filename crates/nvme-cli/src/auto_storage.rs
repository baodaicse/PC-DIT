//! Conservative, file-only preparation. Planning never mounts or creates files.
use super::{DeviceId, NvmeDevice, reject_critical_ui_directory};
use serde::{Deserialize, Serialize};
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct VolumePlan {
    pub alias: usize,
    pub namespace_id: DeviceId,
    pub target_id: DeviceId,
    pub node: PathBuf,
    pub filesystem: String,
    pub uuid: String,
    pub capacity_bytes: u64,
    pub existing_mount: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
struct Block {
    name: PathBuf,
    fstype: Option<String>,
    label: Option<String>,
    parttype: Option<String>,
    ro: bool,
    size: u64,
    uuid: Option<String>,
    #[serde(default)]
    children: Vec<Self>,
}

fn output(program: &str, args: &[&std::ffi::OsStr]) -> Result<Vec<u8>> {
    let result = Command::new("timeout")
        .args(["--signal=TERM", "--kill-after=2s", "15s", program])
        .args(args)
        .env("LC_ALL", "C")
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "{program} failed ({}): {}. No force/repair fallback will be attempted",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        )
        .into());
    }
    Ok(result.stdout)
}

/// `ntfs-3g.probe --readwrite`: an NTFS volume is written only when Windows left it closed.
/// Nothing is forced; the refusal says what to do in Windows.
fn ntfs_probe(node: &Path) -> Result<()> {
    let result = Command::new("timeout")
        .args([
            "--signal=TERM",
            "--kill-after=2s",
            "15s",
            "ntfs-3g.probe",
            "--readwrite",
        ])
        .arg(node)
        .env("LC_ALL", "C")
        .output()?;
    if result.status.success() {
        return Ok(());
    }
    Err(format!(
        "NTFS {} not writable: {} The volume was left untouched.",
        node.display(),
        ntfs_refusal(result.status.code())
    )
    .into())
}

/// Operator guidance for an `ntfs-3g.probe` exit code (ntfs-3g `NTFS_VOLUME_*` values).
fn ntfs_refusal(code: Option<i32>) -> &'static str {
    match code {
        Some(14) => {
            "Windows hibernated it (code 14). Windows Fast Startup is on by default and makes 'Shut down' hibernate. In Windows: hold Shift while clicking Shut down, or choose Restart and boot this USB from the restart; to stop it for good, run 'powercfg /h off' as administrator or untick 'Turn on fast startup' in Power Options."
        }
        Some(15) => {
            "Windows did not close it cleanly (code 15). Boot Windows, let it finish starting, then Shift + Shut down (or run 'chkdsk /f' on the drive)."
        }
        Some(16) => "it is in use by another program or driver (code 16).",
        Some(13) => {
            "its NTFS metadata is inconsistent (code 13). Run 'chkdsk /f' on the drive in Windows before testing it."
        }
        Some(12) => "it does not hold a valid NTFS file system (code 12).",
        Some(124) => "the safety probe timed out.",
        _ => "the ntfs-3g safety probe refused it.",
    }
}

fn block_tree(node: &Path) -> Result<Block> {
    #[derive(Deserialize)]
    struct Listing {
        blockdevices: Vec<Block>,
    }
    let bytes = output(
        "lsblk",
        &[
            "--json".as_ref(),
            "--bytes".as_ref(),
            "--paths".as_ref(),
            "--output".as_ref(),
            "NAME,FSTYPE,LABEL,PARTTYPE,RO,SIZE,UUID".as_ref(),
            "--".as_ref(),
            node.as_os_str(),
        ],
    )?;
    let listing: Listing = serde_json::from_slice(&bytes)?;
    listing
        .blockdevices
        .into_iter()
        .find(|block| block.name == node)
        .ok_or_else(|| format!("lsblk did not return exact target {}", node.display()).into())
}

fn protected_mount(path: &Path) -> bool {
    reject_critical_ui_directory(path).is_err()
        || !(path.starts_with("/mnt") || path.starts_with("/media"))
        || ["/run", "/cdrom", "/live", "/mnt/cpu-dit-boot-check"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn guard_namespace(device: &NvmeDevice, protected: &[PathBuf]) -> Result<()> {
    if device.serial.trim().is_empty()
        || device.serial == "unknown"
        || !device.holders.is_empty()
        || device
            .mount_points
            .iter()
            .any(|path| protected_mount(Path::new(path)))
        || device
            .safety
            .reasons
            .iter()
            .any(|reason| reason.contains("active swap") || reason.contains("explicitly protected"))
        || protected.iter().any(|path| {
            path == &device.device_node
                || device
                    .partition_details
                    .iter()
                    .any(|part| &part.device_node == path)
        })
    {
        return Err(format!("{} is a live system/boot/results/swap/holder device or lacks a stable identity; automatic test refused", device.device_node.display()).into());
    }
    Ok(())
}

fn volume_allowed(block: &Block) -> Result<()> {
    if block.ro || !block.children.is_empty() {
        return Err("read-only volume or mapped/RAID/encrypted holder".into());
    }
    if !matches!(block.fstype.as_deref(), Some("ext4" | "ntfs" | "exfat")) {
        return Err(
            "requires an existing ext4, NTFS or exFAT filesystem (no format, no encryption unlock)"
                .into(),
        );
    }
    // Only ordinary GPT data partitions or MBR Linux/NTFS/exFAT types.
    if let Some(kind) = &block.parttype {
        let kind = kind.to_ascii_lowercase();
        if !matches!(
            kind.as_str(),
            "0fc63daf-8483-4772-8e79-3d69d8477de4"
                | "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7"
                | "0x83"
                | "0x7"
                | "0x07"
        ) {
            return Err(
                "not an ordinary data partition (boot/recovery/reserved types excluded)".into(),
            );
        }
    }
    if block.uuid.as_deref().is_none_or(str::is_empty) {
        return Err("filesystem UUID is unavailable".into());
    }
    if block.label.as_deref().is_some_and(|label| {
        let label = label.to_ascii_uppercase();
        label.starts_with("DIT-") || label.starts_with("CPU-DIT")
    }) {
        return Err("DIT boot/results volume".into());
    }
    Ok(())
}

pub fn plan(device: &NvmeDevice, protected: &[PathBuf], required: u64) -> Result<VolumePlan> {
    guard_namespace(device, protected)?;
    choose_volume(device, &block_tree(&device.device_node)?, required)
}

fn choose_volume(device: &NvmeDevice, tree: &Block, required: u64) -> Result<VolumePlan> {
    if tree.ro
        || std::iter::once(tree).chain(&tree.children).any(|block| {
            block.label.as_deref().is_some_and(|label| {
                let label = label.to_ascii_uppercase();
                label.starts_with("DIT-") || label.starts_with("CPU-DIT")
            })
        })
    {
        return Err("read-only disk or disk containing DIT boot/results media".into());
    }
    let mut candidates = Vec::new();
    let mut rejected = Vec::new();
    let blocks: Vec<&Block> = if device.partition_details.is_empty() {
        vec![tree]
    } else {
        tree.children.iter().collect()
    };
    for block in blocks {
        let attempt = (|| -> Result<VolumePlan> {
            volume_allowed(block)?;
            let (id, mounts) = if block.name == device.device_node && device.partitions.is_empty() {
                (&device.stable_id, &device.mount_points)
            } else {
                let part = device
                    .partition_details
                    .iter()
                    .find(|part| part.device_node == block.name)
                    .ok_or("partition is not in the selected inventory")?;
                if part.active_swap || !part.holders.is_empty() {
                    return Err("active swap or block holders".into());
                }
                (&part.stable_id, &part.mount_points)
            };
            if block.size < required {
                return Err("volume capacity is below test budget plus free-space reserve".into());
            }
            let existing_mount = mounts.first().map(PathBuf::from);
            if let Some(mount) = &existing_mount
                && (protected_mount(mount) || fs2::available_space(mount)? < required)
            {
                return Err("protected mount or insufficient free space".into());
            }
            Ok(VolumePlan {
                alias: device.index,
                namespace_id: device.stable_id.clone(),
                target_id: id.clone(),
                node: block.name.clone(),
                filesystem: block.fstype.clone().unwrap_or_default(),
                uuid: block.uuid.clone().unwrap_or_default(),
                capacity_bytes: block.size,
                existing_mount,
            })
        })();
        match attempt {
            Ok(candidate) => candidates.push(candidate),
            Err(error) => rejected.push(format!("{}: {error}", block.name.display())),
        }
    }
    candidates.sort_by(|a, b| {
        b.capacity_bytes
            .cmp(&a.capacity_bytes)
            .then(a.node.cmp(&b.node))
    });
    candidates.into_iter().next().ok_or_else(|| {
        format!(
            "No safe test filesystem on {}. {}. No partition will be created or formatted",
            device.device_node.display(),
            rejected.join("; ")
        )
        .into()
    })
}

#[derive(Default)]
pub struct PreparedAreas {
    areas: Vec<OwnedArea>,
}

struct OwnedArea {
    mount: PathBuf,
    owned_mountpoint: bool,
    mounted: bool,
    device_number: u64,
    directory: Option<PathBuf>,
}

impl PreparedAreas {
    pub fn prepare(&mut self, plan: &VolumePlan, session: &str, required: u64) -> Result<PathBuf> {
        // Inspect on-disk signatures afresh; don't rely solely on cached udev FSTYPE/UUID.
        let signature = output(
            "blkid",
            &[
                "-p".as_ref(),
                "-o".as_ref(),
                "export".as_ref(),
                plan.node.as_os_str(),
            ],
        )?;
        verify_signature(plan, &String::from_utf8(signature)?)?;
        let metadata = plan.node.symlink_metadata()?;
        if !metadata.file_type().is_block_device() {
            return Err("planned volume is no longer a direct block device".into());
        }
        if plan.filesystem == "ntfs" && plan.existing_mount.is_none() {
            ntfs_probe(&plan.node)?;
        }
        let mount = plan.existing_mount.clone().unwrap_or_else(|| {
            PathBuf::from("/run").join(format!("cpu-nvme-dit-{session}-{}", plan.alias))
        });
        let owned = plan.existing_mount.is_none();
        if owned {
            DirBuilder::new().mode(0o700).create(&mount)?;
        }
        self.areas.push(OwnedArea {
            mount: mount.clone(),
            owned_mountpoint: owned,
            mounted: false,
            device_number: metadata.rdev(),
            directory: None,
        });
        let area = self
            .areas
            .last_mut()
            .ok_or("missing preparation ownership record")?;
        if owned {
            let filesystem = if plan.filesystem == "ntfs" {
                "ntfs3"
            } else {
                &plan.filesystem
            };
            let result = output(
                "mount",
                &[
                    "-t".as_ref(),
                    filesystem.as_ref(),
                    "-o".as_ref(),
                    "rw,nosuid,nodev,noexec,noatime".as_ref(),
                    "--".as_ref(),
                    plan.node.as_os_str(),
                    mount.as_os_str(),
                ],
            );
            // Capture successful mounts even if the helper exits nonzero afterwards.
            area.mounted = mount.metadata()?.dev() == area.device_number;
            result?;
        }
        verify_mount(&mount, area.device_number, &plan.filesystem)?;
        if fs2::available_space(&mount)? < required {
            return Err(format!("{} has insufficient free space; require {required} bytes including reserve. No test file was created", plan.node.display()).into());
        }
        let directory = mount.join(format!(".cpu-nvme-dit-{session}-{}", plan.alias));
        DirBuilder::new().mode(0o700).create(&directory)?;
        area.directory = Some(directory.clone());
        if directory.symlink_metadata()?.dev() != area.device_number {
            return Err("test directory moved off the confirmed filesystem".into());
        }
        Ok(directory)
    }

    pub fn cleanup(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        for area in self.areas.iter_mut().rev() {
            if let Err(error) = area.cleanup() {
                errors.push(format!("{}: {error}", area.mount.display()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "SETUP CLEANUP INCOMPLETE (no forced unmount/deletion): {}",
                errors.join("; ")
            )
            .into())
        }
    }
}

impl OwnedArea {
    fn cleanup(&mut self) -> Result<()> {
        if (self.directory.is_some() || self.mounted)
            && self.mount.metadata()?.dev() != self.device_number
        {
            return Err("mount identity changed; leaving paths untouched".into());
        }
        if let Some(directory) = &self.directory {
            // Empty directory only. Never recursively delete a test file or existing data.
            fs::remove_dir(directory)?;
            self.directory = None;
        }
        if self.mounted {
            output("umount", &["--".as_ref(), self.mount.as_os_str()])?;
            self.mounted = false;
        }
        if self.owned_mountpoint {
            fs::remove_dir(&self.mount)?;
            self.owned_mountpoint = false;
        }
        Ok(())
    }
}

impl Drop for PreparedAreas {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            eprintln!("{error}");
        }
    }
}

fn verify_signature(plan: &VolumePlan, signature: &str) -> Result<()> {
    let field = |name: &str| {
        signature.lines().find_map(|line| {
            line.split_once('=')
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| value)
        })
    };
    if field("TYPE") != Some(plan.filesystem.as_str()) || field("UUID") != Some(plan.uuid.as_str())
    {
        return Err("filesystem type/UUID changed or ambiguous; cancel and select again".into());
    }
    Ok(())
}

fn verify_mount(mount: &Path, device: u64, filesystem: &str) -> Result<()> {
    if mount.metadata()?.dev() != device {
        return Err("mount does not map directly to the confirmed block device (FUSE/LVM/RAID not supported)".into());
    }
    let bytes = output(
        "findmnt",
        &[
            "--noheadings".as_ref(),
            "--output".as_ref(),
            "FSTYPE,OPTIONS".as_ref(),
            "--target".as_ref(),
            mount.as_os_str(),
        ],
    )?;
    verify_mount_options(&String::from_utf8(bytes)?, filesystem)
}

fn verify_mount_options(text: &str, filesystem: &str) -> Result<()> {
    let mut fields = text.split_whitespace();
    let expected = if filesystem == "ntfs" {
        "ntfs3"
    } else {
        filesystem
    };
    if fields.next() != Some(expected)
        || !fields.next().is_some_and(|options| {
            let options: Vec<_> = options.split(',').collect();
            options.contains(&"rw") && !options.contains(&"ro") && !options.contains(&"force")
        })
        || fields.next().is_some()
    {
        return Err("filesystem is read-only, forced, unsupported, or mount information is ambiguous; no remount attempted".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ntfs_refusals_tell_the_operator_what_to_do() {
        assert!(super::ntfs_refusal(Some(14)).contains("Fast Startup"));
        assert!(super::ntfs_refusal(Some(14)).contains("powercfg /h off"));
        assert!(super::ntfs_refusal(Some(15)).contains("chkdsk"));
        assert!(super::ntfs_refusal(None).contains("refused"));
    }

    use super::*;
    use nvme_inventory::{AerSnapshot, PartitionInfo, SafetyClassification, SmartSnapshot};

    fn device() -> NvmeDevice {
        NvmeDevice {
            index: 1,
            stable_id: DeviceId("fixture-ssd".into()),
            device_node: "/dev/nvme9n1".into(),
            namespace: "nvme9n1".into(),
            namespace_id: Some(1),
            controller: "nvme9".into(),
            driver: "nvme".into(),
            transport: "pcie".into(),
            model: "TEST FIXTURE".into(),
            serial: "test-serial".into(),
            firmware: "test".into(),
            capacity_bytes: 100_000,
            logical_block_size: 512,
            physical_block_size: 4096,
            wwid: None,
            nguid: None,
            eui: None,
            pci_bdf: None,
            root_port_bdf: None,
            current_link_speed: None,
            current_link_width: None,
            partitions: vec![],
            partition_details: vec![],
            mount_points: vec![],
            holders: vec![],
            smart: SmartSnapshot::default(),
            aer: AerSnapshot::default(),
            safety: SafetyClassification {
                read_only_allowed: true,
                write_candidate: true,
                protected: false,
                reasons: vec![],
            },
        }
    }

    fn block() -> Block {
        Block {
            name: "/dev/nvme9n1".into(),
            fstype: Some("ext4".into()),
            label: None,
            parttype: None,
            ro: false,
            size: 100_000,
            uuid: Some("fixture-uuid".into()),
            children: vec![],
        }
    }

    fn add_partition(device: &mut NvmeDevice, tree: &mut Block, suffix: &str, size: u64) {
        let name = format!("nvme9n1p{suffix}");
        let node = PathBuf::from(format!("/dev/{name}"));
        let mut child = block();
        child.name.clone_from(&node);
        child.size = size;
        child.parttype = Some("ebd0a0a2-b9e5-4433-87c0-68b6b72699c7".into());
        tree.children.push(child);
        device.partitions.push(name.clone());
        device.partition_details.push(PartitionInfo {
            name,
            stable_id: DeviceId(format!("partition-{suffix}")),
            device_node: node,
            start_bytes: 4096,
            capacity_bytes: size,
            logical_block_size: 512,
            physical_block_size: 4096,
            mount_points: vec![],
            holders: vec![],
            active_swap: false,
            protected: false,
            write_candidate: true,
            reasons: vec![],
        });
    }

    #[test]
    fn policy_accepts_only_supported_existing_data_filesystems() {
        for filesystem in ["ext4", "ntfs", "exfat"] {
            let mut volume = block();
            volume.fstype = Some(filesystem.into());
            assert!(volume_allowed(&volume).is_ok());
        }
        for filesystem in [
            "",
            "vfat",
            "crypto_LUKS",
            "BitLocker",
            "LVM2_member",
            "linux_raid_member",
            "btrfs",
        ] {
            let mut volume = block();
            volume.fstype = Some(filesystem.into());
            assert!(volume_allowed(&volume).is_err());
        }
    }

    #[test]
    fn rejects_boot_recovery_readonly_missing_uuid_and_holders() {
        for kind in [
            "c12a7328-f81f-11d2-ba4b-00a0c93ec93b",
            "de94bba4-06d1-4d40-a16a-bfd50179d6ac",
            "e3c9e316-0b5c-4db8-817d-f92df00215ae",
        ] {
            let mut volume = block();
            volume.parttype = Some(kind.into());
            assert!(volume_allowed(&volume).is_err());
        }
        let mut volume = block();
        volume.ro = true;
        assert!(volume_allowed(&volume).is_err());
        volume = block();
        volume.uuid = None;
        assert!(volume_allowed(&volume).is_err());
        volume = block();
        volume.children.push(block());
        assert!(volume_allowed(&volume).is_err());
    }

    #[test]
    fn live_system_boot_results_swap_and_unknown_identity_are_blocked() {
        for mount in [
            "/",
            "/boot/efi",
            "/home",
            "/mnt/cpu-dit-results",
            "/run/live/medium",
            "/etc",
            "/opt",
        ] {
            let mut disk = device();
            disk.mount_points.push(mount.into());
            assert!(guard_namespace(&disk, &[]).is_err());
        }
        let mut disk = device();
        assert!(guard_namespace(&disk, &[disk.device_node.clone()]).is_err());
        disk.holders.push("dm-0".into());
        assert!(guard_namespace(&disk, &[]).is_err());
        disk = device();
        disk.safety.reasons.push("contains active swap".into());
        assert!(guard_namespace(&disk, &[]).is_err());
        disk = device();
        disk.serial = "unknown".into();
        assert!(guard_namespace(&disk, &[]).is_err());
    }

    #[test]
    fn largest_eligible_partition_is_deterministic_and_alias_scoped() {
        let mut disk = device();
        let mut tree = block();
        add_partition(&mut disk, &mut tree, "1", 20_000);
        add_partition(&mut disk, &mut tree, "2", 40_000);
        add_partition(&mut disk, &mut tree, "3", 90_000);
        tree.children[2].parttype = Some("de94bba4-06d1-4d40-a16a-bfd50179d6ac".into());
        let plan = choose_volume(&disk, &tree, 10_000).unwrap();
        assert_eq!(plan.node, Path::new("/dev/nvme9n1p2"));
        assert_eq!(plan.target_id, DeviceId("partition-2".into()));
        tree.children.reverse();
        assert_eq!(choose_volume(&disk, &tree, 10_000).unwrap(), plan);
        assert!(choose_volume(&disk, &tree, 99_000).is_err());
        disk.partition_details.clear();
        assert!(choose_volume(&disk, &tree, 10_000).is_err());
    }

    #[test]
    fn dit_label_on_any_sibling_blocks_whole_disk() {
        let mut disk = device();
        let mut tree = block();
        add_partition(&mut disk, &mut tree, "1", 20_000);
        add_partition(&mut disk, &mut tree, "2", 40_000);
        tree.children[0].label = Some("DIT-RESULTS".into());
        assert!(choose_volume(&disk, &tree, 10_000).is_err());
    }

    #[test]
    fn changed_uuid_or_type_is_refused_before_mount() {
        let plan = choose_volume(&device(), &block(), 10_000).unwrap();
        assert!(verify_signature(&plan, "UUID=fixture-uuid\nTYPE=ext4\n").is_ok());
        assert!(verify_signature(&plan, "UUID=changed\nTYPE=ext4\n").is_err());
        assert!(verify_signature(&plan, "UUID=fixture-uuid\nTYPE=ntfs\n").is_err());
    }

    #[test]
    fn readonly_forced_fuse_or_ambiguous_mounts_are_refused() {
        assert!(verify_mount_options("ntfs3 rw,nosuid,nodev\n", "ntfs").is_ok());
        assert!(verify_mount_options("ext4 rw,relatime\n", "ext4").is_ok());
        for info in [
            "ntfs3 ro,nosuid",
            "ntfs3 rw,force",
            "fuseblk rw",
            "ntfs3 rw\nntfs3 ro",
            "",
        ] {
            assert!(verify_mount_options(info, "ntfs").is_err());
        }
    }

    #[test]
    fn cleanup_preserves_preexisting_mount_and_nonempty_test_directory() {
        let root = super::super::temporary_path();
        fs::create_dir(&root).unwrap();
        let child = root.join("new-test-directory");
        fs::create_dir(&child).unwrap();
        let sentinel = child.join("do-not-delete");
        fs::write(&sentinel, b"existing-data").unwrap();
        let mut areas = PreparedAreas {
            areas: vec![OwnedArea {
                mount: root.clone(),
                owned_mountpoint: false,
                mounted: false,
                device_number: root.metadata().unwrap().dev(),
                directory: Some(child.clone()),
            }],
        };
        assert!(areas.cleanup().is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"existing-data");
        fs::remove_file(sentinel).unwrap();
        areas.cleanup().unwrap();
        assert!(!child.exists());
        assert!(root.is_dir());
        areas.cleanup().unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn cleanup_refuses_changed_mount_identity() {
        let root = super::super::temporary_path();
        fs::create_dir(&root).unwrap();
        let child = root.join("test-directory");
        fs::create_dir(&child).unwrap();
        let mut area = OwnedArea {
            mount: root.clone(),
            owned_mountpoint: false,
            mounted: false,
            device_number: root.metadata().unwrap().dev().wrapping_add(1),
            directory: Some(child.clone()),
        };
        assert!(area.cleanup().is_err());
        assert!(child.exists());
        fs::remove_dir(child).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
