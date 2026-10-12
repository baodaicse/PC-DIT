# Roadmap chung DIT-01

## U1 — Hợp nhất project

- Một workspace 18 crates và một lockfile cho CPU–RAM/CPU–NVMe/GPU–VRAM/watchdog.
- Một bộ tools/Makefile; image đóng CPU-RAM, CPU-SSD và GPU–VRAM hiện hành, giữ pipeline/cache Ubuntu Minimal.
- Docs theo module, giữ reports/golden/validation; README và status chung.
- Bỏ dependency build vào project cũ; giữ bản nguồn, copy cache độc lập.
- Gate: compile/tests toàn workspace, CPU UI/boot/seed, NVMe integration/payload và unified-path regression. Bằng chứng tại [STATUS.md](STATUS.md).

## U2 — Xác nhận image hợp nhất trên USB

- Operator chạy `make shared-image`, kiểm tra hash, burn USB đã chọn.
- Boot Intel/AMD, mở CPU–RAM và CPU–NVMe từ một menu.
- CPU–RAM seed/duration/temp không regression; NVMe auto-prepare an toàn, nhiều SSD, UI/cleanup/report đúng.
- Chỉ validated cấu hình thực sự đã chạy; app cũ chạy được không tự xác nhận image hợp nhất mới.

### U2.1 — Runtime watchdog và recovery evidence

Trạng thái: `implemented` ở source/host gate; image/hardware validation pending.

- `dit-watchdog` là process supervisor riêng, theo heartbeat/activity của CPU–RAM, CPU–NVMe và GPU–VRAM; không dùng worker/buffer stress của engine.
- Phân loại exit code, signal và cgroup `oom_kill` riêng; stale activity có grace rồi mới kết thúc worker.
- Ghi `watchdog-status.json`/`watchdog-final.json` ở tmpfs và marker vào result partition đã xác thực; menu hiện recovery warning nếu marker còn trạng thái chưa đóng.
- Chưa bao phủ kernel hard lock, firmware hang hoặc mất nguồn: cần platform/BMC watchdog nếu yêu cầu recovery trong các trường hợp đó.
- Gate kế tiếp: USB thực chạy PASS, `kill -9` worker, induced memory pressure/OOM (chỉ lab), stale heartbeat, reboot giữa run và kiểm marker trên result partition. Checklist: [RUNTIME_WATCHDOG.md](RUNTIME_WATCHDOG.md).

### U2.2 — Bắt lỗi khi app treo hoặc phần cứng lỗi thật (FC0–FC8)

Trạng thái: `proposed` (2026-10-12), chưa code. Chi tiết: [FAULT_CATCH_ROADMAP.md](FAULT_CATCH_ROADMAP.md).

- Kết quả thực tế: 8 ô PLATFORM đang hiện là lớp SSD treo trên NTFS (lỗi DIT); khoảng 13 phiên chết giữa chừng (phần lớn app treo, người vận hành reset) không được ghi lại.
- Lối thoát cho người vận hành: `nvidia-smi` có timeout, console cứu hộ ở VT2, SysRq.
- Chặn nguyên nhân: chốt chặn bộ nhớ (PSI, cgroup cho các lớp test).
- Không mất phiên: hộp đen là service độc lập, chỉ quan sát và ghi; khi app chết, treo hoặc không ghi nhận sự cố, nó tự ra result ngay trong phiên và chuyển sang màn kết quả. Chỉ khi cả máy chết mới hậu kiểm ở lần boot sau (lý do reset AMD, pstore).
- Ghi đúng nguyên nhân: watchdog xét I/O và CPU trước khi kết luận treo; PLATFORM tách theo nguyên nhân; lỗi hạ tầng test chỉ làm phiên INVALID.

## U3 — Phát triển module và phối hợp

### CPU–RAM availability và power-platform hardening

