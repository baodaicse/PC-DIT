# GPU-H — Compute/verifier bất đồng bộ và cân bằng tải

Cập nhật: 2026-09-22 (bản 3: thêm mục 3 "Hiệu quả bắt lỗi phần cứng" sau rà soát source
+ đo thủ công trên GTX 960; 2026-09-22 đóng các mục còn lại của H6: ngân sách RAM cho lane,
lane đọc tín hiệu controller, `TRANSFER_BOUND` trên phần cứng, overlap thành mặc định, `make gpu-h-check`).
Trạng thái: **partial** — H1, H2, H3, H5 và H6 (đường CUDA) `implemented` (source/software + bounded
GTX 960; [H1](GPU_H1_IMPLEMENTATION.md), [H2](GPU_H2_IMPLEMENTATION.md), [H3](GPU_H3_IMPLEMENTATION.md),
[H5](GPU_H5_IMPLEMENTATION.md), [H6](GPU_H6_IMPLEMENTATION.md)); H4a, H4b, H7, H8 chưa đạt gate `implemented`. Đây vẫn là roadmap thực thi ưu tiên tiếp theo
của GPU + VRAM test; không tạo module hay menu riêng, không đổi CPU-RAM/NVMe.

## 1. Baseline, trạng thái thực và phạm vi

Kế thừa G1/G2/G3 và CUDA sequential đã implemented:
[G3/CUDA](GPU_G3_CUDA_IMPLEMENTATION.md), [roadmap G](GPU_RESIDENT_COMPUTE_ROADMAP.md).
Auto dùng CUDA trên NVIDIA, Vulkan chủ yếu Intel/AMD; lựa chọn backend tường minh
không fallback âm thầm.

Mục tiêu: GPU có đủ công việc, CPU xác thực bền vững, tận dụng VRAM/RAM trong
ngân sách an toàn. Không đặt GPU usage 100% làm điều kiện PASS hoặc đánh đổi
full verification lấy usage. Hardware pending không chặn triển khai source;
`implemented`, software gates và hardware effectiveness phải ghi riêng.

Roadmap H cụ thể hóa G4, phần overlap G5 và phần scheduler/UI của G6/G7;
không tự đóng các mục đó. FP32/Tensor, CUDA Graphs vẫn là mở rộng riêng sau khi
đo baseline mới, không phải điều kiện để hoàn thành pipeline exact u32.

| Mục cũ | Phần thực thi mới |
|---|---|
| G1 profiling/replay | H1 contract và telemetry mở rộng |
| G2 workspace | H3 budget chung, pools và ownership |
| G3 corpus/oracle | H2 giảm chi phí xác thực và tăng compute/output |
| G4, overlap G5 | H4 async compute/readback/verifier; H5 điều phối |
| G6 lịch workload/VRAM | H6 compute/transfer/VRAM có lịch riêng (track tách) |
| G7 UI/image/evidence | H7 dashboard/report; H8 regression và packaging |

### 1.1 Trạng thái source hiện có (rà soát 2026-09-20)

Đã có, **chỉ CUDA**, bật bằng `--async-compute`:
[balanced.rs](../../crates/gpu-engine/src/balanced.rs) (ledger slot/epoch + controller v0),
[async_compute.rs](../../crates/gpu-cuda/src/async_compute.rs) (mỗi slot một stream, ba event,
pinned readback, verifier worker scoped), `WorkspaceBudget::plan_with_slots`,
`Corpus::verify_bytes` (so sánh trực tiếp buffer readback). Pipeline `GPU-H-CUDA-1`.

| Task | Trạng thái thực | Đã có | Còn thiếu / lỗi đã biết |
|---|---|---|---|
| H1 | **implemented** | counters cộng dồn + bảo toàn, phase `matrix` được điền, core/event/wall tách riêng, đếm lỗi và `mismatch_words`, link/AER, log kernel, stress profile, baseline trong `build/gpu-h1-baseline-20260920/` | API submit/poll và fake device (H4a); confirm-on-fail (H4a); log theo cửa sổ từng lỗi; effective rate theo từng batch |
| H2 | **implemented** (CUDA) | `verify_bytes` theo khối (≈5,7–7,4×), hash policy (mặc định checkpoint cho async), corpus dựng song song + pool xoay có prefetch, kiểm tra toàn vẹn corpus host, pattern hệ thống, đường tuần tự ít copy; giảm ≈89% CPU core-s/GiB verified | Vulkan (ma trận, so sánh/hash); hash batch lỗi + integrity ở đường tuần tự; đổi K/jobs khi xoay; worker least-loaded: đã làm ở H6 |
| H3 | **implemented** (CUDA) | hai nhóm slot device/host với epoch riêng, `QUARANTINE` bị chặn theo số và tuổi, ngân sách async đúng công thức + thang backoff (hàng đợi → overlap → jobs), rollback khi cấp phát lỗi (24 bước × 2 chế độ), cờ CLI slot/worker, chia CPU theo số GPU | giới hạn tuổi cứng của hàng đợi (H5); multi-GPU/iGPU thật; xác nhận khi lỗi (H4a) |
| H4 | partial (CUDA) | event/stream/pinned, cancel + drain 30 s, fault injection FAIL đúng | không có gate/test cho pipeline; chưa fake device; chia worker cứng theo slot; poll 50 µs + clone ledger; lỗi mismatch bị gói thành "unavailable"; **Vulkan chưa có** |
| H5 | **implemented** (CUDA) | controller v1 (backlog đang chờ, pacing theo tốc độ verdict, hysteresis, hard cap tức thời, áp lực bộ nhớ, stall trong burst), regime label, verifier worker thích ứng, coverage floor bằng time-slice (mặc định 0,25 khi không overlap) | tuổi batch và áp lực CPU chưa là input; hoãn hash/corpus phụ khi throttle; chồng lấn thật, `TRANSFER_BOUND` trên phần cứng và lane đọc backlog: đã làm ở H6 |
| H6 | **implemented** (CUDA) | transfer lane chồng lấn compute (stream/event/pinned riêng, planner không chồng byte, retention trước ghi đè) là **mặc định** cho `--async-compute`, bộ đếm H2D/D2H tách chiều và event timing, token bucket, hàng đợi verifier DRR công bằng compute/lane, workload GPU-local (kernel fill/verify, GPU tự kiểm + checkpoint CPU, touched ≠ verified), profile compute-only/transfer-only/combined, ngân sách RAM tính pinned của lane, lane đọc tín hiệu backlog của controller (`active_slot_budget`, không bao giờ dừng hẳn), `TRANSFER_BOUND` có bằng chứng phần cứng (100% burst ở cấu hình dành riêng); đo trên GTX 960: 98% compute và 2,5× độ phủ của time-slice 0,25 cùng lúc | chưa đo overlap trên multi-GPU/iGPU/near-full/soak (`--no-transfer-overlap` cho tới khi đo); lane chỉ đọc backlog compute, chưa có tín hiệu quá tải riêng của nó và không phản hồi ngược vào giới hạn compute; Vulkan (H4b); link/AER dưới tải dùng lại H1 |
| H7 | planned | trường `async_compute` trong report; log/report nguyên nhân nghẽn (`--bottleneck-log`, `bottleneck_summary`, xem [GPU_BOTTLENECK_LOG.md](GPU_BOTTLENECK_LOG.md)) | không có UI state, không có backlog/age/rate trên dashboard, chưa hiển thị nguyên nhân nghẽn trực tiếp trên UI |
| H8 | planned | `make gpu-h1-check` (fmt, Clippy strict, test, baseline + negative gate); Clippy strict đã sạch; `make gpu-h-check` nay chạy H1-H2-H3-H5-H6 nối tiếp (thêm ở H6, xem ghi chú dưới) | golden, fake device, state machine, report round-trip, ma trận injection, A/B adaptive |

