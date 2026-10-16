#!/usr/bin/env python3
"""Drive a Firefox through Marionette for a fixed time and record what it did.

    sv-firefox-soak.py PORT SECS OUTDIR

Called by `scripts/sv-firefox-wayland.sh soak`; see that file for the topology.
Standard library only, so `nix shell nixpkgs#python3` is enough to run it.

What it does, in a loop until SECS have passed: load each page of a fixed tour
(local pages that need no network, then a few real sites), scroll each one with
wheel input the way a user would, and on the WebGL page check that frames are
being drawn and read back. Between pages it asks the browser chrome which
compositor it is using, which is the value about:support's "Compositing" row
is made from. At the end it reads about:support itself.

Every step is a JSON line in OUTDIR/soak.jsonl, so the run can be judged on
what happened rather than on the script returning. The verdict is the
harness's, not this file's: this file only records.
"""

import json
import os
import socket
import sys
import time

PORT, SECS, OUT = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
LOG = open(os.path.join(OUT, "soak.jsonl"), "a", buffering=1)
T0 = time.monotonic()


def log(kind, **kw):
    kw.update(kind=kind, t=round(time.monotonic() - T0, 1))
    LOG.write(json.dumps(kw) + "\n")
    print(json.dumps(kw), flush=True)


class Marionette:
    """The Marionette wire protocol: `<len>:<json>` frames, request
    `[0, id, name, params]`, response `[1, id, error, result]`."""

    def __init__(self, port):
        deadline = time.monotonic() + 60
        while True:
            try:
                self.s = socket.create_connection(("127.0.0.1", port), timeout=120)
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(0.5)
        self.buf = b""
        self.id = 0
        self.hello = self._read()

    def _read(self):
        while b":" not in self.buf:
            chunk = self.s.recv(65536)
            if not chunk:
                raise ConnectionError("marionette closed the connection")
            self.buf += chunk
        n, rest = self.buf.split(b":", 1)
        n = int(n)
        while len(rest) < n:
            chunk = self.s.recv(65536)
            if not chunk:
                raise ConnectionError("marionette closed the connection")
            rest += chunk
        self.buf = rest[n:]
        return json.loads(rest[:n])

    def call(self, name, params=None):
        self.id += 1
        body = json.dumps([0, self.id, name, params or {}]).encode()
        self.s.sendall(str(len(body)).encode() + b":" + body)
        while True:
            msg = self._read()
            if msg[0] == 1 and msg[1] == self.id:
                if msg[2]:
                    raise RuntimeError(f"{name}: {msg[2]}")
                return msg[3]

    def script(self, js, args=None):
        r = self.call("WebDriver:ExecuteScript", {"script": js, "args": args or []})
        return r.get("value") if isinstance(r, dict) else r

    def context(self, which):
        self.call("Marionette:SetContext", {"value": which})


def page_uri(name):
    return "file://" + os.path.join(OUT, "pages", name)


TOUR = [
    page_uri("long.html"),
    page_uri("webgl.html"),
    "https://en.wikipedia.org/wiki/Firefox",
    page_uri("canvas.html"),
    "https://developer.mozilla.org/en-US/docs/Web/API/WebGL_API",
    "https://example.com/",
]


def compositor(m):
    """The value about:support's Compositing row is built from."""
    m.context("chrome")
    try:
        return m.script("return window.windowUtils.layerManagerType;")
    finally:
        m.context("content")


def wheel(m, dy, steps):
    """Wheel input through the WebDriver actions API: real input events, so
    APZ and the compositor see a user's scroll rather than a script's."""
    actions = [{"type": "scroll", "x": 200, "y": 200, "deltaX": 0, "deltaY": dy, "duration": 120}
               for _ in range(steps)]
    m.call("WebDriver:PerformActions",
           {"actions": [{"type": "wheel", "id": "wheel0", "actions": actions}]})


def webgl_check(m):
    return m.script("""
        const i = window.__sv || {};
        return {renderer: i.renderer, version: i.version, frames: i.frames,
                pixel: i.pixel, error: i.error};
    """)


def mark(what):
    """A line in OUTDIR/harness.trace on the clock `WC_FRAME_TRACE` uses
    (microseconds of the wall clock), so the frame timeline can be cut to the
    seconds a wheel was turning (waycompose-design.md §88)."""
    with open(os.path.join(OUT, "harness.trace"), "a") as f:
        f.write(f"{time.time_ns() // 1000} harness.{what}\n")


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    return round(xs[min(len(xs) - 1, int(round(p / 100.0 * (len(xs) - 1))))], 2)


