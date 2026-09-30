#!/bin/bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="$ROOT_DIR/build/boot-image"
OUTPUT_DIR="$ROOT_DIR/dist"
UBUNTU_MIRROR="${UBUNTU_MIRROR:-http://archive.ubuntu.com/ubuntu}"
RESULT_MIB="${RESULT_MIB:-1024}"
# Free room kept on DIT-BOOT for an optional prebuilt driver layer (tools/build_nvidia_driver_layer.sh;
# the nvidia-driver-580-open layer measured 510 MiB). 0 builds a boot partition with no room for one.
LAYER_RESERVE_MIB="${LAYER_RESERVE_MIB:-768}"
NVIDIA_DRIVER_PACKAGE="${NVIDIA_DRIVER_PACKAGE:-}"
BUILD_EPOCH=""
BUILD_TIME_LOCAL=""
GPU_DRIVER_PROFILE="open"
if [[ -n "$NVIDIA_DRIVER_PACKAGE" ]]; then
    [[ "$NVIDIA_DRIVER_PACKAGE" =~ ^nvidia-driver-[0-9]+(-open)?$ ]] \
        || { printf '[CPU-DIT image] ERROR: invalid NVIDIA_DRIVER_PACKAGE: %s\n' \
            "$NVIDIA_DRIVER_PACKAGE" >&2; exit 2; }
    GPU_DRIVER_PROFILE="nvidia"
fi
if [[ "$GPU_DRIVER_PROFILE" == nvidia ]]; then
    ROOTFS="$BUILD_ROOT/rootfs-noble-amd64-nvidia"
    OUTPUT_IMAGE="$OUTPUT_DIR/dit-01-noble-amd64-nvidia.img"
else
    ROOTFS="$BUILD_ROOT/rootfs-noble-amd64"
    OUTPUT_IMAGE="$OUTPUT_DIR/dit-01-noble-amd64.img"
fi
ROOTFS_MARKER="$ROOTFS/.cpu-dit-rootfs-ready"
EXTENSION_DIR="$ROOT_DIR/dist/cpu-nvme-extension-root"
EXTENSION_MANIFEST_SHA="none"

DIAGNOSTIC_PACKAGES=(
    linux-firmware
    libdrm2
    mesa-utils
    mesa-vulkan-drivers
    libvulkan1
    vulkan-tools
    lm-sensors
    i2c-tools
    rasdaemon
    smartmontools
    nvme-cli
    ntfs-3g
    fdisk
    e2fsprogs
    dmidecode
    ethtool
)
if [[ "$GPU_DRIVER_PROFILE" == nvidia ]]; then
    DIAGNOSTIC_PACKAGES+=(linux-headers-generic-hwe-24.04 "$NVIDIA_DRIVER_PACKAGE")
fi

MOUNTED_PATHS=()
LOOP_DEVICE=""
TEMP_DIR=""

log() {
    printf '[CPU-DIT image] %s\n' "$*"
}

