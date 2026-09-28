# TASK-C — Engine NVMe thật một thiết bị

## Trạng thái

TASK-C đã `implemented` ở mức code và automated/safety validation. Đường I/O trên NVMe thật chưa được đánh dấu `validated`, vì máy phát triển chỉ có `/dev/nvme0n1` đang chứa hệ điều hành và safety gate đã từ chối target này trước khi mở device node.

## Thành phần đã triển khai

- Backend contract chung `nvme-io` cho simulator và Linux backend.
- Linux backend chỉ nhận `BoundedWritePlan` đã kiểm tra.
- Mở target bằng `O_DIRECT | O_DSYNC | O_NOFOLLOW`.
- Buffer trực tiếp được căn page bằng anonymous memory map.
- Namespace hoặc scratch partition, với vùng byte tương đối bị giới hạn cứng.
- Kiểm tra logical-block alignment cho offset và length.
- Write budget được kiểm tra trước mỗi submission và đếm theo host bytes thực gửi.
- `sync_data` sau write trước read-back verification.
- Exact 4 KiB rebuild/compare cùng mixed transfer recipe của TASK-A.
- Wall-clock duration hoặc operation/write-budget cap, điều kiện nào đến trước.
- Throughput event gồm current/average/peak read và write.
- Report gồm bytes written/read/verified, failure count, completion reason và performance summary.
- Inventory/SMART/AER snapshot trước và sau phiên chạy.
- CPU–RAM preflight là evidence attribution tùy chọn; stable-ID/target/write authorization vẫn bắt buộc.
- `run-preserving` cho SSD đang có dữ liệu: chỉ tạo file mới, xác minh file nằm trên đúng NVMe, giữ free-space reserve, direct-I/O verify rồi xóa file.
- Simulator và preserving mode không có đường truncate/overwrite file tồn tại.

## Chế độ bảo toàn dữ liệu — mặc định cho vận hành thực tế

`run-preserving` không mở ghi block device. Nó revalidate selection/stable ID, map `st_dev` của thư mục sang đúng namespace/partition đã chọn, kiểm tra free space và chỉ tạo một path mới bằng `O_EXCL | O_NOFOLLOW`. File được ghi bằng `O_DIRECT | O_DSYNC`, flush, đọc lại và xóa khi kết thúc.

Chi tiết và giới hạn cam kết nằm tại [DATA_PRESERVATION.md](DATA_PRESERVATION.md). Raw bounded mode bên dưới chỉ dành cho ổ scratch được phép xóa.

## Hai bước cấp quyền

### 1. Tạo raw plan — không mở SSD

```bash
cargo run -p nvme-cli -- prepare-bounded \
  --selection selection.json \
  --device 2 \
  --target /dev/nvme1n1p3 \
  --range-start-mib 0 \
  --range-size-mib 8192 \
  --write-budget-mib 1024 \
  --duration-seconds 1800 \
  --output bounded-plan.json
```

Chương trình chỉ tạo plan khi target thuộc namespace đã chọn, stable ID còn đúng, không mount, không swap, không có holder, geometry/range hợp lệ. Plan in ra `Confirmation target ID`.

### 2. Chạy raw — có ghi đè dữ liệu trong bounded range

```bash
sudo ./target/release/cpu-nvme-dit run-bounded \
  --plan bounded-plan.json \
  --confirm-target-id 'CHUỖI_TARGET_ID_CHÍNH_XÁC' \
  --acknowledge-data-loss \
  --events events.jsonl \
  --report report.json
```

Lệnh re-scan toàn bộ inventory trước khi mở. Sai stable ID, target đổi node/geometry, target vừa được mount hoặc thiếu acknowledgement đều bị từ chối. `--cpu-ram-preflight` có thể được thêm; nếu đã cung cấp mà evidence không PASS thì lệnh fail closed.

Preflight tùy chọn tối thiểu:

```json
{
  "schema_version": 1,
  "module_id": "dit.cpu-ram",
  "run_id": "cpu-ram-run-id",
  "verdict": "PASS"
}
```

Có thể có thêm field. File được hash SHA-256 và lưu làm evidence trong report.

## Quy tắc quan trọng

- `prepare-bounded` không phải quyền ghi; nó chỉ tạo một proposal có version.
- `run-bounded` sẽ phá dữ liệu nằm trong `range_start_bytes .. range_start+range_length`.
- Không dùng `run-bounded` cho SSD có dữ liệu cần giữ; dùng `run-preserving`.
- Write budget giới hạn tổng host bytes, không phải NAND bytes; SMART delta cần dùng để theo dõi write amplification.
- Không dùng filesystem đang mount hoặc ổ chứa root/boot/results.
- Không dùng `/dev/disk/by-id` symlink ở backend; plan dùng exact `/dev/nvme...` node và stable ID riêng.
- Backend chỉ hỗ trợ một target trong TASK-C. Multi-device scheduler thuộc TASK-D.

## Kiểm thử

```bash
cd /home/hoang/DIT-01
make task-c-check
```

Gate gồm format, Clippy, toàn bộ workspace tests, simulator throughput/report smoke và thử nhắm ổ hệ thống trên host. Bài cuối bắt buộc trả lỗi trước khi plan được tạo.

Hardware validation còn cần một scratch NVMe/partition có thể xóa dữ liệu. Chỉ sau khi chạy positive/negative campaign trên target đó mới đổi trạng thái đường I/O thật thành `validated`.
