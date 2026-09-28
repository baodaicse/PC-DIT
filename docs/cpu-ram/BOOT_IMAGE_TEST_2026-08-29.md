# CPU-DIT-01 Boot Image Test — 2026-08-29

## Phạm vi

Kiểm build artifact, GPT/FAT filesystem, nội dung SquashFS và boot end-to-end
trên QEMU. QEMU dùng TCG vì host không cấp `/dev/kvm`; mỗi VM có 2 GiB RAM và 2
vCPU. BIOS dùng SeaBIOS, UEFI dùng OVMF.

Artifact được kiểm:

```text
image_id  CPU-DIT-01-NOBLE-20260831-f98bc04dccb8
size      2,556,428,288 bytes
sha256    d2085baf7754d97c93535ecc9139d032954e0a2b42034908ff987b2776b5758d
kernel    7.0.0-30-generic
```

## Kiểm tra tĩnh

```text
sha256sum --check                         PASS
sfdisk --verify                           PASS; GPT, 3 partitions
fsck.fat -n DIT-BOOT                      PASS
fsck.fat -n DIT-RESULTS                   PASS
boot/result Image ID                      MATCH
SquashFS service/build-info inspection    PASS
UI/runner/driver-probe shell syntax       PASS
dashboard render 160x45 / compact 100x30 PASS / PASS
report ABI/schema                         7; window/pattern + temperature policy v2
HWE i915/amdgpu/radeon/nouveau modules    PRESENT
HWE k10temp/nct6775/intel_rapl modules    PRESENT
sensor/diagnostic packages                PRESENT
background diagnostic services           MASKED
profile defaults in final SquashFS        Safe 120s; Full 1800s; safe RAM 95%
```

`DIT-BOOT` chứa GRUB BIOS, `EFI/BOOT/BOOTX64.EFI`, kernel, initrd và
`live/filesystem.squashfs`. `DIT-RESULTS` bắt đầu với marker đúng Image ID và
thư mục `runs/` rỗng.

## QEMU legacy BIOS

```text
live root                                 read-only SquashFS + tmpfs overlay
result target                             verified sibling /dev/sda3
UI path                                   auto-Safe; minimum 120 stress seconds
reference exact_verified                  1,048,580 bytes; PASS
system allocated/exact_verified           268,435,456 bytes; PASS
workers/minimum rounds                    2 / 2; completed rounds may be higher
reference_exit/system_exit/overall_exit   0 / 0 / 0
```

## QEMU x86_64 UEFI

```text
live root                                 read-only SquashFS + tmpfs overlay
result target                             verified sibling /dev/sda3
UI path                                   auto-Safe; minimum 120 stress seconds
reference exact_verified                  1,048,580 bytes; PASS
system allocated/exact_verified           268,435,456 bytes; PASS
workers/minimum rounds                    2 / 2; completed rounds may be higher
reference_exit/system_exit/overall_exit   0 / 0 / 0
```

Hai firmware mode đều phát marker sau khi đóng gói dashboard refresh 0,5 giây,
schema v7, Temperature Policy v2 mặc định OFF, Pattern/Window và seed rotation v1:

```text
CPU-DIT BOOT TEST COMPLETE overall_exit=0
```

Serial evidence được giữ tại `build/qemu-smoke/bios-serial.log` và
`build/qemu-smoke/uefi-serial.log`. QEMU dùng snapshot overlay nên release image
không bị sửa bởi smoke test.

## Kết luận và giới hạn

Bước 5 đạt trạng thái `implemented`: image được dựng tái lập, UI tự khởi chạy,
cấu trúc hợp lệ và boot/chạy/report thành công ở BIOS + UEFI ảo hóa. QEMU không
có CPU power sensor thật nên `N/A` trong VM là kết quả đúng. Bằng chứng này chưa đủ cho
`validated`: cần burn USB và chạy Safe/Full trên máy thật, sau đó mở lại phân vùng
`DIT-RESULTS` để kiểm report; hardware matrix/soak vẫn thuộc Bước 6.
