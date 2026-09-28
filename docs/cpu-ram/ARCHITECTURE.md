# CPU-DIT-01 Architecture

## Trạng thái triển khai

Reference data route bên dưới đã có trong `dit-core`. Adaptive memory engine,
CPU inventory/affinity, cross-core verification, scalar/SIMD differential,
known-answer tests, coherency workload, temperature/CPU-power telemetry, safety
stop và diagnostic rerun đã có trong `dit-system`. Ubuntu Noble live image
read-only, UI text, HWE driver/firmware set, USB-only result writer và hard
process watchdog đã được triển khai ở Bước 5. Hardware-matrix
validation vẫn thuộc Bước 6.

System schema v7 hỗ trợ minimum-duration loop: cùng high-water arena set được giữ
và chạy complete round lặp lại tới khi đạt cả duration lẫn minimum round. Full
profile lấy 95% safe allocatable limit sau reserve; live UI nhận progress event
theo phase mà không tham gia correctness oracle.

Schema v7 còn dùng Linux-random session seed và SHA-256 domain-separated round
seed. Seed/commitment được phát từ preflight, lưu trong report và có thể replay
chính xác bằng fixed `--seed`; xem [SEED_ROTATION_V1.md](SEED_ROTATION_V1.md).
Mỗi round lập `CPU-DIT-WINDOW-v1` plan, chạy đủ mandatory size/pattern recipe và
ghi plan digest/per-class evidence; xem
[PATTERN_WINDOW_COVERAGE_PLAN.md](PATTERN_WINDOW_COVERAGE_PLAN.md).

System v1 dùng topology + first-touch cho NUMA nhưng chưa chứng minh physical node
placement. Chi tiết code đã chạy nằm tại
[SYSTEM_PIPELINE_V1.md](SYSTEM_PIPELINE_V1.md); không nên đọc các mục kiến trúc
tương lai bên dưới như capability đã hoàn thành.

## 1. Data route

```text
USB source + manifest
        |
        v
SOURCE_VERIFY
        |
        v
SPLIT / VARIABLE BLOCK RECIPE
        |
        v
INTEGER MATRIX ENCODE + OPTIONAL PARITY
        |
        v
DETERMINISTIC SCATTER TO ALLOCATABLE RAM
        |
        v
READ / PERMUTE / MULTI-CORE COMPUTE
        |
        v
EXACT READBACK VERIFY
        |
        v
INDEPENDENT MATRIX DECODE
        |
        v
ORDERED JOIN + SOURCE COMPARE
        |
        v
FINAL MULTI-ALGORITHM DIGEST
        |
        v
GOLDEN RESULT + HARDWARE EVENT ORACLE
```

Mỗi stage quan trọng có checkpoint/evidence để xác định lỗi bắt đầu từ source,
split, encode, RAM readback, decode, join hay final compute.

## 2. Các thành phần

### Source manager

- Đọc source corpus và manifest versioned từ USB.
- Xác minh file size và SHA-256/BLAKE3 trước khi chạy DIT.
- Cung cấp source ID ổn định cho descriptor và report.
- Cho phép source corpus nhỏ hơn test region. Payload được mở rộng xác định:

```text
payload = Transform(source_slice, seed, round_id, block_index, global_offset)
```

`global_offset` và `block_index` ngăn các vùng RAM khác nhau vô tình nhận cùng một
pattern. Expected payload phải tái tạo được mà không cần giữ một bản sao bằng toàn
bộ dung lượng RAM.

### Recipe engine

Sinh kế hoạch xác định từ `test_id + descriptor_version + seed`, gồm block size,
alignment, source offset, pattern, matrix, memory slot, traversal và worker role.

Block-size recipe ban đầu:

- cache/bus: 64 B, 128 B, 256 B;
- page: 4 KiB, 64 KiB, 2 MiB;
- workload lớn: 12 MiB và block theo tỷ lệ tổng RAM;
- lệch biên: 4093 B, 4097 B, 65537 B và 12 MiB + 17 B.

### Block metadata

Metadata tối thiểu:

- test/run/round/group/block ID;
- source offset, payload size và memory slot;
- pattern, matrix, seed và iteration;
- header checksum, payload checkpoint digest;
- worker/core encode, write, read, decode và verify.

Metadata correctness không được phụ thuộc hoàn toàn vào chính vùng RAM đang test.

### Matrix engine

- Dùng ma trận khả nghịch versioned trên `GF(2^8)`, `GF(2^16)` hoặc unsigned
  arithmetic có semantics được định nghĩa chính xác.
- Cấu hình khởi đầu đề xuất: nhóm 16 data block và 2–4 parity block.
- Matrix/inverse được chọn xác định từ matrix ID và seed.
- Scalar reference và optimized SIMD implementation dùng chung golden vectors.
- Encode và decode/verify có đường code độc lập và có thể chạy trên core khác nhau.
- Số nhóm matrix được tính từ dung lượng đã cấp phát, không gắn với profile 32/64 GB.
- Vùng cuối không đủ một nhóm đầy đủ dùng tail recipe versioned; không byte nào bị
  bỏ chỉ vì kích thước arena không chia hết cho group/block size.

### Memory engine

- Cấp phát anonymous memory bằng nhiều `mmap` arena, thay vì phụ thuộc một mapping
  khổng lồ có thể thất bại do giới hạn hoặc fragmentation.
