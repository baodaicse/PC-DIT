# CPU-DIT-01 Configuration Validation Record

## Release identity

```text
matrix_id:
config_id:
image_id:
image_sha256:
validation_start_utc:
validation_end_utc:
operator:
reviewer:
```

## Hardware identity

```text
manufacturer/model:
cpu:
microcode:
mainboard/revision:
bios_uefi_version:
boot_mode:
secure_boot:
ram_total:
dimm_part_numbers:
dimm_slots:
ram_speed_and_timings:
ecc:
numa_nodes:
psu:
cooling:
ambient_temperature:
overclock_undervolt_xmp_expo:
usb_model_and_size:
```

## Protocol checklist

| Gate | Evidence/session IDs | Result | Notes |
|---|---|---|---|
| P0 inventory/boot isolation | | PENDING | |
| P1 Safe cold | | PENDING | |
| P2 Full cold | | PENDING | |
| P2 Full warm 1 | | PENDING | |
| P2 Full warm 2 | | PENDING | |
| P3 soak ≥8 giờ và ≥3 Full | | PENDING | |
| P4 three power cycles/persistence | | PENDING | |
| P5 release negative campaign | | PENDING/N/A | Có thể dẫn evidence AMD/Intel dùng chung |
| P6 automated session validation | | PENDING | |

## Coverage summary

| Session | Profile | MemAvailable | Requested | Exact verified | Rounds | Peak °C | EDAC CE/UE | Result |
|---|---|---:|---:|---:|---:|---:|---|---|
| | | | | | | | | |

## Sensor/driver availability

```text
temperature:
cpu_package_power:
power_source:
edac:
throttle_counters:
machine_check:
important_loaded_modules:
unavailable_notes:
external_temperature_monitor_if_used:
```

CPU power `N/A` không tự làm config FAIL. Temperature phải có sensor hoặc nguồn
giám sát độc lập cho soak.

## Incidents và rerun

Liệt kê mọi FAIL, INVALID, reboot, hang, thermal stop, corrected/uncorrected ECC,
report thiếu hoặc thay đổi BIOS/DIMM. Không xóa incident đã được khắc phục.

```text
incident_id:
session_id:
symptom:
diagnostic_classification:
resolution:
rerun_session_id:
```

## Evidence integrity

```text
evidence_root:
hashes_sha256_file:
session_validator_output:
backup_location:
```

## Kết luận

```text
config_status: NOT_VALIDATED
scope: Exact hardware/BIOS/RAM configuration above only
open_limitations:
operator_signoff/date:
reviewer_signoff/date:
```

Chỉ dùng `CONFIG_VALIDATED` sau khi mọi gate bắt buộc PASS và reviewer xác minh
hash/report gốc. Một cấu hình thay CPU, BIOS, DIMM, slot hoặc memory tuning là
config revision mới.
