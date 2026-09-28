# GPU-H2 — Verifier ít CPU, chính sách hash, corpus xoay

Trạng thái 2026-09-20: H2 `implemented` cho đường CUDA (source/software + bounded GTX 960
A/B). Vulkan chỉ nhận phần dùng chung (pattern nhanh, pattern hệ thống); đường ma trận và
so sánh/hash của Vulkan chưa đổi. H3–H8 chưa triển khai. Không coi A/B một lần chạy trên
một máy là benchmark; không phải run near-full VRAM, soak hay AMD/Intel.

## Đã triển khai

**Verifier** ([gemm.rs](../../crates/gpu-engine/src/gemm.rs)). `Corpus::verify_bytes` xử lý theo
job và khối 64 word, gom sai khác bằng OR không rẽ nhánh (trình biên dịch tự vector hóa, không
cần SIMD thủ công) và chỉ quét lại khối có sai khác để trả đúng mismatch đầu tiên. Kết quả
giống định nghĩa word-by-word cũ, được kiểm bằng differential test ngẫu nhiên (hàng nghìn
tổ hợp lỗi ở header, payload, biên khối, guard, độ dài). Microbenchmark 17,3 MB: 18,1 → 2,4–3,1 ms
(≈960 → 5.500–7.100 MB/s), gấp 5,7–7,4 lần tùy lần chạy.

**Dựng corpus song song** (`Corpus::new_with_threads`). Sinh input và oracle chia theo khoảng
word/job; SHA-256 của input chạy song song với oracle; kết quả bit-giống bản đơn luồng với mọi
số luồng (test). 256 job × K=512: 0,15 → 0,06 s với 4 luồng. Số luồng tự chọn tối đa một phần
tư số core để chừa chỗ cho verifier, UI và watchdog.

**Chính sách hash** ([verifier.rs](../../crates/gpu-engine/src/verifier.rs)). `--hash-policy
every-batch|checkpoint|checkpoint:<n>`. Mặc định là `checkpoint` (mỗi 64 batch, cộng batch pass
cuối của mỗi burst) với `--async-compute`, và `every-batch` cho đường tuần tự để các gate
CUDA/Vulkan cùng hash giữ nguyên nghĩa. So sánh luôn đầy đủ; khi PASS thì actual bằng expected nên
hash chỉ là bằng chứng. Batch lỗi luôn được hash actual (`failure_hashes`). Khi hash bị bỏ,
`result_sha256` để rỗng (không phải hash giả). Vulkan từ chối `checkpoint` bằng report INVALID
thay vì lặng lẽ bỏ qua.

**Đường tuần tự CUDA ít copy.** Pattern transfer sinh thẳng vào buffer pinned; so sánh trực tiếp
trên buffer readback bằng memcmp theo lát 4 KiB (chỉ quét byte ở lát có sai); không còn
`expected()`, `words(input)`, `Vec` readback và bản sao hash mỗi dispatch. Pattern sinh trên
nhiều luồng khi ≥4 MiB (kết quả bit-giống bản gốc, test).

**Corpus pool xoay có prefetch** ([async_compute.rs](../../crates/gpu-cuda/src/async_compute.rs)).
Sau `--corpus-rotate-batches` batch (mặc định 256, 0 = tắt) pipeline chuyển sang corpus mới suy ra
từ (seed gốc, số lần xoay) bằng `derive_corpus_seed`; corpus kế tiếp được dựng nền khi corpus
hiện tại đang dùng, chỉ đổi ở ranh giới burst và không có batch nào còn treo. Seed suy ra làm N
lần lượt 63/64/65. Batch mang seed hiệu lực nên replay dựng lại được. Ngân sách RAM đã dự trù bản
corpus thứ hai (`plan_with_options`); thiếu RAM thì giảm jobs, không vượt reserve.

**Kiểm tra toàn vẹn corpus ở host.** Corpus lưu thêm SHA-256 của kết quả kỳ vọng. Sau mỗi batch lỗi
và định kỳ mỗi 16 burst, adapter băm lại bản input và expected trong RAM host. Báo `corpus_intact` hay
`host_memory_changed` (`failure_attribution`); nếu corpus đổi thì đó là FAIL host-memory, không đổ cho
GPU. `corpus_intact` chỉ loại trừ hư hại corpus, không chứng minh linh kiện nào lỗi.

**Pattern hệ thống cho transfer/retention** (schedule phiên bản 2). Mỗi thế hệ thứ tám là pattern hệ
thống, lần lượt: all-0, all-1, checkerboard, checkerboard đảo, walking-bit, address-in-address; các thế hệ
còn lại vẫn là splitmix ngẫu nhiên. Lịch tất định theo generation nên retention thấy cùng lớp với lần
ghi. Replay descriptor transfer/retention nay là version 2 (v1 vẫn hợp lệ, nghĩa là pattern ngẫu nhiên
cũ). Vulkan dùng chung hàm nên cũng chạy các pattern này (đã chạy PASS trên GTX 960). Report ghi số thao
tác theo lớp trong `detection.coverage.pattern_classes`.

