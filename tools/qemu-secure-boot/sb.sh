#!/bin/bash
# Secure Boot state of the booted image, to the serial port, then power off.
exec 3>/dev/ttyS0
{
    echo "SBTEST kernel $(uname -r)"
    echo "SBTEST efi $([[ -d /sys/firmware/efi ]] && echo yes || echo no)"
    echo "SBTEST secureboot_var $(od -An -t u1 /sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-e2c4c4e45b4c 2>/dev/null | awk '{print $NF}')"
    echo "SBTEST lockdown $(cat /sys/kernel/security/lockdown 2>&1)"
    dmesg | grep -iE 'secure ?boot|lockdown|Loaded X.509|module verification' | sed 's/^/SBTEST dmesg /'
    k=$(uname -r)
    if [[ -d /usr/lib/dit/nvidia-modules ]]; then
        echo "SBTEST nv_layer $(cat /etc/cpu-dit/nvidia-driver-layer 2>/dev/null)"
        echo "SBTEST nv_open_signer $(modinfo -F signer /usr/lib/modules/$k/updates/dkms/nvidia.ko)"
        echo "SBTEST nv_open_load $(modprobe nvidia 2>&1 | tail -1)"
        rmmod nvidia 2>/dev/null
        echo "SBTEST nv_prop_load $(insmod /usr/lib/dit/nvidia-modules/proprietary/$k/nvidia.ko 2>&1 | tail -1)"
        dmesg | grep -iE 'NVRM|Key was rejected|module verification|nvidia' | tail -12 | sed 's/^/SBTEST nvdmesg /'
    fi
    echo "SBTEST cmdline $(cat /proc/cmdline)"
    echo "SBTEST done"
} >&3 2>&1
sync
sleep 2
systemctl poweroff
