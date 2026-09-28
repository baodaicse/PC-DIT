# ADR-0004: Đánh giá độ nhạy phát hiện lỗi CPU kiểu Raptor Lake (14900K)

- Status: accepted — Phase 1 và Phase 2 implemented 2026-08-31
- Date: 2026-08-31

## Trạng thái triển khai (2026-08-31)

Cả Phase 1 (compute-oracle độc lập) và Phase 2 (chạy đồng thời với route RAM
trong `system-run`) đã `implemented`; xem [STATUS.md](../STATUS.md) mục "Bước 3B".

- Phase 1: module `dit-system::compute_oracle` và lệnh `dit-cli compute-oracle-run`
  hiện thực hoá Recommendation #1, #3 (một phần, qua kernel dense FMA thay vì
  AVX-512 riêng) và #4.
- Phase 2: `system-run` nhận thêm `--compute-oracle-workers`; khi bật, các
  compute-oracle worker được pin vào CPU dư (không trùng CPU của RAM worker),
  spawn trước round loop và chỉ join sau khi round loop kết thúc — tạo overlap
  wall-clock thật giữa RAM DIT và compute-oracle, đúng thiết kế "Coordination
  design" đã đề xuất trước đó. Report schema bump lên v9, thêm
  `SystemComputeOracleEvidence` tách biệt khỏi `SystemMismatch` của RAM.
- Recommendation #2 (idle/burst ramp) có ở cả hai: pha `idle_ramp` tuỳ chọn
  trong compute-oracle độc lập VÀ khi ghép vào `system-run`
  (`--compute-oracle-idle-ramp`).
- Verdict tổng hợp là AND của cả hai oracle (RAM và compute), đúng khuyến nghị
  "gộp một test, tách evidence" đã chốt trước đó: PASS/FAIL dùng chung, nhưng
  `mismatch` (RAM) và `compute_oracle.mismatch` (compute) luôn tách biệt trong
  report, kể cả trong `failure_reason`.

Device evidence thật (16 logical CPU): RAM worker dùng CPU `[0, 1]`,
compute-oracle 4 worker dùng CPU `[2, 3, 4, 5]` (không trùng), chạy PASS đồng
thời; tiêm lỗi vào 1 compute-oracle worker cho FAIL đúng với
`reason: compute-oracle dense_fma_cross_worker mismatch...` trong khi RAM vẫn
`exact_verified_unique_bytes` đủ target — đúng fingerprint "RAM sạch nhưng
compute sai" mà thiết kế này nhắm tới.

### 2026-09-01 — Sửa: dense-FMA chỉ được kiểm ở burst cuối cùng

Rà soát lại độ hiệu quả theo thời gian (xem [STATUS.md](../STATUS.md) mục cùng
ngày) phát hiện scalar-reference tolerance check của dense-FMA trước đây chỉ so
sánh burst **cuối cùng** của cả phiên, không phải mọi burst — một corruption
thoáng qua giữa phiên dài (Full profile 30–60 phút, hàng nghìn burst) có thể
không được phát hiện nếu burst cuối tính đúng. Đã sửa để kiểm tra mọi burst
(`dense_fma_mismatches` cộng dồn, giống cách `branch_predictor`/`aes256` đã làm
từ đầu); `ComputeOracleReport.schema_version` bump 1 → 2. Seed vẫn cố định
suốt phiên theo thiết kế (không đảo mỗi burst) để giữ đúng bất biến cross-worker
consistency check.

## Context

