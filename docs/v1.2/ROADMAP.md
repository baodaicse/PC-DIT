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
| C9 | GPU: GEMM nhiều tầng trên dữ liệu VRAM, CPU vẫn kiểm từng word (M khả nghịch mod 2, xem lại 2026-10-20) | `planned` (thảo luận) |
| C10 | GPU: compute sweep khả nghịch, GPU tự kiểm, CPU kiểm lại theo checkpoint | `planned` (thảo luận) |
| C11 | Cân bằng tải GPU / CPU-RAM trong pha chạy chồng: đo tỉ lệ cân bằng, ngân sách băng thông RAM, điều phối | `parked` (phức tạp) |
| C12 | Nhịp RAM như trước v1.2 trong pha chạy chồng: worker RAM gần 1/3 bus, mọi luồng còn lại chạy oracle | `done` (v1.2.021) |
| C13 | Cấp corpus kịp cho GPU nhanh: G1 `gpu-vram-dit` theo mức ISA, G2 kho corpus không bao giờ chờ, G3 gỡ kiểm toàn vẹn và nạp corpus khỏi luồng điều phối | `done` (v1.2.024–026), đã kiểm thực địa |
| C14 | Chuỗi GEMM có đáp án biết trước (M^L = Π) | `parked` |
| C15 | Sửa sau kết quả thực địa v1.2.023–026: lỗi cấp phát là INVALID, đọc cảm biến không chặn GPU, không chạy lại CPU-RAM trong COMBINED | `planned` |
| C16 | GPU nhanh: bộ điều khiển tải giữ đủ batch trên GPU, batch lớn hơn khi nhiều VRAM, nhãn bottleneck đúng | `planned` |

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

### Kết quả thực địa của C12 và đo vòng lặp GPU (v1.2.023, 2026-10-18)

- **C12 trên hai máy mới** (v1.2.022, COMBINED DEEP `max`, so với pha nền GPU của chính phiên):
  - B760M i5-14400F + RTX 5060: 4 RAM + 12 oracle; GPU 136 % phép tính, lane 45 %;
  - Z890M Ultra 7 265K + RTX 5070 Ti: 6 RAM + 14 oracle; GPU 96 % phép tính, lane 58 %.

  Để so: máy MAG ở v1.2.019 chỉ đạt 46 % và 15 %. Chưa có phiên cũ trên hai máy này nên chưa so cùng máy được.
- **TUF Z790 i9-14900K + RTX 5080 kém hơn v1.1 trên chính máy đó:** pha chạy chồng đạt 0,60 T/s và lane 755 MiB/s,
  so với 0,9 T/s và 1180 MiB/s ở v1.1. Bài thử `balanced` (7 luồng kiểm riêng cho GPU, 14 luồng không bị CPU-RAM
  chiếm) cho kết quả gần như y hệt, nên nguyên nhân không phải thiếu luồng CPU. Triệu chứng đo được: vòng lặp của
  `gpu-vram-dit` nằm ngoài pipeline tính toán 47–56 % thời gian ở pha chạy chồng và pha xoay B (100–130 ms mỗi burst),
  so với 2 ms ở pha nền và 12 ms ở v1.1 cùng máy; pha B hầu như không có tải CPU-RAM mà vẫn bị. Link Gen5 x16 ổn định,
  không có AER, log kernel sạch. Khác biệt so với các lần v1.1 chạy tốt trên máy này: SSD mới (Patriot P410 thay Apacer)
  và mức ghi HEAVY.
- **v1.2.023 đo từng bước của vòng lặp** và ghi vào report, `bottleneck.jsonl` và journal hộp đen
  ([GPU_BOTTLENECK_LOG.md](../gpu-vram/GPU_BOTTLENECK_LOG.md), mục "Thời gian của vòng lặp generation"). Chỉ thêm số
  đo, không đổi cách test. Bước tiếp theo: một phiên COMBINED trên TUF với bản này để biết bước nào chiếm phần thời gian
  ngoài pipeline, rồi mới bàn cách sửa.

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

- **Điều kiện để lỗi không bị nuốt (sửa 2026-10-20).** Điều kiện cũ "mọi hệ số của M_s và B là số lẻ" **sai**: nó chỉ
  xét một bước lan. Khi mọi hệ số lẻ thì M ≡ J (ma trận toàn 1) theo mod 2, và vì K chẵn nên J·J ≡ 0 mod 2; một lỗi
  lật bit 31 ở một tầng biến mất hẳn sau thêm 2 tầng, và cứ 2 tầng dữ liệu mất thêm một bit thấp (dữ liệu suy biến về
  0, đúng cảnh báo "nhân lặp mod 2³² suy biến"). Điều kiện đúng:
  - mỗi M_s **khả nghịch theo mod 2** (định thức lẻ), ví dụ tích của ma trận tam giác dưới và trên có đường chéo lẻ,
    hệ số ngoài đường chéo ngẫu nhiên, nhân thêm một hoán vị. Khi đó X ↦ X·M_s là song ánh trên Z/2³², không lỗi nào
    ở tầng giữa bị triệt tiêu và dữ liệu không suy biến;
  - B **ngẫu nhiên** (không ép lẻ), mỗi hàng có ít nhất một hệ số lẻ. Vì B chiếu K chiều xuống N < K chiều nên luôn có
    lỗi lọt qua phép chiếu: lỗi ở tầng cuối hoặc ở output luôn lộ ra, lỗi ở tầng sớm hơn lọt với xác suất khoảng 2^-N
    (2^-64 với N = 64). Phép so của CPU vẫn chính xác từng word với oracle chính xác; phần xác suất chỉ là việc một
    lỗi bên trong có đi tới output hay không, như một bit lật trong thanh ghi rồi bị ghi đè.
  - Mô phỏng (K = 8, N = 4, L = 4): M toàn lẻ làm mất 20/20 lỗi bit 31, mọi ô của X₄ có ít nhất 2 bit thấp bằng 0;
    M khả nghịch mod 2 giữ 20/20. Ở bước chiếu, B toàn lẻ làm mất 52 % lỗi bit 31 từ tầng 1, B ngẫu nhiên 6,5 %
    (2^-4 = 6,25 %).
  - Phương án không có bước chiếu (output là X_L, N×K) thì không lỗi nào lọt, nhưng output lớn gấp K/N lần nên chỉ
    tăng phép tính trên mỗi word CPU so khoảng L lần (thay vì khoảng 65 lần). Chọn khi thảo luận C9a.
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
máy RTX PRO 6000 hoặc RTX 4090, GPU nhanh nhất trong kết quả thực địa, sau C16).

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

### C11 — Cân bằng tải trong pha chạy chồng (`parked`, 2026-10-16)

