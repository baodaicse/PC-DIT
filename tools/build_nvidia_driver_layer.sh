#!/bin/bash
# Builds a prebuilt NVIDIA driver layer for the generic image: the driver is installed and its
# DKMS modules compiled once, here, instead of on every boot. The result is a squashfs holding
# only what the install changed; burned into /live/ on the DIT-BOOT partition next to
# filesystem.squashfs, live-boot stacks it on top of the root filesystem (images are layered in
# name order, later on top), so the nvidia modules, their modules.alias entries and the userspace
# are already in place when udev coldplugs the GPU. See docs/gpu-vram/NVIDIA_DRIVER_PACKAGE.md.
#
# The layer is tied to one exact image build: it is compiled against that image's kernel and
# replaces whole files of its rootfs (dpkg status, ld.so.cache, modules.dep...). Its manifest
# records the image_id of the rootfs it was built on; burn_usb.sh refuses any other image.
#
# Needs the generic rootfs from `make image` and the offline package from
# `make nvidia-driver-package` (built here if missing or for another driver/kernel).
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOTFS="$ROOT_DIR/build/boot-image/rootfs-noble-amd64"
ROOTFS_MARKER="$ROOTFS/.cpu-dit-rootfs-ready"
PACKAGE_STAGE="$ROOT_DIR/dist/nvidia-driver-package-root"
WORK="$ROOT_DIR/build/nvidia-driver-layer"
LAYER="$ROOT_DIR/dist/dit-nvidia-driver.squashfs"
NVIDIA_DRIVER_PACKAGE="${NVIDIA_DRIVER_PACKAGE:-}"

MOUNTED_PATHS=()

log() { printf '[DIT NVIDIA layer] %s\n' "$*"; }
die() { printf '[DIT NVIDIA layer] ERROR: %s\n' "$*" >&2; exit 1; }

