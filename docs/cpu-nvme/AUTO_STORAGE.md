# Tự chuẩn bị vùng test trên SSD được chọn

Trạng thái: implemented, chưa hardware-validated. Dùng lại image pipeline CPU-DIT-01.

## Luồng người dùng

1. Chọn `CPU + SSD`; CPU–RAM minimum preflight không chạy.
2. Chọn SSD `1`, `2` hoặc `A`. Selection này đồng thời là xác nhận bắt đầu; duration lấy từ cấu hình launcher, mặc định 30 phút.
3. App tự chọn **filesystem dữ liệu hiện có lớn nhất hợp lệ trên mỗi SSD**, rồi tái kiểm tra namespace/partition stable ID và filesystem UUID/type. Đây không phải chọn vùng LBA hay tạo partition mới.
4. Reuse mount dữ liệu hợp lệ dưới `/mnt` hoặc `/media`; nếu chưa mount, app tạo mountpoint riêng `/run/cpu-nvme-dit-<session>-<alias>` và mount `rw,nosuid,nodev,noexec,noatime`.
5. Kiểm tra free space rồi tạo thư mục mới `.cpu-nvme-dit-<session>-<alias>` và file test mới bên trong. Engine kiểm tra mapping trực tiếp lại trước khi chạy các SSD.
6. Đóng/xóa file test theo engine; chỉ xóa thư mục rỗng của phiên, unmount mount của phiên và xóa mountpoint rỗng. Mount có sẵn của operator được giữ nguyên.

File test và telemetry là hai đường ghi tách biệt: payload Deep nằm trên volume thuộc SSD đã chọn; `events.jsonl`, selection và report đang chạy được staging dưới `/run/dit-suite` (tmpfs). Chỉ sau khi engine dừng, orchestrator mới copy bundle report đã đóng một lần sang `DIT-RESULTS` của USB đã xác thực. Boot partition, result partition và parent device được truyền vào danh sách `--protected-device`; nếu result volume không mount/xác thực được, không có fallback qua thư mục mount rỗng hoặc ổ khác.

Luồng UI không còn màn hình plan, prompt duration hay câu `RUN PRESERVING`. Plan kỹ thuật vẫn được lưu vào `auto-storage-plan.json`; rút gọn UI không bỏ bất kỳ stable-ID, boot/results/system disk, filesystem, free-space hoặc cleanup guard nào.

Nếu một SSD chuẩn bị thất bại, không bắt đầu engine trên những SSD còn lại; dọn các vùng đã chuẩn bị theo ownership. Không tự chuyển qua phân vùng khác sau xác nhận. Nếu volume lớn nhất thiếu free space sau mount, app dừng rõ lý do; chưa có vòng thử volume kế tiếp.

## Phạm vi và bảo vệ

