# GPU-A — Data contract, CPU oracle và UI baseline

Ngày hoàn thành source/software gate: 2026-09-06.

GPU-A dựng nền kiểm chứng trước khi GPU-B nối Vulkan. `gpu-engine` tạo block `DITGPUA1`, xáo trộn thứ tự theo seed, join bằng block index, kiểm tra toàn payload và so matrix kernel với oracle độc lập. `gpu-cli` chạy campaign theo duration hoặc số generation hữu hạn, thu inventory/telemetry và ghi report JSON.

## Phạm vi kết quả

`PASS` ở A xác nhận data contract, golden, split/scatter/join, CPU matrix/oracle và report/UI. Hai field sau luôn false:

```json
{
  "hardware_tested": false,
  "vram_hardware_test_active": false
}
```

GPU-B thay mock transport bằng allocation và round-trip Vulkan; GPU-C thay CPU kernel bằng compute shader và giữ CPU oracle.

## Lệnh phát triển

```bash
cd /home/hoang/DIT-01
make gpu-a-check
cargo run -p gpu-cli -- inventory
cargo run -p gpu-cli -- run --generations 10 --blocks-per-generation 32 --no-ui
make preview-menu
```

Menu số 3 chạy `gpu-vram-dit run` với duration mặc định 60 giây và lưu report trước ở `/run/dit-suite`. Khi phân vùng DIT-RESULTS đã được xác thực, Control Center copy bundle cuối đúng một lần.

## UI và telemetry

Dashboard dùng renderer cập nhật tại chỗ 4 FPS, bố cục và màu meter giống họ UI CPU-RAM. Inventory ghép theo PCI BDF. Provider hiện có:

- NVIDIA proprietary: `nvidia-smi` với timeout 2 giây.
- AMD và driver có sysfs tương thích: `gpu_busy_percent`, `mem_info_vram_used/total`, hwmon temperature.
- Provider không có metric tương ứng: hiển thị N/A, không suy đoán.

Thanh `GPU USAGE` và `VRAM USAGE` là số liệu toàn thiết bị. Từ GPU-B sẽ bổ sung test allocation/budget/verified để phân biệt phần VRAM đang dùng bởi hệ thống với phần engine đã thực sự kiểm tra.
