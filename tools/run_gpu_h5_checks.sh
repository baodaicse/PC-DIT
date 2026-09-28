#!/usr/bin/env bash
# GPU-H5 gate: software checks (fake-clock controller traces, scheduler, scaler, pressure), then
# (DIT_GPU_HARDWARE=1) controller behaviour on real regimes and the coverage-floor curve.
#   DIT_GPU_HARDWARE=1 DIT_GPU_H5_SECONDS=15 tools/run_gpu_h5_checks.sh
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
crates=(-p gpu-engine -p gpu-vulkan -p gpu-cuda -p gpu-cli -p dit-system)
cargo fmt --all -- --check
cargo clippy "${crates[@]}" --all-targets -- -D warnings
cargo test "${crates[@]}" -- --test-threads=1

if [[ ${DIT_GPU_HARDWARE:-0} != 1 ]]; then
    printf 'H5 software checks PASS; hardware gates PENDING (set DIT_GPU_HARDWARE=1).\n'
    exit 0
fi
command -v jq >/dev/null
cargo test -p gpu-cuda --test h3 -- --ignored --nocapture --test-threads=1
cargo test -p gpu-cuda --test g3 -- --ignored --nocapture
cargo build --release -p gpu-cli
bin="$project_dir/target/release/gpu-vram-dit"
seconds=${DIT_GPU_H5_SECONDS:-15}
evidence_dir="$project_dir/build/gpu-h5-control-$(date +%Y%m%d)"
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

run() { # name, shape (jobs:K), extra args...
    local name=$1 shape=$2
    shift 2
    "$bin" run --backend cuda --vram-mib 64 --seed 7 --no-ui --async-compute \
        --matrix-jobs "${shape%%:*}" --matrix-inner "${shape##*:}" \
        --output-dir "$evidence_dir/$name" "$@" >"$evidence_dir/$name.log" 2>&1
}
ops() { jq -r '.detection.compute.verified_integer_ops_per_second' "$evidence_dir/$1/report.json"; }

# Controller regimes. The verifier-bound shape has tiny K (little GPU work per output byte) and one
# verifier that hashes every output, so the outputs arrive faster than they can be checked.
run gpu_bound 512:1024 --duration-seconds "$seconds" --coverage-floor 0
run verifier_bound_1w 1024:16 --duration-seconds "$seconds" --coverage-floor 0 \
    --async-workers 1 --hash-policy every-batch
run verifier_bound_scaled 1024:16 --duration-seconds "$seconds" --coverage-floor 0 \
    --async-workers 1 --async-workers-max 4 --hash-policy every-batch

# Coverage floor curve and the default.
for floor in 0 0.25 0.5; do
    run "floor_$floor" 512:1024 --duration-seconds "$seconds" --coverage-floor "$floor"
done
# H6 made the overlapped lane the default for --async-compute; --no-transfer-overlap opts back
# into the time-sliced default this scenario measures.
run floor_default 512:1024 --duration-seconds "$seconds" --no-transfer-overlap

# Memory pressure: raised for a few seconds then lifted (the controller drains, then restarts
# gently), and pressure that never lifts (the run must stop, not throttle for ever).
pressure_file="$evidence_dir/pressure.flag"
printf 0 >"$pressure_file"
(
    sleep 4
    printf 1 >"$pressure_file"
    sleep 3
    printf 0 >"$pressure_file"
) &
writer=$!
DIT_GPU_PRESSURE_FILE="$pressure_file" run pressure_recovers 512:1024 \
    --duration-seconds 20 --coverage-floor 0
wait "$writer"
printf 1 >"$pressure_file"
set +e
DIT_GPU_PRESSURE_FILE="$pressure_file" run pressure_sustained 512:1024 \
    --duration-seconds 60 --coverage-floor 0 --stop-on-error
sustained_status=$?
set -e
[[ $sustained_status -ne 0 ]] || { printf 'sustained pressure unexpectedly passed\n'; exit 1; }

for report in "$evidence_dir"/gpu_bound/report.json "$evidence_dir"/verifier_bound_*/report.json \
    "$evidence_dir"/floor_*/report.json "$evidence_dir"/pressure_recovers/report.json; do
    name=$(basename "$(dirname "$report")")
    jq -e '.verdict == "PASS" and .async_compute.conservation == "ok"
        and .async_compute.unfinished_batches == 0
        and .async_compute.host_slots_used_peak <= .async_compute.config.host_slots
        and .async_compute.controller.in_flight_limit >= 1
        and .async_compute.controller.in_flight_limit <= .async_compute.config.device_slots
        and .async_compute.controller.state != "STOPPED"
        and (.async_compute.controller.decisions | length) <= 64
        and .detection.compute.failed_batches == 0' "$report" >/dev/null ||
        { printf 'H5 gate failed for %s\n' "$name"; exit 1; }
