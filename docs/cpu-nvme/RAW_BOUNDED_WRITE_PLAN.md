# Kế hoạch phát triển RAW BOUNDED WRITE

## Trạng thái hiện tại

Backend raw bounded một thiết bị đã `implemented` ở TASK-C và chỉ được mở qua CLI chuyên gia. Nó yêu cầu scratch target, range 4 KiB-aligned, write budget, stable-ID revalidation, xác nhận target ID và `--acknowledge-data-loss`. CPU–RAM preflight là evidence attribution tùy chọn, không phải quyền mở raw target.

`bounded` chỉ giới hạn phạm vi LBA và tổng host writes. Dữ liệu nằm trong vùng đó sẽ bị ghi đè; FTL/wear-leveling cũng có thể ghi physical NAND ngoài ánh xạ trực tiếp mà host nhìn thấy. Vì vậy chế độ này không được gắn nhãn data-preserving.

## Backlog triển khai

### RBW-1 — Scratch-partition wizard

Trạng thái: `planned`.

- Chỉ liệt kê partition/namespace đủ điều kiện; ẩn root, boot, `DIT-RESULTS`, mounted, swap, holder, RAID/LVM/encryption không rõ.
- Hiển thị model, serial, firmware, driver, PCIe BDF/link và stable target ID.
- Ưu tiên partition scratch riêng; cảnh báo đặc biệt khi target là whole namespace.
- Vẽ range map theo byte/LBA, cảnh báo GPT/header/footer nếu người dùng chọn whole namespace.
- Không tự unmount và không tự sửa partition table.

Gate: fixture với root/boot/results/mount/swap/holder không bao giờ xuất hiện như raw candidate; device reorder không đổi target.

### RBW-2 — Expert UI và xác nhận hai lớp

Trạng thái: `planned`.

- Raw mode nằm trong `Advanced / Destructive`, không xuất hiện ở menu mặc định.
- Màn hình review riêng cho range start/size, target serial/ID, write budget và ước tính phần trăm TBW.
- Yêu cầu nhập lại stable target ID và câu xác nhận phá hủy; timeout/cancel quay về mà không mở device.
- Mỗi phiên UI chỉ mở một raw target. Multi-device raw tiếp tục bị cấm cho đến khi hardware safety matrix đạt.

Gate: mọi đường thiếu một trong hai confirmation trả `ABORTED/INVALID` trước `open(O_RDWR)`.

### RBW-3 — Duration/TBW recipe

Trạng thái: `planned`.

- Tách rõ `planning horizon`, `hard stop`, operation target và write budget trên UI/report.
- Sau khi dùng hết write budget, chuyển sang read/reread/verify soak trên dataset đã ghi để tiếp tục đủ 30/60 phút mà không tăng host writes.
- Xoay seed/generation/block-size 4 KiB, 64 KiB, 128 KiB và 1 MiB trong giới hạn dataset có thể tái tạo chính xác.
- Báo riêng host bytes written, SMART data-units-written delta và write-amplification inference; không đồng nhất host budget với NAND writes.
- Không báo PASS duration nếu chỉ hoàn thành nhanh write budget rồi thoát.

Gate: fake clock/backend chứng minh đủ duration, không vượt một byte host-write budget và mọi reread vẫn có independent expected data.

### RBW-4 — Crash, reset và recovery evidence

Trạng thái: `planned`.

- Flush JSONL/progress định kỳ trước/sau mỗi destructive phase.
- Process-kill, Ctrl+C, timeout, controller reset, hot-unplug và report-partition unavailable campaign.
- Không tự resume raw write sau reboot; session cũ chỉ được mở read-only để thu hồi evidence.
- Identity/link thay đổi giữa plan và open hoặc giữa các phase tạo `INVALID`, không tiếp tục ghi.

Gate: mọi interrupted run giữ first/last confirmed range, submitted host bytes và stop reason; không có false PASS.

### RBW-5 — Hardware validation trước khi mở UI

Trạng thái: `planned`.

- Intel/AMD, NVMe Gen3/4/5 và nhiều controller/firmware.
- Scratch partition ở đầu/giữa/cuối device, range không tròn erase-block và nhiều alignment.
- Known-good/fault-controlled SSD, đổi chéo drive/slot và ép link generation.
- SMART/AER/thermal trước-sau, 30/60 phút và soak dài trong TBW budget.

Gate: chỉ khi RBW-1 đến RBW-4 đạt và matrix được review mới cho phép build flag `expert-raw-ui`; default image vẫn tắt.

## Nguyên tắc release

- `DATA_PRESERVING_FILE` tiếp tục là chế độ UI mặc định.
- Raw bounded không bao giờ tự chạy sau boot hoặc sau inventory.
- Full destructive là milestone khác và không được suy ra từ việc raw bounded đã validated.
- Mỗi thay đổi range/budget/confirmation tạo recipe/schema revision và phải chạy lại negative campaign.
