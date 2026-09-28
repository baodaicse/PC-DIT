# CPU-DIT-01 Implementation Status

**2026-09-25 — UI:** Full nay chạy thêm compute oracle (FMA/AVX, rẽ nhánh, AES-256; ADR-0004) trên 1/4 CPU logic đồng thời với RAM (`--compute-oracle-workers`), RAM dùng phần còn lại; mục mới **[5] CPU COMPUTE**: RAM 256 MiB trên 1 CPU, compute oracle trên mọi CPU còn lại, có idle/burst ramp (`--compute-oracle-idle-ramp`); `[D]` đặt thời lượng cho cả Full và CPU COMPUTE. Đã chạy thử hai bộ tham số bằng `dit-cli system-run` trên máy 16 luồng, 60 s: PASS, compute oracle 15 và 4 worker. Chưa chạy qua image/USB.

> Từ 2026-09-06, tài liệu này thuộc module trong project chung DIT-01. Dùng [README chung](../../README.md) và [status chung](../STATUS.md) cho lệnh build/trạng thái hợp nhất; các mốc bên dưới giữ lịch sử module.

Tài liệu này là sổ trạng thái triển khai. Mỗi lần hoàn thành một phần cụ thể phải
cập nhật ngày, artifact, test evidence và giới hạn còn lại tại đây. Roadmap mô tả
đích đến; STATUS chỉ phản ánh những gì đã thực sự chạy được.

## Quy tắc trạng thái

- `planned`: mới có trong docs.
- `in_progress`: đã bắt đầu code nhưng chưa qua gate.
- `implemented`: code và test tự động tương ứng đã pass.
- `validated`: đã qua hardware matrix được ghi rõ.
- `blocked`: không thể tiếp tục nếu thiếu quyết định hoặc hardware bắt buộc.

## Tổng quan

| Bước | Trạng thái | Bằng chứng |
|---|---|---|
| 1. Reference pipeline | `implemented` | Golden/reference tests và release CLI PASS |
| 2. Adaptive RAM engine | `implemented` | multi-arena/tail/fault tests; device 512 MiB PASS |
| 3. Multi-core/SIMD | `implemented` | 16 logical CPU, scalar/SIMD/KAT/coherency PASS |
| 3A. Pattern/window coverage | `implemented` | Schema v7; 51 tests; 64 MiB/two-seed device run PASS |
| 4. Telemetry/diagnostic | `implemented` | Safety v2; temperature toggle/trusted-sensor regression PASS |
| 5. Ubuntu live USB | `implemented` | Live progress UI; raw image; QEMU BIOS + UEFI DIT PASS |
| 3B. Compute-oracle (standalone + concurrent) | `implemented` | ADR-0004 Phase 1+2; schema v9; 49 tests incl. fault-injection PASS |
| 6. Hardware validation | `planned` | Protocol/matrix đã có; chưa chạy USB vật lý |

## Nhật ký hoàn thành

### 2026-09-19 — 3C-U5/U6: seeded timing, persistent pool và schema 3

U5 `implemented` tại source/host gate. Plan `DIT-POWER-CAMPAIGN-v3` chứa bốn lớp
idle/burst short/long, jitter deterministic theo seed; CLI timing policy dùng
chung cho launcher/watchdog, UI hiện timing thực đã resolve. Diagnostic count-based
vẫn giữ fixed timing và ý nghĩa transition cũ.

U6 `implemented` với source/host gates và generic image direct-kernel smoke:
pool được pin/prefault toàn bộ trước phase
đầu, inactive workers chờ channel; generation + CPU ID checks loại stale/duplicate
ack. Progress heartbeat nhẹ tách khỏi durable checkpoint; 128 transition chi tiết
gần nhất, tổng counters/per-step coverage và first failure không mất khi evict.
Schema 3 validator kiểm counters/recipe/PASS, recovery vẫn đọc schema 1/2.

Evidence: 14 power tests gồm timing replay, cancellation, stable thread IDs,
stale/duplicate events, worker crash/timeout/affinity/mismatch, history 10000
transition giả lập và checkpoint cadence. Host `make test-power-campaign` hai
lần cùng seed qua watchdog PASS, same plan, 2 worker được dùng lại; artifact:
`build/power-host-n1frK8/`. UI + boot policy PASS; Clippy workspace strict PASS.
57 system unit tests, 4 CLI, 2 watchdog và 14 orchestrator tests PASS. Một CPU-RAM
duration regression từng trượt ở assertion `rounds_completed > 1` khi chạy song
song với tác vụ khác; rerun toàn bộ 17 system integration tests tuần tự PASS.

Generic image đã build: `dist/dit-01-noble-amd64.img`, image ID
`DIT-01-NOBLE-20260919-d35716307fb1`, SHA-256
`10b2cd38f1fd3a77cb07f036fdeec90b0a09795f17ba765b1b02a586e51f8b4f`.
Image generic trước thay đổi được giữ tại `dist/u5u6-image-backup-rYZC0Q/`;
NVIDIA image không thay đổi. `make test-power-image` PASS, log
`build/power-qemu-MLNkbY/serial.log`: TCG 1 vCPU, minimum 1 s, đủ 6 transition,
470 verified chunks/bursts được tổng hợp, final elapsed 3664 ms, verdict PASS,
availability COMPLETED, power NO_EVIDENCE. Image SHA check PASS; release CLI hash
khớp payload rootfs. Smoke dùng snapshot disk, direct kernel/initrd lấy từ chính
image; không thay đổi image gốc, không phải BIOS/UEFI validation hay hardware
power effectiveness. U4 opcode evidence và phần recovery/IMC còn lại của 3C
vẫn mở, không đánh dấu toàn bộ 3C đã hoàn tất.

### 2026-09-19 — 3C-U campaign chung và portable heavy FP

Source đã nối một campaign SINGLE_THREAD → CORE_SMT → ALL_CPU → MIXED, mỗi
nhóm/role có mixed-oracle-v1 và heavy-fp-v1. Minimum duration + full coverage
round dùng monotonic clock; missing topology/worker/affinity/burst không PASS.
Plan v2, power report schema 2 (đọc lại schema 1), active worker counts, safety,
per-worker actual burst/work count và recovery context đã có.

Menu START chung, D chọn preset v1 2/10/30 phút hoặc custom; default 10 phút.
Hai diagnostic cũ chuyển vào Advanced, CLI count semantics không đổi. Dashboard
giữ hardware/usage, tách minimum time và round coverage, hiện group CPU/kernel.

