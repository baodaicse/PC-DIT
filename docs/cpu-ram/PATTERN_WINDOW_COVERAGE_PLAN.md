# CPU-DIT-01 Pattern/Window Coverage v1

## Trạng thái và mục tiêu

Trạng thái: `implemented` ngày 2026-08-29, chưa `validated` trên hardware matrix.
Engine/report schema v7 và device bounded run 64 MiB đã PASS; Safe/Full 30/60
phút trên USB thật vẫn là acceptance của Bước 6.

Mục tiêu là tăng độ phủ theo bốn chiều đồng thời:

```text
round seed × window size × alignment/offset × pattern family
```

Mỗi complete round vẫn phải ghi và exact-verify toàn bộ RAM target. Window chỉ là
phân đoạn logic bên trong arena RAM đang giữ đồng thời; engine không tạo hàng
nghìn file trên USB hoặc tmpfs.

## Baseline đã triển khai

- Mỗi complete system round dùng một round seed mới và phủ toàn bộ RAM target.
- Số seed bằng `rounds_completed`; duration dài hơn thường tạo nhiều seed hơn,
  nhưng không có mapping cứng 30 phút = 10 seed.
- Live runner cấp arena mục tiêu 32 MiB, allocation backoff tối thiểu 4 MiB.
- System fill/verify dùng deterministic window scheduler và xuất per-size,
  per-pattern evidence trong từng complete round.
- Reference route chạy một lần với block size cố định 65,536; 131,071 và 4,093
  byte. Duration của system route không làm reference route thêm block size.

## Duration và seed policy

Không đổi seed giữa chừng một window hoặc một RAM write/read round:

```text
complete round
  = derive one round seed
  + schedule full mandatory window recipe
  + write every requested byte
  + read/exact-verify every requested byte
  + commit round evidence
```

Engine tiếp tục chạy complete round đến khi đạt cả minimum duration và minimum
round. Vì vậy trên cùng máy, 60 phút thường hoàn thành gần gấp đôi số seed của 30
phút, nhưng throughput, RAM size và thermal throttling có thể làm tỷ lệ khác.

Không đặt gate cứng `30 phút = 10 seed`, vì một full-RAM round trên máy 128 GB có
thể lâu hơn nhiều máy 16 GB. Gate đúng là:

- Safe: ít nhất 2 complete round/seed và 2 phút.
- Full: ít nhất 4 complete round/seed và duration cấu hình.
- Mỗi complete round phải có full-RAM exact coverage và đủ mandatory recipe.
- Nếu hết duration giữa round, hoàn thành round rồi mới dừng.

Report/UI phải công bố số seed thực tế, không dự đoán trước số seed sẽ hoàn thành.

## Window-size recipe v1 đã khóa

Các size class được chọn để đánh vào page boundary, power-of-two boundary, tail,
cache/page interaction và block lớn:

| ID | Kích thước | Mục đích |
|---|---:|---|
| `W-4K-M3` | 4,093 B | odd tail gần 4 KiB |
| `W-4K` | 4,096 B | page-sized boundary |
| `W-64K-M1` | 65,535 B | off-by-one trước 64 KiB |
| `W-64K` | 65,536 B | 64 KiB aligned class |
| `W-64K-P1` | 65,537 B | crossing 64 KiB boundary |
| `W-128K-M1` | 131,071 B | off-by-one trước 128 KiB |
| `W-128K` | 131,072 B | 128 KiB aligned class |
| `W-128K-P1` | 131,073 B | crossing 128 KiB boundary |
| `W-1M` | 1 MiB | medium streaming/cache route |
| `W-4M` | 4 MiB | huge-page-scale boundary, không tuyên bố huge page |
| `W-12M` | 12 MiB | non-power-of-two large window |
| `W-32M` | 32 MiB | full default arena window |
| `W-TAIL` | phần dư | mọi byte cuối không chia hết |

Safe 256 MiB và Full phải chạy mọi mandatory class trong mỗi complete round.
Explicit development target quá nhỏ được phép đánh dấu size lớn `not_applicable`,
nhưng không được báo rằng size đó đã được cover.

## Pattern-family recipe v1 đã khóa

Window scheduler sẽ gán và hoán vị các family bằng round seed:

- source-expanded pseudorandom hiện có;
- all-zero và all-one;
- checkerboard `0xAA/0x55` và đảo chiều;
- walking-one và walking-zero theo bit lane;
- address/global-offset-derived pattern;

Matrix/integer/hash/coherency CPU route vẫn chạy như oracle CPU riêng. Không gắn
nhãn `matrix_derived` vào window cho đến khi có expected generator độc lập tương
ứng; schema v7 hiện chỉ công bố đúng tám family ở trên.