- Hỗ trợ tự chuẩn bị filesystem ext4, NTFS (kernel ntfs3), exFAT. Kernel/filesystem phải hỗ trợ direct I/O; nếu không, engine báo lỗi, không fallback buffered I/O.
- NTFS chưa mount phải có `ntfs-3g.probe --readwrite` thành công. Image builder đã thêm package `ntfs-3g` để cung cấp probe; không dùng FUSE để chạy test.
- Dirty/hibernated NTFS bị từ chối; không `force`, `remove_hiberfile`, repair/fsck. Windows phải shutdown hoàn toàn. Xem [Tuxera probe exit codes](https://raw.githubusercontent.com/tuxera/ntfs-3g/edge/src/ntfs-3g.probe.8.in) và [NTFS3 mount options](https://cdn.kernel.org/doc/html/latest/filesystems/ntfs3.html).
- Chặn SSD đang chứa hệ điều hành đang chạy, swap, block holders, mount hệ thống/boot/results, explicit protected devices và label DIT-BOOT/DIT-RESULTS. Chỉ whitelist partition dữ liệu GPT/MBR; không chọn EFI/MSR/Recovery.
- Không format, chia lại ổ, unlock mã hóa, remount ổ read-only hay tự bật raw write. SSD trống/chưa format, BitLocker/LUKS/LVM/RAID và filesystem khác sẽ báo lý do và dừng.
- Free reserve mặc định 4096 MiB; yêu cầu dung lượng trống tối thiểu `min(capacity, write budget) + reserve`. Free space chỉ biết chắc sau mount; không bảo đảm chống tiến trình khác đồng thời lấp đầy filesystem.
- Mount ghi/tạo file có thể cập nhật metadata và journal. Không bảo đảm dữ liệu tuyệt đối khi thiết bị thật lỗi; backup dữ liệu quan trọng trước khi stress test.
- Khi SIGINT/SIGKILL/mất điện kết thúc tiến trình trước cleanup, mount/thư mục/file có thể còn lại. Chưa có recovery tự động sau crash; kiểm tra log, chắc chắn process đã dừng và xác minh đúng file phiên trước khi dọn. Không xóa theo wildcard.

## Khi máy treo giữa lúc test (NTFS mã 13/15) và Format nhanh

Test mount NTFS bằng driver `ntfs3`. Lúc mount, driver đánh dấu volume đang dùng và sửa `$MFT`, nhưng bản sao
`$MFTMirr` chỉ được ghi khi sync hoặc unmount. Máy treo giữa hai lúc đó (điều có thể xảy ra khi test một máy yếu)
để lại `$MFTMirr does not match $MFT (record 0)`, và `ntfs-3g.probe` trả mã 13. Tái hiện trên loop image: chép
ảnh đĩa khi volume còn mount (đúng thứ máy treo để lại) cho mã 13; cùng thao tác sau `syncfs` cho mã 0.

| Biện pháp | Chi tiết |
|---|---|
| Sync ngay sau khi chuẩn bị | `syncfs` sau khi mount và tạo thư mục test; file test mới được `fsync` ngay sau khi tạo |
| Sync định kỳ | `SyncKeeper`: `syncfs` 10 giây một lần trong suốt test (CPU-SSD đơn lẻ và holder của COMBINED, gồm cả file của chain), và một lần khi dừng |
| Sửa bằng `ntfsfix` | Khi bị từ chối với mã 13 kèm `$MFTMirr does not match $MFT`, hoặc mã 15, màn hình hỏi `F = repair and continue`. Chỉ chạy khi người dùng chọn: `ntfsfix -d` (chép `$MFT` sang `$MFTMirr`, xoá journal, xoá cờ dirty), kiểm lại bằng probe rồi chuẩn bị lại. Không bao giờ đề nghị cho mã 14 (Windows ngủ đông). Không phải `chkdsk`: ổ có dữ liệu cần giữ thì chạy `chkdsk /f` trong Windows |
| Dọn file của phiên bị treo | Sau khi mount: thư mục `.cpu-nvme-dit-*` ở gốc volume, sửa lần cuối **trước lần boot hiện tại**, trên đúng volume đó: chỉ xoá file của tool (`.cpu-nvme-dit-*.test`, `dit-chain-*.bin/.json`) rồi thư mục nếu đã rỗng; mọi thứ khác giữ nguyên. Màn hình in những gì đã xoá |
| Thông báo mã 13/15 | Nói rõ nguyên nhân thường gặp (test bị ngắt), rằng SSD chưa chắc hỏng, và ba cách xử lý |

**Format nhanh (phím F ở ô chọn SSD, cả CPU-SSD đơn lẻ và COMBINED):** xoá toàn bộ ổ, tạo GPT một phân vùng, format
nhanh NTFS (mặc định, Windows đọc được) hoặc ext4, nhãn `SSD-TEST`. Từ chối ổ hệ thống/boot/kết quả DIT, ổ đang mount,
có swap/holder, chỉ đọc hoặc không có serial ổn định. Phải gõ đúng 4 ký tự cuối số serial của ổ mới chạy; gõ khác là
huỷ. Lệnh: `wipefs` (từng phân vùng, rồi cả ổ), `sfdisk --wipe always --wipe-partitions always`, `mkntfs --quick` hoặc
`mkfs.ext4 -E nodiscard,lazy_itable_init=1`. Trên loop disk 2 GiB có phân vùng NTFS cũ: 0,6 giây, probe mã 0.

## Bằng chứng và kiểm thử

Trong bundle kết quả cuối: `selection.json`, `auto-storage-plan.json` lưu target ID/UUID/filesystem/mount dự kiến; `auto-storage-outcome.json` lưu giai đoạn lỗi và kết quả cleanup khi tiến trình trả về bình thường. `report.json` vẫn là báo cáo engine, không thay thế bằng chứng setup/cleanup. Lỗi cleanup trả exit không thành công, kể cả khi engine đã PASS. `events.jsonl` được ghi ở tmpfs trong lúc test và chỉ copy sau khi file đã đóng.

Preview chỉ đọc (không cần CPU–RAM preflight, không mount hoặc ghi file):

```bash
cd /home/hoang/DIT-01
cargo run -p nvme-cli -- auto-plan --devices all --no-smart
```

Kiểm thử phần mềm: `make task-f-check`. Không image build/QEMU/ghi block device. Hardware gate còn phải chạy trên SSD có backup: ext4, NTFS shutdown sạch, NTFS hibernated/dirty phải bị chặn, exFAT direct I/O, thiếu free space, 2 SSD cùng lúc, mount sẵn, lỗi setup ổ thứ hai và cleanup sau lỗi. Các ca mount/unmount thật chưa được gọi là validated bằng unit tests.

Đóng image mới bằng `make shared-image` từ project này hoặc CPU-DIT-01. USB/image cũ không tự nhận binary mới.
