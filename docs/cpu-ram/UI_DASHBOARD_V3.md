# CPU-DIT-01 Live Test Dashboard v4

## Mục tiêu

Màn hình đang test dùng bố cục full-screen tương tự ảnh tham chiếu của
HardwareValidator, nhưng phần triển khai và dữ liệu thuộc riêng CPU-DIT-01.
HardwareValidator chỉ được đọc để đối chiếu cách chia khung; không có file nào
trong project đó bị sửa hoặc được liên kết vào image.

Dashboard rộng có ba vùng:

1. `SYSTEM HARDWARE`: CPU model, logical CPU, tổng RAM, NUMA, GPU PCI, mainboard và kernel.
2. `LIVE UTILIZATION`: CPU usage/frequency/temperature/power, RAM used/available/
   cache và tỷ lệ DIT target trên tổng RAM.
3. `TESTING / PROCESS INFORMATION`: state, progress, elapsed/minimum duration,
   phase, round, session/round seed, window/pattern recipe, worker policy, exact
   test buffer, coverage và temperature-limit state.

GPU được hiển thị để xác nhận inventory/driver cho giai đoạn mở rộng sau; v4 chưa
chạy GPU/VRAM DIT. `DIT TARGET` vẫn là thông tin trực tiếp cần để đánh giá
allocatable RAM coverage.

## Dữ liệu và semantics

- CPU usage lấy delta counter từ `/proc/stat`.
- RAM used dùng `MemTotal - MemAvailable`; cache gồm `Buffers + Cached +
  SReclaimable` từ `/proc/meminfo`.
- Frequency ưu tiên cpufreq sysfs, fallback về `cpu MHz`.
- Nhiệt độ và CPU package Watt dùng backend sensor hiện có của `dit-cli`.
- Mainboard đọc DMI sysfs; kernel, CPU topology và NUMA đọc từ hệ thống live.
- `DIT TARGET` lấy từ progress event thật của engine, không suy đoán từ profile.
- Progress là tỷ lệ minimum stress duration và giữ tối đa 99% cho tới khi engine
  hoàn thành nguyên write/read/verify round và phát kết quả cuối.
- Thermal warning hiển thị `AUTO-ABORT ON (95 C)`, đúng với policy runner; thiếu
  sensor vẫn được ghi là `N/A`, không tạo số liệu giả.
- Session seed/round seed lấy từ progress event thật; UI không còn hard-code seed.
- Window/pattern recipe lấy từ progress event schema v7; nhiều worker chạy đồng
  thời được ghi trung thực là `MIXED_PARALLEL`/`MULTI_FAMILY`.
- Refresh mặc định 0,5 giây (2 FPS). Sensor backend cập nhật mỗi hai frame với
  sample 100 ms để tránh chi phí đo Watt ở mọi lần vẽ.
- Dashboard chỉ xóa màn hình ở frame đầu hoặc khi terminal đổi kích thước/chế
  độ. Các frame sau dùng cursor-addressed in-place update, loại bỏ chớp đen do
  `ESC[2J` trên Linux VGA console.
- Meter CPU/RAM tự đổi xanh dưới 75%, vàng từ 75% và đỏ từ 90%; DIT target dùng
  cyan, progress dùng xanh.
- Footer hiện `TEMP LIMIT OFF — TELEMETRY ONLY` theo mặc định. Phím `T` ở menu
  đổi sang ON 95°C cho trusted CPU sensor trước khi bắt đầu bài test.

Phase được diễn giải trực tiếp trên màn hình: `PREFLIGHT`, `ALLOCATED`,
`RAM WRITE`, `RAM VERIFY`, `ROUND COMPLETE` và `FINALIZING`.

## Responsive terminal

Dashboard ba khung được dùng khi terminal có ít nhất 110 cột và 32 dòng. Console
nhỏ hơn tự chuyển sang giao diện compact hiện có, nên QEMU curses, serial console
và màn hình VGA thấp không bị vỡ dòng. Các biến sau chỉ phục vụ render test:

```bash
CPU_DIT_UI_COLS=160 CPU_DIT_UI_ROWS=45 \
  image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh --render-running-once
```

## Regression test

```bash
make test-ui
make test
```

`tools/test_ui.sh` cấp một progress event mẫu rồi kiểm cả dashboard 160x45 và
compact 100x30. Test xác nhận các vùng phần cứng, utilization, process, phase,
exact-test identity và temperature status đều có mặt. Regression hai frame còn
bắt buộc chỉ có đúng một full-screen clear. Boot/QEMU regression vẫn phải chạy
lại sau khi đóng gói dashboard vào SquashFS.
