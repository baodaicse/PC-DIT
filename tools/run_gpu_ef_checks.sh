#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
check_dir=$(mktemp -d /tmp/gpu-ef-check.XXXXXX)
cleanup() {
    find "$check_dir" -mindepth 1 -delete 2>/dev/null || true
    rmdir "$check_dir" 2>/dev/null || true
}
trap cleanup EXIT
cd "$project_dir"

printf '[1/7] GPU-E/F format, Clippy, unit/UI/multi-device gates\n'
cargo fmt --all -- --check
cargo clippy -p dit-ui-core -p dit-system -p gpu-engine -p gpu-vulkan -p gpu-cli -p dit-orchestrator \
    --all-targets -- -D warnings
cargo test -p dit-ui-core -p dit-system -p gpu-engine -p gpu-vulkan -p gpu-cli -p dit-orchestrator \
    -- --test-threads=1

printf '[2/7] Vulkan inventory, stable identity and software-renderer policy\n'
cargo run -q -p gpu-cli -- inventory >"$check_dir/inventory.json"
rg -q '"vulkan_devices"' "$check_dir/inventory.json"
hardware_index=$(python3 - "$check_dir/inventory.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
print(next((d["index"] for d in data["vulkan_devices"] if d["hardware_eligible"]), ""))
PY
)
software_index=$(python3 - "$check_dir/inventory.json" <<'PY'
import json, sys
data = json.load(open(sys.argv[1], encoding="utf-8"))
print(next((d["index"] for d in data["vulkan_devices"] if not d["hardware_eligible"]), ""))
PY
)
if [[ -n "$software_index" ]]; then
    set +e
    cargo run -q -p gpu-cli -- run --backend vulkan --device-index "$software_index" \
        --generations 1 --vram-mib 8 --no-ui --output-dir "$check_dir/software-invalid"
    software_status=$?
    set -e
    test "$software_status" -eq 2
    rg -q '"verdict": "INVALID"' "$check_dir/software-invalid/report.json"
    rg -q '"hardware_tested": false' "$check_dir/software-invalid/report.json"
fi

if [[ -n "$hardware_index" ]]; then
    printf '[3/7] bounded real-GPU selection/progress/retention PASS\n'
    cargo run -q -p gpu-cli -- run --backend vulkan --devices "$hardware_index" \
        --generations 16 --vram-mib 8 --max-allocation-mib 8 --matrix-jobs 128 \
        --progress-file "$check_dir/progress.json" --no-ui --output-dir "$check_dir/vulkan-pass"
    rg -q '"state":"PASS"' "$check_dir/progress.json"
    rg -q '"hardware_tested": true' "$check_dir/vulkan-pass/report.json"
    rg -q '"retention_checks_completed": 16' "$check_dir/vulkan-pass/report.json"
    rg -q '"failure_count": 0' "$check_dir/vulkan-pass/report.json"

    printf '[4/7] controlled Vulkan transfer and matrix negatives\n'
    for fault in transfer matrix; do
        set +e
        cargo run -q -p gpu-cli -- run --backend vulkan --device-index "$hardware_index" \
            --generations 1 --vram-mib 8 --max-allocation-mib 8 --matrix-jobs 32 \
            --inject-vulkan-fault "$fault" --no-ui --output-dir "$check_dir/negative-$fault"
        negative_status=$?
        set -e
        test "$negative_status" -eq 1
        rg -q '"verdict": "FAIL"' "$check_dir/negative-$fault/report.json"
        rg -q '"hardware_tested": false' "$check_dir/negative-$fault/report.json"
    done
else
    printf '[3/7] bounded real-GPU PASS: PENDING (no eligible hardware Vulkan GPU)\n'
    printf '[4/7] controlled Vulkan negatives: PENDING (no eligible hardware Vulkan GPU)\n'
fi

printf '[5/7] shared Control Center menu and GPU option\n'
install -m 0755 /bin/true "$check_dir/cpu-ui"
install -m 0755 /bin/true "$check_dir/nvme"
install -m 0755 /bin/true "$check_dir/gpu"
cargo run -q -p dit-orchestrator -- render-menu \
    --cpu-ui "$check_dir/cpu-ui" --nvme-binary "$check_dir/nvme" \
    --gpu-binary "$check_dir/gpu" >"$check_dir/menu.txt"
rg -q 'GPU \+ VRAM.*READY.*dit.vga' "$check_dir/menu.txt"

printf '[6/7] shared image extension, shader, SBOM and validation documents\n'
./tools/build_image_extension.sh
(
    cd dist/cpu-nvme-extension-root
    sha256sum --check MANIFEST.sha256
)
test -x dist/cpu-nvme-extension-root/opt/gpu-vram-dit/gpu-vram-dit
test -x dist/cpu-nvme-extension-root/usr/local/bin/dit-watchdog
test -s dist/cpu-nvme-extension-root/usr/share/gpu-vram-dit/matrix_u32.spv
test -s dist/cpu-nvme-extension-root/usr/share/doc/gpu-vram-dit/VALIDATION.md
test -s dist/cpu-nvme-extension-root/usr/share/doc/dit-suite/UNIFIED_DASHBOARD_AND_COMBINED_PIPELINE.md
rg -q '"gpu_pipeline": "GPU-G3-1"' dist/cpu-nvme-extension-root/etc/dit-suite/modules.d/cpu-nvme.json
rg -q '"ui_contract": "DIT-UNIFIED-1"' dist/cpu-nvme-extension-root/etc/dit-suite/modules.d/cpu-nvme.json

printf '[7/7] shared-image hook syntax and boot policy (no image/QEMU)\n'
bash -n tools/build_boot_image.sh image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh
./tools/test_boot_policy.sh
if NVIDIA_DRIVER_PACKAGE=invalid ./tools/build_boot_image.sh >/dev/null 2>&1; then
    printf 'invalid NVIDIA image package name was accepted\n' >&2
    exit 2
fi
python3 ./tools/test_unified_project.py

printf 'GPU-E/F CHECK: PASS (software/bounded host scope; physical image and hardware matrix remain separate)\n'
