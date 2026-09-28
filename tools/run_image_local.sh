#!/bin/bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${CPU_DIT_VM_IMAGE:-$ROOT_DIR/dist/dit-01-noble-amd64.img}"
MODE="${1:-uefi}"
DISPLAY_MODE="${CPU_DIT_VM_DISPLAY:-gtk}"
MEMORY_MIB="${CPU_DIT_VM_MEMORY_MIB:-4096}"
CPUS="${CPU_DIT_VM_CPUS:-4}"
PERSIST="${CPU_DIT_VM_PERSIST:-0}"
VM_ROOT="$ROOT_DIR/build/local-image-vm"

die() {
    printf '[CPU-DIT local VM] ERROR: %s\n' "$*" >&2
    exit 1
}

case "$MODE" in
    bios|uefi) ;;
    *) die "usage: $0 [uefi|bios]" ;;
esac
case "$DISPLAY_MODE" in
    gtk|sdl|curses) ;;
    *) die "CPU_DIT_VM_DISPLAY must be gtk, sdl or curses" ;;
esac
[[ "$MEMORY_MIB" =~ ^[0-9]+$ && "$MEMORY_MIB" -ge 2048 ]] \
    || die "CPU_DIT_VM_MEMORY_MIB must be an integer >= 2048"
[[ "$CPUS" =~ ^[0-9]+$ && "$CPUS" -ge 1 ]] \
    || die "CPU_DIT_VM_CPUS must be a positive integer"
[[ -f "$IMAGE" ]] || die "image not found: $IMAGE"
command -v qemu-system-x86_64 >/dev/null 2>&1 || die "qemu-system-x86_64 is required"

mkdir -p "$VM_ROOT"
drive_file="$IMAGE"
snapshot=on
if [[ "$PERSIST" == 1 ]]; then
    timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
    drive_file="$VM_ROOT/cpu-dit-local-$timestamp.img"
    cp --reflink=auto --sparse=always "$IMAGE" "$drive_file"
    snapshot=off
    printf '[CPU-DIT local VM] Persistent working copy: %s\n' "$drive_file"
else
    printf '[CPU-DIT local VM] Snapshot mode: release image will not be modified\n'
fi

accel=tcg
if [[ -r /dev/kvm && -w /dev/kvm ]]; then
    accel=kvm
fi

firmware_args=()
if [[ "$MODE" == uefi ]]; then
    ovmf_code="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
    ovmf_vars="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"
    [[ -r "$ovmf_code" && -r "$ovmf_vars" ]] || die "OVMF firmware not found"
    cp "$ovmf_vars" "$VM_ROOT/OVMF_VARS-local.fd"
    firmware_args=(
        -drive "if=pflash,format=raw,readonly=on,file=$ovmf_code"
        -drive "if=pflash,format=raw,file=$VM_ROOT/OVMF_VARS-local.fd"
    )
fi

printf '[CPU-DIT local VM] Booting %s with %s, %s MiB RAM, %s vCPU\n' \
    "$MODE" "$accel" "$MEMORY_MIB" "$CPUS"
printf '[CPU-DIT local VM] Close the VM window only after CPU-DIT has stopped.\n'

exec qemu-system-x86_64 \
    -name CPU-DIT-01-LOCAL \
    -machine "q35,accel=$accel" \
    -m "$MEMORY_MIB" \
    -smp "$CPUS" \
    -drive "if=virtio,format=raw,file=$drive_file,snapshot=$snapshot" \
    -boot menu=on \
    -display "$DISPLAY_MODE" \
    -serial mon:stdio \
    -no-reboot \
    "${firmware_args[@]}"
