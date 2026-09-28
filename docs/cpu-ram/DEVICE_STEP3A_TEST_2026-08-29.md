# CPU-DIT-01 Bước 3A — Device bounded test 2026-08-29

## Phạm vi

Đây là kiểm thử bounded trên máy phát triển, không phải xác nhận USB/hardware
matrix. Binary debug chạy target 64 MiB dưới Linux hiện tại để kích hoạt toàn bộ
release window recipe với hai arena 32 MiB.

```text
memory target       67,108,864 bytes
arenas              2 x 32 MiB
workers             2
minimum rounds      2
minimum duration    3 seconds
schema              6
status              PASS
elapsed             8,245 ms
session seed        0xdf7471af7848b36c (linux_random)
round seeds         0x3593bc643c3ac55e, 0x5ef7f30eec381d81
```

## Gate Bước 3A

- Hai complete round đều `mandatory_recipe_applicable=true` và
  `mandatory_recipe_met=true`.
- Mỗi round có đủ 12 window size class từ 4.093 byte đến 32 MiB và đủ 8 pattern
  family.
- `no_gaps`, `no_overlaps`, `tail_verified` đều true.
- Tổng `size_class_verified_bytes` của mỗi round đúng 67.108.864 byte.
- Plan digest hai seed khác nhau:
  `cdff25cea5e9afefc9699cf67d12590420c6ea7e0d38e0a138b10bbfc7fac6d9` và
  `cee3994844872f89f36888b124da6fc4cb51ee17ab8a4d104a9256ebc2a880c4`.

Evidence local:

- `build/step3a-device/system-report.json`
- `build/step3a-device/window-summary.json`
- `build/step3a-device/console.log`

## Kết luận

Bước 3A là `implemented` và bounded device run PASS. Chưa gọi là `validated` cho
đến khi Safe/Full 30/60 phút chạy từ USB trên các cấu hình RAM/CPU/NUMA trong
hardware matrix.
