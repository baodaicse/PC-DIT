#!/bin/bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GRUB_CONFIG="$ROOT_DIR/image/boot/grub/grub.cfg"
SERVICE="$ROOT_DIR/image/rootfs/etc/systemd/system/cpu-dit.service"
RUNNER="$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh"
DRIVER_PROBE="$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/driver-probe.sh"
UI="$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh"
PARTITION_LOOKUP="$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/partition-lookup.sh"
NVIDIA_INSTALL="$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/nvidia-driver-install.sh"
NVIDIA_PACKAGE_BUILDER="$ROOT_DIR/tools/build_nvidia_driver_package.sh"
BURN_USB="$ROOT_DIR/tools/burn_usb.sh"
NVIDIA_LAYER_BUILDER="$ROOT_DIR/tools/build_nvidia_driver_layer.sh"
ISO_BUILDER="$ROOT_DIR/tools/build_iso.sh"

bash -n "$RUNNER" "$DRIVER_PROBE" "$UI" "$PARTITION_LOOKUP" "$NVIDIA_INSTALL" \
    "$NVIDIA_PACKAGE_BUILDER" "$BURN_USB" "$NVIDIA_LAYER_BUILDER" "$ISO_BUILDER"

if grep -Eq '^[[:space:]]*linux .*console=ttyS0' "$GRUB_CONFIG"; then
    printf 'BOOT_POLICY_FAIL: physical menu forces a serial kernel console\n' >&2
    exit 2
fi
# nouveau is disabled outright (it bound an RTX 5060 before a staged NVIDIA
# package could be installed): every boot entry carries module_blacklist=nouveau,
# which the kernel enforces for every load, explicit modprobe included.
linux_lines="$(grep -c '^[[:space:]]*linux /live/vmlinuz' "$GRUB_CONFIG")"
blacklist_lines="$(grep -c '^[[:space:]]*linux /live/vmlinuz.*module_blacklist=nouveau' "$GRUB_CONFIG")"
if [[ "$linux_lines" -eq 0 || "$blacklist_lines" -ne "$linux_lines" ]]; then
    printf 'BOOT_POLICY_FAIL: every boot entry must carry module_blacklist=nouveau (%s/%s do)\n' \
        "$blacklist_lines" "$linux_lines" >&2
    exit 2
fi
if grep -Fq 'systemd-udev-settle.service' "$SERVICE"; then
    printf 'BOOT_POLICY_FAIL: cpu-dit.service waits for global udev settle\n' >&2
    exit 2
fi
grep -Fq 'Type=simple' "$SERVICE"
grep -Fq 'udevadm settle --timeout=5' "$DRIVER_PROBE"
grep -Fq 'timeout --foreground --signal=TERM --kill-after=1s 5s' "$DRIVER_PROBE"
grep -Fq 'systemd-detect-virt --quiet' "$RUNNER"
grep -Fq 'CPU_DIT_SERIAL_CONSOLE' "$RUNNER"
grep -Fq 'drm drm_kms_helper simpledrm i915 xe amdgpu amdkfd radeon' "$DRIVER_PROBE"
grep -Fq 'if [[ -f /etc/cpu-dit/nvidia-driver-profile ]]' "$DRIVER_PROBE"
if grep -Fq 'probe_module nouveau' "$DRIVER_PROBE"; then
    printf 'BOOT_POLICY_FAIL: driver-probe.sh must not load nouveau\n' >&2
    exit 2
fi
grep -Fq 'gpu_driver_profile open nouveau=disabled' "$DRIVER_PROBE"
grep -Fq 'nouveau_loaded' "$DRIVER_PROBE"
grep -Fq 'install nouveau /bin/false' "$ROOT_DIR/tools/build_boot_image.sh"
grep -Fq 'install nouveau /bin/false' "$ROOT_DIR/tools/build_nvidia_driver_package.sh"
disable_line="$(grep -n '^    disable_nouveau$' "$ROOT_DIR/tools/build_boot_image.sh" | cut -d: -f1)"
[[ -n "$disable_line" ]] \
    || { printf 'BOOT_POLICY_FAIL: build_boot_image.sh main() must call disable_nouveau\n' >&2; exit 2; }
