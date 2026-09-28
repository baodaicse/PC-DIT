# CPU-DIT-01 Implementation Plan

## Công cụ triển khai ban đầu

- Rust stable cho reference core, CLI và JSON report.
- Cargo test, rustfmt và Clippy làm quality gate.
- Python standard library cho independent golden vectors/test tooling.
- Reference path không dùng SIMD/assembly; system path dùng safe SIMD qua `wide`.
- Toolchain/dependency được khóa bằng `rust-toolchain.toml` và `Cargo.lock`.

Quyết định được ghi tại
[ADR-0001](decisions/0001-rust-reference-core.md). Trạng thái và bằng chứng chạy
thực tế nằm tại [STATUS.md](STATUS.md).

## Bước 1 — Reference pipeline

- [x] Khởi tạo Git repository và Cargo workspace.
- [x] Khóa Rust toolchain/dependency.
- [x] Source corpus, manifest và SHA-256 verification.
- [x] Counter-based source expansion theo seed/global offset.
- [x] Variable block recipe và tail không bỏ byte.
- [x] Scalar GF(256) matrix encode/decode.
- [x] Deterministic logical scatter.
- [x] Exact readback, recovered/join compare và final digest.
- [x] Independent Python golden generator.
- [x] Versioned JSON report và mismatch evidence.
- [x] Fault injection tại CPU encode, RAM readback và recovered/join.
- [x] Corrupted-source test.
- [x] rustfmt, Clippy `-D warnings`, unit/integration test và release CLI.

Giới hạn của Bước 1 được ghi tại
[REFERENCE_PIPELINE_V1.md](REFERENCE_PIPELINE_V1.md). Bước 2 và 3 được triển khai
trong `dit-system`; xem [SYSTEM_PIPELINE_V1.md](SYSTEM_PIPELINE_V1.md).

## Bước 2 — Adaptive RAM engine

- [x] Inventory RAM, CPU, cpuset và NUMA từ Linux.
- [x] Target theo byte hoặc phần trăm RAM khả dụng với safety reserve.
- [x] Progressive anonymous multi-arena `mmap` và allocation backoff.
- [x] First-touch/prefault, optional `mlock` và giữ high-water arena set.
- [x] Pattern xác định từ source/seed/global offset, kể cả tail lẻ.
- [x] Exact byte readback và first-mismatch evidence.
- [x] Tách unique coverage khỏi tổng lưu lượng nhiều vòng.
- [x] RAM fault injection sau write không thể PASS.
- [x] CLI/report và regression test cho target phần trăm, multi-arena, tail.

## Bước 3 — Multi-core, SIMD và CPU coverage

- [x] CPU topology/cpuset inventory và worker affinity.
- [x] Writer/verifier role rotation qua logical CPU khác nhau.
- [x] Safe SIMD reduction được scalar implementation kiểm độc lập.
- [x] Integer và SHA-256 known-answer tests.
- [x] FP/FMA known-answer route tách khỏi bit-exact RAM integrity.
- [x] Atomic `SeqCst` coherency workload trên mọi worker.
- [x] SHA-256 digest theo arena/worker/round.
- [x] SIMD-worker fault injection bị scalar oracle phát hiện.
- [x] Device run 16 logical CPU, 512 MiB, 4 vòng PASS.

Chưa đánh dấu `validated`: mới chạy một CPU, một dung lượng RAM và một NUMA node.

## Bước 3A — Pattern/window coverage mở rộng (`implemented`, matrix chưa validated)

- [x] Khóa `CPU-DIT-WINDOW-v1`, eligibility và size list 4,093 B..32 MiB.
- [x] Deterministic planner theo round seed, không gap/overlap/overflow.
- [x] Bảo đảm mỗi Safe/Full complete round đủ mandatory size classes.
- [x] Pattern families: fixed, checkerboard, walking-bit, address và source-expanded.
- [x] Scalar expected generator xác định cho từng family.
- [x] Per-size/per-pattern exact coverage, plan digest và report schema v7.
- [x] Progress/UI window, pattern và recipe coverage (`MIXED_PARALLEL` khi nhiều worker).
- [ ] Fault injection đầy đủ cho order/metadata/cross-boundary của từng class.
- [x] Planner/replay tests cho target và arena size khác nhau; gap/overlap bị phát hiện.
- [ ] Device Safe, Full 30/60 phút; image BIOS/UEFI regression.

