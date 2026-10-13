# Thống kê FAIL theo linh kiện — thuật toán và roadmap

Trạng thái: đề xuất, 2026-09-28. Bối cảnh: một PC Ryzen 7 7800X3D hỏng lane PCIe của CPU, nhưng phiên COMBINED chỉ
báo FAIL ở lớp GPU-VRAM, không nói được lỗi nằm ở đường truyền PCIe hay ở GPU/VRAM. Mục tiêu duy nhất của tài liệu
này: **đếm FAIL đúng linh kiện**, theo phiên và theo máy.

Ngoài phạm vi: khoanh vùng bên trong một link PCIe (CPU, khe cắm, riser hay card). Thay linh kiện là kiểm được, nên
tool không đưa ra gợi ý nào về việc đó.

## 0. Định nghĩa

| Khái niệm | Nghĩa |
|---|---|
| **Linh kiện (ô)** | `CPU`, `RAM`, `PCIe-GPU <n>` (link PCIe của GPU n), `GPU <n>`, `VRAM <n>`, `SSD <n>`, `SSD-path <n>` (đường đọc/ghi của SSD: controller, link PCIe, DMA), `NỀN TẢNG` (nguồn, nhiệt, crash, OOM), `CHƯA TÁCH {tập ứng viên}` |
| **Sự kiện lỗi** | đơn vị đếm: một batch GPU, một vùng của lane, một bản ghi của chuỗi, một block/extent SSD, một cửa sổ RAM, một burst compute oracle. Số từ/bit sai là bằng chứng của sự kiện, không cộng vào số đếm |
| **Mức chắc chắn** | `XÁC ĐỊNH`: một điểm kiểm tại biên hoặc một phép xác nhận chứng minh lỗi nằm ở ô đó. `NGHI NGỜ`: xếp hạng từ bằng chứng gián tiếp, ghi kèm ô đứng thứ hai. `CHƯA TÁCH`: đếm vào ô ghép, ví dụ `GPU\|VRAM\|PCIe` |
| **Hai loại số đếm** | **lỗi dữ liệu** (kết quả sai, làm verdict là FAIL) và **sự kiện phần cứng** (lỗi đã được phần cứng tự sửa: ECC CE, PCIe replay, AER corrected — là cảnh báo). Không cộng lẫn hai loại |

**Quy lỗi ngay trong lần chạy, không chạy lại.** Mỗi sự kiện lỗi được xếp ô vào lúc nó xảy ra, trong chính phiên
test. Màn hình và report đếm theo ô ngay trong lúc chạy; không có lần chạy thứ hai để "tìm nguồn lỗi":
- Điểm kiểm tại biên (2.1) nằm sẵn trong đường chạy bình thường, cho mọi batch, bản ghi, block hay cửa sổ.
- Bước xác nhận (2.2) chạy ngay trên đúng đơn vị vừa lỗi (copy lại, đọc lại, upload lại một đơn vị), mất vài mili-giây.
- Khi lỗi dồn dập: mọi sự kiện vẫn đi qua điểm kiểm; bước xác nhận bị giới hạn (ví dụ 20 lần mỗi giây cho mỗi ô).
  Sự kiện đã xác nhận đếm là `XÁC ĐỊNH`; sự kiện chỉ có điểm kiểm đếm là `NGHI NGỜ` vào cùng ô mà điểm kiểm chỉ ra.

Quy tắc nền: verdict dữ liệu vẫn do oracle của CPU quyết định như hiện nay. Việc quy linh kiện chỉ **thêm thông tin**,
không bao giờ biến một FAIL thành PASS.

## 1. Có phải sửa code tính toán và truyền dữ liệu không?

**Không phải viết lại thuật toán test**: pattern RAM, GEMM, oracle, Deep SSD và chuỗi giữ nguyên. Cần **thêm điểm kiểm**
và **bước xác nhận khi lỗi** vào đường dữ liệu. Riêng GPU phải sửa trong đường truyền và tính toán.

| Engine | Hiện có | Còn thiếu | Loại thay đổi |
|---|---|---|---|
| CPU-RAM (`dit-system`) | mismatch có địa chỉ, syndrome XOR, CPU ghi và CPU kiểm; rerun cùng seed phân loại `reproducible_same_route` / `not_reproduced_transient`; compute oracle (FMA / branch / AES) có kết quả riêng; EDAC | kiểm lại **từ CPU khác**, ghi lại rồi kiểm lại để tách **CPU với RAM**; địa chỉ vật lý (pagemap) để gom theo trang/kênh | thêm bước xác nhận trong đường verify (nhỏ) + log |
| GPU-VRAM (`gpu-cuda`) | CPU so toàn bộ kết quả sau D2H; `burst_attribution` (host đổi / corpus còn nguyên); lane có pha `transfer` / `retention` / `vram-self-check` / `vram-checkpoint`; sweep kiểm ngay trên GPU; Xid, link, tổng AER | **điểm kiểm trên GPU**: băm input ngay sau H2D, GPU tự so kết quả trước D2H, tách chiều H2D/D2H cho DMA của lane, giao thức xác nhận (copy lại, băm lại, upload lại vào vùng khác) | **sửa đường truyền và tính toán** (kernel + pipeline), phần lớn công việc nằm ở đây |
| SSD (`nvme-engine`) | loại lỗi (mismatch / short read / I/O / timeout / mất thiết bị); đọc lại toàn bộ ở chế độ sustained | **đọc lại ngay khi mismatch** để tách "dữ liệu lưu sai" (SSD) với "đọc sai thoáng qua" (SSD-path); link và AER của NVMe | thêm bước xác nhận trong đường đọc + log |
| Chuỗi (`dit-chain`) | cây quy lỗi đầy đủ: `ssd_read_path_transient`, `ssd_stored_data`, `host_ram`, `gpu_vram_pcie`, `stored_answer_prefill`, `cpu_or_ram` | `gpu_vram_pcie` được tách tiếp khi GPU có điểm kiểm (chuỗi dùng cùng pipeline, được hưởng miễn phí) | chỉ đổi cách map |
| `dit-suite` | verdict và số lỗi theo lớp | sổ lỗi, tương quan với sự kiện phần cứng, màn hình, CSV | mới, không đụng engine |

## 2. Thuật toán

### 2.1 Chia đường dữ liệu thành đoạn, đặt điểm kiểm ở biên

Mỗi phép test là một đường `s0 → s1 → … → sn`. Sau mỗi đoạn có một điểm kiểm `c_i` (hash hoặc so sánh, tính ở đầu
nhận). **Lỗi nằm ở đoạn `k` sao cho `c_{k-1}` đúng và `c_k` sai.** Tập ứng viên là các linh kiện mà đoạn `k` đi qua;
tập chỉ có một phần tử thì kết luận `XÁC ĐỊNH`.

