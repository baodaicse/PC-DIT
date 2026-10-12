#!/usr/bin/env python3
"""Boot a DIT-01 .img or .iso in QEMU and record the guest's Secure Boot state (docs/SECURE_BOOT.md).

    sb_qemu.py <image> sb|uefi|bios <tag>

sb is OVMF with Secure Boot on and Microsoft's keys enrolled, as on a PC from the shop. sb.sh is
copied onto the image's DIT-RESULTS, run from the control center's shell (menu "d"), writes
SBTEST lines to the serial log (serial-<tag>.log) and powers off. Works on a copy of the image.
"""
import os
import shutil
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
IMAGE, MODE, TAG = sys.argv[1], sys.argv[2], sys.argv[3]  # MODE: sb | uefi | bios
WORK = os.path.join(HERE, f"work-{TAG}.img")
SERIAL = os.path.join(HERE, f"serial-{TAG}.log")
MONITOR = os.path.join(HERE, f"mon-{TAG}.sock")
VARS = os.path.join(HERE, f"vars-{TAG}.fd")


def sh(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()


def inject():
    loop = sh("sudo", "losetup", "-P", "--show", "-f", WORK)
    mnt = os.path.join(HERE, f"mnt-{TAG}")
    os.makedirs(mnt, exist_ok=True)
    try:
        # Partition 3 on the .img, 4 on the ISO (after its HFS+ one).
        part = sh("sh", "-c", f"for p in {loop}p*; do [ \"$(sudo blkid -s LABEL -o value $p)\" = DIT-RESULTS ] && echo $p; done; true")
        sh("sudo", "mount", part, mnt)
        sh("sudo", "cp", os.path.join(HERE, "sb.sh"), os.path.join(mnt, "sb.sh"))
        sh("sudo", "umount", mnt)
    finally:
        sh("sudo", "losetup", "-d", loop)


def send(sock, text):
    sock.sendall((text + "\n").encode())
    time.sleep(0.3)
    try:
        while sock.recv(65536):
            pass
    except OSError:
        pass


def type_text(sock, text):
    names = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", "_": "shift-minus", "\n": "ret"}
    for ch in text:
        send(sock, f"sendkey {names.get(ch, ch)}")


def wait(needle, timeout):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with open(SERIAL, "rb") as f:
                data = f.read().decode("utf-8", "replace")
            if needle in data:
                return True
        except FileNotFoundError:
            pass
        time.sleep(2)
    return False


def main():
    for path in (SERIAL, MONITOR):
        if os.path.exists(path):
            os.remove(path)
    sh("cp", "--sparse=always", IMAGE, WORK)
    inject()
    args = ["qemu-system-x86_64", "-cpu", "max", "-smp", "4", "-m", "4096", "-nodefaults",
            "-vga", "std", "-display", "none",
            "-device", "virtio-scsi-pci,id=scsi0",
            "-device", "scsi-hd,drive=systemdisk,bootindex=1",
            "-drive", f"id=systemdisk,if=none,format=raw,file={WORK}",
            "-device", "virtio-rng-pci", "-serial", f"file:{SERIAL}",
            "-monitor", f"unix:{MONITOR},server,nowait", "-no-reboot"]
    if MODE == "bios":
        args += ["-machine", "q35,accel=tcg"]
    else:
        code = "/usr/share/OVMF/OVMF_CODE_4M.secboot.fd" if MODE == "sb" else "/usr/share/OVMF/OVMF_CODE_4M.fd"
        vars_src = "/usr/share/OVMF/OVMF_VARS_4M.ms.fd" if MODE == "sb" else "/usr/share/OVMF/OVMF_VARS_4M.fd"
        shutil.copy(vars_src, VARS)
        args += ["-machine", "q35,smm=on,accel=tcg",
                 "-global", "driver=cfi.pflash01,property=secure,value=on",
                 "-drive", f"if=pflash,format=raw,unit=0,file={code},readonly=on",
                 "-drive", f"if=pflash,format=raw,unit=1,file={VARS}"]
    qemu = subprocess.Popen(args)
    try:
        sock = None
        for _ in range(100):
            try:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(MONITOR)
                break
            except OSError:
                time.sleep(0.2)
        sock.settimeout(1)
        if not wait("Root: read-only SquashFS", 2400):
            send(sock, f"screendump {os.path.join(HERE, f'stuck-{TAG}.ppm')}")
            print(f"{TAG}: never reached the root filesystem", flush=True)
            return
        time.sleep(150)
        send(sock, f"screendump {os.path.join(HERE, f'menu-{TAG}.ppm')}")
        type_text(sock, "d\n")
        time.sleep(8)
        type_text(sock, "bash /mnt/cpu-dit-results/sb.sh\n")
        print(f"{TAG}: done={wait('SBTEST done', 300)}", flush=True)
        qemu.wait(timeout=180)
    finally:
        if qemu.poll() is None:
            qemu.kill()


if __name__ == "__main__":
    main()
