# Combined stress: CPU-RAM + GPU-VRAM + SSD trong một phiên — phân tích thiết kế

Cập nhật: 2026-09-25 (bản 2: giữ RAM/VRAM gần tối đa thay vì RAM ~60%; bản 3: chuỗi chủ yếu đọc; bản 4: đáp án tính hai lần lúc nạp, so sánh hiệu quả mục 4.7). Trạng thái: **đã chốt 2026-09-25** (mục 9.1); đang làm `dit-chain`. Cụ thể hoá COMBINED-B → F của
[UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md](UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md); chưa có dòng code nào của
phần mới. Số đo dẫn chứng lấy từ GTX 960 + máy phát triển 16 luồng ghi trong
[GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md](gpu-vram/GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md) và
[cpu-ram/STATUS.md](cpu-ram/STATUS.md).

## 1. Mục tiêu

1. **Tải cao:** CPU, RAM, GPU, VRAM, PCIe và SSD cùng chịu tải gần tối đa trong một phiên, để lộ lỗi chỉ xuất hiện
   khi toàn máy nóng và hút điện cùng lúc (nguồn, VRM, IMC, PCIe, nhiệt). **Giữ gần như toàn bộ RAM và VRAM** như khi
   chạy từng module riêng (mục 6).
2. **Dữ liệu liên kết và chính xác:** một dòng dữ liệu đi xuyên CPU → RAM → GPU → VRAM → SSD và quay về, được kiểm
   **chính xác từng bit** sau mọi bước, và lỗi phải khoanh được về bước gây ra.
3. **Dùng lại thuật toán đã có**, không viết oracle mới: GEMM u32 / FP32 FMA / INT8 Tensor và oracle CPU, block
   DITDEEP2, pattern RAM và compute-oracle CPU, quét DRAM GPU, SHA-256.
4. **Tuỳ chọn** mức tải và thời lượng.

## 2. Vì sao không dồn mọi dữ liệu qua một chuỗi duy nhất

Mục tiêu 1 và 2 kéo ngược nhau: một chuỗi nối tiếp chạy bằng tốc độ **mắt xích chậm nhất**, và SSD còn bị giới hạn
TBW.

| Mắt xích | Thông lượng đo được / ước tính | Ghi chú |
|---|---|---|
| GPU GEMM (menu, u32 + FP32, K=1024, jobs 512) | 202,7 G phép tính/s ⇒ ~47 batch/s ⇒ **~400 MB/s output** | mỗi batch 4,3 G phép tính, 8,4 MB output |
| Kiểm VRAM/PCIe qua CPU | ~1,1–1,4 GiB/s | giới hạn bởi CPU + PCIe |
| Quét DRAM trên GPU | ~70 GiB/s khi chạy (GTX 960) | device-verified |
| SSD Deep | tuỳ ổ; bị chặn bởi **ngân sách ghi** (Light 2 / Standard 16 / Heavy 64 GiB mỗi ổ) | TBW |

Ghi toàn bộ output GPU xuống SSD là **~713 GB mỗi 30 phút**, vượt xa mọi mức ghi. Ngược lại, để SSD quyết định tốc độ
thì GPU gần như rảnh. Vì vậy đề xuất tách thành hai lớp chạy song song.

## 3. Kiến trúc hai lớp: "bão hoà + khâu nối"

```text
 LỚP TẢI NỀN — engine hiện có, mỗi linh kiện gần tối đa, oracle riêng
   CPU-RAM : system-run (pattern RAM + compute-oracle FMA/AVX, rẽ nhánh, AES)
   GPU-VRAM: async GEMM u32/FP32(/INT8) + quét DRAM + lane PCIe (CPU-verified)
   SSD     : Deep DITDEEP2 (20 cỡ I/O, ma trận 16x16, read-soak)

 LỚP CHUỖI — mới (dit-chain), CHỦ YẾU ĐỌC: ghi một lần lúc đầu, sau đó đọc liên tục
   nạp trước (1 lần): CPU dựng corpus + đáp án ─► SSD
   mỗi lần đọc:       SSD ─► RAM ─► PCIe ─► VRAM ─► GPU GEMM ─► PCIe ─► RAM ─► CPU so với đáp án đọc từ SSD
                        ▲                                                                   │
                        └────── bản ghi tiếp theo chọn theo hash kết quả vừa kiểm ◄──────────┘
```

- **Lớp nền** tạo tải, nhiệt, điện và giữ các chỉ số độ phủ của từng module như khi chạy riêng.
- **Lớp chuỗi** chứng minh dữ liệu đi xuyên mọi linh kiện mà không sai, **trong lúc** cả máy đang bị lớp nền ép tải.

## 4. Lớp chuỗi (`dit-chain`) — chủ yếu đọc

Cập nhật bản 3: chuỗi **ghi một lần rồi đọc liên tục**, thay cho thiết kế cũ ghi mỗi thế hệ (giữ ở mục 4.5 để so sánh).
Mức ghi của chuỗi là cố định, không tăng theo thời lượng.

### 4.1 Nạp trước (ghi một lần, trong pha baseline)

- CPU dựng `R` corpus bằng `Corpus::new_arith` (u32 / FP32 / INT8 luân phiên; 64 job, K=1024). Mỗi corpus mang sẵn
  **đáp án của chính nó** (oracle `dot`) cùng SHA-256 của input và của đáp án.
- Mỗi corpus thành một **bản ghi** trên SSD: header 4 KiB (phiên, chỉ số, seed, kiểu số học, N, K, hai SHA) + input +
  đáp án, ghi `O_DIRECT | O_DSYNC` vào file mới, với các chốt an toàn sẵn có của CPU-SSD (create-new, chừa trống,
  stable-ID). Kích thước: ~34,6 MB mỗi bản ghi u32/FP32, ~9,4 MB với INT8.
- **Tập làm việc = mức ghi đã chọn:** Light 2 GiB ≈ 62 bản ghi, Standard 16 GiB ≈ 496, Heavy 64 GiB ≈ 1.985.
  Khuyến nghị tối thiểu Standard (mục 4.6).
- **Đáp án được tính hai lần, bắt buộc.** Mỗi đáp án được dùng lại hàng nghìn lần, nên một lỗi CPU/RAM đúng lúc tính sẽ
  thành một đáp án sai có SHA "đúng" (SHA được tính trên chính đáp án sai), và GPU tính đúng sẽ bị đổ oan. Vì vậy:
  - Tính đáp án **hai lần độc lập, trên hai nhóm lõi CPU khác nhau** (ghim luồng bằng CPU affinity, hai bộ đệm riêng),
    rồi so từng word. Chỉ ghi bản ghi xuống SSD khi hai lần khớp nhau.
  - Hai lần khác nhau thì bản thân đó là bằng chứng lỗi CPU/RAM: ghi FAIL phase `chain-prefill-oracle` (kèm hai nhóm
    lõi, chỉ số bản ghi, word lệch), không ghi bản ghi đó, dựng lại bằng seed kế tiếp.
  - SHA trong header được tính trên đáp án đã khớp hai lần.
- Chi phí oracle chỉ trả lúc nạp trước: ~0,03 s (u32) / ~0,17 s (FP32) mỗi lần tính; với hai lần, ~100 s cho 496 bản
  ghi dùng 4 luồng, chạy chồng lên pha baseline.

### 4.2 Một lượt đọc

| Bước | Dùng lại | Checkpoint: sai ở đây thì khoanh về |
|---|---|---|
| 1. Đọc bản ghi `i` từ SSD vào bộ đệm pinned | đường đọc `O_DIRECT` của CPU-SSD; cỡ I/O xoay theo catalog 4 KiB → 1 MiB của Deep | SHA input hoặc SHA đáp án ≠ header → **SSD / đường lưu trữ / DMA vào RAM** |
| 2. Upload input lên VRAM | pipeline async (bộ đệm pinned, stream riêng) | — |
| 3. GEMM trên GPU, chạy `G` generation liên tiếp trên cùng input | `dit_gemm` / `_f32` / `_i8`, tag theo generation | `verify_bytes` với **đáp án đọc từ SSD** ≠ → **GPU / VRAM / PCIe** |
| 4. Chọn bản ghi kế | `i_next = H_g mod R`, với `H_g = SHA-256(H_{g-1} ‖ SHA(output_g))` | — |
| 5. Kiểm cuối phiên | phát lại đường đọc từ `seed_0` và các đáp án | `H_final` ≠ → **end-to-end** |