| Đường | Đoạn → linh kiện | Điểm kiểm (★ = cần thêm) |
|---|---|---|
| GPU GEMM | RAM host (staging) → **H2D** (PCIe↓) → VRAM → GPU tính → VRAM → **D2H** (PCIe↑) → RAM host → CPU so | c1 hash host trước khi upload (có); ★c2 GPU băm input sau H2D; ★c3 GPU tự so kết quả với đáp án (đáp án ~1 MB, upload một lần, có hash); c4 CPU so sau D2H (có); c5 băm lại bộ đệm host (có) |
| Lane DMA GPU | pattern host → H2D → VRAM (lưu) → D2H → CPU kiểm | ★GPU băm vùng vừa nhận (tách H2D); ★GPU băm lại trước khi đọc về (tách lưu trữ VRAM với D2H) |
| Sweep VRAM | GPU ghi → VRAM → GPU kiểm | toàn bộ trên GPU (có) → `VRAM` hoặc `GPU` |
| CPU-RAM | CPU ghi → RAM → CPU đọc/kiểm | ★kiểm lại từ CPU khác; ★ghi lại từ CPU khác rồi kiểm |
| SSD | CPU tạo → RAM → DMA/PCIe → SSD → PCIe/DMA → RAM → CPU kiểm | ★đọc lại cùng block (O_DIRECT) |
| Chuỗi | SSD → RAM → PCIe → VRAM → GPU → PCIe → RAM → CPU | header và SHA sau khi đọc (có), rồi các điểm của đường GEMM |

Bảng quyết định cho đường GEMM, sau khi có ★:

| c2 GPU băm input | c3 GPU tự so | c4 CPU so | Xác nhận | Ô |
|---|---|---|---|---|
| sai | – | – | upload lại vào **vùng VRAM khác**, băm lại → đúng | `PCIe-GPU` (chiều xuống) |
| sai | – | – | sai lặp lại ở **cùng địa chỉ** sau khi upload lại | `VRAM` |
| đúng | sai | sai, cùng vị trí | GPU tính lại cùng input → vẫn sai | `GPU` (lỗi ổn định) |
| đúng | sai | sai | tính lại → đúng | `GPU\|VRAM` (thoáng qua), xếp hạng theo 2.5 |
| đúng | đúng | sai | copy D2H lần hai khác lần đầu | `PCIe-GPU` (chiều lên) |
| đúng | đúng | sai | host buffer đã đổi (c5) | `RAM` |

Điểm kiểm trên GPU có thể sai theo chính GPU hỏng (một GPU hỏng cũng có thể băm sai). Vì thế nó chỉ dùng để
**khoanh vùng**. Ô `XÁC ĐỊNH` đòi hỏi bước xác nhận cho kết quả nhất quán, ví dụ upload lại vào vùng khác mà băm đúng.

### 2.2 Xác nhận khi lỗi

Chỉ chạy khi có sự kiện lỗi, nên không tốn gì lúc bình thường.

| Phép | Kết quả → ý nghĩa |
|---|---|
| **Đọc lại / copy lại** cùng dữ liệu | lần hai khác lần một → đường truyền (thoáng qua); lần hai giống lần một và vẫn sai → dữ liệu lưu đã sai |
| **Ghi lại rồi đọc** ở cùng địa chỉ | vẫn sai ở cùng địa chỉ và cùng bit → ô nhớ (`RAM` / `VRAM` / `SSD`); lần này đúng → lỗi lần ghi trước (đường ghi hoặc CPU ghi) |
| **Chuyển sang vùng khác** | lỗi đi theo địa chỉ cũ → ô nhớ; lỗi xuất hiện lại ở vùng mới → đường truyền hoặc bộ tính |
| **Tính lại trên đơn vị khác** (CPU khác, nhóm CPU khác, GPU khác nếu có) | đơn vị khác đúng → đơn vị đầu tiên (`CPU` core / `GPU`) |
| **Lặp N lần** | tỉ lệ tái hiện: 0/N → thoáng qua; N/N → ổn định |

### 2.3 Đo sự kiện phần cứng theo cửa sổ thời gian

Lấy mẫu 1 giây một lần, lưu delta theo cửa sổ và tổng theo pha:

| Nguồn | Đọc từ | Ô |
|---|---|---|
| PCIe replay | NVIDIA `nvidia-smi -q -d PCIE` (Replays Since Reset, Replay Number Rollovers); AMD `…/device/pcie_replay_count` | `PCIe-GPU n` |
| AER theo loại (RxErr, BadTLP, BadDLLP, ReplayTimer, Rollover, NonFatal, Fatal) | sysfs `aer_dev_*` ở **cả hai đầu** của link | `PCIe-GPU n` / `SSD-path n` |
| Trạng thái link | `current_link_speed/width` ở **cả hai đầu**; bwctrl / log "link down" | tụt dưới tải → `PCIe-GPU n` / `SSD-path n` |
| Lỗi báo qua firmware | kernel log `[Hardware Error]` (APEI/GHES), rasdaemon | theo nguồn: nhân CPU → `CPU`; PCIe → link của thiết bị được nêu; bộ nhớ → `RAM` |
| EDAC | `ce_count` / `ue_count` | `RAM` |
| Xid NVIDIA | kernel log (đã có) | 48/63/64 → `VRAM`; 79 → `PCIe-GPU\|NỀN TẢNG`; 13/31/43 → `GPU` |
| SMART NVMe | media errors, CRC errors, unsafe shutdowns | `SSD` / `SSD-path` |
| Nhiệt độ, công suất, throttle | đã có | `NỀN TẢNG` (bằng chứng phụ) |

**Gắn sự kiện vào link:** mỗi GPU và mỗi SSD có một link, gồm thiết bị và port ngay phía trên nó (đọc từ sysfs). Sự
kiện ở một trong hai đầu của link được tính cho ô `PCIe-GPU n` hoặc `SSD-path n` của đúng thiết bị đó.

### 2.4 Tương quan sự kiện dữ liệu với sự kiện phần cứng

Với mỗi sự kiện lỗi dữ liệu tại thời điểm `t`, tìm sự kiện phần cứng trên các ô ứng viên trong cửa sổ `[t-Δ, t+Δ]`
(Δ = 2 s). Xác suất trùng ngẫu nhiên là `p = 1 - exp(-λ·2Δ)`, với `λ` là tần suất nền của sự kiện phần cứng đó, đo ở
pha baseline. Nếu có `k/n` sự kiện lỗi trùng và `P(Binomial(n, p) ≥ k) < 0,01` thì coi sự kiện phần cứng đó **ủng hộ**
ô tương ứng. Đây chỉ là bằng chứng xếp hạng (`NGHI NGỜ`), không dùng để kết luận `XÁC ĐỊNH`.

### 2.5 Chữ ký lỗi (thống kê trên nhiều sự kiện)