Heavy kernel có 8 chuỗi vector độc lập, mỗi accumulator/lane so với scalar oracle;
chunk tối đa 4096 iteration, kiểm từng chunk và scratch tail. Đây là portable
workload, không khẳng định opcode FMA/AVX hoặc công suất cực đại.

Software evidence: CLI parse/conflict tests; planner cpuset/topology/role tests;
simulated duration boundary; native bounded campaign/recovery; fault injection
mismatch, heavy mismatch, zero-work, missing participant, affinity failure,
worker panic và completion timeout. Heavy oracle tests kiểm cả 8 × 4 lane và
transient mismatch; CPU-RAM regression được chạy cùng suite. UI mock/render
kiểm wide/compact, finishing-round và verdict qua watchdog.

Kiểm tra: 52 system unit tests + 17 system integration tests + 4 CLI tests PASS;
heavy scalar-oracle test ở release profile PASS; Clippy strict PASS; UI/boot-policy
và 5 unified-project tests PASS. Host smoke 2 worker, minimum 1 s, idle/burst 1 ms,
FMA depth 10: PASS, 18 transition đủ một vòng, mixed + heavy đều có evidence,
elapsed 1280 ms. Đây là bounded software smoke, không hardware effectiveness.
Smoke SIGTERM giữa `BURST_ARMED` rồi recovery schema 2 giữ campaign context,
trả `INTERRUPTED` + `INCOMPLETE`, power `NO_EVIDENCE`; không ghi đè report gốc.

Toàn bộ 3C-U vẫn `in_progress`: variable timing/replay và auto watchdog deadline
(U5), persistent worker pool, storage/checkpoint optimization và image integration
(U6), opcode evidence còn mở. Hiện cap 4096 step/vòng, 10000 transition/campaign;
cap thiếu duration/coverage trả INCOMPLETE. Chưa rebuild image hoặc hardware
validation; các cấu hình diagnostic trước đây vẫn được giữ.

### 2026-09-18 — UI launcher riêng cho availability/power-platform

Control Center có mục 4 `CPU AVAIL / POWER`, dùng UI console CPU-RAM với profile
Power Transitions / Core-SMT Sweep, phím N chọn cycles và T chọn safety nhiệt.
Không autostart. Launcher dùng watchdog và heartbeat riêng do engine ghi;
result writer tái sử dụng xác thực boot/result partition trong boot-runner.
Màn hình hiện completed/target transitions, bursts, failures, seed, CPU/RAM usage,
sensor và availability/power riêng. `INCOMPLETE` không bị watchdog exit code 2
làm hiển thị thành lỗi integrity. Rust, shell UI/render/launcher và boot-policy
gates PASS; source/image manifest đã cập nhật, chưa rebuild image trong lượt này.

### 2026-09-18 — Bước 3C: module availability/power-platform riêng

Đã triển khai CLI `power-platform-run` / `power-platform-recover`, synchronized
worker release, minimum transition/burst/affinity/start-spread gates, per-core/SMT
sweep, telemetry trước/sau và durable checkpoint. System schema v10, standalone
compute schema v4 và watchdog schema v2 ghi evidence execution riêng với power
attribution. Compute minimum-work và duration ramp đã nối vào system-run.

Fault tests bao gồm mismatch, zero-work, missing participant, worker panic,
recovery checkpoint chưa đóng và từ chối recovery process còn sống. Bounded CLI
smoke hai worker/core-sweep đã chạy đủ sáu transition, PASS; SIGKILL process của
campaign rồi recovery cho `INTERRUPTED`, `INCOMPLETE`, `NO_EVIDENCE` về nguồn.
Rust workspace regression và targeted CPU/CLI/watchdog tests đã PASS; seed
known-answer validator PASS. Không có power-cycle/hardware Vcore validation.

Bước 3C tổng thể vẫn `in_progress`: chi tiết phạm vi đã có/còn mở ở
[AVAILABILITY_POWER_PLATFORM.md](AVAILABILITY_POWER_PLATFORM.md). Không dùng
software smoke để nâng thành `validated`.

### 2026-08-28 — Bước 1: Reference pipeline v1

Đã hoàn thành:

- Cargo workspace gồm `dit-core` và `dit-cli`;
- source manifest/SHA-256 verification;
- deterministic source expansion, variable blocks và tail handling;
- scalar GF(256) 4×4 encode/decode;
- deterministic scatter, exact readback, recover/join và final digest;
- Python standard-library implementation tạo golden vector độc lập;
- JSON coverage/checkpoint/mismatch report;
- fault injection ở CPU encode, RAM readback và recovered/join;
- corrupted-source rejection.

Evidence chạy:

```text
cargo fmt --all -- --check                         PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                             PASS (9 tests)
cargo build --release --workspace                  PASS
target/release/dit-cli run                         PASS
readback/recovered fault CLI                       FAIL, exit code 2 (expected)
independent golden regeneration                    byte-for-byte reproducible
```

Reference profile đã generate 1,048,579 payload bytes, exact-verify 1,048,580
encoded bytes (một padding byte ở tail), recover/join lại đủ 1,048,579 byte và
khớp cả năm checkpoint `GENERATE`, `MATRIX_ENCODE`, `READBACK`, `RECOVER_JOIN`,
`FINAL`.

### 2026-08-28 — Bước 2: Adaptive RAM engine v1

Đã hoàn thành:

- crate `dit-system` inventory `/proc`, sysfs, cpuset, cgroup và NUMA;
- target theo MiB hoặc phần trăm effective available RAM;
- safety reserve và từ chối target vượt safe limit trước khi load;
- progressive anonymous `mmap` arenas, giảm arena khi allocation thất bại;
- first-touch/prefault, `mlock` off/best-effort/required và high-water retention;
- pattern từ source/seed/global offset, exact verify mọi byte và tail lẻ;
- unique allocated/written/read/verified coverage tách khỏi total traffic;
- first RAM mismatch có offset, expected, actual, XOR, writer/verifier CPU;
- fault tại global RAM offset tạo `FAIL`, exit code 2.

### 2026-08-28 — Bước 3: Multi-core/SIMD và CPU coverage v1

Đã hoàn thành:

- topology-aware worker selection và Linux CPU affinity;
- writer/verifier rotation sang logical CPU khác;
- scalar/safe-SIMD differential reduction;
- integer, SHA-256 và FP/FMA known-answer tests;
- atomic `SeqCst` coherency workload và SHA-256 round evidence;
- SIMD fault theo worker bị scalar oracle phát hiện và không thể PASS.

