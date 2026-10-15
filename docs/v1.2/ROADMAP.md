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
| C5 | Kernel mới: DGEMM chính xác, ép từng tầng cache, AES-NI/SHA-NI hàng loạt | `done` (v1.2.011) |
| C6 | Xoay từng lõi (A: xoay thuần; B: xoay khi GPU chạy hết tải); combined chạy A+B | `done` (v1.2.012) |
| C7 | GPU: K của GEMM tự tăng khi CPU kiểm không kịp | `done` (v1.2.018) |
| C8 | GPU: INT8 Tensor core trong test đầy đủ và COMBINED | `done` (v1.2.018) |
| C9 | GPU: GEMM nhiều tầng trên dữ liệu VRAM, CPU vẫn kiểm từng word | `planned` (thảo luận) |
| C10 | GPU: compute sweep khả nghịch, GPU tự kiểm, CPU kiểm lại theo checkpoint | `planned` (thảo luận) |

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
- **Ramp theo tầng kernel:** tầng 1 (kernel này) cho mọi phiên; tầng 2 (C5) thêm từ 10 phút. C6 sẽ
  thêm tầng tiếp theo.
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

### C5 — Kernel mới (`done`, v1.2.011)

**Tầng kernel:** phiên từ **10 phút** trở lên (`TIER2_MINIMUM_SECONDS`) chạy tầng 2: mỗi burst
vẫn chạy kernel FMA của C3, rồi thêm **một** kernel tầng 2 theo vòng. Các worker bắt đầu ở kernel
khác nhau, nên cùng lúc máy chịu nhiều loại tải. Phiên ngắn hơn giữ tầng 1. Ép tay:
`--kernel-tier 1|2` (`compute-oracle-run`), `--compute-kernel-tier 1|2` (`system-run`). Menu [1]
Full (30 phút) và các pha dài của combined tự lên tầng 2.

| Kernel | Ép phần nào | Cách kiểm |
|---|---|---|
| `dgemm` | FMA, load/store, L1/L2: ma trận 64×64 (96 KiB), khối 4 hàng × 8 cột (2 hàng trên SSE2) trong thanh ghi vector | phần tử là số nguyên dưới 2^20 lưu dạng `double`: tích dưới 2^40, tổng 64 tích dưới 2^46, nên kết quả **chính xác tuyệt đối** dù có FMA hay không, cộng theo thứ tự nào; so từng bit với tích **số nguyên** (khối ALU khác) |
| `cache_l1`, `cache_l2`, `cache_l3` | đọc–sửa–ghi nhiều lượt trên phần chia cho mỗi luồng của từng tầng (đọc từ `cpu/cpuN/cache`: kích thước chia cho số luồng dùng chung; L3 tối đa 8 MiB) | mỗi lượt XOR mọi word với khoá của lượt; đáp án dạng đóng (giá trị đầu XOR mọi khoá), không đi qua buffer |
| `crypto` | AES-NI (AES-256-CTR) và SHA-NI trên buffer 256 KiB (vừa L2) | AES-256 và SHA-256 **phần mềm** tự viết theo FIPS-197 / FIPS 180-4 (`soft_crypto.rs`), tính một lần đầu phiên |

- Bước của kernel cache chọn bằng phép đo: với phép nhân hay dịch-cộng, cả ba tầng chạy cùng tốc
  độ, tức bị phép tính giới hạn; với chỉ XOR, các tầng tách đúng theo băng thông (3700X, AVX2:
  L1 128 GB/s, L2 58, L3 55 mỗi lõi).
- Mỗi burst tầng 2 khoảng 50–120 ms (3700X, v1 và v3): DGEMM 3000 tích, cache 6 GiB đọc–ghi,
  crypto 320 lượt.
- AES/SHA phần mềm được test với vector FIPS và với kết quả AES-NI/SHA-NI. Kernel test cả trường
  hợp đáp án sai phải bị bắt.
- Report: `compute_oracle.kernel_tier`, `tier2_kernels` (số burst và số lần sai của từng kernel),
  `tier2_kernels_passed` (system schema 17, compute oracle schema 8). Sai ở kernel tầng 2 là FAIL
  ô CPU (`cpu:oracle_tier2`), như các kernel tính toán khác.
