# CPU-DIT-01 Roadmap

Roadmap này sắp xếp sản phẩm theo capability và quality gate, không gắn ngày khi
chưa có dữ liệu về nhân lực, phần cứng lab và thời lượng soak. Checklist kỹ thuật
chi tiết nằm trong [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md).

Trạng thái thực tế và bằng chứng hoàn thành được ghi tại
[STATUS.md](STATUS.md). Một mục chỉ được đánh dấu hoàn thành sau khi code, test và
tài liệu tương ứng đều tồn tại; mô tả thiết kế không được tính là implementation.
`implemented` không phụ thuộc `validated`: automated/fault-injection gate đủ để
tiếp tục bước phát triển kế tiếp, còn hiệu quả trên từng CPU/mainboard và hardware
matrix phải được giữ thành evidence `validated` riêng.

## Trình tự triển khai thực tế

Đây là trình tự làm việc dành cho một người vận hành phần cứng và Codex phụ trách
code. Các bước nhỏ cố ý tạo một vertical slice chạy được trước khi làm USB image.

### Bước 1 — Reference pipeline trên Ubuntu phát triển

Trạng thái: `implemented` ngày 2026-08-28; xem bằng chứng trong
[STATUS.md](STATUS.md) và [REFERENCE_PIPELINE_V1.md](REFERENCE_PIPELINE_V1.md).

- Khởi tạo repository và hệ thống build tái lập.
- Tạo source corpus/manifest nhỏ và source verifier.
- Chạy pipeline RAM nhỏ: split → matrix → scatter → exact readback → decode → join.
- Tạo final digest, JSON report và lệnh CLI.
- Thêm fault injection cùng test reference/sanitizer.

Hoàn thành khi một lệnh build/test chạy được trên Ubuntu hiện tại và mọi fault
injection nền tảng đều làm test không PASS. Bước này không cố chiếm gần hết RAM.

### Bước 2 — Adaptive RAM engine

Trạng thái: `implemented` ngày 2026-08-28; đã có device evidence giới hạn trên
một máy 8 GB. Xem [STATUS.md](STATUS.md),
[SYSTEM_PIPELINE_V1.md](SYSTEM_PIPELINE_V1.md) và
[DEVICE_TEST_2026-08-28.md](DEVICE_TEST_2026-08-28.md).

- Thay vùng nhớ development bằng multi-arena `mmap`.
- Inventory RAM/NUMA, safety reserve, prefault/touch và allocation backoff.
- Unique coverage accounting, giữ high-water arenas và xử lý mọi tail byte.
- Tăng coverage theo nấc an toàn: development → validation → full profile.

Hoàn thành khi cùng binary chạy an toàn trên nhiều dung lượng RAM, không OOM và
không tạo coverage giả do physical-page reuse.

### Bước 3 — Multi-core, SIMD và CPU coverage

Trạng thái: `implemented` ngày 2026-08-28; multi-core/SIMD đã chạy trên 16 logical
CPU. Cross-NUMA vẫn chưa thể kiểm thử vì thiết bị hiện tại chỉ có một NUMA node.

- CPU topology/affinity và cross-core role rotation.
- Scalar reference kiểm optimized/SIMD path.
- Integer/hash/crypto/cache/coherency/IMC workload.
- FP/FMA numerical route tách khỏi bit-exact integrity.

Hoàn thành khi scheduling khác nhau không đổi expected result và fault injection
theo core/SIMD đều được oracle độc lập phát hiện.

### Bước 3A — Pattern/window coverage mở rộng

Trạng thái: `implemented` ngày 2026-08-29, chưa `validated` bằng USB/hardware
matrix; xem
[PATTERN_WINDOW_COVERAGE_PLAN.md](PATTERN_WINDOW_COVERAGE_PLAN.md).

- Mỗi complete full-RAM round chạy đủ recipe size từ 4,093 B đến 32 MiB, gồm
  aligned, off-by-one, non-power-of-two và tail.
- Round seed hoán vị window placement/pattern nhưng không được bỏ mandatory class.
- Thêm fixed/address/walking/source-expanded pattern families và independent oracle.
- Report per-size/per-pattern exact coverage; UI hiện active window/pattern.
- Duration dài hơn tạo thêm complete round/seed/combination, không dùng mapping
  cứng số seed theo phút và không trì hoãn size quan trọng đến bài dài.

