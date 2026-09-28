#!/bin/bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="$ROOT_DIR/dist/dit-01-noble-amd64.img"
DEVICE=''
DRY_RUN=0
LIST_ONLY=0
NVIDIA_PACKAGE=''

usage() {
    cat <<'EOF'
Usage: tools/burn_usb.sh [options]

Interactively select a USB whole disk, write the CPU-DIT raw image, then verify
the written bytes with SHA-256. ALL DATA ON THE SELECTED USB WILL BE ERASED.

Options:
  --device /dev/sdX  Preselect a USB whole disk (still requires confirmation)
  --image FILE       Use FILE instead of the default dist image
  --nvidia-driver-package FILE
                      After a successful burn+verify, also stage FILE (and its
                      .sha256 sidecar) onto the image's DIT-RESULTS partition, so
                      nvidia-driver-install.sh installs it at boot on NVIDIA
                      hardware. Build FILE with 'make nvidia-driver-package'.
  --list             List eligible USB whole disks and exit
  --dry-run          Validate and show the selected target; do not write
  -h, --help         Show this help
EOF
}

die() {
    printf '[CPU-DIT USB] ERROR: %s\n' "$*" >&2
    exit 2
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is missing: $1"
}

usb_disks() {
    lsblk -dnpo NAME,TYPE,TRAN 2>/dev/null \
        | awk '$2 == "disk" && $3 == "usb" {print $1}'
}

show_device() {
    local device="$1"
    lsblk -dnpo NAME,SIZE,MODEL,SERIAL,TRAN,RM,RO "$device"
    lsblk -lnpo NAME,SIZE,FSTYPE,LABEL,MOUNTPOINTS "$device" | sed 's/^/    /'
}

