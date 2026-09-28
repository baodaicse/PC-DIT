# Trạng thái DIT-01

Cập nhật: 2026-09-07.

Unified testing UI: COMBINED-A source implemented. `dit-ui-core::UnifiedDashboard` giữ duy nhất geometry/header/hardware/live/progress/counter/footer; CPU‑SSD/NVMe và GPU‑VRAM (single/multi) đã dùng adapter chung. Chỉ nội dung running thay theo engine. Roadmap combined CPU‑RAM‑NVMe‑GPU đã chốt theo resource arbiter + baseline/overlap/coupled routes; scheduler và native CPU‑RAM adapter còn pending.

## Project hiện hành

`/home/hoang/DIT-01` là nơi duy nhất để phát triển tiếp. Hai project nguồn giữ làm bản đối chiếu, không còn là dependency build. HardwareValidator không bị tác động.

| Phần | Trạng thái |
|---|---|
| Một Cargo workspace / lockfile, 18 crates | implemented; compile và Clippy PASS |
| CPU–RAM + CPU–SSD/NVMe + GPU–VRAM trong workspace chung | 169 Rust tests; full regression và GPU gates PASS |
| CPU–NVMe Deep mặc định | implemented: 4 KiB matrix, independent reference, bounded compute pool, duration read-soak, UI/report evidence; hardware pending |
| GPU–VRAM–DIT | GPU-A..F implemented; CPU‑RAM-style UI + expanded telemetry + shared/multi UI và extension gates PASS; GTX 960 bounded PASS; Deep/image/multi-GPU matrix chưa validated |
| UI CPU–RAM / CPU–SSD | implemented: chọn module chung; CPU–SSD chọn SSD là chạy ngay, không minimum RAM preflight/xác nhận lần hai; hardware pending |
| CPU UI, boot policy, schema-v9 seed validator | PASS |
| Unified workspace/path/image/docs checks | 5 tests PASS |
| Cache Ubuntu Minimal | đã copy độc lập; không cần bootstrap lại từ đầu |
| Source vectors và Result lịch sử | so sánh với nguồn: giống nguyên bản |
| Release/control center/extension | source updated cho CPU–SSD, GPU–VRAM và control center; artifact hash cập nhật sau rebuild |
| Runtime watchdog độc lập | implemented: worker supervisor, heartbeat/activity stale gate, cgroup OOM attribution, final/recovery evidence; USB/hardware validation pending |
| Image/USB hợp nhất mới | chưa build, chưa burn, chưa hardware validated |

`make test` đã PASS. `make -n shared-image` xác nhận chuỗi build/test/image/verify chỉ dùng project hiện hành; không gọi make sang project cũ, không gọi QEMU.

`make gpu-check` PASS: GPU-A mock compatibility/negative, Vulkan inventory, loại llvmpipe, GPU-B transfer, GPU-C matrix, GPU-D retention/final readback và negative comparator. `make preview-menu` hiển thị ba lựa chọn READY; lựa chọn 3 mặc định chạy Vulkan thật.

Payload module chung: `dist/cpu-nvme-extension.tar.gz`; checksum hiện hành nằm ở file `.sha256` cạnh artifact để tránh ghi hash tự tham chiếu vào chính payload docs. Manifest đóng `dit.cpu-ram`, `dit.cpu-nvme`, `dit.vga`, binary CPU-SSD/GPU-VRAM, SPIR-V và control center; đây vẫn chỉ là payload cho pipeline chung, không phải image USB hoàn chỉnh. U2 hardware image validation còn pending.

Các báo cáo lịch sử trong `Result`, `validation` và `build` được giữ lại, không tự chuyển thành bằng chứng validation cho bản image mới. Chi tiết module ở `docs/cpu-ram` và `docs/cpu-nvme`; [roadmap chung](ROADMAP.md) và [migration](MIGRATION.md) mô tả phạm vi hợp nhất.

GPU–VRAM có [roadmap riêng](gpu-vram/ROADMAP.md), [nhật ký task/evidence](gpu-vram/STATUS.md) và [checklist validation](gpu-vram/VALIDATION.md), cập nhật 2026-09-07. GPU-A..F đã triển khai; image USB, Deep 30/60 phút và hardware/multi-GPU matrix còn pending.

`make gpu-ef-check` PASS: unified NVMe/GPU renderer, layout động 1/2/4 GPU, expanded sensor provider, worker/report isolation, software-renderer INVALID, bounded Vulkan 16 transfer/matrix/retention và negative gates, manifest/SBOM/shader/docs trong extension cùng boot policy. Payload mới `dist/cpu-nvme-extension.tar.gz` có SHA-256 `9735ea7c6c92d4bcfb33c6b00bdee55f6ff624d4842ed9b486a753ecd1226846`; payload hiện đóng cả `dit-watchdog` và tài liệu runtime recovery. Không build image hoặc chạy QEMU trong gate này.

Không thực hiện test SSD, format/mount dữ liệu, burn USB, build/boot image hoặc chạy tải toàn RAM trong lượt gộp.

## CPU–NVMe Deep — 2026-09-06

