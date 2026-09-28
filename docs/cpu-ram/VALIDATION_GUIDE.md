# CPU-DIT-01 Validation Guide v1

## 1. “Validated” có nghĩa gì

CPU-DIT không dùng một nhãn `validated` chung chung. Có ba cấp riêng:

| Cấp | Ý nghĩa |
|---|---|
| `SESSION_PASS` | Một session Safe/Full cụ thể đạt mọi gate trong JSON report. |
| `CONFIG_VALIDATED` | Đúng CPU/mainboard/BIOS/RAM topology đó đã hoàn thành toàn bộ protocol bên dưới. |
| `RELEASE_VALIDATED` | Đúng Image ID/SHA-256 đó đã đạt mọi dòng bắt buộc trong `validation/MATRIX.csv`. |

Không được suy rộng từ một Ryzen 32 GB sang mọi Ryzen 32 GB, từ QEMU sang máy
thật, hoặc từ một release image sang image mới. Rebuild làm đổi Image ID/hash và
release mới phải chạy lại gate tối thiểu.

Target hiện tại là:

```text
image_id  CPU-DIT-01-NOBLE-20260901-843d7743c91a
sha256    115c3afe2318285ef633827e725f33cbeb3916b72f8bc537b1a4d69916205395
```

Trạng thái thật được ghi tại
[`validation/VALIDATION_STATUS.md`](../validation/VALIDATION_STATUS.md). File này
ban đầu là `NOT_VALIDATED`; không đổi bằng cảm nhận thủ công.

## 2. Chuẩn bị

### Thiết bị cần có

- USB từ 4 GB, ưu tiên USB tốt và một USB dự phòng.
- Máy test có nguồn điện, tản nhiệt và quạt hoạt động ổn định.
- Bàn phím/màn hình; với server nên lưu thêm log serial/IPMI.
- Một máy khác để chép và kiểm report sau mỗi nhóm chạy.
- Với soak không có temperature sensor: thiết bị theo dõi nhiệt độc lập.

### An toàn trước khi chạy

1. Sao lưu dữ liệu quan trọng. DIT không mount ổ nội bộ nhưng bài test vẫn gây
   tải CPU/RAM cao và không thay thế quy trình backup.
2. Chạy baseline ở BIOS default, không overclock/undervolt/XMP/EXPO thủ công.
   Profile tuning phải là một config validation riêng.
3. Ghi BIOS/UEFI version, CPU, mainboard, từng DIMM, slot, speed, ECC, PSU và
   điều kiện tản nhiệt vào mẫu biên bản.
4. Xác minh quạt quay, heatsink lắp đúng; lượt Safe đầu phải có người giám sát.
5. Tắt Secure Boot cho image v1 unsigned.

## 3. Khóa image và tạo USB

Trên Linux:

```bash
cd /home/hoang/DIT-01/dist
sha256sum --check dit-01-noble-amd64.img.sha256
```

Trên Windows:

```powershell
certutil -hashfile dit-01-noble-amd64.img SHA256
```

Hash phải đúng giá trị ở mục 1. Ghi toàn bộ `.img` bằng Rufus DD/raw hoặc `dd`,
không copy file riêng vào USB. Sau khi ghi, boot USB và xác nhận Image ID trong
session report. Nếu image được build lại, dừng protocol và tạo dòng release mới.

## 4. Protocol bắt buộc cho mỗi cấu hình

### P0 — Inventory và boot isolation

1. Boot UI từ USB ở UEFI hoặc BIOS mode đúng dòng matrix.
2. Vào `Sensor Info`, chụp màn hình CPU/RAM/nhiệt/Watt và driver evidence.
3. Vào `Diagnostic`, chạy `findmnt` và `swapon --show`.
4. Gate: root là live SquashFS/overlay, swap trống, không partition nội bộ nào
   được CPU-DIT mount; chỉ `DIT-BOOT` và `DIT-RESULTS` thuộc cùng USB được dùng.
5. Lưu `preflight.txt`, `sensor-report.json`, `driver-probe.txt` và
   `libsensors.txt`. Power `N/A` được chấp nhận; ghi rõ availability.
