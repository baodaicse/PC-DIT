#!/bin/bash
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UI="$PROJECT_ROOT/image/rootfs/usr/local/lib/cpu-dit/cpu-dit-ui.sh"
DIT="${1:-$PROJECT_ROOT/target/debug/dit-cli}"
PROFILE_CONFIG="$PROJECT_ROOT/image/rootfs/etc/cpu-dit/profiles.conf"
WATCHDOG="$PROJECT_ROOT/target/debug/dit-watchdog"
[[ -x "$WATCHDOG" ]] || { printf 'Build dit-watchdog before UI launcher tests\n' >&2; exit 1; }

[[ -x "$DIT" ]] || {
    printf 'Missing test binary: %s\n' "$DIT" >&2
    exit 1
}

test_dir="$(mktemp -d)"
trap 'rm -rf -- "$test_dir"' EXIT
progress_log="$test_dir/progress.log"
dashboard_output="$test_dir/dashboard.ansi"
compact_output="$test_dir/compact.ansi"
thermal_on_output="$test_dir/thermal-on.ansi"
two_frame_output="$test_dir/two-frame.ansi"
compact_two_frame_output="$test_dir/compact-two-frame.ansi"
menu_output="$test_dir/menu.ansi"

printf '%s\n' \
    'CPU_DIT_PROGRESS phase=RAM_VERIFY round=3 minimum_rounds=4 elapsed_ms=900000 duration_target_ms=1800000 target_bytes=6442450944 calculations_completed=1000000 failures_total=0 session_seed=0x0123456789abcdef round_seed=0xfedcba9876543210 seed_source=linux_random seed_commitment=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa window_recipe=CPU-DIT-WINDOW-v1 pattern_recipe=CPU-DIT-PATTERN-v1 active_window=MIXED_PARALLEL active_pattern=MULTI_FAMILY' \
    'CPU_DIT_COUNTER calculations_completed=123456789 failures_total=2' \
    >"$progress_log"

bash -n "$UI"
CPU_DIT_BINARY="$DIT" \
CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_AUTOSTART_SECONDS=0 \
    "$UI" --render-once >"$menu_output"
grep -Fq 'OFFLINE CPU + MEMORY DATA INTEGRITY TEST' "$menu_output"

CPU_DIT_BINARY="$DIT" \
CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_PROGRESS_FILE="$progress_log" \
CPU_DIT_UI_COLS=160 CPU_DIT_UI_ROWS=45 CPU_DIT_UI_AUTOSTART_SECONDS=0 \
    "$UI" --render-running-once >"$dashboard_output"

for expected in \
    'CPU-DIT-01 / DATA INTEGRITY TEST ENGINE' \
    'SYSTEM HARDWARE' \
    'LIVE UTILIZATION' \
    'TESTING / PROCESS INFORMATION' \
    'CPU USAGE' \
    'MEMORY' \
    'DIT TARGET' \
    'RAM VERIFY' \
    'SESSION SEED' \
    'ROUND SEED' \
    'CPU-DIT-WINDOW-v1' \
    'CPU-DIT-PATTERN-v1' \
    'MIXED_PARALLEL' \
    'MULTI_FAMILY' \
    'linux_random' \
    'CPU-RAM-EXACT-001' \
    'CALCULATIONS COMPLETED  123456789' \
    'FAIL TOTAL  2' \
    'TEMP LIMIT OFF'; do
    grep -Fq "$expected" "$dashboard_output" || {
        printf 'Dashboard is missing: %s\n' "$expected" >&2
        exit 1
    }
done
grep -Fq $'\033[1;38;5;82m' "$dashboard_output"
grep -Fq $'\033[1;38;5;45m' "$dashboard_output"

CPU_DIT_BINARY="$DIT" \
CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_TEMP_LIMIT_ENABLED=1 \
CPU_DIT_UI_PROGRESS_FILE="$progress_log" \
CPU_DIT_UI_COLS=160 CPU_DIT_UI_ROWS=45 CPU_DIT_UI_AUTOSTART_SECONDS=0 \
    "$UI" --render-running-once >"$thermal_on_output"
grep -Fq 'TEMP LIMIT ON (95 C, TRUSTED CPU SENSORS)' "$thermal_on_output"

CPU_DIT_BINARY="$DIT" \
CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_PROGRESS_FILE="$progress_log" \
CPU_DIT_UI_COLS=160 CPU_DIT_UI_ROWS=45 CPU_DIT_UI_AUTOSTART_SECONDS=0 \
    "$UI" --render-running-twice >"$two_frame_output"
clear_count="$(LC_ALL=C grep -ao $'\033\[2J' "$two_frame_output" | wc -l)"
[[ "$clear_count" == 1 ]] || {
    printf 'Two-frame dashboard cleared the display %s times instead of once\n' "$clear_count" >&2
    exit 1
}

