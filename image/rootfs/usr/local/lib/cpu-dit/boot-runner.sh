#!/bin/bash
set -uo pipefail

export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root

mirror_console() {
    if [[ "${CPU_DIT_SERIAL_CONSOLE:-0}" == 1 \
        && -w /dev/tty1 && -w /dev/ttyS0 ]]; then
        exec > >(tee /dev/tty1 /dev/ttyS0) 2>&1
    elif [[ -w /dev/tty1 ]]; then
        exec >/dev/tty1 2>&1
    elif [[ "${CPU_DIT_SERIAL_CONSOLE:-0}" == 1 && -w /dev/ttyS0 ]]; then
        exec >/dev/ttyS0 2>&1
    fi
}

cmdline_value() {
    local key="$1" token
    for token in $(</proc/cmdline); do
        case "$token" in
            "$key"=*) printf '%s\n' "${token#*=}"; return 0 ;;
        esac
    done
    return 1
}

# find_boot_device / find_result_partition / mount_result_partition: shared with
# nvidia-driver-install.sh, which needs the same sibling-partition matching to find
# a burned-on driver package before driver-probe.sh runs.
. /usr/local/lib/cpu-dit/partition-lookup.sh

open_operator_shell() {
    echo
    echo "Operator shell is ready. Reports remain under: $RUN_DIR"
    echo "Use 'poweroff' when finished. Internal disks were not mounted by CPU-DIT."
    if [[ -r /dev/tty1 && -w /dev/tty1 ]]; then
        exec </dev/tty1 >/dev/tty1 2>&1 /bin/bash --noprofile --norc
    fi
    exec /bin/bash --noprofile --norc
}

SERIAL_SETTING="${CPU_DIT_SERIAL_CONSOLE:-$(cmdline_value cpu_dit_serial || true)}"
case "$SERIAL_SETTING" in
    1|on|ON|true|TRUE) CPU_DIT_SERIAL_CONSOLE=1 ;;
    0|off|OFF|false|FALSE) CPU_DIT_SERIAL_CONSOLE=0 ;;
    ""|auto|AUTO)
        if command -v systemd-detect-virt >/dev/null 2>&1 \
            && systemd-detect-virt --quiet; then
            CPU_DIT_SERIAL_CONSOLE=1
        else
            CPU_DIT_SERIAL_CONSOLE=0
        fi
        ;;
    *) CPU_DIT_SERIAL_CONSOLE=0 ;;
esac
export CPU_DIT_SERIAL_CONSOLE

if [[ "${CPU_DIT_UI_CHILD:-0}" != 1 ]]; then
    mirror_console
fi
clear 2>/dev/null || true
echo "============================================================"
echo " DIT-01 — CPU-RAM + CPU-SSD + GPU-VRAM / Ubuntu Noble Minimal Live"
echo " Root: read-only SquashFS + tmpfs overlay"
echo " Internal disks: never mounted by the CPU-DIT runner"
echo "============================================================"

swapoff -a 2>/dev/null || true
# Best-effort: install an NVIDIA driver package burned onto the DIT-RESULTS sibling
# partition (see docs/gpu-vram/NVIDIA_DRIVER_PACKAGE.md), before probing modules, so
# driver-probe.sh sees the result. A generic USB without that package, or hardware
# without an NVIDIA GPU, leaves this a fast no-op; the pre-baked NVIDIA image flavor
# (`/etc/cpu-dit/nvidia-driver-profile` present) always skips it.
/usr/local/lib/cpu-dit/nvidia-driver-install.sh || true
/usr/local/lib/cpu-dit/driver-probe.sh || true