6. Ghi rõ `temperature_limit_enabled`. Mặc định release là OFF; nếu matrix yêu
   cầu auto-abort, nhấn `T` bật ON và xác nhận chỉ trusted CPU sensor được xét.

### P1 — Safe functional run

1. Để máy nguội và boot mới hoàn toàn.
2. Chọn `Safe test` một lần.
3. Gate: UI báo PASS; `reference_exit=0`, `system_exit=0`, `overall_exit=0`;
   reference checkpoints đều match; system exact-verify đúng 256 MiB, ít nhất
   2 round và ít nhất 2 phút.
4. System report phải là schema v7, `seed_source=linux_random`, commitment hợp lệ,
   mọi `round_seed_hex` đúng công thức, không trùng và đủ số complete round; mỗi
   round đủ mandatory window/pattern recipe, không gap/overlap và verified-size
   sum đúng RAM target.
5. Không được có mismatch, worker crash/error, OOM, EDAC UE, kernel hardware event
   hoặc safety violation.

### P2 — Full coverage repeatability

1. Chọn `Full test` ba lần liên tiếp: một lượt sau cold boot, hai lượt warm.
2. Reboot giữa lượt cold và warm đầu; giữ nguyên BIOS và DIMM topology.
3. Gate cho cả ba lượt:
   - `status=PASS`, ít nhất 4 complete round và 30 phút;
   - `coverage_target_met=true`;
   - `requested = allocated = written = read = exact_verified_unique_bytes`;
   - target đúng 95% của `MemAvailable - 512 MiB reserve` tại đầu session;
   - mọi worker có affinity failure và scalar/SIMD mismatch bằng 0;
   - ba session có session seed khác nhau; mọi round seed dẫn xuất đúng và không trùng;
   - không safety/execution violation.

Target mỗi lượt có thể chênh nhẹ vì `MemAvailable` thay đổi; đây không phải lỗi.
Không được lấy `total_read/write_bytes` thay cho unique coverage.

### P3 — Soak nóng

1. Lặp Full liên tục ít nhất 8 giờ và tối thiểu 3 session PASS; dùng mốc nào dài
   hơn. Đây là policy CPU-DIT-01 v1, không phải tuyên bố chuẩn ngành phổ quát.
2. Lưu mọi session, nhiệt độ peak và thời gian bắt đầu/kết thúc.
3. Temperature sensor phải khả dụng. Nếu không, hồ sơ phải có thiết bị đo độc lập,
   vị trí đo, ngưỡng dừng và người giám sát.
4. Dừng ngay nếu có mùi bất thường, quạt lỗi, nhiệt ngoài policy hoặc máy tự reset.
5. Gate: không session FAIL/INVALID, không unexpected reboot/hang, EDAC CE/UE delta
   đều bằng 0 đối với release validation. CE tăng dù engine policy chỉ quan sát
   vẫn làm config chưa đạt cho tới khi điều tra xong.

### P4 — Power-cycle và persistence

1. Poweroff từ UI, chờ tắt hoàn toàn, cold boot lại ít nhất 3 lần.
2. Xác nhận UI/USB boot ổn định và chạy Safe ở lần boot cuối.
3. Cắm USB vào máy quản trị; mọi session phải tồn tại dưới
   `DIT-RESULTS/runs/<session-id>` và đọc được.
4. Gate: không report mất/truncated, Image ID nhất quán, FAT không báo lỗi.

### P5 — Negative/fault campaign cho release

Campaign này bắt buộc một lần trên AMD và một lần trên Intel cho mỗi release,
không cần lặp trên mọi dung lượng RAM. Chạy trên môi trường development hoặc live
shell với target 64 MiB. Các fault sau phải trả exit code 2 và không bao giờ PASS:

```text
RAM global-offset fault
SIMD worker fault
worker crash
deadline timeout
synthetic over-temperature
synthetic EDAC uncorrected event
```

