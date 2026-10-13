# Roadmap DIT-01 v1.2 — test nặng hơn, bắt lỗi nhiều hơn, hiệu quả hơn

Từ 2026-10-13, DIT-01 bước sang dòng **v1.2.xxx**. Đây là một đợt cập nhật mới với một mục tiêu:
cùng thời lượng test, phần mềm ép phần cứng nặng hơn và bắt được nhiều lỗi thật hơn. Số `xxx` tự
tăng sau mỗi thay đổi được commit, xem [VERSIONING.md](VERSIONING.md). Version hiện trên mọi màn
hình (tiêu đề dashboard, control center, rescue console, menu GRUB), nên người vận hành luôn biết
máy được test bằng bản nào.

Các roadmap trước (U1–U3, fault catch FC0–FC8, Secure Boot) vẫn ở [../ROADMAP.md](../ROADMAP.md).

## Nguyên tắc

- **Giữ nguyên thời lượng test**, chỉ tăng độ nặng và độ phủ trong cùng thời gian đó.
- **Đo trước, chọn hằng số sau.** Mọi con số (số worker, độ dài burst, thời gian mỗi lõi) đều phải
  đo trên host hoặc QEMU, sau đó đối chiếu với kết quả thực tế ở `Result/`.
- **Kết quả cũ vẫn so sánh được.** Nếu một tối ưu sinh ra đúng từng byte như trước thì giữ nguyên
  recipe ID. Chỉ tăng version recipe khi dữ liệu kiểm thật sự đổi.
- **FAIL tính theo ô linh kiện** (CPU, RAM, GPU, SSD). Số hiệu lõi hay địa chỉ chỉ nằm trong log
  chi tiết, không gợi ý cô lập hay thay linh kiện.
- **Throttle không ảnh hưởng kết quả.** Tải nặng hơn sẽ làm máy throttle nhiều hơn; chính sách giữ
  nguyên: throttle chỉ là thông tin.
- **Thảo luận trước, code sau.** Mỗi hạng mục được thống nhất rồi mới làm, làm xong thì kiểm bằng
  QEMU/host trước khi sang hạng mục tiếp theo.

## Hiện trạng đo được (trước v1.2)

Số liệu từ 77 lượt CPU-RAM trong `Result/` (2026-10) và đo trên máy build (Ryzen 7 3700X):

| Điểm | Đo được | Ý nghĩa |
|---|---|---|
| Tốc độ ghi + đọc RAM | 3,8 GB/s (7500F) … 12 GB/s (14900K, 9950X) | chỉ khoảng 10–15% băng thông DDR5 |
| Một worker RAM | 0,44 GB/s, khoảng 10 chu kỳ/byte | nghẽn ở CPU (tính byte mong đợi), không phải ở RAM |
| Sinh pattern hiện tại / theo word | 0,32 / 5,4 GB/s mỗi luồng, cùng từng byte | tối ưu được khoảng 15 lần, không đổi recipe |
| Kiểm mỗi arena | đọc 4 lần: so sánh, reduction scalar, reduction SIMD, SHA-256 (khoảng 2 GB/s) | SHA-256 sẽ thành điểm nghẽn tiếp theo |
| Binary release | baseline x86-64 (SSE2), không có lệnh `vfmadd` | "dense FMA" thực chất là `mulpd`+`addpd` 128-bit |
| Kernel FMA của system-run | một chuỗi phụ thuộc | dùng khoảng 1/8 sức FMA của lõi |
| Ramp theo thời lượng | 18,2M → 500M vòng mỗi burst | burst dài hơn, ít lần kiểm hơn, không nặng hơn |
| Chia worker | 3:1 (ví dụ 24 RAM + 8 tính toán trên 32 luồng), lấy theo thứ tự số CPU | nghi tải nặng rơi vào E-core trên Intel hybrid (chưa kiểm) |

## Hạng mục

Trạng thái: `done`, `in_progress`, `planned` (đã thống nhất), `proposed` (chờ xác nhận).

