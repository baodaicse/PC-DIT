# Kiểm thử và tiêu chí nghiệm thu

## Các lớp kiểm thử

### Unit/property/golden

- Generator và verifier 4 KiB/stripe.
- `DITDEEP2` matrix known-answer; kernel producer/readback và oracle u128 độc lập.
- Seed/generation/LBA/device binding.
- Range arithmetic, alignment và write-budget accounting.
- Stable ID normalization và schema compatibility.
- Layout calculation với terminal/device count khác nhau.

### Simulator campaign

- Positive run không mismatch.
- Bit flip ở header/payload/tag.
- Stale generation, wrong seed, wrong LBA, swapped/duplicated block.
- Short read/write, EIO, timeout, reset, disappearance.
- Producer compute, readback compute, reference, split/join và read-soak corruption.
- Read-soak đạt deadline mà host bytes written không tăng sau write budget.
- Counter/report chính xác khi dừng giữa chừng.

### Integration an toàn

- Root, mount, swap, boot USB, results partition, LVM/RAID holders bị chặn.
- Target reorder/hotplug không làm đổi ổ.
- Byte range và host-write budget không thể vượt.
- FULL_DESTRUCTIVE không khả dụng nếu chưa có explicit confirmation.

### PTY/UI

- 1, 2 và 4 SSD; rộng/hẹp, cao/thấp, resize trong lúc chạy.
- Mỗi SSD có đúng speed bar, identity, progress và fail count.
- 2–4 FPS in-place; không clear toàn màn hình, không crash khi thiếu metric.
- Thiết bị lỗi/idle/removed có màu và trạng thái đúng.

### Image dùng chung

- Build extension payload NVMe và xác minh module manifest/SHA-256.
- Stage module bằng pipeline `/home/hoang/DIT-01`; không có rootfs/image/burn implementation thứ hai trong repository NVMe.
- Chạy lại verify-image và QEMU BIOS/UEFI smoke hiện có của CPU–RAM.
- Xác minh base CPU–RAM vẫn khởi chạy và không regression sau khi thêm module.
- Sau boot, module NVMe hiện diện nhưng không tự mở ghi internal disk.
- `DIT-BOOT`, `DIT-RESULTS`, system disk và stable-ID safety gate vẫn được bảo vệ.
- Report ghi được base-image version, module version, schema và hash.

### Hardware matrix

- CPU/platform Intel và AMD.
- NVMe Gen3/4/5 theo phần cứng sẵn có.
- một ổ, hai ổ và đồng thời;
- cùng ổ qua nhiều slot, nhiều ổ qua cùng slot;
- known-good và fault-controlled device;
- 30 phút, 60 phút, soak; profile ghi tuân thủ TBW budget.

## PASS theo thiết bị

Một SSD chỉ PASS nếu:

- target được người dùng chọn và toàn bộ stable-ID/filesystem safety gates hợp lệ; CPU–RAM preflight là evidence attribution tùy chọn;
- đúng target và đúng operation plan;
- hoàn tất duration/recipe hoặc target work đã khai báo;
- bytes/operations/regions/seeds/generations đạt yêu cầu plan;
- mọi exact compare đạt;
- không có fatal I/O/AER/reset/SMART condition;
- không vượt write/temperature policy;
- final report đầy đủ và nhất quán với event log.

Speed thấp không tự động FAIL integrity. Nếu có performance specification riêng, verdict performance phải là trường riêng.

## Kết quả nhiều thiết bị

- Giữ verdict và evidence cho từng SSD.
- Aggregate PASS khi tất cả target bắt buộc PASS.
- Một SSD FAIL làm aggregate FAIL nhưng không đổi verdict PASS hợp lệ của SSD khác.
- Preflight/systemic engine error tạo `INVALID`, không quy kết đồng loạt SSD hỏng.
- Device không được chọn là `NOT_RUN`.

## Xác định drive hay slot

- Lỗi đi theo cùng ổ qua nhiều slot: nghiêng về drive/controller của ổ.
- Hai known-good drive cùng lỗi tại một slot: nghiêng về slot/lane/board/root complex/power.
- Gen5 lỗi nhưng ép Gen4 đạt: nghiêng về signal-integrity/link margin.

Đây là attribution dựa trên ma trận đổi chéo; report phải dùng `follows_drive`, `follows_slot` hoặc `unattributed`, không kết luận quá mức từ một lần chạy.

## Bằng chứng cần lưu

- base-image/build và module/engine/schema/recipe version cùng checksum;
- hardware inventory và stable IDs;
- seed/generation/region/block-size schedule;
- event log, mismatch samples và kernel log liên quan;
- SMART/PCIe/AER trước và sau;
- host read/write bytes và SMART data units delta;
- operator choice, mode, limits, start/stop reason;
- per-device và aggregate verdict.