- Chạy thật (3700X, 8 worker, 12 s): v1 và v3 PASS, mọi kernel đều được chạy, không lần nào sai;
  CPU-RAM tầng 2 với phép chia đo được (12 RAM + 4 tính toán) PASS.
- **Chưa kiểm:** AVX-512 (v4) và CPU hybrid trên máy thật; thời lượng mỗi burst trên E-core.

### C6 — Xoay từng lõi (`done`, v1.2.012)

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

**Đã làm (v1.2.012).**

- **Bộ xoay** (`crates/dit-system/src/rotation.rs`, `CPU-DIT-CORE-ROTATION-v1`): P-core và lõi của
  CPU không hybrid xoay từng lõi trên luồng chính, luồng SMT anh em để trống; E-core xoay theo cụm
  dùng chung L2 (`cache/index2/shared_cpu_list`), cả cụm chạy cùng lúc. Thứ tự theo
  `acpi_cppc/highest_perf` (lõi firmware xếp boost cao trước). Ngân sách chia đều; mỗi bước tối
  thiểu A 10 s, B 20 s; thiếu ngân sách thì giữ các lõi xếp hạng cao, report ghi `not_rotated`.
- **Kernel:** A chạy kernel nhẹ (số nguyên, rẽ nhánh, so với đáp án), để đạt boost cao nhất; B
  chạy kernel nhẹ nửa đầu bước, kernel FMA của C3 nửa sau (dòng lớn). Mọi kết quả đều kiểm.
- **Chạy trong `system-run`** (`--core-rotation a|b --core-rotation-seconds N`), trước khi cấp phát
  RAM: RAM chưa tồn tại nên nghỉ đúng như yêu cầu. Thời gian xoay tính trong
  `--minimum-duration-seconds`, phần RAM chạy phần còn lại. Report: mục `core_rotation` (system
  schema 18), từng bước ghi lõi, loại lõi, hạng firmware, xung tối đa/trung bình/đỉnh, số lần kiểm.
- **Combined:** pha xoay là lớp CPU-RAM với tham số khác (64 MiB, 2 worker, không tính toán), nên
  khởi động, theo dõi, report và quy lỗi đi đúng đường của CPU-RAM. Không sửa code lớp GPU.
  `schedule()` giữ nguyên; `add_core_rotation()` chèn pha: Deep `Baseline → A → Chain → chạy chồng
  (28 phút) → B → nghỉ → đọc lại`, Soak mỗi chu kỳ `A → chạy chồng (40 phút) → B`, Bounded không
  đổi. Không có GPU thì không có B, thời gian của B trả về pha chạy chồng.
- **Pha B:** lớp GPU khởi động như pha nền, chỉ giới hạn `--async-workers-max 2`; khi GPU sẵn sàng,
  pha xoay khởi động với `--core-rotation-keep-off-pid` (PID của lớp GPU): mỗi bước, mọi luồng của
  cây tiến trình GPU được đặt affinity tránh lõi đang test và luồng anh em của nó; xong thì trả lại.
- **Rủi ro của B đã đo trước:** dữ liệu thật trong `Result/` (80 lần chạy GPU): GPU cần tối đa
  khoảng 1,1 lõi để kiểm kịp (RTX 5080, 4090: 0,8–1,1; RTX 5060 Ti: 0,5–1,0), nên 2 luồng kiểm là
  dư. GTX 960 trên máy build: 2 luồng giữ GPU bận 95,04% (mặc định 95,02%), không giây nào bị khâu
  kiểm kìm.
- **Menu [1] Full:** A đầu phiên, 1/5 thời lượng (30 phút → 6 phút xoay, 24 phút RAM). Safe không
  xoay. Menu [4] power-sweep không đổi.
- **Ghi lỗi:** kết quả sai là FAIL ô CPU (`cpu:rotation_a`, `cpu:rotation_b`), ghi ngay vào
  fault-events như các kernel tính toán; không cộng vào `failures_total` (con số đó dùng để quy lỗi
  pattern về RAM). Bước dưới 95% xung tối đa là NOTE `no_boost`: `NOTE[no_boost]` trong
  `fail-summary.csv`, dòng `NOTE` trên màn kết quả; không tính FAIL, không làm phiên INVALID
  (`Findings.notes`, khác đường `causes` vốn ra INVALID).
