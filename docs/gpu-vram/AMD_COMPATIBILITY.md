# GPU-VRAM trên GPU AMD — bảng tương thích

Trạng thái ngày 2026-10-01, image `DIT-01-NOBLE-20261001-…`: kernel **7.0.0-30**, Mesa **25.2.8** (driver Vulkan
RADV), `linux-firmware` 20240318 của Ubuntu (đã có firmware AMD từ GCN1 tới RDNA4, `gc_12_0_x`).

> **Chưa test trên card AMD thật.** Đường Vulkan mới được kiểm trên GTX 960 dùng driver Vulkan của NVIDIA (transfer,
> matrix, chèn lỗi đúng/sai). Telemetry sysfs của AMD chỉ được kiểm bằng fixture. Các mục "✅" bên dưới nghĩa là
> **code có và dùng chung**, chưa phải đã xác nhận trên AMD.

Ký hiệu: ✅ có · ⚠️ có nhưng hạn chế · ❌ không có trên AMD · ❓ chưa biết, cần kiểm trên card thật.

## 1. Dòng card AMD

GPU-VRAM trên AMD chạy qua Vulkan (RADV), nên card phải dùng driver kernel `amdgpu` (driver cũ `radeon` không có Vulkan).

| Thế hệ | Ví dụ | Firmware trong image | Driver / Vulkan | Dự kiến |
|---|---|---|---|---|
| GCN1 (Southern Islands) | HD 7700–7970, R7 240–250, R9 270/280 | ✅ tahiti, pitcairn, verde, oland, hainan | ❓ `amdgpu.si_support=-1` (mặc định của kernel; kernel mới chuyển dần SI rời sang `amdgpu`). Nếu card bám `radeon` thì không có Vulkan → GPU-VRAM báo không có GPU đủ điều kiện | ❓ cần kiểm |
| GCN2 (Sea Islands) | R7 260, R9 290/290X/390, APU Kaveri/Kabini | ✅ bonaire, hawaii, kaveri, kabini, mullins | ❓ `amdgpu.cik_support=-1`, như trên | ❓ cần kiểm |
| GCN3 | R9 285/380, R9 Fury/Nano | ✅ tonga, fiji, topaz, carrizo | `amdgpu` mặc định, RADV đầy đủ | ✅ |
| GCN4 (Polaris) | RX 460–480, RX 550–590 | ✅ polaris10/11/12, vegam | như trên | ✅ |
| GCN5 (Vega) | Vega 56/64, Radeon VII, APU Raven/Picasso/Renoir | ✅ vega10/12/20, raven, picasso, renoir | như trên | ✅ |
| RDNA1 | RX 5500–5700 | ✅ navi10/12/14 | như trên | ✅ |
| RDNA2 | RX 6400–6950, APU Van Gogh / Rembrandt | ✅ sienna, navy, dimgrey, beige, yellow, vangogh | như trên | ✅ |
| RDNA3 | RX 7600–7900, APU Phoenix / Strix | ✅ `gc_11_0_x`, `gc_11_5_x` | như trên | ✅ |
| RDNA4 | RX 9060 / 9070 | ✅ `gc_12_0_0`, `gc_12_0_1` | kernel 7.0 + Mesa 25.2 hỗ trợ | ✅ |
| Instinct (CDNA) | MI100, MI210/250, MI300 | ✅ arcturus, aldebaran, `gc_9_4_x` | không có hàng đợi đồ hoạ; RADV không hỗ trợ | ❌ |

APU (GPU tích hợp) dùng RAM hệ thống làm "VRAM": test vẫn chạy, nhưng khi đó nó kiểm RAM qua GPU chứ không phải VRAM rời.

## 2. Tính năng của test GPU-VRAM

| Tính năng | NVIDIA (CUDA) | AMD (Vulkan) |
|---|---|---|
| Nhận GPU, chọn GPU, bỏ llvmpipe / VGA của BMC | ✅ | ✅ |
| Pipeline | async GPU-H: nhiều batch trên GPU, verifier CPU song song, bộ điều khiển tải | ⚠️ tuần tự: một thao tác một lúc; menu (`--async-auto`) tự bỏ các tuỳ chọn chỉ dành cho async |
| Transfer H2D/D2H, so từng byte | ✅ | ✅ |
| Retention (ghi, giữ, đọc lại) | ✅ | ✅ |
| Matrix compute, so với oracle CPU | ✅ u32, FP32 (FMA); INT8 tensor từ sm_80 | ⚠️ chỉ u32 (`matrix_u32.comp`) |
| Xoay corpus, chính sách hash kết quả | ✅ | ❌ |
| Lane transfer/retention chạy song song với compute (H6) | ✅ | ❌ |
| VRAM sweep tốc độ GPU (GPU tự ghi/kiểm, không qua PCIe) | ✅ | ❌ — VRAM chỉ được phủ qua đường PCIe (transfer/retention) |
| Lượng VRAM giữ | `--vram-max` (card rời), hoặc % | ⚠️ `--vram-percent` 95% của ngân sách `VK_EXT_memory_budget` |
| Profile menu "Tensor INT8 check" | ✅ từ sm_80 | ❌ báo `TENSOR CHECK NOT RUN … Intel/AMD` |
| Profile "Full DIT" | u32 + FP32 xen kẽ, sweep, VRAM tới giới hạn | ⚠️ transfer + retention + matrix u32, 95% VRAM |
| Nhiều GPU cùng lúc (mỗi GPU một worker) | ✅ (chưa kiểm phần cứng) | ✅ (chưa kiểm phần cứng) |
| Chèn lỗi (`--inject-hardware-fault transfer/matrix`) | ✅ | ✅ |
| Log nguyên nhân nghẽn (`--bottleneck-log`) | ✅ | ❌ |