### 1.2 Khuyết điểm đã đo trước H2 (một máy, một lần chạy)

Máy: GTX 960 (CC 5.2, 2 GiB), Ryzen 7 3700X (SHA-NI, AVX2), CUDA, seed 7, 20 s,
`--vram-mib 64`. Đợt đo thủ công này chưa được lưu; H1 đã chạy lại thành evidence bền tại
`build/gpu-h1-baseline-20260920/` (bảng 3.2b).

| Chế độ | Dispatch | GPU util TB / p90 | Verified work |
|---|---|---|---|
| Tuần tự K=256, jobs 1024 | 71 | 7% / 10% | ≈0,15 T int-ops |
| Async K=256, jobs 1024 | 728 | 31% / 55% | ≈1,5 T |
| Async K=1024, jobs 512 | 772 | 58% / 100% | ≈13,3 T |
| Async K=4096 (jobs tự lùi 512→256) | 448 | 62% / 100% | ≈3,7 T (ước tính) |

Các dòng khác K không cùng shape; chỉ minh họa xu hướng. Cùng shape K=256, async cho
khối lượng verify gấp ≈10 lần tuần tự, và chạy đúng (negative gate `--inject-vulkan-fault
matrix` cho FAIL, batch lỗi không vào verified).

Phát hiện dẫn tới các thay đổi trong bản roadmap này:

1. **Regime phụ thuộc K.** K=256: kernel ≈5,6–8,3 ms/batch, D2H ≈1,4 ms, verify ≈19–25 ms/batch
   (verifier là nút thắt). K=1024: kernel ≈24,7 ms, verify ≈12,7 ms (GPU là nút thắt).
2. **Chi phí verify.** Microbenchmark 17,3 MB: `verify_bytes` 18,1 ms (≈955 MB/s, do chia/mod
   từng word); SHA-256 8,2 ms (≈2,1 GB/s với SHA-NI). Hash chiếm ≈30% chi phí verifier
   dù khi so khớp PASS thì hash của actual chỉ là hàm của (corpus, generation).
3. **Controller throttle sai (đã sửa ở H5).** 19–21 sự kiện throttle trong 20 s ở mọi lần chạy async; ở K=1024
   verifier nhanh hơn GPU vẫn kết thúc `DRAINING`, `in_flight_limit=1`. Nguyên nhân: `used`
   của ledger tính cả batch đang chạy trên GPU; với 4 slot mức fill chỉ nhảy 25/50/75/100%,
   3 batch trên GPU (75%) đã vượt ngưỡng 70% và bị gán nhãn "verifier_backlog".
4. **Transfer/retention là nút thắt thứ hai.** Baseline tuần tự: ≈74 ms host so với ≈4 ms device
   mỗi thao tác transfer (pattern + compare + hash). Đồ thị GPU vì vậy răng cưa dù compute đã async.
5. **Đường tuần tự lãng phí** (cấp phát `expected()`/`words(input)` mỗi dispatch, sys-time
   ≈10 s/20 s) — cần làm sạch để A/B công bằng.
6. **Async đổi độ phủ lấy compute.** Compute verified tăng ≈10–90×, công suất từ ≈30 W lên ≈84 W,
   nhưng transfer/retention verified giảm (đo lại ở H1: async chỉ bằng 34–54% tuần tự), và lỗi
   bị ghi một record mỗi burst (H1 đã thêm đếm từng batch). Chi tiết,
   số đo và yêu cầu ở mục 3 (Hiệu quả bắt lỗi phần cứng).

## 2. Kiến trúc đích và bất biến

```text
CPU chuẩn bị corpus/expected ──> corpus resident VRAM ──> GPU compute
                                                           │
                                              device output slot
                                                           │ D2H + event
                                                 host output queue
                                                           │
                                                 CPU verifier pool
                                                           │
                                                 verdict + trả slot
```

- GPU có thể compute N+1 khi CPU verify N. Async API không tự chứng minh overlap;
  phải probe capability và đo event timing, có sequential fallback được báo rõ.
- Mọi batch được tính verified đều phải có full output comparison với expected
  độc lập. Không ghi đè/bỏ output, không tự chuyển sang sampling/GPU digest-only.
- Tách chi phí tạo oracle khỏi compare. CPU không tính lại GEMM mỗi dispatch.
- Một batch phải được nhận diện bằng device, corpus/version/hash, workload shape,
  generation, batch ID, output layout, slot ID và slot epoch chống completion cũ.
- Lỗi sticky; batch thất bại hoặc chưa verify không được cộng vào verified ops.
- Tốc độ sinh output lâu dài không được vượt khả năng verify. RAM chỉ hấp thụ
  burst/độ trễ tạm thời, không dùng hàng đợi vô hạn để che CPU bottleneck.
- Không có kernel vô hạn. Deadline/cancel phải ngừng submit và drain có giới hạn.
- **Mismatch dữ liệu là FAIL, không phải "unavailable".** Lỗi capability/driver (INVALID)
  và lỗi dữ liệu (FAIL) phải có kiểu lỗi riêng và không suy diễn lẫn nhau.
- **Hai quầy đếm tách biệt:** `device_in_flight` (độ sâu hàng đợi GPU) và `host_backlog`
  (batch READY/VERIFYING). Chỉ backlog host là tín hiệu quá tải verifier.
- **Async không được làm yếu khả năng bắt lỗi** (mục 3): không giảm độ phủ VRAM/PCIe dưới
  *coverage floor* đã cấu hình, không đếm lỗi ít hơn thực tế, và mọi lỗi phải giữ được đủ
  bằng chứng để xác nhận lại và phân loại. PASS chỉ nghĩa là "không thấy lỗi trong phạm vi đã
  kiểm", không phải "phần cứng không có lỗi".

### 2.1 Mô hình cân bằng

Với batch có `B = jobs·(N²+4)·4` byte output và K chiều trong:

- `t_gpu ≈ t_kernel(jobs, K) + t_d2h(B)`; `t_verify ≈ B / v_verify + t_hash_policy`;
  với `W` worker, verifier theo kịp khi `t_verify / W ≤ t_gpu`.
- Tỷ số `ρ = output_rate / verify_capacity` xác định **regime**, luôn được báo cáo:
  `GPU_BOUND` (ρ<1, queue rỗng), `VERIFIER_BOUND` (ρ>1 bền vững), `TRANSFER_BOUND`
  (D2H/H2D chiếm phần lớn), `BALANCED`. Regime là chẩn đoán, không phải điều kiện PASS.
- Đòn bẩy theo thứ tự: (1) giảm chi phí verify (H2); (2) tăng worker trong budget CPU;
  (3) tăng công việc GPU trên mỗi byte output bằng K/jobs lớn hơn; (4) giảm tốc độ submit.
  **Không giảm K để giảm tải** — K nhỏ sinh output nhanh hơn. K/jobs chỉ đổi ở ranh giới
  batch và khi corpus mới đã sẵn sàng: đổi K buộc dựng lại corpus/oracle tỉ lệ K, nên
  phải do corpus pool chuẩn bị trước, không dựng đồng bộ trên đường chạy.
