#!/bin/bash
# UI workload for comparing SwiftShader and gfxstream (ADR 0037), run on the
# build VM against a booted guest (vetro boot or QEMU with adb forwarded):
#
#   tools/gfxstream/workload.sh ADB_SERIAL [ROUNDS]
#
# Waits for the launcher, then opens and closes the app drawer ROUNDS times
# (default 5) and prints what the guest spent, in guest CPU seconds (USER_HZ
# 100): SurfaceFlinger, the launcher, system_server, the whole CPU (busy and
# idle), and the launcher's frames (dumpsys gfxinfo). Guest time and guest
# CPU are the emulated machine's: they compare the two paths' guest-side cost
# independently of how fast the host runs.
set -u
S=$1
ROUNDS=${2:-5}
A=${ADB:-$HOME/aosp/out/host/linux-x86/bin/adb}
sh_() { timeout 600 "$A" -s "$S" shell "$@" 2>/dev/null; }
until [ "$(sh_ getprop sys.boot_completed | tr -d '\r')" = 1 ]; do sleep 20; done
until sh_ dumpsys window | grep -m1 mCurrentFocus | grep -q -i launcher; do sleep 20; done
sh_ 'svc power stayon true; settings put system screen_off_timeout 2147483647; input keyevent KEYCODE_WAKEUP'
sleep 30
snap() {
  local sf l ss
  sf=$(sh_ pidof surfaceflinger | tr -d '\r')
  l=$(sh_ pidof com.android.launcher3 | tr -d '\r')
  ss=$(sh_ pidof system_server | tr -d '\r')
  # utime+stime of each process, then the aggregate cpu line.
  sh_ "for p in $sf $l $ss; do awk '{print \$14+\$15}' /proc/\$p/stat; done; head -1 /proc/stat; cat /proc/uptime" | tr -d '\r'
}
sh_ dumpsys gfxinfo com.android.launcher3 reset > /dev/null
before=$(snap)
for _ in $(seq "$ROUNDS"); do
  sh_ input swipe 640 700 640 150 250
  sleep 4
  sh_ input keyevent KEYCODE_HOME
  sleep 4
done
after=$(snap)
gfx=$(sh_ dumpsys gfxinfo com.android.launcher3 | grep -E "Total frames rendered|Janky frames|50th percentile|90th percentile|99th percentile" | tr -d '\r')
python3 - "$before" "$after" "$gfx" <<'PY'
import sys
b, a, gfx = sys.argv[1].split('\n'), sys.argv[2].split('\n'), sys.argv[3]
def cpu(line):
    f = [int(x) for x in line.split()[1:]]
    return sum(f), f[3] + f[4]  # total, idle+iowait
names = ['surfaceflinger', 'launcher3', 'system_server']
print('guest CPU seconds during the workload:')
for i, n in enumerate(names):
    print(f'  {n:16s} {(int(a[i]) - int(b[i])) / 100:8.2f}')
(tb, ib), (ta, ia) = cpu(b[3]), cpu(a[3])
up = float(a[4].split()[0]) - float(b[4].split()[0])
print(f'  {"all busy":16s} {((ta - tb) - (ia - ib)) / 100:8.2f}')
print(f'  {"idle":16s} {(ia - ib) / 100:8.2f}')
print(f'  guest time       {up:8.2f} s')
print(gfx)
PY