| Chữ ký | Cách tính | Gợi ý |
|---|---|---|
| **Vị trí bit** | histogram của bit trong syndrome XOR; kiểm χ² so với phân bố đều | một bit cố định ở nhiều địa chỉ → một đường dữ liệu (DQ) của chip nhớ; bit ngẫu nhiên → đường truyền hoặc nhiễu |
| **Địa chỉ** | lặp lại cùng địa chỉ; gom theo trang, stride, vùng | cùng địa chỉ → ô nhớ; cùng stride hoặc vùng → bank/kênh; rải ngẫu nhiên → đường truyền |
| **Chiều truyền** | tỉ lệ lỗi H2D so với D2H | lệch hẳn một chiều → một cặp phát/nhận của link |
| **Phụ thuộc pha** | tần suất lỗi ở baseline so với overlap, kiểm Poisson | chỉ lỗi khi overlap → phụ thuộc tải, nguồn hoặc nhiệt (`NỀN TẢNG` là nghi phạm phụ) |
| **Nhiệt độ / công suất lúc lỗi** | giá trị tại thời điểm lỗi so với phân bố của cả phiên | lỗi dồn về lúc nóng hoặc lúc công suất cao |

### 2.6 Kết hợp bằng chứng

1. **Tầng 1 — luật xác định:** áp bảng 2.1 và 2.2. Ra được một ô thì kết luận `XÁC ĐỊNH`.
2. **Tầng 2 — chấm điểm:** nếu còn nhiều ứng viên, tính `điểm(ô) = Σ w_e` trên các bằng chứng `e` ủng hộ ô đó (2.3,
   2.4, 2.5). Trọng số được ghi trong bảng cấu hình có phiên bản. Kết quả là `NGHI NGỜ: <ô 1> (điểm), <ô 2> (điểm)`.
   Tầng 2 **không bao giờ** nâng một ô lên `XÁC ĐỊNH`.
3. **Không đủ bằng chứng:** `CHƯA TÁCH {tập ứng viên}`.

Mỗi sự kiện ghi lại những luật và bằng chứng đã áp dụng, để có thể đọc lại và kiểm tra vì sao ra kết luận đó.

### 2.7 Đếm và khử trùng lặp

- Một sự kiện cho mỗi đơn vị (batch / vùng / bản ghi / block / cửa sổ). Các mismatch trong cùng đơn vị được gộp vào
  bằng chứng của sự kiện đó.
- **Bão lỗi:** từ sự kiện thứ 64 của cùng một ô trở đi chỉ tăng bộ đếm, không lưu thêm chi tiết. Mỗi ô ghi thời điểm
  lỗi đầu tiên và tần suất lỗi theo pha.
- Đếm theo pha (baseline / chuỗi riêng / overlap / retention) để thấy lỗi có phụ thuộc tải hay không.

## 3. Định dạng dữ liệu

**`fault-events.jsonl`** (mỗi lớp ghi trực tiếp một file; `dit-suite` gộp lại):

```json
{"t_ms": 812345, "layer": "gpu", "phase": "overlap", "unit": "batch 1042",
 "path": "gemm", "failed_checkpoint": "c4_cpu_compare", "passed_checkpoints": ["c1","c2","c3"],
 "confirm": [{"op": "d2h_recopy", "result": "differs_from_first"}],
 "component": "PCIe-GPU 1", "confidence": "confirmed", "candidates": ["PCIe-GPU 1"],
 "evidence": {"words": 64, "bit_positions": [7], "replay_delta_2s": 31, "aer_root_rxerr_2s": 4},
 "rules": ["2.1/row5"], "device": "0000:01:00.0"}
```

**Report** (`combined-report.json` và report của từng module): mục `fault_ledger` gồm:
- `data_failures {ô: {confirmed, suspected}}`;
- `unresolved {"GPU|VRAM|PCIe": n}`;
- `hardware_events {ô: {loại: delta}}`;
- `by_phase`, `first_seen`.

**Màn hình** (phần dưới, cho mọi test chứ không riêng COMBINED):

```
FAIL BY PART  CPU 0 | RAM 0 | PCIe-GPU1 12 | GPU1 0 | VRAM1 0 | SSD1 0 | SSD1-path 0 | CHƯA TÁCH 0
HW EVENTS     RAM CE 0 | PCIe-GPU1 replay +1234 AER +56 link↓2 | Xid 0 | SSD1 media 0
```

**`fail-summary.csv`**: mỗi phiên một dòng, nối vào phân vùng kết quả của USB. Các cột: thời gian, image, version
DIT-01 (`dit_version`, từ v1.2), test/mức, mainboard, CPU, RAM, GPU, SSD, verdict, số lỗi của từng ô
(confirmed/suspected), unresolved, sự kiện phần cứng chính, đường dẫn report. Mở trong Excel là lọc và đếm được cho
nhiều máy. File dạng cũ (không có cột `dit_version`, tức bản v1.1) được đổi tên thành `fail-summary-v1.1.csv` ở lần
ghi đầu tiên của bản v1.2, nội dung giữ nguyên ([v1.2/ROADMAP.md](v1.2/ROADMAP.md), V0b).

## 4. Roadmap

| Bước | Nội dung | Code đụng tới | Kiểm chứng | Công sức |
|---|---|---|---|---|
| **F0** Sổ lỗi | phân loại từ report hiện có: map `attribution` của chuỗi, loại lỗi SSD, mismatch và compute oracle của CPU-RAM, lane / sweep / matrix / Xid của GPU vào các ô; lỗi GPU matrix → `CHƯA TÁCH GPU\|VRAM\|PCIe`; màn hình 2 dòng; `fault_ledger`; CSV | `dit-suite` (mới), không đụng engine | report có sẵn, report lỗi đã chèn | S–M |
| **F1** Sự kiện có cấu trúc, ghi trực tiếp | các engine ghi `fault-events.jsonl` ngay khi lỗi (đếm theo ô ngay trong lúc chạy, không đợi report) | chỉ phần log của 4 engine | mỗi lỗi chèn sinh đúng một sự kiện | M |
| **F2** Đo phần cứng + tương quan (2.3, 2.4) | replay counter, AER theo loại ở hai đầu link, trạng thái link, GHES/MCE, SMART, gắn vào ô của đúng thiết bị | `hwhealth` mở rộng thành module dùng chung; `dit-suite` | fixture sysfs / nvidia-smi; máy 7800X3D làm ca thật | M |
| **F3** Điểm kiểm GPU (2.1 ★, 2.2) | kernel băm; upload đáp án + kernel tự so; lane tách chiều; giao thức xác nhận; ngân sách hiệu năng ≤ 5% | `gpu-cuda` (kernel, `async_compute`, `lane`), `gpu-engine` | chèn lỗi ở từng đoạn (H2D staging, VRAM, output, D2H, host) → đúng ô, đúng mức chắc chắn | L |
| **F4** Xác nhận cho CPU-RAM và SSD | CPU-RAM: kiểm lại và ghi lại từ CPU khác, pagemap; SSD: đọc lại ngay khi mismatch | `dit-system` (đường verify), `nvme-engine` (đường đọc) | chèn lỗi RAM (ổn định / thoáng qua), chèn lỗi đọc SSD | M |
| **F6** Chữ ký và xếp hạng (2.5, 2.6) | histogram bit, cụm địa chỉ, phụ thuộc pha, trọng số có phiên bản | `dit-suite` | bộ dữ liệu lỗi tổng hợp và dữ liệu thực địa | M |
| **F7** Hiệu chỉnh thực địa | gom CSV nhiều máy; so ô tool đếm với linh kiện thực sự hỏng; chỉnh trọng số tầng 2 | tài liệu + cấu hình | tỉ lệ đếm đúng ô | liên tục |

