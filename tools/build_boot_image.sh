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
# The image's kernel, pinned: Ubuntu signs it (Secure Boot) and publishes NVIDIA modules signed for
# exactly this ABI, matching the driver's userspace version (linux-modules-nvidia-*-<abi>-generic).
# Moving it means checking that both still hold for the new ABI.
KERNEL_ABI="${KERNEL_ABI:-7.0.0-38}"
# Ubuntu's signed UEFI chain (shim signed by Microsoft, GRUB by Canonical), taken from the archive.
EFI_SIGNED="$BUILD_ROOT/efi-signed"
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
    # Canonical's signed modules for the pinned kernel (Secure Boot stays on), not DKMS builds.
    # shellcheck source=tools/nvidia_packages.sh
    . "$ROOT_DIR/tools/nvidia_packages.sh"
    mapfile -t nvidia_packages < <(nvidia_signed_packages "$NVIDIA_DRIVER_PACKAGE" "$KERNEL_ABI-generic")
    DIAGNOSTIC_PACKAGES+=("${nvidia_packages[@]}")
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
    log "Installing live boot, the pinned kernel and minimal diagnostic userland"
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get update
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install -y \
        --no-install-recommends \
        "linux-image-$KERNEL_ABI-generic" "linux-modules-$KERNEL_ABI-generic" \
        live-boot systemd-sysv initramfs-tools \
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
IMAGE_STORAGE_TOOLS=(sfdisk wipefs partx mkntfs mkfs.ext4 ntfsfix ntfsinfo ntfs-3g.probe udevadm blkid lsblk findmnt mount umount dmesg)

verify_image_storage_tools() {
    local missing=() tool
    for tool in "${IMAGE_STORAGE_TOOLS[@]}"; do
        sudo chroot "$ROOTFS" /bin/sh -c "command -v $tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    (( ${#missing[@]} == 0 )) || die "image rootfs lacks storage tools: ${missing[*]}"
    log "Storage tools present: ${IMAGE_STORAGE_TOOLS[*]}"
}

# The pinned kernel (KERNEL_ABI) and no other: the live boot uses the newest vmlinuz in /boot and
# the NVIDIA layer the newest /lib/modules, so an older kernel left behind would only cost room.
ensure_kernel() {
    local kernel="linux-image-$KERNEL_ABI-generic" others
    if ! sudo chroot "$ROOTFS" dpkg-query -W -f='${db:Status-Status}' "$kernel" 2>/dev/null \
        | grep -qx installed; then
        log "Installing the pinned kernel $KERNEL_ABI-generic"
        printf '#!/bin/sh\nexit 101\n' | sudo tee "$ROOTFS/usr/sbin/policy-rc.d" >/dev/null
        sudo chmod 0755 "$ROOTFS/usr/sbin/policy-rc.d"
        mount_chroot_filesystems
        sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get update
        sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install -y \
            --no-install-recommends "$kernel" "linux-modules-$KERNEL_ABI-generic"
        sudo chroot "$ROOTFS" apt-get clean
        sudo rm -f "$ROOTFS/usr/sbin/policy-rc.d"
        unmount_chroot_filesystems
    fi
    others="$(sudo chroot "$ROOTFS" dpkg-query -W -f='${Package} ${db:Status-Status}\n' \
        'linux-image-*' 'linux-modules-*' 'linux-headers-*' 'linux-hwe-*' 2>/dev/null \
        | awk -v keep="$KERNEL_ABI" '$2 == "installed" && index($1, keep) == 0 { print $1 }')"
    if [[ -n "$others" ]]; then
        log "Removing other kernels: $(echo $others)"
        mount_chroot_filesystems
        # shellcheck disable=SC2086
        sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get purge -y $others
        unmount_chroot_filesystems
    fi
    [[ -f "$ROOTFS/boot/vmlinuz-$KERNEL_ABI-generic" && -f "$ROOTFS/boot/initrd.img-$KERNEL_ABI-generic" ]] \
        || die "the pinned kernel $KERNEL_ABI-generic is not complete in the rootfs"
    if command -v sbverify >/dev/null 2>&1; then
        sudo sbverify --list "$ROOTFS/boot/vmlinuz-$KERNEL_ABI-generic" 2>/dev/null \
            | grep -q 'Canonical' || die "vmlinuz-$KERNEL_ABI-generic carries no Canonical signature"
    fi
    log "Kernel: $KERNEL_ABI-generic (signed)"
}