- `DITDEEP2` lưu header gắn seed/generation/device/LBA, A/B/C 16×16, permutation, padding và SHA-256 trong mỗi block 4 KiB.
- CPU producer kiểm tra với oracle độc lập trước khi cho phép write; dữ liệu đọc thật từ backend được split/join, tính lại bằng kernel tối ưu rồi so với C lưu trên SSD và reference u128.
- Preserving-file/raw bounded tự cấp compute workers theo logical CPU và số SSD chạy đồng thời. Queue chỉ cho tối đa hai batch in-flight; một SSD có một I/O coordinator và pool compute riêng.
- Khi hết write budget, engine không PASS sớm: chuyển sang read-soak + matrix compute cho tới duration, không tăng host writes. Simulation có `--operations` vẫn là regression hữu hạn.
- UI hiển thị `MATRIX CHECKS COMPLETED`, I/O, `FAIL TOTAL`, CPU workers, rounds, write operations, read-soak và stage hiện tại; progress chính theo thời gian. Mỗi SSD vẫn thêm đúng một speed row và một progress row.
- 14 negative fault classes gồm data/I/O cũ cùng producer/read/reference/join/read-soak. Các test xác nhận read-soak đạt deadline, phát hiện corruption và không ghi thêm sau ngân sách.
- Golden SHA-256 của block fixture `DITDEEP2`: `055f7fb59f5716e3a1c1269aea6b9c5f68dc653938f25fabec1540d2ff68a53c`.
- Host regular-file Deep smoke PASS: 8/8 I/O, 1,220 matrix checks, 15 compute workers, 0 failure. Evidence: `build/deep-host-check-20260906/report.json` và `events.jsonl`; file dữ liệu tạm đã được xóa.
- Host duration smoke 1 giây PASS: 4 write operations rồi 108 read-soak operations, 8,845 matrix checks, bytes read lớn hơn bytes written và `duration_target_met=true`. Evidence: `build/deep-host-check-20260906/duration-report.json`.
- Rustfmt và Clippy strict PASS. Một test timing CPU–RAM cũ từng trượt khi full workspace chạy song song nhưng PASS ngay khi chạy riêng; full gate cuối được chạy tuần tự. Không test/ghi SSD thật, không build image hoặc QEMU trong lượt này.

Chi tiết thiết kế/gate: [NVME_DEEP_PIPELINE.md](NVME_DEEP_PIPELINE.md). Trạng thái vẫn là `implemented`, không phải `validated` trên NVMe/hardware matrix.

## Luồng CPU–SSD một bước — 2026-09-06

- Control Center đổi nhãn người dùng thành `CPU + RAM` và `CPU + SSD`; backend/binary vẫn dùng tên NVMe để phản ánh đúng driver/protocol.
- Chọn CPU–SSD mở thẳng picker. Nhập SSD `1,2` hoặc `A` là xác nhận bắt đầu duy nhất; duration lấy từ cấu hình launcher, mặc định 1.800 giây.
- Đã bỏ CPU–RAM Safe/minimum preflight, prompt duration và câu xác nhận `RUN PRESERVING` khỏi đường UI. Report chỉ chứa `cpu_ram_preflight` khi CLI chủ động cung cấp evidence cũ.
- Sau selection, app vẫn kiểm tra lại stable ID và volume plan trước mọi side effect; chỉ auto-mount filesystem hiện có được phép, tạo file/thư mục mới, không format/repartition/raw write và cleanup theo ownership.
- Đã sửa đường ghi kết quả: live selection/events/report staging dưới `/run/dit-suite` trên tmpfs, không còn ghi telemetry liên tục vào `DIT-RESULTS`. Chỉ copy bundle file đã đóng một lần sau khi engine dừng; mount result không xác thực thì giữ ở RAM và không fallback. Boot/result partition cùng parent device được đưa vào protected list.
- UI đổi `MEMORY` thành `RAM USAGE`, ghi rõ RAM used/total và hiện capacity của từng SSD riêng. Journal event nóng được downsample mỗi 500 ms/class/device; failure/warning/final state luôn ghi, tránh `events.jsonl` tăng theo hàng chục nghìn I/O.
- CPU‑SSD UI đã bỏ dòng sensor hard-code: dùng cpufreq + cùng hwmon/powercap collector của CPU‑RAM. Host smoke hiện 4.30 GHz/59.9 C; power đúng là N/A trên host này vì CPU‑RAM cũng không tìm thấy power sensor.
- GPU inventory đã hợp nhất. Host đọc `GeForce GTX 960`, BDF `0000:08:00.0`, bound driver `nvidia`; provider `nvidia-smi` bổ sung VRAM 2 GiB và live utilization khi có. Cached image đã có i915/xe/amdgpu/radeon/nouveau; policy mới thêm amdkfd và libdrm/Mesa/Vulkan cho image rebuild.
- UI/orchestrator/CLI tests, 143-test workspace gate, Clippy strict, extension manifest/checksum và boot policy PASS. Journal smoke 128 I/O chỉ sinh 11 dòng/3.846 byte và report PASS. Chưa chạy mount/SSD thật và chưa build image mới.
