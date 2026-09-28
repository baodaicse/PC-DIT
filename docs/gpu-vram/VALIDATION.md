# Checklist validation GPU–VRAM–DIT

Tài liệu này dùng cho GPU-F trên image chung CPU–RAM–SSD–GPU. Mỗi cấu hình thực phải có một bản sao checklist đã điền và đường dẫn report/log. Không đánh dấu `validated` chỉ từ unit test, mock, llvmpipe hoặc host bounded smoke.

## 1. Nhận diện artifact và máy

- Ngày/giờ, người chạy, mã máy:
- Image file và SHA-256:
- Extension manifest SHA-256:
- Kernel/Ubuntu release:
- Secure Boot/CSM/UEFI:
- GPU model, vendor/device ID:
- Vulkan UUID và PCI BDF:
- Kernel driver, Vulkan ICD và driver version:
- GPU driver profile/package trong `/etc/cpu-dit/build-info`:
- GPU đang xuất hình hay GPU phụ:
- VRAM dedicated hoặc RAM shared; heap/budget/usage lúc bắt đầu:
- Shader SHA-256: `7f7e90c20ded5239f060d7751f9ee844b9b6b48e9b08780551434bca6b811182`

## 2. Preflight

- [ ] Boot image vật lý hoàn tất và Control Center hiện CPU + RAM, CPU + SSD, GPU + VRAM.
- [ ] Picker hiện đúng model/BDF/UUID/driver; software renderer mang trạng thái BLOCKED.
- [ ] GPU được chọn trùng stable identity trong report.
- [ ] UI có CPU/RAM, GPU usage, VRAM usage/test allocation, transfer speed, seed, progress và FAIL TOTAL.
- [ ] Sensor thiếu hiển thị N/A, không bị tính là lỗi integrity.
- [ ] Dòng sensor source đúng provider. NVIDIA cần `nvidia-smi + sysfs fallback` để kỳ vọng đủ usage/temp/watt/clock; `nouveau; driver exposes no live counters` là thiếu capability có giải thích, không phải số 0.
- [ ] Temp limit mặc định OFF; thử ON riêng nếu cần xác nhận thermal abort.
- [ ] Một hoặc nhiều iGPU không tự target tổng hơn 60% heap chia sẻ; dGPU không vượt target/budget báo cáo.

## 3. Positive campaign

- [ ] Bounded smoke: transfer + full compare + integer matrix + CPU oracle + retention PASS.
- [ ] Deep 30 phút chạy đủ deadline, khoảng 10 seed quantum (số thực tế ghi từ report).
- [ ] Deep 60 phút chạy đủ deadline, khoảng 20 seed quantum.
- [ ] Allocation thực tế, logical bytes verified, cumulative traffic và sweep được ghi riêng.
- [ ] UI còn phản hồi; Ctrl+C tạo ABORTED và giữ report.
- [ ] Không có Vulkan validation error trong lượt debug tương ứng.

Ghi kết quả:

- Duration target/actual:
- Base seed, seeds started, generations:
- Allocation target percent/bytes và actual bytes:
- Transfer operations/throughput/cumulative bytes:
- Matrix dispatches/results verified:
- Retention checks/allocation sweeps/logical bytes verified:
- Max temp/power/clock và sensor source:
- Failure count/verdict/completion reason:
- Report, worker logs và result bundle:

## 4. Negative và phục hồi

- [ ] Transfer corruption tạo FAIL đúng phase và không ghi hardware PASS.
- [ ] Matrix output corruption tạo FAIL đúng phase.
- [ ] Software/unsupported Vulkan device tạo INVALID.
- [ ] Allocation/capability failure tạo INVALID, không kết luận VRAM hỏng.
- [ ] Ctrl+C tạo ABORTED; report cuối vẫn đọc được.
- [ ] Với nhiều GPU, lỗi/mất report của một worker không xóa report GPU khác; aggregate giữ đúng device attribution.

## 5. Multi-GPU (nếu có)

- [ ] Chọn `1,2` và `A` đúng device.
- [ ] Mỗi GPU có hàng usage/VRAM/speed/progress riêng; paging/resize không phá layout.
- [ ] UUID/BDF/telemetry không bị gán chéo.
- [ ] Budget RAM staging/VRAM không gây OOM hoặc làm UI treo.
- [ ] Chạy đồng thời 30 phút và lưu aggregate cùng mọi per-device report.

## 6. Kết luận cấu hình

- Trạng thái: `PASS / FAIL / INVALID / ABORTED / PENDING`
- Phạm vi được validated:
- Các giới hạn/pending:
- Người duyệt và ngày:

Chỉ PASS các mục đã chạy thật. GPU/driver/image khác hoặc thời lượng chưa chạy vẫn là `pending`.