| ID | Hạng mục | Trạng thái |
|---|---|---|
| V0 | Version v1.2.xxx tự động và hiển thị trên UI | `done` (v1.2.000) |
| V0b | Ghi version vào report và `fail-summary.csv` | `proposed` |
| C1 | Sinh pattern theo word, gộp lượt đọc khi kiểm | `planned` |
| C2 | Chia worker RAM / tính toán bằng phép đo, theo loại lõi | `proposed` |
| C3 | Kernel nhiều chuỗi song song, burst ngắn, ramp theo tầng kernel | `planned` |
| C4 | Binary v1 / v3 (AVX2+FMA) / v4 (AVX-512), tự chọn theo CPU | `proposed` |
| C5 | Kernel mới: DGEMM chính xác, ép từng tầng cache, AES-NI/SHA-NI hàng loạt | `planned` |
| C6 | Xoay từng lõi (A: xoay thuần; B: xoay khi GPU chạy hết tải); combined chạy A+B | `planned` |

### V0 — Version và hiển thị (`done`)

- `VERSION` + `tools/dit-version.sh` cho ra `1.2.<n>`, n là số commit kể từ đầu dòng 1.2.
- `crates/dit-ui-core/build.rs` đưa version vào mọi binary có màn hình
  (`dit_ui_core::DIT_VERSION`, `product_title()`).
- Hiện ở:
  - tiêu đề mọi dashboard (`DIT-01 / DATA INTEGRITY TEST ENGINE / v1.2.xxx`);
  - dòng ENGINE của CPU-RAM;
  - control center (`DIT CONTROL CENTER v1.2.xxx // ...`);
  - rescue console;
  - dòng đầu menu GRUB (`v1.2.xxx | Build ... | <image-id>`).
- Image ghi `/etc/cpu-dit/version`, `dit_version=` trong build-info, `"dit_version"` trong manifest
  của `.img` và ISO.

**V0b (đề xuất):** kết quả trên USB hiện chỉ mang `image_id`. Đề xuất thêm `dit_version` vào
report JSON của từng module và thêm một cột vào `fail-summary.csv`. Khi gộp kết quả thực tế sẽ biết
ngay lượt nào chạy bản nào. Cần thống nhất cách xử lý file CSV cũ trên USB: thêm cột cuối, hoặc
sang file mới khi header khác.

### C1 — Sinh pattern theo word (`planned`)

- `expected_byte` hiện tính lại `window_seed`, `splitmix64` và một phép chia lấy dư cho **từng
  byte**. Chuyển sang tính theo word: seed của cửa sổ tính một lần, `splitmix64` một lần cho mỗi 8
  byte, chỉ số source đi tuần tự. Áp dụng cho cả 8 họ pattern.
- Gộp scalar/SIMD reduction vào lượt so sánh. SHA-256 chỉ băm ở một số vòng, có ghi rõ trong
  report, thay vì băm mọi arena ở mọi vòng.
- **Không đổi dữ liệu:** `CPU-DIT-PATTERN-v1` giữ nguyên. Test so bản mới với hàm cũ trên mọi họ
  pattern, cửa sổ lệch biên 8 byte và phần đuôi.
- Đạt khi: cùng từng byte; một worker ≥ 4 GB/s trên 3700X; lượt full trên máy thật gần chạm băng
  thông RAM.

### C2 — Chia worker theo phép đo và loại lõi (`proposed`)

- Sau C1, khoảng 12–16 worker là đủ làm đầy bus RAM. Thêm worker RAM nữa không làm RAM nặng hơn,
  vì chúng chỉ chờ bus.
- Đầu phiên, chương trình đo vài giây, tăng dần worker RAM đến khi băng thông thôi tăng. Phần luồng
  còn lại chạy kernel tính toán (C3–C5). Con số chọn được ghi vào report.
- Phân luồng theo loại lõi (P/E) và cặp SMT, không theo thứ tự đánh số CPU. Kernel nặng phải chạy
  trên P-core. Trước tiên phải kiểm nghi vấn hiện nay: trên 14900K, phần tính toán có đang chỉ chạy
  trên E-core hay không.
- Kết quả mong muốn: RAM chạy gần đầy bus, đồng thời lõi CPU chịu tải FMA/AVX nặng. Đây là tình
  huống làm lộ lỗi IMC và XMP/EXPO khi dòng điện của lõi cao.

### C3 — Kernel nhiều chuỗi, ramp theo tầng (`planned`)