- **Công tắc tắt:** `dit_rotation=0` (tham số boot) hoặc `DIT_ROTATION=0`: lịch combined và menu
  [1] trở lại y như trước C6.

**Kiểm trên máy build (Ryzen 7 3700X, GTX 960):**

| Kiểm | Kết quả |
|---|---|
| Xoay A, 40 s | 4 lõi xếp hạng cao nhất (CPPC 196, 196, 191, 186), 10 s mỗi lõi, khoảng 22 700 lần kiểm mỗi lõi, không sai; 4,20–4,26 GHz / tối đa 4,43; một lõi 94,8% → NOTE |
| Xoay B, 40 s, cây tiến trình giả 3 luồng chạy liên tục | lõi 0 đang test: affinity của cây 1–7, 9–15; lõi 3: 0–2, 4–10, 12–15; xong trả lại 0–15; kernel nhẹ và FMA đều chạy, không sai |
| Combined Deep thu nhỏ 5% (CPU-RAM + GTX 960) | mọi pha PASS, grade DEEP; xoay A 2 bước; xoay B dời 11 luồng của lớp GPU, GPU bận 94,4% (nền 94,6%, chạy chồng 70%); CSV `PASS` kèm `NOTE[no_boost]` |
| `make test` | PASS (thêm test: nhận dạng bước trên 14900K giả lập, chia ngân sách, ngưỡng boost, lịch Deep/Soak/Bounded, công tắc tắt, quy lỗi và NOTE) |

**Chưa kiểm:** CPU hybrid Intel thật (cụm E, xung 6 GHz của P-core), Ryzen nhiều CCD, image/ISO đầy
đủ, và thước đo hiệu quả của roadmap: số FAIL chỉ xuất hiện ở pha xoay trên kết quả thực tế.

**Sau đánh giá v1.2 (v1.2.013, chốt 2026-10-14).** Hai thay đổi để tăng khả năng bắt lỗi mà không
bỏ phần nào của C6:

- **B là "pha chạy chồng trừ CPU-RAM":** data chain, GPU và SSD khởi động theo đúng thứ tự của pha
  chạy chồng, rồi bộ xoay dời cây tiến trình của cả ba ra khỏi lõi đang test. Bản đầu của C6 lấy 12
  phút tải đồng thời của SSD, GPU và chain trong Deep (40 → 28 phút); nay chỉ CPU-RAM mất 12 phút
  (C1 đã bù gấp 3–4 lần số vòng RAM), còn SSD, GPU, chain chạy gần đủ 40 phút như trước.
  - Ngân sách ghi SSD (Standard 16 GiB, Heavy 64 GiB mỗi pha chạy chồng) được **chia** giữa pha
    chạy chồng và B theo tỷ lệ thời gian (`combined::ssd_budget_split`), nên tổng lượng ghi lên SSD
    của phiên giữ nguyên, không tăng hao mòn.
  - Chain chỉ đọc. GPU giữ 2 luồng kiểm. Không bơm lỗi thử nghiệm ở B (thuộc về pha chạy chồng).
  - B nặng hơn nên xung lõi thấp là bình thường: B vẫn ghi xung của từng bước nhưng **không** ghi
    NOTE `no_boost`; chỉ A (máy nghỉ) mới ghi.
- **Bounded có A 20 s** ngay sau các pha nền (pha chạy chồng 5 phút → 4 phút 40 giây). Bộ lập lịch
  tự giữ 2 lõi firmware xếp boost cao nhất, 10 s mỗi lõi: nhóm lõi chạy điện áp cao nhất, hay suy
  giảm trước nhất trên Raptor Lake. Bounded vẫn không có B.

## Sửa sau kết quả thực địa (v1.2.015–018, 2026-10-15)

