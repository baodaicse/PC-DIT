# CPU-DIT-01 Ubuntu Noble Live Image v1

## Artifact

```text
File:     dist/cpu-dit-01-noble-amd64.img
Size:     2,556,428,288 bytes (khoảng 2.4 GiB)
Image ID: CPU-DIT-01-NOBLE-20260901-843d7743c91a
SHA-256:  115c3afe2318285ef633827e725f33cbeb3916b72f8bc537b1a4d69916205395
```

Đây là raw disk image, không phải installer ISO. Image được dựng từ Ubuntu Noble
`debootstrap --variant=minbase`, dùng HWE kernel, `live-boot`, root SquashFS
read-only và tmpfs overlay. Một USB từ 4 GB trở lên là đủ cho artifact v1.

## Ghi USB bằng Rufus trên Windows

> Cảnh báo: thao tác này xóa toàn bộ dữ liệu trên USB đã chọn. Phải kiểm tra đúng
> tên và dung lượng USB; tuyệt đối không chọn nhầm ổ chứa Windows/dữ liệu.

1. Mở Rufus và chọn đúng USB trong `Device`.
2. `Boot selection` → `SELECT` → chọn `cpu-dit-01-noble-amd64.img`.
3. Nhấn `START`. Nếu Rufus hỏi phương thức, chọn `Write in DD Image mode`.
4. Chờ Rufus hoàn tất rồi eject USB an toàn.
5. Vào BIOS/UEFI của máy test và boot từ USB. Secure Boot để bật cũng được (xem `docs/SECURE_BOOT.md`).

