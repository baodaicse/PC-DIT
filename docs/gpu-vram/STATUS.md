# Trạng thái GPU–VRAM–DIT

Bài test làm gì, đọc dashboard thế nào, PASS chứng minh đến đâu: [GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md](GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md).

Cập nhật: 2026-09-20 (G1/G2/G3 và CUDA sequential; giữ evidence baseline bên dưới).

## GPU-G — Resident compute/CUDA, 2026-09-19

Roadmap kế tiếp: [GPU-H](GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md) (bản 2, 2026-09-20),
trạng thái `partial`. Source đã có adapter CUDA burst (`--async-compute`, pipeline
`GPU-H-CUDA-1`), ledger slot/epoch và controller v0. **H1 `implemented`** 2026-09-20
([GPU_H1_IMPLEMENTATION.md](GPU_H1_IMPLEMENTATION.md)): report cộng dồn, bảo toàn counters,
`detection` (link/AER, log kernel, stress, scorecard), `make gpu-h1-check` PASS và baseline
GTX 960 tại `build/gpu-h1-baseline-20260920/`. **H2 `implemented` (CUDA)** 2026-09-20
([GPU_H2_IMPLEMENTATION.md](GPU_H2_IMPLEMENTATION.md)): verifier ≈6× nhanh hơn, CPU core-s/GiB verified
giảm ≈89%, hash policy, corpus pool xoay, kiểm tra toàn vẹn corpus, pattern hệ thống; `make gpu-h2-check`
PASS, evidence `build/gpu-h2-ab-20260920-185044/`. **H3 `implemented` (CUDA)** 2026-09-20
([GPU_H3_IMPLEMENTATION.md](GPU_H3_IMPLEMENTATION.md)): slot device/host tách vòng đời, `QUARANTINE`, ngân sách async
và thang backoff, rollback cấp phát lỗi tại mọi bước, cờ `--async-*`, chia CPU theo số GPU; `make gpu-h3-check` PASS,
evidence `build/gpu-h3-slots-20260920/`. **H5 `implemented` (CUDA)** 2026-09-20
([GPU_H5_IMPLEMENTATION.md](GPU_H5_IMPLEMENTATION.md)): controller v1 (backlog đang chờ, pacing, hysteresis, áp lực bộ nhớ),
regime label, worker verifier thích ứng, coverage floor mặc định 0,25 (độ phủ ×5, compute −25%; ≥80% độ phủ tuần tự cần floor
≈0,5 với compute ≈1,1× tuần tự); `make gpu-h5-check` PASS, evidence `build/gpu-h5-control-20260920/`. **H6 `implemented` (CUDA)** 2026-09-20,
cập nhật 2026-09-22 ([GPU_H6_IMPLEMENTATION.md](GPU_H6_IMPLEMENTATION.md)): transfer/retention chồng lấn compute trên stream riêng là **mặc định** cho
`--async-compute` (`--no-transfer-overlap` hoặc `--coverage-floor` quay lại time-slice cũ), bộ đếm H2D/D2H tách chiều, hàng đợi verifier công bằng, workload
GPU-local (`--vram-local`) và profile `--workload-profile`; ngân sách RAM tính cả pinned của lane; lane đọc tín hiệu backlog của controller (`active_slot_budget`,
không bao giờ dừng hẳn); nhãn `TRANSFER_BOUND` có bằng chứng phần cứng (100% burst ở cấu hình dành riêng). Trên GTX 960 giữ 98% compute và cho 2,5× độ phủ của
time-slice 0,25 (1,42× compute); `make gpu-h6-check` PASS (58 kiểm tra phần cứng), evidence `build/gpu-h6-lane-20260922-131350/`. `make gpu-h-check` nối
H1/H2/H3/H5/H6 trong một lệnh. H4a, H4b, H8 chưa triển khai; overlap chưa đo trên multi-GPU/iGPU/near-full/soak, chưa có UI state và Vulkan async.
**2026-09-23:** log/report nguyên nhân nghẽn compute (`--bottleneck-log`, `bottleneck_summary`) — một lát cắt của H7, đo thật trên GTX 960 ba kịch bản (GPU-bound, verifier-bound còn chỗ tăng worker, verifier-bound đã hết trần worker), xem [GPU_BOTTLENECK_LOG.md](GPU_BOTTLENECK_LOG.md). H7 tổng thể (UI state trên dashboard) vẫn `planned`.