Thứ tự: **F0 → F2 → F1 → F3 → F4 → F6 → F7**. F0 và F2 mang lại giá trị ngay mà không động vào engine. F3 là bước tách
được `PCIe-GPU` với `GPU` và `VRAM`. (Pha chẩn đoán riêng sau FAIL, như hạ tốc độ link, đã bỏ: điểm kiểm của F3 đủ để
đếm đúng ô.)

Ca 7800X3D sau từng bước:

| Sau | Màn hình / report |
|---|---|
| F0 | `CHƯA TÁCH GPU\|VRAM\|PCIe: 12` (trung thực, không đổ cho GPU) |
| F2 | như trên + `HW: PCIe-GPU1 replay +…, AER +…` → `NGHI NGỜ: PCIe-GPU1` |
| F3 | `PCIe-GPU1: 12 (XÁC ĐỊNH)` |

## 5. Kiểm chứng bằng chèn lỗi

| Chèn lỗi tại | Ô mong đợi | Mức |
|---|---|---|
| staging host sau khi băm, trước H2D | `RAM` (host) | xác định |
| input trong VRAM sau H2D (kernel ghi đè 1 từ) | `PCIe-GPU` hoặc `VRAM` (tuỳ bước xác nhận) | xác định |
| output trên GPU trước khi tự so | `GPU\|VRAM` | xác định / nghi ngờ |
| output sau khi tự so, trước D2H | `PCIe-GPU` | xác định |
| bộ đệm D2H trên host | `RAM` | xác định |
| ô VRAM hỏng cố định (sweep) | `VRAM` | xác định |
| mẫu RAM, cùng một địa chỉ | `RAM` | xác định |
| compute oracle (FMA) | `CPU` | xác định |
| block SSD, lỗi lặp lại | `SSD` | xác định |
| đọc SSD, lỗi thoáng qua | `SSD-path` | xác định |
| fixture replay / AER tăng đồng thời với lỗi dữ liệu (chưa có F3) | `NGHI NGỜ PCIe-GPU` | nghi ngờ |

## 6. Giới hạn

- Ô `PCIe-GPU` / `SSD-path` là cả link. Bên trong link (CPU, khe, riser, card) nằm ngoài phạm vi và tool không gợi ý gì.
- Nhiều BIOS consumer ẩn AER (firmware-first) → lúc đó dựa vào replay counter, trạng thái link và điểm kiểm dữ liệu.
- Điểm kiểm trên GPU chạy trên chính GPU đang bị test: chỉ dùng để khoanh vùng, verdict vẫn do CPU quyết định.
- Đường Vulkan (Intel/AMD) nhận điểm kiểm sau đường CUDA.

## 7. Kế hoạch triển khai, danh sách file và cách giữ bản ổn định

Bản hiện tại chạy ổn định, nên mọi bước phải **quay lại được** và **không đổi hành vi cũ khi tắt**.

### 7.1 Giữ bản ổn định

| Việc | Cách làm |
|---|---|
| **Chụp lại mã nguồn trước khi sửa** | Dự án chưa dùng git. Đề xuất `git init`, commit toàn bộ nguồn (bỏ `target/`, `build/`, `dist/`), tag `stable-2026-09-28`, mỗi bước F làm trên một nhánh riêng. Nếu không dùng git: `backup/DIT-01-src-stable-20260928.tar.gz` (nguồn ~6,6 MB) cùng file sha256 |
| **Giữ image ổn định** | chép `dist/dit-01-noble-amd64-nvidia.iso` (bản build 2026-09-27, có sẵn) cùng `.sha256` và manifest sang `dist/stable/`. USB ổn định luôn ghi lại được; GRUB hiện giờ build nên phân biệt được bản cũ và mới |
| **Không để bản sao file nguồn trong `crates/`** | không tạo `main_old.rs`, `async_compute_v1.rs`… Bản cũ nằm trong tag hoặc tarball. Bản sao trong cây nguồn dễ bị build nhầm và lệch dần |
| **Code mới trong file mới** | logic mới nằm ở file mới (bảng 7.2). File ổn định chỉ sửa ở **điểm nối** (vài dòng gọi sang module mới) |
| **Cờ bật/tắt** | thay đổi trên đường dữ liệu (F3, F4) nằm sau cờ, **mặc định TẮT** cho tới khi kiểm xong trên máy thật; sau đó bật mặc định nhưng vẫn giữ cờ. Tham số kernel `dit_attribution=0` (sửa từ GRUB bằng phím `e`, hoặc thêm một mục GRUB riêng) cho quay về hành vi cũ ngay trên image, không cần build lại |
| **Report chỉ thêm trường** | không đổi và không xoá trường cũ; trường mới là optional; thêm `attribution_version`. Công cụ đọc report cũ vẫn chạy |
| **Hồi quy verdict** | mỗi bước chạy lại bộ report cũ (evidence hiện có trong `build/`) để chứng minh verdict không đổi, chỉ có thêm phần quy ô |

### 7.2 Danh sách file theo bước

Ghi chú: **MỚI** = file mới; **SỬA** = file đang ổn định (kèm số dòng hiện tại). Rủi ro: T = thấp (chỉ đọc / hiển thị),
V = vừa (log / điểm nối), C = cao (đường dữ liệu).

**F0 — Sổ lỗi, màn hình, CSV** (không đụng engine)

| File | Loại | Thay đổi | Rủi ro |
|---|---|---|---|
| `crates/dit-orchestrator/src/fault_ledger.rs` | MỚI | ô, sự kiện, mức chắc chắn; bảng map attribution cũ → ô; đếm, khử trùng lặp, bão lỗi; dòng CSV; unit test | T |
| `crates/dit-orchestrator/src/fault_sources.rs` | MỚI | đọc report từng lớp (CPU-RAM `system-report.json`, GPU `report.json`, SSD `report.json`, chuỗi `report.json`) → sự kiện; nằm trong lib để test bằng report thật | T |
| `crates/dit-orchestrator/src/fault_single.rs` | MỚI | test đơn lẻ GPU-VRAM / CPU-SSD: hai dòng sổ lỗi trên màn hình kết quả + dòng CSV | T |
| `crates/dit-orchestrator/src/testdata/` | MỚI | report mẫu (có lỗi / không lỗi) cho test | T |
| `crates/dit-orchestrator/src/lib.rs` (506) | SỬA | `pub mod fault_ledger;` | T |
| `crates/dit-orchestrator/src/combined_run.rs` (1362) | SỬA | điểm nối: đưa sự kiện vào sổ khi mỗi lớp kết thúc; `fault_ledger` trong `combined-report.json`; ghi CSV | V |
| `crates/dit-orchestrator/src/combined_view.rs` (492) | SỬA | dòng `FAIL BY PART` / `HW EVENTS` | T |
| `crates/dit-orchestrator/src/main.rs` (1192) | SỬA | sau `launch_gpu` / `launch_cpu_nvme`: sổ lỗi + CSV cho test đơn lẻ | V |
| `image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh` (1209) | SỬA (tuỳ chọn) | CPU-RAM đơn lẻ: báo đường dẫn report cho `dit-suite` để ghi CSV | V |

