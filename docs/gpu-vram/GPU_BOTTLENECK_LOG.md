# Log bắt nghẽn GPU-VRAM (`--bottleneck-log`)

Trạng thái 2026-09-23: `implemented` cho đường CUDA async (`--async-compute`); một phần của H7
("UI và report" trong [roadmap](GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md)) — chỉ phần log/report,
chưa có hiển thị trên dashboard tương tác. Đã đo thật trên GTX 960, ba kịch bản khác nhau.

## Vấn đề

Report của H1–H6 đã có đủ tín hiệu để biết vì sao GPU không chạy hết công suất (regime, backlog
verifier, pacing controller, số worker, áp lực bộ nhớ, độ bận của lane...), nhưng rải rác trong
`async_compute.controller.decisions` (tối đa 64 mục gần nhất), `async_compute.regimes` (đếm cộng
dồn cả run) và các field khác — không có **một chỗ duy nhất, theo thời gian thực**, nói "ngay lúc
này cái gì đang cản GPU chạy tối đa."

## Thiết kế

**`gpu_engine::bottleneck`** ([bottleneck.rs](../../crates/gpu-engine/src/bottleneck.rs)) — hàm
thuần `classify(&BottleneckInput) -> BottleneckCause`, không gọi driver, test được như
`classify_regime`. Input là các tín hiệu H5/H6 đã có sẵn (regime, trạng thái controller, pacing,
số worker/trần worker, áp lực bộ nhớ). Thứ tự ưu tiên:

1. `MEMORY_PRESSURE` — áp lực bộ nhớ đang giữ tải xuống hoặc đang drain run.
2. `STALLED` — controller đã dừng hẳn (không phải vì áp lực bộ nhớ, cái đó đã bắt ở trên).
3. `VERIFIER_WORKER_CEILING` — verifier-bound *và* đã ở trần `--async-workers-max`: thêm CPU mới
   giúp được, thêm worker (đã hết chỗ) thì không.
4. `VERIFIER_CPU_BACKLOG` — verifier-bound nhưng còn chỗ tăng worker.
5. `PCIE_TRANSFER` — D2H của output compute lấn át kernel trên timeline thiết bị (nhãn
   `TRANSFER_BOUND` của H5).
6. `PACING_ACTIVE` — không rơi vào các nhánh trên nhưng controller vẫn đang giãn nhịp submit (mới
   hồi phục sau throttle).
7. `NONE` — không có gì cản: GPU-bound hoặc balanced, chạy đúng trần đã cấu hình.

Có chủ đích **không** thêm nhãn kiểu "tăng `--async-device-slots` để nhanh hơn": trên phần cứng
không có concurrent kernels (đo ở H1/H2), thêm device slot chủ yếu giúp overlap D2H/pacing chứ
không tăng compute thô, nên một nhãn hành động kiểu đó dễ gây hiểu lầm.

**CLI** (`crates/gpu-cli/src/main.rs`): mỗi giây (cùng nhịp lấy `telemetry` đã có), lấy
`backend.async_report()` mới nhất, gọi `sample_bottleneck` rồi `classify`:

- `--bottleneck-log FILE` (chỉ với `--async-compute`, việc tuỳ chọn): ghi thêm một dòng JSON vào
  FILE mỗi giây — nguyên nhân, mô tả một câu, và toàn bộ tín hiệu thô đã dùng (regime, trạng
  thái/lý do controller, pacing, `in_flight_limit`/`device_slots`, đỉnh backlog fill, số
  worker/trần, số lần chạm áp lực bộ nhớ, độ bận lane nếu có).
- `bottleneck_summary` trong report JSON: **luôn có** khi `--async-compute` (không cần cờ log) —
  tổng số giây dưới mỗi nguyên nhân và nguyên nhân gần nhất, để có ngay một con số tổng quan mà
  không cần bật log chi tiết.
- Multi-GPU: mỗi process worker ghi log riêng vào `device-N/bottleneck.jsonl` (đường dẫn người
  dùng đưa ra không được ghi trực tiếp bởi nhiều process cùng lúc).

## Đã kiểm — đo thật trên GTX 960

Evidence: `build/gpu-bottleneck-log-20260923/`. Ba kịch bản, mỗi kịch bản khớp đúng nguyên nhân dự
kiến:

| Kịch bản | Cờ | Nguyên nhân đo được |
|---|---|---|
| GPU-bound (K=1024, mặc định) | (không) | `NONE` cả 5 mẫu, `regime=GPU_BOUND`, `pacing_ms=0` |
| Verifier-bound, 1 worker cố định | `--async-workers 1 --hash-policy every-batch` | `VERIFIER_WORKER_CEILING` cả 3 mẫu (`workers=workers_max=1`) |
| Verifier-bound, còn chỗ tăng worker | `--async-workers 1 --async-workers-max 4` | `VERIFIER_CPU_BACKLOG` cả 6 mẫu (`workers` dao động 2–3, `workers_max=4`) |

`bottleneck_summary` trong report khớp: ví dụ kịch bản đầu tiên `samples=4,
seconds_verifier_worker_ceiling=5.29, các mục khác=0, last_cause=VERIFIER_WORKER_CEILING`.

Unit test thuần trong `gpu-engine` (6 test, không cần phần cứng): thứ tự ưu tiên của áp lực bộ nhớ
và trạng thái dừng, phân biệt còn chỗ/hết chỗ tăng worker, transfer-bound không bị che bởi
pacing/worker, pacing chỉ báo khi không có nguyên nhân cụ thể hơn, run sạch báo `NONE`, mọi nguyên
nhân có mô tả. Cộng một test cho `BottleneckSummary::record` trong `gpu-cli`. `make check`/`cargo
test --workspace` PASS toàn bộ, Clippy strict sạch.