Hoàn thành khi planner không gap/overlap, mọi release round đủ mandatory recipe,
fault ở boundary/metadata/order không thể PASS và device 30/60 phút có evidence.

### Bước 3B — Compute-oracle sustained cho lỗi kiểu Raptor Lake

Trạng thái: `implemented` ngày 2026-08-31; xem
[ADR-0004](decisions/0004-cpu-power-virus-coverage-review.md). Chưa `validated`
bằng hardware matrix.

- Kernel dense FMA/AVX sustained (`wide::f64x4::mul_add`), branch-predictor
  nguyên số và AES-256 known-answer, mỗi kernel có oracle độc lập (cross-worker
  bit-exact và/hoặc scalar reference), không dùng benchmark/tốc độ làm oracle.
- Chạy được độc lập (`dit-cli compute-oracle-run`) hoặc đồng thời thật với route
  RAM trong `system-run` (`--compute-oracle-workers`), pin vào CPU không trùng
  RAM worker, spawn trước và join sau vòng lặp round để overlap wall-clock thật.
- Pha `idle_ramp` tuỳ chọn luân phiên idle/burst để tăng số lần chuyển
  P-state/voltage trong một phiên.
- Report tách biệt `SystemComputeOracleEvidence` khỏi `SystemMismatch` của RAM;
  verdict tổng là AND của cả hai oracle nhưng `failure_reason`/`mismatch` không
  bao giờ quy nhầm lỗi compute-oracle thành lỗi RAM hoặc ngược lại.

Implementation gate đạt khi mỗi kernel có fault injection riêng không thể PASS
và chạy đồng thời với RAM route không làm sai lệch coverage RAM. Device Intel
Raptor Lake thật xác nhận tăng tỷ lệ phát hiện so với route RAM đơn thuần là
validation gate riêng, không chặn triển khai các bước tiếp theo.

### Bước 3C — Availability và power-platform evidence

Trạng thái: `in_progress` ngày 2026-09-19; đã có minimum-work gate, hai module
availability/power-platform riêng, menu/launcher UI riêng, CLI synchronized transitions/recovery,
per-core/SMT sweep và schema mới. Chi tiết implementation và các mục còn mở:
[AVAILABILITY_POWER_PLATFORM.md](AVAILABILITY_POWER_PLATFORM.md).
Thiết kế được rút ra từ đối chiếu implementation CPU-RAM
hiện tại với bản TestCPU gốc tại `/home/hoang/Downloads/TestCPU`. Bước này không
thay thế data-integrity/compute oracle và không tự quy một lần reset cho CPU,
Vcore, VRM hay PSU khi chưa có evidence tương ứng.

Mục tiêu là phát hiện ba lớp sự kiện độc lập:

- sai dữ liệu/tính toán trong khi máy vẫn chạy;
- workload không chạy đủ hoặc worker/test process bị lỗi;
- phiên bị đứng, reset hoặc mất nguồn trước khi có final report.

#### P0 — Khóa tính đúng của evidence

- Thêm minimum-work gate cho compute-oracle tích hợp: số worker thực dùng, burst
  mỗi worker và idle/ramp cycle phải đạt mức tối thiểu đã resolve; `0 burst` hoặc
  thiếu coverage không được PASS.
- Phân biệt `FAIL` với `INCOMPLETE`/`INVALID`: mismatch xác định là `FAIL`; worker
  không khởi động, thiếu binary/capability, thiếu burst hoặc không đạt transition
  target là `INCOMPLETE`/`INVALID`, không được diễn giải thành phần cứng PASS.
- Áp dụng `DIT-DURATION-COMPLEXITY-RAMP-v1` cho compute-oracle nằm trong
  `system-run`, thay vì chỉ route standalone; ghi giá trị FMA depth đã resolve
  vào system report để replay không phải suy ngược từ duration.
- Khi operator yêu cầu N compute worker, report phải ghi bốn số `requested`,
  `available`, `started`, `completed`; policy phải quyết định rõ thiếu worker là degrade có công
  bố hay `INCOMPLETE`, không âm thầm PASS với ít worker hơn.