Không đặt `30 phút = 10 seed`. Một seed chỉ được commit sau complete full-RAM
round; duration dài hơn tiếp tục thêm round tùy throughput thực tế. Chi tiết tại
[PATTERN_WINDOW_COVERAGE_PLAN.md](PATTERN_WINDOW_COVERAGE_PLAN.md).

## Bước 3B — Compute-oracle sustained (`implemented`, matrix chưa validated)

- [x] Kernel dense FMA/AVX sustained (`wide::f64x4::mul_add`) với oracle
  cross-worker bit-exact và scalar-reference tolerance.
- [x] Kernel branch-predictor nguyên số, chuyển thể từ app Python cũ, oracle
  bit-exact với reference tính trước.
- [x] AES-256 known-answer test (FIPS-197) qua crate `aes` (AES-NI khi có).
- [x] Pha `idle_ramp` tuỳ chọn để tăng số lần chuyển P-state/voltage.
- [x] Lệnh độc lập `dit-cli compute-oracle-run` (Phase 1) không đụng schema
  v7/v8 của `system-run`.
- [x] Chạy đồng thời thật với route RAM trong `system-run` (Phase 2): CPU
  không trùng RAM worker, spawn trước/join sau round loop, report schema v9
  với `SystemComputeOracleEvidence` tách biệt.
- [x] Fault injection cross-worker không thể PASS, cả ở route độc lập lẫn
  route tích hợp.
- [ ] Device evidence trên chính CPU Intel Raptor Lake (13th/14th Gen) thật.
- [ ] Route AVX-512 differential riêng và IMC dual-channel latency-variance
  evidence (mới là đề xuất ở ADR-0004, chưa triển khai).

Chi tiết tại
[ADR-0004](decisions/0004-cpu-power-virus-coverage-review.md).

## Bước 4 — Telemetry, safety và diagnostic rerun

- [x] Thermal/hwmon sampling, peak và threshold stop.
- [x] EDAC CE/UE, CPU throttle và cgroup OOM delta khi kernel cung cấp.
- [x] Machine-check inventory và kernel-journal event summary.
- [x] Versioned safety policy, deadline và required-sensor gates.
- [x] Cooperative cancellation trong RAM write/read loop.
- [x] Worker panic/crash và execution abort evidence.
- [x] JSON report schema v7 có source/config, duration, CPU power, seed, window và temperature-limit state.
- [x] Linux-random session seed, SHA-256 round derivation, commitment và replay CLI.
- [x] Diagnostic rerun giữ recipe và phân loại không quy component thiếu evidence.
- [x] Thermal, EDAC UE, deadline và crash fault injection.
- [x] Temperature limit operator toggle mặc định OFF; trusted CPU sensor filter;
  thermal abort không diagnostic rerun.
- [x] Device telemetry run và negative campaign.

Deadline engine vẫn là cooperative in-process. Bước 5 đã thêm `timeout` bên ngoài
để TERM/KILL process bị treo; kernel/firmware hard hang cần watchdog hoặc power
control ngoài ở Bước 6.

## Giai đoạn A — Khóa đặc tả và test vector

- [ ] Chốt source corpus, manifest format và quy tắc source failure.
- [x] Chốt session/round seed recipe v1 và replay semantics.
- [ ] Chốt descriptor và block-size recipes version 1.
- [ ] Chọn GF/matrix scheme, group size và chính sách parity.
- [ ] Viết scalar reference độc lập và tạo golden intermediate/final vectors.
- [ ] Chốt status model, ECC policy, timeout và thermal safety policy.
- [ ] Chốt JSON schema cho run, block evidence và summary.

Deliverable: descriptor v1, manifest v1, golden vector pack và report schema v1.

## Giai đoạn B — RAM pipeline MVP

- [x] Inventory installed/Linux-usable/available RAM và NUMA topology.
- [x] Adaptive multi-arena allocation, safety reserve và failure backoff.
- [x] Prefault, touch, optional lock và high-water allocation accounting.
- [x] Giữ toàn bộ arena trong round để ngăn physical-page reuse tạo coverage giả.
- [ ] Per-node allocation/coverage và local/cross-NUMA routes.
- [ ] Split logic, protected metadata và deterministic scatter/permutation.
- [ ] Matrix encode → RAM write → readback → exact verify → decode → join.
- [x] Source expansion bằng seed/global offset, không cần source lớn bằng RAM.
- [x] Reference matrix và system arena tail không bỏ byte phần dư.
- [ ] Pattern/window size, alignment và stride recipes theo Bước 3A.
- [x] Checkpoint evidence và first-mismatch capture.
- [ ] Fault injection cho payload, metadata, order và join.

