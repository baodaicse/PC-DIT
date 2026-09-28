# CPU-DIT-01 UI/Sensor Device Test — 2026-08-29

## Thiết bị và mục tiêu

Kiểm backend sensor, UI và một lượt CPU/RAM DIT giới hạn trên máy phát triển:

```text
CPU       AMD Ryzen 7 3700X 8-Core Processor
Topology  8 core / 16 thread, 1 NUMA node
RAM       khoảng 7.63 GiB Linux-visible
Kernel    Ubuntu generic 7.0.0-30-generic
```

## Sensor evidence

`k10temp` xuất nhiệt độ nhưng không có `power*_input`. Powercap package energy
counter có thể đọc khi chạy root; backend lấy hai mẫu và báo CPU power thực tế
khoảng 30–45 W trong các lượt kiểm. Ví dụ system report cuối ghi:

```text
source       /sys/class/powercap/intel-rapl:0/energy_uj
label        package-0
measurement  powercap_energy_delta
power        45,432 mW
interval     259 ms
```

Lệnh không root báo Power `N/A` vì energy counter trên máy này chỉ root đọc được.
Đây không phải lỗi của UI; live service chạy root nên đọc được counter. TUI render
root đã hiển thị `57.8 C | 34.8 W`.

## Functional run trên thiết bị

Release binary chạy 64 MiB, 4 worker, 1 round:

```text
schema_version          3
status                  PASS
allocated               67,108,864 bytes
exact_verified          67,108,864 bytes
temperature available   true
CPU power available     true
safety violation        false
```

Report: `build/ui-sensor-device-report.json`. UI previews được giữ ở
`build/cpu-dit-ui-preview.ansi` và `build/cpu-dit-ui-root-preview.ansi`.

## Quality gate và image regression

```text
cargo fmt --all -- --check                            PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                                PASS (41 tests)
cargo build --release --workspace                     PASS
driver/UI/runner bash syntax                           PASS
final image SHA-256                                    PASS
FAT filesystems                                        PASS
QEMU BIOS UI auto-Safe + DIT                           PASS
QEMU UEFI UI auto-Safe + DIT                           PASS
```

Final image hiện tại sau duration/coverage/dashboard regression:

```text
image_id  CPU-DIT-01-NOBLE-20260829-dda19bb9474c
size      2,556,428,288 bytes
sha256    f2193a227f21dde2515ee65de30ee579fd68705ee7868eff7a460876ce382133
```

Trong QEMU không có CPU package power counter, nên UI/report ghi unavailable;
điều này xác minh nhánh `N/A`, không xác minh độ chính xác của Watt.

Dashboard v3 còn được render-test ở 160x45 và compact 100x30. Device sanity run
64 MiB/2 worker/minimum 3 giây PASS schema v4: exact verified 67,108,864 byte,
duration gate đạt sau 4.708 giây, peak 68.25°C và không có safety violation.
Report nằm tại `build/ui-dashboard-device-report.json`.

## Kết luận

UI, sensor backend và driver packaging đạt `implemented` trên thiết bị hiện tại.
Chưa nâng thành `validated`: cần boot USB vật lý và chạy hardware matrix Intel/AMD,
32/64 GB, ECC/non-ECC cùng các mainboard sensor khác nhau.
