#!/usr/bin/env python3
"""Where the host's CPU went while a frame was being made (waycompose-design.md §88).

    wc-cpu-sample.py SECS OUT [COMM...]

Samples /proc every second for SECS seconds and writes a summary to OUT:
each CPU's busy share, and the busiest threads of every process whose name
starts with one of COMM (default: firefox, Renderer, wc-proxy, sv-hostd,
sshd), as a share of one CPU. On testhost the host's own work is confined
to the cpuset of `user.slice`; this says whether those CPUs were saturated.
Standard library only.
"""

import os
import sys
import time

SECS, OUT = float(sys.argv[1]), sys.argv[2]
COMMS = sys.argv[3:] or ["firefox", "Renderer", "wc-proxy", "sv-hostd", "sshd", "Isolated", "WebRender",
                         "Socket", "Compositor", "Web Content", "GPU", "glxtest", "RDD"]
HZ = os.sysconf("SC_CLK_TCK")


def cpu_times():
    out = {}
    for line in open("/proc/stat"):
        if line.startswith("cpu") and line[3].isdigit():
            f = line.split()
            vals = list(map(int, f[1:]))
            idle = vals[3] + vals[4]
            out[f[0]] = (sum(vals), idle)
    return out


def proc_comm(pid):
    try:
        return open(f"/proc/{pid}/comm").read().strip()
    except OSError:
        return None


def thread_times():
    out = {}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        pc = proc_comm(pid)
        if pc is None:
            continue
        try:
            tids = os.listdir(f"/proc/{pid}/task")
        except OSError:
            continue
        for tid in tids:
            try:
                s = open(f"/proc/{pid}/task/{tid}/stat").read()
            except OSError:
                continue
            r = s.rfind(")")
            tcomm = s[s.find("(") + 1:r]
            f = s[r + 2:].split()
            ticks = int(f[11]) + int(f[12])
            cpu = int(f[36])
            if any(pc.startswith(c) or tcomm.startswith(c) for c in COMMS):
                out[(pid, tid)] = (pc, tcomm, ticks, cpu)
    return out


c0, t0, w0 = cpu_times(), thread_times(), time.monotonic()
samples = []
end = w0 + SECS
while time.monotonic() < end:
    time.sleep(1.0)
    samples.append(cpu_times())
c1, t1, w1 = cpu_times(), thread_times(), time.monotonic()
wall = w1 - w0

lines = [f"sampled {wall:.1f} s"]
busy = {}
for k in sorted(c1, key=lambda k: int(k[3:])):
    if k in c0:
        dt = c1[k][0] - c0[k][0]
        di = c1[k][1] - c0[k][1]
        busy[k] = 0 if dt == 0 else 100.0 * (dt - di) / dt
lines.append("per CPU busy %: " + " ".join(f"{k[3:]}:{v:.0f}" for k, v in busy.items()))
# Per-second peak for each CPU: an average hides a CPU that was pinned for the
# two seconds the wheel turned.
peaks = {}
prev = c0
for s in samples:
    for k in s:
        if k in prev:
            dt = s[k][0] - prev[k][0]
            di = s[k][1] - prev[k][1]
            if dt:
                peaks[k] = max(peaks.get(k, 0), 100.0 * (dt - di) / dt)
    prev = s
lines.append("per CPU peak 1 s %: " + " ".join(f"{k[3:]}:{v:.0f}" for k, v in sorted(peaks.items(), key=lambda kv: int(kv[0][3:]))))
rows = []
for key, (pc, tc, ticks, cpu) in t1.items():
    before = t0.get(key, (pc, tc, ticks, cpu))[2] if key in t0 else 0
    share = 100.0 * (ticks - before) / HZ / wall
    if share >= 1.0:
        rows.append((share, pc, tc, key[1], cpu))
rows.sort(reverse=True)
by_proc = {}
for share, pc, tc, tid, cpu in rows:
    by_proc[pc] = by_proc.get(pc, 0) + share
lines.append("by process (% of one CPU): " + ", ".join(f"{p} {v:.0f}" for p, v in sorted(by_proc.items(), key=lambda kv: -kv[1])))
lines.append("busiest threads (% of one CPU, last CPU):")
for share, pc, tc, tid, cpu in rows[:16]:
    lines.append(f"  {share:5.1f}  {pc}/{tc} tid {tid} cpu {cpu}")
open(OUT, "w").write("\n".join(lines) + "\n")
print("\n".join(lines))
