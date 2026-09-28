# Device Test — 2026-08-28

## Kết luận

Release binary của Bước 2–3 PASS bài functional load 512 MiB trên thiết bị phát
triển. Hai negative test độc lập cho RAM byte và SIMD worker đều tạo FAIL đúng
oracle. Kết quả này không được suy rộng thành full-RAM hoặc platform validation.

## Thiết bị

| Thuộc tính | Giá trị |
|---|---:|
| CPU | AMD Ryzen 7 3700X 8-Core Processor |
| Core/thread | 8/16 |
| Kiến trúc/features | x86_64; AVX2, AES, FMA, SSE4.2 |
| NUMA | 1 node, CPU 0–15 |
| RAM Linux nhận | 8,200,310,784 byte |
| Kernel | 7.0.0-30-generic |

Ngay trước main run, `MemAvailable` là 1,731,465,216 byte và swap chỉ còn 159,744
byte trống. Do đó test dùng 512 MiB với reserve 512 MiB. Không chạy gần 8 GB vì
phần lớn RAM đang được tiến trình khác sử dụng; cố chiếm hết có nguy cơ OOM và
không tạo bằng chứng tốt hơn.

## Main run

```bash
target/release/dit-cli system-run \
  --memory-mib 512 --reserve-mib 512 \
  --arena-mib 32 --minimum-arena-mib 4 \
  --rounds 4 --workers 16 \
  --lock-policy best-effort \
  --coherency-iterations 250000 \
  --report build/device-evidence/system-512m-4r.json
```

Kết quả:

| Evidence | Giá trị |
|---|---:|
| Status / elapsed | PASS / 3,584 ms |
| Arena | 16 × 32 MiB |
| Allocated/locked/exact-verified unique | 536,870,912 byte |
| Tổng write/read | 2,147,483,648 / 2,147,483,648 byte |
| Worker/round | 16 / 4 trên 4 |
| Affinity failure | 0 |
| Coherency | 4,000,000 trên 4,000,000 |
| Scalar/SIMD, integer, SHA-256, FP/FMA | PASS |
| Mismatch | none |

SHA-256 của JSON report:

```text
13e1335caa9e1898da576ccead6f7b83bb61a1f031b26e0f22d3122f15463658
```

## Adaptive percentage run

Nhánh `--available-percent 5` tính target từ 1,685,336,064 byte effective
available thành 84,266,803 byte. Đây là kích thước lẻ, không tròn page hoặc arena.
Engine tạo 6 arena, giữ và exact-verify đủ 84,266,803 byte qua 2 vòng; status PASS.
Số worker thực dùng là 6 vì worker count không vượt số arena.

Report: `build/device-evidence/system-percent-5.json`, SHA-256:

```text
d284494e85108e27995a38d439cb2385d04fb34dd3f7294aadc3e600a617e48b
```

## Negative test: RAM mismatch

64 MiB được ghi đúng rồi byte tại global offset `1,234,567` bị đảo XOR 1. CLI
trả exit code 2. First mismatch là `RAM_EXACT_VERIFY`, round 0, arena 0; expected
132, actual 133, writer CPU 0 và verifier CPU 1.

Report: `build/device-evidence/system-fault-64m.json`, SHA-256:

```text
126b2ce735a555b5c6a9c57c1ab2afb06c38e78326fb65b4e48e46fd9a276c1e
```

## Negative test: SIMD mismatch

64 MiB vẫn exact-verify đầy đủ, nhưng SIMD result của worker 0 bị sửa. Scalar
oracle phát hiện differential mismatch, status FAIL và exit code 2.

Report: `build/device-evidence/system-simd-fault-64m.json`, SHA-256:

```text
03244eb8320f7211d331079b3b44ba3d13042be2fb2165a1af6dc87c4ccdf533
```

## Quality gate

```text
cargo fmt --all -- --check                            PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                                PASS (22 tests)
cargo build --release --workspace                     PASS
```

Phần chưa được test trên thiết bị này: cross-NUMA, ECC/EDAC/MCE, thermal/throttle,
RAM 16/32/64/128 GB, full-memory soak và USB boot minimal.