**F2 — Đo phần cứng và tương quan**

| File | Loại | Thay đổi | Rủi ro |
|---|---|---|---|
| `crates/dit-system/src/pcie.rs` | MỚI | link ở hai đầu, AER theo loại, replay (NVIDIA `nvidia-smi -q -d PCIE`, AMD sysfs), tìm link của thiết bị | T |
| `crates/dit-system/src/lib.rs` | SỬA | export `pcie` | T |
| `crates/dit-system/src/gpu.rs` (577) | SỬA | parse bộ đếm replay từ nvidia-smi | T |
| `crates/gpu-cli/src/hwhealth.rs` (1511) | SỬA | dùng `pcie.rs` cho link/AER (giữ trường cũ, thêm trường mới) | V |
| `crates/dit-orchestrator/src/hw_events.rs` | MỚI | lấy mẫu 1 s, delta theo cửa sổ, tương quan (2.4), gắn vào ô | T |
| `crates/dit-orchestrator/src/combined_run.rs`, `combined_view.rs` | SỬA | điểm nối lấy mẫu + dòng `HW EVENTS` | T |

**F1 — Engine ghi sự kiện ngay khi lỗi** (chỉ phần log)

| File | Loại | Thay đổi | Rủi ro |
|---|---|---|---|
| `crates/dit-contracts/src/lib.rs` (286) | SỬA | kiểu `FaultEvent` (chỉ thêm) | T |
| `crates/dit-system/src/engine.rs` (1799), `compute_oracle.rs` (890) | SỬA | ghi sự kiện đúng chỗ đang tạo `SystemMismatch` / mismatch của oracle | V |
| `crates/dit-cli/src/main.rs` | SỬA | cờ `--fault-events <file>` | T |
| `crates/gpu-cli/src/main.rs` (4303), `multi.rs` (1281) | SỬA | ghi sự kiện từ failure matrix / lane / sweep / Xid; cờ | V |
| `crates/dit-chain/src/main.rs` (1270) | SỬA | `fail()` ghi thêm sự kiện | T |
| SSD | — | `events.jsonl` đã có sự kiện `Failure`: không sửa | — |

**F3 — Điểm kiểm GPU** (thay đổi đường truyền và tính toán)

| File | Loại | Thay đổi | Rủi ro |
|---|---|---|---|
| `crates/gpu-cuda/src/checkpoints.rs` | MỚI | toàn bộ logic điểm kiểm và giao thức xác nhận, để `async_compute.rs` chỉ thêm lời gọi | C |
| `crates/gpu-cuda/kernels/gemm.cu` (125) → `gemm.ptx` | SỬA | kernel băm vùng; kernel tự so kết quả với đáp án (so theo từ 32 bit, dùng chung cho u32 / FP32 / INT8); build lại PTX | C |
| `crates/gpu-cuda/src/lib.rs` (1079) | SỬA | nạp kernel mới, cập nhật `KERNEL_SHA256`, cờ | V |
| `crates/gpu-cuda/src/async_compute.rs` (1880) | SỬA | điểm nối: băm sau upload; upload đáp án; tự so sau kernel; xác nhận khi lỗi; ghi điểm kiểm vào evidence | C |
| `crates/gpu-cuda/src/lane.rs` (1160) | SỬA | băm trên GPU sau H2D và trước D2H (tách chiều) | C |
| `crates/gpu-engine/src/gemm.rs` (1026) | SỬA | hàm băm dùng chung CPU/GPU + vector kiểm | V |
| `crates/gpu-engine/src/accounting.rs` (652) | SỬA | bộ đếm theo điểm kiểm | V |
| `crates/gpu-cli/src/main.rs`, `multi.rs` | SỬA | cờ `--boundary-checks` (mặc định TẮT lúc đầu), report | V |
| `crates/gpu-cuda/tests/checkpoints.rs` | MỚI | chèn lỗi ở từng đoạn → đúng ô (bảng mục 5) | T |
| `crates/dit-chain/src/main.rs` | SỬA | tách `gpu_vram_pcie` theo điểm kiểm | V |
| `Makefile` (target `gpu-cuda-kernel`) | — | dùng như hiện tại để build lại PTX | — |

**F4 — Xác nhận khi lỗi cho CPU-RAM và SSD**

| File | Loại | Thay đổi | Rủi ro |
|---|---|---|---|
| `crates/dit-system/src/confirm.rs` | MỚI | kiểm lại từ CPU khác, ghi lại rồi kiểm, pagemap → địa chỉ vật lý | V |
| `crates/dit-system/src/engine.rs`, `report.rs` (311) | SỬA | điểm nối trong đường verify; trường bằng chứng mới; cờ `--confirm-on-fail` | C |
| `crates/dit-cli/src/main.rs` | SỬA | cờ | T |
| `crates/nvme-engine/src/deep_run.rs` (549), `deep_block.rs` (277) | SỬA | đọc lại block ngay khi mismatch | C |
| `crates/dit-contracts/src/lib.rs` | SỬA | kết quả xác nhận trong sự kiện `Failure` (optional) | T |
| `crates/nvme-cli/src/main.rs` (2558) | SỬA | cờ, report | V |

**F6 — Chữ ký lỗi và xếp hạng**

| File | Loại | Thay đổi | Rủi ro |
|---|---|---|---|
| `crates/dit-orchestrator/src/fault_signatures.rs` | MỚI | histogram bit, cụm địa chỉ, phụ thuộc pha, chấm điểm tầng 2 (trọng số có phiên bản) | T |

**F7 — Thực địa**

| File | Loại | Thay đổi | Rủi ro |
|---|---|---|---|
| `tools/fail_summary.py` | MỚI (tuỳ chọn) | gộp CSV của nhiều USB / máy | T |

**Chung cho mọi bước:** tài liệu (roadmap này, `docs/gpu-vram/STATUS.md`, `docs/cpu-ram/STATUS.md`,
`docs/cpu-nvme/STATUS.md`); test (`tools/test_boot_policy.sh`, `tools/test_unified_project.py` nếu có file đóng gói
mới); nếu thêm mục GRUB "attribution off": `image/boot/grub/grub.cfg` + `boot-runner.sh`. Script build image không
phải sửa, vì các binary đã được đóng gói sẵn.

Tổng: **9 file mới** (và thư mục `testdata/`), **25 file sửa**. 7 file có rủi ro cao (`async_compute.rs`, `lane.rs`,
`gemm.cu`, `checkpoints.rs`, `engine.rs`, `deep_run.rs`, `deep_block.rs`), đều nằm sau cờ.

### 7.3 Mốc và điều kiện xong của mỗi bước

