# GPU-H6 — Compute, PCIe và VRAM có lịch riêng

Trạng thái 2026-09-22: H6 `implemented` cho đường CUDA (source/software + bounded GTX 960). Vulkan chưa có
pipeline async nên không đổi. Chưa chạy multi-GPU thật, iGPU, near-full VRAM hay soak. **Overlap giờ là mặc định**
cho `--async-compute` (`--no-transfer-overlap` hoặc `--coverage-floor` quay lại time-slice của H5); ngân sách RAM,
controller và nhãn `TRANSFER_BOUND` đã được xử lý (xem "Đã triển khai" và "Chưa làm"). H4a, H4b, H7, H8 chưa triển khai.

## Đã triển khai

**Transfer lane chồng lấn compute** ([lane.rs](../../crates/gpu-cuda/src/lane.rs), lập kế hoạch thuần trong
[lane.rs](../../crates/gpu-engine/src/lane.rs)). Trước H6, transfer/retention chạy đồng bộ xen giữa các burst nên thời gian
chỉ được *chia lại*: đường cong H5 cho thấy độ phủ ≥80% tuần tự đòi floor ≈0,5 và mất gần hết lợi thế compute. Lane đưa chúng lên
stream riêng (một stream, hai event và một cặp buffer pinned cho mỗi slot, mặc định 2 slot) chạy *trong lúc* kernel GEMM chạy,
tận dụng hai copy engine của GPU. CUDA chỉ được gọi trên luồng chủ như trước; việc CPU (sinh pattern kỳ vọng, so sánh) là
`Action` gửi cho verifier worker và quay về dưới dạng completion. Mỗi slot đi `Idle → Preparing → Copying → Comparing → Idle`.

- **`TransferPlanner`** (thuần túy, test bằng lịch ngẫu nhiên): xoay vùng ghi qua các zone, cỡ chunk 64 KiB–64 MiB (bị chặn bởi
  `--lane-chunk-mib`), đọc lại (retention) mỗi vùng sau vài lần ghi, **không bao giờ để hai thao tác chồng byte** và không ghi đè
  vùng còn chờ xác thực (đọc lại bắt buộc `retention-before-overwrite` trước). Lỗi giải phóng vùng và con trỏ vẫn tiến, nên vùng
  hỏng không làm kẹt lane.
- **Vùng riêng:** lane chỉ dùng pool VRAM; corpus/output của compute nằm ở buffer khác nên không thể đè nhau.
- **Bộ đếm theo chiều:** `h2d_bytes/d2h_bytes`, `h2d_ops/d2h_ops`, thời gian event H2D và D2H riêng, byte đã verified, số op theo
  lớp pattern, số sweep, số đọc lại chờ. Với mỗi lần chạy: `h2d_ops = write_ops`, `d2h_ops = write_ops + reread_ops`,
  byte verified = byte D2H sau khi flush cuối (không còn vùng nào ghi mà chưa đọc lại).
- **Đo chồng lấn thật:** `busy_wall_seconds` (hợp khoảng thời gian thiết bị của lane, từ event) và
  `overlap_with_compute_seconds` (phần giao với khoảng bận của compute).
- **Chặn băng thông:** token bucket (`--lane-rate-mib-s`, hai chiều cộng lại, 0 = không chặn) — cho phép thao tác lớn hơn burst từ bucket
  đầy để không bao giờ đói; hai pha `admit`/`charge` vì cỡ thao tác chỉ biết sau khi planner chọn.

**Hàng đợi verifier công bằng** ([queue.rs](../../crates/gpu-engine/src/queue.rs), [workers.rs](../../crates/gpu-cuda/src/workers.rs)).
Thay các kênh riêng từng worker với `slot % workers` bằng **một** hàng đợi chung: deficit round robin theo byte giữa hai lớp
(kiểm tra compute và kiểm tra lane, trọng số mặc định 3:1, `--lane-weight`). Một lớp không thể bỏ đói lớp kia (kể cả item lớn hơn quantum
rất nhiều), lớp rảnh không lãng phí năng lực, thứ tự trong lớp giữ nguyên, worker rảnh luôn nhận việc tiếp theo (cũng giải quyết
"worker least-loaded" ở H3 cho đường CUDA). Report ghi `fairness` (item và byte đã phục vụ mỗi lớp, đỉnh hàng đợi). Mỗi GPU là một process
với hàng đợi riêng nên quota là theo GPU.