die() {
    printf '[CPU-DIT image] ERROR: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    set +e
    for (( index=${#MOUNTED_PATHS[@]}-1; index>=0; index-- )); do
        sudo umount "${MOUNTED_PATHS[$index]}" 2>/dev/null || true
    done
    if [[ -n "$LOOP_DEVICE" ]]; then
        sudo losetup -d "$LOOP_DEVICE" 2>/dev/null || true
    fi
    if [[ -n "$TEMP_DIR" && -d "$TEMP_DIR" ]]; then
        sudo find "$TEMP_DIR" -mindepth 1 -delete 2>/dev/null || true
        rmdir "$TEMP_DIR" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

require_project() {
    [[ -f "$ROOT_DIR/Cargo.toml" ]] || die "project root not found: $ROOT_DIR"
    [[ "$BUILD_ROOT" == "$ROOT_DIR"/build/boot-image ]] || die "unsafe build root"
    [[ "$OUTPUT_IMAGE" == "$ROOT_DIR"/dist/dit-01-noble-amd64.img \
        || "$OUTPUT_IMAGE" == "$ROOT_DIR"/dist/dit-01-noble-amd64-nvidia.img ]] \
        || die "unsafe output path"
}

# nouveau is disabled on every flavor, at three layers: `module_blacklist=nouveau` on the kernel
# command line (grub.cfg; the kernel refuses every load of it, explicit modprobe included), this
# modprobe.d file (`install ... /bin/false` also refuses it if booted with a custom command line),
# and the regenerated initramfs, which carries the same file. NVIDIA GPUs are driven only by the
# proprietary/open NVIDIA stack: baked in (nvidia-image) or installed at boot from a staged
# package (nvidia-driver-install.sh). Without either, an NVIDIA card has no kernel driver.
NOUVEAU_BLACKLIST='# DIT-01: nouveau is disabled on every image flavor.
blacklist nouveau
options nouveau modeset=0
install nouveau /bin/false
'

disable_nouveau() {
    local target="$ROOTFS/etc/modprobe.d/dit-blacklist-nouveau.conf"
    if [[ -f "$target" ]] && [[ "$(sudo cat "$target")" == "${NOUVEAU_BLACKLIST%$'\n'}" ]]; then
        log "nouveau already disabled in the rootfs and its initramfs"
        return
    fi
    log "Disabling nouveau (modprobe.d + initramfs)"
    sudo install -d -m 0755 "$ROOTFS/etc/modprobe.d"
    printf '%s' "$NOUVEAU_BLACKLIST" | sudo tee "$target" >/dev/null
    mount_chroot_filesystems
    sudo chroot "$ROOTFS" update-initramfs -u -k all
    unmount_chroot_filesystems
}

configure_gpu_driver_profile() {
    if [[ "$GPU_DRIVER_PROFILE" != nvidia ]]; then
        log "Using generic GPU driver profile (open Intel/AMD stacks; NVIDIA via staged package)"
        return
    fi

    log "Configuring NVIDIA image profile: $NVIDIA_DRIVER_PACKAGE"
    [[ -x "$ROOTFS/usr/bin/nvidia-smi" ]] \
        || die "$NVIDIA_DRIVER_PACKAGE installed without /usr/bin/nvidia-smi"
    sudo install -d -m 0755 "$ROOTFS/etc/cpu-dit"
    printf '%s\n' "$NVIDIA_DRIVER_PACKAGE" \
        | sudo tee "$ROOTFS/etc/cpu-dit/nvidia-driver-profile" >/dev/null
    mount_chroot_filesystems
    sudo chroot "$ROOTFS" update-initramfs -u -k all
    unmount_chroot_filesystems
}

install_host_dependencies() {
    local missing=()
    for command in debootstrap grub-install mformat mksquashfs sfdisk losetup mkfs.fat; do
        command -v "$command" >/dev/null 2>&1 || missing+=("$command")
    done
    if (( ${#missing[@]} > 0 )); then
        log "Installing host build dependencies: ${missing[*]}"
        sudo apt-get update
        sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
            debootstrap grub-pc-bin grub-efi-amd64-bin mtools squashfs-tools dosfstools fdisk
    fi
}

mount_chroot_filesystems() {
    sudo mount --bind /dev "$ROOTFS/dev"
    MOUNTED_PATHS+=("$ROOTFS/dev")
    sudo mount -t devpts devpts "$ROOTFS/dev/pts"
    MOUNTED_PATHS+=("$ROOTFS/dev/pts")
    sudo mount -t proc proc "$ROOTFS/proc"
    MOUNTED_PATHS+=("$ROOTFS/proc")
    sudo mount -t sysfs sysfs "$ROOTFS/sys"
    MOUNTED_PATHS+=("$ROOTFS/sys")
}

unmount_chroot_filesystems() {
    local path attempt
    for path in "$ROOTFS/sys" "$ROOTFS/proc" "$ROOTFS/dev/pts" "$ROOTFS/dev"; do
        for attempt in 1 2 3; do
            sudo umount "$path" 2>/dev/null && break
            sleep 1
        done
        # A short-lived process (e.g. a failed update-pciids network probe)
        # can keep the mount transiently busy; detach lazily as a last resort
        # instead of failing the whole build over a teardown race.
        if mountpoint -q "$path" 2>/dev/null; then
            sudo umount -l "$path"
        fi
        unset 'MOUNTED_PATHS[-1]'
    done
}

bootstrap_rootfs() {
    if [[ -f "$ROOTFS_MARKER" ]]; then
        log "Reusing cached Ubuntu Noble rootfs"
        return
    fi
    if [[ -e "$ROOTFS" && -n "$(find "$ROOTFS" -mindepth 1 -maxdepth 1 -print -quit 2>/dev/null)" ]]; then
        die "partial rootfs exists without marker: $ROOTFS (remove this exact directory to rebuild)"
    fi

    log "Bootstrapping Ubuntu Noble minbase"
    mkdir -p "$ROOTFS"
    sudo debootstrap --arch=amd64 --variant=minbase noble "$ROOTFS" "$UBUNTU_MIRROR"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/etc/apt/sources.list" "$ROOTFS/etc/apt/sources.list"
    sudo rm -f "$ROOTFS/etc/resolv.conf"
    sudo install -m 0644 /etc/resolv.conf "$ROOTFS/etc/resolv.conf"
    printf '#!/bin/sh\nexit 101\n' | sudo tee "$ROOTFS/usr/sbin/policy-rc.d" >/dev/null
    sudo chmod 0755 "$ROOTFS/usr/sbin/policy-rc.d"

    mount_chroot_filesystems
    log "Installing live boot, HWE kernel and minimal diagnostic userland"
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get update
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install -y \
        --no-install-recommends \
        linux-image-generic-hwe-24.04 live-boot systemd-sysv initramfs-tools \
        bash ca-certificates coreutils util-linux kmod procps pciutils usbutils \
        uuid-runtime rsync less nano "${DIAGNOSTIC_PACKAGES[@]}"
    sudo chroot "$ROOTFS" update-initramfs -u -k all
    sudo chroot "$ROOTFS" apt-get clean
    sudo rm -f "$ROOTFS/usr/sbin/policy-rc.d"
    unmount_chroot_filesystems
    sudo touch "$ROOTFS_MARKER"
}

# Programs the SSD tools run inside the image (FORMAT SSD, NTFS probe/repair, mounts). On Ubuntu
# 24.04 sfdisk is in the separate `fdisk` package, not util-linux: without it FORMAT SSD failed
# with exit status 127 after wiping the old table.
IMAGE_STORAGE_TOOLS=(sfdisk wipefs partx mkntfs mkfs.ext4 ntfsfix ntfs-3g.probe udevadm blkid lsblk findmnt mount umount)

verify_image_storage_tools() {
    local missing=() tool
    for tool in "${IMAGE_STORAGE_TOOLS[@]}"; do
        sudo chroot "$ROOTFS" /bin/sh -c "command -v $tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    (( ${#missing[@]} == 0 )) || die "image rootfs lacks storage tools: ${missing[*]}"
    log "Storage tools present: ${IMAGE_STORAGE_TOOLS[*]}"
}

ensure_diagnostic_packages() {
    local missing=() package
    for package in "${DIAGNOSTIC_PACKAGES[@]}"; do
        if ! sudo chroot "$ROOTFS" dpkg-query -W -f='${db:Status-Status}' "$package" \
            2>/dev/null | grep -qx installed; then
            missing+=("$package")
        fi
    done
    if (( ${#missing[@]} == 0 )); then
        log "Sensor and diagnostic packages are present"
        return
    fi

    log "Installing sensor and diagnostic packages: ${missing[*]}"
    printf '#!/bin/sh\nexit 101\n' | sudo tee "$ROOTFS/usr/sbin/policy-rc.d" >/dev/null
    sudo chmod 0755 "$ROOTFS/usr/sbin/policy-rc.d"
    mount_chroot_filesystems
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get update
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install -y \
        --no-install-recommends "${missing[@]}"
    sudo chroot "$ROOTFS" update-initramfs -u -k all
    sudo chroot "$ROOTFS" apt-get clean
    sudo rm -f "$ROOTFS/usr/sbin/policy-rc.d"
    unmount_chroot_filesystems
}

refresh_pci_ids() {
    # debootstrap's cached pciutils pci.ids can lag newest GPUs (e.g. a
    # just-launched NVIDIA card), so `lspci -nnk` falls back to a generic
    # "Device [xxxx]" name instead of the marketing name. Best-effort refresh
    # from the network at build time; the live image itself never fetches
    # anything at boot.
    log "Refreshing PCI ID database for lspci device name resolution"
    mount_chroot_filesystems
    if sudo chroot "$ROOTFS" timeout --foreground --signal=TERM --kill-after=5s 30s \
        update-pciids >/dev/null 2>&1; then
        log "PCI ID database refreshed"
    else
        log "PCI ID database refresh skipped (no network or update-pciids unavailable);" \
            "lspci device names may stay generic for very new hardware"
    fi
    unmount_chroot_filesystems
}

configure_rootfs() {
    local image_id="$1"
    log "Installing CPU-DIT runner and read-only live policy"
    sudo install -d -m 0755 "$ROOTFS/etc/cpu-dit"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/etc/cpu-dit/profiles.conf" \
        "$ROOTFS/etc/cpu-dit/profiles.conf"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/etc/hostname" "$ROOTFS/etc/hostname"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/etc/systemd/system/cpu-dit.service" \
        "$ROOTFS/etc/systemd/system/cpu-dit.service"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/boot-runner.sh"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/cpu-dit-ui.sh"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/driver-probe.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/driver-probe.sh"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/partition-lookup.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/partition-lookup.sh"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/nvidia-driver-install.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/nvidia-driver-install.sh"
    sudo install -D -m 0755 "$ROOT_DIR/target/release/dit-cli" "$ROOTFS/opt/cpu-dit-01/dit-cli"
    sudo install -D -m 0644 "$ROOT_DIR/vectors/source-v1.dat" \
        "$ROOTFS/opt/cpu-dit-01/vectors/source-v1.dat"
    sudo install -D -m 0644 "$ROOT_DIR/vectors/source-v1.manifest.json" \
        "$ROOTFS/opt/cpu-dit-01/vectors/source-v1.manifest.json"
    sudo install -D -m 0644 "$ROOT_DIR/vectors/golden-v1.json" \
        "$ROOTFS/opt/cpu-dit-01/vectors/golden-v1.json"
    printf '%s\n' "$image_id" | sudo tee "$ROOTFS/etc/cpu-dit/image-id" >/dev/null

    sudo mkdir -p "$ROOTFS/etc/systemd/system/multi-user.target.wants"
    sudo ln -sfn /etc/systemd/system/cpu-dit.service \
        "$ROOTFS/etc/systemd/system/multi-user.target.wants/cpu-dit.service"
    for unit in apt-daily.timer apt-daily-upgrade.timer fstrim.timer \
        getty@tty1.service serial-getty@ttyS0.service rasdaemon.service \
        ras-mc-ctl.service smartmontools.service smartd.service lm-sensors.service \
        nvmf-autoconnect.service nvmefc-boot-connections.service getty-static.service \
        dpkg-db-backup.timer e2scrub_all.timer motd-news.timer; do
        sudo ln -sfn /dev/null "$ROOTFS/etc/systemd/system/$unit"
    done
    sudo truncate -s 0 "$ROOTFS/etc/machine-id"
    printf '127.0.0.1 localhost\n127.0.1.1 cpu-dit-01\n' | sudo tee "$ROOTFS/etc/hosts" >/dev/null
    printf '# CPU-DIT live root has no persistent mounts.\n' | sudo tee "$ROOTFS/etc/fstab" >/dev/null
    printf 'APT::Periodic::Enable "0";\n' | sudo tee "$ROOTFS/etc/apt/apt.conf.d/99cpu-dit-no-periodic" >/dev/null

    {
        echo "image_id=$image_id"
        echo "ubuntu_release=noble"
        echo "architecture=amd64"
        echo "dit_binary_sha256=$(sha256sum "$ROOT_DIR/target/release/dit-cli" | awk '{print $1}')"
        echo "extension_manifest_sha256=$EXTENSION_MANIFEST_SHA"
        echo "gpu_driver_profile=$GPU_DRIVER_PROFILE"
        echo "nvidia_driver_package=${NVIDIA_DRIVER_PACKAGE:-none}"
        echo "built_utc=$(date -u -d "@$BUILD_EPOCH" --iso-8601=seconds)"
        echo "built_local=$BUILD_TIME_LOCAL"
    } | sudo tee "$ROOTFS/etc/cpu-dit/build-info" >/dev/null
}

install_image_extension() {
    # Both modules belong to this workspace. Verify the local payload before
    # replacing any cached module files; the unified image has no CPU-only fallback.
    [[ -d "$EXTENSION_DIR" ]] || die "DIT extension directory not found: $EXTENSION_DIR"
    [[ -f "$EXTENSION_DIR/MANIFEST.sha256" ]] \
        || die "DIT extension MANIFEST.sha256 is missing"
    log "Verifying DIT extension: $EXTENSION_DIR"
    (cd "$EXTENSION_DIR" && sha256sum --check MANIFEST.sha256) \
        || die "DIT extension checksum verification failed"
    EXTENSION_MANIFEST_SHA="$(sha256sum "$EXTENSION_DIR/MANIFEST.sha256" | awk '{print $1}')"
    sudo rsync -a --chown=0:0 --exclude=/MANIFEST.sha256 "$EXTENSION_DIR/" "$ROOTFS/"
    [[ -x "$ROOTFS/usr/local/bin/dit-suite" ]] \
        || die "DIT extension did not install the shared control center"
    [[ -x "$ROOTFS/usr/local/bin/dit-watchdog" ]] \
        || die "DIT extension did not install the independent runtime watchdog"
    [[ -x "$ROOTFS/opt/cpu-nvme-dit/cpu-nvme-dit" ]] \
        || die "DIT extension did not install the CPU-SSD/NVMe binary"
    [[ -x "$ROOTFS/opt/gpu-vram-dit/gpu-vram-dit" ]] \
        || die "DIT extension did not install the GPU-VRAM binary"
    [[ -x "$ROOTFS/opt/dit-chain/dit-chain" ]] \
        || die "DIT extension did not install the combined data-chain binary"
    log "Installed DIT extension manifest SHA-256: $EXTENSION_MANIFEST_SHA"
}

# grub.cfg with its build line filled in: the local build time (the build host's zone, e.g.
# "2026-09-23 18:34 +07") and the image-id, so the boot menu shows which build is on a USB.
render_grub_cfg() {
    local image_id="$1" dest="$2"
    sed -e "s|@DIT_BUILD_TIME@|$BUILD_TIME_LOCAL|" -e "s|@DIT_IMAGE_ID@|$image_id|" \
        "$ROOT_DIR/image/boot/grub/grub.cfg" >"$dest"
    chmod 0644 "$dest"
    ! grep -q '@DIT_' "$dest" || die "unfilled placeholder left in grub.cfg"
}

make_boot_payload() {
    local staging="$1" image_id="$2" kernel initrd
    kernel="$(find "$ROOTFS/boot" -maxdepth 1 -type f -name 'vmlinuz-*' | sort -V | tail -1)"
    [[ -n "$kernel" ]] || die "kernel not found in rootfs"
    initrd="$ROOTFS/boot/initrd.img-${kernel##*/vmlinuz-}"
    [[ -f "$initrd" ]] || die "initrd not found: $initrd"

    mkdir -p "$staging/live" "$staging/cpu-dit" "$staging/boot/grub"
    sudo install -m 0644 "$kernel" "$staging/live/vmlinuz"
    sudo install -m 0644 "$initrd" "$staging/live/initrd.img"
    sudo chown "$(id -u):$(id -g)" "$staging/live/vmlinuz" "$staging/live/initrd.img"
    render_grub_cfg "$image_id" "$staging/boot/grub/grub.cfg"
    install -m 0644 "$ROOT_DIR/image/boot/CPU-DIT-README.txt" "$staging/CPU-DIT-README.txt"
    printf '%s\n' "$image_id" >"$staging/cpu-dit/image-id"
    sudo mksquashfs "$ROOTFS" "$staging/live/filesystem.squashfs" \
        -noappend -all-root -comp zstd -Xcompression-level 15 -wildcards \
        -e 'boot/*' 'dev/*' 'proc/*' 'sys/*' 'run/*' 'tmp/*' '.cpu-dit-rootfs-ready'
    sudo chown "$(id -u):$(id -g)" "$staging/live/filesystem.squashfs"
    sudo du -sx --block-size=1 "$ROOTFS" | awk '{print $1}' >"$staging/live/filesystem.size"
}

create_disk_image() {
    local staging="$1" image_id="$2" temp_image="$3"
    local payload_bytes boot_mib boot_sectors bios_start bios_sectors boot_start result_start
    local result_sectors total_sectors boot_mount result_mount loop_p2 loop_p3

    payload_bytes="$(du -sb "$staging" | awk '{print $1}')"
    boot_mib=$(( (payload_bytes + (256 + LAYER_RESERVE_MIB)*1024*1024 + 128*1024*1024 - 1) \
        / (128*1024*1024) * 128 ))
    boot_sectors=$(( boot_mib * 2048 ))
    result_sectors=$(( RESULT_MIB * 2048 ))
    bios_start=2048
    bios_sectors=8192
    boot_start=$(( bios_start + bios_sectors ))
    result_start=$(( boot_start + boot_sectors ))
    total_sectors=$(( result_start + result_sectors + 2048 ))

    log "Creating GPT image: boot=${boot_mib} MiB (layer reserve ${LAYER_RESERVE_MIB} MiB), results=${RESULT_MIB} MiB"
    truncate -s $(( total_sectors * 512 )) "$temp_image"
    sudo sfdisk "$temp_image" >/dev/null <<EOF
label: gpt
unit: sectors

start=$bios_start, size=$bios_sectors, type=21686148-6449-6E6F-744E-656564454649, name="BIOS-BOOT"
start=$boot_start, size=$boot_sectors, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="DIT-BOOT"
start=$result_start, size=$result_sectors, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="DIT-RESULTS"
EOF

    LOOP_DEVICE="$(sudo losetup --find --show --partscan "$temp_image")"
    sudo udevadm settle
    loop_p2="${LOOP_DEVICE}p2"
    loop_p3="${LOOP_DEVICE}p3"
    [[ -b "$loop_p2" && -b "$loop_p3" ]] || die "loop partitions were not created"
    sudo mkfs.fat -F 32 -n DIT-BOOT "$loop_p2" >/dev/null
    sudo mkfs.fat -F 32 -n DIT-RESULTS "$loop_p3" >/dev/null

    boot_mount="$TEMP_DIR/boot-mount"
    result_mount="$TEMP_DIR/result-mount"
    mkdir -p "$boot_mount" "$result_mount"
    sudo mount "$loop_p2" "$boot_mount"
    MOUNTED_PATHS+=("$boot_mount")
    sudo mount "$loop_p3" "$result_mount"
    MOUNTED_PATHS+=("$result_mount")
    sudo rsync -rt --no-perms --no-owner --no-group "$staging/" "$boot_mount/"
    printf '%s\n' "$image_id" | sudo tee "$result_mount/CPU-DIT-RESULTS.ID" >/dev/null
    sudo mkdir -p "$result_mount/runs"

    log "Installing GRUB for legacy BIOS"
    sudo grub-install --target=i386-pc --boot-directory="$boot_mount/boot" \
        --modules="part_gpt fat normal linux search search_fs_uuid" --no-floppy --recheck "$LOOP_DEVICE"
    log "Installing removable GRUB for x86_64 UEFI"
    sudo grub-install --target=x86_64-efi --efi-directory="$boot_mount" \
        --boot-directory="$boot_mount/boot" --removable --no-nvram --no-uefi-secure-boot \
        --modules="part_gpt fat normal linux search search_fs_uuid"
    render_grub_cfg "$image_id" "$TEMP_DIR/grub.cfg"
    sudo install -m 0644 "$TEMP_DIR/grub.cfg" "$boot_mount/boot/grub/grub.cfg"
    sync

    sudo umount "$result_mount"
    unset 'MOUNTED_PATHS[-1]'
    sudo umount "$boot_mount"
    unset 'MOUNTED_PATHS[-1]'
    sudo losetup -d "$LOOP_DEVICE"
    LOOP_DEVICE=""
}

write_manifest() {
    local image="$1" image_id="$2"
    local sha bytes
    sha="$(sha256sum "$image" | awk '{print $1}')"
    bytes="$(stat -c '%s' "$image")"
    printf '%s  %s\n' "$sha" "$(basename "$image")" >"$image.sha256"
    cat >"$image.manifest.json" <<EOF
{
  "schema_version": 1,
  "project": "DIT-01",
  "modules": ["dit.cpu-ram", "dit.cpu-nvme", "dit.vga", "dit.cpu-power-platform", "dit.combined"],
  "image_id": "$image_id",
  "built_local": "$BUILD_TIME_LOCAL",
  "filename": "$(basename "$image")",
  "size_bytes": $bytes,
  "sha256": "$sha",
  "ubuntu_release": "24.04-noble",
  "architecture": "amd64",
  "boot_modes": ["legacy_bios", "uefi"],
  "secure_boot": false,
  "gpu_driver_profile": "$GPU_DRIVER_PROFILE",
  "nvidia_driver_package": "${NVIDIA_DRIVER_PACKAGE:-none}",
  "layer_reserve_mib": $LAYER_RESERVE_MIB,
  "default_profile": "interactive_ui_auto_safe",
  "extension_manifest_sha256": "$EXTENSION_MANIFEST_SHA"
}
EOF
}

release_payload_fingerprint() {
    {
        sha256sum "$ROOT_DIR/target/release/dit-cli"
        find "$ROOT_DIR/image" "$ROOT_DIR/vectors" -type f -print0 \
            | sort -z \
            | xargs -0 sha256sum
        if [[ -n "$EXTENSION_DIR" ]]; then
            sha256sum "$EXTENSION_DIR/MANIFEST.sha256"
        fi
        printf 'gpu_driver_profile=%s\nnvidia_driver_package=%s\n' \
            "$GPU_DRIVER_PROFILE" "${NVIDIA_DRIVER_PACKAGE:-none}"
    } | sha256sum | awk '{print substr($1,1,12)}'
}

main() {
    require_project
    install_host_dependencies
    mkdir -p "$BUILD_ROOT" "$OUTPUT_DIR"
    log "Building unified DIT-01 workspace (CPU-RAM, CPU-SSD/NVMe, GPU-VRAM, control center)"
    cargo build --release --workspace --manifest-path "$ROOT_DIR/Cargo.toml"
    "$ROOT_DIR/tools/build_image_extension.sh"
    bootstrap_rootfs
    ensure_diagnostic_packages
    verify_image_storage_tools
    disable_nouveau
    configure_gpu_driver_profile
    refresh_pci_ids

    if [[ -n "$EXTENSION_DIR" ]]; then
        [[ -f "$EXTENSION_DIR/MANIFEST.sha256" ]] \
            || die "DIT extension MANIFEST.sha256 is missing: $EXTENSION_DIR"
        EXTENSION_MANIFEST_SHA="$(sha256sum "$EXTENSION_DIR/MANIFEST.sha256" | awk '{print $1}')"
    fi

    local payload_sha image_id staging temp_image
    # One timestamp for the whole build: image-id date, build-info and the boot menu line.
    BUILD_EPOCH="$(date +%s)"
    BUILD_TIME_LOCAL="$(date -d "@$BUILD_EPOCH" +'%Y-%m-%d %H:%M %Z')"
    payload_sha="$(release_payload_fingerprint)"
    image_id="DIT-01-NOBLE-$(date -u -d "@$BUILD_EPOCH" +%Y%m%d)-$payload_sha"
    configure_rootfs "$image_id"
    install_image_extension

    TEMP_DIR="$(mktemp -d "$BUILD_ROOT/work.XXXXXX")"
    staging="$TEMP_DIR/boot-tree"
    temp_image="$TEMP_DIR/cpu-dit-01.img"
    mkdir -p "$staging"
    make_boot_payload "$staging" "$image_id"
    create_disk_image "$staging" "$image_id" "$temp_image"
    mv -f "$temp_image" "$OUTPUT_IMAGE"
    write_manifest "$OUTPUT_IMAGE" "$image_id"
    log "Image ready: $OUTPUT_IMAGE"
    log "SHA-256: $(awk '{print $1}' "$OUTPUT_IMAGE.sha256")"
}

main "$@"
