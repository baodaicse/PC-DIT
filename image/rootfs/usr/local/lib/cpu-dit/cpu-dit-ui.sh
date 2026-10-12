#!/bin/bash
set -uo pipefail

export TERM="${TERM:-linux}"
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

DIT="${CPU_DIT_BINARY:-/opt/cpu-dit-01/dit-cli}"
RUNNER="${CPU_DIT_RUNNER:-/usr/local/lib/cpu-dit/boot-runner.sh}"
WATCHDOG="${CPU_DIT_WATCHDOG_BINARY:-/usr/local/bin/dit-watchdog}"
# Shared dashboard renderer (`dit-suite cpu-ram-view`): the running screen of CPU-RAM tests is
# drawn by the same Rust renderer as CPU-SSD, GPU-VRAM and COMBINED. Without it, the screen
# below (draw_running) is the fallback.
SUITE="${CPU_DIT_SUITE_BINARY:-/usr/local/bin/dit-suite}"
# Menus and result screens are drawn by the same renderer too (`dit-suite option-screen`), when
# this dit-suite has it; otherwise the script draws them itself.
suite_screens=0
if [[ -x "$SUITE" ]] && "$SUITE" option-screen --help >/dev/null 2>&1; then
    suite_screens=1
fi

# shared_screen TITLE ROW...: clears the screen and draws an option screen in the shared look
# (title band, rows; a row '---' starts a new section). Returns 1 when it cannot.
shared_screen() {
    (( suite_screens )) || return 1
    local title="$1" row
    shift
    local -a screen_args=(option-screen --title "$title")
    for row in "$@"; do screen_args+=(--row "$row"); done
    "$SUITE" "${screen_args[@]}"
}

# shared_prompt TEXT: the prompt under a shared screen (no script background colour).
shared_prompt() {
    printf '%s%s%s' "$c_cyan" "$1" "$c_reset"
}
WATCHDOG_RESULTS_ROOT="${CPU_DIT_WATCHDOG_RESULTS_ROOT:-/mnt/cpu-dit-results}"
AUTO_SECONDS="${CPU_DIT_UI_AUTOSTART_SECONDS:-10}"
PROFILE_CONFIG="${CPU_DIT_PROFILE_CONFIG:-/etc/cpu-dit/profiles.conf}"
UI_RUN_DIR="${CPU_DIT_UI_RUN_DIR:-/run}"
UI_REFRESH_SECONDS="${CPU_DIT_UI_REFRESH_SECONDS:-0.25}"
SENSOR_REFRESH_TICKS="${CPU_DIT_SENSOR_REFRESH_TICKS:-1}"
POWER_UI="${CPU_DIT_POWER_UI:-0}"
power_transitions=10
power_duration_seconds=600
if [[ "$POWER_UI" == 1 ]]; then AUTO_SECONDS=0; fi

if [[ -r "$PROFILE_CONFIG" ]]; then
    . "$PROFILE_CONFIG"
else
    SAFE_MEMORY_MIB=256
    SAFE_MINIMUM_ROUNDS=2
    SAFE_MINIMUM_DURATION_SECONDS=120
    FULL_SAFE_PERCENT=95
    FULL_MINIMUM_ROUNDS=4
    FULL_MINIMUM_DURATION_SECONDS=1800
    TEMPERATURE_LIMIT_DEFAULT=off
    TEMPERATURE_LIMIT_C=95
fi
full_duration_seconds="$FULL_MINIMUM_DURATION_SECONDS"
active_run_pid=''
# 1 while active_run_pid is the watchdog itself (it forwards a graceful stop
# to the worker's own process group); 0 when active_run_pid is the worker.
active_run_uses_watchdog=0
# Set by the INT trap so the menu read-failure fallback can tell a Ctrl+C
# apart from a real EOF (lost console), which must still trigger poweroff.
sigint_seen=0

esc=$'\033'
c_reset="${esc}[0m"
c_bg="${esc}[48;5;17m"
c_dashboard_bg="${esc}[40m"
c_white="${esc}[97m"
c_muted="${esc}[38;5;110m"
c_cyan="${esc}[1;38;5;45m"
c_blue="${esc}[1;38;5;39m"
c_green="${esc}[1;38;5;82m"
c_yellow="${esc}[1;38;5;220m"
c_red="${esc}[1;38;5;203m"

# Frame and bar glyphs, as the shared Rust renderer draws them: box drawing and blocks on the
# Linux console (UTF-8, glyphs in the kernel's built-in font) and the usual terminals; plain ASCII
# on a serial line or with DIT_UI_ASCII=1. In UTF-8 mode bash must count characters, not bytes,
# for its padded fields, hence C.UTF-8 (built into glibc).
ui_unicode=0
if [[ "${DIT_UI_ASCII:-0}" == 0 ]]; then
    case "${TERM:-}" in
        linux|xterm*|screen*|tmux*|rxvt*|alacritty*|kitty*|foot*|wezterm*|vte*|konsole*|st-*)
            ui_unicode=1 ;;
    esac
    [[ "${LC_ALL:-${LC_CTYPE:-${LANG:-}}}" =~ [Uu][Tt][Ff]-?8 ]] && ui_unicode=1
fi
if (( ui_unicode )); then
    export LC_ALL=C.UTF-8
    g_h='─' g_v='│' g_tl='┌' g_tr='┐' g_bl='└' g_br='┘' g_ml='├' g_mr='┤'
    # Upper half blocks: a solid bar half a row high, leaving the lower half of its row as space
    # before the line below; the track is the same glyph in grey.
    g_fill='▀' g_empty='▀'
else
    g_h='-' g_v='|' g_tl='+' g_tr='+' g_bl='+' g_br='+' g_ml='+' g_mr='+'
    g_fill='#' g_empty='-'
fi
# Bar fills, as the shared renderer: plain green; a device's load orange from 70 % and red from
# 90 %. On the Linux console (16 colours) colour 3 is orange-brown; elsewhere a 256-colour orange.
if [[ "${TERM:-}" == linux ]]; then
    c_bar_green="${esc}[22;32m" c_orange="${esc}[22;33m"
else
    c_bar_green="${esc}[22;38;5;71m" c_orange="${esc}[22;38;5;208m"
fi
c_bar_red="${esc}[22;31m"
# rule '=' opens a screen's frame the first time and closes it the second; '-' separates.
rule_open=0

ENGINE_VERSION="${CPU_DIT_ENGINE_VERSION:-0.1.0}"
REPORT_ABI="${CPU_DIT_REPORT_ABI:-9}"
THERMAL_LIMIT_C="${CPU_DIT_THERMAL_LIMIT_C:-$TEMPERATURE_LIMIT_C}"
case "${CPU_DIT_TEMP_LIMIT_ENABLED:-$TEMPERATURE_LIMIT_DEFAULT}" in
    1|on|ON|true|TRUE) thermal_limit_enabled=1 ;;
    *) thermal_limit_enabled=0 ;;
esac
cpu_previous_total=''
cpu_previous_idle=''
sensor_refresh_tick=0
cpu_line=''
ram_line=''
gpu_line=''
hardware_static_loaded=0
running_screen_initialized=0
running_screen_mode=''
running_screen_rows=0
running_screen_cols=0
running_frame_count=0
# Kernel messages (e.g. disk errors after the USB is pulled mid-test, which is
# now expected since `toram` lets the run continue) print straight to the
# console and can visually corrupt in-place updates; force a full redraw every
# so often to self-heal instead of chasing every possible external writer.
# 80 frames at the default 0.25s refresh keeps the same ~20s wall-clock cadence
# that 40 frames gave at the previous 0.5s refresh.
RUNNING_SELF_HEAL_FRAMES=80
term_rows=30
term_cols=80
page_width=76
page_margin=2

refresh_geometry() {
    term_rows=30
    term_cols=80
    geometry="$(stty size 2>/dev/null || true)"
    if [[ "$geometry" =~ ^([0-9]+)[[:space:]]+([0-9]+)$ ]]; then
        term_rows="${BASH_REMATCH[1]}"
        term_cols="${BASH_REMATCH[2]}"
    fi
    if [[ "${CPU_DIT_UI_ROWS:-}" =~ ^[0-9]+$ ]]; then
        term_rows="$CPU_DIT_UI_ROWS"
    fi
    if [[ "${CPU_DIT_UI_COLS:-}" =~ ^[0-9]+$ ]]; then
        term_cols="$CPU_DIT_UI_COLS"
    fi
    page_width=$((term_cols - 4))
    (( page_width < 48 )) && page_width=48
    page_margin=0
    (( term_cols > page_width )) && page_margin=$(((term_cols - page_width) / 2))
}

indent() { printf '%*s' "$page_margin" ''; }

repeat_char() {
    local value="$1" count="$2" index
    for ((index=0; index<count; index++)); do printf '%s' "$value"; done
}

rule() {
    local value="$1" left="$g_ml" right="$g_mr" fill="$g_h"
    if [[ "$value" == '=' ]]; then
        if (( rule_open )); then
            left="$g_bl" right="$g_br" rule_open=0
        else
            left="$g_tl" right="$g_tr" rule_open=1
        fi
    fi
    (( ui_unicode )) || fill="$value"
    indent
    printf '%s%s' "$c_blue" "$left"
    repeat_char "$fill" $((page_width - 2))
    printf '%s%s\n' "$right" "$c_reset$c_bg$c_white"
}

# fit_text WIDTH VALUE: VALUE cut or padded to WIDTH characters, in $fit_out. bash's printf pads
# by bytes, which breaks the frame as soon as a field holds box or block characters.
fit_text() {
    local width="$1" cut
    (( width < 0 )) && width=0
    cut="${2:0:width}"
    printf -v fit_out '%s%*s' "$cut" "$((width - ${#cut}))" ''
}

