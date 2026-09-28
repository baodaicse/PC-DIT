# Roadmap DIT CPU–NVMe

Trạng thái dùng ba mức rõ ràng:

- `planned`: đã đặc tả, chưa hoàn tất code;
- `implemented`: code và automated tests đạt;
- `validated`: đã chạy đạt trên ma trận phần cứng thật đã định nghĩa.

Một bước không được ghi `validated` chỉ vì chạy được trên máy phát triển hoặc máy ảo.

## Nhóm triển khai chủ đạo

### Nâng cấp Deep mặc định D1–D4

Trạng thái: `implemented` ngày 2026-09-06 ở mức automated software gates; chưa hardware validated.

- Bỏ phân nhánh Standard; engine CPU–NVMe mặc định dùng block matrix 4 KiB `DITDEEP2` và reference độc lập.
- Compute pool có giới hạn, tự chia worker theo logical CPU/số SSD và gối preparation với I/O hiện tại.
- Sau write budget, chỉ reread + recompute đến hết duration để tiếp tục tải đường NVMe→RAM→CPU mà không tăng TBW.
- UI/report hiển thị matrix checks, CPU workers, write/read-soak operations và thời gian compute/I/O.
- Hardware gate còn lại: một/hai NVMe thật, direct-I/O, CPU utilization, throughput, thermal, shutdown/cancel và fault recovery. Chi tiết ở [Deep pipeline](../NVME_DEEP_PIPELINE.md).

Các mốc M0–M12 bên dưới là checklist kỹ thuật chi tiết. Khi giao việc và triển khai thực tế, chúng được gom thành sáu task chủ đạo. Một task chủ đạo được thực hiện liên tục từ code, automated test đến cập nhật tài liệu; chỉ dừng sớm khi gặp rủi ro an toàn phần cứng hoặc cần người dùng xác nhận thao tác phá hủy.

### TASK-A — Nền tảng và engine mô phỏng

Bao gồm: M0, M1 và M3.

Trạng thái: `implemented`; automated gates đạt ngày 2026-09-04, chưa hardware validated.

- Tạo Rust workspace và hợp đồng event/report v1.
- Cài block 4 KiB, stripe, seed/generation và golden vectors.
- Tạo file-backed/fake backend, duration recipe và fault injection.
- Tạo CLI chạy simulator và bộ automated tests.
- Cập nhật docs/status sau khi test đạt.

Đầu ra: một engine correctness hoàn chỉnh chạy bằng file tạm, chưa mở hoặc ghi SSD thật.

Gate hoàn tất: positive campaign không false FAIL; toàn bộ lỗi bit flip, stale data, swapped LBA, short I/O, EIO và timeout giả lập đều được phát hiện.

### TASK-B — Inventory, lựa chọn thiết bị và safety gate

Bao gồm: M2 và phần safety nền của M4.

Trạng thái: `implemented` ngày 2026-09-04; host read-only smoke đạt, chưa hardware-matrix validated. SMART trên host chưa xác nhận vì thiếu `nvme-cli`.

- Phát hiện controller/namespace, SMART và PCIe topology ở chế độ read-only.
- Nhận diện root, mount, swap, USB boot/results và block-device holders.
- Chọn one/many/all bằng stable ID và chống device reorder/hotplug.
- Cài plan validator và ba mức READ_ONLY/BOUNDED_WRITE/FULL_DESTRUCTIVE.
- Kiểm thử bằng fixtures và inventory của máy hiện tại, chưa ghi ổ.

Đầu ra: công cụ inventory/selection có thể chứng minh ổ nào được phép hoặc bị cấm ghi và vì sao.

Gate hoàn tất: mọi ổ hệ thống/boot/results/mounted/unknown-holder bị chặn; thay đổi identity không thể chuyển target sang ổ khác.

### TASK-C — Engine NVMe thật một thiết bị

Bao gồm: phần còn lại của M4 và M5.

