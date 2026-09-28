# GPU-H1 — Contract, bộ đếm cộng dồn và phép đo

Trạng thái 2026-09-20: H1 `implemented` ở mức source/software và bounded GTX 960
baseline. H2–H8 chưa triển khai; H1 không sửa controller, verifier hay scheduler.
Validation layer, soak, AMD/Intel/multi-GPU và image chưa chạy. Không coi baseline
một lần chạy trên một máy là benchmark hay evidence hiệu quả bắt lỗi thật.

## Đã triển khai

**Bộ đếm cộng dồn và bảo toàn** ([accounting.rs](../../crates/gpu-engine/src/accounting.rs),
[balanced.rs](../../crates/gpu-engine/src/balanced.rs)). `AsyncReport` version 2 cộng dồn
mọi burst của một run. Burst rỗng (deadline tới trước khi submit) tăng `empty_bursts`
và không xóa số liệu trước; `last_burst` chỉ là thông tin phụ. Ledger có thêm
`readback_bytes`/`failed_bytes` và `check_conservation()`: `submitted = verified + failed +
pending`, `completed = readback_ready`, byte readback và byte hàng đợi không lệch. Adapter
chạy kiểm tra này mỗi burst và trên tổng cộng dồn; vi phạm thành lỗi sticky.

**Đặt tên đúng đại lượng.** `*_core_seconds` là thời gian CPU cộng qua các worker (có thể
vượt wall time); `*_event_seconds` là tổng khoảng event theo từng stream (chồng lấn khi
nhiều stream chạy); `*_wall_seconds` là một đồng hồ. `device_busy_wall_seconds` là hợp các
khoảng device của từng batch, tính từ một event gốc chung ghi lúc stream rảnh (không dùng
mốc host). Compare và hash được tách riêng; `queue_wait` là thời gian từ lúc thấy D2H xong
tới lúc verifier bắt đầu. `rates` dẫn xuất: output production MiB/s (wall), verifier MiB/s
mỗi core, tỷ số `output_over_verifier_per_core`, `device_busy_fraction` — là chẩn đoán regime,
không phải điều kiện PASS.

**Đếm lỗi.** Mỗi batch lỗi được đếm (`failed`, `failed_bytes`); trên đường lỗi worker đếm mọi
word lệch (`mismatch_words`) và giữ tối đa 16 mẫu (`Corpus::mismatch_summary`, chỉ chạy
sau khi batch đã biết là sai). `campaign_profile.matrix` nay được điền khi chạy async
(trước đó trống), gồm `compare_core_seconds` và `gpu_busy_wall_seconds`. `input_generation_seconds`
không còn bị ghi 0. Burst rỗng không tạo profile, nên bộ đếm dispatch của CLI không đếm đôi
và `last_compute`/`last_compute_attempt` không bị ghi đè bằng burst rỗng.

**Bằng chứng phần cứng** ([hwhealth.rs](../../crates/gpu-cli/src/hwhealth.rs)), trong khối
`detection` của report (thiếu ở mock và ở report cũ):

- Link PCIe: tốc độ/độ rộng đầu, cuối, min/max thấy được qua sysfs, và delta bộ đếm AER
  (`correctable`/`nonfatal`/`fatal`). Lấy mẫu giữa các generation nên `min` là cận dưới, có thể
  trúng lúc link đang hạ để tiết kiệm điện; không diễn giải là suy giảm.
- Log kernel (`journalctl -k` theo cursor của run): đếm theo họ NVIDIA Xid, device lost, AER
  corrected/uncorrected, amdgpu, Intel, nouveau; khớp thiết bị theo `domain:bus:device` vì dòng
  Xid không ghi function. Không đọc được → `available:false`, `families:null`, không phải 0.
- Stress profile từ thread nền ~1 Hz: công suất TB/đỉnh, % power limit, nhiệt, xung, utilization,
  throttle reasons (mask NVML, giải mã; `null` khi driver không có). Trường mở rộng của
  `nvidia-smi` được thử một lần và tự lùi về truy vấn cũ nếu driver từ chối.