Kết quả thực địa của v1.2.013 (ProArt X870E 9950X + RTX 3050, MAG X870E 9950X3D + RTX 5060 Ti) dẫn tới
bốn thay đổi. Chi tiết trong từng commit.

- **v1.2.015, rút USB bất kỳ lúc nào:** test ghi vào tmpfs, chép lên USB một lần khi xong; mọi kiểm tra "có ổ
  kết quả" xét thiết bị còn thật hay không; cắm lại thì menu gắn lại ổ sau `fsck.fat`. Hộp đen ghi journal mỗi
  60 s (10 s khi máy căng tải) thay vì mỗi lần đổi phase, để USB không bị ghi liên tục và nóng.
- **v1.2.016, nhiều GPU:** heartbeat chung cho watchdog. Trước đó GPU + iGPU bị dừng oan ở phút thứ 5 của pha chạy
  chồng (`INVALID[startup_stall]`) dù cả hai GPU vẫn chạy đúng.
- **v1.2.017, pha chạy chồng `max`:** CPU-RAM chạy `nice 10`. Từ C1 mỗi worker RAM đọc/ghi RAM liên tục và các luồng
  CPU kiểm kết quả GPU bị tụt lại: RTX 5060 Ti trên MAG X870E giảm từ khoảng 1000 MiB/s (trước v1.2) xuống
  507 MiB/s. Đo trên 3700X + GTX 960: GPU bận 60 → 79 %, công suất 66 → 86 W, CPU-RAM ít hơn 6 % số round.
- **v1.2.018:** C7 và C8 bên dưới.

### C7 — K của GEMM tự tăng (`done`, v1.2.018)

Mỗi word output mà CPU so chứa 2·K phép tính của GPU. Khi ≥ 2/3 số burst (tối thiểu 4) kể từ lần quyết định trước
ở trạng thái `VERIFIER_BOUND`, corpus kế tiếp dùng K gấp đôi và số job một nửa, tới K 4096; jobs × K giữ nguyên
nên VRAM của corpus và công dựng oracle không đổi. Chỉ tăng, không giảm; tắt khi đặt `--matrix-inner`. CPU vẫn so
toàn bộ từng word với oracle độc lập. Cái giá: lane CPU-verified và bộ quét DRAM được ít thời gian GPU hơn, nên K
chỉ tăng khi CPU kiểm là giới hạn. GTX 960 chạy cạnh CPU-RAM 12 + 4 luồng: K 286 → 2288 trong 11 s, khoảng
152 G phép tính được kiểm/s so với 80 G ở K cố định 256. Chi tiết: [mục 3.1 tài liệu GPU](../gpu-vram/GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md).

### C8 — INT8 Tensor core trong test đầy đủ (`done`, v1.2.018)

Chế độ 1 của GPU-VRAM và lớp GPU của COMBINED chạy `--compute-mix all` (u32 → FP32 → INT8; INT8 chỉ trên
compute capability ≥ 8.0). Bằng chứng: RTX 5060 Ti (CC 12.0), 30 phút, 2,9·10¹¹ kết quả INT8 khớp oracle CPU từng
bit (2026-10-08). Chưa có lần chạy trên CC 8.x (RTX 30/40): chế độ 2 (Tensor INT8 check) vẫn để kiểm dòng card mới,
chế độ 3 là test đầy đủ không INT8 cho card bị nghi lỗi phần mềm INT8.

### C9 — GEMM nhiều tầng trên dữ liệu VRAM (`planned`)

**Mục tiêu.** GPU tính nặng hơn nhiều trên mỗi word CPU phải so, và ép cả VRAM lẫn ALU, mà vẫn giữ chuẩn của tài
liệu GPU: CPU so **toàn bộ** output với một đáp án độc lập, chính xác từng bit, không lấy mẫu.

**Phép tính đề xuất (u32, lấy dư 2³²).** Mỗi job: X₀ = A (N×K) từ corpus; L tầng X_s = X_{s−1} · M_s với M_s là
ma trận K×K dùng chung cho mọi job, nằm sẵn trong VRAM; cuối cùng Y = X_L · B (K×N) cho output N×N như hiện nay, cộng
tag generation và word canh như mục 3.3.