- **Khi GPU sai mà SHA vẫn đúng** (confirm-on-fail, như H4a): băm lại bộ đệm pinned và đọc lại bản ghi từ SSD. Bộ đệm
  đã đổi → **RAM**; SSD đọc lại khác → **SSD**; cả hai đúng → **GPU / VRAM / PCIe**. Bước này chỉ chạy khi có lỗi, nên
  không tốn gì lúc bình thường.
- **Luôn sai ở cùng một bản ghi** trong khi các bản ghi khác đúng: nghi đáp án của bản ghi đó trước khi nghi GPU. Tính
  lại đáp án bản ghi đó trên CPU (hai lần như lúc nạp) rồi mới phân loại. Lỗi GPU thật hiếm khi theo mẫu này.
- **Vẫn là chuỗi phụ thuộc dữ liệu:** bản ghi đọc tiếp theo do kết quả GEMM vừa kiểm quyết định. Một sai sót lọt qua mọi
  checkpoint cũng đổi `H_g`, đổi đường đọc, và bị lộ ở bước 5. Đường đọc gần như ngẫu nhiên trên cả tập làm việc, nên
  controller SSD không thể phục vụ từ bộ nhớ đệm nhỏ của nó.
- **Chính xác từng bit** như trước: u32 lấy dư 2³², FP32 FMA (đã khớp từng bit trên GTX 960), INT8 cộng dồn int32 (chưa
  kiểm trên phần cứng), SHA-256.

### 4.3 Tốc độ đọc và chi phí CPU

`G` (số generation trên mỗi lần nạp) chỉnh để tốc độ đọc SSD đạt mức mong muốn. Mỗi batch 64 job = 0,54 G phép tính:

| Phần GPU dành cho chuỗi | `G` | Lần nạp/s | Đọc SSD | CPU cho SHA |
|---|---|---|---|---|
| 50 G phép tính/s | 8 | 11,6 | 0,40 GB/s | 0,2 core |
| 50 G phép tính/s | 4 | 23,3 | 0,81 GB/s | 0,4 core |
| 100 G phép tính/s | 4 | 46,6 | 1,61 GB/s | 0,8 core |

- Arbiter đo tốc độ đọc tuần tự của ổ lúc baseline, rồi chọn `G` để chuỗi dùng khoảng 50–70% tốc độ đó (`balanced`) hoặc
  gần hết (`max`).
- CPU gần như không tốn cho oracle trong lúc chạy: chỉ SHA-256 (~2 GB/s mỗi core có SHA-NI) và so sánh.
- Thêm ~0,4–1,6 GB/s upload PCIe vào GPU, chồng lên lane PCIe sẵn có.

### 4.4 Mức ghi

- **Nạp trước:** đúng bằng tập làm việc, một lần mỗi phiên. SOAK 8 giờ cũng không ghi thêm.
- **Làm mới (tuỳ chọn, mặc định tắt ở `balanced`):** ghi lại một bản ghi mỗi `T` phút để đường ghi cũng chạy dưới tải,
  ví dụ một bản ghi mỗi phút ≈ 2 GB/giờ. Tính vào mức ghi đã chọn.
- **Phần ghi dưới tải nặng** vẫn do lớp nền đảm nhận: Deep ghi tới ngân sách của nó trong pha overlap.
- So sánh: 8 giờ SOAK đọc 0,8 GB/s là ~23 TB đọc, **không** tốn TBW (đọc gần như không mài mòn NAND); 16 GiB ghi một lần
  là không đáng kể so với TBW hàng trăm TB.

### 4.5 So sánh với thiết kế cũ (ghi mỗi thế hệ)

| | Cũ: ghi mỗi thế hệ, đọc lại trễ | **Mới: nạp một lần, đọc liên tục** |
|---|---|---|
| Ghi SSD | tăng theo thời lượng, bị ngân sách chặn (~1,1 MiB/s ở Standard 60 phút) | cố định = tập làm việc (2 / 16 / 64 GiB), không tăng theo thời gian |
| Tải SSD trong chuỗi | rất thấp (~1 MiB/s) | **0,4–1,6 GB/s đọc**, chỉnh được |
| CPU cho oracle lúc chạy | dựng corpus + oracle mỗi thế hệ | gần 0 (chỉ SHA + so sánh) |
| Phụ thuộc dữ liệu | input thế hệ sau = output qua SSD | đường đọc do hash kết quả quyết định |
| Khoanh vùng lỗi | 3 checkpoint | 2 checkpoint + confirm-on-fail (RAM / SSD / GPU) |
| Retention SSD | dữ liệu mới ghi, đọc lại sau ít phút | dữ liệu ghi đầu phiên, đọc suốt phiên (tới hàng giờ ở SOAK) |
| Đường ghi SSD dưới tải | có, nhưng rất ít | chỉ ở lớp nền (Deep) và phần làm mới tuỳ chọn |
| Khối lượng code | dựng corpus mỗi thế hệ, điều phối ghi/đọc trễ | định dạng bản ghi, nạp trước, đọc + nạp corpus có sẵn vào GPU |

### 4.6 Đánh giá

- **Khả thi: cao.** Mọi thành phần chính đã có: corpus và oracle, đường `O_DIRECT` và chốt an toàn của CPU-SSD, pipeline
  async GPU, SHA-256. Phần mới: định dạng bản ghi; bước nạp trước; cho pipeline GPU nhận một corpus có sẵn (input + đáp
  án từ SSD, kiểm SHA) thay vì tự dựng từ seed; đường đọc theo hash; confirm-on-fail. Ít việc hơn thiết kế cũ vì không
  phải dựng oracle và điều phối ghi/đọc trễ trong lúc chạy.
- **Hiệu quả: tốt hơn thiết kế cũ ở mục tiêu tải và độ chính xác:** tải SSD cao hơn hàng trăm lần, CPU rảnh hơn cho lớp
  nền, TBW cố định, kiểm từng bit xuyên suốt, khoanh vùng lỗi tốt hơn nhờ confirm-on-fail, và thêm được kiểm retention
  dài hạn.
- **Điểm yếu và cách bù:**
  - Đường ghi SSD dưới tải phụ thuộc vào lớp nền Deep và phần làm mới tuỳ chọn. Muốn ép ghi nặng thì chọn mức ghi
    Standard/Heavy cho Deep.
  - Tập làm việc Light 2 GiB xấp xỉ bộ nhớ đệm DRAM của một số controller SSD. Khuyến nghị tối thiểu Standard
    (16 GiB) để phần lớn lượt đọc đi tới NAND thật.
  - Bắt buộc `O_DIRECT`; nếu không, lượt đọc được phục vụ từ page cache của RAM chứ không từ SSD. RAM đang giữ gần
    hết (mục 6) nên page cache vốn cũng nhỏ.
  - GPU tính lại cùng một input `G` lần. Việc này đã được chấp nhận ở module GPU (tag theo generation chống kết quả
    cũ).

### 4.7 So sánh hiệu quả phát hiện lỗi: chủ yếu đọc và ghi mỗi thế hệ

So sánh theo khả năng **bắt lỗi** (bảng 4.5 so theo tải và chi phí). Đây là phân tích thiết kế; chưa có số đo phần cứng
cho cả hai cách.

**Lượng dữ liệu đi qua chuỗi.** Lỗi thoáng qua (bit lật, DMA hỏng, PCIe replay thất bại) xuất hiện theo xác suất trên
mỗi byte, nên khả năng bắt tỉ lệ với lượng byte đi qua đường cần kiểm. Pha overlap 60 phút, mức Standard:

| | Ghi mỗi thế hệ | Chủ yếu đọc |
|---|---|---|
| Byte qua đường SSD → RAM | ~4 GiB ghi + ~4 GiB đọc | **1,4–5,8 TB đọc** (0,4–1,6 GB/s) |
| Byte qua PCIe RAM → GPU trong chuỗi | ~130 GB (corpus dựng mới mỗi thế hệ) | 1,4–5,8 TB (bản ghi đọc từ SSD) |
| Lần kiểm SHA trên dữ liệu từ SSD | ~4.000 | ~40.000–170.000 |

Riêng đường đọc SSD → RAM, cách chủ yếu đọc có lượng dữ liệu **gấp khoảng 175–700 lần**.

**Theo từng loại lỗi:**

| Loại lỗi | Ghi mỗi thế hệ | Chủ yếu đọc | Ghi chú |
|---|---|---|---|
| Đọc NAND sai, mất dữ liệu theo thời gian, nhiễu do đọc nhiều | yếu (dữ liệu vừa ghi, đọc một lần) | **mạnh** (đọc lặp suốt phiên, dữ liệu cũ tới hàng giờ) | |
| Controller SSD, DMA, link PCIe của SSD dưới tải | yếu (~1 MiB/s) | **mạnh** (GB/s liên tục) | |
| RAM hỏng ở bộ đệm trung gian | có | **mạnh hơn** (GB/s qua bộ đệm) | tách được RAM / SSD / GPU nhờ confirm-on-fail |
| PCIe upload lên GPU, VRAM, tính toán GPU | có | có | tương đương; lớp nền GPU là nơi ép GPU chính |
| **Đường ghi SSD dưới tải** (ghi lỗi, ghi nhầm chỗ, ghi mất) | **mạnh hơn** (ghi liên tục trong overlap) | yếu (chỉ nạp trước, lúc chưa overlap) | bù bằng lớp nền Deep (tự kiểm mọi thứ nó ghi) và phần ghi làm mới |
| Lỗi phụ thuộc mẫu dữ liệu | **mạnh hơn** (dữ liệu mới mỗi thế hệ) | tập cố định 62–1.985 bản ghi | lớp nền GPU và Deep vẫn xoay dữ liệu mới liên tục |
| SSD trả dữ liệu cũ, ghi mất lúc nạp | bắt được | bắt được (SHA sai) | có ghi làm mới thì bắt được cả trả phiên bản cũ |
| Lỗi nguồn/nhiệt do toàn máy cùng tải | yếu hơn (SSD gần như rảnh) | **mạnh hơn** (SSD và root complex cùng tải) | đúng mục tiêu "tải cao" |
| CPU tính sai đáp án | rải đều: hỏng một thế hệ | tập trung: một bản ghi sai dùng lại hàng nghìn lần | chặn bằng tính đáp án hai lần lúc nạp (mục 4.1) |
| Độ trễ phát hiện | ít nhất một thế hệ | **ngay lúc đọc** | |

**Kết luận:**

- Chủ yếu đọc **hiệu quả hơn rõ rệt** ở mục tiêu chính của combined: lỗi đường đọc, controller, DMA, PCIe, RAM trung
  gian, retention và lỗi do cả máy cùng tải. Nguyên nhân là lượng dữ liệu kiểm được lớn hơn hàng trăm lần và phát hiện
  ngay lúc đọc.
- Ghi mỗi thế hệ chỉ hơn ở đường ghi SSD dưới tải và độ đa dạng dữ liệu, là hai phần mà combined đã có engine khác phủ
  (Deep ở lớp nền; GPU nền xoay corpus mới).
- **Chọn chủ yếu đọc, với ba điều kiện:** (1) tính đáp án hai lần lúc nạp trước (mục 4.1); (2) Deep ở lớp nền chạy mức
  ghi Standard hoặc Heavy trong pha overlap; (3) tập làm việc tối thiểu 16 GiB.
- **Cần đo để chốt:** hiệu quả thật chỉ khẳng định được bằng chiến dịch chèn lỗi (COMBINED-F): chèn bit lật ở từng biên
  (SSD, RAM, PCIe, GPU, và đáp án lúc nạp) cho cả hai cách, đo tỉ lệ bắt được và thời gian tới khi bắt được.

## 5. Tuỳ chọn mức tải và thời lượng

| Mức | Thời lượng | Lịch pha |
|---|---|---|
| **BOUNDED** | 15 phút | baseline từng linh kiện (~2 phút mỗi cái) → chuỗi đứng riêng → overlap 5 phút |
| **DEEP** | 60 phút | baseline → chuỗi → overlap 40 phút → nghỉ, đọc lại retention (VRAM, SSD) |
| **SOAK** | 4–8 giờ | như DEEP, lặp chu kỳ tải/nghỉ để lộ lỗi nhiệt và chuyển trạng thái nguồn |

Trục riêng cho cường độ overlap:

Cả hai mức đều **giữ RAM và VRAM gần tối đa** (mục 6); chúng chỉ khác nhau ở cách chia **thời gian tính toán**:

- `balanced` (mặc định): arbiter chia luồng CPU và thời gian GPU để các lớp không giành nhau (duty quét DRAM 0.25,
  luồng dành riêng cho verifier và SSD).
- `max`: mọi lớp chạy hết tốc độ cùng lúc; dễ lộ lỗi nguồn/PSU/VRM nhất nhưng cũng dễ làm sập máy. **Bắt buộc bật giới
  hạn nhiệt** CPU (95 °C) và GPU (90 °C).

**Baseline trước overlap là bắt buộc:** chạy overlap ngay mà gặp mismatch thì không biết linh kiện nào sai. Baseline
PASS từng phần rồi overlap mới sai thì kết luận được "lỗi phụ thuộc tải / nguồn / nhiệt".

## 6. Chia tài nguyên — giữ RAM và VRAM gần tối đa (COMBINED-B)

### 6.1 Nguyên tắc

Giữ nhiều bộ nhớ tự thân không nguy hiểm. Nguy hiểm là bộ nhớ **tăng thêm sau khi đã giữ gần hết** (OOM, OOM killer
giết một tiến trình test). Vì vậy dùng lại đúng cách `--vram-max` đang làm với VRAM: **ai cấp phát sau cùng lấy phần còn
lại, đo sau khi mọi thứ khác đã cấp phát xong**, thay vì chia theo phần trăm đoán trước.

1. **Khai báo đỉnh.** Mỗi thành phần báo trước bộ nhớ lớn nhất nó sẽ dùng trong cả phiên, kể cả phần cấp phát muộn.
   GPU đã có sẵn (`matrix_host_bytes` trong kế hoạch async, gồm cả corpus dựng sẵn cho lần xoay sau). SSD và `dit-chain`
   cần thêm con số này vào heartbeat khởi động.
2. **Thứ tự khởi động:** `dit-chain` → GPU-VRAM → SSD, mỗi cái chờ tới khi heartbeat báo "đã cấp phát xong". **CPU-RAM
   khởi động cuối.**
3. **CPU-RAM lấy phần còn lại:** đọc `MemAvailable` sau bước 2, trừ dự phòng hệ thống và trừ phần *còn có thể tăng*
   (đỉnh đã khai báo trừ đi phần đã cấp phát thật), rồi truyền cho `system-run` qua `--memory-mib`. Đó chính là "95% sau
   dự phòng" như khi chạy riêng, chỉ khác là tính trên phần các module khác để lại.
4. **VRAM:** `dit-chain` cấp phát phần nhỏ của nó trước (corpus ~64 job ≈ 33 MB + output), rồi GPU-VRAM chạy
   `--vram-max` như hiện tại: pool = VRAM driver báo còn trống − 128 MiB.
5. **Dự phòng hệ thống** ≥ 1,5 GiB (kernel, page cache tối thiểu, UI, watchdog), lớn hơn ngưỡng tự hãm tải 256 MiB của
   pipeline GPU. Nếu không, GPU sẽ tưởng máy thiếu RAM và tự giảm tải.
