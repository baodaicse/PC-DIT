#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

echo "[1/5] rustfmt"
cargo fmt --all -- --check

echo "[2/5] clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "[3/5] unit and doc tests"
cargo test --workspace

echo "[4/5] positive and negative campaign"
cargo run -q -p nvme-cli -- campaign --operations 16

echo "[5/5] regular-file smoke and report validation"
validation_dir=$(mktemp -d /tmp/cpu-nvme-task-a-check.XXXXXX)
events_file="$validation_dir/events.jsonl"
report_file="$validation_dir/report.json"
cleanup() {
    rm -f -- "$events_file" "$report_file"
    rmdir -- "$validation_dir"
}
trap cleanup EXIT

cargo run -q -p nvme-cli -- simulate \
    --duration-seconds 3600 \
    --operations 40 \
    --capacity-mib 64 \
    --events "$events_file" \
    --report "$report_file"

test -s "$events_file"
test -s "$report_file"
rg -q '"verdict": "PASS"' "$report_file"
rg -q '"failure_count": 0' "$report_file"
rg -q '"seed_slots": 20' "$report_file"

echo "TASK-A CHECK: PASS"

