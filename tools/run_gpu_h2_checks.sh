#!/usr/bin/env bash
# GPU-H2 gate: software checks and verifier speed, then (DIT_GPU_HARDWARE=1) the A/B matrix.
#   DIT_GPU_HARDWARE=1 DIT_GPU_H2_SECONDS=20 tools/run_gpu_h2_checks.sh
# Compares against the H1 baseline when DIT_GPU_H1_BASELINE points at it (default: newest).
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"
crates=(-p gpu-engine -p gpu-vulkan -p gpu-cuda -p gpu-cli -p dit-system)
cargo fmt --all -- --check
cargo clippy "${crates[@]}" --all-targets -- -D warnings
cargo test "${crates[@]}" -- --test-threads=1

speed=$(cargo test --release -p gpu-engine -- --ignored --nocapture verifier_speed 2>&1)
printf '%s\n' "$speed" | grep -E 'buffer|verify_bytes|reference|speedup|corpus'
factor=$(printf '%s\n' "$speed" | sed -n 's/^speedup \([0-9.]*\)x.*/\1/p')
awk -v f="$factor" 'BEGIN { exit !(f >= 3.0) }' ||
    { printf 'verify_bytes speedup %sx is below the 3x floor\n' "$factor"; exit 1; }

if [[ ${DIT_GPU_HARDWARE:-0} != 1 ]]; then
    printf 'H2 software checks PASS; hardware A/B PENDING (set DIT_GPU_HARDWARE=1).\n'
    exit 0
fi
command -v jq >/dev/null
cargo test -p gpu-cuda --test g3 -- --ignored --nocapture
cargo build --release -p gpu-cli
bin="$project_dir/target/release/gpu-vram-dit"
seconds=${DIT_GPU_H2_SECONDS:-20}
evidence_dir="$project_dir/build/gpu-h2-ab-$(date +%Y%m%d)"
[[ -e $evidence_dir ]] && evidence_dir="$evidence_dir-$(date +%H%M%S)"
mkdir -p "$evidence_dir"
printf 'Evidence retained at %s\n' "$evidence_dir"
printf '%s\n' "$speed" >"$evidence_dir/verifier_speed.txt"
h1=${DIT_GPU_H1_BASELINE:-$(ls -d "$project_dir"/build/gpu-h1-baseline-* 2>/dev/null | sort | head -n1 || true)}

{
    printf '{\n  "date": "%s",\n  "kernel": "%s",\n' "$(date -Is)" "$(uname -r)"
    printf '  "cpu": "%s",\n' "$(lscpu | sed -n 's/^Model name:[[:space:]]*//p')"
    printf '  "gpu": "%s",\n' "$(nvidia-smi --query-gpu=name,driver_version,power.limit --format=csv,noheader 2>/dev/null || echo unavailable)"
    printf '  "binary_sha256": "%s",\n' "$(sha256sum "$bin" | cut -d' ' -f1)"
    printf '  "h1_baseline": "%s",\n' "${h1:-none}"
    printf '  "steady_seconds": %s,\n  "note": "one run per cell, pool 64 MiB; not a near-full VRAM run"\n}\n' "$seconds"
} >"$evidence_dir/environment.json"

declare -A jobs_for=([256]=1024 [1024]=512 [4096]=256)
run() { # name, extra args...
    local name=$1
    shift
    "$bin" run --backend cuda --vram-mib 64 --seed 7 --no-ui \
        --output-dir "$evidence_dir/$name" "$@" >"$evidence_dir/$name.log" 2>&1
}
for k in 256 1024 4096; do
    common=(--matrix-jobs "${jobs_for[$k]}" --matrix-inner "$k")
    for phase in cold steady; do
        length=(--generations 3)
        [[ $phase == steady ]] && length=(--duration-seconds "$seconds")
        run "seq_legacy-K$k-$phase" "${common[@]}" "${length[@]}" --hash-policy every-batch
        run "seq_lean-K$k-$phase" "${common[@]}" "${length[@]}" --hash-policy checkpoint
        run "async_hash_every-K$k-$phase" "${common[@]}" "${length[@]}" --async-compute --coverage-floor 0 \
            --hash-policy every-batch --corpus-rotate-batches 0
        run "async-K$k-$phase" "${common[@]}" "${length[@]}" --async-compute --coverage-floor 0
    done
done

