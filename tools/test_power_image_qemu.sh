#!/bin/bash
# Read-only image smoke: direct kernel boot + snapshot disk; never changes the image.
set -Eeuo pipefail
project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
image_path="${1:-$project_dir/dist/dit-01-noble-amd64.img}"
[[ -f "$image_path" ]] || { echo 'Image not found' >&2; exit 2; }
for tool in qemu-system-x86_64 mcopy sfdisk jq; do command -v "$tool" >/dev/null; done
test_dir="$(mktemp -d "$project_dir/build/power-qemu-XXXXXX")"
qemu_pid=''
cleanup() {
    if [[ -n "$qemu_pid" ]]; then kill "$qemu_pid" 2>/dev/null || true; wait "$qemu_pid" 2>/dev/null || true; fi
}
trap cleanup EXIT INT TERM
offset="$(sfdisk --json "$image_path" | jq -r '.partitiontable.partitions[1].start * .partitiontable.sectorsize')"
[[ "$offset" =~ ^[1-9][0-9]*$ ]]
mcopy -i "$image_path@@$offset" ::/live/vmlinuz "$test_dir/vmlinuz"
mcopy -i "$image_path@@$offset" ::/live/initrd.img "$test_dir/initrd.img"
qemu-system-x86_64 -machine q35,accel=tcg -m 4096 -smp 1 \
    -kernel "$test_dir/vmlinuz" -initrd "$test_dir/initrd.img" \
    -append 'boot=live components toram loglevel=3 live-media-timeout=30 cpu_dit_profile=power-campaign cpu_dit_power_duration=1 cpu_dit_serial=1 console=ttyS0' \
    -drive "file=$image_path,format=raw,if=virtio,snapshot=on" \
    -device virtio-rng-pci -display none -serial "file:$test_dir/serial.log" \
    -monitor none -no-reboot >"$test_dir/qemu.log" 2>&1 &
qemu_pid=$!
started=$SECONDS
while (( SECONDS - started < ${QEMU_TIMEOUT_SECONDS:-300} )); do
    if grep -Fq 'CPU-DIT BOOT TEST COMPLETE overall_exit=0' "$test_dir/serial.log" 2>/dev/null; then
        grep -Fq 'kernel=heavy-fp-v1' "$test_dir/serial.log"
        grep -Fq 'timing=' "$test_dir/serial.log"
        printf 'Power image direct-kernel smoke PASS: %s\n' "$test_dir/serial.log"
        exit 0
    fi
    if ! kill -0 "$qemu_pid" 2>/dev/null || grep -Eq 'CPU-DIT BOOT TEST COMPLETE overall_exit=[1-9]|Kernel panic|INVALID:' "$test_dir/serial.log" 2>/dev/null; then break; fi
    sleep 1
done
tail -n 80 "$test_dir/serial.log" >&2 || true
printf 'Power image smoke failed: %s\n' "$test_dir" >&2
exit 1
