# Duration-Complexity Ramp — thiết kế chung cho DIT-01

Recipe ID: `DIT-DURATION-COMPLEXITY-RAMP-v1`
Trạng thái: GPU-VRAM `implemented`; CPU-RAM `implemented`; CPU-NVMe `researched, not yet coded` (xem mục roadmap ở cuối file).
Tài liệu này mô tả một pattern chung để bất kỳ AI coding tool hoặc lập trình viên nào cũng triển khai nhất quán, không cần đọc lại toàn bộ lịch sử hội thoại.

## Từ khóa tra cứu

`duration-complexity-ramp` `tiered-gate` `capped-linear-ramp` `compute-rounds` `algorithm-depth-scaling`
`DIT-DURATION-COMPLEXITY-RAMP-v1` `resolved_compute_rounds` `suggested_compute_rounds`
`dense_fma_iterations_per_burst` `matrix_rounds` `two-axis-scaling` `fence-timeout-safe-cap`

Khi cần triển khai tính năng "độ phức tạp thuật toán tăng theo duration" ở bất kỳ module DIT nào, tìm đúng file này trước bằng một trong các từ khóa trên.

## Nguyên tắc cốt lõi: hai trục độc lập

DIT-01 luôn tách biệt hai trục cấu hình, không được trộn lẫn:

1. **Trục lặp lại (repeat axis)** — `duration`/`generations` quyết định **số lần** một đơn vị công việc (round/generation/operation) được lặp lại. Trục này đã tồn tại ở cả ba module từ trước.
2. **Trục độ sâu (depth axis)** — một tham số riêng (`compute-rounds`, `dense_fma_iterations_per_burst`, `matrix_rounds`, ...) quyết định **mỗi đơn vị công việc nặng tới đâu**. Trục này độc lập với trục 1.

`DIT-DURATION-COMPLEXITY-RAMP-v1` là cách **gợi ý giá trị mặc định** cho trục 2 dựa trên trục 1, mà không phá vỡ tính độc lập giữa hai trục.

## Vì sao không dùng công thức tuyến tính đơn thuần

Chi phí mỗi round/dispatch bị giới hạn bởi một ngưỡng cứng độc lập với duration (ví dụ `FENCE_TIMEOUT_NS = 30s` ở GPU). Nếu độ sâu tăng tuyến tính không giới hạn theo `duration`, một `duration` rất lớn sẽ tạo ra một round đơn lẻ đủ nặng để tự nó vượt ngưỡng — ngược với ý định "chạy lâu hơn thì an toàn hơn". Vì vậy công thức bắt buộc phải có **trần**.

## Công thức chuẩn

```text
nếu duration_seconds < NGƯỠNG:
    trả về BASELINE                       // bậc thang / tiered gate
ngược lại:
    extra = duration_seconds - NGƯỠNG
    giá_trị = BASELINE + extra * HỆ_SỐ    // tuyến tính có trần
    trả về min(giá_trị, TRẦN)             // capped-linear
```

Ba hằng số bắt buộc phải đo thực nghiệm trước khi chọn, không đoán:

- `NGƯỠNG` — thời lượng tối thiểu để ramp bắt đầu có ý nghĩa (dưới ngưỡng, giữ baseline để smoke test ngắn vẫn nhanh/so sánh được).
- `HỆ_SỐ` — tốc độ tăng mỗi giây.
- `TRẦN` — giá trị tối đa tuyệt đối, phải được đo để đảm bảo chi phí một round ở mức `TRẦN` vẫn có biên độ an toàn rộng (khuyến nghị ≥ 3–4 lần) dưới ngưỡng timeout cứng của module đó.

## Tham chiếu triển khai đã có: GPU-VRAM (`compute-rounds`)

```rust
const DEFAULT_COMPUTE_ROUNDS: u32 = 4_096;                    // BASELINE
const COMPUTE_ROUNDS_RAMP_THRESHOLD_SECONDS: u64 = 60;         // NGƯỠNG
const COMPUTE_ROUNDS_PER_SECOND: u32 = 36;                     // HỆ_SỐ
const MAX_AUTO_COMPUTE_ROUNDS: u32 = 65_536;                   // TRẦN

fn suggested_compute_rounds(duration_seconds: u64) -> u32 { /* công thức chuẩn ở trên */ }

fn resolved_compute_rounds(args: &RunArgs) -> u32 {
    args.compute_rounds.unwrap_or_else(|| suggested_compute_rounds(args.duration_seconds))
}
```