**Workload GPU-local** (`--vram-local`; kernel trong [gemm.cu](../../crates/gpu-cuda/kernels/gemm.cu), PTX và `KERNEL_SHA256` được
sinh lại). Kernel `dit_vram_fill` ghi và `dit_vram_verify` đọc lại một vùng pool hoàn toàn trên thiết bị (không qua PCIe) với dữ liệu
là hàm trộn của (địa chỉ từ, khóa) — không có hai địa chỉ nào cùng dữ liệu, nên lỗi dòng địa chỉ không lọt. Mỗi lần ghi chạy `--vram-passes`
lượt fill+verify với khóa khác nhau; lượt cuối để lại pattern của generation. **GPU tự kiểm** (đếm từ sai trên thiết bị, ≠0 là FAIL
`vram-self-check`) và **checkpoint**: sau vài lần ghi vùng được D2H và CPU so đầy đủ với pattern sinh lại
(`vram-checkpoint`). Report tách *byte thiết bị chạm* (`touched_bytes`, hoạt động) khỏi *byte CPU đã xác thực* (`cpu_verified_bytes`, độ
phủ); chỉ cái sau được tính vào độ phủ. Vùng GPU-local là 1/4 cuối mỗi allocation khi chạy cùng lane DMA. Trần chạm 4 GiB/s để không cướp
thời gian GPU (GTX 960 không chạy kernel đồng thời, nên kernel này chia thời gian với GEMM).

**Profile.** `--workload-profile combined|compute-only|transfer-only` để định vị nút thắt và đo nhiễu lẫn nhau: `compute-only` không
có transfer; `transfer-only` không có compute (với lane thì lane chạy một mình theo lát 0,5 s, không thì vòng transfer tuần tự).
Report có khối `workload` (profile, `transfer_mode` = `sequential`/`overlapped`/`off`, `vram_local`, `compute`). Cờ lane bị từ chối trước
khi chạm phần cứng nếu thiếu `--async-compute`, mâu thuẫn (overlap + coverage floor > 0, hoặc + compute-only) hoặc ngoài dải.

**Tích hợp.** Overlap thay vòng transfer giữa các burst và coverage floor. Lane chỉ nhận việc mới khi
compute đang chạy (hoặc trong lát transfer-only), dừng khi controller ở DRAINING/STOPPED, cancel hoặc có lỗi; burst chỉ kết thúc khi lane
rảnh. Cuối run, `flush_lane` xác thực mọi vùng còn chờ đọc lại/checkpoint. Lỗi của lane thành bản ghi lỗi thường (phase `transfer`, `retention`,
`retention-before-overwrite`, `retention-final`, `vram-self-check`, `vram-checkpoint`) với generation, seed, vùng để phát lại. Pinned staging của lane
(mặc định 2 × 2 × 8 MiB) tính vào `pinned_bytes`; rollback cấp phát đi cùng cơ chế RAII/failpoint của H3 (8 bước mỗi slot).
Cờ được chuyển tiếp sang process từng GPU của multi-GPU.

**Overlap là mặc định (không còn opt-in).** `LaneConfig::pinned_bytes()` báo kích thước pinned của lane;
`--transfer-overlap` bật tường minh (cho script), `--no-transfer-overlap` quay lại time-slice cũ, và tự cho `--coverage-floor`
(vì hai cơ chế lập lịch transfer khác nhau, không cộng dồn được) trừ khi cũng có `--transfer-overlap` tường minh (và floor phải là 0).
`--workload-profile compute-only` luôn tắt lane bất kể cờ nào khác. Gate `default_on` (không cờ lane nào) đo `--async-compute` trần đã
overlap ngay: 2.891 thao tác transfer, 269 Gop/s, `workload.transfer_mode=overlapped`, khớp evidence của `overlap` bên dưới.

