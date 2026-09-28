# DIT CPU–RAM (CPU-DIT-01) Product Specification

## 1. Phạm vi

CPU-DIT-01 kiểm tra correctness trên đường dữ liệu CPU–RAM, không chỉ tạo tải.
Phiên bản đầu chạy từ USB boot, ưu tiên Ubuntu/Linux minimal, không ghi vào ổ đĩa
nội bộ của máy được kiểm tra.

Đây là project **DIT CPU–RAM**. DIT CPU–SSD/NVMe và bài cô lập khe M.2 là project
khác vì có destructive-write policy, thiết bị đích và mô hình quy lỗi riêng.

Phạm vi MVP:

- xác minh dữ liệu nguồn trên USB;
- kiểm tra CPU integer/bitwise, SIMD, hash/crypto và multi-core;
- kiểm tra RAM bằng pattern trực tiếp và dữ liệu biến đổi;
- kiểm tra cache coherence và memory controller dưới tải kết hợp;
- thu thập coverage, mismatch, ECC/MCE/EDAC, timeout và nhiệt độ;
- tạo báo cáo có thể dùng để tái hiện đúng phiên test.

Ngoài phạm vi MVP:

- GPU/VRAM/PCIe;
- kết luận chính xác DIMM vật lý khi platform không cung cấp mapping đáng tin cậy;
- tuyên bố đã kiểm tra 100% physical RAM trong khi chạy dưới Linux;
- dùng benchmark, utilization hoặc nhiệt độ làm oracle PASS.

## 2. Mục tiêu correctness

CPU phải chia dữ liệu chuẩn thành các block có kích thước và alignment khác nhau,
biến đổi chúng bằng matrix xác định, rải lên phần RAM có thể cấp phát, đọc lại bằng
các worker/core khác, exact-verify, phục hồi/join và tính kết quả cuối.

Các failure class cần phát hiện:

- source/manifest trên USB không hợp lệ;
- split sai offset, size hoặc thứ tự;
- CPU/matrix/SIMD tính sai;
- metadata hoặc payload thay đổi trong RAM;
- block bị ghi nhầm slot, mất, lặp hoặc đổi chỗ;
- decode hoặc join sai;
- final computation khác golden result;
- worker crash, timeout, thermal stop, MCE hoặc ECC event vi phạm policy.

## 3. Nguyên tắc sản phẩm

1. Cùng build, descriptor, source, seed và recipe phải cho cùng kết quả.
2. Source lỗi cho trạng thái `INVALID_SOURCE`, không quy lỗi CPU/RAM.
3. Dữ liệu trung gian tồn tại dưới dạng block trong vùng RAM được kiểm soát; không
   tạo hàng nghìn file làm nhiễu bởi USB, filesystem hoặc page cache.
4. Matrix integrity chính dùng số nguyên/Galois field. Floating-point là test riêng
   với tolerance/ULP, không dùng làm oracle bit-exact chính.
5. Checksum/digest giúp phát hiện và khoanh vùng nhưng không thay thế exact compare
   trên correctness path của RAM.
6. Encoder tối ưu không tự xác minh chính nó; cần scalar/reference độc lập.
7. Parity có thể hỗ trợ chẩn đoán nhưng không được sửa lỗi rồi báo PASS.
8. PASS luôn đi cùng coverage thực tế, không mang nghĩa phần cứng chắc chắn không lỗi.
9. Mọi release complete round phải chạy đủ mandatory window-size/pattern recipe;
   duration dài hơn thêm round/seed, không được dùng để che việc bài ngắn thiếu
   size class quan trọng.

## 4. Thích nghi với dung lượng RAM

Engine không hard-code profile riêng cho 32 GB hay 64 GB. Khi preflight, engine
phân biệt và báo các lớp dung lượng:

- RAM lắp đặt hoặc firmware công bố;
- RAM Linux quản lý/sử dụng được;
- `MemAvailable` tại thời điểm chuẩn bị test;
- RAM DIT yêu cầu và cấp phát được;
- byte đã touch, write, read và exact-verify;
- RAM excluded/reserved nếu xác định được.

Allocation target được tính động từ RAM khả dụng trừ vùng dự phòng an toàn cho
kernel, DIT control plane, metadata, telemetry và log. Engine tăng allocation đến
high-water mark an toàn, không được kích hoạt OOM để cố đạt một tỷ lệ đẹp.

Các yêu cầu bắt buộc:

1. Cấp phát nhiều arena và giữ đồng thời trong suốt round để Linux không tái sử
   dụng cùng physical page cho các batch kế tiếp.
2. Prefault/touch toàn bộ page trước khi ghi nhận chúng là allocated coverage.
3. Xử lý toàn bộ phần dư khi dung lượng không chia hết cho block/matrix group.
4. Cấp phát và báo coverage riêng theo NUMA node khi platform có NUMA.
5. Source corpus có thể nhỏ hơn RAM; generator xác định dùng source, seed, block
   index và global offset để tạo payload khác nhau trên toàn test region.
6. Allocation hoặc `mlock` không đạt phải được report; chỉ PASS nếu coverage target
   trong descriptor vẫn đạt.
7. Logical window plan phải phủ requested region đúng một lần mỗi write pass,
   không gap/overlap; size/pattern coverage phải được report tách khỏi unique RAM.

## 5. Trạng thái kết quả

- `PASS`: hoàn tất coverage mục tiêu, mọi oracle đúng và không có event cấm.
- `FAIL`: có data mismatch, golden mismatch hoặc hardware/execution event cấm.
- `INVALID`: source, descriptor hoặc môi trường test không hợp lệ.
- `UNSUPPORTED`: platform thiếu capability bắt buộc cho test được chọn.
- `ABORTED`: người dùng dừng hoặc safety policy chủ động dừng phiên.

Không tự quy combined mismatch cho CPU hoặc RAM. Kết quả mặc định là lỗi data
route/system interaction cho tới khi diagnostic rerun cung cấp đủ evidence.

## 6. Môi trường boot

- Root filesystem read-only, overlay tạm thời nếu cần.
- Tắt swap và service nền không cần thiết.
- DIT engine chạy với quyền cần thiết để affinity, memory lock và đọc telemetry.
- Log chỉ ghi vào phân vùng kết quả trên USB hoặc đích được người vận hành cho phép.
- Preflight thu thập CPU topology, RAM, NUMA, ECC/EDAC và sensor khả dụng.

Nếu cần coverage physical RAM cao hơn, sản phẩm có thể thêm tầng pre-OS Memtest.
Linux DIT vẫn chịu trách nhiệm kiểm tra CPU/cache/coherency/IMC/RAM dưới tải thực.
