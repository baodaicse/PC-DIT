# GPU-VRAM DIT — thuật toán tính toán và cách xác thực VRAM

Cập nhật: 2026-09-24. Tài liệu tham chiếu cho người vận hành và kỹ sư: bài test GPU-VRAM thực sự làm gì,
mỗi con số trên dashboard nghĩa là gì, và PASS chứng minh được đến đâu. Mô tả theo source hiện tại (đường
CUDA; Vulkan dùng cùng thuật toán và cùng oracle). Trạng thái các task nằm ở [STATUS.md](STATUS.md) và
[roadmap GPU-H](GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md).

## 1. Tổng quan: hai phần kiểm chạy cùng lúc

```text
            ┌──────────── (A) VRAM + PCIe ─────────────┐   ┌────────── (B) Tính toán ──────────┐
CPU sinh pattern ─H2D─► vùng VRAM ─D2H─► CPU so từng byte    corpus trong VRAM ─► kernel GEMM u32
                          │  (giữ ≥2 generation)                                   │
                          └──── đọc lại (retention) ─► CPU so    output ─D2H─► CPU so từng word với oracle
```

Nguyên tắc chung: không có kết quả nào được tính là *verified* nếu CPU chưa so **toàn bộ** dữ liệu với một
đáp án độc lập. Không phần nào dùng lấy mẫu.

Menu boot (lựa chọn 3, GPU-VRAM) chạy `--async-auto` từ 2026-09-24: GPU chạy CUDA dùng pipeline async GPU-H
(hai phần A và B chạy song song trên các stream riêng), GPU Intel/AMD chạy tuần tự (A rồi B trong mỗi
generation). Thuật toán và cách xác thực giống nhau ở cả hai chế độ; chỉ lịch chạy khác.

## 2. (A) Kiểm VRAM và PCIe

### 2.1 Vùng được kiểm

- **Menu (`--vram-max`, từ 2026-09-24):** sau khi mọi buffer khác (corpus, slot, lane) đã cấp phát, pool lấy
  đúng phần driver báo còn trống (`cuMemGetInfo`) trừ một biên cố định `--vram-margin-mib` (mặc định 128 MiB)
  cho driver và kernel. Chỉ áp dụng cho GPU rời chạy CUDA; GPU tích hợp (bộ nhớ là RAM hệ thống) và Vulkan giữ
  cách cũ. Report ghi `vram_fill` (chế độ, VRAM trống ngay trước khi cấp pool, biên). Đo trên GTX 960 có
  desktop, biên 256 MiB: `nvidia-smi` báo đúng 256 MiB trống trong lúc chạy.
- Không có `--vram-max`: 95% VRAM còn cấp phát được (`--vram-percent`), trong đó 5% dự phòng cho driver. Trên
  card 8 GB phần không được kiểm khoảng 380 MB; `--vram-max` còn 128 MiB.
- Vùng này được chia thành các allocation tối đa 1 GiB (`--max-allocation-mib`) và được quét lần lượt theo
  từng chunk. Một **sweep** là khi toàn bộ vùng đã được ghi và kiểm một lượt.

### 2.2 Ba bước cho mỗi chunk

Nguồn: `transfer_run` trong [gpu-cuda/src/lib.rs](../../crates/gpu-cuda/src/lib.rs).

1. **Ghi:** CPU sinh pattern thẳng vào bộ đệm pinned (không qua bản sao trung gian), rồi chép **H2D** vào VRAM.
2. **Đọc và so:** chép **D2H** ngược về, CPU so **từng byte**. Sai một byte là FAIL; report ghi vị trí, giá
   trị mong đợi và giá trị thực.
3. **Retention:** ít nhất 2 generation sau, và **bắt buộc trước khi vùng đó bị ghi đè**, chunk được đọc lại
   (chỉ D2H) và so lần nữa. Bước này bắt lỗi VRAM không giữ được dữ liệu theo thời gian, không chỉ lỗi
   ghi/đọc tức thời.

### 2.3 Pattern dữ liệu

Nguồn: [verifier.rs](../../crates/gpu-engine/src/verifier.rs) (`PatternClass`, `pattern_class`,
`transfer_pattern_into`; phiên bản pattern 2).

