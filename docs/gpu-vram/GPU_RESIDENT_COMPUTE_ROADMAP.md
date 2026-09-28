# GPU-G — Resident compute, asynchronous verification và CUDA

Cập nhật: 2026-09-20. G1/G2/G3 `implemented`; CUDA sequential backend
`implemented` (G5 phần nền tảng); G4, phần overlap G5, G6–G7 còn mở.
Chi tiết và evidence: [G3/CUDA implementation](GPU_G3_CUDA_IMPLEMENTATION.md).
Campaign profiling/replay, workspace budget/backoff và fault campaign đã PASS
software/bounded GTX 960 smoke; layer/soak/image vẫn pending.
Evidence tại [implementation G1/G2](GPU_G12_IMPLEMENTATION.md).

## Mục tiêu và nguyên tắc

Kế hoạch thực thi tiếp theo được cụ thể hóa tại
[GPU-H — Async balanced pipeline](GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md).
H kế thừa G1–G3/CUDA sequential và phân rã các phần G4–G7 còn mở; không tạo
hai bộ task triển khai độc lập hoặc thay đổi trạng thái evidence của G.

Tăng lượng tính toán thực trên GPU và giảm khoảng GPU chờ CPU/PCIe. Giữ dữ liệu
trên GPU qua nhiều lượt tính; CPU chủ yếu chuẩn bị dữ liệu/đáp án theo đợt,
điều phối, truyền và xác thực kết quả. Không đánh đổi tính đúng của verdict để
tăng utilization. GPU usage cao không đồng nghĩa ALU throughput hoặc công suất cao.

Tiếp tục từ baseline GPU-A..F `implemented`, không chờ toàn bộ hardware matrix
`validated` mới phát triển. Mỗi task hoàn tất source + software gates được ghi
`implemented`; hiệu quả stress và hardware/image validation ghi riêng theo
thiết bị, driver, backend và evidence thực tế. Roadmap này không tạo hardware PASS.

Auto ưu tiên CUDA cho NVIDIA; Vulkan chủ yếu cho Intel/AMD. Vulkan trên NVIDIA
vẫn có thể chọn tường minh để chẩn đoán/đối chiếu; không fallback âm thầm.
Không thêm menu module riêng: dùng GPU + VRAM hiện có, chung identity, UI và report.
Transfer integrity, compute correctness, VRAM coverage và availability là các trục
riêng. Timeout/reset không tự chứng minh lỗi GPU, nguồn hay PCIe.

## Cơ sở lịch sử trước G1–G3

Các nhận xét dưới đây mô tả baseline lúc lập roadmap, không phải source hiện tại.
G1–G3 đã thêm profiling, persistent resources, tiled GEMM và corpus cache;
xem implementation được liên kết ở đầu tài liệu.

- `gpu-cli/src/main.rs`: retention, transfer và matrix được gọi tuần tự.
- `gpu-vulkan/src/lib.rs`: đường matrix sinh oracle CPU, tạo buffer và pipeline
  mỗi lượt, submit/chờ/readback/compare rồi hủy tài nguyên. VRAM pool transfer đã
  được giữ xuyên vòng, không đồng nghĩa matrix resources đã persistent.
- `gpu-engine/src/lib.rs`: CPU tính toàn bộ expected matrix trước dispatch.
- `matrix_u32.comp`: mỗi job là hai ma trận 8×8; `compute_rounds` tăng chuỗi đọc
  phụ thuộc trong cửa sổ 512 byte, không tăng số phép nhân ma trận. Kết quả chase
  chỉ đổi thứ tự cộng modulo 2^32 nên lỗi riêng ở chase/offset có thể không bị bắt.
- Default hiện tại 65.536 jobs: 32 MiB input và 16 MiB output mỗi lượt matrix.
  Timer matrix bắt đầu sau sinh oracle/cấp phát, chưa đủ để quy bottleneck.