- Giữ zero-tolerance: một RAM/compute mismatch xác định vẫn là `FAIL`; không mang
  ngưỡng kiểu `1–2 lỗi = WARNING` từ TestCPU sang DIT-01.

P0 implementation gate:

- fault injection chứng minh `0 burst`, thiếu worker, worker panic và mismatch
  đều không thể PASS;
- standalone và concurrent route cùng ghi resolved depth/minimum work;
- schema/validator/UI phân biệt rõ `FAIL`, `INCOMPLETE` và `INVALID`.

#### P1 — Availability model và power-transition profile

- Tách các trục kết quả `integrity_status`, `compute_status`,
  `availability_status` và `power_platform_status`; overall verdict được dẫn xuất
  nhưng không làm mất nguyên nhân gốc.
- Chuẩn hóa availability thành tối thiểu `COMPLETED`, `INTERRUPTED`, `STALLED`,
  `WORKER_FAILED` và `RESET_SUSPECTED`. Power-platform dùng mức thận trọng
  `NO_EVIDENCE`, `SUSPECTED`, `HARDWARE_EVENT_OBSERVED`; không ghi `CPU_FAULT`
  chỉ vì marker còn mở sau reboot.
- Hợp nhất evidence từ system progress, process watchdog, cgroup OOM, kernel
  journal và recovery marker thành một recovered result có run ID/test ID/phase
  cuối, thay vì chỉ hiện cảnh báo rời ở menu.
- Làm checkpoint quan trọng crash-durable bằng write-temp, flush, `sync_all`,
  rename và durability policy phù hợp cho thư mục; chỉ ghi theo phase/transition
  để tránh tăng write wear trên USB.
- Thêm power-transition profile riêng theo state machine: `QUIESCE`, `ARM`,
  `GLOBAL_BARRIER_RELEASE`, `FULL_BURST`, `VERIFY`, `SNAPSHOT`, `COOLDOWN`. Không coi
  `system-run` có RAM load nền là package-idle thật.
- Release compute/memory workers gần đồng thời bằng barrier; ghi worker-start
  spread, transition count, actual idle/burst/cooldown duration và minimum
  transition target. Thiếu bất kỳ coverage gate bắt buộc nào cho `INCOMPLETE`.
- Snapshot temperature, clock, package power, throttle, EDAC/MCE/kernel event
  quanh từng transition khi nguồn đáng tin cậy tồn tại. Vcore không khả dụng phải
  ghi `unavailable`, không suy đoán hoặc thay bằng số giả.
- Watchdog process vẫn xử lý worker stall/signal/OOM; kernel hard-lock và mất
  nguồn chỉ có thể được phục hồi gián tiếp từ durable marker. BMC/platform
  watchdog hoặc thiết bị đo ngoài là evidence bổ sung, không phải capability DIT
  tự giả định có.

P1 implementation gate:

- synchronized transition fault tests xác nhận thiếu barrier participant, timeout,
  worker crash và transition thiếu đều không thể PASS;
- kill/reboot simulation tạo recovered `INTERRUPTED` result đúng phase cuối;
- một phiên sạch đạt đủ transition có thể PASS dù sensor power/Vcore unavailable,
  nhưng availability của từng sensor phải được công bố;
- hardware reset/Vcore/VRM effectiveness vẫn thuộc validation gate riêng.

#### P2 — Chẩn đoán CPU/IMC mở rộng

- Thêm per-core/SMT isolation campaign: từng logical CPU, physical core, cặp SMT
  và all-core comparison; correctness dùng oracle xác định, throughput chỉ là
  diagnostic để tìm weak-core candidate.
- Ghi topology, affinity success, bursts, mismatch và telemetry gần nhất cho từng
  core; không quy core lỗi nếu affinity/topology evidence không đủ.
- Có thể thêm IMC latency distribution/variance, local/cross-NUMA và scheduler
  noise làm diagnostic evidence. Không dùng một ngưỡng variance chung làm
  integrity FAIL; chỉ mismatch, EDAC UE hoặc hardware event rõ ràng mới tham gia
  correctness gate.
- Công bố instruction coverage theo bốn trạng thái `supported_by_cpu`,
  `enabled_in_binary`, `executed`, `verified`; tách scalar, SIMD, AVX, AVX2,
  FMA, AES-NI và AVX-512 khi có. Tên workload hoặc compiler auto-vectorization không đủ
  để tuyên bố instruction đã được kiểm tra.
