#!/usr/bin/env python3
"""§105.14: the per-hop timeline of each control request, both machines.

RUNDIR holds guest/*.txt (WC_HOP_TRACE of the launcher, L-<pid>.txt, and
the compositor) and host/proxy-*.txt (the proxy's WC_HOP_TRACE).

    wc-hop-timeline.py RUNDIR [GAP_FILTER_MS]

Guest stamps (QueryPerformanceCounter ns, every guest process) and the
proxy's (CLOCK_MONOTONIC ns). The clock offset comes from the back-to-back
runs, where both sides spin: per request, guest C.ringtx1 -> host P.rx ->
host P.tx -> guest C.reply; the offset of the fastest tenth is the median of
((g1 + g2) - (h1 + h2)) / 2, refitted as a line over time when there are back-to-back
runs at both ends.
"""
import bisect, glob, os, statistics, sys

run = sys.argv[1]

def load(pattern):
    out = []
    for f in glob.glob(pattern):
        tag = os.path.basename(f).split("-")[0]
        for l in open(f, errors="replace"):
            a = l.split()
            if len(a) == 2 and a[0].isdigit():
                out.append((int(a[0]), a[1], tag + ":" + os.path.basename(f)))
    out.sort()
    return out

g = load(os.path.join(run, "guest", "*.txt"))
h = load(os.path.join(run, "host", "proxy-*.txt"))
gt = [e[0] for e in g]
ht = [e[0] for e in h]

# Requests: L.send .. L.recv in each launcher file, in time order.
reqs = []
for f in sorted(glob.glob(os.path.join(run, "guest", "L-*.txt"))):
    ev = [l.split() for l in open(f)]
    ev = [(int(a[0]), a[1]) for a in ev if len(a) == 2]
    pend = None
    for t, n in ev:
        if n == "L.send":
            pend = t
        elif n == "L.recv" and pend is not None:
            reqs.append((pend, t, os.path.basename(f)))
            pend = None
reqs.sort()
by_req_file = {s: f for s, r, f in reqs}

def between(lst, times, a, b, name=None):
    i = bisect.bisect_left(times, a)
    out = []
    while i < len(lst) and lst[i][0] <= b:
        if name is None or lst[i][1] == name:
            out.append(lst[i])
        i += 1
    return out

def first(evs, name):
    return next((e[0] for e in evs if e[1] == name), None)

# Clock offset (guest - host) from requests where nobody parked.
samples = []
for s, r, f in reqs:
    if r - s > 60_000:
        continue
    ge = between(g, gt, s, r)
    g1, g2 = first(ge, "C.ringtx1"), first(ge, "C.reply")
    if g1 is None or g2 is None or any(e[1] == "C.park" for e in ge):
        continue
    samples.append((s, g1, g2))
off_pts = []
for s, g1, g2 in samples:
    # the host's P.rx/P.tx near g1 with any offset within +-1 s: search by a coarse offset first
    off_pts.append((s, g1, g2))

def coarse_points(sub):
    """Fine offset points for one run's samples, after a coarse scan."""
    s0, g10, g20 = sub[len(sub) // 2]
    best = None
    for t, n, _ in h:
        if n != "P.rx":
            continue
        cand = g10 - t
        sc = 0
        for _, g1, g2 in sub[:: max(1, len(sub) // 40)]:
            j = bisect.bisect_left(ht, g1 - cand - 5_000)
            if j < len(h) and h[j][0] <= g2 - cand:
                sc += 1
        if best is None or sc > best[0]:
            best = (sc, cand)
    cand = best[1]
    pts = []
    for s, g1, g2 in sub:
        hs = between(h, ht, g1 - cand - 20_000, g2 - cand + 20_000)
        h1, h2 = first(hs, "P.rx"), first(hs, "P.tx")
        if h1 is None or h2 is None:
            continue
        pts.append((s, (g2 - g1) - (h2 - h1), ((g1 + g2) - (h1 + h2)) / 2))
    return pts

def fit_offset():
    if not samples:
        return None
    pts = []
    for f in sorted(set(by_req_file.values())):
        sub = [x for x in samples if by_req_file.get(x[0]) == f]
        if len(sub) >= 20:
            pts += coarse_points(sub)
    # One point per back-to-back run (its fastest tenth), then a line
    # through the points: the guest's QPC and the host's CLOCK_MONOTONIC
    # drift apart by tens of ppm.
    runs = []
    for f in sorted(set(fname for fname in by_req_file.values())):
        sel = [p for p in pts if by_req_file.get(p[0]) == f]
        if len(sel) < 20:
            continue
        sel.sort(key=lambda p: p[1])
        keep = sel[: max(5, len(sel) // 10)]
        runs.append((statistics.mean(p[0] for p in keep), statistics.median(p[2] for p in keep), max(p[1] for p in keep)))
    runs.sort()
    if not runs:
        return None
    if len(runs) == 1:
        o = runs[0][1]
        return (lambda t: o), f"offset {o/1e3:.1f} us (one run), spread {runs[0][2]/1e3:.1f} us"
    (ta, oa, _), (tb, ob, _) = runs[0], runs[-1]
    slope = (ob - oa) / (tb - ta)
    return (lambda t: oa + slope * (t - ta)), f"offset {oa/1e3:.1f} us, drift {slope*1e6:.2f} ppm over {len(runs)} runs, spread {max(r[2] for r in runs)/1e3:.1f} us"

fit = fit_offset()
off, how = fit if fit else ((lambda t: 0.0), "no offset")
print("clock:", how)

order = ["L.send", "C.line", "C.ringtx0", "bell.poke", "bell.wr0", "bell.issued", "bell.wr1", "C.ringtx1",
         "H:door.rx", "H:P.unpark", "H:P.rx", "H:P.tx", "C.park", "C.unpark", "door.rx", "C.reply", "C.socktx", "L.block", "L.recv"]
gap_filter = sys.argv[2] if len(sys.argv) > 2 else None
by_file = {}
for s, r, f in reqs:
    by_file.setdefault(f, []).append((s, r))
for f, lst in by_file.items():
    rtts = sorted((r - s) / 1e3 for s, r in lst)
    print(f"\n== {f}: n={len(lst)} mean {statistics.mean(rtts):.1f} p50 {rtts[len(rtts)//2]:.1f} p90 {rtts[int(len(rtts)*0.9)]:.1f} us")
    rows = []
    for s, r in lst:
        ge = between(g, gt, s - 1000, r + 1000)
        o = off(s)
        he = between(h, ht, s - o - 1000, r - o + 1000)
        row = {}
        for t, n, _ in ge:
            row.setdefault(n, t)
        for t, n, _ in he:
            row.setdefault("H:" + n, t + o)
        rows.append(row)
    # mean time of each stamp after L.send, and the share that saw it
    print(f"  {'hop':<12} {'seen':>5} {'mean':>8} {'p50':>8} {'p90':>8}   (us after L.send)")
    for n in order:
        v = sorted((row[n] - row["L.send"]) / 1e3 for row in rows if n in row and "L.send" in row)
        if v:
            print(f"  {n:<12} {len(v):5d} {statistics.mean(v):8.1f} {v[len(v)//2]:8.1f} {v[int(len(v)*0.9)]:8.1f}")