6. **Fail closed:** nếu sau bước 2 phần còn lại cho CPU-RAM dưới mức tối thiểu, hoặc tổng đỉnh khai báo vượt
   `MemAvailable`, từ chối chạy ngay từ đầu.

### 6.2 Ví dụ: máy 32 GB, 1 GPU 8 GB, 1 SSD

| | Giữ | Ghi chú |
|---|---|---|
| Image live (`toram`) | ~2,7 GB | đã nằm ngoài `MemAvailable` ngay từ lúc boot |
| `MemAvailable` lúc bắt đầu | ~28 GB | |
| GPU-VRAM phía host (pinned + corpus dựng sẵn) | ~1,5 GB | khai báo sẵn trong kế hoạch async |
| SSD Deep (bộ đệm batch) | ~0,5 GB | ước tính, cần đo |
| `dit-chain` | ~0,3 GB | |
| Dự phòng hệ thống | 1,5 GB | |
| **CPU-RAM (arena kiểm pattern)** | **~24 GB** | phần còn lại |
| **Tổng RAM do các bài test giữ** | **~26,3 GB ≈ 94% `MemAvailable`** | mọi phần giữ đều được kiểm: arena bằng pattern, pinned GPU và bộ đệm SSD/chuỗi bằng oracle của chính chúng |
| **VRAM** | 8 GB − context CUDA − 128 MiB | như menu GPU hiện tại |

Cùng máy chạy CPU-RAM riêng giữ khoảng 95% sau dự phòng, nên mức giữ tổng gần như tương đương. Khác biệt duy nhất là
phần RAM do GPU/SSD/chuỗi giữ được kiểm bằng oracle của chúng thay vì bằng pattern RAM.

### 6.3 Luồng CPU (chỉ khác nhau giữa `balanced` và `max`)

Ví dụ 16 luồng, `balanced`: CPU-RAM 6, compute-oracle 2, verifier GPU 3–4, pool SSD 2–3, `dit-chain` 1, OS/UI 1. Ở
`max`, mỗi engine lấy mặc định của nó và để hệ điều hành chia; riêng lớp CPU-RAM trong overlap chạy `nice 10`
(`ThreadPlan.cpu_ram_nice`, từ v1.2.017). Lý do: CPU-RAM chiếm đủ mọi luồng, và từ v1.2 C1 mỗi worker đọc/ghi RAM
liên tục (nhanh gấp ~3,4 lần), nên ở cùng độ ưu tiên các luồng CPU kiểm kết quả của GPU bị tụt lại và GPU phải chờ.
Đo trên 3700X + GTX 960 (overlap kiểu `max`, 60 s): ưu tiên ngang nhau GPU bận 60 %, 66 W, 157 M phép ma trận được
kiểm/s; CPU-RAM `nice 10` thì 79 %, 86 W, 246 M/s, còn CPU-RAM chỉ ít hơn 6 % số round và vẫn lấp mọi chu kỳ còn
trống. Dành hẳn luồng riêng cho GPU (thử 4 luồng, hoặc 2 core trọn) chỉ được 70–71 %, 77 W và oracle mất một nửa,
vì thứ bị giành chủ yếu là băng thông RAM chứ không phải số core.

### 6.4 Rủi ro và cách giảm

- **Hai context CUDA** (GPU-VRAM và `dit-chain`) tốn thêm ~0,1–0,3 GB VRAM so với một. Cần đo; nếu đáng kể, đưa lớp
  chuỗi vào chính tiến trình `gpu-vram-dit` ở một bước sau.
- **OOM giữa phiên** (một engine vượt đỉnh đã khai báo): kernel giết một tiến trình → watchdog phát hiện → verdict
  `INVALID` cho phiên, không kết luận linh kiện hỏng. Report ghi `MemAvailable` theo thời gian để biết engine nào
  vượt.
- **GPU tích hợp** (VRAM chính là RAM): giữ quy tắc hiện tại (tối đa 60% cho GPU tích hợp) và CPU-RAM lấy phần còn lại
  như trên.
- **Một vòng RAM dài hơn** vì arena lớn nhưng ít luồng hơn (6 thay vì 16). Với ~24 GB và ~6 GB/s ghi + đọc, một vòng
  khoảng 10 s, vẫn đủ số vòng tối thiểu ngay cả ở BOUNDED.
- Arbiter truyền giới hạn qua cờ sẵn có của từng engine: `--memory-mib`, `--workers`, `--compute-oracle-workers`
  (CPU-RAM); `--vram-max`, `DIT_GPU_HOST_BUDGET_BYTES`, `--async-workers-max` (GPU); `--max-workers`,
  `--write-budget-mib` (SSD). Unit test bắt buộc: 8/16/32/64/128 GiB RAM, GPU tích hợp, 0/1/2 GPU, 0/1/2 SSD.

## 7. Kiến trúc chương trình

- **Lớp nền:** chạy nguyên ba engine hiện có dưới dạng tiến trình, với tham số do arbiter cấp. Chúng đã ổn định, đã có
  watchdog, heartbeat và report.
- **Lớp chuỗi:** binary mới `dit-chain` liên kết trực tiếp các crate thư viện `gpu-cuda`, `gpu-engine` (corpus/oracle)
  và `nvme-io` / `nvme-engine` (DITDEEP2, `O_DIRECT`). Chạy dưới watchdog như mọi module.
- **Orchestrator:** lập lịch pha, dashboard chung (`UnifiedDashboard`, đã có từ COMBINED-A), gộp report.

Không gộp tất cả vào một tiến trình: vòng chạy của GPU và SSD hiện nằm trong `main.rs` của từng CLI, tách thành thư
viện là một đợt refactor lớn; và một tiến trình sập sẽ kéo sập cả phiên, mất khả năng khoanh vùng.

## 8. Verdict và giới hạn

- Mỗi lớp giữ verdict riêng: CPU-RAM, CPU compute, GPU-VRAM, SSD và **chuỗi**. Mỗi lỗi chuỗi ghi checkpoint đầu tiên
  sai (bảng 4.2). Một mismatch luôn là FAIL cố định; tổng điểm không che lỗi.
- Lỗi phần cứng đã sửa / không sửa được gom chung cho cả máy (mục 6 của tài liệu GPU): lỗi nặng → FAIL `hardware`,
  lỗi đã sửa → PASS kèm cảnh báo.
- Kết luận cả máy chỉ khi mọi lớp PASS, xếp hạng `BOUNDED` / `DEEP` / `SOAK` theo pha đã chạy thật.
- **Giới hạn:** CPU vừa tính oracle cho mọi thứ vừa là linh kiện bị test. Lỗi common-mode (CPU/RAM sai theo cùng một
  cách ở cả đường chính lẫn oracle) có thể lọt. Pha baseline và compute-oracle cross-worker giảm rủi ro này, không loại
  bỏ được. Hash chuỗi phát lại được trên **máy khác** là cách kiểm độc lập duy nhất.
- INT8 Tensor chỉ vào chuỗi sau khi đã PASS "Tensor INT8 check" trên loại card đó.

## 9. Kế hoạch và quyết định cần chốt

| Bước | Nội dung | Task |
|---|---|---|
| 1 | Arbiter + lập lịch pha (BOUNDED/DEEP/SOAK, balanced/max, dừng/drain, UI không reset khi đổi pha) | COMBINED-B, C |
| 2 | `dit-chain`: định dạng bản ghi có provenance, nạp trước với đáp án tính hai lần, đọc + nạp corpus có sẵn vào GPU, đường đọc theo hash, confirm-on-fail, lỗi chèn cho từng biên | COMBINED-D |
| 3 | Report gộp + phân hạng | COMBINED-E |
| 4 | Menu "COMBINED" (chọn mức + cường độ); chạy thử GTX 960 + SSD máy phát triển; ma trận phần cứng thật để sau | COMBINED-F |

Bước 2 là phần mới nhiều nhất; các bước còn lại chủ yếu điều phối engine có sẵn.

## 9.1 Quyết định đã chốt (2026-09-25)

