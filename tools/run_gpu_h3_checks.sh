#!/usr/bin/env bash
# GPU-H3 gate: software checks, then (DIT_GPU_HARDWARE=1) allocation-failure/slot/quarantine
# hardware gates and a slot-configuration matrix with retained evidence.
#   DIT_GPU_HARDWARE=1 DIT_GPU_H3_SECONDS=15 tools/run_gpu_h3_checks.sh
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
crates=(-p gpu-engine -p gpu-vulkan -p gpu-cuda -p gpu-cli -p dit-system)
cargo fmt --all -- --check
cargo clippy "${crates[@]}" --all-targets -- -D warnings
cargo test "${crates[@]}" -- --test-threads=1

if [[ ${DIT_GPU_HARDWARE:-0} != 1 ]]; then
    printf 'H3 software checks PASS; hardware gates PENDING (set DIT_GPU_HARDWARE=1).\n'
    exit 0
fi
command -v jq >/dev/null
cargo test -p gpu-cuda --test h3 -- --ignored --nocapture --test-threads=1
cargo test -p gpu-cuda --test g3 -- --ignored --nocapture
cargo build --release -p gpu-cli
bin="$project_dir/target/release/gpu-vram-dit"
seconds=${DIT_GPU_H3_SECONDS:-15}
evidence_dir="$project_dir/build/gpu-h3-slots-$(date +%Y%m%d)"
[[ -e $evidence_dir ]] && evidence_dir="$evidence_dir-$(date +%H%M%S)"
mkdir -p "$evidence_dir"
printf 'Evidence retained at %s\n' "$evidence_dir"
{
    printf '{\n  "date": "%s",\n  "kernel": "%s",\n' "$(date -Is)" "$(uname -r)"
    printf '  "cpu": "%s",\n' "$(lscpu | sed -n 's/^Model name:[[:space:]]*//p')"
    printf '  "gpu": "%s",\n' "$(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null || echo unavailable)"
    printf '  "binary_sha256": "%s",\n' "$(sha256sum "$bin" | cut -d' ' -f1)"
    printf '  "steady_seconds": %s,\n  "note": "one run per cell, pool 64 MiB; not a near-full VRAM run"\n}\n' "$seconds"
} >"$evidence_dir/environment.json"

