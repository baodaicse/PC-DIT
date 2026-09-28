# Trạng thái dự án

**2026-09-25 — UI:** sau khi chọn ổ, UI hỏi thời lượng (10 / 30 / 60 phút hoặc tự nhập 1–1440) và mức ghi mỗi ổ: Light 2 GiB (mặc định cũ), Standard 16 GiB, Heavy 64 GiB (file test lớn bằng ngân sách, giữ trống 4 GiB; ổ không đủ chỗ thì từ chối trước khi ghi). Hết ngân sách thì chuyển sang đọc lại và kiểm lại như trước. Report ghi `capacity_mib`/`write_budget_mib` đã chọn.

> Từ 2026-09-06, tài liệu này thuộc module trong project chung DIT-01. Dùng [README chung](../../README.md) và [status chung](../STATUS.md) cho lệnh build/trạng thái hợp nhất; các mốc bên dưới giữ lịch sử module.

Cập nhật: 2026-09-06

## Task chủ đạo

| Task | Bao gồm | Trạng thái | Có thể chạy liên tục |
|---|---|---|---|
| TASK-A Nền tảng/simulator | M0, M1, M3 | implemented; automated gates PASS | Hoàn tất, không ghi SSD |
| TASK-B Inventory/safety | M2 + safety M4 | implemented; tests + host read-only PASS | Hoàn tất, không ghi SSD |
| TASK-C Một NVMe thật | M4, M5 | implemented; hardware I/O pending | Có preserving-file + raw safety gate; chờ hardware validation |
| TASK-D Multi-NVMe/UI | M6, M7 | implemented; automated gates PASS | Preserving multi-run + UI động; chờ hai NVMe thật |
| TASK-E Shared image/validation | M8, M9, M10 | M8 implemented; M9/M10 planned | Extension/hook xong; chưa build/burn/hardware validate |
| TASK-F Combined/release | M11, M12 | implemented; automated gates PASS | Control center + adapters + artifact xong; hardware pending |

Khi hoàn thành một task, bảng này và các mốc chi tiết bên dưới phải được cập nhật trong cùng lượt triển khai.

| Mốc | Trạng thái | Ghi chú |
|---|---|---|
| M0 Đặc tả/roadmap | implemented (documentation) | Đã chốt phạm vi, safety, module contract và UI động |
| M1 Workspace/contracts | implemented | Schema v1, block 4 KiB, golden vector |
| M2 Inventory read-only | implemented | Host scan/selection/revalidation PASS; SMART host N/A |
| M3 Simulator/fault injection | implemented | Deep positive + 14 negative classes PASS |
| M4 Linux NVMe backend | implemented | O_DIRECT preserving-file/raw bounded; hardware chưa validated |
| M5 Telemetry/report | implemented | Throughput/report xong; SMART host N/A |
| M6 Multi-device scheduler | implemented | Per-device worker/report, barrier và worker limit; hardware pending |
| M7 Dynamic UI | implemented | Dashboard bố cục CPU–RAM, một speed row/SSD, interactive 8 FPS diff-line, compact/auto-page; hardware pending |
| M8 Shared-image extension | implemented | Extension + checksum + CPU image hook; chưa build image |
| M9 Negative/soak campaign | partially implemented | 14 memory fault classes + duration read-soak PASS; process/hardware faults pending |
| M10 Hardware matrix | planned | Chưa validated |
| M11 Combined modules | implemented | CPU–RAM/NVMe/GPU registry, UI, adapters; GPU–VRAM Vulkan ở lựa chọn số 3 |
| M12 Release | implemented (development artifact) | Extension tar/hash xong; RC hardware soak/signing pending |

Không có thao tác ghi nào đã được thực hiện trên SSD/NVMe trong mốc tài liệu này.

## Deep pipeline mặc định — 2026-09-06

- D1–D4 `implemented`: `DITDEEP2`, matrix 16×16 nhiều vòng, producer/reference độc lập, verifier tính trên readback buffer, bounded compute pool và read-soak theo duration.
- Preserving-file và raw bounded dùng Deep tự động; simulation có operation cap vẫn giữ đường regression hữu hạn.
- UI Deep hiển thị matrix checks/I/O/fail, workers/rounds/write/read-soak và stage, trong khi số hàng SSD vẫn thay đổi động theo selection.
- Report/event ABI bổ sung evidence Deep tùy chọn, giữ khả năng đọc report không có trường `deep`.
- Rustfmt/Clippy strict và automated tests PASS; SSD thật/shared image/hardware matrix vẫn pending. Không có thao tác ghi SSD trong lần triển khai này.

