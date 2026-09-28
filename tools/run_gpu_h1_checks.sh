#!/usr/bin/env bash
# GPU-H1 gate: software checks, then (DIT_GPU_HARDWARE=1) the retained baseline matrix.
#   DIT_GPU_HARDWARE=1 DIT_GPU_H1_SECONDS=20 tools/run_gpu_h1_checks.sh
# Evidence goes to build/gpu-h1-baseline-<date>; an existing directory is never reused.
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
crates=(-p gpu-engine -p gpu-vulkan -p gpu-cuda -p gpu-cli -p dit-system)
cargo fmt --all -- --check
cargo clippy "${crates[@]}" --all-targets -- -D warnings
cargo test "${crates[@]}" -- --test-threads=1

if [[ ${DIT_GPU_HARDWARE:-0} != 1 ]]; then
    printf 'H1 software checks PASS; hardware baseline PENDING (set DIT_GPU_HARDWARE=1).\n'
    exit 0
fi
command -v jq >/dev/null
cargo build --release -p gpu-cli
bin="$project_dir/target/release/gpu-vram-dit"
seconds=${DIT_GPU_H1_SECONDS:-20}
evidence_dir="$project_dir/build/gpu-h1-baseline-$(date +%Y%m%d)"
[[ -e $evidence_dir ]] && evidence_dir="$evidence_dir-$(date +%H%M%S)"
mkdir -p "$evidence_dir"
printf 'Evidence retained at %s\n' "$evidence_dir"

{
    printf '{\n'
    printf '  "date": "%s",\n' "$(date -Is)"
    printf '  "kernel": "%s",\n' "$(uname -r)"
    printf '  "cpu": "%s",\n' "$(lscpu | sed -n 's/^Model name:[[:space:]]*//p')"
    printf '  "gpu": "%s",\n' "$(nvidia-smi --query-gpu=name,driver_version,power.limit,pcie.link.gen.max --format=csv,noheader 2>/dev/null || echo unavailable)"
    printf '  "binary_sha256": "%s",\n' "$(sha256sum "$bin" | cut -d' ' -f1)"
    printf '  "steady_seconds": %s,\n' "$seconds"
    printf '  "note": "one run per cell, pool 64 MiB; not a near-full VRAM run"\n'
    printf '}\n'
} >"$evidence_dir/environment.json"

# K -> jobs keeps the same shape for both modes and fits a 2 GiB card.
declare -A jobs_for=([256]=1024 [1024]=512 [4096]=256)
run() { # name, extra args...
    local name=$1
    shift
    "$bin" run --backend cuda --vram-mib 64 --seed 7 --no-ui \
        --output-dir "$evidence_dir/$name" "$@" >"$evidence_dir/$name.log" 2>&1
}
for k in 256 1024 4096; do
    common=(--matrix-jobs "${jobs_for[$k]}" --matrix-inner "$k")
    for mode in seq async; do
        extra=()
        [[ $mode == async ]] && extra=(--async-compute --coverage-floor 0)
        run "$mode-K$k-cold" "${common[@]}" "${extra[@]}" --generations 3
        run "$mode-K$k-steady" "${common[@]}" "${extra[@]}" --duration-seconds "$seconds"
    done
done

for report in "$evidence_dir"/*/report.json; do
    name=$(basename "$(dirname "$report")")
    jq -e '.verdict == "PASS" and .detection.version == 1
        and .detection.link.available == true
        and (.detection.kernel_log.available == true)
        and (.detection.kernel_log.families.nvidia_xid == 0)
        and (.detection.kernel_log.families.device_lost == 0)
        and .detection.compute.failed_batches == 0' "$report" >/dev/null ||
        { printf 'H1 gate failed for %s\n' "$name"; exit 1; }
    if [[ $name == async-* ]]; then
        jq -e '.async_compute.version == 2 and .async_compute.conservation == "ok"
            and (.async_compute.totals.submitted
                == .async_compute.totals.verified + .async_compute.totals.failed
                    + .async_compute.unfinished_batches)
            and .async_compute.unfinished_batches == 0
            and .campaign_profile.matrix.verified == .async_compute.totals.verified
            and .matrix_dispatches_completed == .async_compute.totals.verified
            and .async_compute.timing.oracle_seconds >= 0
            and .async_compute.timing.device_busy_wall_seconds
                <= .async_compute.timing.burst_wall_seconds + 0.05' "$report" >/dev/null ||
            { printf 'H1 async accounting gate failed for %s\n' "$name"; exit 1; }
    fi
done

# Negative gate: an injected batch must FAIL, be counted, and never enter verified.
set +e
"$bin" run --backend cuda --generations 3 --vram-mib 64 --matrix-jobs 256 --matrix-inner 64 \
    --seed 7 --no-ui --async-compute --coverage-floor 0 --inject-vulkan-fault matrix \
    --output-dir "$evidence_dir/negative-async" >"$evidence_dir/negative-async.log" 2>&1
negative_status=$?
set -e
[[ $negative_status -ne 0 ]] || { printf 'negative async run unexpectedly exited 0\n'; exit 1; }
jq -e '.verdict == "FAIL" and .hardware_tested == false
    and .async_compute.totals.failed == 1
    and .async_compute.totals.failed_bytes > 0
    and .async_compute.mismatch_words == 256
    and .async_compute.conservation == "ok"
    and .campaign_profile.matrix.failed == 1
    and .detection.compute.failed_batches == 1
    and .detection.first_failure != null' \
    "$evidence_dir/negative-async/report.json" >/dev/null ||
    { printf 'negative async gate failed\n'; exit 1; }

python3 tools/gpu_h1_summary.py "$evidence_dir"
printf 'H1 hardware baseline PASS; evidence: %s\n' "$evidence_dir"