Số đo thực nghiệm làm căn cứ chọn `TRẦN`: `rounds=4096` ở `matrix-jobs=65536` mất ~0.24–0.48s; `rounds=65536` (trần) ước tính ~7.7s — dưới `FENCE_TIMEOUT_NS=30s` với biên độ ~4 lần.

## Bốn yêu cầu bắt buộc khi triển khai ở module bất kỳ

1. **Tham số phải là `Option<T>`** trên CLI (`compute_rounds: Option<u32>`), không đặt `default_value_t` cố định — nếu người dùng không truyền, mới áp dụng công thức; nếu có truyền, giá trị đó luôn thắng tuyệt đối.
2. **Giá trị đã resolve phải được ghi vào report/progress**, không được để việc replay phải suy ngược lại từ `duration`. (Xem `ComputeEvidence.rounds` ở [gpu-vulkan/src/lib.rs](../crates/gpu-vulkan/src/lib.rs) làm mẫu.)
3. **`TRẦN` phải là hằng số cố định**, không phụ thuộc input người dùng — dù `duration` lớn tới đâu, giá trị tự động không bao giờ vượt `TRẦN`.
4. **Validate riêng cho override tường minh**, không áp lên giá trị tự động (giá trị tự động đã an toàn theo thiết kế công thức).

## Đã triển khai: CPU-RAM (`dense_fma_iterations_per_burst`)

Cập nhật 3C (2026-09-18): `system-run` cũng resolve FMA depth theo duration khi
không truyền `--compute-oracle-dense-fma-iterations`, ghi vào system schema v10.
Standalone hiện dùng compute schema v4 với minimum-work/availability evidence;
schema v3 bên dưới là mốc lịch sử đưa resolved depth vào report. Power-transition
profile riêng dùng burst duration/transition count, không tự áp ramp sustained.

File: [crates/dit-cli/src/main.rs](../crates/dit-cli/src/main.rs), [crates/dit-system/src/compute_oracle.rs](../crates/dit-system/src/compute_oracle.rs)

```rust
const DEFAULT_DENSE_FMA_ITERATIONS: u32 = 200_000;      // BASELINE (giữ nguyên default CLI cũ)
const DENSE_FMA_RAMP_THRESHOLD_SECONDS: u64 = 60;       // NGƯỠNG
const DENSE_FMA_ITERATIONS_PER_SECOND: u32 = 300_000;   // HỆ_SỐ
const MAX_AUTO_DENSE_FMA_ITERATIONS: u32 = 500_000_000; // TRẦN
const MAX_MANUAL_DENSE_FMA_ITERATIONS: u32 = 2_000_000_000; // trần cho override tường minh

fn suggested_dense_fma_iterations(duration_seconds: u64) -> u32 { /* công thức chuẩn */ }
fn resolved_dense_fma_iterations(args: &ComputeOracleArgs) -> u32 {
    args.dense_fma_iterations.unwrap_or_else(|| suggested_dense_fma_iterations(args.minimum_duration_seconds))
}
```

Số đo thực nghiệm làm căn cứ (lệnh `dit-cli compute-oracle-run --workers 1 --minimum-duration-seconds 0 --dense-fma-iterations N`):

| iterations | elapsed |
|---|---|
| 200,000 | ~1ms |
| 2,000,000 | ~3ms |
| 20,000,000 | ~29ms |

Tuyến tính rõ ràng (~1.45ms / triệu iteration). `TRẦN=500,000,000` ước tính ~725ms/burst — an toàn so với thời lượng burst thực tế trong một phiên compute-oracle.

Giá trị đã resolve được ghi vào `ComputeOracleReport.dense_fma_iterations_per_burst` (field mới, `schema_version` nâng từ `2` lên `3`) để replay không cần suy ngược từ `duration`.

Đã kiểm chứng end-to-end: `--minimum-duration-seconds 61` (ngay trên ngưỡng) → `dense_fma_iterations_per_burst = 500,000` (khớp chính xác `200,000 + 1×300,000`), `status=pass`.

