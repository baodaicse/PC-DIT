# TASK-F — Module control center và shared-image extension

## Trạng thái

TASK-F đã `implemented` ngày 2026-09-05 ở mức code, host test và artifact extension. Chưa đánh dấu `validated` trên USB/phần cứng thật; image dùng chung chưa được build trong lượt này theo quy trình người vận hành chủ động đóng image.

## UI lựa chọn bài test

Binary `dit-suite` là control center khởi chạy khi image chứa extension:

```text
+----------------------------------------------------------------------------------------------------------------------+
| DIT CONTROL CENTER // SELECT TEST MODULE                                                                             |
| CPU-RAM and CPU-SSD share one boot image                                                                             |
+----------------------------------------------------------------------------------------------------------------------+
|  [1] CPU + RAM            READY         dit.cpu-ram                                                                  |
|  [2] CPU + SSD            READY         dit.cpu-nvme                                                                 |
|  [3] VGA (future module)  NOT INSTALLED dit.vga                                                                      |
|  [I] NVMe inventory (read-only)                                                                                      |
|  [F] Format SSD (quick, erases a whole disk)                                                                         |
|  [D] Diagnostic shell                                                                                                |
|  [R] Reboot          [P] Power off                                                                                   |
+----------------------------------------------------------------------------------------------------------------------+
```

- `CPU + RAM` mở UI CPU–RAM hiện có; phím `B` trở lại module control center.
- `CPU + SSD` mở thẳng device picker; không chạy CPU–RAM Safe/minimum preflight.
- Device picker hiện device node, model, dung lượng, serial, kernel driver, PCIe link và safety state.
- CPU–SSD UI chỉ chạy `DATA_PRESERVING_FILE`: chọn một/nhiều SSD là bắt đầu ngay với duration cấu hình sẵn. Không còn màn hình plan, prompt duration, câu `RUN PRESERVING` hoặc nhập thư mục mount.
- Sau selection, UI tái kiểm tra stable ID, tự mount filesystem dữ liệu hiện có nếu cần và tạo thư mục test mới. Không format/chia partition, không mở raw mode. Chi tiết và giới hạn trong [AUTO_STORAGE.md](AUTO_STORAGE.md).
- Raw bounded/destructive vẫn chỉ có ở CLI chuyên gia và giữ toàn bộ confirmation gate của TASK-C.
- Giá trị 1.800 giây là duration/hard stop mặc định. Deep ghi đến write budget rồi read-soak cho đủ duration; chỉ PASS duration khi `duration_target_met=true`.

## Orchestrator và report

UI đang chạy NVMe đã đổi sang dashboard cùng bố cục CPU–RAM ngày 2026-09-06: Hardware/Live Utilization hai cột, Process bên dưới; mỗi SSD thêm một hàng live speed và một hàng progress/fail. Khung tự giãn, compact hoặc auto-page khi thiếu chỗ. Xem trước an toàn bằng `make preview-test-ui SSDS=2 FRAMES=80` (chỉ dữ liệu demo, không chạy test SSD). Binary mới cần được đóng lại vào shared image để USB nhận giao diện này.

Crate `dit-orchestrator` cung cấp:

- registry có version cho `dit.cpu-ram`, `dit.cpu-nvme` và placeholder `dit.vga`;
- adapter từ native CPU–RAM report/preflight và NVMe multi-device report;
- `SuiteReport` giữ verdict từng module và aggregate verdict;
- precedence an toàn `INVALID > FAIL > ABORTED > PASS > NOT_RUN`;
- resource plan chặn việc các module đồng thời tự nhận toàn bộ CPU/RAM;
- lệnh `aggregate` để ghép evidence offline mà không scrape UI text.

CPU–SSD chạy độc lập và suite report của lượt này chỉ chứa module `dit.cpu-nvme`. CPU–RAM report vẫn có thể ghép offline bằng lệnh `aggregate`; không còn preflight chặn đường test storage.