cleanup() {
    set +e
    for (( index=${#MOUNTED_PATHS[@]}-1; index>=0; index-- )); do
        sudo umount "${MOUNTED_PATHS[$index]}" 2>/dev/null \
            || sudo umount -l "${MOUNTED_PATHS[$index]}" 2>/dev/null
    done
}
trap cleanup EXIT INT TERM

mount_tracked() {
    sudo mount "$@"
    MOUNTED_PATHS+=("${!#}")
}

[[ -n "$NVIDIA_DRIVER_PACKAGE" ]] \
    || die 'Set NVIDIA_DRIVER_PACKAGE (example: nvidia-driver-580-open)'
[[ "$NVIDIA_DRIVER_PACKAGE" =~ ^nvidia-driver-[0-9]+(-open)?$ ]] \
    || die "invalid NVIDIA_DRIVER_PACKAGE: $NVIDIA_DRIVER_PACKAGE"
[[ -f "$ROOTFS_MARKER" ]] \
    || die "generic rootfs not found: $ROOTFS (run 'make image' first)"
[[ -s "$ROOTFS/etc/cpu-dit/image-id" ]] \
    || die "the generic rootfs has no image-id (run 'make image' first)"
case "$WORK" in
    "$ROOT_DIR"/build/nvidia-driver-layer) ;;
    *) die "unsafe work path: $WORK" ;;
esac

IMAGE_ID="$(<"$ROOTFS/etc/cpu-dit/image-id")"
KERNEL_VERSION="$(ls "$ROOTFS/lib/modules" 2>/dev/null | sort -V | tail -1)"
[[ -n "$KERNEL_VERSION" ]] || die "no kernel found under $ROOTFS/lib/modules"
log "Base image: $IMAGE_ID"
log "Target kernel: $KERNEL_VERSION"
log "Target driver: $NVIDIA_DRIVER_PACKAGE"

# Reuse the offline package's resolved .deb closure when it was built for this driver and kernel.
# An open driver's package must also carry the proprietary modules (older packages do not).
if ! grep -q "\"nvidia_driver_package\": \"$NVIDIA_DRIVER_PACKAGE\"" "$PACKAGE_STAGE/manifest.json" 2>/dev/null \
    || ! grep -q "\"target_kernel_version\": \"$KERNEL_VERSION\"" "$PACKAGE_STAGE/manifest.json" 2>/dev/null \
    || { [[ "$NVIDIA_DRIVER_PACKAGE" == *-open ]] \
        && ! compgen -G "$PACKAGE_STAGE/pkgs-proprietary/*.deb" >/dev/null; }; then
    log "No matching offline package; building it first"
    NVIDIA_DRIVER_PACKAGE="$NVIDIA_DRIVER_PACKAGE" "$ROOT_DIR/tools/build_nvidia_driver_package.sh"
fi
(cd "$PACKAGE_STAGE" && sha256sum --quiet --check MANIFEST.sha256) \
    || die "offline package staging failed its own MANIFEST.sha256"

log "Installing into an overlay on the generic rootfs (the rootfs itself is never modified)"
# An interrupted run can leave the chroot mounted, host /dev bind mount included: rm -rf through
# it would delete the host's device nodes. Unmount leftovers (deepest first) and refuse otherwise.
while read -r leftover; do
    sudo umount "$leftover" 2>/dev/null || sudo umount -l "$leftover" 2>/dev/null || true
done < <(findmnt -rn -o TARGET | grep -F "$WORK/" | sort -r)
! findmnt -rn -o TARGET | grep -qF "$WORK/" \
    || die "mounts are still active under $WORK; unmount them before rebuilding"
sudo rm -rf "$WORK"
mkdir -p "$WORK/upper" "$WORK/work" "$WORK/merged"
mount_tracked -t overlay overlay \
    -o "lowerdir=$ROOTFS,upperdir=$WORK/upper,workdir=$WORK/work" "$WORK/merged"
mount_tracked --bind /dev "$WORK/merged/dev"
mount_tracked -t devpts devpts "$WORK/merged/dev/pts"
mount_tracked -t proc proc "$WORK/merged/proc"
mount_tracked -t sysfs sysfs "$WORK/merged/sys"
printf '#!/bin/sh\nexit 101\n' | sudo tee "$WORK/merged/usr/sbin/policy-rc.d" >/dev/null
sudo chmod 0755 "$WORK/merged/usr/sbin/policy-rc.d"
sudo mkdir -p "$WORK/merged/var/tmp/dit-layer-build"
sudo cp -a "$PACKAGE_STAGE/." "$WORK/merged/var/tmp/dit-layer-build/"
sudo chroot "$WORK/merged" /usr/bin/env DEBIAN_FRONTEND=noninteractive \
    sh /var/tmp/dit-layer-build/install.sh </dev/null \
    || die "offline install failed inside the overlay"

# Both module flavors of the same driver version (image/rootfs/usr/local/lib/cpu-dit/
# nvidia-flavor.sh chooses at boot): the open ones just built stay in updates/dkms and are kept
# under /usr/lib/dit/nvidia-modules/open; the proprietary ones (GTX 9xx/10xx, Titan V) are built
# by swapping the conflicting DKMS packages, then kept under .../proprietary.
flavors_root="/usr/lib/dit/nvidia-modules"
merged_dkms="$WORK/merged/usr/lib/modules/$KERNEL_VERSION/updates/dkms"
if compgen -G "$PACKAGE_STAGE/pkgs-proprietary/*.deb" >/dev/null; then
    branch="${NVIDIA_DRIVER_PACKAGE#nvidia-driver-}"
    branch="${branch%-open}"
    open_keep="$WORK/merged$flavors_root/open/$KERNEL_VERSION"
    proprietary_keep="$WORK/merged$flavors_root/proprietary/$KERNEL_VERSION"
    sudo mkdir -p "$open_keep" "$proprietary_keep"
    sudo cp -a "$merged_dkms"/nvidia*.ko* "$open_keep/"
    log "Building the proprietary modules (GTX 9xx/10xx, Titan V) of the same driver"
    sudo chroot "$WORK/merged" /usr/bin/env DEBIAN_FRONTEND=noninteractive \
        dpkg --purge --force-depends "nvidia-dkms-$branch-open" "nvidia-kernel-source-$branch-open" \
        </dev/null || die "could not set the open module packages aside"
    sudo chroot "$WORK/merged" /usr/bin/env DEBIAN_FRONTEND=noninteractive \
        sh -c 'dpkg -i /var/tmp/dit-layer-build/pkgs-proprietary/*.deb' </dev/null \
        || die "the proprietary modules did not install or build"
    compgen -G "$merged_dkms/nvidia.ko*" >/dev/null \
        || die "DKMS did not produce the proprietary nvidia.ko for $KERNEL_VERSION"
    sudo cp -a "$merged_dkms"/nvidia*.ko* "$proprietary_keep/"
    # The open modules stay the default in updates/dkms.
    sudo rm -f "$merged_dkms"/nvidia*.ko*
    sudo cp -a "$open_keep"/nvidia*.ko* "$merged_dkms/"
    for flavor in open proprietary; do
        license="$(sudo chroot "$WORK/merged" modinfo -F license \
            "$flavors_root/$flavor/$KERNEL_VERSION/nvidia.ko" 2>/dev/null | head -1)"
        log "  $flavor nvidia.ko: license ${license:-unknown}"
        case "$flavor:$license" in
            open:*GPL*) ;;
            proprietary:NVIDIA) ;;
            *) die "$flavor nvidia.ko has an unexpected license: ${license:-none}" ;;
        esac
    done
    module_flavors='"open", "proprietary"'