- Tải chính dùng 8–12 chuỗi FMA độc lập (như `heavy_fp_pass` của menu 4) thay cho một chuỗi.
- Burst ngắn khoảng 50–200 ms để lần kiểm dày hơn.
- Ramp theo thời lượng thêm **tầng kernel** thay vì kéo dài burst. Ví dụ: 120 s chạy kernel nhiều
  chuỗi; từ 10 phút trở lên thêm C5; mức dài thêm C6. Hằng số phải đo trước khi chọn
  (`docs/DURATION_COMPLEXITY_RAMP.md`).

### C4 — Binary theo mức ISA (`proposed`)

- Build ba bản: v1 (baseline, như hiện nay), v3 (AVX2+FMA), v4 (AVX-512). Repo cấm `unsafe` nên
  tách binary là cách sạch nhất. ISO lớn thêm vài MB.
- **Tự chọn mặc định** theo cờ trong `/proc/cpuinfo`: có `avx512f` chạy v4; có `avx2` và `fma`
  chạy v3; còn lại chạy v1. Người vận hành không phải chọn, nên không có rủi ro chọn sai gây chết
  chương trình bằng SIGILL.
- **Chỉnh tay (ẩn) cho kỹ thuật viên:** tham số boot `dit_isa=v1|v3|v4` hoặc biến môi trường trong
  shell. Nếu ép mức CPU không hỗ trợ thì chương trình từ chối ngay khi bắt đầu, kèm lý do.
- UI và report ghi mức đang dùng, ví dụ `CPU kernels: AVX-512 (v4, auto)`. Kết quả đúng/sai không
  phụ thuộc mức ISA, vì mọi kernel so với đáp án tính độc lập.

| CPU | Mức tự chọn |
|---|---|
| Intel 12–14 gen, Core Ultra 200 | v3 (AVX-512 bị tắt trên dòng desktop) |
| Ryzen 7000 (Zen 4) | v4 (2 lượt 256-bit) |
| Ryzen 9000 (Zen 5) | v4 (datapath 512-bit thật) |

### C5 — Kernel mới (`planned`)

| Kernel | Ép phần nào | Cách kiểm |
|---|---|---|
| DGEMM khối nằm trong L1/L2 | FMA, load/store, cache | số nguyên biểu diễn bằng double, kết quả chính xác tuyệt đối, so từng bit |
| Ép từng tầng cache (L1/L2/L3) | cache, ring, fabric | đọc–sửa–ghi trên tập dữ liệu vừa từng tầng, so với đáp án |
| AES-NI CTR + SHA-NI hàng loạt | đơn vị crypto | tính lại độc lập trên buffer cỡ L2 |

### C6 — Xoay từng lõi (`planned`)

**Lý do.** Xung cao nhất (ví dụ 6,0 GHz trên 14900K, 5,7 GHz trên 9950X) chỉ xuất hiện khi 1–2
lõi hoạt động. Khi test toàn lõi, mỗi lõi chạy khoảng 5,0–5,5 GHz, còn dư biên ổn định, nên lõi bị
suy giảm (Raptor Lake gen 13/14) hoặc Curve Optimizer chỉnh quá tay vẫn PASS. Lỗi chỉ lộ ra khi chơi
game hoặc chạy tải nhẹ một luồng. Test toàn lõi về nguyên lý không bắt được loại lỗi này.

**Cách xoay (chung cho A và B).**

- Mỗi bước chỉ một lõi chạy kernel có kiểm kết quả; lõi SMT anh em và các lõi khác nghỉ, để lõi
  đó boost tối đa. Hết bước thì sang lõi kế tiếp, cho đến hết mọi lõi.
- Kernel nhẹ (boost cao nhất) và kernel AVX (dòng điện lớn). Mỗi lần chuyển lõi cũng là một phép
  thử sụt áp khi tải tăng đột ngột.
- **Intel hybrid:** P-core xoay từng lõi; E-core xoay **theo cụm 4 lõi** (cả cụm dùng chung xung
  nhịp). Ví dụ 14900K và Ultra 9 285K: 8 P + 4 cụm E = 12 bước, thay vì 24.

**Hai kiểu.**

