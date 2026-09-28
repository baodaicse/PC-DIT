# GPU-E/F — UI nhiều GPU và tích hợp image chung

Cập nhật: 2026-09-07. Trạng thái source/software: `implemented`. Hardware matrix ngoài GTX 960 và boot USB thật: `pending`.

## GPU-E

- GPU dùng chung Control Center và `dit-ui-core` của image CPU–RAM–SSD. Picker có cùng khung, nhận alias `1,2` hoặc `A`, hiện model, Vulkan index, BDF, UUID, driver, local memory và trạng thái READY/BLOCKED.
- Software Vulkan renderer như llvmpipe bị chặn. `--device-index` chỉ định một device bị chặn vẫn đi qua đường single để tạo report `INVALID`, không tạo hardware PASS.
- Một GPU giữ dashboard chuẩn. Nhiều GPU chạy thành process worker độc lập, mỗi worker có progress/report/log riêng; parent tạo aggregate report và không làm mất kết quả GPU còn lại khi một worker lỗi. Final USB result bundle giữ cả aggregate và ba artifact của từng `device-N`.
- Dashboard một GPU bản wide dùng cùng geometry CPU‑RAM: hai khung SYSTEM HARDWARE/LIVE UTILIZATION phía trên và khung TESTING/PROCESS INFORMATION phía dưới. CPU frequency/temp/package power lấy cùng `LiveSensorSampler` của CPU‑RAM. Nhiều GPU dùng cùng renderer in-place và năm hàng động mỗi GPU: identity/state, GPU usage, VRAM usage, allocation/chunk/transfer và seed/sweep/verified/matrix/fail/sensor. Console thấp tự phân trang 5 giây/lần.
- Parent render 8 FPS. Inventory/driver probe chỉ chạy một lần ngoài render loop; progress JSON được thay atomically để tránh frame rách và giảm chớp.
- NVIDIA provider lấy GPU/VRAM utilization, used/total VRAM, temperature, power, graphics clock và memory clock trong một query `nvidia-smi`; từng field thiếu được fallback sang DRM/sysfs/hwmon. AMD/Intel đọc busy, VRAM, hwmon và DPM/frequency sysfs khi driver expose. Nếu không có live counter, source ghi rõ nguyên nhân thay vì chỉ hiện `sysfs` chung chung.
- dGPU dùng target `--vram-percent` tối đa 95% budget khả dụng. iGPU mặc định bị chặn ở tổng 60% vì device-local heap thường là RAM chia sẻ; nếu chọn nhiều iGPU, parent chia mức 60% này cho số iGPU worker. `--vram-mib` là override giới hạn tường minh của operator.
- Temp limit mặc định OFF. Ctrl+C cho worker thời gian drain retention/final report; sau timeout parent kết thúc worker treo và aggregate thành INVALID nếu thiếu report.

## GPU-F

- `gpu-vram-dit`, shader source/SPIR-V, roadmap, status, validation checklist, Cargo.lock và SBOM được đóng trong extension/image DIT-01 đang dùng cho CPU–RAM–SSD.
- Manifest ghi module `dit.vga`, backend Vulkan, pipeline GPU-F, shader hash và checksum từng binary. Image tiếp tục dùng Ubuntu Minimal Noble, firmware/Mesa/Vulkan chung; không tạo image GPU riêng.
- Image generic vẫn dùng open driver. Builder hỗ trợ flavor NVIDIA tách biệt qua `NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open`; rootfs/artifact riêng tránh làm nhiễm cache generic, blacklist nouveau, đóng kernel headers/driver/utils và ghi profile/package vào manifest. Đây là khả năng build, chưa phải xác nhận mọi nhánh driver hoặc Secure Boot.
- 2026-09-22: thêm đường thay thế không cần build image riêng — gói driver NVIDIA cài lúc boot trên image generic (`make nvidia-driver-package` + `make nvidia-driver-burn-usb`), xem [NVIDIA_DRIVER_PACKAGE.md](NVIDIA_DRIVER_PACKAGE.md). `nvidia-image` vẫn giữ nguyên song song; boot thật đầu tiên trên RTX 5060 lộ ra race `nouveau`; từ 2026-09-23 `nouveau` bị tắt hẳn trên mọi flavor (kernel cmdline + modprobe.d + initramfs), nên card NVIDIA chỉ chạy bằng driver NVIDIA (gói hoặc `nvidia-image`); chưa xác nhận lại trên phần cứng.
- `make gpu-ef-check` thực hiện source gates, inventory/software rejection, bounded hardware positive/negative nếu host có GPU Vulkan thật, multi-selection path, extension manifest/checksum và boot policy. Không build image và không chạy QEMU.
- Hardware support chỉ được công bố theo GPU/driver/ICD/image đã có hồ sơ trong [VALIDATION.md](VALIDATION.md). `implemented` không đồng nghĩa mọi NVIDIA/AMD/Intel hoặc multi-GPU đã `validated`.

## CLI vận hành

```text
gpu-vram-dit run                         # TTY: mở picker; non-TTY: GPU thật đầu tiên
gpu-vram-dit run --devices all           # tất cả GPU READY
gpu-vram-dit run --devices 0,2           # Vulkan indices cụ thể
gpu-vram-dit run --device-index 0        # tương thích single-GPU
gpu-vram-dit run --vram-mib 256          # smoke có giới hạn tường minh
```

Deep mặc định vẫn chạy 1.800 giây, xoay seed mỗi 180 giây và không kết thúc chỉ vì chạy hết một task. Các gate phát triển dùng `--generations` và `--vram-mib` nhỏ để không biến kiểm tra source thành stress dài.

Engine luôn verify record retention đang nằm trên vùng sắp bị ring allocator ghi đè trước khi thực hiện write mới. Việc này ngăn một lượt duration/chunk nhỏ tạo false FAIL vì chính scheduler overwrite dữ liệu vẫn còn chờ delayed readback.
