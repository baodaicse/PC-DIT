# Ghi CPU-DIT image vào USB trên Linux

> Cảnh báo: công cụ này xóa toàn bộ dữ liệu trên USB được chọn. Không chọn ổ hệ
> điều hành, ổ dữ liệu hoặc một partition như `/dev/sda1`.

## Dùng tương tác

Sau khi `make image` hoàn tất:

```bash
cd /home/hoang/DIT-01
make burn-usb
```

Công cụ chỉ liệt kê whole-disk có transport `usb`, hiển thị model, serial, dung
lượng, partition và mountpoint. Sau khi chọn, phải nhập đúng chuỗi
`ERASE /dev/sdX`; nhập sai sẽ dừng mà không ghi.

## Kiểm tra trước mà không ghi

```bash
./tools/burn_usb.sh --list
./tools/burn_usb.sh --device /dev/sdX --dry-run
```

`--dry-run` kiểm SHA-256 image, loại thiết bị, transport, read-only flag, dung
lượng và đảm bảo USB không chứa `/`, `/home`, project hoặc chính image. Nó không
unmount và không ghi dữ liệu.

## Quy trình ghi và xác minh

Sau xác nhận, công cụ:

1. kiểm lại identity của USB để chống đổi thiết bị giữa lúc chọn và ghi;
2. unmount filesystem trên USB;
3. ghi raw image bằng `dd` và flush block cache;
4. đọc ngược đúng số byte của image;
5. chỉ báo `CPU-DIT USB BURN PASS` nếu SHA-256 đọc ngược trùng image nguồn.

USB có active swap hoặc không đủ dung lượng sẽ bị từ chối. Công cụ không bao giờ
tự chọn USB và không có chế độ bỏ qua confirmation/read-back verification.
