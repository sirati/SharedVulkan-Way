#!/usr/bin/env bash
#
# **Which codec, and what it costs, on a machine with no GPU.** Runs *on*
# testhost's Linux host, like `sv-firefox-guest.sh` and `sv-guest-host.sh` --
# and, with `SVC_CPUS`, on any workstation, because the thing it measures is a
# CPU cost and the constraint that makes it visible is a core count.
#
#   scripts/sv-firefox-codec.sh up            weston + firefox + marionette
#   scripts/sv-firefox-codec.sh codecs        which preferences remove which codec
#   scripts/sv-firefox-codec.sh compare [s]   AV1 vs H.264, interleaved, at 1080p60
#   scripts/sv-firefox-codec.sh overlay TAG ARM   the player's own stats panel, photographed
#   scripts/sv-firefox-codec.sh support TAG   about:support, as JSON and as pixels
#   scripts/sv-firefox-codec.sh down
#
# `SVC_CPUS=0,1` pins weston and every Firefox process to two CPUs. testhost's
# host has exactly two and shares them with the VM, so on a 32-core workstation
# nothing is ever dropped and the difference between the codecs shows up only as
# CPU-seconds. Pinning is what makes the *dropped-frame* number reappear, and it
# is an honest reproduction of the constraint rather than of the machine: the
# cores are not the same cores, so the ratio transfers and the absolute does not.
# `SVC_FF` names the Firefox; the store path is what makes two machines
# comparable at all, and 151.0.2 is not 153.
#
# WHY A COMPOSITOR OF ITS OWN AND NOT THE LIVE SESSION
#
# The obvious venue is the running `wc-launch` session, because that is where
# the user watches video. It cannot be used: `wc-proxy` accepts a second client
# and the guest never answers it. Measured, with `WAYLAND_DEBUG=1` on a Firefox
# pointed at the live `wl-proxy`:
#
#     -> wl_display#1.get_registry(new id wl_registry#2)
#     -> wl_display#1.sync(new id wl_callback#3)
#     (nothing back, ever; zero CPU consumed; `client N: 0 client msgs` in
#      proxy.log)
#
# So this stands up its own weston -- headless, **pixman**, `wl_shm` and no
# dmabuf, which is the same shape `wc-proxy` offers -- and everything below is
# decode plus Software WebRender plus one shm copy. It is not the whole of the
# user's path and design.md §0.17 says so where it matters.
#
# WHY MARIONETTE AND NOT A SCREENSHOT
#
# The two numbers this is about are the player's own: `getStatsForNerds()` is
# what the "Stats for nerds" overlay renders from, and `getVideoPlaybackQuality()`
# is where its "Frames dropped of" line comes from. Reading them is not an
# inference about the overlay; it is the overlay's source. `about:support` is
# read the same way -- `Troubleshoot.snapshot()` is the object that page is
# built from -- and both are photographed as well, because a number quoted from
# a JSON blob and a number a user can see should be the same number.
set -uo pipefail
export PATH=/run/current-system/sw/bin:/run/wrappers/bin

