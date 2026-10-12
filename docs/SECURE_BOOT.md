# Secure Boot — boot USB không cần tắt Secure Boot

Từ 2026-10-12, cả `.img` lẫn ISO đều boot được khi **Secure Boot đang bật**, trên máy dùng khóa
mặc định của Microsoft (gần như mọi máy PC/laptop/server bán ra). Không cần tắt Secure Boot, không
cần enroll khóa (MOK).

## Cách làm: dùng nguyên chuỗi đã ký của Ubuntu

Firmware chỉ chạy file EFI có chữ ký nó tin. DIT-01 không tự ký gì cả mà dùng nguyên các thành
phần Ubuntu đã ký:

| Bước | File trên USB | Ai ký | Ai kiểm |
|---|---|---|---|
| 1 | `EFI/BOOT/BOOTX64.EFI` = `shimx64.efi.signed` (gói `shim-signed`) | Microsoft UEFI CA | firmware |
| 2 | `EFI/BOOT/grubx64.efi` = `grubx64.efi.signed` (gói `grub-efi-amd64-signed`) | Canonical | shim |
| 3 | `/live/vmlinuz` = `vmlinuz-7.0.0-38-generic` | Canonical | GRUB qua shim |
| 4 | module kernel, kể cả NVIDIA | Canonical | kernel |

`EFI/BOOT/mmx64.efi` (MokManager) đi kèm để shim không dừng lại khi cần nó.

GRUB đã ký có sẵn các module của nó và luôn đọc `/EFI/ubuntu/grub.cfg` trên phân vùng nó được nạp
từ. File này chỉ có ba dòng: tìm thiết bị chứa `/cpu-dit/boot-<image-id>.mark` của đúng build này,
rồi mở `/boot/grub/grub.cfg`, tức là **cùng một menu** với BIOS. Menu không đổi.

Boot BIOS (legacy/CSM) giữ nguyên: `grub-install --target=i386-pc` như trước.

### Kernel được ghim

`build_boot_image.sh` ghim kernel `KERNEL_ABI` (mặc định `7.0.0-38`) thay vì gói meta HWE. Lý do:
module NVIDIA đã ký được Ubuntu phát hành theo **đúng từng ABI kernel**, và phải khớp phiên bản
userspace của driver. `ensure_kernel` cài kernel này, gỡ mọi kernel khác trong rootfs, và kiểm
`vmlinuz` có chữ ký Canonical (`sbverify`). `fetch_signed_efi` tải shim và GRUB đã ký từ archive
Ubuntu rồi kiểm chữ ký Microsoft và Canonical.

Khi đổi `KERNEL_ABI`, phải kiểm cho ABI mới rằng archive có
`linux-modules-nvidia-580-open-<abi>-generic` và rằng nó đòi đúng phiên bản `nvidia-kernel-common-580`
mà archive đang phát.

### ISO

`grub-mkrescue` tự tạo `/efi.img` với GRUB của nó, không có chữ ký. `build_iso.sh` dựng một
`efi.img` FAT mới chứa chuỗi đã ký lấy từ DIT-BOOT của `.img`, rồi dùng
`xorriso -boot_image any replay -map efi.img /efi.img` để thay vào. Boot BIOS, bảng phân vùng và
DIT-RESULTS được giữ nguyên.

## Driver NVIDIA: module đã ký sẵn thay vì DKMS

Module do DKMS build trên máy mình không có chữ ký. Khi Secure Boot bật, kernel Ubuntu ở chế độ
lockdown và từ chối nạp chúng ("Key was rejected by service"). Vì vậy lớp driver nay dùng module
Canonical đã build và ký cho đúng kernel của image:

- **open**: `linux-modules-nvidia-580-open-<kernel>` cung cấp sẵn `.ko` đã ký.
- **proprietary** (GTX 9xx/10xx, Titan V): `linux-modules-nvidia-580-<kernel>` link `.ko` lúc cài,
  từ `linux-objects-nvidia-580-<kernel>` (object do Canonical build) và
  `linux-signatures-nvidia-<kernel>` (chữ ký tách rời). Gói kiểm SHA-256 của `.ko` sau khi link,
  nên kết quả trùng từng byte với bản Canonical đã ký. Bước link cần `binutils` và header của
  kernel (`objtool`, `module.lds`). Những thứ này chỉ dùng lúc build và không nằm trong lớp.