CPU_DIT_BINARY="$DIT" \
CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_PROGRESS_FILE="$progress_log" \
CPU_DIT_UI_COLS=100 CPU_DIT_UI_ROWS=30 CPU_DIT_UI_AUTOSTART_SECONDS=0 \
    "$UI" --render-running-once >"$compact_output"
grep -Fq 'ACTIVE TEST' "$compact_output"
grep -Fq 'RUNNING' "$compact_output"
grep -Fq 'Calculations completed: 123456789 (117.73 MiB)  |  FAIL total: 2' "$compact_output"

CPU_DIT_BINARY="$DIT" \
CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_PROGRESS_FILE="$progress_log" \
CPU_DIT_UI_COLS=100 CPU_DIT_UI_ROWS=30 CPU_DIT_UI_AUTOSTART_SECONDS=0 \
    "$UI" --render-running-twice >"$compact_two_frame_output"
clear_count="$(LC_ALL=C grep -ao $'\033\[2J' "$compact_two_frame_output" | wc -l)"
[[ "$clear_count" == 1 ]] || {
    printf 'Two-frame compact UI cleared the display %s times instead of once\n' \
        "$clear_count" >&2
    exit 1
}

printf 'CPU-DIT dashboard render tests: PASS (wide + compact)\n'
[[ "$("$DIT" power-platform-watchdog-seconds --campaign)" == 16 ]]
[[ "$("$DIT" power-platform-watchdog-seconds --campaign --idle-ms 30000 --timeout-ms 120000)" == 305 ]]

CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
    "$UI" --render-once >"$test_dir/power-menu.ansi"
grep -Fq 'CPU AVAILABILITY / POWER PLATFORM' "$test_dir/power-menu.ansi"
grep -Fq 'START AVAILABILITY / POWER TEST' "$test_dir/power-menu.ansi"
grep -Fq 'ADVANCED' "$test_dir/power-menu.ansi"
! grep -Fq 'starts automatically' "$test_dir/power-menu.ansi"
printf '2bB' | CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
    "$UI" >"$test_dir/power-advanced.ansi"
grep -Fq 'CORE / SMT SWEEP' "$test_dir/power-advanced.ansi"
grep -Fq 'POWER TRANSITIONS' "$test_dir/power-advanced.ansi"
printf 'd1B' | CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
    "$UI" >"$test_dir/power-duration.ansi"
grep -Fq 'Minimum 00:02:00' "$test_dir/power-duration.ansi"
printf '%s\n' 'CPU_DIT_POWER_PROGRESS phase=BURST_ARMED completed=3 target=10 workers=2 bursts=25 failures=1 availability=Running power_platform=NoEvidence seed=0x1234' >"$test_dir/power.log"
CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_PROGRESS_FILE="$test_dir/power.log" \
CPU_DIT_UI_COLS=100 CPU_DIT_UI_ROWS=30 \
    "$UI" --render-running-twice >"$test_dir/power-running.ansi"
for expected in 'TRANSITIONS' '3 / 10' '30%' 'CPU USAGE [' 'RAM USAGE [' 'BURST_ARMED' 'FAIL TOTAL 1' 'AVAILABILITY' 'VCORE unavailable'; do
    grep -Fq "$expected" "$test_dir/power-running.ansi"
done
[[ "$(LC_ALL=C grep -ao $'\033\[2J' "$test_dir/power-running.ansi" | wc -l)" == 1 ]]
for geometry in '160 45' '110 32'; do
    read -r cols rows <<<"$geometry"
    CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
    CPU_DIT_UI_PROGRESS_FILE="$test_dir/power.log" \
    CPU_DIT_UI_COLS="$cols" CPU_DIT_UI_ROWS="$rows" \
        "$UI" --render-running-twice >"$test_dir/power-dashboard.ansi"
    for expected in 'SYSTEM HARDWARE' 'LIVE UTILIZATION' 'TESTING / PROCESS INFORMATION' \
        'CPU USAGE' 'MEMORY' 'PROGRESS' '30%' 'THREADS:' 'NUMA:' 'BOARD:' 'KERNEL:' \
        'TRANSITIONS 3 / 10' 'FAIL TOTAL 1' 'AVAILABILITY Running' 'POWER NoEvidence' \
        'VCORE unavailable' 'TEMP LIMIT OFF'; do
        grep -Fq "$expected" "$test_dir/power-dashboard.ansi" || {
            printf 'Power dashboard (%s) is missing: %s\n' "$geometry" "$expected" >&2
            exit 1
        }
    done
    ! grep -Fq 'DIT TARGET' "$test_dir/power-dashboard.ansi"
    [[ "$(LC_ALL=C grep -ao $'\033\[2J' "$test_dir/power-dashboard.ansi" | wc -l)" == 1 ]]
