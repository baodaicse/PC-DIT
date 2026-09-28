#!/bin/bash
# Best-effort NVIDIA driver install from a package burned onto the DIT-RESULTS
# sibling partition (see tools/build_nvidia_driver_package.sh and
# docs/gpu-vram/NVIDIA_DRIVER_PACKAGE.md). Runs once per boot-runner invocation,
# before driver-probe.sh, so a freshly loaded module is visible to the probe.
#
# This is the offline alternative to a pre-baked `nvidia-image` flavor: one
# generic image, plus an optional driver package staged onto the same USB.
#
# nouveau is disabled outright on every image flavor, so there is nothing to
# unbind before installing: grub.cfg carries `module_blacklist=nouveau`, which
# the kernel checks in load_module() and so refuses every load (udev coldplug
# and explicit modprobe alike), and the rootfs/initramfs carry modprobe.d
# `install nouveau /bin/false`. (Found on real hardware: an RTX 5060 came up on
# nouveau before this, because udev coldplug bound it before this script ran.)
set -uo pipefail

LOG=/run/cpu-dit-nvidia-driver-install.txt
MARKER=/run/cpu-dit-nvidia-driver-runtime-installed
mkdir -p /run
: >"$LOG"
rm -f "$MARKER"

log() {
    printf '%s %s\n' "$(date -u --iso-8601=seconds)" "$*" >>"$LOG"
}

# Why a build or a module load failed: DKMS's own build log and the kernel's NVRM messages
# (unsupported GPU, GSP firmware, version mismatch...).
capture_dkms_logs() {
    local make_log
    for make_log in /var/lib/dkms/nvidia/*/build/make.log; do
        [[ -f $make_log ]] || continue
        log "--- $make_log (last 60 lines)"
        tail -n 60 "$make_log" >>"$LOG" 2>&1
    done
    log "--- dmesg NVRM/nvidia"
    dmesg 2>/dev/null | grep -iE 'nvrm|nvidia' | tail -n 60 >>"$LOG" 2>&1
}

# Also on the console: an install runs dpkg on ~250 packages and a DKMS compile before the menu
# appears, which can take several minutes with nothing else on screen.
say() {
    printf 'NVIDIA driver: %s\n' "$*"
    log "$*"
}

finish() {
    printf '%s\n' "$1" >"$MARKER"
    log "result: $1"
    case "$1" in
        skipped_no_nvidia_pci_device | skipped_baked_nvidia_flavor) ;;
        *) printf 'NVIDIA driver: %s (log: DIT-RESULTS/boot/nvidia-driver-install.txt)\n' "$1" ;;
    esac
    # This log otherwise lives only in /run (tmpfs, gone at reboot), and the interactive menu
    # profile execs straight into dit-suite, so boot-runner.sh's own end-of-run RUN_DIR copy is
    # never reached. Leave a copy on DIT-RESULTS for every outcome, mounting it if an early skip
    # returned before the mount below.
    mountpoint -q /mnt/cpu-dit-results 2>/dev/null || mount_result_partition 2>/dev/null
    if mountpoint -q /mnt/cpu-dit-results 2>/dev/null; then
        mkdir -p /mnt/cpu-dit-results/boot 2>/dev/null \
            && cp -f "$LOG" /mnt/cpu-dit-results/boot/nvidia-driver-install.txt 2>/dev/null
    fi
    exit 0
}

. /usr/local/lib/cpu-dit/partition-lookup.sh

if [[ -f /etc/cpu-dit/nvidia-driver-profile ]]; then
    finish "skipped_baked_nvidia_flavor"
fi

if ! command -v lspci >/dev/null 2>&1 \
    || ! lspci -nn -d 10de: 2>/dev/null | grep -q .; then
    finish "skipped_no_nvidia_pci_device"
fi
log "NVIDIA PCI device present: $(lspci -nn -d 10de: 2>/dev/null | head -1)"

if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >>"$LOG" 2>&1; then
    finish "skipped_already_working"
fi

if ! mount_result_partition; then
    finish "skipped_no_result_partition"
fi
log "DIT-RESULTS mounted at /mnt/cpu-dit-results"

PACKAGE_DIR=/mnt/cpu-dit-results/nvidia-driver
PACKAGE=$PACKAGE_DIR/dit-nvidia-driver-package.tar.gz
if [[ ! -s $PACKAGE || ! -s $PACKAGE.sha256 ]]; then
    finish "skipped_no_package_staged"
fi
log "found package: $PACKAGE"

if ! (cd "$PACKAGE_DIR" && sha256sum -c "$(basename "$PACKAGE").sha256") >>"$LOG" 2>&1; then
    finish "failed_package_checksum"
fi

# Not /run: the initramfs mounts it noexec (install.sh could not run from there -- the first
# real boot failed exactly so) and caps it at 10% of RAM. The root overlay allows exec and is
# live-boot's larger tmpfs.
STAGE=/var/tmp/dit-nvidia-driver
rm -rf "$STAGE"
mkdir -p "$STAGE"
package_kib=$(( $(stat -c %s "$PACKAGE") / 1024 ))
free_kib=$(df -Pk "$STAGE" | awk 'NR == 2 {print $4}')
log "stage $STAGE: package ${package_kib} KiB, free ${free_kib} KiB"
# The archive, its unpacked install and the DKMS build tree all land in RAM-backed overlay.
if [[ -z "$free_kib" ]] || (( free_kib < package_kib * 4 )); then
    finish "failed_no_space_for_install"
fi
say "package found, installing (dpkg + DKMS build, may take several minutes)..."
if ! tar -xzf "$PACKAGE" -C "$STAGE" >>"$LOG" 2>&1; then
    finish "failed_package_extract"
fi
if [[ ! -s $STAGE/MANIFEST.sha256 ]]; then
    finish "failed_package_layout"
fi
if ! (cd "$STAGE" && sha256sum -c MANIFEST.sha256) >>"$LOG" 2>&1; then
    finish "failed_manifest_checksum"
fi
log "package contents verified against MANIFEST.sha256"
if [[ -s $STAGE/manifest.json ]]; then
    log "manifest: $(tr -s '[:space:]' ' ' <"$STAGE/manifest.json")"
fi

if lsmod 2>/dev/null | grep -q '^nouveau '; then
    # Should be impossible with this image's kernel command line; refuse rather than fight it.
    finish "failed_nouveau_loaded_despite_blacklist"
fi

if [[ ! -s $STAGE/install.sh ]]; then
    finish "failed_missing_install_script"
fi
log "running bundled install.sh"
# Through sh, not by exec bit: independent of the stage filesystem's mount options.
# No tty (cpu-dit.service has StandardInput=null): never let a maintainer script prompt.
if ! DEBIAN_FRONTEND=noninteractive sh "$STAGE/install.sh" </dev/null >>"$LOG" 2>&1; then
    capture_dkms_logs
    finish "failed_install_script"
fi
# The .debs are installed; drop them to give the RAM back before the test starts.
rm -rf "$STAGE/pkgs"

for module in nvidia nvidia_modeset nvidia_drm nvidia_uvm; do
    if modprobe "$module" >>"$LOG" 2>&1; then
        log "loaded $module"
    else
        log "could not load $module"
    fi
done

if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >>"$LOG" 2>&1; then
    finish "installed_and_working"
fi
capture_dkms_logs
finish "installed_but_not_working"
