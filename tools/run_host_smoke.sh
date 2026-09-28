#!/bin/bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIT_BINARY="${1:-$ROOT_DIR/target/release/dit-cli}"
OUTPUT_DIR="${2:-$ROOT_DIR/build/host-smoke}"
UI="$ROOT_DIR/image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh"
PROFILE_CONFIG="$ROOT_DIR/image/rootfs/etc/cpu-dit/profiles.conf"

if [[ ! -x "$DIT_BINARY" ]]; then
    printf 'CPU-DIT binary is not executable: %s\n' "$DIT_BINARY" >&2
    exit 3
fi

mkdir -p "$OUTPUT_DIR"
cd "$ROOT_DIR"

printf '[CPU-DIT host] Running UI startup regression\n'
CPU_DIT_BINARY="$DIT_BINARY" \
CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_AUTOSTART_SECONDS=0 \
    "$UI" --render-once >"$OUTPUT_DIR/ui-startup.ansi"
grep -Fq 'OFFLINE CPU + MEMORY DATA INTEGRITY TEST' \
    "$OUTPUT_DIR/ui-startup.ansi"

printf '[CPU-DIT host] Running reference route\n'
"$DIT_BINARY" run \
    --report "$OUTPUT_DIR/reference-report.json" \
    >"$OUTPUT_DIR/reference-console.log" 2>&1

printf '[CPU-DIT host] Running short positive CPU/RAM route\n'
"$DIT_BINARY" system-run \
    --memory-mib 128 \
    --reserve-mib 512 \
    --arena-mib 16 \
    --minimum-arena-mib 4 \
    --rounds 2 \
    --minimum-duration-seconds 2 \
    --workers 2 \
    --lock-policy best-effort \
    --deadline-seconds 60 \
    --monitor-interval-ms 100 \
    --report "$OUTPUT_DIR/system-report.json" \
    >"$OUTPUT_DIR/system-console.log" 2>&1

python3 - "$OUTPUT_DIR/reference-report.json" "$OUTPUT_DIR/system-report.json" <<'PY'
import json
import sys

reference_path, system_path = sys.argv[1:]
with open(reference_path, encoding="utf-8") as stream:
    reference = json.load(stream)
with open(system_path, encoding="utf-8") as stream:
    system = json.load(stream)

checks = [
    (reference.get("schema_version") == 1, "reference schema is not v1"),
    (reference.get("status") == "PASS", "reference route did not PASS"),
    (system.get("schema_version") == 10, "system schema is not v10"),
    (system.get("status") == "PASS", "system route did not PASS"),
    (system.get("rounds_completed", 0) >= 2, "system route completed fewer than two rounds"),
    (system.get("round_seeds_unique") is True, "round seed rotation is not unique"),
    (system.get("duration_target_met") is True, "duration gate was not met"),
    (system.get("calculations_completed", 0) > 0, "calculation counter is empty"),
    (system.get("failures_total") == 0, "positive smoke recorded a failure"),
    (system.get("safety", {}).get("violations") == [], "unexpected safety violation"),
    (system.get("execution", {}).get("worker_errors") == [], "worker error detected"),
]
errors = [message for passed, message in checks if not passed]
if errors:
    for message in errors:
        print(f"HOST_SMOKE_FAIL: {message}", file=sys.stderr)
    raise SystemExit(2)
PY

printf 'HOST_SMOKE_PASS\n'
printf 'Evidence: %s\n' "$OUTPUT_DIR"