Test: `dense_fma_iterations_stay_flat_below_threshold_then_ramp_and_cap`, `explicit_dense_fma_iterations_overrides_the_duration_derived_ramp` trong `crates/dit-cli/src/main.rs`.

## Roadmap còn lại: CPU-NVMe (`matrix_rounds`) — chưa code, đã nghiên cứu đủ để tiếp tục ngay

Vị trí chính xác đã xác nhận qua code (không cần dò lại):

- Field: [`RunConfig.matrix_rounds: u32`](../crates/nvme-engine/src/lib.rs) tại `crates/nvme-engine/src/lib.rs`, `impl RunConfig::new()` đặt baseline `matrix_rounds: 4`.
- Trần đã tồn tại sẵn: [`validate()`](../crates/nvme-engine/src/deep_run.rs) tại `crates/nvme-engine/src/deep_run.rs` có `!(1..=32).contains(&config.matrix_rounds)` — nghĩa là `TRẦN = 32` đã được validate, **không cần chọn số mới**, chỉ cần áp dụng bậc thang + ramp để chọn giá trị trong `1..=32` thay vì hằng số cố định `4`.
- **Chưa có CLI flag nào** expose `matrix_rounds` ở `nvme-cli` (đã grep xác nhận, 0 kết quả cho `matrix-rounds` flag). Đây là điểm khác GPU/CPU-RAM: phải **thêm flag mới**, không phải sửa flag có sẵn.
- `nvme-cli/src/main.rs` có **nhiều điểm** khởi tạo `RunConfig`/set `config.wall_clock_limit`/`config.sustain_until_deadline` độc lập nhau (interactive Deep, auto-storage preserving run, bounded raw run, campaign, simulate...); mỗi điểm đều đã có sẵn biến `duration_seconds` cục bộ. Các điểm đã xác nhận qua grep (dòng số có thể lệch nhẹ theo commit sau, dò lại bằng từ khóa `config.compute_workers = automatic_compute_workers`):
  - dòng ~646-685 (Deep run chính, non-interactive)
  - dòng ~812-900 (auto-storage / data-preserving run)
  - dòng ~1054-1105 (interactive run)
  - dòng ~1247-1358 (một biến thể run khác, có UI)
  - dòng ~1528-1597 (campaign/multi-device)
  - dòng ~1967-1986 (simulate)
  - dòng ~2080: `soak_config.matrix_rounds = 1;` — một override cứng có sẵn cho chế độ soak riêng, **không đụng vào**, giữ nguyên logic đặc thù đó.

### Đề xuất bước tiếp theo cho CPU-NVMe (chưa làm, để AI/người kế tiếp thực hiện)

1. Đo thời gian một batch thật ở `matrix_rounds=1` và `matrix_rounds=32` (dùng `cargo run -p nvme-cli -- simulate --duration-seconds 5 --operations 8` hoặc lệnh Deep bounded có sẵn) để xác nhận `32` vẫn an toàn trong `sustain_until_deadline` mode — **bắt buộc phải đo trước khi chọn `HỆ_SỐ`/`NGƯỠNG`**, không suy ra từ số của GPU/CPU-RAM.
2. Thêm **một** hàm dùng chung `suggested_matrix_rounds(duration_seconds) -> u32` + `resolved_matrix_rounds(...)` (đặt ở một chỗ trung tâm trong `nvme-cli/src/main.rs`, không lặp lại code ở 6 điểm).
3. Thêm **một** CLI flag mới `--matrix-rounds` kiểu `Option<u32>` dùng chung cho các subcommand Deep (kiểm tra xem các struct Args ở 6 điểm trên có chung một `#[derive(Args)]` struct hay tách riêng — nếu tách riêng, cần thêm field vào từng struct, ưu tiên dùng `#[command(flatten)]` nếu clap hỗ trợ tái dùng struct chung để tránh lặp).
4. Gán `config.matrix_rounds = resolved_matrix_rounds(...)` ở từng điểm khởi tạo `RunConfig`, **trừ** điểm `soak_config.matrix_rounds = 1;` (dòng ~2080) — giữ nguyên override đặc thù đó.
5. Ghi giá trị đã resolve vào report (kiểm tra struct report của nvme-cli xem đã có field `matrix_rounds`/tương đương chưa; nếu chưa, thêm field mới, tương tự cách đã làm với `ComputeOracleReport.dense_fma_iterations_per_burst`).
6. Test tối thiểu: 2 test thuần (flat-below-threshold, ramp-and-cap-at-32) + 1 test override-wins, theo đúng mẫu 2 test đã viết cho GPU/CPU-RAM.
7. Chạy `make gpu-check`-tương-đương cho NVMe (`tools/run_task_*_checks.sh` liên quan) để xác nhận không phá vỡ negative campaign/write-budget hiện có.

