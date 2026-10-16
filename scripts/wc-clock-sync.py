#!/usr/bin/env python3
"""The offset between the guest's and the host's monotonic clocks, measured
(waycompose-design.md §89).

    wc-clock-sync.py SOCKET SECS OUT [PING_MS]

The host's end of `probe-tunnel-latency.exe`, like wc-tunnel-latency.py, but
it keeps every ping: each line of OUT is

    sync <host-send-us> <guest-echo-us> <host-recv-us>

with the host's times from CLOCK_MONOTONIC and the guest's from
QueryPerformanceCounter, both in microseconds -- the two clocks
WC_FRAME_TRACE_CLOCK=mono stamps traces with. For a ping whose round trip is
r, the guest's clock minus the host's is (guest - (send + recv) / 2), wrong by
at most r / 2; scripts/wc-input-latency.py keeps the fastest pings of each
window and fits a line through them, so a slow drift between the two counters
is followed rather than averaged away.

Run it over a tunnel whose guest-to-host direction does not hold small
messages (MSYS2's ssh measured p99 1 ms where Windows' ssh.exe held 38 ms), or
the bound is the hold, not the link.
"""

import socket
import struct
import sys
import threading
import time

SOCK, SECS, OUT = sys.argv[1], float(sys.argv[2]), sys.argv[3]
PING = float(sys.argv[4]) if len(sys.argv) > 4 else 5.0

s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect(SOCK)
lock = threading.Lock()
sent = {}
stop = False


def now_us():
    return time.monotonic_ns() // 1000


def pinger():
    seq = 0
    while not stop:
        with lock:
            sent[seq] = now_us()
        s.sendall(struct.pack("<Q", seq))
        seq += 1
        time.sleep(PING / 1000.0)


threading.Thread(target=pinger, daemon=True).start()
out = open(OUT, "w")
buf = b""
n = 0
end = time.monotonic() + SECS
s.settimeout(0.5)
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
        if tag == b"E":
            with lock:
                t0 = sent.pop(a, None)
            if t0 is not None:
                out.write("sync %d %d %d\n" % (t0, b, t))
                n += 1
stop = True
out.close()
print("wc-clock-sync: %d pings kept in %s" % (n, OUT))
