# Project identity — DIT CPU–RAM

## Tên và phạm vi hiện tại

- Tên sản phẩm làm việc: **DIT CPU–RAM**.
- Mã project/repository hiện tại: **CPU-DIT-01**.
- Phạm vi: CPU core/cache/SIMD/compute oracle, memory controller, RAM allocatable,
  seed/pattern/window rotation, telemetry, safety và report từ USB boot.

Project này không chứa bài ghi/xóa SSD/NVMe. Driver GPU/storage trong live image
chỉ phục vụ boot, inventory và telemetry; chúng không biến CPU-DIT-01 thành bài
DIT storage.

## Ranh giới với project tiếp theo

DIT CPU–SSD/NVMe sẽ được tạo ở folder và bộ docs riêng. Nó có thể tái sử dụng
định dạng source, seed commitment, report status và UI conventions, nhưng không
dùng chung engine ghi storage với DIT CPU–RAM. Ranh giới này tránh việc một bài
test storage có tính phá hủy vô tình chạy trong profile CPU–RAM.