Quality gate của workspace sau Bước 2–3:

```text
cargo fmt --all -- --check                            PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                                PASS (22 tests)
cargo build --release --workspace                     PASS
```

Device run chính: AMD Ryzen 7 3700X, 8 core/16 thread, 8,200,310,784 byte RAM,
một NUMA node; 512 MiB trong 16 arena được lock, ghi và exact-verify đủ qua 4
vòng. Tổng traffic là 2 GiB write + 2 GiB read; mọi affinity, scalar/SIMD, integer,
SHA-256, FP/FMA và coherency oracle PASS. Chi tiết tại
[DEVICE_TEST_2026-08-28.md](DEVICE_TEST_2026-08-28.md).

### 2026-08-29 — Bước 4: Telemetry, safety và diagnostic v1

Đã hoàn thành:

- sysfs telemetry cho hwmon/thermal, EDAC và CPU throttle counter;
- cgroup OOM delta, machine-check configuration và kernel journal event summary;
- snapshot trước/sau, peak nhiệt và sampling monitor chạy song song;
- policy `CPU-DIT-01-SAFETY-v1`: deadline, thermal, EDAC, throttle, OOM và
  required-telemetry gates;
- cooperative safety stop được worker kiểm tra trong vòng ghi/đọc RAM;
- writer/verify panic capture, worker crash và execution abort evidence;
- JSON schema v2 có source hash, seed, config, telemetry, safety và execution;
- diagnostic rerun giữ seed/target/fault, so fingerprint và chỉ phân loại route;
- CLI fault injection cho timeout, thermal, EDAC UE và worker crash.

Quality gate:

```text
cargo fmt --all -- --check                            PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                                PASS (35 tests)
cargo build --release --workspace                     PASS
```

Main device run 512 MiB/16 worker/4 vòng PASS, 34 telemetry samples, peak sensor
67.25°C, không có safety violation. RAM mismatch được diagnostic rerun tái hiện
đúng cùng offset/fingerprint. Thermal, EDAC UE, worker crash và deadline đều
không thể PASS. Chi tiết tại [DEVICE_TEST_2026-08-29.md](DEVICE_TEST_2026-08-29.md).

### 2026-08-29 — Bước 5: Ubuntu Noble minimal live image v1

Đã hoàn thành:

- raw GPT image ghi trực tiếp bằng Rufus/DD, boot legacy BIOS và x86_64 UEFI;
- Ubuntu Noble minbase, HWE kernel, `live-boot`, root SquashFS read-only với
  runtime overlay trong RAM;
- profile Safe chạy ít nhất 2 phút; Full chạy ít nhất 30 phút và dùng 95% safe
  allocatable RAM sau reserve 512 MiB;
- swap bị tắt, service định kỳ/getty gây nhiễu bị mask;
- preflight CPU/RAM/block-device và toàn bộ JSON/log được ghi vào `DIT-RESULTS`;
- runner chỉ chấp nhận result partition cùng parent block device với live medium
  và có Image ID trùng; không fallback sang ổ nội bộ;
- hard process watchdog dùng `timeout` bao ngoài deadline cooperative;
- build tạo image, SHA-256 và manifest có Image ID/version.
- UI text hiển thị CPU/RAM/nhiệt độ/Watt và cho chọn Safe, Full, Sensor Info,
  diagnostic shell, reboot hoặc poweroff;
- CPU package power lấy từ hwmon hoặc chênh lệch energy counter powercap, không
  có nguồn hợp lệ thì ghi `N/A`;
- image chứa Ubuntu HWE modules/firmware cùng `lm-sensors`, `i2c-tools`,
  `rasdaemon`, `smartmontools`, `nvme-cli`, `dmidecode` và `ethtool`;
- driver probe chỉ nạp module sensor/power/EDAC/IPMI tự nhận dạng phần cứng, không
  force-bind driver.
- `make rufus-image` chạy quality gate, build raw image, tạo manifest/SHA-256 và
  kiểm lại artifact trước khi giao cho Rufus/DD.
- live UI hiển thị elapsed/minimum, progress, write/verify phase, round, RAM target
  và sensor; phím `D` thay đổi minimum duration của Full;
- dashboard khi test chia ba khung hardware/live utilization/process, hiển thị
  CPU/RAM bars, DIT target, temperature-limit state và fallback compact cho console nhỏ;
- system route dùng Linux-random session seed, SHA-256 round-seed rotation,
  commitment, replay `--seed` và validator schema v7;
- `make run-image` mở release image tương tác trong QEMU snapshot trên máy dev.

Artifact đã kiểm thử:

```text
image_id  CPU-DIT-01-NOBLE-20260831-f98bc04dccb8
size      2,556,428,288 bytes
sha256    d2085baf7754d97c93535ecc9139d032954e0a2b42034908ff987b2776b5758d
BIOS      UI auto-Safe; reference PASS; 256 MiB system PASS; overall_exit=0
UEFI      UI auto-Safe; reference PASS; 256 MiB system PASS; overall_exit=0
```

Chi tiết tại [BOOT_IMAGE_V1.md](BOOT_IMAGE_V1.md) và
[BOOT_IMAGE_TEST_2026-08-29.md](BOOT_IMAGE_TEST_2026-08-29.md). Backend sensor và
UI còn được kiểm trực tiếp trên thiết bị; xem
[DEVICE_TEST_2026-08-29_UI_SENSOR.md](DEVICE_TEST_2026-08-29_UI_SENSOR.md).

### 2026-08-29 — Bước 3A: Pattern/Window Coverage v1

Đã triển khai và qua automated/bounded device gate:

- phân biệt arena allocation với logical test window;
- khóa size list 4,093 B đến 32 MiB và tám pattern families;
- quy định mỗi release complete round phải đủ mandatory recipe;
- duration dài hơn thêm complete seed/round, không map cứng seed theo phút;
- deterministic planner không gap/overlap, seed-dependent placement/pattern;
- scalar expected generator, exact window fill/verify, tail handling;
- report schema v7 có plan digest và per-size/per-pattern evidence;
- UI refresh 0,5 giây hiển thị recipe/window/pattern và meter có màu;
- 51 Rust tests, Clippy, UI render và seed-validator đều PASS;
- device bounded 64 MiB/two arena/two random seed PASS; mỗi round đủ 12 size
  class, 8 pattern family và verified sum đúng target.

