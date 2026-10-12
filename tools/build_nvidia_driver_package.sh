#!/bin/bash
# Builds an NVIDIA driver package for offline install at boot time, as an
# alternative to `make nvidia-image` (a whole separate baked rootfs/image).
# One generic image (`make image`) + this package burned onto the same USB's
# DIT-RESULTS partition (`make nvidia-driver-burn-usb`); nvidia-driver-install.sh
# on the image installs it at boot if it finds an NVIDIA GPU. See
# docs/gpu-vram/NVIDIA_DRIVER_PACKAGE.md.
#
# Needs the generic rootfs already built by `make image` (reused so the exact
# kernel/header version matches what the image actually boots), plus network
# and sudo to resolve and download the driver's full dependency closure -- the
# same prerequisites `make nvidia-image` already has, just at package-build
# time instead of whole-image-build time.
#
# Kernel modules: Canonical's signed ones for the image's kernel when the archive has them
# (they load with Secure Boot on), DKMS otherwise (unsigned: Secure Boot must be off).
# NVIDIA_MODULE_SOURCE=signed|dkms forces one; the default, auto, prefers signed.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOTFS="$ROOT_DIR/build/boot-image/rootfs-noble-amd64"
ROOTFS_MARKER="$ROOTFS/.cpu-dit-rootfs-ready"
STAGING_DIR="$ROOT_DIR/dist/nvidia-driver-package-root"
ARCHIVE="$ROOT_DIR/dist/dit-nvidia-driver-package.tar.gz"
NVIDIA_DRIVER_PACKAGE="${NVIDIA_DRIVER_PACKAGE:-}"
NVIDIA_MODULE_SOURCE="${NVIDIA_MODULE_SOURCE:-auto}"
# shellcheck source=tools/nvidia_packages.sh
. "$ROOT_DIR/tools/nvidia_packages.sh"

MOUNTED_PATHS=()

log() { printf '[DIT NVIDIA package] %s\n' "$*"; }
die() { printf '[DIT NVIDIA package] ERROR: %s\n' "$*" >&2; exit 1; }

cleanup() {
    set +e
    for (( index=${#MOUNTED_PATHS[@]}-1; index>=0; index-- )); do
        sudo umount "${MOUNTED_PATHS[$index]}" 2>/dev/null || true
    done
    sudo rm -f "$ROOTFS/usr/sbin/policy-rc.d" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

[[ -n "$NVIDIA_DRIVER_PACKAGE" ]] \
    || die 'Set NVIDIA_DRIVER_PACKAGE (example: nvidia-driver-580-open)'
[[ "$NVIDIA_DRIVER_PACKAGE" =~ ^nvidia-driver-[0-9]+(-open)?$ ]] \
    || die "invalid NVIDIA_DRIVER_PACKAGE: $NVIDIA_DRIVER_PACKAGE"
[[ -f "$ROOTFS_MARKER" ]] \
    || die "generic rootfs not found: $ROOTFS (run 'make image' first -- the driver package targets its exact kernel)"
case "$STAGING_DIR" in
    "$ROOT_DIR"/dist/nvidia-driver-package-root) ;;
    *) die "unsafe staging path: $STAGING_DIR" ;;
esac

KERNEL_VERSION="$(ls "$ROOTFS/lib/modules" 2>/dev/null | sort -V | tail -1)"
[[ -n "$KERNEL_VERSION" ]] || die "no kernel found under $ROOTFS/lib/modules"
log "Target kernel: $KERNEL_VERSION"
log "Target driver: $NVIDIA_DRIVER_PACKAGE"

mount_chroot_filesystems() {
    sudo mount --bind /dev "$ROOTFS/dev"; MOUNTED_PATHS+=("$ROOTFS/dev")
    sudo mount -t devpts devpts "$ROOTFS/dev/pts"; MOUNTED_PATHS+=("$ROOTFS/dev/pts")
    sudo mount -t proc proc "$ROOTFS/proc"; MOUNTED_PATHS+=("$ROOTFS/proc")
    sudo mount -t sysfs sysfs "$ROOTFS/sys"; MOUNTED_PATHS+=("$ROOTFS/sys")
}
unmount_chroot_filesystems() {
    local path
    for path in "$ROOTFS/sys" "$ROOTFS/proc" "$ROOTFS/dev/pts" "$ROOTFS/dev"; do
        sudo umount "$path" 2>/dev/null || sudo umount -l "$path" 2>/dev/null || true
        unset 'MOUNTED_PATHS[-1]'
    done
}

log "Resolving and downloading the full dependency closure (network required)"
printf '#!/bin/sh\nexit 101\n' | sudo tee "$ROOTFS/usr/sbin/policy-rc.d" >/dev/null
sudo chmod 0755 "$ROOTFS/usr/sbin/policy-rc.d"
mount_chroot_filesystems
sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get update
branch="$(nvidia_branch "$NVIDIA_DRIVER_PACKAGE")"
signed_module="$(nvidia_signed_module_package "$NVIDIA_DRIVER_PACKAGE" "$KERNEL_VERSION")"
case "$NVIDIA_MODULE_SOURCE" in
    auto)
        if sudo chroot "$ROOTFS" apt-cache show "$signed_module" >/dev/null 2>&1; then
            module_source=signed
        else
            module_source=dkms
        fi
        ;;
    signed | dkms) module_source="$NVIDIA_MODULE_SOURCE" ;;
    *) die "invalid NVIDIA_MODULE_SOURCE: $NVIDIA_MODULE_SOURCE (auto, signed or dkms)" ;;
