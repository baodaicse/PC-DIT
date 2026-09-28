# Đặc tả sản phẩm DIT CPU–NVMe

## 1. Bài toán

DIT CPU–NVMe kiểm tra tính toàn vẹn trên đường dữ liệu:

`nguồn dữ liệu -> CPU tạo/chia block -> RAM buffer -> PCIe/DMA -> NVMe controller -> media -> đọc lại -> CPU xác minh`

Một lỗi có thể xuất hiện dưới dạng byte sai, block cũ, block đặt nhầm LBA, I/O error, timeout, controller reset, PCIe AER, link tụt tốc hoặc thiết bị biến mất. CPU–SSD chạy độc lập không cần CPU–RAM preflight. Vì buffer vẫn đi qua CPU và RAM, phiên không có preflight dùng attribution `CPU_SSD_PATH_UNATTRIBUTED`: mismatch chứng minh đường dữ liệu/tính toán có lỗi nhưng không tự kết luận riêng SSD hỏng. Nếu CLI có evidence CPU–RAM PASS thì report dùng `SSD_AFTER_CPU_RAM_PREFLIGHT` để tăng độ tin cậy attribution.

## 2. Phạm vi phiên bản đầu

- Chạy trên Ubuntu Noble minimal x86-64/HWE của image DIT CPU–RAM dùng chung.
- NVMe namespace dạng block device.
- Engine chạy standalone trên Linux khi phát triển; bản USB tích hợp vào image DIT CPU–RAM hiện có, không tạo image NVMe riêng.
- Chọn một, nhiều hoặc tất cả NVMe phát hiện được.
- Theo dõi riêng từng namespace/thiết bị bằng định danh ổn định.
- Test tuần tự và ngẫu nhiên, read-only hoặc ghi có giới hạn.
- Báo cáo JSON và bản tóm tắt dễ đọc; không chỉ dựa vào màn hình UI.

Ngoài phạm vi bước đầu: SATA/AHCI, RAID phần cứng, vendor firmware updater, tự sửa lỗi, benchmark dùng để xếp hạng hiệu năng, Secure Boot ký riêng.

## 3. Mô hình dữ liệu kiểm chứng

Đơn vị nền là block 4 KiB. Mỗi block chứa:

- magic và phiên bản format;
- run/session ID và seed;
- generation;
- stable device ID và namespace;
- LBA dự kiến, stripe ID và chỉ số block;
- ba ma trận số nguyên A/B/C 16×16 được permutation trong block;
- padding xác định và SHA-256 toàn block.

Format mặc định là `DITDEEP2`. Producer tạo A/B, tính C bằng kernel tối ưu rồi đối chiếu oracle u128 độc lập trước khi cho phép ghi. Sau readback, verifier chỉ dùng buffer vừa đọc để split/join, tính lại C và đối chiếu cả C lưu trên SSD lẫn oracle tái tạo từ context. Header, hash và padding cũng được kiểm tra. Nhờ vậy engine phát hiện byte flip, stale data, duplicate block, swapped LBA, chia/ghép sai và sai số tính toán CPU ở cả producer/readback/reference path.

Recipe không chỉ dùng 4 KiB:

- 4 KiB chiếm tỷ trọng cao để stress queue, FTL và IOPS;
- 64/128 KiB kiểm tra gom/tách stripe và đường DMA lớn hơn;
- 1 MiB tuần tự tạo tải bandwidth, PCIe và nhiệt;
- read-after-write, delayed read và reread bằng seed khác nhau.

Duration dài hơn phải tăng generation, seed, vùng/LBA và recipe đã chạy; không chỉ lặp vô hạn một mẫu. Báo cáo phải ghi rõ bytes touched, bytes verified, unique regions, seeds/generations và host writes.

Ở chế độ Deep theo duration, engine ghi đến operation/write budget đã duyệt rồi chuyển sang reread + recompute các extent hợp lệ đến deadline. Giai đoạn này tiếp tục tải đường NVMe→RAM→CPU nhưng không phát sinh thêm host write/TBW.

## 4. Chế độ vận hành

1. `READ_ONLY`: inventory, telemetry và xác minh vùng dữ liệu DIT đã được chuẩn bị. Không tuyên bố phủ toàn ổ nếu chưa có dataset chuẩn.
2. `DATA_PRESERVING_FILE`: mặc định khi SSD có dữ liệu; chỉ tạo file mới trong vùng trống rồi direct-write/read/verify và cleanup.
3. `BOUNDED_WRITE`: chỉ ghi vào scratch partition/namespace được chọn rõ ràng, có trần host-write/TBW cho phiên.
4. `FULL_DESTRUCTIVE`: dùng toàn namespace, xóa dữ liệu; chỉ mở sau khi nhập/xác nhận đúng serial và cảnh báo rõ.

Mặc định an toàn trước selection là `READ_ONLY`. Trong UI, việc người dùng chọn rõ alias SSD hoặc `A` đồng thời cho phép bắt đầu `DATA_PRESERVING_FILE`; chỉ phát hiện thấy SSD không tự khởi động test.

## 5. Nhiều SSD

Danh sách hiển thị nhãn ngắn `Device 1`, `Device 2`, ... nhưng nội bộ phải khóa theo serial + NGUID/EUI64 (nếu có) + namespace + PCI BDF. Người dùng có thể toggle từng thiết bị hoặc `All`.

Khi chọn nhiều SSD:

- mỗi thiết bị có queue/worker, counters, telemetry và verdict riêng;
- lỗi một SSD không được làm mất bằng chứng của SSD khác;
- có aggregate verdict nhưng luôn giữ verdict theo thiết bị;
- có thể chạy đồng thời để stress PCIe/root complex và nguồn;
- scheduler phải giới hạn CPU/RAM để bản thân engine không gây OOM hoặc làm telemetry chết đói.

## 6. Tiêu chí kết quả

Hiệu năng thấp không tự động là FAIL tính toàn vẹn. Tốc độ là telemetry và dấu hiệu chẩn đoán. PASS của mỗi thiết bị yêu cầu recipe/budget hoàn tất, mọi compare chính xác, không có lỗi fatal và báo cáo đầy đủ. Tổng phiên PASS chỉ khi tất cả thiết bị đã chọn PASS. CPU–RAM preflight là evidence attribution tùy chọn; nếu được cung cấp nhưng không hợp lệ, CLI fail closed trước khi chạy.