Xem [PATTERN_WINDOW_COVERAGE_PLAN.md](PATTERN_WINDOW_COVERAGE_PLAN.md) và
[DEVICE_STEP3A_TEST_2026-08-29.md](DEVICE_STEP3A_TEST_2026-08-29.md).

### 2026-08-31 — Temperature Policy v2 và false-positive AUXTIN

Năm phiên USB trên Intel i9-14900KF cho thấy bốn lần abort tức thời do
`nct6798:AUXTIN2/AUXTIN4` nhảy tới sentinel 127°C; không có RAM mismatch, EDAC,
OOM hay worker error. Đã sửa:

- temperature limit thành operator toggle, mặc định OFF; telemetry luôn bật;
- khi ON chỉ Intel coretemp/x86_pkg_temp, AMD k10temp/k8temp/zenpower và CPU/SoC
  thermal zone được tham gia safety gate;
- AUXTIN/Super-I/O generic vẫn lưu raw evidence nhưng không auto-abort;
- thermal abort không tự diagnostic rerun;
- report schema v7 và Safety Policy `CPU-DIT-01-SAFETY-v2` công bố enable state;
- CLI synthetic 100°C: OFF PASS; ON FAIL và `skipped_temperature_safety_abort`;
- 51 Rust tests cùng UI OFF/ON render PASS.

Chi tiết tại [TEMPERATURE_POLICY_V2.md](TEMPERATURE_POLICY_V2.md). Đây là sửa
false positive phần mềm; một peak CPU thật vẫn phải được đánh giá bằng trusted
sensor và cooling evidence.

### 2026-08-31 — UI in-place refresh v4.1

Video USB thật cho thấy console chớp theo chu kỳ vì dashboard gửi `ESC[2J` ở mọi
frame. Đã đổi sang clear đúng một lần khi bắt đầu/đổi geometry, sau đó cập nhật
theo tọa độ cursor. Nhịp UI giữ ở 2 FPS để không tranh CPU với DIT/QEMU; sensor vẫn sample khoảng
mỗi giây. Regression hai frame xác nhận chỉ có một full-screen clear.

### 2026-08-31 — Host-only development gate

Thêm `make host-check` cho vòng lặp phát triển nhẹ: Rust quality/tests, UI
two-frame regression, seed validator, sáu negative gates, reference route và
CPU/RAM positive smoke 128 MiB. Target không build image, không khởi động QEMU
và không ghi USB. Việc đóng image do người vận hành chủ động thực hiện sau khi
host gate PASS; boot USB thật vẫn là validation riêng.

### 2026-08-31 — Ryzen 9 7950X3D physical-boot compatibility

Ảnh boot USB cho thấy kernel/live root đã nạp nhưng từng thông báo systemd bị
trễ gần 30 giây và `cpu-dit.service` chưa khởi động. Hai nguồn chờ đã được loại
bỏ khỏi đường boot vật lý: kernel không còn bị ép xuất sang `ttyS0`, và service
không còn phụ thuộc `systemd-udev-settle.service`. Serial runner chuyển sang
opt-in/auto-VM; từng `modprobe` bị giới hạn 5 giây và `udevadm settle` bị giới
hạn 5 giây. Thêm static boot-policy regression vào `make host-check`. Bản sửa
đang ở trạng thái host-tested; cần rebuild image và boot lại 7950X3D để xác nhận.

### 2026-08-31 — UI startup geometry regression

Sửa lỗi `term_rows: unbound variable` khi menu đầu tiên gọi in-place renderer
trước lần đọc geometry. UI nay có geometry mặc định an toàn, menu refresh geometry
trước khi vẽ, và compact running view cũng cập nhật tại chỗ thay vì full clear.
Regression mới chạy menu với stdin không phải TTY và kiểm tra cả wide/compact hai
frame chỉ phát một `ESC[2J`.

`tools/run_host_smoke.sh` cũng chạy trực tiếp UI startup regression và lưu output
tại `build/host-smoke/ui-startup.ansi`, nên gọi riêng script này vẫn kiểm được lỗi
geometry/unbound-variable mà không cần đi qua `make host-check`.

### 2026-08-31 — Driver probe timeout console noise

Sửa thông báo Bash `Killed timeout ... modprobe` trên USB thật. Wrapper nay dùng
`timeout --foreground`, nên TERM/KILL chỉ áp dụng cho `modprobe` con; tên module,
exit code và trạng thái timeout được ghi vào `/run/cpu-dit-driver-probe.txt` thay
vì tràn ra console. Timeout module vẫn là telemetry `unavailable`, không được
diễn giải thành CPU/RAM FAIL.

### 2026-08-31 — Safe interactive USB burner

Thêm `make burn-usb` và `tools/burn_usb.sh`: chỉ nhận whole-disk transport USB,
chặn backing device của host/project/image, kiểm size + source SHA-256, yêu cầu
confirmation chứa đúng device, kiểm lại USB identity, unmount, raw write và đọc
ngược SHA-256. Có `--list`/`--dry-run`; không có auto-select hoặc skip verify.

### 2026-08-31 — Bước 3B: Compute-oracle độc lập (ADR-0004 Phase 1)

Thêm module `dit-system::compute_oracle` chạy tách biệt khỏi route RAM hiện có
(`system-run`), theo Phase 1 của
[ADR-0004](decisions/0004-cpu-power-virus-coverage-review.md). Ba kernel sustained,
mỗi cái có oracle độc lập:

- dense FMA/AVX qua `wide::f64x4::mul_add` (dùng `vfmadd` phần cứng khi build có
  target feature `fma`), oracle chính là so bit-exact kết quả giữa mọi worker
  pinned-CPU cùng seed (silicon lỗi trên một core sẽ lộ ngay), cộng oracle phụ
  so sánh tolerance với scalar reference `f64::mul_add`;
- branch predictor kernel nguyên số nhiều nhánh xác định, chuyển thể ý tưởng từ
  `test_branch_predictor.py` của app Python cũ nhưng chạy sustained thật trên
  worker pinned-CPU (không qua GIL) và so bit-exact với reference tính trước;
- AES-256 known-answer test theo vector FIPS-197, dùng crate `aes` (tự nhận
  AES-NI trên `x86_64`).

Có pha `idle_ramp` tuỳ chọn (ngủ ngắn rồi burst tải ngay, lặp lại) để tăng số
lần chuyển P-state/voltage trong một phiên, chuyển thể từ ý tưởng "Power State
Transition Test" trong roadmap app Python cũ.

