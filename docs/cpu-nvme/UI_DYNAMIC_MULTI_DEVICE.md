# UI động cho nhiều SSD

Trạng thái: `implemented` ngày 2026-09-05 trong crate `dit-ui-core`; automated layout/render gates đạt, hardware/PTY matrix thực tế chưa validated.

## Ngôn ngữ giao diện

Giữ phong cách DIT CPU–RAM hiện tại: nền đen, khung cyan, text trạng thái có màu, header sản phẩm, khu vực phần cứng/telemetry và khu vực tiến trình. Đây là cùng thiết kế hiển thị, không phải phụ thuộc vào source của HardwareValidator.

UI là event-driven và render in-place. Không xóa toàn màn hình mỗi frame. Mục tiêu 2–4 FPS; tốc độ I/O được sample 250–500 ms, sensor/SMART khoảng 1 Hz.

Phần `SYSTEM HARDWARE` và `LIVE UTILIZATION` đã dùng lại ngôn ngữ bố cục DIT CPU–RAM. CPU model, tổng RAM và board đọc từ procfs/sysfs; CPU/RAM usage được cập nhật khoảng 250 ms trong lúc UI hoạt động.

## Màn hình chọn thiết bị

Mỗi dòng hiển thị:

```text
[x] 1  /dev/nvme0n1  Samsung ...  1.0 TB  SN...  0000:01:00.0  Gen4 x4
[ ] 2  /dev/nvme1n1  WD ...       2.0 TB  SN...  0000:04:00.0  Gen4 x4
```

Phím `1`, `2`, ... toggle thiết bị; `A` chọn/bỏ tất cả; Enter xác nhận. Con số chỉ là alias của phiên hiện tại. Plan lưu stable ID đầy đủ và được kiểm tra lại ngay trước khi mở thiết bị.

Ngay cả khi chỉ có một SSD, mọi chế độ ghi vẫn yêu cầu xác nhận target/mode; không auto-start destructive test.

## Layout khi chạy

```text
+ DIT CPU-NVMe -------------------------------------------------------------+
| SYSTEM HARDWARE                 | LIVE UTILIZATION                        |
| CPU/RAM/BOARD                   | CPU / RAM / temperatures                |
+-------------------------------------------------------------------------+
| TESTING / PROCESS INFORMATION                                           |
| STATE ...  PROGRESS ...  TIME ...  OPERATIONS ...  FAIL ...             |
|                                                                         |
| SSD 1  model / stable short ID / PCIe Gen4 x4                           |
| SPEED [###############--------] 3.21 GiB/s  R 3.21  W 0  AVG/PEAK ...    |
| VERIFIED ...  IOPS ...  TEMP ...  FAIL ...  PHASE ...                   |
|                                                                         |
| SSD 2  model / stable short ID / PCIe Gen4 x4                           |
| SPEED [########---------------] 1.74 GiB/s  R 1.20  W .54 AVG/PEAK ...   |
| VERIFIED ...  IOPS ...  TEMP ...  FAIL ...  PHASE ...                   |
+-------------------------------------------------------------------------+
```

## Chiều cao động

Không dùng tọa độ/hàng cố định theo hai SSD. Layout nhận `terminal_rows`, `terminal_cols`, số module và danh sách thiết bị được chọn.

```text
testing_rows = fixed_summary_rows
             + selected_device_count * expanded_rows_per_device
             + spacing_rows
```

Nếu đủ chiều cao, mỗi SSD dùng 3–4 dòng. Nếu thiếu:

1. chuyển từng SSD sang compact row;
2. ẩn trường ít quan trọng nhưng không ẩn FAIL/verdict/identity;
3. phân trang/scroll danh sách thiết bị;
4. giữ header và footer nhỏ nhất.

Resize terminal phải tính lại layout, không panic vì biến/hàng chưa khởi tạo. Snapshot tests bao phủ 1, 2, 4 SSD và terminal hẹp/thấp.

## Thanh tốc độ

Mỗi SSD đã chọn có một thanh độc lập và luôn có số tuyệt đối MiB/s hoặc GiB/s. Màu gợi ý:

- cyan/green: hoạt động bình thường;
- yellow: throttling/cảnh báo nhiệt/link;
- red: lỗi hoặc fatal state;
- gray: idle/chưa có sample.

Độ đầy thanh có thể chuẩn hóa theo ceiling đo/calibrate hoặc giới hạn hiệu dụng của PCIe đã negotiated; nhãn phải cho biết cơ sở. Thanh không phải tiêu chí PASS/FAIL vì ổ chậm vẫn có thể toàn vẹn. Hiển thị `R`, `W`, `AVG`, `PEAK` giúp tránh hiểu sai khi recipe đổi phase.

## Nhiều module về sau

UI dựng section từ event/module descriptors. CPU–RAM, NVMe và VGA đăng ký widget/metrics của mình; layout engine sắp xếp section theo không gian hiện có. Không thêm nhánh hard-code kiểu “nếu SSD 2 thì vẽ dòng 27”.