Người vận hành đã chạy CPU-DIT-01 trên một số máy Intel Core i9-14900K và nghi
ngờ hiệu quả phát hiện lỗi không cao: không có FAIL/crash rõ ràng từ tool, nhưng
CPU dòng Raptor Lake (13th/14th Gen) được cộng đồng và Intel xác nhận có lỗi tính
toán do điện áp vận hành bị đẩy quá cao qua thuật toán eTVB/SVID, kết hợp với cấu
hình mainboard "unlimited power". Lỗi này gây sai lệch tính toán âm thầm (silent
compute corruption), thường lộ ra trong thực tế qua workload compute-bound, sustained
cao (biên dịch code, video encode, Chromium/renderer, decompression, AVX-512
ray-tracing) — tức tải nặng vector/FMA kéo dài đủ lâu để package power/voltage đạt
đỉnh và có nhiều lần chuyển P-state — chứ không phải loop RAM bandwidth-bound đơn
giản. Các công cụ cộng đồng hay dùng để tái hiện lỗi này (y-cruncher, Prime95
small-FFT, CoreCycler) đều dựa trên workload FMA/AVX dày đặc và sustained.

Yêu cầu review: đánh giá lại phần chia dữ liệu (split/window), phần verify và
công thức tính toán hiện tại trong `dit-core`/`dit-system` xem có tiệm cận với
loại workload nói trên hay không, độc lập với vụ false-positive AUXTIN2/AUXTIN4
đã sửa ở Temperature Policy v2 (đó là lỗi sensor, không liên quan độ nhạy phát
hiện lỗi tính toán).

## Evidence: workload primitive hiện có

| Primitive | File | Compute hay memory-bound | Sustained hay one-shot | Có khớp pattern kích hoạt Raptor Lake không |
|---|---|---|---|---|
| RAM fill/verify theo window/pattern | [crates/dit-system/src/window.rs](../../crates/dit-system/src/window.rs), `memory.rs` | Memory-bandwidth-bound | Sustained suốt round | Không — đây là stress RAM/bus, không đẩy CPU vào sustained cao-power vector state |
| `scalar_reduction`/`simd_reduction` | [crates/dit-system/src/cpu.rs](../../crates/dit-system/src/cpu.rs) | Nhẹ: chỉ XOR + `wrapping_add` trên `wide::u64x4` (256-bit, không AVX-512, không FMA) | Sustained (chạy trên toàn bộ arena mỗi round) nhưng mật độ phép tính/byte rất thấp | Không — không tạo đủ power/current draw hay áp lực FMA unit |
| `integer_known_answer_test` | `cpu.rs` | Compute, nhưng input cố định 256 byte | One-shot mỗi lần cpu_tests_completed, không lặp trong round | Không — quá ngắn để ảnh hưởng nhiệt/điện áp |
| `fp_fma_known_answer_test` | `cpu.rs` | Compute (dùng `mul_add`, tức FMA scalar) | One-shot 100.000 lần, chạy đúng một lần đầu phiên | Gần nhất với FMA thật nhưng quá ngắn và không sustained toàn phiên |
| `coherency_test` (atomic `fetch_add` SeqCst) | `cpu.rs` | Bound bởi cache-line ping-pong/bus lock, không phải ALU/FMA | Sustained | Không — đây là coherency stress, không phải power-virus compute |
| GF(256) Vandermonde matrix encode/decode | [crates/dit-core/src/gf256.rs](../../crates/dit-core/src/gf256.rs), `pipeline.rs` | Compute, nhưng scalar bit-loop (8 vòng shift/XOR mỗi phép nhân), không SIMD | One-shot, giới hạn `MAX_REFERENCE_BYTES` = 64 MiB, chạy trước route hệ thống dài hạn | Không — route reference tách biệt, không nằm trong phần sustained lâu dài |
| `dit-cli` flags | [crates/dit-cli/src/main.rs](../../crates/dit-cli/src/main.rs) | — | — | Không có flag/profile nào bật workload AVX-512/FMA sustained kiểu "power-virus" |

Kết luận evidence: toàn bộ phần compute hiện có được thiết kế làm **oracle
correctness** (đúng/sai bit-exact, đối chiếu golden vector) chứ không nhằm tối đa
hoá package power/voltage transition. Đây khớp với nguyên tắc sản phẩm đã ghi ở
[PRODUCT_SPEC.md](../PRODUCT_SPEC.md) mục 3.4: floating-point là test riêng có
tolerance, "không dùng benchmark/utilization/nhiệt độ làm oracle PASS" — nghĩa là
sản phẩm cố tình không tối ưu theo hướng power-virus. Vì vậy phần lớn khoảng
trống ở đây là **giới hạn phạm vi có chủ đích của M0–M5**, không phải lỗi cài đặt.

