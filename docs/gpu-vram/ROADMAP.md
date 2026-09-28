# Roadmap GPU–VRAM–DIT

Cập nhật: 2026-09-20. Trạng thái: GPU-A..F và GPU-G1/G2/G3 và CUDA sequential `implemented`; G4, overlap G5, G6/G7 còn mở; hardware/image matrix còn `pending`. Project: `/home/hoang/DIT-01`.

## Mục tiêu và phạm vi

Roadmap thực thi ưu tiên tiếp theo: [GPU-H — Async balanced pipeline](GPU_ASYNC_BALANCED_PIPELINE_ROADMAP.md).
Thuật toán tính toán và cách xác thực VRAM hiện tại: [GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md](GPU_ALGORITHMS_AND_VRAM_VERIFICATION.md).
H hiện `partial` (adapter CUDA burst + controller v0 đã có trong source, chưa đạt gate):
giảm chi phí CPU verifier, tận dụng VRAM trong budget, RAM output queue có giới hạn,
tự giảm/phục hồi compute khi backlog thay đổi. H6 (transfer/PCIe/VRAM) và H4b (Vulkan)
là track tách. Kế thừa G1/G2/G3/CUDA sequential; cụ thể hóa G4 và overlap G5, không coi đã implemented.

Kiểm tra toàn vẹn dữ liệu qua CPU/RAM staging → GPU → VRAM → tính toán GPU → CPU kiểm chứng. Kế thừa split/scatter/join, pattern và seed theo duration của CPU–RAM; dùng matrix có kết quả kiểm chứng được để tăng tải GPU. Module chạy độc lập, sau này tham gia campaign CPU–RAM–SSD–GPU.

Dùng chung workspace, `dit-contracts`, `dit-system`, `dit-ui-core`, control center, pipeline Ubuntu Minimal image và cơ chế lưu report của DIT-01. Vulkan Compute là backend đầu tiên được đề xuất; khả năng chạy phải được xác nhận bằng capability probe và smoke thực tế cho từng GPU/driver. CPU–RAM và CPU–SSD đang hoạt động không phải bằng chứng GPU engine đã sẵn sàng.

Mỗi TASK bên dưới là một đơn vị giao việc chính. Khi giao thực hiện một task, hoàn thành source, kiểm thử phần mềm phù hợp và cập nhật evidence/status của task đó. Kiểm thử hardware chưa có thiết bị được ghi `pending`, không chuyển thành PASS.

| Task | Kết quả chính | Phụ thuộc | Trạng thái |
|---|---|---|---|
| GPU-A | Contract, dữ liệu chuẩn, CPU oracle và engine mô phỏng | Không | implemented |
| GPU-B | Nhận diện Vulkan/GPU, quản lý VRAM, copy round-trip thật | A | implemented |
| GPU-C | Pattern, split/join và matrix trên GPU có kiểm chứng | B | implemented |
| GPU-D | Deep theo duration, seed, coverage và xử lý lỗi | C | implemented |
| GPU-E | UI chung, nhiều GPU, sensor và control center | D | implemented |
| GPU-F | Payload/image chung, negative campaign và hồ sơ hardware | E | implemented |

Thứ tự triển khai: A → B → C → D → E → F. Mốc chạy được trên một GPU là C; mốc Deep độc lập đủ UI/report là E; mốc phát hành phải có bằng chứng F cho cấu hình được công bố hỗ trợ.

## Các quyết định thiết kế ban đầu

### Dữ liệu và thuật toán

- “File chia nhỏ” trong bài GPU được biểu diễn bằng block/buffer trong RAM và VRAM; USB giữ nguồn/golden và bundle kết quả. Không tạo chuỗi file payload trên USB cho mỗi vòng.
- Block logic khởi điểm 4 KiB; chunk truyền/lưu gồm 64/128/256 KiB, 1/4/12/32/64/128/256 MiB, lọc theo giới hạn thiết bị. Kích thước matrix tile và chunk là hai tham số riêng.
- Header chứa schema, seed, generation, device identity, allocation/chunk/block index, logical offset và payload length. Chốt byte layout/endian, giới hạn và golden hash trong A. GPU có format version riêng; chỉ tái sử dụng tiện ích nguồn NVMe nếu không kéo theo giả định LBA/storage.
- Matrix số nguyên `u32` modulo 2^32 là baseline; CPU reference có cách tính độc lập. `u64`, floating point và các nhân chuyên dụng là phần mở rộng theo capability, không bắt buộc cho baseline.
- GPU phải tạo output thực sự được CPU đọc và kiểm chứng; checksum GPU chỉ hỗ trợ khoanh vùng. Mọi output được tính là verified đều phải qua oracle tương ứng. Theo dõi backlog CPU verifier để không gọi lượng dispatch là lượng tính toán đã xác minh.
- Pattern: zero/one, AA55/55AA, walking bits, inverted, address-as-data và PRNG tái tạo được. Scatter/gather dùng permutation hợp lệ; test ranh giới buffer và phần đuôi với alignment đúng API.
- CPU oracle cũng chạy trên phần cứng đang được kiểm tra. Golden cố định và implementation độc lập giảm lỗi phần mềm chung, không bảo đảm CPU là nguồn tuyệt đối không thể sai.