Trạng thái: `implemented` ngày 2026-09-04; automated gates và host protected-target refusal đạt. Chưa validated đường I/O trên scratch NVMe thật.

- Backend Linux với alignment, O_DIRECT, queueing, flush/FUA và reread.
- Chế độ `DATA_PRESERVING_FILE` mặc định: tạo file mới trong vùng trống, không ghi đè dữ liệu cũ.
- Chỉ mở scratch partition/namespace đã xác nhận.
- Thực thi recipe 4 KiB/64 KiB/128 KiB/1 MiB với write budget.
- Thu throughput, IOPS, latency, SMART, AER và kernel events.
- Ghi JSONL event, final report và evidence khi lỗi/Ctrl+C.

Đầu ra: bản test một NVMe thật ở READ_ONLY, DATA_PRESERVING_FILE và raw BOUNDED_WRITE.

Gate hoàn tất: automated safety tests đạt; simulator campaign đạt; data-preserving mode fail closed nếu path/device mapping không rõ; chỉ chạy raw BOUNDED_WRITE sau khi người dùng chỉ định scratch target và xác nhận dữ liệu có thể bị mất.

### TASK-D — Multi-NVMe và UI động

Bao gồm: M6 và M7.

Trạng thái: `implemented` ngày 2026-09-05; automated gates đạt, chưa validated trên nhiều NVMe thật.

- Scheduler cho một, nhiều hoặc tất cả NVMe.
- Worker/counter/verdict độc lập theo thiết bị.
- Device picker và một speed bar cho mỗi SSD được chọn.
- Layout tăng/giảm theo N, compact mode, scroll/page và terminal resize.
- Render in-place 2–4 FPS và snapshot/PTY tests.

Đầu ra: ứng dụng standalone hoàn chỉnh có cùng ngôn ngữ UI với DIT CPU–RAM và không fix cứng số SSD.

Gate hoàn tất: fake backend đạt với 1/2/4 thiết bị; hai SSD thật chỉ được chạy sau khi TASK-C đã qua safety gate trên từng target.

### TASK-E — Tích hợp image dùng chung, chiến dịch lỗi và hardware validation

Bao gồm: M8, M9 và M10.

- Đóng gói NVMe thành extension và tích hợp vào pipeline image hiện có của DIT CPU–RAM.
- Tái sử dụng nguyên Ubuntu Noble/HWE, firmware/driver, live-boot, GRUB, partition layout, results partition, verify/QEMU và tool burn hiện có; không tạo image builder riêng.
- Chạy negative campaign, process-kill, hot-unplug và soak.
- Kiểm tra Intel/AMD, Gen3/4/5, một/hai SSD và đổi chéo drive/slot.
- Lưu base-image/module manifest, report và phân biệt `implemented` với `validated`.

Đầu ra: NVMe extension có version/hash, image dùng chung được build bằng pipeline CPU–RAM hiện có và hồ sơ validation theo từng cấu hình phần cứng.

Gate hoàn tất: image CPU–RAM không regression; module/hash có thể truy vết; boot/results disks luôn được bảo vệ; negative campaign đạt; chỉ cấu hình thực sự đã chạy mới được đánh dấu validated.

### TASK-F — Bộ tổng hợp CPU–RAM–NVMe–VGA và release

Bao gồm: M11 và M12.

Trạng thái: `implemented` ngày 2026-09-05; automated/host gates đạt, shared image và hardware validation pending.

