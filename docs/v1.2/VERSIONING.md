# Đánh số version DIT-01 (từ v1.2)

## Dạng

`v<SERIES>.<n>`, với n có ba chữ số: `v1.2.000`, `v1.2.001`, ... Hiện tại `SERIES` là `1.2`.

- **n tự tăng sau mỗi commit**, không ai phải sửa tay. n bằng số commit từ commit đầu tiên của dòng
  1.2. Commit đầu tiên đó là `v1.2.000`.
- **Build từ cây có thay đổi chưa commit** hiện số mà commit kế tiếp sẽ nhận, kèm `-dev`. Ví dụ
  `v1.2.004-dev` là bản `v1.2.003` cộng các thay đổi sẽ thành `v1.2.004`. Image dùng thật ngoài
  hiện trường nên build từ cây đã commit, để không còn `-dev`.
- Bản sao mã nguồn không có git hiện `v1.2.unknown`. Checkout từ trước dòng 1.2 hiện
  `v1.2.000-pre`.

## Cơ chế

| Thành phần | Vai trò |
|---|---|
| `VERSION` | `SERIES=1.2` và `FIRST=52`: số commit (`git rev-list --count HEAD`) của commit đầu tiên của dòng |
| `tools/dit-version.sh` | Nguồn duy nhất của quy tắc: in `1.2.<n>` hoặc `1.2.<n>-dev` |
| `crates/dit-ui-core/build.rs` | Đưa version vào `dit_ui_core::DIT_VERSION`. Dùng `DIT_VERSION` trong môi trường nếu có, không có thì tự gọi script |
| `tools/build_boot_image.sh`, `tools/build_image_extension.sh` | Tính version một lần cho cả lần build, export `DIT_VERSION` cho cargo, ghi vào image |

Commit hoặc checkout làm cargo build lại `dit-ui-core` (theo dõi `.git/HEAD`, `.git/index`,
`.git/logs/HEAD`). Riêng trạng thái `-dev` của một lần `cargo build` thủ công có thể cũ hơn thực tế
nếu chỉ sửa file mà chưa stage. Build image luôn đúng vì script truyền `DIT_VERSION` mới tính.

## Hiện ở đâu

- Tiêu đề mọi dashboard: `DIT-01 / DATA INTEGRITY TEST ENGINE / v1.2.xxx`. Gồm CPU-RAM, CPU-SSD,
  GPU-VRAM, combined và màn hình hộp đen.
- Dòng ENGINE của CPU-RAM, control center (`DIT CONTROL CENTER v1.2.xxx // ...`), rescue console.
- Dòng đầu menu GRUB: `v1.2.xxx | Build <giờ build> | <image-id>`.
- Trong image: `/etc/cpu-dit/version`, `dit_version=` trong `/etc/cpu-dit/build-info`,
  `"dit_version"` trong manifest của `.img` và ISO.
- Version của các crate trong Cargo (`version.workspace`) là `1.2.0`. Nó chỉ đổi khi sang dòng mới;
  số đầy đủ nằm ở `DIT_VERSION`.

## Sang dòng mới

Khi bắt đầu dòng mới (ví dụ 1.3): sửa `SERIES=1.3`, đặt `FIRST` bằng `git rev-list --count HEAD`
cộng 1, rồi commit thay đổi đó. Commit này là `v1.3.000`. Đồng thời đổi `version` trong
`Cargo.toml` thành `1.3.0`.

## Lưu ý

n đếm theo lịch sử commit của nhánh đang build. Nếu viết lại lịch sử (rebase, squash) thì số có thể
đổi. Nhánh merge vào `main` vẫn tăng đều, vì commit merge cũng được đếm.
