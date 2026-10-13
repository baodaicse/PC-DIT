#!/bin/bash
# Builds a hybrid ISO of the generic image with the prebuilt NVIDIA driver layer already in
# /live, so the nvidia modules load at boot like on the baked nvidia-image flavor -- nothing is
# installed at boot. See docs/gpu-vram/NVIDIA_DRIVER_PACKAGE.md.
#
# The boot payload (kernel, initrd, root squashfs, image-id, grub.cfg) is copied out of the
# verified dist/ .img rather than rebuilt, so the ISO and the .img are the same build and the
# driver layer (compiled against that build's kernel and dpkg state) is checked to match it.
#
# Written to a USB stick (Rufus in DD mode, dd, Etcher), the ISO also carries an appended FAT
# partition labelled DIT-RESULTS, stamped with the image-id, where reports and boot logs persist
# exactly as on the .img. Booted as a CD (VM, BMC virtual media) it runs, but keeps nothing.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${IMAGE:-$ROOT_DIR/dist/dit-01-noble-amd64.img}"
LAYER="${LAYER:-$ROOT_DIR/dist/dit-nvidia-driver.squashfs}"
ISO="${ISO:-$ROOT_DIR/dist/dit-01-noble-amd64-nvidia.iso}"
ISO_RESULT_MIB="${ISO_RESULT_MIB:-1024}"
WORK="$ROOT_DIR/build/iso"

log() { printf '[DIT ISO] %s\n' "$*"; }
die() { printf '[DIT ISO] ERROR: %s\n' "$*" >&2; exit 1; }

case "$WORK" in
    "$ROOT_DIR"/build/iso) ;;
    *) die "unsafe work path: $WORK" ;;
esac
[[ "$ISO_RESULT_MIB" =~ ^[0-9]+$ ]] && (( ISO_RESULT_MIB >= 64 )) \
    || die "ISO_RESULT_MIB must be at least 64 (FAT32): $ISO_RESULT_MIB"
for tool in grub-mkrescue xorriso mcopy mmd mkfs.fat sfdisk; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required (apt: grub-common xorriso mtools dosfstools fdisk)"
done
for platform in i386-pc x86_64-efi; do
    [[ -d /usr/lib/grub/$platform ]] \
        || die "/usr/lib/grub/$platform missing (apt: grub-pc-bin grub-efi-amd64-bin)"
done

[[ -s "$IMAGE" && -s "$IMAGE.sha256" ]] || die "image not found: $IMAGE (run 'make image')"
log "Verifying $(basename "$IMAGE")"
(cd "$(dirname "$IMAGE")" && sha256sum --quiet --check "$(basename "$IMAGE").sha256") \
    || die "image checksum mismatch: $IMAGE"
[[ -s "$LAYER" && -s "$LAYER.sha256" && -s "$LAYER.manifest.json" ]] \
    || die "driver layer not found: $LAYER (run 'make nvidia-driver-layer')"
(cd "$(dirname "$LAYER")" && sha256sum --quiet --check "$(basename "$LAYER").sha256") \
    || die "driver layer checksum mismatch: $LAYER"

rm -rf "$WORK"
mkdir -p "$WORK/root/boot/grub"

# DIT-BOOT is the image's only EFI System partition; read it in place with mtools (no mount).
boot_start="$(sfdisk --dump "$IMAGE" | awk -F'[=,]' '/name="DIT-BOOT"/ { gsub(/ /, "", $2); print $2 }')"
[[ "$boot_start" =~ ^[0-9]+$ ]] || die "DIT-BOOT partition not found in $IMAGE"
boot_fs="$IMAGE@@$(( boot_start * 512 ))"
log "Copying the boot payload out of DIT-BOOT"
mcopy -s -n -i "$boot_fs" ::/live ::/cpu-dit ::/CPU-DIT-README.txt "$WORK/root/"
mcopy -n -i "$boot_fs" ::/boot/grub/grub.cfg "$WORK/root/boot/grub/grub.cfg"
# The signed UEFI chain of the image (shim -> Canonical GRUB, Secure Boot on) and its small
# EFI/ubuntu/grub.cfg, which finds this build's marker on the ISO and opens the same menu.
mkdir -p "$WORK/efi"
mcopy -s -n -i "$boot_fs" ::/EFI "$WORK/efi/"
for file in EFI/BOOT/BOOTX64.EFI EFI/BOOT/grubx64.efi EFI/BOOT/mmx64.efi EFI/ubuntu/grub.cfg; do
    [[ -s "$WORK/efi/$file" ]] || die "the image's signed UEFI chain is missing $file (rebuild it: make image)"
done
for file in live/vmlinuz live/initrd.img live/filesystem.squashfs cpu-dit/image-id; do
    [[ -s "$WORK/root/$file" ]] || die "boot payload is missing $file"
done
image_id="$(<"$WORK/root/cpu-dit/image-id")"
# The version the image was built from (its manifest); the ISO adds no code of its own.
dit_version="$(awk -F'"' '/"dit_version":/ { print $4 }' "$IMAGE.manifest.json" 2>/dev/null || true)"
iso_time="$(date +'%Y-%m-%d %H:%M %Z')"

layer_image_id="$(awk -F'"' '/"image_id":/ { print $4 }' "$LAYER.manifest.json")"
[[ "$layer_image_id" == "$image_id" ]] \
    || die "driver layer was built for $layer_image_id, image is $image_id (run 'make nvidia-driver-layer')"
