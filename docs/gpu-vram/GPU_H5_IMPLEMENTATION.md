# GPU-H5 — Điều khiển cân bằng tải và độ phủ

Trạng thái 2026-09-20: H5 `implemented` cho đường CUDA (source/software + bounded GTX 960).
Vulkan chưa có pipeline async. Chưa chạy multi-GPU, iGPU, near-full VRAM, soak. Mọi số đo là một lần
chạy trên một máy, pool 64 MiB, và các shape "verifier-bound" là shape tổng hợp (K nhỏ, một verifier
hash mọi output) để buộc verifier thành nút thắt — sau H2 các shape thực tế hiếm khi như vậy.

## Đã triển khai

**Bộ điều khiển v1** ([control.rs](../../crates/gpu-engine/src/control.rs)). Máy trạng thái thuần túy, điều
khiển bằng mẫu và đồng hồ do người gọi cấp, nên mọi quyết định tái phát được từ vết mẫu. Trạng thái
WARMUP / RAMP_UP / BALANCED / THROTTLED / DRAINING / STOPPED.

- **Đo đúng thứ cần đo.** Đại lượng điều khiển là backlog verifier: số output *đang chờ* (Ready, và hàng đợi
  của worker) trên chỗ chờ còn lại sau khi trừ các worker (`waiting_fill`). Batch còn đang chạy trên GPU không phải
  backlog, và một verifier đang bận xử lý cũng không phải backlog (service, không phải hàng đợi). Không dùng GPU usage.
- **Đòn bẩy:** `in_flight_limit` (số batch trên GPU) và **pacing** (khoảng cách tối thiểu giữa hai lần submit) lấy từ
  tốc độ verdict đã làm mịn, nên khi verifier chậm tốc độ tạo batch hội tụ về tốc độ verify thay vì dao động giữa
  dừng hẳn và chạy hết tốc độ. Không bao giờ giảm K/jobs để giảm tải.
- **Ngưỡng** (điểm khởi đầu của roadmap, chưa là default đã validate): tăng dưới 30%, throttle từ 70% hoặc khi xu hướng tăng
  liên tục từ 50%, dừng submit từ 90% (hoặc hàng đợi đầy và ≥50% chờ; tác động ngay trên mẫu, không chờ cửa sổ), hồi
  phục dưới 40%. Hysteresis, cửa sổ 50 ms lấy trung bình, cooldown 2 cửa sổ, mỗi lần đổi một bước, pacing đổi tối đa 25%/bước.
- **Khởi động:** WARMUP với giới hạn 2 (đủ overlap ngay từ batch đầu), tăng theo số verdict và theo thời gian, không
  theo cooldown 250 ms cố định như v0.
- **Hồi phục có kỷ luật:** mỗi bước cần một chuỗi cửa sổ "yên" (không tăng, verdict còn đến, không áp lực); nếu ngay sau đó bị
  throttle lại thì lần sau phải chờ gấp đôi (tới 8 s). Pacing chỉ được thả thẳng về 0 khi khoảng cách submit thực đo được
  gấp ≥2 lần pacing (pacing đang không làm gì); nếu không thì nới từng bước 3/4.
- **Tiến triển tính trong burst**, trên chân trời 500 ms: thời gian chuyển giao giữa các burst không phải là "không
  tiến triển", và stall (30 s không verdict khi còn việc) chỉ tính trong burst hiện tại (lỗi H1/v0: mốc từ burst trước).
- **Áp lực bộ nhớ** ([pressure.rs](../../crates/gpu-engine/src/pressure.rs)): `MemAvailable` dưới 256 MiB hoặc PSI memory
  `some avg10` ≥ 25%. Chỉ đọc, không đổi swap. Có áp lực thì dừng submit và drain, hết thì khởi động lại từng bước;
  áp lực không hết sau 30 s thì kết thúc run với lý do `host_memory_pressure_sustained` thay vì throttle vô hạn.
- **Lý do ghi rõ** trong từng quyết định: `verifier_backlog`, `backlog_rising`, `queue_hard_cap`, `host_memory_pressure`,
  `recovering`, `stalled`… kèm mức fill, xu hướng và tốc độ verdict tại thời điểm đó; report giữ 64 quyết định gần nhất,
  thời gian lưu ở từng trạng thái và bộ đếm.

