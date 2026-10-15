#!/usr/bin/env python3
"""One timeline of a frame, from the `WC_FRAME_TRACE` files of every process
it crossed (waycompose-design.md §88).

    wc-frame-timeline.py TRACE... [--harness FILE] [--cpu FILE] [--soak FILE]

TRACE is a file or a directory of files, each `<us> <event> k=v ...` lines
written by `sv_transport::frametrace`: `icd.*` by the ICD inside Firefox,
`px.*` by wc-proxy, `wc.*` by the compositor and `gs.*` by the GPU side. The
host's and the guest's files are on two wall clocks; their offset is estimated
from the commits and frame callbacks both sides saw, NTP-style, and every
cross-VM number below says it is an estimate.

`--harness` is `sv-firefox-soak.py`'s `harness.trace`: when it has
`scroll-begin`/`scroll-end` marks, only the events between them count.
`--cpu` is `scripts/wc-cpu-sample.sh`'s output. Standard library only.
"""

import collections
import json
import os
import statistics
import sys


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    k = (len(xs) - 1) * p / 100.0
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return xs[lo] + (xs[hi] - xs[lo]) * (k - lo)


def dist(xs, unit=1000.0):
    """p50/p95/p99/mean in ms of microsecond samples."""
    if not xs:
        return "      -"
    return "p50 %6.2f  p95 %6.2f  p99 %6.2f  mean %6.2f  n=%d" % (
        pct(xs, 50) / unit, pct(xs, 95) / unit, pct(xs, 99) / unit,
        statistics.fmean(xs) / unit, len(xs))


def parse(paths):
    events = []
    files = []
    for p in paths:
        if os.path.isdir(p):
            files += [os.path.join(p, f) for f in sorted(os.listdir(p))]
        else:
            files.append(p)
    for f in files:
        try:
            lines = open(f, errors="replace").read().splitlines()
        except OSError:
            continue
        for line in lines:
            parts = line.split()
            if len(parts) < 2 or not parts[0].isdigit():
                continue
            kv = {}
            for w in parts[2:]:
                if "=" in w:
                    k, v = w.split("=", 1)
                    kv[k] = v
                else:
                    kv.setdefault("_", []).append(w)
            events.append((int(parts[0]), parts[1], kv, os.path.basename(f)))
    events.sort(key=lambda e: e[0])
    return events


def num(kv, k, default=0):
    try:
        return int(kv.get(k, default), 0)
    except (TypeError, ValueError):
        return default


def windows_from(harness):
    out, begin = [], None
    if not harness or not os.path.exists(harness):
        return out
    for line in open(harness):
        parts = line.split()
        if len(parts) < 2:
            continue
        if parts[1] == "harness.scroll-begin":
            begin = int(parts[0])
        elif parts[1] == "harness.scroll-end" and begin is not None:
            out.append((begin, int(parts[0])))
            begin = None
    return out


