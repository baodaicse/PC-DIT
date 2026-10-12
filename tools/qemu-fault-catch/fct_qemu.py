#!/usr/bin/env python3
"""Drive the DIT-01 image in QEMU through the fault-catch checks (docs/FAULT_CATCH_ROADMAP.md):
fct.sh runs S1 app killed, S2 app frozen, S3 rescue stop, S4 memory guard, S6 reset mid-session;
then, booted again on the same disk (QEMU -no-reboot exits on the reset), fct-after.sh collects
what the post-mortem wrote.

    python3 tools/qemu-fault-catch/fct_qemu.py dist/dit-01-noble-amd64.img
    python3 tools/qemu-fault-catch/fct_qemu.py dist/dit-01-noble-amd64.img --phase2

Work files, screenshots and logs go to build/qemu-fault-catch/.

Keys go in through the HMP monitor (sendkey), screens come out through screendump, and the
guest tells where it is with FCTEST lines on its serial port.
"""
import os
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "..", "build", "qemu-fault-catch")
os.makedirs(OUT, exist_ok=True)
IMAGE = sys.argv[1]
WORK = os.path.join(OUT, "work.img")
SERIAL = os.path.join(OUT, "serial.log")
MONITOR = os.path.join(OUT, "mon.sock")
SHOTS = os.path.join(OUT, "shots")
LOG = os.path.join(OUT, "harness.log")


def log(text):
    line = f"{time.strftime('%H:%M:%S')} {text}"
    print(line, flush=True)
    with open(LOG, "a") as f:
        f.write(line + "\n")


def sh(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout.strip()


def inject():
    loop = sh("sudo", "losetup", "-P", "--show", "-f", WORK)
    mnt = os.path.join(OUT, "mnt")
    os.makedirs(mnt, exist_ok=True)
    try:
        sh("sudo", "mount", f"{loop}p3", mnt)
        for name in ("fct.sh", "fct-after.sh"):
            sh("sudo", "cp", os.path.join(HERE, name), os.path.join(mnt, name))
        sh("sudo", "umount", mnt)
    finally:
        sh("sudo", "losetup", "-d", loop)


class Monitor:
    def __init__(self, path):
        for _ in range(100):
            try:
                self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.sock.connect(path)
                break
            except OSError:
                time.sleep(0.2)
        self.sock.settimeout(2)
        self.drain()

    def drain(self):
        try:
            while self.sock.recv(65536):
                pass
        except OSError:
            pass

    def cmd(self, text):
        self.sock.sendall((text + "\n").encode())
        time.sleep(0.15)
        self.drain()

    def key(self, name, hold=None):
        self.cmd(f"sendkey {name}" + (f" {hold}" if hold else ""))
        time.sleep(0.25)

    def type(self, text):
        names = {" ": "spc", "/": "slash", ".": "dot", "-": "minus", "_": "shift-minus",
                 "\n": "ret", "=": "equal"}
        for ch in text:
            self.key(names.get(ch, ch))

    def shot(self, name):
        os.makedirs(SHOTS, exist_ok=True)
        ppm = os.path.join(SHOTS, name + ".ppm")
        self.cmd(f"screendump {ppm}")
        time.sleep(1.5)
        try:
            from PIL import Image
            Image.open(ppm).save(os.path.join(SHOTS, name + ".png"))
            os.remove(ppm)
        except Exception as error:  # noqa: BLE001
            log(f"screenshot {name} failed: {error}")
        log(f"screenshot {name}")


class Serial:
    def __init__(self, path):
        self.path = path
        self.offset = 0

    def lines(self):
        try:
            with open(self.path, "rb") as f:
                f.seek(self.offset)
                data = f.read()
        except FileNotFoundError:
            return []
        cut = data.rfind(b"\n")
        if cut < 0:
            return []
        self.offset += cut + 1
        return data[: cut + 1].decode("utf-8", "replace").splitlines()

    def wait(self, needle, timeout):
        end = time.time() + timeout
        while time.time() < end:
            for line in self.lines():
                if needle in line:
                    return line
            time.sleep(1)
        raise TimeoutError(needle)


def main():
    phase2 = "--phase2" in sys.argv
    for path in (SERIAL, MONITOR) + (() if phase2 else (LOG,)):
        if os.path.exists(path):
            os.remove(path)
    if not phase2:
        log(f"copying {IMAGE}")
        sh("cp", "--sparse=always", IMAGE, WORK)
        inject()
    qemu = subprocess.Popen([
        "qemu-system-x86_64", "-machine", "q35,accel=tcg", "-cpu", "max", "-smp", "4",
        "-m", "4096", "-nodefaults", "-vga", "std", "-display", "none",
        "-device", "virtio-scsi-pci,id=scsi0",
        "-device", "scsi-hd,drive=systemdisk,bootindex=1",
        "-drive", f"id=systemdisk,if=none,format=raw,file={WORK}",
        "-device", "virtio-rng-pci", "-serial", f"file:{SERIAL}",
        "-monitor", f"unix:{MONITOR},server,nowait", "-no-reboot",
    ])
    mon = Monitor(MONITOR)
    serial = Serial(SERIAL)
    try:
        if not phase2:
            boot_to_shell(mon, serial, "fct.sh", "boot1")
            first = serial.wait("FCTEST start", 600)
            log(first)
            events(mon, serial)
            qemu.wait(timeout=120)
            return
        boot_to_shell(mon, serial, "fct-after.sh", "boot2")
        log(serial.wait("FCTEST after done", 900))
        qemu.wait(timeout=600)
        log("guest powered off")
    finally:
        if qemu.poll() is None:
            qemu.kill()


def boot_to_shell(mon, serial, script, name):
    log(serial.wait("Root: read-only SquashFS", 2400))
    # Driver probe, results volume, post-mortem, then the control center's menu.
    if name == "boot2":
        try:
            log(serial.wait("PREVIOUS SESSION", 900))
        except TimeoutError:
            log("no PREVIOUS SESSION line")
    time.sleep(150)
    mon.shot(f"{name}-menu")
    mon.type("d\n")
    time.sleep(8)
    mon.type(f"bash /mnt/cpu-dit-results/{script}\n")


def events(mon, serial):
    end = time.time() + 7200
    while time.time() < end:
        for line in serial.lines():
            if "FCTEST" not in line:
                continue
            log(line)
            if "S1 screen" in line:
                time.sleep(3)
                mon.shot("s1-blackbox-result")
                mon.key("alt-f1")
            elif "S2 claim" in line:
                time.sleep(8)
                mon.shot("s2-blackbox-result")
                mon.key("alt-f1")
            elif "S3 ready" in line:
                time.sleep(3)
                mon.key("alt-f2")
                time.sleep(6)
                mon.shot("s3-rescue")
                mon.key("s")
                time.sleep(1.5)
                mon.key("s")
                time.sleep(6)
                mon.shot("s3-rescue-stopping")
            elif "S3 claim" in line:
                time.sleep(4)
                mon.shot("s3-rescue-after")
                mon.key("l")
                time.sleep(1.5)
                mon.key("l")
                time.sleep(10)
                mon.shot("s3-rescue-diagnostics")
                mon.key("alt-f1")
            elif "S6 reset" in line:
                time.sleep(5)
                mon.shot("s6-before-reset")
                mon.cmd("system_reset")
                log("system_reset")
                return
        time.sleep(1)
    raise TimeoutError("events")


if __name__ == "__main__":
    main()
