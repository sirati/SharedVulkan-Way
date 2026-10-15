#!/usr/bin/env python3
"""The host's half of scripts/probe-tunnel-latency.c (waycompose-design.md §88).

    wc-tunnel-latency.py SOCKET SECS [POKE_MS]

Connects to the host end of the tunnel and, for SECS seconds, records each
unprompted 'U' message's arrival minus its guest send time. The two clocks
differ by a constant, so only the spread means anything: the minimum is the
fastest crossing plus the offset, and every other sample's distance above it
is how long that message waited somewhere. With POKE_MS, sends an 8-byte
ping every POKE_MS ms the whole time (host-to-guest traffic, as a rendering
client makes) and reports the prompted round trip too.
"""

import socket
import struct
import sys
import threading
import time

SOCK, SECS = sys.argv[1], float(sys.argv[2])
POKE = float(sys.argv[3]) if len(sys.argv) > 3 else 0.0

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(SOCK)
lock = threading.Lock()
sent = {}
stop = False


def now_us():
    return time.monotonic_ns() // 1000


def poker():
    seq = 0
    while not stop:
        with lock:
            sent[seq] = now_us()
        s.sendall(struct.pack("<Q", seq))
        seq += 1
        time.sleep(POKE / 1000.0)


if POKE > 0:
    threading.Thread(target=poker, daemon=True).start()

u, rtt = [], []
buf = b""
end = time.monotonic() + SECS
s.settimeout(1.0)
while time.monotonic() < end:
    try:
        chunk = s.recv(4096)
    except socket.timeout:
        continue
    if not chunk:
        break
    t = now_us()
    buf += chunk
    while len(buf) >= 24:
        tag, a, b = buf[0:1], struct.unpack("<Q", buf[8:16])[0], struct.unpack("<Q", buf[16:24])[0]
        buf = buf[24:]
        if tag == b"U":
            u.append(t - b)
        elif tag == b"E":
            with lock:
                t0 = sent.pop(a, None)
            if t0 is not None:
                rtt.append(t - t0)
stop = True


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(p / 100.0 * (len(xs) - 1)))] if xs else float("nan")


if u:
    base = min(u)
    extra = [(x - base) / 1000.0 for x in u]
    print("unprompted, wait above the fastest (ms): n=%d p50 %.2f p90 %.2f p99 %.2f max %.2f" % (
        len(extra), pct(extra, 50), pct(extra, 90), pct(extra, 99), max(extra)))
if rtt:
    r = [x / 1000.0 for x in rtt]
    print("prompted round trip (ms): n=%d p50 %.2f p90 %.2f p99 %.2f" % (len(r), pct(r, 50), pct(r, 90), pct(r, 99)))