## Gap cụ thể

1. **Không có workload compute-bound sustained**: không kernel nào (SIMD hay
   scalar) chạy đủ dày và đủ lâu để đẩy package power/voltage lên vùng mà lỗi
   Raptor Lake thường lộ ra. `simd_reduction` mật độ phép tính rất thấp so với
   FMA/AVX-512 dense kernel.
2. **Không có P-state/voltage transition stress**: engine không có pha chủ đích
   luân phiên idle↔full-load (turbo ramp) — chính kiểu chuyển trạng thái này là
   nơi thuật toán eTVB dễ tính sai điện áp nhất theo phân tích cộng đồng.
3. **`fp_fma_known_answer_test` và `integer_known_answer_test` chỉ one-shot**:
   chạy đúng một lần ở đầu phiên, không lặp lại xuyên suốt round dài, nên không
   có cơ hội bắt lỗi compute xuất hiện muộn sau khi CPU đã nóng/đã throttle.
4. **Không có oracle differential cho AVX-512** (nếu CPU hỗ trợ): hiện `wide`
   crate chỉ dùng 256-bit; các báo cáo lỗi Raptor Lake thực tế nhiều khi liên
   quan tới workload AVX-512 (ví dụ y-cruncher, một số renderer).
5. **`dit-cli` không có profile "compute stress"** tách biệt khỏi profile
   RAM-centric hiện tại (Safe/Full), nên người vận hành không có cách chọn một
   bài test thiên compute để tăng khả năng bắt lỗi kiểu này.

## Recommendations (chỉ đề xuất, chưa triển khai)

1. Thêm một compute-oracle mới chạy sustained trong suốt round (không phải
   one-shot): dense FMA/AVX kernel có scalar reference độc lập kiểm bit-exact,
   tương tự cách `scalar_simd_differential` đã làm cho reduction hiện tại.
2. Thêm một pha luân phiên tải cao/thấp (busy↔idle ramping) có chủ đích trong
   plan của round, để tăng số lần chuyển P-state/voltage trong một phiên dài.
3. Cân nhắc thêm route AVX-512 differential (khi CPU hỗ trợ, phát hiện qua
   `CpuInventory`) chạy song song oracle scalar, giữ nguyên nguyên tắc "encoder
   tối ưu không tự xác minh chính nó" đã có trong `ARCHITECTURE.md` mục 4.
4. Thêm một profile CLI riêng (ví dụ `--compute-stress`) để người vận hành chọn
   bài test thiên sustained-compute thay vì RAM-centric mặc định, có report
   schema riêng để không phá vỡ semantics Safe/Full hiện tại.
5. Mọi thay đổi trên vẫn phải giữ nguyên tắc PRODUCT_SPEC mục 5 (không dùng
   benchmark/utilization/nhiệt độ làm oracle) — sai số phải được phát hiện bằng
   so sánh bit-exact với scalar reference, không suy luận từ crash/nhiệt độ.

## Coordination design: chạy đồng thời RAM DIT và compute-oracle

Mục tiêu là chạy compute-oracle mới **song song thật** (đồng thời về wall-clock)
với route RAM hiện có trong cùng một round, để tổng package power/voltage giống
tải thực tế hỗn hợp (vừa memory traffic vừa compute vector), đồng thời vẫn giữ
được kết quả PASS/FAIL của hai oracle tách biệt, không quy lẫn lỗi.

Quan sát nền tảng từ `engine.rs`: `run_system` hiện tính
`worker_count = all_worker_cpus.len().min(arenas.len())` rồi chỉ dùng
`all_worker_cpus[..worker_count]` cho RAM workers; nếu số logical CPU nhiều hơn
số arena, các CPU còn lại (`all_worker_cpus[worker_count..]`) không làm gì trong
suốt round. Ngoài ra `run_write_phase` và `run_verify_phase` mỗi cái tự
`join_workers()` trước khi phase sau bắt đầu, nên một nhóm thread muốn chạy
"đồng thời" xuyên suốt round phải có vòng đời độc lập với hai phase đó, không
nằm lồng bên trong.