- Kiểm tra mô hình bằng ma trận K ∈ {256, 1024, 4096} ở H8; không hard-code K mặc định
  cho tới khi có evidence.

## 3. Hiệu quả bắt lỗi phần cứng

Pipeline async được đánh giá bằng khả năng bắt và chẩn đoán lỗi phần cứng, không chỉ bằng
throughput hay utilization. Mục này định nghĩa bề mặt bắt lỗi, các số đo baseline, và các yêu
cầu mà H1–H8 phải thỏa; nó không đặt mục tiêu "% lỗi thật bắt được" khi chưa đo bằng lỗi thật.

### 3.1 Bề mặt bắt lỗi

| Loại lỗi | Hiện tại | Cơ chế / giới hạn | Xử lý |
|---|---|---|---|
| ALU số nguyên sai (mul/add) | Mạnh | Full compare với oracle CPU độc lập; cộng modulo 2^32 không che bit; mọi MAC ảnh hưởng output. Giới hạn: một corpus dùng lại ≤180 s, N chỉ 63–65 | 3.8 |
| Không ghi / ghi sai hàng / output cũ / đè guard | Mạnh | Guard memset mỗi lần chạy, header generation 64-bit; 5 chế độ fault đã chạy trên GPU thật | giữ |
| Ô nhớ VRAM (stuck-at, ghép cặp) | Trung bình | H2D→D2H dữ liệu splitmix ngẫu nhiên, chỉ qua copy engine; chưa có pattern hệ thống; chưa có workload GPU-local đọc pool | 3.8, H6 |
| Retention | Yếu (tên quá lời) | Chỉ đọc lại sau 2 generation ≈0,5–2 s, không phải dwell time DRAM | 3.8 |
| PCIe / DMA | Một phần | Dữ liệu DMA nằm trong vòng kiểm; lỗi đã được hardware retry thì vô hình; link gen/width chưa ghi | 3.6 |
| Treo / rơi bus / reset driver | Yếu | Chỉ timeout 30 s / API error; `gpu-cli` không thu log Xid/AER/amdgpu/i915; parser hiện có chỉ cho host (MCE/EDAC/thermal) và chưa được `gpu-cli` gọi | 3.6 |
| Lỗi phụ thuộc tải (PSU, VRM, nhiệt) | Tùy chế độ | Tuần tự chỉ ≈25% công suất; async K lớn kéo tới giới hạn công suất | 3.7 |
| Lỗi host RAM / CPU verifier | Gán nhầm thành "FAIL GPU" | Không có bước xác nhận; RAM host thường non-ECC | 3.4, 3.9 |

### 3.2 Baseline đo (GTX 960, một lần chạy 20–25 s, pool 64 MiB)

| Chỉ số | Tuần tự K=256 | Async K=256 | Async K=1024 |
|---|---|---|---|
| Compute verified (cùng 20 s) | 1× | ≈10× | ≈90× (khác shape) |
| Công suất TB / đỉnh | 29,8 W / 40,5 W | – | 84,3 W / 128,9 W (limit 120 W) |
| Nhiệt tối đa | 44 °C | – | 64 °C |
| Transfer + retention verified | 179 MiB/s | 57 MiB/s | 70 MiB/s |
| Số transfer trong 20 s | 71 | 23 | 25 |

Máy: GTX 960 không ECC, 2 async engine, `CONCURRENT_KERNELS=1`; PCIe gen1 khi rảnh, gen3 dưới
tải (một lần lấy mẫu). Với pool thực 85–95% VRAM, độ phủ theo thời gian phải đo lại.

**3.2b Evidence H1** (`build/gpu-h1-baseline-20260920/`, steady 20 s, seed 7, một lần chạy mỗi ô):

| K (jobs) | Gop/s seq → async | VRAM/PCIe MiB/s seq → async | Độ phủ async/seq | Công suất TB W seq → async | `out/verify` | busy frac |
|---|---|---|---|---|---|---|
| 256 (1024) | 7,9 → 80,3 | 184 → 63 | 34% | 29,8 → 68,5 | 1,24 | 0,39 |
| 1024 (512) | 12,2 → 167,9 | 144 → 70 | 49% | 31,8 → 80,7 | 0,69 | 0,77 |
| 4096 (256) | 15,1 → 196,8 | 94 → 51 | 54% | 36,2 → 88,0 | 0,20 | 0,92 |

Verifier ≈620–650 MiB/s mỗi core. `out/verify` giảm từ 1,24 xuống 0,20 khi K tăng: xác nhận
mô hình 2.1 (K nhỏ bị giới hạn bởi verifier, K lớn bởi GPU).

**3.2c Evidence H2** (`build/gpu-h2-ab-20260920-185044/`, cùng cấu hình; `seq_lean` = tuần tự đã làm
sạch + hash checkpoint). Con số "≈10–14×" của H1 so với đường tuần tự còn lãng phí; so với `seq_lean`:

| K | Gop/s seq_lean → async | Độ phủ VRAM/PCIe MiB/s seq_lean → async | Độ phủ async/seq_lean |
|---|---|---|---|
| 256 | 64 → 187 (2,9×) | 1.399 → 134 | 10% |
| 1024 | 110 → 234 (2,1×) | 1.204 → 89 | 7% |
| 4096 | 158 → 246 (1,6×) | 871 → 56 | 6% |

### 3.3 Coverage floor — async không được đổi độ phủ lấy compute

Burst 32 batch chạy nối tiếp giữa các transfer làm độ phủ VRAM/PCIe giảm mạnh (evidence H1: async
đạt 34%/49%/54% tuần tự ở K=256/1024/4096). **Sau H2 tệ hơn về tương đối**: transfer tuần tự nhanh gấp
7–9× nên async chỉ còn **6–10%** tuần tự tốt nhất (bảng 3.2c), dù tuyệt đối tốt hơn H1 (1,1–2,1×). Vì transfer
nay rẻ (≈12 ms/op), chia thời gian với burst compute là khả thi và là việc của H5. Yêu cầu:

- `coverage_floor`: tỷ lệ tối thiểu tốc độ VRAM/PCIe verified (MiB/s, số sweep/giờ) so với
  baseline tuần tự cùng budget; tham số cấu hình được, ghi vào report cùng giá trị thực đạt.
  Giá trị khởi điểm là giả thuyết ≥80%, chốt từ evidence sau H1.
- Đạt floor bằng chia thời gian (time-slice burst compute và transfer/retention) ở H5 trước;
  chồng lấn thật (transfer trên stream/copy engine riêng, khi capability probe cho phép)
  thuộc H6. Không cần chờ H6 để có floor.
- Khi compute và coverage tranh chấp, controller giảm burst compute trước khi bỏ transfer;
  coverage bị bỏ đói là lỗi thiết kế, không phải hiệu ứng phụ chấp nhận được.

### 3.4 Xác nhận khi lỗi (confirm-on-fail) và phân loại

Khi một batch hoặc transfer sai, slot chuyển sang trạng thái `QUARANTINE` (bounded theo số
slot và thời gian) và chạy tuần tự các bước, bỏ qua nếu context đã stopped/device loss:

1. So sánh lại cùng buffer host → khớp: `VERIFIER_TRANSIENT`.
2. D2H lại cùng slot device rồi so sánh → khớp: `TRANSFER_OR_HOST_TRANSIENT`; sai giống
   hệt: dữ liệu lưu trên device hoặc compute, đi tiếp.