**Bằng chứng (v1.2.019, MAG X870E 9950X3D + RTX 5060 Ti, journal hộp đen).** Baseline GPU: bận 88–93 %, 93–98 W,
tiến trình GPU dùng 3,8 lõi. Pha chạy chồng `max`: GPU bận dao động 0–96 % (trung bình khoảng 35 %, kiểu chạy–dừng),
khoảng 70 W, trong khi tiến trình GPU nhận **4,2–4,4 lõi** (nhờ `nice 10`) và CPU-RAM dùng 22,4 lõi. Thời gian CPU
không còn thiếu: mỗi giây CPU của phía GPU (kiểm output, sinh và so pattern của lane, nhận DMA) làm được ít việc hơn vì
băng thông RAM và L3 bị worker RAM chiếm. Máy dev (3700X + GTX 960) không tái hiện được: GPU bận khoảng 88 % ở mọi tỉ
lệ RAM/oracle; nhưng bớt worker RAM (12 + 4 → 4 + 12) thì K không cần tăng (4096 → 256), PCIe đã kiểm tăng 4,6 lần và
oracle tính gấp đôi, CPU vẫn bận 100 %.

**Hướng đã bàn (gác lại vì phức tạp):**

- Đo trước: tỉ lệ hiệu quả của từng lớp (pha chạy chồng ÷ baseline của chính nó) trong `combined-report.json` và trên
  màn kết quả (`OVERLAP BALANCE`); journal ghi % GPU bận trung bình, nguyên nhân giới hạn, K, phép tính đã kiểm, và
  GiB/s của CPU-RAM, để cả phiên sập cũng còn dữ liệu. Mục tiêu đề xuất: GPU ≥ 70 % phép tính và ≥ 85 % công suất so
  với baseline, CPU-RAM ≥ 50 % GiB/s.
- L1 ngân sách băng thông RAM trong pha chạy chồng (worker RAM chiếm 60–70 % băng thông đo bởi C2b, còn lại oracle);
  L2 điều phối vòng kín (combined đọc trạng thái GPU, chỉnh nhịp worker RAM); L3 build phần host của GPU theo ISA như
  C4; L4 GPU tự lấp thời gian rảnh (duty sweep, C10); L5 bỏ iGPU khỏi pha chạy chồng; L6 tách CCD; L7 cửa sổ xen kẽ
  GPU/RAM; L8 nới trần 4 luồng kiểm.
- Thử nhanh không cần code: COMBINED BOUNDED `max`, `balanced`, `max` + `dit_isa=v3` trên cùng một máy thực địa.

### C12 — Nhịp RAM như trước v1.2 (`done`, v1.2.021)

Hướng thay thế, đơn giản hơn C11: **bỏ mục tiêu ép kịch bus RAM**, đưa phần RAM của CPU-RAM về khoảng nhịp của v1.1,
giữ mọi cải tiến khác của v1.2 (C2–C8, `nice 10`). Lý do: v1.1 đã PASS nhiều phiên DEEP `max` trên chính máy MAG;
cái làm tải lệch là lượng băng thông RAM C1 + C4 mở ra (RAM ghi + kiểm gấp 3,6 lần), không phải các phần khác.

| Cách | Làm gì | Ưu | Khuyết |
|---|---|---|---|
| R1 gỡ C1/C4 cho đường RAM | về mã v1.1: sinh từng byte, kiểm 4 lượt | đúng hệt v1.1 | phí CPU vào việc thừa; gỡ ngược nhiều commit (schema, cấu trúc kiểm); không khuyến nghị |
| R2 giữ mã C1, giới hạn nhịp từng worker | mỗi worker RAM giữ một tốc độ trần (ví dụ ngang mức v1.1), nghỉ xen giữa các khối 64 KiB | số worker, phân arena, độ phủ mỗi round như cũ; một hằng số | thêm một cơ chế nhịp trong engine; luồng nghỉ thì CPU bớt bận, trừ khi oracle lấp |
| R3 giữ mã C1, bớt worker RAM | chọn số worker RAM cho ra băng thông ngang v1.1 (một worker C1 ≈ 6–7 worker cũ), các luồng còn lại chạy oracle | chỉ đổi cách chia luồng, không đụng engine; CPU vẫn 100 %, các luồng còn lại chạy thêm oracle có kiểm | CPU nóng và tốn điện hơn v1.1; không phải lý do bớt tải (throttle chỉ là thông tin) |

- Máy dev, cùng bài với C11: 2 worker RAM + 14 oracle cho RAM ghi + kiểm 2,3 GiB/s (v1.1: 2,6), GPU bận 90 %, K giữ
  256, PCIe đã kiểm khoảng 2,3 GiB/s (pha chạy chồng v1.1: khoảng 2,0), oracle 345 GFLOP/s.
- Trên máy thực địa, v1.1 vẫn để GPU tụt (25–45 % baseline) vì worker RAM khi đó chiếm mọi luồng ở cùng độ ưu tiên;
  nay có `nice 10` và K tự tăng, nên kỳ vọng tốt hơn cả v1.1 lẫn v1.2.019. Cần một phiên thực địa để xác nhận.
- Cái mất: RAM ghi + kiểm mỗi giờ về mức v1.1 (khoảng 1/3,6 hiện nay), ép bộ điều khiển bộ nhớ ít hơn. Cái được thêm:
  thời gian mỗi byte nằm yên giữa ghi và kiểm dài lại khoảng 3,4 lần (nhạy hơn với lỗi lưu giữ), RAM bớt nóng.
- **Đã rõ (2026-10-16):** các luồng được giải phóng chạy oracle (kernel có kiểm của C3/C5), không để nghỉ, không đổi
  sang kernel nhẹ: combined nhằm chạy được nhiều thuật toán có kiểm nhất cùng lúc.
- **Đã chốt (2026-10-17):** R3, chỉ trong pha chạy chồng của COMBINED. Baseline CPU-RAM và menu [1] CPU + RAM giữ C2b
  lấp đầy bus.

**Đã làm (v1.2.021).**

- **Quy tắc** (`combined::split_overlap_ram`): khi lớp CPU-RAM của baseline kết thúc, combined đọc các bước đo C2b trong
  report của nó (`worker_split.steps`) và chọn số worker RAM có tốc độ đo **gần 1/3 đỉnh nhất** (`OVERLAP_RAM_BUS_SHARE`;
  hòa thì lấy ít worker hơn). Mọi luồng còn lại của CPU-RAM chạy compute-oracle; tổng số luồng của CPU-RAM không đổi.
  Áp dụng cho cả `max` (CPU-RAM nhận mọi luồng, `nice 10` như cũ) và `balanced` (luồng của GPU, SSD, chain, OS giữ nguyên).
  Trước baseline, hoặc khi report không có phép đo (crash, chia cố định): 1 worker RAM cho 8 luồng.