box_line() {
    local value="$1" inner=$((page_width - 4))
    fit_text "$inner" "$value"
    indent
    printf '%s%s%s %s %s%s%s\n' "$c_blue" "$g_v" "$c_reset$c_bg$c_white" \
        "$fit_out" "$c_blue" "$g_v" "$c_reset$c_bg$c_white"
}

box_center() {
    local value="$1" color="${2:-$c_white}" inner=$((page_width - 4))
    local length=${#value} left
    (( length > inner )) && length=$inner
    left=$(((inner - length) / 2))
    fit_text "$((inner - left))" "$value"
    indent
    printf '%s%s%s %*s%s%s%s %s%s%s\n' "$c_blue" "$g_v" \
        "$c_reset$c_bg$c_white" "$left" '' "$color" "$fit_out" \
        "$c_reset$c_bg$c_white" "$c_blue" "$g_v" "$c_reset$c_bg$c_white"
}

clear_screen() {
    refresh_geometry
    running_screen_initialized=0
    rule_open=0
    printf '%s%s%s[?25l%s[H%s[2J' "$c_reset$c_bg$c_white" "$esc" "$esc" "$esc" "$esc"
}

begin_running_frame() {
    local mode="$1" background="$c_bg"
    [[ "$mode" == dashboard ]] && background="$c_dashboard_bg"
    running_frame_count=$((running_frame_count + 1))
    if (( running_frame_count % RUNNING_SELF_HEAL_FRAMES == 0 )); then
        running_screen_initialized=0
    fi
    if (( running_screen_initialized == 0 )) \
        || [[ "$running_screen_mode" != "$mode" ]] \
        || (( running_screen_rows != term_rows || running_screen_cols != term_cols )); then
        printf '%s%s%s[?25l%s[H%s[2J' "$c_reset$background$c_white" \
            "$esc" "$esc" "$esc" "$esc"
        running_screen_initialized=1
        running_screen_mode="$mode"
        running_screen_rows="$term_rows"
        running_screen_cols="$term_cols"
        return
    fi
    printf '%s%s%s[?25l%s[H' "$c_reset$background$c_white" "$esc" "$esc" "$esc"
}

ui_reset() { printf '%s%s[?25h' "$c_reset" "$esc"; }

pause_screen() {
    printf '\n'
    indent
    printf '%sPress Enter to return...%s' "$c_muted" "$c_reset$c_bg$c_white"
    IFS= read -r _unused || true
}

format_duration() {
    local seconds="$1"
    printf '%02d:%02d:%02d' "$((seconds / 3600))" \
        "$(((seconds % 3600) / 60))" "$((seconds % 60))"
}

progress_bar() {
    local percent="$1" width=36 filled empty
    filled=$((percent * width / 100))
    empty=$((width - filled))
    printf '['
    repeat_char "$g_fill" "$filled"
    repeat_char "$g_empty" "$empty"
    printf ']'
}

progress_field() {
    local line="$1" key="$2" token prefix="${2}=" result=''
    for token in $line; do
        if [[ "$token" == "$prefix"* ]]; then
            result="${token#*=}"
        fi
    done
    printf '%s' "$result"
}

latest_progress_line() {
    local file="$1" line progress='' counters=''
    [[ -r "$file" ]] || return
    while IFS= read -r line; do
        [[ "$line" == *'CPU_DIT_PROGRESS phase='* ]] && progress="$line"
        [[ "$line" == *'CPU_DIT_COUNTER calculations_completed='* ]] && counters="$line"
    done <"$file"
    printf '%s %s' "$progress" "$counters"
}

trim_text() {
    local value="$1"
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    printf '%s' "$value"
}

format_gib_kib() {
    local kib="${1:-0}" unit=1048576 whole fraction
    [[ "$kib" =~ ^[0-9]+$ ]] || kib=0
    whole=$((kib / unit))
    fraction=$(((kib % unit) * 100 / unit))
    printf '%d.%02d GiB' "$whole" "$fraction"
}

# calculations_completed is a raw byte-verified counter (1 unit per byte
# compared in RAM_VERIFY, plus a handful of fixed per-arena/CPU-KAT units),
# not megabytes and not a task count; show the human-readable size alongside
# the raw count so operators do not have to guess the unit.
format_calculations() {
    local value="${1:-0}" unit=1 label='B' whole fraction
    [[ "$value" =~ ^[0-9]+$ ]] || value=0
    if (( value >= 1073741824 )); then
        unit=1073741824
        label='GiB'
    elif (( value >= 1048576 )); then
        unit=1048576
        label='MiB'
    elif (( value >= 1024 )); then
        unit=1024
        label='KiB'
    fi
    if (( unit == 1 )); then
        printf '%d' "$value"
        return
    fi
    whole=$((value / unit))
    fraction=$(((value % unit) * 100 / unit))
    printf '%d (%d.%02d %s)' "$value" "$whole" "$fraction" "$label"
}

read_live_metrics() {
    local cpu_tag user nice system idle iowait irq softirq steal guest guest_nice
    local total idle_total delta_total delta_idle value key amount _unit mhz ghz_hundredths
    read -r cpu_tag user nice system idle iowait irq softirq steal guest guest_nice \
        </proc/stat || true
    user="${user:-0}"; nice="${nice:-0}"; system="${system:-0}"
    idle="${idle:-0}"; iowait="${iowait:-0}"; irq="${irq:-0}"
    softirq="${softirq:-0}"; steal="${steal:-0}"
    total=$((user + nice + system + idle + iowait + irq + softirq + steal))
    idle_total=$((idle + iowait))
    cpu_usage_percent=0
    if [[ "$cpu_previous_total" =~ ^[0-9]+$ ]] && (( total > cpu_previous_total )); then
        delta_total=$((total - cpu_previous_total))
        delta_idle=$((idle_total - cpu_previous_idle))
        cpu_usage_percent=$(((delta_total - delta_idle) * 100 / delta_total))
    elif (( total > 0 )); then
        cpu_usage_percent=$(((total - idle_total) * 100 / total))
    fi
    cpu_previous_total="$total"
    cpu_previous_idle="$idle_total"
    (( cpu_usage_percent < 0 )) && cpu_usage_percent=0
    (( cpu_usage_percent > 100 )) && cpu_usage_percent=100

    mem_total_kib=0; mem_available_kib=0; mem_buffers_kib=0
    mem_cached_kib=0; mem_reclaimable_kib=0
    while read -r key amount _unit; do
        case "$key" in
            MemTotal:) mem_total_kib="${amount:-0}" ;;
            MemAvailable:) mem_available_kib="${amount:-0}" ;;
            Buffers:) mem_buffers_kib="${amount:-0}" ;;
            Cached:) mem_cached_kib="${amount:-0}" ;;
            SReclaimable:) mem_reclaimable_kib="${amount:-0}" ;;
        esac
    done </proc/meminfo
    mem_used_kib=$((mem_total_kib - mem_available_kib))
    (( mem_used_kib < 0 )) && mem_used_kib=0
    mem_cache_kib=$((mem_buffers_kib + mem_cached_kib + mem_reclaimable_kib))
    memory_usage_percent=0
    if (( mem_total_kib > 0 )); then
        memory_usage_percent=$((mem_used_kib * 100 / mem_total_kib))
    fi

    if (( hardware_static_loaded == 0 )); then
        cpu_model=''
        while IFS=: read -r key value; do
            if [[ "$key" == *'model name'* ]]; then
                cpu_model="$(trim_text "$value")"
                break
            fi
        done </proc/cpuinfo
        cpu_model="${cpu_model:-$(uname -m)}"
        cpu_threads="$(nproc 2>/dev/null || printf 'N/A')"
        board_vendor="$(cat /sys/devices/virtual/dmi/id/board_vendor 2>/dev/null || true)"
        board_name="$(cat /sys/devices/virtual/dmi/id/board_name 2>/dev/null || true)"
        board_text="$(trim_text "$board_vendor $board_name")"
        board_text="${board_text:-Unavailable}"
        gpu_text="$(lspci 2>/dev/null | sed -n '/VGA compatible controller\|3D controller\|Display controller/{s/^[^ ]* //;p;q;}')"
        gpu_text="${gpu_text:-Unavailable}"
        numa_nodes="$(find /sys/devices/system/node -maxdepth 1 -type d -name 'node[0-9]*' 2>/dev/null | wc -l)"
        (( numa_nodes < 1 )) && numa_nodes=1
        kernel_text="$(uname -r)"
        hardware_static_loaded=1
    fi
    cpu_frequency_ghz='N/A'
    if [[ -r /sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq ]]; then
        value="$(</sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq)"
        if [[ "$value" =~ ^[0-9]+$ ]]; then
            cpu_frequency_ghz="$(awk -v khz="$value" 'BEGIN { printf "%.2f GHz", khz / 1000000 }')"
        fi
    else
        value=''
        while IFS=: read -r key amount; do
            if [[ "$key" == *'cpu MHz'* ]]; then
                amount="$(trim_text "$amount")"
                mhz="${amount%%.*}"
                if [[ "$mhz" =~ ^[0-9]+$ ]]; then
                    ghz_hundredths=$((mhz / 10))
                    value="$(printf '%d.%02d' "$((ghz_hundredths / 100))" "$((ghz_hundredths % 100))")"
                fi
                break
            fi
        done </proc/cpuinfo
        [[ -n "$value" ]] && cpu_frequency_ghz="$value GHz"
    fi

    sensor_summary
    local -a cpu_parts
    IFS='|' read -r -a cpu_parts <<<"$cpu_line"
    cpu_temperature="$(trim_text "${cpu_parts[2]:-N/A}")"
    cpu_power="$(trim_text "${cpu_parts[3]:-N/A}")"
    cpu_temperature="${cpu_temperature:-N/A}"
    cpu_power="${cpu_power:-N/A}"
    if [[ "$gpu_line" == GPU:* ]]; then
        gpu_text="$(trim_text "${gpu_line#GPU:}")"
        gpu_text="${gpu_text:-Unavailable}"
    fi
}

