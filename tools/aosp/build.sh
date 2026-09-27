#!/bin/sh
# Image build on the VM, from the Mac.
#   tools/aosp/build.sh start    syncs (sync.sh) and launches the build detached
#                                (nohup + setsid: survives the ssh session closing)
#   tools/aosp/build.sh status   status (RUNNING/OK/FAIL) and last lines of the log
#   tools/aosp/build.sh wait     waits for the end, then exits 0 if OK
# The build is incremental: after the VM stops, `start` is enough.
# VETRO_AOSP_CCACHE=1 enables ccache in out/.ccache (see remote/build.sh:
# the first time it recompiles all the C/C++).
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
env="VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK VETRO_AOSP_LUNCH=$VETRO_AOSP_LUNCH VETRO_AOSP_CCACHE=${VETRO_AOSP_CCACHE:-0}"
case "${1:-status}" in
  start)
    # Check first, then sync: never touch the tree under
    # a running build.
    if vm "p=\$(cat $VETRO_AOSP_WORK/build.pid 2>/dev/null) && grep -qs remote/build.sh /proc/\$p/cmdline"; then
      echo "build already running on $VETRO_AOSP_HOST: no sync" >&2
      exit 1
    fi
    "$here/sync.sh"
    vm "$env nohup setsid bash $VETRO_AOSP_WORK/remote/build.sh </dev/null >/dev/null 2>&1 &"
    echo "build launched on $VETRO_AOSP_HOST ($VETRO_AOSP_LUNCH, Vetro version $(vm "cat $VETRO_AOSP_WORK/sync.rev"))" ;;
  status)
    vm "cat $VETRO_AOSP_WORK/build.status 2>/dev/null || echo 'no build'; tail -n 5 $VETRO_AOSP_WORK/build.log 2>/dev/null" ;;
  wait)
    while :; do
      s="$(vm "cat $VETRO_AOSP_WORK/build.status 2>/dev/null" || echo UNREACHABLE)"
      case "$s" in
        OK) echo OK; vm "grep -E '^=== ' $VETRO_AOSP_WORK/build.log | tail -n 4"; exit 0 ;;
        FAIL*) echo "$s"; vm "grep -n -m 20 -E 'FAILED:|error:' $VETRO_AOSP_WORK/build.log | tail -n 20"; exit 1 ;;
      esac
      sleep 300
    done ;;
  *) echo "usage: $0 start|status|wait" >&2; exit 2 ;;
esac