| Bước | Xong khi |
|---|---|
| F0 | report combined và test đơn lẻ có `fault_ledger`; CSV có một dòng mỗi phiên; màn hình có hai dòng mới; verdict của bộ report cũ **không đổi**; `make test` pass |
| F2 | bộ đếm replay, AER, link đọc được trên GTX 960 và trên máy 7800X3D; đọc không được thì ghi `unavailable`, không ghi 0 |
| F1 | mỗi lỗi chèn sinh đúng một sự kiện, đếm trực tiếp khớp với report cuối |
| F3 | bảng chèn lỗi mục 5 đúng hết; hiệu năng GTX 960 giảm ≤ 5%; **cờ tắt cho kết quả giống hệt bản cũ**; máy 7800X3D ra `PCIe-GPU1 … (XÁC ĐỊNH)` |
| F4 | chèn lỗi RAM (ổn định / thoáng qua) và lỗi đọc SSD vào đúng ô; cờ tắt giống bản cũ |
| F6 | trên dữ liệu tổng hợp, ô xếp hạng đầu trùng ô đã biết ≥ 90% |

Mỗi bước kết thúc bằng: `make test` → test phần cứng → chạy lại các bài kiểm của combined (chèn lỗi, crash, Ctrl+C) →
build ISO → chạy trên máy thật → tag (`f0-done`, `f2-done`…). Image ổn định trong `dist/stable/` luôn sẵn để quay lại.

## 8. Tiến độ

### F0 — xong (2026-09-28, nhánh `f0-fault-ledger`)

| Hạng mục | Kết quả |
|---|---|
| Sổ lỗi | `fault_ledger.rs`: ô, mức chắc chắn, đếm theo pha, giữ chi tiết tối đa 64 sự kiện mỗi ô (bão lỗi vẫn đếm đủ), hai dòng hiển thị, CSV |
| Quy ô từ report | `fault_sources.rs`: CPU-RAM (KAT / coherency / SIMD / compute oracle → `CPU` xác định; mẫu RAM → `RAM` nghi ngờ, đếm theo vòng verify lỗi; EDAC UE → `RAM`; crash → `PLATFORM`); GPU (theo detector: sweep và vram-self-check → `VRAM` xác định; transfer / retention / checkpoint → `PCIe-GPU\|VRAM`; compute → `PCIe-GPU\|GPU\|VRAM`, đếm theo số batch trong burst; host corpus đổi → `RAM`; Xid 48/63/64/92/94/95 → `VRAM`, 79 → `PCIe-GPU\|PLATFORM`, AER → `PCIe-GPU`); SSD (mọi lỗi dữ liệu và I/O → `SSD\|SSD-path` cho tới F4); chuỗi (theo cây xác nhận của nó); lớp không có report → `PLATFORM`. Sự kiện phần cứng: AER hai đầu, link tụt độ rộng, ECC, cảnh báo kernel, EDAC CE, MCE, throttle, OOM |
| COMBINED | quy ô khi mỗi lớp kết thúc; màn hình có `FAIL BY PART` (lỗi của lớp đang chạy hiện là `pending`) và `HW EVENTS`; `fault_ledger` trong `combined-report.json`; một dòng `fail-summary.csv` mỗi phiên trên phân vùng kết quả |
| Test đơn lẻ | GPU-VRAM và CPU-SSD: hai dòng sổ lỗi dưới thông báo kết quả + dòng CSV |
| Kiểm | 21 test lib (fixture là report thật: GPU compute lỗi, Xid 48, retention lỗi, chuỗi lỗi GPU, SSD bit-flip) + test đơn lẻ; phiên COMBINED thật với lỗi chèn vào CPU-RAM: `FAIL BY PART CPU 1`, CSV `cpu_fail=1, confirmed=1`; phiên PASS ghi toàn 0; `make test` pass |

Giới hạn còn lại của F0:
- Quy ô khi lớp kết thúc (đọc report); F1 sẽ quy ngay khi lỗi xảy ra.
- CPU-RAM chạy đơn lẻ (UI bash) chưa ghi CSV.
- Trên máy phát triển, driver NVIDIA chưa build cho kernel 7.0.0-34 nên phiên thật chưa có lớp GPU. Các luật GPU và chuỗi
  được kiểm bằng report thật chụp trên GTX 960.

### F1 + F2 — xong (2026-09-28, commit `8710f3f`)

| Hạng mục | Kết quả |
|---|---|
| Bản ghi lỗi trực tiếp | `dit-contracts::fault` (`FaultRecord`, `FaultWriter`, `FaultTail`): mỗi lỗi một dòng JSONL ngay khi xảy ra (`--fault-events`) ở `dit-cli system-run`, `gpu-dit` (kể cả từng worker nhiều GPU), `dit-chain`; SSD lấy từ sự kiện `failure` sẵn có. COMBINED đọc tiếp mỗi tick, quy ô ngay và ghi thời điểm (`at_seconds`); report cuối chỉ bù phần chênh (`reconcile:report`) |
| Đo link | `dit-system::pcie`: tốc độ / độ rộng, AER theo loại ở **cả hai đầu** (thiết bị và port phía trên), replay (NVIDIA `nvidia-smi -q -d PCIE`, AMD `pcie_replay_count`); đọc không được là `unavailable`, không phải 0 |
| Theo dõi trong phiên | `hw_events.rs` (`HwMonitor`): đọc mọi link GPU và SSD 5 s một lần, delta thành sự kiện phần cứng có thời điểm trên ô `PCIe-GPU n` / `SSD-path n`; report có `hardware_links` |
| Kiểm | CPU-RAM chèn lỗi SIMD / RAM: đúng một bản ghi mỗi lỗi (lần chạy chẩn đoán lại không ghi); phiên COMBINED quy ô trực tiếp (`at_seconds` 66.7, luật `cpu-ram/cpu_check`); monitor đọc được link SSD và AER trên máy phát triển |

### F4 — xong (2026-09-28, commit `0bfc2d4`, sau cờ `--attribution-checks` / boot `dit_attribution=1`)

| Hạng mục | Kết quả |
|---|---|
| SSD (`--confirm-on-fail`) | block lỗi được kiểm lại (qua → `recheck_clean` → `CPU\|RAM`) và đọc lại từ thiết bị (sạch → `reread_clean` → `SSD-path`; sai y hệt → `reread_same` → `SSD`; sai khác → `reread_differs` → `SSD-path`) |
| CPU-RAM (`--confirm-on-fail`) | mismatch đầu tiên của mỗi worker: đuổi cache qua các arena khác rồi đọc lại từ CPU khác (đúng ở CPU khác, sai ở CPU verify → `verifier_core` → `CPU`; đúng cả hai → `transient` → `CPU\|RAM`; sai → ghi lại và đọc: đúng → `rewrite_fixed` → `RAM` nghi ngờ, vẫn sai → `cell_persistent` → `RAM` xác định). Tối đa 8 lần mỗi lần chạy |
| Kiểm | multi-simulate: bit-flip → `reread_same`, read-soak-bit-flip → `reread_clean`, read-compute → `recheck_clean`; CPU-RAM chèn lỗi ở một offset → `rewrite_fixed` (cả bản ghi trực tiếp lẫn report). Cờ tắt: không đổi gì |