ui_cursor() {
    printf '%s[%s;%sH' "$esc" "$1" "$2"
}

ui_put() {
    local row="$1" col="$2" width="$3" color="$4" value="$5"
    (( width < 1 )) && return
    value="${value//$'\n'/ }"
    ui_cursor "$row" "$col"
    fit_text "$width" "$value"
    printf '%s%s%s' "$color" "$fit_out" "$c_reset$c_dashboard_bg$c_white"
}

ui_box() {
    local row="$1" col="$2" height="$3" width="$4" title="$5"
    local edge row_index edge_width
    if [[ -n "$title" ]]; then
        edge_width=$((width - ${#title} - 6))
        (( edge_width < 0 )) && edge_width=0
        edge="$g_tl$g_h$g_h $title $(repeat_char "$g_h" "$edge_width")$g_tr"
    else
        edge="$g_tl$(repeat_char "$g_h" "$((width - 2))")$g_tr"
    fi
    ui_put "$row" "$col" "$width" "$c_cyan" "$edge"
    for ((row_index=1; row_index<height-1; row_index++)); do
        ui_put "$((row + row_index))" "$col" 1 "$c_cyan" "$g_v"
        ui_put "$((row + row_index))" "$((col + width - 1))" 1 "$c_cyan" "$g_v"
    done
    edge="$g_bl$(repeat_char "$g_h" "$((width - 2))")$g_br"
    ui_put "$((row + height - 1))" "$col" "$width" "$c_cyan" "$edge"
}

bar_text() {
    local percent="$1" width="$2" filled empty
    (( percent < 0 )) && percent=0
    (( percent > 100 )) && percent=100
    (( width < 4 )) && width=4
    filled=$((percent * width / 100))
    empty=$((width - filled))
    printf '['
    repeat_char "$g_fill" "$filled"
    repeat_char "$g_empty" "$empty"
    printf ']'
}

ui_meter() {
    local row="$1" col="$2" width="$3" label="$4" percent="$5" mode="${6:-auto}"
    local bar_width filled empty fill_color label_width
    (( percent < 0 )) && percent=0
    (( percent > 100 )) && percent=100
    label_width=11
    bar_width=$((width - label_width - 8))
    (( bar_width < 4 )) && bar_width=4
    filled=$((percent * bar_width / 100))
    empty=$((bar_width - filled))
    case "$mode" in
        cyan) fill_color="$c_cyan" ;;
        green) fill_color="$c_bar_green" ;;
        *)
            if (( percent >= 90 )); then
                fill_color="$c_bar_red"
            elif (( percent >= 70 )); then
                fill_color="$c_orange"
            else
                fill_color="$c_bar_green"
            fi
            ;;
    esac
    ui_put "$row" "$col" "$width" "$c_white" ''
    ui_cursor "$row" "$col"
    printf '%s%-*.*s%s[%s' "$c_white" "$label_width" "$label_width" "$label" \
        "$c_muted" "$fill_color"
    repeat_char "$g_fill" "$filled"
    printf '%s' "$c_muted"
    repeat_char "$g_empty" "$empty"
    printf ']%s %3d%%%s' "$c_white" "$percent" "$c_reset$c_dashboard_bg$c_white"
}

phase_description() {
    case "$1" in
        PREFLIGHT) printf 'source verify + inventory + sensor/driver preflight' ;;
        ALLOCATED) printf 'high-water RAM arenas allocated and prefaulted' ;;
        RAM_WRITE) printf 'CPU pattern generation -> scattered RAM writes' ;;
        RAM_VERIFY) printf 'cross-core RAM readback -> exact comparison' ;;
        ROUND_COMPLETE) printf 'exact round committed; next complete round starts' ;;
        FINALIZING) printf 'telemetry, safety gates and report finalization' ;;
        *) printf 'deterministic CPU/RAM data integrity pipeline' ;;
    esac
}

phase_state() {
    case "$1" in
        PREFLIGHT|ALLOCATED) printf 'RAMPING' ;;
        FINALIZING) printf 'FINALIZING' ;;
        *) printf 'RUNNING' ;;
    esac
}

sensor_summary() {
    local force="${1:-}"
    sensor_refresh_tick=$((sensor_refresh_tick + 1))
    if [[ "$force" != force && -n "$cpu_line" ]] \
        && (( sensor_refresh_tick < SENSOR_REFRESH_TICKS )); then
        return
    fi
    sensor_refresh_tick=0
    mapfile -t sensor_lines < <("$DIT" sensors --compact --interval-ms 100 2>/dev/null || true)
    cpu_line="${sensor_lines[0]:-CPU: inventory unavailable}"
    ram_line="${sensor_lines[1]:-RAM: inventory unavailable}"
    gpu_line="${sensor_lines[2]:-GPU: inventory unavailable}"
}

draw_menu() {
    if [[ "$POWER_UI" == 1 ]]; then draw_power_menu; return; fi
    refresh_geometry
    sensor_summary force
    local limit_row back_row=() start_row
    if (( thermal_limit_enabled == 1 )); then
        limit_row=" [T] TEMP LIMIT      ON at ${THERMAL_LIMIT_C} C (trusted CPU sensors)"
    else
        limit_row=' [T] TEMP LIMIT      OFF (temperature telemetry is still recorded)'
    fi
    [[ "${CPU_DIT_SUITE_CHILD:-0}" == 1 ]] && back_row=(' [B] BACK            Return to DIT module selection')
    if (( AUTO_SECONDS > 0 )); then
        start_row="Safe test starts automatically in ${AUTO_SECONDS}s; press a key to choose"
    else
        start_row='Select 1-5, D, T, R or P'
    fi
    if shared_screen 'DIT CPU + RAM // OFFLINE CPU + MEMORY DATA INTEGRITY TEST' \
        "$cpu_line" "$ram_line" --- \
        " [1] SAFE TEST       ${SAFE_MEMORY_MIB} MiB, min $(format_duration "$SAFE_MINIMUM_DURATION_SECONDS")" \
        " [2] FULL TEST       ${FULL_SAFE_PERCENT}% safe RAM + CPU compute oracle, min $(format_duration "$full_duration_seconds")" \
        " [5] CPU COMPUTE     FMA/AVX, branch, AES oracles, min $(format_duration "$full_duration_seconds")" \
        ' [3] SENSOR INFO     Temperature, CPU package Watt, EDAC and drivers' \
        ' [4] DIAGNOSTIC      Open root shell; type exit to return' \
        ' [D] TEST DURATION   Change minimum duration for Full and CPU Compute' \
        "$limit_row" \
        ' [R] REBOOT          [P] POWER OFF' \
        "${back_row[@]}" --- \
        'Root is read-only; reports only go to verified DIT-RESULTS on this USB.' \
        "$start_row"; then
        shared_prompt 'Selection > '
        return
    fi
    begin_running_frame compact
    rule '='
    box_center 'CPU-DIT-01' "$c_cyan"
    box_center 'OFFLINE CPU + MEMORY DATA INTEGRITY TEST' "$c_muted"
    rule '-'
    box_line "$cpu_line"
    box_line "$ram_line"
    rule '-'
    box_line " [1] SAFE TEST       ${SAFE_MEMORY_MIB} MiB, min $(format_duration "$SAFE_MINIMUM_DURATION_SECONDS")"
    box_line " [2] FULL TEST       ${FULL_SAFE_PERCENT}% safe RAM + CPU compute oracle, min $(format_duration "$full_duration_seconds")"
    box_line " [5] CPU COMPUTE     FMA/AVX, branch, AES oracles, min $(format_duration "$full_duration_seconds")"
    box_line ' [3] SENSOR INFO     Temperature, CPU package Watt, EDAC and drivers'
    box_line ' [4] DIAGNOSTIC      Open root shell; type exit to return'
    box_line ' [D] TEST DURATION  Change minimum duration for Full and CPU Compute'
    if (( thermal_limit_enabled == 1 )); then
        box_line " [T] TEMP LIMIT     ON at ${THERMAL_LIMIT_C} C (trusted CPU sensors)"
    else
        box_line ' [T] TEMP LIMIT     OFF (temperature telemetry is still recorded)'
    fi
    box_line ' [R] REBOOT          [P] POWER OFF'
    if [[ "${CPU_DIT_SUITE_CHILD:-0}" == 1 ]]; then
        box_line ' [B] BACK            Return to DIT module selection'
    fi
    box_line ''
    box_line ' Root is read-only; reports only go to verified DIT-RESULTS on this USB.'
    menu_fill=$((term_rows - 21))
    [[ "${CPU_DIT_SUITE_CHILD:-0}" == 1 ]] && menu_fill=$((menu_fill - 1))
    (( menu_fill < 0 )) && menu_fill=0
    while (( menu_fill > 0 )); do box_line ''; menu_fill=$((menu_fill - 1)); done
    rule '-'
    if (( AUTO_SECONDS > 0 )); then
        box_center "Safe test starts automatically in ${AUTO_SECONDS}s; press a key to choose" "$c_yellow"
    else
        box_center 'Select 1-5, D, T, R or P' "$c_yellow"
    fi
    rule '='
    ui_reset
    printf '\n'
    indent
    printf '%sSelection > %s' "$c_cyan" "$c_reset$c_bg$c_white"
}