grep -Fq 'NVIDIA_DRIVER_PACKAGE' "$ROOT_DIR/tools/build_boot_image.sh"
for gpu_package in libdrm2 mesa-utils mesa-vulkan-drivers vulkan-tools; do
    grep -Fxq "    $gpu_package" "$ROOT_DIR/tools/build_boot_image.sh"
done
grep -Fq 'MENU_ARGS+=(--protected-device "$BOOT_PARTITION")' "$RUNNER"
grep -Fq 'if mount_result_partition; then' "$RUNNER"
grep -Fq 'MENU_ARGS+=(--results-root /mnt/cpu-dit-results)' "$RUNNER"
grep -Fq 'MENU_ARGS+=(--results-root /run/dit-suite-no-persistent-results)' "$RUNNER"

# --- NVIDIA driver package: install-at-boot alternative to the baked nvidia-image flavor ---
grep -Fq '. /usr/local/lib/cpu-dit/partition-lookup.sh' "$RUNNER"
grep -Fq 'find_boot_device()' "$PARTITION_LOOKUP"
grep -Fq 'find_result_partition()' "$PARTITION_LOOKUP"
grep -Fq 'mount_result_partition()' "$PARTITION_LOOKUP"

# nvidia-driver-install.sh must run before driver-probe.sh, so a freshly loaded
# module is visible to the same boot's probe.
install_line="$(grep -n '/usr/local/lib/cpu-dit/nvidia-driver-install.sh' "$RUNNER" | head -1 | cut -d: -f1)"
probe_line="$(grep -n '/usr/local/lib/cpu-dit/driver-probe.sh' "$RUNNER" | head -1 | cut -d: -f1)"
[[ -n "$install_line" && -n "$probe_line" ]] \
    || { printf 'BOOT_POLICY_FAIL: nvidia-driver-install.sh or driver-probe.sh not called from boot-runner.sh\n' >&2; exit 2; }
if (( install_line >= probe_line )); then
    printf 'BOOT_POLICY_FAIL: nvidia-driver-install.sh must run before driver-probe.sh\n' >&2
    exit 2
fi
grep -Fq 'cp /run/cpu-dit-nvidia-driver-install.txt' "$RUNNER"

grep -Fq '. /usr/local/lib/cpu-dit/partition-lookup.sh' "$NVIDIA_INSTALL"
grep -Fq 'if [[ -f /etc/cpu-dit/nvidia-driver-profile ]]; then' "$NVIDIA_INSTALL"
grep -Fq "lspci -nn -d 10de:" "$NVIDIA_INSTALL"
grep -Fq 'nvidia-smi >>"$LOG" 2>&1' "$NVIDIA_INSTALL"
grep -Fq 'sha256sum -c "$(basename "$PACKAGE").sha256"' "$NVIDIA_INSTALL"
grep -Fq 'sha256sum -c MANIFEST.sha256' "$NVIDIA_INSTALL"
grep -Fq 'failed_nouveau_loaded_despite_blacklist' "$NVIDIA_INSTALL"
grep -Fq 'module_blacklist=nouveau' "$NVIDIA_INSTALL"
if grep -Fq 'modprobe -r nouveau' "$NVIDIA_INSTALL"; then
    printf 'BOOT_POLICY_FAIL: nouveau cannot be loaded, so there is nothing to modprobe -r\n' >&2
    exit 2
fi
grep -Fq '/run/cpu-dit-nvidia-driver-runtime-installed' "$NVIDIA_INSTALL"
# The interactive menu profile execs into dit-suite before boot-runner.sh's own end-of-run copy
# runs, so both diagnostic logs must persist themselves onto DIT-RESULTS if it happens to be
# mounted already, or an operator using the normal menu never sees them again after reboot.
grep -Fq '/mnt/cpu-dit-results/boot/nvidia-driver-install.txt' "$NVIDIA_INSTALL"
# The initramfs mounts /run noexec at 10% of RAM: the first real boot could not run install.sh
# from there. Stage on the root overlay and run it through sh, never relying on an exec bit.
if grep -Eq '^STAGE=/run' "$NVIDIA_INSTALL"; then
    printf 'BOOT_POLICY_FAIL: the NVIDIA package must not be staged under noexec /run\n' >&2
    exit 2
