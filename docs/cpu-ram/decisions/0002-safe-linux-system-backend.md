# ADR-0002: Safe Linux system backend

- Status: accepted
- Date: 2026-08-28

## Context

Bước 2–3 cần cấp phát RAM theo dung lượng runtime, pin worker CPU và có optimized
SIMD path nhưng vẫn giữ code audit được bởi nhóm nhỏ. Backend phải chạy trước trên
Ubuntu Minimal/Linux x86_64 và không được phụ thuộc source corpus lớn bằng RAM.

## Decision

- Dùng nhiều anonymous mapping qua `memmap2`, giữ đồng thời tới hết phiên.
- Dùng Linux affinity qua safe API của `nix` và chỉ chọn CPU trong cpuset.
- Dùng `wide` cho safe SIMD reduction; scalar path là differential oracle.
- NUMA v1 dùng topology inventory và first-touch placement, đồng thời report rõ
  rằng physical placement chưa được chứng minh.
- Dùng source/seed/global offset để tái tạo expected byte tại chỗ.
- Từ chối target vượt effective available RAM trừ safety reserve.

## Consequences

Code v1 không cần tự viết `unsafe` block cho mmap, affinity hay SIMD. Cùng binary
có thể tăng/giảm arena theo RAM khả dụng và vẫn kiểm tail chính xác. Đổi lại, Linux
anonymous mapping chỉ chứng minh allocatable-memory coverage; validation physical
NUMA/DIMM cần cơ chế và phần cứng khác ở giai đoạn sau.