**Ngân sách RAM tính cả pinned của lane** ([budget.rs](../../crates/gpu-vulkan/src/budget.rs)). `plan_async` nhận thêm `lane_pinned_bytes`
(từ `LaneConfig::pinned_bytes()`, 0 khi lane tắt) và cộng vào `matrix_host_bytes` trước khi so với ngân sách host — trước H6 khoảng ≈32 MiB
pinned của lane không nằm trong tính toán này (chỉ báo trong report, không được trừ khi lập kế hoạch). `WorkspaceBudget` có field
`lane_pinned_bytes` riêng để báo cáo tách bạch. Backoff bộ nhớ vẫn theo thang cũ (hàng đợi → overlap → jobs); lane không phải là mục tiêu
backoff (số slot của nó cố định theo `--lane-slots`), nên áp lực bộ nhớ nặng có thể vẫn từ chối kế hoạch nếu lane không vừa — đúng như hành vi
"báo lỗi thay vì cố tăng queue" đã có từ H3.

**Lane là bên tiêu thụ tín hiệu backlog của controller** ([lane.rs](../../crates/gpu-engine/src/lane.rs) `active_slot_budget`,
[lane.rs](../../crates/gpu-cuda/src/lane.rs) `poll`). Trước đây lane chỉ dừng hẳn khi controller ở DRAINING/STOPPED hoặc có áp lực, nộp việc
mới ở tốc độ cố định bất kể backlog của compute đang cao hay thấp. Giờ mỗi tick lane nhận cùng `fill` (backlog đang chờ / chỗ chờ còn lại) mà
controller dùng để ra quyết định, và `active_slot_budget(total_slots, fill)` giảm dần số slot được phép mở việc mới: đầy đủ dưới 30% fill,
tuyến tính còn 1 slot ở ≥90% fill, **không bao giờ về 0** — vẫn còn ≥1 slot hoạt động và các đọc lại/checkpoint đã lên lịch luôn hoàn tất, nên
độ phủ không bao giờ mất hẳn (bất biến "coverage không mất khi throttle" của roadmap), lane chỉ ngừng góp thêm áp lực khi compute đang căng.
Đây không phải thiết kế lại state machine của controller (H5 vẫn nguyên ngưỡng/hysteresis đã hiệu chỉnh); lane chỉ đọc tín hiệu backlog có sẵn.

**Nhãn `TRANSFER_BOUND` được kích hoạt trên phần cứng thật.** Roadmap H5 để lại "TRANSFER_BOUND chưa được kích hoạt trên phần cứng" vì cần
D2H > kernel (đủ theo `classify_regime`) *và* backlog không cao (nếu không thì VERIFIER_BOUND thắng trước). Với workload ma trận, cả hai điều
kiện khó cùng đạt: D2H và chi phí so sánh CPU đều tỉ lệ với cùng số byte output, trong khi kernel co lại khi K nhỏ — K càng nhỏ thì D2H/kernel
càng cao nhưng verifier càng khó theo kịp. Đo thực nghiệm tìm ra vùng khả thi: `jobs=64, K=4` (đủ nhỏ để so sánh CPU gần như tức thời) với nhiều
device/host slot và verifier worker hơn mặc định (8 device / 32 host / 12 worker) để backlog không bao giờ chạm ngưỡng — gate `transfer_bound`
đo **4.777/4.777 burst (100%) TRANSFER_BOUND**, 0 GPU_BOUND, 0 VERIFIER_BOUND trên GTX 960.

## Kết quả (GTX 960, Ryzen 7 3700X, seed 7, pool 64 MiB, K=1024 jobs 512, 15 s trừ khi ghi khác, một lần chạy mỗi ô)

Evidence: `build/gpu-h6-lane-20260922-131350/` (`SUMMARY.md`, `summary.json`, `environment.json`, mọi report và log);
baseline đầu tiên (trước các mục bổ sung của lượt này) ở `build/gpu-h6-lane-20260920/`.

| Chạy | Gop/s | Độ phủ VRAM/PCIe MiB/s (CPU đã so) |
|---|---|---|
| chỉ compute | 275 | — |
| chỉ transfer, tuần tự | — | 2.165 |
| chỉ transfer, lane | — | 2.320 |
| tuần tự, floor 0 | 257 | 108 |
| tuần tự, floor 0,25 (`--no-transfer-overlap`, mặc định H5 cũ) | 191 | 561 |
| **`--async-compute` trần, không cờ lane (mặc định mới)** | **269** | **1.366** |
| lane chồng lấn tường minh (`--transfer-overlap`) | 270 | 1.392 |
| lane + GPU-local | 265 | 737 (+ 3.072 MiB thiết bị chạm, 384 MiB CPU đã xác thực) |