## Image extension

`make extension` tạo:

```text
dist/cpu-nvme-extension-root/
  opt/cpu-nvme-dit/cpu-nvme-dit
  usr/local/bin/dit-suite
  etc/dit-suite/cpu-nvme.conf
  etc/dit-suite/modules.d/cpu-nvme.json
  MANIFEST.sha256
dist/cpu-nvme-extension.tar.gz
dist/cpu-nvme-extension.tar.gz.sha256
```

Pipeline `/home/hoang/DIT-01` xác minh `MANIFEST.sha256`, stage payload trước khi đóng SquashFS và ghi extension hash vào image manifest. Không có Ubuntu rootfs, GRUB, partition hoặc burn implementation thứ hai.

Để người vận hành chủ động tạo image dùng chung:

```bash
cd /home/hoang/DIT-01
make shared-image
```

Lệnh trên gọi lại `make rufus-image` của `/home/hoang/DIT-01`. Nó có thể mất thời gian và không nằm trong host gate.

Target tương đương cũng được khai báo trong `/home/hoang/DIT-01/Makefile`, nên `make shared-image` hoạt động từ cả hai project. Cả hai đường đều dùng image builder của CPU–RAM; không tạo pipeline thứ hai.

Chỉ xem trước menu trên host, không khởi chạy test hay image:

```bash
make preview-menu
```

## Automated evidence

```bash
cd /home/hoang/DIT-01
make task-f-check
```

Kết quả ngày 2026-09-05:

- rustfmt và Clippy strict: PASS;
- 45 workspace tests: PASS;
- module menu/registry READY detection: PASS;
- CPU–RAM + hai NVMe simulated aggregate PASS: PASS;
- NVMe device 2 injected FAIL, CPU–RAM vẫn PASS, aggregate FAIL: PASS;
- extension binary/config/manifest SHA-256: PASS;
- dependency SBOM/Cargo.lock và operator/data-preservation manual nằm trong payload;
- hai lần build extension liên tiếp cho cùng SHA-256: PASS;
- CPU image-hook/runner Bash syntax và physical boot policy: PASS;
- không build image, không QEMU, không ghi SSD và không burn USB trong gate này.

## Phần còn phải hardware-validate

### Lịch sử sửa lỗi thư mục test chưa mount (luồng nhập tay cũ)

Ảnh thử nghiệm báo `No such file or directory (os error 2)` sau khi nhập đường dẫn ví dụ `/mnt/ssd-test`. UI trước đây chỉ kiểm tra đường dẫn sau confirmation; thông báo không chỉ ra file/thư mục nào thiếu.

Luồng nhập tay trước đây đã được bổ sung validation/retry. Bản hiện tại thay bước đó bằng automatic storage preparation; CLI `run-preserving-multi --target` vẫn hỗ trợ thư mục thủ công và giữ validation tại backend.

Chỉ `mkdir /mnt/ssd-test` là chưa đủ: đó có thể vẫn là thư mục trên RAM của live USB. Phải chọn filesystem dữ liệu hiện có và mount đúng partition trước khi dùng file mode. Không format ổ để xử lý lỗi này. Dùng Diagnostic chạy `lsblk -f` và `findmnt` để xác định filesystem/mount hiện tại; filesystem mã hóa hoặc chưa nhận diện cần xử lý riêng.

Kiểm thử bản sửa: Clippy strict và 4 CLI tests PASS, gồm missing-path không tạo file, path là regular file, alias trỏ về đường dẫn được bảo vệ. USB/hardware cần build lại image và kiểm tra sau.

### Các bước validation còn lại

- build shared image và xác minh checksum mới;
- boot BIOS/UEFI trên USB thật;
- menu [1]/[2], CPU–SSD mở thẳng picker/chạy sau selection và phím Back trên console thật;
- data-preserving run trên một/hai NVMe scratch filesystem;
- M9 negative/soak và M10 Intel/AMD/Gen3/4/5 matrix.