- Tương quan mismatch với transition ID và telemetry sample gần nhất, nhưng kết
  luận component vẫn giữ `unattributed` nếu evidence không đủ.

P2 implementation gate:

- per-core/SMT fault injection bị phát hiện và report đúng target;
- IMC performance anomaly không tự biến thành integrity failure;
- binary/runtime evidence chứng minh instruction path trước khi report `verified`.

#### Thứ tự phụ thuộc của Bước 3C

```text
minimum-work + status semantics (P0)
        |
durable availability/recovery evidence (P1)
        |
synchronized power-transition profile (P1)
        |
per-core/SMT + IMC/instruction diagnostics (P2)
        |
hardware/platform validation (Bước 6)
```

Các phần P0/P1/P2 được chuyển sang `implemented` ngay khi code, automated tests,
fault injection và docs tương ứng hoàn tất. Chỉ những CPU/mainboard/PSU/BMC đã
chạy campaign thật mới được chuyển sang `validated` ở Bước 6.

#### 3C-U — Hợp nhất Power Transitions và Core/SMT Sweep

Trạng thái: `in_progress` ngày 2026-09-19. Đã có source/automated evidence cho
planner, scheduler duration + complete round, UI campaign và portable heavy
kernel. U5/U6 `implemented`: source/host gates và generic image direct-kernel
QEMU smoke đã PASS, ghi nhận trong STATUS. Opcode artifact/runtime verification và hardware
validation vẫn mở. Xem [STATUS.md](STATUS.md) và
[hướng dẫn campaign](AVAILABILITY_POWER_PLATFORM.md).

Mục tiêu: **một quy trình test nhiều pha, một scheduler, một phiên report**.
"Một luồng test" không có nghĩa chỉ dùng một CPU thread. Trong mỗi transition,
chỉ worker thuộc nhóm đang chọn được release đồng bộ; worker ngoài nhóm chờ
blocking. Không chạy all-CPU load nền đồng thời với pha isolation.

Baseline trước 3C-U: kernel và transition engine đã dùng chung; hai menu khác
nhau chủ yếu ở danh sách nhóm CPU. Mặc định 10 transition/nhóm, idle 500 ms và
burst 100 ms, chưa có thời lượng tối thiểu. FMA hiện là một chuỗi accumulator
vector phụ thuộc; AES là known-answer check một block; scratch 64 KiB/worker.
Đây là nền tảng chuyển tải có oracle, chưa phải bằng chứng tải công suất cực đại.

Trình tự thực thi: **3C-U1 → U2 → U3 → U4 → U5 → U6**. Mỗi mục phải có
code, automated/fault-injection tests và docs trước khi ghi `implemented`.
Hardware validation không chặn mục kế tiếp; các backlog recovery/IMC của 3C
vẫn được giữ, không ngầm coi là hoàn tất hoặc bắt buộc hoàn tất hết trước U1.

##### 3C-U1 — Contract và planner cho một campaign

- [x] Định nghĩa plan/version gồm allowed CPU set, topology, group ID, phase,
  kernel ID/version, cycles/group, idle/burst, seed và giới hạn thời gian.
- [x] Một vòng coverage gồm từng logical CPU → nhóm SMT cùng core → toàn bộ
  CPU được chọn → mixed groups. Loại nhóm trùng nhưng giữ coverage role của pha;
  không suy SMT bằng số thứ tự CPU. Topology thiếu phải công bố degraded coverage.
- [x] Mỗi lượt chạy ít transition/nhóm rồi chuyển nhóm; không tiêu hết ngân sách
  trên một core. Plan ghi rõ nhóm bắt buộc và phase không áp dụng trên máy đó.
- [x] Giữ CLI cũ làm đường diagnostic/replay có mode rõ ràng; campaign mới dùng
  cùng transition engine, không fork một engine riêng. Version schema mới khi
  thay contract và giữ đọc report lịch sử.

Gate: planner deterministic; test SMT on/off, một CPU, nhiều socket, cpuset
không liên tiếp và topology thiếu; không chạy CPU ngoài allowed set, không tạo
coverage giả hoặc làm thay đổi ngầm ý nghĩa `--transitions` cũ.

