# Kiến trúc

## Nguyên tắc

1. Engine NVMe độc lập với DIT CPU–RAM.
2. UI chỉ đọc event chuẩn, không biết chi tiết thuật toán engine.
3. Mọi thao tác ghi phải đi qua safety gate tập trung.
4. Kết quả theo thiết bị là nguồn sự thật; tổng hợp chỉ là lớp ngoài.
5. Hardware backend có thể thay bằng file-backed/fake backend để kiểm thử không phá hủy.
6. Image boot dùng chung do `/home/hoang/DIT-01` sở hữu; repository NVMe chỉ cung cấp extension payload có version/hash.

## Cấu trúc workspace dự kiến

```text
crates/
  dit-contracts/       # schema event, module/device ID, verdict
  dit-source/          # seed, block 4 KiB, stripe, tag và golden vectors
  dit-ui-core/         # widget/layout terminal động
  nvme-inventory/      # sysfs, nvme-cli, SMART, PCIe topology
  nvme-engine/         # scheduler, I/O recipe, verify, fault handling
  nvme-scheduler/      # per-device workers, start barrier, aggregate report
  nvme-backend-file/   # simulator/fault injection
  nvme-backend-linux/  # block device, O_DIRECT, flush/FUA
  nvme-cli/            # ứng dụng standalone
  dit-orchestrator/    # module registry, report adapters, resource budget, control center
```

Các crate common chỉ chứa hợp đồng và primitive chung. `nvme-engine` không import engine CPU–RAM. Khi cần bộ tổng hợp, adapter của từng engine cùng phát event qua `dit-contracts`.

`dit-orchestrator` không import engine CPU–RAM, NVMe hoặc GPU. Nó phát hiện launcher, thực thi lifecycle tuần tự, chuyển native report sang `ModuleResult`, giữ verdict theo module và chỉ tính aggregate ở lớp ngoài. Lựa chọn số 3 chạy GPU–VRAM; đường mặc định dùng Vulkan GPU-B/C/D, còn GPU-A CPU mock chỉ dùng khi gọi rõ `--backend mock`.

Việc dùng chung boot image không thay đổi ranh giới engine. Pipeline CPU–RAM stage binary/config/manifest NVMe qua extension hook trước khi đóng SquashFS; NVMe không fork Ubuntu rootfs, bootloader, partition layout hoặc burn tool. Xem [Tích hợp image DIT dùng chung](SHARED_IMAGE_INTEGRATION.md).

## Luồng chạy

```text
Inventory -> Safety classification -> Operator SSD selection
          -> Plan validation -> SMART/AER snapshot before
          -> Per-device workers -> Verify/events/telemetry
          -> SMART/AER snapshot after -> Per-device verdict -> Aggregate report
```

## Thành phần chính

### Inventory

Đọc `/sys`, `lsblk`, `nvme-cli`, `smartctl` và PCI topology. Chuẩn hóa model, serial, firmware, namespace, capacity, sector sizes, BDF, root port, link speed/width, mount/holder/swap và quan hệ với boot/results USB.

### Safety gate

Nhận operation plan và inventory snapshot, từ chối ổ hệ thống, USB boot/results, mounted filesystem, swap, holder/RAID/LVM không rõ, thiết bị đổi identity hoặc vượt write budget. Engine Linux không có đường ghi nào bỏ qua gate này.

### Source/verification

Sinh dữ liệu xác định được từ session seed nhưng thay đổi theo generation/LBA. Expected value được tái tạo độc lập khi đọc, tránh dùng cùng buffer làm expected và actual. Golden vectors và implementation tham chiếu bảo vệ lỗi thuật toán chung.

### Scheduler

Tạo recipe theo duration, capacity, mode và write budget. Mỗi SSD có executor riêng; coordinator giữ mốc start/stop, giới hạn RAM, CPU cores, queue depth và phát tiến độ.

### Telemetry

Tách khỏi hot path. Sample tốc độ/counters khoảng 250–500 ms, sensor/SMART khoảng 1 giây hoặc chậm hơn phù hợp thiết bị. UI render in-place 2–4 FPS, không clear toàn màn hình.

### Reporter

Ghi event JSONL trong khi chạy và summary JSON sau khi finalize. Flush định kỳ để vẫn còn bằng chứng khi mất thiết bị hoặc người dùng Ctrl+C.

## Lỗi và verdict

- Recoverable compare mismatch: tăng fail counter, giữ first-N evidence, có thể tiếp tục theo policy.
- I/O error hoặc short I/O: ghi đủ opcode/LBA/length/errno và xử lý theo budget.
- Fatal: device disappearance, reset storm, I/O hang, fatal AER, nhiệt vượt limit đã bật hoặc write budget bị vượt; dừng thiết bị liên quan an toàn.
- Engine/process crash: `INVALID`, không giả làm SSD FAIL. Không có CPU–RAM evidence thì mismatch dùng attribution `CPU_SSD_PATH_UNATTRIBUTED`.