def scroll_fps(m, label, steps=40):
    """The §87 measurement: the long page, wheel input for about five seconds,
    and every requestAnimationFrame's timestamp, so the result is a frame-time
    distribution and not only a mean."""
    m.call("WebDriver:Navigate", {"url": TOUR[0]})
    time.sleep(2)
    m.script("""window.__fps = {ts: [], t0: performance.now()};
                (function f(t){ window.__fps.ts.push(t); requestAnimationFrame(f); })(performance.now());""")
    mark(f"scroll-begin {label}")
    wheel(m, 120, steps)
    mark(f"scroll-end {label}")
    r = m.script("""const d = performance.now() - window.__fps.t0;
                    return {ts: window.__fps.ts, ms: d, y: window.scrollY};""")
    ts = r["ts"]
    dts = [b - a for a, b in zip(ts, ts[1:])]
    log("scroll-fps", label=label, fps=round(len(ts) * 1000.0 / r["ms"], 1), frames=len(ts),
        ms=round(r["ms"], 1), y=r["y"], p50=pct(dts, 50), p95=pct(dts, 95), p99=pct(dts, 99),
        max=round(max(dts), 2) if dts else None)


def scroll_only():
    """`SOAK_MODE=scroll`: no tour, only the scroll measurement, ROUNDS times.
    A guest run is a budgeted thing; this is the shortest one that answers the
    frame-rate question."""
    m = Marionette(PORT)
    log("hello", hello=m.hello)
    m.call("WebDriver:NewSession", {"capabilities": {"alwaysMatch": {"pageLoadStrategy": "normal"}}})
    m.call("WebDriver:SetTimeouts", {"pageLoad": 60000, "script": 30000})
    try:
        log("window", rect=m.call("WebDriver:SetWindowRect", {"width": 1280, "height": 900}))
    except RuntimeError as e:
        log("window", error=str(e)[:200])
    log("compositor", value=compositor(m))
    for i in range(int(os.environ.get("SOAK_ROUNDS", "3"))):
        try:
            scroll_fps(m, f"r{i + 1}")
        except (RuntimeError, socket.timeout, KeyError, TypeError) as e:
            log("scroll-fps", label=f"r{i + 1}", error=str(e)[:200])
    log("done")


def two_windows():
    """`SOAK_MODE=two-windows`: the session that ran out of buffers on the
    guest (waycompose-design.md §95). Two windows at 1280x752, one page in
    the first retitling itself ten times a second, both grown to 1296x768
    (what a maximise gave there), and then the tour, alternating windows,
    until SECS have passed."""
    m = Marionette(PORT)
    log("hello", hello=m.hello)
    m.call("WebDriver:NewSession", {"capabilities": {"alwaysMatch": {"pageLoadStrategy": "normal"}}})
    m.call("WebDriver:SetTimeouts", {"pageLoad": 60000, "script": 30000})

    def size(w, h):
        try:
            log("window", rect=m.call("WebDriver:SetWindowRect", {"width": w, "height": h}))
        except RuntimeError as e:
            log("window", error=str(e)[:200])

    first = m.call("WebDriver:GetWindowHandle")
    first = first.get("value", first) if isinstance(first, dict) else first
    size(1280, 752)
    # `SOAK_VIDEO=1`: the first window plays a looping clip for the whole run
    # (§98: two windows plus a video), and the tour stays in the second.
    video = os.environ.get("SOAK_VIDEO") == "1"
    m.call("WebDriver:Navigate", {"url": page_uri("video.html" if video else "retitle.html")})
    second = m.call("WebDriver:NewWindow", {"type": "window"})["handle"]
    m.call("WebDriver:SwitchToWindow", {"handle": second})
    size(1280, 752)
    m.call("WebDriver:Navigate", {"url": TOUR[0]})
    time.sleep(5)
    for h in (first, second):
        m.call("WebDriver:SwitchToWindow", {"handle": h})
        size(1296, 768)
    log("session", windows=[first, second])
    rounds = 0
    while time.monotonic() - T0 < SECS:
        rounds += 1
        for i, url in enumerate(TOUR + [page_uri("retitle.html")]):
            if time.monotonic() - T0 >= SECS:
                break
            h = second if video else (first, second)[i % 2]
            try:
                m.call("WebDriver:SwitchToWindow", {"handle": h})
                t = time.monotonic()
                m.call("WebDriver:Navigate", {"url": url})
                log("load", round=rounds, window=h, url=url, secs=round(time.monotonic() - t, 2))
                wheel(m, 240, 12)
                wheel(m, -240, 6)
                time.sleep(2)
                log("compositor", value=compositor(m))
            except (RuntimeError, socket.timeout) as e:
                log("step-error", url=url, error=str(e)[:300])
        if video:
            try:
                m.call("WebDriver:SwitchToWindow", {"handle": first})
                t = m.call("WebDriver:GetTitle")
                log("video", title=t.get("value", t) if isinstance(t, dict) else t)
            except (RuntimeError, socket.timeout) as e:
                log("step-error", url="video.html", error=str(e)[:300])
    log("done", rounds=rounds)
    # Quit as a person does: close one window, then quit from the last one
    # (`Marionette:Quit`; closing the last window over WebDriver leaves this
    # browser running with no window at all). A session on the guest crashed
    # in xpcom-shutdown-threads at its last window (§95). The harness waits
    # for the exit and looks for a minidump.
    try:
        m.call("WebDriver:SwitchToWindow", {"handle": second})
        m.call("WebDriver:CloseWindow")
        log("closed", window=second)
        m.call("WebDriver:SwitchToWindow", {"handle": first})
        m.call("Marionette:Quit", {"flags": ["eAttemptQuit"]})
        log("closed", window=first, quit=True)
    except (RuntimeError, socket.timeout, ConnectionError, OSError) as e:
        log("closed", error=str(e)[:200])