##### 3C-U2 — Scheduler xoay vòng và thời lượng tối thiểu

- [x] Chạy các pha tuần tự trong một session/run ID; chỉ nhóm active tham gia
  ready/release/completion barrier. Ban đầu tái sử dụng kernel hiện tại.
- [x] Kết thúc bình thường khi **đủ minimum duration và hoàn thành vòng coverage
  hiện tại**. Duration dùng monotonic clock; thời gian dài tạo thêm vòng/seed,
  không tự tăng FMA depth làm mất số cạnh tải.
- [x] Phân biệt minimum duration, transition timeout và session hard deadline;
  deadline/safety/abort được phép dừng sớm nhưng không tạo PASS thiếu coverage.
- [x] Report thời gian thực, vòng hoàn thành, nhóm/pha đã verify, active CPU IDs,
  requested/available/started/completed workers và thiếu hụt coverage.

Gate: simulated clock chứng minh không dừng giữa vòng chỉ vì đủ phút; một nhóm
bị bỏ sót, zero-work, affinity failure, barrier timeout hoặc panic không PASS;
Ctrl+C/safety vẫn dừng được, không chờ hết campaign mới trả quyền điều khiển.

##### 3C-U3 — Launcher và dashboard cho một bài test

- [x] Menu chính có một START AVAILABILITY / POWER TEST; chọn minimum duration.
  Quick/Standard/Extended là preset thời gian có version, không là mức chứng nhận;
  chốt giá trị mặc định và ngân sách runtime trong bước này, công bố trên UI/docs.
- [x] Power-only và Sweep-only giữ ở Advanced để khoanh vùng/replay, không còn
  là hai luồng vận hành mặc định. Không tự chạy khi mở menu.
- [x] Giữ hardware Info và CPU/RAM usage meters; thêm phase, active group/CPU IDs,
  active worker count, kernel, vòng và coverage còn thiếu.
- [x] Thanh thời gian và coverage tách riêng. Đủ minimum duration nhưng chưa đủ
  vòng phải hiện "Đang hoàn thành vòng cuối", không hiện DONE/PASS hoặc ETA giả.
- [x] Cập nhật boot runner, watchdog heartbeat/deadline và recovery metadata theo
  plan; giữ policy verified result partition, tmpfs fallback có cảnh báo.
  Boot runner/heartbeat/recovery metadata đã nối; launcher lấy supervisor budget
  từ `power-platform-watchdog-seconds`, cùng timing policy với engine.

Gate: wide/compact render, launcher mock, phase/group switching, minimum-duration
boundary, PASS/FAIL/INCOMPLETE/abort và menu Advanced đều có regression tests.

##### 3C-U4 — Light/mixed và heavy-compute kernel chung

- [x] Giữ light/mixed integer/branch, AES known-answer và scratch verification;
  không quảng bá một block AES hoặc scratch nhỏ thành crypto/IMC stress nặng.
- [x] Thêm heavy floating-point kernel nhiều accumulator độc lập, expected result
  từ oracle độc lập, tolerance policy rõ ràng và final readback đầy đủ.
- [x] Chạy cả light và heavy trên các coverage role bắt buộc; chunk hữu hạn để
  kiểm cancellation/timeout. Report work count, actual burst time và kernel ID.
- [ ] Ghi supported/enabled/executed/verified riêng; kiểm artifact/runtime trước
  khi tuyên bố FMA/AVX cụ thể. Fallback phải công bố, không giả heavy-path coverage.
  Portable heavy FP 8 accumulator đã có oracle/fault tests; instruction inventory
  vẫn giữ `executed=null`, `verified=false` khi chưa có opcode evidence.

Gate: fault injection theo từng kernel/accumulator và giữa burst không PASS;
test fallback/cancellation, optimizer không loại bỏ work. Throughput/package
power là diagnostic, không là correctness oracle hay bằng chứng công suất cực đại.

##### 3C-U5 — Timing biến thiên và replay

Trạng thái: `implemented` ở source/automated + bounded host gates, 2026-09-19.
Plan v3 có bốn timing class trên mỗi vòng; seed quyết định class/jitter, thứ tự
nhóm vẫn cố định để giữ coverage role. Không khẳng định physical timing replay.

