# Tích hợp vào image DIT dùng chung

> Từ 2026-09-06, tài liệu này thuộc module trong project chung DIT-01. Dùng [README chung](../../README.md) và [status chung](../STATUS.md) cho lệnh build/trạng thái hợp nhất; các mốc bên dưới giữ lịch sử module.

## Quyết định kiến trúc

TASK-E không tạo Ubuntu Minimal rootfs, bootloader, partition layout hoặc công cụ burn riêng cho DIT CPU–NVMe. Image đang có của DIT CPU–RAM tại `/home/hoang/DIT-01` là nguồn duy nhất sở hữu pipeline image.

Quyết định này tái sử dụng hoàn toàn các thành phần đã có:

- Ubuntu Noble minimal và HWE kernel;
- `linux-firmware`, driver kernel và các gói sensor/hardware;
- `nvme-cli`, `smartmontools` và `pciutils`;
- initrd/live-boot, SquashFS read-only và lớp ghi tmpfs;
- chế độ `toram` để rút/giảm phụ thuộc vào USB khi đang test;
- GRUB cho UEFI/Legacy BIOS;
- raw GPT image dùng được với Rufus hoặc `dd`;
- phân vùng `DIT-BOOT`, `DIT-RESULTS` và cơ chế nhận dạng/protect boot-results disk;
- công cụ verify image, QEMU smoke và burn USB có xác nhận target;
- checksum/manifest của image.

Không sao chép hoặc fork các script tạo image sang repository NVMe. Mọi image kết hợp được build từ pipeline `/home/hoang/DIT-01`.

## Ranh giới module NVMe

Repository `/home/hoang/DIT-01` chỉ xuất một extension payload có phiên bản, gồm:

- binary release `cpu-nvme-dit`;
- cấu hình/profile mặc định;
- launcher hoặc service/UI hook cần thiết;
- module manifest ghi version, schema, dependency và SHA-256 của artifact;
- tài liệu vận hành và schema thư mục kết quả.

Engine NVMe vẫn chạy standalone trên Linux để phát triển và kiểm thử host. Việc dùng chung image không tạo phụ thuộc runtime giữa engine CPU–RAM và NVMe; điểm ghép vẫn là module/event contract có phiên bản.

## Luồng build TASK-E

```text
DIT-CPU-NVME-01
  -> automated gates + release binary
  -> extension payload + module manifest + SHA-256
  -> extension hook của pipeline CPU-DIT-01
  -> rootfs/SquashFS hiện có
  -> make rufus-image / verify-image / qemu-smoke
  -> raw GPT image dùng cho Rufus hoặc công cụ burn hiện có
```

Nếu pipeline CPU–RAM chưa có extension hook, TASK-E chỉ thêm một hook tổng quát, fail-closed để stage module payload trước khi đóng SquashFS. Không viết lại rootfs/bootstrap, GRUB, partitioning hoặc burn logic.

## Quy tắc an toàn tích hợp

- Giữ nguyên cơ chế nhận diện và bảo vệ USB boot/results của image CPU–RAM.
- Module NVMe không tự bắt đầu chế độ ghi sau boot; operator vẫn phải chọn đúng target và mode.
- Safety gate NVMe vẫn revalidate stable ID ngay trước khi mở target.
- Kết quả NVMe ghi trong namespace riêng, đề nghị `runs/<run-id>/cpu-nvme/`, trên `DIT-RESULTS`; không thay đổi layout vật lý của image.
- Manifest cuối ghi hash/version của base image và từng module để report có thể truy vết chính xác.
- CPU–SSD chạy độc lập, không tự chạy CPU–RAM preflight. Evidence CPU–RAM cũ chỉ được truyền khi CLI cung cấp tùy chọn; không có evidence thì report ghi `CPU_SSD_PATH_UNATTRIBUTED`.
- Một module thiếu, sai hash hoặc sai schema phải làm bước đóng image thất bại, không âm thầm tạo image thiếu thành phần.

## Gate nghiệm thu tích hợp

1. Không tồn tại image builder, GRUB config, partition script hoặc burn tool thứ hai trong repository NVMe.
2. Extension payload tái lập được và manifest/hash khớp.
3. Image dùng chung vẫn qua toàn bộ regression gate hiện có của CPU–RAM.
4. Sau boot, inventory nhận module NVMe và không tự mở ghi bất kỳ internal disk nào.
5. UEFI/Legacy smoke, nhận diện `DIT-RESULTS`, bảo vệ boot/results disk và burn verification vẫn đạt.
6. Report cuối truy vết được base-image version và NVMe module version.

## Trạng thái

Quyết định dùng image chung: `accepted` ngày 2026-09-05.

Extension hook/payload: `implemented` ngày 2026-09-05. Pipeline `/home/hoang/DIT-01` xác minh manifest rồi stage `cpu-nvme-dit` và `dit-suite`; boot profile `menu` ưu tiên shared control center khi extension hiện diện.

Build/boot shared image và validation trên USB/phần cứng thật vẫn `planned` trong TASK-E. Không build image hoặc chạy QEMU trong host gate TASK-F.
