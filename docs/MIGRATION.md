# Hợp nhất ngày 2026-09-06

Nguồn: `/home/hoang/CPU-DIT-01` và `/home/hoang/DIT-CPU-NVME-01`.
Project hiện hành: `/home/hoang/DIT-01`.

- Ba crates CPU–RAM và 11 crates NVMe/shared nằm trong `crates/`; không workspace lồng nhau. Dependencies hợp nhất, Cargo bổ sung dependencies thiếu offline vào lockfile CPU–RAM.
- Image/rootfs/boot/tools CPU–RAM làm nền; payload NVMe được đóng local và luôn cài vào image chung.
- Docs chuyển vào `docs/cpu-ram` và `docs/cpu-nvme`, giữ nội dung lịch sử; README/roadmap/status chung ưu tiên cho cách build hiện hành.
- Sao chép `vectors`, `validation`, `Result` và các report/log lịch sử trong `build`, không sửa bytes/report/seed/golden cũ.
- Cache `build/boot-image` (~1.5 GiB) được copy độc lập bằng `cp -a --reflink=auto`, không symlink/hardlink sang nguồn. Không chroot/mount/build image khi gộp.
- Không copy `target/`: compile lại trong workspace mới. Không gắn nhãn mới cho image cũ; output mới là `dist/dit-01-noble-amd64.img` sau `make shared-image`.
- Giữ nguyên report ABI, test IDs, seed derivation labels, installed module paths và filesystem labels DIT-BOOT/DIT-RESULTS để tương thích.
- Không sửa HardwareValidator, không xóa hai project nguồn, không test SSD/ghi USB/QEMU hoặc stress toàn RAM.

`make shared-image` và `make rufus-image` cùng một luồng local. `make image` cũng tự đóng NVMe payload. Không cần `DIT_EXTENSION_DIR`/`NVME_PROJECT` nữa.

Từ nay chỉ phát triển trong DIT-01, không sửa song song ba nơi. Báo cáo lịch sử có thể giữ tên/path cũ, không phải dependency build. Hai bản nguồn được giữ để đối chiếu/quay lại.