# live-boot stacks /live/*.squashfs in name order, later on top: filesystem < nvidia-driver.
cp "$LAYER" "$WORK/root/live/nvidia-driver.squashfs"
layer_driver="$(awk -F'"' '/"nvidia_driver_package":/ { print $4 }' "$LAYER.manifest.json")"
log "Image $image_id + driver layer $layer_driver"
# A second information line under the image's own build line in the boot menu: when this ISO
# was packed and which driver it carries prebuilt.
awk -v title="ISO $iso_time | $layer_driver prebuilt" '
    /^menuentry "DIT-01 / && !done {
        printf "menuentry \"%s\" --id iso-info {\n    configfile /boot/grub/grub.cfg\n}\n\n", title
        done = 1
    }
    { print }
    END { if (!done) exit 1 }
' "$WORK/root/boot/grub/grub.cfg" >"$WORK/grub.cfg" \
    || die "boot menu has no DIT-01 entry to place the ISO line above"
mv -f "$WORK/grub.cfg" "$WORK/root/boot/grub/grub.cfg"

log "Creating the ${ISO_RESULT_MIB} MiB DIT-RESULTS partition"
truncate -s "${ISO_RESULT_MIB}M" "$WORK/results.img"
mkfs.fat -F 32 -n DIT-RESULTS "$WORK/results.img" >/dev/null
printf '%s\n' "$image_id" >"$WORK/CPU-DIT-RESULTS.ID"
mcopy -i "$WORK/results.img" "$WORK/CPU-DIT-RESULTS.ID" ::/CPU-DIT-RESULTS.ID
mmd -i "$WORK/results.img" ::/runs

# grub-mkrescue builds both the BIOS El Torito/MBR boot and the UEFI El Torito ESP, and uses the
# staged boot/grub/grub.cfg as-is (same menu, same kernel command line as the .img). The volume
# id is what partition-lookup.sh matches, as the FAT label on the .img.
log "Writing $(basename "$ISO")"
grub-mkrescue -o "$ISO.tmp" "$WORK/root" -- \
    -volid DIT-BOOT >"$WORK/grub-mkrescue.log" 2>&1 \
    || { tail -n 30 "$WORK/grub-mkrescue.log" >&2; die "grub-mkrescue failed"; }
# grub-mkrescue's own UEFI image (/efi.img: its GRUB, unsigned, 2.88 MB) is replaced by the
# image's signed chain, which needs more room. Replaying the boot equipment keeps the BIOS boot,
# the GPT EFI partition and the HFS+ one; the isohybrid-style MBR entries would overlap the
# protective one once the EFI image grows, so the MBR keeps only the protective entry and
# DIT-RESULTS, appended here (as partition 3, 0x0c, like before).
log "Putting the signed UEFI chain in place of grub-mkrescue's EFI image"
truncate -s 8M "$WORK/efi.img"
mkfs.fat -n DIT-EFI "$WORK/efi.img" >/dev/null
mmd -i "$WORK/efi.img" ::/EFI ::/EFI/BOOT ::/EFI/ubuntu
for file in EFI/BOOT/BOOTX64.EFI EFI/BOOT/grubx64.efi EFI/BOOT/mmx64.efi EFI/ubuntu/grub.cfg; do
    mcopy -i "$WORK/efi.img" "$WORK/efi/$file" "::/$file"
done
xorriso -indev "$ISO.tmp" -outdev "$ISO.signed" -boot_image any replay \
    -map "$WORK/efi.img" /efi.img -boot_image any part_like_isohybrid=off \
    -append_partition 3 0x0c "$WORK/results.img" -commit >"$WORK/xorriso-replay.log" 2>&1 \
    || { tail -n 30 "$WORK/xorriso-replay.log" >&2; die "xorriso could not put the signed EFI image in"; }
rm -f "$ISO.tmp"
mv -f "$ISO.signed" "$ISO"

sha="$(sha256sum "$ISO" | awk '{print $1}')"
bytes="$(stat -c %s "$ISO")"
printf '%s  %s\n' "$sha" "$(basename "$ISO")" >"$ISO.sha256"
cat >"$ISO.manifest.json" <<EOF
{
  "schema_version": 1,
  "project": "DIT-01",
  "format": "hybrid_iso",
  "image_id": "$image_id",
  "dit_version": "${dit_version:-unknown}",
  "built_local": "$iso_time",
  "filename": "$(basename "$ISO")",
  "size_bytes": $bytes,
  "sha256": "$sha",
  "source_image": "$(basename "$IMAGE")",
  "source_image_sha256": "$(awk '{print $1}' "$IMAGE.sha256")",
  "boot_modes": ["legacy_bios", "uefi"],
  "secure_boot": true,
  "nvidia_driver_layer": {
    "nvidia_driver_package": "$layer_driver",
    "target_kernel_version": "$(awk -F'"' '/"target_kernel_version":/ { print $4 }' "$LAYER.manifest.json")",
    "sha256": "$(awk '{print $1}' "$LAYER.sha256")"
  },
  "result_partition_mib": $ISO_RESULT_MIB,
  "write_mode": "raw (Rufus DD mode, dd, Etcher)"
}
EOF
rm -rf "$WORK"

log "Ready: $ISO ($(( bytes / 1024 / 1024 )) MiB)"
log "SHA-256: $sha"
log "Write it raw to a USB stick (Rufus: DD mode). Reports go to its DIT-RESULTS partition."
