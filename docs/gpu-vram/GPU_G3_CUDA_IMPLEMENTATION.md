# GPU-G3 và CUDA backend

Cập nhật 2026-09-20. G3 và CUDA sequential backend đã implemented;
G4 overlap/verifier pool, CUDA Graphs và G6 FP32/GPU-local VRAM chưa triển khai.
Không dùng hardware validation làm điều kiện chặn source tiếp theo.

## Backend và workload

- `--backend auto`: NVIDIA dùng CUDA, Intel/AMD dùng Vulkan. NVIDIA thiếu CUDA
  được báo không đủ capability, không âm thầm fallback Vulkan/mock.
- `--backend cuda` và `--backend vulkan` chọn tường minh. Ghép device bằng PCI/UUID,
  không giả định ordinal CUDA trùng ordinal Vulkan. `inventory` có danh sách riêng
  và danh sách auto; index được hiểu theo backend đã chọn.
- Rust crate `gpu-cuda` tải Driver API động qua `libcuda.so.1`; PTX nhúng trong
  binary. Cargo build, máy Intel/AMD và CPU/NVMe không cần toolkit/NVRTC runtime.
- Kernel exact u32 GEMM modulo 2^32, tile 16x16 shared memory; N=63/64/65 theo
  seed, K=1..4096. Tất cả multiply/add ảnh hưởng output được CPU kiểm tra.
  Mỗi job có generation 64-bit và hai guard words; full result readback.
- CPU oracle i-k-j độc lập với GPU tiled reduction. Corpus keyed seed/jobs/K,
  hash SHA-256; oracle và input GPU được tái sử dụng khi corpus không đổi.
  Golden fixture khóa bằng phép tính Python số nguyên không giới hạn trước khi
  lấy modulo. Không coi kiểm output là quan sát trực tiếp mọi thanh ghi trung gian.
- `--matrix-inner K` thay nghĩa pointer-chase cũ; `--compute-rounds` là alias tương
  thích cú pháp nhưng nay mang nghĩa K. Default jobs=64, K tự tăng theo duration
  từ 256 đến 1024. Không so sánh số rounds cũ với K như cùng lượng công việc.
- Report pipeline `GPU-G3-1`, workload `G3-U32-GEMM-1`; matrix replay version 2
  phân biệt thuật toán cũ version 1. Transfer/retention giữ replay version 1.
  `integer_operations=2*jobs*N*N*K` là integer ops, không phải FLOPS; không tính
  padding tile và generation tag vào useful ops.

## Quản lý tài nguyên và giới hạn

Device/pinned buffers, module, stream và events có ownership riêng; reuse một
slot tuần tự. Budget/reserve tính theo K và N tối đa, allocation backoff chỉ
cho CUDA out-of-memory. Lỗi API khác không được biến thành partial success.
Probe CC, block/grid/shared-memory limits; dispatch z tối đa 65535 jobs.
Timeout polling 30 giây đánh dấu context stopped, không submit thêm và giữ tài
nguyên cho tới process exit thay vì free vùng đang được GPU dùng. Đây không phải
cam kết chặn được một driver call tự treo; watchdog/process isolation vẫn cần.

CUDA dùng một stream; chưa overlap transfer/compute/verifier. Cold corpus vẫn
tốn CPU oracle; warm corpus vẫn cần readback/hash/compare. Vì vậy G3 không phải
cam kết 100% GPU utilization. Đo campaign queue-empty và device timing để tiếp
tục G4, không bỏ xác thực nhằm làm đẹp usage.

## Toolchain và chạy lại

PTX compile bằng NVIDIA NVRTC 12.9.86, `compute_52`, C++11, `--fmad=false`.
Build host GTX 960/driver 580.173.02; driver hiển thị CUDA 13 không đồng nghĩa
dùng compiler CUDA 13 cho Maxwell. Các GPU/driver khác cần probe/JIT và hardware
matrix riêng; không suy ra PASS từ khả năng build.

