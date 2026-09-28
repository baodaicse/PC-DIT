#!/bin/bash
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIT_BINARY="${1:-$ROOT_DIR/target/release/dit-cli}"
OUTPUT_DIR="${2:-$ROOT_DIR/build/validation-negative-campaign}"

if [[ ! -x "$DIT_BINARY" ]]; then
    echo "CPU-DIT binary is not executable: $DIT_BINARY" >&2
    exit 3
fi

mkdir -p "$OUTPUT_DIR"
cd "$ROOT_DIR" || exit 3

COMMON_ARGS=(
    system-run
    --memory-mib 64
    --reserve-mib 512
    --arena-mib 16
    --minimum-arena-mib 4
    --rounds 1
    --workers 2
    --lock-policy best-effort
    --deadline-seconds 60
    --monitor-interval-ms 100
)

campaign_failed=0

run_fault() {
    local name="$1"
    shift
    local exit_code

    echo "Running negative gate: $name"
    set +e
    "$DIT_BINARY" "${COMMON_ARGS[@]}" \
        --report "$OUTPUT_DIR/$name.json" "$@" \
        >"$OUTPUT_DIR/$name.console.log" 2>&1
    exit_code=$?
    set -e

    printf 'expected_exit=2 actual_exit=%s\n' "$exit_code" \
        >"$OUTPUT_DIR/$name.exit.txt"
    if (( exit_code != 2 )); then
        echo "FAIL: $name returned $exit_code instead of 2" >&2
        campaign_failed=1
    else
        echo "PASS: $name was detected"
    fi
}

run_fault ram_global_offset --inject-global-offset 0
run_fault simd_worker --inject-simd-worker 0
run_fault worker_crash --inject-worker-crash 0
run_fault deadline_timeout --inject-timeout
run_fault synthetic_temperature --enable-temperature-limit --inject-temperature-c 100
run_fault synthetic_edac_ue --inject-edac-uncorrected

if (( campaign_failed != 0 )); then
    echo "NEGATIVE_CAMPAIGN_FAIL"
    exit 2
fi

echo "NEGATIVE_CAMPAIGN_PASS"
echo "Evidence: $OUTPUT_DIR"