Đây là nhận xét cấu trúc code, chưa phải phép đo tỷ lệ CPU/PCIe/kernel. Các phần
overlap/verifier workers từng nêu trong GPU-D được triển khai cụ thể tại G2/G4;
nhãn baseline D `implemented` không có nghĩa các tối ưu này đã hiện diện.

## Kiến trúc đích

```text
CPU:  chuẩn bị corpus/expected → nạp batch N+1 → xác thực output N-1
GPU:  input resident          → tính batch N → checkpoint output
                               events/slots điều phối quyền sở hữu
```

- Tài nguyên tái sử dụng; khởi điểm 2–3 slots, điều chỉnh theo budget thiết bị.
- Input resident được dùng cho nhiều dispatch; mỗi output có batch/seed/generation
  và trạng thái submitted/completed/readback/verified rõ ràng.
- Không ghi đè output chưa xác thực. Backlog có giới hạn; verifier chậm thì
  backpressure và ghi lý do giảm tải, không bỏ kết quả để giữ GPU usage.
- Batch có giới hạn thời gian và checkpoint; không dùng kernel vô hạn để né
  CPU overhead. Deadline ngừng submit, drain có giới hạn, giữ cancel/UI/watchdog.
- Compute-heavy, GPU-local VRAM và transfer/retention là các pha trong một
  campaign; không bắt buộc round-trip toàn VRAM trước từng compute dispatch.
- Corpus được CPU/golden xác thực độc lập trước, tái sử dụng expected và thay
  seed/corpus theo đợt. Không nhân lại toàn bộ GEMM trên CPU trước mọi dispatch.
- Full output compare là baseline exact. GPU digest/self-check chỉ bổ trợ.
  Nếu chỉ kiểm tra checkpoint cuối, report phải nói rõ intermediate chưa được
  xác thực; không cộng các lượt bị ghi đè vào verified calculations.

## Thứ tự và phụ thuộc

| Task | Kết quả chính | Phụ thuộc | Trạng thái |
|---|---|---|---|
| GPU-G1 | Contract, đo bottleneck và baseline replay | A..F implemented | implemented |
| GPU-G2 | Tái sử dụng tài nguyên Vulkan | G1 | implemented |
| GPU-G3 | Tiled integer GEMM và corpus/oracle | G1, G2 | implemented |
| GPU-G4 | Pipeline nhiều slot, verifier bất đồng bộ | G2, G3 | planned |
| GPU-G5 | CUDA backend và capability/toolchain policy | G1, G3; overlap cần G4 | sequential implemented; overlap pending |
| GPU-G6 | FP32 compute và GPU-local VRAM workloads | G3, G4, G5 cho CUDA | planned |
| GPU-G7 | UI, image, regression và performance evidence | G2..G6 | planned |

Thứ tự hiện tại G1 → G2 → G3 + CUDA sequential → G4 → phần G5 còn lại → G6 → G7. Contract/report/tests
được cập nhật cùng từng task, không chờ G7 mới tích hợp. Có thể khảo sát CUDA
trong lúc làm Vulkan; không lấy CUDA làm điều kiện để bắt đầu sửa pipeline.

## G1 — Đo và khóa contract

- Tách host wall-time: generate, oracle, allocation/pipeline, submit/wait,
  CPU compare và finalize. GPU timestamps: H2D, kernel, D2H khi hỗ trợ; thiếu
  capability ghi N/A, không dùng thời gian host làm kernel time.
- Báo work submitted/completed/verified, GPU idle gaps khi đo được, backlog,
  corpus preparation và throughput steady-state/end-to-end riêng.
- Chốt schema/version, batch identity, workload/math mode và replay descriptor.
  Bổ sung parser compatibility/fixtures; không làm hỏng report cũ.
- Lập baseline cùng seed, bytes, duration, memory budget và coverage; lưu device,
  driver, clock/power/temp và tool/build identity. Chưa có GPU thì hardware pending.

Gate: fake-clock/counter tests; không double-count thời gian overlap; fixture
phân biệt completed và verified; baseline command/evidence tái chạy được.

## G2 — Persistent Vulkan resources