## Lưu ý khi chỉnh sửa code (rút ra từ quá trình triển khai GPU + CPU-RAM)

- **Đừng tin số đo ở debug build cho các bước liên quan tới truyền dữ liệu lớn.** Khi đo GPU, `cargo run` (debug) với allocation ~1.7 GiB khiến bước so sánh byte-by-byte trong Rust debug (không tối ưu) mất hàng chục giây — tưởng nhầm là do thuật toán mới bị treo, thực ra là do build mode + kích thước dữ liệu, không liên quan gì tới tham số vừa thêm. Luôn thu nhỏ tham số liên quan (`--vram-mib`, `--max-allocation-mib`) khi đo bằng debug build, hoặc đo bằng `--release`.
- **Kiểm tra định dạng dữ liệu (kiểu push-constant, packing byte) ngay sau khi đổi API**, đừng chỉ dựa vào việc code compile được. Một lỗi packing push-constant (GPU) từng khiến `rounds` đọc phải giá trị rác cực lớn, gây vòng lặp hàng tỷ lần — biểu hiện y hệt "treo", dễ nhầm là do tải nặng thật.
- **Sau khi kill một tiến trình đang giữ GPU/tài nguyên hardware, luôn xác minh lại bằng `nvidia-smi`/tương đương trước khi đổ lỗi cho code.** Có lúc treo là do tiến trình cũ chưa nhả tài nguyên, không phải bug mới.
- **Test song song (mặc định `cargo test`) có thể tạo false failure do tranh chấp CPU/timing giữa các test**, đặc biệt các test có `minimum_duration_ms`/deadline. Luôn xác nhận lại bằng `cargo test -- --test-threads=1` (đúng quy ước trong `Makefile`) trước khi kết luận có regression thật.
- **Khi thêm field mới vào một report struct đã có `schema_version` cố định trong code (không phải hằng số suy ra), phải tăng version thủ công** và ghi chú ngắn trong code giống style đã có (`// v2: ...`, `// v3: ...`) — đây là quy ước đã tồn tại trong `compute_oracle.rs`, không phải quy ước riêng của tính năng này.
- **Đo thực nghiệm riêng cho từng module, không suy ra hằng số từ module khác.** GPU dùng compute shader (throughput hàng trăm triệu ALU-op/giây trên hàng nghìn lõi), CPU-RAM dùng scalar/SIMD FMA (một luồng), NVMe dùng matrix check trên I/O path — ba đặc tính hiệu năng hoàn toàn khác nhau; hằng số `HỆ_SỐ`/`TRẦN` của module này gần như chắc chắn sai nếu áp trực tiếp cho module khác.

## Checklist khi triển khai (áp dụng cho mọi module)

- [x] GPU-VRAM: đo thực nghiệm, `Option<T>`, `suggested_compute_rounds`/`resolved_compute_rounds`, test đầy đủ, ghi resolved value vào `ComputeEvidence.rounds`, xác nhận thật trên hardware.
- [x] CPU-RAM: đo thực nghiệm, `Option<T>`, `suggested_dense_fma_iterations`/`resolved_dense_fma_iterations`, test đầy đủ, ghi resolved value vào `ComputeOracleReport.dense_fma_iterations_per_burst` (schema v3), xác nhận thật (`--minimum-duration-seconds 61` → `500,000`, PASS).
- [ ] CPU-NVMe: xem mục roadmap ở trên — đo thực nghiệm, thêm CLI flag mới, wiring 6 điểm khởi tạo `RunConfig`, ghi report, test, chạy lại negative campaign.
