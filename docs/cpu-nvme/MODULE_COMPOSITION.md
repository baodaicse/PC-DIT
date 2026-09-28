# Hợp đồng ghép CPU–RAM–NVMe–VGA

## Mục tiêu

DIT CPU–NVMe phải chạy được như binary độc lập, đồng thời không khóa thiết kế khiến bộ test tổng hợp phải gọi shell hoặc scrape màn hình. Điểm ghép là API/event có phiên bản, không phải phụ thuộc chéo giữa các engine.

## Module lifecycle

Mỗi module triển khai cùng lifecycle logic:

```text
inventory()
validate_plan(plan)
start(plan, event_sink)
request_stop(reason)
finalize_report()
```

Implementation Rust có thể dùng trait/async channel, nhưng schema wire phải độc lập ngôn ngữ để adapter của DIT CPU–RAM hiện tại tham gia được.

## Event schema tối thiểu

Mỗi event có:

- `schema_version`, `run_id`, `module_id`;
- `device_id` khi event thuộc thiết bị;
- monotonic timestamp và wall-clock timestamp;
- sequence number để phát hiện event thiếu/đảo thứ tự;
- payload có kiểu rõ ràng.

Các event chuẩn: `hello`, `inventory`, `selection`, `phase`, `progress`, `throughput`, `telemetry`, `failure`, `warning`, `stopping`, `completed`.

`throughput` của NVMe gồm current/average/peak read và write; `progress` gồm completed operations, verified bytes, target/duration và failure count. UI không đọc trực tiếp log text của engine.

## Verdict tổng hợp

Mỗi module và mỗi device trả một trong:

- `PASS`
- `FAIL`
- `ABORTED`
- `INVALID`
- `NOT_RUN`

Orchestrator giữ nguyên verdict con. Kết quả tổng chỉ PASS khi tất cả module bắt buộc PASS. `INVALID` không được chuyển thành FAIL phần cứng cụ thể.

## Quản lý tài nguyên khi chạy chung

Orchestrator phân bổ thay vì để các module tự chiếm hết:

- CPU core/worker quota;
- RAM buffer quota;
- PCIe/SSD queue depth;
- power/thermal policy;
- sensor polling và report writer chung.

Do đó standalone NVMe phải nhận resource budget từ plan, không hard-code dùng toàn CPU/RAM. Tương lai build có thể chọn feature/module `cpu-ram`, `nvme`, `vga`, nhưng runtime vẫn dựa trên cùng event contract.

## Đóng gói vào image dùng chung

Standalone ở đây có nghĩa engine/binary có thể chạy và kiểm thử độc lập, không có nghĩa duy trì một Ubuntu image riêng. Bản boot USB được tạo bởi pipeline `/home/hoang/DIT-01`; NVMe tham gia dưới dạng extension payload có manifest/hash. Base image, driver, bootloader, partition layout và burn tool chỉ có một chủ sở hữu. Chi tiết tại [Tích hợp image DIT dùng chung](SHARED_IMAGE_INTEGRATION.md).

## Quy tắc tương thích

- Thêm field theo kiểu backward-compatible trong cùng major schema.
- Thay nghĩa/xóa field phải tăng major.
- UI phải bỏ qua event/field chưa biết và hiện cảnh báo schema nếu không thể diễn giải.
- Golden event fixtures được dùng chung giữa engine, UI và orchestrator.