- Scorecard: verified ops và ops/s, `failed_batches`, `mismatch_words`, `failure_records`,
  độ phủ VRAM/PCIe (MiB/s, transfer, `delayed_reread_checks`, sweep), `first_failure` (thời
  điểm và số kết quả đã verify trước đó). "Retention" hiện chỉ là đọc lại sau ngắn hạn.

## Chưa làm (không thuộc H1 hoặc để task sau)

- API submit/poll/completion và thiết bị giả: H4a. Xác nhận khi lỗi và nhãn phân loại
  (`INTERMITTENT`…), phân loại lỗi mismatch thành FAIL thay vì "unavailable": H4a.
- Cửa sổ log kernel quanh từng lỗi (hiện là cả run), lịch sử effective rate theo từng batch
  (hiện chỉ `in_flight_limit` min/max), xác định steady-state: chưa có.
- Controller vẫn là v0 (H5): trong baseline có 21 sự kiện throttle mỗi 20 s ở K=1024 dù
  `out/verify` = 0,69.
- Report multi-GPU tổng hợp không gộp `detection`; mỗi `device-N/report.json` mang khối riêng.

## Gate và evidence

`make gpu-h1-check` = fmt, Clippy strict, test các crate liên quan
([run_gpu_h1_checks.sh](../../tools/run_gpu_h1_checks.sh)). `DIT_GPU_HARDWARE=1` chạy thêm
ma trận 12 run + negative gate và kiểm bằng `jq` (verdict PASS, bảo toàn, campaign khớp
async, không có Xid/device lost, `unfinished=0`). Sửa luôn lỗi Clippy có sẵn của bản WIP
(`collapsible_if`, `RunArgs` bốn bool).

Test mới: bảo toàn dưới 20.000 bước ngẫu nhiên gồm cả chuyển trạng thái bất hợp lệ (bị từ
chối, không đổi bộ đếm), completion bị mất, cộng dồn qua burst và burst rỗng, hợp khoảng,
`mismatch_summary`, `record_async_burst` với đồng hồ giả, parser sysfs/AER/log/throttle với
fixture và dữ liệu thiếu.

Evidence: `build/gpu-h1-baseline-20260920/` (`SUMMARY.md`, `summary.json`,
`environment.json`, `<mode>-K<k>-<cold|steady>/report.json`, `negative-async/report.json`).
GTX 960, driver 580.173.02, Ryzen 7 3700X, CUDA, seed 7, pool 64 MiB (không phải run near-full),
steady 20 s, một lần chạy mỗi ô — chênh dưới ~10% coi là nhiễu.

| K (jobs) | Gop/s seq → async | VRAM/PCIe MiB/s seq → async | TB công suất W | `out/verify` | busy frac |
|---|---|---|---|---|---|
| 256 (1024) | 7,9 → 80,3 | 184 → 63 | 29,8 → 68,5 | 1,24 | 0,39 |
| 1024 (512) | 12,2 → 167,9 | 144 → 70 | 31,8 → 80,7 | 0,69 | 0,77 |
| 4096 (256) | 15,1 → 196,8 | 94 → 51 | 36,2 → 88,0 | 0,20 | 0,92 |

Kết luận có bằng chứng (lưu ý: sau H2, so với đường tuần tự đã làm sạch, lợi thế của async còn ≈1,6–2,9×,
xem [GPU_H2_IMPLEMENTATION.md](GPU_H2_IMPLEMENTATION.md)): verified compute tăng ≈10–14× nhưng tốc độ VRAM/PCIe verified của
async chỉ bằng 34%, 49%, 54% tuần tự (K=256/1024/4096), thấp hơn giả thuyết floor ≥80% của
mục 3.3. Verifier ≈620–650 MiB/s mỗi core; ở K=256 một core không theo kịp output. Công suất
đỉnh chạm 125–128 W (≈105% power limit 120 W), không có throttle bit nào ngoài GpuIdle.
Negative gate: FAIL, `mismatch_words=256` (một word mỗi job × 256 job), `failed=1`,
bảo toàn OK, `hardware_tested=false`. Đây là số đo bounded, không phải soak.
