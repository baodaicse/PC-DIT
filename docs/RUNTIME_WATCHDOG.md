# Runtime watchdog độc lập

`dit-watchdog` là supervisor process riêng cho từng workload CPU–RAM, CPU–SSD/NVMe hoặc GPU–VRAM. Nó không nằm trong worker pool, không cấp phát test buffer và không tham gia phép tính integrity. Vì vậy worker bị kill hoặc kẹt vẫn để lại evidence do supervisor ghi.

## Cơ chế

- Supervisor spawn worker bằng process riêng và poll mỗi giây.
- CPU–RAM theo dõi `system-run` progress JSON; GPU–VRAM theo dõi progress JSON; CPU–NVMe theo dõi `events.jsonl` do engine append/rate-limit.
- Sau khi từng có activity, không có cập nhật trong 90 giây (NVMe: 180 giây) tạo trạng thái `STALLED`. Sau 30 giây grace, watchdog buộc dừng worker và ghi `STALLED_KILLED`.
- Khi worker thoát, watchdog phân biệt `PASS`, exit code failure, signal termination và `OOM_KILLED`. `OOM_KILLED` chỉ được gán khi counter `oom_kill` của cgroup tăng trong run; signal `SIGKILL` không tự động được gọi là OOM.
- Runtime evidence nằm trong `/run/dit-suite/<run-id>/<module>/watchdog-status.json`; kết thúc bình thường/không bình thường có thêm `watchdog-final.json`.
- Nếu boot runner đã mount result partition đã xác thực, record được mirror vào `DIT-RESULTS/watchdog/<run-id>-<module>.json`. Khi bật menu sau boot, record còn state `STARTING`, `RUNNING` hoặc `STALLED` sẽ hiện `WATCHDOG RECOVERY`; đó là bằng chứng run cũ không đóng bình thường.

## Ý nghĩa trạng thái

| State | Kết luận |
|---|---|
| `PASSED` | Worker trả `0`; terminal label rút gọn là `PASS`; vẫn phải xem report integrity của module. |
| `WORKER_FAILED` | Worker trả exit code khác 0. |
| `WORKER_SIGNALED` | Worker bị signal nhưng không có cgroup OOM evidence. |
| `OOM_KILLED` | cgroup memory controller ghi nhận OOM kill trong run. |
| `STALLED_KILLED` | Đã có heartbeat/activity, sau đó hết cập nhật qua stale + grace window. |
| `STARTING` / `RUNNING` / `STALLED` trên partition bền ở boot kế | Mất nguồn, reset, kernel panic hoặc supervisor không kịp đóng record; **không phải PASS**. |

## Giới hạn cố ý

Watchdog không thể chạy khi kernel đã hard-lock, CPU reset hoặc mất điện. Nó cũng không thể đảm bảo marker được flush nếu USB/result partition mất cùng lúc. Trường hợp đó cần BMC/platform hardware watchdog hoặc power controller ngoài máy. Không suy luận lỗi phần cứng chỉ từ một `WORKER_FAILED`: cần xem `watchdog-final.json`, report module, kernel log và tái lập test.

Không có heartbeat ban đầu (ví dụ còn ở picker/menu) sẽ không bị watchdog kill. Liveness contract chỉ arm sau heartbeat/activity đầu tiên để thao tác chọn test không bị nhận nhầm là treo.