- Tạo adapter CPU–RAM theo event contract chung.
- Thêm module registry/orchestrator và resource arbitration.
- Giữ khả năng chạy từng module riêng; mọi boot flavor/combined image đều được đóng từ pipeline image CPU–RAM dùng chung.
- Mở contract cho VGA, sau đó tích hợp khi VGA engine sẵn sàng.
- Khóa version/dependencies, checksum, SBOM, manual và release artifacts.
- Automatic file-test preparation: `implemented`; chọn SSD → đề xuất filesystem dữ liệu → xác nhận → auto-mount/thư mục/file test mới → cleanup theo ownership. Không format/raw. Chi tiết [AUTO_STORAGE.md](AUTO_STORAGE.md).
- Gate còn lại cho auto preparation: mount/unmount thật ext4/NTFS/exFAT, dirty/hibernated NTFS bị chặn, thiếu free space, lỗi giữa chừng và hai SSD. Chưa hardware validated; crash recovery và chọn volume thay thế là backlog.

Đầu ra: nền tảng DIT hợp nhất nhưng verdict/evidence của từng module vẫn độc lập.

Gate hoàn tất: standalone và combined cho kết quả tương đương với cùng plan; một module lỗi không làm sai attribution module khác.

## Thứ tự chạy đề nghị

```text
TASK-A -> TASK-B -> TASK-C -> TASK-D -> TASK-E -> TASK-F
```

TASK-A và TASK-B có thể phát triển không phá hủy trên máy hiện tại. TASK-C trở đi có checkpoint bắt buộc trước lần ghi SSD thật. Không gom checkpoint phá hủy vào quyền thực hiện task chung.

## M0 — Đặc tả, an toàn và hợp đồng module

Trạng thái: `implemented (documentation)`; chưa cần/không được coi là hardware validated.

- Chốt phạm vi NVMe, mô hình lỗi và verdict.
- Chốt block 4 KiB + stripe/mixed I/O recipe.
- Chốt ba chế độ READ_ONLY/BOUNDED_WRITE/FULL_DESTRUCTIVE.
- Chốt event schema/lifecycle để ghép CPU–RAM–NVMe–VGA.
- Chốt UI nhiều thiết bị và nguyên tắc chiều cao động.

Gate: tài liệu thống nhất, không có đường ghi mơ hồ.

## M1 — Rust workspace, contracts và golden vectors

Trạng thái: `implemented` ngày 2026-09-04; chưa hardware validated.

- Tạo Cargo workspace/crates.
- Định nghĩa stable device ID, plan, event, report và verdict schema v1.
- Cài generator/verifier block 4 KiB, stripe 64 KiB và golden vectors độc lập.
- Fake event producer để UI/orchestrator phát triển không cần SSD.
- CI: fmt, clippy, unit/property tests, schema fixtures.

Gate: cùng seed/LBA/generation tạo expected cố định; mutation bất kỳ trong golden vectors bị phát hiện.

## M2 — Inventory và safety classification read-only

Trạng thái: `implemented` ngày 2026-09-04; chưa hardware-matrix validated.

- Liệt kê NVMe controller/namespace qua sysfs và nvme-cli.
- Map namespace -> controller -> PCI BDF/root port/link.
- Thu SMART và kernel/AER baseline.
- Phát hiện mount, root, swap, USB boot/results và holders.
- Device selection theo stable ID; hỗ trợ one/many/all.

Gate: tuyệt đối chưa mở write; fixture và máy thật nhận đúng system/boot/result disk, hotplug/reorder không đổi target.

## M3 — File-backed simulator và fault injection

Trạng thái: `implemented` ngày 2026-09-04; chưa hardware validated.

- Engine chạy trên regular/sparse file.
- Recipe theo duration, seed, generation, vùng và block-size mix.
- Deep matrix compare + metadata location + SHA/padding; producer/readback/reference là các đường kiểm tra tách biệt.
- Inject bit flip, stale generation, swapped/duplicate block, short I/O, EIO, timeout, device disappearance, producer/read/reference/join và read-soak giả lập.
- Report sống sót qua Ctrl+C/process error.

Gate: mọi lỗi đã inject được phát hiện/attribution đúng; positive campaign 0 false FAIL.

## M4 — Linux NVMe backend an toàn

Trạng thái: `implemented` ngày 2026-09-04; chưa hardware validated trên scratch NVMe thật.