draw_power_menu() {
    refresh_geometry
    sensor_summary force
    local limit_row
    if (( thermal_limit_enabled )); then
        limit_row=' [T] TEMP LIMIT          ON at 95 C (trusted CPU sensors)'
    else
        limit_row=' [T] TEMP LIMIT          OFF; telemetry remains active'
    fi
    if shared_screen 'DIT CPU AVAILABILITY / POWER // PLATFORM TEST' \
        "$cpu_line" "$ram_line" --- \
        ' [1] START AVAILABILITY / POWER TEST' \
        " [D] DURATION            Minimum $(format_duration "$power_duration_seconds") + complete coverage round" \
        ' [2] ADVANCED            Power-only / Core-SMT-only diagnostics' \
        ' [3] SENSOR INFO         Temperature, package power and drivers' \
        ' [4] DIAGNOSTIC          Open shell' \
        "$limit_row" \
        ' [B] BACK                Return to module menu' \
        ' [R] REBOOT              [P] POWER OFF' --- \
        'Single-thread -> Core/SMT -> All-CPU -> Mixed; repeat full rounds.' \
        'Seeded short/long idle + burst; mixed/heavy FP, 64 KiB/worker.' \
        'Availability and power evidence are reported separately.' \
        'Vcore unavailable; a completed run does not prove power stability.'; then
        shared_prompt 'Selection > '
        return
    fi
    begin_running_frame compact
    rule '='
    box_center 'CPU AVAILABILITY / POWER PLATFORM' "$c_cyan"
    rule '-'
    box_line "$cpu_line"
    box_line "$ram_line"
    rule '-'
    box_line ' [1] START AVAILABILITY / POWER TEST'
    box_line " [D] DURATION            Minimum $(format_duration "$power_duration_seconds") + complete coverage round"
    box_line ' [2] ADVANCED            Power-only / Core-SMT-only diagnostics'
    box_line ' [3] SENSOR INFO         Temperature, package power and drivers'
    box_line ' [4] DIAGNOSTIC          Open shell'
    if (( thermal_limit_enabled )); then
        box_line ' [T] TEMP LIMIT          ON at 95 C (trusted CPU sensors)'
    else
        box_line ' [T] TEMP LIMIT          OFF; telemetry remains active'
    fi
    box_line ' [B] BACK                Return to module menu'
    box_line ' [R] REBOOT              [P] POWER OFF'
    rule '-'
    box_line ' Single-thread -> Core/SMT -> All-CPU -> Mixed; repeat full rounds.'
    box_line ' Seeded short/long idle + burst; mixed/heavy FP, 64 KiB/worker.'
    box_line ' Availability and power evidence are reported separately.'
    box_line ' Vcore unavailable; a completed run does not prove power stability.'
    rule '='
    ui_reset
    printf '\nSelection > '
}

power_advanced_menu() {
    if ! shared_screen 'DIT CPU AVAILABILITY / POWER // ADVANCED DIAGNOSTICS' \
        " [1] POWER TRANSITIONS   $power_transitions cycles" \
        " [2] CORE / SMT SWEEP    $power_transitions cycles per group" \
        ' [N] TRANSITIONS         Set cycles/group (1..1000)' \
        ' [B] BACK                No test started'; then
        clear_screen
        rule '='
        box_center 'ADVANCED POWER DIAGNOSTICS' "$c_cyan"
        box_line " [1] POWER TRANSITIONS   $power_transitions cycles"
        box_line " [2] CORE / SMT SWEEP    $power_transitions cycles per group"
        box_line ' [N] TRANSITIONS         Set cycles/group (1..1000)'
        box_line ' [B] BACK                No test started'
        rule '='
    fi
    local selected requested
    IFS= read -r -n 1 selected || return 0
    case "$selected" in
        1) run_profile power 'POWER TRANSITIONS' 0 ;;
        2) run_profile power-sweep 'CORE / SMT SWEEP' 0 ;;
        n|N)
            printf '\nTransitions per group (1..1000): '
            IFS= read -r requested || return 0
            if [[ "$requested" =~ ^[1-9][0-9]{0,3}$ ]] && (( requested <= 1000 )); then power_transitions="$requested"; fi ;;
    esac
}

set_power_duration() {
    local selected
    printf '\nMinimum duration: [1] Quick 2 min  [2] Standard 10 min  [3] Extended 30 min  [C] Custom minutes: '
    IFS= read -r -n 1 selected || return 0
    case "$selected" in
        1) power_duration_seconds=120 ;;
        2) power_duration_seconds=600 ;;
        3) power_duration_seconds=1800 ;;
        c|C)
            printf '\nMinimum minutes (1..1440): '
            IFS= read -r selected || return 0
            if [[ "$selected" =~ ^[1-9][0-9]{0,3}$ ]] && (( selected <= 1440 )); then power_duration_seconds=$((selected * 60)); fi ;;
    esac
}

