# CPU-DIT-01 Reference Pipeline v1

## 1. Mục đích

Reference v1 là vertical slice correctness chạy trên Ubuntu phát triển trước khi
làm adaptive RAM engine. Nó chứng minh toàn bộ route và report format với payload
nhỏ, không nhằm đo performance hoặc kiểm tra gần hết RAM.

## 2. Artifact

- `crates/dit-core`: source verification, GF(256), pipeline, oracle và report.
- `crates/dit-cli`: lệnh chạy và ghi JSON report.
- `vectors/source-v1.dat`: source corpus nhỏ.
- `vectors/source-v1.manifest.json`: size/SHA-256 evidence.
- `vectors/golden-v1.json`: independent expected checkpoints.
- `tools/generate_golden.py`: Python reference tạo manifest/golden.

Tool Python không tham gia runtime. Rust pipeline không tự tạo expected golden của
chính nó.

## 3. Descriptor v1

| Field | Giá trị |
|---|---|
| Test ID | `CPU-DIT-01-REF-v1` |
| Seed | `0x0123456789abcdef` |
| Payload | 1,048,579 byte |
| Block recipe | 65,536; 131,071; 4,093 byte |
| Matrix | GF(256), primitive polynomial `0x11d`, Vandermonde 4×4 |
| Groups/blocks | 4 groups / 16 encoded blocks |
| Scatter | deterministic Fisher–Yates từ versioned seed domain |
| Reference safety limit | 64 MiB payload |

Payload không chia hết matrix group nên có đúng một padding byte. Report tách
`requested_payload_bytes` khỏi `allocated_unique_bytes`; padding được write/read/
exact-verify nhưng không được cộng vào recovered source coverage.

## 4. Oracle chain

```text
source manifest SHA-256
  → expanded payload golden SHA-256
  → GF(256) encoded golden SHA-256
  → byte-exact scattered readback
  → inverse matrix + byte-exact recover/join
  → versioned final SHA-256
```

Five checkpoints trong PASS report:

- `GENERATE`;
- `MATRIX_ENCODE`;
- `READBACK`;
- `RECOVER_JOIN`;
- `FINAL`.

## 5. Chạy và kiểm tra

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --workspace
target/release/dit-cli run
```

Report mặc định: `build/reference-report.json`.

Ví dụ cưỡng bức lỗi RAM readback:

```bash
target/release/dit-cli run \
  --inject-logical-block 0 \
  --inject-byte-offset 17 \
  --report build/injected-report.json
```

Kỳ vọng: status `FAIL`, exit code `2`, stage `READBACK`, có expected/actual/XOR.

## 6. Quality gates đã qua

- Rust source được format.
- Clippy toàn workspace/all targets với `-D warnings`.
- Workspace tests: 9 PASS.
- Source hỏng bị từ chối.
- CPU matrix encode fault không thể PASS.
- RAM readback fault không thể PASS.
- Recovered/join fault không thể PASS.
- Golden regeneration cho output byte-for-byte giống nhau.
- Release CLI PASS và JSON report parse hợp lệ.

Workspace cấm `unsafe` ở Bước 1.

## 7. Chưa được chứng minh

- Physical/allocatable RAM coverage.
- `mmap`, prefault, `mlock`, allocation backoff và OOM safety trên vùng lớn.
- Unique physical-page coverage.
- NUMA, multi-core, SIMD và cache coherence.
- ECC/MCE/thermal telemetry.
- USB boot và không ghi ổ nội bộ.
- Hardware validation.

Các mục này thuộc Bước 2–6 và không được suy ra từ PASS của reference v1.
