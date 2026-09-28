#!/usr/bin/env bash
# GPU-H6 gate: software checks (lane planner, fair queue, token bucket, GPU-local pattern, CLI), then
# (DIT_GPU_HARDWARE=1) the overlapped transfer lane and GPU-local VRAM workload on the real GPU,
# with retained evidence and relative checks against time-slicing and against each workload alone.
#   DIT_GPU_HARDWARE=1 DIT_GPU_H6_SECONDS=15 tools/run_gpu_h6_checks.sh
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
crates=(-p gpu-engine -p gpu-vulkan -p gpu-cuda -p gpu-cli -p dit-system)
cargo fmt --all -- --check
cargo clippy "${crates[@]}" --all-targets -- -D warnings
cargo test "${crates[@]}" -- --test-threads=1

if [[ ${DIT_GPU_HARDWARE:-0} != 1 ]]; then
    printf 'H6 software checks PASS; hardware gates PENDING (set DIT_GPU_HARDWARE=1).\n'
    exit 0
fi
command -v jq >/dev/null
command -v python3 >/dev/null
cargo test -p gpu-cuda --test h6 -- --ignored --nocapture --test-threads=1
cargo test -p gpu-cuda --test h3 -- --ignored --nocapture --test-threads=1
cargo test -p gpu-cuda --test g3 -- --ignored --nocapture
cargo build --release -p gpu-cli
bin="$project_dir/target/release/gpu-vram-dit"
seconds=${DIT_GPU_H6_SECONDS:-15}
evidence_dir="$project_dir/build/gpu-h6-lane-$(date +%Y%m%d)"
[[ -e $evidence_dir ]] && evidence_dir="$evidence_dir-$(date +%H%M%S)"
mkdir -p "$evidence_dir"
printf 'Evidence retained at %s\n' "$evidence_dir"
{
    printf '{\n  "date": "%s",\n  "kernel": "%s",\n' "$(date -Is)" "$(uname -r)"
    printf '  "cpu": "%s",\n' "$(lscpu | sed -n 's/^Model name:[[:space:]]*//p')"
    printf '  "gpu": "%s",\n' "$(nvidia-smi --query-gpu=name,driver_version,pcie.link.gen.current,pcie.link.width.current --format=csv,noheader 2>/dev/null || echo unavailable)"
    printf '  "binary_sha256": "%s",\n' "$(sha256sum "$bin" | cut -d' ' -f1)"
    printf '  "steady_seconds": %s,\n  "note": "one run per cell, pool 64 MiB; not a near-full VRAM run; link may downshift when idle"\n}\n' "$seconds"
} >"$evidence_dir/environment.json"

run() { # name, shape (jobs:K), seconds, extra args...; a failing exit status is the caller's to judge
    local name=$1 shape=$2 secs=$3
    shift 3
    "$bin" run --backend cuda --vram-mib 64 --seed 7 --no-ui --async-compute \
        --matrix-jobs "${shape%%:*}" --matrix-inner "${shape##*:}" --duration-seconds "$secs" \
        --output-dir "$evidence_dir/$name" "$@" >"$evidence_dir/$name.log" 2>&1
}

# GPU-bound shape: every workload alone, time-sliced (floor 0 and the 0.25 default) and overlapped.
run compute_only 512:1024 "$seconds" --workload-profile compute-only
run transfer_only_seq 512:1024 "$seconds" --workload-profile transfer-only --no-transfer-overlap
run transfer_only_overlap 512:1024 "$seconds" --workload-profile transfer-only --transfer-overlap
run seq_lean 512:1024 "$seconds" --coverage-floor 0
run seq_default 512:1024 "$seconds" --no-transfer-overlap
run overlap 512:1024 "$seconds" --transfer-overlap
run default_on 512:1024 "$seconds"
run overlap_vram 512:1024 "$seconds" --transfer-overlap --vram-local
run rate_cap 512:1024 10 --workload-profile transfer-only --transfer-overlap --lane-rate-mib-s 300
# Verifier-bound shape: one verifier that hashes every output. The lane must not be starved and
# must not take compute's share.
run vb_seq 1024:16 "$seconds" --coverage-floor 0 --async-workers 1 --hash-policy every-batch
run vb_overlap 1024:16 "$seconds" --transfer-overlap --async-workers 1 --hash-policy every-batch
# Transfer-bound shape (H5 leftover, activated here): tiny jobs/K make the compute output
# dominate the device timeline over the kernel while enough device/host slots and verifier
# workers keep the backlog low, so the regime label is TRANSFER_BOUND rather than
# VERIFIER_BOUND. Compute-only: the label comes from the pipeline's own output D2H, not the lane.
# Generous slots/workers and a longer run: this classification is sensitive to how busy the
# machine already is (it followed straight after every scenario above), and a tighter margin
# was seen to fall back to mixed verifier/transfer-bound under that load.
run transfer_bound 64:4 30 --workload-profile compute-only \
    --async-device-slots 8 --async-host-slots 32 --async-workers 12 --async-workers-max 12
# Negative: an injected fault must fail the run, be attributed to its phase and be counted once.
set +e
run neg_transfer 512:1024 5 --transfer-overlap --inject-hardware-fault transfer
neg_transfer_status=$?
run neg_matrix 512:1024 5 --transfer-overlap --inject-hardware-fault matrix
neg_matrix_status=$?
set -e
[[ $neg_transfer_status -eq 1 && $neg_matrix_status -eq 1 ]] ||
    { printf 'injected faults must fail the run (statuses %s, %s)\n' "$neg_transfer_status" "$neg_matrix_status"; exit 1; }

python3 "$project_dir/tools/gpu_h6_summary.py" "$evidence_dir"
printf 'H6 gate PASS (evidence: %s)\n' "$evidence_dir"
