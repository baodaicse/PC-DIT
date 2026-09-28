# TASK-D — Multi-NVMe và UI động

## Trạng thái

TASK-D đã `implemented` ngày 2026-09-05. Automated scheduler/UI gates và host read-only inventory đạt. Chưa đánh dấu `validated` trên hai NVMe thật vì máy phát triển hiện chỉ có một NVMe chứa hệ điều hành.

## Scheduler nhiều thiết bị

Crate `nvme-scheduler` nhận danh sách backend cùng `RunConfig` và tạo worker riêng cho từng SSD.

- Mọi worker trong một batch dùng start barrier chung.
- `max_parallel_workers=1` chạy tuần tự; giá trị lớn hơn chạy đồng thời nhưng vẫn giới hạn áp lực CPU.
- Event của mỗi SSD giữ `device_id` và sequence riêng, sau đó được coordinator serialize.
- Report giữ `DeviceOutcome` độc lập; một SSD FAIL không đổi hai SSD PASS còn lại.
- Worker panic/engine error trở thành outcome `INVALID`, không làm mất report của worker khác.
- Aggregate verdict theo thứ tự an toàn: systemic/worker error → INVALID; có device FAIL → FAIL; tất cả PASS → PASS.
- CLI simulator có hard aggregate memory budget trước khi cấp buffer.

## Chạy nhiều SSD có dữ liệu

`run-preserving-multi` nối scheduler với direct-file backend của TASK-C. Selection plan chứa đúng tập SSD cần chạy; mỗi SSD nhận một thư mục được mount trực tiếp từ namespace/partition tương ứng.

```bash
cpu-nvme-dit run-preserving-multi \
  --selection selection.json \
  --target 1=/mnt/ssd-one/dit \
  --target 2=/mnt/ssd-two/dit \
  --confirm-device-id 'STABLE_ID_SSD_1' \
  --confirm-device-id 'STABLE_ID_SSD_2' \
  --capacity-mib 8192 \
  --write-budget-mib 2048 \
  --reserve-free-mib 4096 \
  --duration-seconds 1800 \
  --max-workers 2 \
  --cpu-ram-preflight cpu-ram-preflight.json \
  --events events.jsonl \
  --report report.json
```

Safety gate yêu cầu alias-directory map phủ đúng toàn bộ selection và tập stable ID xác nhận khớp tuyệt đối. Mỗi file test dùng `create_new`, free-space reserve, `O_DIRECT | O_DSYNC`, write budget và cleanup. Nếu chuẩn bị một target thất bại, mọi file đã tạo ở các target trước được đóng/xóa trước khi trả lỗi.

Không có multi-device raw command trong TASK-D. Raw destructive vẫn chỉ chạy từng scratch target qua `run-bounded` để tránh một xác nhận có thể phá nhiều ổ.

## UI động

Crate `dit-ui-core` tiêu thụ `EventEnvelope`; engine/scheduler không phụ thuộc terminal.

- Device picker hiển thị alias, device node, model, dung lượng, serial, kernel driver, PCIe link và safety mode.
- Header giữ hai vùng `SYSTEM HARDWARE` và `LIVE UTILIZATION`; CPU model, RAM, board lấy từ procfs/sysfs, CPU/RAM usage sample mỗi 250 ms.
- Dashboard ngày 2026-09-06 theo bố cục CPU–RAM: hai khung Hardware/Live Utilization nằm cạnh nhau, bên dưới là `TESTING / PROCESS INFORMATION`.
- Mỗi SSD thêm đúng một hàng speed vào Live Utilization và một hàng operation-progress/fail vào Process; khung phía trên tăng chiều cao theo số SSD hiển thị, không fix cứng 1/2 ổ.
- Dashboard từ 110 cột × 30 dòng; compact trên màn hình nhỏ, tự chuyển trang 5 giây khi không đủ chiều cao. Trang cuối tính đúng page count, tổng completed/fail vẫn bao gồm mọi SSD.
- Kích thước được đọc lại mỗi frame nên resize không dùng tọa độ cố định.
- Màu cyan/green/yellow/red/gray theo running/pass/warning/fail/idle.
- UI interactive mặc định 8 FPS; cấu hình 1–20 FPS theo lệnh chạy.
- Renderer chỉ ghi dòng thay đổi, gom output một lần/frame, không clear trước repaint; clear-line chỉ dùng cho dòng cũ cần dọn sau resize. Không newline trong frame và chừa dòng cuối terminal để tránh auto-scroll.
- Cursor được khôi phục khi kết thúc hoặc renderer bị drop.

Chạy demo an toàn bằng memory backend:

Xem riêng UI bằng số liệu tổng hợp giả, không engine/preflight/inventory/ghi SSD và không image:

```bash
make preview-test-ui SSDS=2
make preview-test-ui SSDS=4 FRAMES=80
```

`FRAMES=80` phát khoảng 10 giây preview. Title có `UI PREVIEW ONLY`; số liệu không phải kết quả hardware. Sensor chưa được NVMe sampler cung cấp vẫn hiển thị N/A. Speed meter biểu diễn R+W hiện tại so với đỉnh R+W quan sát trong phiên; không phải SSD busy%. Progress là số operation đã hoàn tất, TIME là elapsed/stop-limit, không hứa chạy đủ duration.

Chạy engine memory backend (có tải CPU nhẹ):

```bash
cargo run -p nvme-cli -- multi-simulate \
  --devices 4 \
  --duration-seconds 60 \
  --operations 512 \
  --capacity-mib 64 \
  --memory-budget-mib 512 \
  --max-workers 2 \
  --report multi.report.json
```

## Bằng chứng automated

```bash
cd /home/hoang/DIT-01
make task-d-check
```

Gate hiện tại:

- rustfmt + Clippy strict: PASS;
- 40 workspace tests: PASS;
- positive scheduler với 1/2/4 device: PASS;
- three-device negative campaign: SSD 2 FAIL, SSD 1 và 3 vẫn PASS, aggregate FAIL;
- sequential và bounded-parallel scheduler: PASS;
- expanded/compact/paged layouts, picker, resize dimensions và no-full-clear renderer: PASS;
- host inventory: read-only PASS.

Hardware validation còn cần hai SSD thật với filesystem test riêng, chạy tuần tự và đồng thời rồi lưu SMART/AER/report trước/sau. Chỉ khi đó mới đổi TASK-D thành `validated` trên cấu hình tương ứng.