Thiết kế đề xuất:

1. Chia CPU pool mỗi round thành `ram_worker_cpus` (giữ arena, chạy write→verify
   tuần tự như hiện tại) và `compute_worker_cpus` (phần CPU dư, hoặc SMT-sibling
   của `ram_worker_cpus` nếu muốn cộng dồn power draw trên cùng physical core).
2. Spawn thread cho compute-oracle **trước** khi gọi `run_write_phase`, dùng
   chung `Arc<RunControl>` cooperative-stop token hiện có; các thread này chạy
   xuyên suốt cả write lẫn verify phase của round, chỉ `join` lại sau khi
   `run_verify_phase` hoàn tất — đây là điểm mấu chốt tạo overlap thật thay vì
   chạy tuần tự.
3. Kernel compute phải sustained (nhiều vòng lặp FMA/AVX trong toàn bộ round,
   không one-shot) và có scalar reference độc lập kiểm bit-exact, input xác
   định từ round seed (giống cách `expected_byte` sinh pattern hiện tại) để
   mismatch replay được bằng `--seed`.
4. Thêm evidence/stage tách biệt để không lẫn attribution:
   - `SystemMismatch.stage = "COMPUTE_ORACLE_DIFFERENTIAL"`, khác với
     `RAM_EXACT_VERIFY`/`MATRIX_ENCODE` đang có.
   - `ComputeOracleEvidence` mới (worker_id, cpu_id, kernel_id, iterations,
     mismatches, sustained_ms) tương tự `CpuEvidence` hiện có.
   - PASS đòi cả `coverage_target_met` (RAM) và `compute_oracle_passed` đều
     đúng; nếu chỉ compute-oracle fail còn RAM sạch, đây là fingerprint đặc
     trưng của lỗi kiểu Raptor Lake (ALU/FMA sai dưới điện áp, không liên quan
     RAM/cache) và phải được báo cáo tách biệt, không gộp vào
     `RAM_EXACT_VERIFY`.
5. Compute worker cũng phải tự kiểm `control.should_stop()` định kỳ như
   `verify_worker` đang làm, để thermal/EDAC/deadline abort dừng được cả hai
   nhóm cùng lúc, giữ đúng nguyên tắc evidence-only diagnosis ở ADR-0003.

Thiết kế này chỉ là đề xuất phối hợp lịch trình; kernel compute cụ thể và
report schema field vẫn theo khuyến nghị 1–4 ở mục trên, chưa triển khai trong
review này.

### Gộp hay tách hai test: khuyến nghị

Nên **gộp thành một phiên chạy phối hợp** (chạy đồng thời trong cùng
`system-run`), không phải hai lệnh CLI riêng chạy tuần tự — vì chạy tuần tự sẽ
mất đúng điều kiện muốn tạo ra: combined package power draw giống tải thực tế
(vừa memory traffic vừa compute vector cùng lúc). Nhưng verdict cuối và evidence
phải giữ tách biệt theo oracle:

- Exit code/PASS tổng thể là **AND** của cả RAM oracle và compute-oracle (một
  trong hai fail thì tổng thể FAIL), cộng dồn vào `failures_total` như cơ chế
  hiện có.
- Report vẫn phải ghi riêng `stage = RAM_EXACT_VERIFY` so với
  `COMPUTE_ORACLE_DIFFERENTIAL`, để giữ được fingerprint "RAM sạch nhưng compute
  sai" — dấu hiệu đặc trưng của lỗi kiểu Raptor Lake, sẽ mất nếu gộp thành một
  verdict không phân biệt.

