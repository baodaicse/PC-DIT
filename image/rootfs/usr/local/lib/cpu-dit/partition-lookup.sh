#!/bin/bash
# Shared partition-lookup helpers for the DIT-01 live boot USB. Sourced by
# boot-runner.sh and by any other runner-side script that needs to find its own
# boot/result sibling partitions (e.g. nvidia-driver-install.sh) without
# duplicating this matching logic. Not executed directly.

find_boot_device() {
    # With `toram`, live-boot mount --move's a tmpfs copy over /run/live/medium,
    # so its mount source is no longer the physical USB; identify our own boot
    # device instead by matching the DIT-BOOT partition's baked-in image ID.
    # Two layouts carry it: the .img's FAT partition, and the hybrid ISO
    # (tools/build_iso.sh), whose ISO9660 volume sits on the whole disk once
    # written to a USB stick (on the drive itself when booted as a CD). The
    # ISO's Apple HFS+ partition repeats the label and is skipped.
    local expected_id candidate mnt actual_id
    expected_id="$(</etc/cpu-dit/image-id)"
    [[ -n "$expected_id" ]] || return 1
    mnt="/run/cpu-dit-boot-check"
    mkdir -p "$mnt"
    while read -r candidate label type fstype; do
        [[ "$label" == DIT-BOOT ]] || continue
        case "$type:$fstype" in
            part:vfat | disk:iso9660 | part:iso9660 | rom:iso9660) ;;
            *) continue ;;
        esac
        if mount -o ro "$candidate" "$mnt" 2>/dev/null; then
            actual_id="$(cat "$mnt/cpu-dit/image-id" 2>/dev/null || true)"
            umount "$mnt"
            if [[ "$actual_id" == "$expected_id" ]]; then
                printf '%s\n' "$candidate"
                return 0
            fi
        fi
    done < <(lsblk -lnpo NAME,LABEL,TYPE,FSTYPE 2>/dev/null)
    return 1
}

find_result_partition() {
    local boot_device parent candidate
    boot_device="$(find_boot_device || true)"
    [[ -n "$boot_device" ]] || return 1
    parent="$(lsblk -ndo PKNAME "$boot_device" 2>/dev/null || true)"
    # A hybrid ISO written to a USB stick: the boot volume is the disk itself
    # and DIT-RESULTS is its own appended partition. (A CD drive has none.)
    if [[ -z "$parent" && "$(lsblk -ndo TYPE "$boot_device" 2>/dev/null)" == disk ]]; then
        parent="${boot_device#/dev/}"
    fi
    [[ -n "$parent" && -b "/dev/$parent" ]] || return 1
    while read -r candidate label type; do
        if [[ "$type" == part && "$label" == DIT-RESULTS ]]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done < <(lsblk -lnpo NAME,LABEL,TYPE "/dev/$parent" 2>/dev/null)
    return 1
}

mount_result_partition() {
    local result_device expected_id actual_id
    result_device="$(find_result_partition || true)"
    [[ -n "$result_device" ]] || return 1
    mkdir -p /mnt/cpu-dit-results
    if mountpoint -q /mnt/cpu-dit-results; then
        local mounted_device
        mounted_device="$(findmnt -nro SOURCE /mnt/cpu-dit-results 2>/dev/null || true)"
        [[ "$(readlink -f "$mounted_device")" == "$(readlink -f "$result_device")" ]] || return 1
    else
        mount -o rw,nosuid,nodev,noexec,umask=0077 "$result_device" /mnt/cpu-dit-results \
            || return 1
    fi
    expected_id="$(</etc/cpu-dit/image-id)"
    actual_id="$(cat /mnt/cpu-dit-results/CPU-DIT-RESULTS.ID 2>/dev/null || true)"
    if [[ -z "$expected_id" || "$actual_id" != "$expected_id" ]]; then
        umount /mnt/cpu-dit-results
        return 1
    fi
    return 0
}