draw_power_running() {
    local title="$1" started="$2" log_file="$3" line phase completed target bursts failures availability power percent=0
    local elapsed workers seed top_row=5 top_height left_col=3 left_width right_col right_width lower_row inner_width
    local mode minimum_ms elapsed_ms rounds campaign_phase active_cpus kernel
    local progress_label=PROGRESS coverage_label=TRANSITIONS timing_text detail_text
    local load_timing='Idle 500 ms / burst 100 ms'
    line="$(grep 'CPU_DIT_POWER_PROGRESS phase=' "$log_file" 2>/dev/null | tail -n 1 || true)"
    phase="$(progress_field "$line" phase)"
    completed="$(progress_field "$line" completed)"
    target="$(progress_field "$line" target)"
    bursts="$(progress_field "$line" bursts)"
    failures="$(progress_field "$line" failures)"
    availability="$(progress_field "$line" availability)"
    power="$(progress_field "$line" power_platform)"
    workers="$(progress_field "$line" workers)"
    seed="$(progress_field "$line" seed)"
    if [[ "$completed" =~ ^[0-9]+$ && "$target" =~ ^[1-9][0-9]*$ ]]; then percent=$((completed * 100 / target)); fi
    (( percent > 100 )) && percent=100
    elapsed=$(( ${EPOCHSECONDS:-$(date +%s)} - started ))
    (( elapsed < 0 )) && elapsed=0
    timing_text="ELAPSED $(format_duration "$elapsed")"
    detail_text='Progress counts completed transitions; completion does not prove power stability.'
    mode="$(progress_field "$line" mode)"
    if [[ "$mode" == campaign ]]; then
        minimum_ms="$(progress_field "$line" minimum_ms)"
        elapsed_ms="$(progress_field "$line" elapsed_ms)"
        rounds="$(progress_field "$line" rounds)"
        completed="$(progress_field "$line" round_completed)"
        target="$(progress_field "$line" round_target)"
        workers="$(progress_field "$line" active_workers)"
        campaign_phase="$(progress_field "$line" campaign_phase)"
        active_cpus="$(progress_field "$line" active_cpus)"
        kernel="$(progress_field "$line" kernel)"
        load_timing="Idle $(progress_field "$line" idle_ms) ms / burst $(progress_field "$line" burst_ms) ms / $(progress_field "$line" timing)"
        progress_label='MIN TIME'
        coverage_label='ROUND COVERAGE'
        percent=0
        if [[ "$elapsed_ms" =~ ^[0-9]+$ && "$minimum_ms" =~ ^[1-9][0-9]*$ ]]; then
            percent=$((elapsed_ms * 100 / minimum_ms))
            (( percent > 100 )) && percent=100
            timing_text="TIME $(format_duration "$((elapsed_ms / 1000))") / minimum $(format_duration "$((minimum_ms / 1000))")"
        fi
        detail_text="Completed rounds ${rounds:-0}; full coverage round required. Not power validation."
        if (( percent >= 100 )) && [[ "${availability:-}" == Running ]]; then
            detail_text='Finishing current coverage round; minimum time reached, test is not complete.'
        fi
        if [[ "$(progress_field "$line" topology_complete)" == false ]]; then
            detail_text='INCOMPLETE TOPOLOGY: core/SMT coverage unavailable; this run cannot PASS.'
        fi
        if [[ "$phase" == FINAL || "$phase" == TRANSITION_DONE ]] && [[ "$completed" == 0 && "${rounds:-0}" =~ ^[1-9][0-9]*$ ]]; then
            completed="$target"
        fi
        phase="${campaign_phase:-planning} / ${phase:-PREFLIGHT}"
    fi
    refresh_geometry
    read_live_metrics
    if (( term_cols >= 110 && term_rows >= 32 )); then
        top_height=$((term_rows / 3))
        (( top_height < 11 )) && top_height=11
        (( top_height > 15 )) && top_height=15
        left_width=$(((term_cols - 8) / 2))
        right_col=$((left_col + left_width + 2))
        right_width=$((term_cols - right_col - 1))
        lower_row=$((top_row + top_height + 1))
        inner_width=$((term_cols - 6))
        begin_running_frame dashboard
        ui_box 1 1 "$term_rows" "$term_cols" ''
        ui_put 2 4 "$((term_cols - 7))" "$c_cyan" 'CPU AVAILABILITY / POWER PLATFORM'
        ui_put 3 4 "$((term_cols - 7))" "$c_cyan" "ENGINE $ENGINE_VERSION    PROFILE $title"
        ui_box "$top_row" "$left_col" "$top_height" "$left_width" 'SYSTEM HARDWARE'
        ui_put "$((top_row + 2))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" "CPU:    $cpu_model"
        ui_put "$((top_row + 3))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" "THREADS: $cpu_threads logical CPU(s)"
        ui_put "$((top_row + 4))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" "RAM:    $(format_gib_kib "$mem_total_kib") total system memory"
        ui_put "$((top_row + 5))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" "NUMA:   $numa_nodes node(s)"
        ui_put "$((top_row + 6))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" "GPU:    $gpu_text"
        ui_put "$((top_row + 7))" "$((left_col + 2))" "$((left_width - 4))" "$c_cyan" "BOARD:  $board_text"
        ui_put "$((top_row + 8))" "$((left_col + 2))" "$((left_width - 4))" "$c_cyan" "KERNEL: $kernel_text"
        ui_box "$top_row" "$right_col" "$top_height" "$right_width" 'LIVE UTILIZATION'
        ui_meter "$((top_row + 2))" "$((right_col + 2))" "$((right_width - 4))" 'CPU USAGE' "$cpu_usage_percent" auto
        ui_put "$((top_row + 3))" "$((right_col + 4))" "$((right_width - 6))" "$c_cyan" "$cpu_frequency_ghz / Temp $cpu_temperature / Power $cpu_power"
        ui_meter "$((top_row + 5))" "$((right_col + 2))" "$((right_width - 4))" 'MEMORY' "$memory_usage_percent" auto
        ui_put "$((top_row + 6))" "$((right_col + 4))" "$((right_width - 6))" "$c_cyan" "Used $(format_gib_kib "$mem_used_kib") / $(format_gib_kib "$mem_total_kib")"
        ui_put "$((top_row + 7))" "$((right_col + 4))" "$((right_width - 6))" "$c_cyan" "Available $(format_gib_kib "$mem_available_kib") / Cache $(format_gib_kib "$mem_cache_kib")"
        ui_put "$((top_row + 8))" "$((right_col + 2))" "$((right_width - 4))" "$c_yellow" 'VCORE unavailable'
        ui_put "$((top_row + 9))" "$((right_col + 2))" "$((right_width - 4))" "$c_muted" 'Scratch 64 KiB/worker; not full RAM coverage'
        ui_box "$lower_row" "$left_col" "$((term_rows - lower_row))" "$inner_width" 'TESTING / PROCESS INFORMATION'
        ui_put "$((lower_row + 2))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" "PHASE ${phase:-PREFLIGHT}    $timing_text"
        ui_meter "$((lower_row + 4))" "$((left_col + 2))" "$((inner_width - 4))" "$progress_label" "$percent" green
        ui_put "$((lower_row + 6))" "$((left_col + 2))" "$((inner_width - 4))" "$c_white" "$coverage_label ${completed:-0} / ${target:-planning}    WORKERS ${workers:-planning}    BURSTS ${bursts:-0}    FAIL TOTAL ${failures:-0}"
        ui_put "$((lower_row + 7))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" "AVAILABILITY ${availability:-Starting}    POWER ${power:-NoEvidence}"
        ui_put "$((lower_row + 8))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" "SEED ${seed:-planning}    $load_timing"
        if [[ "$mode" == campaign ]]; then
            ui_put "$((lower_row + 9))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" "KERNEL $kernel    ROUNDS ${rounds:-0}    GROUP $(progress_field "$line" group)    ACTIVE CPUs $active_cpus"
        fi
        ui_put "$((lower_row + 10))" "$((left_col + 2))" "$((inner_width - 4))" "$c_muted" "$detail_text"
        ui_put "$((term_rows - 2))" "$((left_col + 2))" "$((inner_width - 4))" "$c_yellow" "TEMP LIMIT $([[ "$thermal_limit_enabled" == 1 ]] && echo "ON (${THERMAL_LIMIT_C} C)" || echo OFF)    |    Ctrl+C: stop + preserve report"
        return
    fi
    begin_running_frame compact
    rule '='
    box_center 'CPU AVAILABILITY / POWER PLATFORM' "$c_cyan"
    box_center "$title" "$c_white"
    rule '-'
    box_center 'SYSTEM HARDWARE / LIVE SENSORS' "$c_cyan"
    box_line "$cpu_line"
    box_line "$ram_line"
    box_line " CPU USAGE $(bar_text "$cpu_usage_percent" 20) ${cpu_usage_percent}%"
    box_line " RAM USAGE $(bar_text "$memory_usage_percent" 20) ${memory_usage_percent}%"
    rule '-'
    box_center 'TESTING / PROCESS INFORMATION' "$c_cyan"
    box_line " PHASE         ${phase:-PREFLIGHT}"
    box_line " $coverage_label ${completed:-0} / ${target:-planning}"
    box_line " $progress_label $(progress_bar "$percent") ${percent}%"
    box_line " BURSTS        ${bursts:-0}       FAIL TOTAL ${failures:-0}"
    box_line " $timing_text"
    box_line " AVAILABILITY  ${availability:-Starting}"
    box_line " POWER         ${power:-NoEvidence}       VCORE unavailable"
    box_line " SEED          $(progress_field "$line" seed)"
    if [[ "$mode" == campaign ]]; then
        box_line " ACTIVE CPUs $active_cpus / $kernel / rounds ${rounds:-0}"
        box_line " $load_timing"
        box_line " $detail_text"
    fi
    box_line " TEMP LIMIT    $([[ "$thermal_limit_enabled" == 1 ]] && echo 'ON (95 C)' || echo OFF)"
    box_line ' Scratch RAM: 64 KiB/worker; full RAM coverage is a separate test.'
    rule '-'
    box_line ' Ctrl+C stops the supervised test and returns to the menu.'
    rule '='
}

draw_running_compact() {
    local title="$1" duration="$2" started="$3" log_file="$4"
    local now elapsed percent progress_line phase round target_bytes target_mib bar
    local session_seed round_seed seed_source window_recipe pattern_recipe calculations failures
    now="${EPOCHSECONDS:-$(date +%s)}"
    elapsed=$((now - started))
    (( elapsed < 0 )) && elapsed=0
    if (( duration > 0 )); then
        percent=$((elapsed * 100 / duration))
    else
        percent=0
    fi
    (( percent > 99 )) && percent=99
    progress_line="$(latest_progress_line "$log_file")"
    phase="$(progress_field "$progress_line" phase)"
    round="$(progress_field "$progress_line" round)"
    target_bytes="$(progress_field "$progress_line" target_bytes)"
    session_seed="$(progress_field "$progress_line" session_seed)"
    round_seed="$(progress_field "$progress_line" round_seed)"
    seed_source="$(progress_field "$progress_line" seed_source)"
    window_recipe="$(progress_field "$progress_line" window_recipe)"
    pattern_recipe="$(progress_field "$progress_line" pattern_recipe)"
    calculations="$(progress_field "$progress_line" calculations_completed)"
    failures="$(progress_field "$progress_line" failures_total)"
    phase="${phase:-PREFLIGHT}"
    phase="${phase//_/ }"
    round="${round:-0}"
    session_seed="${session_seed:-planning}"
    round_seed="${round_seed:-N/A}"
    seed_source="${seed_source:-pending}"
    window_recipe="${window_recipe:-planning}"
    pattern_recipe="${pattern_recipe:-planning}"
    calculations="${calculations:-0}"
    failures="${failures:-0}"
    target_mib='planning'
    if [[ "$target_bytes" =~ ^[0-9]+$ ]]; then
        target_mib="$((target_bytes / 1024 / 1024)) MiB"
    fi
    bar="$(progress_bar "$percent")"

    begin_running_frame compact
    sensor_summary
    rule '='
    box_center 'CPU-DIT-01  //  ACTIVE TEST' "$c_cyan"
    box_center "$title" "$c_white"
    rule '-'
    box_center 'RUNNING — exact verification in progress' "$c_yellow"
    box_line "$cpu_line"
    box_line "$ram_line"
    rule '-'
    box_center "$bar  ${percent}%" "$c_green"
    box_line " Calculations completed: $(format_calculations "$calculations")  |  FAIL total: $failures"
    box_line " Elapsed $(format_duration "$elapsed") / minimum $(format_duration "$duration")"
    box_line " Phase: $phase | completed/current round: $round | RAM target: $target_mib"
    box_line " Session seed: $session_seed ($seed_source) | round seed: $round_seed"
    box_line " Recipe: $window_recipe | $pattern_recipe"
    box_line ' Test stops only after the minimum time and a complete write/read round.'
    box_line ' Ctrl+C requests ABORT; never remove power or USB while RUNNING.'
    running_fill=$((term_rows - 20))
    (( running_fill < 0 )) && running_fill=0
    while (( running_fill > 0 )); do box_line ''; running_fill=$((running_fill - 1)); done
    rule '-'
    box_center 'CPU and RAM remain under sustained verified load' "$c_muted"
    rule '='
    ui_reset
}