```sh
make gpu-g3-check
DIT_GPU_HARDWARE=1 make gpu-g3-check
cargo test -p gpu-cuda --test g3 -- --ignored --nocapture
cargo test -p gpu-vulkan -- --ignored --test-threads=1
cargo build --release -p gpu-cli
target/release/gpu-vram-dit inventory
target/release/gpu-vram-dit run --backend auto --generations 3 \
  --vram-mib 8 --matrix-jobs 8 --matrix-inner 129 --no-ui
```

Regenerate kernel chỉ khi sửa CUDA source:

```sh
make gpu-cuda-kernel NVRTC_LIBRARY=/absolute/path/libnvrtc.so.12
```

Cập nhật `KERNEL_SHA256` cùng PTX; `embedded_kernel_identity` kiểm hash và NUL
terminator. Runtime không cần NVRTC. Tham khảo [NVIDIA NVRTC 12.9](https://docs.nvidia.com/cuda/archive/12.9.0/nvrtc/index.html).

## Kiểm thử và phạm vi evidence

- Software golden, overflow/tail/generation, backend selection, budget, replay,
  profiling/fake-clock và Clippy warnings-as-errors.
- GTX 960: CUDA/Vulkan cùng hash ở 12 tổ hợp seed/K (N63/64/65, K1/17/64/129),
  warm resident upload=0, oracle time=0; transfer/retention thật.
- CUDA device-side injection: intermediate corruption, wrong row, missing write,
  stale generation, guard overwrite đều phải Mismatch, verified=0; chạy sạch
  tiếp theo phải thành công. Vulkan negative gate hiện vẫn là readback injection.
- Vulkan regression: persistent resources, 18-point partial initialization,
  transfer/retention campaign timing đã chạy trên GPU thật.
- Extension đóng gói cả SPIR-V/PTX/source và manifest hash. ISO/image boot,
  validation layer, soak dài, AMD/Intel và NVIDIA đời khác vẫn cần evidence riêng.
  Không đánh dấu toàn bộ G5/G7 hoàn tất chỉ vì CUDA sequential đã chạy được.

Evidence release: `build/gpu-g3-auto-release-20260920/report.json`,
`build/gpu-g3-vulkan-release-20260920/report.json` PASS;
`build/gpu-g3-cuda-negative-20260920/report.json` FAIL đúng kỳ vọng.
Finite `--generations` cố ý đổi seed mỗi generation để gate corpus mới;
chạy theo duration giữ seed trong quantum và dùng resident corpus.
`build/gpu-g3-resident-release-20260920/report.json` PASS, warm input resident,
uploaded_bytes=0. Gói extension: `dist/cpu-nvme-extension.tar.gz`; chưa rebuild ISO.

Kết quả regression: 33 software tests GPU PASS, cross-backend CUDA gate và 3
Vulkan hardware tests PASS; G1/G2 campaign PASS tại `build/gpu-g12.N4A45Q/`.
`cargo test --workspace -- --test-threads=1` PASS; structure tests 5/5 PASS.
Lần chạy workspace song song có một timing assertion CPU-RAM
`minimum_duration_repeats_complete_rounds` fail; chạy riêng và chạy tuần tự PASS.
Không sửa module CPU-RAM để che lỗi timing này.

## UI dung lượng và throughput

Thanh VRAM dùng `vram_used_bytes / vram_total_bytes`, kèm Used GiB / tổng GiB;
không dùng `memory_utilization_percent` (hoạt động bộ điều khiển/bus). Thiếu
dung lượng used thì hiển thị N/A, không lấy bus activity làm fallback. Áp dụng
single GPU, worker progress, multi GPU và legacy/compact UI. RAM hệ thống giữ
used/total như trước. Memory clock vẫn lưu telemetry nhưng bỏ khỏi dòng Used.

`H2D+D2H MiB/s` hiện là throughput end-to-end của transfer gần nhất: bao gồm
CPU tạo pattern, staging, copy, chờ GPU, CPU compare và hash. Không phải phép đo
băng thông GPU-local VRAM. GPU usage thấp cần đối chiếu campaign prepare/compare,
queue-empty và CUDA event timings; G4 overlap/compute scheduling còn mở.