Corrupt source/manifest phải trả `INVALID_SOURCE`, không quy lỗi cho CPU/RAM.
Automated tests cũng phải PASS trước campaign:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --workspace
```

Sau quality gate, chạy campaign tự động:

```bash
./tools/run_negative_campaign.sh
```

Kết quả bắt buộc là `NEGATIVE_CAMPAIGN_PASS`. JSON, console log và exit code của
từng fault được lưu ở `build/validation-negative-campaign/`. Có thể truyền binary
và output directory riêng ở argument 1 và 2.

Không tạo fault nhiệt vật lý và không tháo DIMM khi máy đang chạy; temperature và
EDAC negative gate dùng injection của phần mềm.

### P6 — Kiểm tra report tự động

Sau khi copy một session từ USB sang máy phát triển:

```bash
python3 tools/validate_session.py \
  /duong-dan/toi/session-id \
  --manifest dist/dit-01-noble-amd64.img.manifest.json
```

Tool chỉ cấp `SESSION_PASS`; nó không tự cấp `CONFIG_VALIDATED`. Biên bản cấu hình
vẫn phải có đủ P0–P5, soak và chữ ký review.

## 5. Ma trận để cấp RELEASE_VALIDATED v1

Mọi dòng `required=yes` trong [`validation/MATRIX.csv`](../validation/MATRIX.csv)
phải là `CONFIG_VALIDATED`. Ma trận v1 tối thiểu bao phủ:

- Intel và AMD;
- non-ECC 16, 32 và 64 GB trên hai vendor;
- ECC 64 GB và ECC 128 GB;
- một hệ thống multi-NUMA;
- UEFI vật lý; một máy legacy BIOS nếu release tiếp tục công bố hỗ trợ BIOS;
- CPU power available trên ít nhất một Intel và một AMD; `N/A` vẫn hợp lệ trên
  config khác;
- temperature sensor hoặc giám sát nhiệt độc lập cho mọi soak.

Nếu lab chưa có một lớp phần cứng, giữ `PLANNED/BLOCKED_BY_HARDWARE`; không được
đánh dấu release validated và không suy diễn từ cấu hình gần giống.

## 6. Cấu trúc evidence

Tạo thư mục không commit image/log lớn vào source tree:

```text
validation/evidence/<image-id>/<config-id>/
  RUN_RECORD.md
  inventory/
  safe-cold/
  full-cold-01/
  full-warm-01/
  full-warm-02/
  soak-*/
  negative/
  hashes.sha256
```

Copy [`validation/RUN_RECORD_TEMPLATE.md`](../validation/RUN_RECORD_TEMPLATE.md)
thành `RUN_RECORD.md`. Tạo `hashes.sha256` cho toàn bộ evidence và lưu thêm một
bản ngoài USB test. Không sửa JSON report gốc; nhận xét để trong biên bản.

## 7. Quy tắc promote trạng thái

Một config chỉ được đổi thành `CONFIG_VALIDATED` khi:

- toàn bộ P0–P6 đạt;
- session validator không có lỗi;
- không còn incident chưa giải quyết;
- evidence có hash và reviewer kiểm độc lập;
- exact Image ID, hardware/BIOS/RAM config được khóa trong record.

Release chỉ được đổi thành `RELEASE_VALIDATED` khi:

- mọi dòng matrix bắt buộc đạt;
- build quality gate, BIOS/UEFI QEMU và physical protocol đều đạt;
- fault campaign AMD + Intel đạt;
- không có false PASS đã biết;
- `VALIDATION_STATUS.md` liên kết tới từng record và ghi ngày/reviewer.

Bất kỳ thay đổi nào vào engine, source/golden vectors, kernel, initramfs, driver,
boot runner hoặc image đều tạo release candidate mới. Fix tài liệu thuần túy có
thể giữ release, nhưng phải ghi rõ thay đổi không tác động artifact.

## 8. Khi có FAIL

1. Không chạy lại rồi xóa lần FAIL.
2. Poweroff an toàn nếu có thermal/electrical symptom.
3. Copy nguyên session, hash evidence và đánh dấu matrix `FAIL_INVESTIGATING`.
4. Giữ seed, offset, profile, BIOS settings và DIMM placement.
5. Replay bằng `--seed 0x...` từ `config.seed_hex`; xác nhận commitment và round
   seed/digest trước khi so failure fingerprint.
6. Chạy diagnostic rerun có kiểm soát; không quy ngay lỗi cho DIMM/CPU nếu evidence
   chỉ cho thấy route/system failure.
7. Sau khi sửa hardware/software, tạo config revision hoặc Image ID mới và chạy
   lại protocol liên quan; record cũ vẫn được giữ.