draw_running_dashboard() {
    local title="$1" duration="$2" started="$3" log_file="$4"
    local now elapsed percent progress_line phase phase_label state round minimum_rounds
    local elapsed_ms target_bytes target_mib target_percent process_text profile_name
    local session_seed round_seed seed_source window_recipe pattern_recipe active_window active_pattern
    local calculations failures counter_color
    local top_row=5 top_height left_col=3 right_col left_width right_width gap=2
    local lower_row lower_height inner_width
    local used_text total_text available_text cache_text target_text active_test

    now="${EPOCHSECONDS:-$(date +%s)}"
    elapsed=$((now - started))
    (( elapsed < 0 )) && elapsed=0
    progress_line="$(latest_progress_line "$log_file")"
    phase="$(progress_field "$progress_line" phase)"
    round="$(progress_field "$progress_line" round)"
    minimum_rounds="$(progress_field "$progress_line" minimum_rounds)"
    elapsed_ms="$(progress_field "$progress_line" elapsed_ms)"
    target_bytes="$(progress_field "$progress_line" target_bytes)"
    session_seed="$(progress_field "$progress_line" session_seed)"
    round_seed="$(progress_field "$progress_line" round_seed)"
    seed_source="$(progress_field "$progress_line" seed_source)"
    window_recipe="$(progress_field "$progress_line" window_recipe)"
    pattern_recipe="$(progress_field "$progress_line" pattern_recipe)"
    active_window="$(progress_field "$progress_line" active_window)"
    active_pattern="$(progress_field "$progress_line" active_pattern)"
    calculations="$(progress_field "$progress_line" calculations_completed)"
    failures="$(progress_field "$progress_line" failures_total)"
    phase="${phase:-PREFLIGHT}"
    round="${round:-0}"
    minimum_rounds="${minimum_rounds:-0}"
    session_seed="${session_seed:-planning}"
    round_seed="${round_seed:-N/A}"
    seed_source="${seed_source:-pending}"
    window_recipe="${window_recipe:-planning}"
    pattern_recipe="${pattern_recipe:-planning}"
    active_window="${active_window:-planning}"
    active_pattern="${active_pattern:-planning}"
    calculations="${calculations:-0}"
    failures="${failures:-0}"
    counter_color="$c_cyan"
    [[ "$failures" =~ ^[0-9]+$ ]] && (( failures > 0 )) && counter_color="$c_red"
    if [[ "$elapsed_ms" =~ ^[0-9]+$ ]]; then
        elapsed=$((elapsed_ms / 1000))
    fi
    if (( duration > 0 )); then
        percent=$((elapsed * 100 / duration))
    else
        percent=0
    fi
    (( percent > 99 )) && percent=99
    (( percent < 0 )) && percent=0

    read_live_metrics
    target_mib='planning'
    target_percent=0
    if [[ "$target_bytes" =~ ^[0-9]+$ ]]; then
        target_mib="$((target_bytes / 1024 / 1024)) MiB"
        if (( mem_total_kib > 0 )); then
            target_percent=$((target_bytes / 1024 * 100 / mem_total_kib))
            (( target_percent > 100 )) && target_percent=100
        fi
    fi

    profile_name="${title% TEST}"
    state="$(phase_state "$phase")"
    phase_label="${phase//_/ }"
    process_text="$(phase_description "$phase")"
    used_text="$(format_gib_kib "$mem_used_kib")"
    total_text="$(format_gib_kib "$mem_total_kib")"
    available_text="$(format_gib_kib "$mem_available_kib")"
    cache_text="$(format_gib_kib "$mem_cache_kib")"
    target_text="$target_mib"

    top_height=$((term_rows / 3))
    (( top_height < 11 )) && top_height=11
    (( top_height > 15 )) && top_height=15
    lower_row=$((top_row + top_height + 1))
    lower_height=$((term_rows - lower_row))
    left_width=$(((term_cols - 6 - gap) / 2))
    right_col=$((left_col + left_width + gap))
    right_width=$((term_cols - right_col - 1))
    inner_width=$((term_cols - 6))
    begin_running_frame dashboard
    ui_box 1 1 "$term_rows" "$term_cols" ''
    ui_put 2 4 "$((term_cols - 7))" "$c_cyan" \
        "CPU-DIT-01 / DATA INTEGRITY TEST ENGINE"
    ui_put 3 4 "$((term_cols - 7))" "$c_cyan" \
        "ENGINE $ENGINE_VERSION    REPORT ABI $REPORT_ABI    PROFILE $profile_name / CPU + RAM"

    ui_box "$top_row" "$left_col" "$top_height" "$left_width" 'SYSTEM HARDWARE'
    ui_put "$((top_row + 2))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" \
        "CPU:    $cpu_model"
    ui_put "$((top_row + 3))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" \
        "THREADS: $cpu_threads logical CPU(s)"
    ui_put "$((top_row + 4))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" \
        "RAM:    $total_text total system memory"
    ui_put "$((top_row + 5))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" \
        "NUMA:   $numa_nodes node(s)"
    ui_put "$((top_row + 6))" "$((left_col + 2))" "$((left_width - 4))" "$c_white" \
        "GPU:    $gpu_text"
    ui_put "$((top_row + 7))" "$((left_col + 2))" "$((left_width - 4))" "$c_cyan" \
        "BOARD:  $board_text"
    ui_put "$((top_row + 8))" "$((left_col + 2))" "$((left_width - 4))" "$c_cyan" \
        "KERNEL: $kernel_text"

    ui_box "$top_row" "$right_col" "$top_height" "$right_width" 'LIVE UTILIZATION'
    ui_meter "$((top_row + 2))" "$((right_col + 2))" "$((right_width - 4))" \
        'CPU USAGE' "$cpu_usage_percent" auto
    ui_put "$((top_row + 3))" "$((right_col + 4))" "$((right_width - 6))" "$c_cyan" \
        "$cpu_frequency_ghz  /  Temp $cpu_temperature  /  Power $cpu_power"
    ui_meter "$((top_row + 5))" "$((right_col + 2))" "$((right_width - 4))" \
        'MEMORY' "$memory_usage_percent" auto
    ui_put "$((top_row + 6))" "$((right_col + 4))" "$((right_width - 6))" "$c_cyan" \
        "Used $used_text / $total_text  |  Available $available_text  |  Cache $cache_text"
    ui_meter "$((top_row + 8))" "$((right_col + 2))" "$((right_width - 4))" \
        'DIT TARGET' "$target_percent" cyan
    ui_put "$((top_row + 9))" "$((right_col + 4))" "$((right_width - 6))" "$c_cyan" \
        "Exact test buffer $target_text  |  Full policy ${FULL_SAFE_PERCENT}% safe allocatable"

    ui_box "$lower_row" "$left_col" "$lower_height" "$inner_width" \
        'TESTING / PROCESS INFORMATION'
    ui_put "$((lower_row + 2))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" \
        "STATE: $state       PROGRESS: ${percent}%       TIME: $(format_duration "$elapsed") / $(format_duration "$duration")"
    ui_meter "$((lower_row + 4))" "$((left_col + 2))" "$((inner_width - 4))" \
        'PROGRESS' "$percent" green
    active_test="[$round/${minimum_rounds}+] CPU-RAM-EXACT-001  $phase_label"
    ui_put "$((lower_row + 6))" "$((left_col + 2))" "$((inner_width - 4))" "$c_white" \
        "ACTIVE TEST   $active_test"
    ui_put "$((lower_row + 7))" "$((left_col + 2))" "$((inner_width - 4))" "$c_white" \
        "PROCESS       $process_text"
    ui_put "$((lower_row + 8))" "$((left_col + 2))" "$((inner_width - 4))" "$counter_color" \
        "CALCULATIONS COMPLETED  $(format_calculations "$calculations")    |    FAIL TOTAL  $failures"
    ui_put "$((lower_row + 9))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" \
        "SESSION SEED  $session_seed    SOURCE  $seed_source"
    ui_put "$((lower_row + 10))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" \
        "ROUND SEED    $round_seed"
    ui_put "$((lower_row + 11))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" \
        "WORKERS       auto topology    TEST BUFFER  $target_text"
    ui_put "$((lower_row + 12))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" \
        "RECIPE        $window_recipe | $pattern_recipe"
    ui_put "$((lower_row + 13))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" \
        "WINDOW        $active_window    PATTERN  $active_pattern    exact full-target write + readback"
    if (( thermal_limit_enabled == 1 )); then
        ui_put "$((term_rows - 2))" "$((left_col + 2))" "$((inner_width - 4))" "$c_yellow" \
            "TEMP LIMIT ON (${THERMAL_LIMIT_C} C, TRUSTED CPU SENSORS)    |    Ctrl+C: stop + preserve report"
    else
        ui_put "$((term_rows - 2))" "$((left_col + 2))" "$((inner_width - 4))" "$c_cyan" \
            'TEMP LIMIT OFF — TELEMETRY ONLY    |    Ctrl+C: stop + preserve report'
    fi
}

draw_running() {
    refresh_geometry
    if (( term_cols >= 110 && term_rows >= 32 )); then
        draw_running_dashboard "$@"
    else
        draw_running_compact "$@"
    fi
}

show_result() {
    local status="$1" text_value="$2" color="$3"
    if shared_screen 'DIT CPU + RAM // TEST RESULT' \
        "RESULT: $status    $text_value" --- \
        'Reports: DIT-RESULTS/runs/<session-id> when the verified USB partition is available.'; then
        shared_prompt 'Press Enter to return to the control center '
        IFS= read -r _unused || true
        return
    fi
    clear_screen
    rule '='
    box_center 'CPU-DIT-01  //  TEST RESULT' "$c_cyan"
    rule '-'
    box_center "$status" "$color"
    box_center "$text_value"
    box_line ''
    box_line 'Reports: DIT-RESULTS/runs/<session-id> when the verified USB partition is available.'
    result_fill=$((term_rows - 12))
    (( result_fill < 0 )) && result_fill=0
    while (( result_fill > 0 )); do box_line ''; result_fill=$((result_fill - 1)); done
    rule '-'
    box_center 'Press Enter to return to the control center' "$c_yellow"
    rule '='
    ui_reset
    IFS= read -r _unused || true
}