Expected data luôn là hàm versioned của source, recipe ID, session/round seed,
window ID, pattern ID và global offset. Write và independent verify không được
dùng chung mutable expected buffer.

## Scheduler invariants

1. Mọi requested byte thuộc đúng một logical window trong một write pass.
2. Không có gap hoặc overlap giữa window ranges.
3. Window không vượt arena; scheduler chuyển sang arena kế tiếp hoặc tạo tail.
4. Toàn bộ arena vẫn được giữ đồng thời đến khi round hoàn tất.
5. Mandatory size/pattern set không phụ thuộc thread scheduling.
6. Round seed chỉ hoán vị order, placement và pattern association; nó không được
   loại bỏ mandatory class.
7. Same source/build/recipe/session seed phải tái tạo cùng window plan.
8. Different round seed phải tạo plan/association khác và được ghi evidence.

## Report schema v7

Các field đã khóa:

```text
config.window_recipe_id
config.window_sizes_bytes[]
config.pattern_recipe_id
round_digests[].round_seed_hex
round_digests[].window_plan_sha256
round_digests[].size_class_counts{}
round_digests[].size_class_verified_bytes{}
round_digests[].pattern_counts{}
round_digests[].mandatory_recipe_met
window_coverage.no_gaps
window_coverage.no_overlaps
window_coverage.tail_verified
```

Unique RAM coverage vẫn là số byte khác nhau được giữ đồng thời. Không cộng các
size class hoặc round thành unique RAM lớn hơn allocation target.

## UI đã triển khai

Dashboard bổ sung recipe/window/pattern từ progress event, không thay correctness oracle:

```text
ROUND SEED     0x...
WINDOW         W-128K-P1 / 131073 B
PATTERN        address-derived
RECIPE         WINDOW-v1 / 9 of 13 classes observed
```

UI chỉ hiển thị progress event từ engine. PASS/FAIL lấy từ report gates.

## Trình tự triển khai

### PW1 — Recipe và deterministic planner — `implemented`

- Khóa recipe/version và size eligibility.
- Sinh window plan không gap/overlap, xử lý arena/tail.
- Golden known-answer vector cho planner.

### PW2 — Pattern families và independent oracle — `implemented`

- Triển khai từng family bằng scalar expected generator trước.
- Tối ưu fill/read path sau khi scalar tests PASS.
- Thêm matrix-derived window sau cùng, không dùng cùng implementation làm oracle.

### PW3 — System integration và evidence — `implemented`

- Thay arena-wide single pattern bằng planned windows.
- Thêm per-size/per-pattern counters, plan digest và schema v7.
- Progress/UI hiện active window/pattern/recipe coverage.

### PW4 — Negative tests — `partially validated`

- Inject gap, overlap, wrong size, wrong window order và metadata corruption.
- Inject payload tại đầu/cuối/cross-boundary của từng size class.
- Một mandatory class thiếu hoặc một tail byte chưa verify phải làm phiên không PASS.

### PW5 — Performance và device acceptance — `in progress`

- So sánh throughput trước/sau; không hy sinh exact coverage để đạt utilization.
- Device bounded test, Safe, Full 30 phút và Full 60 phút.
- Xác nhận 60 phút tạo thêm complete seed/round trên cùng cấu hình khi hệ thống
  không bị safety stop; không dùng tỷ lệ 2× làm correctness gate.
- Rebuild image và boot-smoke BIOS/UEFI cho artifact schema v7.

## Bằng chứng bounded trên thiết bị phát triển

Ngày 2026-08-29, target 64 MiB, hai arena 32 MiB, hai worker và hai random seed
PASS. Mỗi round đủ 12 mandatory size class, 8 pattern family, không gap/overlap,
verified-size sum đúng 67.108.864 byte; hai plan digest khác nhau. Xem
[DEVICE_STEP3A_TEST_2026-08-29.md](DEVICE_STEP3A_TEST_2026-08-29.md).

## Acceptance gates

- Planner known-answer và replay byte-for-byte PASS.
- Property tests nhiều target/arena size chứng minh không gap, overlap, overflow.
- Mỗi release profile complete round cover toàn mandatory size/pattern recipe.
- Per-size verified counters cộng đúng total exact-verified bytes.
- Same seed cho cùng plan/digest; different round seed cho association khác.
- Fault campaign PW4 không có false PASS.
- Duration không cắt ngang round/window và không làm mất mandatory class.
- Device/QEMU evidence, docs, validator và final image checksum được cập nhật.