def main(argv):
    paths, harness, cpu, soak = [], None, None, None
    it = iter(argv)
    for a in it:
        if a == "--harness":
            harness = next(it)
        elif a == "--cpu":
            cpu = next(it)
        elif a == "--soak":
            soak = next(it)
        elif a.endswith("harness.trace"):
            harness = a
        else:
            paths.append(a)
    ev = parse(paths)
    if not ev:
        print("no trace events")
        return 1

    guest_files = {f for _, e, _, f in ev if e.startswith("wc.") or e.startswith("gs.")}
    host = [e for e in ev if e[3] not in guest_files]
    guest = [e for e in ev if e[3] in guest_files]

    # ---------------------------------------------------- the clock offset --
    # Host -> guest: the k-th commit on surface S at the proxy and at the
    # compositor. Guest -> host: the k-th frame callback answered for surface S
    # and the k-th `done` the proxy passed on for a callback requested on S.
    def by_surf(evs, name, key="surf"):
        d = collections.defaultdict(list)
        for t, e, kv, _ in evs:
            if e == name:
                d[kv.get(key)].append(t)
        return d

    def shifts(hs, gs, sign):
        """Every pairing of two sequences of the same events on two clocks, by
        index shift. Both processes trace from their first message, so the k-th
        event on one side is the k-th on the other up to a small shift (a
        process that is killed loses its last unflushed quarter second, and a
        late start loses a head). Returns {k: sorted differences}."""
        out = {}
        for k in range(-4, 5):
            d = sorted(sign * (gs[i + k] - hs[i]) for i in range(len(hs)) if 0 <= i + k < len(gs))
            if len(d) >= 8:
                out[k] = d
        return out

    def busiest(d):
        return max(d, key=lambda s: len(d[s])) if d else None

    px_c = by_surf(host, "px.c")
    wc_c = by_surf(guest, "wc.c")
    s1 = busiest(px_c)
    fwd = shifts(px_c[s1], wc_c.get(s1, []), 1) if s1 is not None else {}
    cb_surf = {}
    px_done = collections.defaultdict(list)
    # The compositor's answer arriving at the proxy. Older traces have only
    # `px.done`, which was the same thing before the proxy answered any itself.
    gdone = "px.gdone" if any(e == "px.gdone" for _, e, _, _ in host) else "px.done"
    for t, e, kv, _ in host:
        if e == "px.frame":
            cb_surf[kv.get("cb")] = kv.get("surf")
        elif e == gdone:
            s = cb_surf.pop(kv.get("cb"), None)
            if s is not None:
                px_done[s].append(t)
    wc_done = collections.defaultdict(list)
    for t, e, kv, _ in guest:
        if e in ("wc.done", "wc.done_now", "wc.done_sub"):
            wc_done[kv.get("surf")] += [t] * max(1, num(kv, "n", 1))
    s2 = busiest(px_done)
    rev = shifts(px_done[s2], wc_done.get(s2, []), -1) if s2 is not None else {}
    # The pair of shifts whose round trip is smallest without being negative:
    # a wrong shift on one side makes the sum either impossible (below zero,
    # an event seen before it happened) or a whole frame too long.
    offset, d1, d2 = None, [], []
    best = None
    for k1, a in fwd.items():
        for k2, b in rev.items():
            rt = pct(a, 2) + pct(b, 2)
            if rt >= -200 and (best is None or (rt, abs(k1) + abs(k2)) < best[0]):
                best = ((rt, abs(k1) + abs(k2)), k1, k2)
    if best is not None:
        d1, d2 = fwd[best[1]], rev[best[2]]
        a, b = pct(d1, 2), pct(d2, 2)
        offset = (a - b) / 2.0  # guest clock minus host clock
        one_way = (a + b) / 2.0
    elif guest:
        print("! no commit or callback seen by both sides; cross-VM numbers omitted")

    # ------------------------------------------------------- the window --
    wins = windows_from(harness)

    def inside(t, guest_clock=False):
        if not wins:
            return True
        if guest_clock:
            if offset is None:
                return True
            t = t - offset
        return any(a <= t <= b for a, b in wins)

    H = [e for e in host if inside(e[0])]
    G = [e for e in guest if inside(e[0], True)]
    span = sum(b - a for a, b in wins) if wins else (ev[-1][0] - ev[0][0])
    print("=== frame timeline (waycompose-design.md §88) ===")
    print("window: %s, %.1f s" % ("scroll marks x%d" % len(wins) if wins else "whole trace", span / 1e6))
    if soak and os.path.exists(soak):
        for line in open(soak):
            if '"scroll-fps"' in line:
                print("harness:", line.strip())

    # ------------------------------------------------------- frame rate --
    pres = [(t, kv) for t, e, kv, _ in H if e == "icd.present"]
    by_sc = collections.defaultdict(list)
    for t, kv in pres:
        by_sc[kv.get("sc")].append((t, kv))
    main_sc = max(by_sc, key=lambda s: len(by_sc[s])) if by_sc else None
    P = by_sc.get(main_sc, [])
    intervals = []
    for (a, _), (b, _) in zip(P, P[1:]):
        if not wins or any(x <= a and b <= y for x, y in wins):
            intervals.append(b - a)
    print("\n--- presents (swapchain %s, %s) ---" % (main_sc, P[0][1].get("_", [""])[0] if P and P[0][1].get("_") else ""))
    if intervals:
        busy = sum(intervals)
        print("frames %d, %.1f fps over the frames' own span" % (len(intervals), len(intervals) / (busy / 1e6)))
        print("present-to-present   ", dist(intervals))
    for k, label in (("gap_us", "app (between presents)"), ("rt_us", "  of which round trips"),
                     ("rt", "  round trips (count)"), ("reset_us", "present: reset fence"),
                     ("submit_us", "present: submit copy"), ("queued_us", "present: queued (pipelined)"), ("wait_us", "present: wait fence"),
                     ("show_us", "present: wl_shm put")):
        xs = [num(kv, k) for _, kv in P]
        if k == "rt":
            print("%-22s p50 %6d  p95 %6d  max %6d" % (label, pct(xs, 50), pct(xs, 95), max(xs or [0])))
        else:
            print("%-22s %s" % (label, dist(xs)))
    tops = collections.Counter()
    tcount = collections.Counter()
    for _, kv in P:
        for item in kv.get("top", "").split(","):
            bits = item.split(":")
            if len(bits) == 3:
                tops[bits[0]] += int(bits[2])
                tcount[bits[0]] += int(bits[1])
    if tops:
        n = max(1, len(P))
        print("round trips by opcode (per frame):", ", ".join(
            "%s %.2f ms x%.1f" % (op, us / n / 1000, tcount[op] / n) for op, us in tops.most_common(6)))
    puts = [kv for t, e, kv, _ in H if e == "icd.put"]
    if puts:
        print("wl_shm put: waited for a buffer in %d of %d" % (sum(num(kv, "waited") for kv in puts), len(puts)))
        for k in ("pick_us", "copy_us", "commit_us"):
            print("  %-20s %s" % (k, dist([num(kv, k) for kv in puts])))

    # ------------------------------------------------------------ proxy --
    pc = [kv for t, e, kv, _ in H if e == "px.commit" and kv.get("attached") == "1"]
    print("\n--- wc-proxy ---")
    print("buffer commits %d; copy %s" % (len(pc), dist([num(kv, "copy_us") for kv in pc])))
    put_ends = sorted(t + num(kv, "pick_us") + num(kv, "copy_us") + num(kv, "commit_us")
                      for t, e, kv, _ in H if e == "icd.put")
    pcs = sorted(t for t, e, kv, _ in H if e == "px.commit" and kv.get("attached") == "1")
    lat, j = [], 0
    for te in put_ends:
        while j < len(pcs) and pcs[j] < te:
            j += 1
        if j < len(pcs) and pcs[j] - te < 100000:
            lat.append(pcs[j] - te)
    print("ICD commit -> proxy copy start", dist(lat))

    # ---------------------------------------------------- frame callbacks --
    print("\n--- frame callbacks (host side: wl_surface.frame -> wl_callback.done) ---")
    req = {}
    lat_by = collections.defaultdict(list)
    count_by = collections.Counter()
    for t, e, kv, _ in H:
        if e == "px.frame":
            req[kv.get("cb")] = (t, kv.get("surf"))
            count_by[kv.get("surf")] += 1
        elif e == "px.done" and kv.get("cb") in req:
            t0, s = req.pop(kv.get("cb"))
            lat_by[s].append(t - t0)
    buf_surfs = collections.Counter(kv.get("surf") for t, e, kv, _ in H if e == "px.commit" and kv.get("attached") == "1")
    for s in sorted(count_by, key=lambda s: -count_by[s]):
        print("surface %-6s frame requests %5d (%5.1f/s), buffer commits %5d; request->done %s" % (
            s, count_by[s], count_by[s] / (span / 1e6), buf_surfs.get(s, 0), dist(lat_by[s])))
    dones = collections.Counter(e for t, e, kv, _ in G if e in ("wc.done", "wc.done_now", "wc.done_sub"))
    if dones:
        print("guest answered: %s" % dict(dones))
    srcs = collections.Counter(kv.get("src", "guest") for t, e, kv, _ in H if e == "px.done")
    print("the client was answered by: %s" % dict(srcs))

    # ------------------------------------------------------------- guest --
    if G:
        print("\n--- compositor (guest) ---")
        if offset is not None:
            print("clock offset guest-host %.1f ms (estimated); one-way floor %.2f ms" % (offset / 1000, one_way / 1000))
            hg = [x - offset for x in d1]
            gh = [x + offset for x in d2]
            print("host commit -> guest commit handled", dist(hg))
            print("guest done -> host proxy sees it   ", dist(gh))
        turns = [kv for t, e, kv, _ in G if e == "wc.turn"]
        print("turns with commits %d: dispatch %s" % (len(turns), dist([num(kv, "dispatch_us") for kv in turns])))
        print("                     flush    %s" % dist([num(kv, "flush_us") for kv in turns]))
        wp = [kv for t, e, kv, _ in G if e == "wc.present"]
        print("present: read %s" % dist([num(kv, "read_us") for kv in wp]))
        print("         compose %s" % dist([num(kv, "compose_us") for kv in wp]))
        paints = [kv for t, e, kv, _ in G if e == "wc.paint"]
        print("paint: waited for WM_PAINT %s" % dist([num(kv, "wait_us") for kv in paints]))
        print("       SetDIBitsToDevice   %s" % dist([num(kv, "blit_us") for kv in paints]))
        pt = [t for t, e, kv, _ in G if e == "wc.paint"]
        pi = [b - a for a, b in zip(pt, pt[1:]) if b - a < 200000]
        print("       paint-to-paint      %s" % dist(pi))
        ops = [kv for t, e, kv, _ in G if e == "gs.ops"]
        if ops:
            busy = collections.defaultdict(list)
            agg = collections.Counter()
            for kv in ops:
                busy[kv.get("ring")].append(num(kv, "busy_us") / max(1, num(kv, "window_us")))
                for item in kv.get("top", "").split(","):
                    bits = item.split(":")
                    if len(bits) == 3:
                        agg[bits[0]] += int(bits[2])
            print("GPU side busy: " + ", ".join("ring %s %.0f%%" % (r, 100 * statistics.fmean(v)) for r, v in sorted(busy.items())))
            secs = max(1, len(ops))
            print("GPU side top: " + ", ".join("%s %.1f ms/s" % (op, us / 1000 / (span / 1e6)) for op, us in agg.most_common(6)))

    # -------------------------------------------------- one typical frame --
    def med(xs):
        return pct(xs, 50) / 1000 if xs else float("nan")
    print("\n--- one frame, medians, in order ---")
    rows = [
        ("Firefox: between presents (WebRender, Zink, forwarded calls)", med([num(kv, "gap_us") for _, kv in P])),
        ("  of it, blocked on GPU-side round trips", med([num(kv, "rt_us") for _, kv in P])),
        ("present: reset + submit", med([num(kv, "reset_us") + num(kv, "submit_us") for _, kv in P])),
        ("present: queued for the presenter thread (pipelined)", med([num(kv, "queued_us") for _, kv in P])),
        ("present: fence (GPU render tail + copy to arena + crossing)", med([num(kv, "wait_us") for _, kv in P])),
        ("present: copy arena -> wl_shm, attach, commit", med([num(kv, "show_us") for _, kv in P])),
        ("proxy: picks up the commit", med(lat)),
        ("proxy: copy wl_shm -> compositor arena", med([num(kv, "copy_us") for kv in pc])),
    ]
    if G and offset is not None:
        rows += [("crossing + wait for the compositor's turn", med([x - offset for x in d1]))]
    if G:
        rows += [
            ("compositor: read arena -> staging", med([num(kv, "read_us") for kv in wp])),
            ("compositor: compose the window", med([num(kv, "compose_us") for kv in wp])),
            ("UI thread: wait for WM_PAINT", med([num(kv, "wait_us") for kv in paints])),
            ("UI thread: SetDIBitsToDevice", med([num(kv, "blit_us") for kv in paints])),
        ]
    for label, v in rows:
        print("  %-62s %7.2f ms" % (label, v))

    if cpu and os.path.exists(cpu):
        print("\n--- host CPU ---")
        print(open(cpu).read().rstrip())
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