| | GEMM hiện nay (K 1024) | C9, L = 4 |
|---|---|---|
| Phép tính mỗi job | 2·N²·K ≈ 8,4 M | L·2·N·K² + 2·N²·K ≈ 545 M |
| Output CPU phải so mỗi job | N² + 4 word (16 KiB) | không đổi |
| Phép tính trên mỗi word CPU so | ~2 K | ~130 K (gấp ~65 lần) |
| Dữ liệu trung gian | trong shared memory | X_s ghi/đọc VRAM (N×K mỗi job, 128 MiB mỗi tầng với 512 job) |

- **Điều kiện để lỗi không bị nuốt:** mọi hệ số của M_s và B là số **lẻ**. Với phép lấy dư 2³², lỗi e ở một ô trung
  gian lan sang cả hàng kế tiếp là e·m; nếu m lẻ thì e·m ≡ 0 chỉ khi e ≡ 0, nên một lỗi đơn ở bất kỳ tầng nào chắc
  chắn đổi output cuối. (Nếu m chẵn, lỗi e = 2³¹ có thể biến mất.)
- **Oracle:** CPU tính cả chuỗi bằng vòng lặp vô hướng độc lập, một lần cho mỗi corpus, dựng sẵn ở nền như hiện nay;
  corpus được GPU tính lại 256 batch với generation khác nhau, nên công CPU chia đều cho 256 lần. Với L = 4 và 512 job:
  khoảng 275 G phép tính mỗi corpus (vài chục giây trên 4 luồng phụ); cần đo để chắc dựng kịp trên CPU yếu.
- **Lỗi cài sẵn để kiểm bộ xác thực** (thêm vào `MatrixFault`): lật 1 bit một ô trung gian; bỏ ghi một tầng (đọc lại X
  cũ); đọc nhầm M của tầng khác; mỗi loại phải ra FAIL.
- **Báo cáo:** phép tính thứ tư trong vòng luân phiên (`u32-chain`), số phép tính đã kiểm tính riêng; tầng L có thể
  là bậc tiếp theo của C7 sau K 4096 (L 1 → 2 → 4).
- **Rủi ro:** công dựng oracle trên CPU ít lõi (giảm số job hoặc L theo máy); kernel tile cơ bản chỉ đạt một phần nhỏ
  sức GPU (tối ưu tile là việc riêng); batch dài hơn làm bộ điều khiển phản ứng chậm hơn; VRAM dành cho trung gian
  (2 bộ đệm luân phiên khoảng 256 MiB) bớt khỏi vùng `--vram-max`.

**Cần chốt khi thảo luận:** chỉ u32 trước hay cả FP32 (cần chuẩn hóa để không tràn) và INT8 (trung gian int32 phải gập
về int8 bằng XOR các byte để lỗi bit cao không bị cắt mất); L cố định hay tự tăng; thêm vào vòng `all` hay thay u32
thường khi CPU kiểm là giới hạn; ngưỡng chấp nhận.

**Các bước:** C9a oracle CPU, logic thuần và test (điều kiện hệ số lẻ, lan lỗi); C9b kernel CUDA, các lỗi cài sẵn, test
phần cứng trên GTX 960; C9c đưa vào vòng luân phiên, báo cáo, UI, nối với C7; C9d đo trên máy thật (COMBINED trên
một máy 9950X3D + RTX 5060 Ti).

### C10 — Compute sweep khả nghịch, GPU tự kiểm (`planned`)

**Mục tiêu.** Lấp thời gian GPU đang rảnh vì chờ CPU kiểm bằng tải ALU + VRAM nặng, tự kiểm trên GPU. Theo đúng quy
tắc của bộ quét DRAM hiện có (mục 2.5 tài liệu GPU): phần này là **device-verified**, báo riêng, **không bao giờ** cộng
vào phần CPU đã kiểm; CPU kiểm lại theo checkpoint.