3. Chạy lại batch cùng generation → khớp: `COMPUTE_TRANSIENT`; sai giống hệt:
   `DETERMINISTIC_COMPUTE_OR_DEVICE_MEMORY`; sai khác lần trước: `INTERMITTENT`.

Với mỗi lỗi lưu vùng evidence có giới hạn: số word/byte sai, N mismatch đầu, histogram theo
bit/job/hàng/cột hoặc offset, và replay keys. Nhãn là phân loại thực nghiệm; **verdict vẫn FAIL
bất kể nhãn**, nhãn không được biến FAIL thành PASS và không kết luận linh kiện cụ thể.
Chi phí confirm bị chặn: đầy đủ cho lỗi đầu tiên và một mẫu giới hạn các lỗi sau.

### 3.5 Đếm lỗi và ước lượng tần suất

- Mỗi batch/transfer lỗi được đếm riêng, cộng dồn qua mọi burst. Sau lỗi đầu tiên, các batch
  đã submit vẫn được drain và verify; không dừng submit mới trừ khi `--stop-on-error`.
- Report có: `failed_batches`, `mismatch_words`, `first_failure_at` (thời điểm và số verified
  trước đó), tỷ lệ lỗi theo GiB verified và theo Gop verified, phân bố theo thời gian.
- Không suy ra "lỗi tất định" hay "lỗi ngẫu nhiên" từ một lần lỗi; dùng nhãn 3.4 và tỷ lệ.

### 3.6 Log lỗi phần cứng và sức khỏe link

- Thu log kernel (`journalctl -k`, hoặc dmesg) đầu run, cuối run và cửa sổ quanh mỗi lỗi; đếm
  riêng theo họ: NVIDIA (`NVRM: Xid`), PCIe AER (corrected/uncorrected), `amdgpu` (ring timeout,
  reset, VM fault), `i915`/`xe` (GPU hang, reset), `nouveau` (timeout/fifo). Tách khỏi
  MCE/EDAC của host. Không đọc được → `unavailable`, không ghi 0.
- Log là bằng chứng bổ trợ, không quyết định verdict dữ liệu; parse chuỗi phải chịu thay đổi
  định dạng theo driver và có test fixture. Data PASS không chứng minh không có corrected error.
- Ghi link gen/width đầu, giữa và cuối lúc **đang chịu tải** (giá trị lúc rảnh có thể thấp hơn).
  Tách data verdict và link health (mục H6).

### 3.7 Cường độ tải (stress profile)

Report ghi công suất TB/đỉnh và % power limit, xung SM/mem, nhiệt đỉnh, throttle reasons, và
phân biệt giai đoạn tăng nhiệt với steady-state. Lý do: tuần tự gần idle nên lộ lỗi phụ thuộc
tải kém. Không đặt mục tiêu công suất tuyệt đối; K/jobs chọn theo evidence và theo mô hình 2.1.
Chạm hoặc vượt power limit thoáng qua là quan sát bình thường, không phải điều kiện PASS/FAIL.
Thermal/availability abort policy giữ độc lập; temp limit mặc định vẫn OFF theo quyết định hiện có.

### 3.8 Đa dạng dữ liệu và pattern

- Compute: corpus pool xoay theo lịch thay vì chờ hết 180 s; biến thiên N và K trong giới hạn
  capability; seed replay được.
- Transfer: thêm pattern hệ thống (all-0, all-1, checkerboard, walking-bit, address-in-address,
  cặp pattern/nghịch đảo) xen kẽ với splitmix theo lịch xác định, replay được, ghi pattern class
  vào từng batch.
- "Retention" hiện tại đổi tên thành `delayed_reread` trong report; thêm dwell thật (một phần
  nhỏ pool, thời gian cấu hình, có giới hạn) và ghi dwell thực tế thay vì giả định.
- Workload GPU-local đọc/ghi pool (H6) là cách duy nhất đưa vùng lớn của VRAM qua đường SM/L2.

### 3.9 Phân biệt lỗi host với lỗi GPU

- Bước 1–2 của 3.4 loại các lỗi tạm thời ở buffer host và verifier.
- **Corpus integrity check:** expected corpus (input + dot) nằm trong RAM host nhiều phút;
  hash lại định kỳ ở checkpoint (chi phí có giới hạn). Hash đổi → lỗi host memory, báo riêng,
  không quy cho GPU.
- Khuyến nghị chạy CPU-RAM test trước GPU trong campaign; report ghi kết quả preflight nếu có.

### 3.10 Kiểm chứng hiệu quả bắt lỗi

- **Ma trận injection** mở rộng cho cả tuần tự và async, CUDA và Vulkan: header, guard, byte đầu/
  giữa/cuối, một bit, nhiều word, output cũ, tái dùng slot, transfer và retention, lỗi ngắt
  quãng theo xác suất. Với injection tất định phải bắt 100%; với injection theo xác suất, số
  lỗi injected và số lỗi báo phải bảo toàn (không mất, không đếm trùng).
- **Nhạy cảm với lỗi thật (time-to-first-failure):** đo dưới điều kiện bất ổn có kiểm soát do
  người vận hành tự gây (ví dụ offset xung/áp) và tự quyết định. Không nằm trong `make`, không
  tự động; ghi `pending` khi chưa có. So sánh tuần tự, async và K để có số đo hiệu quả thật.
- Injection kiểm chính bộ so sánh, **không phải bằng chứng bắt lỗi phần cứng thật**; không tuyên
  bố tỷ lệ phát hiện lỗi thật khi chưa có phép đo trên.

### 3.11 Scorecard hiệu quả bắt lỗi (report/H8)

Compute verified ops/s; VRAM/PCIe verified MiB/s, coverage share so với baseline và số sweep;
công suất/xung/nhiệt/throttle; `failed_batches`, `mismatch_words`, tỷ lệ lỗi, `first_failure_at`;
nhãn xác nhận; số log lỗi theo họ và link gen/width dưới tải; corpus integrity; pattern class
đã dùng; time-to-first-failure khi có. H8 so sánh scorecard giữa tuần tự → ít copy → async → adaptive.

### 3.12 Phân bổ vào task và điều không được làm

| Nội dung | Task |
|---|---|
| Đếm lỗi cộng dồn (3.5), thu log lỗi và link dưới tải (3.6), stress profile (3.7), baseline scorecard | H1 |
| Pattern hệ thống, corpus xoay, corpus integrity check (3.8, 3.9) | H2 |
| Trạng thái `QUARANTINE` trong ledger, confirm-on-fail, tiếp tục drain sau lỗi (3.4, 3.5) | H3, H4a |
| Coverage floor bằng time-slice (3.3) | H5 |
| Transfer chồng lấn compute, workload GPU-local, quota verifier (3.3, 3.8) | H6 |
| Hiển thị scorecard, nhãn xác nhận và log lỗi trên UI/report | H7 |
| Ma trận injection, TTFF, so sánh scorecard theo mode | H8 |

Không làm: giảm full compare để lấy throughput; thay compare bằng hash hoặc sampling; coi
injection PASS là bằng chứng phần cứng; tự động gây bất ổn phần cứng; kết luận linh kiện hỏng
từ một nhãn phân loại.

## 4. Danh sách task và thứ tự

