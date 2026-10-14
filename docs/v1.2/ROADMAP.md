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
| V0b | Ghi version vào report và `fail-summary.csv` | `done` (v1.2.004) |
| C1 | Sinh pattern theo word, gộp lượt đọc khi kiểm | `done` (v1.2.006) |
| C2 | Chia worker RAM / tính toán bằng phép đo, theo loại lõi | `done` (C2a: v1.2.008, C2b: v1.2.009) |
| C3 | Kernel nhiều chuỗi song song, burst ngắn, ramp theo tầng kernel | `done` (v1.2.007) |
| C4 | Binary v1 / v3 (AVX2+FMA) / v4 (AVX-512), tự chọn theo CPU | `done` (v1.2.010) |
| C5 | Kernel mới: DGEMM chính xác, ép từng tầng cache, AES-NI/SHA-NI hàng loạt | `planned` |
| C6 | Xoay từng lõi (A: xoay thuần; B: xoay khi GPU chạy hết tải); combined chạy A+B | `planned` |

### V0 — Version và hiển thị (`done`)

- `VERSION` + `tools/dit-version.sh` cho ra `1.2.<n>`, n là số commit kể từ đầu dòng 1.2.
- `crates/dit-ui-core/build.rs` đưa version vào mọi binary có màn hình
  (`dit_ui_core::DIT_VERSION`, `product_title()`). Từ V0b, `build.rs` chuyển sang `dit-contracts`.
- Hiện ở:
  - tiêu đề mọi dashboard (`DIT-01 / DATA INTEGRITY TEST ENGINE / v1.2.xxx`);
  - dòng ENGINE của CPU-RAM;
  - control center (`DIT CONTROL CENTER v1.2.xxx // ...`);
  - rescue console;
  - dòng đầu menu GRUB (`v1.2.xxx | Build ... | <image-id>`).
- Image ghi `/etc/cpu-dit/version`, `dit_version=` trong build-info, `"dit_version"` trong manifest
  của `.img` và ISO.

### V0b — Version trong kết quả (`done`, v1.2.004)

Kết quả trên USB trước đây chỉ mang `image_id`. Nay mọi kết quả ghi luôn version đã tạo ra nó.

- **Một chỗ giữ version cho mọi binary:** `dit_contracts::DIT_VERSION`. `crates/dit-contracts/build.rs`
  lấy số từ `tools/dit-version.sh`. `dit-contracts` là crate nền mà engine, orchestrator, hộp đen
  và UI đều dùng; `dit_ui_core::DIT_VERSION` chỉ dùng lại hằng số đó. Gắn lúc build chứ không
  truyền lúc chạy: binary build từ commit nào thì tự biết commit đó, không thể ghi nhầm.
- **Report có trường `dit_version`:**

| Report | File | Schema |
|---|---|---|
| CPU-RAM | `system-report.json` | 10 → 11 |
| Menu [4] power | report power-platform | 3 → 4 (kiểm như 3) |
| Compute oracle | report compute-oracle | 4 → 5 |
| CPU-SSD | report từng SSD, report nhiều SSD, report của `nvme-cli` | schema chung, không đổi |
| GPU-VRAM | report một GPU, nhiều GPU | schema chung, không đổi |
| Combined | `combined-report.json` | 1 → 2 |
| Suite | `suite-report.json` | schema chung, không đổi |
| Hộp đen / hậu kiểm | kết quả do hộp đen ghi | 1 → 2 |
| Watchdog | `watchdog-status.json`, `watchdog-final.json` | 2 → 3 |
| Chain | report của chain | 1 → 2 |

- Report đọc lại được đánh `#[serde(default)]`: report cũ không có trường này vẫn đọc được, và
  `dit_version` rỗng nghĩa là **v1.1**.
- **`fail-summary.csv`:** thêm cột `dit_version` ngay sau `image_id`. Lần ghi đầu tiên gặp file có
  header khác, chương trình đổi tên file cũ trước khi ghi, nội dung giữ nguyên:
  - header không có cột `dit_version` (v1.1) → `fail-summary-v1.1.csv`;
  - header có cột nhưng khác bố cục → đặt theo version ở dòng cuối, ví dụ
    `fail-summary-v1.2.005.csv`;
  - tên đã có thì thêm `-2`, `-3`...
  - file rỗng (bị cắt giữa lúc tạo và lúc ghi header, ví dụ mất điện) thì xóa đi, dòng mới ghi lại
    từ header.
