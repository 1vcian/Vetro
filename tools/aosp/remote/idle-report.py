#!/usr/bin/env python3
# Report of remote/idle.sh: idle %, load and the busiest processes over the
# measured window, from the guest's /proc/stat and /proc/PID/stat deltas.
#   idle-report.py DIR   (DIR/raw = the guest's /data/local/tmp/vetro-idle)
import os
import re
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


def optional(name):
    try:
        with open(os.path.join(d, name)) as f:
            return f.read()
    except OSError:
        return ""


# Boot times, memory and inventory (ADR 0043), when idle.sh collected them.
m = re.search(r"\[\s*([0-9.]+)\]", optional("boot_completed.txt"))
if m:
    print(f"boot_completed at guest {float(m.group(1)):.0f} s")
home = optional("home").strip()
if home:
    print(f"launcher focused at guest {float(home):.0f} s")
m = re.match(r"\s*([0-9.]+)", optional("displayed.txt"))
if m:
    print(f"launcher displayed at guest {float(m.group(1)):.0f} s")
mem = {}
for line in optional("meminfo.txt").splitlines():
    k, _, v = line.partition(":")
    if v.strip().endswith("kB"):
        mem[k.strip()] = int(v.split()[0])
if mem:
    used = mem["MemTotal"] - mem["MemAvailable"]
    print(f"meminfo: total {mem['MemTotal'] // 1024} MiB, available {mem['MemAvailable'] // 1024} MiB, "
          f"used (total - available) {used // 1024} MiB, free {mem['MemFree'] // 1024} MiB, "
          f"cached {mem['Cached'] // 1024} MiB, anon {(mem.get('AnonPages', 0)) // 1024} MiB, "
          f"shmem {mem.get('Shmem', 0) // 1024} MiB, slab {mem.get('Slab', 0) // 1024} MiB")
pss = [line.split(" ", 1) for line in optional("pss.txt").splitlines() if line.strip()]
pss = [(int(k), (n[0] if n else "").strip()) for k, *n in pss if k.isdigit()]
if pss:
    print(f"PSS of user-space processes: {sum(k for k, _ in pss) // 1024} MiB in {len(pss)} processes; largest:")
    for k, name in pss[:12]:
        print(f"  {k // 1024:5d} MiB  {name[:80]}")
for name, label in (("packages.txt", "packages"), ("apex.txt", "/apex entries"),
                    ("features.txt", "features"), ("services.txt", "binder services"),
                    ("ps.txt", "processes (ps -A)")):
    text = optional(name)
    if text:
        print(f"{label}: {len([x for x in text.splitlines() if x.strip()])}")