**Phép tính đề xuất.** Trên nửa vùng VRAM của bộ quét: (1) ghi pattern `sweep_word(địa chỉ, khóa)` như hiện nay;
(2) kernel tiến chạy R vòng một hàm trộn **khả nghịch** trên từng khối 128 bit (cộng, xoay bit, xor với khóa vòng, nhân
với hằng số lẻ, trộn giữa 4 word) rồi ghi lại VRAM; (3) dữ liệu nằm trong VRAM trong lúc phần còn lại của vùng được
quét (retention); (4) kernel lùi chạy R vòng nghịch đảo theo thứ tự ngược, so với pattern gốc và đếm word sai trên GPU.

- **Vì sao bắt lỗi chắc chắn:** mọi bước là song ánh trên khối 128 bit, nên một khối bị sai ở bất kỳ thời điểm nào (ALU
  khi tiến, lật bit khi nằm trong VRAM, ALU khi lùi) cho kết quả cuối khác pattern gốc. Lỗi địa chỉ: pattern là song ánh
  theo địa chỉ, nên hai địa chỉ trùng một ô thì một trong hai sai.
- **Kiểm lại trên CPU:** theo chu kỳ (như checkpoint 16 MiB mỗi ~2 s của bộ quét), đọc về một khối ở trạng thái **sau
  khi tiến** và CPU tự tính hàm tiến bằng mã độc lập để so. Nhờ đó chứng minh GPU tính đúng, chứ không chỉ "đi rồi về
  khớp nhau" (một ALU hỏng theo cách nhất quán ở cả hai chiều vẫn bị bắt). Sai ở checkpoint là FAIL phase
  `compute-sweep-checkpoint`; GPU đếm sai là FAIL phase `compute-sweep`.
- **Chia thời gian:** dùng chung stream và duty của bộ quét DRAM. Duty tăng (ví dụ 0,25 → 0,75) chỉ khi CPU kiểm là
  giới hạn (`VERIFIER_BOUND`), để không lấy thời gian của GEMM CPU-verified khi GPU tự là giới hạn. R điều chỉnh tỉ lệ
  ALU / băng thông DRAM.
- **Lỗi cài sẵn:** lật bit trạng thái giữa tiến và lùi; sai khóa vòng khi lùi; bỏ qua một khối khi tiến; làm hỏng
  checkpoint; mỗi loại phải ra FAIL.
- **Báo cáo và UI:** dòng `COMPUTE SWEEP` (phép tính device-verified/s, số pass, checkpoint CPU đã kiểm); report
  `async_compute.compute_sweep` và `detection.coverage.device_verified_compute_ops`, tách khỏi số CPU-verified.
- **Rủi ro:** bằng chứng yếu hơn phần CPU kiểm toàn bộ (vì vậy báo tách); kernel mỗi lần gửi phải ngắn (≤ ~50 ms) để
  bộ điều khiển còn phản ứng; chỉ CUDA lúc đầu (Vulkan chưa có bộ quét).

**Cần chốt khi thảo luận:** hàm trộn cụ thể và R; chính sách duty (cố định hay theo `VERIFIER_BOUND`); có thay hẳn
pattern đơn giản của bộ quét DRAM hiện tại hay chạy xen; ngưỡng chấp nhận.

**Các bước:** C10a hàm trộn và nghịch đảo trong `gpu-engine` cùng test thuộc tính (lùi ∘ tiến = đồng nhất, đổi 1 bit
lan khắp khối); C10b kernel CUDA tiến/lùi + kiểm, gắn vào stream của bộ quét; C10c chính sách duty, checkpoint CPU, báo
cáo, UI; C10d đo trên máy thật.

**Thứ tự đề xuất:** đo COMBINED v1.2.018 trên một máy thật trước, để biết sau C7 + `nice 10` GPU còn chờ CPU tới đâu.
Rồi C9 (tăng phần CPU-verified, chuẩn mạnh nhất) trước C10 (device-verified, lấp phần rảnh còn lại).

## Quyết định