| # | Quyết định | Ưu điểm chính | Khuyết điểm chính | Điều chỉnh khi chốt |
|---|---|---|---|---|
| 1 | Hai lớp "bão hoà + khâu nối", giữ RAM/VRAM gần tối đa ("cấp phát sau cùng lấy phần còn lại", mục 6) | mỗi linh kiện gần tối đa bằng engine đã kiểm chứng; chuỗi chứng minh dữ liệu xuyên suốt dưới tải; RAM ~94% `MemAvailable`; verdict từng module giữ nguyên | nhiều tiến trình và thứ tự khởi động; SSD và `dit-chain` phải khai báo đỉnh bộ nhớ, khai báo thiếu thì OOM; hai context CUDA (+0,1–0,3 GB VRAM); các lớp giành CPU; rủi ro common-mode của oracle CPU | — |
| 2 | Mặc định BOUNDED 15 phút + `balanced`; `max` tuỳ chọn, bắt buộc bật giới hạn nhiệt | đủ ngắn để chạy thường xuyên; baseline trước giữ khả năng khoanh vùng; `balanced` không tự gây sập máy | 15 phút ngắn với lỗi nhiệt / lỗi thoáng qua; baseline ~2 phút mỗi linh kiện phủ ít; `balanced` không tới công suất đỉnh; giới hạn nhiệt có thể dừng máy nóng nhưng khỏe | màn hình chọn ghi rõ: BOUNDED là kiểm nhanh, **kết luận ổn định cần DEEP trở lên** |
| 3 | Chuỗi chủ yếu đọc (mục 4) + ba điều kiện (mục 4.7) | lượng dữ liệu đường đọc gấp ~175–700 lần; TBW cố định; CPU gần như không tốn cho oracle lúc chạy; phát hiện ngay lúc đọc; kiểm retention dài | đường ghi dưới tải dựa vào Deep; tập dữ liệu cố định; cần trống ≥ 16 + 4 GiB; nạp trước tốn ~100 s CPU + ~16 GiB ghi | (a) người dùng chọn Light ở UI SSD thì chuỗi vẫn chạy với tập làm việc nhỏ và report ghi cảnh báo "tập làm việc dưới 16 GiB: có thể bị bộ đệm SSD che"; ổ không đủ chỗ thì chuỗi báo `INVALID`, lớp nền vẫn chạy; (b) **nạp trước chạy trước pha baseline SSD** để không làm lệch số đo baseline |
| 4 | Thứ tự làm | kiểm phần mới và rủi ro nhất sớm; số đo thật quyết định arbiter | chưa có combined mode cho tới khi làm arbiter | **làm `dit-chain` trước**, chạy độc lập có chèn lỗi; sau đó 1 → 3 → 4 |

## 9.2 Tiến độ

- `dit-chain` (bước 2): **xong phần chạy độc lập**, có chèn lỗi đủ 4 biên — xem mục 10. Chưa nối vào image/menu.
- Tiếp theo: kế hoạch chi tiết ở mục 11 — bước 0 hoàn thiện `dit-chain`, rồi arbiter + lập lịch pha, report gộp và
  menu COMBINED.

## 10. `dit-chain` — cài đặt và số đo (2026-09-25)

Crate `crates/dit-chain`, binary `dit-chain run`. Report JSON `module_id = dit.chain`; mã thoát PASS 0 / FAIL 2 /
INVALID 3. File chuỗi bị xoá khi kết thúc, trừ khi có `--keep-file`.

### 10.1 Cách làm

| Phần | Cài đặt |
|---|---|
| Bản ghi | `DITCHAN1`: header 4 KiB (phiên, chỉ số, seed, phép tính, kích thước, SHA input, SHA đáp án, SHA của chính header), rồi input và đáp án; stride cố định (bội 4 KiB) để bản ghi `i` nằm ở `i × stride`. Bản ghi sai phiên, sai chỉ số hoặc hỏng header bị từ chối trước khi dùng dữ liệu |
| Nạp trước | kiểm chỗ trống (tập làm việc + `--reserve-free-mib`), thiếu thì `INVALID`. Mỗi bản ghi được **tính hai lần song song** trên hai nhóm CPU rời nhau (`sched_setaffinity`, bộ đệm riêng); lệch → lỗi `chain-prefill-oracle` / `cpu_or_ram`, dựng lại bằng seed kế tiếp. Ghi `O_DIRECT \| O_DSYNC` qua `DirectFileBackend` |
| Đọc | cỡ I/O xoay theo catalog; mỗi bản ghi đọc bằng `--read-queue-depth` handle `O_DIRECT` chỉ đọc (mặc định 8, mỗi handle một luồng, chunk `i` → handle `i mod QD`); giải mã header → dựng `Corpus::from_parts` → so SHA với header. Sai thì đọc lại: lần đọc lại sạch → `ssd_read_path_transient`, vẫn sai → `ssd_stored_data` |
| GPU | nạp corpus đã đọc vào pipeline async (`CudaBackend::load_corpus`, không tự dựng từ seed), chạy `G` generation (`--generations-per-load`, mặc định 8), kiểm từng bit. Sai thì dùng `last_burst_attribution`: bộ đệm host đổi → `host_ram`; không thì đọc lại từ SSD, khác → `ssd_read_path_transient`, giống → `gpu_vram_pcie`. Sai lặp lại ở cùng bản ghi → tính lại đáp án hai lần trên CPU, lệch header → `stored_answer_prefill` |
| Đường đọc | `H_0 = SHA("DITCHAN1-path", phiên, seed)`, `H_{t+1} = SHA(H_t, chỉ số, SHA input, SHA đáp án, số batch đã kiểm, ok)`, bản ghi kế tiếp = `H_{t+1} mod số bản ghi`. Cuối phiên phát lại từ seed, so `H_final` |
| Phép tính | `--arith alternate` (mặc định) xoay u32 / FP32 theo bản ghi, thêm INT8 khi GPU có tensor core |

**Khác so với mục 4.2:** mỗi bước băm **danh tính đã được kiểm** (chỉ số, SHA input, SHA đáp án, số batch, ok) chứ không
băm đầu ra GPU. Đầu ra GPU đã so từng bit với đáp án, nên hai cách mạnh ngang nhau ở chỗ phát hiện lỗi. Cách này phát lại
được mà không cần GPU, và mỗi bước rẻ hơn. Đường đọc vẫn phụ thuộc kết quả: một bước sai (`ok = false`) đổi mọi bước sau.

Chèn lỗi (`--inject`), mỗi loại một lần ở lần nạp thứ 2: `ssd-read` (lật bit trong bộ đệm sau khi đọc), `host-ram`
(lật bit input sau khi kiểm SHA, trước khi upload), `gpu` (lật bit đầu ra trên device), `prefill-oracle` (lật bit đáp án
của lần tính thứ hai). Sau lỗi, chuỗi vẫn chạy tiếp để gom bằng chứng và kết thúc FAIL.

### 10.2 Số đo trên GTX 960 + NVMe Intel

Evidence: `build/dit-chain-20260925-191905/` (`SUMMARY.md`, report JSON, log).

| Lần chạy | Kết quả |
|---|---|
| Dương tính: 1 GiB, 60 s, `G` = 8 | **PASS**. Nạp trước 30 bản ghi (1,06 GB) trong 5,9 s, 0 lệch oracle. 456 lần nạp, 14,94 GiB đọc ở 0,25 GiB/s, 456/456 SHA đúng. GPU 3648 batch, 1,94 T phép tính được kiểm (~32 G phép tính/s). Phát lại đường đọc khớp |
| `ssd-read` | FAIL, `chain-read` → `ssd_read_path_transient` |
| `host-ram` | FAIL, `chain-gpu` → `host_ram` |
| `gpu` | FAIL, `chain-gpu` → `gpu_vram_pcie` |
| `prefill-oracle` | FAIL, `chain-prefill-oracle` → `cpu_or_ram` |

Nhận xét:

