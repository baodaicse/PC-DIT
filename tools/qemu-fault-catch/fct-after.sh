#!/bin/bash
# After the reset (FC3b): what the post-mortem wrote, then power off.
R=/mnt/cpu-dit-results/fct
mkdir -p "$R/after"
exec 3>/dev/ttyS0 2>>"$R/after/script.err"
say() { printf 'FCTEST %s\n' "$*" >&3; printf '%s %s\n' "$(date +%T)" "$*" >>"$R/steps.log"; }
say after start
cp /run/dit-suite/postmortem-notice.txt "$R/after/" 2>/dev/null
journalctl -b -u dit-blackbox -u dit-rescue --no-pager >"$R/after/services.log" 2>&1
journalctl -b -k --no-pager | grep -i "reset reason" >"$R/after/reset-reason.txt" 2>&1
ls -la /mnt/cpu-dit-results/postmortem /mnt/cpu-dit-results/claims \
    /mnt/cpu-dit-results/blackbox >"$R/after/listing.txt" 2>&1
grep -h '"state"' /mnt/cpu-dit-results/watchdog/*.json >"$R/after/markers.txt" 2>&1
sync
say after done
sleep 2
poweroff