## Cách dùng

```sh
gpu-vram-dit run --backend cuda --async-compute --duration-seconds 60 \
    --bottleneck-log bottleneck.jsonl --output-dir out/
# xem theo thời gian thực trong lúc chạy:
tail -f bottleneck.jsonl
# hoặc chỉ xem tổng kết sau khi chạy xong, không cần cờ log:
jq .bottleneck_summary out/report.json
```

## Thời gian của vòng lặp generation (v1.2.023)

**Vì sao.** Phiên COMBINED DEEP `max` v1.2.022 trên TUF Z790 (i9-14900K, RTX 5080) cho thấy vòng lặp của
`gpu-vram-dit` nằm **ngoài** pipeline tính toán khoảng một nửa thời gian của pha chạy chồng và pha xoay B: 100–130 ms
mỗi burst, so với 2 ms ở pha nền GPU và 12 ms ở v1.1 trên cùng máy. Trong lúc đó GPU không được giao việc nên đứng yên.
Hai máy khác chạy v1.2.022 (B760M, Z890M) không bị. Report cũ không nói vòng lặp làm gì giữa các burst, nên giờ mọi bước
đều được bấm giờ. Nguồn: [loop_profile.rs](../../crates/gpu-cli/src/loop_profile.rs).

**Các bước của một generation:** `pipeline` (lời gọi burst hoặc lát transfer), `transfers` (cặp transfer/retention
tuần tự, khi không có lane), `lane_collect`, `link_sample` (link PCIe và AER qua sysfs), `kernel_log` (`journalctl -k`,
10 s một lần), `telemetry`, `bottleneck_sample`, `host_sample` (CPU, RAM, cảm biến cho màn hình và progress),
`ui_state`, `progress_write`, `render`, `fault_events`. Phần không bước nào phủ là `other_seconds`. Mỗi bước có tổng
giây, số lần và lần lâu nhất (`max_seconds`).

**Trong lời gọi pipeline** (đường CUDA async, `async_compute.timing`):

| Trường | Gồm |
|---|---|
| `prepare_seconds` | từ đầu lời gọi đến vòng burst; gồm bốn trường dưới |
| `corpus_wait_seconds` | chờ corpus dựng sẵn chưa xong |
| `corpus_install_seconds` | nạp corpus mới lên VRAM (từ v1.2.026, khi corpus đã được chép sẵn vào staging thì chỉ còn DMA: `corpus_staged_installs`) |
| `origin_sync_seconds` | đóng mốc đồng hồ thiết bị (chờ stream) |
| `setup_seconds` | gắn lane, dựng bộ quét DRAM |
| `loop_seconds` | vòng burst: gửi batch, chờ, giao output cho luồng kiểm |
| `after_loop_seconds` | sau vòng: drain bộ quét, hash cuối, kiểm biên, kiểm toàn vẹn corpus (`integrity_check_seconds`; từ v1.2.026 chỉ sau batch sai, lần kiểm định kỳ chạy trên luồng phụ: `integrity_background_seconds`) |

Thời gian `pipeline` của CLI trừ `prepare + loop + after_loop` là phần đuôi của lời gọi (cộng sổ, dựng report).

**Ở đâu:**

- `report.json`: `loop_profile` (tổng cả lần chạy, kèm `pipeline` là phần chia trên).
- `bottleneck.jsonl`: mỗi dòng có `loop_window`, tức những gì xảy ra kể từ dòng trước: số generation, giây, giây trong
  pipeline, các bước khác từ 1 ms trở lên, `other_seconds`.
- `progress.json`: `loop` là tổng tính tới generation vừa xong. Hộp đen chép nó vào mỗi dòng journal (`gpu_loops`, một
  mục cho mỗi GPU, máy nhiều GPU lấy từ `device-N/progress.json`), nên phiên bị sập vẫn còn số liệu. Lấy hiệu giữa hai
  dòng journal để có tỉ lệ của từng khoảng.

**Đọc thế nào:** tỉ lệ `pipeline / wall_seconds` khoảng 95 % là bình thường (máy dev và hai máy thực địa trên). Nếu thấp,
bước nào chiếm phần còn lại thì đó là chỗ kẹt; nếu chính `pipeline` dài mà GPU vẫn rảnh, xem phần chia trong pipeline.

**Kiểm trên máy dev** (3700X + GTX 960, COMBINED BOUNDED `max` thu nhỏ 20 %, có hộp đen chạy kèm): pha chạy chồng 60 s,
309 generation, `pipeline` 57,1 s (96 %), `host_sample` 2,3 s (lần lâu nhất 93 ms), `progress_write` 0,17 s,
`kernel_log` 0,10 s; trong pipeline `loop` 54,4 s, `prepare` 1,1 s (nạp corpus 0,54 s), `after_loop` 1,7 s (kiểm toàn
vẹn corpus 0,85 s). Journal có `gpu_loops` ở mọi dòng của pha có GPU.

## Chưa làm

- Chỉ có ở đường CUDA async; đường tuần tự và Vulkan không có (không có controller/regime để phân
  loại theo cùng cách).
- Chưa hiển thị trên dashboard tương tác (phần UI của H7 vẫn `planned`); hiện chỉ là file log +
  field report.
- Nhịp lấy mẫu cố định 1 giây, không cấu hình được qua CLI.
- `lane_fill` trong mỗi dòng log là đỉnh tích luỹ từ đầu run (`in_flight_peak`/`slots`), không phải
  độ bận tức thời của lane tại giây đó — CLI chỉ thấy được report tích luỹ, không thấy trạng thái
  sống bên trong pipeline.