# Ubuntu's signed UEFI boot chain, downloaded once from the archive into $EFI_SIGNED: shimx64
# (signed by Microsoft's UEFI CA), MokManager, and GRUB signed by Canonical. Firmware with Secure
# Boot on trusts shim, shim trusts Canonical's GRUB and kernel: the USB boots without turning
# Secure Boot off.
fetch_signed_efi() {
    local file
    if [[ -s "$EFI_SIGNED/shimx64.efi.signed" && -s "$EFI_SIGNED/grubx64.efi.signed" \
        && -s "$EFI_SIGNED/mmx64.efi" ]]; then
        return
    fi
    log "Downloading Ubuntu's signed shim and GRUB"
    sudo rm -rf "$ROOTFS/var/tmp/dit-efi" "$EFI_SIGNED"
    sudo mkdir -p "$ROOTFS/var/tmp/dit-efi"
    mount_chroot_filesystems
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get update
    sudo chroot "$ROOTFS" sh -c 'cd /var/tmp/dit-efi && apt-get download shim-signed grub-efi-amd64-signed'
    unmount_chroot_filesystems
    mkdir -p "$EFI_SIGNED/x"
    for file in "$ROOTFS"/var/tmp/dit-efi/*.deb; do
        dpkg-deb -x "$file" "$EFI_SIGNED/x"
    done
    sudo rm -rf "$ROOTFS/var/tmp/dit-efi"
    if [[ -f "$EFI_SIGNED/x/usr/lib/shim/shimx64.efi.signed.latest" ]]; then
        install -m 0644 "$EFI_SIGNED/x/usr/lib/shim/shimx64.efi.signed.latest" "$EFI_SIGNED/shimx64.efi.signed"
    else
        install -m 0644 "$EFI_SIGNED/x/usr/lib/shim/shimx64.efi.signed" "$EFI_SIGNED/shimx64.efi.signed"
    fi
    install -m 0644 "$EFI_SIGNED/x/usr/lib/shim/mmx64.efi" "$EFI_SIGNED/mmx64.efi"
    install -m 0644 "$EFI_SIGNED/x/usr/lib/grub/x86_64-efi-signed/grubx64.efi.signed" \
        "$EFI_SIGNED/grubx64.efi.signed"
    rm -rf "$EFI_SIGNED/x"
    if command -v sbverify >/dev/null 2>&1; then
        sbverify --list "$EFI_SIGNED/shimx64.efi.signed" | grep -q 'Microsoft' \
            || die "shimx64 is not signed by Microsoft"
        sbverify --list "$EFI_SIGNED/grubx64.efi.signed" | grep -q 'Canonical' \
            || die "grubx64 is not signed by Canonical"
    fi
}

# Signed GRUB reads /EFI/ubuntu/grub.cfg on the partition it was loaded from; that file finds the
# boot payload by this build's own marker and opens the one boot menu, shared with BIOS.
render_efi_grub_cfg() {
    local image_id="$1" dest="$2"
    printf '%s\n' \
        "search --no-floppy --file --set=root /cpu-dit/boot-$image_id.mark" \
        'set prefix=($root)/boot/grub' \
        'configfile ($root)/boot/grub/grub.cfg' >"$dest"
    chmod 0644 "$dest"
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
    # The rescue console (tty2) and Magic SysRq: a way out, with a record, when the test screen
    # stops answering (docs/FAULT_CATCH_ROADMAP.md, FC1).
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/etc/systemd/system/dit-rescue.service" \
        "$ROOTFS/etc/systemd/system/dit-rescue.service"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/etc/sysctl.d/99-dit-sysrq.conf" \
        "$ROOTFS/etc/sysctl.d/99-dit-sysrq.conf"
    # The black box (tty3): a journal of every session, and its result when the test cannot
    # write it (FC3a).
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/etc/systemd/system/dit-blackbox.service" \
        "$ROOTFS/etc/systemd/system/dit-blackbox.service"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/boot-runner.sh"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/cpu-dit-ui.sh"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/driver-probe.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/driver-probe.sh"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/partition-lookup.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/partition-lookup.sh"
    sudo install -D -m 0644 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/nvidia-flavor.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/nvidia-flavor.sh"
    sudo install -D -m 0755 "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/nvidia-driver-install.sh" \
        "$ROOTFS/usr/local/lib/cpu-dit/nvidia-driver-install.sh"
    sudo install -D -m 0755 "$ROOT_DIR/target/release/dit-cli" "$ROOTFS/opt/cpu-dit-01/dit-cli"
    for level in "${ISA_VARIANTS[@]}"; do
        sudo install -D -m 0755 "$(isa_variant_binary "$level")" "$ROOTFS/opt/cpu-dit-01/dit-cli-$level"
    done
    sudo install -D -m 0644 "$ROOT_DIR/vectors/source-v1.dat" \
        "$ROOTFS/opt/cpu-dit-01/vectors/source-v1.dat"
    sudo install -D -m 0644 "$ROOT_DIR/vectors/source-v1.manifest.json" \
        "$ROOTFS/opt/cpu-dit-01/vectors/source-v1.manifest.json"
    sudo install -D -m 0644 "$ROOT_DIR/vectors/golden-v1.json" \
        "$ROOTFS/opt/cpu-dit-01/vectors/golden-v1.json"
    printf '%s\n' "$image_id" | sudo tee "$ROOTFS/etc/cpu-dit/image-id" >/dev/null
    printf '%s\n' "$DIT_VERSION" | sudo tee "$ROOTFS/etc/cpu-dit/version" >/dev/null

    sudo mkdir -p "$ROOTFS/etc/systemd/system/multi-user.target.wants"
    sudo ln -sfn /etc/systemd/system/cpu-dit.service \
        "$ROOTFS/etc/systemd/system/multi-user.target.wants/cpu-dit.service"
    sudo ln -sfn /etc/systemd/system/dit-rescue.service \
        "$ROOTFS/etc/systemd/system/multi-user.target.wants/dit-rescue.service"
    sudo ln -sfn /etc/systemd/system/dit-blackbox.service \
        "$ROOTFS/etc/systemd/system/multi-user.target.wants/dit-blackbox.service"
    for unit in apt-daily.timer apt-daily-upgrade.timer fstrim.timer \
        getty@tty1.service getty@tty2.service getty@tty3.service \
        serial-getty@ttyS0.service rasdaemon.service \
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
        echo "dit_version=$DIT_VERSION"
        echo "ubuntu_release=noble"
        echo "architecture=amd64"
        echo "dit_binary_sha256=$(sha256sum "$ROOT_DIR/target/release/dit-cli" | awk '{print $1}')"
        for level in "${ISA_VARIANTS[@]}"; do
            echo "dit_binary_${level}_sha256=$(sha256sum "$(isa_variant_binary "$level")" | awk '{print $1}')"
        done
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
    [[ -x "$ROOTFS/usr/local/bin/dit-rescue" ]] \
        || die "DIT extension did not install the rescue console"
    [[ -x "$ROOTFS/usr/local/bin/dit-blackbox" ]] \
        || die "DIT extension did not install the black box"
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
        -e "s|@DIT_VERSION@|$DIT_VERSION|" \
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
    printf '%s\n' "$image_id" >"$staging/cpu-dit/boot-$image_id.mark"
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
    log "Installing the signed UEFI chain (shim -> Canonical GRUB): boots with Secure Boot on"
    sudo install -D -m 0644 "$EFI_SIGNED/shimx64.efi.signed" "$boot_mount/EFI/BOOT/BOOTX64.EFI"
    sudo install -D -m 0644 "$EFI_SIGNED/grubx64.efi.signed" "$boot_mount/EFI/BOOT/grubx64.efi"
    sudo install -D -m 0644 "$EFI_SIGNED/mmx64.efi" "$boot_mount/EFI/BOOT/mmx64.efi"
    render_efi_grub_cfg "$image_id" "$TEMP_DIR/efi-grub.cfg"
    sudo install -D -m 0644 "$TEMP_DIR/efi-grub.cfg" "$boot_mount/EFI/ubuntu/grub.cfg"
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
  "dit_version": "$DIT_VERSION",
  "built_local": "$BUILD_TIME_LOCAL",
  "filename": "$(basename "$image")",
  "size_bytes": $bytes,
  "sha256": "$sha",
  "ubuntu_release": "24.04-noble",
  "architecture": "amd64",
  "boot_modes": ["legacy_bios", "uefi"],
  "secure_boot": true,
  "kernel": "$KERNEL_ABI-generic",
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
        for level in "${ISA_VARIANTS[@]}"; do
            sha256sum "$(isa_variant_binary "$level")"
        done
        printf 'kernel %s\nversion %s\n' "$KERNEL_ABI" "$DIT_VERSION"
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

# The CPU kernels (dit-cli) built again for x86-64-v3 (AVX2+FMA) and x86-64-v4 (AVX-512), each in
# its own target directory; the baseline dit-cli runs the widest one the CPU has (v1.2 C4,
# crates/dit-system/src/isa.rs). The explicit --target keeps RUSTFLAGS off the build scripts and
# proc macros, which run on this build machine: built for AVX-512, they kill rustc with SIGILL on
# a machine without it.
ISA_VARIANTS=(v3 v4)
ISA_TARGET=x86_64-unknown-linux-gnu
isa_variant_binary() {
    printf '%s\n' "$ROOT_DIR/target/isa-$1/$ISA_TARGET/release/dit-cli"
}

build_isa_variants() {
    local level
    for level in "${ISA_VARIANTS[@]}"; do
        log "Building dit-cli for x86-64-$level"
        CARGO_TARGET_DIR="$ROOT_DIR/target/isa-$level" RUSTFLAGS="-C target-cpu=x86-64-$level" \
            cargo build --release -p dit-cli --target "$ISA_TARGET" \
            --manifest-path "$ROOT_DIR/Cargo.toml"
        [[ -x "$(isa_variant_binary "$level")" ]] || die "dit-cli for x86-64-$level was not built"
    done
}

main() {
    require_project
    install_host_dependencies
    mkdir -p "$BUILD_ROOT" "$OUTPUT_DIR"
    # One version for the whole build: every screen (through cargo), the image, its menu.
    DIT_VERSION="$("$ROOT_DIR/tools/dit-version.sh")"
    export DIT_VERSION
    log "Building unified DIT-01 v$DIT_VERSION workspace (CPU-RAM, CPU-SSD/NVMe, GPU-VRAM, control center)"
    cargo build --release --workspace --manifest-path "$ROOT_DIR/Cargo.toml"
    build_isa_variants
    "$ROOT_DIR/tools/build_image_extension.sh"
    bootstrap_rootfs
    ensure_diagnostic_packages
    ensure_kernel
    fetch_signed_efi
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