fi
grep -Fq 'STAGE=/var/tmp/dit-nvidia-driver' "$NVIDIA_INSTALL"
grep -Fq 'DEBIAN_FRONTEND=noninteractive sh "$STAGE/install.sh"' "$NVIDIA_INSTALL"
grep -Fq 'failed_no_space_for_install' "$NVIDIA_INSTALL"
grep -Fq 'capture_dkms_logs' "$NVIDIA_INSTALL"
grep -Fq '/mnt/cpu-dit-results/boot/driver-probe.txt' "$DRIVER_PROBE"

grep -Fq 'gpu_driver_profile nvidia_baked' "$DRIVER_PROBE"
grep -Fq '/run/cpu-dit-nvidia-driver-runtime-installed' "$DRIVER_PROBE"
grep -Fq 'gpu_driver_profile nvidia_runtime' "$DRIVER_PROBE"

grep -Fq 'ROOTFS="$ROOT_DIR/build/boot-image/rootfs-noble-amd64"' "$NVIDIA_PACKAGE_BUILDER"
grep -Fq 'NVIDIA_DRIVER_PACKAGE="${NVIDIA_DRIVER_PACKAGE:-}"' "$NVIDIA_PACKAGE_BUILDER"
grep -Fq 'nvidia-driver-[0-9]+(-open)?' "$NVIDIA_PACKAGE_BUILDER"
grep -Fq -- '--download-only' "$NVIDIA_PACKAGE_BUILDER"
grep -Fq 'build-essential' "$NVIDIA_PACKAGE_BUILDER"
grep -Fq 'dkms' "$NVIDIA_PACKAGE_BUILDER"
grep -Fq 'apt-get clean' "$NVIDIA_PACKAGE_BUILDER"

grep -Fq -- '--nvidia-driver-package' "$BURN_USB"
grep -Fq 'DIT-RESULTS' "$BURN_USB"
grep -Fq 'sha256sum --check --strict' "$BURN_USB"

grep -Fq 'nvidia-driver-install.sh' "$ROOT_DIR/tools/build_boot_image.sh"
grep -Fq 'partition-lookup.sh' "$ROOT_DIR/tools/build_boot_image.sh"

# Prebuilt driver layer: built in an overlay (never touching the image's rootfs), tied to one
# image_id, and marked as the baked flavor so nothing is installed at boot.
grep -Fq 'lowerdir=$ROOTFS,upperdir=$WORK/upper' "$NVIDIA_LAYER_BUILDER"
grep -Fq '"image_id": "$IMAGE_ID"' "$NVIDIA_LAYER_BUILDER"
grep -Fq 'etc/cpu-dit/nvidia-driver-profile' "$NVIDIA_LAYER_BUILDER"
grep -Fq "modules.alias has no NVIDIA PCI alias" "$NVIDIA_LAYER_BUILDER"
# Hybrid ISO: same payload as the verified .img, layer checked against its image_id, results
# partition stamped with it, volume id matched by partition-lookup.sh.
grep -Fq 'sha256sum --quiet --check "$(basename "$IMAGE").sha256"' "$ISO_BUILDER"
grep -Fq 'layer_image_id" == "$image_id"' "$ISO_BUILDER"
grep -Fq 'live/nvidia-driver.squashfs' "$ISO_BUILDER"
grep -Fq -- '-volid DIT-BOOT' "$ISO_BUILDER"
grep -Fq -- '-append_partition 3 0x0c' "$ISO_BUILDER"
grep -Fq 'mkfs.fat -F 32 -n DIT-RESULTS' "$ISO_BUILDER"
grep -Fq '::/CPU-DIT-RESULTS.ID' "$ISO_BUILDER"
grep -Fq 'disk:iso9660' "$PARTITION_LOOKUP"
grep -Fq 'part:vfat' "$PARTITION_LOOKUP"
# The ISO's Apple HFS+ partition repeats the DIT-BOOT label; only FAT/ISO9660 may match.
if grep -Fq 'hfsplus)' "$PARTITION_LOOKUP"; then
    printf 'BOOT_POLICY_FAIL: partition lookup must not accept the ISO HFS+ partition\n' >&2
    exit 2