| | A — xoay thuần | B — xoay khi GPU chạy hết tải |
|---|---|---|
| Chạy cùng lúc | không có gì | GPU/VRAM test chạy hết tải |
| RAM, SSD | nghỉ | nghỉ (chiếm lõi CPU, làm mất boost) |
| Xung đạt được | cao nhất (máy mát) | có thể thấp hơn một chút (máy nóng) |
| Bắt được | lỗi riêng của lõi ở xung đỉnh | lỗi đó cộng với nguồn/VRM dao động dưới tải GPU, nhiệt cả thùng máy |
| Giống tình huống | app một luồng, mở app, biên dịch | chơi game: GPU 100% và vài lõi boost |

**Lịch trong combined (đã chốt 2026-10-13).**

```
Deep: Baseline → [A] → Chain → Chạy chồng → [B] → Nghỉ → Đọc lại
```

- **Deep:** A và B dùng chung ngân sách cố định khoảng 20% (12 phút), **lấy từ pha chạy chồng**
  (40 → 28 phút). Tổng thời lượng không đổi. A đặt sớm, lúc máy còn mát, để đạt xung đỉnh. B đặt
  sau pha chạy chồng, lúc máy đã nóng.
- **Ngân sách chia đều cho số bước:** A lấy 1/3, B lấy 2/3 (B chạy 2 kernel). Tối thiểu A ≥ 10 s
  và B ≥ 20 s mỗi bước để xung và nhiệt kịp ổn định. Máy quá nhiều lõi so với ngân sách thì ưu tiên
  các lõi firmware xếp boost cao nhất, và report ghi rõ lõi nào chưa được xoay.
- **Bounded:** không xoay. Pha chạy chồng chỉ có 5 phút.
- **Soak:** mỗi chu kỳ một lượt A+B (khoảng 10 phút, lấy từ pha chạy chồng 50 phút của chu kỳ).
  Lỗi xoay được thử lặp lại ở nhiều trạng thái nhiệt.

| CPU (Deep) | Số bước | A mỗi bước | B mỗi bước |
|---|---|---|---|
| Ryzen 5 7500F | 6 | 40 s | 80 s |
| i5-14400F | 6 P + 1 cụm E = 7 | 34 s | 68 s |
| i7-12700KF | 8 P + 1 cụm E = 9 | 26 s | 53 s |
| i9-14900K, Ultra 9 285K | 8 P + 4 cụm E = 12 | 20 s | 40 s |
| Ryzen 9 9950X | 16 | 15 s | 30 s |

**Menu [1] CPU + RAM (đã chốt 2026-10-13).**

- **Full (30 phút):** A đặt đầu phiên (máy mát), ngân sách khoảng 20%, tức 6 phút. Phần RAM còn
  24 phút. Sau C1 phần RAM nhanh gấp khoảng 10 lần, nên số vòng kiểm RAM vẫn cao hơn nhiều so với
  30 phút hiện nay.
- **Safe (120 s):** không xoay.

| CPU (Full) | Số bước | A mỗi bước |
|---|---|---|
| Ryzen 5 7500F | 6 | 60 s |
| i5-14400F | 7 | 51 s |
| i9-14900K, Ultra 9 285K | 12 | 30 s |
| Ryzen 9 9950X | 16 | 22 s |

**Menu [4] CPU AVAIL / POWER: power-sweep giữ nguyên (đã chốt 2026-10-13).** Power-sweep đã chạy
từng CPU một, nhưng mục đích khác: burst 100 ms xen nghỉ 500 ms để thử sụt áp khi tải nhảy đột
ngột, không giữ lõi ở xung đỉnh, không ghi xung thực tế. Nó bắt một loại lỗi khác nên giữ nguyên.
Bộ xoay của C6 dùng chung phần nhận dạng lõi với nó, không thay thế nó.

**Bằng chứng và ghi lỗi.**

- Report ghi xung thực tế của lõi ở từng bước, đọc qua cpufreq (MSR bị khóa khi Secure Boot bật).
  Bước dưới khoảng 95% xung tối đa của lõi đó được ghi NOTE "không đạt boost", không tính FAIL.
- Kết quả sai tính FAIL ô CPU, kể cả trong B. Report ghi rõ lỗi xảy ra ở pha A hay B; số hiệu lõi
  chỉ nằm trong log chi tiết.