RUN=${SVC_RUN:-/tmp/svcodec}
PROFILE=$RUN/profile
PORT=${SVC_PORT:-2929}
W=${SVC_W:-1280}
H=${SVC_H:-800}
VIDEO=${SVC_VIDEO:-aqz-KE-bpKQ}          # Big Buck Bunny, 60 fps, avc1 to 1080p60
# Named rather than searched for: a different build is a different measurement.
WESTON=${SVC_WESTON:-$(nix build --no-link --print-out-paths nixpkgs#weston 2>/dev/null)}
PY=${SVC_PY:-$(nix build --no-link --print-out-paths nixpkgs#python3 2>/dev/null)}
CPUS=${SVC_CPUS:-}
# `pixman` is the experiment and `gl` is its control. A pixman weston offers
# `wl_shm` and no `zwp_linux_dmabuf`, which is the shape `wc-proxy` offers; a gl
# weston offers dmabuf. Running the same Firefox against both, on a machine that
# does have a GPU, is what separates "this host has no DRM device" from "this
# compositor does not offer dmabuf" -- two causes that produce the same
# `about:support` row and that §0.17 had conflated.
RENDERER=${SVC_RENDERER:-pixman}

say() { echo "[$(date +%H:%M:%S)] $*"; }
# `taskset` and not a cgroup: affinity is inherited across `fork`, so pinning the
# parent pins every content and RDD process without having to find them, and the
# RDD process is where the decode actually happens. An array so that the empty
# case expands to nothing rather than to an empty argument.
PIN=(); [ -n "$CPUS" ] && PIN=(taskset -c "$CPUS")

# ------------------------------------------------------------------ user.js --
# Rendered exactly as `wc_launch::plan::user_js` renders `prefs` from
# `wc-launch.ini`, because the deployment is where this belongs and that is the
# mechanism it has (waycompose-design.md §28.5). The two lines that matter are
# pinned by `plan::tests::the_deployment_can_state_the_codec_the_host_can_afford`.
write_userjs() {
  cat >"$1" <<'JS'
// Written by wc-launch on every start. See docs/waycompose-design.md §28.5.
user_pref("middlemouse.paste", false);
JS
  cat >>"$1" <<JS
user_pref("marionette.port", $PORT);
user_pref("intl.accept_languages", "en-US,en");
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("media.autoplay.default", 0);
user_pref("media.autoplay.blocking_policy", 0);
${SVC_EXTRA_PREFS:-}
JS
}

# ------------------------------------------------------------------ python ---
write_client() {
  cat >"$RUN/mar.py" <<'PYEOF'
"""A Marionette client small enough to read: `<byte length>:<json>` frames,
`[0, id, name, params]` out and `[1, id, error, result]` back."""
import base64, json, socket


class Marionette:
    def __init__(self, port, host="127.0.0.1", timeout=240):
        self.s = socket.create_connection((host, port), timeout=30)
        self.s.settimeout(timeout)
        self.buf, self.msgid = b"", 0
        self.hello = self._recv()

    def _raw(self, n):
        while len(self.buf) < n:
            c = self.s.recv(65536)
            if not c:
                raise IOError("marionette closed")
            self.buf += c
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def _recv(self):
        while b":" not in self.buf:
            c = self.s.recv(65536)
            if not c:
                raise IOError("marionette closed")
            self.buf += c
        n, _, self.buf = self.buf.partition(b":")
        return json.loads(self._raw(int(n)))

    def cmd(self, name, params=None):
        self.msgid += 1
        body = json.dumps([0, self.msgid, name, params or {}]).encode()
        self.s.sendall(b"%d:%s" % (len(body), body))
        while True:
            m = self._recv()
            if m[0] == 1 and m[1] == self.msgid:
                if m[2]:
                    raise RuntimeError("%s: %s" % (name, json.dumps(m[2])[:400]))
                return m[3]

    def _v(self, r):
        return r["value"] if isinstance(r, dict) and "value" in r else r

    def start(self):
        self.cmd("WebDriver:NewSession", {"capabilities": {}})
        self.cmd("WebDriver:SetTimeouts",
                 {"script": 120000, "pageLoad": 180000, "implicit": 0})

    def context(self, w):
        return self.cmd("Marionette:SetContext", {"value": w})

    def navigate(self, u):
        return self.cmd("WebDriver:Navigate", {"url": u})

    def script(self, s, a=None):
        return self._v(self.cmd("WebDriver:ExecuteScript",
                                {"script": s, "args": a or [], "newSandbox": False}))

    def script_async(self, s, a=None):
        return self._v(self.cmd("WebDriver:ExecuteAsyncScript",
                                {"script": s, "args": a or [], "newSandbox": False}))

    def shot(self, path):
        d = self._v(self.cmd("WebDriver:TakeScreenshot", {"full": False, "hash": False}))
        open(path, "wb").write(base64.b64decode(d))
PYEOF

  cat >"$RUN/drive.py" <<'PYEOF'
"""codecs | compare | overlay | support -- see sv-firefox-codec.sh."""
import json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from mar import Marionette

RUN = os.path.dirname(os.path.abspath(__file__))
MODE, PORT = sys.argv[1], int(os.environ.get("SVC_PORT", "2929"))
VIDEO = os.environ.get("SVC_VIDEO", "aqz-KE-bpKQ")
WATCH = "https://www.youtube.com/watch?v=" + VIDEO
EMBED = "http://127.0.0.1:8931/"
# The two the deployment sets. `media.mediasource.vp9.enabled` is *not* one of
# them: with it and `media.av1.enabled` both false the site still served
# `vp09...(302)`, and `MediaSource.isTypeSupported` still answered yes for VP9
# in both containers. Only `media.webm.enabled` takes VP9 off the table.
FORCED = ["media.av1.enabled", "media.webm.enabled"]
# Every process of *this* Firefox, by the environment it was started with. Not
# the command line -- content processes are `-contentproc` and carry no profile
# path, and matching on it found the parent alone: 0.02 cores for a 720p60
# playback, which is not a number anything could believe.
MARK = ("XDG_RUNTIME_DIR=" + RUN + "/xdg").encode()


def our_pids():
    out = []
    for p in os.listdir("/proc"):
        if not p.isdigit():
            continue
        try:
            if MARK in open("/proc/%s/environ" % p, "rb").read():
                out.append(int(p))
        except (IOError, OSError):
            pass
    return out


def cpu_jiffies(pids):
    t = 0
    for p in pids:
        try:
            f = open("/proc/%d/stat" % p).read().rsplit(") ", 1)[1].split()
            t += int(f[11]) + int(f[12])
        except (IOError, OSError, IndexError, ValueError):
            pass
    return t / float(os.sysconf("SC_CLK_TCK"))


def set_codec(m, arm):
    m.context("chrome")
    for k in FORCED:
        m.script("Services.prefs.setBoolPref(arguments[0], arguments[1]); return 1;",
                 [k, arm != "h264"])
    m.context("content")


def play(m, url, quality, frame=False):
    m.navigate(url)
    if frame:
        m.cmd("WebDriver:SwitchToFrame", {"id": 0, "focus": True})
    m.script_async("""
    const cb = arguments[arguments.length-1]; let n = 0;
    const t = setInterval(() => {
      const p = document.getElementById('movie_player');
      if (p && p.getStatsForNerds) { clearInterval(t); cb('ok'); }
      else if (++n > 120) { clearInterval(t); cb('no player'); }
    }, 500);""")
    return m.script_async("""
    const cb = arguments[arguments.length-1];
    const p = document.getElementById('movie_player');
    try { p.setPlaybackQualityRange(arguments[0], arguments[0]); p.mute(); p.playVideo(); }
    catch (e) {}
    let n = 0;
    const t = setInterval(() => {
      const v = document.querySelector('video');
      if (v && !v.paused && v.currentTime > 1.0) { clearInterval(t); cb('playing'); }
      else if (++n > 100) { clearInterval(t); cb('stuck ' + p.getPlayerState()); }
    }, 500);""", [quality])


STATS = """
const p = document.getElementById('movie_player');
const v = document.querySelector('video');
const q = v.getVideoPlaybackQuality();
let s = {};
try { s = p.getStatsForNerds(); } catch (e) { s = {error: '' + e}; }
return {codecs: s.codecs, resolution: s.resolution, dims_and_frames: s.dims_and_frames,
        dropped: q.droppedVideoFrames, total: q.totalVideoFrames, t: v.currentTime,
        vw: v.videoWidth, vh: v.videoHeight};
"""

m = Marionette(PORT)
m.start()

if MODE == "codecs":
    TYPES = {"av1-mp4": 'video/mp4; codecs="av01.0.08M.08"',
             "vp9-webm": 'video/webm; codecs="vp9"',
             "vp9-mp4": 'video/mp4; codecs="vp09.00.51.08"',
             "h264-mp4": 'video/mp4; codecs="avc1.4d401f"',
             "opus-webm": 'audio/webm; codecs="opus"',
             "aac-mp4": 'audio/mp4; codecs="mp4a.40.2"'}
    COMBOS = [("default", {}),
              ("av1 off", {"media.av1.enabled": False}),
              ("mediasource.vp9 off", {"media.mediasource.vp9.enabled": False}),
              ("webm off", {"media.webm.enabled": False}),
              ("av1 off + mediasource.vp9 off",
               {"media.av1.enabled": False, "media.mediasource.vp9.enabled": False}),
              ("av1 off + webm off",
               {"media.av1.enabled": False, "media.webm.enabled": False})]
    keys = sorted({k for _, c in COMBOS for k in c})
    print("%-31s " % "combination" + " ".join("%-9s" % k for k in TYPES)
          + " what the site then served")
    for name, combo in COMBOS:
        m.context("chrome")
        for k in keys:
            m.script("Services.prefs.setBoolPref(arguments[0], arguments[1]); return 1;",
                     [k, combo.get(k, True)])
        m.context("content")
        m.navigate(EMBED)
        row = m.script("return %s.map(t => MediaSource.isTypeSupported(t));"
                       % json.dumps(list(TYPES.values())))
        # `isTypeSupported` is what the *page* asks; what the page then chooses
        # is a different question and it is the one that matters. Both, on one
        # line, because the two have already disagreed once here:
        # `media.mediasource.vp9.enabled` answers NO for VP9 and the site serves
        # itag 302 -- VP9 in WebM -- regardless.
        served = "-"
        try:
            print(name, play(m, EMBED, "hd1080", frame=True), end=" ")
            time.sleep(6)
            served = m.script(STATS)["codecs"]
        except RuntimeError as e:
            served = "ERR " + str(e)[:80]
        print()
        print("%-31s " % name + " ".join("%-9s" % ("yes" if x else "NO") for x in row)
              + " " + str(served))

elif MODE == "compare":
    secs = int(sys.argv[2]) if len(sys.argv) > 2 else 60
    quality = sys.argv[3] if len(sys.argv) > 3 else "hd1080"
    # A B A B: this host's two cores are shared with the VM, so a straight
    # before/after would measure the drift as well as the codec.
    rows = []
    for arm in ["av1", "h264", "av1", "h264"]:
        set_codec(m, arm)
        print(arm, play(m, EMBED, quality, frame=True))
        time.sleep(25)                     # a player starting up is not the steady state
        pids = our_pids()
        j0, w0 = cpu_jiffies(pids), time.time()
        mk = m.script(STATS)
        time.sleep(secs)
        j1, w1 = cpu_jiffies(our_pids()), time.time()
        r = m.script(STATS)
        r.update(arm=arm, nproc=len(pids), cpu_secs=j1 - j0, wall_secs=w1 - w0,
                 dropped_in_window=r["dropped"] - mk["dropped"],
                 frames_in_window=r["total"] - mk["total"])
        r["cores"] = r["cpu_secs"] / r["wall_secs"]
        rows.append(r)
        print("%-5s %-38s %-24s dropped %5d of %6d   %.1f cpu-s (%.2f cores)" % (
            arm, r["codecs"], r["resolution"], r["dropped_in_window"],
            r["frames_in_window"], r["cpu_secs"], r["cores"]))
    open(RUN + "/compare.json", "w").write(json.dumps(rows, indent=1))

elif MODE == "overlay":
    tag, arm = sys.argv[2], sys.argv[3]
    quality = sys.argv[4] if len(sys.argv) > 4 else "hd720"
    set_codec(m, arm)
    print(play(m, WATCH, quality))
    # Before the panel and not after it: the consent wall is a *dialog over* the
    # page, so dismissing it once the panel is open leaves the panel behind an
    # overlay in the photograph. The first version of this did it in the other
    # order and the h264 shot came back with the codec line half covered.
    for _ in range(3):
        try:
            hit = m.script("""
            const b = Array.from(document.querySelectorAll('button, [role=button]'))
              .find(e => /reject all|alle ablehnen/i.test(e.textContent || ''));
            if (b) { b.click(); return 1; } return 0;""")
        except RuntimeError:
            hit = 0
        if not hit:
            break
        time.sleep(4)
    time.sleep(20)
    # `showVideoInfo` is the player's own method -- the one its context menu
    # item calls. The menu itself does not open under a synthesised right
    # click; the events were checked as trusted and at the video's own centre
    # (`mousedown`, `contextmenu`, `mouseup` all arrived) before this was
    # accepted, so what is missing is YouTube's handler and not the input.
    print("panel:", m.script("""
    const p = document.getElementById('movie_player');
    if (p.showVideoInfo) p.showVideoInfo();
    const el = p.querySelector('.html5-video-info-panel');
    return el ? el.innerText.replace(/\\n+/g, ' | ').slice(0, 400) : 'no panel';
    """))
    try:                                   # the consent interstitial is not the experiment
        m.script("""
        const b = Array.from(document.querySelectorAll('button, [role=button]'))
          .find(e => /reject all|accept all/i.test(e.textContent || ''));
        if (b) b.click();
        return 1;""")
        time.sleep(4)
        m.script("""
        const p = document.getElementById('movie_player');
        if (p.showVideoInfo && !p.isVideoInfoVisible()) p.showVideoInfo();
        return 1;""")
    except RuntimeError:
        pass
    m.shot("%s/shot-overlay-%s.png" % (RUN, tag))
    print("shot: %s/shot-overlay-%s.png" % (RUN, tag))

elif MODE == "support":
    tag = sys.argv[2] if len(sys.argv) > 2 else "now"
    m.context("chrome")
    snap = m.script_async("""
    const cb = arguments[arguments.length-1];
    const { Troubleshoot } = ChromeUtils.importESModule('resource://gre/modules/Troubleshoot.sys.mjs');
    Troubleshoot.snapshot().then(s => cb(JSON.stringify(s)), e => cb('ERR ' + e));""")
    open("%s/support-%s.json" % (RUN, tag), "w").write(snap)
    d = json.loads(snap)
    print("media:", json.dumps(d["media"], indent=1))
    for f in d["graphics"]["featureLog"]["features"]:
        if "VIDEO" in f["name"] or "DMABUF" in f["name"] or f["name"] == "WEBRENDER":
            print("%-26s %-14s %s" % (f["name"], f["status"],
                                      (f.get("log") or [{}])[-1].get("failureId", "")))
    m.context("content")
    m.navigate("about:support")
    time.sleep(8)
    # By *heading text* and not by element id. `media-title` scrolled nothing and
    # both photographs came back showing the top of the page, which is the one
    # failure mode a screenshot cannot report: it looks like a screenshot.
    for name, want in [("media", "Media"), ("graphics", "Graphics")]:
        got = m.script("""
        const h = Array.from(document.querySelectorAll('h2, h3'))
          .find(e => (e.textContent || '').trim() === arguments[0]);
        if (!h) return 'no heading';
        h.scrollIntoView({block: 'start'});
        return '' + Math.round(window.scrollY);""", [want])
        time.sleep(2)
        m.shot("%s/shot-support-%s-%s.png" % (RUN, name, tag))
        print("%s: scrollY %s" % (name, got))
    print("shots written")
PYEOF
}

case "${1:-}" in
up)
  mkdir -p "$RUN/xdg" "$PROFILE" "$RUN/www"; chmod 700 "$RUN/xdg"
  write_userjs "$PROFILE/user.js"
  write_client
  export XDG_RUNTIME_DIR=$RUN/xdg
  if [ ! -S "$RUN/xdg/svcodec" ]; then
    # pixman, not GL. This host has no DRM device and a GL renderer would
    # advertise a dmabuf the real topology cannot have.
    say "weston, headless, $RENDERER"
    setsid "${PIN[@]}" "$WESTON/bin/weston" --backend=headless --renderer=$RENDERER \
      --width=$W --height=$H --socket=svcodec --idle-time=0 \
      >"$RUN/weston.log" 2>&1 </dev/null &
    for _ in $(seq 1 60); do [ -S "$RUN/xdg/svcodec" ] && break; sleep 0.5; done
  fi
  [ -S "$RUN/xdg/svcodec" ] || { echo "FAIL: weston never listened"; tail -20 "$RUN/weston.log"; exit 1; }
  # A local origin for the embedded player: loading /embed/ at top level answers
  # `Error 153, Video player configuration error`, and the site's own consent
  # wall stands in front of a fresh profile on the watch page.
  sed "s/VIDEOID/$VIDEO/" >"$RUN/www/index.html" <<'HTML'
<!doctype html><html><head><meta charset=utf-8><title>codec harness</title>
<style>html,body{margin:0;background:#111}iframe{display:block;border:0}</style></head><body>
<iframe width=1280 height=720 allow="autoplay; encrypted-media" allowfullscreen
 src="https://www.youtube.com/embed/VIDEOID?autoplay=1&mute=1&rel=0&modestbranding=1&enablejsapi=1"></iframe>
</body></html>
HTML
  pkill -f "http.server 8931" 2>/dev/null
  (cd "$RUN/www" && setsid "$PY/bin/python3" -m http.server 8931 --bind 127.0.0.1 \
     >"$RUN/www.log" 2>&1 </dev/null &)
  FF=${SVC_FF:-$(ls -d /nix/store/*-firefox-[0-9]*/bin/firefox | sort -V | tail -1)}
  say "firefox $("$FF" --version) -- $FF"
  # `--remote-allow-system-access` is what lets Marionette reach chrome context,
  # which is where `Troubleshoot.snapshot()` -- about:support's own object -- is.
  setsid "${PIN[@]}" env -u DISPLAY XDG_RUNTIME_DIR="$RUN/xdg" WAYLAND_DISPLAY=svcodec \
    MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
    MOZ_LOG="${MOZ_LOG:-}" MOZ_LOG_FILE="${MOZ_LOG_FILE:-}" \
    "$FF" --profile "$PROFILE" --marionette --remote-allow-system-access \
    --no-remote --new-window about:blank >"$RUN/ff.log" 2>&1 </dev/null &
  for _ in $(seq 1 120); do
    ss -ltn 2>/dev/null | grep -q ":$PORT " && { say "marionette on $PORT"; exit 0; }
    sleep 1
  done
  echo "FAIL: marionette never listened"; tail -20 "$RUN/ff.log"; exit 1
  ;;
codecs|compare|overlay|support)
  SVC_PORT=$PORT SVC_VIDEO=$VIDEO "$PY/bin/python3" -u "$RUN/drive.py" "$@" ;;
down)
  # By the profile path, never by image name: the user's own Firefox is on this
  # machine and is called the same thing.
  for p in $(pgrep -f -- "--profile $PROFILE"); do kill "$p" 2>/dev/null; done
  sleep 3
  for p in $(pgrep -f -- "--profile $PROFILE"); do kill -9 "$p" 2>/dev/null; done
  for p in $(pgrep -f -- "--socket=svcodec"); do kill "$p" 2>/dev/null; done
  pkill -f "http.server 8931" 2>/dev/null
  echo "down" ;;
*) echo "usage: up|codecs|compare|overlay|support|down"; exit 2 ;;
esac