- Prefault/touch toàn bộ page; `mlock` khi khả dụng và policy cho phép.
- Scatter bằng deterministic permutation/PRP để lỗi luôn tái hiện được từ seed.
- Rotate allocation, alignment, slot, stride và traversal giữa các round.
- Báo requested, allocated, touched, written, read và exact-verified bytes.
- Giữ tất cả arena cùng lúc cho tới khi round hoàn tất; cấp phát-test-giải phóng từng
  batch có thể khiến Linux tái cấp cùng physical page và tạo coverage giả.

#### Adaptive allocation

Quy trình cấp phát cho mọi cấu hình RAM:

```text
inventory installed/Linux-usable RAM and NUMA
        |
calculate descriptor target + per-node safety reserve
        |
allocate arenas progressively
        |
prefault/touch every page
        |
allocation fails? reduce arena size and back off safely
        |
freeze high-water allocation set
        |
run all write/read/exact-verify rounds
```

Arena size là recipe/configuration, dự kiến bắt đầu từ 256 MiB–1 GiB rồi giảm khi
allocation thất bại. Không dùng allocation thành công trên giấy làm coverage:
page chỉ được tính sau khi đã fault-in/touch thành công.

Safety reserve phải kết hợp mức tối thiểu cố định với tỷ lệ dung lượng, có thể điều
chỉnh theo boot footprint và NUMA. Giá trị cuối chỉ được khóa sau khi đo thực tế;
không hard-code giả định rằng máy 32 GB hay 64 GB luôn mất cùng một lượng RAM.

#### NUMA

- Tính target/reserve và cấp phát arena theo từng NUMA node.
- Pin worker để chạy local-node và cross-node routes riêng.
- Không dùng tổng coverage để che một node chưa đạt target.
- Report requested/allocated/verified/excluded bytes cho từng node.

#### Coverage model

```text
installed_bytes
linux_usable_bytes
available_at_start_bytes
requested_bytes
allocated_unique_bytes
touched_unique_bytes
written_unique_bytes
read_unique_bytes
exact_verified_unique_bytes
excluded_or_reserved_bytes
coverage_target_bytes
coverage_target_met
total_write_bytes
total_read_bytes
```

`MemAvailable` chỉ là đầu vào lập kế hoạch, không phải evidence cuối. Coverage chính
đến từ `exact_verified_unique_bytes`; tỷ lệ cần công bố cả trên Linux-usable và
installed RAM để người vận hành không hiểu sai mẫu số. Các trường `total_*` là lưu
lượng cộng dồn qua nhiều iteration và không được dùng làm unique memory coverage.

Linux chỉ báo `allocatable RAM coverage`. Vùng kernel/device/reserved phải được
ghi là excluded; không tuyên bố 100% physical coverage.

### Compute và oracle

Chuỗi tính toán sử dụng nhiều primitive độc lập:

1. integer/GF matrix transform;
2. CRC32/CRC64 để sàng lọc và khoanh vùng;
3. AES/ChaCha hoặc integer mixing versioned;
4. BLAKE3/SHA-256 trên recovered blocks;
5. ordered Merkle/reduction root;
6. final digest gắn source hash, test ID, seed và descriptor version.

Expected checkpoint/final digest được sinh trước bằng reference implementation.
RAM correctness vẫn chạy exact compare và ghi expected/actual/XOR syndrome.

### Scheduler đa core

- Pin worker vào logical CPU/core và lưu topology trong report.
- Worker encode không luôn là worker read/decode/verify.
- Đảo vai trò và cross-check output giữa các core theo round.
- Barrier/control/telemetry thread tách khỏi stress loop.
- Phối hợp single-core, cross-core và all-core combined workload.

### Telemetry và safety

Thu thập khi platform hỗ trợ:

- EDAC, ECC corrected/uncorrected và MCE;
- nhiệt độ CPU, CPU package power, throttle và sensor availability;
- worker crash, timeout và signal;
- memory allocation/lock failure;
- monotonic timestamps tương quan với mismatch.

Sensor không khả dụng phải được ghi `unavailable`, không được thay bằng số 0.

## 3. Pattern RAM bắt buộc

Matrix/pseudorandom data bổ sung nhưng không thay thế các pattern nhắm tới lỗi vật
lý cụ thể:

- zero/ones và AA/55;
- address/inverse-address;
- walking one/zero;
- sparse/dense bits;
- counter-based pseudorandom;
- block move, sequential/reverse, stride/modulo và shuffled traversal.

## 4. Chống lỗi tự che lấp

- Scalar reference kiểm optimized/SIMD output.
- Encode và decode dùng implementation/code path độc lập.
- Golden intermediate vectors kiểm từng stage.
- Core ghi và core đọc/verify được luân chuyển.
- Một phần recovered block được so byte trực tiếp với source chuẩn.
- Nhiều oracle độc lập được dùng thay vì một phép tính thuận/nghịch duy nhất.
- Parity-detected mismatch vẫn là FAIL dù payload có thể phục hồi.

## 5. Diagnostic rerun

Khi mismatch, giữ nguyên seed, block, offset và recipe rồi thử:

1. verify lại source;
2. identity RAM write/read;
3. scalar matrix path;
4. đổi worker/core;
5. đổi allocation nhưng giữ payload;
6. lặp readback vùng cũ;
7. so kết quả repeatable, transient hoặc unattributed.

Chỉ quy trách nhiệm component khi evidence đủ mạnh; nếu không, báo route/system
failure và giữ toàn bộ thông tin cần để điều tra tiếp.