# run_result VERDICT TEXT COLOR: the result of a CPU-RAM or power run. With the shared view, inside its
# last screen (verdict and where the reports went, under the progress); otherwise the result
# screen. Uses run_profile's shared_view, final_frame and run_log.
run_result() {
    local verdict="$1" text="$2" color="$3" reports report_row line
    local -a rows=() ledger_rows=() csv_rows=() ledger_args=()
    if (( ${shared_view:-0} )) && [[ -s "${final_frame:-}" ]]; then
        reports="$(grep '^Reports: ' "$run_log" 2>/dev/null | tail -n 1)"
        reports="${reports#Reports: }"
        if grep -q 'verified USB result partition unavailable' "$run_log" 2>/dev/null; then
            report_row="REPORT   Verified result volume unavailable; reports remain in tmpfs: ${reports:-unknown}"
        else
            report_row="REPORT   Reports saved to: ${reports:-unknown}"
        fi
        # CPU-RAM: the failures on their component cells and the fail-summary row, as the other
        # tests show them.
        if [[ "${module:-cpu-ram}" == cpu-ram && -n "$reports" && -s "$reports/system-report.json" ]]; then
            ledger_args=(ledger --report "$reports/system-report.json"
                --results-root "$WATCHDOG_RESULTS_ROOT" --run-dir "$reports"
                --run-id "${reports##*/}" --verdict "$verdict" --session "$run_id")
            grep -qw 'dit_attribution=1' /proc/cmdline 2>/dev/null && ledger_args+=(--attribution)
            while IFS= read -r line; do
                case "$line" in
                    'FAIL BY PART'*|'HW EVENTS'*|'THROTTLE'*) ledger_rows+=("$line") ;;
                    'Fail summary'*) csv_rows+=("CSV      $line") ;;
                esac
            done < <("$SUITE" "${ledger_args[@]}" 2>/dev/null)
        fi
        # The session ends here (docs/FAULT_CATCH_ROADMAP.md, FC3a): a run without a report still
        # gets its CSV row, and the black box stops waiting for this result.
        if [[ "${module:-cpu-ram}" == cpu-ram && -n "${run_id:-}" ]]; then
            local -a end_args=(session-end --run-id "$run_id" --verdict "$verdict"
                --results-root "$WATCHDOG_RESULTS_ROOT" --run-dir "${reports:-$UI_RUN_DIR}")
            grep -qw 'dit_attribution=1' /proc/cmdline 2>/dev/null && end_args+=(--attribution)
            while IFS= read -r line; do
                [[ "$line" == 'Fail summary'* ]] && csv_rows+=("CSV      $line")
            done < <("$SUITE" "${end_args[@]}" 2>/dev/null)
        fi
        # The order every test uses: verdict, ledger, report, CSV.
        rows=(--row "RESULT: $verdict    $text")
        for line in "${ledger_rows[@]}"; do rows+=(--row "$line"); done
        rows+=(--row "$report_row")
        for line in "${csv_rows[@]}"; do rows+=(--row "$line"); done
        if "$SUITE" final-frame --frame "$final_frame" "${rows[@]}"; then
            rm -f -- "$final_frame"
            ui_reset
            IFS= read -r _unused || true
            return
        fi
    fi
    show_result "$verdict" "$text" "$color"
}

run_profile() {
    local profile="$1" title="$2" duration="$3" status=0
    local watchdog_stale=90
    local run_log="$UI_RUN_DIR/cpu-dit-ui-active-$$.log" started run_id watchdog_dir heartbeat
    local module=cpu-ram
    [[ "$profile" == power || "$profile" == power-sweep || "$profile" == power-campaign ]] && module=cpu-power-platform
    if [[ "$module" == cpu-power-platform ]]; then
        local -a timing_policy=()
        [[ "$profile" == power-campaign ]] && timing_policy+=(--campaign)
        watchdog_stale="$("$DIT" power-platform-watchdog-seconds "${timing_policy[@]}" 2>/dev/null || true)"
        if [[ ! "$watchdog_stale" =~ ^[1-9][0-9]*$ ]]; then
            show_result 'INVALID' 'Missing power timing policy; rebuild matching CLI/UI.' "$c_red"
            return
        fi
    fi
    local -a watchdog_marker=()
    mkdir -p "$UI_RUN_DIR"
    : >"$run_log"
    started="$(date +%s)"
    run_id="$module-$(date +%s)-$$"
    watchdog_dir="$UI_RUN_DIR/dit-suite/$run_id/$module"
    heartbeat="$watchdog_dir/watchdog-heartbeat.json"
    mkdir -p "$watchdog_dir"
    if [[ -d "$WATCHDOG_RESULTS_ROOT" ]]; then
        watchdog_marker=(--recovery-marker "$WATCHDOG_RESULTS_ROOT/watchdog/$run_id-$module.json")
    fi
    if [[ -x "$WATCHDOG" ]]; then
        active_run_uses_watchdog=1
        setsid env CPU_DIT_UI_CHILD=1 CPU_DIT_RETURN_TO_UI=1 \
            CPU_DIT_POWER_TRANSITIONS="$power_transitions" \
            CPU_DIT_MIN_DURATION_SECONDS="$duration" \
            CPU_DIT_TEMP_LIMIT_ENABLED="$thermal_limit_enabled" \
            CPU_DIT_WATCHDOG_HEARTBEAT="$heartbeat" \
            "$WATCHDOG" supervise --module "$module" --run-id "$run_id" \
            --run-dir "$watchdog_dir" --heartbeat "$heartbeat" \
            --stale-after-seconds "$watchdog_stale" --kill-after-seconds 30 \
            "${watchdog_marker[@]}" -- "$RUNNER" "$profile" >"$run_log" 2>&1 &
    else
        active_run_uses_watchdog=0
        setsid env CPU_DIT_UI_CHILD=1 CPU_DIT_RETURN_TO_UI=1 \
            CPU_DIT_POWER_TRANSITIONS="$power_transitions" \
            CPU_DIT_MIN_DURATION_SECONDS="$duration" \
            CPU_DIT_TEMP_LIMIT_ENABLED="$thermal_limit_enabled" \
            "$RUNNER" "$profile" >"$run_log" 2>&1 &
    fi
    active_run_pid=$!
    running_screen_initialized=0
    sensor_refresh_tick="$SENSOR_REFRESH_TICKS"
    local shared_view=0 view_status
    # The shared view saves its last screen here; the result is then drawn inside it.
    local final_frame="$UI_RUN_DIR/cpu-ram-final-$$.json"
    rm -f -- "$final_frame"
    local view_mode=cpu-ram
    [[ "$module" == cpu-power-platform ]] && view_mode=power
    if [[ -x "$SUITE" && "$("$SUITE" cpu-ram-view --help 2>/dev/null)" == *--mode* ]]; then
        shared_view=1
        printf '%s%s[H%s[2J' "$c_reset" "$esc" "$esc"
    fi
    while kill -0 "$active_run_pid" 2>/dev/null; do
        if (( shared_view )); then
            # Returns when the runner exits. Ctrl+C ends it too (status 130): the INT trap stops
            # the runner, and the view starts again to show the stop. Any other failure falls
            # back to the script's own screen.
            local -a view_args=(cpu-ram-view --mode "$view_mode" --log "$run_log" --pid "$active_run_pid"
                --title "$title" --duration-seconds "$duration" --started-unix "$started"
                --full-safe-percent "$FULL_SAFE_PERCENT" --engine-version "$ENGINE_VERSION"
                --report-abi "$REPORT_ABI")
            (( thermal_limit_enabled == 1 )) && view_args+=(--temp-limit-celsius "$THERMAL_LIMIT_C")
            # The view beats this run's session for the black box while it draws (FC3a).
            if [[ "$module" == cpu-ram ]]; then
                view_args+=(--session-run-id "$run_id" --watchdog-dir "$watchdog_dir"
                    --results-root "$WATCHDOG_RESULTS_ROOT")
                grep -qw 'dit_attribution=1' /proc/cmdline 2>/dev/null && view_args+=(--attribution)
            fi
            view_status=0
            DIT_UI_FINAL_FRAME="$final_frame" "$SUITE" "${view_args[@]}" || view_status=$?
            if (( view_status != 0 && view_status != 130 )); then
                shared_view=0
                running_screen_initialized=0
            fi
            continue
        elif [[ "$module" == cpu-power-platform ]]; then
            draw_power_running "$title" "$started" "$run_log"
        else
            draw_running "$title" "$duration" "$started" "$run_log"
        fi
        sleep "$UI_REFRESH_SECONDS"
    done
    wait "$active_run_pid" || status=$?
    active_run_pid=''
    completion_marker="$(grep 'CPU-DIT BOOT TEST COMPLETE' "$run_log" 2>/dev/null \
        | tail -n 1 || true)"
    if [[ -n "$completion_marker" && "${CPU_DIT_SERIAL_CONSOLE:-0}" == 1 \
        && -w /dev/ttyS0 ]]; then
        printf '%s\n' "$completion_marker" >/dev/ttyS0
    fi
    sync
    if (( sigint_seen )); then
        # The watchdog exits with its own convention code (2) after an operator
        # stop, not a signal-derived status; classify from the request, not $status.
        sigint_seen=0
        run_result 'ABORTED' 'The operator stopped the active test; this is not PASS.' "$c_yellow"
    else
        if [[ "$module" == cpu-power-platform ]]; then
            local result_line verdict evidence
            result_line="$(grep 'CPU_DIT_POWER_RESULT verdict=' "$run_log" | tail -n 1 || true)"
            verdict="$(progress_field "$result_line" verdict)"
            evidence="Availability: $(progress_field "$result_line" availability); power: $(progress_field "$result_line" power_platform)"
            case "$verdict:$status" in
                Pass:0) run_result 'PASS' "$evidence" "$c_green" ;;
                Fail:*) run_result 'FAIL' "$evidence" "$c_red" ;;
                Incomplete:*) run_result 'INCOMPLETE' "$evidence" "$c_yellow" ;;
                *) run_result 'INTERRUPTED / INVALID' 'No successful final result; preserve checkpoint and watchdog evidence.' "$c_yellow" ;;
            esac
            return
        fi
        case "$status" in
            0) run_result 'PASS' 'All required correctness and coverage gates passed.' "$c_green" ;;
            2) run_result 'FAIL' 'A correctness, telemetry or execution gate failed.' "$c_red" ;;
            3) run_result 'INVALID' 'Source, profile or environment is invalid.' "$c_yellow" ;;
            130|143) run_result 'ABORTED' 'The operator stopped the active test; this is not PASS.' "$c_yellow" ;;
            *) run_result "ERROR $status" 'The runner ended unexpectedly; preserve the report.' "$c_red" ;;
        esac
    fi
}