- ~~Tốc độ đọc 0,25 GiB/s bị GPU giới hạn~~ — **nhận định này sai**, đã sửa ở mục 10.3: với `G` = 1 tốc độ đọc gần như
  không đổi. Giới hạn là đọc đồng bộ từng I/O một và phần giải mã + SHA trên CPU.
- Tập làm việc 1 GiB vẫn nằm trong cache của máy/SSD. Lần chạy đó ghi cảnh báo đúng như quyết định 3(a). Muốn đo đường
  đọc thật thì chạy với 16 GiB trở lên.
- Chưa kiểm INT8 trong chuỗi, vì GTX 960 không có tensor core.

### 10.3 Bước 0: giao diện cho scheduler và số đo mới (2026-09-25)

Evidence: `build/dit-chain-step0-20260925-200652/` (`SUMMARY.md`).

**Giao diện:**

| Lệnh / cờ | Việc |
|---|---|
| `dit-chain prefill --dir D` | chỉ nạp trước; giữ `dit-chain-<phiên>.bin` và manifest `.json` (header niêm phong của mọi bản ghi + SHA-256 của danh sách header, nhóm CPU oracle, các lần lệch) |
| `dit-chain run --existing F [--keep-file]` | đọc lại file đã nạp, không nạp trước; hình dạng lấy từ manifest; manifest hỏng → `INVALID`. Mỗi lần chạy có `nonce` riêng trộn vào `H_0`, nên hai pha trên cùng file đọc theo hai đường khác nhau; report ghi `nonce` để phát lại |
| `run --dir D` | như trước (nạp trước + đọc, xoá file); `--keep-file` ghi thêm manifest để dùng lại |
| `--progress-file P` | heartbeat JSON thay nguyên tử mỗi ~1 s: `phase` (`opening` / `prefill` / `reading` / `finishing` / `done`), `ready` (true khi mọi cấp phát của pha đọc đã xong), `host_rss_bytes`, `host_peak_bytes` = `VmHWM` + 3 × stride (bộ đệm của nhánh lỗi), `vram_bytes`, số lần nạp, tốc độ đọc và GPU của cửa sổ gần nhất, `G` hiện tại, **bộ đếm lỗi** `failures`, `verdict` ở cuối |
| `--target-read-gibps X` | tự chỉnh `G` (1–64) mỗi cửa sổ 2 s theo `G × đo được / mục tiêu`, mỗi lần đổi tối đa ×2. Không đạt mục tiêu ngay cả ở `G` = 1 → report ghi cảnh báo "đường đọc hoặc CPU là giới hạn" |
| `--read-queue-depth N` | số lần đọc đồng thời trên một bản ghi (mặc định 8) |
| SIGTERM / Ctrl-C | xong bước đang chạy, ghi report rồi mới xoá file. Đo được: dừng trong 0,15 s khi đang đọc, 0,09 s khi đang nạp trước (file dở dang bị xoá). Kịp trong 2 s ân hạn của watchdog |
| `report.reads.stage_seconds` | thời gian từng khâu: `ssd_read`, `decode_hash`, `upload`, `gpu_verify` — khâu lớn nhất là nút thắt |

Binary được đóng vào image: `/opt/dit-chain/dit-chain`, có trong `MANIFEST.sha256` và `modules.d/cpu-nvme.json`
(`launchers.chain`). Tài liệu này đi kèm ở `/usr/share/doc/dit-suite/`.

**Số đo (Ryzen 7 3700X có SHA-NI, GTX 960, NVMe Intel):**

| Lần chạy | Đọc | GPU kiểm | ms mỗi lần nạp: đọc / giải mã+SHA / upload / GPU |
|---|---|---|---|
| 1 GiB, QD 1, `G` 1 (trước khi sửa) | 0,28 GiB/s | 4,6 G/s | 60,6 / 45,0 / 9,3 / 3,5 |
| 1 GiB, QD 8, `G` 1 (trước khi sửa giải mã) | 0,37 GiB/s | 6,1 G/s | 31,1 / 44,2 / 9,1 / 3,5 |
| 1 GiB, QD 8, `G` 1 (sau khi bỏ một lần chép trong giải mã) | 0,44 GiB/s | — | 30,7 / 31,9 / 9,2 / 3,5 |
| **16 GiB** (488 bản ghi, nạp trước 94,7 s), QD 8, `G` 8, 180 s | **0,34 GiB/s**, 60,5 GiB | 44 G/s | 36,7 / 32,0 / 9,2 / 19,4 |
| **16 GiB**, QD 8, `G` 1, 120 s | **0,41 GiB/s**, 49,0 GiB | 6,7 G/s | 36,4 / 31,3 / 9,0 / 3,5 |

Mọi lần chạy PASS, phát lại đường đọc khớp. Bộ nhớ: đỉnh khai báo 377 MiB, VRAM của chuỗi 35 MiB.

Kiểm âm tính (mọi lần đều FAIL với đúng quy kết): `ssd-read`, `host-ram`, `gpu` trên `run --dir`; `gpu` trên
`run --existing`; `prefill-oracle` trên `prefill`. Thêm hai kiểm mới nhờ có file giữ lại: **lật một byte của mọi bản ghi
ngay trên đĩa** → `chain-read` / `ssd_stored_data` (lần đọc lại vẫn sai — nhánh này trước đây chưa được kiểm); **sửa một
header trong manifest** → `INVALID` "manifest header digest mismatch".

**Nhận xét:**

- Với `G` = 1, GPU chỉ chiếm 3,5 ms mỗi lần nạp; phần lớn thời gian là đọc (~36 ms cho 35 MB với catalog cỡ I/O từ
  4 KiB) và giải mã + SHA (~31 ms, CPU một luồng). Trên máy này **trần của chuỗi khoảng 0,4 GiB/s**, thấp hơn giả định
  0,4–1,6 GB/s ở mục 4.3. Chuỗi không cần làm bão hoà SSD vì lớp nền (SSD Deep) chạy song song; nhưng arbiter nên dùng
  `--target-read-gibps` và đọc `stage_seconds` thay vì bảng 4.3.
- Muốn vượt trần này phải cho nhiều bản ghi cùng được đọc và băm. Hiện chưa làm được vì bản ghi kế tiếp do kết quả của
  bước vừa xong quyết định. Cách khả thi: **đường đọc trễ L bước** (bản ghi `t + L` do `H_t` chọn), cho `L` bản ghi
  được đọc/băm song song. Đường vẫn phụ thuộc dữ liệu và phát lại được, chỉ trễ L bước. Đây là thay đổi thiết kế, cần
  chốt trước khi làm.

## 11. Kế hoạch triển khai phần còn lại (chốt 2026-09-25)

Thứ tự: **0 → 1 → 2 → 3 → 4**. Mỗi bước có unit test hoặc evidence riêng trước khi sang bước sau.

### 11.1 Quyết định bổ sung

| Câu hỏi | Chốt |
|---|---|
| Một lớp FAIL trong overlap | **Không dừng phiên.** Lỗi được ghi vào report và cộng vào **bộ đếm lỗi của lớp đó trên màn hình chung**; mọi lớp chạy tiếp hết pha để gom bằng chứng. Chỉ tiến trình chết / OOM / heartbeat trễ mới dừng mọi lớp và cho phiên `INVALID` |
| Ngưỡng nhiệt CPU cho `max` | **Không thêm.** `max` chỉ bắt buộc giới hạn nhiệt GPU (`--temp-limit`, mặc định 90 °C). Mục 5 được hiểu theo điều chỉnh này |

### 11.2 Bước 0 — hoàn thiện `dit-chain` để ghép được

