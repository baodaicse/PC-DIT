#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

validation_dir=$(mktemp -d /tmp/cpu-nvme-task-f-check.XXXXXX)
cleanup() {
    find "$validation_dir" -mindepth 1 -delete 2>/dev/null || true
    rmdir "$validation_dir" 2>/dev/null || true
}
trap cleanup EXIT

printf '[1/6] rustfmt, Clippy and workspace tests\n'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1

printf '[2/6] module-selection UI and registry\n'
install -m 0755 /bin/true "$validation_dir/cpu-ui"
install -m 0755 /bin/true "$validation_dir/nvme"
install -m 0755 /bin/true "$validation_dir/gpu"
cargo run -q -p dit-orchestrator -- render-menu \
    --cpu-ui "$validation_dir/cpu-ui" \
    --nvme-binary "$validation_dir/nvme" \
    --gpu-binary "$validation_dir/gpu" >"$validation_dir/menu.txt"
rg -q 'CPU \+ RAM.*READY' "$validation_dir/menu.txt"
rg -q 'CPU \+ SSD.*READY' "$validation_dir/menu.txt"
rg -q 'GPU \+ VRAM.*READY' "$validation_dir/menu.txt"
cargo run -q -p dit-orchestrator -- registry \
    --cpu-ui "$validation_dir/cpu-ui" \
    --nvme-binary "$validation_dir/nvme" \
    --gpu-binary "$validation_dir/gpu" >"$validation_dir/registry.json"
rg -q '"module_id": "dit.cpu-ram"' "$validation_dir/registry.json"
rg -q '"module_id": "dit.cpu-nvme"' "$validation_dir/registry.json"
rg -q '"module_id": "dit.vga"' "$validation_dir/registry.json"

printf '[3/6] native report adapters and aggregate verdict\n'
printf '%s\n' \
    '{"schema_version":9,"test_id":"CPU-DIT-01-SYSTEM-v1","status":"PASS"}' \
    >"$validation_dir/cpu-report.json"
cargo run -q -p nvme-cli -- multi-simulate \
    --devices 2 --duration-seconds 1 --operations 4 --capacity-mib 2 \
    --memory-budget-mib 8 --no-ui --report "$validation_dir/nvme-report.json"
cargo run -q -p dit-orchestrator -- aggregate \
    --cpu-report "$validation_dir/cpu-report.json" \
    --nvme-report "$validation_dir/nvme-report.json" \
    --output "$validation_dir/suite-report.json"
rg -q '"aggregate_verdict": "PASS"' "$validation_dir/suite-report.json"
rg -q '"module_id": "dit.cpu-ram"' "$validation_dir/suite-report.json"
rg -q '"module_id": "dit.cpu-nvme"' "$validation_dir/suite-report.json"

printf '[4/6] negative attribution remains module-scoped\n'
set +e
cargo run -q -p nvme-cli -- multi-simulate \
    --devices 2 --duration-seconds 1 --operations 8 --capacity-mib 2 \
    --memory-budget-mib 8 --inject-device 2 --inject bit-flip --inject-at 3 \
    --no-ui --report "$validation_dir/nvme-fail.json"
nvme_status=$?
cargo run -q -p dit-orchestrator -- aggregate \
    --cpu-report "$validation_dir/cpu-report.json" \
    --nvme-report "$validation_dir/nvme-fail.json" \
    --output "$validation_dir/suite-fail.json"
suite_status=$?
set -e
test "$nvme_status" -eq 1
test "$suite_status" -eq 1
rg -q '"aggregate_verdict": "FAIL"' "$validation_dir/suite-fail.json"
rg -A4 '"module_id": "dit.cpu-ram"' "$validation_dir/suite-fail.json" | rg -q '"verdict": "PASS"'
rg -A4 '"module_id": "dit.cpu-nvme"' "$validation_dir/suite-fail.json" | rg -q '"verdict": "FAIL"'

printf '[5/6] versioned image extension\n'
./tools/build_image_extension.sh
(
    cd dist/cpu-nvme-extension-root
    sha256sum --check MANIFEST.sha256
)
test -x dist/cpu-nvme-extension-root/opt/cpu-nvme-dit/cpu-nvme-dit
test -x dist/cpu-nvme-extension-root/opt/gpu-vram-dit/gpu-vram-dit
test -s dist/cpu-nvme-extension-root/usr/share/gpu-vram-dit/matrix_u32.spv
test -x dist/cpu-nvme-extension-root/usr/local/bin/dit-suite
test -x dist/cpu-nvme-extension-root/usr/local/bin/dit-watchdog
test -s dist/cpu-nvme-extension-root/usr/share/doc/cpu-nvme-dit/SBOM.tsv
rg -q $'^serde\t' dist/cpu-nvme-extension-root/usr/share/doc/cpu-nvme-dit/SBOM.tsv
tar -tzf dist/cpu-nvme-extension.tar.gz >"$validation_dir/archive-files.txt"
rg -q './etc/dit-suite/modules.d/cpu-nvme.json' "$validation_dir/archive-files.txt"

printf '[6/6] shared CPU-RAM image hook syntax/policy (no image build, no QEMU)\n'
bash -n "$project_dir/tools/build_boot_image.sh"
bash -n "$project_dir/image/rootfs/usr/local/lib/cpu-dit/boot-runner.sh"
"$project_dir/tools/test_boot_policy.sh"
python3 "$project_dir/tools/test_unified_project.py"

printf 'TASK-F CHECK: PASS (host/software gates; hardware validation pending)\n'