set_full_duration() {
    local requested
    if shared_screen 'DIT CPU + RAM // FULL TEST DURATION' \
        "Current minimum: $(format_duration "$full_duration_seconds")" --- \
        'Enter 60..86400 seconds. Validation default is 1800 seconds (30 min).' \
        'The engine finishes the active exact write/read round after time expires.' \
        'Press Enter without a value to keep the current duration.'; then
        shared_prompt 'Seconds > '
    else
        clear_screen
        rule '='
        box_center 'CPU-DIT-01  //  FULL TEST DURATION' "$c_cyan"
        rule '-'
        box_center "Current minimum: $(format_duration "$full_duration_seconds")" "$c_white"
        box_line ' Enter 60..86400 seconds. Validation default is 1800 seconds (30 min).'
        box_line ' The engine finishes the active exact write/read round after time expires.'
        rule '-'
        box_center 'Press Enter without a value to keep the current duration' "$c_yellow"
        rule '='
        ui_reset
        printf '\n'
        indent
        printf '%sSeconds > %s' "$c_cyan" "$c_reset$c_bg$c_white"
    fi
    IFS= read -r requested || requested=''
    [[ -z "$requested" ]] && return
    if [[ "$requested" =~ ^[0-9]+$ ]] && (( requested >= 60 && requested <= 86400 )); then
        full_duration_seconds="$requested"
        return
    fi
    indent
    printf '%sInvalid duration; keeping %ss.%s\n' "$c_red" \
        "$full_duration_seconds" "$c_reset"
    sleep 1
}

abort_active_run() {
    if [[ -n "$active_run_pid" ]] && kill -0 "$active_run_pid" 2>/dev/null; then
        if (( active_run_uses_watchdog )); then
            # SIGINT (not TERM) to the watchdog process itself: its own ctrlc
            # handler then gracefully TERM/KILLs the worker's process group and
            # waits for it. TERM/group here would kill only the watchdog
            # (no handler installed for TERM) and orphan the still-running
            # worker in its own session, doubling load until it finishes on
            # its own -- the actual cause of the reported hang/reboot.
            kill -INT "$active_run_pid" 2>/dev/null || true
        else
            kill -TERM -- "-$active_run_pid" 2>/dev/null || true
        fi
    fi
}

show_sensors() {
    if ! shared_screen 'DIT CPU + RAM // SENSOR / DRIVER EVIDENCE' \
        'CPU and platform sensors, libsensors and the driver probe follow.'; then
        clear_screen
        ui_reset
        printf '%s%s%s[2J%s[H' "$c_bg" "$c_white" "$esc" "$esc"
        printf '%sCPU-DIT-01 SENSOR / DRIVER EVIDENCE%s\n\n' "$c_cyan" "$c_reset$c_bg$c_white"
    fi
    "$DIT" sensors --interval-ms 500 || true
    if command -v sensors >/dev/null 2>&1; then
        printf '\n%sLIBSENSORS VIEW%s\n\n' "$c_cyan" "$c_reset$c_bg$c_white"
        sensors 2>/dev/null || true
    fi
    printf '\n%sDRIVER PROBE%s\n\n' "$c_cyan" "$c_reset$c_bg$c_white"
    sed -n '1,160p' /run/cpu-dit-driver-probe.txt 2>/dev/null || true
    pause_screen
}

trap ui_reset EXIT
trap 'abort_active_run; sigint_seen=1' INT
trap 'ui_reset; exit 0' HUP TERM

if [[ "${1:-}" == --render-once ]]; then
    AUTO_SECONDS=0
    draw_menu
    exit 0
fi
if [[ "${1:-}" == --sensors-once ]]; then
    show_sensors
    exit 0
fi
if [[ "${1:-}" == --render-running-once ]]; then
    if [[ "$POWER_UI" == 1 ]]; then
        draw_power_running 'POWER TRANSITIONS' "$(date +%s)" "${CPU_DIT_UI_PROGRESS_FILE:-/run/no-power-progress}"
        exit 0
    fi
    draw_running 'FULL TEST' 1800 "$(date +%s)" \
        "${CPU_DIT_UI_PROGRESS_FILE:-/run/cpu-dit-ui-no-progress.log}"
    exit 0
fi
if [[ "${1:-}" == --render-running-twice ]]; then
    if [[ "$POWER_UI" == 1 ]]; then
        draw_power_running 'POWER TRANSITIONS' "$(date +%s)" "${CPU_DIT_UI_PROGRESS_FILE:-/run/no-power-progress}"
        draw_power_running 'POWER TRANSITIONS' "$(date +%s)" "${CPU_DIT_UI_PROGRESS_FILE:-/run/no-power-progress}"
        exit 0
    fi
    draw_running 'FULL TEST' 1800 "$(date +%s)" \
        "${CPU_DIT_UI_PROGRESS_FILE:-/run/cpu-dit-ui-no-progress.log}"
    draw_running 'FULL TEST' 1800 "$(date +%s)" \
        "${CPU_DIT_UI_PROGRESS_FILE:-/run/cpu-dit-ui-no-progress.log}"
    exit 0
fi
if [[ "${1:-}" == --run-profile-once ]]; then
    case "${2:-}" in
        safe) run_profile safe 'SAFE TEST' "$SAFE_MINIMUM_DURATION_SECONDS" ;;
        full) run_profile full 'FULL TEST' "$full_duration_seconds" ;;
        power) run_profile power 'POWER TRANSITIONS' 0 ;;
        power-sweep) run_profile power-sweep 'CORE / SMT SWEEP' 0 ;;
        power-campaign) run_profile power-campaign 'UNIFIED POWER CAMPAIGN' "$power_duration_seconds" ;;
        *) printf '%s\n' 'Usage: cpu-dit-ui.sh --run-profile-once safe|full|power|power-sweep|power-campaign' >&2; exit 2 ;;
    esac
    exit 0
fi

first_draw=1
while true; do
    draw_menu
    choice=''
    if (( first_draw == 1 && AUTO_SECONDS > 0 )); then
        IFS= read -r -n 1 -t "$AUTO_SECONDS" choice || choice=1
        first_draw=0
        AUTO_SECONDS=0
    else
        if ! IFS= read -r -n 1 choice; then
            # A trapped Ctrl+C interrupts `read` the same way EOF does; only
            # treat it as EOF (poweroff) when no SIGINT caused the failure.
            if (( sigint_seen )); then
                sigint_seen=0
                choice=''
            else
                if [[ "$POWER_UI" == 1 ]]; then choice=b; else choice=p; fi
            fi
        fi
    fi
    printf '\n'
    case "$choice" in
        1) if [[ "$POWER_UI" == 1 ]]; then run_profile power-campaign 'UNIFIED POWER CAMPAIGN' "$power_duration_seconds"; else run_profile safe 'SAFE TEST' "$SAFE_MINIMUM_DURATION_SECONDS"; fi ;;
        2) if [[ "$POWER_UI" == 1 ]]; then power_advanced_menu; else run_profile full 'FULL TEST' "$full_duration_seconds"; fi ;;
        n|N)
            if [[ "$POWER_UI" == 1 ]]; then
                printf '\nTransitions per group (1..1000): '
                IFS= read -r requested || requested=''
                if [[ "$requested" =~ ^[1-9][0-9]{0,3}$ ]] && (( requested <= 1000 )); then power_transitions="$requested"; fi
            fi ;;
        5) if [[ "$POWER_UI" != 1 ]]; then run_profile compute 'CPU COMPUTE' "$full_duration_seconds"; fi ;;
        3) show_sensors ;;
        4)
            clear_screen
            ui_reset
            printf 'Diagnostic shell. Type exit to return to CPU-DIT-01.\n'
            /bin/bash --noprofile --norc
            ;;
        d|D) if [[ "$POWER_UI" == 1 ]]; then set_power_duration; else set_full_duration; fi ;;
        t|T)
            if (( thermal_limit_enabled == 1 )); then
                thermal_limit_enabled=0
            else
                thermal_limit_enabled=1
            fi
            ;;
        r|R) ui_reset; sync; reboot ;;
        p|P) ui_reset; sync; poweroff ;;
        b|B)
            if [[ "${CPU_DIT_SUITE_CHILD:-0}" == 1 || "$POWER_UI" == 1 ]]; then
                ui_reset
                exit 0
            fi
            ;;
        *) sleep 1 ;;
    esac
done
