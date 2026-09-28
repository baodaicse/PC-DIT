#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

echo "[1/5] rustfmt and clippy"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

echo "[2/5] workspace tests"
cargo test --workspace

echo "[3/5] simulator telemetry/report smoke"
validation_dir=$(mktemp -d /tmp/cpu-nvme-task-c-check.XXXXXX)
inventory_file="$validation_dir/inventory.json"
selection_file="$validation_dir/selection.json"
plan_file="$validation_dir/bounded.json"
events_file="$validation_dir/events.jsonl"
report_file="$validation_dir/report.json"
prepare_log="$validation_dir/prepare.log"
cleanup() {
    rm -f -- "$inventory_file" "$selection_file" "$plan_file" "$events_file" "$report_file" "$prepare_log"
    rmdir -- "$validation_dir"
}
trap cleanup EXIT

cargo run -q -p nvme-cli -- simulate \
    --duration-seconds 1 \
    --operations 8 \
    --capacity-mib 8 \
    --events "$events_file" \
    --report "$report_file"
rg -q '"type":"throughput"' "$events_file"
rg -q '"bytes_read"' "$report_file"
rg -q '"completion_reason"' "$report_file"
rg -q '"performance"' "$report_file"

echo "[4/5] host inventory remains read-only"
cargo run -q -p nvme-cli -- inventory --json --no-smart >"$inventory_file"

echo "[5/5] protected system-target refusal"
if test -b /dev/nvme0n1 && rg -q 'contains the running root filesystem' "$inventory_file"; then
    cargo run -q -p nvme-cli -- select \
        --devices 1 \
        --no-smart \
        --json \
        --output "$selection_file" >/dev/null
    set +e
    cargo run -q -p nvme-cli -- prepare-bounded \
        --selection "$selection_file" \
        --device 1 \
        --target /dev/nvme0n1 \
        --range-size-mib 64 \
        --write-budget-mib 16 \
        --duration-seconds 60 \
        --output "$plan_file" >"$prepare_log" 2>&1
    prepare_status=$?
    set -e
    test "$prepare_status" -ne 0
    test ! -e "$plan_file"
    rg -q 'protected or not a write candidate' "$prepare_log"
else
    echo "SKIP host root-NVMe refusal: /dev/nvme0n1 is not the running root target"
fi

echo "TASK-C CHECK: PASS"

