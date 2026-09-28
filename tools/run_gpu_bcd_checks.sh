#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
check_dir=$(mktemp -d /tmp/gpu-bcd-check.XXXXXX)
cleanup() {
    find "$check_dir" -mindepth 1 -delete 2>/dev/null || true
    rmdir "$check_dir" 2>/dev/null || true
}
trap cleanup EXIT
cd "$project_dir"

printf '[1/6] GPU-A..D format, Clippy and tests\n'
cargo fmt --all -- --check
cargo clippy -p gpu-engine -p gpu-vulkan -p gpu-cli -p dit-system -p dit-orchestrator \
    --all-targets -- -D warnings
cargo test -p gpu-engine -p gpu-vulkan -p gpu-cli -p dit-system -p dit-orchestrator \
    -- --test-threads=1

printf '[2/6] GPU-A explicit mock compatibility and negative injection\n'
cargo run -q -p gpu-cli -- run --backend mock --generations 2 \
    --blocks-per-generation 8 --no-ui --output-dir "$check_dir/mock-pass"
rg -q '"hardware_tested": false' "$check_dir/mock-pass/report.json"
set +e
cargo run -q -p gpu-cli -- run --backend mock --generations 1 \
    --inject-fault matrix-output --no-ui --output-dir "$check_dir/mock-fail"
negative_status=$?
set -e
test "$negative_status" -eq 1
rg -q '"verdict": "FAIL"' "$check_dir/mock-fail/report.json"

printf '[3/6] Vulkan inventory and software-renderer exclusion\n'
cargo run -q -p gpu-cli -- inventory >"$check_dir/inventory.json"
rg -q '"vulkan_devices"' "$check_dir/inventory.json"
if rg -q 'llvmpipe' "$check_dir/inventory.json"; then
    rg -q 'software/CPU Vulkan renderer is forbidden' "$check_dir/inventory.json"
fi

hardware_index=$(python3 - "$check_dir/inventory.json" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as handle:
    inventory = json.load(handle)
for device in inventory["vulkan_devices"]:
    if device["hardware_eligible"]:
        print(device["index"])
        break
PY
)
test -n "$hardware_index" || {
    printf 'GPU-B/C/D hardware smoke: PENDING (no eligible Vulkan GPU)\n'
    exit 0
}

printf '[4/6] bounded 64 MiB GPU-B transfer + GPU-C matrix + GPU-D retention\n'
cargo run -q -p gpu-cli -- run --backend vulkan --device-index "$hardware_index" \
    --generations 5 --vram-mib 64 --max-allocation-mib 32 --matrix-jobs 256 \
    --no-ui --output-dir "$check_dir/vulkan-pass"
for evidence in \
    '"verdict": "PASS"' \
    '"hardware_tested": true' \
    '"transfer_operations_completed": 5' \
    '"matrix_dispatches_completed": 5' \
    '"retention_checks_completed": 5' \
    '"failure_count": 0'; do
    rg -q "$evidence" "$check_dir/vulkan-pass/report.json"
done

printf '[5/6] Vulkan transfer comparator negative injection\n'
set +e
cargo run -q -p gpu-cli -- run --backend vulkan --device-index "$hardware_index" \
    --generations 1 --vram-mib 8 --max-allocation-mib 8 --matrix-jobs 32 \
    --inject-vulkan-fault transfer --no-ui --output-dir "$check_dir/vulkan-fail"
vulkan_negative_status=$?
set -e
test "$vulkan_negative_status" -eq 1
rg -q 'transfer mismatch at byte' "$check_dir/vulkan-fail/report.json"
rg -q '"hardware_tested": false' "$check_dir/vulkan-fail/report.json"
set +e
cargo run -q -p gpu-cli -- run --backend vulkan --device-index "$hardware_index" \
    --generations 1 --vram-mib 8 --max-allocation-mib 8 --matrix-jobs 32 \
    --inject-vulkan-fault matrix --no-ui --output-dir "$check_dir/vulkan-matrix-fail"
matrix_negative_status=$?
set -e
test "$matrix_negative_status" -eq 1
rg -q 'matrix mismatch at result' "$check_dir/vulkan-matrix-fail/report.json"

printf '[6/6] shared menu option 3\n'
install -m 0755 /bin/true "$check_dir/cpu-ui"
install -m 0755 /bin/true "$check_dir/nvme"
install -m 0755 /bin/true "$check_dir/gpu"
cargo run -q -p dit-orchestrator -- render-menu \
    --cpu-ui "$check_dir/cpu-ui" --nvme-binary "$check_dir/nvme" \
    --gpu-binary "$check_dir/gpu" >"$check_dir/menu.txt"
rg -q 'GPU \+ VRAM.*READY.*dit.vga' "$check_dir/menu.txt"

printf 'GPU-A/B/C/D CHECK: PASS (bounded hardware smoke; Deep soak still separate)\n'