Deliverable: deterministic single-core CPU–RAM pipeline tự thích nghi dung lượng,
với exact verification và coverage evidence không phụ thuộc cấu hình 32/64 GB.

## Giai đoạn C — CPU và multi-core

- [x] Worker affinity, topology-aware scheduling và cross-core role rotation.
- [x] Optimized SIMD path được scalar reference kiểm độc lập.
- [x] SHA-256/integer reduction, known-answer và round digest.
- [x] Numerical FP/FMA exact known-answer test riêng.
- [x] Combined CPU/cache/coherency/IMC/RAM rounds.
- [x] Fault injection vào SIMD optimized result.

Deliverable: multi-core combined DIT với golden end-to-end oracle.

## Giai đoạn D — Telemetry, safety và chẩn đoán

- [x] Thu thập EDAC/ECC/MCE, nhiệt độ, throttle, crash và timeout khi nguồn Linux
  tương ứng khả dụng; ghi `unavailable` nếu không có.
- [x] Diagnostic rerun với seed/offset/allocation được kiểm soát.
- [x] JSON report versioned và summary dễ đọc cho người vận hành.
- [x] Artifact/log writer chỉ ghi vào đích được phép trên USB.
- [x] Phân biệt component evidence với system/route failure.

Deliverable: phiên test an toàn, có thể tái hiện và điều tra tại hiện trường.

## Giai đoạn E — Linux boot và validation sản phẩm

- [x] Tạo USB boot Linux minimal, root read-only, swap/service thừa bị tắt.
- [x] Preflight inventory CPU/RAM/NUMA/ECC/sensor.
- [x] Boot-smoke legacy BIOS và x86_64 UEFI trên QEMU.
- [x] Hard process watchdog và USB-only result writer.
- [x] UI text Safe/Full/Sensor Info/Diagnostic với auto-Safe 10 giây.
- [x] Live test UI có progress, phase, round, elapsed, RAM target và sensor.
- [x] Dashboard test ba khung responsive, live CPU/RAM utilization và compact fallback.
- [x] Minimum-duration engine: Safe 2 phút, Full 30 phút, hoàn tất round trước stop.
- [x] Full target bằng 95% safe allocatable limit sau reserve 512 MiB.
- [x] CPU power hwmon + powercap fallback và semantics `N/A` rõ ràng.
- [x] Ubuntu HWE module/firmware cùng bộ công cụ sensor/RAS/ổ đĩa/network cơ bản.
- [x] Driver probe an toàn cho sensor/power/EDAC/IPMI và lưu evidence theo run.
- [x] Makefile một lệnh tạo/kiểm raw image dùng cho Rufus/DD.
- [ ] Differential test scalar/SIMD và single-core/multi-core.
- [x] Protocol P0–P6, hardware matrix, record template và session validator.
- [x] Automated negative/fault campaign runner cho release evidence.
- [ ] Soak/repeatability trên nhiều CPU, RAM size, ECC và NUMA topology.
- [ ] Validation tối thiểu trên các lớp 16/32/64/128 GB khi có phần cứng.
- [ ] Đo boot footprint, allocatable coverage, throughput và thời gian test.
- [ ] Xác minh không có false PASS khi parity phát hiện hoặc sửa được lỗi.
- [ ] Đánh giá tầng pre-OS nếu cần physical RAM coverage cao hơn.

Deliverable: USB image và validation report cho hardware matrix đã công bố.

## Thứ tự ưu tiên MVP

1. Reference/golden vectors.
2. Single-core exact RAM pipeline.
3. Fault injection và evidence.
4. Multi-core role rotation.
5. Telemetry/safety.
6. Linux minimal boot image.
7. Tối ưu SIMD và performance.

Không tối ưu trước khi scalar reference, golden vector và fault-injection gate đã
hoạt động; tốc độ cao không có giá trị nếu correctness path có thể false PASS.