Trạng thái: `in_progress`; menu số 4 và launcher UI power-platform riêng đã có;
CLI power-platform/recovery riêng, minimum-work và
evidence availability/power-platform đã có ở source. Chi tiết phạm vi còn mở tại
[implementation 3C](cpu-ram/AVAILABILITY_POWER_PLATFORM.md).
Implementation không bị chặn bởi hardware validation.
Roadmap chi tiết nằm tại [CPU-RAM Bước 3C](cpu-ram/ROADMAP.md#bước-3c--availability-và-power-platform-evidence).

- P0 khóa minimum-work/worker/burst coverage, tách `FAIL` khỏi
  `INCOMPLETE`/`INVALID` và áp dụng duration-complexity ramp nhất quán cho
  compute-oracle trong `system-run`.
- P1 hợp nhất progress/watchdog/recovery thành availability evidence bền qua
  reboot và thêm profile idle-to-burst đồng bộ bằng global barrier, có minimum
  transition gate.
- P2 thêm per-core/SMT isolation, IMC diagnostic không tham gia integrity verdict
  và instruction-path evidence rõ ràng.
- **3C-U1..U6 — in_progress (2026-09-19):** hợp nhất Power Transitions và Core/SMT
  Sweep thành một campaign nhiều pha, giữ module riêng. Thứ tự: contract/planner
  → scheduler theo duration + complete coverage round → launcher/dashboard chung
  → light/heavy kernels → seeded timing/replay → persistent workers/integration.
  Hai mode cũ giữ ở Advanced/CLI diagnostic. Chi tiết và gate từng bước tại
  [CPU-RAM roadmap](cpu-ram/ROADMAP.md#3c-u--hợp-nhất-power-transitions-và-coresmt-sweep).
  Source đã có planner/scheduler, UI minimum time + coverage, heavy FP 8 accumulator,
  U5 seeded timing và U6 persistent workers/bounded report/schema validator.
  U5/U6 implemented, generic image build + QEMU direct-kernel smoke PASS tại
  module STATUS. Opcode verification
  và hardware effectiveness vẫn là evidence còn mở.
- Data integrity, compute correctness, availability và power-platform là các trục
  riêng. Reset/mất nguồn chỉ tạo `INTERRUPTED`/`RESET_SUSPECTED`; không tự quy
  CPU/Vcore/VRM/PSU nếu thiếu WHEA/MCE/EDAC/BMC hoặc evidence ngoài tương ứng.
- Code + automated/fault-injection gates đủ để ghi `implemented`; chỉ hardware đã
  chạy campaign thật mới được ghi `validated`.

### CPU–NVMe Deep mặc định (D1–D4)

Trạng thái: `implemented` ngày 2026-09-06 ở mức source/automated gates; chưa hardware validated.

Không duy trì Standard. Wire format matrix 4 KiB, reference độc lập, compute pool/batch gối I/O, read-soak sau ngân sách ghi và UI/report tương ứng đã được nối vào đường chạy mặc định. Gate phần cứng còn mở tại [NVME_DEEP_PIPELINE.md](NVME_DEEP_PIPELINE.md).

### UI CPU–RAM / CPU–SSD và one-step SSD start

Trạng thái: `implemented` ngày 2026-09-06; software gates PASS, physical USB pending.

- Control Center hiển thị `CPU + RAM` và `CPU + SSD`; tên module nội bộ vẫn là `dit.cpu-nvme` để giữ contract kỹ thuật.
- CPU–SSD chạy độc lập, không bắt buộc CPU–RAM minimum preflight. Evidence preflight cũ vẫn là input tùy chọn cho CLI/attribution.
- Picker SSD là quyết định bắt đầu duy nhất. Sau khi nhập alias hoặc `A`, app tự revalidate stable ID, chọn/mount volume hợp lệ, tạo file mới và chạy Deep với duration đã cấu hình; bỏ prompt duration và `RUN PRESERVING`.
- Các guard system/boot/results, filesystem policy, free-space reserve, create-new, no raw/no format và cleanup không bị hạ thấp.
- Tách I/O: payload test chỉ ở SSD được chọn; live telemetry/report staging trong `/run` tmpfs. USB boot chỉ nhận bundle kết quả một lần sau khi engine dừng. Boot/result partition và parent device được truyền thành protected targets; không fallback nếu result mount chưa xác thực.
- UI phân biệt rõ `RAM USAGE` với SSD capacity; mỗi SSD hiện total capacity riêng. Event journal được rate-limit 500 ms/class/device để RAM/tmpfs không phình theo số I/O, trong khi failure/completion và report tổng vẫn đầy đủ.
- CPU‑SSD dashboard tái sử dụng CPU‑RAM telemetry cho temp/package power và bổ sung cpufreq fallback; source gate và host UI smoke đã PASS, còn sensor matrix Intel/AMD trên image thật cần hardware validation.
- Shared GPU inventory cho CPU‑RAM/CPU‑SSD đã implemented: model/BDF/bound driver/VRAM-safe. Image policy thêm Intel `xe`, AMD KFD và libdrm/Mesa/Vulkan; cần physical matrix Intel iGPU/Arc, AMD iGPU/dGPU, NVIDIA nouveau và các image NVIDIA proprietary theo thế hệ/Secure Boot.

### GPU–VRAM–DIT (GPU-A..GPU-F)

Trạng thái: GPU-A..F `implemented` ngày 2026-09-07. Bounded hardware smoke PASS trên GTX 960; Deep soak, image USB và hardware/multi-GPU matrix chưa validated. [Roadmap chi tiết](gpu-vram/ROADMAP.md) · [Trạng thái/evidence](gpu-vram/STATUS.md).

- GPU-A: contract, golden, CPU oracle, backend mô phỏng, menu số 3 và UI baseline — implemented/software PASS.
- GPU-B: Vulkan capability, GPU identity, memory budget và transfer round-trip — implemented.
- GPU-C: VRAM pattern, split/scatter/join và integer matrix có CPU kiểm chứng — implemented.
- GPU-D: Deep theo duration, xoay seed/chunk, logical coverage, retention và error handling — implemented.
- GPU-E: UI/sensors chung, picker và process worker nhiều GPU, hàng usage/VRAM/speed/progress động — implemented; multi-GPU hardware pending.
- GPU-F: payload/image hiện có, negative campaign, manifest/SBOM và checklist validation theo GPU/driver thực — implemented; physical image matrix pending.
- UI/sensor follow-up 2026-09-07: GPU wide layout đồng bộ CPU‑RAM, CPU dùng shared sampler; NVIDIA provider lấy đủ utilization/VRAM/temp/watt/core+memory clock, sysfs mở rộng cho AMD/Intel. Builder có flavor NVIDIA tách biệt bằng package tường minh; generic nouveau và proprietary flavor phải validation riêng.

Mỗi task gồm source, gate phù hợp và cập nhật docs khi thực hiện. Backend đầu tiên đề xuất Vulkan Compute; GPU inventory/driver bind hiện có không đồng nghĩa engine đã hỗ trợ compute. Độ phủ báo theo vùng logic đã verify; chưa cam kết quét mọi cell VRAM vật lý. Giai đoạn sau F mở campaign CPU–RAM–SSD–GPU với ngân sách tài nguyên chung.

### GPU resident compute và CUDA (GPU-G1..G7)

Ưu tiên thực thi mới: [GPU-H H1–H8 — Async balanced pipeline](gpu-vram/GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md),
trạng thái `partial` (adapter CUDA burst + controller v0 có trong source, chưa đạt gate).
Tách compute/verifier, tối ưu CPU compare và corpus,
budget VRAM/RAM queue, tự throttle/recover khi verifier không theo kịp; kế thừa
G và không chờ toàn bộ hardware validated để triển khai source.

Trạng thái: G1/G2/G3 và CUDA sequential `implemented`; G4, overlap G5, G6/G7 còn mở ngày 2026-09-20. [Roadmap triển khai](gpu-vram/GPU_RESIDENT_COMPUTE_ROADMAP.md).
Phát triển tiếp từ baseline GPU-A..F implemented, không chờ toàn bộ hardware
validated. Campaign profiling/replay, workspace budget/backoff, persistent Vulkan
resources và fault campaign đã có source/software gates + GTX 960 bounded smoke
PASS; layer/soak/image còn mở được ghi trong
[implementation G1/G2](gpu-vram/GPU_G12_IMPLEMENTATION.md). Đã triển khai tiled
GEMM và CUDA sequential; xem [G3/CUDA](gpu-vram/GPU_G3_CUDA_IMPLEMENTATION.md).
Chưa triển khai overlap hoặc rebuild image boot.

- G1–G2: đo CPU/oracle/PCIe/kernel riêng, tái sử dụng tài nguyên Vulkan.
- G3–G4: tiled integer GEMM, input/expected corpus resident và pipeline nhiều
  slot với CPU verifier có giới hạn; không ghi đè output chưa xác thực.
- G5: CUDA backend tùy chọn cho NVIDIA, dùng chung contract/oracle/report và
  policy toolchain phù hợp GPU cũ; generic image vẫn chạy Vulkan.
- G6–G7: FP32 compute, GPU-local VRAM transform, lịch transfer/retention riêng,
  dashboard/backlog/verified-throughput, image và evidence A/B.
- GPU utilization không thay compute throughput/coverage. Intermediate chưa
  kiểm tra không được tính là verified; hardware effectiveness ghi riêng.

### Unified UI và combined CPU–RAM–NVMe–GPU

[Thiết kế và roadmap COMBINED-A..F](UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md). COMBINED-A đã implemented ở source: NVMe và GPU/multi-GPU dùng cùng `UnifiedDashboard`; CPU–RAM hiện giữ cùng visual contract và sẽ chuyển qua native adapter khi combined scheduler được triển khai. COMBINED-B..F gồm resource arbiter, scheduler xoay vòng, coupled exact route, aggregate stability evidence và image/hardware validation.

- Tiếp tục [kế hoạch CPU–RAM](cpu-ram/IMPLEMENTATION_PLAN.md) và [roadmap CPU–NVMe](cpu-nvme/ROADMAP.md).
- Raw bounded vẫn là nhánh expert riêng, không bật tự động.
- Runner CPU–RAM–NVMe–VGA đồng thời cần resource/scheduling/thermal gates riêng. Hiện có control center, preflight và report adapters; gộp source không đồng nghĩa đã hoàn thành concurrent stress runner.