- **Lấy bước gần nhất, không lấy số nhỏ nhất vượt 1/3:** mỗi bước đo chỉ 300 ms và số bước nhảy 4 → 6. Trên 3700X, quy
  tắc "nhỏ nhất vượt 1/3" cho 6 worker (4 worker đo 8,5 GB/s, đích 8,6) ở một phiên và 4 ở phiên khác; quy tắc gần nhất
  cho 4 ở cả ba phiên đã chạy. Với phép đo của máy MAG (đỉnh 40,3 GB/s, đích 13,4) cả hai quy tắc đều ra 3.
- **Ví dụ:** MAG X870E 9950X3D, 32 luồng: 3 RAM + 29 oracle (trước: 24 + 8). 3700X, 16 luồng: 4 + 12 (trước: 12 + 4).
- **Report:** `combined-report.json` schema 4, `threads.ram_split`: `source` (`probe` hoặc `per_threads`), đỉnh, mức đích,
  tốc độ đo được của số worker đã chọn. Dòng `THREADS` trên màn hình: `RAM 4 (probe 6.4 of 21.3 GB/s) / ORACLE 12 / ...`.
- **Đo trên máy dev** (3700X + GTX 960, COMBINED BOUNDED `max` thu nhỏ 20 %, pha chạy chồng khoảng 56 s, cùng máy).
  Chạy liền nhau theo cặp: v1.2.020 với v1.2.021 (a), v1.1 (commit `1c8c520`) với v1.2.021 (b). Số worker RAM của
  v1.2.021 là 4 ở lần (a) và 3 ở lần (b): mỗi phiên tự đo đầu phiên và chọn bước gần 1/3 đỉnh nhất. Phần trăm so với
  baseline GPU của chính phiên đó.

| Pha chạy chồng | v1.1 | v1.2.020 | v1.2.021 (a) | v1.2.021 (b) |
|---|---|---|---|---|
| Chia luồng RAM + oracle | 12 + 4 | 12 + 4 | 4 + 12 | 3 + 13 |
| GPU, phép tính đã kiểm | 157 G/s (95 %) | 172 G/s (102 %), K phải tăng tới 4096 vì CPU kiểm không kịp | 171 G/s (99 %), K 256 | 172 G/s (100 %), K 256 |
| Lane VRAM/PCIe do CPU kiểm | 906 MiB/s (50 %) | 1018 MiB/s (55 %) | 1173 MiB/s (64 %) | 1231 MiB/s (67 %) |
| Sweep DRAM GPU tự kiểm | 7,07 GiB/s (94 %) | 6,97 GiB/s (91 %) | 7,62 GiB/s (99 %) | 7,77 GiB/s (98 %) |
| GPU bận, công suất | 85 %, 94 W | 86 %, 96 W | 89 %, 99 W | 90 %, 100 W |
| Compute-oracle | 7 GFLOP/s (kernel một chuỗi) | 69 GFLOP/s | 156 GFLOP/s | 165 GFLOP/s |
| RAM ghi + kiểm | 2,5 GiB/s | 9,3 GiB/s | 4,2 GiB/s | 3,3 GiB/s |
| Một round (arena 8–9 GiB) | 7,0 s | 1,7 s | 3,9 s | 5,7 s |
| Kết quả | PASS | PASS | PASS | PASS |

  So với v1.1, RAM ghi + kiểm của C12 cao hơn khoảng 30 % và thời gian một round gần như cũ (5,7 so với 7,0 s), nên độ
  nhạy với lỗi lưu giữ giữ được. Khác ở cách đọc: v1.1 đọc mỗi arena 4 lượt khi kiểm (so sánh, hai reduction, SHA-256),
  v1.2 đọc 1 lượt và băm 4 round một lần; con số "RAM ghi + kiểm" ở cả hai bản đều đếm mỗi byte một lần, nên lưu lượng
  thật qua bus của v1.1 cao hơn con số trong bảng (chưa đo trực tiếp). Cùng pattern `CPU-DIT-PATTERN-v1`, round nào cũng
  so đủ từng byte.

- **Thực địa, MAG X870E 9950X3D + RTX 5060 Ti** (COMBINED DEEP `max`, cùng máy, arena khoảng 16 GiB):

| Pha chạy chồng | RAM ghi + kiểm | Một round |
|---|---|---|
| v1.1, 5 phiên PASS (2026-10-07 … 10-08) | 5,6–7,5 GiB/s | 4,3–5,7 s |
| v1.2.019 (2026-10-16) | 14,6 GiB/s | 2,1 s |
| v1.2.021, 3 RAM + 29 oracle | chưa đo; ước tính 6–8 GiB/s từ phép đo đầu phiên | chưa đo |

  Máy dev không tái hiện được tình trạng GPU bị bỏ đói của máy thực địa (GPU vẫn bận khoảng 85–90 % ở mọi bản), nên
  phần GPU cần một phiên COMBINED trên máy MAG để xác nhận: so với v1.2.019 (GPU 46 % phép tính, lane 15 % baseline) và
  với pha xoay B của cùng phiên (81 %, 90 %).

### C13 — Cấp corpus kịp cho GPU nhanh (`done`, v1.2.024–026, 2026-10-18)

**Bằng chứng** (v1.2.023, đo vòng lặp; pha chạy chồng `max`):

| | TUF i9-14900K + RTX 5080 | MAG 9950X3D + RTX 5060 Ti |
|---|---|---|
| Chờ CPU dựng corpus | 33 % | 27 % |
| Nạp corpus lên VRAM (chặn luồng điều phối) | 4,8 % | 4,0 % |
| Kiểm toàn vẹn corpus (chặn luồng điều phối) | 5,9 % | 5,8 % |
| Vòng burst | 49 % | 59 % |
| GPU bận trong vòng burst | 59 % | 87 % |
| GPU bận trên cả pha | 29 % | 52 % |
| CPU dành cho dựng corpus | 0,83 lõi | 0,82 lõi |

GPU dùng hết một corpus (256 batch) trong 0,2–0,5 s; CPU dựng một corpus FP32 mất 1,5 s vì `gpu-vram-dit` chỉ build cho
x86-64 cơ bản, `mul_add` thành lời gọi hàm thư viện cho từng phần tử. Đo trên 3700X, 4 luồng, corpus 128 job × K 4096:

| Dựng một corpus | SSE2 (hiện nay) | AVX2+FMA |
|---|---|---|
| u32 | 0,21 s | 0,20 s |
| FP32 | 1,47 s | 0,23 s |
| INT8 | 0,11 s | 0,05 s |
| Cả vòng u32 → FP32 → INT8 | 1,79 s | 0,48 s (−73 %) |