## Quyết định image dùng chung

Ngày 2026-09-05, TASK-E được đổi từ tạo Ubuntu Minimal image riêng sang tích hợp module NVMe vào pipeline image hiện có tại `/home/hoang/DIT-01`.

- Base OS, HWE kernel, firmware/driver, live-boot, GRUB BIOS/UEFI, GPT layout, `toram`, `DIT-RESULTS`, verify/QEMU và burn tool đều được tái sử dụng.
- Repository NVMe chỉ xuất binary, profile/hook và manifest/hash của module.
- Chưa sửa pipeline CPU–RAM và chưa tạo image mới trong lần cập nhật kiến trúc này.
- Chi tiết contract: [Tích hợp image DIT dùng chung](SHARED_IMAGE_INTEGRATION.md).

## Bằng chứng TASK-A

- `cargo fmt --all -- --check`: PASS.
- `cargo clippy --workspace --all-targets -- -D warnings`: PASS.
- `cargo test --workspace`: PASS, 14 tests.
- CLI campaign: PASS, 1 positive control + 9 injected-fault classes.
- Regular-file smoke: PASS, 40/40 operations, seed slots 20, failure count 0.
- Golden block SHA-256: `239f6123270948524c303d83813be458444d448664f2ba4fa751ea519d74d889`.

Lưu ý: đây là validation phần mềm trên memory/regular-file backend, không phải hardware validation của SSD/NVMe.

## Bằng chứng TASK-B

- Inventory fixture: root/mount/swap/partition/holder/protected-device guards PASS.
- Stable-ID selection, deduplication, reorder và node-change revalidation tests PASS.
- READ_ONLY/BOUNDED_WRITE/FULL_DESTRUCTIVE mode-policy tests PASS.
- Host scan: nhận đúng model, serial, firmware, kernel driver, controller, PCIe BDF/root port/link và AER counters.
- Host safety: `/dev/nvme0n1` được đánh dấu `PROTECTED / READ-ONLY` vì chứa hệ điều hành.
- Host select-all + stable-ID revalidate: PASS.
- Không mở hoặc ghi `/dev/nvme0n1`; mọi host command của TASK-B là inventory read-only.
- SMART host vẫn N/A do máy chưa cài `nvme-cli`; code/parser đã implemented nhưng chưa hardware validated.

## Bằng chứng TASK-C

- `O_DIRECT | O_DSYNC | O_NOFOLLOW` Linux block backend: implemented.
- Bounded range, alignment, ownership, stable-ID, exact confirmation và write-budget tests: PASS.
- Wall-clock stop, throughput events và report performance fields: PASS.
- CPU–RAM preflight PASS + SHA-256 evidence từng được enforced ở TASK-C; hiện là input attribution tùy chọn, còn device safety gates vẫn enforced trước open.
- Workspace: 27 automated tests PASS; rustfmt/Clippy strict PASS.
- Simulator telemetry/report smoke: PASS.
- Data-preserving backend: existing path luôn bị từ chối; direct-I/O write budget bị chặn trước submission; PASS.
- Host negative safety test: `/dev/nvme0n1` bị từ chối vì chứa `/` và `/boot/efi`; không tạo plan, không mở/ghi device node.
- Scratch NVMe positive run: pending vì máy hiện tại không có target an toàn để xóa.

## Bằng chứng TASK-D

- Workspace hiện tại: 40 automated tests PASS; rustfmt/Clippy strict PASS.
- Scheduler positive 1/2/4 memory devices: PASS.
- Scheduler giới hạn 1 hoặc 2 worker, giữ đúng thứ tự outcome: PASS.
- Negative 3-device run: device 2 FAIL có chủ đích; device 1 và 3 vẫn PASS; aggregate FAIL.
- Report schema lưu per-device outcome và aggregate verdict: PASS.
- UI snapshot cho 1/2/4 SSD; expanded/compact/paged layout: PASS.
- CPU/RAM/board host sampler và live utilization rows: PASS.
- In-place renderer không phát full-screen clear và khôi phục cursor: PASS.
- Multi preserving-file CLI revalidate toàn selection, exact stable-ID set, alias-directory mapping, budget/free-space và cleanup tất cả target.
- Hai NVMe thật chạy đồng thời: pending; TASK-D chưa hardware validated.

