# ADR-0001: Rust cho reference core và control plane

- Status: Accepted
- Date: 2026-08-28

## Context

CPU-DIT-01 cần memory-safe control plane, deterministic reference implementation,
JSON report và khả năng cô lập phần code thấp tầng. Máy phát triển có Rust stable,
Cargo, rustfmt và Clippy thông qua rustup.

## Decision

Dùng Rust stable cho reference core, CLI, report và test. Workspace khóa toolchain
bằng `rust-toolchain.toml` và dependency bằng `Cargo.lock`.

Reference path không dùng SIMD intrinsic hoặc assembly. Mọi `unsafe` trong các
bước `mmap`, affinity hoặc SIMD sau này phải nằm trong module nhỏ, có invariant
được tài liệu hóa và được sanitizer/Miri hoặc test tương ứng kiểm tra khi khả dụng.

Python standard library được phép dùng làm implementation độc lập để tạo golden
vectors. Python không tham gia runtime DIT trên USB.

## Consequences

- Giảm nguy cơ lỗi pointer/lifetime của engine bị hiểu nhầm thành lỗi RAM.
- Cargo test, rustfmt và Clippy trở thành quality gate mặc định.
- Runtime reference có thể là một binary gọn; dependency được vendor hoặc đóng gói
  khi tạo live image ở Bước 5.
- SIMD/low-level optimization chỉ được thêm sau khi reference/golden gates pass.