PROFILE="${1:-$(cmdline_value cpu_dit_profile || true)}"
PROFILE="${PROFILE:-menu}"
. /etc/cpu-dit/profiles.conf
if [[ "$PROFILE" == menu ]]; then
    if [[ -x /usr/local/bin/dit-suite ]]; then
        MENU_ARGS=(menu)
        # Component fault ledger F3/F4 confirmation steps: off unless dit_attribution=1.
        if [[ "$(cmdline_value dit_attribution || true)" == 1 ]]; then
            MENU_ARGS+=(--attribution-checks)
        fi
        BOOT_PARTITION="$(find_boot_device || true)"
        RESULT_PARTITION="$(find_result_partition || true)"
        if [[ -n "$BOOT_PARTITION" ]]; then
            MENU_ARGS+=(--protected-device "$BOOT_PARTITION")
            BOOT_PARENT="$(lsblk -ndo PKNAME "$BOOT_PARTITION" 2>/dev/null || true)"
            if [[ -n "$BOOT_PARENT" && -b "/dev/$BOOT_PARENT" ]]; then
                MENU_ARGS+=(--protected-device "/dev/$BOOT_PARENT")
            fi
        fi
        if [[ -n "$RESULT_PARTITION" ]]; then
            MENU_ARGS+=(--protected-device "$RESULT_PARTITION")
        fi
        # Mount only the image-matched sibling result partition. CPU-SSD keeps all
        # live telemetry in /run and copies the closed report bundle here once.
        if mount_result_partition; then
            MENU_ARGS+=(--results-root /mnt/cpu-dit-results)
        else
            # This path deliberately does not exist, so the orchestrator retains
            # artifacts in tmpfs instead of writing through an unmounted directory.
            MENU_ARGS+=(--results-root /run/dit-suite-no-persistent-results)
        fi
        if [[ -r /dev/tty1 && -w /dev/tty1 ]]; then
            exec /usr/local/bin/dit-suite "${MENU_ARGS[@]}" </dev/tty1 >/dev/tty1 2>&1
        fi
        exec /usr/local/bin/dit-suite "${MENU_ARGS[@]}"
    fi
    if [[ -r /dev/tty1 && -w /dev/tty1 ]]; then
        exec /usr/local/lib/cpu-dit/cpu-dit-ui.sh </dev/tty1 >/dev/tty1 2>&1
    fi
    exec /usr/local/lib/cpu-dit/cpu-dit-ui.sh
fi
IMAGE_ID="$(</etc/cpu-dit/image-id)"
SESSION_ID="$(date -u +%Y%m%dT%H%M%SZ)-$(cat /proc/sys/kernel/random/uuid)"
TMP_RESULTS="/run/cpu-dit-results/$SESSION_ID"
mkdir -p "$TMP_RESULTS"
RUN_DIR="$TMP_RESULTS"

if mount_result_partition; then
    RUN_DIR="/mnt/cpu-dit-results/runs/$SESSION_ID"
    mkdir -p "$RUN_DIR"
    echo "Result target: verified USB sibling partition"
    echo "Result device: $(findmnt -nro SOURCE /mnt/cpu-dit-results)"
else
    echo "WARNING: verified USB result partition unavailable."
    echo "Reports will stay in tmpfs only; no internal disk fallback is allowed."
fi

echo "Image ID: $IMAGE_ID" | tee "$RUN_DIR/session.txt"
echo "Profile: $PROFILE" | tee -a "$RUN_DIR/session.txt"
echo "Minimum duration override: ${CPU_DIT_MIN_DURATION_SECONDS:-default}" \
    | tee -a "$RUN_DIR/session.txt"
TEMP_LIMIT_SETTING="${CPU_DIT_TEMP_LIMIT_ENABLED:-$TEMPERATURE_LIMIT_DEFAULT}"
case "$TEMP_LIMIT_SETTING" in
    1|on|ON|true|TRUE) TEMP_LIMIT_ENABLED=1 ;;
    0|off|OFF|false|FALSE) TEMP_LIMIT_ENABLED=0 ;;
    *)
        echo "Invalid temperature limit setting: $TEMP_LIMIT_SETTING" | tee -a "$RUN_DIR/session.txt"
        exit 3
        ;;
esac
echo "Temperature limit: $([[ "$TEMP_LIMIT_ENABLED" == 1 ]] && echo on || echo off) (${TEMPERATURE_LIMIT_C} C)" \
    | tee -a "$RUN_DIR/session.txt"
echo "Fault attribution: $([[ "$(cmdline_value dit_attribution || true)" == 1 ]] && echo on || echo off)" \
    | tee -a "$RUN_DIR/session.txt"
echo "Kernel: $(uname -r)" | tee -a "$RUN_DIR/session.txt"
echo "UTC start: $(date -u --iso-8601=seconds)" | tee -a "$RUN_DIR/session.txt"

{
    echo "=== uname ==="
    uname -a
    echo "=== lscpu ==="
    lscpu
    echo "=== memory ==="
    free -b
    echo "=== block devices (read-only inventory) ==="
    lsblk -o NAME,PATH,TYPE,SIZE,RO,RM,TRAN,LABEL,MODEL,SERIAL
    echo "=== kernel command line ==="
    cat /proc/cmdline
} >"$RUN_DIR/preflight.txt" 2>&1
cp /run/cpu-dit-driver-probe.txt "$RUN_DIR/driver-probe.txt" 2>/dev/null || true
cp /run/cpu-dit-nvidia-driver-install.txt "$RUN_DIR/nvidia-driver-install.txt" 2>/dev/null || true
/opt/cpu-dit-01/dit-cli sensors --json --interval-ms 250 \
    >"$RUN_DIR/sensor-report.json" 2>"$RUN_DIR/sensor-report.error" || true