run() { # name, env-assignment-or-'', shape (jobs:K), extra args...
    local name=$1 environment=$2 shape=$3
    shift 3
    env ${environment:+"$environment"} "$bin" run --backend cuda --vram-mib 64 --seed 7 --no-ui \
        --matrix-jobs "${shape%%:*}" --matrix-inner "${shape##*:}" --async-compute --coverage-floor 0 \
        --output-dir "$evidence_dir/$name" "$@" >"$evidence_dir/$name.log" 2>&1
}
# Slot configurations (device/host) on a GPU-bound and a verifier-bound shape.
for shape in 512:1024 1024:256; do
    k=${shape##*:}
    run "slots_3_8-K$k" '' "$shape" --duration-seconds "$seconds"
    run "slots_2_16-K$k" '' "$shape" --duration-seconds "$seconds" \
        --async-device-slots 2 --async-host-slots 16
    run "slots_4_12-K$k" '' "$shape" --duration-seconds "$seconds" \
        --async-device-slots 4 --async-host-slots 12
    run "slots_3_4-K$k" '' "$shape" --duration-seconds "$seconds" --async-host-slots 4
done
# Memory pressure: less host RAM than the full plan needs sheds queue depth first.
# (Rotation off: the second corpus would otherwise dominate the budget and force jobs down.)
run "pressure_host-K1024" 'DIT_GPU_HOST_BUDGET_BYTES=750000000' 512:1024 \
    --duration-seconds "$seconds" --async-host-slots 32 --corpus-rotate-batches 0
# Peers share the CPU: eight GPU processes leave one verifier worker each.
run "cpu_share_peers8-K1024" 'DIT_GPU_PEER_COUNT=8' 512:1024 --duration-seconds "$seconds" --async-workers 4

for report in "$evidence_dir"/slots_*/report.json "$evidence_dir"/pressure_*/report.json \
    "$evidence_dir"/cpu_share_*/report.json; do
    name=$(basename "$(dirname "$report")")
    jq -e '.verdict == "PASS" and .async_compute.conservation == "ok"
        and .async_compute.unfinished_batches == 0
        and .async_compute.device_in_flight_peak <= .async_compute.config.device_slots
        and .async_compute.host_slots_used_peak <= .async_compute.config.host_slots
        and .async_compute.config.host_slots > .async_compute.config.device_slots
        and .workspace_budget.async_device_slots == .async_compute.config.device_slots
        and .workspace_budget.async_host_slots == .async_compute.config.host_slots
        and .async_compute.pending == []
        and .detection.compute.failed_batches == 0' "$report" >/dev/null ||
        { printf 'H3 gate failed for %s\n' "$name"; exit 1; }
done
jq -e '.async_compute.config.device_slots == 3 and .async_compute.config.host_slots == 8
    and .workspace_budget.backoff_steps == 0' "$evidence_dir/slots_3_8-K1024/report.json" >/dev/null ||
    { printf 'default slot plan gate failed\n'; exit 1; }
jq -e '.async_compute.config.device_slots == 2 and .async_compute.config.host_slots == 16
    and .async_compute.device_in_flight_peak == 2' \
    "$evidence_dir/slots_2_16-K256/report.json" >/dev/null ||
    { printf '2/16 slot plan gate failed\n'; exit 1; }
# Pressure sheds the queue before GPU overlap or jobs, and never exceeds the host budget.
jq -e '.workspace_budget.backoff_steps > 0
    and .async_compute.config.host_slots < 32
    and .async_compute.config.device_slots == 3
    and .workspace_budget.matrix_jobs_effective == 512
    and (.workspace_budget.matrix_host_bytes + .workspace_budget.reserve_host_bytes) < 750000000' \
    "$evidence_dir/pressure_host-K1024/report.json" >/dev/null ||
    { printf 'memory pressure gate failed\n'; exit 1; }
jq -e '.cpu_share.gpus == 8 and .cpu_share.verifier_workers == 1
    and .async_compute.config.workers == 1' \
    "$evidence_dir/cpu_share_peers8-K1024/report.json" >/dev/null ||
    { printf 'CPU share gate failed\n'; exit 1; }

# Budget too small for even the smallest plan: INVALID with a report, never a bigger queue.
set +e
DIT_GPU_HOST_BUDGET_BYTES=100000000 "$bin" run --backend cuda --vram-mib 64 --seed 7 --no-ui \
    --matrix-jobs 512 --matrix-inner 1024 --async-compute --coverage-floor 0 --generations 1 \
    --output-dir "$evidence_dir/invalid_budget" >"$evidence_dir/invalid_budget.log" 2>&1
invalid_status=$?
"$bin" run --backend cuda --async-compute --async-device-slots 3 --async-host-slots 3 --generations 1 \
    --output-dir "$evidence_dir/rejected_flags" >"$evidence_dir/rejected_flags.log" 2>&1
rejected_status=$?
set -e
[[ $invalid_status -ne 0 && $rejected_status -ne 0 ]] ||
    { printf 'invalid budget or flags unexpectedly succeeded\n'; exit 1; }
jq -e '.verdict == "INVALID" and .hardware_tested == false' \
    "$evidence_dir/invalid_budget/report.json" >/dev/null ||
    { printf 'invalid budget gate failed\n'; exit 1; }
[[ ! -e $evidence_dir/rejected_flags/report.json ]] || { printf 'rejected flags produced a report\n'; exit 1; }

# Quarantine appears in the report of an injected failure and is released before the run ends.
set +e
"$bin" run --backend cuda --generations 3 --vram-mib 64 --matrix-jobs 256 --matrix-inner 64 \
    --seed 7 --no-ui --async-compute --coverage-floor 0 --inject-vulkan-fault matrix \
    --output-dir "$evidence_dir/negative-async" >"$evidence_dir/negative-async.log" 2>&1
[[ $? -ne 0 ]] || { printf 'negative run unexpectedly exited 0\n'; exit 1; }
set -e
jq -e '.verdict == "FAIL" and .async_compute.totals.quarantined == 1
    and .async_compute.totals.quarantine_released == 1
    and .async_compute.totals.quarantine_expired == 0
    and .async_compute.conservation == "ok" and .async_compute.pending == []' \
    "$evidence_dir/negative-async/report.json" >/dev/null ||
    { printf 'quarantine accounting gate failed\n'; exit 1; }

python3 tools/gpu_h3_summary.py "$evidence_dir"
printf 'H3 hardware gates PASS; evidence: %s\n' "$evidence_dir"