**Regime.** Mỗi burst được gắn nhãn `GPU_BOUND` / `VERIFIER_BOUND` / `TRANSFER_BOUND` / `BALANCED` từ backlog trung bình,
độ bận của thiết bị, tỷ lệ D2H/kernel và việc controller có đang giữ tải hay không (một run verifier-bound được pacing tốt
có hàng đợi ngắn, nhưng vẫn là verifier-bound). Nhãn chỉ để chẩn đoán, không bao giờ là điều kiện PASS.

**Số verifier worker thích ứng** ([schedule.rs](../../crates/gpu-engine/src/schedule.rs)). Chỉ tăng khi burst verifier-bound và
chưa chạm trần CPU; giữ worker thêm chỉ khi throughput verified đo được tăng ≥10%, nếu không thì trả lại và nghỉ 6 cửa sổ;
giảm dần khi tải qua đi. Khi tăng, pacing được nới theo đúng tỷ lệ năng lực thêm (a→b worker thì pacing × a/b). `--async-workers N`
đơn lẻ cố định số worker; `--async-workers-max` mở trần.

**Coverage floor** ([schedule.rs](../../crates/gpu-engine/src/schedule.rs), CLI `--coverage-floor`, mặc định 0,25).
Là tỷ lệ thời gian tối thiểu dành cho transfer/retention. Bộ lập lịch tự sửa (theo tổng tích lũy) rút ngắn burst compute và,
khi một batch dài hơn phần thời gian cho phép, thêm cặp transfer, để tỷ lệ hội tụ về floor; compute luôn được ≥1 batch mỗi vòng.
Report ghi floor cấu hình và share đạt được. `--coverage-floor 0` khôi phục vòng lặp cũ.

## Kết quả (GTX 960, Ryzen 7 3700X, seed 7, pool 64 MiB, 15 s, một lần chạy mỗi ô)

Evidence: `build/gpu-h5-control-20260920/` (`SUMMARY.md`, `summary.json`, mọi report).

| Kịch bản | Gop/s | Controller | Regime |
|---|---|---|---|
| GPU-bound (K=1024) | 254 | 0 throttle, BALANCED, giới hạn 3 | GPU_BOUND cả 28 burst |
| Verifier-bound, 1 worker, K=16 | 10 | THROTTLED, pacing 11 ms, 5 sự kiện, 8 lần đổi trạng thái/phút | VERIFIER_BOUND cả 35 burst |
| Như trên, worker 1→4 | 15 (+50%) | pacing 6 ms, worker đỉnh 4 | VERIFIER_BOUND |
| Áp lực 3 s rồi hết | 213 | drain rồi hồi phục về BALANCED, 12 mẫu áp lực | — |
| Áp lực không hết | — | STOPPED sau 30 s (`host_memory_pressure_sustained`) | — |

Coverage floor, K=1024 (so với tuần tự đã làm sạch ở H2: 1.204 MiB/s và 110 Gop/s):

| floor | share đạt được | VRAM/PCIe MiB/s (so seq_lean) | compute Gop/s (so seq_lean) |
|---|---|---|---|
| 0 | 0,059 | 108 (9%) | 253 (2,3×) |
| 0,10 | 0,102 | 194 (16%) | 239 (2,2×) |
| **0,25 (mặc định)** | 0,264 | 559 (46%) | 189 (1,7×) |
| 0,40 | 0,413 | 883 (73%) | 147 (1,3×) |
| 0,50 | 0,516 | 1.079 (90%) | 121 (1,1×) |
| 0,60 | 0,618 | 1.306 (108%) | 92 (0,8×) |

K=256 cho đường cong tương tự (floor 0,5 ≈ 93% độ phủ với ≈1,6× compute). Share đạt được sát floor ở mọi mức.

## Điều hiệu chỉnh và điều học được

1. **Ngưỡng ≥80% của mục 3.3 đạt được ở floor ≈0,45–0,5, và cái giá là gần hết lợi thế compute.** Chia thời gian chỉ chia lại
   một tổng cố định: độ phủ 90% tuần tự đi cùng compute 1,1× tuần tự ở K=1024 (1,6× ở K=256). Muốn cả hai thì phải chồng lấn thật
   (transfer trên copy engine riêng lúc kernel chạy): đó là H6, máy này có 2 async engine nên khả thi. Mặc định 0,25 là thỏa hiệp
   đo được (độ phủ ×5 so với trước, compute −25%), không phải đích.
