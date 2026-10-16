#!/usr/bin/env python3
"""Where an input event's time goes, from Windows to the client's socket
(waycompose-design.md §89).

    wc-input-latency.py INJ GUEST_TRACE HOST_TRACE SYNC [--label L]

INJ is probe-input-latency.exe's stdout (`inj <qpc-us> <kind> <tick> <x> <y>`),
GUEST_TRACE the compositor's WC_FRAME_TRACE file, HOST_TRACE wc-proxy's, both
written with WC_FRAME_TRACE_CLOCK=mono, and SYNC wc-clock-sync.py's pings.

Each injected event is followed through six stamps:

    post     probe-input-latency posts WM_MOUSEMOVE / WM_MOUSEWHEEL     guest
    wc.in    the compositor's window procedure has it                   guest
    wc.in_tx the Wayland thread hands it to smithay                     guest
    wc.flush the turn's flush: its bytes leave the compositor           guest
    px.in    wc-proxy parses it from the compositor                     host
    px.out   wc-proxy's write to the client's socket returns            host

and the gaps are reported as percentiles. The crossing (wc.flush -> px.in) and
the totals need the two clocks on one scale: the guest's counter minus the
host's is fitted, as a line in time, through the fastest ping of every
two-second window of SYNC. Each of those is wrong by at most half its round
trip, and the line's worst distance from them is printed with the result, so
the totals are good to that bound and no better.

PAIRING. A post is the first `wc.in` of its kind after it and before the next
post. `wc.in_tx` and, where the event has a `time` (motion, axis, button,
key), the proxy's `px.in` are found by that time, which the compositor sends
and the proxy logs back. A `wl_pointer.enter` has no time, so it is aligned in
order instead: the first proxy event of the kind no earlier than the
compositor's flush. On the host one event can be several -- a client that
bound `wl_pointer` twice (Firefox does) is sent each event twice -- and the
first copy is the one timed. An event lost on the way (a notch with no pointer
focus is not sent at all) is counted under its reason, never paired with a
neighbour.

KINDS. In session 0 the real cursor is never over the window, so the
compositor's TrackMouseEvent answers every posted move with WM_MOUSELEAVE and
every move becomes `wl_pointer.enter` rather than `motion`. Both go the same
way through the same code from `wc.in` on; they are one kind here, `move`.
"""

import bisect
import sys
from collections import defaultdict


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return float("nan")
    return xs[min(len(xs) - 1, int(round(p / 100.0 * (len(xs) - 1))))]


def fields(parts):
    out = {}
    for w in parts:
        if "=" in w:
            k, v = w.split("=", 1)
            out[k] = v
    return out


def read_trace(path, prefix):
    ev = []
    for line in open(path, errors="replace"):
        p = line.split()
        if len(p) < 2 or not p[1].startswith(prefix):
            continue
        try:
            us = int(p[0])
        except ValueError:
            continue
        ev.append((us, p[1], fields(p[2:])))
    ev.sort(key=lambda e: e[0])
    return ev