CLI mới: `dit-cli compute-oracle-run` (`--workers`, `--minimum-duration-seconds`,
`--dense-fma-iterations`, `--idle-ramp`, `--seed`, `--inject-worker-fault`,
`--report`). Report JSON schema riêng (`schema_version: 1`), không đụng tới
schema v7/v8 của `system-run`.

Evidence:

```text
cargo fmt --all -- --check                            PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                                46/47 PASS*
cargo build --release --workspace                     PASS
dit-cli compute-oracle-run (4 worker, idle-ramp)       PASS, exit code 0
dit-cli compute-oracle-run --inject-worker-fault 1     FAIL, exit code 2 (expected)
```

7 test mới cho `compute_oracle` đều PASS, gồm `injected_worker_fault_cannot_pass`
xác nhận cross-worker oracle không thể bị qua mặt. `simd_worker_fault_cannot_pass_scalar_oracle`
trong `system_pipeline.rs` (route RAM hiện có, không liên quan compute-oracle)
từng FAIL trên máy 16 logical CPU vì fault injection áp dụng theo từng arena
thay vì theo từng worker trong round 0 — đã sửa cùng ngày, xem mục bên dưới.

Đây là Phase 1 (standalone). Phase 2 được triển khai ngay sau, xem mục kế tiếp.

### 2026-08-31 — Sửa fault injection SIMD áp dụng nhiều lần mỗi worker

`fault_injection_simd_worker` trong `verify_worker` (`engine.rs`) áp dụng XOR lỗi
cho **mọi** arena mà worker đó xử lý ở round 0, thay vì đúng một lần cho cả
worker — trên máy có nhiều arena/worker, số mismatch ghi nhận vượt quá 1, làm
test `simd_worker_fault_cannot_pass_scalar_oracle` FAIL (kỳ vọng đúng 1 mismatch).
Thêm cờ `simd_fault_injected` để chỉ tiêm lỗi vào arena đầu tiên của worker đó;
hành vi FAIL/exit code không đổi, chỉ số lượng mismatch ghi nhận đúng theo thiết
kế "một fault, một mismatch". `cargo test --workspace` sau đó 47/47 PASS.

### 2026-08-31 — Bước 3B: Compute-oracle chạy đồng thời với RAM DIT (ADR-0004 Phase 2)

Tích hợp compute-oracle vào `system-run`: khi `--compute-oracle-workers > 0`,
engine chọn CPU dư từ cpuset (không trùng CPU của RAM worker), spawn worker
compute-oracle **trước** vòng lặp round và chỉ `join` **sau khi** vòng lặp kết
thúc — tạo overlap wall-clock thật giữa RAM write/verify và dense
FMA/branch-predictor/AES-256, giống tải hỗn hợp thực tế. Compute-oracle worker
dùng chung cooperative stop token (`RunControl`) nên thermal/EDAC/deadline abort
dừng được cả hai nhóm cùng lúc.

Report schema bump lên `schema_version: 9`, thêm `SystemComputeOracleEvidence`
(`enabled`, `workers_used`, `worker_cpu_ids`, `bursts_completed`,
`idle_ramp_cycles`, bốn cờ PASS theo kernel, `mismatch` riêng) — tách biệt hoàn
toàn khỏi `SystemMismatch` của RAM, đúng nguyên tắc "một test, hai oracle tách
biệt" đã chốt. Verdict tổng `passed` là AND của cả hai; `failure_reason` phân
biệt rõ `compute-oracle ... mismatch` với `exact memory mismatch`.

CLI mới trên `system-run`: `--compute-oracle-workers`,
`--compute-oracle-dense-fma-iterations`, `--compute-oracle-idle-ramp`,
`--compute-oracle-idle-sleep-ms`, `--inject-compute-oracle-worker`.

Evidence:

```text
cargo fmt --all -- --check                            PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                                49/49 PASS
cargo build --release --workspace                     PASS
python3 tools/test_seed_validator.py                  PASS (schema v9)
```

Device run thật (16 logical CPU, 256 MiB, 2 round):

```text
RAM worker CPU              [0, 1]
compute-oracle worker CPU   [2, 3, 4, 5]  (không trùng RAM worker)
status                      PASS, schema_version=9
--inject-compute-oracle-worker 1
  -> FAIL, exit code 2
  -> reason: compute-oracle dense_fma_cross_worker mismatch: worker 1 dense-FMA
     result diverged from worker 0
  -> RAM exact_verified_unique_bytes vẫn đạt đủ target
```

Kết quả tiêm lỗi đúng như thiết kế: RAM sạch nhưng compute sai vẫn tạo FAIL,
và report phân biệt rõ đây là lỗi compute-oracle chứ không phải lỗi RAM.

### 2026-09-01 — `toram` và phát hiện DIT-RESULTS độc lập với `/run/live/medium`

Sự cố thật trên máy Intel i9-14900K: máy bị treo giữa phiên test, phải cắt
nguồn cứng, sau đó firmware báo `GPT header corruption` trên USB boot. Điều tra
cho thấy nguyên nhân không phải do phần ghi report của `dit-cli`/`boot-runner.sh`
(chỉ ghi file thường trong ranh giới `DIT-RESULTS`, không đụng cấu trúc GPT), mà
là rủi ro chung của flash USB tiêu dùng khi mất điện đột ngột giữa chừng. Vấn đề
liên quan được người vận hành nêu thêm: root filesystem đọc trực tiếp từ USB
trong suốt phiên nghĩa là rút USB giữa chừng có thể làm crash tiến trình đang
chạy.

Đã thêm `toram` vào mọi kernel command line trong
[image/boot/grub/grub.cfg](../image/boot/grub/grub.cfg): `live-boot` copy toàn
bộ `DIT-BOOT` (kernel, initrd, SquashFS) vào tmpfs trước khi chạy test, nên toàn
bộ OS/`dit-cli` chạy từ RAM trong suốt phần CPU/RAM stress, không phụ thuộc USB
còn cắm hay không.

`toram` khiến `live-boot` tự `mount --move` một tmpfs đè lên mount point
`/run/live/medium`, nên logic phát hiện `DIT-RESULTS` cũ (dựa vào
`findmnt SOURCE /run/live/medium` để suy ra parent block device) không còn hoạt
động — đã xác nhận bằng cách đọc trực tiếp
`lib/live/boot/9990-toram-todisk.sh` trong rootfs cache. Thay bằng cách quét mọi
partition có label `DIT-BOOT`, mount read-only tạm thời và so
`cpu-dit/image-id` với `/etc/cpu-dit/image-id` của chính bản build đang chạy để
xác định đúng parent block device — độc lập hoàn toàn với hành vi mount-move của
`live-boot`, không cần vá package bên thứ ba.