## Bằng chứng TASK-F

- `dit-orchestrator`: module registry, CPU–RAM/NVMe report adapters, aggregate verdict và resource budget implemented.
- Shared UI chọn `CPU + RAM`, `CPU + SSD`, read-only NVMe inventory hoặc diagnostic shell.
- CPU–SSD mở thẳng device picker, không chạy CPU–RAM minimum preflight; evidence preflight cũ chỉ còn input tùy chọn trong CLI.
- Interactive storage mode chỉ là `DATA_PRESERVING_FILE`; raw write không xuất hiện trong UI.
- Chọn alias SSD hoặc `A` là xác nhận bắt đầu duy nhất; app lấy duration cấu hình sẵn và bỏ màn hình plan/prompt `RUN PRESERVING`, nhưng vẫn revalidate stable ID/volume trước side effect.
- Live telemetry được chuyển hoàn toàn sang `/run/dit-suite` (tmpfs); USB `DIT-RESULTS` chỉ nhận một lần bundle đã đóng sau khi engine dừng. Boot/result partition và parent device được protect rõ ràng; mount result không xác thực thì không fallback.
- `61.88 GiB` kiểu hiển thị cũ được xác định là RAM 64 GB (`MemTotal`), không phải USB. UI mới dùng nhãn `RAM USAGE`, SSD capacity riêng; event journal giới hạn 500 ms/class/device để tmpfs không tăng theo từng I/O.
- Freq/temp/power CPU‑SSD dùng lại nguồn CPU‑RAM thay cho hard-code N/A. Host UI smoke backend RAM 1 giây PASS, hiện 4.30 GHz và 59.9 C; Watt N/A khớp sensor report của host Ryzen 7 3700X hiện tại.
- GPU không còn hard-code N/A: shared inventory hiện model/BDF/bound driver; sysfs hoặc `nvidia-smi` bổ sung VRAM/telemetry khi có. Host xác nhận GTX 960/`nvidia`/2 GiB; image open stack gồm i915/xe/amdgpu/amdkfd/radeon/nouveau plus Mesa/Vulkan, proprietary NVIDIA vẫn là build profile riêng.
- GPU-A..F bổ sung Vulkan, shared/multi-GPU UI và image validation payload; workspace hiện có 169 Rust tests. Checksum extension được cập nhật sau lượt rebuild GPU.
- Workspace hợp nhất hiện tại: 169 Rust tests được liệt kê; rustfmt/Clippy strict PASS.
- Positive aggregate CPU–RAM + 2 NVMe simulated: PASS.
- Negative attribution: CPU–RAM PASS, NVMe device 2 FAIL, aggregate FAIL: PASS.
- Versioned extension archive và mọi file trong payload qua SHA-256 verification.
- Dependency SBOM/Cargo.lock và operator/data-preservation manual được đóng trong extension.
- Extension hiện tại đã qua manifest/checksum gate; checksum authoritative nằm ở `dist/cpu-nvme-extension.tar.gz.sha256`. Journal smoke 128 I/O PASS với 11 event/3.846 byte. Reproducibility hai lần build của payload mới sẽ được chốt cùng image release.
- CPU image hook và boot-runner Bash syntax/physical boot policy: PASS.
- `make task-f-check`: PASS; không build image, không QEMU, không ghi SSD, không burn USB.
- Shared image/USB và multi-NVMe thật: pending hardware validation.

## Backlog RAW BOUNDED WRITE

Sửa lỗi UI chọn thư mục: kiểm tra existence/type, canonical path, mapping SSD và free space trước confirmation; hiển thị partition `NOT MOUNTED`, cho retry/Q. Clippy strict và 4 CLI tests PASS. Đây là kiểm thử phần mềm; lỗi ảnh chưa cung cấp log đủ để khẳng định duy nhất đường dẫn nào thiếu. Xem phần troubleshooting trong [TASK-F](TASK_F_IMPLEMENTATION.md).