- Explicit scratch partition/namespace only.
- Data-preserving file backend cho SSD có dữ liệu; `create_new`, direct I/O, free-space reserve và cleanup evidence.
- Alignment, O_DIRECT, queueing, flush/FUA và reread semantics.
- Enforce byte range và host-write budget trước submission.
- Revalidate stable ID ngay trước open.
- CPU–RAM preflight tùy chọn; không có evidence thì dùng attribution `CPU_SSD_PATH_UNATTRIBUTED`.

Gate: file mode không thể overwrite path đã tồn tại; raw mode không thể ghi ngoài range/budget qua public API; power-loss/process-kill recovery giữ được audit log.

## M5 — Telemetry và report schema

Trạng thái: `implemented` ngày 2026-09-04; SMART host vẫn N/A do thiếu `nvme-cli`.

- Current/average/peak read/write throughput, IOPS và latency.
- Bytes touched/verified, unique regions, operations, seeds/generations và failures.
- SMART/PCIe/AER/kernel events trước, trong, sau.
- JSONL events + final JSON + human-readable summary.

Gate: counter monotonic/chính xác qua concurrency; schema versioned và parser compatibility tests đạt.

## M6 — Multi-device scheduler

Trạng thái: `implemented` ngày 2026-09-05; 1/2/4 fake-device gates đạt, hardware multi-NVMe pending.

- Chọn một, nhiều hoặc tất cả thiết bị.
- Executor/counter/verdict riêng cho từng SSD.
- Đồng thời hoặc tuần tự theo plan; start barrier tùy chọn.
- CPU/RAM/queue-depth budget chung.
- Một thiết bị lỗi không làm mất report hoặc gán sai lỗi cho thiết bị khác.

Gate: test 1/2/4 fake devices và ít nhất 2 SSD thật; selection/reorder/hot-unplug đúng target.

## M7 — UI dùng chung, chiều cao động

Trạng thái: `implemented` ngày 2026-09-05; layout/snapshot/in-place renderer gates đạt, hardware/PTY matrix mở rộng pending.

- Port ngôn ngữ UI DIT CPU–RAM sang `dit-ui-core` dựa trên event.
- Device picker one/many/all.
- Một hàng speed cho mỗi SSD đã chọn trong Live Utilization; một hàng operation-progress/fail riêng trong Process. Dashboard đồng bộ bố cục CPU–RAM implemented ngày 2026-09-06.
- Layout expand/collapse theo N, compact mode và scroll/page.
- Interactive 8 FPS, diff-line repaint không clear-before-draw; chừa dòng cuối chống auto-scroll, compact/auto-page theo kích thước.
- Có `make preview-test-ui SSDS=N FRAMES=N` để xem UI không chạy engine/image hay ghi SSD.
- Chuẩn bị widget registry cho CPU–RAM/VGA.

Gate: snapshot/PTY tests ở nhiều kích thước terminal với 1/2/4 SSD; không dòng tràn, unbound state hoặc full-screen flicker.

## M8 — Tích hợp module vào image DIT CPU–RAM dùng chung

Trạng thái: `implemented (extension + image hook)` ngày 2026-09-05; chưa build/boot shared image.

- Dùng `/home/hoang/DIT-01` làm nguồn duy nhất của Ubuntu Noble/HWE rootfs và pipeline image.
- Đóng gói binary, profile, launcher/UI hook và module manifest/hash của NVMe thành extension payload.
- Stage extension qua hook tổng quát trước khi đóng SquashFS; không fork script rootfs, GRUB, partition hoặc burn USB.
- Tái sử dụng `nvme-cli`, `smartmontools`, `pciutils`, firmware/kernel modules, `toram`, UEFI/Legacy và `DIT-RESULTS` hiện có.
- Giữ nguyên cơ chế chặn boot/results/internal system disks; kết quả NVMe nằm trong namespace riêng trên results partition.
- Chạy lại toàn bộ verify/QEMU/regression gate của base image sau khi thêm module.