**Độ phức tạp và rủi ro.**

| Phần | Mức |
|---|---|
| Nhận dạng lõi (lõi vật lý, cặp SMT, P/E, cụm E, xung tối đa); dùng chung với C2 | trung bình |
| Bộ xoay: ghim, chạy kernel của C3/C5, chuyển bước theo ngân sách | trung bình |
| Bằng chứng boost qua cpufreq | trung bình |
| Pha A trong combined | thấp |
| **Pha B:** lớp GPU chạy cùng bộ xoay, luồng kiểm GPU giới hạn 1–2 và ghim xa lõi đang test | **cao** |
| Ghi lỗi (mã NOTE mới), UI (lõi đang xoay, xung thực tế/tối đa, tiến độ) | thấp–trung bình |

- **Rủi ro chính của B:** ở Balanced, lớp GPU dùng khá nhiều luồng CPU để kiểm kết quả GPU (máy 32
  luồng: 7 luồng). Trong B chỉ còn 1–2 luồng. Nếu khâu kiểm chậm hơn GPU thì GPU phải chờ và không
  còn chạy hết tải. Phải đo trước; nếu đúng vậy, lớp GPU cần một chế độ cho B (GPU chạy liên tục,
  khâu kiểm theo sau) mà vẫn kiểm đủ mọi kết quả.
- QEMU chỉ kiểm được logic (lịch, xoay, ghi kết quả). CPU ảo không boost, nên hiệu quả thật chỉ đo
  được trên máy thật.
- Thước đo hiệu quả sau khi triển khai: số FAIL chỉ xuất hiện ở pha xoay trong khi mọi pha khác
  PASS, đếm trên kết quả thực tế.

## Quyết định

| Chủ đề | Quyết định | Trạng thái |
|---|---|---|
| Thời lượng | Giữ nguyên, tăng độ nặng | đã chốt (2026-10-13) |
| Throttle | Không ảnh hưởng kết quả, giữ chính sách hiện nay | đã chốt (2026-10-13) |
| Version | v1.2.xxx, xxx tự tăng theo commit, hiện trên UI | đã chốt (2026-10-13) |
| C2 | Tự đo đầu phiên, chia theo loại lõi | chờ xác nhận |
| C4 | Tự chọn mặc định, `dit_isa=` ẩn cho kỹ thuật viên | chờ xác nhận |
| C6 combined | Deep: A+B, ngân sách cố định khoảng 20% (12 phút) lấy từ pha chạy chồng; Bounded: không xoay; Soak: mỗi chu kỳ một lượt A+B; E-core xoay theo cụm | đã chốt (2026-10-13) |
| C6 boost | Bước không đạt gần xung tối đa ghi NOTE "không đạt boost", không tính FAIL | đã chốt (2026-10-13) |
| C6 menu [1] CPU + RAM | Full: A đầu phiên, khoảng 6 phút trong 30 phút; Safe: không xoay | đã chốt (2026-10-13) |
| C6 menu [4] | Power-sweep giữ nguyên | đã chốt (2026-10-13) |
| V0b | Version trong report và CSV | chờ xác nhận |

## Thứ tự triển khai

1. C1: sinh pattern theo word. Độc lập, không đổi dữ liệu, lợi ngay.
2. C3 + C2: kernel nhiều chuỗi và cách chia worker mới.
3. C4: binary v3/v4 và cơ chế tự chọn.
4. C5: các kernel mới.
5. C6: xoay từng lõi. Bộ xoay và A trước, B sau, vì B cần đo và có thể phải sửa lớp GPU.

Mỗi bước kết thúc bằng: `make test`, đo trên host, kiểm QEMU nếu đụng tới image, và cập nhật bảng
hạng mục ở trên.

## Việc tồn đọng từ trước (chưa xếp lịch)

- CPU-NVMe `matrix_rounds` ramp ([../DURATION_COMPLEXITY_RAMP.md](../DURATION_COMPLEXITY_RAMP.md)).
- FC8 đang tạm gác ([../FAULT_CATCH_ROADMAP.md](../FAULT_CATCH_ROADMAP.md)).
- Cách hiển thị "RAM 1 (1 susp)" làm người vận hành bối rối, đã gác từ 2026-10-09.