- Overlap giữ **98% compute** so với chạy compute một mình và **60% tốc độ lane** so với lane một mình; so với time-slice mặc định cũ:
  **1,42× compute và 2,48× độ phủ** cùng lúc; so với floor 0: 1,05× compute và 12,8× độ phủ. Đây là điều H5 chỉ ra là không thể đạt bằng chia thời gian.
  Chạy `--async-compute` trần (không cờ lane nào) cho đúng số này (269 Gop/s, 1.366 MiB/s, `transfer_mode=overlapped`): mặc định mới hoạt động
  đúng như tường minh `--transfer-overlap`.
- 96–100% thời gian thiết bị của lane trùng với compute; cả bảy lớp pattern đều được phủ.
- **Verifier-bound** (K=16, một worker băm mọi output): lane làm 375 thao tác transfer so với 35 của tuần tự, compute 11,0 so với 9,8 Gop/s:
  độ phủ không mất khi bị throttle và compute không bị lấy mất; cả hai lớp đều được hàng đợi công bằng phục vụ, và `active_slot_budget` giữ lane
  chạy chậm lại thay vì dừng khi backlog của compute cao.
- **`TRANSFER_BOUND`** (`jobs=64, K=4`, compute-only, 8 device/32 host slot, 12 worker, 30 s): 4.777/4.777 burst (100%) TRANSFER_BOUND, 0
  GPU_BOUND, 0 VERIFIER_BOUND — nhãn giờ có bằng chứng phần cứng thật thay vì chỉ có unit test fake-clock.
- GPU-local: compute 96% so với chạy riêng; độ phủ DMA giảm (vùng DMA còn 3/4 và slot chia cho hai loại), byte thiết bị chạm gấp ≈8× byte CPU xác thực.
- Chặn băng thông 300 MiB/s: đo 288 MiB/s.
- Âm: lỗi transfer chèn vào lane → FAIL, đúng một bản ghi phase `transfer`; lỗi matrix → FAIL phase `compute`, lane sạch. Test phần cứng chèn từng loại
  (transfer, retention, vram-self-check, vram-checkpoint) → đúng một bản ghi, đúng phase, sổ cái vẫn bảo toàn và lane chạy tiếp.

## Điều học được và giới hạn

1. Độ phủ của lane tốn CPU (sinh pattern ≈ 1,1 core-giây/6 s ở 8 MiB, so sánh ≈ 0,7): với shape verifier-bound, lane và compute tranh cùng worker; hàng đợi
   công bằng đảm bảo hai bên cùng tiến chứ không tạo thêm năng lực. `--lane-weight` chỉnh tỷ lệ.
2. Trên GTX 960 (`CONCURRENT_KERNELS=0`) kernel GPU-local không chạy song song GEMM; chi phí đo được nhỏ ở trần 4 GiB/s.
3. Bản debug của test chậm hơn nhiều và không đủ thao tác cho cả bảy lớp pattern trong 6 s (một generation trong tám là pattern hệ thống); test phần cứng chờ đủ số lần ghi thay vì thời gian cố định.
4. **`TRANSFER_BOUND` cần cấu hình lệch khỏi mặc định để tách khỏi `VERIFIER_BOUND`.** D2H và chi phí so sánh CPU cùng tỉ lệ với số byte output;
   với slot/worker mặc định, cấu hình đủ nhỏ để D2H vượt kernel cũng luôn khiến backlog vượt ngưỡng verifier-bound trước (đo được: cùng
   `jobs=64, K=4` nhưng 3 device/8 host/2 worker mặc định cho VERIFIER_BOUND, không phải TRANSFER_BOUND). Cần nhiều device/host slot và
   verifier worker hơn hẳn mặc định để backlog không bao giờ chạm ngưỡng; ở mức biên (6/24/8) kết quả dao động theo tải máy lúc đó (62–100%
   TRANSFER_BOUND qua các lần chạy), nên gate dùng cấu hình dư dả hơn (8/32/12, 30 s) và ngưỡng kiểm ≥75% thay vì đòi 100% tuyệt đối.
