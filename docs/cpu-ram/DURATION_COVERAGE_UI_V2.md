# CPU-DIT-01 Duration, Coverage và Live UI v2

## Phân tích kết quả thực tế trước thay đổi

Ba session trong `Result/runs/` đều PASS nhưng cho thấy duration quá ngắn:

| Profile | Exact unique RAM | Rounds | Tổng write + read | Engine elapsed |
|---|---:|---:|---:|---:|
| Safe | 256 MiB | 2 | 1 GiB | 0.542 giây |
| Full | 12,064,358,400 B | 4 | 96,514,867,200 B | 24.837 giây |
| Full | 12,124,572,876 B | 4 | 96,996,583,008 B | 24.865 giây |

Các lượt Full không “nhẹ” về lượng dữ liệu: mỗi lượt đã ghi và đọc gần 97 GB.
Chúng ngắn vì engine optimized hoàn thành đúng 4 round rồi dừng, không có minimum
duration. Bằng chứng này phù hợp cho functional correctness, nhưng chưa đủ để
đánh giá lỗi phụ thuộc nhiệt, retention hoặc trạng thái tải kéo dài.

## Semantics duration mới

- Safe: ít nhất 2 phút và ít nhất 2 complete round.
- Full: mặc định ít nhất 30 phút và ít nhất 4 complete round.
- UI cho phép đổi Full từ 60 đến 86,400 giây bằng phím `D`.
- Hết thời gian không cắt ngang arena. Engine hoàn thành nguyên write → read →
  exact verify round rồi mới dừng.
- PASS yêu cầu đồng thời đủ target byte, minimum round, minimum duration, CPU
  oracle và safety policy.

Các trường duration có từ schema v4 và được giữ nguyên trong schema v7 hiện tại:

```text
config.minimum_duration_ms
duration_target_ms
duration_target_met
rounds_requested       minimum round count
rounds_completed       có thể lớn hơn rounds_requested
elapsed_ms             toàn bộ wall-clock của engine
```

Unique coverage vẫn chỉ bằng số byte RAM khác nhau được giữ đồng thời. Các round
bổ sung làm tăng `total_write_bytes` và `total_read_bytes`, không được cộng giả
thành unique coverage mới.

Lượt device test 64 MiB với minimum 3 giây đã hoàn thành 16 round, exact-verify
đủ 64 MiB mỗi round, tổng 1 GiB write + 1 GiB read và PASS sau 3.359 giây.

## Target RAM 95% an toàn

Không dùng `/proc/meminfo:MemFree` vì nó bỏ qua page cache có thể reclaim. Engine
dùng `effective_available_bytes`, là `MemAvailable` đã giới hạn thêm bởi cgroup
nếu cần. Full v2 tính:

```text
safe_limit = effective_available_bytes - 512 MiB reserve
requested  = floor(safe_limit * 95 / 100)
```

Như vậy “95%” áp dụng lên vùng còn an toàn sau reserve, không phải 95% physical
RAM. Linux luôn còn 512 MiB reserve cộng với 5% của safe limit. Cách này tăng
coverage nhưng tránh cấu hình RAM nhỏ bị OOM do lấy 95% trước rồi mới kiểm reserve.

Với session cũ có `effective_available=15,080,448,000`, target mới tương đương
13,816,398,233 byte thay vì 12,064,358,400 byte của profile 80% cũ.

Report ghi `target_kind=percent_safe:95`, `safe_limit_bytes`, `requested_bytes` và
exact coverage để kiểm lại phép tính.

## Live UI khi chạy

Màn hình `ACTIVE TEST` kế thừa các ý tưởng trình bày console từ HardwareValidator
nhưng được triển khai riêng trong CPU-DIT-01. Dashboard v3 cập nhật:

- trạng thái RUNNING và progress bar theo minimum duration;
- elapsed/minimum time;
- phase `PREFLIGHT`, `ALLOCATED`, `RAM WRITE`, `RAM VERIFY`, `ROUND COMPLETE`;
- round hiện tại, RAM target;
- CPU model/core/thread, nhiệt độ, Watt;
- tổng/available RAM, NUMA và hwmon count.
- CPU/RAM utilization bar, frequency, mainboard, kernel và DIT target;
- process description, deterministic seed, worker policy, coverage và trạng thái
  trạng thái temperature limit (mặc định OFF) và telemetry nhiệt.

Progress giữ 99% nếu đã đủ thời gian nhưng complete round/report chưa xong. Ctrl+C
gửi TERM tới toàn process group và kết quả là ABORTED, không phải PASS.
Terminal từ 110x32 dùng dashboard ba khung; terminal nhỏ hơn tự dùng bản compact.
Thiết kế và regression test chi tiết ở [UI_DASHBOARD_V3.md](UI_DASHBOARD_V3.md).

## Chạy image giả lập trên máy phát triển

```bash
cd /home/hoang/DIT-01
make run-image
```

Mặc định launcher mở UEFI QEMU bằng GTK, dùng 4 GiB RAM/4 vCPU và snapshot mode,
nên không sửa release image. Tùy chỉnh:

```bash
CPU_DIT_VM_MEMORY_MIB=8192 CPU_DIT_VM_CPUS=8 make run-image
CPU_DIT_VM_DISPLAY=curses ./tools/run_image_local.sh bios
CPU_DIT_VM_PERSIST=1 ./tools/run_image_local.sh uefi
```

`CPU_DIT_VM_PERSIST=1` tạo working copy trong `build/local-image-vm/` để giữ
report của VM; release `.img` gốc vẫn không bị sửa. QEMU dùng KVM khi quyền truy
cập `/dev/kvm` khả dụng, nếu không sẽ fallback TCG và chạy chậm hơn nhiều.

Giả lập chỉ kiểm boot/UI/integration. Duration và throughput trong TCG không đại
diện hiệu năng hay mức stress trên CPU/RAM vật lý.
