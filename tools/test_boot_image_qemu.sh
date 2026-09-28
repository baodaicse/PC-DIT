#!/bin/bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${1:-$ROOT_DIR/dist/dit-01-noble-amd64.img}"
TEST_ROOT="$ROOT_DIR/build/qemu-smoke"
TIMEOUT_SECONDS="${QEMU_TIMEOUT_SECONDS:-420}"
# `toram` (see image/boot/grub/grub.cfg) copies the whole ~900 MiB live medium
# into a tmpfs at boot; 2048 MiB left too little effective-available RAM for
# even the Safe profile's 256 MiB target once that copy landed.
QEMU_MEMORY_MIB="${QEMU_MEMORY_MIB:-3072}"
QEMU_CPUS="${QEMU_CPUS:-2}"
OVMF_CODE="${OVMF_CODE:-/usr/share/OVMF/OVMF_CODE_4M.fd}"
OVMF_VARS="${OVMF_VARS:-/usr/share/OVMF/OVMF_VARS_4M.fd}"
ACTIVE_PID=""

log() {
    printf '[CPU-DIT QEMU] %s\n' "$*"
}

die() {
    printf '[CPU-DIT QEMU] ERROR: %s\n' "$*" >&2
    exit 1
}

cleanup() {
    if [[ -n "$ACTIVE_PID" ]] && kill -0 "$ACTIVE_PID" 2>/dev/null; then
        kill "$ACTIVE_PID" 2>/dev/null || true
        wait "$ACTIVE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT INT TERM

wait_for_completion() {
    local log_file="$1" started now
    started="$(date +%s)"
    while true; do
        if grep -Fq 'CPU-DIT BOOT TEST COMPLETE overall_exit=0' "$log_file" 2>/dev/null; then
            return 0
        fi
        if grep -Eq 'CPU-DIT BOOT TEST COMPLETE overall_exit=[1-9]|Kernel panic|emergency mode' \
            "$log_file" 2>/dev/null; then
            return 1
        fi
        if ! kill -0 "$ACTIVE_PID" 2>/dev/null; then
            wait "$ACTIVE_PID" || true
            return 1
        fi
        now="$(date +%s)"
        if (( now - started >= TIMEOUT_SECONDS )); then
            return 1
        fi
        sleep 2
    done
}

run_mode() {
    local mode="$1" log_file
    log_file="$TEST_ROOT/$mode-serial.log"
    local -a firmware_args=()
    : >"$log_file"

    if [[ "$mode" == uefi ]]; then
        [[ -r "$OVMF_CODE" && -r "$OVMF_VARS" ]] || die "OVMF firmware not found"
        cp "$OVMF_VARS" "$TEST_ROOT/OVMF_VARS.fd"
        firmware_args=(
            -drive "if=pflash,format=raw,readonly=on,file=$OVMF_CODE"
            -drive "if=pflash,format=raw,file=$TEST_ROOT/OVMF_VARS.fd"
        )
    fi

    log "Booting $mode (TCG, ${QEMU_MEMORY_MIB} MiB RAM, timeout ${TIMEOUT_SECONDS}s)"
    qemu-system-x86_64 \
        -machine q35,accel=tcg \
        -m "$QEMU_MEMORY_MIB" \
        -smp "$QEMU_CPUS" \
        -nodefaults \
        -device virtio-scsi-pci,id=scsi0 \
        -device scsi-hd,drive=systemdisk,bootindex=1 \
        -drive "id=systemdisk,if=none,format=raw,file=$IMAGE,snapshot=on" \
        -device virtio-serial-pci \
        -device virtio-rng-pci \
        -display none \
        -serial "file:$log_file" \
        -monitor none \
        -no-reboot \
        "${firmware_args[@]}" &
    ACTIVE_PID=$!

    if wait_for_completion "$log_file"; then
        log "$mode PASS"
    else
        tail -n 120 "$log_file" >&2 || true
        die "$mode did not complete successfully; log: $log_file"
    fi

    kill "$ACTIVE_PID" 2>/dev/null || true
    wait "$ACTIVE_PID" 2>/dev/null || true
    ACTIVE_PID=""
}

[[ -f "$IMAGE" ]] || die "image not found: $IMAGE"
command -v qemu-system-x86_64 >/dev/null 2>&1 || die "qemu-system-x86_64 is required"
mkdir -p "$TEST_ROOT"

run_mode bios
run_mode uefi
log "Both firmware modes passed"
