#!/usr/bin/env python3
"""§105.14: round-trip statistics per launcher run, with the samples that met a
GPU power transition set apart.

    wc-hop-stats.py RUNDIR [GPU_WAKES]

RUNDIR as for wc-hop-timeline.py (guest/L-*.txt, the launcher's stamps, and
the proxy's, which fit the clocks). GPU_WAKES: a file of host CLOCK_MONOTONIC
seconds, one per line, at which the passed-through GPU left D3 (`journalctl
-o short-monotonic | grep 'PM transition D3->D0'`); on the Windows guest each
can freeze every vCPU for up to ~6 s (§105.14). A sample whose round trip
overlaps [wake - 0.2 s, wake + 7 s] is counted apart and named, never
dropped silently.
"""
import glob, os, re, statistics, subprocess, sys

run = sys.argv[1]
wakes = []
if len(sys.argv) > 2:
    wakes = sorted(float(l.split()[0]) * 1e9 for l in open(sys.argv[2]) if l.strip())
here = os.path.dirname(os.path.abspath(__file__))
out = subprocess.run([sys.executable, os.path.join(here, "wc-hop-timeline.py"), run], capture_output=True, text=True).stdout
m = re.search(r"offset (-?[\d.]+) us", out)
off = float(m.group(1)) * 1000 if m else None
print(out.splitlines()[0] if out else "clock: none")


def q(v, f):
    return v[min(len(v) - 1, int(round((len(v) - 1) * f)))]


def line(tag, v):
    if not v:
        return f"  {tag:<28} n=0"
    s = sorted(v)
    return f"  {tag:<28} n={len(s):5d} mean {statistics.mean(s):8.1f} p50 {q(s,.5):7.1f} p90 {q(s,.9):7.1f} p99 {q(s,.99):8.1f} max {s[-1]:9.1f} us"


for f in sorted(glob.glob(os.path.join(run, "guest", "L-*.txt"))):
    ev = [l.split() for l in open(f)]
    ev = [(int(a[0]), a[1]) for a in ev if len(a) == 2]
    reqs, s = [], None
    for t, n in ev:
        if n == "L.send":
            s = t
        elif n == "L.recv" and s is not None:
            reqs.append((s, t))
            s = None
    if not reqs:
        continue
    gaps = [(b[0] - a[1]) / 1e6 for a, b in zip(reqs, reqs[1:])]
    gap = statistics.median(gaps) if gaps else 0
    clean, hit = [], []
    for s, r in reqs:
        us = (r - s) / 1e3
        if off is not None and wakes:
            hs, hr = s - off, r - off
            if any(w - 2e8 <= hr and hs <= w + 7e9 for w in wakes):
                hit.append(us)
                continue
        clean.append(us)
    print(f"{os.path.basename(f)}: gap {gap:.0f} ms")
    print(line("all", clean + hit))
    if wakes:
        print(line("outside GPU wakes", clean))
        print(line("within a GPU wake (set apart)", hit))