- Userspace giữ đúng như `nvidia-driver-580-open` nhưng bỏ `nvidia-dkms-*` và
  `nvidia-kernel-source-*` (danh sách ở `tools/nvidia_packages.sh`).

`build_nvidia_driver_package.sh` tự chọn nguồn module (`NVIDIA_MODULE_SOURCE=auto`): dùng bản đã ký
nếu archive có cho kernel của image, ngược lại quay về DKMS (khi đó phải tắt Secure Boot). Có thể ép
bằng `NVIDIA_MODULE_SOURCE=signed` hoặc `dkms`. Manifest của gói và của lớp ghi `module_source`.
Lớp còn ghi `secure_boot: true|false`.

`build_nvidia_driver_layer.sh` đặt bản open vào `updates/dkms` làm mặc định và cất cả hai bộ vào
`/usr/lib/dit/nvidia-modules/{open,proprietary}/<kernel>/` như trước, nên `nvidia-flavor.sh` không
phải đổi gì. Build dừng lại nếu có `nvidia.ko` nào thiếu chữ ký (`modinfo -F signer`).

Profile `nvidia-image` (rootfs nướng sẵn) cũng chuyển sang module đã ký.

## Điều gì đổi khi Secure Boot bật

Kernel vào **lockdown (integrity)**: không nạp module chưa ký, không ghi `/dev/mem`, không
`kexec` ảnh chưa ký. DIT-01 không dùng những thứ này: không đọc MSR qua `/dev/cpu/*/msr` và không
đọc `/dev/mem`. Sensor đi qua hwmon/sysfs, SMART qua ioctl NVMe, GPU qua driver. Vì vậy các bài
test chạy như khi Secure Boot tắt.

## Đã kiểm / chưa kiểm

Kiểm bằng QEMU (q35, TCG) ngày 2026-10-12, harness `tools/qemu-secure-boot/`. Firmware Secure Boot là `OVMF_CODE_4M.secboot.fd` +
`OVMF_VARS_4M.ms.fd`: Secure Boot bật sẵn, có khóa Microsoft, giống một PC bán ra.

| Kiểm | Kết quả |
|---|---|
| `make test` (gồm `test_boot_policy.sh`: không còn `--no-uefi-secure-boot`, có shim/GRUB đã ký, ISO thay `efi.img`, lớp NVIDIA kiểm chữ ký) | PASS |
| `.img`, UEFI + Secure Boot | shim → GRUB → menu DIT → kernel `7.0.0-38-generic`; `secureboot: Secure boot enabled`, `lockdown: [integrity]`; control center chạy |
| `.img`, BIOS | boot bình thường (`lockdown: [none]`) |
| ISO (ghi như ổ USB), UEFI + Secure Boot | như `.img`; lớp NVIDIA `modules=signed` |
| ISO, nạp `nvidia.ko` khi lockdown | cả bản open (`modprobe nvidia`) lẫn proprietary (`insmod`) đều qua kiểm chữ ký, kernel chạy tới `NVRM: No NVIDIA GPU found` (QEMU không có GPU), không có `Key was rejected` |
| ISO, BIOS | boot bình thường (`lockdown: [none]`), lớp NVIDIA có mặt |
| Build lớp NVIDIA | 501 MiB; cả hai `nvidia.ko` mang chữ ký `Canonical Ltd. Kernel Module Signing`; license open `Dual MIT/GPL`, proprietary `NVIDIA` |

Chưa kiểm:

- **Máy thật.** Một số firmware cũ chưa có khóa "Microsoft UEFI CA 2023" hoặc có dbx cũ/mới khác
  QEMU. Shim là bản `15.8` (gói `shim-signed 1.58`) Ubuntu đang phát.
- GPU NVIDIA thật dưới Secure Boot: cần xem `nvidia-smi` và GPU-VRAM chạy CUDA.
- Máy đã bật Secure Boot với khóa riêng của doanh nghiệp (không có khóa Microsoft): khi đó vẫn phải
  tắt Secure Boot hoặc enroll khóa.