| Việc | Lý do |
|---|---|
| Heartbeat JSON tương thích watchdog, thêm `phase`, `ready`, `peak_host_bytes`, `allocated_host_bytes`, bộ đếm lỗi và tiến độ | mục 6.1: khai báo đỉnh bộ nhớ + báo "đã cấp phát xong"; dashboard chung đọc bộ đếm lỗi |
| Tách `dit-chain prefill` (để lại file) và `dit-chain run --existing <file>` | quyết định 3(b): nạp trước chạy trước baseline SSD |
| Tự chọn `G` theo mục tiêu tốc độ đọc (`--target-read-gibps`) | mục 10.2: tốc độ đọc bị GPU giới hạn, `G` cố định không hợp mọi máy |
| Dừng êm khi SIGTERM: xong bước đang chạy, ghi report, xoá file | scheduler drain các lớp khi hết pha |
| Đưa binary vào image (Makefile, `build_boot_image.sh`, `test_boot_policy.sh`) | hiện chỉ có trong `target/` |
| Chạy với tập làm việc ≥ 16 GiB | đo đường đọc thật, không bị cache che |

### 11.3 Bước 1 — Arbiter (COMBINED-B), hàm thuần trong `dit-orchestrator/src/lib.rs`

- `CombinedPlan` từ `MemAvailable`, số luồng, GPU (rời / tích hợp), SSD đã chọn, mức và cường độ; thay nhánh chia
  cứng `CombinedConcurrent` hiện có (SSD 1/5 RAM).
- Chia luồng theo mục 6.3; `cpu_ram_remainder()` = `MemAvailable` sau khi các lớp khác `ready` − dự phòng 1,5 GiB −
  phần còn có thể tăng; truyền cho `dit-cli system-run --memory-mib`.
- Fail closed khi tổng đỉnh vượt `MemAvailable` hoặc phần còn lại dưới mức tối thiểu.
- Khai báo đỉnh: GPU đưa `matrix_host_bytes` ra heartbeat; SSD thêm sự kiện `started{peak_host_bytes}` vào
  `events.jsonl`.
- Unit test ma trận mục 6.4 (8/16/32/64/128 GiB, GPU tích hợp, 0/1/2 GPU, 0/1/2 SSD).

### 11.4 Bước 2 — Lập lịch pha (COMBINED-C), module `combined.rs`

```
Preflight → nạp trước chuỗi → baseline (CPU-RAM, GPU, SSD ~2 phút mỗi cái) → chuỗi riêng → overlap
→ [DEEP/SOAK: nghỉ → đọc lại retention] → drain → report
```

- Mọi lớp chạy dưới `watchdog_command()`; engine không tương tác: `gpu-vram-dit` (cờ profile Full + `--temp-limit`),
  `nvme-cli run-preserving-multi`, `dit-cli system-run`, `dit-chain run --existing`.
- Overlap khởi động theo thứ tự chuỗi → GPU → SSD, chờ `ready` từng lớp, đo `MemAvailable`, gọi arbiter rồi mới khởi
  động CPU-RAM.
- Lỗi theo 11.1: mismatch cộng bộ đếm, không dừng; tiến trình chết / OOM / heartbeat trễ → dừng tất cả, `INVALID`.
- `UnifiedDashboard` đọc heartbeat từng lớp, hiện bộ đếm lỗi từng lớp, không reset khi đổi pha.

### 11.5 Bước 3 — Report gộp (COMBINED-E)

- Adapter `adapt_gpu_report`, `adapt_chain_report` cạnh CPU-RAM / SSD.
- `SuiteReport` thêm danh sách pha (verdict từng lớp mỗi pha), timeline `MemAvailable`, `hardware_status` chung.
- Xếp hạng BOUNDED / DEEP / SOAK theo pha đã chạy thật; mismatch bất kỳ là FAIL.

### 11.6 Bước 4 — Menu COMBINED (COMBINED-F)

- `ModuleKind::Combined`; chọn mức, cường độ, SSD (dùng lại phần chọn ổ của `nvme-cli`); ghi rõ "BOUNDED là kiểm
  nhanh, kết luận ổn định cần DEEP".
- Chạy thử GTX 960 + NVMe máy phát triển, lưu evidence.

### 11.7 Tiến độ

- **Bước 0: xong** (mục 10.3). Còn lại ngoài phạm vi máy này: kiểm INT8 trong chuỗi trên GPU có tensor core.
- **Bước 1–4: xong** (mục 12): arbiter, lập lịch pha, report gộp, menu `[5] COMBINED STRESS` và màn hình chung.
- Chờ chốt: đường đọc trễ L bước (mục 10.3) nếu cần chuỗi đọc nhanh hơn ~0,4 GiB/s.
- Tiếp theo (đề xuất): thống kê FAIL theo linh kiện (CPU / RAM / PCIe / GPU / VRAM / SSD) — [COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md](COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md).
- Còn phải kiểm trên máy thật boot từ image (máy phát triển không có SSD được phép ghi và chỉ có 7,8 GB RAM): lớp SSD
  trong phiên combined, RAM giữ gần tối đa với dự phòng mặc định 1,5 GiB, DEEP/SOAK đủ thời lượng.

## 12. COMBINED: cài đặt bước 1–4 (2026-09-25)

### 12.1 Thành phần

| Thành phần | Nơi | Việc |
|---|---|---|
| Logic thuần (bước 1) | `crates/dit-orchestrator/src/combined.rs` | lịch pha theo mức (mục 5), chia luồng `balanced`/`max` (mục 6.3), ngân sách host của GPU (1/16 `MemAvailable`, 512 MiB–2 GiB), đỉnh khai báo từ baseline (HWM × 1,25 + 64 MiB), `check_fits` (từ chối trước khi chạy), `cpu_ram_remainder_mib` ("cấp phát sau cùng lấy phần còn lại"), verdict phiên và độ sâu đạt được. Có unit test cho ma trận 8–128 GiB và 0–2 GPU/SSD |
| Giữ vùng SSD | `cpu-nvme-dit combined-storage` | dùng lại picker, kiểm an toàn và chuẩn bị vùng tự động của CPU-SSD; SSD đầu tiên được chọn vùng đủ chỗ cho cả chuỗi (16 GiB, hoặc 4 GiB với mức Light); ghi `storage-plan.json`, rời process group của terminal (Ctrl+C không tháo mount khi test còn chạy) và chỉ giải phóng khi nhận SIGTERM hoặc tiến trình cha biến mất |
| Lập lịch pha (bước 2) | `combined_run.rs` | chạy từng pha; mỗi lớp là tiến trình con dưới `dit-watchdog`, log riêng; overlap khởi động theo thứ tự chuỗi → GPU → SSD (mỗi lớp chờ `ready`), rồi đo `MemAvailable`, gọi arbiter và mới khởi động CPU-RAM với `--memory-mib`; mọi lớp kết thúc cùng lúc |
| Lớp và tiến độ | `combined_layers.rs` | lệnh của từng lớp (CPU-RAM `system-run`, GPU profile Full `--devices all`, SSD `run-preserving-multi`, chuỗi `prefill` / `run --existing`), đọc progress / event journal, kết quả từ report, bộ nhớ của cây tiến trình (`VmRSS`/`VmHWM`) |
| Màn hình | `combined_view.rs` | cùng khung `UnifiedDashboard` với các test khác (xem 12.2) |
| Report gộp (bước 3) | `combined-report.json` | mức, cường độ, luồng, ngân sách, arena CPU-RAM, kế hoạch lưu trữ, mỗi pha (mỗi lớp: kết quả, số lỗi, số phép tính, đỉnh bộ nhớ, thư mục), timeline `MemAvailable` mỗi 5 s, verdict, độ sâu đạt được; cả thư mục phiên được chép một lần sang `runs/<id>/combined` trên phân vùng kết quả |
| Menu (bước 4) | `dit-suite` | mục `[5] COMBINED STRESS`: chọn mức (BOUNDED ghi rõ là kiểm nhanh), số giờ SOAK, cường độ, giới hạn nhiệt GPU (bắt buộc với `max`), rồi chọn SSD; lệnh không tương tác `dit-suite combined` cho tự động hoá |

Quy tắc lỗi (mục 11.1): mismatch ở một lớp cộng vào bộ đếm FAIL của lớp đó trên màn hình và trong report, các lớp khác
chạy tiếp tới hết pha. Lớp kết thúc **không có report** (crash, OOM, watchdog giết) dừng mọi lớp (SIGINT tới watchdog
của từng lớp), phiên `INVALID`. Ctrl+C cũng dừng mọi lớp và giữ report. Không có ngưỡng nhiệt CPU; giới hạn nhiệt GPU
90 °C bắt buộc với `max`.