7 trên 8 generation dùng dữ liệu ngẫu nhiên sinh từ (seed, generation, địa chỉ), nên khoảng 50% số bit đổi
trạng thái giữa các lần ghi. Generation thứ 8 lần lượt dùng 6 pattern kinh điển:

| Pattern | Bắt loại lỗi |
|---|---|
| zeros / ones | bit kẹt ở 1 / kẹt ở 0 |
| checkerboard 0x55 / 0xAA (đảo) | nhiễu giữa các bit kề nhau; mỗi ô nhớ thấy cả hai cực |
| walking bit (một bit 1 mỗi word 64 bit, dịch theo địa chỉ và generation) | đường dữ liệu hỏng từng bit |
| address-in-address (mỗi word 64 bit chứa chính địa chỉ của nó) | lỗi đường địa chỉ, ghi nhầm chỗ |

Pattern là hàm thuần của (seed, generation, offset) nên được sinh lại khi cần, không lưu bản sao, và có thể
phát lại (replay) để điều tra một lỗi. Lần đọc retention dùng đúng generation lúc ghi, nên thấy cùng pattern.

### 2.4 Ở chế độ async

Phần A chạy trên **lane truyền tải riêng** (H6): stream CUDA, event và bộ đệm pinned riêng, chạy chồng lên
phần tính toán thay vì chen giữa các lần tính. Bộ lập lịch không cho hai thao tác chạm cùng vùng byte cùng
lúc, và vẫn giữ quy tắc kiểm retention trước khi ghi đè. Hàng đợi xác thực chia CPU công bằng giữa lane và
tính toán (DRR). Chi tiết: [GPU_H6_IMPLEMENTATION.md](GPU_H6_IMPLEMENTATION.md).

### 2.5 Quét DRAM trên GPU ở băng thông cao (`--vram-sweep`, menu)

Kiểm qua CPU bị giới hạn ở tốc độ PCIe và CPU (~1,2–1,4 GiB/s). Để ép DRAM ở băng thông thật, GPU tự ghi và
tự kiểm. Nguồn: [sweep.rs (engine)](../../crates/gpu-engine/src/sweep.rs),
[sweep.rs (CUDA)](../../crates/gpu-cuda/src/sweep.rs), kernel `dit_vram_sweep_fill/verify` trong
[gemm.cu](../../crates/gpu-cuda/kernels/gemm.cu).

- **Một pass:** ghi **toàn bộ** vùng rồi mới đọc kiểm **toàn bộ** vùng, để lượt đọc thật sự đi từ DRAM chứ
  không từ cache L2 còn giữ thứ vừa ghi. Mỗi lần gửi kernel tối đa 256 MiB, đọc/ghi 16 byte mỗi thao tác.
  Mỗi pass dùng khóa mới, nên mỗi bit đổi trạng thái với xác suất ~1/2 giữa hai pass.
- **Pattern:** `sweep_word` = xor địa chỉ với khóa rồi xorshift32, không có phép nhân. Với một khóa cố định,
  đây là song ánh theo địa chỉ: mọi word trong một allocation dưới 16 GiB có giá trị khác nhau, nên lỗi đường
  địa chỉ không lọt.
- **GPU tự kiểm, CPU kiểm lại theo mẫu:** kernel đếm word sai trên thiết bị; khác 0 là FAIL phase
  `vram-sweep`. Cứ ~2 s, ở ranh giới giữa hai pass, 16 MiB (xoay vòng qua cả vùng) được đọc về qua PCIe và CPU
  so với pattern sinh lại trên host. Sai là FAIL phase `vram-sweep-checkpoint`: bộ tự kiểm của GPU không còn
  đáng tin.
- **Chia vùng với lane:** mỗi allocation chia hai nửa. Lane DMA (CPU-verified) làm một nửa, bộ quét làm nửa
  kia, và **đổi nửa** khi cả hai đã phủ xong nửa của mình (tối thiểu 10 s một lần). Trước khi đổi, lane đọc
  lại mọi vùng còn chờ kiểm retention. Nhờ vậy cả CPU-verified lẫn device-verified đều phủ 100% pool.