- Tái sử dụng shader/pipeline/layout/descriptors và matrix device/staging buffers;
  resize chỉ ở boundary an toàn. Có cleanup cả partial initialization/error path.
- Giữ barrier, host coherence và device ownership đúng; vẫn chạy sequential để
  so sánh riêng tác động resource reuse trước khi thêm overlap.
- Tính staging/output/workspace vào budget; OOM backoff có lý do, không âm thầm
  chuyển dedicated VRAM test thành host-memory test.

Gate: allocation-failure/cleanup/reuse tests, regression golden/fault injection;
Vulkan validation-layer smoke khi có thiết bị. So A/B cùng workload, không hứa
mức tăng phần trăm trước khi đo.

## G3 — Tiled integer GEMM và expected corpus

- Thêm GEMM u32 modulo 2^32 có tiling/shared-memory/register reuse; chọn tile,
  kích thước ma trận và dispatch theo capability/budget, có tail/boundary cases.
- Tăng useful multiply-add thay vì tăng pointer chase. Phần tính toán nặng phải
  ảnh hưởng output được oracle kiểm tra; pointer chase giữ làm latency diagnostic
  tùy chọn, không dùng làm thước đo compute throughput chính.
- CPU reference độc lập, corpus có seed/layout/generation/hash và expected đã
  kiểm chứng. Thay corpus định kỳ, kiểm input/guard/stale-output, không chỉ lặp
  một fixture hoặc chỉ tin metadata generation.
- Chốt ops count theo phép toán thực; không gọi integer ops là FLOPS. Nếu nhiều
  phép tính trung gian chỉ góp vào một output, báo đúng phạm vi xác thực cuối.

Gate: nhiều size/seed, modulo overflow, tail, wrong index/missing write/stale
batch và intermediate corruption có đường truyền tới output; CPU oracle golden
không phụ thuộc implementation GPU. Hardware comparison pending khi thiếu GPU.

## G4 — Overlap và bounded CPU verifier

- Slot state machine + workers xác thực có giới hạn; chuẩn bị N+1, compute N,
  readback/verify N-1 có thể chồng lấp nếu capability cho phép.
- Không mặc định nhiều queue sẽ tăng throughput; có sequential fallback và
  explicit dependencies, ownership, non-coherent flush/invalidate cho Vulkan.
- Budget RAM/VRAM/verifier dùng chung khi nhiều GPU; chống starvation giữa thiết
  bị, first-failure bounded evidence và sticky failure.
- Deadline/cancel/device loss: stop submit, bounded drain, liệt kê unverified
  batches; không PASS khi batch bắt buộc còn thiếu xác thực.

Gate: delayed/out-of-order completion, verifier lag, queue đầy, stale/reused slot,
cancel từng state, timeout/device loss và drain; sequential/overlap cùng corpus
cho cùng kết quả và coverage. Không bỏ output khi nghẽn.

## G5 — CUDA backend tùy chọn

- Giữ Rust orchestration; cô lập CUDA/FFI và ownership trong backend riêng.
  Tên crate/API cụ thể chốt lúc triển khai, không thêm dependency CUDA bắt buộc
  vào generic Vulkan build hoặc CPU/NVMe modules.
- Probe runtime/driver/device capability; map identity UUID/BDF, không map bằng
  thứ tự enumeration. Explicit CUDA request không được âm thầm chạy Vulkan/mock.
- Device allocations và pinned host buffers tái sử dụng; streams/events cho
  copy/compute/verification. Khả năng overlap phải probe/đo, không suy từ API.
- Port exact GEMM G3 trước; cùng corpus/oracle/report với Vulkan. CUDA Graphs
  chỉ tối ưu thêm sau khi đo launch overhead, không thay thế kernel đủ tải.
- Lập toolchain/driver/architecture matrix trước khi khóa dependency. Nếu cần
  GTX 960/Maxwell: chọn nhánh toolkit còn hỗ trợ, ví dụ CUDA 12.9; không dùng
  CUDA 13 để build đường Maxwell. Tensor path chỉ bật khi capability phù hợp.