Rufus hỗ trợ ghi flat `.img` theo DD/raw mode; [FAQ chính thức của Rufus](https://github.com/pbatard/rufus/wiki/FAQ)
cũng mô tả DD Image mode. Không format hoặc copy riêng các file bên trong image.

Kiểm hash trên Windows trước khi ghi:

```powershell
certutil -hashfile cpu-dit-01-noble-amd64.img SHA256
```

## Ghi bằng Linux

Xác định chính xác whole-device của USB bằng `lsblk`; ví dụ `/dev/sdX`, không phải
partition `/dev/sdX1`. Sau đó unmount các partition của USB và ghi raw image:

```bash
sudo dd if=dist/cpu-dit-01-noble-amd64.img of=/dev/sdX bs=16M status=progress conv=fsync
sync
```

`/dev/sdX` chỉ là ví dụ. Chọn sai device sẽ ghi đè dữ liệu không thể hoàn tác.

## Layout và chính sách ghi

| Partition | Kích thước | Mục đích |
|---|---:|---|
| 1 `BIOS-BOOT` | 4 MiB | GRUB metadata cho legacy BIOS |
| 2 `DIT-BOOT` | 1,408 MiB | FAT32 ESP, kernel/initrd/SquashFS |
| 3 `DIT-RESULTS` | 1,024 MiB | FAT32, report của các phiên test |

Runner chỉ mount `DIT-RESULTS` khi tìm được đúng partition `DIT-BOOT` (xác nhận
bằng Image ID khớp `/etc/cpu-dit/image-id`, không còn phụ thuộc mount point
`/run/live/medium` của `live-boot` vì `toram` sẽ thay nguồn của mount đó bằng
tmpfs) và `DIT-RESULTS` phải là sibling partition trên cùng parent block device,
có file marker đúng Image ID. Nếu điều kiện không đạt, report chỉ ở tmpfs và mất
khi tắt máy; runner không fallback sang ổ nội bộ. Image không có `fstab`
persistent mount, không có installer và không tự format ổ của máy test.

## Chạy toàn bộ từ RAM (`toram`)

Từ bản này, mọi kernel command line đều thêm `toram`: `live-boot` copy toàn bộ
nội dung `DIT-BOOT` (kernel, initrd, SquashFS) vào tmpfs ngay khi boot, trước khi
chạy bất kỳ test nào. Lý do: nếu root filesystem vẫn đọc trực tiếp từ USB, một
lần rút USB giữa chừng (hoặc cổng USB lỏng/chập chờn) sẽ làm crash tiến trình
đang chạy vì trang squashfs chưa cache có thể đọc lỗi bất kỳ lúc nào. Với
`toram`, sau khi copy xong, toàn bộ hệ điều hành và `dit-cli` chạy hoàn toàn từ
RAM, không phụ thuộc USB còn cắm hay không trong suốt phần CPU/RAM stress.

`DIT-RESULTS` (nơi ghi report cuối) vẫn là một partition thật trên USB. USB có
thể rút ra bất kỳ lúc nào, kể cả giữa lúc test đang chạy:

- Mỗi test ghi report vào tmpfs (`/run/cpu-dit-results/<session>`,
  `/run/dit-suite/...`), rồi chép một lần sang `DIT-RESULTS/runs/<session>` khi
  kết thúc, nếu USB vẫn còn cắm. Màn hình kết quả ghi `Reports saved to:` (đã lưu
  vào USB), hoặc `reports remain in tmpfs` (USB đã rút; report mất khi tắt máy,
  không fallback sang ổ nội bộ).
- Khi USB bị rút, mount cũ vẫn còn đó, `-d` và `mountpoint` vẫn báo có, nhưng mọi
  lần ghi đều lỗi EIO. Vì vậy mọi kiểm tra "có ổ kết quả không" đều đi qua
  `volume_alive` (Rust, `dit_orchestrator::session`) hoặc `results_volume_alive`
  (shell, `partition-lookup.sh`): mount chỉ được tính khi node `/dev` mà nó trỏ
  tới vẫn còn và vẫn là đúng thiết bị (`major:minor`).
- Các bước ghi lên USB trong lúc test chạy (marker của watchdog, journal hộp đen,
  claim kết quả, dòng `fail-summary.csv`) đều không làm test thất bại khi ghi
  lỗi. Claim kết quả quyết định bằng file trong tmpfs, còn bản trên USB chỉ là
  bản sao.
- Cắm USB lại: trước mỗi màn hình menu, `dit-suite --results-remount` gỡ mount
  cũ đã chết rồi mount lại đúng `DIT-RESULTS` (vẫn xác nhận theo Image ID). Từ đó
  các test sau lại lưu được report lên USB.

`toram` tiêu tốn thêm RAM đúng bằng dung lượng `DIT-BOOT` đã dùng (khoảng 900
MiB ở image hiện tại) trong suốt phiên; trên máy test thật (16–64+ GiB RAM) mức
này không đáng kể, và target RAM của Safe/Full vẫn tự tính từ `MemAvailable`
thực tế sau khi `toram` đã tiêu thụ, không cần chỉnh gì thêm. Máy QEMU dùng cho
`make qemu-smoke` phải cấp tối thiểu ~3 GiB RAM để còn đủ chỗ cho Safe profile
sau khi trừ phần `toram`.


## Giao diện và profile khi boot

GRUB mặc định mở UI text. UI cập nhật CPU model, core/thread, RAM, NUMA, nhiệt độ
và CPU package Watt nếu kernel cung cấp nguồn hợp lệ. Các lựa chọn:

- `1 Safe test`: 256 MiB, ít nhất 2 round/2 phút; tự chạy sau 10 giây lần đầu.
- `2 Full test`: 95% safe allocatable RAM sau reserve 512 MiB, ít nhất 4 round/30 phút.
- `3 Sensor Info`: nguồn nhiệt, Watt, EDAC, hwmon, powercap và module đã nạp.
- `4 Diagnostic`: root shell thủ công; `exit` để quay lại UI.
- `D Test Duration`: đổi minimum duration của Full từ 60 giây đến 24 giờ.
- `T Temp Limit`: bật/tắt auto-abort nhiệt; mặc định OFF, telemetry luôn ghi.
- `R/P`: reboot hoặc poweroff.

GRUB vẫn có entry Safe/Full unattended và shell để phục vụ automation.

Các entry vật lý chỉ đặt `console=tty0`; không ép kernel ghi sang UART/`ttyS0`.
Runner chỉ mirror sang serial khi phát hiện đang chạy trong máy ảo hoặc khi người
vận hành chủ động thêm `cpu_dit_serial=1`. Service khởi động sau `local-fs.target`
và không chờ global `systemd-udev-settle`; probe driver có timeout riêng để một
thiết bị/module lỗi không giữ UI vô thời hạn.

Full không có nghĩa là 100% physical RAM. Linux/kernel/firmware cần phần RAM
reserved và phần mềm chỉ công bố `allocatable RAM coverage`. Với RAM 32/64/128 GB,
cùng binary tự tính target theo RAM khả dụng, không cần image riêng.

Khi đang test, UI rộng chia thành ba khung hardware, live utilization và process;
hiển thị progress theo minimum duration, phase write/verify, round, RAM target,
window/pattern recipe, nhiệt độ, Watt và trạng thái temperature limit. Refresh mặc định
0,5 giây, cập nhật tại chỗ không clear mỗi frame; các meter CPU/RAM/progress có
màu và console nhỏ tự dùng bản compact. Đủ thời
gian chưa có nghĩa được dừng giữa round; engine hoàn tất exact verification rồi
mới quyết định PASS/FAIL. Xem [UI_DASHBOARD_V3.md](UI_DASHBOARD_V3.md).

Khi hoàn tất, màn hình hiển thị:

```text
CPU-DIT BOOT TEST COMPLETE overall_exit=0
```

Report nằm trong `DIT-RESULTS/runs/<session-id>/`, gồm JSON test, preflight,
`sensor-report.json`, `libsensors.txt` và `driver-probe.txt`. Dùng `poweroff`, chờ máy
tắt hẳn rồi mới rút USB. `overall_exit` khác 0 là FAIL/INVALID và phải giữ toàn bộ
thư mục session để điều tra.

## Build và kiểm thử lại

Phương án đơn giản được khuyến nghị:

```bash
make rufus-image
make image-info
make run-image
```

Chi tiết tại [MAKE_RUFUS_IMAGE.md](MAKE_RUFUS_IMAGE.md). Các script thấp hơn vẫn
có thể gọi trực tiếp:

```bash
./tools/build_boot_image.sh
(cd dist && sha256sum --check cpu-dit-01-noble-amd64.img.sha256)
./tools/test_boot_image_qemu.sh
```

Build sinh `.img`, `.img.sha256` và `.img.manifest.json`. Rootfs Noble được cache
ở `build/boot-image`; xóa cache này chỉ khi chủ động muốn bootstrap sạch.

## Sensor và driver

Image dùng Ubuntu HWE generic kernel và firmware. Nó bổ sung `lm-sensors`,
`i2c-tools`, `rasdaemon`, `smartmontools`, `nvme-cli`, `dmidecode`, `ethtool` và
probe có kiểm soát các module CPU temperature/power, I2C board sensor, EDAC,
IPMI và GPU nguồn mở `i915`, `amdgpu`, `radeon`, `nouveau`. Mỗi module tự đối
chiếu PCI/ACPI/DMI/CPU; script không dùng tham số force.

Watt ưu tiên hwmon `power*_input`/`power*_average`. Khi chỉ có cumulative energy,
collector lấy hai mẫu `powercap/*/energy_uj` và tính Watt từ chênh lệch năng lượng
theo thời gian monotonic. Sensor không tồn tại hoặc không đọc được sẽ hiện `N/A`.
Đây là evidence trung thực, vì không phải CPU/mainboard nào cũng xuất power counter.

“Driver rộng” ở đây là driver nguồn mở/module/firmware do Ubuntu HWE cung cấp cho
phạm vi CPU/RAM và diagnostic phổ biến. Image không thể cam kết mọi thiết bị đời
mới, driver độc quyền GPU, vendor out-of-tree hoặc sensor bị firmware khóa.
NVIDIA cần flavor/lớp driver riêng khớp kernel; module là bản Canonical đã ký nên chạy
được khi Secure Boot bật. Image mặc định chặn nouveau.

## Giới hạn v1

- UEFI dùng chuỗi Ubuntu đã ký (shim → GRUB → kernel của Canonical), boot được khi Secure Boot bật
  (từ 2026-10-12, `docs/SECURE_BOOT.md`).
- Đã kiểm boot bằng QEMU BIOS/UEFI nhưng chưa được `validated` trên USB/hardware
  matrix thật.
- Hard watchdog có thể TERM/KILL process bị treo; kernel/firmware hard hang vẫn
  cần watchdog hoặc power control ngoài.
- Result partition 1 GiB; cần sao lưu/xóa các run cũ khi gần đầy.

Ubuntu Noble tương ứng Ubuntu 24.04 LTS; tham khảo
[Ubuntu Server download](https://ubuntu.com/download/server) và
[Ubuntu 24.04.4 release](https://releases.ubuntu.com/24.04.4/).
