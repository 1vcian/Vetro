#!/usr/bin/env python3
"""Compares two serial logs (QEMU and Vetro) without timestamps.

Removes the printk prefix `[  t][ Tn]`, the duration numbers ("took 12ms",
"0.131 seconds", "duration=131") and the lines that depend only on time, then
prints the first divergence in order and the lines present in only one log
(set comparison, like tests/boot: the order of asynchronous initcalls and
of processes depends on timing).

Usage: tools/android-emu/compare.py qemu.log vetro.log [context lines]
"""
import re
import sys

PREFIX = re.compile(r"^\[\s*\d+\.\d+\]\[\s*[TC]\d+\]\s?")
NOISE = [
    (re.compile(r"took \d+ms"), "took Nms"),
    (re.compile(r"\d+\.\d+ seconds"), "N seconds"),
    (re.compile(r"duration=\d+"), "duration=N"),
    (re.compile(r"\bpid \d+\b"), "pid N"),
    (re.compile(r"\(pid=\d+"), "(pid=N"),
    (re.compile(r"/pid_\d+"), "/pid_N"),
    (re.compile(r"\bT\d+\b"), "Tn"),
    (re.compile(r"\d+\.\d+ BogoMIPS"), "N BogoMIPS"),
    (re.compile(r"lpj=\d+"), "lpj=N"),
    (re.compile(r"klogd: \d+"), "klogd: N"),
    (re.compile(r"tv_sec: \d+"), "tv_sec: N"),
]


def load(path):
    out = []
    for raw in open(path, "rb").read().decode("utf-8", "replace").splitlines():
        line = PREFIX.sub("", raw.rstrip("\r"))
        for rx, rep in NOISE:
            line = rx.sub(rep, line)
        out.append(line)
    return out


def main():
    a, b = load(sys.argv[1]), load(sys.argv[2])
    ctx = int(sys.argv[3]) if len(sys.argv) > 3 else 3
    for i, (x, y) in enumerate(zip(a, b)):
        if x != y:
            print(f"first divergence at line {i + 1}:")
            for j in range(max(0, i - ctx), i):
                print(f"   = {a[j]}")
            for j in range(i, min(len(a), i + ctx)):
                print(f"  q< {a[j]}")
            for j in range(i, min(len(b), i + ctx)):
                print(f"  v> {b[j]}")
            break
    else:
        print(f"no divergence in the first {min(len(a), len(b))} lines")
    sa, sb = set(a), set(b)
    only_a = [x for x in a if x not in sb]
    only_b = [x for x in b if x not in sa]
    print(f"\nlines only in QEMU: {len(only_a)}; only in Vetro: {len(only_b)}")
    for x in only_a[:int(sys.argv[4]) if len(sys.argv) > 4 else 40]:
        print(f"  q< {x}")
    for x in only_b[:int(sys.argv[4]) if len(sys.argv) > 4 else 40]:
        print(f"  v> {x}")


if __name__ == "__main__":
    main()
