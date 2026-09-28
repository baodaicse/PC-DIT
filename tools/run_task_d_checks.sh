#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

echo "[1/5] rustfmt, clippy and workspace tests"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

validation_dir=$(mktemp -d /tmp/cpu-nvme-task-d-check.XXXXXX)
created_files=()
cleanup() {
    if ((${#created_files[@]})); then
        rm -f -- "${created_files[@]}"
    fi
    rmdir -- "$validation_dir"
}
trap cleanup EXIT

echo "[2/5] positive scheduler smoke for 1, 2 and 4 devices"
for count in 1 2 4; do
    report_file="$validation_dir/multi-$count.report.json"
    events_file="$validation_dir/multi-$count.events.jsonl"
    created_files+=("$report_file" "$events_file")
    cargo run -q -p nvme-cli -- multi-simulate \
        --devices "$count" \
        --duration-seconds 1 \
        --operations 4 \
        --capacity-mib 2 \
        --memory-budget-mib 16 \
        --max-workers 2 \
        --no-ui \
        --events "$events_file" \
        --report "$report_file"
    rg -q '"aggregate_verdict": "PASS"' "$report_file"
    rg -q "\"selected_devices\": $count" "$report_file"
    test "$(rg -c '"type":"completed"' "$events_file")" -eq "$count"
done

echo "[3/5] isolated failure attribution"
negative_report="$validation_dir/multi-negative.report.json"
negative_events="$validation_dir/multi-negative.events.jsonl"
created_files+=("$negative_report" "$negative_events")
set +e
cargo run -q -p nvme-cli -- multi-simulate \
    --devices 3 \
    --duration-seconds 1 \
    --operations 8 \
    --capacity-mib 2 \
    --memory-budget-mib 16 \
    --inject-device 2 \
    --inject bit-flip \
    --inject-at 3 \
    --no-ui \
    --events "$negative_events" \
    --report "$negative_report"
negative_status=$?
set -e
test "$negative_status" -eq 1
rg -q '"aggregate_verdict": "FAIL"' "$negative_report"
test "$(rg -o '"verdict": "PASS"' "$negative_report" | wc -l)" -eq 2
test "$(rg -o '"verdict": "FAIL"' "$negative_report" | wc -l)" -eq 1

echo "[4/5] dynamic layout, picker and no-full-clear UI tests"
cargo test -q -p dit-ui-core

echo "[5/5] host inventory remains read-only"
inventory_file="$validation_dir/inventory.json"
created_files+=("$inventory_file")
cargo run -q -p nvme-cli -- inventory --json --no-smart >"$inventory_file"
rg -q '"devices"' "$inventory_file"

echo "TASK-D CHECK: PASS"