Raw bounded backend một target vẫn `implemented` ở CLI chuyên gia; raw UI chưa được mở. Các mốc RBW-1..RBW-5 đã được đưa vào M9: scratch wizard, confirmation hai lớp, duration/TBW read-soak, crash/reset recovery và hardware gate. Chi tiết tại [Kế hoạch phát triển RAW BOUNDED WRITE](RAW_BOUNDED_WRITE_PLAN.md).

## Automatic storage preparation — 2026-09-05

- Đã thay prompt mount directory bằng tự chọn volume dữ liệu lớn nhất hợp lệ, xác nhận trước auto-mount/tạo thư mục và file mới. Không format/chia partition/raw write.
- Stable-ID/UUID/type revalidation, direct block mapping, free-space reserve và ownership cleanup implemented. Có `auto-storage-plan.json` và `auto-storage-outcome.json` cho chẩn đoán.
- Hỗ trợ policy ext4, NTFS qua ntfs3 với probe hibernation/dirty, exFAT. Image builder CPU-DIT-01 thêm package `ntfs-3g`; không tác động HardwareValidator.
- `make task-f-check`: PASS; 56 workspace tests, rustfmt và Clippy strict PASS. Thêm 9 tests về policy/candidate selection/identity/mount options/cleanup thư mục giả lập.
- Positive/negative multi-memory campaign, report aggregation, extension checksum và CPU physical boot policy: PASS.
- Release `auto-plan --devices all --no-smart` trên host: từ chối `/dev/nvme0n1` chứa hệ điều hành, exit 2 đúng dự kiến. Không mount/test/ghi device node.
- Chưa chạy mount/unmount thật, chưa test SSD dữ liệu, chưa build image/QEMU/burn USB. Hardware validation theo [AUTO_STORAGE.md](AUTO_STORAGE.md) vẫn pending; `implemented` không đồng nghĩa `validated` trên hardware matrix.

## Đồng bộ UI test với CPU–RAM — 2026-09-06

- Hai khung `SYSTEM HARDWARE` / `LIVE UTILIZATION` phía trên, `TESTING / PROCESS INFORMATION` phía dưới; viền cyan, thanh tô màu, state/time/session seed/completed/fail tổng.
- Mỗi SSD thêm đúng một hàng live speed R/W và một hàng operation-progress/fail. Khung trên dịch chiều cao theo N; màn hình nhỏ dùng compact, thiếu chiều cao tự chuyển trang mỗi 5 giây. Speed scale dùng đỉnh tổng R+W quan sát trong phiên, không gọi là SSD busy%.
- Diff-line renderer bỏ clear-before-repaint/newline, chừa hàng cuối tránh auto-scroll, bỏ qua frame không đổi. Sensor NVMe sampler chưa cung cấp vẫn N/A, không lấy số demo làm telemetry thật.
- `make task-f-check`: PASS; 62 workspace tests, trong đó 13 UI tests; rustfmt/Clippy strict, positive/negative memory campaign, report adapters, extension checksum và boot-policy PASS.
- UI geometry matrix: 0/1/2/4/8/32 SSD, 40–240 cột, 17–60 dòng; số hàng, chiều rộng, trang cuối, global failures và ANSI/plain geometry PASS. Có regression test mỗi SSD thêm đúng một hàng speed và không redraw frame giống nhau.
- Preview số liệu tổng hợp trên PTY 140×40, 2 SSD, 16 frames: exit 0, dùng ANSI màu/diff-line và khôi phục cursor. Preview không mở inventory/SSD hay chạy engine.
- Thêm `make preview-test-ui SSDS=2 FRAMES=80` để operator xem UI khoảng 10 giây trên host.
- Extension đã build: SHA-256 `f61f7058b97401343ad9dd3991670fecba3234f67da4e57b7446b6596f2bd9de`. Không build/burn image, không QEMU; cần `make shared-image` và USB mới để nhận UI. Console/GPU thật vẫn pending validation.
- Không sửa HardwareValidator hoặc UI/engine CPU–RAM; lần này chỉ đọc CPU–RAM UI để đối chiếu bố cục.