2. **Kết luận của H3 về hàng đợi host 4 slot cần sửa.** "27–31 throttle và −39% thông lượng" là lỗi metric của controller v0
   (đếm cả batch đang chạy và đang verify). Với v1: 0 và 3 sự kiện, 250 và 211 Gop/s (−6% ở K=256). Vẫn nên ≥8 host slot
   nhưng lý do không còn là bị throttle nhầm.
3. **Mô phỏng vòng kín bắt được ba lỗi thiết kế** mà unit test đơn điểm không thấy: (a) trong THROTTLED mỗi cửa sổ ồn lại "giảm
   tải" thêm (333 sự kiện/60 s); (b) sau mỗi hồi phục hàng đợi đầy lại và bị throttle lại (dao động răng cưa, 201 đổi trạng
   thái/30 s); (c) đếm verifier đang bận là backlog. Phần cứng thật lộ thêm hai điều mà mô phỏng không có: thả pacing về 0 khi khoảng
   cách submit chỉ sát pacing (đầy hàng đợi trong vài chục ms), và nới pacing quá mạnh khi thêm worker.

## Chưa làm

- Đưa tuổi của batch cũ nhất và áp lực CPU của verifier vào controller; hoãn hash/corpus phụ khi throttle: roadmap có nhưng chưa làm
  (stall vẫn chỉ theo tiến triển).
- `TRANSFER_BOUND` có nhãn và unit test; đã kích hoạt trên phần cứng ở H6 ([GPU_H6_IMPLEMENTATION.md](GPU_H6_IMPLEMENTATION.md), cấu hình dành riêng).
- Chồng lấn compute và transfer, quota verifier theo workload: đã làm ở H6 ([GPU_H6_IMPLEMENTATION.md](GPU_H6_IMPLEMENTATION.md)). Vulkan async: H4b.
- Áp lực kéo dài trả FAIL vì lỗi async hiện được phân loại là compute failure; phân loại INVALID/hạ tầng là việc của H4a.
- Multi-GPU, iGPU, near-full VRAM, soak. Ngưỡng 30/60/70/90% chưa được tinh chỉnh bằng dữ liệu; cấu hình được chỉ qua code.

## Gate

`make gpu-h5-check` = fmt, Clippy strict, test và, với `DIT_GPU_HARDWARE=1`, test H3/G3 rồi các gate: GPU-bound không
throttle và mọi burst là GPU_BOUND; verifier-bound được throttle, pacing >0, dán nhãn đúng, đổi trạng thái ≤16 lần và ≤60/phút,
bảo toàn, không mất batch; worker thích ứng tăng, không vượt trần và tăng throughput ≥10%; share đạt floor ±0,05 ở 0,25 và 0,5,
độ phủ tăng và compute giảm đều theo floor, mặc định là 0,25; áp lực drain rồi hồi phục; áp lực kéo dài kết thúc run với lý do rõ.
Unit test fake-clock (13 test cho controller, 6 cho scheduler/scaler, 3 cho áp lực): overload → throttle → drain → recover,
burst so với quá tải kéo dài, zero progress, áp lực, nhiễu quanh ngưỡng không gây dao động, tái phát quyết định từ vết mẫu,
đòn bẩy không vượt biên, GPU-bound không bao giờ throttle, verifier bận là service, pacing sát khoảng cách submit không bị thả.
Các gate H1–H3 được ghim `--coverage-floor 0` để giữ nguyên ý nghĩa đo; H3 chạy lại PASS với controller mới.
Từ H6, overlap là mặc định cho `--async-compute`; scenario `floor_default` của gate này ghim thêm `--no-transfer-overlap` để tiếp tục
đo đúng time-slice floor 0,25 nó kiểm tra, không phải lane chồng lấn. Gate đã chạy lại PASS sau toàn bộ đổi của H6 (hàng đợi verifier công bằng
thay kênh riêng từng worker, ngân sách RAM có lane, lane đọc backlog controller, mặc định overlap): evidence `build/gpu-h5-control-20260922/`.
