#!/bin/bash
# Mounts the results volume again once its USB stick is back (dit-suite menu --results-remount).
# The image runs from RAM, so the stick can be pulled and put back at any time; tests run on
# without it and keep their results in tmpfs.
set -u
exec timeout --kill-after=5s 30s bash -c \
    '. /usr/local/lib/cpu-dit/partition-lookup.sh && mount_result_partition'
