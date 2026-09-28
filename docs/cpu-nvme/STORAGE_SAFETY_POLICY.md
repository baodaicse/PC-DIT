# Chính sách an toàn lưu trữ

## Mặc định

Mặc định là `READ_ONLY`. Khi người dùng yêu cầu test có ghi nhưng phải giữ dữ liệu, ứng dụng dùng `DATA_PRESERVING_FILE`; raw `BOUNDED_WRITE` không được dùng trên ổ có dữ liệu cần giữ. Tốc độ hoặc mục tiêu test không được phép lấn át bảo vệ dữ liệu.

## Thiết bị luôn bị chặn ghi

- block device chứa root filesystem hoặc filesystem đang mount;
- swap đang dùng;
- USB/live boot device và phân vùng kết quả;
- disk có holder/mapping RAID, LVM, dm-crypt hoặc multipath chưa được giải quyết;
- thiết bị identity thay đổi giữa lúc chọn và lúc mở;
- target không phải scratch được xác nhận trong `BOUNDED_WRITE`;
- thiết bị/namespace không đủ alignment hoặc topology không xác định an toàn.

Safety check phải chạy ở discovery, trước start và ngay trước mở write handle.

## Các mức ghi

### READ_ONLY

Không ghi. Có thể đọc/xác minh dataset DIT đã chuẩn bị hoặc chạy scan/telemetry. Một ổ ngẫu nhiên chưa có expected dataset không thể nhận PASS integrity đầy đủ chỉ bằng đọc và hash chính nó.

### DATA_PRESERVING_FILE

Đây là chế độ ghi mặc định cho SSD đang chứa dữ liệu:

- chỉ tạo file test mới bằng `O_EXCL`, không truncate/reuse path đã tồn tại;
- xác minh filesystem của thư mục test thuộc đúng stable ID NVMe đã chọn;
- giữ lại một lượng free space dự phòng cấu hình được;
- direct write, flush, direct reread và exact verify chỉ trong file test;
- xóa file test sau khi đóng backend và ghi trạng thái cleanup vào report.
- selection/event/report chỉ được tạo mới, không ghi đè output path đã tồn tại.

Chế độ này không bao phủ các LBA chứa dữ liệu cũ. Nó bảo vệ ở cấp ứng dụng nhưng không thể loại bỏ rủi ro một SSD vốn đã lỗi nặng bị mất dữ liệu khi chịu tải; dữ liệu quan trọng vẫn cần backup.

### BOUNDED_WRITE

Chỉ scratch partition/namespace được phép xóa. Không dùng trên SSD có dữ liệu cần giữ. Plan có:

- phạm vi LBA;
- số byte host write tối đa;
- duration tối đa;
- queue depth và block-size set;
- flush/FUA policy;
- temperature policy;
- serial/stable ID đã xác nhận.

Engine dừng trước khi vượt budget, không “vượt một chút” do batch chưa tính.

### FULL_DESTRUCTIVE

Toàn namespace có thể bị ghi đè và mất dữ liệu. Cần opt-in rõ ràng và xác nhận model + serial/NGUID. Không có default hoặc shortcut trong image phát hành đầu tiên.

## TBW và write amplification

4 KiB host write không đồng nghĩa NAND write thấp; random small writes có thể tăng write amplification. Vì vậy profile khuyến nghị:

- chuẩn bị/fill có giới hạn;
- nhiều vòng đọc và exact-verify;
- random-read 4 KiB để stress queue/FTL;
- sequential read lớn để stress PCIe/nhiệt;
- overwrite chỉ trong bounded region;
- ghi nhận SMART data units written trước/sau và host-write counter của engine.

## Thermal và stop policy

Temperature limit phải cho phép bật/tắt, mặc định tắt giống hướng vận hành DIT CPU–RAM, nhưng UI luôn hiện nhiệt và cảnh báo. Khi bật, ngưỡng/hysteresis phải nằm trong plan/report.

Fatal stop gồm device disappearance, controller reset storm, I/O hang không phục hồi, fatal AER, vượt write budget hoặc nhiệt vượt ngưỡng đã bật. Mismatch dữ liệu có thể tiếp tục và đếm theo policy để thu bằng chứng, nhưng first-N mẫu phải được bảo toàn.

## Báo cáo bắt buộc

Chụp trước/sau: SMART data units read/written, media/data integrity errors, error-log entries, unsafe shutdowns, available spare, percentage used, temperature và controller busy time. Ghi PCI BDF/root port, negotiated link speed/width, AER, kernel timeout/reset và mọi thay đổi identity.
