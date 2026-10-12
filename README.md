# DIT-01 — CPU–RAM, CPU–SSD và GPU–VRAM

Project chung từ ngày 2026-09-06 tại `/home/hoang/DIT-01`: một Cargo workspace, một Cargo.lock, một Makefile và một pipeline Ubuntu Minimal image. Không cần build trong hai project cũ; không liên quan HardwareValidator.

```bash
cd /home/hoang/DIT-01
make test                  # phần mềm các module, không image/QEMU
make release               # CPU-RAM + CPU-SSD + GPU-VRAM + control center
make gpu-a-check           # regression contract/mock GPU-A
make gpu-check             # GPU-A..D + Vulkan smoke giới hạn trên host
make gpu-ef-check          # GPU-E/F UI/multi/extension gates, không build image/QEMU
make preview-menu          # chỉ xem menu, không bắt đầu test
make preview-test-ui SSDS=2 FRAMES=80
make shared-image          # alias: make rufus-image
make burn-usb              # chọn USB, xác nhận xóa/ghi/verify
```

Image mới: `dist/dit-01-noble-amd64.img`, kèm `.sha256` và `.manifest.json`. Rufus dùng DD/raw mode. Burn sẽ xóa USB được chọn sau xác nhận. Không tự burn/QEMU trong `make test` hoặc `make shared-image`.

Menu USB chọn `CPU + RAM`, `CPU + SSD`, `GPU + VRAM` hoặc `[4] CPU AVAIL / POWER`. Mục 4 mở profile chuyển tải đồng bộ và core/SMT sweep với availability/power evidence riêng; xem [hướng dẫn](docs/cpu-ram/AVAILABILITY_POWER_PLATFORM.md). CPU–SSD dùng NVMe Deep mặc định: block matrix 4 KiB, CPU oracle độc lập, bounded compute workers và read-soak đến hết duration sau khi dùng xong write budget. GPU–VRAM mặc định dùng Vulkan hardware: cấp phát VRAM theo budget, upload/readback toàn byte, retention readback trễ và matrix 8×8 `u32` chạy trên GPU rồi so full output với CPU oracle. `--backend mock` chỉ dành cho regression GPU-A và luôn ghi `hardware_tested=false`.

Khi testing, các module dùng một giao diện thống nhất từ `dit-ui-core`: hardware, live utilization, progress, calculations, FAIL và footer giữ nguyên; chỉ `ACTIVE TEST`, `PROCESS` và running counters thay theo CPU‑RAM/NVMe/GPU. Thiết kế combined CPU‑RAM‑NVMe‑GPU và các bước resource arbiter/round-robin/coupled route nằm tại [Unified dashboard và combined pipeline](docs/UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md).

GPU Deep mặc định 1.800 giây, temp limit OFF, stop-on-error OFF, allocation target 85% phần budget hiện có; người vận hành có thể đặt `--vram-percent 95` trên GPU headless hoặc cấu hình đã xác nhận. Giới hạn 95% là target của budget có thể cấp phát, không phải cam kết chiếm 95% tổng VRAM trên mọi driver/GPU đang xuất hình. Smoke phát triển dùng `--vram-mib 64`, không tự chạy near-full.

Trong CPU–SSD, file test dung lượng lớn chỉ được tạo trên filesystem thuộc đúng SSD đã chọn. Selection/telemetry đang chạy nằm ở `/run/dit-suite` (tmpfs/RAM), không ghi liên tục vào USB boot. Khi engine đã đóng file và report, một bundle kết quả mới được copy một lần sang phân vùng `DIT-RESULTS` đã xác thực; nếu phân vùng đó không hợp lệ, kết quả ở lại tmpfs và không fallback sang thiết bị khác.

Dashboard ghi rõ `RAM USAGE` cho số liệu kiểu `Used / Total`; đây không phải dung lượng ổ đĩa. Mỗi hàng SSD hiển thị capacity namespace riêng (ví dụ khoảng `953.87 GiB` cho ổ 1 TB), tốc độ R/W và tiến độ. Event journal trên tmpfs được downsample 500 ms theo từng loại event/SSD để tránh log tăng theo mọi I/O; failure/completion luôn được giữ.