Triển khai nên chia hai giai đoạn để giảm rủi ro, đúng tinh thần
IMPLEMENTATION_PLAN.md ("không tối ưu trước khi oracle đã chạy đúng"):

1. Làm compute-oracle như một sub-command/profile độc lập trước, tự kiểm
   scalar reference/golden vector riêng, không đụng vào schema v7/v8 đã có
   device evidence — giống cách route reference (`dit-core`) và route hệ thống
   (`dit-system`) hiện cũng đang là hai command/report tách biệt.
2. Sau khi compute-oracle tự đứng vững, mới tích hợp chạy đồng thời trong
   `system-run` theo thiết kế coordination ở trên, bump schema version và cập
   nhật validator/device test.

## Tái sử dụng từ app Python cũ: ánh xạ cụ thể vào compute-oracle

Bối cảnh đầy đủ và bảng so sánh tính năng nằm ở
[BINARY_PROVENANCE_REVIEW_CPU-DIT-EXE_2026-08-31.md](../BINARY_PROVENANCE_REVIEW_CPU-DIT-EXE_2026-08-31.md).
Mục này chỉ chốt lại: **cái gì tái sử dụng được (ở mức ý tưởng/thuật toán,
không tái sử dụng code Python trực tiếp)** và ghép vào đâu trong thiết kế
compute-oracle/coordination đã có ở trên.

### Không tái sử dụng code, chỉ tái sử dụng ý tưởng thuật toán

Toàn bộ code Python của app cũ (`tests/*.py`, `utils/*.py`) không thể chuyển
thẳng sang Rust: khác ngôn ngữ, phụ thuộc `numpy`/`psutil`/`pywin32`/WMI
(Windows-only, trong khi `dit-system` đọc `/proc`/`sysfs` Linux), và bị giới
hạn bởi Python GIL nên "đa luồng" của nó không tương đương worker pinned-CPU
hiện có. Vì vậy việc "chuyển tính năng" đúng nghĩa là **viết lại kernel tương
đương bằng Rust**, giữ nguyên tắc bit-exact/zero-tolerance của `PRODUCT_SPEC.md`
— không port nguyên khối logic hay ngưỡng dung sai (`WARNING` ở ≤2 lỗi) của
app cũ.

### Ánh xạ cụ thể