| Task | Kết quả | Phụ thuộc | Trạng thái |
|---|---|---|---|
| H1 | Contract, counters cộng dồn, replay, baseline evidence, log lỗi/stress profile | G1–G3/CUDA sequential | **implemented** |
| H2 | Verifier ít copy/ít CPU, hash policy, corpus pool, oracle song song, pattern hệ thống | H1 | **implemented** (CUDA) |
| H3 | Budget VRAM/RAM; tách device slot và host slot; slot QUARANTINE | H1 | **implemented** (CUDA) |
| H4a | Harden adapter CUDA: fake device, gate, worker động, confirm-on-fail, report | H1, H3 | partial |
| H4b | Async Vulkan (Intel/AMD/iGPU) | H4a | planned |
| H5 | Backpressure, giảm tải/phục hồi, regime, coverage floor | H2, H4a | **implemented** (CUDA) |
| H7 | UI/report backlog, trạng thái điều phối, scorecard | H1, H4a, H5 | planned |
| H8 | Negative gates, ma trận injection, A/B theo K, release/extension/image | H1–H5, H7 | planned |
| H6 | **Track riêng:** lịch compute/PCIe/VRAM, transfer chồng lấn, workload GPU-local | H4a, H5 | **implemented** (CUDA; [chi tiết](GPU_H6_IMPLEMENTATION.md)) |

**2026-09-24 — pipeline async đã nối vào menu boot (một phần của H8 "image nhận source mới").**
Menu GPU-VRAM (`dit-orchestrator`, [`gpu_run_arguments`](../../crates/dit-orchestrator/src/main.rs)) nay chạy
`--async-auto --bottleneck-log <run>/bottleneck.jsonl`. `--async-auto` quyết định pipeline **theo từng GPU**: GPU chạy CUDA
dùng đường async H1–H6 (controller H5, lane chồng lấn H6 mặc định, hash checkpoint, xoay corpus, jobs 512 nếu không truyền
`--matrix-jobs`); GPU Intel/AMD (Vulkan) chạy tuần tự, các cờ chỉ dành cho async bị bỏ thay vì bị từ chối, nên máy trộn
NVIDIA + iGPU vẫn chạy được. Đo trên GTX 960 (`build/gpu-async-menu-20260924-131640/SUMMARY.md`): menu cũ 10,2 G int-op/s
và 898 MiB/s; menu mới ở 95% VRAM đạt 257,5 G int-op/s (×25) và 1.409 MiB/s (×1,57), PASS, 57/57 burst `GPU_BOUND`.
`--vram-local` **không** được bật: trên GTX 960 nó hạ độ phủ CPU đã xác thực xuống 520 MiB/s, dưới cả đường tuần tự.
Chưa có số đo trên RTX 50-series, multi-GPU thật hay soak dài; image/ISO phải build lại mới nhận thay đổi này.

**2026-09-24 (chiều) — mở rộng workload ngoài H1–H8** (theo yêu cầu: giữ VRAM gần tối đa, FP32, Tensor core, DRAM
băng thông cao, xử lý lỗi phần cứng đã sửa). Đã làm: chấm lỗi phần cứng theo mức độ (một phần của scorecard 3.11/H7),
`--vram-max`, quét DRAM trên GPU (`--vram-sweep`, device-verified đếm riêng), GEMM FP32 FMA (`--compute-mix`), tất cả
có trong menu; GEMM INT8 Tensor core có sẵn nhưng chưa kiểm trên phần cứng (máy phát triển là GTX 960). Còn mở:
xác nhận INT8 trên GPU CC ≥ 8.0 rồi mới bật `all` ở menu; đo trên RTX 50 (khả năng chạy đồng thời sweep + GEMM có
thể khác GTX 960, nơi hai tải chỉ chia thời gian); tối ưu kernel GEMM (tile/register blocking) vì kernel hiện chỉ đạt
một phần nhỏ công suất đỉnh. Chi tiết: [GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md](GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md).

Thứ tự: H1 → H2 → H3 → H4a → H5 → H7 → H8; H4b và H6 chạy song song/sau, không chặn
`source complete` của đường CUDA. **Ràng buộc mới:** H5 không được ghi `implemented`
khi verifier chậm hơn GPU ở shape mặc định (ρ>1) — tune controller trên verifier đắt
gấp nhiều lần GPU không có ý nghĩa; H2 phải có số đo trước/sau. Mỗi task phải cập nhật
test, report/docs cùng source; H7/H8 không phải lý do trì hoãn telemetry và regression.

### H1 — Contract và phép đo

Trạng thái: `implemented` 2026-09-20 — xem [GPU_H1_IMPLEMENTATION.md](GPU_H1_IMPLEMENTATION.md) cho phần đã
làm, phần hoãn sang H4a và evidence. Nội dung dưới đây là yêu cầu gốc.

- Mở API submit/poll/completion/verify thay vì một lời gọi submit-wait-compare.
  GPU owner giữ context/queue và tài nguyên; CPU workers không tự gọi driver.
- Khóa output layout, endian, guards, expected reference, ownership và replay
  version. Ghi effective shape/rate của từng batch khi controller đổi tải.
- Counters: submitted, completed, readback-ready, verified, failed, pending,
  verified integer ops và bytes; kiểm tra conservation không đếm trùng.
  **Report cộng dồn qua mọi burst** (totals toàn run + high-water marks); dữ liệu từng burst
  chỉ là chi tiết phụ. Burst cuối rỗng (deadline cắt ngay) không được ghi đè số liệu trước.
- Đo oracle generation, packing/copy, host compare/hash, device compute/H2D/D2H,
  queue wait, queue bytes/age, output production rate và verifier rate.
  Điền đủ `input_generation_seconds` và phase `matrix` của campaign khi chạy async.
- **Đặt tên đúng đại lượng:** thời gian CPU của worker là *core-seconds* (cộng dồn qua worker),
  không trộn với wall time; tổng khoảng event của nhiều stream có thể chồng lấn nên không phải
  thời gian GPU bận — báo thêm `device_busy_wall` (từ event đầu tới event cuối / hợp khoảng).
- Giữ chỉ số transfer end-to-end nhưng phân biệt với device copy throughput;
  không gọi H2D+D2H là băng thông GPU-local VRAM.
- Lưu baseline vào `build/` (không chỉ trong docs): sequential và async, K ∈ {256,1024,4096},
  kèm hardware/driver/seed/shape; cold-start và steady-state riêng.
- **Bắt lỗi (mục 3):** đếm lỗi từng batch/transfer cộng dồn (3.5); thu log kernel theo họ lỗi GPU
  và ghi link gen/width dưới tải (3.6); `stress_profile` công suất/xung/nhiệt/throttle (3.7);
  baseline scorecard cho tuần tự và async (3.11), gồm độ phủ VRAM/PCIe theo MiB/s.

Gate: fake-clock/counters/replay, completion sai thứ tự/trùng/mất; baseline
sequential cùng seed/shape làm mốc; report round-trip có totals cộng dồn qua ≥2 burst và
qua burst rỗng. Không suy tỷ lệ bottleneck chỉ từ ảnh usage. Log kernel: fixture cho từng họ
lỗi, trường hợp không đọc được cho `unavailable`, không ghi 0; nhiều lỗi trong một burst được
đếm đủ, không chỉ lỗi đầu.

### H2 — Giảm chi phí CPU trên mỗi phép tính