CPU‑SSD UI dùng chung nguồn sensor với CPU‑RAM: xung hiện tại từ Linux cpufreq (fallback `/proc/cpuinfo`), nhiệt CPU từ hwmon/thermal và package power từ hwmon/powercap. Sensor CPU được refresh mỗi giây, còn CPU usage/RAM mỗi 250 ms. Chỉ field thật sự không được kernel/BIOS cung cấp mới hiện `N/A`.

GPU inventory cũng dùng chung: `lspci/sysfs` cung cấp model, PCI address, kernel driver đang bind, module candidates và VRAM khi driver expose số liệu chuẩn. Dashboard GPU bản wide dùng đúng bố cục CPU‑RAM: SYSTEM HARDWARE và LIVE UTILIZATION ở hai khung trên, TESTING/PROCESS INFORMATION ở khung dưới; mỗi GPU bổ sung hàng GPU/VRAM usage và sensor tương ứng. CPU temp/package power cũng dùng cùng collector với CPU‑RAM.

Image generic chứa open kernel stack `i915`, `xe`, `amdgpu`, `amdkfd`, `radeon`, `nouveau`, `linux-firmware` cùng libdrm/Mesa/Vulkan userspace. Với NVIDIA, `nouveau` có thể chạy Vulkan nhưng thường không xuất đủ usage/temp/watt/clock trên GPU mới; UI sẽ ghi rõ source và giữ N/A, không dựng số giả. Muốn đầy đủ telemetry NVIDIA, build flavor riêng với đúng package có trong Ubuntu Noble:

```bash
make nvidia-image NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open
```

Artifact là `dist/dit-01-noble-amd64-nvidia.img`. Flavor này có rootfs/cache riêng, blacklist `nouveau`, probe duy nhất module NVIDIA và ghi package/profile vào build-info + image manifest; module NVIDIA là bản Canonical đã ký nên boot được khi Secure Boot bật (xem `docs/SECURE_BOOT.md`). Thay `NNN` bằng nhánh driver đã chọn cho GPU/hardware matrix, rồi validation lại image/driver cụ thể.

```text
DIT-01/
  Cargo.toml, Cargo.lock, Makefile
  crates/             19 crates: CPU-RAM, CPU-SSD, GPU engine/CUDA/Vulkan, UI, control center và watchdog
  image/              một rootfs/boot config + module payload
  tools/              build, test, preview, burn USB
  vectors/            source/golden CPU–RAM giữ nguyên
  docs/               roadmap/status/migration chung
    cpu-ram/          tài liệu và lịch sử CPU–RAM
    cpu-nvme/         tài liệu và lịch sử CPU–NVMe
    gpu-vram/         roadmap/checklist GPU-A..GPU-F; source implemented
  validation/         hồ sơ validation cũ
  Result/             bản sao kết quả CPU–RAM cũ
  build/boot-image/   bản sao độc lập cache Ubuntu Minimal
  dist/               artifact mới
```

[Roadmap chung](docs/ROADMAP.md) · [Trạng thái](docs/STATUS.md) · [Deep pipeline](docs/NVME_DEEP_PIPELINE.md) · [Hồ sơ gộp](docs/MIGRATION.md).

[Roadmap GPU–VRAM–DIT](docs/gpu-vram/ROADMAP.md) chia sáu task triển khai trên nền UI/image hiện có. GPU-A..F đã implemented: lựa chọn số 3 mở picker GPU, dùng UI chung, hỗ trợ worker/report nhiều GPU và đóng vào extension chung. Hardware multi-GPU, image USB và Deep 30/60 phút vẫn phải theo [checklist validation](docs/gpu-vram/VALIDATION.md).

Hai thư mục cũ giữ làm bản đối chiếu, không tự xóa. Từ nay sửa code/build/docs trong DIT-01. Không đổi tên image cũ rồi coi là bản hợp nhất; cần build image mới để nhận binary/manifest mới.
