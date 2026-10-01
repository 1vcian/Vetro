#!/bin/bash
# On the build VM (launched detached by tools/aosp/qemu-vm.sh idle): how idle
# the guest is once the home screen has settled (ADR 0037, "Idle guest").
#   1. waits for sys.boot_completed, applies the app's settings after adb
#      connects (screen on, `light` graphics: web/node/android.mjs
#      ANDROID_WAKE and ANDROID_GRAPHICS.light), waits for the launcher focused;
#   2. settles: at least $VETRO_IDLE_SETTLE s (default 600) after the launcher,
#      and until no dumpstate/dex2oat/crash_dump is running (at most
#      $VETRO_IDLE_MAX_SETTLE s, default 2400), polling once a minute (cheap
#      commands only: adb itself costs guest CPU);
#   3. measures $VETRO_IDLE_SECS (default 120) guest seconds with ONE adb
#      command: /proc/stat and every /proc/PID/stat before and after,
#      /proc/loadavg every 10 s; then `dumpsys cpuinfo` and `top`;
#   4. remote/idle-report.py writes report.txt (idle %, load, top processes).
# Output in ~/$WORK/qemu/$VETRO_QEMU_NAME/idle/ (state in idle/status).
set -uo pipefail
work="$HOME/${VETRO_AOSP_WORK:-vetro-aosp}"
tree="$HOME/${VETRO_AOSP_TREE:-aosp}"
name="${VETRO_QEMU_NAME:-aosp}"
port="${VETRO_ADB_PORT:-5565}"
secs="${VETRO_IDLE_SECS:-120}"
settle="${VETRO_IDLE_SETTLE:-600}"
max_settle="${VETRO_IDLE_MAX_SETTLE:-2400}"
d="$work/qemu/$name/idle"
A="$tree/out/host/linux-x86/bin/adb"
s="127.0.0.1:$port"
rm -rf "$d"; mkdir -p "$d"
log() { echo "$(date -u +%T) $*" >> "$d/log"; }
st() { echo "$*" > "$d/status"; log "$*"; }
sh_() { timeout 120 "$A" -s "$s" shell "$@" 2>/dev/null; }
up() { sh_ cat /proc/uptime | cut -d' ' -f1; }
st WAIT-BOOT
while :; do
  "$A" connect "$s" >/dev/null 2>&1
  [ "$(sh_ getprop sys.boot_completed | tr -d '\r')" = 1 ] && break
  sleep 30
done
log "boot_completed seen at guest $(up)"
sh_ "svc power stayon true; settings put system screen_off_timeout 2147483647; input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard; settings put global window_animation_scale 0; settings put global transition_animation_scale 0; settings put global animator_duration_scale 0.5; settings put global disable_window_blurs 1" >/dev/null
st WAIT-HOME
while :; do
  sh_ dumpsys window | grep -q 'mCurrentFocus=.*[Ll]auncher' && break
  sleep 30
done
home="$(up)"
log "launcher focused at guest $home"
echo "$home" > "$d/home"
# Launcher drawn (ActivityTaskManager's "Displayed", guest seconds since boot).
sh_ "logcat -d -v monotonic -s ActivityTaskManager:I | grep -m1 'Displayed com.android.launcher3'" > "$d/displayed.txt"
st SETTLE
t0=$(date +%s)
while :; do
  el=$(( $(date +%s) - t0 ))
  busy="$(sh_ 'pidof dumpstate dex2oat64 dex2oat crash_dump64 2>/dev/null; true' | tr -d '\r')"
  log "settle ${el}s load $(sh_ cat /proc/loadavg | tr -d '\r') busy=[$busy]"
  if [ "$el" -ge "$settle" ] && [ -z "$busy" ]; then break; fi
  if [ "$el" -ge "$max_settle" ]; then log "not settled after ${el}s: measuring anyway"; break; fi
  sleep 60
done
st MEASURE
cat > "$d/sample.sh" <<EOF
o=/data/local/tmp/vetro-idle
rm -rf \$o; mkdir -p \$o
snap() {
  cat /proc/stat > \$o/stat.\$1
  for p in /proc/[0-9]*; do read -r l < \$p/stat 2>/dev/null && echo "\$l"; done > \$o/pids.\$1
}
cat /proc/uptime > \$o/uptime.0
snap 0
i=0
while [ \$i -lt $secs ]; do cat /proc/loadavg >> \$o/load; sleep 10; i=\$((i + 10)); done
snap 1
cat /proc/uptime > \$o/uptime.1
for p in /proc/[0-9]*; do printf '%s ' \${p#/proc/}; tr '\0' ' ' < \$p/cmdline 2>/dev/null; echo; done > \$o/cmd
EOF
"$A" -s "$s" push "$d/sample.sh" /data/local/tmp/vetro-idle.sh >/dev/null
timeout $((secs * 4 + 300)) "$A" -s "$s" shell su 0 sh /data/local/tmp/vetro-idle.sh
"$A" -s "$s" pull /data/local/tmp/vetro-idle "$d/raw" >/dev/null
sh_ dumpsys cpuinfo > "$d/cpuinfo.txt"
sh_ top -b -n 1 -m 20 -o PID,USER,S,%CPU,TIME+,CMDLINE > "$d/top.txt"
sh_ ps -A -o PID,NAME > "$d/ps.txt"
# Memory and inventory of the settled guest (ADR 0043): /proc/meminfo, the
# framework's RAM summary and per-process PSS, packages, APEXes, features and
# system services; boot_completed from the serial console (init.vetro.rc).
sh_ cat /proc/meminfo > "$d/meminfo.txt"
sh_ dumpsys meminfo > "$d/dumpsys-meminfo.txt"
sh_ pm list packages -f > "$d/packages.txt"
sh_ pm list features > "$d/features.txt"
sh_ ls /apex > "$d/apex.txt"
sh_ service list > "$d/services.txt"
grep -a -m1 'VETRO: sys.boot_completed=1' "$work/qemu/$name/serial.log" > "$d/boot_completed.txt" || true
python3 "$work/remote/idle-report.py" "$d" > "$d/report.txt" 2>&1
st DONE