- [x] Recipe có version luân phiên idle ngắn/dài, burst ngắn/dài và nhóm nhỏ/toàn
  CPU; giới hạn an toàn tường minh, không random vô hạn hoặc bỏ mandatory class.
- [x] Seed quyết định lịch logic; report resolved timing, group/kernel selection
  và actual timing. Replay lịch không được hứa tái tạo timing vật lý chính xác.
- [x] Barrier start spread, actual idle/burst và telemetry availability gắn với
  transition ID; worker idle không đồng nghĩa package đã vào deep C-state.

Gate: cùng seed/topology/config sinh cùng plan; mọi timing class bắt buộc được
thực hiện; timeout/watchdog thích nghi với plan, không báo stall vì idle hợp lệ.

##### 3C-U6 — Persistent workers và integration hardening

Trạng thái: `implemented`, 2026-09-19; source/host gates, generic image build và
QEMU direct-kernel smoke PASS. Đây không phải BIOS/UEFI boot matrix hoặc hardware
power validation. NVIDIA image không được suy ra từ generic build.

- [x] Tái sử dụng worker pool đã pin/scratch-prefault suốt phiên; worker ngoài
  nhóm chờ blocking, không busy-spin. Dùng transition generation ID để tránh
  stale acknowledgement hoặc release nhầm nhóm.
- [x] Heartbeat độc lập với hot compute loop; checkpoint crash-durable theo
  policy phase/transition, không fsync trong burst. Bounded report/telemetry
  storage và checkpoint cadence cho campaign dài, công bố recovery granularity.
  Normal burst không fsync; mismatch là ngoại lệ cần durable ngay. Report giữ
  128 chi tiết gần nhất + aggregate coverage/first failure, công bố omitted count.
  Checkpoint cadence 5 s ở boundary; phase dài có thể làm commit trễ thêm một phase.
- [x] Recovery giữ last phase/group/kernel/transition và FAIL đã có; vẫn tách
  compute correctness, availability và power-platform attribution.
- [x] Cập nhật schema/validator/docs, build image và chạy image smoke sau software
  gate. Hardware reset/Vcore/PSU campaign được theo dõi riêng tại Bước 6.

Gate: fault tests stale generation, participant missing, worker crash, timeout,
abort và checkpoint interruption; bounded-resource long-run test; regression
CPU-RAM và hai CLI diagnostic cũ. Không ghi `validated` từ mock/QEMU/software PASS.

### Bước 4 — Telemetry, safety và diagnostic rerun

Trạng thái: `implemented` ngày 2026-08-29; xem
[TELEMETRY_SAFETY_V1.md](TELEMETRY_SAFETY_V1.md) và
[DEVICE_TEST_2026-08-29.md](DEVICE_TEST_2026-08-29.md). Hard process watchdog và
privileged live-USB integration đã được hoàn tất ở Bước 5.

- EDAC/ECC/MCE, thermal, throttle, timeout và crash evidence.
- Temperature Policy v2: telemetry luôn bật; operator toggle giới hạn nhiệt,
  mặc định OFF; khi ON chỉ trusted CPU sensor được phép auto-abort.
- Safety stop và policy versioned.
- Rerun giữ seed/block/offset để phân loại lỗi.
- Session seed mặc định lấy từ Linux CSPRNG; round seed dẫn xuất SHA-256, có
  commitment/evidence và replay bằng `--seed` (schema v7).

Hoàn thành khi hardware/execution event bị xử lý đúng policy và report đủ dữ liệu
để tái hiện hoặc kết luận `unattributed` thay vì quy linh kiện thiếu căn cứ.

### Bước 5 — Ubuntu Noble minimal live USB

Trạng thái: `implemented` ngày 2026-08-29; image raw đã qua kiểm tra cấu trúc và
boot-smoke trên QEMU ở cả legacy BIOS lẫn x86_64 UEFI. Xem
[BOOT_IMAGE_V1.md](BOOT_IMAGE_V1.md) và
[BOOT_IMAGE_TEST_2026-08-29.md](BOOT_IMAGE_TEST_2026-08-29.md). Chưa nâng thành
`validated` cho đến khi chạy trên USB và hardware matrix thật ở Bước 6.

