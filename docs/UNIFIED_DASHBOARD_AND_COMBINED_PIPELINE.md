# Unified dashboard và combined stability pipeline

Cập nhật: 2026-09-07.

## Hợp đồng giao diện

Mọi bài test đang chạy dùng một `UnifiedDashboard` của `dit-ui-core`, contract `DIT-UNIFIED-1`. Phần cố định không được engine tự thay đổi:

1. `DIT-01 / DATA INTEGRITY TEST ENGINE` và engine/report ABI/profile;
2. `SYSTEM HARDWARE` bên trái;
3. `LIVE UTILIZATION` bên phải, tự tăng hàng theo SSD/GPU được chọn;
4. `TESTING / PROCESS INFORMATION` với state, progress, elapsed/target;
5. `CALCULATIONS COMPLETED` và sticky `FAIL TOTAL`;
6. temperature policy và Ctrl+C/preserve-report ở footer.

Chỉ vùng running thay đổi theo module: `ACTIVE TEST`, `PROCESS`, seed/chunk/route, transfer/matrix/retention và per-device counters. Wide, compact, paging, màu và refresh in-place đều do renderer chung xử lý. CPU–RAM Bash hiện có giữ đúng hợp đồng hình ảnh này; khi chuyển CPU–RAM runner sang Rust hoặc chạy combined mode, nó dùng trực tiếp cùng model thay vì dựng UI mới.

## Combined mode dự kiến

Combined không phải ghép ba PASS độc lập thành một màn hình. Một session có base seed và deadline chung, resource arbiter, event stream và aggregate report; scheduler xoay các route:

1. CPU ↔ RAM exact pattern/matrix để tạo baseline CPU/IMC/RAM.
2. CPU ↔ RAM ↔ NVMe 4 KiB/file-window, readback và CPU oracle.
3. CPU ↔ RAM ↔ PCIe ↔ GPU/VRAM, Vulkan matrix và CPU oracle.
4. Coupled route: tạo/split trên CPU, scatter RAM, ghi/đọc NVMe, upload VRAM, GPU transform, readback/join và CPU recompute độc lập.
5. Overlap burst có kiểm soát: CPU–RAM, NVMe và GPU cùng hoạt động để bắt lỗi nguồn/thermal/PCIe/IMC phụ thuộc tải.
6. Recovery/retention checkpoint giữa các burst để phân biệt mismatch dữ liệu với lỗi allocation/driver/timeout.

Combined vẫn giữ RAM và VRAM gần tối đa (cập nhật 2026-09-25): không chia theo phần trăm đoán trước mà theo thứ tự cấp phát, thành phần cấp phát sau cùng (CPU-RAM với RAM, GPU `--vram-max` với VRAM) lấy phần còn lại sau khi các thành phần khác đã cấp phát và khai báo đỉnh bộ nhớ; arbiter vẫn giữ reserve cho kernel, staging, UI và oracle. Chi tiết: [COMBINED_STRESS_CHAIN_ANALYSIS.md](COMBINED_STRESS_CHAIN_ANALYSIS.md) mục 6. Full overlap chỉ chạy sau khi từng route baseline đã PASS, nếu không sẽ làm mất khả năng attribution và có thể tự gây OOM.

## Verdict và mức stable

- Một exact mismatch luôn là FAIL sticky; điểm tổng không được che lỗi.
- Driver/capability/allocation/report thiếu là INVALID, không kết luận linh kiện hỏng.
- ABORTED vẫn drain checkpoint và giữ evidence.
- Mức stable dựa trên route đã phủ, duration thực, seed/chunk/sweep, allocation/traffic và hardware matrix: `BOUNDED`, `DEEP`, `SOAK`, không dùng một phần trăm tùy ý.
- Aggregate report giữ verdict riêng CPU–RAM, CPU–NVMe, GPU–VRAM và coupled route, sau đó mới đưa ra kết luận toàn máy.

## Các task phát triển

Phân tích thiết kế chi tiết cho COMBINED-B → F (hai lớp "bão hoà + khâu nối", chuỗi dữ liệu SSD → RAM → GPU → VRAM → CPU chủ yếu đọc (ghi một lần, đọc liên tục), mức BOUNDED/DEEP/SOAK, chia tài nguyên): [COMBINED_STRESS_CHAIN_ANALYSIS.md](COMBINED_STRESS_CHAIN_ANALYSIS.md) — đã chốt 2026-09-25. **COMBINED-B → F đã cài đặt** (menu `[5] COMBINED STRESS`, lệnh `dit-suite combined`, report `combined-report.json`); cách làm và kết quả kiểm ở mục 12 của tài liệu đó. Màn hình combined dùng đúng khung chung này: phần cứng và kế hoạch bên trái, thanh usage (CPU GHz/nhiệt/W, RAM, GPU, VRAM, tốc độ SSD, chuỗi) bên phải, tiến trình và bộ đếm FAIL từng lớp bên dưới.

### COMBINED-A — UI/event contract

Trạng thái: `implemented` ở source ngày 2026-09-07. `UnifiedDashboard`, dynamic meters, running rows và NVMe/GPU adapters đã có unit/layout gates. **2026-10-02:** màn hình đang chạy của CPU–RAM (Safe/Full/Compute) nay cũng do bộ vẽ chung vẽ: `cpu-dit-ui.sh` khởi chạy runner rồi gọi `dit-suite cpu-ram-view`, lệnh này đọc các dòng `CPU_DIT_PROGRESS`/`CPU_DIT_COUNTER` của runner và vẽ cho tới khi runner kết thúc (số FAIL không giảm khi lần chạy chẩn đoán lại đếm từ 0). Ctrl+C: view thoát, trap của script dừng runner, view mở lại để hiện quá trình dừng. Thiếu `dit-suite` thì script dùng màn hình bash cũ làm dự phòng. Menu và màn hình kết quả của CPU–RAM, và màn hình Power (CPU AVAIL / POWER), vẫn là bash.

### COMBINED-B — Resource arbiter

Pending. Lập ngân sách RAM/VRAM/staging/worker/I/O theo phase; fail closed nếu tổng reserve không an toàn. Thêm unit test 32/64/128 GiB, iGPU shared RAM, 1/2 NVMe và 1/2 GPU.

### COMBINED-C — Round-robin scheduler

Pending. Deadline/seed chung, weighted time slices, baseline trước overlap, stop/cancel/drain và progress không reset khi đổi engine.

### COMBINED-D — Coupled exact route

Pending. Versioned block header và provenance xuyên CPU–RAM–NVMe–VRAM; checkpoint tại từng biên để biết sai ở create/split/storage/upload/GPU/readback/join/oracle.

### COMBINED-E — Aggregate evidence và stability classification

Pending. Report chung nhưng không làm mất report module; thống kê duration, coverage, failure attribution, telemetry correlation và mức `BOUNDED/DEEP/SOAK`.

### COMBINED-F — Image và hardware validation

Pending. Dùng image chung hiện có; chạy negative campaign, 30/60 phút và hardware matrix Intel/AMD/NVIDIA, nhiều SSD/GPU, sau đó mới đánh dấu validated.
