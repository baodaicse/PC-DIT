# DIT CPU–RAM (CPU-DIT-01)

> Từ 2026-09-06, tài liệu này thuộc module trong project chung DIT-01. Dùng [README chung](../../README.md) và [status chung](../STATUS.md) cho lệnh build/trạng thái hợp nhất; các mốc bên dưới giữ lịch sử module.

CPU-DIT-01 là dự án Data Integrity Test độc lập, giai đoạn đầu tập trung kiểm
tra CPU, cache, memory controller và RAM trong môi trường USB boot Linux minimal.
Tên sản phẩm tạm thời được chốt là **DIT CPU–RAM**; các bài CPU–SSD/NVMe sẽ thuộc
project riêng, không được thêm vào engine ghi storage của repository này.

Mục tiêu là tạo một chuỗi kiểm tra xác định:

```text
source verify → split → matrix encode → scatter RAM → compute/readback
              → exact verify → matrix decode → join → golden result
```

Nếu CPU chia/join/tính toán sai, RAM làm thay đổi dữ liệu, block bị nhầm vị trí,
worker lỗi hoặc hardware event vi phạm policy thì phiên không được PASS.

## Tài liệu

- [Đặc tả sản phẩm](PRODUCT_SPEC.md)
- [Định danh và ranh giới project DIT CPU–RAM](PROJECT_IDENTITY.md)
- [Kiến trúc kỹ thuật](ARCHITECTURE.md)
- [Roadmap](ROADMAP.md)
- [Trạng thái triển khai](STATUS.md)
- [Kế hoạch triển khai](IMPLEMENTATION_PLAN.md)
- [Kiểm thử và tiêu chí nghiệm thu](TEST_AND_ACCEPTANCE.md)
- [Reference pipeline v1](REFERENCE_PIPELINE_V1.md)
- [System pipeline v1 — RAM đa arena và đa core](SYSTEM_PIPELINE_V1.md)
- [Kết quả kiểm thử thiết bị 2026-08-28](DEVICE_TEST_2026-08-28.md)
- [Telemetry, safety và diagnostic v1](TELEMETRY_SAFETY_V1.md)
- [Kết quả kiểm thử Bước 4 ngày 2026-08-29](DEVICE_TEST_2026-08-29.md)
- [Ubuntu live image v1 và cách ghi USB](BOOT_IMAGE_V1.md)
- [Tạo image cho Rufus bằng Makefile](MAKE_RUFUS_IMAGE.md)
- [Ghi và xác minh image trực tiếp vào USB trên Linux](BURN_USB.md)
- [Kết quả kiểm thử boot image 2026-08-29](BOOT_IMAGE_TEST_2026-08-29.md)
- [UI, sensor và driver v1](UI_SENSORS_DRIVERS_V1.md)
- [Duration, RAM coverage 95% và live UI v2](DURATION_COVERAGE_UI_V2.md)
- [Live test dashboard v4](UI_DASHBOARD_V3.md)
- [Session và round seed rotation v1](SEED_ROTATION_V1.md)
- [Temperature Policy v2 — toggle mặc định OFF](TEMPERATURE_POLICY_V2.md)
- [Kế hoạch Pattern/Window Coverage v1](PATTERN_WINDOW_COVERAGE_PLAN.md)
- [Kết quả bounded device Bước 3A](DEVICE_STEP3A_TEST_2026-08-29.md)
- [Kiểm thử UI/sensor trên thiết bị 2026-08-29](DEVICE_TEST_2026-08-29_UI_SENSOR.md)
- [Hướng dẫn validation đầy đủ](VALIDATION_GUIDE.md)
- [Trạng thái và ma trận validation](validation/VALIDATION_STATUS.md)
- [Các quyết định kỹ thuật](decisions/README.md)

## Trạng thái

Bước 1–5 gồm cả Bước 3A đã `implemented`: reference pipeline, adaptive RAM,
multi-core/SIMD, pattern/window recipe,
telemetry, safety/diagnostic và Ubuntu Noble minimal live image. Image đã PASS
boot-smoke BIOS + UEFI bằng QEMU; live dashboard, sensor CPU/RAM và CPU package
power đã được kiểm trên thiết bị phát triển. Đây là implementation evidence, chưa phải
`validated` trên hardware matrix.