Trạng thái G1/G2/G3 và CUDA sequential: `implemented`; G4, overlap G5, G6/G7 còn mở. Đã thêm
[roadmap chi tiết](GPU_RESIDENT_COMPUTE_ROADMAP.md): profiling → persistent
Vulkan resources → tiled integer GEMM/corpus → asynchronous verifier → CUDA
→ FP32/GPU-local VRAM → UI/image/evidence. Các mục overlap/verifier workers
của thiết kế GPU-D được theo dõi tiếp tại G2/G4, không coi là đã hoàn tất chỉ
dựa vào trạng thái baseline D. Hardware validation không chặn triển khai source.

Hoàn tất 2026-09-20: timing/aggregate transfer/retention/matrix, queue-empty lower
bound, batch/replay v1 + golden/fake-clock gates; workspace reserve/backoff,
multi-worker host shares và 18-point initialization fault campaign.
`DIT_GPU_HARDWARE=1 make gpu-g12-check` PASS: 29 software tests và 2 hardware tests;
evidence `build/gpu-g12.kmHVGj/`. Layer chưa cài nên gate layer PENDING.

Lịch sử đợt đầu G1/G2 đã có matrix host/GPU timing, versioned profile/replay identity,
submitted/completed/verified counters, persistent buffers/pipeline và recreation
diagnostic cho A/B. Clippy, 23 software tests, `make gpu-check` và
`DIT_GPU_HARDWARE=1 make gpu-g12-check` PASS trên GTX 960; evidence
`build/gpu-g12.OZd5Sa/`. Negative mismatch vẫn FAIL, không tăng verified count.
Chi tiết source, lệnh và giới hạn còn mở tại [GPU_G12_IMPLEMENTATION.md](GPU_G12_IMPLEMENTATION.md).
G3/CUDA đã có; async verifier, validation-layer, soak và image boot còn pending.
Xem [G3/CUDA implementation](GPU_G3_CUDA_IMPLEMENTATION.md).

## Baseline GPU-A..F

| Task | Trạng thái source/software | Hardware evidence |
|---|---|---|
| GPU-A — contract/golden/mock | implemented; gates PASS | Không áp dụng cho CPU mock |
| GPU-B — capability/memory/transfer | implemented; gates PASS | GTX 960 bounded PASS |
| GPU-C — pattern/split/join/matrix | implemented; gates PASS | GTX 960 bounded PASS |
| GPU-D — Deep/duration/seed/coverage | implemented; gates PASS | bounded PASS; 30/60 phút pending |
| GPU-E — UI/multi-GPU/sensors | implemented; gates PASS | GTX 960 picker/telemetry PASS; multi-GPU thật pending |
| GPU-F — shared image/release validation | implemented; extension/negative/boot-policy gates PASS | image USB và 30/60 phút matrix pending |

## GPU-A — 2026-09-06

- Thêm `gpu-engine` và `gpu-cli`; giữ module ID tương thích `dit.vga`, nhãn người dùng `GPU + VRAM`.
- Control Center lựa chọn số 3 chạy binary chung. GPU-A được giữ bằng `--backend mock`; đường mặc định đã là Vulkan GPU-B/C/D.
- Format `DITGPUA1`: block 4 KiB có magic/version/seed/generation/block index/payload length/SHA-256; scatter/join đổi theo seed.
- Matrix baseline 8×8 `u32` modulo 2^32. CPU mock kernel và CPU oracle dùng hai implementation; report chỉ đếm kết quả đã so khớp.
- Golden fixture SHA-256: `947c732fe0f088959e81d0b84b7a21238a186156758c11f1e13c249b77c679ae`.
- Fault tests bắt bit flip, payload sai dù checksum đã tính lại, stale generation, sai block index, block thiếu/trùng và matrix output mismatch.
- Dashboard render in-place cùng phong cách CPU-RAM: thông tin CPU/RAM/GPU/VRAM; thanh CPU, RAM, GPU và VRAM usage; seed, generations, verified calculations và FAIL TOTAL.
- Telemetry chung đọc AMD/Intel/DRM sysfs/hwmon hoặc NVIDIA `nvidia-smi` khi có. NVIDIA query gồm GPU/VRAM usage, used/total VRAM, nhiệt, watt và core/memory clock; từng field fallback sysfs. Trường driver không cung cấp vẫn hiện N/A và source giải thích rõ, không thay bằng zero.
- Chế độ GPU-A luôn ghi `backend=CPU_MOCK_GPU_A`, `hardware_tested=false`, `vram_hardware_test_active=false`; PASS của A không phải PASS phần cứng GPU/VRAM.
- Shared extension đóng `gpu-vram-dit`, docs và manifest module số 3; image đầy đủ chưa được build/burn trong task này.

