# CPU–NVMe Deep — pipeline mặc định

Trạng thái: `implemented` ở mức source/automated host gates ngày 2026-09-06; chưa hardware validated. Không giữ profile Standard.

## D1 — Định dạng và kiểm chứng tính toán

- Block 4 KiB `DITDEEP2`: header định danh seed/generation/device/LBA, ma trận A/B/C số nguyên 16×16, padding và SHA-256.
- Tính C bằng các vòng nhân ma trận modulo 2^32 có phép trộn theo vòng. Producer dùng đường tối ưu; reference dùng tích vô hướng tích lũy u128, không gọi lại kernel tối ưu.
- Permutation khi chia/ghép các word được kiểm tra; header/padding và toàn bộ A/B/C phải đúng. Sau SSD read, tính lại từ A/B thực sự đọc về; đối chiếu C trên SSD và reference tái tạo từ context.
- Golden/known-answer và fault injection phải bắt lỗi producer, readback compute, join, dữ liệu, generation/LBA và reference.

## D2 — Worker và pipeline có giới hạn

- Một coordinator I/O cho mỗi SSD; pool compute riêng xử lý các phần của batch.
- Chuẩn bị batch kế tiếp trong khi coordinator ghi/flush/đọc batch hiện tại. Queue/in-flight batch bị giới hạn, không tăng vô hạn theo duration.
- Không ghi batch kế tiếp trước khi batch hiện tại được verify; các vùng read-soak chỉ giữ record chưa bị ghi đè, không đọc context cũ của vùng overlap.
- Phân bổ tổng compute workers theo logical CPUs và số SSD chạy đồng thời; chừa tài nguyên cho I/O/UI/OS. Không hứa CPU usage 100% hay throughput tối đa.

## D3 — Ngân sách ghi và duration

- Khi có duration, hết số operation ghi được cấp phép thì chuyển sang direct reread + matrix verify trên các batch đã ghi/verify; không phát sinh thêm host writes.
- Deadline chặn submission mới, drain batch đang chạy rồi trả report. Không ép ngắt syscall I/O đang kẹt; thời gian thực có thể vượt deadline bởi batch/syscall đang chạy.
- Report phân biệt write operations, read-soak operations, byte counters, matrix checks, workers, thời gian compute/I/O/wait và việc đạt duration.
- CLI simulation có `--operations` vẫn hỗ trợ campaign hữu hạn để regression không kéo dài hàng giờ; đây là giới hạn số việc, không phải profile Standard.

### Seed và kích thước I/O

- Mỗi SSD dùng một file test mới, không tạo hàng chục file nhỏ. Bên trong file, từng operation tự xoay kích thước transfer để kiểm tra split/join và nhiều đường DMA/I/O khác nhau.
- Catalog hiện tại: 4, 8, 12, 16, 24, 32, 48, 64, 80, 96, 128, 160, 192, 256, 320, 384, 512, 640, 768 KiB và 1 MiB (thứ tự chạy bắt đầu bằng 4 KiB, 64 KiB, 128 KiB, 1 MiB rồi các cỡ mở rộng).
- Duration 30 phút dùng 10 seed slot và 10 kích thước; 60 phút dùng 20 seed slot và đủ 20 kích thước. Operation luân phiên seed/size, generation tăng theo vòng seed.
- Read-soak không tạo file/seed mới vì sẽ cần ghi thêm. Nó luân phiên lại các extent đã ghi cùng seed, generation, LBA và kích thước tương ứng để không tăng TBW.

## D4 — UI, gates và phát hành

- UI hiển thị DEEP, CPU workers, compute checks và read-soak; progress theo thời gian cho phiên duration, counters vẫn là công việc đã hoàn tất.
- Giữ file-only, create-new, stable-ID, budget/free-space và cleanup guards. Không format/raw, không nhân lượng ghi vì tăng workers.
- Gate phần mềm: oracle/golden, input/output/compute/reference faults, giới hạn queue/worker/budget, read-soak đủ duration, context overlap và 1/2 SSD memory/file simulation.
- Gate hardware sau: SSD đã backup, một/nhiều NVMe, đo CPU/SSD throughput theo batch và workers, direct I/O, shutdown/cancel, error recovery và thermal behavior. Không suy lỗi riêng CPU hay SSD chỉ từ một mismatch.

CPU, RAM và reference đều chạy trên cùng máy nên không có bảo đảm phát hiện mọi common-mode hardware fault. Việc module Deep bỏ PASS sớm theo write budget không thay đổi nguyên tắc cần backup dữ liệu trước stress test.

## Bằng chứng triển khai

- Đường engine mặc định dùng `DITDEEP2`; producer và verifier không gọi block generator v1 làm oracle.
- Preserving-file và raw bounded tự chia logical CPU theo số SSD đồng thời, chừa một logical CPU cho Linux/UI và giới hạn 1–64 compute workers mỗi SSD.
- Hết write operations theo ngân sách sẽ chỉ reread các extent mới nhất chưa bị overlap và chạy matrix/reference cho đến deadline; test tự động xác nhận host bytes written không tăng trong read-soak.
- Event/report có evidence cho workers, rounds, compute jobs, matrix checks, write/read-soak operations, thời gian CPU/I/O/wait và duration target.
- UI Deep hiển thị progress theo duration, tổng matrix checks, I/O, failure, write/read-soak và một speed row động cho mỗi SSD.
- `RAM used / total` lấy từ `MemTotal - MemAvailable`, không phải dung lượng USB/SSD. SSD capacity được hiển thị riêng theo namespace. UI vẫn ingest mọi event để giữ counter live, còn `events.jsonl` chỉ journal snapshot tối đa mỗi 500 ms cho từng class/SSD; failure, warning, finalizing và completion không bị bỏ.
- CPU frequency/temp/power không còn hard-code `N/A`: frequency đọc cpufreq rồi fallback `/proc/cpuinfo`; temp và power dùng cùng live hwmon/powercap collector với CPU‑RAM, refresh 1 giây. Từng giá trị vẫn `N/A` độc lập khi platform không expose sensor tương ứng.
- CPU‑RAM và CPU‑SSD dùng cùng GPU inventory parser: mọi PCI display class 0300/0302/0380 được liệt kê với BDF, model và driver bind. VRAM chỉ hiển thị khi sysfs driver cung cấp `mem_info_vram_total`; không suy đoán từ PCI BAR. Compact UI hiện GPU khi đủ chiều cao.
- Fault campaign có 14 lớp âm: 9 lỗi data/I/O cũ, producer compute, readback compute, independent reference, join và read-soak bit flip.
- Golden SHA-256 của block fixture `DITDEEP2`: `055f7fb59f5716e3a1c1269aea6b9c5f68dc653938f25fabec1540d2ff68a53c`.
- Host regular-file smoke PASS 8/8 operations, 1.220 matrix checks, 15 compute workers và 0 failure; report/event ở `build/deep-host-check-20260906`.
- Duration smoke 1 giây PASS: 4 write operations + 108 read-soak operations, 8.845 matrix checks, không tăng bytes written trong soak và đạt duration target.
- `cargo fmt --check`, Clippy strict và test Deep/UI/contracts/CLI đã PASS. Full workspace chạy tuần tự được dùng làm gate cuối để tránh false negative từ test timing CPU–RAM cũ khi test song song.

Các kết quả trên chỉ xác nhận phần mềm bằng memory/regular-file backend. Chưa có kết luận về hiệu quả tải CPU, direct-I/O throughput, nhiệt độ hay phát hiện lỗi trên NVMe thật cho tới khi hoàn tất hardware matrix.
