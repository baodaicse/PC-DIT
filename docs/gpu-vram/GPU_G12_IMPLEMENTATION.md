# GPU-G1/G2 — Implementation và evidence

Trạng thái 2026-09-20: G1/G2 `implemented` ở mức source/software và bounded
GTX 960 smoke. G3–G7 chưa triển khai. Validation layer, soak và image còn pending;
không gọi đây là hoàn tất CUDA hoặc đã cải thiện stress trên mọi GPU.

## Hoàn tất G1/G2 — 2026-09-20

- `campaign_profile` version 1 cộng dồn transfer/retention/matrix, cả lượt lỗi:
  attempts/submitted/completed/verified/failed, host total, CPU prepare/compare,
  submit/wait và GPU H2D/kernel/D2H. `gpu_samples` là số lượt đo được; khi bằng 0,
  không diễn giải tổng GPU bằng 0 là GPU chạy tức thì. GPU intervals nằm trong
  host wait, không cộng hai hệ thời gian với nhau. Phần host còn lại gồm staging,
  allocation, command record, hashing và cleanup; matrix vẫn có breakdown riêng.
- Đồng hồ quan sát bắt đầu sau backend open, gồm setup/test/drain. Queue-empty
  lower bound đo từ host-confirmed completion tới submit tiếp theo và phần đuôi;
  sau timeout chưa xác nhận completion không cộng idle giả. Đây chỉ là queue của
  DIT, không phải physical GPU idle. Snapshot không mutate/double-count phần đuôi.
- Batch v1 khóa phase/seed/generation/allocation/range/jobs/rounds, canonical
  little-endian SHA-256 có golden độc lập và validation version/range. Identity
  là khóa nội dung replay, `last_attempt_sequence` phân biệt lượt thử lặp.
  Replay phải dùng kèm GPU/backend/shader identity trong report cha; retention
  descriptor chỉ định dữ liệu của write gốc, không yêu cầu retention trên VRAM
  mới chưa được ghi. Giữ first failed batch qua các lượt drain sau đó.
- Fake-clock tests bao phủ success, mismatch, timeout, cancel trước submit và
  snapshot tail; không cộng GPU-completed mismatch thành verified.
- `workspace_budget` version 1 dự trù device reserve tối thiểu 32 MiB/5%, host
  reserve tối thiểu 128 MiB/10%, matrix device/staging/oracle/temporaries và tối đa
  bốn bản chunk host. Transfer chunk bị giới hạn theo host headroom, tối đa 64 MiB.
  iGPU tính pool/device workspace vào cùng host budget, không đếm RAM dùng được
  hai lần. Multi-GPU chia MemAvailable cho workers qua `DIT_GPU_HOST_BUDGET_BYTES`.
- Cấp matrix slot trước test pool, giới hạn jobs theo dispatch/storage limits.
  Thiếu budget hoặc allocation OOM thì halve jobs hữu hạn tới 1; API/capability
  errors khác không bị che thành backoff. Report ghi requested/effective jobs,
  requested/effective pool và backoff steps. Không còn đủ reserve trả INVALID
  có report, không fallback sang host-memory test. Budget là estimate tại setup,
  không bảo đảm hệ thống không có áp lực bộ nhớ từ process khác trong lúc chạy.
- Backoff matrix diễn ra tại setup; lỗi allocation mới giữa run vẫn là lỗi rõ
  ràng, không âm thầm đổi batch. Resource recreation là diagnostic A/B, không phải
  chính sách mặc định. Host reserve bao gồm headroom cho alignment/driver; các số
  buffer byte trong report không phải đo physical residency.
- Hardware fault campaign đi qua 18 checkpoint: buffer handle/bound allocation/
  owner insertion ×4 và 6 pipeline objects; kiểm cache không còn partial object,
  reinitialize và verify lại. OOM injection ở buffer/pipeline kiểm halve 8→4 jobs.
- Script có validation-layer gate: nếu Khronos layer được cài ở system manifest
  path, chạy hardware tests với layer và fail khi có Validation Error/VUID. Host
  này chưa có layer nên ghi PENDING; không cài dependency hệ thống trong task.