- **Chia thời gian với GEMM:** stream của bộ quét có độ ưu tiên cao nhất. Duty mặc định 0.25: bộ quét chạy ở
  băng thông đầy đủ trong 1/4 thời gian GPU, GEMM giữ phần còn lại (`--vram-sweep-duty`,
  `--vram-sweep-share`). Trên GTX 960 hai loại tải chỉ chia thời gian, không chồng lên nhau: tổng hai tỉ lệ
  đã chuẩn hóa ở mọi mức duty/share thử đều ~1,0–1,06
  (`build/gpu-vram-sweep-tune-20260924-191831/`). Chưa đo trên GPU có khả năng chạy đồng thời tốt hơn.
- **Hai chỗ đã sửa khi đo:** (1) lệnh chép bộ đếm lỗi chỉ được gửi sau khi kernel xong, vì nếu xếp sau một
  kernel đang chạy nó chặn mọi lệnh D2H phía sau, kể cả của lane (độ phủ CPU-verified từng tụt 1.242 →
  845 MiB/s); (2) độ ưu tiên stream, vì không có nó bộ quét chỉ được xếp vào khe trống giữa các block GEMM
  (10 GiB/s thay vì 70 GiB/s lưu lượng).
- Report: `async_compute.sweep` và `detection.coverage.device_verified_bytes` /
  `device_verified_gib_per_second` / `sweep_checkpoint_cpu_verified_bytes`. **Device-verified được đếm riêng,
  không cộng vào CPU-verified.**

## 3. (B) Thuật toán tính toán: GEMM số nguyên 32 bit

### 3.1 Phép tính

Nhân ma trận số nguyên u32, lấy dư 2³² (wrap-around), nên kết quả **chính xác tuyệt đối** và so được
bit-by-bit, không cần sai số cho phép như số thực. Nguồn: kernel `dit_gemm` trong
[gemm.cu](../../crates/gpu-cuda/kernels/gemm.cu), corpus và oracle trong
[gemm.rs](../../crates/gpu-engine/src/gemm.rs).

| Tham số | Giá trị |
|---|---|
| Kích thước mỗi job | A (N×K) × B (K×N), N = 63, 64 hoặc 65 (theo seed) |
| K (chiều trong) | khởi đầu 1024 ở menu (256 cho lần chạy < 60 s, tăng 1 mỗi giây sau đó, trần 1024; bội số của 32 khi có INT8), rồi **tự tăng** (xem dưới); `--matrix-inner` đặt tay thì giữ nguyên, tối đa 4096 |
| Số job mỗi lần gửi | 512 ở async (`--async-auto` trên CUDA), 64 ở tuần tự; `--matrix-jobs` đặt tay |
| Phép tính mỗi job | 2·N²·K ≈ 8,4 triệu phép tính số nguyên (N=64, K=1024) |
| Output mỗi job | (N²+4) word ≈ 16 KB |
| Kernel | tile 16×16 trong shared memory, một thread một ô output |

**K tự tăng (v1.2.018, đường async).** Chi phí kiểm của CPU tỉ lệ với số word output, còn mỗi word chứa 2·K phép tính
của GPU. Khi CPU kiểm không kịp (≥ 2/3 số burst kể từ lần quyết định trước, tối thiểu 4 burst, ở trạng thái
`VERIFIER_BOUND`), corpus kế tiếp được dựng với K gấp đôi và số job một nửa, tới K tối đa 4096. Nguồn:
[shape.rs](../../crates/gpu-engine/src/shape.rs).

- **Chuẩn kiểm không đổi:** vẫn cùng GEMM, cùng oracle độc lập, CPU vẫn so **toàn bộ** từng word; chỉ mỗi word mang
  nhiều phép tính hơn. jobs × K giữ nguyên nên VRAM của corpus và công dựng oracle không đổi.
- **Chỉ tăng, không giảm:** ở cỡ mới CPU đã theo kịp, nên lùi một bậc sẽ lại nghẽn ở CPU.
- **Cái giá:** kernel dài hơn chiếm thời gian GPU lâu hơn, nên lane CPU-verified và bộ quét DRAM được ít thời gian hơn
  (GTX 960, K 1024 → 4096: lane −15 %, sweep −45 %). Vì vậy K chỉ tăng khi CPU kiểm là giới hạn.
- **Báo cáo:** `async_compute.inner_now` (K đang dùng), `async_compute.shape_steps` (thời điểm, lượt xoay corpus, jobs,
  K, % burst nghẽn ở CPU kiểm); số phép tính đã xác thực cộng theo K của từng corpus (`u32_ops_verified`,
  `f32_flops_verified`, `i8_ops_verified`). Dòng `COMPUTE MIX` hiện `K <đầu>-><hiện tại>`.