5. **Không thể chỉ chặn hẳn lane khi controller THROTTLED.** Thử đầu tiên (chặn `try_start` hoàn toàn khi state là `Throttled`) mâu thuẫn trực
   tiếp với bất biến "coverage không mất khi throttle" của roadmap — trong shape verifier-bound, controller ở THROTTLED gần suốt burst nên lane
   gần như không mở việc mới. `active_slot_budget` (giảm dần, không bao giờ về 0, theo `fill` liên tục thay vì theo state rời rạc) giữ được cả
   hai: lane vẫn tiến, và vẫn nhẹ tay hơn khi backlog cao.

## Chưa làm

- Overlap là mặc định nhưng chưa đo multi-GPU thật, iGPU, near-full VRAM hay soak với nó bật; các môi trường đó vẫn nên dùng
  `--no-transfer-overlap` cho tới khi được đo.
- Thu link width/generation dưới tải và AER dùng lại instrument của H1 (`detection`), H6 không thêm gì; `environment.json` ghi link hiện tại lúc chạy.
  Data PASS không chứng minh không có lỗi corrected và mismatch không cho kết luận về lane vật lý cụ thể.
- `active_slot_budget` chỉ đọc backlog của **compute**; lane không có tín hiệu quá tải của riêng nó (ví dụ hàng đợi lớp `LANE` trong
  `FairQueue` đang dài) đưa ngược vào quyết định của chính nó, và giới hạn `in_flight_limit`/pacing của compute không tính đến việc lane
  đang chiếm bao nhiêu CPU verifier — H5 vẫn kiểm soát compute như trước H6.
- Vulkan (H4b): lane, kernel GPU-local và profile chỉ có ở CUDA. Xác nhận khi lỗi, phân loại lỗi hạ tầng (H4a); UI/report trực quan cho lane (H7).

## Gate

`make gpu-h6-check` = fmt, Clippy strict, test và, với `DIT_GPU_HARDWARE=1`, [h6.rs](../../crates/gpu-cuda/tests/h6.rs) (lane chồng compute và mọi byte
được xác thực; bốn loại lỗi chèn; lát transfer-only và chặn băng thông; lỗi cấp phát tại mọi bước của lane không rò), H3, G3, rồi ma trận CLI
và [gpu_h6_summary.py](../../tools/gpu_h6_summary.py) (58 kiểm tra tương đối: nhiễu compute ≥90% so với chạy riêng, giữ ≥40% lane, hơn time-slice ≥2× độ phủ và ≥1,2× compute,
verifier-bound không mất độ phủ/compute, bộ đếm theo chiều nhất quán, chặn băng thông, profile cô lập, mặc định trần đã overlap và `--no-transfer-overlap` khôi phục time-slice,
≥75% burst của kịch bản `transfer_bound` là TRANSFER_BOUND và không GPU_BOUND nào, âm). Unit test mới: planner (lịch ngẫu nhiên nhiều độ sâu, không chồng,
đọc lại trước ghi đè, wrap-around, lỗi), token bucket, DRR (trọng số, chi phí theo byte, không đói, thứ tự), hàng đợi worker thật với nhiều luồng, hàm pattern VRAM (theo địa chỉ,
theo khóa, phát lại) và hằng số kernel khớp engine, `active_slot_budget` (đầy dưới ngưỡng, không bao giờ về 0, đơn điệu), `LaneConfig::pinned_bytes`, ngân sách RAM có lane
(`plan_async` với `lane_pinned_bytes`), cờ CLI (kiểm tra, chuyển tiếp, cấu hình, mặc định mới và `--no-transfer-overlap`).
`make gpu-h-check` chạy H1–H6 theo thứ tự trong một lệnh (H4a/H4b/H7/H8 chưa có gate).

Vì H6 thay hàng đợi verifier riêng từng worker bằng hàng đợi công bằng dùng chung và đổi mặc định overlap, `make gpu-h5-check` được chạy lại
làm regression sau toàn bộ các đổi của lượt này: PASS, evidence `build/gpu-h5-control-20260922/` (gate `floor_default` được sửa thêm
`--no-transfer-overlap`, đo lại đúng floor 0,25/share 0,248 như trước H6).