def main():
    if os.environ.get("SOAK_MODE") == "scroll":
        return scroll_only()
    if os.environ.get("SOAK_MODE") == "two-windows":
        return two_windows()
    m = Marionette(PORT)
    log("hello", hello=m.hello)
    m.call("WebDriver:NewSession", {"capabilities": {"alwaysMatch": {"pageLoadStrategy": "normal"}}})
    m.call("WebDriver:SetTimeouts", {"pageLoad": 60000, "script": 30000})
    # A fresh profile's first window can come up small enough that the wheel
    # at (200, 200) is outside it (waycompose-design.md §87 measured a 500x52
    # viewport through the launcher), and every scroll is then an error rather
    # than a scroll. Ask for the size the other harnesses use; a compositor
    # that refuses leaves the window as it was, and the log says which.
    try:
        rect = m.call("WebDriver:SetWindowRect", {"width": 1280, "height": 900})
        log("window", rect=rect)
    except RuntimeError as e:
        log("window", error=str(e)[:200])
    log("session")
    rounds = 0
    while time.monotonic() - T0 < SECS:
        rounds += 1
        for url in TOUR:
            if time.monotonic() - T0 >= SECS:
                break
            try:
                t = time.monotonic()
                m.call("WebDriver:Navigate", {"url": url})
                title = m.script("return document.title;")
                log("load", round=rounds, url=url, title=title, secs=round(time.monotonic() - t, 2))
                wheel(m, 240, 12)
                y1 = m.script("return window.scrollY;")
                wheel(m, -240, 6)
                y2 = m.script("return window.scrollY;")
                log("scroll", url=url, down_to=y1, back_to=y2)
                if url.endswith("webgl.html"):
                    time.sleep(3)
                    log("webgl", **webgl_check(m))
                time.sleep(2)
                log("compositor", value=compositor(m))
            except (RuntimeError, socket.timeout) as e:
                # A page that fails to load is recorded and the tour goes on;
                # a browser that has gone away ends the run below.
                log("step-error", url=url, error=str(e)[:300])
    # The frame rate *while scrolling* (waycompose-design.md §87): the long
    # page, a requestAnimationFrame counter, and wheel input for about five
    # seconds. rAF runs at the rate the compositor's frame callbacks allow, so
    # this is what a person scrolling sees, not what the GPU could do.
    try:
        scroll_fps(m, "tour")
    except (RuntimeError, socket.timeout, KeyError, TypeError) as e:
        log("scroll-fps", error=str(e)[:200])
    m.call("WebDriver:Navigate", {"url": "about:support"})
    time.sleep(3)
    text = m.script("return document.body.innerText;")
    rows = [line for line in text.splitlines()
            if any(k in line for k in ("Compositing", "WebGL 1 Driver Renderer",
                                       "WebGL 2 Driver Renderer", "GPU #1", "Driver Vendor",
                                       "Device ID", "Driver Version", "Window Protocol",
                                       "Target Frame Rate", "HW_COMPOSITING", "WEBRENDER"))]
    log("about-support", rows=rows[:40])
    open(os.path.join(OUT, "about-support.txt"), "w").write(text)
    log("done", rounds=rounds)


if __name__ == "__main__":
    try:
        main()
    except Exception as e:  # recorded, not swallowed: the harness reads it
        log("fatal", error=repr(e)[:500])
        sys.exit(1)