for report in "$evidence_dir"/*/report.json; do
    name=$(basename "$(dirname "$report")")
    jq -e '.verdict == "PASS" and .detection.compute.failed_batches == 0
        and .detection.kernel_log.families.nvidia_xid == 0
        and .detection.kernel_log.families.device_lost == 0' "$report" >/dev/null ||
        { printf 'H2 gate failed for %s\n' "$name"; exit 1; }
    case $name in
    seq_legacy-*)
        jq -e '.hash_policy == "every_batch"' "$report" >/dev/null || { printf 'policy %s\n' "$name"; exit 1; } ;;
    seq_lean-*)
        jq -e '.hash_policy == "checkpoint:64"' "$report" >/dev/null || { printf 'policy %s\n' "$name"; exit 1; } ;;
    async_hash_every-*)
        jq -e '.hash_policy == "every_batch" and .async_compute.conservation == "ok"
            and .async_compute.hashes_computed == .async_compute.totals.verified
            and .async_compute.corpus_rotations == 0' "$report" >/dev/null ||
            { printf 'async every-batch accounting failed for %s\n' "$name"; exit 1; } ;;
    async-*)
        jq -e '.hash_policy == "checkpoint:64" and .async_compute.conservation == "ok"
            and .async_compute.unfinished_batches == 0
            and .async_compute.hashes_computed * 8 < .async_compute.totals.verified
            and .last_compute.result_sha256 != ""
            and .matrix_dispatches_completed == .async_compute.totals.verified' "$report" >/dev/null ||
            { printf 'async checkpoint accounting failed for %s\n' "$name"; exit 1; } ;;
    esac
done
# Steady runs are long enough to reach the first systematic pattern (generation 7).
for report in "$evidence_dir"/*-steady/report.json; do
    jq -e '.detection.coverage.pattern_schedule_version == 2
        and (.detection.coverage.pattern_classes | length) >= 2
        and .detection.coverage.pattern_classes.random > 0' "$report" >/dev/null ||
        { printf 'pattern schedule gate failed for %s\n' "$report"; exit 1; }
done
# The periodic host-corpus check ran and found nothing in every async steady run that had
# at least sixteen bursts, and no clean run may carry a failure attribution.
for k in 256 1024; do
    jq -e '.async_compute.integrity_checks >= 1 and .async_compute.integrity_failures == 0
        and .async_compute.failure_attribution == null' \
        "$evidence_dir/async-K$k-steady/report.json" >/dev/null ||
        { printf 'corpus integrity gate failed for K=%s\n' "$k"; exit 1; }
done
# A steady async run long enough for 256 batches must actually rotate, with prefetch hits.
for k in 256 1024; do
    jq -e '.async_compute.corpus_rotations >= 1
        and .async_compute.corpus_prefetch_hits == .async_compute.corpus_rotations' \
        "$evidence_dir/async-K$k-steady/report.json" >/dev/null ||
        { printf 'corpus rotation gate failed for K=%s\n' "$k"; exit 1; }
done

# Negative gate: an injected batch must still FAIL, be counted word by word, and be hashed.
set +e
"$bin" run --backend cuda --generations 3 --vram-mib 64 --matrix-jobs 256 --matrix-inner 64 \
    --seed 7 --no-ui --async-compute --coverage-floor 0 --inject-vulkan-fault matrix \
    --output-dir "$evidence_dir/negative-async" >"$evidence_dir/negative-async.log" 2>&1
negative_status=$?
set -e
[[ $negative_status -ne 0 ]] || { printf 'negative async run unexpectedly exited 0\n'; exit 1; }
jq -e '.verdict == "FAIL" and .async_compute.totals.failed == 1
    and .async_compute.mismatch_words == 256
    and (.async_compute.failure_hashes | length) == 1
    and (.async_compute.failure_hashes[0].actual_sha256 | length) == 64
    and .async_compute.integrity_checks == 1 and .async_compute.integrity_failures == 0
    and .async_compute.failure_attribution == "corpus_intact"
    and .async_compute.conservation == "ok"' \
    "$evidence_dir/negative-async/report.json" >/dev/null ||
    { printf 'negative async gate failed\n'; exit 1; }
# The sequential path keeps failing loudly under the lean policy too.
set +e
"$bin" run --backend cuda --generations 2 --vram-mib 64 --matrix-jobs 64 --matrix-inner 64 \
    --seed 7 --no-ui --hash-policy checkpoint --inject-vulkan-fault matrix \
    --output-dir "$evidence_dir/negative-seq-lean" >"$evidence_dir/negative-seq-lean.log" 2>&1
[[ $? -ne 0 ]] || { printf 'negative sequential run unexpectedly exited 0\n'; exit 1; }
set -e
jq -e '.verdict == "FAIL" and .failure_count >= 1' \
    "$evidence_dir/negative-seq-lean/report.json" >/dev/null ||
    { printf 'negative sequential gate failed\n'; exit 1; }

python3 tools/gpu_h2_summary.py "$evidence_dir" "$h1"
printf 'H2 hardware A/B PASS; evidence: %s\n' "$evidence_dir"