- Đo trên GTX 960 (90 s, chạy cạnh CPU-RAM 12 + 4 luồng): K 286 → 572 → 1144 → 2288 trong 11 s đầu, khoảng 152 G phép
  tính được kiểm/s so với ~80 G khi K cố định 256 cùng điều kiện; PASS, không mismatch.

### 3.2 Corpus và oracle

- Đầu vào là u32 ngẫu nhiên (splitmix64 theo seed và chỉ số word).
- CPU tính đáp án (**oracle**) **một lần cho mỗi corpus**, bằng vòng lặp vô hướng đơn giản, cố ý độc lập
  với cách GPU tính (không dùng tile, không dùng cùng thứ tự cộng).
- **Bản build theo mức ISA (v1.2.024, C13):** `gpu-vram-dit` có thêm `gpu-vram-dit-v3` (AVX2+FMA) và `-v4` (AVX-512) cạnh
  nó; bản cơ bản chọn bản rộng nhất CPU chạy được rồi `exec` sang, như `dit-cli` (chỉnh tay bằng `DIT_ISA` hoặc
  `dit_isa=`). Thứ tự phép tính của oracle không đổi, nên corpus và đáp án trùng từng bit giữa các bản
  (`tools/test_gpu_isa.sh`, chạy mỗi lần build extension). Dựng một corpus 128 job × K 4096 trên 3700X, 4 luồng: FP32
  1,42 → 0,23 s (bản cơ bản không có lệnh FMA, `mul_add` là lời gọi hàm cho từng phần tử), INT8 0,11 → 0,06 s, u32 không
  đổi (0,2 s). Report ghi `cpu_isa`.
- Corpus nằm sẵn trong VRAM; GPU tính lại nhiều lần với generation khác nhau. Ở async, cứ 256 batch thì đổi
  sang corpus mới (seed dẫn xuất, dựng sẵn ở nền).
- **Kho corpus (v1.2.025, C13 G2):** ngoài corpus đang dùng, pipeline giữ `--corpus-pool` corpus đã dựng (mặc định 3).
  Đến lượt đổi mà corpus dựng nền chưa xong thì GPU chuyển sang corpus cũ nhất trong kho thay vì chờ CPU; mỗi corpus
  trong kho vẫn có input, đáp án oracle và SHA-256 riêng, nên mỗi batch vẫn so toàn bộ output với đáp án. Kho tính vào
  ngân sách RAM và bị bỏ trước tiên khi thiếu RAM; đổi K (C7) thì xóa kho. Report: `async_compute.corpus_pool_serves`
  (số lần lấy từ kho), `workspace_budget.corpus_pool`/`corpus_pool_bytes`. Máy dev, CPU cố ý không kịp: phép tính đã
  kiểm 128 → 223 G/s, thời gian chờ corpus 43 % → 1 %.
- Cứ 512 batch, CPU băm lại corpus phía host và so với SHA-256 lúc dựng. Nếu RAM máy làm hỏng đáp án thì lỗi
  đó không bị đổ oan cho GPU. Từ v1.2.026 (C13 G3) lần kiểm định kỳ chạy trên luồng phụ cạnh các burst kế tiếp và
  báo ở ranh giới burst sau hoặc cuối phiên (`timing.integrity_background_seconds`); sau một batch sai, corpus vẫn
  được kiểm ngay trước verdict của burst.
- **Chép sẵn (v1.2.026, C13 G3):** corpus mà lần đổi sắp tới sẽ lấy được chép vào staging pinned trên luồng phụ trong
  lúc burst chạy, nên lúc đổi chỉ còn DMA lên VRAM (`corpus_staged_installs`); corpus bỏ đi được giải phóng trên luồng
  phụ. Không thêm RAM hay VRAM.

### 3.3 Chống kết quả cũ và ghi sai chỗ

Mỗi word output không phải là tích thô mà được trộn thêm:

```text
out = rotl(dot ^ gen_lo ^ rotl(gen_hi, 13), 7) ^ (job · 0x9E3779B9) ^ cell
```

cộng thêm hai word generation ở đầu mỗi job và hai word canh `0xA55A1973` ở cuối. Nhờ vậy:

- GPU trả lại output của lần trước thì sai, vì generation khác;
- ghi nhầm job hoặc nhầm ô thì sai, vì tag của ô khác;
- ghi tràn thì đè lên word canh.

Kernel có 5 kiểu lỗi cài sẵn để kiểm chính bộ xác thực: lật 1 bit kết quả, đọc nhầm hàng (lỗi địa chỉ),
bỏ sót một lần ghi, sai generation, đè word canh (`MatrixFault`). Test phần cứng
[crates/gpu-cuda/tests/g3.rs](../../crates/gpu-cuda/tests/g3.rs) chèn từng kiểu và yêu cầu bộ xác thực báo FAIL.

### 3.4 FP32 và INT8 Tensor core (`--compute-mix`)

Cùng bố cục, cùng cách gắn tag và cùng bộ xác thực như GEMM u32; chỉ khác kernel, đầu vào và oracle. Nguồn:
`Arith`/`ArithMix` trong [gemm.rs](../../crates/gpu-engine/src/gemm.rs), `dit_gemm_f32` trong
[gemm.cu](../../crates/gpu-cuda/kernels/gemm.cu), `dit_gemm_i8` trong
[tensor.cu](../../crates/gpu-cuda/kernels/tensor.cu).

| Kiểu | Phần cứng dùng tới | Vì sao kiểm được chính xác từng bit | Trạng thái |
|---|---|---|---|
| `u32` | ALU số nguyên | cộng/nhân lấy dư 2³² là phép chính xác | như mục 3.1 |
| `fp32` | pipe FP32 FMA | mỗi bước là **một** phép fused multiply-add IEEE-754 (làm tròn đúng một lần), cộng dồn theo đúng thứ tự k tăng dần ở cả kernel (`__fmaf_rn`) lẫn oracle CPU (`f32::mul_add`); đầu vào là số thực đủ 23 bit mantissa, có dấu, trong [0.5, 4) nên không tràn/không subnormal | **đã kiểm trên GTX 960**: khớp từng bit qua nhiều corpus, lỗi chèn bị bắt |
| `int8` | Tensor core (`mma.sync.m16n8k32 .s8.s8.s32`, compute capability ≥ 8.0) | nhân int8 cộng dồn int32; \|tổng\| ≤ 128²·4096 < 2³¹ nên không tràn | **đã kiểm trên RTX 5060 Ti** (CC 12.0, 2026-10-08): chế độ 3 cũ chạy 30 phút, 2,9·10¹¹ kết quả INT8 khớp oracle CPU từng bit, PASS; chưa có lần chạy trên CC 8.x (RTX 30/40). Cách tính địa chỉ theo bố cục fragment của PTX ISA, giống nhau từ `sm_80` ([tensor_layout.rs](../../crates/gpu-cuda/tests/tensor_layout.rs)) |
| `u32_chain` (v1.2.039, C9) | ALU số nguyên + VRAM | mỗi job `A·M₁·M₂·M₃·M₄·B` modulo 2³²: 4 tầng kc×kc (kc = min(K, 1024), chung mọi job, ngẫu nhiên và **khả nghịch mod 2**, kiểm bằng khử Gauss GF(2)) rồi chiếu xuống n×n bằng B ngẫu nhiên (mỗi hàng có số lẻ); dữ liệu trung gian nằm trong VRAM, XOR mặt nạ theo generation/tầng/vị trí để một tầng bỏ ghi lộ ra; output n×n gắn tag như GEMM, CPU so toàn bộ với oracle tính cả chuỗi | **đã kiểm trên GTX 960**: khớp từng bit qua nhiều corpus; 8 lỗi chèn (5 lỗi output, lật bit 31 một ô tầng 1, bỏ tầng 2, tầng 3 dùng nhầm M₁) đều FAIL; digest corpus trùng giữa bản v1 và v3 |

- FP16/BF16 trên Tensor core **không** được dùng: thứ tự cộng bên trong Tensor core không được đặc tả, nên
  không so từng bit được.