## Bằng chứng

- `make gpu-a-check`: PASS; positive mock, negative injection, inventory/telemetry, dashboard tests và menu option 3 READY.
- Workspace hiện có 169 Rust tests; GPU gate và Clippy strict PASS.
- Host PTY dashboard smoke: PASS; không phát full-screen clear theo chu kỳ.
- Host nhận `GeForce GTX 960`, BDF `0000:08:00.0`, bound driver `nvidia`, VRAM 2 GiB. `nvidia-smi` cung cấp GPU usage, VRAM used/total, nhiệt, watt và clock.
- Host bounded report: `build/gpu-a-host-check-20260906/report.json`: 10 generations, 320 block, 20.480 matrix results, 0 failure, software PASS.
- Clippy strict và rustfmt: PASS cho các crate bị tác động.

## GPU-B/C/D — 2026-09-06

- Thêm crate `gpu-vulkan`, dùng Vulkan 1.1/`ash`; unsafe FFI được cô lập trong crate này. Inventory ghi device type, UUID, PCI BDF, API/driver, compute queue, device-local heap và `VK_EXT_memory_budget`.
- Policy loại thẳng CPU/software renderer. Host liệt kê GTX 960 index 0 là hardware eligible và `llvmpipe` index 1 là forbidden; không thể dùng llvmpipe để tạo hardware PASS.
- Pool VRAM device-local chia allocation, có bounded backoff. Default target 85% budget khả dụng; CLI cho phép tối đa 95% và có `--vram-mib` để smoke giới hạn. Đây là target budget, không cam kết 95% tổng VRAM trên mọi GPU.
- GPU-B thực hiện host staging upload → device-local VRAM → download, barrier/fence và so toàn byte. GPU-D giữ record rồi readback lại sau các generation, đồng thời drain/final verify trước report.
- GPU-C shader matrix 8×8 `u32` wrapping chạy thật trên GPU, CPU đọc toàn output và so oracle độc lập. SPIR-V SHA-256: `7f7e90c20ded5239f060d7751f9ee844b9b6b48e9b08780551434bca6b811182`.
- Deep mặc định 1.800 giây; duration là deadline, không phải số task hữu hạn. Seed quantum 180 giây; chunk xoay 64/128/256 KiB, 1/4/12/32/64 MiB. Temp limit mặc định OFF, `stop_on_error` mặc định OFF và FAIL sticky.
- UI wide dùng đúng bố cục CPU-RAM: hai khung SYSTEM HARDWARE/LIVE UTILIZATION phía trên và TESTING/PROCESS INFORMATION phía dưới; có CPU frequency/temp/power từ shared sampler, CPU/RAM/GPU/VRAM usage, test allocation, progress, seed index, chunk, H2D+D2H, sweep, matrix verified và FAIL TOTAL.
- Report chỉ ghi `hardware_tested=true` khi ít nhất một transfer và một matrix dispatch đã hoàn tất và không có failure. Capability/allocation failure tạo report INVALID; mock không thể tạo hardware PASS.

## Bằng chứng GPU-B/C/D

- `make gpu-check`: PASS, gồm rustfmt/Clippy/tests, mock regression, Vulkan inventory, hardware positive và negative transfer/matrix.
- Evidence giữ lại: `build/gpu-bcd-retention-smoke-20260906-2216/report.json` — GTX 960, UUID `6c57e2fb7786dd6d7ce34a27131ac119`, BDF `0000:08:00.0`, allocation 64 MiB, 5 transfer, 5 matrix dispatch, 5 retention checks, 81.920 matrix results, 0 failure, PASS.
- Negative transfer: `build/gpu-b-negative-transfer-20260906-2225/report.json` — injected first-byte mismatch, FAIL, `hardware_tested=false`.
- Negative matrix: `build/gpu-c-negative-matrix-20260906-2227/report.json` — injected result 0 mismatch, FAIL, `hardware_tested=false`.
- Cancel smoke: `build/gpu-cancel-smoke-20260906-2300/report.json` — Ctrl+C tạo verdict ABORTED, drain 17 retention records, 0 failure và giữ final report.

