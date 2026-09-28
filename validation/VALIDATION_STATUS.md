# CPU-DIT-01 Release Validation Status

```text
release_status  NOT_VALIDATED
image_id        CPU-DIT-01-NOBLE-20260901-843d7743c91a
sha256          115c3afe2318285ef633827e725f33cbeb3916b72f8bc537b1a4d69916205395
updated_utc     2026-09-01
```

## Bằng chứng đã có

- 51 automated tests, rustfmt, Clippy và release build: PASS.
- QEMU legacy BIOS và x86_64 UEFI qua UI auto-Safe: PASS.
- Image schema v7 qua BIOS/UEFI với Temperature Policy v2 mặc định OFF,
  Linux-random seed và Pattern/Window v1: PASS.
- Device duration run trên AMD Ryzen 7 3700X: 64 MiB, minimum 3 giây, 16 round,
  schema v4 và duration/coverage gates PASS (evidence lịch sử trước seed rotation).
- Device seed rotation v1: random 64 MiB/2 round PASS; replay cùng seed cho
  cùng commitment, round seed và digest.
- Device bounded Pattern/Window v1: 64 MiB/two random seed, mỗi round đủ 12 size
  class và 8 pattern family, không gap/overlap, verified-size sum đúng target.
- CPU temperature và package-power fallback trên máy development: PASS.
- AMD development negative campaign gồm RAM, SIMD, worker crash, timeout,
  temperature và EDAC UE injection: PASS; evidence ở
  `build/validation-negative-campaign/`.
- Bước 3B compute-oracle (dense FMA/branch-predictor/AES-256), độc lập và chạy
  đồng thời với route RAM: 49 tests PASS gồm fault injection cross-worker.
- `toram` (boot chạy hoàn toàn từ RAM) và phát hiện `DIT-RESULTS` độc lập với
  `/run/live/medium`: QEMU legacy BIOS + UEFI PASS trên image này.
- Rút USB giữa chừng (mô phỏng bằng QEMU `device_del`) không còn làm phiên test
  gián đoạn nhờ `toram`; dashboard tự full-redraw mỗi ~20 giây
  (`cpu-dit-ui.sh`) để tự phục hồi khỏi nhiễu console do kernel in lỗi ổ đĩa.
  QEMU legacy BIOS + UEFI PASS trên image này.

Các bằng chứng trên giữ trạng thái `implemented`; chúng chưa hoàn thành physical
USB protocol và hardware matrix trong `MATRIX.csv`.

## Điều kiện còn thiếu

- Ghi và boot USB vật lý.
- Safe/Full repeatability, soak 8 giờ và power-cycle persistence.
- Ma trận Intel/AMD 16/32/64 GB non-ECC, ECC 64/128 GB và multi-NUMA.
- Negative campaign trên Intel với đúng release binary; AMD development campaign
  đã PASS nhưng vẫn cần được gắn vào reviewed release record.
- Run record, checksum evidence và reviewer cho từng cấu hình.

## Promotion log

Chưa có config nào đạt `CONFIG_VALIDATED`. Không đổi `release_status` trước khi
mọi dòng `required=yes` trong `MATRIX.csv` đạt và được review.
