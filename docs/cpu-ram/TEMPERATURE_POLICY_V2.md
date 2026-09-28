# CPU-DIT-01 Temperature Policy v2

## Kết quả

Từ report schema v7, giới hạn nhiệt là tùy chọn operator và mặc định `OFF`.
Telemetry nhiệt độ, CPU package Watt và throttle counter vẫn luôn được thu thập
dù giới hạn tắt.

Trong live UI:

```text
[T] TEMP LIMIT  OFF (temperature telemetry is still recorded)
```

Nhấn `T` trước khi bắt đầu Safe/Full để chuyển ON/OFF. Trạng thái được truyền vào
runner và lưu tại `safety.policy.temperature_limit_enabled` trong report.

CLI tương đương:

```bash
dit-cli system-run --enable-temperature-limit --max-temperature-c 95
```

Không truyền `--enable-temperature-limit` nghĩa là telemetry-only, không thermal
abort. Deadline, EDAC, OOM, execution và correctness gates vẫn hoạt động.

## Sensor được phép kích hoạt safety

Khi ON, chỉ các nguồn CPU có mapping đáng tin được dùng:

- Intel `coretemp` và `x86_pkg_temp`;
- AMD `k10temp`, `k8temp`, `zenpower`;
- CPU/SoC thermal zone chuẩn.

Super-I/O generic như `nct6798:AUXTIN*`, SYSTIN và CPUTIN vẫn nằm trong raw sensor
evidence nhưng không được phép dừng bài test. Quy tắc này sửa false positive trên
Intel i9-14900KF, nơi AUXTIN2/AUXTIN4 nhảy 22–34°C sang sentinel 127°C trong vài
trăm millisecond.

## Diagnostic safety

Thermal abort không tự chạy lại workload. Report ghi
`diagnostic.classification=skipped_temperature_safety_abort`. Người vận hành phải
kiểm tra cooling, BIOS power limit và raw sensor evidence trước khi chủ động chạy
lại.

## Bằng chứng regression

- Temperature limit OFF + synthetic 100°C: PASS, peak vẫn được ghi.
- Temperature limit ON + synthetic 100°C: FAIL, không diagnostic rerun.
- AUXTIN 127°C + coretemp 84°C: safety peak là 84°C.
- Rust workspace 51 tests, UI OFF/ON render và schema-v7 seed KAT: PASS.

Giới hạn OFF là lựa chọn vận hành, không phải xác nhận nhiệt độ an toàn. Khi tắt,
người dùng chịu trách nhiệm giám sát cooling và dừng nếu có dấu hiệu bất thường.
