# Bắt lỗi khi app treo hoặc phần cứng lỗi thật — phân tích và roadmap

Trạng thái: đề xuất, 2026-10-12. Chưa code: thảo luận trước, code sau. Nguồn: phân tích kết quả thực tế trong
`Result/1–4` (bản sao phân vùng `DIT-RESULTS` từ nhiều máy, 2026-10-05 → 10-10), kiểm tra watchdog và các buổi
thảo luận với người vận hành.

Bối cảnh: màn kết quả chỉ ghi `PLATFORM 1` mà không cho biết lỗi gì. Phân tích cho thấy hai vấn đề ngược nhau:

- **8 ô PLATFORM đang hiện đều là lỗi của chính DIT** (lớp SSD treo trên NTFS), không phải lỗi phần cứng.
- **Khoảng 13 phiên chết giữa chừng thì không được ghi lại gì**: không report, không dòng CSV, không thông báo.
  Phần lớn là app treo (OS vẫn sống, Num Lock vẫn đáp ứng) và người vận hành phải bấm reset vì không còn lối thoát.

## 0. Mục tiêu, phạm vi và định nghĩa

Mục tiêu:

1. **Không mất phiên nào.** Phiên bị dừng giữa chừng vẫn có result và một dòng CSV: ngay trong phiên (hộp đen), hoặc
   ở lần boot sau nếu cả máy chết.
2. **Tách lỗi hạ tầng test khỏi lỗi phần cứng.** DIT treo, filesystem chặn, kế hoạch bộ nhớ quá chặt chỉ làm phiên
   INVALID. Chỉ đếm PLATFORM khi có bằng chứng phần cứng (mất nguồn, ngắt nhiệt, panic, MCE, GPU rớt bus...).
3. **Người vận hành luôn có lối thoát có ghi nhận** khi app treo, thay vì chỉ có nút nguồn.
4. **Hiện rõ nguyên nhân** trên màn kết quả và trong CSV, thay cho một ô PLATFORM khó hiểu.

Ngoài phạm vi (giữ nguyên quy ước của [COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md](COMPONENT_FAULT_ATTRIBUTION_ROADMAP.md)):
đếm FAIL theo ô linh kiện, không khoanh vùng bên trong một link (CPU, khe, riser, card), không gợi ý đổi linh kiện.
Tool chỉ nói **điều gì đã xảy ra**.

| Thuật ngữ | Nghĩa |
|---|---|
| PLATFORM | lỗi phần cứng thật nhưng không quy được cho một linh kiện cụ thể; tính FAIL |
| Lỗi hạ tầng test | DIT, filesystem, kế hoạch bộ nhớ hoặc người vận hành làm phiên không kết luận được; phiên INVALID, không tính FAIL |
| INCOMPLETE | phiên không đóng, được phát hiện ở lần boot sau; nguyên nhân lấy từ bằng chứng còn lại |
| App treo | userspace đứng (UI, các lớp test) nhưng kernel vẫn sống: bàn phím, Num Lock, chuyển VT vẫn hoạt động |

## 1. Dữ liệu thực tế (`Result/1–4`; bản 5 trùng bản 4)

**64 lần chạy có report:** 24 PASS, 2 FAIL (hai lỗi RAM nghi ngờ, mục 3.5), 38 INVALID. Trong 38 INVALID có 29 lần
người vận hành tự dừng và 8 lần lớp SSD bị watchdog kill.

**Bản ghi watchdog (theo lớp) trên USB:**

| Trạng thái | Số bản ghi | Ghi chú |
|---|---|---|
| `PASSED` | 395 | |
| `WORKER_SIGNALED` | 87 | người vận hành dừng; watchdog dừng sạch cả nhóm tiến trình |
| `STALLED_KILLED` | 9 | đều là lớp SSD; 8 lần là treo NTFS (mục 3.1) |
| `WORKER_FAILED` | 2 | |
| `OOM_KILLED` | 0 | |
| `RUNNING` (không đóng) | 24 | phiên chết giữa chừng |
| file rỗng 0 byte | 7 | đều thuộc các phiên chết giữa chừng, tức hỏng đúng lúc cần |

**77 phiên suite có marker, 16 phiên không có report.** Khoảng 13 phiên là phiên chết giữa chừng (giờ UTC):

| Thời điểm | USB | Máy (suy từ các lần chạy liền kề) | Chết ở đâu |
|---|---|---|---|
| 10/5 09:09, 09:56, 11:42 | 4 | Ryzen 9950X + RTX 5080 | khoảng 20 s sau khi vào OVERLAP max (cả 3 lần) |
| 10/6 09:37 | 3 | Ryzen 9950X + RTX 4090 | khoảng 21–25 s sau khi vào OVERLAP |
| 10/8 16:08 | 2 | Ryzen 9950X3D + RTX 5060 Ti | khoảng 17 s sau khi vào overlap lần 2 của SOAK |
| 10/7 18:09, 18:25, 18:38 | 3 | Ryzen 9950X3D | 3 phiên liền nhau; 18:09 dừng ở pha SSD riêng khi chưa có hoạt động nào (giống treo NTFS) |
| 10/9 17:56 | 4 | Ryzen 7500F + ASUS B650M | CPU-RAM chạy lẻ, sau 15 phút |
| 10/9 12:15 | 1 | Ryzen 7800X3D + RX 9070 | pha GPU riêng |
| 10/5 16:32 | 3 | chưa rõ (có thể GTX 1050 Ti) | GPU-VRAM chạy lẻ, ngay khi bắt đầu |
| 10/5 19:46 | 2 | i7-12700KF | pha CPU-RAM riêng (p01), sau 56 s |