| Ý tưởng từ app cũ | Kernel Rust mới nên tạo | Ghép vào đâu |
|---|---|---|
| `test_branch_predictor.py`: vòng lặp nhiều nhánh, seed cố định, so kết quả với reference tính trước | `branch_predictor_kernel` trong `cpu.rs`: hàm số nguyên nhiều nhánh (giữ nguyên công thức dạng bitwise/branch của app cũ vì đã có tính deterministic tốt), sustained loop chạy suốt round, scalar reference tính trước 1 lần rồi so lặp lại nhiều lần trong lúc CPU nóng dần — khác one-shot `integer_known_answer_test` hiện tại | Recommendation #1 (dense sustained compute-oracle); thêm như một trong các kernel con của compute-oracle, không thay thế `scalar_reduction` |
| `test_fpu_accuracy.py::test_numpy_vector_accuracy` (dot product, matmul 256×256, FFT round-trip qua BLAS/AVX) | Kernel dense FMA/AVX2 tự viết bằng `std::arch::x86_64` intrinsics (hoặc `wide` nếu đủ) cho phép nhân-cộng ma trận nhỏ lặp lại, kèm scalar reference riêng — **không kéo numpy/BLAS vào Rust** (thêm dependency lớn, không cần thiết, và bản thân BLAS tối ưu có thể tự "che" lỗi bằng cách chọn code path khác trên máy khác nhau) | Chính là kernel lõi cho Recommendation #1; đây là ứng viên tốt nhất vì matmul/FFT là loại workload có mật độ FMA cao, sát với workload thực tế (video encode, AI) hay lộ lỗi Raptor Lake |
| `test_multicore_float_consistency`: cùng phép tính trên nhiều core phải ra cùng kết quả | Mở rộng `scalar_simd_differential` hiện có: chạy cùng kernel dense-FMA trên mọi compute-oracle worker với cùng input xác định từ round seed, so kết quả giữa các worker, không chỉ so với scalar reference đơn lẻ | Bổ sung vào `ComputeOracleEvidence` một trường `cross_worker_consistency_passed` |
| Roadmap v1.5 "Power State Transition Test" (idle C6 → burst load đột ngột, lặp 1000 lần) | Pha `idle_ramp` trong lịch trình compute-oracle: worker ngủ ngắn (không phải sleep hệ điều hành sâu, mà là bỏ qua vài chu kỳ round) rồi burst full-load ngay, lặp nhiều lần trong một round | Bổ sung trực tiếp vào Recommendation #2 (đã có) của mục Recommendations phía trên — đây là bằng chứng độc lập nên ưu tiên triển khai recommendation này sớm |
| Roadmap v1.4 "Hardware crypto correctness" (AES-NI/SHA-NI vs software reference) | KAT mới `aesni_known_answer_test`/mở rộng `sha256_known_answer_test` thành sustained loop, dùng crate `aes`/`sha2` có backend hardware-accelerated (AES-NI/SHA extensions tự động qua `cpufeatures`) so với scalar reference table cố định (NIST test vector) | Thêm như kernel con thứ ba của compute-oracle, độc lập với dense-FMA kernel để tách attribution (lỗi crypto silicon riêng biệt với lỗi ALU/FPU) |
| Roadmap v1.3 "IMC dual-channel stress + latency variance" | **Chỉ nên làm evidence, không làm oracle PASS/FAIL** vì đây vốn là phép đo biến thiên (variance), thuộc dạng benchmark — `PRODUCT_SPEC.md` mục 3 cấm dùng benchmark làm oracle. Có thể ghi `memory_latency_variance_ns` vào telemetry để hỗ trợ chẩn đoán, không tham gia gate PASS | Bổ sung field thông tin vào `MemoryPlanEvidence` hoặc telemetry, không phải oracle mới |
| Ý tưởng "thermal correlation" (log error kèm nhiệt độ tại thời điểm đó) | Đã có sẵn hạ tầng gần đủ: `SystemMismatch` có thể thêm trường tham chiếu tới sample telemetry gần nhất (nhiệt độ/clock tại thời điểm phát hiện mismatch) | Bổ sung trường nhỏ vào `SystemMismatch`/`ComputeOracleEvidence`, tận dụng `TelemetryEvidence` đã có ở Bước 4, không cần module riêng |

### Rõ ràng không nên mang sang

- Ngưỡng dung sai lỗi kiểu `WARNING` khi ≤2 lỗi — vi phạm trực tiếp nguyên tắc
  zero-tolerance của `PRODUCT_SPEC.md`.
- Dùng số đo bandwidth/latency làm tiêu chí PASS/FAIL (chỉ được làm evidence).
- Bất kỳ phần nào dựa vào Python `threading`/GIL để mô phỏng "đa luồng" — phải
  dùng worker pinned-CPU thật như kiến trúc `dit-system` hiện có.
- Storage pipeline (SSD↔CPU↔RAM) và GPU↔CPU/VRAM transfer — đã bị loại rõ khỏi
  phạm vi MVP trong `PRODUCT_SPEC.md`.
- Thu thập sysinfo qua `wmic`/`powershell`/WMI — không áp dụng được cho Linux;
  `dit-system::inventory.rs` đã có đường tương đương qua `/proc`/`sysfs`.

## Consequences

Đây là tài liệu review, chưa đổi trạng thái `implemented` của Bước 3/3A trong
[STATUS.md](../STATUS.md) — các bước đó vẫn đúng với phạm vi đã công bố (multi-core/
SIMD correctness, không phải power-virus). Nếu các khuyến nghị trên được chấp
nhận, chúng nên trở thành một mục roadmap mới (ví dụ Bước 3B) với gate riêng,
không gộp lẫn vào gate hiện có của Bước 3/3A để tránh nhầm lẫn evidence.
