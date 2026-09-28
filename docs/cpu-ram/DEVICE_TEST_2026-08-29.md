# Device Test Bước 4 — 2026-08-29

## Kết luận

Release schema v2 PASS main run 512 MiB với live telemetry và không có safety
violation. RAM diagnostic, thermal, EDAC UE, worker crash và deadline gates đều
tạo FAIL/exit 2. Kết quả này là bounded implementation evidence, chưa phải ECC,
full-memory hoặc hardware-matrix validation.

## Thiết bị và điều kiện

- AMD Ryzen 7 3700X, 8 core/16 thread, một NUMA node;
- Linux nhận 8,200,310,784 byte RAM;
- trước bài chính còn khoảng 1,634,500,608 byte `MemAvailable`;
- swap gần đầy, nên giữ target 512 MiB và reserve 512 MiB.

## Main telemetry run

```bash
target/release/dit-cli system-run \
  --memory-mib 512 --reserve-mib 512 \
  --arena-mib 32 --minimum-arena-mib 4 \
  --rounds 4 --workers 16 --lock-policy best-effort \
  --coherency-iterations 250000 \
  --deadline-seconds 30 --monitor-interval-ms 100 \
  --report build/device-evidence/step4/system-512m-4r-telemetry.json
```

Kết quả:

| Evidence | Giá trị |
|---|---:|
| Status / elapsed | PASS / 3,932 ms |
| Allocated/locked/exact-verified unique | 536,870,912 byte |
| Tổng write/read | 2 GiB / 2 GiB |
| Worker/round | 16 / 4 trên 4 |
| Telemetry samples | 34 |
| Peak sensor | 67,250 millidegrees C |
| Safety stop/violation | false / none |
| Machine-check sysfs | 16 CPU, 448 bank attributes |
| Kernel event kể từ start | 0 dòng MCE/EDAC/thermal |

Sensor thực gồm `k10temp:Tctl/Tccd1`, ASUS EC, NVMe và Wi-Fi. EDAC memory-controller
counter và CPU thermal-throttle counter không được kernel xuất. Kernel journal
đọc được nhưng không có event trong cửa sổ bài test.

SHA-256 report:

```text
9708cae7280bda2abea71282eeabcec5523616325ba7e9483e86a6f77f9b8133
```

## Fault và safety campaign

| Bài | Kết quả | Evidence chính |
|---|---|---|
| RAM XOR tại offset 1,234,567 | FAIL | rerun cùng fingerprint, `reproducible_same_route` |
| Synthetic temperature 100°C | FAIL | stop tại policy 95°C |
| Synthetic EDAC UE +1 | FAIL | `corrected=0, uncorrected=1` |
| Synthetic worker crash 0 | FAIL | `RAM_WRITE`, worker/CPU 0, crash evidence |
| Deadline thật 1 giây | FAIL | stop sau 1/10 round, không coverage PASS |
| Bắt buộc EDAC trên máy này | FAIL | required telemetry unavailable |

Report hashes:

```text
1f0550428e704886e0f05b8ddd2faf7c773ec60fb760416f7eab4712a9aba7c2  ram-fault-diagnostic.json
3de87963349f0afd0a4d9e4fe68141a308e5c2e0889ab9c1b7933f0021859c3d  thermal-stop.json
70fcff9021d04975310cfbe9db11cb54bac6c9eecb379c76371903b3292894dd  edac-ue-stop.json
221bb6829824ad7c6b01348da372d8b503114477876ed665b75a3d9e9463df2f  worker-crash.json
bb016d86e208be5929d333379294ad6ce63f209fb6d7b4a4a33cd49b80a49c98  real-deadline-stop.json
74238a2253d7a2f21b8e6003caf2309509065da13df0d6e6a4622b70e0a234e6  required-edac-unavailable.json
```

## Quality gate

```text
cargo fmt --all -- --check                            PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace                                PASS (35 tests)
cargo build --release --workspace                     PASS
```

Chưa được xác minh trên thiết bị này: ECC event thật, MCE thật, throttle thật,
external hard watchdog, cross-NUMA, full-memory soak và USB boot minimal.
