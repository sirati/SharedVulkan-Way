#!/usr/bin/env python3
"""§105.14: the host's half of each parked control request, from bpftrace + the proxy's hops.

RUNDIR holds host/proxy-*.txt, host/bpf.txt (scripts/wc-hop-host.bt run as
root with the QEMU, hub and proxy pids) and run.log with a line naming them
("qemu PID hub PID proxy PID").

    wc-hop-host.py RUNDIR

For every proxy `P.rx` that follows a `P.unpark` (the request found the proxy
parked, so it crossed by doorbell), walk back through the kernel trace:

  qW   QEMU's main thread woken (by a vCPU thread: the ioeventfd kick)
  qR   QEMU's main thread running
  qO   QEMU writes the byte to the hub (sendmsg/write, 1 byte)
  hW/hR  the hub's reader woken / running
  hI   the hub reads it;  hO the hub's send to the proxy
  pW/pR  the proxy's doorman woken / running;  pI its read
  door  `door.rx` (proxy stamp);  cW/cR the control thread woken / running
  unpark, rx  proxy stamps
"""
import bisect, collections, glob, os, statistics, sys

run = sys.argv[1]
hops = []
for f in glob.glob(os.path.join(run, "host", "proxy-*.txt")):
    for l in open(f):
        a = l.split()
        if len(a) == 2:
            hops.append((int(a[0]), a[1]))
hops.sort()

T = {}          # tid -> (pid, comm)
ev = collections.defaultdict(list)  # kind -> list of tuples
qpid = hpid = ppid = None
for l in open(os.path.join(run, "run.log")):
    if " qemu " in l:
        w = l.split()
        qpid, hpid, ppid = int(w[w.index("qemu") + 1]), int(w[w.index("hub") + 1]), int(w[w.index("proxy") + 1])
for l in open(os.path.join(run, "host", "bpf.txt"), errors="replace"):
    a = l.split()
    if len(a) < 3 or len(a[0]) != 1:
        continue
    k = a[0]
    try:
        t = int(a[1])
    except ValueError:
        continue
    if k == "T":
        T[int(a[3])] = (int(a[2]), a[4] if len(a) > 4 else "")
    elif k == "W":   # W ns tid target_cpu waker_tid waker_cpu comm
        ev["W"].append((t, int(a[2]), int(a[3]), int(a[4])))
    elif k == "R":   # R ns tid cpu cstate idle_exit_ago
        ev["R"].append((t, int(a[2]), int(a[3]), int(a[4]), int(a[5])))
    elif k == "O":   # O ns pid tid fd ret t_enter
        ev["O"].append((t, int(a[2]), int(a[3]), int(a[4])))
    elif k == "I":
        ev["I"].append((t, int(a[2]), int(a[3]), int(a[4])))
    elif k == "F":
        ev["F"].append((t, int(a[2])))
for k in ev:
    ev[k].sort()
times = {k: [e[0] for e in v] for k, v in ev.items()}

def last_before(kind, t, pred, window=3_000_000):
    i = bisect.bisect_right(times[kind], t) - 1
    while i >= 0 and t - ev[kind][i][0] <= window:
        if pred(ev[kind][i]):
            return ev[kind][i]
        i -= 1
    return None

def first_after(kind, t, pred, window=3_000_000):
    i = bisect.bisect_left(times[kind], t)
    while i < len(ev[kind]) and ev[kind][i][0] - t <= window:
        if pred(ev[kind][i]):
            return ev[kind][i]
        i += 1
    return None

tid_pid = {tid: p for tid, (p, _) in T.items()}
rows = []
prev = None
for i, (t, name) in enumerate(hops):
    if name == "P.rx" and prev and prev[1] == "P.unpark" and t - prev[0] < 2_000_000:
        rx = t
        door = next((h for h in reversed(hops[max(0, i - 6):i]) if h[1] == "door.rx" and rx - h[0] < 2_000_000), None)
        if not door:
            prev = (t, name); continue
        d = door[0]
        pI = last_before("I", d, lambda e: e[1] == ppid)
        if not pI:
            prev = (t, name); continue
        doorman = pI[2]
        hO = last_before("O", pI[0], lambda e: e[1] == hpid)
        hI = last_before("I", hO[0], lambda e: e[1] == hpid) if hO else None
        qO = last_before("O", hI[0], lambda e: e[1] == qpid) if hI else None
        if not (hO and hI and qO):
            prev = (t, name); continue
        qtid = qO[2]
        qW = last_before("W", qO[0], lambda e: e[1] == qtid)
        qR = first_after("R", qW[0], lambda e: e[1] == qtid) if qW else None
        hW = last_before("W", hI[0], lambda e: e[1] == hI[2])
        hR = first_after("R", hW[0], lambda e: e[1] == hI[2]) if hW else None
        pW = last_before("W", pI[0], lambda e: e[1] == doorman)
        pR = first_after("R", pW[0], lambda e: e[1] == doorman) if pW else None
        unpark = prev[0]
        ctl = [tid for tid, p in tid_pid.items() if p == ppid]
        # The control thread: woken by the doorman (before §105.14) or by the
        # hub itself (after), whichever came first after the hub read the byte.
        cW = first_after("W", hI[0], lambda e: tid_pid.get(e[1]) == ppid and e[1] != doorman and e[3] in (doorman, hI[2]))
        cR = first_after("R", cW[0], lambda e: e[1] == cW[1]) if cW else None
        r = dict(qW=qW, qR=qR, qO=qO, hW=hW, hR=hR, hI=hI, hO=hO, pW=pW, pR=pR, pI=pI, door=d, cW=cW, cR=cR, unpark=unpark, rx=rx)
        rows.append(r)
    prev = (t, name)

def ts(x):
    if x is None:
        return None
    return x if isinstance(x, int) else x[0]

steps = [("qW", "qR"), ("qR", "qO"), ("qO", "hW"), ("hW", "hR"), ("hR", "hI"), ("hI", "hO"), ("hO", "pW"), ("pW", "pR"),
         ("pR", "pI"), ("pI", "door"), ("hI", "cW"), ("door", "cW"), ("cW", "cR"), ("cR", "unpark"), ("unpark", "rx"), ("qW", "rx")]
print(f"{len(rows)} parked requests traced")
for a, b in steps:
    v = [(ts(r[b]) - ts(r[a])) / 1000 for r in rows if ts(r[a]) is not None and ts(r[b]) is not None]
    if not v:
        print(f"{a:>6} -> {b:<6}  (none)"); continue
    v.sort()
    print(f"{a:>6} -> {b:<6} n={len(v):4d} mean {statistics.mean(v):7.1f} p50 {v[len(v)//2]:7.1f} p90 {v[int(len(v)*0.9)]:7.1f} us")
# Where each woken thread ran, and out of which idle state its CPU came.
for name in ("qR", "hR", "pR", "cR"):
    c = collections.Counter((r[name][2], r[name][3]) for r in rows if r[name])
    print(name, "cpu,cstate:", dict(c.most_common(8)))