Trạng thái: `implemented` cho đường CUDA 2026-09-20 — xem [GPU_H2_IMPLEMENTATION.md](GPU_H2_IMPLEMENTATION.md) cho phần
đã làm, số đo A/B, phần còn mở (Vulkan; worker least-loaded đã làm ở H6) và hai điều chỉnh so với kết luận H1. Nội dung
dưới đây là yêu cầu gốc.

- Pool nhiều corpus CPU-validated, expected immutable và input resident; thay
  corpus định kỳ bằng worker có giới hạn CPU/RAM. Batch giữ reference corpus đến
  khi verify xong; không evict expected đang được dùng. Pool cũng phục vụ đổi K/jobs (2.1).
- **Oracle/corpus dựng song song hoặc SIMD** trong giới hạn core (hiện scalar đơn luồng,
  ≈0,5 s mỗi corpus lạnh ở K=256 và tăng tuyến tính theo K); không làm GPU đứng chờ khi đổi seed.
- Tăng compute/output bằng lựa chọn M/N/K/jobs theo capability và budget; tính
  đúng useful ops. Shape chỉ đổi ở ranh giới batch và khi expected đã sẵn sàng.
- So sánh trực tiếp readback buffer sau DMA, tránh Vec/word packing/bản sao thừa.
  **Viết lại `verify_bytes` theo hàng/job** (bỏ chia/mod từng word, tách header/guard/payload,
  SIMD qua `wide`); đầu ra vẫn là mismatch đầu tiên chính xác (index/expected/actual).
  Mismatch chỉ lưu vùng evidence có giới hạn và replay keys.
- Full compare luôn bật. **Hash policy mặc định: `checkpoint`** — hash actual ở batch cuối,
  ở checkpoint định kỳ và trên mọi batch lỗi; hash mỗi batch là policy tùy chọn nếu report
  nêu rõ. Khi so khớp PASS, hash actual là hàm của (corpus, generation) nên không làm tăng độ
  tin cậy verdict. Không đưa expected hash vào trường actual hash để tiết kiệm công việc.
- **Phạm vi mở rộng sang transfer/retention verifier** (pattern, compare, hash: ≈17× thời gian
  device ở baseline) vì đây là nguồn răng cưa GPU khi compute đã async.
- **Làm sạch đường tuần tự** (`expected()`/`words(input)`/hash mỗi dispatch) để A/B công bằng.
- **Đa dạng dữ liệu (3.8, 3.9):** corpus pool xoay theo lịch; pattern hệ thống xen kẽ splitmix
  cho transfer; corpus integrity check ở checkpoint. Viết lại `verify_bytes` phải giữ nguyên độ
  nhạy: differential test với bản cũ trên tập fault ngẫu nhiên, không chỉ vài vị trí cố định.
- Verifier pool nhỏ/configurable; dành headroom cho OS, UI, GPU owner, telemetry.
  Chọn số workers bằng đo throughput, không mặc định dùng toàn bộ CPU cores. Worker nhận việc
  từ hàng đợi chung/least-loaded, không gán cố định theo số slot.

Gate: cùng verdict/hash khi bật hash, golden độc lập; lỗi header/payload/guard,
stale generation, wrong corpus; oracle không bị chạy lại trên warm batch; mismatch
đầu tiên của verifier mới trùng verifier cũ trên tập fault. Benchmark verify (MB/s và
core-seconds/GiB verified) trước/sau — baseline ≈955 MB/s + 2,1 GB/s SHA. Mục tiêu khởi
điểm (giả thuyết, chốt sau H1): core-time/GiB verified giảm ≥50%. Đo CPU time và byte RAM
copy trên mỗi verified output; không chỉ usage.

### H3 — VRAM/RAM budget và vòng đời slot

Trạng thái: `implemented` cho đường CUDA 2026-09-20 — xem [GPU_H3_IMPLEMENTATION.md](GPU_H3_IMPLEMENTATION.md) cho phần
đã làm, số đo (hàng đợi host 4 slot gây 27–31 throttle với controller v0 — lỗi metric đã được H5 sửa — và ≥8 slot thì 0) và phần còn mở. Nội dung dưới đây là yêu cầu gốc.

VRAM: reserve driver/display + corpus + device outputs + workspace + vùng
transfer/VRAM test. RAM: reserve OS + expected corpus + verifier scratch + pinned
staging + queued outputs + evidence. Không cộng hai lần vùng staging cũng là queue.
Multi-GPU dùng chung ngân sách host; iGPU tính shared RAM, không xem là hai pool
độc lập. Budget phải tính cả đỉnh cấp phát lúc thay corpus/resize và partial init.

- Tận dụng tối đa **ngân sách an toàn**, không cấp kín 100% VRAM/RAM.
- Device output: FREE → GPU_WRITING → COPYING → FREE sau event D2H hoàn tất.
- Host output: FREE → DMA_WRITING → READY → VERIFYING → FREE sau CPU verify.
- **Tách hai vòng đời.** Bản hiện tại gộp device slot và pinned slot 1:1 nên hàng đợi host
  bị giới hạn bằng số slot GPU (mặc định 4). Cần số device slot nhỏ (đủ bão hòa GPU) và số
  host slot riêng, lớn hơn (đủ mịn để điều khiển theo phần trăm — xem H5).
- Event xác nhận DMA hoàn tất mới cho CPU đọc; không tái sử dụng slot còn owner.
- **Trạng thái `QUARANTINE`** (3.4): slot của batch lỗi được giữ nguyên dữ liệu device và host
  cho tới khi confirm-on-fail xong hoặc hết thời hạn; chiếm slot có giới hạn (số slot và thời
  gian), không làm cạn pool và không giữ vô hạn.
- Queue giới hạn byte, batch count, pinned bytes và tuổi batch lâu nhất.
- Khởi đầu pinned pool vừa đủ. Tầng RAM thường chỉ thêm nếu A/B chứng minh lợi
  ích vượt chi phí copy; nếu thêm, phải hạch toán cả hai bản trong thời gian copy.
- **Công thức budget theo pipeline:** async chỉ cần pinned readback cho mỗi host slot, không
  cần các bản sao `Vec` của đường tuần tự; tính riêng để không lùi `jobs` không cần thiết
  (đã thấy jobs 512→256 ở K=4096) nhưng vẫn không vượt reserve.
- **Tham số cấu hình được:** số device slot, host slot, verifier worker, batch/burst qua CLI
  và ghi vào report (hiện cố định `AsyncConfig::default()`).
- **Multi-GPU:** mỗi process GPU đang có pool verifier riêng. Parent phải chia ngân sách
  core CPU và RAM cho tổng các worker; không để N GPU × W worker vượt headroom.
- Tránh gây swap pressure; budget thấp/OOM thì backoff hoặc báo không đủ tài nguyên,
  không cố tăng queue. Không thay đổi swap toàn hệ thống để ép test chạy.

Gate: allocation failure tại mỗi bước, rollback đúng; budget cực thấp/multi-GPU (tổng
worker/RAM ≤ headroom), slot wraparound/epoch cũ, reference corpus; chứng minh không reuse
trước completion.

### H4a — Harden pipeline CUDA

- CUDA: event dependencies cho compute/copy, pinned buffers, streams phù hợp;
  một owner điều phối context (đã có). Reserve output capacity trước submit; chuyển quyền
  sở hữu bằng completion token. Readback-ready queue đưa batch cho CPU workers, không
  chia sẻ buffer mutable.