Evidence (QEMU trực tiếp bằng `-kernel`/`-initrd`/`-append cpu_dit_profile=safe`,
bỏ qua UI để lấy log console đầy đủ):

```text
Begin: Copying live media to ram ...  * Copying whole medium to RAM
run-cpu\x2ddit\x2dboot\x2dcheck.mount: Deactivated successfully.
Result target: verified USB sibling partition
Result device: /dev/sda3
Running reference/golden route... PASS
```

Phát hiện phụ: `toram` copy thêm ~900 MiB vào RAM ngay từ đầu boot, nên
`tools/test_boot_image_qemu.sh` với `QEMU_MEMORY_MIB=2048` cũ không còn đủ
`effective available` cho Safe profile (256 MiB + 512 MiB reserve); đã bump
default lên 3072 MiB. Trên máy test thật (≥16 GiB RAM) mức tiêu tốn thêm này
không đáng kể và không cần chỉnh gì.

`bash tools/test_boot_policy.sh` vẫn PASS sau thay đổi; `make qemu-smoke` (BIOS +
UEFI, 3072 MiB) PASS cả hai firmware mode với image mới
(`CPU-DIT-01-NOBLE-20260901-6af9576d97f6`,
`7efb1d690a595e88ee7581d7b53ffc14c8a759cd4c086ea10d42f0f9246d84e4`).

### 2026-09-01 — Console bị "scratch" khi rút USB giữa chừng

Thiết bị thật cho thấy sau khi rút USB, phiên test vẫn chạy tiếp đúng như thiết
kế `toram` (FAIL TOTAL vẫn 0, progress vẫn tăng), nhưng màn hình bị "scratch":
kernel in liên tục `device offline error`/`Buffer I/O error ... lost async page
write` đè lên dashboard đang vẽ bằng cursor-addressed update.

Đã tái hiện chính xác bằng QEMU: boot `system-run`, dùng QEMU monitor
`device_del` để rút ổ đĩa ảo giữa lúc `RAM_WRITE`, rồi so log console. Hai phát
hiện:

- Thêm `loglevel=3` vào kernel command line **không** ngăn được các dòng
  `device offline error`/`Buffer I/O error` — đã thử lại với `loglevel=1` (chỉ
  cho EMERG qua) và log console vẫn giống hệt, chứng minh các message lỗi ổ đĩa
  này cố tình vượt qua bộ lọc `console_loglevel` (hành vi an toàn có chủ đích
  của kernel cho lỗi storage nghiêm trọng, không phải bug). `loglevel=3` vẫn giữ
  lại vì làm giảm được nhiễu boot thông thường khác (đã xác nhận các dòng
  `Command line:`/INFO-level không còn in ra), nhưng không phải fix cho vấn đề
  "scratch" cụ thể này.
- Fix thật: [cpu-dit-ui.sh](../image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh)
  thêm `RUNNING_SELF_HEAL_FRAMES=40` — cứ mỗi 40 frame (~20 giây ở refresh mặc
  định 0,5 giây) dashboard tự ép một lần full-screen clear+redraw thay vì chỉ
  cursor-addressed update, để tự phục hồi khỏi bất kỳ tiến trình bên ngoài nào
  ghi thẳng vào console (kernel messages, v.v.) trong vòng tối đa ~20 giây thay
  vì bị "scratch" vĩnh viễn suốt phần còn lại của phiên.

`make test-ui` (two-frame regression) vẫn PASS vì chỉ kiểm 2 frame đầu, chưa
chạm ngưỡng self-heal 40 frame. `make qemu-smoke` (BIOS + UEFI) PASS lại với
image mới sau thay đổi này (`CPU-DIT-01-NOBLE-20260901-6af9576d97f6`,
`7efb1d690a595e88ee7581d7b53ffc14c8a759cd4c086ea10d42f0f9246d84e4`).

### 2026-09-01 — Progress-report heartbeat cho trường hợp dump/đứng máy

Trước đây, `system-report.json` chỉ được ghi một lần sau khi `run_system()`
trả về; nếu tiến trình bị dump (panic không bắt được, OOM-kill) hoặc máy đứng
(kernel/firmware hang), watchdog `timeout` trong `boot-runner.sh` cuối cùng sẽ
kill tiến trình nhưng không có report nào được sinh ra — hoàn toàn mất bằng
chứng đang chạy ở giai đoạn nào.

Đã thêm `--progress-report <path>` cho `dit-cli system-run`: `engine.rs` giờ
ghi đè một file JSON heartbeat (`SystemProgressReport`) tại mỗi lần chuyển pha
(`PREFLIGHT` → `ALLOCATED` → `RAM_WRITE` → `RAM_VERIFY` → `ROUND_COMPLETE` →
`FINALIZING`), gồm phase/round/elapsed_ms/calculations_completed/failures_total
và seed. Ghi bằng write-tmp-then-rename cùng thư mục để không bao giờ để lại
file half-written; mọi lỗi I/O khi ghi heartbeat đều bị nuốt có chủ đích (best
effort — không được phép làm hỏng bài test thật). `boot-runner.sh` truyền
`--progress-report "$RUN_DIR/system-progress.json"` và giờ ghi thêm cờ
`system_report_written` vào `summary.txt` để biết ngay report cuối có được
sinh ra hay không.

Đã xác minh bằng SIGKILL thủ công giữa chừng `RAM_WRITE`: `system-report.json`
không tồn tại (như trước), nhưng `system-progress.json` vẫn còn và phản ánh
đúng `phase=RAM_WRITE round=1` tại thời điểm bị kill. Nếu USB bị rút vật lý,
ghi heartbeat cũng thất bại âm thầm như mọi write khác vào `$RUN_DIR` — không
có gì để lưu khi không còn phương tiện lưu trữ, đúng như kỳ vọng.

`cargo test --workspace` (55 tests, thêm
`progress_report_heartbeat_reflects_last_phase`), `cargo clippy --workspace
--all-targets -- -D warnings`, `bash tools/test_boot_policy.sh` đều PASS.

### 2026-09-01 — Seed rotation: RAM route đã đảo mỗi round; compute-oracle chỉ
### kiểm burst cuối cùng (đã sửa)