- Custom read-only image từ Ubuntu Noble `debootstrap --variant=minbase`.
- Swap/service thừa bị tắt; runtime dùng `tmpfs`.
- Tự preflight, chạy DIT và chỉ ghi result vào phân vùng USB được phép.
- GPT raw image boot được bằng legacy BIOS/UEFI; hard process watchdog bao ngoài
  cooperative deadline của engine.
- UI text hiển thị CPU/RAM/nhiệt độ/Watt, chọn Safe/Full, xem sensor hoặc vào
  diagnostic shell; mặc định tự chạy Safe sau 10 giây.
- UI khi chạy hiển thị progress/phase/round/elapsed/RAM target và sensor; Full có
  duration chỉnh được, mặc định tối thiểu 30 phút.
- Dashboard khi test có ba khung hardware/utilization/process, thermal status và
  tự fallback compact khi console nhỏ.
- Full dùng 95% safe allocatable limit sau khi trừ reserve, không tuyên bố 95%
  physical RAM.
- Ubuntu HWE kernel, firmware và bộ module sensor/EDAC phổ biến; chỉ probe module
  tự nhận dạng phần cứng, không dùng tham số force.
- CPU Watt ưu tiên hwmon, fallback sang chênh lệch energy counter powercap; thiếu
  counter hoặc quyền đọc phải hiện `N/A`.

Hoàn thành khi boot/chạy/report được mà không cài hoặc ghi vào ổ nội bộ máy test.

### Bước 6 — Hardware validation và MVP release

- Fault-injection campaign, repeatability và soak.
- Intel/AMD; 16/32/64/128 GB khi lab có phần cứng; non-ECC/ECC; NUMA nếu có.
- Availability/power-platform campaign: repeated idle-to-burst, reboot giữa run,
  watchdog recovery, WHEA/MCE/EDAC correlation và BMC/platform evidence nếu có.
- Với CPU có lịch sử reset/mất nguồn dưới transient load, ghi riêng số transition,
  profile BIOS/firmware, PSU/mainboard và evidence ngoài DIT; không dùng một phiên
  không mismatch để kết luận power-platform ổn định.
- Khóa USB image, schemas, golden vectors và validation report.

Protocol, ma trận và mẫu biên bản đã được chuẩn hóa tại
[VALIDATION_GUIDE.md](VALIDATION_GUIDE.md) và
[`validation/VALIDATION_STATUS.md`](../validation/VALIDATION_STATUS.md). Trạng thái
hiện vẫn là `NOT_VALIDATED` vì chưa có USB/hardware matrix evidence.

Chỉ cấu hình đã thực sự chạy mới được ghi `validated`.

## M0 — Specification lock

Kết quả cần có:

- product scope và status model v1;
- source manifest, descriptor và report schema v1;
- matrix/GF, parity, block/tail recipe và coverage policy được chốt;
- ECC, timeout và thermal safety policy;
- validation hardware matrix dự kiến.

Gate hoàn thành:

- mọi stage có input, output và oracle rõ;
- định nghĩa PASS không phụ thuộc utilization/benchmark;
- Linux coverage dùng `allocatable RAM`, không tuyên bố physical 100%;
- các quyết định versioned để cùng seed có thể tái hiện.
- random session seed phải được ghi trước tải, round seed không trùng và validator
  phải tính lại được commitment/derivation.

## M1 — Reference core và golden vectors

Kết quả cần có:

- source verifier;
- deterministic generator/recipe engine;
- scalar split, GF/matrix encode/decode, join và exact verifier;
- source expansion theo seed/block/global offset;
- intermediate và final golden vector pack;
- unit test cho full group và mọi tail case.

Gate hoàn thành:

- kết quả bit-exact, độc lập thứ tự thread;
- mọi golden vector chạy lặp lại giống nhau;
- fault injection ở split/matrix/join/final result đều bị phát hiện.

## M2 — Adaptive RAM engine

Kết quả cần có:

- RAM/NUMA inventory;
- dynamic safety reserve;
- progressive multi-arena allocation và safe backoff;
- prefault/touch/optional-lock;
- giữ high-water arena set trong suốt round;
- deterministic scatter, direct RAM patterns và exact readback;
- installed/usable và allocated/touched/written/read/verified unique coverage;
- total read/write traffic tách riêng khỏi unique coverage.