fi

# The boot menu names its build: a first information line filled at image build time, with the
# default chosen by id so that line never becomes what boots unattended.
grep -Fq 'menuentry "Build @DIT_BUILD_TIME@ | @DIT_IMAGE_ID@" --id build-info' "$GRUB_CONFIG"
grep -Fq 'set default=control-center' "$GRUB_CONFIG"
grep -Fq 'menuentry "DIT-01 Control center (CPU-RAM / CPU-NVMe)" --id control-center' "$GRUB_CONFIG"
# Fault Attribution: its own entry (same control center, dit_attribution=1), never the default;
# the plain control center stays without it; the CPU-RAM runner passes --confirm-on-fail with it.
grep -Fq 'menuentry "DIT-01 Fault Attribution (control center + component checks)" --id fault-attribution' "$GRUB_CONFIG"
[[ "$(grep -c 'dit_attribution=1' "$GRUB_CONFIG")" -eq 1 ]]
grep -A1 -- '--id fault-attribution' "$GRUB_CONFIG" | grep -Fq 'cpu_dit_profile=menu dit_attribution=1'
! grep -A1 -- '--id control-center' "$GRUB_CONFIG" | grep -Fq 'dit_attribution'
grep -Fq 'SYSTEM_ARGS+=(--confirm-on-fail)' "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh"
grep -Fq 'MENU_ARGS+=(--attribution-checks)' "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh"
# The control center on tty1 knows it is the Linux console (box and block characters).
grep -Fq 'export TERM="${TERM:-linux}"' "$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh"
if grep -Eq '^set default=[0-9]' "$GRUB_CONFIG"; then
    printf 'BOOT_POLICY_FAIL: the default boot entry must be chosen by id, not position\n' >&2
    exit 2
fi
if [[ "$(grep -Fc 'install -m 0644 "$ROOT_DIR/image/boot/grub/grub.cfg"' "$ROOT_DIR/tools/build_boot_image.sh")" -ne 0 ]]; then
    printf 'BOOT_POLICY_FAIL: grub.cfg must be installed through render_grub_cfg (placeholders)\n' >&2
    exit 2
fi
[[ "$(grep -c 'render_grub_cfg "$image_id"' "$ROOT_DIR/tools/build_boot_image.sh")" -eq 2 ]]
grep -Fq "unfilled placeholder left in grub.cfg" "$ROOT_DIR/tools/build_boot_image.sh"
grep -Fq -- '--id iso-info' "$ISO_BUILDER"
grub-script-check "$GRUB_CONFIG" 2>/dev/null || ! command -v grub-script-check >/dev/null

# CPU compute oracle: Full runs it on a quarter of the logical CPUs next to the RAM rounds, and
# the CPU COMPUTE profile runs it on all but one, with idle/burst ramps.
grep -Fq -- '--compute-oracle-workers "$ORACLE_CPUS"' "$RUNNER"
grep -Fq -- '--compute-oracle-workers "$((LOGICAL_CPUS - 1))"' "$RUNNER"
grep -Fq -- '--compute-oracle-idle-ramp' "$RUNNER"
grep -Fq "run_profile compute 'CPU COMPUTE'" "$UI"
grep -Fq '[5] CPU COMPUTE' "$UI"

grep -Fq 'nvidia-driver-layer:' "$ROOT_DIR/Makefile"
grep -Fq 'iso-only:' "$ROOT_DIR/Makefile"
grep -Fq 'nvidia-driver-package:' "$ROOT_DIR/Makefile"
grep -Fq 'nvidia-driver-burn-usb:' "$ROOT_DIR/Makefile"

printf 'CPU-DIT physical boot policy tests: PASS\n'