Evidence: `build/gpu-g12.kmHVGj/{recreate,reuse,negative,budget-invalid,budget-backoff}/report.json`.
`DIT_GPU_HARDWARE=1 make gpu-g12-check` PASS: Clippy, 29 software tests,
2 hardware tests (mặc định ignored), A/B hash/coverage, negative mismatch,
budget INVALID và budget backoff PASS. Đây là bounded debug smoke, không phải
benchmark hoặc soak validation. Image chưa rebuild.

Gate bổ sung cuối lượt: `make gpu-check` PASS; Clippy và bộ `hardware_smoke`
PASS cả 3 tests sau khi thêm transfer/retention mismatch accounting và pool
alignment/backoff 4 KiB. Release binary build PASS; bounded release report:
`build/gpu-g12-complete-release-20260920/report.json`. Validation-layer chưa chạy
vì thiếu layer, không nâng thành validated từ các lượt smoke này.

## Lịch sử đợt đầu 2026-09-19

Các mô tả và evidence dưới đây giữ lại phạm vi đợt đầu; phần còn thiếu G1/G2
ở thời điểm đó được hoàn tất bằng thay đổi 2026-09-20 bên trên.

## Thay đổi runtime

- `gpu-engine`: tách đo sinh input và CPU oracle; giữ nguyên bytes/expected và
  golden của matrix 8×8. CPU vẫn tính lại oracle mỗi lượt, chưa có corpus G3.
- `gpu-vulkan`: thêm `ComputeEvidence.profile` version 1 với seed/generation,
  workload identity, các khoảng host generation/oracle/setup/upload/record/wait/
  readback/compare/cleanup/hash và end-to-end. `elapsed_seconds` cũ giữ phạm vi
  đo cũ để không âm thầm thay nghĩa field.
- GPU timestamps tách H2D/kernel/D2H khi queue hỗ trợ; mask valid bits, xử lý
  wrap và từ chối khoảng đo có thể đi qua trọn chu kỳ wrap. Không hỗ trợ/lỗi
  query ghi `gpu=null` với lý do, không thay bằng thời gian host hay số 0.
