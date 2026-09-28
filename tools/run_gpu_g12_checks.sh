#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
cargo fmt --all -- --check
cargo clippy -p gpu-engine -p gpu-vulkan -p gpu-cli --all-targets -- -D warnings
cargo test -p gpu-engine -p gpu-vulkan -p gpu-cli -- --test-threads=1

if [[ ${DIT_GPU_HARDWARE:-0} != 1 ]]; then
    printf 'G1/G2 software checks PASS; hardware PENDING (set DIT_GPU_HARDWARE=1).\n'
    exit 0
fi
command -v jq >/dev/null
cargo test -p gpu-vulkan hardware_smoke -- --ignored --test-threads=1
mkdir -p "$project_dir/build"
evidence_dir=$(mktemp -d "$project_dir/build/gpu-g12.XXXXXX")
printf 'Evidence retained at %s\n' "$evidence_dir"
if rg -l 'VK_LAYER_KHRONOS_validation' /usr/share/vulkan/explicit_layer.d /etc/vulkan/explicit_layer.d 2>/dev/null; then
    VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation cargo test -p gpu-vulkan hardware_smoke \
        -- --ignored --test-threads=1 >"$evidence_dir/validation.log" 2>&1
    if rg 'Validation Error|VUID-' "$evidence_dir/validation.log"; then
        printf 'Vulkan validation errors detected\n'
        exit 1
    fi
else
    printf 'Validation layer gate PENDING: Khronos layer not installed.\n'
fi

common=(run --backend vulkan --generations 3 --vram-mib 8
    --max-allocation-mib 8 --matrix-jobs 256 --compute-rounds 64 --no-ui)
cargo run -q -p gpu-cli -- "${common[@]}" --recreate-compute-resources \
    --output-dir "$evidence_dir/recreate"
cargo run -q -p gpu-cli -- "${common[@]}" --output-dir "$evidence_dir/reuse"
jq -e '.verdict == "PASS" and .last_compute.profile.version == 1
    and .last_compute.profile.buffers_reused == false
    and .last_compute.profile.pipeline_reused == false' \
    "$evidence_dir/recreate/report.json" >/dev/null
jq -e '.verdict == "PASS" and .matrix_dispatches_submitted == 3
    and .matrix_dispatches_gpu_completed == 3 and .matrix_dispatches_completed == 3
    and .last_compute.profile.buffers_reused and .last_compute.profile.pipeline_reused
    and .campaign_profile.version == 1
    and .campaign_profile.transfer.verified == 3
    and .campaign_profile.retention.verified == 3
    and .campaign_profile.matrix.verified == 3
    and (.campaign_profile.dit_queue_empty_lower_bound_seconds <= .campaign_profile.observed_seconds)
    and .workspace_budget.matrix_jobs_effective == 256
    and (.last_compute.profile.gpu != null or .last_compute.profile.gpu_unavailable_reason != null)
    and (.last_compute.profile.end_to_end_seconds >= .last_compute.elapsed_seconds)' \
    "$evidence_dir/reuse/report.json" >/dev/null
jq -e -s '.[0].last_compute.result_sha256 == .[1].last_compute.result_sha256
    and .[0].matrix_results_verified == .[1].matrix_results_verified
    and .[0].bytes_verified == .[1].bytes_verified' \
    "$evidence_dir/recreate/report.json" "$evidence_dir/reuse/report.json" >/dev/null

set +e
cargo run -q -p gpu-cli -- run --backend vulkan --generations 1 --vram-mib 8 \
    --max-allocation-mib 8 --matrix-jobs 32 --compute-rounds 2 \
    --inject-vulkan-fault matrix --no-ui --output-dir "$evidence_dir/negative"
negative_status=$?
set -e
test "$negative_status" -eq 1
jq -e '.verdict == "FAIL" and .matrix_dispatches_submitted == 1
    and .matrix_dispatches_gpu_completed == 1 and .matrix_dispatches_completed == 0
    and .last_compute_attempt.dispatches_verified == 0' \
    "$evidence_dir/negative/report.json" >/dev/null
set +e
DIT_GPU_HOST_BUDGET_BYTES=1 cargo run -q -p gpu-cli -- "${common[@]}" \
    --output-dir "$evidence_dir/budget-invalid"
invalid_status=$?
set -e
test "$invalid_status" -eq 2
jq -e '.verdict == "INVALID" and .hardware_tested == false' "$evidence_dir/budget-invalid/report.json" >/dev/null
DIT_GPU_HOST_BUDGET_BYTES=167772160 cargo run -q -p gpu-cli -- run --backend vulkan \
    --generations 1 --vram-mib 8 --matrix-jobs 65536 --compute-rounds 1 --no-ui \
    --output-dir "$evidence_dir/budget-backoff"
jq -e '.verdict == "PASS" and .workspace_budget.backoff_steps > 0
    and .workspace_budget.matrix_jobs_effective < .workspace_budget.matrix_jobs_requested
    and .last_compute.jobs == .workspace_budget.matrix_jobs_effective' \
    "$evidence_dir/budget-backoff/report.json" >/dev/null
printf 'G1/G2 bounded smoke PASS (not a soak/performance validation): %s\n' "$evidence_dir"