### F6 — xong (2026-09-28)

`fault_signatures.rs`, chạy một lần cuối phiên COMBINED (và cuối test đơn lẻ GPU-VRAM / CPU-SSD, không có dữ liệu thời
gian). Chỉ xét các nhóm `CHƯA TÁCH`; mỗi chữ ký cộng trọng số (phiên bản `WEIGHTS_VERSION = 1`) cho ô ứng viên của nhóm:

| Chữ ký | Kiểm | Ô được cộng | Trọng số |
|---|---|---|---|
| Thời gian (2.4) | lỗi (đếm theo giây riêng biệt, bão lỗi tính một) nằm trong cửa sổ `[T-6, T+2]` quanh sự kiện link; so với tỉ lệ thời gian các cửa sổ phủ, binomial `p < 0,001` | link có sự kiện (`PCIe-GPU n`, `SSD-path n`) | 4 |
| Phần cứng | ô có đếm sự kiện phần cứng trong phiên | ô đó | 1 |
| Địa chỉ lặp (2.5) | ≥ 3 lỗi có địa chỉ, ≥ một nửa rơi vào địa chỉ lặp | ô nhớ (`RAM`, `VRAM n`, `SSD n`) | 3 |
| Địa chỉ rải | > 3 lỗi, không địa chỉ nào lặp | đường (`CPU`, `PCIe-GPU n`, `GPU n`, `SSD-path n`) | 1 |
| Bit cố định (2.5) | chỉ lỗi có địa chỉ; một bit có trong ≥ 80% syndrome, binomial `p < 0,01` | ô nhớ | 2 |
| Chỉ lỗi khi tải đủ (2.5) | lỗi chỉ ở OVERLAP, không lỗi ở pha chạy riêng; Poisson `p < 0,01` | `PLATFORM` nếu có trong nhóm (không thì chỉ ghi chú) | 3 |

Nhóm chuyển sang ô dẫn đầu, mức `NGHI NGỜ`, khi ô đó đạt ≥ 3 điểm **và** hơn ô thứ hai ≥ 2 điểm. Bằng chứng yếu (chỉ
đếm phần cứng, chỉ địa chỉ rải) hoặc mâu thuẫn thì nhóm giữ nguyên `CHƯA TÁCH`. Tầng 2 không bao giờ tạo `XÁC ĐỊNH`.
Mỗi lần chuyển được ghi trong `fault_ledger.rankings` (nhóm, ô, điểm từng ô, lý do). Địa chỉ và syndrome lấy từ chi tiết
lỗi (`at byte N`, `global offset N`, `offset N:`, `expected/actual`, `syndrome`) hoặc evidence của bản ghi CPU-RAM.

| Kiểm | Kết quả |
|---|---|
| Dữ liệu tổng hợp (mục 7.3) | 2000 phiên / 10 seed, 5 kịch bản (`PCIe-GPU` vs `VRAM` ở transfer, `VRAM` trong nhóm compute, `SSD` vs `SSD-path`), nhiễu AER nền tới 30 sự kiện, 20% lỗi nhớ ở địa chỉ ngẫu nhiên, 1/3 lỗi link không đọc được bộ đếm: **1663 đúng, 0 sai, 337 giữ CHƯA TÁCH** (độ chính xác 100% ≥ 90%; độ phủ 83%, phần còn lại chủ yếu là lỗi link không có bộ đếm) |
| Report thật GTX 960 (retention) | 5 lỗi chỉ ở hai offset (0 và 5701632) → `VRAM1 5 (5 susp)` thay vì `? PCIe-GPU1\|VRAM1 5` |
| Trường hợp giữ nguyên | lỗi compute không có địa chỉ, không sự kiện link → vẫn `CHƯA TÁCH`; Xid 79 chỉ ở OVERLAP sau 600 s baseline sạch → `PLATFORM` nghi ngờ |

Trong quá trình chỉnh: ngưỡng 0,01 cho chữ ký thời gian từng đặt nhầm một lỗi VRAM sang `PCIe-GPU` do trùng ngẫu nhiên
với nhiễu nền, nên hạ xuống 0,001; và chỉ riêng số đếm phần cứng từng đẩy lỗi SSD sang `SSD-path`, nên thêm ngưỡng điểm
tối thiểu và khoảng cách.

### F3 — xong: đường GEMM và lane (2026-09-29, sau cờ `--boundary-checks` / boot `dit_attribution=1`)

| Hạng mục | Kết quả |
|---|---|
| c2 — sau mỗi lần upload corpus | GPU băm input và đáp án vừa nhận (`dit_hash_words`), so với hash CPU tính trên bản đã gửi. Sai → băm lại (GPU băm hai kết quả khác nhau → `device`), rồi upload lại: đúng → `h2d`, vẫn sai → `h2d-persistent` |
| c3 — sau mỗi GEMM, trước D2H | gộp vào kernel GEMM (`dit_gemm_checked`, `dit_gemm_f32_checked`; không thêm launch nào): mỗi thread đọc lại từ vừa ghi, so với đáp án đã gắn tag và băm; block xong cuối cùng cộng các phần, seal tail `{bad, seal, hash}` sau output và tự đặt lại bộ đếm. INT8 tensor dùng hai kernel riêng (`dit_check_output`, `dit_check_seal`) |
| Khi một batch lỗi | CPU kiểm lại chính các byte đó và băm: GPU đã thấy sai → `device`; GPU thấy đúng, host khác → `d2h` (ô RAM host không giữ được từ đúng → `host-ram`); host giống hệt bản GPU đã kiểm, hoặc CPU kiểm lần hai đúng → `host` (`CPU\|RAM`) |
| Sau burst có lỗi | băm lại input và đáp án trên GPU (hai lần, phải trùng nhau): đổi so với lúc upload → `vram-input` / `vram-answers`, corpus được upload lại |
| Hàm băm | tổng mod 2^32 của `từ XOR khoá(vị trí)` (chỉ nhân 32 bit, rẻ trên mọi GPU); một từ đổi luôn làm tổng đổi; chung CPU/GPU (`gpu_engine::gemm::boundary_mix`) |
| Cờ tắt | chạy đúng `dit_gemm` / `dit_gemm_f32` cũ (PTX của chúng không đổi byte nào), report không có mục `boundary`, chi tiết lỗi như cũ |
| Lane (transfer, retention, vram-checkpoint) | vùng lane còn nằm trong VRAM (planner khoá nó tới khi thao tác kết thúc), nên **chỉ khi CPU thấy sai** GPU mới băm vùng đó: vẫn đúng → `d2h` (ô RAM host không giữ được byte đúng → `host-ram`; host giống hệt bản GPU, hoặc so lần hai đúng → `host`); sai (băm hai lần như nhau) → transfer: upload lại vùng đó, đúng → `h2d`, vẫn sai → `h2d-persistent`; retention / checkpoint → `vram-rest` (đúng lúc ghi, sai khi đọc lại). Vùng sạch không tốn gì thêm |
| `dit-suite` | lớp GPU và chain trong COMBINED, và GPU-VRAM đơn lẻ, nhận `--boundary-checks` khi `dit_attribution=1`; tag mới `host` → `CPU\|RAM`, `host-ram` → `RAM`, `vram-rest` → `VRAM` |