### Bộ nhớ và độ phủ

- Phân biệt dedicated VRAM với RAM chia sẻ của iGPU. Chặn software Vulkan device khỏi kết quả hardware validation; backend mô phỏng được ghi rõ.
- Cấp phát theo từng nấc, có reserve cho display/driver, staging, output và workspace. Mức 80–90% budget khả dụng cho GPU xuất hình, 90–95% cho GPU headless là mục tiêu thử nghiệm cần hiệu chỉnh, chưa phải cam kết/default đã validated.
- Khi có `VK_EXT_memory_budget`, dùng headroom ước lượng từ `heapBudget - heapUsage`, cập nhật trong lúc chạy và tính mọi allocation của app vào ngân sách. Đây là ước lượng theo heap/process, không phải phép đo chính xác toàn bộ VRAM còn trống. Xem [Khronos memory budget](https://docs.vulkan.org/refpages/latest/refpages/source/VK_EXT_memory_budget.html).
- Chọn heap/memory type theo capability; thiếu budget extension thì fallback bảo thủ và báo mức tin cậy. Không tự chuyển bài dedicated VRAM thành host-memory test khi allocation thất bại. Memory type device-local không tự chứng minh residency vật lý trên mọi driver; report ghi khả năng quan sát thực tế. Xem [Khronos memory allocation](https://docs.vulkan.org/spec/latest/chapters/memory.html).
- Xoay offset/permutation, thay allocation và quét đầy đủ phạm vi logic đã cấp phát. Allocation mới có thể được driver ánh xạ lại đúng trang vật lý cũ; không quảng cáo độ phủ mọi cell/bank/channel VRAM.
- Report tách allocated bytes, verified logical bytes trong từng allocation generation, cumulative traffic, sweep count và vùng chưa verify. Không cộng số byte các allocation chồng lặp rồi gọi là dung lượng VRAM vật lý duy nhất.
- Working set của bài bandwidth/retention phải đủ lớn để hạn chế chỉ lặp trong cache. Đo kết quả và lượng dữ liệu thật; buffer lớn không tự bảo đảm mọi lượt đọc đều đi tới chip VRAM.

### Duration, seed và verdict

- Deep là chế độ vận hành chính, duration mặc định đề xuất 1.800 giây. Smoke ngắn là công cụ phát triển. Dùng monotonic clock; kết thúc một vòng không tự kết thúc campaign.
- Quantum seed khởi điểm đề xuất 180 giây. Trong điều kiện tải cho phép, 30 phút hướng đến khoảng 10 seed, 60 phút khoảng 20; số hoàn tất thực tế phụ thuộc tốc độ GPU và thời gian verify.
- Đổi seed tại ranh giới batch đã xác minh, ưu tiên đủ sweep trước khi tái sử dụng vùng nhớ. Batch được chia nhỏ để tránh trì hoãn seed quá lâu. Lưu lịch planned/started/completed và seed bị bỏ lỡ; thời lượng cuối thiếu chỉ ghi partial coverage.
- Chunk size xoay trên danh sách chuẩn; duration dài tăng tổ hợp seed × pattern × chunk × kernel × allocation map. Không đặt quy tắc 60 phút bắt buộc có gấp đôi số kích thước chunk khác nhau.
- Khi deadline đến, ngừng submit và drain có giới hạn. Report tách active duration, drain/finalize và nguyên nhân dừng. GPU reset/hang hay thiếu kiểm chứng cuối phải ghi rõ; không PASS chỉ vì đồng hồ đã hết.
- Có `stop_on_error`; mặc định tiếp tục đếm mismatch nếu device còn hoạt động và batch có thể cách ly. Device lost/timeout dừng GPU đó. FAIL giữ nguyên đến hết campaign dù vòng sau đúng; count phát hiện lặp không được mô tả như số cell hỏng duy nhất.
- UI có công tắc temp limit, mặc định tắt theo quy ước dự án; báo trạng thái và sensor source. Missing sensor không tự là mismatch. OOM/capability/driver lỗi được ghi execution/coverage status riêng, không tự kết luận hỏng VRAM.

## GPU-A — Contract, golden và engine mô phỏng

Trạng thái: `implemented` ngày 2026-09-06; software gates và host UI smoke PASS. GPU/VRAM hardware test chưa hoạt động trong A.

Đầu ra:

- Chốt module identity: registry hiện có placeholder `dit.vga`; ưu tiên giữ ID tương thích và đổi nhãn `GPU + VRAM`. Nếu chọn `dit.gpu-vram` thì phải có alias/schema migration và test đọc report cũ trước khi tích hợp.
- Tách interface inventory, allocation, upload/download, dispatch và completion khỏi engine; cấu trúc crate cụ thể chốt khi triển khai, không nhân bản UI/sensor.
- Định nghĩa source format, matrix semantics, replay seed và report/event version; fixture golden nhỏ được kiểm tra độc lập trước khi khóa hash.
- CPU reference cho generate/split/permutation/join/matrix và backend RAM mô phỏng có fault injection.
- Error evidence gồm phase phát hiện, expected/actual, first mismatch, seed/generation/device/offset, shader/build identity. Root cause mặc định `undetermined` nếu chưa có thí nghiệm phân biệt.

Gate hoàn tất: golden/replay ổn định; bắt được bit flip, stale generation, sai địa chỉ, đảo chunk, bỏ/nhân đôi block, sai matrix và sai oracle fixture. Mock PASS không được ghi thành GPU hardware PASS.

Implementation hiện tại dùng format `DITGPUA1`, block 4 KiB, scatter/join theo seed và matrix 8×8 `u32`. Control Center đã mở lựa chọn số 3 `GPU + VRAM`; dashboard baseline dùng theme/render in-place hiện có và hiển thị GPU/VRAM telemetry khi provider hỗ trợ. Multi-GPU picker, VRAM allocation, transfer và shader vẫn thuộc B–E. Chi tiết tại [GPU_A_IMPLEMENTATION.md](GPU_A_IMPLEMENTATION.md).

## GPU-B — Vulkan capability, allocation và transfer

Trạng thái: `implemented` ngày 2026-09-06. `gpu-vulkan` dùng Vulkan 1.1 qua `ash`, nhận UUID/BDF/queue/heap/budget, loại `PHYSICAL_DEVICE_TYPE_CPU`, cấp phát device-local theo nhiều chunk có backoff và thực hiện staging upload → VRAM → readback → full byte compare. Unsafe chỉ tồn tại trong crate FFI này. Host bounded smoke GTX 960 PASS; validation layer matrix và các GPU khác vẫn pending.

Đầu ra:

- Probe Vulkan loader/ICD, GPU UUID, PCI BDF khi extension cung cấp, driver/version, queue, heaps, features/limits. Ghép với inventory chung bằng identity đủ mạnh; không đoán theo thứ tự liệt kê/model giống nhau.
- Picker/CLI phân biệt GPU thật, iGPU, unsupported và software renderer; có lý do không chạy được. Kernel driver bind hoặc `lspci` có model chưa đủ chứng minh Vulkan Compute hoạt động.
- Baseline một queue, buffer device-local và staging có giới hạn; upload → download → CPU byte compare. Có RAII/cleanup cho lỗi từng bước, allocation backoff và budget report.
- Xử lý flush/invalidate cho host memory non-coherent, fence và memory dependency đúng phạm vi. Alignment tuân theo buffer requirements, copy command và `nonCoherentAtomSize`.
- Tools dự kiến `gpu-inventory` và `gpu-host-smoke` chỉ được ghi là dùng được sau khi có implementation; smoke hardware nhỏ tách riêng khỏi unit/mock gate.

Gate hoàn tất: mock allocation failure/partial initialization không rò tài nguyên; round-trip nhiều size/boundary qua Vulkan validation layers không có lỗi trên host hỗ trợ. Chưa có GPU phù hợp thì hardware gate pending, không dùng software renderer để thay thế bằng chứng.

## GPU-C — Pattern, split/scatter/join và matrix thật

Trạng thái: `implemented` ngày 2026-09-06 cho baseline integer. Shader SPIR-V đóng trong binary nhận A/B 8×8 theo từng job, tính `u32` modulo 2^32, barrier transfer→compute→readback và so từng output với CPU oracle độc lập. Shader SHA-256 `7f7e90c20ded5239f060d7751f9ee844b9b6b48e9b08780551434bca6b811182`. Fault injection transfer/matrix bị phát hiện; host GTX 960 bounded smoke PASS.

Đầu ra:

- Ba pipeline có checkpoint: transfer-only; VRAM pattern/retention; transfer + scatter/transpose/matrix/gather/join + readback.
- Shader baseline `u32` và CPU oracle độc lập; tile/workgroup/config được chọn theo device limits. Chunk lớn gom nhiều tile; GPU ghi output và CPU kiểm tra full output của batch trong baseline correctness.
- Guard vùng đầu/cuối, kiểm tra logical address và generation, phát hiện write thiếu hoặc ghi nhầm buffer. Header/digest không được dùng thay cho việc xác minh payload cần test.
- Kernel count, shader hash, uploaded/downloaded bytes, verified results và checkpoint mismatch đưa vào event/report. Build lưu SPIR-V/compiler identity để replay.
- Barrier compute-write → copy-read → host-read được kiểm chứng. Các ví dụ [Khronos synchronization](https://docs.vulkan.org/guide/latest/synchronization_examples.html) là tài liệu tham chiếu cho thiết kế; bật validation layers trong lượt debug.

Gate hoàn tất: so GPU/CPU trên fixture và nhiều seed/size; fault-injection có chủ đích vào data/output/index bị bắt đúng checkpoint. Một GPU thực chạy đủ ba pipeline mới đạt hardware smoke của C. Không suy lỗi tại checkpoint thành kết luận linh kiện hỏng.

## GPU-D — Deep scheduler, seed và độ phủ theo duration

Trạng thái: `implemented` ngày 2026-09-06 ở mức source/bounded host gate. Deep mặc định 1.800 giây và không dừng khi hết một task; seed đổi mỗi 180 giây ở campaign duration, chunk xoay 64 KiB..64 MiB, pool VRAM giữ xuyên vòng, mỗi vùng có immediate compare và delayed/final retention compare. Report tách allocation, cumulative traffic, verified logical bytes, sweeps, transfer/matrix/retention counters và sticky failure. Temp limit mặc định OFF, stop-on-error mặc định OFF. Chưa chạy soak 30/60 phút nên D chưa `validated` cho release/hardware matrix.

Đầu ra:

- Lịch Deep luân phiên transfer-heavy, compute-heavy, full sweep và retention; tự lặp đến deadline. Retention giữ buffer qua thời gian/tải và có lượt verify cuối.
- Scheduler cấp batch/CPU oracle workers có giới hạn; ghi verifier backlog, matrix results verified/s, transfer throughput và thời gian mỗi phase. Điều chỉnh batch/workload để CPU verifier không khiến GPU liên tục đói việc.
- Đổi seed/chunk/pattern/kernel/allocation theo quy tắc trên; ledger ghi coverage thực tế và replay một failure cụ thể.
- Sau baseline một queue, thêm overlap transfer/compute theo capability. Queue family ownership và semaphore/fence kiểm tra riêng; không giả định nhiều queue luôn chạy đồng thời.
- Bound dispatch, timeout/cancel/drain, sticky FAIL, per-device failure counters và giới hạn log. Lỗi đầu có full evidence, lỗi lặp giữ tổng đếm và mẫu giới hạn để tmpfs không đầy.

Gate hoàn tất: fake clock xác nhận lịch 30/60 phút, không PASS sớm khi hết vòng; actual short smoke xác nhận clock/drain. Negative campaign có corruption, stale data, backend timeout/device lost mô phỏng, cancel và verifier backlog. So sánh sequential/overlap cho cùng dữ liệu; evidence soak dài trên hardware vẫn được ghi riêng trong F.

## GPU-E — UI chung, nhiều GPU và sensors

Trạng thái: `implemented` ngày 2026-09-07. Picker/shared UI, process isolation, per-GPU progress/report, aggregate verdict, dynamic 1/2/4-GPU layout, paging, stable identity và iGPU budget gate đã PASS software. UI wide một GPU đã đồng bộ bố cục hai khung + panel test của CPU‑RAM; CPU sensor dùng cùng sampler, GPU provider bổ sung GPU/VRAM usage, temperature, watt và core/memory clock. Live picker/telemetry một GTX 960 proprietary PASS; multi-GPU thật còn pending.

Đầu ra:

- Control Center thêm `GPU + VRAM`; chọn GPU 1/2/A bằng model, BDF/UUID, driver và memory type. Duration/temp limit lấy cấu hình chung, không thêm chuỗi prompt sau picker.
- Tái sử dụng layout/theme/render in-place của `dit-ui-core`; mọi module dùng cùng nguồn inventory/sensor. Chốt widget chia sẻ và adapter CPU–RAM Bash trong task này nếu cần để tránh thêm một UI hoàn toàn riêng.
- Mỗi GPU thêm nhóm hàng động: GPU usage; VRAM test allocation/budget/verified; H2D/D2H speed; progress/checks/fail. Cao/thấp theo số GPU, có compact/paging/resize cho console nhỏ.
- Hiển thị seed index, active kernel/pattern/chunk, elapsed/target, verified calculations và FAIL TOTAL. Không nhầm reserved VRAM, logical verified bytes với RAM usage hoặc cumulative traffic.
- Temp/clock/power/utilization lấy từ provider phù hợp và ghi source/unit/timestamp. Missing capability là N/A có lý do; driver name, VRAM capacity và utilization là các field độc lập.
- Scheduler nhiều GPU giữ budget RAM staging/CPU verifier chung và verdict theo thiết bị. Một GPU lỗi không làm mất report của các GPU còn lại; không tự reset toàn GPU đang xuất UI.

Gate hoàn tất: mock UI 1/2/4 GPU, resize/compact, counter/verdict/cancel và budget iGPU + dGPU; snapshot/PTY không full-screen clear theo mỗi frame. Live smoke một GPU chứng minh telemetry nối đúng identity; nhiều GPU thật để pending nếu chưa có thiết bị.

## GPU-F — Image chung, negative campaign và hardware validation

Trạng thái: `implemented` ngày 2026-09-07 ở mức source, bounded host, extension và boot-policy gate. Binary/shader/docs/SBOM/manifest đã nằm trong payload của image CPU–RAM–SSD; image vật lý chưa build/boot và hardware matrix vẫn pending.

Đầu ra:

- Đóng binary/shader/schema/SBOM/hash vào pipeline DIT-01 hiện có; dùng lại cache Ubuntu Minimal, loader/ICD, firmware, boot/result layout và tools burn. Chốt driver profile theo hardware thực tế; gói firmware/Mesa có mặt chưa đủ công bố mọi GPU được hỗ trợ.
- Giữ image generic open-driver và hỗ trợ flavor NVIDIA tách rootfs/artifact bằng package tường minh. Không probe `nouveau` và `nvidia` cạnh tranh trong cùng flavor; manifest/build-info phải ghi profile/package. Mỗi package/driver/GPU vẫn cần validation riêng.
- Source test/host smoke trước; người dùng build `make shared-image` và burn bằng workflow hiện có. Không yêu cầu QEMU để qua gate GPU thực.
- Logs đang chạy ở tmpfs có giới hạn; bundle cuối ghi qua cơ chế results đã xác thực. Manifest ghi cả image/kernel/ICD/GPU driver/shader/module hash.
- Negative campaign kiểm tra tất cả phase, fault attribution, memory pressure mô phỏng, mất device, cancel và report partial. Fault GPU trong test là injection kiểm soát vào dữ liệu/backend; không chủ động bỏ barrier rồi coi undefined behavior là test có kết quả ổn định.
- Operator checklist và validation template theo cấu hình, seed, memory allocation/verified, duration thực, throughput, telemetry, errors và final report. Lưu đường dẫn evidence cho mỗi PASS/FAIL/blocked.

Ma trận hardware tối thiểu để mở rộng hỗ trợ:

| Nhóm | Bằng chứng cần có |
|---|---|
| NVIDIA dGPU | GPU/driver/ICD cụ thể; round-trip, matrix, coverage, Deep 30/60 phút |
| AMD dGPU | Cùng nhóm test, heap budget và sensor source |
| Intel iGPU | Compute thật, shared-RAM accounting, không cấp trùng RAM budget |
| AMD iGPU | Shared-memory/driver path và Deep thực tế |
| Intel dGPU/Arc | Dedicated heap, matrix/transfer và sensor capability |
| GPU đang xuất hình và GPU phụ | UI còn phản hồi, reserve/budget, bounded dispatch |
| Hai GPU | Identity/sensor không lẫn, budget/verdict độc lập, đồng thời 30 phút |

Mỗi hàng là mục tiêu cấu hình, không yêu cầu có đủ mọi hàng để ghi nhận một cấu hình đã validated. Release công bố đúng danh sách đã chạy; cấu hình chưa có thiết bị vẫn pending. Soak nhiều giờ bổ sung cho release Deep, ghi duration cụ thể chứ không diễn giải 30 phút PASS là bảo đảm không lỗi phần cứng.

Gate hoàn tất phần mềm: regression workspace/UI/boot/report, negative campaign, manifest/checksum PASS. Gate hardware: lưu đủ evidence trên ít nhất cấu hình công bố hỗ trợ; image mới được build/boot thật. Hoàn thành code F có thể là `implemented` trong khi một phần matrix vẫn `pending`.

## Phần mở rộng sau F

### GPU-G — Resident compute và CUDA (ưu tiên kế tiếp)

[Roadmap triển khai G1–G7](GPU_RESIDENT_COMPUTE_ROADMAP.md), G1/G2 `implemented`
ngày 2026-09-20: campaign profiling/replay, workspace budget/backoff, resource
reuse và fault campaign có software/bounded smoke PASS; layer/soak/image pending.
Evidence trong [implementation](GPU_G12_IMPLEMENTATION.md); G3/CUDA sequential đã implemented; G4, overlap G5, G6/G7 còn mở.
Tiếp tục từ baseline implemented, không chờ hardware validated.
Mục tiêu: giảm GPU chờ CPU/PCIe, thay tải pointer-chase bằng tính toán có output
được xác thực và giữ dữ liệu lâu trên GPU.

Thứ tự: đo/contract → persistent Vulkan resources → tiled integer GEMM/corpus
→ asynchronous slots/verifier → CUDA tùy chọn → FP32/GPU-local VRAM → UI/image
và evidence. Giữ transfer/retention riêng trong campaign, không bỏ coverage để
tăng utilization. Overlap/verifier workers trong thiết kế GPU-D chưa phải bằng
chứng đã có trong đường chạy; phần triển khai cụ thể được theo dõi tại G2/G4.

### Các hướng mở rộng khác

- Campaign CPU–RAM–SSD–GPU: mở resource contract hiện tại cho GPU staging, iGPU shared RAM, verifier workers và PCIe bandwidth. Bắt đầu chạy tuần tự có aggregate report; chế độ đồng thời chỉ thêm sau budget/scheduling tests và hardware gate riêng.
- CUDA được cụ thể hóa tại GPU-G5; HIP/Level Zero tiếp tục là hướng tùy chọn sau baseline Vulkan. Giữ cùng golden/verdict, capability matrix và provenance; không chốt dependency/version trước khi khảo sát thiết bị.
- FP32 được cụ thể hóa tại GPU-G6; FP64/tensor/mixed precision mở theo capability, có oracle và tiêu chí numerical error riêng; không áp byte-exact của integer vào mọi phép floating point.
- GPU↔GPU peer transfer, ECC counters và diagnostics vendor nếu thiết bị hỗ trợ. Không biến thiếu telemetry vendor thành FAIL tính toàn vẹn dữ liệu.
- Phân tích nguyên nhân bằng checkpoint/replay, chạy ở cấu hình mặc định, đổi GPU/slot/host và so log driver; báo confidence/evidence, không gán linh kiện chỉ từ một checksum mismatch.

## Quy tắc cập nhật tiến độ

Sau từng task, cập nhật [STATUS.md](STATUS.md) với source/artifact, lệnh kiểm thử thực sự đã chạy, kết quả, evidence path và hardware pending. Chỉ cập nhật checksum khi artifact mới tồn tại. `implemented` nghĩa là source và software gates hoàn tất; `validated` luôn kèm phạm vi cấu hình/phiên bản và evidence. Tài liệu roadmap hoàn tất không thay trạng thái các task triển khai.