Câu hỏi: "seed có đảo theo thời gian không, vì cố định 1 seed cho full test
không khả thi". Kiểm tra lại toàn bộ:

- RAM route (`system-run`): `round_seed = derive_round_seed(session_seed,
  test_id, round)` đã đảo (SHA-256, domain-separated) ở **mỗi round**, và
  `round_seed` này quyết định trực tiếp cả window plan lẫn nội dung byte
  (`expected_byte`/`expected_word`). Với arena nhỏ, một round Full-profile có
  thể lặp lại hàng trăm/nghìn lần trong 30–60 phút — seed hiệu dụng đã đảo
  liên tục, không phải one-shot. Không cần sửa gì ở route này.
- Compute-oracle (ADR-0004): phát hiện một khoảng trống thật — mỗi burst gọi
  lại `dense_fma_pass(config.seed, iterations)` với `seed` **cố định suốt cả
  phiên**, nhưng quan trọng hơn: so sánh scalar-reference (đường tính độc lập,
  không vector hoá) trước đây **chỉ áp dụng cho burst CUỐI CÙNG** của cả
  session (qua `aggregate_outcomes` so `outcomes[0].dense_fma_final_bits` với
  scalar reference một lần duy nhất). Một corruption thoáng qua ở burst giữa
  phiên (vd. burst thứ 500 trong tổng số 5000) sẽ không được phát hiện nếu
  burst cuối cùng tính đúng — gần như toàn bộ compute trong phiên không được
  kiểm chứng.

Đã sửa trong `compute_oracle.rs`: tính scalar reference một lần trước khi
worker bắt đầu (hàm thuần tuý của `(seed, iterations)`, không cần tính lại mỗi
burst), rồi so sánh **mỗi burst** với `dense_fma_within_tolerance(...)`, tăng
`dense_fma_mismatches` mỗi khi lệch — y hệt cách `branch_predictor_mismatches`
và `aes256_mismatches` đã làm từ trước. `aggregate_outcomes` giờ cộng dồn
mismatch qua tất cả burst của tất cả worker thay vì chỉ nhìn burst cuối; tên
field JSON (`dense_fma_scalar_reference_passed`) giữ nguyên nên không cần bump
schema của `SystemRunReport`. `ComputeOracleWorkerEvidence` có thêm field
`dense_fma_mismatches`; `ComputeOracleReport.schema_version` bump 1 → 2 cho
thay đổi cấu trúc này (chỉ ảnh hưởng report độc lập của `compute-oracle-run`,
không ảnh hưởng `system-report.json` schema v9).

Không đảo `seed` mỗi burst của compute-oracle: nếu đảo, cross-worker
consistency check (so kết quả burst cuối giữa các worker) sẽ so sánh các
worker đang ở burst-index khác nhau (do lệch lịch chạy thread) → false
positive liên tục. Giữ seed cố định + kiểm tra mọi burst độc lập là thiết kế
đúng cho oracle này (tương tự Prime95/OCCT lặp lại một phép tính đã biết đáp
án đúng để bắt lỗi ngắt quãng do nhiệt/điện áp).