- `--compute-mix`: `u32` (mặc định CLI), `fp32`, `int8`, `alternate` (u32 và fp32 luân phiên mỗi lần xoay
  corpus), `all` (u32 → fp32 → int8 → chuỗi u32; bỏ int8 nếu GPU không có Tensor core), `chain` (chỉ chuỗi). Trong
  mix nhiều phép tính, mỗi corpus giữ khoảng bằng thời gian GPU mà `--corpus-rotate-batches` batch của phép tính nhanh
  nhất cần (v1.2.039): một batch chuỗi dài gấp hàng chục lần batch GEMM, không cân thì chuỗi chiếm gần hết GPU. **Menu (chế độ 1) và COMBINED dùng
  `all` từ v1.2.018**, sau khi INT8 đã khớp oracle trên RTX 5060 Ti; chế độ 3 chạy `alternate` (không INT8) cho card
  mà INT8 bị nghi lỗi phần mềm (xem mục 7).
- Corpus INT8 dùng N = 64 cố định (tile Tensor là 16×8) và cần K chia hết cho 32. Module Tensor là một PTX
  riêng build cho `compute_80`, chỉ nạp trên GPU hỗ trợ.
- Report tách: `detection.compute.verified_integer_ops`, `verified_fp32_flops`, `verified_int8_tensor_ops`
  (và `/s`), `async_compute.u32/f32/i8_results_verified`. Dashboard có dòng `COMPUTE MIX`.
- Trên GTX 960, kernel FP32 (tile 16×16 cơ bản) đạt ~190 GFLOP/s lúc chạy, khoảng 8% đỉnh lý thuyết: cùng
  giới hạn shared memory như kernel u32 (chưa tối ưu kernel, xem roadmap).

### 3.5 Xác thực

- CPU so **toàn bộ** output của mọi batch (`Corpus::verify_bytes`: theo khối 64 word, vector hóa, không cấp
  phát bản expected).
- SHA-256 của output: mọi batch ở tuần tự; theo checkpoint ở async (`--hash-policy`). Batch sai luôn được băm.
  Phép so đầy đủ thì luôn có, bất kể chính sách hash.
- Batch sai **không bao giờ** được cộng vào số đã xác thực. Ở async, slot chứa nó được đưa vào `QUARANTINE`
  (giữ lại có giới hạn số lượng và thời gian) để còn bằng chứng xác nhận.

## 4. Đọc đúng các con số trên dashboard

| Dòng | Nghĩa | Đổi ra thông lượng |
|---|---|---|
| CALCULATIONS COMPLETED / MATRIX RESULTS | số **word output** đã so khớp | × 2K = số phép tính (K=1024: × 2048) |
| VERIFIED … logical | số byte VRAM/PCIe CPU đã so (transfer + retention) | chia cho thời gian = độ phủ MiB/s |
| SWEEPS | số lượt đã phủ trọn vùng VRAM cấp phát | |
| DIT TARGET Allocated | VRAM **được cấp**, chưa phải VRAM **đã kiểm** | |
| Profile `CUDA GPU-H ASYNC` / engine `GPU-H-CUDA-1` | đang chạy pipeline async | `CUDA GPU-G3` / `GPU-G3-1` là tuần tự |
| DRAM SWEEP | byte GPU tự kiểm (device-verified), tốc độ, số pass, phần CPU đã kiểm lại qua checkpoint, số lần đổi nửa | đã là GiB/s |
| HARDWARE | lỗi phần cứng của GPU này trong lúc chạy (mục 6) | trạng thái `PASS (HW WARNINGS)` khi có cảnh báo |

Ví dụ: 2,45 tỷ *results* sau 20 s là khoảng **5 nghìn tỷ phép tính** (250 G phép tính/s), không phải 2,45 GB.
Dashboard hiện chỉ hiển thị số đếm, chưa hiển thị tốc độ (thuộc H7).

Report (`report.json`) có đủ số để tính lại: `matrix_results_verified`, `bytes_verified`,
`active_elapsed_seconds`, `allocation_sweeps_completed`; ở async thêm khối `async_compute` (regime, controller,
lane) và `bottleneck_summary`. File `bottleneck.jsonl` cạnh report ghi mỗi giây cái gì đang giới hạn tốc độ
([GPU_BOTTLENECK_LOG.md](GPU_BOTTLENECK_LOG.md)).

### 4.1 Số đo tham chiếu

GTX 960 (2 GiB), cấu hình menu, 95% VRAM, 30 s (`build/gpu-async-menu-20260924-131640/`):

