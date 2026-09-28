#!/bin/bash
set -eu
[[ "$1" == power || "$1" == power-sweep || "$1" == power-campaign ]]
[[ "${CPU_DIT_POWER_TRANSITIONS:-}" == 10 ]]
if [[ "$1" == power-campaign ]]; then
    [[ "${CPU_DIT_MIN_DURATION_SECONDS:-}" == 600 ]]
fi
echo 'CPU_DIT_POWER_PROGRESS phase=FINAL completed=10 target=10 workers=2 bursts=20 failures=0 availability=Completed power_platform=NoEvidence seed=0x1234'
[[ "${DIT_TEST_POWER_VERDICT:-Pass}" == NoFinal ]] && exit 0
echo "CPU_DIT_POWER_RESULT verdict=${DIT_TEST_POWER_VERDICT:-Pass} availability=Completed power_platform=NoEvidence"
case "${DIT_TEST_POWER_VERDICT:-Pass}" in
    Pass) exit 0 ;;
    Incomplete) exit 3 ;;
    Fail) exit 2 ;;
esac