elif [[ "$NVIDIA_DRIVER_PACKAGE" == *-open ]]; then
    module_flavors='"open"'
else
    module_flavors='"proprietary"'
fi
sudo chroot "$WORK/merged" depmod -a "$KERNEL_VERSION"

# Noble is usrmerged: /lib is a symlink to usr/lib, so the install lands under usr/lib/modules.
module_dir="$WORK/upper/usr/lib/modules/$KERNEL_VERSION"
[[ -L "$ROOTFS/lib" ]] || module_dir="$WORK/upper/lib/modules/$KERNEL_VERSION"
shopt -s nullglob
modules=("$module_dir"/updates/dkms/nvidia.ko*)
shopt -u nullglob
(( ${#modules[@]} > 0 )) || die "DKMS did not produce nvidia.ko for $KERNEL_VERSION"
grep -q '^alias pci:v000010DE.* nvidia$' "$module_dir/modules.alias" \
    || die "modules.alias has no NVIDIA PCI alias; udev would not load nvidia at boot"
log "Built $(basename "${modules[0]}") for $KERNEL_VERSION; modules.alias carries its PCI aliases"

# Record the driver as part of the root filesystem, exactly like the baked nvidia-image flavor:
# driver-probe.sh probes the nvidia stack, nvidia-driver-install.sh skips at once.
printf '%s\n' "$NVIDIA_DRIVER_PACKAGE" \
    | sudo tee "$WORK/merged/etc/cpu-dit/nvidia-driver-profile" >/dev/null
printf 'layer image_id=%s kernel=%s built_utc=%s\n' \
    "$IMAGE_ID" "$KERNEL_VERSION" "$(date -u --iso-8601=seconds)" \
    | sudo tee "$WORK/merged/etc/cpu-dit/nvidia-driver-layer" >/dev/null
sudo rm -rf "$WORK/merged/var/tmp/dit-layer-build"
sudo rm -f "$WORK/merged/usr/sbin/policy-rc.d"
cleanup
MOUNTED_PATHS=()

# Build-only content stays out of the layer: the modules are already compiled and installed
# under updates/dkms (DKMS's own tree only holds a second copy of them plus build state), and the
# live kernel/initrd come from DIT-BOOT, not from the root filesystem.
log "Compressing the changes into $(basename "$LAYER")"
sudo mksquashfs "$WORK/upper" "$LAYER.tmp" -noappend -all-root -comp zstd \
    -Xcompression-level 15 -wildcards \
    -e 'boot/*' 'dev/*' 'proc/*' 'sys/*' 'run/*' 'tmp/*' 'var/tmp/*' 'var/cache/*' \
    'usr/src/*' 'var/lib/dkms/*' 'usr/share/doc/*' 'usr/share/man/*' >/dev/null
sudo chown "$(id -u):$(id -g)" "$LAYER.tmp"
mv -f "$LAYER.tmp" "$LAYER"
sha256sum "$LAYER" | awk -v name="$(basename "$LAYER")" '{print $1 "  " name}' >"$LAYER.sha256"
layer_bytes="$(stat -c %s "$LAYER")"
cat >"$LAYER.manifest.json" <<EOF
{
  "schema_version": 1,
  "layer_id": "dit.nvidia-driver-layer",
  "image_id": "$IMAGE_ID",
  "target_kernel_version": "$KERNEL_VERSION",
  "nvidia_driver_package": "$NVIDIA_DRIVER_PACKAGE",
  "kernel_module_flavors": [$module_flavors],
  "live_name": "nvidia-driver.squashfs",
  "size_bytes": $layer_bytes,
  "sha256": "$(awk '{print $1}' "$LAYER.sha256")",
  "built_utc": "$(date -u --iso-8601=seconds)"
}
EOF
sudo rm -rf "$WORK"

log "Ready: $LAYER ($(( layer_bytes / 1024 / 1024 )) MiB)"
log "Pairs only with image $IMAGE_ID; burn with: make nvidia-driver-layer-burn-usb"
