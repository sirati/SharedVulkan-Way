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


def main():
    m = Marionette(PORT)
    log("hello", hello=m.hello)
    m.call("WebDriver:NewSession", {"capabilities": {"alwaysMatch": {"pageLoadStrategy": "normal"}}})
    m.call("WebDriver:SetTimeouts", {"pageLoad": 60000, "script": 30000})
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
