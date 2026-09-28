# TASK-A — Nền tảng và engine mô phỏng

## Phạm vi đã triển khai

- Cargo workspace gồm `dit-contracts`, `dit-source`, `nvme-backend-file`, `nvme-engine` và `nvme-cli`.
- Event/report schema v1 với run/module/device ID, sequence, timestamp, progress, failure và verdict.
- Block 4 KiB chứa seed, generation, device hash, LBA, stripe metadata, payload digest và independent tag.
- Exact-byte rebuild/compare; golden SHA-256 vector khóa format v1.
- Stripe 16 x 4 KiB và transfer recipe động.
- Memory backend và regular-file backend. TASK-A không chứa block-device backend.
- Fault injection: bit flip, stale generation, swapped LBA, duplicate block, short read/write, I/O error, timeout và device disappearance.
- CLI chạy simulator, xuất JSONL events/final JSON và chạy full negative campaign.

## Duration và coverage

Recipe tối thiểu luôn có 4 KiB, 64 KiB, 128 KiB và 1 MiB. Duration dùng làm planning horizon:

- dưới 3 phút: 1 seed slot, tối thiểu 4 transfer sizes;
- 30 phút: 10 seed slots và 10 transfer sizes;
- 60 phút: 20 seed slots và 20 transfer sizes.

Trong TASK-A, số operations vẫn là budget hữu hạn để simulator hoàn tất nhanh; backend ổ thật và enforcement theo wall-clock thuộc TASK-C. CLI mặc định dùng bốn operations cho mỗi giây planning, hoặc nhận `--operations` rõ ràng.

## Chạy kiểm tra

```bash
cd /home/hoang/DIT-01
make task-a-check
```

Chạy riêng:

```bash
cargo run -p nvme-cli -- campaign --operations 16
cargo run -p nvme-cli -- simulate --duration-seconds 60 --operations 16
```

Simulator mặc định tạo một regular file duy nhất trong `/tmp` và xóa nó khi kết thúc. Có thể chỉ định `--file`, nhưng mọi path đã tồn tại đều bị từ chối và không còn tùy chọn overwrite. Symlink và block device luôn bị backend TASK-A từ chối.

## Giới hạn có chủ ý

- Chưa inventory NVMe/SMART/PCIe: TASK-B.
- Chưa O_DIRECT, FUA/flush semantics cho block device hoặc ghi scratch NVMe: TASK-C.
- Chưa scheduler nhiều SSD và UI động: TASK-D.
- Trạng thái hiện tại là `implemented`, không phải `validated` trên hardware matrix.
