# Bảo toàn dữ liệu khi kiểm tra SSD

## Cam kết của phần mềm

Chế độ vận hành mặc định cho SSD đang chứa dữ liệu là `DATA_PRESERVING_FILE`. Ứng dụng chỉ tạo một file test mới trong vùng trống của filesystem và không mở ghi bất kỳ file hay block device đã tồn tại nào.

Các điều kiện bắt buộc:

- người vận hành chọn namespace bằng stable ID và xác nhận lại đúng ID;
- thư mục test phải nằm trên filesystem được mount trực tiếp từ namespace hoặc partition thuộc đúng NVMe đã chọn;
- file test có tên duy nhất và được tạo bằng cơ chế `create_new`/`O_EXCL | O_NOFOLLOW`; nếu path đã tồn tại thì dừng;
- backend dùng `O_DIRECT | O_DSYNC`, giới hạn 4 KiB alignment và host-write budget trước mỗi submission;
- dung lượng có thể cấp phát cộng vùng trống dự phòng phải nhỏ hơn free space hiện có;
- sau mỗi lần ghi có flush và direct reread để verify;
- file test bị xóa sau khi engine đóng file; report ghi nhận `test_file_was_new=true` và `test_file_removed=true`.

Simulator cũng không còn tùy chọn overwrite file đã tồn tại.
Các file selection, event và report cũng dùng `create_new`; path output đã tồn tại sẽ bị từ chối thay vì truncate. Trong image, chúng được ghi vào tmpfs khi engine chạy và chỉ được copy sang USB `DIT-RESULTS` sau khi đóng; USB không nhận luồng event liên tục.

Nếu tiến trình bị mất điện hoặc `SIGKILL`, file `.cpu-nvme-dit-*.test` có thể còn lại. File này chỉ thuộc phiên DIT và có thể xóa sau khi xác minh không còn process test; dữ liệu cũ không bị app ghi đè để tạo file đó.

## Tự chuẩn bị vùng test

UI hiện tự chuẩn bị thư mục/file test sau xác nhận, không tạo partition. Tái sử dụng mount dữ liệu an toàn hoặc mount filesystem ext4/NTFS/exFAT hiện có; không format, unlock BitLocker/LUKS, chạy fsck, force mount hay xóa hiberfile. NTFS chưa mount phải vượt qua `ntfs-3g.probe --readwrite`, sau đó dùng kernel `ntfs3`; FUSE/LVM/RAID không được tự chuyển đổi.

Mount ghi có thể cập nhật metadata/journal của filesystem; không đồng nghĩa toàn bộ SSD giữ nguyên từng byte. App chỉ dọn thư mục mới nếu rỗng và unmount mount do chính phiên đó tạo. Nếu dọn thất bại, báo lỗi và giữ lại đường dẫn, không xóa đệ quy/force unmount. Xem [AUTO_STORAGE.md](AUTO_STORAGE.md).

## Chế độ raw

`run-bounded` ghi trực tiếp vào block range và chắc chắn phá dữ liệu trong range đó. Chế độ này chỉ dành cho namespace/partition scratch trong phòng kiểm thử, tuyệt đối không dùng cho SSD đang có dữ liệu cần giữ. UI phát hành thông thường chỉ đưa `DATA_PRESERVING_FILE` làm lựa chọn mặc định; raw mode phải nằm trong phần lab/expert và có cảnh báo riêng.

## Giới hạn của cam kết

Không phần mềm stress test nào có thể bảo đảm tuyệt đối dữ liệu cũ vẫn đọc được nếu SSD, firmware, nguồn điện, RAM hoặc filesystem thực sự hỏng trong lúc test. Cam kết ở đây là tính toàn vẹn ở cấp ứng dụng: DIT không chọn, truncate hoặc ghi đè dữ liệu đã tồn tại.

Với dữ liệu không thể thay thế, vẫn cần backup độc lập trước stress test. Chế độ file không bao phủ các LBA đang chứa file cũ; nó kiểm tra đường CPU/RAM/filesystem/block/NVMe/media trên các block trống được filesystem cấp cho file test. Muốn phủ toàn bộ media phải dùng SSD scratch và raw destructive campaign.

Device Mapper, LVM, RAID và dm-crypt chưa được chấp nhận trong phiên bản này vì chưa chứng minh được mapping trực tiếp tới đúng NVMe. Công cụ sẽ fail closed thay vì đoán target.

## CLI hiện tại

```bash
cargo run -p nvme-cli -- run-preserving \
  --selection selection.json \
  --device 1 \
  --directory /mnt/ssd-test \
  --confirm-device-id 'STABLE_ID_CHÍNH_XÁC' \
  --capacity-mib 8192 \
  --write-budget-mib 2048 \
  --reserve-free-mib 4096 \
  --duration-seconds 1800 \
  --events events.jsonl \
  --report report.json
```

Lệnh chỉ chạy sau khi selection được revalidate, filesystem map đúng vào NVMe đã chọn và free-space guard đạt. Có thể thêm `--cpu-ram-preflight cpu-ram-preflight.json` để tăng evidence attribution; UI mặc định không chạy preflight.

TASK-D bổ sung `run-preserving-multi`: lặp `--target ALIAS=/thư/mục` và `--confirm-device-id` cho toàn bộ SSD trong selection. Mỗi target vẫn có file, budget, free-space evidence và cleanup riêng; không có lệnh raw destructive nhiều ổ.
