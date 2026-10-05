#!/bin/bash
set -uo pipefail

LOG=/run/cpu-dit-driver-probe.txt
mkdir -p /run
: >"$LOG"

probe_module() {
    local module="$1" probe_status
    printf 'probing %s\n' "$module" >>"$LOG"
    if timeout --foreground --signal=TERM --kill-after=1s 5s \
        modprobe "$module" >>"$LOG" 2>&1; then
        printf 'ready %s\n' "$module" >>"$LOG"
        return
    else
        probe_status=$?
    fi
    case "$probe_status" in
        124|137) printf 'timeout %s exit=%s\n' "$module" "$probe_status" >>"$LOG" ;;
        *) printf 'unavailable_or_not_applicable %s exit=%s\n' \
            "$module" "$probe_status" >>"$LOG" ;;
    esac
}

probe_optional_module() {
    # Module names use `_`; their files may use `-` (nvidia_modeset is nvidia-modeset.ko).
    local module="$1" file="${1//_/-}"
    if find "/lib/modules/$(uname -r)" -type f \
        \( -name "${module}.ko" -o -name "${module}.ko.xz" -o -name "${module}.ko.zst" \
        -o -name "${file}.ko" -o -name "${file}.ko.xz" -o -name "${file}.ko.zst" \) \
        -print -quit 2>/dev/null | grep -q .; then
        probe_module "$module"
    else
        printf 'not_installed %s\n' "$module" >>"$LOG"
    fi
}

# Every module below performs its own PCI/ACPI/DMI/CPU identification. No force
# or unsafe probe parameter is used; unsupported modules simply decline to bind.
for module in \
    drm drm_kms_helper simpledrm i915 xe amdgpu amdkfd radeon \
    msr coretemp k8temp k10temp fam15h_power amd_energy \
    intel_rapl_msr intel_rapl_tpmi \
    i2c-i801 i2c-piix4 \
    nct6775 nct6683 it87 f71882fg w83627ehf \
    smsc47m1 smsc47m192 sch5627 sch5636 \
    asus_atk0110 asus_wmi_sensors asus_ec_sensors \
    hp_wmi_sensors dell_smm_hwmon lenovo_ec_sensors \
    acpi_power_meter \
    amd64_edac_mod i10nm_edac igen6_edac ie31200_edac sb_edac skx_edac pnd2_edac \
    ipmi_si ipmi_ssif ipmi_devintf acpi_ipmi; do
    probe_module "$module"
done

# nouveau is disabled on every flavor (kernel `module_blacklist=nouveau`, plus
# modprobe.d `install nouveau /bin/false` baked into the rootfs and initramfs),
# so NVIDIA cards are driven only by the NVIDIA stack: baked in (nvidia-image,
# recorded in /etc/cpu-dit/nvidia-driver-profile) or installed just before this
# script by nvidia-driver-install.sh from a package staged on DIT-RESULTS.
# Open or proprietary NVIDIA modules: whichever drives this machine's GPUs (GTX 9xx/10xx need the
# proprietary ones), chosen before the rest of the NVIDIA stack loads.
. /usr/local/lib/cpu-dit/nvidia-flavor.sh
if [[ -f /etc/cpu-dit/nvidia-driver-profile ]]; then
    printf 'gpu_driver_profile nvidia_baked package=%s\n' \
        "$(cat /etc/cpu-dit/nvidia-driver-profile)" >>"$LOG"
    select_nvidia_flavor "$LOG"
    for module in nvidia nvidia_modeset nvidia_drm nvidia_uvm; do
        probe_optional_module "$module"
    done
elif [[ -f /run/cpu-dit-nvidia-driver-runtime-installed ]]; then
    printf 'gpu_driver_profile nvidia_runtime result=%s\n' \
        "$(cat /run/cpu-dit-nvidia-driver-runtime-installed)" >>"$LOG"
    select_nvidia_flavor "$LOG"
    for module in nvidia nvidia_modeset nvidia_drm nvidia_uvm; do
        probe_optional_module "$module"
    done
else
    printf 'gpu_driver_profile open nouveau=disabled\n' >>"$LOG"
fi
if lsmod 2>/dev/null | grep -q '^nouveau '; then
    printf 'nouveau_loaded UNEXPECTED (the image blacklists it; check the kernel command line)\n' \
        >>"$LOG"
else
    printf 'nouveau_loaded no\n' >>"$LOG"
fi

udevadm settle --timeout=5 2>>"$LOG" || true
printf 'gpu_inventory_begin\n' >>"$LOG"
lspci -nnk 2>>"$LOG" \
    | sed -n '/VGA compatible controller\|3D controller\|Display controller/,+3p' \
    >>"$LOG" || true
printf 'gpu_inventory_end\n' >>"$LOG"
printf 'completed_utc %s\n' "$(date -u --iso-8601=seconds)" >>"$LOG"
# Best-effort, same reasoning as nvidia-driver-install.sh: the interactive menu profile never
# reaches boot-runner.sh's own end-of-run copy, so persist here if the sibling partition happens
# to already be mounted (nvidia-driver-install.sh, run just before this script, mounts it when it
# looks for a staged package).
if mountpoint -q /mnt/cpu-dit-results 2>/dev/null; then
    mkdir -p /mnt/cpu-dit-results/boot 2>/dev/null \
        && cp -f "$LOG" /mnt/cpu-dit-results/boot/driver-probe.txt 2>/dev/null
fi