if command -v sensors >/dev/null 2>&1; then
    sensors -u >"$RUN_DIR/libsensors.txt" 2>&1 || true
fi

if [[ "$PROFILE" == shell ]]; then
    echo "Shell-only profile selected; no DIT load was started."
    sync /mnt/cpu-dit-results 2>/dev/null || true
    if [[ "${CPU_DIT_RETURN_TO_UI:-0}" == 1 ]]; then
        exit 0
    fi
    open_operator_shell
fi

cd /opt/cpu-dit-01 || exit 1

if [[ "$PROFILE" == power || "$PROFILE" == power-sweep || "$PROFILE" == power-campaign ]]; then
    POWER_TRANSITIONS="${CPU_DIT_POWER_TRANSITIONS:-10}"
    if [[ ! "$POWER_TRANSITIONS" =~ ^[1-9][0-9]{0,3}$ ]] || (( POWER_TRANSITIONS > 1000 )); then
        echo 'INVALID: power transitions must be 1..1000 per group'
        exit 3
    fi
    POWER_ARGS=(power-platform-run --run-id "$SESSION_ID" --workers "$(nproc)"
        --checkpoint "$RUN_DIR/power-platform.json")
    if [[ "$PROFILE" == power-campaign ]]; then
        POWER_DURATION="${CPU_DIT_MIN_DURATION_SECONDS:-$(cmdline_value cpu_dit_power_duration || true)}"
        POWER_DURATION="${POWER_DURATION:-600}"
        if [[ ! "$POWER_DURATION" =~ ^[1-9][0-9]{0,4}$ ]] || (( POWER_DURATION > 86400 )); then
            echo 'INVALID: campaign minimum duration must be 1..86400 seconds'
            exit 3
        fi
        POWER_ARGS+=(--minimum-duration-seconds "$POWER_DURATION")
    else
        POWER_ARGS+=(--transitions "$POWER_TRANSITIONS")
    fi
    if [[ -n "${CPU_DIT_WATCHDOG_HEARTBEAT:-}" ]]; then
        POWER_ARGS+=(--progress-report "$CPU_DIT_WATCHDOG_HEARTBEAT")
    fi
    [[ "$PROFILE" == power-sweep ]] && POWER_ARGS+=(--core-sweep)
    [[ "$TEMP_LIMIT_ENABLED" == 1 ]] && POWER_ARGS+=(--enable-temperature-limit)
    ./dit-cli "${POWER_ARGS[@]}" 2>&1 | tee "$RUN_DIR/power-console.log"
    POWER_EXIT=${PIPESTATUS[0]}
    {
        echo "image_id=$IMAGE_ID"
        echo "profile=$PROFILE"
        echo "power_exit=$POWER_EXIT"
        echo "overall_exit=$POWER_EXIT"
    } | tee "$RUN_DIR/summary.txt"
    echo "Reports: $RUN_DIR"
    echo "CPU-DIT BOOT TEST COMPLETE overall_exit=$POWER_EXIT"
    sync "$RUN_DIR" 2>/dev/null || true
    exit "$POWER_EXIT"
fi

echo
echo "Running reference/golden route..."
set +e
timeout --foreground --signal=TERM --kill-after=15s 360s \
    ./dit-cli run --report "$RUN_DIR/reference-report.json" \
    2>&1 | tee "$RUN_DIR/reference-console.log"
REFERENCE_EXIT=${PIPESTATUS[0]}
set -e
if (( REFERENCE_EXIT == 124 || REFERENCE_EXIT == 137 )); then
    echo "HARD WATCHDOG: reference route exceeded 360 seconds." | tee -a "$RUN_DIR/reference-console.log"
fi

# CPU compute oracle (ADR-0004: dense FMA/AVX, integer branch predictor, AES-256 known-answer,
# each with its own oracle) runs on logical CPUs the RAM workers leave free, concurrently with
# the RAM rounds. Full gives it a quarter of the CPUs; the compute profile gives it all but one.
LOGICAL_CPUS="$(nproc)"
ORACLE_CPUS=$(( LOGICAL_CPUS / 4 ))
(( LOGICAL_CPUS >= 2 && ORACLE_CPUS < 1 )) && ORACLE_CPUS=1