done
# No progress yet must render safely without inventing a transition target.
CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_PROGRESS_FILE="$test_dir/missing.log" CPU_DIT_TEMP_LIMIT_ENABLED=1 \
CPU_DIT_UI_COLS=110 CPU_DIT_UI_ROWS=32 \
    "$UI" --render-running-once >"$test_dir/power-preflight.ansi"
for expected in 'PREFLIGHT' 'TRANSITIONS 0 / planning' '0%' 'TEMP LIMIT ON (95 C)'; do
    grep -Fq "$expected" "$test_dir/power-preflight.ansi"
done
printf '%s\n' 'CPU_DIT_POWER_PROGRESS phase=BURST_ARMED completed=15 target=0 workers=8 bursts=25 failures=0 availability=Running power_platform=NoEvidence seed=0x1234 mode=campaign campaign_phase=CORE_SMT group=3 active_cpus=3,7 active_workers=2 kernel=mixed-oracle-v1 rounds=1 round_completed=3 round_target=12 elapsed_ms=600100 minimum_ms=600000 topology_complete=true idle_ms=220 burst_ms=160 timing=SHORT_LONG' >"$test_dir/campaign.log"
for geometry in '160 45' '110 32' '100 30'; do
    read -r cols rows <<<"$geometry"
    CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
    CPU_DIT_UI_PROGRESS_FILE="$test_dir/campaign.log" \
    CPU_DIT_UI_COLS="$cols" CPU_DIT_UI_ROWS="$rows" \
        "$UI" --render-running-twice >"$test_dir/campaign.ansi"
    for expected in 'CORE_SMT' 'MIN TIME' '100%' 'ROUND COVERAGE 3 / 12' 'ACTIVE CPUs 3,7' 'mixed-oracle-v1' 'Finishing current coverage round' 'Idle 220 ms / burst 160 ms / SHORT_LONG'; do
        grep -Fq "$expected" "$test_dir/campaign.ansi"
    done
    [[ "$(LC_ALL=C grep -ao $'\033\[2J' "$test_dir/campaign.ansi" | wc -l)" == 1 ]]
done
printf '%s\n' 'CPU_DIT_POWER_PROGRESS phase=ARM completed=1 target=0 workers=2 bursts=1 failures=0 availability=Running power_platform=NoEvidence seed=0x1234 mode=campaign campaign_phase=SINGLE_THREAD group=0 active_cpus=3 active_workers=1 kernel=heavy-fp-v1 rounds=0 round_completed=1 round_target=24 elapsed_ms=10 minimum_ms=600000 topology_complete=false' >"$test_dir/campaign-heavy.log"
CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" \
CPU_DIT_UI_PROGRESS_FILE="$test_dir/campaign-heavy.log" CPU_DIT_UI_COLS=110 CPU_DIT_UI_ROWS=32 \
    "$UI" --render-running-once >"$test_dir/campaign-heavy.ansi"
for expected in 'heavy-fp-v1' 'ROUND COVERAGE 1 / 24' 'INCOMPLETE TOPOLOGY' 'GROUP 0'; do
    grep -Fq "$expected" "$test_dir/campaign-heavy.ansi"
done
for profile in power-sweep power-campaign; do
for verdict in Pass Incomplete Fail NoFinal; do
    DIT_TEST_POWER_VERDICT="$verdict" CPU_DIT_POWER_UI=1 CPU_DIT_BINARY="$DIT" \
    CPU_DIT_PROFILE_CONFIG="$PROFILE_CONFIG" CPU_DIT_UI_RUN_DIR="$test_dir" \
    CPU_DIT_WATCHDOG_RESULTS_ROOT="$test_dir/no-persistent-results" \
    CPU_DIT_WATCHDOG_BINARY="$WATCHDOG" \
    CPU_DIT_RUNNER="$PROJECT_ROOT/tools/fixtures/power_ui_runner.sh" \
        "$UI" --run-profile-once "$profile" </dev/null >"$test_dir/power-$verdict.ansi"
    expected="${verdict^^}"
    [[ "$verdict" == NoFinal ]] && expected='INTERRUPTED / INVALID'
    sed -E 's/\x1B\[[0-9;]*[[:alpha:]]//g' "$test_dir/power-$verdict.ansi" >"$test_dir/power-result.txt"
    grep -Eq "\|[[:space:]]+$expected[[:space:]]+\|" "$test_dir/power-result.txt"
done
done
printf 'Power-platform menu, progress and supervised result tests: PASS\n'