Kiểm trên GTX 960 (`build/f3/`):

| Kiểm | Kết quả |
|---|---|
| Chạy sạch, cờ bật | khoảng 48.000 batch mỗi 30 s, **0** lần GPU và CPU kết luận khác nhau; mọi hash upload trùng hash CPU |
| Hiệu năng, cấu hình menu (k = 1024, 512 job; 3 cặp xen kẽ, chờ GPU < 50 °C) | 291,6 → 286,9 GOPS: **−1,6 %** (yêu cầu ≤ 5 %) |
| Hiệu năng, lượt ngắn (k = 256, 64 job) | khoảng −6 %: phần kiểm cố định theo số ô, GEMM tỉ lệ với k |
| `--inject-hardware-fault matrix` | `[boundary: device=1]` → `GPU1` nghi ngờ; cờ tắt: cùng lỗi, không tag → `PCIe-GPU1\|GPU1\|VRAM1` |
| lỗi sau khi GPU kiểm, trước D2H (`after-check`) | `d2h=1` → `PCIe-GPU1` xác định |
| input hỏng sau upload (`h2d`) / hỏng cả sau upload lại (`h2d-persistent`) | `h2d=1` → `PCIe-GPU1` xác định / `h2d-persistent=1` → `PCIe-GPU1\|VRAM1` |
| input đổi trong VRAM sau khi kiểm (`vram-input`) | 7 batch lỗi, `vram-input=7` → `VRAM1` xác định |
| đáp án đổi trong VRAM (`vram-answers`) | 32 batch GPU tự kiểm báo sai nhưng CPU đúng → kiểm lúc nghỉ → `vram-answers=1` → `VRAM1`, phiên FAIL |
| buffer host đổi sau khi copy về (`host-buffer`) | `d2h=1` — giới hạn đã biết, xem dưới |
| Lane, chạy sạch cờ bật (GEMM + lane song song) | khoảng 7.600 thao tác transfer + retention mỗi 30 s, không lỗi giả |
| Lane, byte hỏng ở host sau D2H (`--inject-hardware-fault transfer`) | `d2h=1` → `PCIe-GPU1` (cũng chứng minh hash GPU khớp mẫu CPU); cờ tắt: không tag → `PCIe-GPU1\|VRAM1` |
| Lane, vùng hỏng trên GPU sau H2D (`lane-h2d`) / hỏng cả sau upload lại (`lane-h2d-persistent`) | `h2d=1` → `PCIe-GPU1` / `h2d-persistent=1` → `PCIe-GPU1\|VRAM1` |
| Lane, vùng đổi trong VRAM giữa lúc ghi và lúc đọc lại (`lane-rest`) | `vram-rest=1` → `VRAM1` xác định |
| Hiệu năng, GEMM + lane song song, cấu hình menu (3 cặp xen kẽ) | compute 285,4 → 281,5 GOPS (**−1,4 %**); lane kiểm 1.823 → 1.841 MiB/s (**không giảm**) |
| Sổ lỗi | test `injected_gpu_boundary_faults_land_on_their_cells` và `injected_lane_boundary_faults_land_on_their_cells` đưa đúng các dòng lỗi thật trên qua bộ quy ô |

Giới hạn còn lại:
- Buffer host bị đổi **thoáng qua** sau khi copy về không tách được với lỗi trên đường về: ghi là `d2h`. Ô RAM host hỏng
  **cố định** thì được bắt (`host-ram`).
- Lần đọc lại ngay sau khi ghi thường lấy từ L2 cache, nên một ô DRAM hỏng đúng trong vài micro-giây giữa lúc kiểm và
  lúc copy về cũng bị ghi là `d2h`.
- Bản đầu của lane băm mọi vùng trước D2H: kernel băm phải xếp hàng sau các block GEMM nên độ phủ lane giảm 53 %. Bản cuối
  chỉ băm khi có lỗi (vùng vẫn còn trong VRAM), cùng kết luận mà không tốn gì trên vùng sạch.
- Lỗi lane trong khoảng thời gian rất ngắn giữa lúc CPU so và lúc GPU băm lại (một ô VRAM hỏng đúng lúc đó) có thể bị ghi
  sai loại; vùng bị khoá nên không có gì khác ghi đè nó.
- Bộ điều khiển tải (có sẵn) xét tải theo nhịp 50 ms nhưng burst chỉ 10–20 ms và reset cửa sổ đo, nên hầu như luôn ở
  `WARMUP` với 2 batch trên GPU. Không do F3; khi bật cờ F3 cho phép 3 batch từ đầu để che độ trễ.

### Mục boot riêng "DIT-01 Fault Attribution" (2026-09-30)

Bật F3/F4 không cần sửa dòng lệnh kernel: menu GRUB (image và ISO) có mục riêng
`DIT-01 Fault Attribution (control center + component checks)` (`--id fault-attribution`), là control center thường
cộng `dit_attribution=1`. Mục mặc định vẫn là control center thường, không bật gì.

**Cập nhật 2026-10-03:** mục này nay là mục test đầu tiên và là mục mặc định (`set default=fault-attribution`);
mục control center thường (`--id control-center`) đã bỏ. Mọi lần chạy từ control center đều bật F3/F4 và
kết quả mang dấu `+ATTRIBUTION`. Hai mục CPU-RAM Safe/Full unattended cũng đã bỏ (CPU-RAM chạy từ control
center); chỉ giữ Operator shell, Reboot, Power off. Profile `safe/full` vẫn dùng được khi gõ tay `cpu_dit_profile=`
trên dòng lệnh kernel (test QEMU).

| Trong chế độ này | Chạy thêm |
|---|---|
| Control center | tiêu đề `FAULT ATTRIBUTION`, hai dòng báo những kiểm nào đang bật |
| COMBINED | lớp GPU và chain `--boundary-checks`; CPU-RAM và SSD `--confirm-on-fail` |
| GPU-VRAM đơn lẻ | `--boundary-checks` |
| CPU-SSD đơn lẻ | `--confirm-on-fail` |
| CPU-RAM đơn lẻ (Safe / Full / Compute từ giao diện CPU) | `boot-runner.sh` thêm `--confirm-on-fail` cho `dit-cli system-run`; `session.txt` ghi `Fault attribution: on` |
| Kết quả | cột `test` của `fail-summary.csv` có đuôi ` +ATTRIBUTION`; `combined-report.json` có `attribution_checks: true` |

`tools/test_boot_policy.sh` kiểm: mục có mặt, `dit_attribution=1` chỉ nằm trong mục đó, mục thường không có, và
`boot-runner.sh` truyền cờ cho CPU-RAM và control center.