Verdict phiên: có FAIL → `FAIL`; có lớp INVALID / crash, dừng giữa chừng hoặc thiếu pha → `INVALID`; còn lại `PASS`,
hoặc `PASS (HW WARNINGS)` khi GPU báo lỗi phần cứng đã sửa. Độ sâu (BOUNDED/DEEP/SOAK) chỉ ghi khi mọi pha đã chạy
hết.

### 12.2 Màn hình

Cùng bố cục với các test đơn lẻ:

- **Trái (SYSTEM HARDWARE):** CPU, số luồng, RAM, từng GPU, từng SSD, kế hoạch luồng (RAM / ORACLE / GPU / SSD / CHAIN /
  OS), kế hoạch bộ nhớ (arena CPU-RAM, host của GPU, dự phòng), lưu trữ (mức ghi, tập làm việc của chuỗi), bo mạch, kernel.
- **Phải (LIVE UTILIZATION):** CPU (GHz / nhiệt độ / W), RAM (đang dùng / tổng / test đang giữ / còn trống), mỗi GPU
  (usage, xung, nhiệt, W) và VRAM (đang dùng / tổng / xung bộ nhớ), mỗi SSD (tốc độ đọc/ghi từ `/proc/diskstats`, nhiệt
  độ, đỉnh), tốc độ đọc của chuỗi (G, số lần nạp).
- **Dưới (TESTING / PROCESS):** trạng thái, thanh tiến trình của cả phiên, pha hiện tại, các lớp đang chạy, tổng phép tính,
  FAIL TOTAL; mỗi lớp một dòng (trạng thái, bộ đếm FAIL, chi tiết); dòng các pha (`[OK]` xong, `[>>]` đang chạy, `[--]`
  chưa chạy, `[!!]` lỗi).

### 12.3 Kiểm tra trên máy phát triển (Ryzen 7 3700X, GTX 960, 7,8 GB RAM)

Máy này không có SSD được phép ghi (NVMe duy nhất chứa root filesystem, CPU-SSD từ chối — đúng) và chỉ còn ~1,4 GiB
`MemAvailable`. Vì vậy chạy BOUNDED rút còn 20% thời lượng, dự phòng 256 MiB, CPU-RAM tối thiểu 128 MiB (cờ ẩn chỉ dùng
để kiểm), lớp SSD tắt, chuỗi đặt trong `build/chain-test`. Với cấu hình mặc định, phiên **từ chối chạy ngay từ đầu**
("declared peaks 1024 MiB + reserve 1536 MiB + CPU-RAM minimum 1024 MiB exceed MemAvailable 1460 MiB") — đúng quy tắc
fail closed.

| Lần chạy | Kết quả |
|---|---|
| Dương tính | **PASS**, độ sâu BOUNDED: nạp trước 2 GiB 12 s, baseline CPU-RAM / GPU, chuỗi riêng, overlap 60 s với CPU-RAM + GPU-VRAM + chuỗi cùng lúc. Arbiter cấp CPU-RAM 445 MiB sau khi các lớp khác sẵn sàng. Report 38 file được chép sang phân vùng kết quả, file chuỗi bị xoá |
| Chèn lỗi chuỗi trong overlap | CHAIN `FAIL 1` trên màn hình, FAIL TOTAL 1; CPU-RAM và GPU-VRAM chạy tiếp tới hết pha và PASS; phiên `FAIL`, mã thoát 2 |
| SIGKILL `dit-chain` giữa overlap | phiên dừng, `INVALID`: CHAIN `CRASHED` (signal 9), CPU-RAM và GPU-VRAM `INVALID` ("stopped by the session"); không còn tiến trình con |
| SIGINT (Ctrl+C) cả nhóm tiến trình giữa baseline GPU | mọi lớp dừng, report vẫn ghi, `stopped_by_operator`, `INVALID`; file chuỗi bị xoá |
| Menu thật trong pseudo-terminal | `[5]` → BOUNDED / balanced / không giới hạn nhiệt → picker SSD thật → `N` → từ chối fail closed (chính sách mặc định) → quay lại menu |
| Dương tính sau các sửa bên dưới | **PASS**; màn hình overlap: RAM 93%, test giữ 1,13 GiB, GPU 75% / 85 W, SSD đọc 158 MiB/s, chuỗi 0,14 GiB/s |

Sửa trong lúc kiểm:

- Lớp bị phiên dừng (drain, Ctrl+C) mà không có report là `INVALID`, không phải `CRASHED`; chỉ lớp tự kết thúc không
  report (signal, OOM, watchdog giết) mới là `CRASHED` và dừng phiên.
- Engine từ chối cấu hình (mã thoát 3, không report) là `INVALID`, kèm thông báo của engine; không dừng phiên.
- Arena CPU-RAM từng vượt giới hạn an toàn của chính `dit-cli` vì bộ nhớ dịch chuyển giữa lúc đo và lúc khởi động:
  dự phòng của engine trong overlap nay là một nửa dự phòng của arbiter (tối đa 512 MiB).
- GPU có mặt hay không lấy theo danh sách `hardware_eligible` của chính GPU engine (VGA của BMC hay renderer phần
  mềm không khởi động lớp GPU); chuỗi cần một thiết bị CUDA hợp lệ.

Evidence: `build/combined-20260925-210335/SUMMARY.md` (report từng lần chạy, bản chụp màn hình dựng lại từ terminal).

### 12.4 Sửa sau lần chạy thật đầu tiên (2026-09-26)

| Hiện tượng trên máy thật | Nguyên nhân | Sửa |
|---|---|---|
| Reboot / power off chờ "Job cpu-dit.service/stop running (… / 1min 30s)" | `dit-chain` và `cpu-nvme-dit` bật feature `termination` của `ctrlc`; Cargo hợp nhất feature khi build chung, nên `dit-suite` cũng bắt SIGTERM nhưng handler của menu chỉ đặt cờ, không thoát → systemd chờ đủ 90 s | bỏ feature đó; `dit-chain` và bộ giữ vùng SSD nhận SIGINT/SIGTERM/SIGHUP bằng một luồng `sigwait` riêng (hai tiến trình này không sinh tiến trình con cần SIGTERM); `dit-suite` và watchdog trở lại thoát ngay khi nhận SIGTERM; unit thêm `TimeoutStopSec=10s`. Đo: `dit-suite` thoát 0,05 s sau SIGTERM, Ctrl+C ở menu vẫn bị bỏ qua, tiến trình con có mặt nạ tín hiệu rỗng |
| "NTFS safety probe refused … exit status: 14" dù đã Shut down Windows | mã 14 = volume đang hibernate: Windows Fast Startup (mặc định bật) khiến "Shut down" thực chất là hibernate | không ép ghi (dữ liệu Windows vẫn an toàn); thông báo nay nói rõ cách xử lý: giữ Shift khi bấm Shut down, hoặc Restart rồi boot USB, hoặc `powercfg /h off` / tắt "Turn on fast startup". Mã 15 (đóng không sạch), 13 (hỏng metadata), 16 (đang dùng) cũng có hướng dẫn riêng |
| Sau lỗi SSD, màn hình ghi "continues without the SSD layer" nhưng lại "Press Enter to return to the control center" | câu nhắc dùng chung | chọn `R` thử chọn SSD lại, `C` chạy tiếp không SSD và không chuỗi, `Q` về menu |
| (phòng ngừa) máy tắt giữa phiên để lại file chuỗi 16 GiB trên SSD | file chuỗi được giữ giữa các pha; người xoá là `dit-suite`, vốn bị tắt trước | bộ giữ vùng SSD xoá `dit-chain-*.bin/.json` trong thư mục do chính nó tạo trước khi tháo, và thử tháo lại tới 6 s nếu mount còn bận |
