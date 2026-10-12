#!/bin/bash
# FC0-FC7 checks inside the DIT-01 image (QEMU), run from the control center's shell (menu "d").
# Each step says where it is on the serial port (FCTEST ...) so the host can press keys, take a
# screenshot or reset the machine at the right moment.
R=/mnt/cpu-dit-results/fct
mkdir -p "$R"
exec 3>/dev/ttyS0 2>>"$R/script.err"
say() { printf 'FCTEST %s\n' "$*" >&3; printf '%s %s\n' "$(date +%T)" "$*" >>"$R/steps.log"; }
CLAIMS=/mnt/cpu-dit-results/claims
say start

{
    echo "== own cgroup"; cat /proc/self/cgroup
    echo "DIT_TEST_CGROUP=$DIT_TEST_CGROUP"
    echo "== sysrq"; cat /proc/sys/kernel/sysrq
    echo "== services"; systemctl is-active dit-blackbox dit-rescue cpu-dit
    echo "== tests memory.max / app memory.min"
    cat "$DIT_TEST_CGROUP/memory.max" "${DIT_TEST_CGROUP%/tests}/app/memory.min"
    echo "== service subtree_control"; cat "${DIT_TEST_CGROUP%/tests}/cgroup.subtree_control"
    echo "== cgroup setup log"; cat /run/dit-cgroup-setup.log
    echo "== service cgroup"; ls /sys/fs/cgroup/system.slice/cpu-dit.service/
    echo "== meminfo"; head -3 /proc/meminfo
    echo "== rescue and black box processes"
    ps -o pid,ni,rss,stat,args -C dit-rescue,dit-blackbox
    echo "== locked memory"; grep -H VmLck /proc/$(pgrep -x dit-rescue)/status /proc/$(pgrep -x dit-blackbox)/status
} >"$R/env.txt" 2>&1

start_combined() {
    dit-suite combined --level bounded --time-scale-percent 30 --reserve-mib 256 \
        --cpu-ram-minimum-mib 32 </dev/null >"$R/$1-app.log" 2>&1 &
    echo $!
}

# The session the app `pid` opened, once its CPU-RAM layer runs.
wait_layer() {
    local pid="$1" d i
    for i in $(seq 1 400); do
        for d in /run/dit-suite/suite-combined-*/; do
            grep -q "\"app_pid\":$pid," "$d/session.json" 2>/dev/null || continue
            if grep -q '"name":"cpu-ram"' "$d/session-status.json" 2>/dev/null; then
                basename "$d"
                return 0
            fi
        done
        sleep 1
    done
    return 1
}

wait_claim() {
    local run="$1" seconds="$2" i
    for i in $(seq 1 "$seconds"); do
        [[ -f "$CLAIMS/$run.claim" ]] && { head -1 "$CLAIMS/$run.claim"; return 0; }
        sleep 1
    done
    echo none
    return 1
}

leftovers() {
    pkill -KILL -f 'dit-watchdog supervise' 2>/dev/null
    pkill -KILL -f 'dit-cli system-run' 2>/dev/null
    sleep 2
}

keep() {
    local run="$1" step="$2"
    mkdir -p "$R/$step"
    cp -r "/run/dit-suite/$run" "$R/$step/session" 2>/dev/null
    cp "/mnt/cpu-dit-results/blackbox/$run.jsonl" "$R/$step/" 2>/dev/null
    ls "/sys/fs/cgroup${DIT_TEST_CGROUP#/sys/fs/cgroup}" >"$R/$step/test-cgroups.txt" 2>&1
}

# S1 (FC3a): the app killed outright; the black box writes the result after 10 s.
pid="$(start_combined s1)"
run="$(wait_layer "$pid")"
sleep 10
kill -9 "$pid"
say "S1 killed $run"
by="$(wait_claim "$run" 120)"
sleep 5
say "S1 claim $by"
keep "$run" s1
say "S1 screen"
sleep 25
leftovers

# S2 (FC3a): the app frozen (SIGSTOP); the black box writes the result after 60 + 30 s.
pid="$(start_combined s2)"
run="$(wait_layer "$pid")"
sleep 10
kill -STOP "$pid"
say "S2 stopped $run"
by="$(wait_claim "$run" 300)"
say "S2 claim $by"
keep "$run" s2
kill -9 "$pid"
leftovers

# S3 (FC1): stopped from the rescue console; the app writes its own result, marked.
pid="$(start_combined s3)"
run="$(wait_layer "$pid")"
sleep 10
say "S3 ready $run"
by="$(wait_claim "$run" 300)"
say "S3 claim $by"
keep "$run" s3
for i in $(seq 1 120); do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
say "S3 app ended"
sleep 30
leftovers

# S4 (FC2): the memory guard stops a CPU-RAM worker when memory runs short (its limit is set
# just under what is free now, so a modest allocation trips it).
d=/run/fct-guard
rm -rf "$d"; mkdir -p "$d"
avail="$(awk '/^MemAvailable:/ { print int($2 / 1024) }' /proc/meminfo)"
limit=$((avail - 300))
dit-watchdog supervise --module cpu-ram --run-id fct-guard --run-dir "$d" --heartbeat "$d/hb" \
    --memory-guard --memory-guard-min-available-mib "$limit" --stale-after-seconds 600 \
    -- /bin/sh -c "touch $d/hb; exec sleep 900" >"$R/s4-watchdog.log" 2>&1 &
wpid=$!
sleep 5
head -c 600M /dev/zero >/dev/shm/fct-hog
for i in $(seq 1 60); do kill -0 "$wpid" 2>/dev/null || break; sleep 1; done
wait "$wpid"; code=$?
rm -f /dev/shm/fct-hog
cp "$d/watchdog-final.json" "$R/s4-watchdog-final.json" 2>/dev/null
say "S4 exit $code limit ${limit} MiB"

# S6 (FC3b): the machine reset in the middle of a session; the next boot's post-mortem.
pid="$(start_combined s6)"
run="$(wait_layer "$pid")"
sleep 30
sync
echo "$run" >"$R/s6-run"
sync
say "S6 reset $run"
sleep 900
