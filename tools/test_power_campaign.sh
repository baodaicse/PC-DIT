#!/bin/bash
set -Eeuo pipefail
project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dit="${1:-$project_dir/target/debug/dit-cli}"
watchdog="$project_dir/target/debug/dit-watchdog"
command -v jq >/dev/null
[[ -x "$dit" && -x "$watchdog" ]]
mkdir -p "$project_dir/build"
test_dir="$(mktemp -d "$project_dir/build/power-host-XXXXXX")"
budget="$("$dit" power-platform-watchdog-seconds --campaign --idle-ms 1 --timeout-ms 1000)"
for run in 1 2; do
    "$watchdog" supervise --module cpu-power-platform --run-id "power-u5u6-$run" \
        --run-dir "$test_dir/watchdog-$run" --heartbeat "$test_dir/heartbeat-$run.json" \
        --stale-after-seconds "$budget" --kill-after-seconds 5 -- \
        "$dit" power-platform-run --run-id "power-u5u6-$run" --workers 2 \
        --minimum-duration-seconds 5 --idle-ms 1 --burst-ms 1 --timeout-ms 1000 \
        --fma-iterations 10 --max-start-spread-ms 500 --seed 123 \
        --checkpoint "$test_dir/report-$run.json" --progress-report "$test_dir/heartbeat-$run.json" \
        >"$test_dir/console-$run.log" 2>&1
    jq -e '.schema_version == 3 and .verdict == "PASS" and .pool_workers_started == 2
        and .total_transitions == .verified_transitions
        and .total_transitions == ((.transitions | length) + .transitions_omitted)
        and (.transitions | length) <= 128 and .campaign.elapsed_ms >= 5000
        and .campaign.completed_in_round == 0
        and ([.campaign.steps[].timing_class] | unique | length) == 4' "$test_dir/report-$run.json" >/dev/null
    jq -S '.campaign.steps' "$test_dir/report-$run.json" >"$test_dir/plan-$run.json"
    jq -e '.kind == "power-platform-progress" and .last_phase == "FINAL"' "$test_dir/heartbeat-$run.json" >/dev/null
done
cmp "$test_dir/plan-1.json" "$test_dir/plan-2.json"
printf 'Seeded campaign + bounded report + supervised heartbeat PASS: %s\n' "$test_dir"