- **File cũ không chuyển đổi.** Kết quả nào không có `dit_version` (tạo trước 2026-10-13) là bản
  v1.1.

### C1 — Sinh pattern theo word (`done`, v1.2.006)

**Đã làm:**

- **Sinh theo đoạn, không theo byte.** `WindowExpected` (`crates/dit-system/src/window.rs`) tính
  seed của cửa sổ một lần rồi sinh cả đoạn byte:
  - `source_expanded`: một `splitmix64` cho mỗi 8 byte (`dit_core::expand_into`, đọc source qua
    `ExpansionSource`, bảng source nối thêm 7 byte để 8 byte liền nhau luôn đọc được một lần);
  - all-zero, all-one, checkerboard, walking-one/zero chỉ phụ thuộc offset modulo 8: tính 8 byte
    một lần rồi chép ra cả đoạn;
  - `address_derived`: vẫn một `mix64` cho mỗi byte (bản chất của họ này), nhưng không còn tính
    lại seed cửa sổ cho từng byte.
- **Gộp lượt đọc khi kiểm.** Arena được đọc **một lần**, theo khối 64 KiB: so khối với byte mong
  đợi (sinh từng đoạn 4 KiB nằm trong L1), rồi tính scalar/SIMD reduction và SHA-256 trên chính
  khối đó khi nó còn trong cache. Trước đây arena được đọc từ RAM bốn lần.
- **SHA-256 mỗi 4 vòng:** băm nội dung RAM ở vòng 0, 4, 8... Vòng nào cũng vẫn so **đủ từng
  byte**. Report ghi `config.content_hash_every_rounds` (4) và `round_digests[].content_hashed`;
  vòng không băm có `sha256` rỗng. `system-report.json` schema 11 → 12. Report cũ không có hai
  trường này được đọc là băm mọi vòng.
- **Không đổi dữ liệu:** `CPU-DIT-PATTERN-v1` giữ nguyên. Test so `WindowExpected` với
  `expected_byte` (giữ làm chuẩn) trên cả 8 họ, cửa sổ bắt đầu ở mọi lane của word, đoạn cắt
  giữa word và phần đuôi; `expand_into` với source ngắn hơn 8 byte và offset gần `u64::MAX`;
  reduction tính theo khối so với bản cũ. Chạy bản cũ và bản mới cùng seed: SHA-256 nội dung RAM
  của các vòng trùng nhau từng vòng.

**Đo trên máy build (Ryzen 7 3700X, 2 GiB, seed cố định):**

| | Trước | Sau |
|---|---|---|
| 1 worker, ghi | 0,51 GB/s | 3,9–4,0 GB/s |
| 1 worker, kiểm (vòng không băm) | 0,46 GB/s | 2,8–3,0 GB/s |
| 1 worker, kiểm (vòng có băm) | 0,46 GB/s | 1,2 GB/s |
| 16 luồng, ghi | 3,9 GB/s | 11–11,7 GB/s |
| 16 luồng, kiểm (vòng không băm) | 3,9 GB/s | 18–21 GB/s |

Tốc độ sinh từng họ (một luồng): các họ lặp chu kỳ khoảng 50 GB/s, `source_expanded` 4,3 GB/s,
`address_derived` 0,9 GB/s.

**Chưa đạt:**

- Mục tiêu "một worker ≥ 4 GB/s": ghi đạt, kiểm mới 2,8–3,0 GB/s. Phần còn lại nằm ở
  `address_derived` (khoảng 1/8 số byte, mỗi byte một `mix64`). Binary SSE2 không có lệnh nhân
  64-bit vector; tắt vector hoá cũng chỉ được 0,94 GB/s. Bản AVX2 đo được 1,6 GB/s, AVX-512 có
  `vpmullq` sẽ nhanh hơn nữa: phần này đi cùng **C4**.
- "Lượt full gần chạm băng thông RAM": cần đo trên máy thật (DDR5) sau C4 và C2.

### C2 — Chia worker theo phép đo và loại lõi (`done`, v1.2.008–009)

- Sau C1, khoảng 12–16 worker là đủ làm đầy bus RAM. Thêm worker RAM nữa không làm RAM nặng hơn,
  vì chúng chỉ chờ bus.