SYSTEM_ARGS=(
    system-run
    --arena-mib 32
    --minimum-arena-mib 4
    --lock-policy best-effort
    --max-temperature-c "$TEMPERATURE_LIMIT_C"
    --monitor-interval-ms 250
    --progress
    --progress-report "${CPU_DIT_WATCHDOG_HEARTBEAT:-$RUN_DIR/system-progress.json}"
    --report "$RUN_DIR/system-report.json"
)
if [[ "$TEMP_LIMIT_ENABLED" == 1 ]]; then
    SYSTEM_ARGS+=(--enable-temperature-limit)
fi
# Fault Attribution boot entry (dit_attribution=1): a failing RAM word is re-checked from another
# CPU and rewritten, so the failure is counted on CPU or RAM.
if [[ "$(cmdline_value dit_attribution || true)" == 1 ]]; then
    SYSTEM_ARGS+=(--confirm-on-fail)
fi

case "$PROFILE" in
    safe)
        PROFILE_DURATION="${CPU_DIT_MIN_DURATION_SECONDS:-$SAFE_MINIMUM_DURATION_SECONDS}"
        [[ "$PROFILE_DURATION" =~ ^[0-9]+$ && "$PROFILE_DURATION" -ge 1 ]] || exit 3
        DEADLINE_SECONDS=$((PROFILE_DURATION + SAFE_DEADLINE_MARGIN_SECONDS))
        SYSTEM_ARGS+=(
            --workers 0
            --memory-mib "$SAFE_MEMORY_MIB"
            --reserve-mib "$SAFE_RESERVE_MIB"
            --rounds "$SAFE_MINIMUM_ROUNDS"
            --minimum-duration-seconds "$PROFILE_DURATION"
            --deadline-seconds "$DEADLINE_SECONDS"
        )
        HARD_SYSTEM_TIMEOUT=$((DEADLINE_SECONDS + 60))
        ;;
    full)
        PROFILE_DURATION="${CPU_DIT_MIN_DURATION_SECONDS:-$FULL_MINIMUM_DURATION_SECONDS}"
        [[ "$PROFILE_DURATION" =~ ^[0-9]+$ && "$PROFILE_DURATION" -ge 60 ]] || exit 3
        DEADLINE_SECONDS=$((PROFILE_DURATION + FULL_DEADLINE_MARGIN_SECONDS))
        if (( ORACLE_CPUS > 0 )); then
            SYSTEM_ARGS+=(
                --workers "$((LOGICAL_CPUS - ORACLE_CPUS))"
                --compute-oracle-workers "$ORACLE_CPUS"
            )
        else
            SYSTEM_ARGS+=(--workers 0)
        fi
        SYSTEM_ARGS+=(
            --safe-percent "$FULL_SAFE_PERCENT"
            --reserve-mib "$FULL_RESERVE_MIB"
            --rounds "$FULL_MINIMUM_ROUNDS"
            --minimum-duration-seconds "$PROFILE_DURATION"
            --deadline-seconds "$DEADLINE_SECONDS"
        )
        HARD_SYSTEM_TIMEOUT=$((DEADLINE_SECONDS + 60))
        ;;
    compute)
        # CPU compute stress: a small exact RAM round on one CPU, the compute oracle on every other
        # CPU, alternating short idle and full bursts to add P-state/voltage transitions.
        if (( LOGICAL_CPUS < 2 )); then
            echo "CPU COMPUTE needs at least 2 logical CPUs (one keeps the RAM round); found $LOGICAL_CPUS." \
                | tee "$RUN_DIR/invalid-profile.txt"
            exit 3
        fi
        PROFILE_DURATION="${CPU_DIT_MIN_DURATION_SECONDS:-$FULL_MINIMUM_DURATION_SECONDS}"
        [[ "$PROFILE_DURATION" =~ ^[0-9]+$ && "$PROFILE_DURATION" -ge 60 ]] || exit 3
        DEADLINE_SECONDS=$((PROFILE_DURATION + SAFE_DEADLINE_MARGIN_SECONDS))
        SYSTEM_ARGS+=(
            --workers 1
            --compute-oracle-workers "$((LOGICAL_CPUS - 1))"
            --compute-oracle-idle-ramp
            --memory-mib "$SAFE_MEMORY_MIB"
            --reserve-mib "$SAFE_RESERVE_MIB"
            --rounds "$SAFE_MINIMUM_ROUNDS"
            --minimum-duration-seconds "$PROFILE_DURATION"
            --deadline-seconds "$DEADLINE_SECONDS"
        )
        HARD_SYSTEM_TIMEOUT=$((DEADLINE_SECONDS + 60))
        ;;
    *)
        echo "Unknown cpu_dit_profile '$PROFILE'; refusing to guess." | tee "$RUN_DIR/invalid-profile.txt"
        sync /mnt/cpu-dit-results 2>/dev/null || true
        if [[ "${CPU_DIT_RETURN_TO_UI:-0}" == 1 ]]; then
            exit 3
        fi
        open_operator_shell
        ;;