## Kết quả A/B (GTX 960, Ryzen 7 3700X, seed 7, pool 64 MiB, một lần chạy mỗi ô)

Evidence: `build/gpu-h2-ab-20260920-185044/` (`SUMMARY.md`, `summary.json`, 24 run + 2 negative +
`verifier_speed.txt`), so với H1 tại `build/gpu-h1-baseline-20260920/`. Steady 20 s:

| K | verify core-s/GiB H1 → H2 | verify MiB/s/core H1 → H2 | transfer ms/op H1 → lean | async Gop/s H1 → H2 |
|---|---|---|---|---|
| 256 | 1,650 → 0,188 | 621 → 5.452 | 66 → 12 | 80 → 187 |
| 1024 | 1,617 → 0,174 | 633 → 5.876 | 69 → 12 | 168 → 234 |
| 4096 | 1,578 → 0,170 | 649 → 6.041 | 74 → 12 | 197 → 246 |

Chi phí CPU mỗi GiB verified giảm ≈89% (giả thuyết ≥50% của roadmap đạt). Tách yếu tố: chỉ bỏ hash mỗi
batch đã giảm từ 0,66–0,69 core-s/GiB (`async_hash_every`) xuống 0,17–0,19; so sánh nhanh một mình đưa
1,6 → 0,67. SHA-256 (8,2 ms/17 MB) đắt hơn phép so sánh mới (2,4 ms), nên hash mỗi batch là chi phí
chính còn lại nếu bật lại. Corpus xoay: steady K=256/1024/4096 xoay 7/4/2 lần, mọi lần trúng prefetch,
1 lần dựng nguội lúc khởi động; kiểm tra toàn vẹn tốn ≈0,15 s mỗi lần (≈1,4% của run 20 s ở K=1024).
Pattern hệ thống xuất hiện trong mọi run steady (zeros, ones, checkerboard, đảo, walking, address).

## Hai điều chỉnh so với kết luận H1

1. **Lợi thế của async nhỏ hơn con số H1.** Con số "≈10–14×" so với một đường tuần tự còn lãng phí.
   Sau khi làm sạch, tuần tự `seq_lean` đạt 64/110/158 Gop/s (K=256/1024/4096) và async chỉ hơn
   **2,9× / 2,1× / 1,6×**. Async vẫn thắng rõ nhất khi K nhỏ (verifier-bound cũ) và dùng nhiều công suất hơn,
   nhưng phần lớn lợi ích H1 đến từ việc bỏ lãng phí, không phải từ overlap.
2. **Độ phủ VRAM/PCIe của async so với tuần tự tệ hơn, không tốt hơn.** Transfer/retention nhanh gấp 7–9× ở
   tuần tự (871–1.399 MiB/s) nhưng async chỉ 56–134 MiB/s vì mỗi burst 32 batch chạy nối tiếp giữa các
   transfer: độ phủ async bằng **6–10%** tuần tự tốt nhất (H1: 34–54%). Tuyệt đối async tốt hơn H1 (1,1–2,1×),
   nhưng floor ≥80% ở mục 3.3 của roadmap càng cần scheduler (H5 time-slice, H6 chồng lấn thật) chứ không phải
   thứ H2 giải quyết được.

## Chưa làm

- Vulkan: đường ma trận (`expected()` + so sánh + hash mỗi dispatch), so sánh/hash transfer. Vulkan
  giữ hash mọi output và từ chối `--hash-policy checkpoint`.
- Worker nhận việc từ hàng đợi chung/least-loaded: đã làm ở H6 cho đường CUDA (hàng đợi verifier công bằng thay `slot % workers`,
  xem [GPU_H6_IMPLEMENTATION.md](GPU_H6_IMPLEMENTATION.md)); Vulkan chưa có.
- Hash actual của batch lỗi trên đường tuần tự (hiện chỉ async có `failure_hashes`); kiểm tra toàn vẹn
  corpus trên đường tuần tự.
- Đổi K/jobs khi xoay corpus (cần đổi cỡ buffer); N đã đổi theo seed.
- Không cần `wide`/SIMD thủ công: đã đạt mức memory-bound gần đúng, chưa đo trên CPU khác.
- Controller vẫn v0 (H5): vẫn ≈20 sự kiện throttle mỗi 20 s ở các run async, không thay đổi ở H2.

## Gate

`make gpu-h2-check` = fmt, Clippy strict, test, microbenchmark (sàn 3×) và, với `DIT_GPU_HARDWARE=1`, test
G3 (CUDA/Vulkan cùng hash, 5 kiểu lỗi phía device), ma trận 24 run và kiểm bằng `jq`: policy đúng, bảo toàn,
`hashes_computed == verified` ở every-batch và `×8 < verified` ở checkpoint, xoay corpus trúng prefetch,
integrity check chạy và sạch, có pattern hệ thống; negative async (FAIL, `mismatch_words=256`, một hash lỗi,
`corpus_intact`) và negative tuần tự dưới policy checkpoint. Ngoài gate: `cargo test -p gpu-vulkan --
--ignored` và một run Vulkan 20 generation PASS với pattern hệ thống.