def fit_offset(sync_path):
    """guest - host, as a + b * (host_us - h0), through each window's fastest ping."""
    rows = []
    for line in open(sync_path):
        p = line.split()
        if len(p) == 4 and p[0] == "sync":
            rows.append((int(p[1]), int(p[2]), int(p[3])))
    if not rows:
        raise SystemExit("no sync rows in %s" % sync_path)
    rows.sort()
    h0 = rows[0][0]
    windows = defaultdict(list)
    for t0, g, t1 in rows:
        windows[(t0 - h0) // 2_000_000].append((t1 - t0, t0, g, t1))
    pts = []
    for k in sorted(windows):
        rtt, t0, g, t1 = min(windows[k])
        mid = (t0 + t1) / 2.0
        pts.append((mid - h0, g - mid, rtt))
    n = len(pts)
    if n == 1:
        a, b = pts[0][1], 0.0
    else:
        mx = sum(p[0] for p in pts) / n
        my = sum(p[1] for p in pts) / n
        sxx = sum((p[0] - mx) ** 2 for p in pts)
        b = sum((p[0] - mx) * (p[1] - my) for p in pts) / sxx if sxx else 0.0
        a = my - b * mx
    resid = [abs(p[1] - (a + b * p[0])) for p in pts]
    rtts = [p[2] for p in pts]
    info = {
        "pings": len(rows),
        "windows": n,
        "rtt_min_us": min(rtts),
        "rtt_med_us": pct(rtts, 50),
        "rtt_max_us": max(rtts),
        "drift_ppm": b * 1e6,
        "resid_max_us": max(resid),
    }
    return (lambda host_us: a + b * (host_us - h0)), info


def kind_of(raw):
    return {"motion": "move", "enter": "move", "wheel": "wheel", "axis": "wheel",
            "button": "button", "key": "key"}.get(raw)


def main():
    argv = sys.argv[1:]
    label = ""
    if "--label" in argv:
        i = argv.index("--label")
        label = argv[i + 1]
        del argv[i:i + 2]
    inj_path, gpath, hpath, spath = argv[:4]
    offset, sinfo = fit_offset(spath)
    to_guest = lambda hus: hus + offset(hus)

    inj = defaultdict(list)
    for line in open(inj_path, errors="replace"):
        p = line.split()
        if len(p) == 6 and p[0] == "inj":
            inj[kind_of(p[2])].append((int(p[1]), int(p[3])))
    g = read_trace(gpath, "wc.")
    h = read_trace(hpath, "px.")
    wc_in, wc_tx = defaultdict(list), defaultdict(list)
    flushes = [us for us, e, f in g if e == "wc.flush"]
    num = lambda f, key: float(f[key]) if key in f else None
    for us, e, f in g:
        k = kind_of(f.get("kind"))
        if k is None:
            continue
        rec = (us, int(f["t"]) if "t" in f else None, num(f, "x"), num(f, "y"))
        if e == "wc.in":
            wc_in[k].append(rec)
        elif e == "wc.in_tx":
            wc_tx[k].append(rec)
    px_in = defaultdict(list)
    outs = [us for us, e, f in h if e == "px.out"]
    for us, e, f in h:
        if e == "px.in":
            k = kind_of(f.get("kind"))
            if k is not None:
                px_in[k].append((us, int(f["t"]) if "t" in f else None, num(f, "x"), num(f, "y")))

    def after(xs, t):
        i = bisect.bisect_left(xs, t)
        return xs[i] if i < len(xs) else None

    gaps = defaultdict(list)
    notes = []
    total_inj = sum(len(v) for v in inj.values())
    paired = 0
    for k in sorted(inj):
        posts, ins, txs, pxs = inj[k], wc_in[k], wc_tx[k], px_in[k]
        lost = defaultdict(int)
        # wc.in_tx by its message time, which wc.in carries too.
        tx_by_t = defaultdict(list)
        for rec in txs:
            tx_by_t[rec[1]].append(rec[0])
        # px.in by time where the event has one (motion, axis, button, key).
        # An `enter` has none; it has a position, which is the window's point
        # moved by the surface's offset in it (the window geometry), constant
        # for a run and estimated as the commonest difference between the two.
        px_by_t = defaultdict(list)
        px_by_pos = defaultdict(list)
        untimed = [r for r in pxs if r[1] is None and r[2] is not None]
        votes = defaultdict(int)
        for ti, _t, x, y in ins[:80]:
            if x is None:
                continue
            for ph, _t2, px, py in untimed[:400]:
                if 0 <= to_guest(ph) - ti < 200_000:
                    votes[(round(px - x), round(py - y))] += 1
        dxy = max(votes, key=votes.get) if votes else (0, 0)
        for ph, t_px, x, y in pxs:
            if t_px is not None:
                px_by_t[t_px].append(ph)
            elif x is not None:
                px_by_pos[(round(x - dxy[0]), round(y - dxy[1]))].append(ph)
        wi = 0
        for j, (tp, _tick) in enumerate(posts):
            nxt = posts[j + 1][0] if j + 1 < len(posts) else float("inf")
            while wi < len(ins) and ins[wi][0] < tp:
                wi += 1
            if wi >= len(ins) or ins[wi][0] >= nxt:
                lost["never reached the window procedure"] += 1
                continue
            ti, t_in, x_in, y_in = ins[wi]
            wi += 1
            if not tx_by_t.get(t_in):
                lost["not handed to smithay"] += 1
                continue
            tt = tx_by_t[t_in].pop(0)
            fl = after(flushes, tt)
            if fl is None:
                lost["no flush"] += 1
                continue
            pos = None if x_in is None else (round(x_in), round(y_in))
            cands = [c for c in (px_by_t.get(t_in), px_by_pos.get(pos)) if c]
            # The earliest of what the two keys found that is not before the
            # flush; the client's second copy of the event goes with it.
            cands = sorted(ph for c in cands for ph in c if to_guest(ph) >= fl - 1000)
            if not cands:
                lost["never reached the proxy (no focus, or not sent)"] += 1
                continue
            ph = cands[0]
            for c in (px_by_t.get(t_in), px_by_pos.get(pos)):
                if c:
                    c[:] = [q for q in c if q - ph > 200_000]
            out = after(outs, ph)
            if out is None:
                lost["no px.out"] += 1
                continue
            paired += 1
            gaps["post->wc.in"].append(ti - tp)
            gaps["wc.in->wc.in_tx (turn)"].append(tt - ti)
            gaps["wc.in_tx->wc.flush"].append(fl - tt)
            gaps["wc.flush->px.in (crossing)"].append(to_guest(ph) - fl)
            gaps["px.in->px.out (proxy)"].append(out - ph)
            gaps["wc.in->px.out (ours)"].append(to_guest(out) - ti)
            gaps["post->px.out (total)"].append(to_guest(out) - tp)
            gaps["ours, %s only" % k].append(to_guest(out) - ti)
        notes.append("%s: %d posted%s" % (
            k, len(posts), "".join(", %d %s" % (n, why) for why, n in sorted(lost.items()))))

    print("== input latency %s" % label)
    print("clock: %(pings)d pings in %(windows)d two-second windows; the windows' fastest round trips "
          "%(rtt_min_us)d..%(rtt_max_us)d us (median %(rtt_med_us)d), drift %(drift_ppm).1f ppm, "
          "worst window off the fitted line %(resid_max_us).0f us" % sinfo)
    print("events: %d posted, %d followed end to end. %s" % (total_inj, paired, "; ".join(notes)))
    print("%-30s %6s %8s %8s %8s %8s %8s" % ("stage (ms)", "n", "p50", "p90", "p99", "max", "mean"))
    order = ["post->wc.in", "wc.in->wc.in_tx (turn)", "wc.in_tx->wc.flush", "wc.flush->px.in (crossing)",
             "px.in->px.out (proxy)", "wc.in->px.out (ours)", "post->px.out (total)"]
    order += sorted(k for k in gaps if k.startswith("ours, "))
    for k in order:
        xs = [v / 1000.0 for v in gaps.get(k, [])]
        if xs:
            print("%-30s %6d %8.2f %8.2f %8.2f %8.2f %8.2f" % (
                k, len(xs), pct(xs, 50), pct(xs, 90), pct(xs, 99), max(xs), sum(xs) / len(xs)))


main()