esac

echo
echo "Running system profile: $PROFILE"
echo "Minimum stress duration: ${PROFILE_DURATION}s; test stops only after a complete round."
set +e
timeout --foreground --signal=TERM --kill-after=30s "${HARD_SYSTEM_TIMEOUT}s" \
    ./dit-cli "${SYSTEM_ARGS[@]}" 2>&1 | tee "$RUN_DIR/system-console.log"
SYSTEM_EXIT=${PIPESTATUS[0]}
set -e
if (( SYSTEM_EXIT == 124 || SYSTEM_EXIT == 137 )); then
    echo "HARD WATCHDOG: system route exceeded ${HARD_SYSTEM_TIMEOUT} seconds." \
        | tee -a "$RUN_DIR/system-console.log"
fi

OVERALL_EXIT=0
if (( REFERENCE_EXIT != 0 || SYSTEM_EXIT != 0 )); then
    OVERALL_EXIT=2
fi

SYSTEM_REPORT_WRITTEN=0
[[ -s "$RUN_DIR/system-report.json" ]] && SYSTEM_REPORT_WRITTEN=1
if (( SYSTEM_REPORT_WRITTEN == 0 && SYSTEM_EXIT != 0 )); then
    echo "No final system-report.json (crash or hang); see system-progress.json for the last reached phase." \
        | tee -a "$RUN_DIR/system-console.log"
fi

if [[ -n "${CPU_DIT_PREFLIGHT_EXPORT:-}" ]]; then
    case "$CPU_DIT_PREFLIGHT_EXPORT" in
        /run/dit-suite/*.json)
            mkdir -p /run/dit-suite
            PREFLIGHT_VERDICT=INVALID
            if (( SYSTEM_REPORT_WRITTEN == 1 )); then
                if (( OVERALL_EXIT == 0 )) \
                    && grep -q '"status"[[:space:]]*:[[:space:]]*"PASS"' \
                        "$RUN_DIR/system-report.json"; then
                    PREFLIGHT_VERDICT=PASS
                else
                    PREFLIGHT_VERDICT=FAIL
                fi
            fi
            if (set -o noclobber; printf '%s\n' \
                '{' \
                '  "schema_version": 1,' \
                '  "module_id": "dit.cpu-ram",' \
                "  \"run_id\": \"$SESSION_ID\"," \
                "  \"verdict\": \"$PREFLIGHT_VERDICT\"" \
                '}' >"$CPU_DIT_PREFLIGHT_EXPORT"); then
                echo "CPU-RAM preflight evidence: $CPU_DIT_PREFLIGHT_EXPORT"
            else
                echo "Refusing to overwrite CPU-RAM preflight evidence: $CPU_DIT_PREFLIGHT_EXPORT"
                OVERALL_EXIT=3
            fi
            ;;
        *)
            echo "Invalid CPU_DIT_PREFLIGHT_EXPORT path; only /run/dit-suite/*.json is allowed."
            OVERALL_EXIT=3
            ;;
    esac
fi

{
    echo "image_id=$IMAGE_ID"
    echo "profile=$PROFILE"
    echo "reference_exit=$REFERENCE_EXIT"
    echo "system_exit=$SYSTEM_EXIT"
    echo "system_report_written=$SYSTEM_REPORT_WRITTEN"
    echo "overall_exit=$OVERALL_EXIT"
    echo "utc_end=$(date -u --iso-8601=seconds)"
} | tee "$RUN_DIR/summary.txt"

sync /mnt/cpu-dit-results 2>/dev/null || true
echo
echo "CPU-DIT BOOT TEST COMPLETE overall_exit=$OVERALL_EXIT"
echo "Reports: $RUN_DIR"
if [[ "${CPU_DIT_RETURN_TO_UI:-0}" == 1 ]]; then
    exit "$OVERALL_EXIT"
fi
open_operator_shell