source_is_on_device() {
    local source="$1" device="$2" ancestor
    [[ "$source" == /dev/* && -b "$source" ]] || return 1
    while IFS= read -r ancestor; do
        [[ "$(readlink -f "$ancestor")" == "$device" ]] && return 0
    done < <(lsblk -snrpo NAME "$source" 2>/dev/null || true)
    return 1
}

guard_host_storage() {
    local device="$1" path source
    for path in / /home "$ROOT_DIR" "$IMAGE"; do
        [[ -e "$path" ]] || continue
        source="$(findmnt -nro SOURCE -T "$path" 2>/dev/null || true)"
        if source_is_on_device "$source" "$device"; then
            die "refusing to overwrite $device because it backs $path ($source)"
        fi
    done
}

guard_active_swap() {
    local device="$1" swap_device
    while IFS= read -r swap_device; do
        [[ -n "$swap_device" ]] || continue
        if source_is_on_device "$swap_device" "$device"; then
            die "active swap is present on $device; disable it manually before burning"
        fi
    done < <(swapon --noheadings --raw --show=NAME 2>/dev/null || true)
}

validate_device() {
    local requested="$1" device_type transport read_only
    [[ "$requested" == /dev/* ]] || die "target must be an absolute /dev path"
    DEVICE="$(readlink -f "$requested")"
    [[ -b "$DEVICE" ]] || die "target is not a block device: $DEVICE"
    device_type="$(lsblk -dnro TYPE "$DEVICE" 2>/dev/null || true)"
    transport="$(lsblk -dnro TRAN "$DEVICE" 2>/dev/null || true)"
    read_only="$(lsblk -dnro RO "$DEVICE" 2>/dev/null || true)"
    [[ "$device_type" == disk ]] || die "select the whole disk, not a partition: $DEVICE"
    [[ "$transport" == usb ]] || die "refusing non-USB target: $DEVICE (TRAN=$transport)"
    [[ "$read_only" == 0 ]] || die "USB is read-only: $DEVICE"
    guard_host_storage "$DEVICE"
    guard_active_swap "$DEVICE"
}

select_device() {
    local selection index
    local -a candidates=()
    mapfile -t candidates < <(usb_disks)
    ((${#candidates[@]} > 0)) || die 'no USB whole disk was detected'
    [[ -t 0 ]] || die 'interactive selection requires a terminal; use --device /dev/sdX'

    printf '[CPU-DIT USB] Eligible USB disks:\n'
    for index in "${!candidates[@]}"; do
        printf '  [%d] ' "$((index + 1))"
        lsblk -dnpo NAME,SIZE,MODEL,SERIAL,TRAN,RM "${candidates[$index]}"
    done
    printf 'Select USB number: '
    IFS= read -r selection
    [[ "$selection" =~ ^[0-9]+$ ]] || die 'selection must be a number'
    ((selection >= 1 && selection <= ${#candidates[@]})) || die 'selection is out of range'
    DEVICE="${candidates[$((selection - 1))]}"
}

while (($# > 0)); do
    case "$1" in
        --device)
            (($# >= 2)) || die '--device requires a value'
            DEVICE="$2"
            shift 2
            ;;
        --image)
            (($# >= 2)) || die '--image requires a value'
            IMAGE="$2"
            shift 2
            ;;
        --nvidia-driver-package)
            (($# >= 2)) || die '--nvidia-driver-package requires a value'
            NVIDIA_PACKAGE="$2"
            shift 2
            ;;
        --list) LIST_ONLY=1; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1" ;;
    esac
done

for command_name in lsblk findmnt readlink awk sed tac swapon umount \
    sha256sum stat sudo dd head blockdev; do
    require_command "$command_name"
done
if [[ -n "$NVIDIA_PACKAGE" ]]; then
    for command_name in partprobe udevadm mount mktemp; do
        require_command "$command_name"
    done
fi

if ((LIST_ONLY == 1)); then
    mapfile -t listed_devices < <(usb_disks)
    if ((${#listed_devices[@]} == 0)); then
        printf '[CPU-DIT USB] No USB whole disk detected.\n'
        exit 0
    fi
    for listed_device in "${listed_devices[@]}"; do
        show_device "$listed_device"
    done
    exit 0
fi

IMAGE="$(readlink -f "$IMAGE")"
[[ -f "$IMAGE" && -s "$IMAGE" ]] || die "image is missing or empty: $IMAGE"
CHECKSUM_FILE="$IMAGE.sha256"
[[ -f "$CHECKSUM_FILE" && -s "$CHECKSUM_FILE" ]] \
    || die "checksum file is missing: $CHECKSUM_FILE"

printf '[CPU-DIT USB] Verifying source image checksum...\n'
EXPECTED_SHA="$(awk 'NR == 1 {print $1}' "$CHECKSUM_FILE")"
[[ "$EXPECTED_SHA" =~ ^[0-9a-fA-F]{64}$ ]] || die 'invalid SHA-256 checksum file'
ACTUAL_SHA="$(sha256sum "$IMAGE" | awk '{print $1}')"
[[ "${ACTUAL_SHA,,}" == "${EXPECTED_SHA,,}" ]] || die 'source image checksum mismatch'
IMAGE_BYTES="$(stat -Lc %s "$IMAGE")"

if [[ -n "$NVIDIA_PACKAGE" ]]; then
    NVIDIA_PACKAGE="$(readlink -f "$NVIDIA_PACKAGE")"
    [[ -f "$NVIDIA_PACKAGE" && -s "$NVIDIA_PACKAGE" ]] \
        || die "NVIDIA driver package is missing or empty: $NVIDIA_PACKAGE"
    NVIDIA_CHECKSUM_FILE="$NVIDIA_PACKAGE.sha256"
    [[ -f "$NVIDIA_CHECKSUM_FILE" && -s "$NVIDIA_CHECKSUM_FILE" ]] \
        || die "NVIDIA driver package checksum file is missing: $NVIDIA_CHECKSUM_FILE"
    printf '[CPU-DIT USB] Verifying NVIDIA driver package checksum...\n'
    (cd "$(dirname "$NVIDIA_PACKAGE")" && sha256sum --check --strict \
        "$(basename "$NVIDIA_CHECKSUM_FILE")") \
        || die "NVIDIA driver package checksum mismatch: $NVIDIA_PACKAGE"
fi

if [[ -n "$DEVICE" ]]; then
    validate_device "$DEVICE"
else
    select_device
    validate_device "$DEVICE"
fi

DEVICE_BYTES="$(lsblk -bdnro SIZE "$DEVICE")"
[[ "$DEVICE_BYTES" =~ ^[0-9]+$ ]] || die "cannot read target size: $DEVICE"
((DEVICE_BYTES >= IMAGE_BYTES)) \
    || die "USB is too small: $DEVICE_BYTES bytes; image needs $IMAGE_BYTES bytes"

printf '\n[CPU-DIT USB] Source image:\n  %s\n' "$IMAGE"
printf '  bytes=%s sha256=%s\n' "$IMAGE_BYTES" "$ACTUAL_SHA"
printf '[CPU-DIT USB] Selected target (ALL DATA WILL BE ERASED):\n'
show_device "$DEVICE"

if ((DRY_RUN == 1)); then
    printf '\n[CPU-DIT USB] DRY RUN PASS — no device was unmounted or written.\n'
    exit 0
fi

CONFIRM_TEXT="ERASE $DEVICE"
printf '\nType exactly "%s" to continue: ' "$CONFIRM_TEXT"
IFS= read -r confirmation
[[ "$confirmation" == "$CONFIRM_TEXT" ]] || die 'confirmation did not match; nothing was written'

INITIAL_IDENTITY="$(lsblk -bdnro MAJ:MIN,SIZE,MODEL,SERIAL,TRAN "$DEVICE")"
sudo -v
CURRENT_IDENTITY="$(lsblk -bdnro MAJ:MIN,SIZE,MODEL,SERIAL,TRAN "$DEVICE")"
[[ "$CURRENT_IDENTITY" == "$INITIAL_IDENTITY" ]] \
    || die 'USB identity changed after confirmation; refusing to write'

while IFS= read -r mounted_node; do
    [[ -n "$mounted_node" ]] || continue
    printf '[CPU-DIT USB] Unmounting %s\n' "$mounted_node"
    sudo umount -- "$mounted_node"
done < <(lsblk -lnpo NAME,MOUNTPOINTS "$DEVICE" \
    | awk 'NF >= 2 && $2 != "" {print $1}' | tac)

CURRENT_IDENTITY="$(lsblk -bdnro MAJ:MIN,SIZE,MODEL,SERIAL,TRAN "$DEVICE")"
[[ "$CURRENT_IDENTITY" == "$INITIAL_IDENTITY" ]] \
    || die 'USB identity changed before write; refusing to continue'

printf '[CPU-DIT USB] Writing image to %s...\n' "$DEVICE"
sudo dd if="$IMAGE" of="$DEVICE" bs=16M iflag=fullblock status=progress conv=fsync
sudo blockdev --flushbufs "$DEVICE"

printf '[CPU-DIT USB] Reading back %s bytes for SHA-256 verification...\n' "$IMAGE_BYTES"
READBACK_SHA="$(sudo head -c "$IMAGE_BYTES" "$DEVICE" | sha256sum | awk '{print $1}')"
[[ "${READBACK_SHA,,}" == "${ACTUAL_SHA,,}" ]] \
    || die "USB read-back checksum mismatch: $READBACK_SHA"

NVIDIA_STAGED=0
if [[ -n "$NVIDIA_PACKAGE" ]]; then
    printf '[CPU-DIT USB] Staging NVIDIA driver package onto DIT-RESULTS...\n'
    sudo partprobe "$DEVICE" 2>/dev/null || true
    sudo udevadm settle --timeout=10 2>/dev/null || true
    RESULTS_PARTITION="$(lsblk -lnpo NAME,LABEL,TYPE "$DEVICE" \
        | awk '$3 == "part" && $2 == "DIT-RESULTS" {print $1; exit}')"
    [[ -n "$RESULTS_PARTITION" ]] \
        || die "DIT-RESULTS partition not found on $DEVICE after burning; the image may predate it"
    RESULTS_MOUNT="$(mktemp -d)"
    sudo mount -o rw,nosuid,nodev,noexec,umask=0077 "$RESULTS_PARTITION" "$RESULTS_MOUNT"
    sudo mkdir -p "$RESULTS_MOUNT/nvidia-driver"
    sudo rm -f "$RESULTS_MOUNT/nvidia-driver/dit-nvidia-driver-package.tar.gz" \
        "$RESULTS_MOUNT/nvidia-driver/dit-nvidia-driver-package.tar.gz.sha256"
    sudo cp "$NVIDIA_PACKAGE" \
        "$RESULTS_MOUNT/nvidia-driver/dit-nvidia-driver-package.tar.gz"
    printf '%s  dit-nvidia-driver-package.tar.gz\n' \
        "$(awk '{print $1}' "$NVIDIA_CHECKSUM_FILE")" \
        | sudo tee "$RESULTS_MOUNT/nvidia-driver/dit-nvidia-driver-package.tar.gz.sha256" \
        >/dev/null
    sudo sync "$RESULTS_MOUNT"
    # The vfat mount (umask=0077, no uid=/gid= given) makes nvidia-driver/ root-owned
    # mode 0700, so even the cd to read it back needs root, not just the file writes.
    sudo sh -c 'cd "$1" && sha256sum --check --strict dit-nvidia-driver-package.tar.gz.sha256' \
        _ "$RESULTS_MOUNT/nvidia-driver" \
        || die "staged NVIDIA driver package failed read-back verification"
    sudo umount "$RESULTS_MOUNT"
    rmdir "$RESULTS_MOUNT"
    NVIDIA_STAGED=1
fi

printf '\nCPU-DIT USB BURN PASS\n'
printf 'Device: %s\nSHA-256: %s\n' "$DEVICE" "$READBACK_SHA"
if ((NVIDIA_STAGED == 1)); then
    printf 'NVIDIA driver package staged: %s\n' "$NVIDIA_PACKAGE"
fi
printf 'Wait for desktop automount activity to finish, then safely eject the USB.\n'
