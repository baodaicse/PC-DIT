# TASK-B — Inventory, lựa chọn thiết bị và safety gate

## Phạm vi đã triển khai

- Read-only scan trực tiếp từ sysfs/procfs; không mở namespace device node.
- Liệt kê controller và namespace `NVMe` theo alias `[1]`, `[2]`, ...
- Hiển thị `/dev` node, model, serial, firmware, kernel driver, controller và transport.
- Thu PCI BDF, upstream/root-port BDF, negotiated link speed/width và AER counters.
- Thu SMART bằng `nvme-cli` khi công cụ khả dụng; thiếu công cụ được báo `N/A`, không làm inventory thất bại.
- Nhận diện root filesystem, mount, swap, partitions, block holders và boot/results device được bảo vệ bổ sung.
- Safety classification tách `READ_ONLY` khỏi write candidate.
- Chọn một/nhiều/tất cả thiết bị và lưu selection plan bằng stable ID.
- Revalidate stable ID + device node để phát hiện reorder, hotplug, disappearance hoặc target thay đổi.
- Plan validator cho `READ_ONLY`, `BOUNDED_WRITE`, `FULL_DESTRUCTIVE`; Task-B không có write backend.

## Thông tin lựa chọn cho người dùng

Ví dụ một mục:

```text
[1] /dev/nvme0n1  INTEL SSDPEKKW010T8L  953.86 GiB
    Serial: ... | Firmware: ... | Namespace: nvme0n1
    Driver: nvme | Controller: nvme0 | Transport: pcie
    PCIe: 0000:01:00.0 via 0000:00:01.1 | Link: 8.0 GT/s PCIe x4
    Safety: PROTECTED / READ-ONLY: contains the running root filesystem ...
```

Alias chỉ để thao tác dễ dàng trong snapshot hiện tại. Selection plan không tin vào số thứ tự mà lưu serial/WWID/namespace/PCI identity trong `stable_id`.

## Lệnh sử dụng

```bash
cd /home/hoang/DIT-01

# Bảng dễ đọc; SMART tự dùng khi nvme-cli có sẵn
cargo run -p nvme-cli -- inventory

# Chọn ổ 1 và 2
cargo run -p nvme-cli -- select --devices 1,2 --output selection.json

# Chọn toàn bộ
cargo run -p nvme-cli -- select --devices all --output selection.json

# Kiểm tra target chưa thay đổi ngay trước bước chạy sau này
cargo run -p nvme-cli -- revalidate --plan selection.json

# Toàn bộ quality gate TASK-B
make task-b-check
```

Nếu bỏ `--devices` trong terminal tương tác, chương trình hiện prompt. Có thể lặp `--protected-device /dev/...` để đánh dấu boot/results device bổ sung.

## Safety semantics

- Ổ chứa root, mount, swap, partition hoặc holder vẫn có thể được chọn cho kế hoạch `READ_ONLY`.
- Các ổ đó không bao giờ là write candidate.
- `BOUNDED_WRITE` chỉ qua bước phân loại nếu mọi target là raw/unmounted/unheld và có stable identity; TASK-C vẫn phải xác nhận scratch range và write budget.
- `FULL_DESTRUCTIVE` cần exact stable-ID confirmation cho từng target; TASK-C chưa triển khai hoặc cấp quyền backend này.

## Kết quả trên máy phát triển

- Phát hiện 1 namespace: `/dev/nvme0n1`.
- Model: `INTEL SSDPEKKW010T8L`, driver: `nvme`, controller: `nvme0`.
- Link: `8.0 GT/s PCIe x4`, BDF `0000:01:00.0`, upstream `0000:00:01.1`.
- Safety: `PROTECTED / READ-ONLY` vì chứa `/`, `/boot/efi` và các partition hệ thống.
- Stable selection/revalidation: PASS.
- SMART host validation: chưa chạy vì máy hiện tại chưa cài `nvme-cli`; parser/availability behavior đã có automated tests.

TASK-B là `implemented`; đây chưa phải hardware-matrix `validated`.
