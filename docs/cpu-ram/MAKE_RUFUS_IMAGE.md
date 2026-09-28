# Tạo CPU-DIT USB image bằng Makefile

## Kết quả đầu ra

Lệnh `make rufus-image` dựng raw disk image dùng trực tiếp với Rufus hoặc `dd`:

```text
dist/dit-01-noble-amd64.img
dist/dit-01-noble-amd64.img.sha256
dist/dit-01-noble-amd64.img.manifest.json
```

File `.img` đã gồm GPT, GRUB legacy BIOS, x86_64 UEFI, Ubuntu live read-only,
CPU-DIT UI và phân vùng `DIT-RESULTS`. Đây không phải installer ISO.

## 1. Chuẩn bị máy build

Dùng Ubuntu/Debian x86_64 có Internet, Rust toolchain, `make`, `sudo` và khoảng
10 GB dung lượng trống. Nếu thiếu Make:

```bash
sudo apt update
sudo apt install -y make
```

Kiểm tra môi trường cơ bản:

```bash
cd /home/hoang/DIT-01
make doctor
```

Build script sẽ tự cài các host package còn thiếu như `debootstrap`, GRUB,
SquashFS, FAT và partition tools bằng `apt`. `sudo` sẽ hỏi mật khẩu khi cần.

## 2. Build image cho Rufus

```bash
cd /home/hoang/DIT-01
make rufus-image
```

Quy trình tự động:

1. chạy rustfmt, Clippy và toàn bộ automated tests;
2. build Rust release binary;
3. tạo hoặc tái sử dụng Ubuntu Noble minbase rootfs cache;
4. cài HWE kernel, firmware, sensor/diagnostic packages;
5. tạo SquashFS read-only và initramfs;
6. tạo raw GPT image với `DIT-BOOT` và `DIT-RESULTS`;
7. cài GRUB BIOS + UEFI;
8. sinh Image ID, manifest và SHA-256;
9. kiểm checksum file `.img` vừa tạo.

Khi thành công, terminal in:

```text
RUFUS IMAGE READY
File: /home/hoang/DIT-01/dist/dit-01-noble-amd64.img
SHA-256: <hash mới>
```

Rootfs được cache tại `build/boot-image/`, nên các lượt build sau nhanh hơn. Không
xóa cache bằng lệnh recursive nếu chưa kiểm chính xác đường dẫn.

## 3. Kiểm image mà không build lại

```bash
make verify-image
make image-info
```

`verify-image` bắt buộc image, checksum và manifest cùng tồn tại rồi chạy
`sha256sum --check`. `image-info` in thêm dung lượng, Image ID và boot mode.

Kiểm boot bằng QEMU nếu máy build đã có QEMU/OVMF:

```bash
make qemu-smoke
```

Mở image tương tác như một máy thật ngay trên máy phát triển:

```bash
make run-image
```

Launcher mặc định dùng UEFI, GTK, 4 GiB RAM, 4 vCPU và snapshot mode nên không sửa
image gốc. Xem các biến tùy chỉnh tại
[DURATION_COVERAGE_UI_V2.md](DURATION_COVERAGE_UI_V2.md).

## 4. Ghi USB bằng Rufus

> Rufus sẽ xóa toàn bộ USB đã chọn. Phải kiểm tra đúng tên và dung lượng USB.

1. Mở Rufus trên Windows.
2. `Device`: chọn đúng USB từ 4 GB trở lên.
3. `Boot selection` → `SELECT`.
4. Chọn `dist/dit-01-noble-amd64.img`.
5. Nhấn `START` và chọn `Write in DD Image mode` nếu được hỏi.
6. Eject USB an toàn, tắt Secure Boot và boot từ USB.

Kiểm hash trên Windows trước khi ghi:

```powershell
certutil -hashfile dit-01-noble-amd64.img SHA256
```

Giá trị phải trùng dòng SHA-256 được `make rufus-image` in ra.

## 4A. Ghi USB trực tiếp trên Linux

Có thể chọn, ghi và đọc ngược kiểm SHA-256 bằng công cụ tích hợp:

```bash
make burn-usb
```

Xem quy trình an toàn và chế độ `--dry-run` tại [BURN_USB.md](BURN_USB.md).

## 5. Quan hệ với validation

Mỗi lần build lại có thể tạo checksum mới. Không dùng record validation của image
cũ cho image mới. Sau build:

1. ghi Image ID và SHA-256 mới vào validation status;
2. chạy `make qemu-smoke`;
3. ghi USB vật lý;
4. thực hiện protocol trong [VALIDATION_GUIDE.md](VALIDATION_GUIDE.md).

Các lệnh hỗ trợ khác:

```bash
make test
make validation-check
make host-check
make help
```

`make validation-check` chạy Rust quality gate và sáu negative fault gates; nó
không thay thế physical USB/hardware-matrix validation.

Trong vòng lặp phát triển hiện tại, dùng `make host-check` trước khi đóng image.
Target này chạy toàn bộ quality/negative gate cộng với reference route và một
CPU/RAM positive smoke ngắn trên host. Nó không build image, không mở QEMU và
không ghi USB. QEMU chỉ chạy khi người vận hành chủ động gọi `make qemu-smoke`.

## 6. Image dùng chung CPU–RAM + CPU–NVMe

Ubuntu rootfs, kernel, driver, GRUB, GPT layout, results partition và burn tool
vẫn chỉ do project này sở hữu. Project `/home/hoang/DIT-01` xuất một
extension payload có `MANIFEST.sha256`; image builder xác minh rồi stage payload
trước khi đóng SquashFS.

Người vận hành chủ động chạy:

```bash
cd /home/hoang/DIT-01
make task-f-check
make shared-image
```

Lệnh tương đương cũng chạy được ngay tại image owner:

```bash
cd /home/hoang/DIT-01
make shared-image
```

Hoặc gọi pipeline trực tiếp sau khi đã tạo extension:

```bash
DIT_EXTENSION_DIR=/home/hoang/DIT-01/dist/cpu-nvme-extension-root \
  make rufus-image
```

Image manifest và Image ID bao gồm fingerprint extension. Không được tái sử dụng
validation record của image CPU–RAM cũ cho image mới. Nếu không truyền
`DIT_EXTENSION_DIR`, builder xóa các file extension đã stage trong rootfs cache
và tạo đúng flavor CPU–RAM, tránh nhiễm module từ lần build trước.