## USB image hiện tại

Artifact có thể ghi trực tiếp bằng Rufus/DD:

```text
dist/dit-01-noble-amd64.img
SHA-256: 115c3afe2318285ef633827e725f33cbeb3916b72f8bc537b1a4d69916205395
```

USB tối thiểu 4 GB. Ghi toàn bộ file `.img` ở chế độ raw/DD, tắt Secure Boot,
boot USB. Dashboard khi test sẽ hiện phần cứng, live CPU/RAM, phase, round, target,
nhiệt độ và Watt nếu kernel cung cấp sensor;
Safe tự chạy sau 10 giây nếu không bấm phím và giữ tải ít nhất 2 phút. Full mặc
định chạy ít nhất 30 phút, dùng 95% vùng allocatable còn lại sau reserve 512 MiB.
Hướng dẫn đầy đủ nằm tại [BOOT_IMAGE_V1.md](BOOT_IMAGE_V1.md).

Tự build lại image bằng một lệnh:

```bash
make rufus-image
```

Pipeline này cũng là chủ sở hữu duy nhất của image dùng chung. Khi cần thêm
module CPU–NVMe, chạy từ project module (không sao chép rootfs/GRUB sang đó):

```bash
cd /home/hoang/DIT-01
make shared-image
```

Image builder xác minh extension manifest/SHA-256 trước khi stage module. Không
có extension, `make rufus-image` vẫn tạo image CPU–RAM như trước.

Xem hướng dẫn môi trường build, kiểm checksum và Rufus tại
[MAKE_RUFUS_IMAGE.md](MAKE_RUFUS_IMAGE.md).

Chạy image trực tiếp trong cửa sổ QEMU trên máy phát triển:

```bash
make run-image
```

Nguồn chuẩn không cần có dung lượng bằng RAM. Recipe xác định mở rộng source bằng
seed, block index và global offset; engine tự thích nghi với RAM 16/32/64/128 GB
hoặc lớn hơn và báo `allocatable RAM coverage` thực tế thay vì tuyên bố 100% RAM.

## Chạy reference pipeline

```bash
cargo test --workspace
cargo run --release -p dit-cli -- run
```

Kết quả development được ghi vào `build/reference-report.json`. Reference v1 chỉ
dùng khoảng 1 MiB payload để kiểm chứng data route; đây chưa phải full-memory DIT.

## Chạy adaptive RAM và CPU pipeline

Ví dụ an toàn mặc định dùng 256 MiB, giữ lại 512 MiB cho hệ điều hành và tự chọn
mọi CPU được cpuset cho phép:

```bash
cargo run --release -p dit-cli -- system-run
```

Target có thể đặt theo dung lượng hoặc phần trăm RAM khả dụng tại đầu phiên:

```bash
target/release/dit-cli system-run \
  --available-percent 50 --reserve-mib 512 \
  --arena-mib 32 --rounds 4 --workers 0
```

Engine từ chối target vượt `MemAvailable - reserve`; vì vậy không dùng tùy chọn
này để cố lấy 100% physical RAM trong Linux. Xem các trường coverage và giới hạn
chính xác tại [SYSTEM_PIPELINE_V1.md](SYSTEM_PIPELINE_V1.md).

Safety policy mặc định dùng deadline 300 giây, monitor 250 ms, ngưỡng 95°C và
không cho PASS khi có uncorrected EDAC, kernel hardware event hoặc cgroup OOM.
Sensor không có sẽ được ghi `unavailable`; có thể bắt buộc bằng
`--require-temperature-sensor` hoặc `--require-edac-counters`.

Xem sensor/driver trực tiếp trên Ubuntu phát triển:

```bash
sudo target/release/dit-cli sensors --interval-ms 500
sudo target/release/dit-cli sensors --json --interval-ms 500
```

CPU Watt ưu tiên hwmon `power*_input`/`power*_average`, sau đó mới tính từ chênh
lệch counter `powercap/*/energy_uj`. Nếu cả hai không khả dụng hoặc không đọc
được, kết quả đúng là `N/A`, không phải 0 W.