- Đầu phiên, chương trình đo vài giây, tăng dần worker RAM đến khi băng thông thôi tăng. Phần luồng
  còn lại chạy kernel tính toán (C3–C5). Con số chọn được ghi vào report.
- Phân luồng theo loại lõi (P/E) và cặp SMT, không theo thứ tự đánh số CPU. Kernel nặng phải chạy
  trên P-core. Trước tiên phải kiểm nghi vấn hiện nay: trên 14900K, phần tính toán có đang chỉ chạy
  trên E-core hay không.
- Kết quả mong muốn: RAM chạy gần đầy bus, đồng thời lõi CPU chịu tải FMA/AVX nặng. Đây là tình
  huống làm lộ lỗi IMC và XMP/EXPO khi dòng điện của lõi cao.

**C2a — đặt worker theo loại lõi (`done`, v1.2.008):**

- Nghi vấn đã kiểm trên dữ liệu thật trong `Result/`: worker tính toán lấy các CPU cuối cùng theo
  số thứ tự, nên trên **i9-14900K/KF** (P-core 0–15, E-core 16–31) cả 8 worker tính toán chạy trên
  E-core (CPU 24–31); trên **i5-14400F** cả 4 cũng vậy (CPU 12–15); 12700KF có 4/5 trên E-core.
- Inventory nay ghi cho từng luồng: `core_kind` (`performance`/`efficient` theo
  `/sys/devices/cpu_core` và `cpu_atom`, `uniform` khi CPU không hybrid), `smt_rank` (0: luồng
  chính của lõi, 1: luồng SMT thứ hai) và `l3_id` (CCD/CCX trên AMD).
- Quy tắc `CPU-DIT-PLACEMENT-v1` (`config.worker_placement`): worker tính toán lấy luồng chính của
  P-core (mọi lõi trên CPU không hybrid), rải lần lượt qua các cụm L3; rồi E-core; rồi luồng SMT
  thứ hai. Worker RAM lấy từ đầu kia. Ví dụ 14900K: tính toán trên 8 P-core (CPU 0, 2, … 14),
  RAM trên 8 luồng SMT thứ hai của P-core và 16 E-core. 9950X: tính toán xen kẽ hai CCD.
- `compute-oracle-run` cũng chọn luồng theo thứ tự này. Menu [4] power-sweep giữ nguyên.

**C2b — đo đầu phiên để chia worker (`done`, v1.2.009):**

- `system-run --measure-split` (`split.rs`, `CPU-DIT-SPLIT-PROBE-v1`): sau khi cấp phát, chương
  trình thử 2, 3, 4, 6, 8, 10, 12, 16… worker RAM. Mỗi bước chạy 100 ms khởi động rồi đo 300 ms;
  các luồng còn lại **cùng lúc chạy kernel FMA C3** như khi test thật, vì worker RAM chung lõi
  với worker tính toán qua SMT sẽ chậm hơn khi đứng một mình.
- Đo bằng **pha kiểm** (sinh byte mong đợi, so sánh, reduction), không bằng pha ghi. Đo bằng pha
  ghi thì trên 3700X bus đầy ở 6–8 worker (khoảng 11 GB/s); nhưng mỗi worker kiểm chậm hơn và
  phía đọc của bus rộng hơn, nên pha kiểm cần nhiều worker hơn. Pha ghi tự đầy bus với số đó.
- Dừng khi hai bước liền nhau tăng dưới 5%. Chọn số worker RAM **nhỏ nhất** đạt ≥ 95% mức cao
  nhất đo được; phần còn lại chạy tính toán, **ít nhất 1/4 số luồng** như trước v1.2.
- Report: `worker_split` (chế độ, các bước đo, số worker mỗi bên, thời gian đo); khi số worker do
  dòng lệnh đặt thì `mode: fixed`. System schema 15.
- Dùng ở menu [1] Full (`boot-runner.sh`) và pha nền của combined (CPU-RAM chạy một mình). Pha
  chạy chồng của combined giữ số luồng của bộ điều phối, vì lúc đó các lớp khác đang chạy.
- Đo trên 3700X (16 luồng, DDR4): băng thông kiểm vẫn tăng đến 12 worker (20–22 GB/s), chọn
  10–12 worker RAM và 4–6 tính toán; mất khoảng 2,9 s. Máy này CPU là nút thắt, chưa phải bus
  RAM. Máy DDR5 nhiều lõi, hoặc sau C4 khi mỗi worker nhanh hơn, phép đo sẽ chọn ít worker RAM hơn
  và để nhiều luồng hơn cho tính toán.