| Chế độ | Compute đã xác thực | Độ phủ VRAM/PCIe đã xác thực | Ghi chú |
|---|---|---|---|
| Tuần tự (menu trước 2026-09-24), 20 s | 10,2 G phép tính/s | 898 MiB/s | |
| Async (menu ngày 2026-09-24 sáng) | 257,5 G phép tính/s | 1.409 MiB/s | pool 1.186 MiB quét trọn 8 lần; 57/57 burst giới hạn bởi GPU |
| Menu `--vram-max --vram-sweep` (u32), 60 s, biên 256 MiB | 199,8 G phép tính/s | 1.142 MiB/s, 34 lượt quét pool | **thêm 6,4 GiB/s device-verified** (386 GiB, 826 pass, 28 checkpoint CPU khớp); cùng máy không quét: 237,7 G và 1.238 MiB/s (`build/gpu-menu-sweep-20260924-193247/`) |
| **Menu hiện tại** (thêm `--compute-mix alternate`), K=1024, 60 s | u32 108,5 G + FP32 94,1 GFLOP/s = 202,7 G | 1.075 MiB/s | 6,5 GiB/s device-verified; 11 lần xoay corpus, đều dựng sẵn kịp (`build/gpu-menu-full-20260924-195348/`) |

Chưa có số đo trên RTX 50-series.

## 5. PASS nghĩa là gì, và điều gì chưa được kiểm

PASS nghĩa là **không thấy lỗi trong phạm vi đã kiểm**, không có nghĩa là phần cứng chắc chắn không có lỗi.
Những gì nằm ngoài phạm vi:

- **VRAM không được kiểm:** biên 128 MiB của `--vram-max` và phần driver hoặc chương trình khác đang giữ.
  Vùng chứa corpus và output chỉ được kiểm gián tiếp qua kết quả GEMM.
- **Tensor core mới được kiểm trên RTX 5060 Ti (CC 12.0)**, chưa trên CC 8.x; menu bật INT8 cho mọi GPU từ CC 8.0
  vì bố cục `mma.sync` do PTX ISA quy định, giống nhau giữa các thế hệ. Kernel GEMM (u32 và FP32) là loại tile cơ
  bản, chỉ đạt một phần nhỏ công suất đỉnh.
- **Device-verified là GPU tự kiểm chính nó.** Checkpoint CPU theo mẫu chứng minh bộ tự kiểm đúng, nhưng không
  phải mọi byte device-verified đều được CPU so. Hai con số luôn được báo tách riêng.
- Workload `--vram-local` của H6 vẫn có nhưng không bật ở menu: trên GTX 960 nó hạ độ phủ đã xác thực xuống
  520 MiB/s. `--vram-sweep` thay thế nó ở menu và không dùng chung cờ với nó.
- Intel/AMD (Vulkan) chưa có pipeline async (H4b), nên không có `--vram-sweep` và `--vram-max`.

## 6. Lỗi phần cứng trong lúc chạy

Nguồn: [hwhealth.rs](../../crates/gpu-cli/src/hwhealth.rs) (`assess_health`). Mỗi run gom và chấm cho **riêng
GPU đang test**:

| Nguồn | Cách đọc | Không đọc được thì |
|---|---|---|
| Kernel log (`journalctl -k`) | từ lúc bắt đầu run, đọc lại mỗi 10 s để dashboard báo ngay | `unavailable`, không phải "sạch" |
| PCIe AER của GPU và của port phía trên | bộ đếm sysfs lúc đầu và lúc cuối, lấy hiệu | ghi vào `sources_unavailable` |
| ECC / row-remap (NVIDIA) | `nvidia-smi` lúc đầu và lúc cuối | GeForce báo N/A: ghi là không có dữ liệu |

Chỉ tính dòng log nêu địa chỉ PCI của GPU này hoặc của port phía trên nó. Dòng nêu thiết bị khác bị bỏ qua (máy
nhiều GPU: lỗi card này không làm FAIL card kia). Dòng không nêu thiết bị nào thì lỗi nặng chỉ bị hạ thành cảnh
báo.

