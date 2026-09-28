# CPU-RAM availability / power-platform — Bước 3C

Source implementation: 2026-09-19. Hardware effectiveness chưa `validated`.

Hai module Rust riêng trong `dit-system`:

- `availability.rs`: tình trạng thực thi và durable checkpoint writer.
- `power_platform.rs`: campaign chuyển tải đồng bộ, per-core/SMT sweep, report và
  phục hồi phiên bị gián đoạn.

## Campaign thống nhất — 3C-U

Source đã có campaign nhiều pha, duration + complete coverage round, UI chung
và mixed/heavy kernels. Theo
[roadmap 3C-U1..U6](ROADMAP.md#3c-u--hợp-nhất-power-transitions-và-coresmt-sweep),
U5 đã có seeded variable timing; U6 đã có persistent workers, bounded JSON report,
heartbeat riêng và schema validator. Artifact/runtime opcode verification vẫn
chưa hoàn tất; xem STATUS cho image smoke và hardware evidence riêng.

Menu image có mục **[4] CPU AVAIL / POWER**, mở launcher cùng bố cục console và
sensor collector của CPU-RAM. **[1] START AVAILABILITY / POWER TEST** chạy một
campaign; **D** chọn Quick 2 phút / Standard 10 phút (mặc định) / Extended 30 phút
hoặc custom 1..1440 phút. Preset v1 chỉ là minimum duration, không phải mức chứng
nhận. **[2] Advanced** giữ Power-only / Core-SMT-only và **N** đổi cycles/group
(1..1000). **T** bật/tắt safety nhiệt 95 C, **B** trở về menu. Không autostart.

Mỗi vòng chạy SINGLE_THREAD → CORE_SMT → ALL_CPU → MIXED_SMALL/MIXED_ALL;
mỗi nhóm/role chạy mixed rồi heavy, một transition mỗi kernel trước khi chuyển
nhóm. Không có background all-core stress trong pha isolation. Campaign chỉ
kết thúc bình thường khi đủ minimum duration monotonic và trọn vòng; timeout,
safety và thiếu coverage không được PASS. Topology thiếu được công bố và không
cho PASS core/SMT coverage. Plan `DIT-POWER-CAMPAIGN-v3` lưu group ID/CPU IDs,
role/kernel và resolved timing. Thứ tự nhóm giữ nguyên để không bỏ mandatory role;
seed quyết định timing class/jitter và dữ liệu tính toán.

```sh
dit-cli power-platform-run --run-id campaign-001 --workers 2 \
  --minimum-duration-seconds 600 \
  --checkpoint /path/operator-authorized/campaign-001.json
```

`--minimum-duration-seconds` (1..86400) bật campaign, không kết hợp với
`--core-sweep` hoặc `--transitions`. Không truyền duration giữ nguyên diagnostic
count-based. API Rust dùng `minimum_duration_ms > 0`, `transitions=1`,
`core_sweep=false`. Hard deadline được resolve bằng minimum duration + ngân sách
một vòng + 30 s; per-transition timeout vẫn riêng.

Giới hạn: tối đa 4096 step/vòng; report chỉ giữ 128 transition chi tiết gần nhất,
không còn dừng campaign tại 10000 transition. `total_transitions`,
`verified_transitions`, `total_bursts`, `total_mismatches`, `step_verified` và
`transitions_omitted` giữ coverage tổng; `first_failure` giữ lỗi đầu tiên ngay cả
khi chi tiết đó ra khỏi cửa sổ 128. Không diễn giải `transitions.length` thành
tổng lượng công việc. Console stream không phải kho history đầy đủ của report;
operator vẫn cần quản lý dung lượng log khi chạy CLI dài.

Launcher chạy qua `dit-watchdog`, lưu checkpoint `power-platform.json`,
`power-console.log` và `summary.txt` dưới session trên result partition đã xác
thực; thiếu partition thì giữ trong `/run` và công bố không có persistence.
UI hiện phase, transition completed/target, burst count, failures, seed, live
CPU/RAM usage, temperature/power và hai trạng thái evidence riêng. `INCOMPLETE`
giữ nguyên trên màn hình kết quả ngay cả khi watchdog trả exit code tổng 2.
Ctrl+C dừng process group qua watchdog rồi trở lại UI.

Màn hình chạy ở terminal từ **110 cột × 32 dòng** dùng dashboard như CPU-RAM:
khung **SYSTEM HARDWARE** (CPU, threads, RAM, NUMA, GPU, board, kernel), khung
**LIVE UTILIZATION** với thanh CPU/RAM usage và khung **TESTING / PROCESS
INFORMATION** với thanh tiến trình. Terminal nhỏ dùng giao diện compact, vẫn có
thanh CPU/RAM usage và tiến trình. Campaign tách `MIN TIME` khỏi `ROUND COVERAGE`,
hiện active CPU IDs/kernel/rounds và "Finishing current coverage round" khi đủ
thời gian nhưng vòng chưa xong. Diagnostic cũ vẫn tính phần trăm theo transition.
Không dùng thanh thời gian để khẳng định đã hoàn thành hoặc đã bao phủ toàn RAM.
Không hiển thị `DIT TARGET` như bài RAM exact: power-platform chỉ dùng scratch
64 KiB/worker. `VCORE unavailable` và evidence riêng vẫn được giữ; 100% không
đồng nghĩa đã validated độ ổn định nguồn.

Image cũ cần rebuild bằng `make shared-image` hoặc
`make nvidia-image NVIDIA_DRIVER_PACKAGE=...`; flavor generic và NVIDIA là các
artifact riêng, cập nhật một flavor không cập nhật flavor còn lại. Từ 2026-09-22 có thêm đường
không cần rebuild rootfs riêng cho NVIDIA: `make nvidia-driver-package` +
`make nvidia-driver-burn-usb` đóng driver thành gói cài lúc boot trên image generic — xem
[NVIDIA_DRIVER_PACKAGE.md](../gpu-vram/NVIDIA_DRIVER_PACKAGE.md).

## U5 — Timing recipe và watchdog

Mỗi vòng có đủ `SHORT_SHORT`, `SHORT_LONG`, `LONG_SHORT`, `LONG_LONG` trên toàn
plan (không khẳng định mỗi kernel/core có cả bốn class trong một vòng).
Với base mặc định idle 500 ms, burst 100 ms: short idle 125..250 ms, long idle
500..1000 ms; short burst 25..50 ms, long burst 100..200 ms. Seed xoay class và
tạo jitter xác định; recipe được lặp qua các vòng, transition seed vẫn thay đổi.
Replay cần cùng seed, selected CPU IDs/topology, config và plan version; không
hứa tái tạo timing vật lý, C-state hoặc điện áp thực.

Campaign yêu cầu base idle/burst <=30000 ms và timeout lớn hơn
`2 * max(base burst, 2)` ms. Diagnostic cũ giữ timing cố định. UI hiện resolved
idle/burst/class đang chạy, không còn ghi cứng 500/100 cho campaign.

```sh
dit-cli power-platform-watchdog-seconds --campaign --idle-ms 500 --timeout-ms 5000
```

Lệnh read-only trên trả 16 giây theo policy
`max(15, ceil((max_idle + 2*timeout + 5000)/1000))`; campaign dùng
`max_idle = 2*max(base idle,2)`. Launcher gọi cùng policy trước khi chạy; thiếu
CLI tương thích thì INVALID thay vì chọn deadline đoán. Với CLI tự bọc watchdog,
phải truyền cùng timing vào policy và dùng `--progress-report` làm heartbeat.

## U6 — Worker pool và durability

Một thread được tạo, pin và prefault scratch cho mỗi selected CPU. Toàn pool
hoàn tất initialization trước pha isolation đầu; worker ngoài nhóm chờ channel
blocking, không chạy background all-core load. Jobs/ack mang generation và CPU ID;
stale generation, duplicate participant, wrong phase hoặc worker crash không PASS.
Drop/error/cancellation đánh thức gate, đóng channel và join worker; idle có
cancellation checks, compute có checks giữa các chunk hữu hạn.

`--progress-report` nay là snapshot nhẹ `kind=power-platform-progress`, schema 1,
atomic rename **không fsync**; không phải file recovery. Coordinator cập nhật nó
trong khi chờ worker (250 ms) hoặc idle (100 ms), độc lập với hot compute loop.
Watchdog không thể dùng heartbeat sống để kết luận worker có tiến triển: engine
vẫn áp ready/completion timeout riêng. Pool startup/reference/checkpoint có thể
tạm ngừng heartbeat; supervisor budget có overhead margin tương ứng.

Checkpoint chính vẫn write-temp + fsync + rename + directory fsync. Campaign
ghi START, BURST_ARMED đầu, FINAL và lỗi ngay; phase boundary bình thường chỉ
ghi khi đã qua 5 s. Đây là **cadence tại boundary**, không cam kết mất tối đa
5 s evidence: phase dài có thể làm checkpoint chậm thêm một phase. Normal burst
không fsync; mismatch quan sát được là ngoại lệ được lưu ngay để không mất FAIL
nếu worker khác sau đó timeout. Diagnostic cũ giữ checkpoint mỗi boundary.
Recovery chỉ dùng checkpoint bền, giữ context lần commit cuối và không suy
marker `BURST_ARMED` thành chứng minh đã release hoặc đã có sụt Vcore.

Gate tái chạy: `make test-power-campaign` (host, 2 worker, hai seed replay cùng
seed, mỗi run tối thiểu 5 s); `make test-power-image` (QEMU TCG direct-kernel boot
từ image, snapshot disk, không ghi image gốc). Image smoke không kiểm đường
GRUB BIOS/UEFI hoặc hiệu quả lỗi nguồn trên hardware. Kernel parameter
`cpu_dit_power_duration=1` chỉ đặt minimum duration của profile power-campaign;
không bỏ coverage, oracle hoặc safety gate.

## Diagnostic count-based tương thích

```sh
dit-cli power-platform-run --run-id power-001 --workers 2 \
  --transitions 10 --idle-ms 500 --burst-ms 100 \
  --checkpoint /path/operator-authorized/power-001.json
```

`--checkpoint` bắt buộc là đích được người vận hành cho phép; khi boot USB phải
dùng result partition đã được launcher xác thực. CLI không tự mount, không
fallback sang ổ nội bộ. File đã tồn tại bị từ chối để giữ evidence cũ.

Thêm `--core-sweep` để chạy từng logical CPU đã chọn, nhóm sibling theo topology
sysfs rồi toàn nhóm đã chọn. `--transitions` áp dụng cho **mỗi nhóm**; report lưu
`worker_groups` và `topology`. Không suy sibling bằng công thức CPU N + số core.

Các worker được pin, cấp scratch RAM trước khi báo ready, cùng chờ condition
variable. Coordinator đợi đủ worker, ghi `BURST_ARMED`, chờ idle rồi release
chung. Mỗi burst kiểm FMA, branch, AES và 64 KiB scratch/worker; mọi lần ghi cuối
đều có readback. Đây là scratch coverage, không thay full-RAM `system-run`.

Số burst từng worker phải dương và start spread không quá
`--max-start-spread-ms` (mặc định 50 ms). Timeout chờ ready/completion được giới
hạn bằng `--timeout-ms`; coordinator lỗi vẫn cancel/join worker. Kernel hard-lock
cần supervisor/platform recovery bên ngoài. Có thể bọc command bằng
`dit-watchdog supervise`, dùng progress-report riêng làm heartbeat và đặt stale deadline
phù hợp tổng thời gian idle/burst/chờ của transition.

`--fma-iterations` mặc định 200000, giới hạn 1..2000000 để mỗi compute chunk hữu
hạn. Power profile dùng transition count và burst duration riêng; tăng FMA depth
theo duration ở đây có thể làm giảm số cạnh tải nên không tự áp ramp sustained.

## Kernel và evidence mới

- `mixed-oracle-v1`: FMA một chuỗi vector, integer/branch, AES known-answer và
  scratch verification như diagnostic cũ; AES không được quảng bá là crypto stress.
- `heavy-fp-v1`: 8 accumulator `f64x4` độc lập, 32 lane, mỗi lane có scalar
  reference riêng; relative tolerance 1e-9 (expected zero dùng absolute error),
  NaN không PASS. Mỗi chunk tối đa 4096 iteration/accumulator, lặp đủ burst time;
  kiểm mọi chunk và final scratch readback, cancellation giữa các chunk.
- Heavy dùng portable SIMD, có thể fallback theo compile features. `black_box`
  bảo vệ đầu vào/kết quả; chưa chứng minh opcode hay peak power bằng tên kernel.
- Power report schema **3**, recovery vẫn đọc **1/2**: lưu plan/config, worker counts
  requested/available/started/ready/completed (theo nhóm active), safety evidence,
  kernel, iterations/chunk, FP lane updates và actual burst ms từng worker.
  FP lane updates không phải tổng instruction hoặc benchmark FLOPS. Transition ID
  ánh xạ vào plan bằng modulo số step; seed transition = session seed + ID (wrapping).
  `pool_workers_started` là số thread tạo cho phiên; per-transition `started` là
  worker đã nhận dispatch, không phải số thread được tạo lại. Validator schema 3
  kiểm retained/omitted totals, recipe deterministic và complete PASS coverage.
- Progress campaign dùng `target=0` vì chưa biết tổng transition cuối cùng;
  consumer dùng `round_target`, `round_completed`, `rounds`, `minimum_ms`,
  `elapsed_ms` để hiển thị. Diagnostic vẫn có target cố định.

## Verdict và recovery

- `verdict`: `PASS`, `FAIL`, `INCOMPLETE`; exit code lần lượt 0, 2, 3.
- `availability.status`: `RUNNING`, `COMPLETED`, `INCOMPLETE`, `INTERRUPTED`,
  `WORKER_FAILED`, `STALLED` tùy engine/supervisor.
- `power_platform.status`: `NO_EVIDENCE`, `SUSPECTED`,
  `HARDWARE_EVENT_OBSERVED`. Hiện hardware event lấy từ EDAC/MCE evidence;
  chưa có provider để tự phát kết luận Vcore/VRM/PSU.

Mismatch là FAIL ngay cả khi execution hoàn tất. Thiếu worker/burst/transition
không được PASS. Một marker chưa đóng chỉ chứng minh gián đoạn. Snapshot trước/
sau transition và telemetry/safety toàn campaign giữ riêng trong report.

```sh
dit-cli power-platform-recover /path/operator-authorized/power-001.json \
  --report /path/operator-authorized/power-001-recovered.json
```

Recovery giữ file nguồn, yêu cầu output mới, kiểm boot ID/PID và từ chối recovery
khi owner process còn tồn tại trong cùng boot. File đang `RUNNING` được chuyển
thành `INTERRUPTED` + `INCOMPLETE`, giữ phase cuối và không tự quy lỗi power.
Nếu checkpoint đã ghi mismatch/FAIL, recovery giữ `FAIL` cùng `INTERRUPTED`.
Exit 0 của lệnh recovery nghĩa là ghi recovered report thành công, không phải
campaign PASS. `BURST_ARMED` nghĩa là đã chuẩn bị release; mất nguồn ở đây không
chứng minh worker đã bắt đầu burst.

Checkpoint dùng flush + file sync + rename + directory sync. Ghi phase boundary,
không ghi hot loop. Telemetry thiếu được lưu theo availability; Vcore chưa đo
được và idle chưa chứng minh C-state residency.

## Thay đổi CPU-RAM hiện có

- System report schema **10** thêm `availability`, `power_platform`, compute
  coverage gate, requested worker count, minimum bursts và resolved FMA depth.
- `system-run --compute-oracle-dense-fma-iterations` là override tùy chọn; mặc
  định dùng duration ramp chung với standalone. Compute thiếu minimum work mà
  không có correctness/safety failure được trả `INVALID` cùng availability
  `INCOMPLETE`; giữ enum top-level cũ để tương thích adapters.
- Standalone compute report schema **4** thêm coverage và availability;
  verdict bổ sung `incomplete`. Minimum là một burst đã kiểm mỗi worker.
- System progress thêm file/directory sync. Watchdog schema **2** thêm hai evidence
  riêng; signal/OOM/stall không bị tự gán thành lỗi nguồn.
- Session validator đọc schema 9 lịch sử và 10; schema 10 PASS phải có availability
  `COMPLETED` và compute coverage đạt khi enabled.

## Phạm vi đã có và phần còn lại

Đã có CLI riêng, synchronized transitions, minimum-work gate, durable checkpoint,
recovery CLI, per-core/SMT grouping, telemetry trước/sau và fault tests.
Instruction evidence ghi CPU support và compile features; `executed=null`,
`verified=false` khi chưa có bằng chứng opcode của artifact. Correctness của
kernel không được tự chuyển thành xác nhận đã chạy AVX2/FMA/AES-NI cụ thể.

Các mục 3C còn mở: tự hợp nhất system/watchdog journal thành recovered result ở
menu boot, IMC latency/local-cross-NUMA diagnostic và xác nhận
instruction path bằng artifact/runtime evidence. Không đánh dấu toàn bộ Bước 3C
`implemented` chỉ vì CLI mới chạy được. Hardware reset/Vcore campaign là validation
riêng, không chặn phát triển các mục còn lại.
