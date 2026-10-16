#!/usr/bin/env python3
"""Frame rate and frame pacing of a `scripts/sv-minecraft.sh` run.

    sv-minecraft-fps.py TRACE_DIR MARKS

TRACE_DIR holds the `WC_FRAME_TRACE` files of the run (waycompose-design.md
§88): `icd.present` is written by our ICD inside the game, once per
`vkQueuePresentKHR` - Zink's for the stock game, VulkanMod's own for the modded
one - and `wc.paint`/`wc.present` by the compositor. MARKS is the harness's
`<unix seconds> <what>` file; only the frames between `steady begin` and
`steady end` count, which is a stretch where nobody touches the game.

Pacing is the distribution of the intervals between consecutive presents. A
frame that took more than twice the median interval is counted as a hitch,
which is what a person sees as a stutter. Standard library only.
"""

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


def events(trace_dir, names):
    out = {n: [] for n in names}
    for f in sorted(os.listdir(trace_dir)):
        try:
            lines = open(os.path.join(trace_dir, f), errors="replace").read().splitlines()
        except OSError:
            continue
        for line in lines:
            parts = line.split()
            if len(parts) >= 2 and parts[0].isdigit() and parts[1] in out:
                out[parts[1]].append(int(parts[0]))
    return {n: sorted(v) for n, v in out.items()}


def window(marks_path):
    begin = end = None
    for line in open(marks_path):
        t, _, what = line.strip().partition(" ")
        if what == "steady begin":
            begin = float(t) * 1e6
        elif what == "steady end":
            end = float(t) * 1e6
    return begin, end


def report(name, ts, begin, end):
    ts = [t for t in ts if begin <= t <= end]
    if len(ts) < 3:
        print("%-12s too few frames in the steady window (%d)" % (name, len(ts)))
        return
    gaps = [(b - a) / 1000.0 for a, b in zip(ts, ts[1:])]
    secs = (ts[-1] - ts[0]) / 1e6
    med = statistics.median(gaps)
    hitches = sum(1 for g in gaps if g > 2 * med)
    print(
        "%-12s %6.1f fps over %4.1f s  interval ms p50 %5.2f p95 %5.2f p99 %5.2f max %6.2f  "
        "hitches(>2x p50) %d" % (name, (len(ts) - 1) / secs, secs, med, pct(gaps, 95), pct(gaps, 99),
                                 max(gaps), hitches)
    )


def main():
    trace_dir, marks = sys.argv[1], sys.argv[2]
    begin, end = window(marks)
    if begin is None or end is None:
        print("no steady window in %s" % marks)
        return 1
    ev = events(trace_dir, ["icd.present", "wc.paint", "wc.present"])
    report("icd.present", ev["icd.present"], begin, end)
    report("wc.present", ev["wc.present"], begin, end)
    report("wc.paint", ev["wc.paint"], begin, end)
    return 0


if __name__ == "__main__":
    sys.exit(main())