| Chủ đề | Quyết định | Trạng thái |
|---|---|---|
| Thời lượng | Giữ nguyên, tăng độ nặng | đã chốt (2026-10-13) |
| Throttle | Không ảnh hưởng kết quả, giữ chính sách hiện nay | đã chốt (2026-10-13) |
| Version | v1.2.xxx, xxx tự tăng theo commit, hiện trên UI | đã chốt (2026-10-13) |
| C2 | Đầu phiên tự đo để chia worker RAM và worker tính toán; xếp luồng theo loại lõi (P/E) | đã chốt (2026-10-13) |
| C4 | Tự chọn AVX2/AVX-512 theo CPU là mặc định; `dit_isa=` ẩn cho kỹ thuật viên | đã chốt (2026-10-13) |
| C6 combined | Deep: A+B, ngân sách cố định khoảng 20% (12 phút) lấy từ pha chạy chồng; Bounded: không xoay; Soak: mỗi chu kỳ một lượt A+B; E-core xoay theo cụm | đã chốt (2026-10-13) |
| C6 sau đánh giá | B chạy cùng GPU, SSD, chain (chỉ CPU-RAM nghỉ), ngân sách ghi SSD chia giữa pha chạy chồng và B; Bounded xoay A trên 2 lõi xếp hạng cao nhất (20 s); chỉ A ghi NOTE `no_boost` | đã chốt (2026-10-14) |
| C6 boost | Bước không đạt gần xung tối đa ghi NOTE "không đạt boost", không tính FAIL | đã chốt (2026-10-13) |
| C6 menu [1] CPU + RAM | Full: A đầu phiên, khoảng 6 phút trong 30 phút; Safe: không xoay | đã chốt (2026-10-13) |
| C6 menu [4] | Power-sweep giữ nguyên | đã chốt (2026-10-13) |
| Pha chạy chồng `max` | CPU-RAM `nice 10` (không dành luồng riêng cho GPU: dành luồng thì GPU hồi phục ít hơn và mất nửa oracle) | đã chốt (2026-10-15) |
| C7 | K tự tăng khi CPU kiểm là giới hạn, chỉ tăng, jobs × K giữ nguyên, tối đa 4096 | đã chốt (2026-10-15) |
| C8 | INT8 Tensor vào chế độ 1 và COMBINED sau bằng chứng RTX 5060 Ti; chế độ 3 = không INT8 | đã chốt (2026-10-15) |
| C9, C10 | Giữ chuẩn: CPU-verified chỉ khi CPU so toàn bộ với oracle độc lập; device-verified báo riêng. Không dùng tổng kiểm tra kiểu ABFT/Freivalds, FP16/BF16 hay thuật toán không có đáp án chính xác từng bit | đã chốt hướng (2026-10-15), chi tiết cần thảo luận |
| V0b | Version trong report và `fail-summary.csv`; file cũ không chuyển đổi, thiếu `dit_version` (trước 2026-10-13) là v1.1; CSV dạng cũ đổi tên `fail-summary-v1.1.csv` | đã chốt (2026-10-13) |

## Thứ tự triển khai

1. V0b: version trong report và CSV (`done`, v1.2.004).
2. C1: sinh pattern theo word (`done`, v1.2.006).
3. C3 + C2: kernel nhiều chuỗi (C3 `done`, v1.2.007) và cách chia worker mới (C2 `done`, v1.2.008–009).
4. C4: binary v3/v4 và cơ chế tự chọn (`done`, v1.2.010).
5. C5: các kernel mới (`done`, v1.2.011).
6. C6: xoay từng lõi (`done`, v1.2.012). B không cần sửa lớp GPU (đã đo).
7. C7 + C8: K tự tăng, INT8 trong test đầy đủ (`done`, v1.2.018).
8. Đo COMBINED v1.2.018 trên máy thật, rồi C9 → C10 sau khi thảo luận.

Mỗi bước kết thúc bằng: `make test`, đo trên host, kiểm QEMU nếu đụng tới image, và cập nhật bảng
hạng mục ở trên.

## Việc tồn đọng từ trước (chưa xếp lịch)

- CPU-NVMe `matrix_rounds` ramp ([../DURATION_COMPLEXITY_RAMP.md](../DURATION_COMPLEXITY_RAMP.md)).
- FC8 đang tạm gác ([../FAULT_CATCH_ROADMAP.md](../FAULT_CATCH_ROADMAP.md)).
- Cách hiển thị "RAM 1 (1 susp)" làm người vận hành bối rối, đã gác từ 2026-10-09.