**Ba bước** (không đổi chuẩn kiểm: CPU vẫn so toàn bộ từng word với oracle độc lập):

- **G1 — `gpu-vram-dit` theo mức ISA.** Build thêm `gpu-vram-dit-v3` và `-v4`, bản cơ bản chọn bản rộng nhất CPU chạy
  được rồi `exec` sang, như C4 làm cho `dit-cli` (chung `dit_system::isa`, chỉnh tay bằng `DIT_ISA` / `dit_isa=`).
  Thuật toán và thứ tự cộng của oracle không đổi, nên corpus và đáp án trùng từng bit giữa các bản; một test bắt buộc
  so digest của corpus giữa các bản.
- **G2 — kho corpus, GPU không bao giờ chờ.** Giữ vài corpus đã dựng trong RAM; bộ dựng chạy nền, xong cái mới thì thay
  cái cũ nhất. Đến lượt đổi mà cái mới chưa xong thì dùng cái kế trong kho thay vì đứng chờ. Mọi batch vẫn so toàn bộ
  với oracle; report ghi số lần đổi sang corpus trong kho (dùng lại) để thấy khi nào CPU không theo kịp. Kho tính vào
  ngân sách RAM của GPU, tự nhỏ lại trên máy ít RAM.
- **G3 — gỡ việc chặn luồng điều phối.** Kiểm toàn vẹn corpus định kỳ chạy trên luồng phụ (batch sai vẫn kiểm ngay như
  cũ); corpus kế tiếp được chuẩn bị sẵn để lúc đổi chỉ còn nạp lên VRAM.

**G1 đã làm (v1.2.024).** `dit_system::isa::exec_build_for_cpu` dùng chung cho `dit-cli` và `gpu-vram-dit`; extension
cài `gpu-vram-dit-v3` / `-v4` (khoảng 3,9 MiB mỗi bản) và hash chúng trong manifest; `tools/test_gpu_isa.sh` (cũng là
`make test-gpu-isa`, chạy mỗi lần build extension) so digest input và đáp án của 9 corpus u32 / FP32 / INT8, tới cỡ
128 × K 4096, giữa bản cơ bản và các bản máy chạy được: trùng hết trên 3700X (v4 chỉ kiểm được trên máy có AVX-512).
Lệnh ẩn `gpu-vram-dit corpus-digest` phục vụ phép so này. Report GPU ghi `cpu_isa`.

**G2 đã làm (v1.2.025).** `--corpus-pool N` (chỉ async, mặc định 3, tối đa 16): ngoài corpus đang dùng, pipeline giữ
N corpus đã dựng. Đến lượt đổi mà corpus dựng nền chưa xong thì lấy corpus cũ nhất trong kho (đẩy cái đang dùng vào
cuối kho) thay vì đứng chờ; corpus mới xong thì vào dùng và cái vừa thay vào kho, kho đầy thì bỏ cái cũ nhất. Kho
không đổi chuẩn kiểm: corpus nào cũng giữ input, đáp án oracle và SHA-256 lúc dựng, mỗi batch trộn generation riêng
và so toàn bộ output với đáp án như mọi batch khác (mỗi corpus vốn đã được tính lại 256 batch). Đổi hình dạng (C7 tăng
K) thì xóa kho, và một corpus dựng sẵn sai hình dạng không bao giờ được nạp (dựng nguội lại). Ngân sách RAM của GPU
tính kho (`corpus_pool`, `corpus_pool_bytes` trong `workspace_budget`) và bỏ kho trước tiên khi thiếu RAM, trước mọi
bậc backoff khác. Report có `async_compute.corpus_pool_serves` (số lần đổi sang corpus trong kho: lớn nghĩa là CPU
dựng không kịp GPU); thời gian oracle và sinh input chỉ cộng cho corpus mới dựng. Đo trên máy dev (GTX 960, 3700X,
40 s, `--compute-mix alternate`, 128 job × K 4096, đổi corpus mỗi 2 batch để CPU chắc chắn không kịp):

| | Không kho (`--corpus-pool 0`) | Kho 3 corpus |
|---|---|---|
| Phép tính đã kiểm | 128 G/s | 223 G/s (+74 %) |
| Chờ CPU dựng corpus | 17,1 s (43 %) | 0,2 s (1 %) |
| Nạp corpus lên VRAM | 1,6 s | 2,7 s (nạp nhiều lần hơn: việc của G3) |
| Vòng burst | 19,7 s (49 %) | 35,4 s (88 %) |
| Corpus mới dựng / lấy từ kho | 37 / 0 | 32 / 33 |
| RAM của kho | — | 786 MiB |
| Verdict | PASS | PASS |

Lỗi chèn khi có kho vẫn FAIL đúng. Các test phần cứng đếm số lần xoay theo lịch (`fp32.rs`, `tensor.rs`) chạy với
`corpus_pool: 0`.

**G3 đã làm (v1.2.026).** Ba việc rời luồng điều phối; chuẩn kiểm không đổi:

- **Kiểm toàn vẹn định kỳ trên luồng phụ.** Cứ 512 batch, lần băm lại corpus phía host chạy trên luồng
  `gpu-corpus-integrity` cạnh các burst kế tiếp; kết quả được lấy ở ranh giới burst sau, hoặc cuối phiên
  (`finish_async`, gpu-cli gọi một lần trước khi tổng kết, nên lần kiểm cuối không bị mất). Batch sai vẫn được kiểm
  ngay, trên luồng điều phối, trước verdict của burst, để gán `corpus_intact` / `host_memory_changed` như cũ. Corpus
  nạp từ ngoài (dit-chain, đổi mỗi bước) vẫn kiểm như cũ. Thay đổi trong RAM host mà chỉ lần kiểm định kỳ thấy được
  báo trễ thêm tối đa một burst; verdict không đổi.
- **Corpus kế tiếp chép sẵn vào staging.** Corpus mà lần đổi sắp tới sẽ lấy (corpus mới khi đã dựng xong; khi có kho
  và corpus mới chưa xong thì corpus cũ nhất trong kho, từ hai burst trước lượt đổi) được chép vào vùng staging pinned
  trên luồng `gpu-corpus-stage` trong lúc burst chạy; lúc đổi chỉ còn DMA lên VRAM. Không thêm RAM hay VRAM: dùng lại
  staging sẵn có, và luồng điều phối chỉ đụng tới staging sau khi luồng chép đã xong (join).
- **Giải phóng corpus trên luồng phụ.** Giải phóng một corpus 264 MB (rời kho, bị thay) tốn khoảng 25 ms.