- Kiểm QEMU (image `1.2.009-dev`, 4 vCPU, menu [1] Full 60 s qua `boot-runner.sh`): PASS, schema
  15; đo 2 → 3 worker, chọn 3 RAM + 1 tính toán (1,8 s); kernel C3 6 chuỗi, mọi burst khớp; 6 vòng
  RAM, băm ở vòng 0 và 4.

### C3 — Kernel nhiều chuỗi, ramp theo tầng (`done`, v1.2.007)

Áp dụng cho phần tính toán của CPU-RAM (`system-run --compute-oracle-workers`) và
`compute-oracle-run`. Menu [4] power-sweep giữ kernel cũ.

- **Kernel `CPU-DIT-FMA-CHAINS-v1`:** nhiều chuỗi FMA vector độc lập. Số chuỗi theo tập lệnh của
  binary, chọn bằng phép đo trên 3700X:

| Binary | Chuỗi | GFLOP/s mỗi lõi | Kernel cũ |
|---|---|---|---|
| SSE2 (hiện tại) | 6 | 30,4 | 5,5 |
| AVX2+FMA (sau C4) | 10 | 64 (khoảng 95% đỉnh) | 6,6 |

  Nhiều chuỗi hơn thì hết thanh ghi và chậm hẳn (SSE2 8 chuỗi: 12–19 GFLOP/s; AVX2 16 chuỗi: 28).
- **Kiểm khớp từng bit**, không còn ngưỡng 1e-9: đáp án chuẩn tính bằng code scalar riêng, làm
  tròn đúng như đường vector (một lần khi binary có FMA; nhân rồi cộng khi không có), mỗi chuỗi một
  luồng lúc bắt đầu phiên. IEEE-754 cho kết quả tất định, nên lệch 1 ulp cũng là lỗi.
- **Phép lặp quên chậm:** `x = m·x + a` với `m = 1 - 2^-27`. Kernel cũ dùng `m = 1 - 9e-7`, quên
  một giá trị sai sau khoảng 1 triệu vòng. Với `1 - 2^-27`, sai số còn hơn 60% sau cả một burst.
- **Burst cố định 64 triệu vòng** (khoảng 100 ms; đo thật 150 ms khi chung lõi với luồng RAM qua
  SMT), thay cho ramp theo thời lượng lên tới 500 triệu vòng. Phiên dài thì kiểm nhiều burst hơn.
- **Ramp theo tầng kernel:** hiện chỉ có tầng 1. C5 và C6 sẽ thêm tầng theo thời lượng.
- Report: `compute_oracle.fma_kernel`, `compute_oracle.fma_chains` (system schema 13, compute
  oracle schema 6); report cũ không có hai trường này là kernel một chuỗi.

### C4 — Binary theo mức ISA (`done`, v1.2.010)

- **Ba bản `dit-cli`** trong image, cùng thư mục `/opt/cpu-dit-01/`: `dit-cli` (v1, SSE2),
  `dit-cli-v3` (x86-64-v3: AVX2, FMA…), `dit-cli-v4` (x86-64-v4: AVX-512). Mỗi bản khoảng 3,5 MB.
  `build_boot_image.sh` build hai bản sau với `--target x86_64-unknown-linux-gnu`; thiếu
  `--target`, `RUSTFLAGS` áp cả cho proc-macro, mà proc-macro chạy trong `rustc` trên máy build,
  nên máy build không có AVX-512 sẽ chết vì SIGILL.
- **Một điểm vào:** `dit-cli` (v1) chọn mức khi khởi động rồi `exec` sang bản đó
  (`crates/dit-system/src/isa.rs`, `choose_isa` trong `dit-cli`). Cùng PID, cùng tham số, nên
  launcher, watchdog và combined không phải đổi gì.
- **Tự chọn:** mức cao nhất mà CPU chạy được và image có. Kiểm bằng `is_x86_feature_detected!`
  đủ bộ tính năng của mức đó (v3: AVX, AVX2, FMA, BMI1/2, F16C, LZCNT, MOVBE…; v4: AVX-512 F, BW,
  CD, DQ, VL); cách này kiểm cả việc kernel đã bật thanh ghi AVX, điều cờ `/proc/cpuinfo` không nói.
