#!/bin/sh
# Build dell'immagine sulla VM, dal Mac.
#   tools/aosp/build.sh start    sincronizza (sync.sh) e lancia la build staccata
#                                (nohup + setsid: sopravvive alla chiusura di ssh)
#   tools/aosp/build.sh status   stato (RUNNING/OK/FAIL) e ultime righe del log
#   tools/aosp/build.sh wait     aspetta la fine, poi esce 0 se OK
# La build è incrementale: dopo un arresto della VM (Spot) basta `start`.
set -eu
. "$(cd "$(dirname "$0")" && pwd)/common.sh"
env="VETRO_AOSP_TREE=$VETRO_AOSP_TREE VETRO_AOSP_WORK=$VETRO_AOSP_WORK VETRO_AOSP_LUNCH=$VETRO_AOSP_LUNCH"
case "${1:-status}" in
  start)
    "$here/sync.sh"
    if vm "p=\$(cat $VETRO_AOSP_WORK/build.pid 2>/dev/null) && grep -qs remote/build.sh /proc/\$p/cmdline"; then
      echo "build già in corso" >&2
    else
      vm "$env nohup setsid bash $VETRO_AOSP_WORK/remote/build.sh </dev/null >/dev/null 2>&1 &"
      echo "build lanciata su $VETRO_AOSP_HOST ($VETRO_AOSP_LUNCH)"
    fi ;;
  status)
    vm "cat $VETRO_AOSP_WORK/build.status 2>/dev/null || echo 'nessuna build'; tail -n 5 $VETRO_AOSP_WORK/build.log 2>/dev/null" ;;
  wait)
    while :; do
      s="$(vm "cat $VETRO_AOSP_WORK/build.status 2>/dev/null" || echo UNREACHABLE)"
      case "$s" in
        OK) echo OK; exit 0 ;;
        FAIL*) echo "$s"; vm "grep -n -m 20 -E 'FAILED:|error:' $VETRO_AOSP_WORK/build.log | tail -n 20"; exit 1 ;;
      esac
      sleep 300
    done ;;
  *) echo "uso: $0 start|status|wait" >&2; exit 2 ;;
esac
