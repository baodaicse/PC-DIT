#!/usr/bin/env bash
set -euo pipefail

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$project_dir"

echo "[1/5] rustfmt"
cargo fmt --all -- --check

echo "[2/5] clippy"
cargo clippy --workspace --all-targets -- -D warnings

echo "[3/5] workspace tests"
cargo test --workspace

echo "[4/5] read-only host inventory"
validation_dir=$(mktemp -d /tmp/cpu-nvme-task-b-check.XXXXXX)
inventory_file="$validation_dir/inventory.json"
selection_file="$validation_dir/selection.json"
cleanup() {
    rm -f -- "$inventory_file" "$selection_file"
    rmdir -- "$validation_dir"
}
trap cleanup EXIT

cargo run -q -p nvme-cli -- inventory --json --no-smart >"$inventory_file"
rg -q '"schema_version": 1' "$inventory_file"

echo "[5/5] stable-ID selection and revalidation"
if rg -q '"index": [0-9]+' "$inventory_file"; then
    cargo run -q -p nvme-cli -- select --devices all --no-smart --output "$selection_file"
    cargo run -q -p nvme-cli -- revalidate --plan "$selection_file" --no-smart
    rg -q '"stable_id": "nvme:' "$selection_file"
else
    echo "SKIP host selection: no NVMe namespace detected; fixture coverage still passed"
fi

echo "TASK-B CHECK: PASS"