## 3. Telemetry và sức khoẻ phần cứng

| Mục | NVIDIA | AMD |
|---|---|---|
| % bận GPU, VRAM đã dùng | `nvidia-smi`, fallback sysfs | ✅ sysfs `gpu_busy_percent`, `mem_info_vram_used` |
| Nhiệt độ, công suất, xung GPU/VRAM | `nvidia-smi` | ✅ hwmon `temp*_input`, `power1_average`/`power1_input`, xung DPM / `freq*` |
| Lý do throttle | ✅ | ❌ (hiện `N/A`, không ghi 0) |
| ECC / row remap VRAM | ✅ `nvidia-smi` | ❌ chưa đọc RAS của `amdgpu` (`ras/*_err_count`, chủ yếu Radeon Pro / Instinct) |
| Log kernel | Xid được phân loại; Xid nặng → run FAIL | ⚠️ `amdgpu` ring timeout, GPU reset, VM/page fault → chỉ **cảnh báo**, không làm FAIL và không vào sổ lỗi |
| Link PCIe: tốc độ, độ rộng, AER ở hai đầu | ✅ | ✅ (sysfs, không phụ thuộc hãng) |
| Bộ đếm PCIe replay | ✅ `nvidia-smi -q -d PCIE` | ✅ sysfs `pcie_replay_count` (do `amdgpu` cung cấp) |

## 4. Đếm FAIL theo component (fault attribution)

| Bước | NVIDIA | AMD |
|---|---|---|
| F0 sổ lỗi | ✅ | ✅ transfer / retention → `PCIe-GPU\|VRAM`, compute → `PCIe-GPU\|GPU\|VRAM` (chưa tách) |
| F1 ghi lỗi ngay khi xảy ra | ✅ | ✅ |
| F2 sự kiện link (AER, replay, tụt độ rộng) | ✅ | ✅ |
| F3 điểm kiểm GPU (`d2h`, `h2d`, `device`, `vram-*`) | ✅ | ❌ chỉ có ở CUDA; mục boot Fault Attribution tự bỏ cờ này trên AMD |
| F6 xếp hạng theo chữ ký lỗi (trùng thời điểm với AER/replay, địa chỉ lặp, bit cố định) | ✅ | ✅ — kết quả chỉ ở mức **nghi ngờ** |
| Lỗi từ log kernel | Xid → ô (48/63/64/92/94/95 → VRAM, 79 → `PCIe-GPU\|PLATFORM`) | ❌ thông báo `amdgpu` chưa được quy ô |

Hệ quả: trên AMD, một lỗi lane PCIe kiểu máy 7800X3D hiện là `? PCIe-GPU1|GPU1|VRAM1 n`. Nếu lỗi trùng thời điểm với
AER/replay, F6 chuyển nó thành `PCIe-GPU1 n (n susp)`. Không có kết luận **xác định** như trên NVIDIA.

## 5. COMBINED trên máy chỉ có GPU AMD

| Lớp | AMD |
|---|---|
| CPU-RAM, SSD | ✅ (không phụ thuộc GPU) |
| GPU | ⚠️ chạy đường Vulkan tuần tự (mục 2) |
| Data chain SSD → RAM → GPU → CPU | ❌ cần một GPU CUDA; phiên chạy tiếp mà không có chain |

## 6. Cần làm để gọi là "hỗ trợ AMD"

Kiểm trên card thật (ưu tiên từ trên xuống):

| Card | Mục đích |
|---|---|
| RX 580 (Polaris) hoặc RX 6600 (RDNA2) | đường Vulkan cơ bản: PASS sạch, chèn lỗi transfer/matrix, telemetry sysfs, `pcie_replay_count` |
| RX 7600 (RDNA3) | thế hệ phổ biến hiện tại |
| RX 9060/9070 (RDNA4) | firmware `gc_12` và Mesa 25.2 trong image |
| R9 290/390 hoặc HD 7970 | GCN1/GCN2 có lên `amdgpu` + Vulkan không; nếu không, thêm tham số kernel `radeon.si_support=0 amdgpu.si_support=1 radeon.cik_support=0 amdgpu.cik_support=1` vào GRUB |
| APU Ryzen (Vega/RDNA iGPU) | GPU tích hợp, VRAM là RAM hệ thống |

Việc phần mềm, xếp theo giá trị cho việc đếm lỗi đúng component:

1. Đưa thông báo `amdgpu` (ring timeout, GPU reset, VM fault) vào sổ lỗi như Xid, và coi reset/hang là lỗi.
2. Đọc RAS của `amdgpu` (ECC VRAM) khi card có.
3. Shader Vulkan tự kiểm VRAM trên GPU (sweep), để tách VRAM khỏi PCIe như CUDA.
4. Shader FP32 cho Vulkan.
5. F3 cho Vulkan: băm và tự kiểm output trên GPU trước khi copy về.
