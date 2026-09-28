# CPU-DIT-01 System Pipeline v1

## Phạm vi đã triển khai

`dit-system` là backend Linux cho Bước 2 và 3. Nó dùng source đã xác minh của
`dit-core` để sinh expected byte xác định, phủ một target RAM động bằng nhiều
anonymous `mmap` arena, rồi ghi/đọc/kiểm tra bằng các worker được pin CPU.

```text
inventory + CPU KAT/coherency
        |
safe target = effective available - reserve
        |
progressive mmap arenas + high-water retention
        |
writer CPU: first-touch + deterministic fill + optional mlock
        |
different verifier CPU: exact byte compare + scalar/SIMD + SHA-256
        |
coverage/oracle gates -> PASS or FAIL JSON
```

Reference command ở Bước 1 vẫn chịu trách nhiệm route split → GF(256) matrix →
scatter → decode → join với independent golden vector. `system-run` mở rộng phần
RAM/CPU lên dung lượng lớn và dùng chung source-expansion primitive; hai report
chưa được gộp thành một orchestration report.

## Memory planning

Target có hai dạng:

- `--memory-mib N`: số byte cụ thể;
- `--available-percent P`: `P%` của effective available RAM tại đầu phiên.
- `--safe-percent P`: `P%` của phần còn lại sau safety reserve; Full dùng 95%.

Trong cả hai trường hợp, target phải nhỏ hơn hoặc bằng:

```text
safe_limit = effective_available_bytes - safety_reserve_bytes
```

CLI mặc định target 256 MiB và reserve 512 MiB. Percentage hợp lệ là 1–95; nếu
target vượt safe limit, engine trả `INVALID` trước khi tạo tải. Cách này tự tăng
số arena trên máy 32/64/128 GB nhưng không hứa sử dụng phần RAM kernel/device đã
giữ lại.

Arena được cấp phát dần. Nếu một mapping lớn thất bại, kích thước thử được giảm
tới `minimum_arena_bytes`. Mọi arena thành công được giữ đồng thời qua toàn bộ
round để không cộng cùng một page tái sử dụng thành unique coverage giả.

Writer chạm và ghi mọi byte, kể cả arena cuối không tròn page. `mlock` có ba
policy: `off`, `best-effort`, `required`. Lock failure được ghi theo arena;
`required` không cho phiên PASS.

## CPU và data oracle

- Worker chỉ chọn từ cpuset Linux cho phép và gọi `sched_setaffinity`.
- Arena được first-touch trên writer CPU; verifier role xoay sang logical CPU kế.
- Expected byte là hàm của source, round seed và global offset.
- Exact verifier kiểm từng byte và giữ mismatch đầu tiên.
- Mỗi arena chạy cùng một reduction scalar và safe-SIMD; kết quả phải bằng nhau.
- Integer, SHA-256 và FP/FMA known-answer tests là CPU oracle bổ sung.
- Atomic `SeqCst` counter kiểm coherency đồng thời trên các worker.
- SHA-256 digest được lưu theo worker và toàn round để làm evidence.

`--inject-global-offset` đảo một bit RAM sau write. `--inject-simd-worker` chỉ làm
sai optimized result của worker đã chọn. Cả hai phải tạo exit code 2.

## Điều kiện PASS

PASS chỉ được tạo khi đồng thời:

- allocation và exact-verified unique bytes đạt đúng target;
- hoàn thành đủ mọi round và không có byte mismatch;
- không có affinity failure;
- scalar/SIMD differential, integer, SHA-256, FP/FMA và coherency đều PASS;
- mọi arena lock thành công nếu policy là `required`.

`total_write_bytes`/`total_read_bytes` là lưu lượng cộng dồn. Chúng không được dùng
thay cho `exact_verified_unique_bytes`, là unique allocatable-memory coverage.

## Cách chạy

```bash
cargo build --release --workspace

target/release/dit-cli system-run \
  --memory-mib 512 \
  --reserve-mib 512 \
  --arena-mib 32 \
  --minimum-arena-mib 4 \
  --rounds 4 \
  --workers 0 \
  --lock-policy best-effort \
  --report build/system-report.json
```

Exit code: `0` PASS, `2` FAIL, `3` invalid source/config/runtime setup và `4` nếu
không ghi được report.

Từ schema v7, report còn chứa source hash, session/round seed cùng commitment,
minimum-duration evidence, CPU package power khi khả dụng, safety/execution event
và diagnostic rerun summary, window plan digest cùng per-size/per-pattern exact
coverage. Chi tiết tại
[TELEMETRY_SAFETY_V1.md](TELEMETRY_SAFETY_V1.md).

`--minimum-duration-seconds N` giữ nguyên high-water arena set và lặp complete
round cho tới khi vừa đủ minimum round vừa đủ thời gian. Hết thời gian không cắt
ngang write/read. `--progress` phát event phase/round để live UI hiển thị trạng thái.
Mặc định session seed lấy từ Linux CSPRNG; `--seed 0x...` chỉ định replay seed.

## Giới hạn v1

- Anonymous Linux mapping chứng minh allocatable virtual-memory coverage, không
  chứng minh 100% physical RAM hay vị trí DIMM.
- NUMA target và writer placement dùng topology + first-touch. Chưa đọc PFN để
  chứng minh physical node placement.
- Cross-NUMA chưa được kiểm thử vì thiết bị hiện tại chỉ có một node.
- Read-only live image đã có ở Bước 5; soak và hardware matrix vẫn thuộc Bước 6.
