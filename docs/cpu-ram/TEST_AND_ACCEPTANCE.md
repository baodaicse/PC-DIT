# CPU-DIT-01 Test and Acceptance

## Trạng thái acceptance hiện tại

Bước 1–5 đã qua 41 automated tests, bounded device run 512 MiB/16 logical CPU,
sensor/power run 64 MiB và
live-image boot-smoke BIOS + UEFI trên QEMU.
RAM-byte, SIMD-worker, thermal, EDAC UE, timeout và worker-crash injection đều tạo
FAIL. Đây mới là
implementation acceptance; validation đầy đủ chưa đạt cho đến khi chạy USB vật
lý, soak và hardware matrix. Xem [DEVICE_TEST_2026-08-29.md](DEVICE_TEST_2026-08-29.md)
và [BOOT_IMAGE_TEST_2026-08-29.md](BOOT_IMAGE_TEST_2026-08-29.md). UI/sensor
evidence nằm tại [DEVICE_TEST_2026-08-29_UI_SENSOR.md](DEVICE_TEST_2026-08-29_UI_SENSOR.md).

## 1. Tiêu chí hoàn thành MVP

MVP đạt khi USB boot Linux minimal có thể:

1. xác minh source/manifest;
2. preflight CPU, RAM, topology và telemetry;
3. cấp phát và chạm phần RAM theo coverage target;
4. split, matrix encode và deterministic-scatter dữ liệu;
5. đọc lại bằng worker/core theo recipe;
6. exact-verify, decode, join và so source;
7. tính final result và so golden vector;
8. phát hiện mọi fault injection bắt buộc;
9. xuất report đủ thông tin để chạy lại cùng seed/recipe;
10. không ghi vào ổ nội bộ của máy được kiểm tra.

Ngoài ra, cùng một binary/descriptor phải tự thích nghi với dung lượng RAM khác
nhau mà không cần build riêng cho 32 GB, 64 GB hoặc 128 GB.

## 2. Fault-injection matrix tối thiểu

| Vị trí inject | Kỳ vọng |
|---|---|
| Source byte/manifest | `INVALID_SOURCE`, không quy lỗi CPU/RAM |
| Split offset/size | checkpoint hoặc join mismatch |
| Block index/metadata | metadata/order mismatch |
| Payload trước RAM write | encode/checkpoint mismatch |
| Payload sau RAM write | exact readback mismatch |
| Hai memory slot đổi chỗ | slot/order mismatch |
| Matrix coefficient/output | matrix golden/decode mismatch |
| Decode output | source compare/final mismatch |
| Join order | source compare/final mismatch |
| SIMD-only calculation | scalar differential mismatch |
| Final digest input/output | golden final mismatch |
| Worker crash/timeout | execution FAIL, không PASS |
| ECC/MCE synthetic event | kết quả theo policy, evidence bắt buộc |

Không fault injection nào được parity/correction che đi để tạo PASS.

## 3. Repeatability

- Cùng source, build, descriptor và seed phải tạo cùng recipe/checkpoint/final result.
- Một mismatch record phải đủ để tái hiện cùng stage, block và logical offset.
- Scheduling timing có thể khác nhưng không được làm thay đổi expected data.
- Counter-based generator và permutation không phụ thuộc thứ tự thực thi thread.

## 4. Coverage evidence

Report tối thiểu ghi:

- installed/Linux-usable/available-at-start/requested RAM;
- allocated/touched/written/read/exact-verified unique bytes;
- total read/write bytes cộng dồn qua mọi iteration, tách khỏi unique coverage;
- excluded/reserved memory nếu biết;
- coverage target, mẫu số tỷ lệ và cờ target-met;
- các trường trên theo từng NUMA node khi có NUMA;
- block, iteration, pattern và matrix count;
- core/worker participation;
- elapsed time, deadline và aborted reason;
- source, descriptor, recipe, build và seed ID.
- window recipe ID, plan digest, per-size/per-pattern count và verified bytes.

PASS chỉ hợp lệ khi coverage thực tế đạt target. Linux report phải dùng thuật ngữ
`allocatable RAM coverage`, không dùng `100% physical RAM` nếu không có bằng chứng.

Các invariant bắt buộc:

```text
exact_verified_unique_bytes <= read_unique_bytes <= written_unique_bytes
written_unique_bytes <= touched_unique_bytes <= allocated_unique_bytes
PASS => coverage_target_met == true
```

`total_read_bytes` và `total_write_bytes` có thể lớn hơn RAM vì cộng dồn qua nhiều
round/iteration; hai counter này không tham gia invariant unique coverage ở trên.

Allocation phải giữ các arena đồng thời trong round. Test batch nhỏ rồi giải phóng
không được dùng để cộng dồn thành full-memory coverage nếu không có physical-page
evidence chứng minh các batch không tái sử dụng cùng page.

Pattern/window coverage Bước 3A chỉ được nghiệm thu khi mỗi Safe/Full complete
round chạy đủ mandatory class 4,093 B..32 MiB, plan không gap/overlap, mọi tail
được exact-verify và per-size counters cộng đúng total. Bài 60 phút được kỳ vọng
tạo thêm round/seed so với 30 phút trên cùng máy, nhưng tỷ lệ seed/phút không phải
correctness gate vì phụ thuộc RAM size, throughput và thermal state.

## 5. Mismatch evidence

Mỗi lỗi dữ liệu cần có:

- stage phát hiện đầu tiên;
- group/block/global hoặc source offset/memory slot;
- expected, actual và XOR syndrome;
- size, alignment, seed, pattern, matrix và iteration;
- writer, reader, decoder và verifier core;
- timestamp và telemetry/hardware event liên quan;
- virtual/physical/topology evidence nếu platform cung cấp đáng tin cậy.

## 6. Validation matrix ban đầu

Tối thiểu bao phủ:

- CPU không SIMD và CPU có SIMD optimized path;
- 1 core, nhiều core và SMT on/off nếu điều khiển được;
- RAM dung lượng nhỏ/lớn và số channel khác nhau;
- các lớp 16/32/64/128 GB hoặc lớn hơn khi lab có phần cứng;
- non-ECC và ECC;
- single-node và NUMA nếu có;
- allocation/lock thành công và graceful degradation khi không lock được;
- cold run, sustained hot run và safety stop.

Implementation chỉ được nâng từ prototype lên validated đối với hardware matrix đã
thực sự chạy; không suy rộng validation sang toàn bộ platform chưa kiểm tra.

## 7. Bài test thích nghi dung lượng

| Tình huống | Kỳ vọng |
|---|---|
| Source nhỏ hơn RAM | Mọi block vẫn có expected payload tái tạo được |
| RAM không chia hết matrix group | Tail recipe verify toàn bộ phần dư |
| Arena lớn cấp phát thất bại | Giảm arena và backoff, không OOM/false PASS |
| `mlock` thất bại | Report rõ; áp dụng policy/coverage target |
| Một NUMA node thiếu coverage | Không dùng node khác bù để báo target-met |
| RAM tăng từ 32 lên 64 GB | Group/arena count tự tăng, không cần build mới |
| Kernel/firmware giữ RAM | Report excluded; không tuyên bố physical 100% |
| Batch page bị tái sử dụng | Không được cộng vào unique coverage nếu thiếu evidence |