- **Chỉnh tay (ẩn) cho kỹ thuật viên:** biến `DIT_ISA=v1|v3|v4|auto` trong shell, hoặc tham số boot
  `dit_isa=` (nhấn `e` ở menu GRUB, thêm vào dòng `linux`); biến môi trường thắng. Ép mức CPU không
  có thì chương trình từ chối ngay, exit 3, ví dụ: `INVALID: dit_isa=v4 needs AVX-512 F, BW, CD,
  DQ and VL, which this CPU does not have (it runs up to v3)`.
- **Ghi lại:** `cpu_isa` (mức đang chạy, `auto`/`manual`/`direct`, mức CPU hỗ trợ) trong report
  CPU-RAM (schema 16), compute oracle (7) và power (5); dòng `CPU KERNELS  AVX2+FMA (v3, auto)`
  trên dashboard CPU+RAM.
- **Đo trên 3700X (v3 so với v1, cùng seed):**

| | v1 | v3 |
|---|---|---|
| 1 worker, ghi RAM | 3,83 GB/s | 5,58 GB/s |
| 1 worker, kiểm RAM | 2,72 GB/s | 3,65 GB/s |
| 16 luồng, kiểm RAM | 19,6 GB/s | 24,1 GB/s |
| 16 luồng, ghi RAM | 11,6 GB/s | 11,4 GB/s (đã chạm bus ghi) |
| Tính toán mỗi worker | 29 GFLOP/s (6 chuỗi) | 55 GFLOP/s (10 chuỗi) |

  Digest nội dung RAM giống hệt giữa v1 và v3. Bản v4 có `vpmullq` (nhân 64-bit vector, phần
  `address_derived` của C1 cần), nhưng máy build không có AVX-512 nên chưa đo.
- **Chưa kiểm:** bản v4 trên máy thật (Ryzen 7000/9000), image/ISO build đầy đủ.

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
| C2 | Đầu phiên tự đo để chia worker RAM và worker tính toán; xếp luồng theo loại lõi (P/E) | đã chốt (2026-10-13) |
| C4 | Tự chọn AVX2/AVX-512 theo CPU là mặc định; `dit_isa=` ẩn cho kỹ thuật viên | đã chốt (2026-10-13) |
| C6 combined | Deep: A+B, ngân sách cố định khoảng 20% (12 phút) lấy từ pha chạy chồng; Bounded: không xoay; Soak: mỗi chu kỳ một lượt A+B; E-core xoay theo cụm | đã chốt (2026-10-13) |
| C6 boost | Bước không đạt gần xung tối đa ghi NOTE "không đạt boost", không tính FAIL | đã chốt (2026-10-13) |
| C6 menu [1] CPU + RAM | Full: A đầu phiên, khoảng 6 phút trong 30 phút; Safe: không xoay | đã chốt (2026-10-13) |
| C6 menu [4] | Power-sweep giữ nguyên | đã chốt (2026-10-13) |
| V0b | Version trong report và `fail-summary.csv`; file cũ không chuyển đổi, thiếu `dit_version` (trước 2026-10-13) là v1.1; CSV dạng cũ đổi tên `fail-summary-v1.1.csv` | đã chốt (2026-10-13) |

## Thứ tự triển khai

1. V0b: version trong report và CSV (`done`, v1.2.004).
2. C1: sinh pattern theo word (`done`, v1.2.006).
3. C3 + C2: kernel nhiều chuỗi (C3 `done`, v1.2.007) và cách chia worker mới (C2 `done`, v1.2.008–009).
4. C4: binary v3/v4 và cơ chế tự chọn (`done`, v1.2.010).
5. C5: các kernel mới.
6. C6: xoay từng lõi. Bộ xoay và A trước, B sau, vì B cần đo và có thể phải sửa lớp GPU.

Mỗi bước kết thúc bằng: `make test`, đo trên host, kiểm QEMU nếu đụng tới image, và cập nhật bảng
hạng mục ở trên.

## Việc tồn đọng từ trước (chưa xếp lịch)

- CPU-NVMe `matrix_rounds` ramp ([../DURATION_COMPLEXITY_RAMP.md](../DURATION_COMPLEXITY_RAMP.md)).
- FC8 đang tạm gác ([../FAULT_CATCH_ROADMAP.md](../FAULT_CATCH_ROADMAP.md)).
- Cách hiển thị "RAM 1 (1 susp)" làm người vận hành bối rối, đã gác từ 2026-10-09.
