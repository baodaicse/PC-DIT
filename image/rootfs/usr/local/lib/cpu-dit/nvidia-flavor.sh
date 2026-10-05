#!/bin/bash
# NVIDIA kernel module flavor for this machine, sourced by driver-probe.sh. The NVIDIA driver
# layer carries both flavors of the same driver version (docs/gpu-vram/NVIDIA_DRIVER_PACKAGE.md):
#   open         the default: Turing (GTX 16xx, RTX 20xx) and newer, and the only flavor that
#                drives Blackwell (RTX 50xx). It needs the GPU's GSP, which older GPUs lack.
#   proprietary  also Maxwell, Pascal and Volta (GTX 9xx / 10xx, Titan V); not Blackwell.
# No device-ID table: the open modules load first; when they leave an NVIDIA GPU without a driver
# the proprietary ones are tried, and the flavor that drives more of this machine's NVIDIA GPUs
# stays (the open one on a tie). The paths and commands can be pointed elsewhere for the tests
# (tools/test_nvidia_flavor.sh).

NVIDIA_FLAVOR_SYS="${NVIDIA_FLAVOR_SYS:-/sys}"
NVIDIA_FLAVOR_ROOT="${NVIDIA_FLAVOR_ROOT:-/usr/lib/dit/nvidia-modules}"
NVIDIA_FLAVOR_KERNEL="${NVIDIA_FLAVOR_KERNEL:-$(uname -r)}"
NVIDIA_FLAVOR_MODULES="${NVIDIA_FLAVOR_MODULES:-/lib/modules/$NVIDIA_FLAVOR_KERNEL}"
NVIDIA_FLAVOR_STATE="${NVIDIA_FLAVOR_STATE:-/run/cpu-dit-nvidia-flavor}"
NVIDIA_FLAVOR_MODPROBE="${NVIDIA_FLAVOR_MODPROBE:-modprobe}"
NVIDIA_FLAVOR_DEPMOD="${NVIDIA_FLAVOR_DEPMOD:-depmod}"

# NVIDIA display controllers (VGA, 3D or display class) on the PCI bus.
nvidia_gpu_count() {
    local device count=0
    for device in "$NVIDIA_FLAVOR_SYS"/bus/pci/devices/*; do
        [[ "$(cat "$device/vendor" 2>/dev/null)" == 0x10de ]] || continue
        [[ "$(cat "$device/class" 2>/dev/null)" == 0x03* ]] || continue
        count=$((count + 1))
    done
    printf '%s\n' "$count"
}

# NVIDIA GPUs the loaded nvidia module drives (bound to it on the PCI bus).
nvidia_bound_count() {
    local device count=0
    for device in "$NVIDIA_FLAVOR_SYS"/bus/pci/drivers/nvidia/0000:*; do
        [[ -e "$device" ]] && count=$((count + 1))
    done
    printf '%s\n' "$count"
}

# Loads nvidia (no-op when udev already did) and prints how many GPUs it drives.
nvidia_try() {
    timeout --foreground --signal=TERM --kill-after=5s 30s \
        "$NVIDIA_FLAVOR_MODPROBE" nvidia >/dev/null 2>&1 || true
    nvidia_bound_count
}

# Unloads the nvidia stack; fails when it stays loaded (a console or a process holds it).
nvidia_unload() {
    local module
    for module in nvidia_uvm nvidia_drm nvidia_modeset nvidia_peermem nvidia; do
        "$NVIDIA_FLAVOR_MODPROBE" -r "$module" >/dev/null 2>&1 || true
    done
    [[ ! -d "$NVIDIA_FLAVOR_SYS/module/nvidia" ]]
}

# Puts `flavor`'s modules where modprobe finds them (updates/dkms) and rebuilds the index.
nvidia_use_flavor() {
    local source="$NVIDIA_FLAVOR_ROOT/$1/$NVIDIA_FLAVOR_KERNEL"
    [[ -f "$source/nvidia.ko" || -f "$source/nvidia.ko.zst" ]] || return 1
    nvidia_unload || return 1
    mkdir -p "$NVIDIA_FLAVOR_MODULES/updates/dkms" || return 1
    rm -f "$NVIDIA_FLAVOR_MODULES"/updates/dkms/nvidia*.ko*
    cp -f "$source"/nvidia*.ko* "$NVIDIA_FLAVOR_MODULES/updates/dkms/" || return 1
    "$NVIDIA_FLAVOR_DEPMOD" -a "$NVIDIA_FLAVOR_KERNEL" >/dev/null 2>&1
}

# Chooses the flavor, appending each step to `log` (the driver-probe log) and the choice to
# NVIDIA_FLAVOR_STATE.
select_nvidia_flavor() {
    local log="$1" total open proprietary
    total="$(nvidia_gpu_count)"
    if (( total == 0 )); then
        printf 'nvidia_flavor none (no NVIDIA GPU)\n' >>"$log"
        return 0
    fi
    open="$(nvidia_try)"
    printf 'nvidia_flavor open drives %s of %s NVIDIA GPU(s)\n' "$open" "$total" >>"$log"
    if (( open >= total )) || [[ ! -d "$NVIDIA_FLAVOR_ROOT/proprietary/$NVIDIA_FLAVOR_KERNEL" ]]; then
        printf 'open\n' >"$NVIDIA_FLAVOR_STATE"
        return 0
    fi
    if ! nvidia_use_flavor proprietary; then
        printf 'nvidia_flavor could not switch to proprietary (nvidia stays loaded); open kept\n' \
            >>"$log"
        printf 'open\n' >"$NVIDIA_FLAVOR_STATE"
        return 0
    fi
    proprietary="$(nvidia_try)"
    printf 'nvidia_flavor proprietary drives %s of %s NVIDIA GPU(s)\n' "$proprietary" "$total" \
        >>"$log"
    if (( proprietary > open )); then
        printf 'proprietary\n' >"$NVIDIA_FLAVOR_STATE"
        return 0
    fi
    # Not better (a Blackwell card the proprietary modules refuse): back to open.
    if nvidia_use_flavor open; then
        open="$(nvidia_try)"
    fi
    printf 'nvidia_flavor back to open, drives %s of %s NVIDIA GPU(s)\n' "$open" "$total" >>"$log"
    printf 'open\n' >"$NVIDIA_FLAVOR_STATE"
}