- **Tách logic pipeline khỏi FFI bằng trait thiết bị** (`submit`/`poll`/`readback`/`release`)
  để có fake device chạy được trong unit test: completion sai thứ tự, trùng, mất, device treo,
  verifier chậm/treo/crash — các kịch bản này hiện không test được nếu không có GPU.
- Owner loop dựa trên event/back-off có giới hạn thay vì poll 50 µs cố định và clone toàn
  bộ ledger mỗi vòng.
- Deadline-aware: không mở burst mới khi deadline đã tới; không để burst cuối rỗng ghi đè
  số liệu (H1). Độ dài burst đo theo thời gian, không chỉ số batch cố định.
- Phân loại lỗi: mismatch → FAIL dữ liệu; capability/driver/timeout → INVALID hoặc lỗi hạ tầng
  theo contract hiện có. Chuỗi báo lỗi không được nhãn "unavailable" cho mismatch.
- **Confirm-on-fail (3.4) và đếm lỗi (3.5):** lỗi vào `QUARANTINE`, chạy bước 1–3 và gắn nhãn;
  các batch đã submit tiếp tục drain/verify và được đếm; ngừng submit sau lỗi chỉ khi
  `--stop-on-error`. Không chạy lại kernel nếu context đã stopped.
- Giữ sequential path để fallback/A-B. Chạy riêng từng backend, không giả định
  CUDA/Vulkan enumeration hoặc tốc độ tương đương.
- Khi verifier crash/timeout/device loss: ngừng submit, lưu first failure và
  pending ledger; không free vùng DMA còn có thể dùng.
- Cancel/deadline: stop submit → bounded drain → verify phần đã nhận → finalize.
  Báo incomplete/aborted theo contract nếu còn batch bắt buộc chưa xác thực; không PASS.

Gate: `make gpu-h-check` (fmt, Clippy strict, unit + fake device, negative CUDA thật khi có
GPU); verifier chậm/treo/crash, out-of-order completion, queue đầy, cancel ở mọi
state, timeout/device loss; exact output giống sequential và không mất batch. Fake device có
chế độ lỗi tạm thời và lỗi tất định để kiểm từng nhãn 3.4; số lỗi đếm được bằng số lỗi injected
qua nhiều burst; `QUARANTINE` không làm cạn slot và được giải phóng khi hết thời hạn.
(`make gpu-h-check` hiện đã tồn tại — nối H1/H2/H3/H5/H6 — nhưng chưa có gate riêng cho fake device/H4a mô tả ở trên;
khi H4a có gate, thêm vào chuỗi này.)

### H4b — Async Vulkan (track sau)

Queue/fence hoặc timeline semaphore theo capability, barriers và non-coherent cache handling
đúng; không yêu cầu nhiều queue mới chạy. Dùng cùng trait thiết bị, ledger và controller như
CUDA. Cần phần cứng Intel/AMD/iGPU để có hardware evidence; máy phát triển hiện chỉ có NVIDIA
+ llvmpipe (bị chặn), nên source/software gate có thể hoàn tất trước, hardware ghi `pending`.
Backend Vulkan không có async không được báo là `--async-compute` chạy được (giữ lỗi tường minh).

### H5 — Điều khiển cân bằng tải

Trạng thái: `implemented` cho đường CUDA 2026-09-20 — xem [GPU_H5_IMPLEMENTATION.md](GPU_H5_IMPLEMENTATION.md) cho thiết kế, số đo
(ngưỡng ≥80% độ phủ đạt ở floor ≈0,45–0,5 với compute ≈1,1× tuần tự), ba lỗi thiết kế mô phỏng vòng kín đã bắt được và phần còn mở.
Nội dung dưới đây là yêu cầu gốc.

Controller dùng **host backlog** (batch READY/VERIFYING: số lượng, byte, tuổi lớn nhất),
xu hướng backlog, tốc độ sinh/verify, CPU/pinned-memory pressure; không điều khiển chỉ dựa
vào GPU usage và **không tính batch đang chạy trên GPU vào tín hiệu quá tải**. `in_flight_limit`
giới hạn độ sâu hàng đợi GPU và là *đầu ra* của controller.

| Ngưỡng thử nghiệm (theo host backlog / host queue cap) | Hành động |
|---|---|
| <30%, ổn định | Tăng submit rate/in-flight từng bước trong budget |
| 30–60% | Giữ tải nếu backlog/age ổn định |
| >70% hoặc backlog tăng liên tục | Giảm submit rate/in-flight, hoãn corpus/hash phụ |
| >90% hoặc chạm hard cap | Dừng submit mới; drain/verify, không hủy output |
| Sau throttle, <40% và ổn định | Phục hồi từng bước, không bật ngay tải tối đa |

Đây là điểm khởi đầu để đo, chưa phải default validated. Vùng giữa các ngưỡng
giữ state trước; cần hysteresis, cửa sổ đo, smoothing, cooldown và giới hạn bước
điều chỉnh rõ ràng. Queue cap cứng vẫn áp dụng ngay, không chờ cửa sổ đo.

- **Độ mịn:** ngưỡng phần trăm chỉ có nghĩa khi host queue đủ slot (khuyến nghị ≥8, quyết
  định sau đo). Với ít slot hơn phải dùng ngưỡng theo byte/tuổi/xu hướng thay vì phần trăm slot.
- **Khởi động và ramp:** không bắt đầu ở `in_flight_limit=1` (không có overlap). Warm-start tối
  thiểu 2 và ramp theo số batch hoàn tất *và* thời gian; cooldown cố định 250 ms dài hơn cả
  một burst (≈0,7–1,4 s) làm controller chưa kịp ramp.
- **Regime label** (2.1) và `throttle_reason` phân biệt: `verifier_backlog`, `host_memory_pressure`,
  `queue_hard_cap`, `stalled`. Không dán nhãn "verifier_backlog" khi hàng đợi chỉ chứa batch
  đang ở GPU.
- Giảm tải ưu tiên giảm tốc độ tạo batch/output. **Không mặc định giảm K**, vì
  kernel ngắn hơn có thể làm output sinh nhanh hơn. Có thể tăng compute/output
  với corpus đã chuẩn bị (2.1), nhưng phải giữ giới hạn thời gian kernel/cancel.
- Verifier workers chỉ tăng trong CPU/RAM budget nếu throughput thực tăng.
- Recovery chỉ khi verifier còn tiến triển và memory pressure đã giảm; stalled
  verifier phải xử lý lỗi, không throttle vô hạn để che sự cố. Stall detection tính theo
  tiến triển *trong run hiện tại*, không dùng mốc từ burst trước.
- Controller không thay thế thermal/availability abort policy; ghi rõ lý do throttle.
- **Coverage floor (3.3):** scheduler chia thời gian giữa burst compute và transfer/retention để
  tốc độ VRAM/PCIe verified không thấp hơn floor cấu hình; khi tranh chấp thì giảm burst compute
  trước. Ghi tỷ lệ độ phủ thực đạt vào report.

Gate: fake-clock trace overload → throttle → drain → recover; burst vs overload
kéo dài, zero progress, memory pressure, không oscillation/tăng vô hạn; replay
được quyết định bằng trace. **Hồi quy từ lỗi đã đo:** trace GPU_BOUND (verify nhanh hơn GPU,
host backlog ≈0) phải cho `throttle_events == 0`; trace VERIFIER_BOUND phải throttle và ổn
định; tốc độ đổi trạng thái (events/phút) là chỉ số có ngưỡng. Trong overload bền vững,
không bỏ output để đạt usage. Coverage floor: trace có burst compute dài vẫn giữ tốc độ
transfer/retention ≥ floor; không có trace nào làm transfer bị đói.