done

# GPU-bound: verifiers keep up, so the controller must never throttle (regression of the v0 flaw).
jq -e '.async_compute.controller.throttle_events == 0
    and .async_compute.controller.state == "BALANCED"
    and .async_compute.controller.in_flight_limit == .async_compute.config.device_slots
    and .async_compute.controller.pacing_ms == 0
    and .async_compute.backlog_fill_peak < 0.3
    and .async_compute.regimes.gpu_bound >= 0.9 * (.async_compute.bursts - .async_compute.empty_bursts)' \
    "$evidence_dir/gpu_bound/report.json" >/dev/null ||
    { printf 'GPU-bound controller gate failed\n'; exit 1; }
# Verifier-bound: throttled and paced, labelled as such, bounded, not flapping, nothing lost.
jq -e '.async_compute.controller.throttle_events >= 1
    and .async_compute.controller.pacing_ms > 0
    and .async_compute.regimes.verifier_bound >= 0.9 * (.async_compute.bursts - .async_compute.empty_bursts)
    and .async_compute.controller.state_changes <= 16
    and (.async_compute.controller.state_changes / (.active_elapsed_seconds / 60)) <= 60
    and .async_compute.totals.submitted == .async_compute.totals.verified + .async_compute.totals.failed
    and .async_compute.config.workers == 1 and .async_compute.workers.peak == 1' \
    "$evidence_dir/verifier_bound_1w/report.json" >/dev/null ||
    { printf 'verifier-bound controller gate failed\n'; exit 1; }
# Worker scaling: grows only under a verifier bound, stays within the ceiling, and pays off.
jq -e '.async_compute.workers.peak >= 2 and .async_compute.workers.peak <= .async_compute.config.workers_max
    and .async_compute.workers.increases >= 1' \
    "$evidence_dir/verifier_bound_scaled/report.json" >/dev/null ||
    { printf 'worker scaling gate failed\n'; exit 1; }
awk -v a="$(ops verifier_bound_scaled)" -v b="$(ops verifier_bound_1w)" 'BEGIN { exit !(a >= 1.10 * b) }' ||
    { printf 'extra verifier workers did not raise verified throughput by 10%%\n'; exit 1; }
# Coverage floor: the achieved share tracks the floor, coverage rises with it, compute stays alive.
for floor in 0.25 0.5; do
    jq -e --argjson f "$floor" '.coverage_schedule.floor == $f
        and ((.coverage_schedule.share - $f) | fabs) <= 0.05
        and .coverage_schedule.iterations > 10
        and .async_compute.totals.verified > 0' "$evidence_dir/floor_$floor/report.json" >/dev/null ||
        { printf 'coverage floor %s gate failed\n' "$floor"; exit 1; }
done
cov() { jq -r '.detection.coverage.vram_pcie_verified_mib_per_second' "$evidence_dir/$1/report.json"; }
awk -v a="$(cov floor_0)" -v b="$(cov floor_0.25)" -v c="$(cov floor_0.5)" \
    'BEGIN { exit !(a < b && b < c) }' ||
    { printf 'coverage does not rise with the floor\n'; exit 1; }
awk -v a="$(ops floor_0)" -v b="$(ops floor_0.25)" -v c="$(ops floor_0.5)" \
    'BEGIN { exit !(a > b && b > c && c > 0) }' ||
    { printf 'compute does not trade off smoothly against the floor\n'; exit 1; }
jq -e '.coverage_schedule.floor == 0.25' "$evidence_dir/floor_default/report.json" >/dev/null ||
    { printf 'default coverage floor gate failed\n'; exit 1; }
# Memory pressure drains and recovers; sustained pressure ends the run with a clear reason.
jq -e '.async_compute.memory_pressure_hits > 0
    and (.async_compute.controller.decisions | any(.reason == "host_memory_pressure"))
    and (.async_compute.controller.decisions | any(.reason == "recovering"))' \
    "$evidence_dir/pressure_recovers/report.json" >/dev/null ||
    { printf 'pressure recovery gate failed\n'; exit 1; }
jq -e '.verdict != "PASS"
    and (.failures[0].detail | contains("host_memory_pressure_sustained"))
    and .async_compute.controller.state == "STOPPED"' \
    "$evidence_dir/pressure_sustained/report.json" >/dev/null ||
    { printf 'sustained pressure gate failed\n'; exit 1; }

python3 tools/gpu_h5_summary.py "$evidence_dir"
printf 'H5 hardware gates PASS; evidence: %s\n' "$evidence_dir"