Report: `timing.integrity_background_seconds` (thời gian kiểm trên luồng phụ); `integrity_check_seconds` giờ chỉ là
thời gian luồng điều phối chờ kiểm (sau batch sai); `corpus_staged_installs` (lần nạp chỉ còn DMA). Gate phần cứng mới
`crates/gpu-cuda/tests/corpus_pipeline.rs`: đổi một bit corpus phía host (bản trên GPU vẫn đúng) thì lần kiểm nền bắt
được và gán `host_memory_changed`, không batch nào FAIL, luồng điều phối không chờ lần kiểm nào; corpus chép sẵn được
kiểm như mọi corpus và lỗi chèn vẫn FAIL. Đo trên máy dev (GTX 960, 3700X, 40 s, 128 job × K 4096, `alternate`, kho
3; G2 = v1.2.025; mỗi ô 2 lần chạy xen kẽ, lệch nhau dưới 1,5 %):

| | Đổi corpus mỗi 256 batch (CPU theo kịp), G2 → G3 | Mỗi 2 batch (CPU không kịp), G2 → G3 |
|---|---|---|
| Phép tính đã kiểm | 233 → 238 G/s | 224 → 235 G/s |
| Luồng điều phối bị chặn (chuẩn bị + sau vòng) | 1,39 → 0,50 s | 4,25 → 2,37 s |
| Vòng burst | 96 → 99 % | 89 → 94 % |
| Lần đổi chỉ còn DMA | 8 / 8 | 32 / 67 |
| Verdict / lỗi chèn | PASS / FAIL | PASS / FAIL |

GTX 960 chậm nên trên máy dev GPU vẫn là giới hạn và mức tăng nhỏ. Khi đổi mỗi 2 batch, corpus mới hay xong trong burst
cuối trước lượt đổi, sau khi corpus trong kho đã được chép, nên chỉ một nửa số lần đổi trúng; ở thực địa một corpus kéo
dài khoảng 8 burst nên corpus đã xong có nhiều ranh giới burst để được chép. **Ước tính cho TUF** (pha chạy chồng
v1.2.023): kiểm toàn vẹn 5,9 % + giải phóng corpus 0,8 % + phần chép của nạp corpus (khoảng 60–75 % của 4,8 % trên PCIe
5.0) trả về vòng burst khoảng 9 % thời gian pha, cộng với G2 (chờ corpus 33 % → khoảng 1 %); pha GPU chạy riêng khoảng
8 %. **Cái giá:** hai luồng phụ ngắn làm đúng phần việc đã rời luồng điều phối (chép khoảng 22 ms mỗi lần đổi, băm
khoảng 0,2 s mỗi 512 batch); một lần chép thừa khi corpus được chép sẵn không được lấy (tối đa một lần mỗi lượt đổi).

**Ước tính** (kiểm lại bằng phiên thực địa): GPU bận trên cả pha TUF 29 % → khoảng 53 %, MAG 52 % → khoảng 78 %; phép
tính đã kiểm ×1,5–1,8; CPU cho dựng corpus 0,83 → khoảng 0,35–0,40 lõi (lõi-giây mỗi corpus −73 %), phần trả lại về
oracle CPU-RAM. **Cái giá:** image thêm khoảng 7,4 MiB; kho corpus khoảng 270 MiB mỗi corpus ở K 4096 (0,5–1 GiB) lấy từ
ngân sách RAM của GPU; lỗi RAM host trong corpus được báo trễ tối đa khoảng một lần đổi corpus (verdict không đổi). Sau
C13, giới hạn kế tiếp là độ bận trong vòng burst. (Thực địa 2026-10-20: không phải trần luồng kiểm mà bộ điều khiển tải
chỉ cho 1 batch trên GPU, xem C16.)

### Kết quả thực địa của C13 (v1.2.026, 2026-10-20)

`Result/2010`: 25 phiên v1.2.026 trên 7 máy. Pha chạy chồng `max`, v1.2.023 (`Result/1810`, `Result/1910`) → v1.2.026:

| Máy | Phép tính đã kiểm | Chờ corpus | Vòng burst | GPU bận |
|---|---|---|---|---|
| TUF i9-14900KF + RTX 5080 | 1,06 → 1,39 T/s (+31 %) | 33,3 → 0 % | 48,7 → 60,1 % | 29 → 35 % |
| Z790 Lightning i7-14700K + RTX 3060 | 0,82 → 0,92 T/s (+12 %) | 2,9 → 0 % | 82 → 90 % | 75 → 85 % |

Máy mới, chỉ có v1.2.026: RTX PRO 6000 (9950X) 1,65–1,68 T/s, vòng burst 90–92 %, GPU bận 24 %; RTX 4090 (9950X)
2,0 T/s, 90 %, 38 %; RTX 4090 (7500F) 2,2 T/s, 86 %, 43 %; RTX 4050 laptop 0,66 T/s, 90 %, 81 %; RTX 3050 (Xeon E5-2696
v3) 0,41 T/s, 84 %, 76 %.

- **Đúng như dự tính:** chờ corpus khoảng 0 % trên mọi máy; luồng điều phối không còn kiểm toàn vẹn (trên Xeon, lần kiểm
  nền tốn 718 s trong pha 1675 s, trước G3 phần này chặn thẳng luồng điều phối); gần như mọi lần đổi corpus chỉ còn DMA
  (`corpus_staged_installs` bằng số lần nạp); bản `gpu-vram-dit` được chọn là v4 trên Zen 4/5, v3 trên Intel và Haswell.
- **TUF thấp hơn ước tính** (vòng burst 60 % thay vì khoảng 90 %) vì một lỗi khác: đọc cảm biến chiếm 35 % pha, xem C15b.
- **Giới hạn kế tiếp (sửa lại 2026-10-20):** report ghi `verifier_worker_ceiling` cả pha trên mọi GPU nhanh (RTX PRO
  6000, 4090, 5080), nhưng CPU so không phải là giới hạn: hàng đợi kiểm rỗng, luồng kiểm chỉ dùng 0,14–0,2 lõi. Thứ giữ
  GPU ở mức bận 24–43 % là bộ điều khiển tải, kẹt ở 1 batch trên GPU; nhãn trên chỉ phản ánh việc bộ điều khiển đang
  giữ tải. Chi tiết và hướng sửa ở C16. GPU chậm (3060, 4050, 3050) thì GPU là giới hạn (bận 76–85 %).

### C14 — Chuỗi GEMM có đáp án biết trước (`parked`, 2026-10-18)

