# ADR-0003: Telemetry, cooperative safety và evidence-only diagnosis

- Status: accepted
- Date: 2026-08-29

## Context

CPU/RAM load có thể làm lộ thermal, ECC, machine-check, OOM hoặc execution fault.
PASS không được chỉ dựa vào data compare nếu hardware/runtime event vi phạm policy.
Đồng thời tool không được tự quy lỗi cho DIMM hoặc CPU khi evidence chỉ chứng minh
một route thất bại.

## Decision

- Thu telemetry từ sysfs/cgroup và kernel journal bằng read-only interfaces.
- Ghi availability riêng; không thay sensor/counter thiếu bằng zero giả.
- Khóa semantics bằng `CPU-DIT-01-SAFETY-v1` và report schema v2.
- Dùng cooperative stop token trong worker để giữ được structured report.
- Diagnostic rerun giữ recipe và chỉ so failure fingerprint.
- Chỉ xuất classification route/system; component attribution cần evidence khác.
- Để hard process watchdog và privileged boot integration cho Bước 5.

## Consequences

Safety event, timeout và worker crash trong boundary không thể tạo PASS và vẫn có
report tái hiện. Tool chạy được không cần root, nhưng nguồn privileged hoặc driver
không có sẽ thành `unavailable`; vì vậy chưa thể tuyên bố ECC/MCE validation trên
mọi platform.