Người vận hành xác nhận: phần lớn các phiên này là **app treo rồi tự bấm reset**; OS vẫn hoạt động (Num Lock đáp ứng)
nhưng không có cách nào khác để thoát hoặc ghi nhận tiến trình.

Ngoài ra, trong 29 lần người vận hành dừng có **1 lần SSD treo ngầm** (USB 2, i5-14400F + Lexar NM610 PRO, 10/10 20:26):
lớp SSD đã im 66 s khi người vận hành dừng, trước khi watchdog kịp kill.

**RAM trống thấp nhất** (`MemAvailable`, lấy mẫu khoảng 5 s) trong các phiên kết thúc bình thường: 1,37–5,78 GiB, sát
mức dự phòng 1,5 GiB ([combined.rs:13](../crates/dit-orchestrator/src/combined.rs#L13)). Arena CPU-RAM tới khoảng
141 GiB; bộ đệm host của GPU tới 2048 MiB.

## 2. Watchdog hiện nay làm gì

Chi tiết cơ chế ở [RUNTIME_WATCHDOG.md](RUNTIME_WATCHDOG.md). Tóm tắt:

- Mỗi lớp có một tiến trình `dit-watchdog` riêng ([main.rs:130](../crates/dit-watchdog/src/main.rs#L130)), poll mỗi giây.
- **Dấu hiệu sống chỉ là mtime** của file heartbeat (CPU-RAM, GPU-VRAM, chuỗi) hoặc `events.jsonl` (SSD).
- Im quá 90 s (SSD: 180 s), chờ thêm 30 s rồi kill cả nhóm tiến trình, ghi `STALLED_KILLED`.
- Người vận hành dừng: TERM, sau 2 s thì KILL.
- OOM nhận qua `memory.events` của cgroup (dùng chung cho cả service).
- Marker trên USB được ghi lại toàn bộ (file tạm + `fsync` + rename + `fsync` thư mục,
  [main.rs:418](../crates/dit-watchdog/src/main.rs#L418)) mỗi khi có hoạt động mới.
- Heartbeat chưa từng được tạo thì không bao giờ bị coi là treo.
- Menu sau boot chỉ in một dòng `WATCHDOG RECOVERY` nếu còn marker chưa đóng
  ([dit-orchestrator main.rs:238](../crates/dit-orchestrator/src/main.rs#L238)); không có dòng CSV, không phân loại.

### Hiệu quả trên dữ liệu thật

| Tình huống | Số lần | Watchdog | Đánh giá |
|---|---|---|---|
| Kết thúc bình thường | 395 bản ghi | `PASSED` | tốt |
| Người vận hành dừng | 87 | dừng sạch | tốt |
| Thoát với mã lỗi | 2 | `WORKER_FAILED` | tốt |
| Lớp "treo" | 9 (SSD) | kill sau 210 s | **8 lần kill nhầm** (SSD vẫn đang ghi số 0 trên NTFS), rồi bị tính PLATFORM |
| Phiên chết giữa chừng | khoảng 13 | không làm được gì | dấu vết còn trên USB nhưng **không ai đọc lại** |
| Marker bị hỏng | 7 file rỗng | | hỏng đúng ở các phiên chết |

### Lỗ hổng

| Mã | Lỗ hổng | Hậu quả |
|---|---|---|
| W1 | chỉ dựa vào mtime, không phân biệt "I/O chậm" với "treo thật" | 8 lần kill nhầm trên NTFS |
| W2 | không có hạn chót cho heartbeat đầu tiên | treo lúc khởi tạo (ví dụ driver GPU) thì chờ mãi |
| W3 | marker dở dang không được đọc lại ở lần boot sau | phiên chết biến mất khỏi CSV và thống kê |
| W4 | ghi đè marker mỗi giây trên USB FAT không bền khi mất điện | 7 file rỗng |
| W5 | không có hardware watchdog | máy treo cứng thì đứng luôn, chờ người bấm reset |
| W6 | mọi `STALLED_KILLED` đều tính PLATFORM | lỗi của DIT thành lỗi phần cứng |

Hai điểm nhỏ, chưa gặp trên dữ liệu thật: bộ đếm OOM dùng chung cho cả service nên có thể gán nhầm lớp; tiến trình
điều phối COMBINED không có ai giám sát.

## 3. Phát hiện

### 3.1 PLATFORM giả: lớp SSD treo trên NTFS

- **8/8 cùng một nguyên nhân:** volume NTFS + mức ghi HEAVY (file test 64 GiB), SSD rẻ không DRAM (GIGABYTE
  G3NVME512G 6 lần, Lexar NM610 PRO 2 lần). 7/8 lần `events.jsonl` trống: engine kẹt ngay từ lần ghi đầu.
- **Cơ chế:** engine tạo file bằng `set_len(capacity)` + `sync_all`
  ([nvme-backend-file lib.rs:117](../crates/nvme-backend-file/src/lib.rs#L117)), rồi ghi `O_DIRECT|O_DSYNC` vào vị trí
  ngẫu nhiên. Driver `ntfs3` giữ "valid data length" cho mỗi file: ghi vào một vị trí xa thì nó phải **ghi số 0 lấp toàn
  bộ khoảng trước đó** rồi mới trả về.
- **Thí nghiệm trên host** (file 4 GiB, một lần ghi 4 KiB `O_DIRECT` gần cuối file): ext4 0,011 s; **ntfs3 3,87 s**.
  Với 64 GiB trên SSD đã hết SLC cache, việc lấp có thể kéo dài nhiều phút, vượt ngưỡng 180 s.
- **Vì sao thành PLATFORM:** lớp kết thúc không có report thì luật `session/no_report`
  ([fault_sources.rs:1002](../crates/dit-orchestrator/src/fault_sources.rs#L1002)) ghi vào PLATFORM.
- **Hệ quả phụ:** trên NTFS, HEAVY ghi thêm khoảng 64 GiB số 0, TBW gần gấp đôi tính toán.

### 3.2 App treo: lỗi chắc chắn — `nvidia-smi` không có giới hạn thời gian

Màn COMBINED, GPU và các màn khác gọi `nvidia-smi` mỗi 1–2 s ngay trong luồng vẽ
([gpu.rs:418](../crates/dit-system/src/gpu.rs#L418)). Lệnh `timeout` chỉ được dùng khi **không chạy được** chương
trình, không dùng khi chương trình **bị treo**. Driver NVIDIA kẹt thì màn hình đứng vĩnh viễn và Ctrl+C không còn tác
dụng.

### 3.3 App treo: giả thuyết mạnh nhất — cạn RAM làm cả userspace đứng (memory livelock)

Bằng chứng:

- Ở 5 phiên chết đầu overlap, marker của **mọi lớp cùng ngừng cập nhật một lúc**, khoảng 20 s sau khi vào overlap.
  Các lớp là tiến trình riêng, ghi file riêng; nếu chỉ UI treo thì các lớp vẫn phải chạy tiếp.
- Image không có swap (`swapoff -a`, [boot-runner.sh:69](../image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh#L69))
  và hệ điều hành nằm trong RAM (toram). Khi RAM cạn, kernel liên tục bỏ rồi nạp lại trang mã của các chương trình:
  mọi tiến trình gần như đứng yên, OOM killer có thể không kích hoạt, nhưng kernel vẫn xử lý bàn phím (Num Lock).
- Đầu overlap là lúc mọi lớp cùng xin RAM: CPU-RAM nhận phần còn lại (tới khoảng 137–141 GiB), GPU cấp bộ đệm, SSD,
  chuỗi. Trong phiên bình thường RAM trống thấp nhất đã chỉ còn 1,37–5,78 GiB, mà bộ nhớ chỉ được đo mỗi 5 s
  ([combined_run.rs:1148](../crates/dit-orchestrator/src/combined_run.rs#L1148)), nên một đợt tụt ngắn có thể không
  được ghi lại.
- 7 marker rỗng cũng khớp: watchdog đang ghi rất chậm vì máy nghẹt RAM thì người vận hành bấm tắt.

**Chưa chứng minh được**, vì dữ liệu của chính các phiên treo đã mất. Cần hộp đen (FC3) để xác nhận.

### 3.4 Tính năng kernel có sẵn nhưng chưa dùng

| Tính năng | Trạng thái | Dùng để |
|---|---|---|
| AMD "Previous system reset reason" | đã xác nhận có trên kernel 7.0 (host): `x86/amd: Previous system reset reason [...]: ...` | phân biệt mất nguồn, ngắt nhiệt, giữ nút nguồn, watchdog phần cứng; phần lớn máy trong dữ liệu là AMD. Intel không có thông tin tương đương |
| pstore | `CONFIG_PSTORE=y`, `EFI_VARS_PSTORE=m` | log panic/MCE nếu kernel kịp lưu trước khi chết |
| Hardware watchdog | `SP5100_TCO=m` (AMD), `ITCO_WDT=m` (Intel) | board tự reset khi máy treo cứng |
| Soft/hard lockup detector | bật trong kernel | phát hiện CPU kẹt trong kernel |
| Magic SysRq | Ubuntu mặc định `kernel.sysrq = 176` | đã cho phép S (sync), U (remount ro), B (reboot); **chưa** cho F (OOM kill, cần thêm bit 64) |

### 3.5 Lỗi RAM nghi ngờ (`RAM 1 (1 susp)`) — tạm gác

| | Máy 1 | Máy 2 |
|---|---|---|
| Board | MSI MAG X870E TOMAHAWK | ASUS TUF B650M-E |
| CPU | Ryzen 5 7500F | Ryzen 5 7500F |
| Pha | CPU-RAM riêng, vòng 0 | OVERLAP, vòng 22 |
| Byte | 0x7f → 0x5f | 0xf7 → 0xd7 |
| Nhân ghi → nhân đọc | CPU 7 → CPU 8 | CPU 7 → CPU 8 |

- Hai máy, cùng chữ ký: bit 5 bị xóa, cùng nhân CPU 7 ghi và CPU 8 đọc. Khó là trùng hợp.
- Điểm yếu của DIT: khi xác nhận lỗi, `confirm_one` ghi lại byte từ **một nhân khác**, không phải nhân đã ghi ban đầu
  ([engine.rs:1161](../crates/dit-system/src/engine.rs#L1161)). Vì vậy `rewrite_fixed` không phân biệt được "nhân
  CPU 7 ghi sai" với "ô RAM lật bit", và mặc định ghi RAM nghi ngờ.
- Người vận hành chưa hiểu dòng `RAM 1 (1 susp)`. Cả phần hiển thị lẫn phần xác nhận được tạm gác (FC8).

## 4. Roadmap

Thứ tự đề xuất: **FC0 → FC1 → FC2 → FC3**, sau đó **FC4 + FC5 + FC6**, cuối cùng FC7 và FC8. FC0–FC3 xử lý đúng chỗ
đang mất dữ liệu thật và chỗ người vận hành đang bị kẹt.

### FC0 — `nvidia-smi` có giới hạn thời gian, đo đạc ở luồng nền

- Mọi lần gọi `nvidia-smi` đều có hạn (ví dụ 3 s, sau đó KILL), không chỉ khi spawn lỗi.
- Đo đạc GPU chạy ở luồng nền và cập nhật một snapshot chung; luồng vẽ chỉ đọc snapshot. Snapshot cũ quá vài chu kỳ
  thì hiện `GPU telemetry stale Ns` thay vì đứng màn hình.
- Ctrl+C và phím dừng luôn được xử lý, kể cả khi driver kẹt.
- Kiểm: đặt một `nvidia-smi` giả ngủ mãi vào `PATH`; màn hình vẫn chạy, Ctrl+C vẫn dừng được, dòng stale hiện ra.
- File: `crates/dit-system/src/gpu.rs` và nơi gọi (dashboard, `combined_view`, `gpu-cli`).

### FC1 — Lối thoát khi app treo: console cứu hộ ở VT2 và SysRq

Console cứu hộ:

- Một tiến trình nhỏ khởi động sẵn từ lúc boot trên tty2, khóa trong RAM (`mlockall`), không phụ thuộc app chính.
  Chuyển VT do kernel xử lý nên Alt+F2 vẫn hoạt động khi app treo (UI ở chế độ text, không giữ VT).
- Hiện: lớp nào còn sống, lần hoạt động cuối của từng lớp, RAM trống, áp lực bộ nhớ (PSI).
- Phím:
  - **[S]** dừng test có ghi nhận: phiên được lưu INVALID với lý do `operator_rescue`; nếu tiến trình điều phối đã chết,
    console tự ghi report tối thiểu và dòng CSV.
  - **[K]** kill tiến trình test chiếm RAM nhiều nhất (chỉ trong nhóm tiến trình test) để giải phóng RAM.
  - **[L]** lưu gói chẩn đoán lên USB: log kernel, PSI, danh sách tiến trình, trạng thái watchdog.
  - **[R]** sync, umount phân vùng kết quả, khởi động lại sạch.
- Alt+F1 để quay lại app.
- Đây là chỗ duy nhất có hành động can thiệp ngoài app, và chỉ khi người vận hành bấm phím. Hộp đen (FC3) không can
  thiệp. [S] dùng cơ chế claim của FC3 nên không sinh hai result cho một phiên.

SysRq (phương án cuối):

- Đặt `kernel.sysrq = 240` (176 + 64) để thêm Alt+PrtSc+F (kernel tự kill tiến trình lớn nhất).
- Dòng gợi ý ở chân mọi màn đang chạy, ví dụ: `Treo? Alt+F2: cứu hộ | Alt+PrtSc+F: giải phóng RAM`.

Kiểm (QEMU): SIGSTOP tiến trình điều phối, Alt+F2, [S] → có dòng CSV INVALID đúng lý do; chạy một tiến trình chiếm
RAM, [K] → RAM được giải phóng, UI chạy lại.

### FC2 — Chốt chặn bộ nhớ

- Đọc `/proc/pressure/memory` và `MemAvailable` mỗi giây. Áp lực vượt ngưỡng hoặc RAM trống dưới mức dự phòng: dừng
  lớp CPU-RAM trước (lớp giữ arena lớn nhất), ghi phiên INVALID `memory_guard` kèm số liệu.
- Các lớp test chạy trong cgroup con có `memory.max`: vượt thì kernel kill tiến trình test thay vì cả máy đứng.
  UI, watchdog và console cứu hộ nằm ngoài cgroup đó và được giữ RAM tối thiểu (`memory.min`). Mỗi lớp một cgroup thì
  bộ đếm OOM cũng đúng lớp (sửa điểm nhỏ ở mục 2).
- Đầu mỗi overlap: đo RAM mỗi giây trong 2 phút đầu thay vì mỗi 5 s; khởi động các lớp cách nhau vài giây để thấy
  được lớp nào làm RAM tụt.
- Ngưỡng PSI cần đặt từ dữ liệu hộp đen thực tế (FC3), ban đầu để thận trọng.
- Kiểm (QEMU RAM nhỏ): tiến trình chiếm RAM trong cgroup test → chốt chặn kích hoạt, UI vẫn đáp ứng, phiên được ghi
  `memory_guard`.

### FC3 — Hộp đen: trọng tài độc lập, tự ra result ngay trong phiên

Mục tiêu: app bị kill, treo, SSD không trả lời, lớp bị OOM kill... đều có result **ngay trong phiên đó**, không phải
chờ lần boot sau. Chỉ khi cả máy chết (mất nguồn, kernel treo cứng, panic) mới cần lần boot sau (FC3b).

Quyết định (2026-10-12):

- Hộp đen **chỉ quan sát và ghi, không can thiệp app**: không kill, không dừng lớp, không giải phóng RAM. Hành động
  can thiệp chỉ đến từ app (chốt chặn bộ nhớ FC2) hoặc từ người vận hành (console cứu hộ FC1).
- Thời gian chờ: app chết 10 s; app treo 60 s rồi thêm 30 s; lớp treo 180 s; app có 60 s để tự ghi nhận một sự cố.
- Khi hộp đen ghi result, nó **tự chuyển sang màn kết quả**.

#### FC3a — Hộp đen trong phiên

Tiến trình:

- Service riêng `dit-blackbox.service`, khởi động lúc boot trước app, cgroup riêng ngoài `cpu-dit.service`. App bị
  kill, OOM hay treo không kéo theo hộp đen; nếu chính hộp đen chết, systemd khởi động lại nó.
- Chạy được khi máy nghẹt RAM: `mlockall`, cấp phát sẵn, `MemoryMin`, `OOMScoreAdjust=-1000`, không gọi chương trình
  ngoài (chỉ đọc `/proc`, `/sys`, `/dev/kmsg`), file trên USB mở sẵn. Vì hộp đen không giải cứu, nó phải tự sống qua
  tình trạng nghẹt RAM cho tới hết thời gian chờ.

App báo trạng thái (chỉ ghi vào tmpfs, không chờ phản hồi, không tự ghi lên USB):

- Lúc bắt đầu phiên: run id, pid, loại test, danh sách lớp và cgroup từng lớp, ví dụ
  `/run/dit-suite/<run-id>/session.json`.
- Mỗi giây: heartbeat của luồng chính, pha, tiến độ và trạng thái từng lớp (kể cả lớp đã kết thúc và vì sao), số FAIL
  theo ô tính đến lúc đó.
- Lúc kết thúc: trạng thái đóng phiên.

Nhật ký trên USB:

- Hộp đen là **bên duy nhất ghi nhật ký** lên `DIT-RESULTS`: mỗi phiên một file **chỉ ghi nối thêm**
  `blackbox/<run-id>.jsonl`, `fsync` sau mỗi dòng.
- Một dòng khoảng mỗi 10 s và mỗi khi đổi pha, đổi trạng thái lớp hoặc có sự kiện: thời điểm, pha, tiến độ từng lớp,
  số FAIL theo ô, RAM trống, PSI, nhiệt và công suất CPU/GPU, bộ đếm throttle, sự kiện kernel. Khoảng 100 KB cho
  1 giờ.
- Không bao giờ ghi đè, nên bền khi mất điện; bên đọc bỏ qua dòng cuối bị cắt dở. Thay cho việc ghi lại marker mỗi
  giây (W4); marker chỉ còn ghi lúc bắt đầu và kết thúc.

Tín hiệu hộp đen tự đọc, không qua app:

| Tín hiệu | Bắt được |
|---|---|
| pidfd của app | app thoát hoặc bị kill, biết ngay |
| heartbeat luồng chính của app | app treo |
| tiến độ lớp, `/proc/<pid>/io`, thời gian CPU, trạng thái D | treo thật hay I/O chậm |
| `memory.events`, `cgroup.events` của từng lớp | OOM kill, lớp chết |
| PSI trigger trên `/proc/pressure/memory` | nghẹt RAM (kernel đánh thức, không cần poll) |
| `/dev/kmsg` (chỉ dòng của kernel) | NVMe timeout/reset, lỗi I/O, Xid, MCE, segfault, `blocked for more than 120 seconds`, soft lockup |
| `/sys/class/nvme/*/state` | controller SSD `live`, `resetting`, `dead` |

Khi nào hộp đen tự ghi result:

- **Chỉ khi app không còn làm được việc của nó**: app chết, app treo, hoặc app còn sống nhưng không ghi nhận một sự
  cố trong 60 s (ví dụ lớp SSD kẹt ở trạng thái D, watchdog không kết thúc được).
- Khi app còn sống và tự ghi nhận sự cố (lớp kết thúc, có trong trạng thái app), hộp đen chỉ ghi bằng chứng và phân
  loại vào nhật ký; result vẫn do app viết lúc phiên kết thúc và dùng phân loại đó.

| Tình huống | Phát hiện | Kết luận (ghi vào nhật ký) | Hộp đen tự ghi result khi |
|---|---|---|---|
| App bị kill hoặc crash | pidfd | INVALID `app_killed`; bị OOM kill: INVALID `oom` | sau 10 s vẫn chưa có result |
| App treo, còn sống | heartbeat app đứng 60 s | PSI cho thấy nghẹt RAM: INVALID `memory_livelock`; không: INVALID `app_hang` | app vẫn đứng thêm 30 s (tổng 90 s) |
| SSD không trả lời | lớp không tiến độ 180 s, I/O không tăng, tiến trình ở trạng thái D | log kernel có timeout, reset, lỗi I/O hoặc controller `dead`: FAIL theo luật `ssd/io` (`? SSD\|SSD-path`); không có log: INVALID `layer_stall`; I/O vẫn chạy chậm: chỉ cảnh báo `slow_io` | app không ghi nhận lớp kết thúc trong 60 s sau đó |
| Lớp bị OOM kill | `memory.events` của lớp | INVALID `oom`, kèm lớp và số liệu RAM | app không ghi nhận trong 60 s |
| Lớp crash (segfault, bus error) | watchdog, log kernel | PLATFORM `worker_crash` | app không ghi nhận trong 60 s |
| GPU rớt bus | Xid 79 | `? PCIe-GPU\|PLATFORM` | app không ghi nhận trong 60 s |
| Cả máy chết | không phát hiện được | | FC3b, ở lần boot sau |

Một phiên chỉ có một result:

- App luôn được ghi trước; thời gian chờ của hộp đen luôn dài hơn của app và watchdog.
- Bên nào ghi result trước thì tạo file claim độc quyền (`O_EXCL`) trong thư mục phiên trên USB; bên đến sau không ghi
  đè và không thêm dòng CSV thứ hai. App tỉnh lại thấy claim thì chỉ báo "result đã được hộp đen ghi". Console cứu hộ
  (FC1) dùng cùng cơ chế này.

Result của hộp đen:

- Cùng định dạng với app: report JSON (`written_by: blackbox`), dòng CSV cùng header, màn kết quả vẽ bằng cùng
  renderer của `dit-ui-core`.
- FAIL đã thấy trước sự cố vẫn được tính (lấy từ số FAIL theo ô trong trạng thái app cuối cùng).
- Thêm dòng nguyên nhân (CAUSE) và tình trạng cuối (LAST STATE).
- Màn kết quả giống app: verdict cỡ lớn ở đáy khung TESTING, căn giữa (FAIL đỏ, INVALID vàng, PASS xanh); dưới đó
  mỗi ô linh kiện bị FAIL một dòng kèm thiết bị, ví dụ `SSD1 2  -  Samsung SSD 990 PRO 1TB`. Phần này đã có trong
  `dit-ui-core` (2026-10-12), dùng chung cho mọi màn test và màn kết quả.
- Vì không can thiệp, các lớp còn chạy lúc hộp đen ghi result vẫn chạy tiếp; result ghi tên các lớp đó. Report của
  chúng nếu xong sau vẫn nằm trong thư mục phiên nhưng không đổi result đã ghi. Muốn dừng thì người vận hành dùng
  console cứu hộ (FC1) hoặc khởi động lại.

Hiển thị:

- Khi ghi result, hộp đen tự chuyển sang VT riêng (ví dụ tty3) và vẽ màn kết quả; Alt+F1 quay lại app. Chuyển VT do
  kernel xử lý nên vẫn làm được khi app treo.
- Nếu màn hình không vẽ được (driver GPU kẹt giữ console), result vẫn nằm trên USB.

```
RESULT        FAIL  (written by the black box: the app stopped answering)
CAUSE         SSD1 not responding 180 s; kernel: nvme0 I/O timeout, controller reset
LAST STATE    COMBINED OVERLAP max, 14:32 in; RAM free 3.1 GiB; CPU 82 C; GPU1 71 C
STILL RUNNING CPU-RAM, GPU1
FAIL BY PART  CPU 0 | RAM 0 | PCIe-GPU1 0 | GPU1 0 | VRAM1 0 | ? SSD1|SSD-path1 1 | PLATFORM 0
```

Kiểm (QEMU):

- `kill -9` app → result `app_killed` trong khoảng 10 s, màn kết quả hiện ra.
- `SIGSTOP` app → result `app_hang` sau 90 s.
- Tiến trình chiếm RAM tới nghẹt → result `memory_livelock` sau 90 s; hộp đen vẫn đáp ứng.
- `dmsetup suspend` thiết bị test (I/O treo thật, tiến trình ở trạng thái D) → `layer_stall`; `dm-flakey` (lỗi I/O
  thật trong log kernel) → FAIL `ssd/io`.
- App ghi result kịp trước thời gian chờ → hộp đen không ghi; app tỉnh lại sau khi hộp đen đã ghi → không có dòng CSV
  thứ hai.
- Unit test cho dòng cuối bị cắt dở.

#### FC3b — Hậu kiểm ở lần boot sau (chỉ khi cả máy chết)

Trong boot runner, sau khi mount phân vùng kết quả, trước menu:

- Tìm các phiên có nhật ký chưa đóng và chưa có result (hộp đen chết cùng máy). Với mỗi phiên: viết report INCOMPLETE
  (pha cuối, tiến độ, số liệu cuối cùng) và thêm một dòng CSV. Phiên đã có result của hộp đen thì bỏ qua.
- Thu bằng chứng từ chính lần boot này:
  - AMD: dòng "Previous system reset reason" trong log kernel.
  - pstore: mount `/sys/fs/pstore`, nạp `efi_pstore`, chép bản ghi panic/MCE vào report rồi xóa khỏi NVRAM.
- Hiện thông báo trước menu, thay dòng `WATCHDOG RECOVERY` hiện nay, ví dụ:
  `Phiên trước dừng giữa chừng: COMBINED OVERLAP max, 20 s sau khi mọi lớp cùng chạy; RAM trống cuối 0,9 GiB; reset bằng nút nguồn`.
- Phân loại theo bảng ở FC5. Lần boot sau trên Intel không có lý do reset: INCOMPLETE, không tính FAIL.
- Kiểm (QEMU): reset máy ảo giữa overlap (`system_reset` qua monitor) → lần boot sau có thông báo, report và dòng CSV.

### FC4 — Watchdog: dấu hiệu sống thật, hạn chót khởi động, bằng chứng khi kill

- Trước khi kết luận treo (W1), xem thêm cho cả nhóm tiến trình: bộ đếm I/O (`/proc/<pid>/io`), thời gian CPU, số
  tiến trình ở trạng thái D.
  - I/O hoặc CPU vẫn tăng: **I/O chậm**, chờ tiếp tới một giới hạn cứng và hiện cảnh báo, không kill.
  - Không có gì tăng: **treo thật**.
- Hạn chót cho heartbeat đầu tiên khoảng 5 phút (W2), quá hạn ghi `STARTUP_STALLED`. Không áp dụng cho lúc người vận
  hành đang chọn test ở picker.
- `watchdog-final.json` ghi bằng chứng của cửa sổ cuối: biến thiên I/O, CPU, trạng thái D, lỗi I/O trong log kernel.
- Tiến trình điều phối COMBINED ghi heartbeat luồng chính vào trạng thái phiên (FC3a); hộp đen canh nó.
- Kiểm: SIGSTOP một worker (không I/O, không CPU) → treo thật; worker trên NTFS loop hoặc thiết bị bị làm chậm → không
  kill, có cảnh báo.

### FC5 — Phân loại PLATFORM, hiện nguyên nhân, mã trong CSV

| Nguyên nhân (mã) | Bằng chứng | Tính là |
|---|---|---|
| `reset_under_load` | nhật ký chưa đóng + lý do reset AMD là mất nguồn hoặc ngắt nhiệt | PLATFORM |
| `kernel_panic`, `mce` | pstore | PLATFORM |
| `thermal_stop` | `gpu/thermal_policy` | PLATFORM (tản nhiệt) |
| `gpu_off_bus` | Xid 79 | `? PCIe-GPU\|PLATFORM` như hiện nay |
| `worker_crash` | signal SEGV/BUS/ILL hoặc `cpu-ram/worker_crash` | PLATFORM |
| `layer_stall_io_error` | treo thật + lỗi I/O trong log kernel | SSD-path |
| `layer_stall` | treo thật, kernel không báo lỗi | INVALID, hạ tầng test |
| `slow_io` | I/O chậm quá giới hạn cứng | INVALID, hạ tầng test |
| `oom`, `memory_guard` | OOM của cgroup lớp, hoặc chốt chặn FC2 | INVALID, hạ tầng test |
| `app_hang_reset` | nhật ký chưa đóng, mọi lớp cùng đứng, reset bằng nút nguồn | INVALID, hạ tầng test |
| `unknown_reset` | nhật ký chưa đóng, không có lý do reset (Intel) | INCOMPLETE, không tính FAIL |
| `operator_rescue` | dừng từ console cứu hộ (FC1) | INVALID |

Hiển thị:

- Thêm một hàng dưới `FAIL BY PART`, chỉ hiện khi có PLATFORM hoặc INVALID có nguyên nhân, mỗi nguyên nhân một dòng
  kèm lớp và pha, ví dụ:
  ```
  FAIL BY PART  CPU 0 | RAM 0 | PCIe-GPU1 0 | GPU1 0 | VRAM1 0 | SSD1 0 | PLATFORM 1
  PLATFORM      reset under load: COMBINED OVERLAP, 20 s after every layer started (max)
  INVALID       SSD1 layer stalled 180 s, no kernel I/O error (test infrastructure)
  ```
- CSV giữ nguyên header; mã nguyên nhân ghi vào cột `cells`, ví dụ `PLATFORM=1(c1/s0)[reset_under_load]`.
- Cảnh báo sớm khi đang chạy: lớp không tiến triển từ 60 s thì hiện ở chân màn hình, ví dụ
  `! SSD1 no progress 60 s (I/O still moving)`, cùng cơ chế với cảnh báo throttle. Người vận hành không dừng nhầm hoặc
  reset khi chưa rõ chuyện gì.

### FC6 — Sửa treo NTFS (và exFAT)

- Trên `ntfs3`/exFAT, lượt đầu ghi **tuần tự** toàn bộ file test bằng mẫu dữ liệu test, có báo tiến độ vào
  `events.jsonl`. Lượt này được tính là dữ liệu kiểm, không lãng phí, và không còn bước lấp số 0 bị chặn. Sau đó mới
  vào pha ghi ngẫu nhiên.
- Engine báo tiến độ cả trong lúc tạo và chuẩn bị file.
- Kiểm: ảnh NTFS loop (`mkfs.ntfs` trên file, mount `ntfs3`), mức HEAVY thu nhỏ: sự kiện đầu tiên trong vài giây,
  tổng thời gian và byte ghi so với ext4.

### FC7 — Hardware watchdog (tùy chọn, bật thử)

- Tham số boot bật thử (ví dụ `dit_hw_watchdog=1`): nạp `sp5100_tco` hoặc `iTCO_wdt`, mở `/dev/watchdog` với thời
  gian chờ khoảng 120 s, do console cứu hộ (đã `mlockall`) giữ.
- Máy treo cứng thì board tự reset; lần boot sau AMD báo lý do "watchdog", FC3 ghi đúng là máy treo.
- Rủi ro: TCO trên một số board bị BIOS tắt hoặc chạy không ổn định. Chỉ bật mặc định sau khi thử trên vài board.
- Cần chốt: memory livelock cũng làm tiến trình giữ watchdog đứng, nên board sẽ reset sau 120 s. Đó là ghi nhận đúng
  ("treo"), nhưng mất cơ hội dùng console cứu hộ; cân nhắc thời gian chờ dài hơn.

### FC8 — RAM nghi ngờ (tạm gác)

- Xác nhận: khi `rewrite_fixed`, cho **chính nhân đã ghi ban đầu** ghi lại nhiều lần rồi đọc từ một nhân khác. Vẫn sai
  thì ghi CPU xác định; đúng thì giữ RAM nghi ngờ.
- Hiển thị: ghi rõ "nghi ngờ" và kết quả xác nhận thay vì `(1 susp)`, trỏ tới chi tiết trong `fault_ledger.events` của
  report. Vẫn trong phạm vi đếm theo ô linh kiện, không gợi ý đổi khe hoặc CPU.

## 5. Kiểm chứng bằng chèn lỗi

| Lỗi giả lập | Cách tạo | Bước kiểm |
|---|---|---|
| driver NVIDIA kẹt | `nvidia-smi` giả ngủ mãi trong `PATH` | FC0 |
| app bị kill | `kill -9` tiến trình điều phối | FC3a |
| app treo | SIGSTOP tiến trình điều phối hoặc UI | FC1, FC3a, FC4 |
| cạn RAM | tiến trình cấp phát dư trong QEMU RAM nhỏ | FC1 [K], FC2, FC3a |
| SSD không trả lời | `dmsetup suspend` thiết bị test | FC3a, FC4 |
| lỗi I/O thật trong log kernel | `dm-flakey` | FC3a, FC5 |
| mất nguồn hoặc reset giữa test | `system_reset` qua QEMU monitor | FC3b, FC5 |
| marker hoặc nhật ký cắt dở | cắt ngắn file ở giữa dòng | FC3 (unit test) |
| I/O chậm | NTFS loop hoặc thiết bị bị làm chậm | FC4, FC6 |
| treo thật không I/O | SIGSTOP worker | FC4, FC5 |

Không chứng minh được trên QEMU: lý do reset AMD, pstore trên NVRAM thật, hardware watchdog. Ba mục này cần kiểm trên
máy thật.

## 6. Câu hỏi mở

1. Phiên GPU-VRAM ngày 10/5 16:32 (USB 3) có phải lần GTX 1050 Ti bị đen màn không?
2. Hai lỗi RAM nghi ngờ trên 7500F có phải cùng một con CPU cắm sang hai board không?
3. Thêm giá trị `INCOMPLETE` cho cột `verdict` của CSV: bảng tổng hợp phía sau có đọc được không?
4. Thời gian chờ của hardware watchdog (FC7) và danh sách board để thử.

## 7. Bằng chứng

Dữ liệu thực tế không nằm trong git (khoảng 670 MB); bản 4 và 5 trùng nhau.

- `Result/<n>/fail-summary.csv`: một dòng mỗi lần chạy (đọc bằng parser CSV thật, cột GPU có dấu phẩy trong ngoặc kép).
- `Result/<n>/watchdog/`: marker từng lớp; các file `RUNNING` và file rỗng là phiên chết giữa chừng.
- `Result/<n>/runs/suite-combined-*/combined/combined-report.json` và thư mục từng lớp `pNN-<layer>/`
  (`watchdog-final.json`, `console.log`, `events.jsonl` của SSD).
- `Result/<n>/boot/`: `driver-probe.txt`, `nvidia-driver-install.txt` của lần boot cuối.

## 8. Tiến độ

| Bước | Trạng thái |
|---|---|
| FC0 `nvidia-smi` timeout | xong (2026-10-12): mọi `nvidia-smi` và `nvme smart-log` qua `dit_contracts::bounded` (hạn 3 s / 5 s / 10 s, lệnh kẹt bị bỏ lại, gate chặn lệnh mới); đo GPU và SMART ở luồng nền (`GpuTelemetryFeed`, `SmartFeed`); màn hiện `nvidia-smi silent N s (driver not answering)`. Test: `dit-system/tests/nvidia_smi_stuck.rs` |
| FC1 console cứu hộ + SysRq | đề xuất |
| FC2 chốt chặn bộ nhớ | đề xuất |
| FC3a hộp đen tự ra result trong phiên | đề xuất; đã chốt quan sát và ghi, thời gian chờ, tự chuyển màn kết quả (2026-10-12) |
| FC3b hậu kiểm ở lần boot sau | đề xuất |
| FC4 watchdog | đề xuất |
| FC5 phân loại PLATFORM | đề xuất |
| FC6 NTFS | đề xuất |
| FC7 hardware watchdog | đề xuất, tùy chọn |
| FC8 RAM nghi ngờ | tạm gác |
