# CPU-DIT-01 Session và Round Seed Rotation v1

## Mục tiêu

Seed rotation tăng pattern diversity giữa các lần boot mà vẫn giữ khả năng tái
hiện bit-exact. Randomness không thay oracle: mọi byte vẫn phải được tạo lại độc
lập và exact-compare; seed chỉ chọn một route dữ liệu khác trong recipe đã khóa.

## Semantics seed v1 (giữ nguyên trong schema v7)

`system-run` không còn dùng fixed seed mặc định. Khi không truyền `--seed`, CLI
lấy 64 bit từ Linux CSPRNG qua `getrandom()` và ghi:

```text
config.seed_hex                  session seed
config.seed_source               linux_random | operator_replay
config.seed_derivation           sha256:CPU-DIT-ROUND-SEED-v1
config.seed_commitment_sha256    binding của test/source/session seed
round_digests[].round_seed_hex   seed thật của từng complete round
round_seeds_unique               collision gate
```

Reference/golden route chạy trước system route vẫn dùng fixed certified seed.
Như vậy Safe/Full boot session có cả fixed regression evidence và random pattern
diversity.

Round seed được lấy từ 8 byte đầu của:

```text
SHA256("CPU-DIT-ROUND-SEED-v1" || 0x00 || test_id || 0x00
       || session_seed_le64 || round_index_le32)
```

Write và exact-readback của cùng round dùng cùng round seed. Global offset tiếp
tục domain-separate dữ liệu giữa arena/block; round khác phải có seed khác. PASS
yêu cầu `round_seeds_unique=true`.

Commitment được tính bằng:

```text
SHA256("CPU-DIT-SEED-COMMITMENT-v1" || 0x00 || test_id || 0x00
       || lowercase(source_sha256) || session_seed_le64)
```

Commitment gắn seed với test/source và cho validator phát hiện report bị ghép sai.
Nó không thay chữ ký số hoặc trusted timestamp.

## Replay

Report luôn chứa session seed. Có thể chạy lại đúng recipe bằng decimal hoặc hex:

```bash
dit-cli system-run --seed 0x7bc0239b31d734ed [các tham số profile cũ]
```

Run replay ghi `seed_source=operator_replay`. Diagnostic rerun nội bộ giữ nguyên
session seed, target và fault route. Hardware validation thông thường bắt buộc
`linux_random`; `operator_replay` dành cho regression/điều tra lỗi.

## UI và validation

Dashboard hiển thị `SESSION SEED`, `ROUND SEED` và `SOURCE`. Progress event phát
seed/commitment ngay từ `PREFLIGHT`, rồi cập nhật round seed ở RAM write/verify.

`tools/validate_session.py` schema v7 tự:

- tính lại commitment;
- dẫn xuất lại mọi round seed;
- kiểm round index, số evidence và tính duy nhất;
- từ chối positive validation session không dùng Linux random source.

## Evidence thiết bị 2026-08-29

Random run 64 MiB, 2 worker, 2 round và minimum 3 giây PASS với session seed
`0x7bc0239b31d734ed`. Hai round seed là `0x9ea085116157a55f` và
`0x84d7fa32bd6e9ddd`. Replay cùng session seed cho đúng cùng commitment, round
seed và round digest. Report:

- `build/seed-random-device-report.json`
- `build/seed-replay-device-report.json`
