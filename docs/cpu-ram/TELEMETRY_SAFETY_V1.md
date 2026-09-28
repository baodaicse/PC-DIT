# CPU-DIT-01 Telemetry, Safety và Diagnostic v2

## Phạm vi

Bước 4 bổ sung ba lớp vào `system-run`:

1. telemetry trước, trong và sau tải;
2. versioned safety policy cùng cooperative stop;
3. diagnostic rerun giữ nguyên recipe và phân loại evidence.

Report hiện dùng schema v7 và chứa source ID/hash, session/round seed,
seed commitment, memory/worker config, telemetry, safety, execution và diagnostic evidence.

## Nguồn telemetry Linux

| Evidence | Nguồn ưu tiên |
|---|---|
| Nhiệt độ | `/sys/class/hwmon`, `/sys/class/thermal` |
| CPU package power | hwmon `power*_input`/`power*_average`; fallback `powercap/*/energy_uj` delta |
| ECC/EDAC | `/sys/devices/system/edac/mc/mc*/ce_count`, `ue_count` |
| Throttle | `cpu*/thermal_throttle/*_throttle_count` |
| OOM | cgroup v2 `memory.events` |
| Machine-check capability | `/sys/devices/system/machinecheck` |
| MCE/RAS/thermal kernel event | `journalctl -k` từ timestamp bắt đầu |

Collector không thay nguồn thiếu bằng số 0. Availability và lý do thiếu được ghi
riêng. Kernel journal chỉ được phân loại/đếm dòng có marker MCE, hardware error,
EDAC/ECC hoặc thermal/throttle; nội dung log không được chép vào report.

Powercap cung cấp năng lượng tích lũy theo microjoule, nên collector lấy hai mẫu
theo đồng hồ monotonic và tính milliwatt từ `delta energy / delta time`. Nó xử lý
counter wrap theo `max_energy_range_uj`, loại trùng symlink sysfs và chỉ ưu tiên
package/CPU zone. `dit-cli sensors --json` giữ source path, label, measurement và
sample interval để người vận hành kiểm chứng cách tính.

## Safety policy v2

Policy ID: `CPU-DIT-01-SAFETY-v2`.

Mặc định:

- deadline 300,000 ms, sample interval 250 ms;
- temperature telemetry luôn bật; thermal limit mặc định OFF;
- khi operator bật, thermal stop tại 95,000 millidegrees C hoặc critical limit
  từ trusted CPU sensor; AUXTIN/Super-I/O generic chỉ là evidence;
- fail khi EDAC uncorrected delta tăng;
- fail khi kernel journal có machine-check/hardware/EDAC event mới;
- fail khi cgroup OOM/OOM-kill tăng;
- throttle delta được ghi nhưng chỉ fail với `--fail-on-throttle`;
- sensor nhiệt và EDAC không bắt buộc nếu platform không cung cấp.

`--require-temperature-sensor` và `--require-edac-counters` biến availability
thành gate. Thiếu nguồn bắt buộc tạo FAIL trước khi RAM load bắt đầu.

Monitor chạy song song. Worker kiểm stop token định kỳ trong vòng fill và exact
verify; arena chưa hoàn thành không được cộng unique coverage. Deadline v1 là
cooperative in-process, vì vậy không thay thế hard watchdog bên ngoài khi cả
process hoặc kernel bị treo.

## Crash và timeout evidence

Panic trong RAM writer/verifier được bắt tại worker boundary khi còn giữ arena,
ghi phase/round/worker/CPU/message rồi yêu cầu dừng. Synthetic worker-crash fault
kiểm chứng rằng crash không thể tạo PASS. Infrastructure failure xảy ra ngoài
boundary vẫn có thể làm process mất report; hard supervisor thuộc Bước 5.

## Diagnostic rerun

Khi primary correctness run FAIL và không dùng `--no-diagnostic-rerun`, engine chạy lại với
cùng source, seed, target, arena recipe, workers, policy và fault configuration.
Thermal abort không tự chạy lại workload và được ghi
`skipped_temperature_safety_abort`.
Fingerprint ưu tiên stage/round/global offset/expected/actual/XOR, sau đó tới
scalar/SIMD, worker crash, abort event hoặc failure reason.

Classification chỉ có ý nghĩa route/system:

- `reproducible_same_route`: cùng fingerprint xuất hiện lại;
- `not_reproduced_transient_or_unattributed`: rerun PASS;
- `different_failure_unattributed`: rerun FAIL theo fingerprint khác;
- `rerun_invalid_unattributed`: không thể thực hiện rerun;
- `disabled` hoặc `not_required`.

Không classification nào tự quy lỗi cho DIMM, CPU hay mainboard.

## CLI

```bash
target/release/dit-cli system-run \
  --memory-mib 512 --rounds 4 --workers 0 \
  --deadline-seconds 300 --monitor-interval-ms 250 \
  --enable-temperature-limit --max-temperature-c 95 \
  --edac-policy fail-uncorrected
```

Fault gates:

```bash
--inject-temperature-c 100
--inject-edac-uncorrected
--inject-timeout
--inject-worker-crash 0
```

Chỉ một fault injection được phép trong một CLI run. Diagnostic rerun mặc định
bật cho correctness FAIL và có thể tắt bằng `--no-diagnostic-rerun`.

Temperature limit và cách UI toggle được mô tả tại
[TEMPERATURE_POLICY_V2.md](TEMPERATURE_POLICY_V2.md).

## Giới hạn

- Availability phụ thuộc driver, kernel config và quyền của runner.
- Kernel-log classifier là evidence sàng lọc, không phải DIMM attribution.
- Máy không có ECC/EDAC counter không thể được tuyên bố đã kiểm chứng ECC thật.
- Live runner đã có hard process watchdog và USB-only artifact path. Kernel hoặc
  firmware hard hang vẫn cần platform watchdog/power control bên ngoài.