Gate: extension và image dùng chung reproducible; manifest/hash khớp; base CPU–RAM không regression; boot và inventory đạt trên máy Intel/AMD; boot/results disk luôn bị chặn.

## M9 — Campaign lỗi và độ bền phần mềm

Trạng thái: `partially implemented`; 14 fault classes và duration read-soak đã PASS bằng memory backend, process-kill/reset/hot-unplug/hardware soak còn planned.

- Mismatch classes, I/O errors, timeout/reset storm, fatal AER, thermal, budget exceeded.
- Device removal/reinsert, identity change, report disk unavailable.
- Long soak, memory pressure, Ctrl+C và unexpected termination.
- Xác minh không false PASS khi run chưa hoàn tất.
- Thực hiện backlog hardening RAW BOUNDED WRITE: scratch wizard, expert confirmation, duration/TBW read-soak và crash/reset recovery.

Gate: negative campaign đạt 100% lỗi bắt buộc; evidence và verdict đúng taxonomy.

### Backlog RAW BOUNDED WRITE sau TASK-F

Chi tiết: [Kế hoạch phát triển RAW BOUNDED WRITE](RAW_BOUNDED_WRITE_PLAN.md).

- `RBW-1`: scratch-partition wizard và range/LBA map, không tự mount/unmount.
- `RBW-2`: Expert/Destructive UI với stable-ID + câu xác nhận hai lớp; default image vẫn tắt raw UI.
- `RBW-3`: khi hết write budget, chuyển sang read/reread verify để đủ duration mà không tăng TBW.
- `RBW-4`: process-kill/reset/hot-unplug recovery evidence, không auto-resume sau reboot.
- `RBW-5`: hardware matrix phải đạt trước khi bật build flag `expert-raw-ui`.

Gate tổng: không vượt range/write budget, không false PASS duration, không raw multi-device và không mở `O_RDWR` trước toàn bộ confirmation.

## M10 — Hardware validation matrix

Trạng thái: `planned`.

- Intel và AMD; PCIe Gen3/4/5 nếu có.
- Một và hai NVMe chạy riêng/đồng thời.
- Known-good và known-fault/fixture-controlled drive.
- Cùng ổ qua nhiều slot; hai ổ known-good qua cùng slot.
- Link width/speed, thermal/throttling, power/reset behavior.
- Profile 30 phút, 60 phút và soak dài theo write budget.

Gate: lưu manifest, log, report và verdict có người xác nhận. Chỉ các cấu hình đã chạy mới mang trạng thái `validated`.

## M11 — Ghép CPU–RAM–NVMe–VGA

Trạng thái: `implemented` ngày 2026-09-05; module GPU–VRAM đã tiến đến GPU-D/Vulkan ngày 2026-09-06, multi-GPU và release validation tiếp tục ở GPU-E/F.

- Adapter CPU–RAM phát event/schema chung.
- NVMe module chạy nguyên engine độc lập trong orchestrator.
- Placeholder/adapter contract cho VGA.
- Resource arbitration CPU/RAM/PCIe/power/thermal.
- UI section registry và aggregate verdict.
- Build binary standalone từng module; mọi boot flavor/combined image dùng cùng pipeline `/home/hoang/DIT-01` và release manifest hợp nhất.

Gate: standalone và combined cho cùng module verdict tương đương khi cùng plan; lỗi module nào được attribution module đó.

## M12 — Release

Trạng thái: `implemented (development artifact)` ngày 2026-09-05; release-candidate hardware soak/signing pending.

- Lock dependency, SBOM/checksum, reproducible artifact.
- Version/migration policy cho event/report/recipe.
- Operator manual, recovery và support bundle.
- Release candidate soak và ký artifact khi hạ tầng sẵn sàng.

Gate: acceptance suite đạt, artifacts kiểm checksum được, không còn blocker safety.