Gate: generic build không có CUDA vẫn PASS; unsupported/missing runtime xử lý
đúng; FFI lifecycle/error mapping tests; cross-backend exact fixtures và CUDA
smoke/injection trên GPU phù hợp khi có. Mock không tạo CUDA hardware PASS.

## G6 — Workload compute và VRAM bổ sung

- FP32 GEMM/SGEMM: ưu tiên arithmetic intensity và nhiều accumulator độc lập;
  ghi precision, math mode, FMA/TF32/tensor policy và library/compiler identity.
  Expected độc lập, tolerance có cơ sở theo workload; bắt NaN/Inf bất thường.
  Không dùng bit-exact integer rule cho mọi floating-point path, không nới
  tolerance tùy tiện để che mismatch. FP64/tensor là optional theo capability.
- GPU-local VRAM transform: coalesced read/write qua ping-pong buffers, dữ liệu
  phụ thuộc seed/address/generation; working set xét cache và budget thực tế.
  Có checkpoint CPU full compare, xoay vùng; không chỉ kiểm hash GPU hay cặp
  biến đổi thuận/nghịch có thể che lỗi. Báo logical coverage, không cam kết cell.
- Lịch pha giữ transfer/retention riêng, có minimum work/coverage cho mỗi pha;
  compute-heavy không được làm retention hết hạn hoặc pha khác bị bỏ đói.

Gate: numerical fixtures/known-invalid cases, output/intermediate/address/stale
faults, checkpoint-overwrite guard và duration/phase coverage bằng fake clock.
Hiệu quả công suất/ALU/VRAM được đo riêng, không suy từ tỷ lệ GPU usage.

## G7 — UI, đóng gói và evidence

- Dashboard chung hiện backend/kernel/math mode, GPU/RAM/VRAM usage, progress,
  compute active time, H2D/D2H, verified results/s, backlog và unverified work.
  Telemetry không hỗ trợ hiện N/A; không trộn allocated với verified bytes.
- Image generic giữ Vulkan; NVIDIA flavor đóng đúng runtime/kernel artifacts,
  manifest/SBOM/license/provenance. Build image không đồng nghĩa boot validated.
- Regression menu/multi-GPU/watchdog/result persistence, bounded logs và parser
  report cũ; negative campaign xuyên tất cả backend/phase.
- A/B cùng điều kiện: baseline → resource reuse → new GEMM → overlap → CUDA.
  Báo preparation và steady-state riêng, throughput đã verify, kernel/transfer
  timing, backlog, memory budget, power/clock/temp nếu có. Không cố định mục tiêu
  100% usage hoặc TDP như điều kiện PASS.
- Hardware evidence: bounded smoke trước, 30/60 phút và display/headless,
  cancel/retention, nhiều GPU theo thiết bị sẵn có; unsupported/pending ghi rõ.

Gate implemented: source/software/injection/schema/UI/build gates phù hợp PASS,
cập nhật STATUS với lệnh và evidence thật. Hardware effectiveness/release matrix
vẫn được phát triển tiếp nếu pending; chỉ công bố validated đúng cấu hình đã chạy.

## Tham chiếu thiết kế

- [CUDA Best Practices](https://docs.nvidia.com/cuda/cuda-c-best-practices-guide/):
  giảm host-device transfers, resident data, pinned memory và overlap.
- [CUTLASS Efficient GEMM](https://docs.nvidia.com/cutlass/latest/media/docs/cpp/efficient_gemm.html):
  hierarchical tiling và data reuse; tham khảo thuật toán, không mặc định thư viện
  mới hỗ trợ mọi GPU cũ.
- [CUDA 13 release notes](https://docs.nvidia.com/cuda/archive/13.0.2/cuda-toolkit-release-notes/index.html):
  giới hạn offline compilation/library support cho Maxwell/Pascal/Volta.

Sau mỗi task cập nhật [STATUS.md](STATUS.md). Roadmap cha:
[GPU–VRAM](ROADMAP.md), [DIT-01](../ROADMAP.md).