Thêm test `run_burst_flags_a_mismatch_on_every_burst_not_only_the_last` và 2
test cho `dense_fma_within_tolerance`. `cargo test --workspace -- --test-threads=1`
(58 tests), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt
--check` đều PASS.

### 2026-09-01 — UI refresh rate nhanh hơn; refresh pci.ids để tên GPU bớt chung chung

Hai yêu cầu độc lập:

- `cpu-dit-ui.sh`: `UI_REFRESH_SECONDS` 0,5 → 0,25 giây và
  `SENSOR_REFRESH_TICKS` 2 → 1 (sensor Watt/nhiệt độ giờ cập nhật mỗi frame
  thay vì mỗi 2 frame) để dễ theo dõi nhiệt độ/Watt lúc test đang chạy.
  `RUNNING_SELF_HEAL_FRAMES` bump 40 → 80 để giữ nguyên chu kỳ tự phục hồi
  màn hình ~20 giây đã chốt trước đó (xem mục self-heal ở trên).
- Tên thiết bị PCI (GPU) trong `driver-probe.txt` dựa vào `pci.ids` đóng gói
  cùng `pciutils`; bản cache từ debootstrap có thể cũ hơn phần cứng vừa ra mắt
  (vd. RTX 5080), khiến `lspci -nnk` hiện `NVIDIA Corporation Device
  [10de:xxxx]` thay vì tên marketing thật. `tools/build_boot_image.sh` thêm
  hàm `refresh_pci_ids()` chạy `update-pciids` (best-effort, timeout 30s) sau
  `ensure_diagnostic_packages`, làm mới `pci.ids` tại thời điểm build — live
  image lúc boot vẫn không tự tải gì qua mạng. Nếu máy build không có mạng,
  bước này bị bỏ qua và tên vẫn có thể chung chung; mã số PCI vendor:device
  trong ngoặc vuông luôn chính xác dù tên không phân giải được.

`bash -n` cả hai script, `make test-ui`, `bash tools/test_boot_policy.sh`,
`cargo test --workspace -- --test-threads=1` đều PASS. Rebuild `make image`
lần đầu lộ một bug thật: `unmount_chroot_filesystems()` không retry khi
`/proc` báo "target is busy" sau bước `update-pciids`, làm toàn bộ build fail
(`set -e`). Đã sửa để retry 3 lần rồi fallback `umount -l` (lazy) thay vì để
cả build chết vì một race lúc teardown. `make image` (image mới
`CPU-DIT-01-NOBLE-20260901-843d7743c91a`,
`115c3afe2318285ef633827e725f33cbeb3916b72f8bc537b1a4d69916205395`) và
`make qemu-smoke` (BIOS + UEFI) đều PASS sau khi sửa.

### 2026-09-02 — `calculations_completed` tính theo byte, không phải MB hay task

Trả lời câu hỏi: hiện tại `calculations_completed` (JSON field và dòng
`CPU_DIT_PROGRESS`/`CPU_DIT_COUNTER`) đếm theo đơn vị nào?

- RAM route (`verify_arena` trong `engine.rs`): mỗi byte so khớp trong
  `RAM_VERIFY` cộng đúng 1 vào counter (`pending_calculations += 8` cho mỗi
  word 8 byte) — tức đơn vị là **byte**, không phải MB và không phải "số
  task". Cộng thêm 4 đơn vị cố định cho 4 bài CPU known-answer/coherency lúc
  `PREFLIGHT`, và 1 đơn vị cho mỗi lần so scalar/SIMD reduction mỗi arena —
  hai nguồn này không đáng kể so với số byte verify thực tế.
- Compute-oracle (ADR-0004) không cộng vào counter này; nó có
  `bursts_completed` riêng, tách biệt hoàn toàn.
- Test `adaptive_multi_core_pipeline_passes` xác nhận: target 4 MiB × 2 round
  → `calculations_completed >= 8 * 1024 * 1024` (đúng bằng số byte, không chia
  cho 1e6/2^20).

Số byte thô hiển thị trên dashboard trước đây không có đơn vị (vd.
`123456789`), dễ hiểu nhầm là MB hoặc số lượng phép tính rời rạc. Đã thêm
`format_calculations()` vào `cpu-dit-ui.sh`: giữ nguyên số byte thô rồi thêm
đơn vị human-readable tự chọn KiB/MiB/GiB, vd. `123456789 (117.73 MiB)`. JSON
schema và field name không đổi, chỉ format hiển thị trên UI thay đổi.

`make test-ui`, `bash tools/test_boot_policy.sh`, `cargo test --workspace --
--test-threads=1` đều PASS.

Xác nhận trực tiếp trên host (không qua image/QEMU): chạy thật
`dit-cli system-run --memory-mib 256 --rounds 2 --workers 2 --progress`, PASS
với `calculations_completed=536870948` (đúng 256 MiB × 2 round = 512 MiB), rồi
nạp log thật đó vào `cpu-dit-ui.sh --render-running-once` — dashboard hiện
đúng `CALCULATIONS COMPLETED  536870947 (512.00 MiB)`. `make image` đã build
xong một lần (`CPU-DIT-01-NOBLE-20260902-7d23c7a20ea0`,
`74b71da5437feeca0736fb495fa168e5b32a262e74dd788f9966cd1f2312f99c`) nhưng
`make qemu-smoke` chưa chạy — theo yêu cầu, phần build/test image tiếp theo do
người vận hành tự chạy.

## Giới hạn hiện tại

- Reference pipeline và high-memory system pipeline là hai command/report riêng.
- High-memory route đã có window-size/pattern-family rotation; fault campaign
  metadata/order/cross-boundary đầy đủ và Safe/Full 30/60 phút còn chờ validation.
- Device evidence hiện chỉ có một AMD CPU, khoảng 8 GB RAM và một NUMA node.
- Máy hiện tại không xuất EDAC hoặc CPU throttle counter; policy path mới được
  xác minh bằng injection, chưa bằng ECC hardware event thật.
- Kernel journal đọc được nhưng không có MCE/EDAC/thermal event trong main run.
- Deadline engine là cooperative; live runner đã có hard process watchdog, nhưng
  kernel/firmware hard hang vẫn cần platform watchdog hoặc power control ngoài.
- First-touch chỉ là NUMA placement inference; chưa chứng minh physical PFN/node.
- Linux chỉ kiểm `allocatable RAM`; không thể tuyên bố 100% physical RAM.
- Bài 512 MiB là functional/device evidence, không phải full-RAM soak hay hardware
  matrix validation.
- Live image mới boot-smoke bằng QEMU TCG; chưa ghi/chạy trên USB vật lý của máy
  thật và chưa kiểm Secure Boot (v1 unsigned, yêu cầu tắt Secure Boot).
- Driver coverage là bộ driver nguồn mở rộng của Ubuntu HWE cộng firmware và
  module mục tiêu; không bao hàm mọi driver độc quyền hoặc mọi mainboard sensor.
- `implemented` chưa mang nghĩa `validated` trên phần cứng khác.
- Compute-oracle (Bước 3B) mới có device evidence trên máy phát triển 16 logical
  CPU; chưa chạy trên chính Intel Raptor Lake (13th/14th Gen) để xác nhận tăng
  tỷ lệ phát hiện thật, và chưa có route AVX-512 differential riêng hay IMC
  dual-channel latency-variance evidence (chỉ mới ghi trong ADR-0004 làm đề
  xuất, chưa triển khai).

## Chuẩn bị Bước 6

Đã tạo [VALIDATION_GUIDE.md](VALIDATION_GUIDE.md), matrix, run-record template và
session-report validator. Release hiện được khóa `NOT_VALIDATED` tại
[`validation/VALIDATION_STATUS.md`](../validation/VALIDATION_STATUS.md); chỉ đổi
trạng thái sau khi có đủ physical USB, repeatability, soak, fault campaign và
reviewed evidence cho mọi dòng matrix bắt buộc.

Negative campaign runner đã PASS cả sáu gate trên máy AMD development hiện tại:
RAM offset, SIMD worker, worker crash, deadline, temperature và EDAC UE đều trả
exit code 2 như yêu cầu. Đây mới là một phần evidence Bước 6, chưa phải validation
cho cấu hình hoặc release.

### 2026-09-04 — Project identity frozen as DIT CPU–RAM

Chốt tên làm việc của CPU-DIT-01 là **DIT CPU–RAM**. Storage/NVMe destructive
testing sẽ không được ghép vào repository này; project CPU–SSD/M.2 sau này dùng
folder, docs, safety policy và validation matrix riêng.

### 2026-09-05 — Shared-image extension hook và module control center

Giữ nguyên ranh giới trên: engine/source NVMe vẫn ở project riêng
`/home/hoang/DIT-01`. Project CPU-DIT-01 chỉ thêm một extension hook
checksum-verified vào pipeline image mà nó sở hữu.

- `DIT_EXTENSION_DIR` phải có `MANIFEST.sha256` hợp lệ trước khi stage rootfs;
- Image ID/fingerprint và image manifest ghi extension manifest SHA-256;
- không truyền extension thì builder xóa chính xác các file CPU–NVMe đã stage và
  tạo flavor CPU–RAM như cũ;
- profile `menu` mở `dit-suite` khi extension có mặt, fallback nguyên UI CPU–RAM
  nếu không có;
- CPU UI có phím `B` quay lại module selector khi chạy dưới suite;
- CPU runner chỉ xuất preflight adapter vào `/run/dit-suite/*.json`, không cho
  ghi đè file và chỉ ghi PASS khi native system report thật sự PASS;
- `bash -n` và `tools/test_boot_policy.sh`: PASS.

Chưa build image hoặc chạy QEMU/USB trong thay đổi này. Artifact và host gates
được quản lý bởi `make task-f-check` ở project CPU–NVMe; người vận hành sẽ chủ
động gọi `make shared-image` sau.