esac
# --download-only never unpacks or runs maintainer scripts; it only computes and
# fetches the full Depends closure apt resolves for these names.
if [[ "$module_source" == signed ]]; then
    log "Kernel modules: $signed_module (signed by Canonical, loads with Secure Boot on)"
    mapfile -t wanted < <(nvidia_signed_packages "$NVIDIA_DRIVER_PACKAGE" "$KERNEL_VERSION")
    # The modules pin the driver version they were built from; the userspace must be that one.
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install \
        --download-only -y "${wanted[@]}" \
        || die "the archive's NVIDIA userspace does not match $signed_module (another driver version?); NVIDIA_MODULE_SOURCE=dkms builds instead, with Secure Boot off"
    installed_check="$signed_module"
else
    log "Kernel modules: DKMS (no $signed_module in the archive); unsigned, Secure Boot must be off"
    # build-essential is explicit (not left to Recommends) since DKMS needs a compiler at install
    # time on the target, and this must work with no network there.
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install \
        --download-only -y \
        "$NVIDIA_DRIVER_PACKAGE" dkms "build-essential" "linux-headers-$KERNEL_VERSION"
    installed_check="$NVIDIA_DRIVER_PACKAGE"
fi
mkdir -p "$STAGING_DIR/pkgs"
find "$STAGING_DIR/pkgs" -mindepth 1 -delete
sudo cp "$ROOTFS"/var/cache/apt/archives/*.deb "$STAGING_DIR/pkgs/"
sudo chown "$(id -u):$(id -g)" "$STAGING_DIR/pkgs"/*.deb
package_count=$(find "$STAGING_DIR/pkgs" -name '*.deb' | wc -l)
(( package_count > 0 )) || die "no .deb files were downloaded"
log "Downloaded $package_count package(s)"

# The open modules need the GPU's GSP (Turing and newer). For GTX 9xx/10xx and Titan V the
# driver layer also carries the proprietary modules of the very same version, kept apart in
# pkgs-proprietary/: signed, Canonical's late-linked packages plus what linking them needs (only
# the files not already in pkgs/); with DKMS, the two packages that conflict with the open ones
# (the rest of their dependencies already is in the closure above).
mkdir -p "$STAGING_DIR/pkgs-proprietary"
find "$STAGING_DIR/pkgs-proprietary" -mindepth 1 -delete
if [[ "$NVIDIA_DRIVER_PACKAGE" == *-open && "$module_source" == signed ]]; then
    log "Downloading the signed proprietary modules (Maxwell, Pascal, Volta)"
    sudo chroot "$ROOTFS" apt-get clean
    mapfile -t wanted < <(nvidia_proprietary_module_packages "$branch" "$KERNEL_VERSION")
    sudo chroot "$ROOTFS" /usr/bin/env DEBIAN_FRONTEND=noninteractive apt-get install \
        --download-only -y "${wanted[@]}" \
        || die "the signed proprietary modules of $branch for $KERNEL_VERSION could not be resolved"
    for deb in "$ROOTFS"/var/cache/apt/archives/*.deb; do
        [[ -f "$STAGING_DIR/pkgs/$(basename "$deb")" ]] && continue
        sudo cp "$deb" "$STAGING_DIR/pkgs-proprietary/"
    done
    sudo chown "$(id -u):$(id -g)" "$STAGING_DIR/pkgs-proprietary"/*.deb
    compgen -G "$STAGING_DIR/pkgs-proprietary/linux-modules-nvidia-$branch-${KERNEL_VERSION}_*.deb" >/dev/null \
        || die "linux-modules-nvidia-$branch-$KERNEL_VERSION was not downloaded"
elif [[ "$NVIDIA_DRIVER_PACKAGE" == *-open ]]; then
    open_dkms=("$STAGING_DIR"/pkgs/nvidia-dkms-"$branch"-open_*.deb)
    [[ -f "${open_dkms[0]}" ]] || die "nvidia-dkms-$branch-open is not in the closure"
    driver_version="$(dpkg-deb -f "${open_dkms[0]}" Version)"
    log "Downloading the proprietary modules of $driver_version (Maxwell, Pascal, Volta)"
    sudo mkdir -p "$ROOTFS/var/tmp/dit-proprietary"
    sudo chroot "$ROOTFS" sh -c "cd /var/tmp/dit-proprietary && apt-get download \
        nvidia-dkms-$branch=$driver_version nvidia-kernel-source-$branch=$driver_version"
    sudo cp "$ROOTFS"/var/tmp/dit-proprietary/*.deb "$STAGING_DIR/pkgs-proprietary/"
    sudo rm -rf "$ROOTFS/var/tmp/dit-proprietary"
    sudo chown "$(id -u):$(id -g)" "$STAGING_DIR/pkgs-proprietary"/*.deb
    [[ "$(find "$STAGING_DIR/pkgs-proprietary" -name '*.deb' | wc -l)" -eq 2 ]] \
        || die "the proprietary module packages were not both downloaded"
fi
# Leave nothing but the base rootfs's own apt lists behind (build_boot_image.sh's
# own bootstrap already tolerates those); the .deb archives must not end up
# baked into the generic image this rootfs also builds.
sudo chroot "$ROOTFS" apt-get clean
unmount_chroot_filesystems
sudo rm -f "$ROOTFS/usr/sbin/policy-rc.d"

install -m 0644 /dev/stdin "$STAGING_DIR/blacklist-nouveau.conf" <<'EOF'
# DIT-01: nouveau is disabled on every image flavor.
blacklist nouveau
options nouveau modeset=0
install nouveau /bin/false
EOF

install -m 0755 /dev/stdin "$STAGING_DIR/install.sh" <<'EOF'
#!/bin/sh
# Offline install of the bundled .deb closure. No network, no apt sources: the
# packages named on the dpkg command line already carry every dependency apt
# resolved for them at package-build time -- but a single `dpkg -i pkgs/*.deb`
# is not enough by itself. dpkg processes the glob in the order given (here,
# alphabetical) and only checks Pre-Depends against what is *already
# configured*, so e.g. dkms (Pre-Depends: lsb-release) fails on a pass where
# lsb-release sorts later and has not been configured yet. Each retry of the
# same full glob configures whatever the previous pass unblocked, so looping
# it converges (confirmed by hand: 2 stalled passes, 3rd clean, for the actual
# 257-package nvidia-driver-580-open closure); the loop below gives it margin.
set -eu
export DEBIAN_FRONTEND=noninteractive
cd "$(dirname "$0")"
attempt=1
while [ "$attempt" -le 6 ]; do
    if dpkg -i pkgs/*.deb; then
        break
    fi
    attempt=$((attempt + 1))
done
dpkg --configure -a || true
install -D -m 0644 blacklist-nouveau.conf /etc/modprobe.d/dit-blacklist-nouveau.conf
# The package that proves the install: the signed modules, or the driver itself with DKMS.
driver_status=$(dpkg-query -W -f='${db:Status-Status}' "$(cat installed-check.txt)" 2>/dev/null || true)
[ "$driver_status" = installed ]
EOF

rm -f "$STAGING_DIR/driver-package-name.txt"
printf '%s' "$installed_check" >"$STAGING_DIR/installed-check.txt"

version=$(awk -F '"' '/^version = "/ { print $2; exit }' "$ROOT_DIR/Cargo.toml")
{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "package_id": "dit.nvidia-driver",\n'
    printf '  "version": "%s",\n' "$version"
    printf '  "nvidia_driver_package": "%s",\n' "$NVIDIA_DRIVER_PACKAGE"
    printf '  "target_kernel_version": "%s",\n' "$KERNEL_VERSION"
    printf '  "module_source": "%s",\n' "$module_source"
    printf '  "packages": ['
    first=1
    for deb in "$STAGING_DIR"/pkgs/*.deb; do
        [[ $first -eq 1 ]] || printf ','
        first=0
        printf '"%s"' "$(basename "$deb")"
    done
    printf '],\n'
    printf '  "proprietary_packages": ['
    first=1
    for deb in "$STAGING_DIR"/pkgs-proprietary/*.deb; do
        [[ -f "$deb" ]] || continue
        [[ $first -eq 1 ]] || printf ','
        first=0
        printf '"%s"' "$(basename "$deb")"
    done
    printf '],\n'
    printf '  "built_utc": "%s"\n' "$(date -u --iso-8601=seconds)"
    printf '}\n'
} >"$STAGING_DIR/manifest.json"

(
    cd "$STAGING_DIR"
    find . -type f ! -name MANIFEST.sha256 -print0 | sort -z \
        | xargs -0 sha256sum >MANIFEST.sha256
    sha256sum --check MANIFEST.sha256
)

archive_tmp="$ARCHIVE.tmp"
tar --sort=name --mtime="@${SOURCE_DATE_EPOCH:-0}" --owner=0 --group=0 --numeric-owner \
    -C "$STAGING_DIR" -czf "$archive_tmp" .
mv -f "$archive_tmp" "$ARCHIVE"
sha256sum "$ARCHIVE" | awk '{print $1}' >"$ARCHIVE.sha256.tmp"
printf '%s  %s\n' "$(cat "$ARCHIVE.sha256.tmp")" "$(basename "$ARCHIVE")" >"$ARCHIVE.sha256"
rm -f "$ARCHIVE.sha256.tmp"

log "Ready: $ARCHIVE"
log "SHA-256: $(awk '{print $1}' "$ARCHIVE.sha256")"
log "Burn onto a USB already holding the generic image with: make nvidia-driver-burn-usb"