- GPU intervals gồm overhead boundary/barrier, không phải PCIe wire time thuần;
  nằm trong host submit/wait nên không cộng vào tổng host time. Tham chiếu:
  [Vulkan timestamp queries](https://docs.vulkan.org/spec/latest/chapters/queries.html#queries-timestamps).
- Tái sử dụng matrix upload/download/device buffers khi kích thước không đổi;
  resize tại boundary sau completion. Giữ shader/pipeline/layout/pool/set qua
  nhiều lượt. Owner cleanup tài nguyên được tạo trước lỗi partial construction.
- Report ghi `buffers_reused`, `pipeline_reused` và requested buffer bytes cho
  device/host; đây không phải physical VRAM residency hoặc allocation alignment
  overhead. Chưa thay budget planner để dự trù đầy đủ mọi workspace.
- CLI thêm `--recreate-compute-resources` cho baseline A/B tuần tự, truyền tiếp
  xuống multi-GPU workers. Mặc định bật reuse, không đổi shader/rounds/coverage.
- Khi queue submit/wait lỗi, backend không nhận thêm việc; fence/command và
  buffers có thể còn pending được giữ đến teardown, không free ngay khi timeout.
  Teardown vẫn dùng `device_wait_idle`; bounded recovery khi driver hard hang
  vẫn là việc còn mở, không quảng cáo timeout đồng nghĩa teardown có deadline.
- Host map toàn allocation khi dùng non-coherent flush/invalidate WHOLE_SIZE;
  kiểm kích thước host read/write trước copy.

## Report compatibility và lỗi

Report ABI chung không đổi: các field là additive. `ComputeEvidence` cũ thiếu
`profile` vẫn deserialize được. Profile có version riêng; parser cũ phải tiếp tục
bỏ qua field không biết theo contract hiện hành.

- `matrix_dispatches_completed` giữ nghĩa baseline: matrix đã so khớp thành công.
- `matrix_dispatches_submitted`: queue đã nhận matrix dispatch.
- `matrix_dispatches_gpu_completed`: fence xác nhận hoàn thành, kể cả mismatch.
- `last_compute`: lượt xác thực thành công gần nhất, có thể không phải lượt cuối.
- `last_compute_attempt`: profile lượt thử mới nhất, kể cả compare/submit failure;
  lỗi trước khi thiết lập/dispatch có thể không có profile, không dùng lại profile
  cũ. Không coi các bộ đếm này là backlog của pipeline bất đồng bộ G4.

Negative smoke xác nhận submitted=1, GPU completed=1, verified/completed baseline=0,
verdict FAIL. Không có thay đổi sticky-failure policy để tăng số PASS.

## Kiểm thử và evidence

Đã chạy PASS:

- `cargo clippy -p gpu-engine -p gpu-vulkan -p gpu-cli --all-targets -- -D warnings`.
- `cargo test -p gpu-engine -p gpu-vulkan -p gpu-cli -- --test-threads=1`:
  23 tests PASS, 1 hardware test mặc định ignored.
- `cargo test -p gpu-vulkan persistent_matrix_hardware_smoke -- --ignored --test-threads=1`:
  hardware test PASS; cold/warm reuse, resize 32→17 jobs, replay hash, mismatch,
  tiếp tục sau mismatch và từ chối work sau terminal submission state mô phỏng.
- `make gpu-check`: GPU-A..D regression, mock/transfer/matrix injection, retention,
  hardware smoke và menu PASS.
- `DIT_GPU_HARDWARE=1 make gpu-g12-check`: software + bounded A/B + negative PASS.
  Evidence giữ tại `build/gpu-g12.OZd5Sa/{recreate,reuse,negative}/report.json`.
  A/B cùng 3 generations, 8 MiB pool, 256 jobs, 64 chase rounds; result hash,
  matrix verified count và byte coverage giống nhau.
- `cargo build --release -p gpu-cli`: PASS; binary host
  `target/release/gpu-vram-dit` đã cập nhật. Release smoke 2 generations,
  8 MiB pool, 256 jobs, 64 rounds PASS; evidence
  `build/gpu-g12-release-20260919/report.json`. Đây chưa phải release A/B/soak.

Máy thử: NVIDIA GTX 960, UUID `6c57e2fb7786dd6d7ce34a27131ac119`, BDF
`0000:08:00.0`; driver/provenance và telemetry trong report. Các lượt A/B trên
được build debug, chỉ chứng minh plumbing/correctness/reuse, không dùng để công
bố phần trăm tăng performance của bản release hoặc hiệu quả power stress.

Baseline trước refactor giữ ở `build/gpu-g1-baseline-20260919/report.json`;
smoke reuse ban đầu ở `build/gpu-g2-reuse-20260919/report.json`.
Không rebuild image/ISO trong đợt này. CUDA toolkit chưa có trên host, chưa cài
dependency mới. Validation layer Khronos không có trên host nên layer gate pending.

Chạy lại software không dùng GPU: `make gpu-g12-check`.
Opt-in hardware: `DIT_GPU_HARDWARE=1 make gpu-g12-check`; script giữ evidence tại
thư mục build mới, không ghi đè lượt cũ. Không PASS hardware bằng mock.

## Công việc tiếp theo sau G1/G2

1. Validation-layer/hardware matrix và soak khi có điều kiện; source không bị chặn.
2. Matrix cache vẫn một slot tuần tự; resident corpus thuộc G3, overlap thuộc G4.
3. G3: tiled integer GEMM có phần tính toán nặng ảnh hưởng output, corpus độc lập;
   rà dispatch limits thay vì chỉ tăng số jobs/rounds.
4. G4–G7: overlap/verifier pool, CUDA, FP32/GPU-local VRAM, UI và image theo
   [roadmap](GPU_RESIDENT_COMPUTE_ROADMAP.md). Hardware pending không chặn source.
