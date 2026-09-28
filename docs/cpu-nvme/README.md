# DIT CPU–NVMe

> Từ 2026-09-06, tài liệu này thuộc module trong project chung DIT-01. Dùng [README chung](../../README.md) và [status chung](../STATUS.md) cho lệnh build/trạng thái hợp nhất; các mốc bên dưới giữ lịch sử module.

DIT CPU–NVMe là dự án kiểm tra đường dữ liệu từ CPU/RAM, PCIe, bộ điều khiển NVMe đến media lưu trữ bằng dữ liệu có thể kiểm chứng. Dự án chạy độc lập với DIT CPU–RAM nhưng dùng một hợp đồng module mở để về sau ghép thành bộ kiểm tra CPU–RAM–SSD–VGA.

## Mục tiêu

- Phát hiện sai dữ liệu, lỗi I/O, timeout/reset, lỗi PCIe AER, SMART bất thường, quá nhiệt và thiết bị biến mất.
- Hỗ trợ một hoặc nhiều SSD NVMe; người vận hành có thể chọn từng thiết bị hoặc chọn tất cả.
- Hiển thị tốc độ và kết quả riêng cho từng SSD trên giao diện động.
- Dùng block kiểm chứng 4 KiB làm nền tảng, kết hợp I/O 4 KiB, 64 KiB, 128 KiB và 1 MiB để phủ nhiều đường xử lý.
- Quản lý ngân sách ghi để hạn chế TBW và không bao giờ tự ý ghi vào ổ hệ thống.

## Ranh giới dự án

- Thư mục dự án: `/home/hoang/DIT-01`.
- Không phụ thuộc runtime và không sửa mã trong `HardwareValidator`.
- Engine NVMe độc lập với engine CPU–RAM; riêng boot USB tái sử dụng pipeline image tại `/home/hoang/DIT-01` qua extension hook, không tạo image builder thứ hai.
- Giao diện kế thừa ngôn ngữ hiển thị của DIT CPU–RAM, không sao chép phụ thuộc từ HardwareValidator.
- Bản đầu chỉ hỗ trợ NVMe. SATA SSD cần backend và tiêu chí riêng nếu bổ sung sau.

## Tài liệu

- [Đặc tả sản phẩm](PRODUCT_SPEC.md)
- [Kiến trúc](ARCHITECTURE.md)
- [Hợp đồng ghép module](MODULE_COMPOSITION.md)
- [UI nhiều thiết bị](UI_DYNAMIC_MULTI_DEVICE.md)
- [Chính sách an toàn lưu trữ](STORAGE_SAFETY_POLICY.md)
- [Bảo toàn dữ liệu khi kiểm tra SSD](DATA_PRESERVATION.md)
- [Kế hoạch phát triển RAW BOUNDED WRITE](RAW_BOUNDED_WRITE_PLAN.md)
- [Tích hợp image DIT dùng chung](SHARED_IMAGE_INTEGRATION.md)
- [Roadmap](ROADMAP.md)
- [Kiểm thử và tiêu chí nghiệm thu](TEST_AND_ACCEPTANCE.md)
- [Trạng thái](STATUS.md)
- [Kết quả triển khai TASK-A](TASK_A_IMPLEMENTATION.md)
- [Kết quả triển khai TASK-B](TASK_B_IMPLEMENTATION.md)
- [Kết quả triển khai TASK-C](TASK_C_IMPLEMENTATION.md)
- [Kết quả triển khai TASK-D](TASK_D_IMPLEMENTATION.md)
- [Kết quả triển khai TASK-F](TASK_F_IMPLEMENTATION.md)
- [Tự chuẩn bị thư mục/file test trên SSD đã chọn](AUTO_STORAGE.md) — không cần nhập mount path, không format/chia partition.
- Xem thử UI test kiểu CPU–RAM, không chạy engine/ghi SSD: `make preview-test-ui SSDS=2 FRAMES=80`.

## Trạng thái hiện tại

TASK-A đến TASK-D và TASK-F đã `implemented`. TASK-F thêm control center chọn `CPU + RAM` hoặc `CPU + SSD`, module registry/orchestrator, aggregate report và extension có checksum cho image dùng chung. CPU–SSD mở thẳng picker; chọn SSD là chạy ngay, không CPU–RAM minimum preflight hoặc confirmation thứ hai. TASK-E còn các chiến dịch image/hardware validation M9–M10; chưa hardware-validated trên hai NVMe thật.

Việc triển khai được giao theo sáu task chủ đạo `TASK-A` đến `TASK-F`; mỗi task gom các mốc code, kiểm thử và cập nhật tài liệu có liên quan. Xem [Roadmap](ROADMAP.md) để biết đầu ra và gate của từng task.

Kiểm tra TASK-A bằng một lệnh:

```bash
cd /home/hoang/DIT-01
make task-a-check
make task-b-check
make task-c-check
make task-d-check
make task-f-check
```

Tạo extension hoặc chủ động gọi pipeline image CPU–RAM dùng chung:

```bash
make extension
make preview-menu
make shared-image
```