Gate hoàn thành:

- chạy cùng binary trên nhiều dung lượng mà không cần profile hard-code;
- không OOM trong các case allocation failure đã thử;
- không bỏ tail byte;
- `PASS` bất khả thi nếu `coverage_target_met` là false;
- fault injection sau RAM write tạo mismatch evidence đúng offset.
- Window/size-class và pattern-family recipe Bước 3A đã tích hợp vào system
  engine; hardware matrix 30/60 phút vẫn chưa validated.

## M3 — End-to-end CPU–RAM DIT

Kết quả cần có:

- matrix → scatter → compute/readback → exact verify → decode → join;
- CRC/hash/crypto/reduction và golden final result;
- independent scalar verifier cho optimized path;
- checkpoint theo stage và diagnostic rerun cơ bản.

Gate hoàn thành:

- lỗi CPU chia/join/tính toán và lỗi RAM đều làm phiên không PASS;
- parity không che mismatch;
- cùng evidence có thể tái hiện cùng recipe/stage/block;
- combined failure không bị quy component khi thiếu bằng chứng.

## M4 — Multi-core, SIMD và NUMA

Kết quả cần có:

- CPU affinity và topology-aware scheduler;
- cross-core role rotation;
- scalar/SIMD differential checking;
- local-node và cross-node NUMA routes;
- combined cache/coherence/IMC/RAM stress;
- FP/FMA numerical route tách khỏi bit-exact integrity.

Gate hoàn thành:

- mọi core mục tiêu tham gia đúng coverage;
- một NUMA node thiếu coverage không được node khác che;
- SIMD fault injection bị scalar/golden oracle phát hiện;
- scheduling khác nhau không thay expected result.

## M5 — Boot, telemetry và field diagnostics

Kết quả cần có:

- USB boot Linux minimal, root read-only và swap off;
- UI vận hành offline cùng sensor/driver evidence;
- inventory/preflight tự động;
- EDAC/ECC/MCE, thermal, throttle, timeout và crash telemetry;
- safe stop và diagnostic rerun;
- versioned JSON report và human-readable summary;
- log chỉ ghi vào đích được phép trên USB.

Gate hoàn thành:

- máy test không bị ghi vào ổ nội bộ;
- source hỏng cho `INVALID_SOURCE`;
- hardware/execution event theo policy không thể tạo PASS;
- report đủ build/source/descriptor/seed để tái hiện.
- positive hardware validation dùng `linux_random`; fixed seed chỉ dùng cho
  golden regression hoặc replay điều tra.

## M6 — Validation và MVP release

Kết quả cần có:

- fault-injection campaign toàn pipeline;
- soak và repeatability campaign;
- validation trên hardware matrix thực có sẵn, ưu tiên lớp RAM
  16/32/64/128 GB, non-ECC/ECC và single-node/NUMA;
- đo boot footprint, allocatable coverage, throughput và thời gian;
- release USB image, golden vectors, schemas và validation report.

Gate hoàn thành:

- mọi fault injection bắt buộc làm phiên không PASS;
- không có false PASS đã biết;
- coverage target đạt ổn định trên từng cấu hình được công bố;
- chỉ hardware/platform đã chạy mới mang trạng thái `validated`.

## Sau MVP

- tầng pre-OS Memtest nếu cần gần toàn bộ physical RAM coverage;
- topology-aware DIMM attribution khi platform cung cấp mapping đáng tin cậy;
- retention/long-duration profiles;
- signed manifest, signed report và secure update;
- GPU/VRAM/PCIe route trong dự án hoặc sản phẩm kế tiếp.

## Chuỗi phụ thuộc chính

```text
M0 Specification
        |
M1 Reference + golden vectors
        |
M2 Adaptive RAM engine
        |
M3 End-to-end correctness
        |
M4 Multi-core/SIMD/NUMA
        |
PW Pattern/window coverage
        |
M5 Boot + telemetry
        |
M6 Validation + MVP release
```

M1–M3 là critical path correctness. Tối ưu SIMD, throughput hoặc UI không được
đi trước các gate reference, exact verification và fault injection tương ứng.