GPU nhân X·M·…·M (L tầng) với M dựng sao cho M^L = Π (hoán vị có đổi dấu): đáp án Π·X có sẵn, CPU so toàn bộ mà không
phải dựng oracle. Gác lại vì: (1) chỉ đúng tuyệt đối với u32, FP32 và INT8 mất tính chất này (làm tròn, ép về int8), nên
mất phần ép pipe FP32 FMA và Tensor core; (2) kiểm cả chuỗi chứ không từng tích, nên cùng họ với C10 và phải báo riêng
khỏi CPU-verified; (3) không áp vào data chain: đáp án của chain tính sẵn hai lần lúc nạp và đi qua đường SSD → RAM như
một phần dữ liệu được kiểm, bỏ nó thì chain mất nửa lượng byte và mất FP32.

### C15 — Sửa sau kết quả thực địa v1.2.023–026 (`planned`, 2026-10-20)

Bằng chứng: `Result/1910` (5 phiên v1.2.023) và `Result/2010` (25 phiên v1.2.026). C15a và C15c ảnh hưởng tới
verdict; C15b trả lại vòng burst cho GPU. Mỗi mục một commit, có test.

- **C15a — Lỗi cấp phát là INVALID, không phải FAIL.** Hai phiên FAIL `?PCIe-GPU2|VRAM2` (1 và 11 sự kiện) thực chất là
  `Vulkan call failed in allocate_memory: ERROR_OUT_OF_DEVICE_MEMORY` trên iGPU lúc máy đã hết RAM (`oom_kill`), bị
  ghi thành sự kiện `gpu:transfer` / `gpu:retention`. Theo nguyên tắc INVALID không trộn với FAIL: thiếu bộ nhớ khi cấp
  phát (Vulkan và CUDA, device lẫn host) không tạo fault record; lớp GPU kết thúc INVALID với lý do thiếu bộ nhớ.
  Test: cấp phát lỗi giả lập ra INVALID, ledger không có sự kiện.
  **Đã làm (v1.2.031):** gpu-vram-dit đặt riêng lỗi thiếu bộ nhớ (`ERROR_OUT_OF_DEVICE_MEMORY`,
  `ERROR_OUT_OF_HOST_MEMORY`, `CUDA_ERROR_OUT_OF_MEMORY`) vào `resource_errors` trước khi ghi fault record, dừng chạy
  (`completion_reason` `out_of_memory`) và ra INVALID; lỗi dữ liệu vẫn FAIL dù có thiếu bộ nhớ. Lỗi CUDA giờ in kèm tên
  mã (`status 2 (CUDA_ERROR_OUT_OF_MEMORY)`). dit-chain thiếu bộ nhớ khi nạp corpus hoặc khi kiểm thì dừng vòng đọc,
  INVALID có lý do. Phiên ghi lý do vào lớp GPU (`out of memory: …`, kể cả nhiều GPU). Negative gate mới
  `--inject-hardware-fault out-of-memory`: trên GTX 960 ra INVALID, 0 fault record; `transfer` vẫn FAIL, 1 fault record.
- **C15b — Đọc cảm biến không chặn vòng lặp GPU, không gửi lệnh tới SSD đang test.** TUF (SSD Lexar NM610 PRO, không
  DRAM): bước `host_sample` của `gpu-vram-dit` chiếm 35 % pha chạy chồng (một lần tối đa 3 s), 14 % rotation B, 5,5 %
  pha GPU chạy riêng; các máy khác 1–6 %. Mỗi giây nó đọc mọi chip hwmon, cả nhiệt độ NVMe: mỗi lần đọc là một lệnh
  admin tới SSD, chờ sau hàng đợi ghi của lớp SSD. Đây cũng là hiện tượng "thời gian ngoài vòng lặp" của TUF ngày 17/10
  (Patriot P410). **Sửa:** `gpu-vram-dit` đọc cảm biến trên luồng phụ (vòng lặp lấy giá trị mới nhất) và chỉ đọc chip
  của CPU; rà các nơi khác đọc hwmon theo chu kỳ để SSD đang test không nhận thêm lệnh admin không cần. Kỳ vọng: vòng
  burst của TUF từ 60 % lên khoảng 90 % như các máy khác.
  **Đã làm (v1.2.032):** bộ thu thập chung của `dit-system` không đọc chip hwmon của ổ đĩa (`nvme`, `drivetemp`) và
  thermal zone `nvme*`. Nhờ vậy năm nơi đọc theo chu kỳ đều hết gửi lệnh tới SSD: `gpu-vram-dit`, bộ giám sát an toàn
  của CPU-RAM (mỗi 250 ms), dashboard COMBINED, màn hình CPU-RAM, giao diện SSD. Nhiệt độ SSD vẫn lấy từ SMART của lớp
  SSD; nhiệt độ dùng cho an toàn vốn chỉ là của CPU. `gpu-vram-dit` đọc cảm biến CPU trên luồng phụ mỗi giây, vòng lặp
  chỉ lấy giá trị mới nhất. Máy dev: đọc nhiệt độ NVMe lúc ổ rảnh mất 1 ms (trung vị), tối đa 10,7 ms; bước
  `host_sample` sau khi sửa trung bình 0,13 ms, tối đa 0,16 ms (0,09 % thời gian). Kiểm lại vòng burst của TUF bằng phiên
  thực địa.
- **C15c — Không chạy lại chẩn đoán CPU-RAM trong COMBINED.** MAG 9950X3D (`Result/1910`, phiên 921): CPU-RAM FAIL trong
  pha chạy chồng (4 lần lật 1 bit, `confirm_rewrite_fixed`). Engine tự chạy lại nguyên bài (1670 s) để phân loại; pha
  hết giờ, phiên chờ thêm 900 s rồi drain: report lượt đầu mất (lớp CPU-RAM của pha thành INVALID, verdict vẫn FAIL nhờ
  ledger), phiên dài thêm 15 phút, lượt chạy lại thấy thêm byte sai nhưng mất chi tiết vì không ghi fault record.
  **Sửa:** COMBINED truyền `--no-diagnostic-rerun` cho mọi lần chạy CPU-RAM (pha có giới hạn giờ; mỗi lỗi đã có fault
  record và bước xác nhận tại chỗ); menu [1] giữ rerun. Phụ: lớp báo 8 lỗi (byte sai engine thấy, mỗi lỗi thấy hai lần)
  còn ledger 4; CSV dùng ledger nên đúng, gom vào việc hiển thị "susp" đang gác.
  **Đã làm (v1.2.030):** `combined_layers::cpu_ram` thêm `--no-diagnostic-rerun` cho mọi lần COMBINED chạy CPU-RAM
  (baseline, pha chạy chồng, xoay lõi). Kiểm trên máy dev với lỗi RAM cài sẵn, 8 s: có rerun 16,6 s
  (`reproducible_same_route`), có cờ 8,5 s (`disabled`), cả hai FAIL.

### C16 — GPU nhanh: giữ đủ batch trên GPU (`planned`, 2026-10-20)

**Bằng chứng** (`Result/2010`, pha chạy chồng `max`, v1.2.026):

| | RTX PRO 6000 | RTX 4090 | RTX 5080 | RTX 3060 |
|---|---|---|---|---|
| Batch trên GPU cùng lúc cuối pha (cho phép 3) | 1 | 1 | 1 | 3 |
| Thời gian bộ điều khiển ở `THROTTLED` | 99,8 % | 99,8 % | 99,8 % | 19 % (`BALANCED` 80 %) |
| Lần hạ tải / lần hồi phục xong, cả pha | 66 / 0 | 61 / 0 | 32 / 1 | 121 / 121 |
| GPU bận | 24 % | 38 % | 35 % | 85 % |
| Output cần so | 0,82 GB/s | 0,97 GB/s | 0,68 GB/s | 0,45 GB/s |
| Lõi CPU so + băm | 0,20 | 0,19 | 0,14 | 0,09 |
| Hàng đợi kiểm | rỗng, chờ tối đa 0,01 s | như trái | như trái | như trái |

- Luồng kiểm so 5,4 GB/s mỗi lõi; 4 luồng xử lý được khoảng 21 GB/s, gấp hơn 20 lần lượng output thực tế. CPU so
  **không** phải giới hạn.
- Mỗi batch trên PRO 6000 chỉ có 0,49 ms kernel và 0,13 ms chép về; với 1 batch trên GPU, một vòng gửi → tính → chép
  → kiểm mất 2,6 ms, nên GPU bận khoảng 24 %, khớp số đo. Phần thời gian cố định của mỗi batch (luồng điều phối,
  event, giao cho luồng kiểm, bộ điều khiển) lớn hơn kernel nhiều lần.
- Thỉnh thoảng hàng đợi host chạm ngưỡng cứng (`fill` ≥ 0,9, hoặc đủ 8 slot với `fill` ≥ 0,5), bộ điều khiển hạ về 1
  batch. Nó hồi phục từng bước: pacing giảm dần (×3/4 mỗi bước) rồi mới tăng số batch; mỗi bước phải yên `hold` tick,
  `hold` nhân đôi tới 160 tick (8 s) khi một bước bị theo sau bởi lần đầy mới. Trên GPU nhanh hai lần hạ tải cách nhau
  khoảng 25–50 s, không đủ để đi hết đường hồi phục, nên bộ điều khiển gần như không bao giờ về lại 3 batch (1 lần
  hồi phục xong trên 159 lần hạ tải của ba máy). RTX 3060 hồi phục xong sau mọi lần hạ tải.
- Bộ phân loại ghi `VERIFIER_BOUND` mỗi khi bộ điều khiển đang giữ tải (`backlog_throttled`), kể cả khi hàng đợi
  rỗng, nên nhãn `verifier_worker_ceiling` che mất nguyên nhân thật.

**Các bước** (chuẩn kiểm không đổi: CPU vẫn so toàn bộ output với oracle độc lập):

- **C16a — vì sao hàng đợi chạm ngưỡng cứng.** Ghi mỗi lần đầy (thời điểm, `fill`, slot đang dùng, luồng kiểm đang làm
  gì, việc nền đang chạy: dựng corpus, kiểm toàn vẹn, chép staging, CPU-RAM) vào nhật ký bottleneck, rồi đo trên máy
  dev với tải CPU như pha chạy chồng. Nghi vấn: luồng kiểm bị chậm trong chốc lát khi CPU bị chiếm hết.
  **Đã đo (v1.2.033).** Tái hiện trên máy dev: GTX 960, K 256 (kernel 1,1 ms mỗi batch, gần tỉ lệ kiểm/kernel của RTX
  PRO 6000 ở K 4096), CPU-RAM `nice 10` chiếm cả 16 luồng. Bộ điều khiển cũ nằm ở `THROTTLED` 95–97 % thời gian, kết
  thúc ở 1 batch, 0 lần hồi phục xong, giống thực địa. Nguyên nhân: điều kiện cứng thứ hai "dùng hết 8 slot host và
  `fill` ≥ 0,5" xảy ra ngay trong vận hành bình thường. 3 batch trên GPU + 4 batch đang kiểm đã gần đủ 8 slot, chỉ cần
  2 batch chờ (phòng chờ 4 slot). Lần nào cũng chạm 17–880 ms sau khi vừa lên lại 3 batch, `backlog_fill` đúng 0,5.
  Mỗi lần chạm đưa về 1 batch và một chuỗi hồi phục 6–11 s (`hold` nhân đôi). Luồng kiểm không phải bị chậm lâu: chỉ
  cần một nhịp dồn vài mili giây.
- **C16b — hồi phục tương xứng với nguyên nhân.** Một lần đầy ngắn rồi hàng đợi rỗng lại không làm `hold` leo tới 8 s;
  đi lại tới đủ số batch khi hàng đợi rỗng và kết quả kiểm vẫn về đều. Giữ các bất biến của bộ điều khiển
  ([../gpu-vram/GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md](../gpu-vram/GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md) §2):
  không bỏ kiểm, không lấy mẫu, không giảm K.
  **Đã làm (v1.2.033).** Hàng đợi đầy vẫn dừng gửi ngay như trước, nhưng chỉ là một lần tạm dừng: giữ nguyên số batch
  và pacing; mẫu đầu tiên hàng đợi về dưới `resume` thì chạy lại đúng mức cũ (`resumed`, đếm `pause_events`), không
  tính là lần hạ tải, không làm `hold` tăng. Lần tạm dừng thứ ba trong 1 s nghĩa là luồng kiểm sát giới hạn: giảm một
  nấc bằng đòn bẩy sẵn có (bớt 1 batch, pacing về tốc độ kiểm; `queue_full_overload`). Hàng đợi vẫn đầy sau 500 ms là
  quá tải thật: xử lý như cũ (về 1 batch, hồi phục từng bước). Áp lực RAM host không đổi. Đợt tạm dừng không làm burst
  bị gán `VERIFIER_BOUND` (chỉ `queue_full_overload` mới gán). Mô phỏng quá tải: 3 lần đầy lúc hội tụ, sau đó pacing
  đúng bằng thời gian kiểm và luồng kiểm chạy hết sức. Đo trên máy dev, xen kẽ, mỗi bản 2 lần:

  | 30 s, K 256, CPU-RAM `nice 10` cả 16 luồng | Bản cũ | C16b |
  |---|---|---|
  | Batch/s | 597–609 | 811–816 (+35 %) |
  | GPU bận | 70,9 % | 88,6–89,1 % |
  | Ở `THROTTLED` | 95–97 % | 0 % (`BALANCED` 87–89 %) |
  | Batch trên GPU cuối pha | 1 | 3 |

  Máy rảnh không đổi (K 256: 845 so với 837 batch/s; K 4096: 54 batch/s cả hai). Cả 17 gate phần cứng CUDA qua.
