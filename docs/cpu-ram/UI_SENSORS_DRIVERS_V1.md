# CPU-DIT-01 UI, Sensor và Driver v1

## Kết quả triển khai

Live image có UI text riêng trên console, giữ cách vận hành đơn giản tương tự một
appliance kiểm tra phần cứng. Màn hình chính hiển thị CPU model, core/thread, tổng
RAM, RAM khả dụng, NUMA node, nhiệt độ và CPU package Watt khi có sensor.

Các lựa chọn gồm Safe, Full, Sensor Info, Diagnostic shell, reboot và poweroff.
Lần mở đầu tự chạy Safe sau 10 giây để máy không cần bàn phím vẫn kiểm thử được.
Sau mỗi bài test, UI hiện PASS/FAIL/INVALID và quay lại màn hình chính.

Từ UI v2, khi tải đang chạy màn hình còn hiển thị progress, elapsed/minimum time,
write/verify phase, round và RAM target. Chi tiết duration/coverage nằm tại
[DURATION_COVERAGE_UI_V2.md](DURATION_COVERAGE_UI_V2.md).

UI v4 refresh mặc định mỗi 0,25 giây; sensor Watt/nhiệt độ lấy mẫu 100 ms và cập
nhật mỗi frame (từ 2026-09-01, trước đó cache hai frame ~1 giây) để dễ theo dõi
trong lúc test đang chạy. Frame sau cập nhật tại chỗ, không xóa toàn màn hình
nên console VGA không còn chớp theo chu kỳ. Thanh CPU/RAM đổi xanh, vàng, đỏ tại 75%/90%;
DIT target dùng cyan và progress dùng xanh. Dashboard còn hiện GPU PCI inventory,
window/pattern recipe của schema v6. Từ schema v7, menu có phím `T` bật/tắt
temperature limit. Mặc định OFF; sensor vẫn được hiển thị và lưu. Khi ON, chỉ
trusted CPU sensor tham gia auto-abort.

## Luồng đọc CPU Watt

Collector dùng thứ tự sau:

1. hwmon trên CPU package: `power*_input`, sau đó `power*_average`;
2. Linux powercap: lấy hai mẫu `energy_uj`, dùng thời gian monotonic và tính
   `mW = delta_energy_uj / delta_time_ms`;
3. nếu không có counter hợp lệ hoặc thiếu quyền đọc: `N/A`.

Report lưu source sysfs, label, loại phép đo, milliwatt và khoảng lấy mẫu. Counter
wrap được xử lý bằng `max_energy_range_uj`; sysfs symlink trùng được canonicalize
để không đếm một package hai lần. Collector không suy Watt từ nhiệt độ/TDP vì đó
không phải số đo công suất tức thời.

Trên HardwareValidator được xem read-only, engine đã có hướng hwmon và powercap
fallback, nhưng image Buildroot tham khảo không thấy bật powercap/RAPL. Ngoài ra
`k10temp` của máy AMD hiện tại chỉ xuất nhiệt độ. Đây là hai nguyên nhân hợp lý
khi UI cũ hiện `N/A`; CPU-DIT không sửa mã hay image HardwareValidator.

## Driver và thư viện trong image

Nền tảng là Ubuntu Noble minbase với HWE generic kernel, `linux-firmware` và
microcode theo kiến trúc. Các công cụ diagnostic thêm vào gồm:

- `lm-sensors`, `i2c-tools`, `rasdaemon`;
- `smartmontools`, `nvme-cli`, `dmidecode`, `ethtool`.

Boot probe thử module phổ biến cho CPU temperature/power/RAPL, I2C/super-I/O
sensor, EDAC và IPMI. Module tự nhận dạng thiết bị qua CPU/PCI/ACPI/DMI; không có
force parameter. Kết quả được lưu ở `/run/cpu-dit-driver-probe.txt` và chép vào
thư mục run trên `DIT-RESULTS`.

Tên thiết bị PCI (vd. GPU) dựa vào database `pci.ids` đóng gói cùng `pciutils`.
Database này của debootstrap cache có thể cũ hơn phần cứng vừa ra mắt, khiến
`lspci -nnk` hiện tên chung chung kiểu `NVIDIA Corporation Device [10de:xxxx]`
thay vì tên marketing thật. Từ 2026-09-01, `tools/build_boot_image.sh` chạy
`update-pciids` (best-effort, có timeout) ngay sau khi cài gói diagnostic để
làm mới database này tại thời điểm build — live image lúc boot không tự tải gì
qua mạng. Nếu máy build không có mạng, bước này bị bỏ qua và tên thiết bị vẫn
có thể chung chung; mã số PCI vendor:device trong ngoặc vuông luôn chính xác dù
tên không phân giải được.

GPU kernel/firmware baseline gồm `i915` (Intel), `amdgpu` và `radeon` (AMD),
`nouveau` (NVIDIA open driver), DRM/KMS, Ubuntu HWE kernel và `linux-firmware`.
Probe lưu thêm `lspci -nnk` cho VGA/3D/display controller. Các module proprietary
`nvidia`, `nvidia_modeset`, `nvidia_drm` chỉ được probe nếu đã tồn tại trong một
image chuyên biệt đúng kernel.

Không thể gọi đây là “mọi driver”: driver độc quyền GPU, vendor out-of-tree, phần
cứng mới hơn kernel/firmware trong image hoặc sensor bị BIOS khóa vẫn có thể thiếu.
Không bundle NVIDIA proprietary mặc định vì version phải khớp GPU/kernel, DKMS,
license phân phối và Secure Boot signing; quyết định sai có thể làm mất console.
Image chuẩn vẫn dùng nouveau, còn flavor NVIDIA riêng sẽ được tạo khi có model
GPU và thiết bị acceptance cụ thể.

## CLI và artifact evidence

```bash
target/release/dit-cli sensors --compact --interval-ms 250
sudo target/release/dit-cli sensors --interval-ms 500
sudo target/release/dit-cli sensors --json --interval-ms 500
```

Mỗi live run lưu thêm:

- `sensor-report.json`: inventory và source sensor có cấu trúc;
- `libsensors.txt`: góc nhìn từ libsensors;
- `driver-probe.txt`: module ready/not applicable;
- JSON system/reference report và boot/preflight log hiện có.

Availability không phải correctness gate mặc định. Có thể dùng
`--require-temperature-sensor` hoặc `--require-edac-counters` khi quy trình test
bắt buộc hai nguồn đó. CPU Watt hiện là evidence, chưa phải safety threshold.

## Giới hạn và bước validation tiếp theo

- Kiểm Safe/Full từ USB thật trên ít nhất một Intel và một AMD.
- Kiểm sensor trên mainboard desktop, laptop và server/IPMI.
- So Watt với một nguồn đo độc lập nếu cần đánh giá độ chính xác.
- Kiểm ECC/EDAC bằng phần cứng ECC thật; presence của module không chứng minh ECC.
- Chỉ thêm driver vendor khi có hardware cụ thể và license cho phép phân phối.

Thiết kế powercap theo Linux power capping sysfs; hwmon dùng đơn vị chuẩn của Linux
(`power*_input` là microwatt). HWE image dùng Ubuntu generic HWE kernel để có bộ
module rộng hơn một kernel minimal tự cắt giảm.