### H6 — Compute, PCIe và VRAM có lịch riêng (track tách)

**Trạng thái: `implemented` cho đường CUDA (2026-09-20, cập nhật 2026-09-22)** — xem [GPU_H6_IMPLEMENTATION.md](GPU_H6_IMPLEMENTATION.md) cho thiết kế, số đo và phần chưa làm
(overlap là mặc định nhưng chưa đo trên multi-GPU/iGPU/near-full/soak; Vulkan; lane chỉ đọc backlog compute, chưa có tín hiệu quá tải riêng).

Không chặn `source complete` của H1–H5/H7/H8. Bắt đầu sau khi H4a/H5 ổn định.

- Compute resident không phải đợi vòng transfer/retention trước mỗi dispatch. Hiện transfer/
  retention chạy đồng bộ xen giữa các burst và tốn ≈74 ms host/op; đưa chúng lên cùng cơ chế
  slot/event/verifier (phụ thuộc H2 cho verifier transfer).
- Transfer: H2D, D2H và hai chiều khi hỗ trợ, pattern/offset/chunk thay đổi,
  full comparison; vùng riêng không đè compute corpus/output.
- GPU-local VRAM workload truy cập vùng lớn, rotating regions và full-check
  checkpoints theo contract; allocated bytes không đồng nghĩa verified coverage.
- Fair scheduling và quota verifier cho mỗi workload/GPU để transfer không chiếm
  toàn bộ queue, compute không làm retention/coverage bị bỏ đói.
- Có profile chạy riêng để định vị nút thắt và combined để test đồng thời; không
  cam kết compute/PCIe/VRAM đều đạt tốc độ tối đa khi tranh chấp tài nguyên.
- Thu link width/generation dưới tải (lưu ý link có thể hạ khi rảnh), AER/root-port,
  replay/reset/driver errors khi hỗ trợ. Missing telemetry ghi unavailable, không diễn giải là
  zero errors. Data PASS không chứng minh không có corrected errors; không kết luận lane vật lý
  cụ thể hỏng từ mismatch hay throughput thấp. Tách data verdict và link health.

Gate: coverage không mất khi throttle, transfer âm/dương, counters theo chiều,
workload isolation/fairness; có hardware thì đối chiếu link và event timings.

### H7 — UI và report

**2026-09-23: phần log/report đã có** cho nguyên nhân nghẽn (`--bottleneck-log`, `bottleneck_summary`
trong report) — xem [GPU_BOTTLENECK_LOG.md](GPU_BOTTLENECK_LOG.md). Đây là một lát cắt nhỏ của H7
(chỉ phần phân loại + ghi log, chưa có hiển thị trên dashboard tương tác); H7 tổng thể vẫn `planned`.

- Giữ RAM/VRAM used/total GiB và capacity %, không thay bằng bus activity.
- Hiển thị queue used/cap GiB, pinned GiB, pending batches, oldest age,
  compute/output rate, verifier MiB/s và verified ops/s đúng loại phép tính;
  tách `device_in_flight` khỏi `host_backlog`.
- State: WARMUP / RAMP_UP / BALANCED / THROTTLED / DRAINING / STOPPED;
  kèm throttle reason, regime (`GPU_BOUND`/`VERIFIER_BOUND`/`TRANSFER_BOUND`/`BALANCED`) và
  effective submit/shape/worker settings. Hiện dashboard chưa có state nào của controller.
- Report phân biệt full-verified, failed và unverified; sticky first failure,
  high-water marks, thời gian throttle, số lần đổi state, corpus rotations và ledger chưa hoàn tất.
- **Scorecard bắt lỗi (3.11):** hiển thị và ghi coverage share, stress profile, số lỗi/tỷ lệ lỗi,
  nhãn xác nhận, log lỗi theo họ và link gen/width dưới tải; trường không đo được là `unavailable`.
  UI không dùng "PASS sạch" khi còn lỗi log hoặc lỗi chưa xác nhận.
- Tách host end-to-end transfer, device H2D/D2H và GPU-local memory workload;
  giữ tương thích report cũ bằng version/default fields phù hợp.

Gate: single/multi GPU, terminal nhỏ, missing sensors, mọi controller state;
UI/report/counters nhất quán, không tô PASS khi queue chưa xử lý xong.

### H8 — Đóng gói và nghiệm thu

- Software gates: golden, state machine, fake-clock controller, fake device, budgets, negative
  faults, report round-trip/compatibility; **Clippy strict/format sạch** (hiện `async_compute.rs:425`
  làm fail) và regression workspace. `make gpu-h-check` đã tồn tại (nối H1/H2/H3/H5/H6, xem H6);
  H8 vẫn cần bổ sung fake device/golden/state-machine/report round-trip vào chuỗi đó.
- A/B cố định seed/corpus/shape/budget/duration: sequential → ít copy → async →
  adaptive, **lặp theo K ∈ {256, 1024, 4096}** để thấy regime. Ghi CPU core-time/verified work,
  GPU event timing, throughput verified, queue age/peak RAM/pinned/VRAM; cold-start và
  steady-state riêng. Tiêu chí nghiệm thu là *tương đối* so với baseline đã lưu (ví dụ verified
  ops/s so với sequential cùng shape; core-time/GiB verified), không phải utilization tuyệt đối.
- **Kiểm chứng bắt lỗi (3.10):** ma trận injection cho tuần tự và async, CUDA và Vulkan (bảo
  toàn số lỗi injected so với báo cáo); so sánh scorecard 3.11 giữa các mode; time-to-first-
  failure chỉ khi người vận hành tự gây bất ổn có kiểm soát, ngoài `make`, ghi pending nếu chưa có.
- Hardware: NVIDIA CUDA, Intel/AMD Vulkan, missing/unsupported backend, iGPU và
  multi-GPU khi có; soak và validation layer ghi pending nếu chưa có môi trường.
- Release/extension manifest, kernel provenance, image boot smoke; image chưa
  rebuild không được ghi là đã nhận source mới. Không tự flash USB.

Điều kiện source complete (đường CUDA): mọi gate phần mềm liên quan của H1–H5, H7, H8 PASS, không
lost/duplicate output, memory bounded, overload/recovery được kiểm tra, cancel/drain giữ bằng
chứng, `make gpu-h-check` + Clippy strict PASS, và các yêu cầu mục 3 đạt ở mức phần mềm
(coverage floor, đếm lỗi cộng dồn, confirm-on-fail, thu log lỗi, ma trận injection). H4b (Vulkan) và H6 có trạng thái riêng, không
kéo hoặc bị kéo theo trạng thái của đường CUDA — `make gpu-h-check` chạy chung cả gate H6 để tiện (một lệnh cho mọi gate H đang có),
nhưng "source complete" của đường CUDA chỉ xét PASS của phần H1-H5/H7/H8 trong đó, không phải toàn bộ lệnh. Hardware effectiveness chỉ công bố cho cấu hình
đã đo; không khóa source tiếp theo vào toàn bộ hardware matrix. Chốt các giá trị mặc định từ
evidence, không gán mục tiêu utilization hoặc tốc độ tuyệt đối chưa có baseline.
