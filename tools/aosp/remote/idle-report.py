#!/usr/bin/env python3
# Report of remote/idle.sh: idle %, load and the busiest processes over the
# measured window, from the guest's /proc/stat and /proc/PID/stat deltas.
#   idle-report.py DIR   (DIR/raw = the guest's /data/local/tmp/vetro-idle)
import os
import sys

d = sys.argv[1]
raw = os.path.join(d, "raw")
if not os.path.isdir(raw):
    raw = os.path.join(d, "raw", "vetro-idle")


def read(name):
    with open(os.path.join(raw, name)) as f:
        return f.read()


def cpu(name):
    for line in read(name).splitlines():
        if line.startswith("cpu "):
            return [int(x) for x in line.split()[1:]]
    raise SystemExit("no cpu line in " + name)


def pids(name):
    out = {}
    for line in read(name).splitlines():
        a, b = line.find("("), line.rfind(")")
        if a < 0 or b < 0:
            continue
        pid = int(line[:a])
        rest = line[b + 2:].split()
        # rest[0] is field 3 (state): utime = field 14, stime = field 15.
        out[pid] = (line[a + 1:b], int(rest[11]) + int(rest[12]))
    return out


c0, c1 = cpu("stat.0"), cpu("stat.1")
delta = [b - a for a, b in zip(c0, c1)]
total = sum(delta[:8])  # user nice system idle iowait irq softirq steal
idle = delta[3] + delta[4]
up0 = float(read("uptime.0").split()[0])
up1 = float(read("uptime.1").split()[0])
print(f"window: guest {up0:.0f} -> {up1:.0f} s ({up1 - up0:.0f} s)")
names = "user nice system idle iowait irq softirq steal".split()
print("cpu: " + ", ".join(f"{n} {100 * v / total:.1f}%" for n, v in zip(names, delta[:8])))
print(f"IDLE {100 * idle / total:.1f}%  (busy {100 * (total - idle) / total:.1f}%)")
loads = [line.split()[:3] for line in read("load").splitlines() if line.strip()]
if loads:
    l1 = [float(x[0]) for x in loads]
    print(f"loadavg 1 min: first {l1[0]:.2f}, last {l1[-1]:.2f}, mean {sum(l1) / len(l1):.2f}; "
          f"last line {' '.join(loads[-1])}")
cmd = {}
for line in read("cmd").splitlines():
    p = line.split(" ", 1)
    if p[0].isdigit():
        cmd[int(p[0])] = (p[1].strip() if len(p) > 1 else "")
p0, p1 = pids("pids.0"), pids("pids.1")
rows = []
for pid, (comm, t1) in p1.items():
    t0 = p0.get(pid, (comm, 0))[1]
    if t1 - t0 > 0:
        rows.append((t1 - t0, pid, cmd.get(pid) or f"[{comm}]"))
rows.sort(reverse=True)
# Processes that exited during the window are counted in /proc/stat only.
print(f"processes with CPU in the window: {len(rows)}; exited during it: {len(p0.keys() - p1.keys())}, "
      f"new: {len(p1.keys() - p0.keys())}")
print("top processes (% of the window's CPU time):")
for t, pid, name in rows[:15]:
    print(f"  {100 * t / total:5.1f}%  {pid:6d}  {name[:90]}")
