DIT-01 CPU-RAM + CPU-SSD + GPU-VRAM / Ubuntu Noble Minimal Live USB

Write the .img file in DD/raw mode. Disable Secure Boot for this unsigned v1 image.

Partitions:
  1. BIOS boot metadata
  2. DIT-BOOT (FAT32, Ubuntu live system)
  3. DIT-RESULTS (FAT32, reports only)

The runner mounts the results partition only when it can find the DIT-BOOT
partition matching this build's image marker (independent of live-boot's own
mount state), and DIT-RESULTS must be its sibling partition with a matching
marker. It never falls back to an internal disk.

Boot uses `toram`: the whole DIT-BOOT payload is copied into RAM before any
test starts, so the running test does not depend on the USB staying inserted.
DIT-RESULTS is still a real USB partition, so reports are only persisted while
the USB is present; otherwise reports stay in tmpfs only and are lost at
poweroff.

The first boot menu line shows when this build was made (build host's local
time) and its image-id; the ISO adds a second line with its own packing time
and the NVIDIA driver it carries prebuilt. Both are information only: selecting
one just reloads the menu.

The default entry opens the shared module control center. Select CPU-RAM,
CPU-SSD or GPU-VRAM explicitly. Selecting an SSD starts the data-preserving
file-mode test after stable-ID and volume revalidation; it never formats a
selected SSD. GPU-A validates the data contract with a CPU mock backend and
does not yet test VRAM hardware; Vulkan/VRAM access begins in GPU-B.
CPU package Watts in CPU-RAM are shown only when hwmon or powercap exposes a
readable hardware counter; N/A is reported explicitly instead of inventing data.

Safe runs for at least 2 minutes. Full defaults to at least 30 minutes and uses
95% of the allocatable limit remaining after the Linux safety reserve. The UI
shows live elapsed time, phase, round, RAM target, temperature and CPU power.
