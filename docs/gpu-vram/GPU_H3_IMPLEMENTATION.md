# GPU-H3 — Ngân sách VRAM/RAM và vòng đời slot

Trạng thái 2026-09-20: H3 `implemented` cho đường CUDA (source/software + bounded GTX 960).
Vulkan chưa có pipeline async nên không đổi. Chưa chạy multi-GPU thật (máy chỉ có một GPU),
iGPU, near-full VRAM hay soak. H4a–H8 chưa triển khai.

## Đã triển khai

**Hai nhóm slot với vòng đời riêng** ([balanced.rs](../../crates/gpu-engine/src/balanced.rs)).
Trước H3 mỗi slot gộp buffer device và buffer pinned 1:1 nên hàng đợi host bằng số batch trên GPU.
Nay:

- device slot: `FREE → GPU_WRITING → COPYING → FREE`, được nhả ngay khi event D2H hoàn tất;
- host slot: `FREE → (DMA_WRITING) → READY → VERIFYING → FREE`, chỉ nhả bằng verdict của CPU.

Mỗi batch giữ cả hai từ lúc submit (dự trù chỗ trước khi submit) và có epoch cho cả hai phía, nên
completion cũ hoặc trùng bị từ chối dù chỉ một trong hai slot đã được dùng lại. GPU có thể chạy batch
tiếp theo trong khi các output trước còn chờ verifier, và hàng đợi host có thể sâu hơn số batch trên
GPU. Sổ cái kiểm bảo toàn mỗi burst: số batch, số byte readback, byte hàng đợi, khớp giữa hai nhóm slot
và các hold quarantine. Mặc định 3 device slot + 8 host slot (trước đó 4 + 4).

**`QUARANTINE`.** Batch lỗi giữ nguyên slot host và byte của nó (tối đa `--quarantine-slots`, mặc định 1, và
không bao giờ giữ hết mọi slot; quá hạn 30 giây thì tự nhả). Đầy thì batch lỗi nhả ngay và được đếm là
`quarantine_refused`. Adapter nhả các hold sau bước kiểm tra toàn vẹn corpus của burst; slot dùng lại được
ngay cho burst sau. Các bước xác nhận (đọc lại, D2H lại, chạy lại) là việc của H4a; H3 chỉ giữ bằng chứng còn nguyên.

**Ngân sách async đúng công thức** ([budget.rs](../../crates/gpu-vulkan/src/budget.rs)). `plan_async` chỉ tính
RAM host cho những gì pipeline thật sự giữ: corpus + staging pinned (2× input), kết quả kỳ vọng, hàng đợi pinned
(host slot × output) và corpus prefetch khi xoay. Không còn các bản sao tạm của đường tuần tự nên ước lượng
chặt hơn và ít phải giảm jobs. Dưới áp lực bộ nhớ, thang backoff dùng chung (`SlotPlan`) giảm theo thứ tự
**độ sâu hàng đợi → số device slot → jobs**, không bao giờ tăng gì, và báo lỗi thay vì cố tăng queue khi ngay
cả kế hoạch nhỏ nhất không đủ. Một corpus nguội giờ giải phóng corpus cũ trước khi dựng (đỉnh RAM là 1 corpus
+ staging, không phải 2). Báo cáo có `async_device_slots/async_host_slots`, `pinned_queue_bytes`,
`corpus_prefetch_bytes` thực tế sau backoff. Từ v1.2.025 (C13 G2) ngân sách còn tính kho corpus (`--corpus-pool`,
mỗi corpus một input + đáp án) và bỏ kho trước bậc backoff đầu tiên; báo cáo ghi `corpus_pool`/`corpus_pool_bytes`.

**Rollback khi cấp phát lỗi.** Mọi tài nguyên của pipeline là RAII cho tới khi pipeline tồn tại (event gốc, vốn là
handle thô, được tạo cuối cùng; trước H3 nó bị rò nếu cấp phát sau đó lỗi). Hết bộ nhớ (thật hoặc mô phỏng)
thì `prepare_matrix_resources` đi xuống thang backoff. Bộ đếm tài nguyên sống (buffer, pinned, stream, event) là
chính xác theo tiến trình, nên test chứng minh không rò rỉ mà không phụ thuộc bộ nhớ GPU của tiến trình khác.

**Cấu hình qua CLI và báo cáo.** `--async-device-slots`, `--async-host-slots`, `--async-workers`,
`--async-batches`, `--corpus-threads`, `--quarantine-slots` (chỉ với `--async-compute`; giá trị sai bị từ chối
trước khi chạm phần cứng). Cấu hình hiệu lực sau backoff nằm trong `async_compute.config`, cùng peak của hai nhóm
slot (`device_in_flight_peak`, `host_slots_used_peak`), `host_queue_bytes`, `device_output_bytes`, `pinned_bytes`.

