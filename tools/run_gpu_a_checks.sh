#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
check_dir=$(mktemp -d /tmp/gpu-a-check.XXXXXX)
cleanup() {
    find "$check_dir" -mindepth 1 -delete 2>/dev/null || true
    rmdir "$check_dir" 2>/dev/null || true
}
trap cleanup EXIT
cd "$project_dir"

printf '[1/5] GPU-A engine, telemetry, UI and orchestrator tests\n'
cargo fmt --all -- --check
cargo clippy -p gpu-engine -p gpu-cli -p dit-system -p dit-orchestrator --all-targets -- -D warnings
cargo test -p gpu-engine -p gpu-cli -p dit-system -p dit-orchestrator -- --test-threads=1

printf '[2/5] bounded positive CPU-mock run\n'
cargo run -q -p gpu-cli -- run --generations 3 --blocks-per-generation 8 \
    --backend mock --no-ui --output-dir "$check_dir/pass"
rg -q '"module_id": "dit.vga"' "$check_dir/pass/report.json"
rg -q '"verdict": "PASS"' "$check_dir/pass/report.json"
rg -q '"hardware_tested": false' "$check_dir/pass/report.json"
rg -q '"vram_hardware_test_active": false' "$check_dir/pass/report.json"
rg -q '"generations_completed": 3' "$check_dir/pass/report.json"

printf '[3/5] negative injection stays a software FAIL\n'
set +e
cargo run -q -p gpu-cli -- run --generations 1 --blocks-per-generation 4 \
    --backend mock --inject-fault payload-bit-flip --no-ui --output-dir "$check_dir/fail"
negative_status=$?
set -e
test "$negative_status" -eq 1
rg -q '"verdict": "FAIL"' "$check_dir/fail/report.json"
rg -q 'digest mismatch' "$check_dir/fail/report.json"

printf '[4/5] GPU inventory/telemetry command\n'
cargo run -q -p gpu-cli -- inventory >"$check_dir/inventory.json"
rg -q '"pci_devices"' "$check_dir/inventory.json"
rg -q '"telemetry"' "$check_dir/inventory.json"

printf '[5/5] shared menu option 3 is READY\n'
install -m 0755 /bin/true "$check_dir/cpu-ui"
install -m 0755 /bin/true "$check_dir/nvme"
install -m 0755 /bin/true "$check_dir/gpu"
cargo run -q -p dit-orchestrator -- render-menu \
    --cpu-ui "$check_dir/cpu-ui" --nvme-binary "$check_dir/nvme" \
    --gpu-binary "$check_dir/gpu" >"$check_dir/menu.txt"
rg -q 'GPU \+ VRAM.*READY.*dit.vga' "$check_dir/menu.txt"

printf 'GPU-A CHECK: PASS (software contract/mock/UI; VRAM hardware test not active)\n'