| Mức | Gồm | Kết quả |
|---|---|---|
| `errors` | Xid 48, 64, 74, 79, 94, 95, 119, 120, 140, 154; GPU mất khỏi bus; AER uncorrected (non-fatal/fatal); ECC uncorrected; row remap thất bại | thêm lỗi phase `hardware` → **FAIL**, tách khỏi FAIL dữ liệu |
| `warnings` | AER corrected; ECC corrected; row remap đang chờ; mọi Xid khác (13, 31, 43, 45, 63, 92…); lỗi driver amdgpu/i915 | vẫn **PASS**, nhưng dashboard ghi `PASS (HW WARNINGS)` và dòng HARDWARE, menu in `HARDWARE WARNINGS` |
| `clean` | đọc được kernel log và không có gì | PASS thường |
| `unavailable` | không đọc được kernel log | PASS thường, report ghi rõ không thể loại trừ lỗi đã sửa |

Report: `hardware_status` ở cấp cao nhất và `detection.hardware_health` (lý do, số lỗi/cảnh báo, nguồn không đọc
được), `detection.kernel_log.findings.xid_codes`, `detection.ecc`. Đã thử trên GTX 960 bằng dòng log giả chèn
qua `/dev/kmsg`: Xid 13 → PASS + `warnings`; Xid 48 → FAIL `hardware` (`build/gpu-hwhealth-20260924-190128/`).

## 7. Xác nhận INT8 Tensor core trên GPU compute capability 8.0+ (RTX 30/40/50, A-series trở lên)

Chạy thẳng từ menu: **Control center → 3 (GPU-VRAM) → 2 (Tensor INT8 check)**. Khi chọn GPU-VRAM, menu hỏi chế độ:

| Chế độ | Chạy gì |
|---|---|
| 1 (mặc định, Enter) | test đầy đủ: u32 + FP32 + INT8 Tensor luân phiên (`--compute-mix all`; INT8 chỉ trên CC ≥ 8.0), quét DRAM, giữ VRAM tới giới hạn |
| 2 Tensor INT8 check | chỉ GEMM INT8 Tensor core trong 2 phút (`--compute-mix int8 --matrix-inner 1024 --duration-seconds 120`); dùng cho dòng card chưa từng chạy INT8 |
| 3 Full DIT without INT8 | như chế độ 1 nhưng chỉ u32 + FP32 (`--compute-mix alternate`), cho card mà chế độ 2 báo FAILED |
| D | thời lượng cho chế độ 1 và 3: 10 / 30 (mặc định) / 60 / 120 phút hoặc tự nhập 1–1440 |
| T | giới hạn nhiệt GPU: tắt (mặc định, nhiệt vẫn được ghi) hoặc bật ở 90 °C (`--temp-limit-celsius 90`) |

Kết thúc chế độ 2, menu in một dòng diễn giải:

- `TENSOR CHECK PASSED`: có N phép tính INT8 khớp oracle CPU từng bit; chế độ 1 chạy INT8 trên loại card này.
- `TENSOR CHECK NOT AVAILABLE`: GPU không có đường INT8 Tensor này (compute capability dưới 8.0, gồm cả
  Volta/Turing). Run kết thúc ngay, report `INVALID` ghi lý do. Dùng chế độ 1.
- `TENSOR CHECK FAILED`: kết quả khác oracle. Nếu **mọi** batch sai ngay từ đầu, và sai trên nhiều card, nhiều khả
  năng là lỗi phần mềm trong cách sắp xếp fragment, **không** kết luận là lỗi phần cứng; giữ lại `report.json`.
  Lỗi phần cứng thật thường thưa, ngẫu nhiên và chỉ trên một card. Trong lúc chờ, dùng chế độ 3 (không INT8).
- `TENSOR CHECK NOT RUN`: không có GPU CUDA nào chạy đường Tensor (ví dụ chỉ có GPU Intel/AMD).

Report và `bottleneck.jsonl` được lưu vào `DIT-RESULTS/runs/<run>/gpu-vram/` như mọi lần chạy khác. Đã thử trên GTX 960
(CC 5.2): chế độ 2 kết thúc ngay với `INVALID` và lý do "needs compute capability 8.0", menu in `NOT AVAILABLE`.
Trên RTX 5060 Ti (CC 12.0, 2026-10-08, máy B760M): chế độ "Full DIT + INT8" cũ (nay là chế độ 1) chạy 30 phút,
2,9·10¹¹ kết quả INT8 khớp oracle, PASS (`Result/1/runs/suite-gpu-vram-1791460766558-762/`). Chưa có lần chạy trên
CC 8.x (RTX 30/40): nên chạy chế độ 2 một lần khi có máy.