**Chia CPU giữa các GPU.** Mỗi process worker nhận `DIT_GPU_PEER_COUNT` từ parent và giữ lại ít nhất một phần tư
số core (tối thiểu hai) cho OS/UI/watchdog; số verifier worker là trần chứ không phải mục tiêu (`cpu_share`, được
ghi vào report). RAM host đã được chia đều theo số worker từ G12. Bộ điều khiển v0 nay dùng số batch đang chạy
trên GPU cho giới hạn in-flight thay vì tổng slot.

## Kết quả (GTX 960, Ryzen 7 3700X, seed 7, pool 64 MiB, một lần chạy mỗi ô)

Evidence: `build/gpu-h3-slots-20260920/` (`SUMMARY.md`, `summary.json`, báo cáo từng run). Steady 15 s:

| Cấu hình dev/host | K=1024 Gop/s · throttle | K=256 Gop/s · throttle |
|---|---|---|
| 3/8 (mặc định) | 253 · 0 | 224 · 0 |
| 2/16 | 252 · 0 | 222 · 0 |
| 4/12 | 250 · 0 | 223 · 0 |
| 3/4 | 246 · 27 | 137 · 31 |

Bảng trên đo với bộ điều khiển v0 (evidence của lần chạy đó đã được thay bằng lần chạy lại với controller v1 trong cùng
thư mục). Sau H5, cùng gate H3 cho 3/4: **250 · 0** và **211 · 3**; các cấu hình ≥8 host slot không đổi.

- Hàng đợi host sâu ≥8 không tốn gì ở shape GPU-bound và **không còn sự kiện throttle nào** (H2: 24 mỗi 20 s). Với v0, hàng
  đợi 4 slot bị throttle 27–31 lần và giảm 39% thông lượng ở K=256 — nhưng đó là lỗi metric của v0 (đếm cả batch đang chạy
  trên GPU và đang verify), không phải giới hạn của 4 slot: H5 sửa metric và số đó thành 0–3 sự kiện, −6%
  ([GPU_H5_IMPLEMENTATION.md](GPU_H5_IMPLEMENTATION.md)). Nên giữ ≥8 host slot vì độ mịn, không vì bị throttle nhầm.
- Ở K=256 (verifier-bound), 3/8 đạt 224 Gop/s so với 187 ở H2 (khác cấu hình và một lần chạy, nên coi là xu hướng).
- Áp lực RAM (ngân sách 750 MB, không xoay corpus): kế hoạch giữ 3 device slot và 512 jobs, chỉ giảm host slot 32 → 7
  (3 bước backoff), PASS 273 Gop/s, tổng RAM + reserve < 750 MB. Ngân sách 100 MB: INVALID có report.
- Tám peer giả lập: 1 verifier worker mỗi process, vẫn 240 Gop/s ở K=1024 (một verifier đủ sau H2).
- Negative async: FAIL, `quarantined=1`, `released=1`, `expired=0`, bảo toàn OK, không giữ gì sau burst.

## Gate

`make gpu-h3-check` = fmt, Clippy strict, test và, với `DIT_GPU_HARDWARE=1`, ba gate phần cứng
([h3.rs](../../crates/gpu-cuda/tests/h3.rs)): (1) lỗi cấp phát tại **mỗi** trong 24 bước, cả chế độ liên tục lẫn một
lần, cộng lỗi ở bước đầu (cạn thang) và phục hồi — tài nguyên sống luôn về đúng mức ban đầu, backoff đúng thứ tự,
pipeline thu nhỏ vẫn chạy PASS; (2) 160 batch qua 3 host + 2 device slot (mỗi slot tái dùng hàng chục lần, không
lần nào trước verified completion); (3) quarantine giữ rồi nhả và slot chạy lại được. Cộng với test G3, ma trận cấu
hình slot, áp lực RAM, chia CPU, ngân sách INVALID, cờ sai bị từ chối trước khi chạy và negative gate.
Unit test mới: sổ cái bảo toàn qua 30.000 bước ngẫu nhiên của cả hai nhóm slot (gồm chuyển trạng thái bất hợp lệ,
epoch cũ, replay batch đã xong, quarantine đầy/hết hạn/nhả), thang backoff, chia CPU, planner ngân sách và cờ CLI.

## Chưa làm

- Xác nhận khi lỗi và nhãn phân loại (H4a); trait thiết bị/fake device (H4a). Worker least-loaded thay cho `slot % workers`:
  đã làm ở H6 cho đường CUDA (hàng đợi verifier công bằng, xem [GPU_H6_IMPLEMENTATION.md](GPU_H6_IMPLEMENTATION.md)).
- Giới hạn tuổi của batch cũ nhất chưa là giới hạn cứng của hàng đợi hay input của controller (H5 chỉ dùng stall theo tiến triển).
- Metric fill, hysteresis và coverage floor đã được xử lý ở H5.
- Multi-GPU thật, iGPU và near-full VRAM chưa chạy; chia CPU mới kiểm bằng peer giả lập và unit test.
- Vulkan async (H4b).
