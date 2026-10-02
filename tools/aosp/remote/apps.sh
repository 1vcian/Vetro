#!/bin/bash
# On the build VM (launched detached by tools/aosp/qemu-vm.sh apps): the app
# catalog (ADR 0033, catalog/v1.json on R2) on a booted guest, after
# remote/idle.sh has finished (ADR 0043, slim image): every app downloaded
# once (checked against the catalog's sha256, cached in ~/$WORK/cache),
# installed with adb, opened from its launcher activity, then checked after
# $VETRO_APPS_WAIT guest seconds (default 60): the app's process alive, its
# window focused, no crash in the crash buffer. A screenshot of each app.
# Output in ~/$WORK/qemu/$VETRO_QEMU_NAME/apps/ (report.txt; state in status).
set -uo pipefail
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
name="${VETRO_QEMU_NAME:-aosp}"
port="${VETRO_ADB_PORT:-5565}"
wait_s="${VETRO_APPS_WAIT:-60}"
catalog="${VETRO_CATALOG:-https://pub-06e88fdd7f374fffb06844d60083f2ae.r2.dev/catalog/v1.json}"
d="$work/qemu/$name/apps"
cache="$work/cache"
A="$tree/out/host/linux-x86/bin/adb"
s="127.0.0.1:$port"
mkdir -p "$work/qemu/$name" "$cache"
# One check per guest at a time.
exec 9>"$work/qemu/$name/apps.lock"
flock -n 9 || { echo "apps.sh already running for $name" >&2; exit 1; }
rm -rf "$d"; mkdir -p "$d"
log() { echo "$(date -u +%T) $*" >> "$d/log"; }
st() { echo "$*" > "$d/status"; log "$*"; }
# stdin from /dev/null: adb would read the loop's list of apps.
sh_() { timeout 600 "$A" -s "$s" shell "$@" 2>/dev/null </dev/null; }
sub() { awk -v a="$1" -v b="$2" 'BEGIN { print a - b }'; }
up() { sh_ cat /proc/uptime | cut -d' ' -f1; }
"$A" connect "$s" >/dev/null 2>&1
# Waits for idle.sh (one measurement at a time on the guest).
while [ "$(cat "$work/qemu/$name/idle/status" 2>/dev/null)" != DONE ]; do st WAIT-IDLE; sleep 60; done
st DOWNLOAD
curl -fsS "$catalog" -o "$d/catalog.json" || { st "FAIL catalog"; exit 1; }
python3 - "$d/catalog.json" "$catalog" > "$d/apps.tsv" <<'EOF'
import json, sys, urllib.parse
c = json.load(open(sys.argv[1]))
for a in c["apps"]:
    print("\t".join([a["id"], a["package"], urllib.parse.urljoin(sys.argv[2], a["apk"]), a["sha256"]]))
EOF
sh_ logcat -b crash -c
: > "$d/report.txt"
fails=0
while IFS=$'\t' read -r id pkg url sha; do
  f="$cache/$(basename "$url")"
  if [ "$(sha256sum "$f" 2>/dev/null | cut -d' ' -f1)" != "$sha" ]; then
    curl -fsS "$url" -o "$f.part" && mv "$f.part" "$f"
  fi
  [ "$(sha256sum "$f" | cut -d' ' -f1)" = "$sha" ] || { echo "$id: FAIL download (sha256)" >> "$d/report.txt"; fails=$((fails + 1)); continue; }
  st "INSTALL $id"
  g0="$(up)"; t0=$(date +%s)
  out="$(timeout 3600 "$A" -s "$s" install -r -g "$f" 2>&1 </dev/null | tail -n 1)"
  g1="$(up)"; t1=$(date +%s)
  if [ "$out" != Success ]; then
    echo "$id: FAIL install: $out" >> "$d/report.txt"; fails=$((fails + 1)); continue
  fi
  comp="$(sh_ cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER "$pkg" | tr -d '\r' | tail -n 1)"
  st "OPEN $id $comp"
  start="$(sh_ am start -W -n "$comp" | tr -d '\r')"
  g2="$(up)"
  total="$(echo "$start" | sed -n 's/^TotalTime: //p')"
  sleep "$wait_s"
  focus="$(sh_ dumpsys window | grep -m1 'mCurrentFocus=' | tr -d '\r')"
  pid="$(sh_ pidof "$pkg" | tr -d '\r')"
  crash="$(sh_ logcat -b crash -d | tr -d '\r')"
  "$A" -s "$s" exec-out screencap -p > "$d/$id.png" 2>/dev/null </dev/null
  ok=yes
  case "$focus" in *"$pkg"*) ;; *) ok=no ;; esac
  [ -n "$pid" ] || ok=no
  case "$crash" in *"$pkg"*) ok=no ;; esac
  [ "$ok" = yes ] || fails=$((fails + 1))
  printf '%s: %s install %.0f s guest (%d s wall), open %s ms (am start, guest %.0f s), pid [%s], focus [%s]\n' \
    "$id" "$([ "$ok" = yes ] && echo OK || echo FAIL)" "$(sub "$g1" "$g0")" $((t1 - t0)) "${total:-?}" "$(sub "$g2" "$g1")" "$pid" "${focus#*mCurrentFocus=}" >> "$d/report.txt"
  [ -z "$crash" ] || { echo "$crash" > "$d/$id.crash.txt"; sh_ logcat -b crash -c; }
  sh_ input keyevent KEYCODE_HOME
  sleep 10
done < "$d/apps.tsv"
echo "failures: $fails" >> "$d/report.txt"
st DONE