## Giới hạn còn mở

- GPU-D đã implemented nhưng chưa chạy Deep 30/60 phút; chưa validated near-full allocation, full sweep hoặc thermal behavior trên image boot.
- Chưa chạy Vulkan validation layers trong image, chưa test AMD/Intel/iGPU/multi-GPU và chưa xác nhận driver proprietary NVIDIA trong Ubuntu Minimal image. Đây là GPU-E/F và hardware matrix.
- GPU/VRAM usage là telemetry toàn device; `TEST ALLOC` và report allocation mới là phần bộ nhớ engine giữ. Logical verified/cumulative traffic không được diễn giải thành số cell VRAM vật lý duy nhất.

## GPU-E/F — 2026-09-07

- Picker GPU tái sử dụng đúng component/geometry của picker CPU–SSD, hiển thị alias, model, Vulkan index, BDF/UUID, driver, local memory và READY/BLOCKED. TTY smoke chọn GTX 960 index 0 PASS; llvmpipe index 1 tạo report INVALID và `hardware_tested=false`.
- `--devices 0,2` và `--devices all` hỗ trợ chọn nhiều GPU. Khi có hơn một GPU, parent chạy mỗi GPU trong process riêng với progress JSON, worker log và report riêng, rồi tạo aggregate report có verdict/failure attribution theo device. Final result copy giữ aggregate cùng ba artifact của từng `device-N`.
- Single/multi-GPU và CPU‑SSD/NVMe nay đều đưa model vào `dit-ui-core::UnifiedDashboard`; geometry/header/hardware/live/progress/calculations/fail/footer do một renderer quyết định. Mỗi GPU thêm meter usage/VRAM cùng allocation/transfer/progress/sensor; console thấp tự paging. Inventory/vendor probe nằm ngoài render loop để tránh nvidia-smi làm trễ/chớp frame.
- dGPU theo target budget tối đa 95%. iGPU không có `--vram-mib` tự giới hạn tổng 60% device-local/shared heap; nhiều iGPU chia đều cap này để chừa RAM cho Linux, CPU oracle và staging. Unit gate xác nhận single/multi cap và override.
- `make gpu-ef-check`: PASS 7/7 nhóm — strict source gates, capability/software rejection, bounded positive, transfer/matrix negative, menu dùng chung, extension checksum và boot policy. Không build image/QEMU.
- Đã sửa scheduler retention: trước khi ring write đè một range còn pending, engine verify range đó trước. Reproducer 8 MiB từng false FAIL đã PASS với 0 mismatch sau sửa; gate bounded tăng lên 16 generation để bắt hồi quy wrap/overwrite.
- Builder thêm NVIDIA flavor tách biệt (`make nvidia-image NVIDIA_DRIVER_PACKAGE=nvidia-driver-NNN-open`), blacklist nouveau và ghi profile/package. Generic image vẫn giữ open stack; cả hai flavor còn cần physical boot validation.
- 2026-09-24 (chiều): menu GPU-VRAM chạy `--async-auto --vram-max --vram-sweep --compute-mix alternate`. (1) Lỗi phần cứng được chấm theo mức độ cho riêng GPU đang test (Xid theo số, AER của GPU và port phía trên, ECC/row-remap khi có): lỗi không sửa được → FAIL phase `hardware`; lỗi đã sửa → PASS kèm `PASS (HW WARNINGS)`. (2) `--vram-max` giữ VRAM tới mức driver báo trống trừ 128 MiB. (3) Quét DRAM trên GPU ở băng thông đầy đủ (device-verified, đếm riêng, CPU kiểm lại theo mẫu; luân phiên nửa pool với lane để CPU-verified vẫn phủ toàn bộ). (4) GEMM FP32 FMA khớp oracle CPU từng bit, xen kẽ với u32. (5) GEMM INT8 Tensor core (CC ≥ 8.0) có sẵn nhưng **chưa kiểm trên phần cứng**, chưa bật ở menu. GTX 960, K=1024, 60 s: u32 108,5 G + FP32 94,1 GFLOP/s, CPU-verified 1.075 MiB/s, device-verified 6,5 GiB/s, PASS. Chi tiết: [GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md](GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md); evidence `build/gpu-menu-full-20260924-195348/`, `build/gpu-vram-sweep-tune-20260924-191831/`, `build/gpu-hwhealth-20260924-190128/`.
- 2026-09-24: menu GPU-VRAM chạy pipeline async GPU-H trên mọi GPU CUDA (`--async-auto`; Intel/AMD vẫn tuần tự) kèm log nguyên nhân nghẽn trong thư mục kết quả. GTX 960, cấu hình menu, 95% VRAM: 257,5 G int-op/s đã xác thực và 1.409 MiB/s độ phủ PCIe/VRAM, so với 10,2 G và 898 MiB/s của menu cũ; PASS. `--vram-local` không bật vì làm giảm độ phủ đã xác thực. Chi tiết: roadmap GPU-H mục 4, evidence `build/gpu-async-menu-20260924-131640/`.
- 2026-09-22: gói driver NVIDIA cài lúc boot (`make nvidia-driver-package` + `make nvidia-driver-burn-usb`) thay cho việc build hẳn một image NVIDIA riêng — một image generic, gói burn cùng USB, cài lúc boot nếu phát hiện GPU NVIDIA thật; `nvidia-image`/`nvidia-burn-usb` giữ nguyên song song. Đã build thật (257 gói, 445 MB) và cài offline thật trong chroot (DKMS build ra 5 module `.ko`); boot thật đầu tiên trên RTX 5060 lộ ra race `nouveau` (driver chạy nouveau thay vì nvidia), từ 2026-09-23 `nouveau` bị tắt hẳn trên mọi flavor (kernel `module_blacklist=nouveau` + modprobe.d `install nouveau /bin/false` + initramfs) — card NVIDIA không có gói driver thì không có driver nào. Boot thật lần 2 (2026-09-23): driver không được cài vì initramfs mount `/run` noexec nên `install.sh` giải nén vào đó không chạy được; đã chuyển sang `/var/tmp` + `sh install.sh`, log luôn lưu vào `DIT-RESULTS/boot/`, chạy thử trong môi trường mô phỏng live boot OK (DKMS build 5 module, 183 s); chưa build lại image + boot lại trên phần cứng — xem [NVIDIA_DRIVER_PACKAGE.md](NVIDIA_DRIVER_PACKAGE.md).
- Evidence sau sửa: `build/gpu-ui-layout-sensor-pty-20260907/terminal.typescript` + `report.json` (terminal 180×40, 2 giây, 87 transfer/matrix/retention, 0 failure, PASS) và `build/gpu-ui-sensor-retention-20260907/report.json` (bounded 16/16/16, 0 failure, PASS). Telemetry cuối trên GTX 960 ghi đủ GPU/memory-controller usage, 41 C, 29,01 W, core 1.126 MHz và memory 3.505 MHz từ `nvidia-smi + sysfs fallback`.
- Unified renderer evidence: `build/unified-nvme-ui-20260907.txt` và `build/unified-gpu-ui-pty-20260907/terminal.typescript`; GPU report cạnh đó PASS 89 transfer + 89 matrix + 89 retention, 0 failure. Unit gate khóa vị trí các heading cố định giữa hai engine và chỉ cho running payload thay đổi.
- Evidence bền: `build/gpu-e-picker-pty-20260907/report.json` (picker thật), `build/gpu-ef-bounded-20260907/report.json` và progress cạnh đó (GPU-F-1, GTX 960, 3 transfer/matrix/retention, PASS), `build/gpu-ef-software-invalid-20260907/report.json` (llvmpipe INVALID).
- Extension chung: `dist/cpu-nvme-extension.tar.gz`; checksum hiện hành nằm ở file `.sha256` cạnh artifact để tránh hash tự tham chiếu trong docs được đóng gói. Manifest chứa ba module, pipeline `GPU-F-1`, Vulkan backend, binary/shader hashes, SBOM và [checklist validation](VALIDATION.md).
- Chưa có GPU thứ hai trên host nên process orchestration/aggregate/dynamic UI đã qua unit gate nhưng concurrent multi-GPU hardware vẫn pending. Chưa build/burn/boot image, chưa chạy Deep 30/60 phút, AMD/Intel/iGPU hoặc Vulkan validation layers trên image.

## Mẫu cập nhật sau mỗi task

- Task / ngày / trạng thái:
- Hành vi đã triển khai và source/artifact:
- Lệnh kiểm thử đã chạy và kết quả:
- Hardware: GPU/UUID/BDF, driver/ICD, kernel/image/shader hash:
- Duration/seed/allocated/verified/FAIL counters:
- Report/log/evidence path:
- Giới hạn, gate còn pending và task tiếp theo:
