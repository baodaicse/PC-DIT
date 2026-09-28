# Review: /home/hoang/Downloads/CPU-DIT.exe — so sánh tính năng với CPU-DIT-01

## Cập nhật: đã xác định đúng bản chất file

Phần đầu tài liệu này (giữ nguyên bên dưới để lưu vết) kết luận sai rằng file
không có dấu vết liên quan vì tìm nhầm hướng (tưởng là binary Rust/C++ thuần).
Sau khi người dùng xác nhận đây là app cũ của chính họ và mục đích là **so sánh
tính năng** với `CPU-DIT-01` hiện tại, tôi phân tích tiếp và xác định chính xác:

- File là ứng dụng **Python 3.13** (dùng `numpy`, `psutil`, `pywin32`), đóng gói
  thành một `.exe` Windows bằng **PyInstaller (chế độ onefile)** — đó là lý do
  không có chuỗi `rustc`/`cargo`/`*.rs` (không phải Rust) và vì sao import table
  nhỏ (bootloader C của PyInstaller chỉ import vài hàm, phần còn lại nạp
  `python313.dll` và các `.pyd` lúc chạy).
- PyInstaller đóng gói **không mã hoá bytecode** theo mặc định, và app này còn
  nhúng thẳng `README.md`, `ROADMAP.md` cùng toàn bộ `tests/*.py`, `utils/*.py`
  dưới dạng resource nén zlib (đọc được nguyên văn plaintext) — nên tôi trích
  xuất trực tiếp bằng cách phân tích cấu trúc CArchive/TOC của PyInstaller
  (kỹ thuật tương đương công cụ cộng đồng `pyinstxtractor`), không cần
  disassemble mã máy hay decompile bytecode.
- Vì người dùng xác nhận đây là app do họ tự viết, tôi không tiếp tục coi đây là
  rủi ro bảo mật/pháp lý; phần phân tích PE ban đầu vẫn đúng về mặt kỹ thuật
  (không liên quan tới mã Rust trong workspace này) — chỉ là **hai project khác
  nhau của cùng một người**, không phải file đáng ngờ.

## Kiến trúc app cũ (`CPU-DIT` Python/Windows)

| Thuộc tính | Giá trị |
|---|---|
| Ngôn ngữ | Python 3.13, đóng gói PyInstaller onefile |
| Thư viện chính | numpy (SIMD/AVX qua BLAS), psutil, pywin32 (WMI), hashlib, threading |
| Nền tảng | Windows only (dùng `wmic`/`powershell`/WMI để lấy CPU/RAM/SSD info) |
| Song song hoá | Python `threading` — bị giới hạn bởi GIL, chính README/ROADMAP của app cũ cũng tự nhận: *"Branch Predictor test chưa stress BPU hardware thực sự (bị giới hạn bởi Python GIL)"*, *"Cache Coherence không test MESI protocol hardware trực tiếp (GIL)"* |
| Trạng thái PASS/FAIL | `PASS` / `WARNING` (khi ≤ 2 lỗi) / `FAIL` (khi > 2 lỗi) — **có ngưỡng dung sai lỗi**, khác hẳn nguyên tắc bit-exact/zero-tolerance của `CPU-DIT-01` |
| Test module đã có (v1.0) | `data_integrity`, `cache_coherence`, `storage_pipeline`, `fpu_accuracy`, `memory_bandwidth`, `branch_predictor`, `multicore_sync` |

## Bảng so sánh tính năng: app Python cũ vs `CPU-DIT-01` (Rust) hiện tại

| Tính năng | App Python cũ | `CPU-DIT-01` (Rust) hiện tại |
|---|---|---|
| Data integrity CPU↔RAM | `test_data_integrity.py`: XOR/byte-swap reversible transform qua numpy, so SHA256+CRC32 trước/sau, đa luồng Python (GIL) | `dit-core`/`dit-system`: GF(256) matrix encode/decode + exact byte-verify thật trên worker pinned CPU riêng biệt, oracle scalar độc lập — mạnh hơn vì không bị GIL và có golden vector độc lập |
| Cache coherence | `test_cache_coherence.py`: writer/reader thread qua `ctypes` array + numpy shared array, chấp nhận sai số do race điều kiện | `coherency_test` (`cpu.rs`): atomic `fetch_add` SeqCst thật trên CPU pinned, kỳ vọng chính xác tuyệt đối `expected == actual`, không có vùng "chấp nhận race" |
| FPU/SIMD accuracy | `test_fpu_accuracy.py`: transcendental/integer/numpy dot-matmul/FFT + multicore consistency, tolerance `1e-12` | `fp_fma_known_answer_test`/`integer_known_answer_test` (`cpu.rs`): bit-exact so `to_bits()`, không dùng tolerance; nhưng **one-shot**, không sustained — xem gap đã ghi ở [ADR-0004](decisions/0004-cpu-power-virus-coverage-review.md) |
| Memory bandwidth/latency | `test_memory_bandwidth.py`: đo GB/s và random-access latency bằng numpy, có đo nhưng **dùng benchmark số làm một phần đánh giá** | `PRODUCT_SPEC.md` mục 3 cố tình cấm dùng benchmark/utilization làm oracle PASS — CPU-DIT-01 chỉ báo coverage/thời gian, không lấy tốc độ làm tiêu chí đạt/không đạt |
| Branch predictor / speculative execution | `test_branch_predictor.py`: vòng lặp Python nhiều nhánh + `numpy.where`, so kết quả với reference tính trước | Không có route tương đương trong `CPU-DIT-01` hiện tại — đây là **gap thật**, có thể cân nhắc thêm dưới dạng một compute-oracle mới (scalar reference độc lập, không dùng benchmark) |
| Storage pipeline (SSD↔CPU↔RAM) | `test_storage_pipeline.py`: ghi/đọc file thật, CPU XOR transform, so CRC32/manifest | Nằm ngoài phạm vi MVP theo `PRODUCT_SPEC.md` ("Ngoài phạm vi MVP: GPU/VRAM/PCIe"); CPU-DIT-01 chỉ tập trung CPU–RAM |
| Multi-core sync | `test_multicore_sync.py`: atomic counter qua `threading.Lock` (không phải lock-free thật vì GIL), producer-consumer checksum, barrier sync | `coherency_test` dùng atomic thật không qua GIL, cộng với cross-core role rotation (writer/verifier đổi CPU) đã có ở Bước 3 |
| Thermal/telemetry | Chưa có ở v1.0 (roadmap v1.2 mới định làm) | Đã `implemented` ở Bước 4: EDAC/ECC/MCE, thermal, throttle, Temperature Policy v2 (xem `TELEMETRY_SAFETY_V1.md`) — CPU-DIT-01 vượt trội hẳn ở mảng này |
| Phân biệt lỗi CPU vs RAM vs IMC | ROADMAP tự ghi "chưa phân biệt lỗi do CPU vs RAM vs Motherboard IMC" | `PRODUCT_SPEC.md` mục 5 đã có state machine `PASS/FAIL/INVALID/UNSUPPORTED/ABORTED` và nguyên tắc "không tự quy lỗi cho CPU hoặc RAM khi thiếu evidence" — CPU-DIT-01 xử lý bài bản hơn |
| Reproducibility/seed | Không thấy cơ chế seed/replay xuyên suốt trong các file đã đọc | `SEED_ROTATION_V1.md`: Linux CSPRNG + SHA-256 round-seed + commitment + `--seed` replay — CPU-DIT-01 vượt trội |
| Fault-injection tự kiểm oracle | Không có | Có campaign đầy đủ (`run_negative_campaign.sh`), bắt buộc trước mỗi release |
| GPU↔CPU PCIe/VRAM transfer | Roadmap v1.1 (chưa code, chỉ có kế hoạch dùng `cupy`/`pyopencl`) | Ngoài phạm vi MVP, đã ghi rõ trong `PRODUCT_SPEC.md` |
| **IMC dual-channel stress + latency variance** | Roadmap v1.3 (chưa code): ép IMC dùng cả 2 channel interleaved, đo latency variance, so single-core vs all-core access — nhắm thẳng vào DDR5 IMC instability Intel 13/14th gen | Chưa có route tương đương — **đáng cân nhắc bổ sung**, vì đây là hướng khác với window/pattern coverage hiện tại (đo biến thiên latency thay vì chỉ exact-verify byte) |
| **Power-state/C-state transition test** | Roadmap v1.5 (chưa code): idle (C6) → burst load đột ngột, lặp 1000 lần, đếm lỗi sau mỗi lần chuyển trạng thái — tác giả app cũ tự nhận đây là *"pattern chính của Intel 13th/14th gen instability"* | Trùng khớp với gap đã nêu ở [ADR-0004](decisions/0004-cpu-power-virus-coverage-review.md) mục "Không có P-state/voltage transition stress" — **bằng chứng độc lập củng cố khuyến nghị đó** |
| Hardware crypto correctness (AES-NI/SHA-NI) | Roadmap v1.4 (chưa code): so AES-NI/SHA-NI hardware với reference software | CPU-DIT-01 chỉ có SHA-256 KAT bằng software (`sha2` crate); chưa test riêng đường tăng tốc phần cứng AES-NI/SHA-NI — **gap đáng cân nhắc** nếu muốn bắt lỗi silicon instruction cụ thể |
| GUI/dashboard | Roadmap v2.0 (chưa code): tkinter + matplotlib + PDF export | CPU-DIT-01 đã có live text UI trong Bước 5 (`UI_DASHBOARD_V3.md`), không cần GUI vì chạy từ USB boot minimal |

## Nhận định tổng hợp

1. **Zero-tolerance oracle là điểm mạnh cốt lõi của CPU-DIT-01** so với app cũ:
   app cũ chấp nhận tới 2 lỗi vẫn là `WARNING` chứ không phải `FAIL`, và một số
   test (cache coherence) tự nới lỏng do nghi ngờ race condition — đây là kiểu
   thiết kế PRODUCT_SPEC.md của CPU-DIT-01 cố tình từ chối ("Parity có thể hỗ
   trợ chẩn đoán nhưng không được sửa lỗi rồi báo PASS").
2. **Giới hạn GIL của app cũ là thật và được chính tác giả cũ thừa nhận** — mọi
   kết luận "đa luồng" trong app Python đó không tạo áp lực phần cứng thật như
   worker pinned-CPU của CPU-DIT-01.
3. **Hai ý tưởng trong roadmap app cũ trùng khớp độc lập với gap đã nêu ở
   ADR-0004** (P-state/voltage transition test, và một phần thermal correlation)
   — đây là tín hiệu đáng tin cậy rằng nên ưu tiên các khuyến nghị đó.
4. **Hai tính năng mới đáng cân nhắc bổ sung vào CPU-DIT-01** (ngoài các
   khuyến nghị đã có ở ADR-0004): IMC dual-channel latency-variance stress, và
   hardware crypto correctness (AES-NI/SHA-NI vs software) — cả hai đều giữ được
   nguyên tắc bit-exact/không dùng benchmark làm oracle nếu triển khai đúng
   cách (so sánh kết quả, không so tốc độ).
5. Storage pipeline và GPU/VRAM của app cũ **không nên đưa vào CPU-DIT-01** vì
   đã bị loại rõ khỏi phạm vi MVP trong `PRODUCT_SPEC.md`.

---

## (Giữ nguyên) Phân tích PE tĩnh ban đầu — vẫn đúng về mặt kỹ thuật



Yêu cầu ban đầu là "phân giải app ngược lại" (decompile/reverse-engineer) file
này để làm tài liệu tham khảo. Trước khi phân tích sâu, tôi hỏi lại nguồn gốc và
quyền reverse-engineer; người dùng không có mặt để xác nhận. Vì không thể xác
minh file này là sản phẩm của chính người dùng hay phần mềm bên thứ ba có bản
quyền, tôi chỉ thực hiện **phân tích tĩnh, không thực thi file**, để xác định
trước xem file có thực sự liên quan tới mã nguồn `CPU-DIT-01` trong workspace
này hay không — và **dừng lại trước khi disassemble/decompile logic bên trong**,
vì bằng chứng thu được cho thấy khả năng cao đây không phải build của dự án này.

## Bằng chứng thu thập được (đọc tĩnh, không chạy file)

```text
path      /home/hoang/Downloads/CPU-DIT.exe
size      23,785,083 bytes
sha256    528b57c1ca85810cb27c488324dcf1d071105f9c4abf7bc854bd8338bef438dd
format    PE32+ (x86-64), Windows CUI console subsystem, 7 section
linker    MSVC 14.44
timestamp PE header: 2026-04-02 12:12:10 (giá trị do linker ghi, không tự xác minh được)
```

Section table: `.text .rdata .data .pdata .fptable .rsrc .reloc` — bố cục điển
hình của một binary Windows biên dịch bằng MSVC toolchain, có Load Config
(CFG/GuardCF) và POGO optimization data, **không có CodeView/PDB debug entry**
(chỉ có debug directory loại POGO, không loại CodeView) — tức bản build đã cố
tình bỏ debug info tham chiếu PDB.

Kiểm string (ASCII và UTF-16LE) toàn bộ file: **không tìm thấy bất kỳ chuỗi nào**
liên quan tới:

- `CPU-DIT`, `dit-core`, `dit-system`, `dit-cli` (tên crate/test ID của dự án này);
- `rustc`, `cargo`, `panicked at`, hoặc bất kỳ đường dẫn `*.rs` nào (Rust luôn
  nhúng các chuỗi này dưới dạng literal trong rodata cho panic/unwrap/assert trừ
  khi build bằng cấu hình rất đặc biệt như `panic_immediate_abort` với
  `-Z build-std`, điều mà repo này không dùng — xem `rust-toolchain.toml` và
  `Makefile`, chỉ dùng `cargo build --release --workspace` chuẩn);
- bất kỳ text người dùng có thể đọc được (menu, thông báo lỗi, version info) —
  phần lớn "string" trích được thực chất là chuỗi opcode x86-64 (ví dụ
  `WAVAWH`, `UVWATAUAVAWH`) bị nhận nhầm thành text in được, không phải dữ liệu
  văn bản thật.

Import table chỉ 0x50 byte (rất ít hàm import), không có Export Directory.

Đối chiếu với chính repo: không có bất kỳ target Windows nào trong
`Cargo.toml`, `rust-toolchain.toml` hay `Makefile`; toàn bộ [PRODUCT_SPEC.md](PRODUCT_SPEC.md)
và [ROADMAP.md](ROADMAP.md) mô tả sản phẩm là **USB boot Ubuntu/Linux minimal**,
chưa từng có kế hoạch build `.exe` Windows.

## Kết luận

Dù trùng tên file, **không có bằng chứng nào cho thấy `CPU-DIT.exe` được build
từ mã nguồn trong workspace này**. Đặc điểm binary (MSVC linker, không dấu vết
Rust, string bị loại bỏ gần như hoàn toàn, POGO nhưng không PDB, import table
rất nhỏ) khớp với một binary Windows biên dịch từ ngôn ngữ/toolchain khác và đã
qua bước làm sạch string/debug info có chủ đích — đây là dấu hiệu cần thận
trọng chứ không phải bằng chứng để yên tâm coi là "bản Windows của dự án".

## Vì sao tôi dừng lại ở đây, chưa decompile logic

Không xác minh được nguồn gốc/quyền sử dụng của file, và bằng chứng tĩnh cho
thấy khả năng cao đây là phần mềm không liên quan tới dự án `CPU-DIT-01`. Việc
disassemble/decompile sâu hơn một binary bên thứ ba không rõ nguồn gốc và
không rõ giấy phép có thể vi phạm điều khoản sử dụng phần mềm hoặc bản quyền,
nên tôi dừng ở mức phân tích tĩnh cấu trúc PE thay vì đọc/dịch ngược logic bên
trong.

## Khuyến nghị

1. Xác minh lại nguồn gốc thật của file này (tải từ đâu, có đúng là bạn tự build
   không) trước khi mở hoặc chạy nó.
2. Không chạy file này trên máy thật nếu không chắc nguồn gốc; nên quét bằng
   antivirus/sandbox trước.
3. Nếu bạn thực sự cần một bản Windows của CPU-DIT-01, nên tự cross-compile từ
   chính source trong workspace (`cargo build --release --target
   x86_64-pc-windows-msvc` sau khi thêm target phù hợp) thay vì dùng file tải
   sẵn không rõ nguồn gốc — như vậy provenance sẽ luôn đối chiếu được với commit
   hash và `Cargo.lock` hiện tại.
4. Nếu bạn xác nhận đây đúng là file bạn tự build và có quyền phân tích, cho
   tôi biết công cụ build/toolchain đã dùng (không phải cargo/rustc mặc định
   của repo này) để tôi đối chiếu lại, hoặc chỉ định rõ phạm vi muốn tôi
   disassemble tiếp.