- **C16c — batch lớn hơn trên GPU nhiều VRAM.** Thêm job mỗi batch khi VRAM cho phép (4090 24 GB, PRO 6000 96 GB) để
  kernel mỗi batch dài hơn hẳn phần thời gian cố định. Output và công so tăng cùng tỉ lệ với phép tính, nên tỉ lệ
  phép tính trên mỗi word CPU so không đổi; corpus và oracle lớn theo số job, vẫn chia cho 256 lần tính lại; kho
  corpus tính vào ngân sách RAM như G2. Độ dài kernel mục tiêu đo rồi chốt.
- **C16d — nhãn bottleneck đúng.** Bộ điều khiển giữ tải trong khi hàng đợi rỗng thì ghi một nhãn riêng (bộ điều khiển
  đang giữ), không ghi `VERIFIER_BOUND`.

**Kỳ vọng** (kiểm bằng phiên thực địa): GPU nhanh bận gấp 2–3 lần, toàn bộ vẫn CPU-verified; CPU so tăng tương ứng,
từ 0,2 lên khoảng 0,5 lõi.

### Đã cân nhắc, không làm: kiểm GEMM bằng Freivalds/ABFT (xem lại 2026-10-20)

Đề xuất (từ một phiên phân tích commit c431fd6): GPU tính C = A·B với n lớn (2048–8192), CPU kiểm C·r = A·(B·r) trong
O(n²) thay vì dựng oracle O(n³); thêm chuỗi GPU → GPU (A_{t+1} = mix(C_t)) và các bài toán "tính nặng, kiểm rẻ" khác
(sort kiểm bằng fingerprint, NTT, giải hệ). Không làm, vì:

- **Trái quyết định đã chốt** (C9, C10, 2026-10-15) và nguyên tắc 5: checksum không thay phép so chính xác. Phần "so
  chính xác 64 ô lấy mẫu" là lấy mẫu, không được tính CPU-verified. Muốn đổi phải có ADR.
- **Không tăng tỉ lệ đang cần tăng.** CPU vẫn phải đọc hết n² word output (tính C·r), mỗi word 2–3 phép tính, cùng bậc
  với phép so hiện nay. Phép tính GPU trên mỗi word output bằng 2 × chiều trong: Freivalds n = 4096 cho 8192, GEMM
  hiện nay K = 4096 cũng 8192. Thứ nó tiết kiệm là oracle bậc ba, vốn đã được chia cho 256 lần tính lại và từ C13 không
  còn chặn GPU.
- **Giữ lại hai nhận xét đúng:** nhân lặp mod 2³² suy biến (đã sửa vào điều kiện của C9); và dữ liệu nên đổi giữa các
  bước để không lặp mãi cùng một mẫu (C9 tạo dữ liệu trung gian mới ở mỗi tầng trong VRAM).

**Ý "CPU tính một lần, GPU tính lại nhiều lần":** đó là thiết kế hiện tại (oracle một lần mỗi corpus, GPU tính lại 256
lần với tag generation riêng, CPU so toàn bộ). Để GPU tự so các lần lặp với bản CPU đã kiểm thì phần đó là
device-verified, báo riêng, cùng họ với C10; chỉ cần một lần khác là FAIL, không biểu quyết theo đa số. Chưa cần
khi CPU so còn dư hơn 20 lần (xem C16).

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
| C11 | Gác lại: cân bằng tải bằng đo tỉ lệ + ngân sách băng thông + điều phối là hướng đúng nhưng phức tạp | gác (2026-10-16) |
| C12 | R3 trong pha chạy chồng của COMBINED (`max` và `balanced`): worker RAM theo phép đo C2b của baseline, gần 1/3 đỉnh; mọi luồng còn lại của CPU-RAM chạy oracle, không nghỉ, không đổi kernel nhẹ. Baseline và menu [1] giữ C2b lấp đầy bus | đã chốt (2026-10-17) |
| C13 | G1 + G2 (kho corpus) + G3; chuẩn kiểm không đổi; sau đó C9 cho trần luồng kiểm | đã chốt (2026-10-18) |
| C14 | Gác: chỉ u32, kiểm cả chuỗi (báo riêng), không áp vào data chain | gác (2026-10-18) |
| C15 | C15a → C15c, mỗi mục một commit, trước C9 | đã chốt (2026-10-20) |
| C16 | Sửa bộ điều khiển tải và cỡ batch cho GPU nhanh trước C9; CPU so còn dư hơn 20 lần, không phải giới hạn | đã chốt (2026-10-20) |
| C9, xem lại | M khả nghịch mod 2, B ngẫu nhiên; bước chiếu hay không chốt khi làm C9a | đã chốt hướng (2026-10-20) |
| Freivalds/ABFT, xem lại | Vẫn không làm: trái chuẩn so chính xác và không tăng phép tính trên mỗi word CPU so | đã chốt (2026-10-20) |
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
9. C12: R3 trong pha chạy chồng (`done`, v1.2.021); C11 gác lại.
10. C13: G1 → G2 → G3 (`done`, v1.2.024–026), đã kiểm thực địa (2026-10-20).
11. C15: C15a (lỗi cấp phát là INVALID) → C15b (đọc cảm biến) → C15c (rerun CPU-RAM).
12. C16: C16a (vì sao hàng đợi đầy) → C16b (hồi phục) → C16c (batch lớn hơn) → C16d (nhãn), rồi phiên thực địa trên
    ProArt X870E, TUF và một máy RTX PRO 6000 hoặc 4090.
13. C9 (M khả nghịch mod 2; đo C9d trên máy RTX PRO 6000 hoặc 4090) → C10.

Mỗi bước kết thúc bằng: `make test`, đo trên host, kiểm QEMU nếu đụng tới image, và cập nhật bảng
hạng mục ở trên.

## Việc tồn đọng từ trước (chưa xếp lịch)

- CPU-NVMe `matrix_rounds` ramp ([../DURATION_COMPLEXITY_RAMP.md](../DURATION_COMPLEXITY_RAMP.md)).
- FC8 đang tạm gác ([../FAULT_CATCH_ROADMAP.md](../FAULT_CATCH_ROADMAP.md)).
- Cách hiển thị "RAM 1 (1 susp)" làm người vận hành bối rối, đã gác từ 2026-10-09.
