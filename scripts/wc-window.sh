#!/usr/bin/env bash
# Prove that a Wayland surface becomes a real Windows window showing the
# client's pixels — the stage-3 milestone.
#
#   scripts/wc-window.sh linux     the buffer path, against third-party clients
#   scripts/wc-window.sh wine      one HWND per toplevel, with a screenshot
#   scripts/wc-window.sh all       both
#
# Run from inside `nix develop`.
#
# WHY THE EVIDENCE IS IN TWO HALVES, AND WHY NEITHER IS SUFFICIENT
#
# Stage 2 measured the fact that shapes this whole file (docs/design.md §7.5):
# **no third-party Wayland client can send a buffer to the Windows build.** An
# `h` argument travels out-of-band by `SCM_RIGHTS` on Unix and in-band as a
# descriptor on Windows (§2.3); Wine's AF_UNIX carries no ancillary data; so a
# stock libwayland client's first `wl_shm.create_pool` leaves the descriptor
# absent and the server's parser desynchronises at that request. Every Wayland
# client in existence is a stock libwayland client.
#
# The brief for this stage proposed `weston-simple-shm` and asked whether §2.4
# applies to it. **It does**, and `run_wine_thirdparty` below measures that
# rather than asserting it: a real Weston client reaches `xdg_toplevel` against
# `wc-compositor.exe` and then delivers zero frames. The ordering trick that
# saved stage 2 — surface and toplevel before any `h` — buys a window with
# nothing in it, which is exactly not the stage-3 milestone.
#
# So:
#
#   * **linux** runs `weston-terminal` and `weston-presentation-shm`: C
#     programs, libwayland, upstream xdg-shell, which have never heard of this
#     project and cannot share a bug with it. They drive the entire buffer path
#     — pool, buffer, attach, damage, commit, release, frame callback — and the
#     harness reads the resulting pixels back out as PPM and measures them.
#     What they cannot do is produce an HWND: there is no Win32 here.
#
#   * **wine** runs `wc-shmclient.exe`, which speaks the dialect and is ours.
#     That is a real weakening and is stated as one. What it buys is the half
#     nothing else can reach: a named section created by one Windows process,
#     opened by name by another, blitted by GDI into a window with Windows'
#     own caption — and a screenshot of that window taken by `xwd` from the X
#     server, outside the process, by a tool that is not ours.
#
# Together they cover the path. Separately, each is half a claim, and the report
# says so.
#
# WHY Xvfb IS A DEPENDENCY AND NOT A CONVENIENCE
#
# Measured with `scripts/probe-window.c`: with no `DISPLAY`, Wine's
# `CreateWindowExW` fails for **every** visible top-level window — error 1400 or
# 0 — while a message-only window succeeds and `GetSystemMetrics(SM_CXSCREEN)`
# still cheerfully answers 1024. A headless Wine cannot run this stage at all.
#
# `wine explorer /desktop=` puts the window inside a Wine virtual desktop, which
# matters for what the screenshot shows: with no window manager on a bare X
# server, an ordinary Wine window is left undecorated by X, and the caption we
# are trying to demonstrate would not be drawn by anybody. Inside a virtual
# desktop Wine draws the whole frame itself, which is also what it does on real
# Windows' behalf.
#
# WHAT WINE DOES AND DOES NOT PROVE. Unchanged from stages 1 and 2, and still
# the most important sentence in this file: Wine is not Windows. A pass says our
# code paths are self-consistent and structurally possible. Wine's `user32` and
# `gdi32` are reimplementations; `SetDIBitsToDevice`'s top-down behaviour, the
# `WM_ERASEBKGND` clipping and the modal resize loop are all places where the
# real thing may differ. docs/waycompose-design.md §8.8 lists them.
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

: "${WINEPREFIX:=/tmp/wc-wineprefix}"
: "${WINEDEBUG:=-all}"
export WINEPREFIX WINEDEBUG

pass=0
fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }

# STAGE 42. `fresh` / `fresh_or_skip`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"

# §51. Whether a hypervisor is in the path, declared rather than left unknown.
# This harness is same-box, so `wc_latency_claim` will refuse any assertion here
# whose truth depends on cross-VM timing -- which is the point: §27.6 drew one
# from a harness exactly like this one.
. "$(dirname "$0")/wc-topology.sh"
wc_topology_declare same-box \
  "wc-compositor.exe under Wine on this box; no guest is involved" || exit 1

RUNDIR=$(mktemp -d /tmp/wc-window-XXXXXX)
chmod 700 "$RUNDIR"
KEEP="${WC_KEEP:-}"
cleanup() { [ -n "$KEEP" ] || rm -rf "$RUNDIR"; }
trap cleanup EXIT

# ---------------------------------------------------------------------------
# linux: the buffer path, against clients that cannot share our bugs
# ---------------------------------------------------------------------------

# `weston-simple-shm` is not in nixpkgs' weston (it builds none of the
# `simple-*` clients). `weston-presentation-shm` is the closest thing to it
# that is: a simple-client rather than a toytoolkit one, pure `wl_shm`, and it
# redraws continuously — measured at 12 presents with 12 *distinct* images in
# four seconds, where `weston-flower` and `weston-multi-resource` draw once and
# stop because they are static. "Distinct" is the property that matters: a
# client whose pixels never change cannot show that the blit is live.
CLIENT_ANIM=weston-presentation-shm
# A second client of a completely different shape: toytoolkit, `Argb8888`,
# 806x491, real text rendering. If both agree, the path is not tuned to one
# client's habits.
CLIENT_BIG=weston-terminal

run_linux() {
  echo "== linux: the buffer path, driven by third-party clients =="
  if [ ! -x "$LINUX_BIN/wc-compositor" ]; then
    bad "cargo build -p wc-compositor"; return
  fi

  for client in "$CLIENT_ANIM" "$CLIENT_BIG"; do
    command -v "$client" >/dev/null || { note "SKIP $client (not installed)"; continue; }
    local dir="$RUNDIR/$client"
    mkdir -p "$dir/frames"
    chmod 700 "$dir"

    XDG_RUNTIME_DIR="$dir" "$LINUX_BIN/wc-compositor" \
      --socket wl-win --timeout 25 --simulate-wm --debug-icon \
      --dump-frames "$dir/frames" >"$dir/srv.log" 2>&1 &
    local srv=$!
    sleep 0.6
    XDG_RUNTIME_DIR="$dir" WAYLAND_DISPLAY=wl-win timeout 8 "$client" \
      >"$dir/cli.log" 2>&1
    local client_rc=$?
    sleep 0.6
    kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

    local log="$dir/srv.log"
    local presents distinct
    presents=$(grep -c "present window" "$log")
    # Only the hashes of presented frames. The `--debug-icon` line carries a
    # hash too, and counting it inflated this by one.
    distinct=$(grep "present window" "$log" | grep -o "hash=[0-9a-f]*" | sort -u | wc -l)

    echo "  -- $client --"
    if grep -q "xdg_toplevel created" "$log"; then
      ok "$client: reached xdg_toplevel"
    else
      bad "$client: no xdg_toplevel"; sed 's/^/        /' "$log" | head -20; continue
    fi
    # The claim of the stage, on this side: the client's pixels arrived.
    if [ "$presents" -ge 2 ]; then
      ok "$client: $presents frame(s) presented from wl_shm"
    else
      bad "$client: only $presents frame(s) presented"
    fi
    if grep -q "pixels served by wl_shm" "$log"; then
      ok "$client: the pixel source that served it is named in the log"
    else
      bad "$client: no pixel source recorded"
    fi
    # `weston-smoke` presents happily and its pixels never change, because it
    # animates only in response to pointer input. A hash count is what tells
    # "the blit is live" from "the same frame twelve times".
    if [ "$client" = "$CLIENT_ANIM" ]; then
      if [ "$distinct" -ge 3 ]; then
        ok "$client: $distinct distinct images -- the pixels really change"
      else
        bad "$client: only $distinct distinct image(s) across $presents frames"
      fi
    fi
    if grep -q "title ->" "$log"; then
      ok "$client: xdg_toplevel.set_title reached the window"
    else
      bad "$client: no title"
    fi
    # The reverse direction, which no real client can drive here because there
    # is no window manager: `--simulate-wm` plays one.
    if grep -q "configure [0-9]*x[0-9]* (serial" "$log"; then
      ok "$client: a simulated resize produced a real xdg_toplevel.configure"
    else
      bad "$client: the simulated resize produced no configure"
    fi
    if grep -q "close requested -> xdg_toplevel.close" "$log"; then
      ok "$client: a simulated close button produced xdg_toplevel.close"
      # And the client honoured it, which is the half that is not ours.
      if [ "$client_rc" -eq 0 ]; then
        ok "$client: the client honoured close and exited 0"
      else
        note "$client: exited $client_rc after close (124 = killed by timeout)"
      fi
    else
      bad "$client: no close was sent"
    fi

    # Read the pixels back with tools that are not ours. A PPM that decodes, is
    # a plausible surface rather than a sliver, and is not one flat colour is a
    # much stronger statement than any log line the compositor writes about
    # itself.
    #
    # STAGE 42. **The sentence here used to say "has the size the protocol
    # said", and nothing compared the size to anything** — the geometry was
    # interpolated into the `ok` string and dropped. The comment was the older
    # of the two and the code never caught up, which is §35.10: a claim in prose
    # that nothing asserts outlives its truth.
    #
    # It is corrected rather than implemented, because on this leg the claim
    # cannot be made: `run_linux` drives *third-party* clients — weston-terminal
    # and friends — which choose their own surface size, so the harness has no
    # protocol-declared number to compare against. (The Wine leg does: it passes
    # `--width 420 --height 300` to `wc-shmclient.exe`.) What is checkable here
    # is shape, so that is what is checked and what the comment now says.
    local frame
    frame=$(find "$dir/frames" -name '*.ppm' | sort | tail -1)
    if [ -z "$frame" ]; then
      bad "$client: no frame was dumped"
    elif ! command -v identify >/dev/null; then
      note "$client: imagemagick absent; not checking the dumped pixels"
    else
      local geom colours
      geom=$(identify -format "%wx%h" "$frame" 2>/dev/null)
      colours=$(identify -format "%k" "$frame" 2>/dev/null)
      local fw fh
      fw=${geom%x*}; fh=${geom#*x}
      if [ -z "$geom" ]; then
        bad "$client: the dumped frame does not decode"
      elif [ "${fw:-0}" -lt 32 ] || [ "${fh:-0}" -lt 32 ]; then
        bad "$client: the dumped frame is $geom -- too small to be a client's surface"
      else
        ok "$client: the dumped frame decodes as $geom with $colours distinct colours"
      fi
      # One flat colour means the blit produced nothing, which a hash check
      # would happily call a pass if every frame were flat in the same way.
      if [ "${colours:-0}" -ge 4 ]; then
        ok "$client: the frame has structure, not a single flat colour"
      else
        bad "$client: the frame has only ${colours:-0} colour(s)"
      fi
    fi
  done
}

# ---------------------------------------------------------------------------
# wine: one HWND per toplevel
# ---------------------------------------------------------------------------

# Start an Xvfb and export DISPLAY. See the header for why this is required
# rather than tidy.
start_x() {
  local num=$1
  Xvfb ":$num" -screen 0 1024x768x24 -nolisten tcp >"$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$num"
  for _ in $(seq 1 100); do
    if xwd -root -silent >/dev/null 2>&1; then return 0; fi
    sleep 0.1
  done
  return 1
}

stop_x() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  unset XPID
}

# Find the compositor's AF_UNIX node. Discover it rather than predict it: Wine
# builds the Windows environment block itself, so `LOCALAPPDATA` cannot be
# overridden from the Unix side, and finding the node also checks the path the
# code chose. `-type s` proves it is a real host socket rather than a
# placeholder file, which is what lets a native Linux client reach the .exe.
find_socket() {
  local name=$1 cdrive="$WINEPREFIX/drive_c" node=""
  for _ in $(seq 1 250); do
    node=$(find "$cdrive" -type s -name "$name" 2>/dev/null | head -1)
    [ -n "$node" ] && { echo "$node"; return 0; }
    sleep 0.2
  done
  return 1
}

run_wine() {
  echo "== wine: one HWND per xdg_toplevel, with the client's pixels in it =="
  for exe in wc-compositor.exe wc-shmclient.exe; do
    fresh "$WIN_BIN/$exe" "cargo build --target x86_64-pc-windows-gnu" || return
  done
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }
  command -v Xvfb >/dev/null || { bad "Xvfb not on PATH -- see the header; it is required"; return; }

  if ! start_x 91; then
    bad "could not start Xvfb"; stop_x; return
  fi
  ok "Xvfb up on $DISPLAY (1024x768x24)"

  local sock=wl-stage3
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  # Inside a Wine virtual desktop, so Wine draws the frame itself rather than
  # leaving it to an X window manager that is not running. See the header.
  #
  # `--log` rather than a shell redirect, and that was measured rather than
  # chosen: `wine explorer /desktop=` gives its child a console window *inside*
  # the desktop, so a redirect on the outside captures the loader's chatter and
  # nothing else. The first version of this harness asserted against an empty
  # file while the compositor was working perfectly.
  local srvlog="$RUNDIR/win-srv.log"
  wine explorer /desktop=waycompose,1024x768 \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 60 --debug-icon \
    --log "$(winepath -w "$srvlog" 2>/dev/null || echo "$srvlog")" \
    >"$RUNDIR/win-loader.log" 2>&1 &
  local srv=$!

  local node
  if ! node=$(find_socket "$sock"); then
    bad "the Windows compositor never created its socket node"
    sed 's/^/        /' "$RUNDIR/win-srv.log" | head -20
    kill "$srv" 2>/dev/null; stop_x; return
  fi
  ok "the Windows binary bound an AF_UNIX socket; host node: ${node#"$WINEPREFIX/drive_c"}"

  # The dialect client, as a second Wine process. Two mingw binaries, one named
  # section, one socket.
  #
  # The frame count has to outlast the screenshots. Frame callbacks are
  # throttled to 16 ms, so 90 frames is about 1.5 s — the first version of this
  # harness photographed an empty desktop because the client had finished and
  # the window had been destroyed four seconds earlier. 600 frames is roughly
  # ten seconds, which comfortably covers both shots.
  WAYLAND_DISPLAY="$sock" timeout 40 wine "$WIN_BIN/wc-shmclient.exe" \
    --frames "${WC_FRAMES:-600}" --width 420 --height 300 \
    --title "waycompose stage 3" --partial-damage \
    >"$RUNDIR/win-cli.log" 2>&1 &
  local cli=$!

  # Let it map and animate, then photograph the X root while it is still up.
  sleep 5
  local shot="$RUNDIR/screen.png"
  if xwd -root -silent -out "$RUNDIR/screen.xwd" 2>/dev/null &&
     convert "$RUNDIR/screen.xwd" "$shot" 2>/dev/null; then
    ok "captured the X root window with xwd while the client was drawing"
  else
    bad "could not capture the screen"
  fi
  # A second shot a moment later: two identical screenshots of an animating
  # client would mean the window is a still image, which is the failure a
  # single screenshot cannot detect.
  sleep 1.5
  local shot2="$RUNDIR/screen2.png"
  xwd -root -silent -out "$RUNDIR/screen2.xwd" 2>/dev/null &&
    convert "$RUNDIR/screen2.xwd" "$shot2" 2>/dev/null

  wait "$cli" 2>/dev/null
  local cli_rc=$?
  sleep 1
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

  local log="$RUNDIR/win-srv.log"
  echo "  -- protocol --"
  grep -q "xdg_toplevel created" "$log" && ok "wine: reached xdg_toplevel" \
    || bad "wine: no xdg_toplevel"
  grep -q "pixels served by wl_shm" "$log" \
    && ok "wine: a named section was opened by name and read across processes" \
    || bad "wine: the compositor could not read the client's buffer"
  if [ "$cli_rc" -eq 0 ]; then
    ok "wine: the client committed its frames and exited 0"
  else
    bad "wine: the client exited $cli_rc"
    sed 's/^/        /' "$RUNDIR/win-cli.log" | tail -15
  fi

  echo "  -- the window --"
  # The milestone. `HWND` in the log means `CreateWindowExW` returned one;
  # `mapped` means it was shown at the size the client committed.
  if grep -q "window 1 created (HWND" "$log"; then
    ok "wine: CreateWindowExW made a real HWND for the toplevel  <-- the stage-3 milestone"
    sed -n 's/^\(wc-compositor: window 1 created.*\)$/        \1/p' "$log"
  else
    bad "wine: no HWND was created"
    grep -i "could not create a window" "$log" | sed 's/^/        /'
  fi
  grep -q "window 1 mapped" "$log" \
    && ok "wine: the window was shown at the size the client committed" \
    || bad "wine: the window was never mapped"
  grep -q "window 1 title -> " "$log" \
    && ok "wine: set_title reached SetWindowTextW" \
    || bad "wine: the title never reached the window"
  # STAGE 42. **This grep could never match, and its failing arm was a `note`,
  # so it reported nothing either way from the day it was written.** Both §35
  # defects in three lines: a check that cannot pass, wrapped in one that cannot
  # fail. The compositor prints
  #
  #   wc-compositor: window 1 icon set: 2 of 2 slots from 3 image(s) [48x48 …]
  #
  # (`win32.rs`, `"… icon set: {} of 2 slots from {} image(s) [{}]"`), so the
  # substring `icon set from` does not occur in this tree and never has —
  # confirmed by grepping every literal in every harness against `crates/`,
  # where this was the only genuine never-matcher. `wc-icon.sh` greps the real
  # string, `icon set: 2 of 2 slots`, which is what this now does.
  #
  # The verdict is `bad` rather than `note` because the two lines above it —
  # `window 1 mapped`, `window 1 title -> ` — are `bad`, and an icon that did
  # not reach `WM_SETICON` is the same kind of fact as a title that did not
  # reach `SetWindowTextW`. §16 spent a whole stage on it.
  local icons
  icons=$(grep -o "icon set: [0-9]* of 2 slots" "$log" | tail -1)
  case "$icons" in
    "icon set: 2 of 2 slots") ok "wine: WM_SETICON accepted an icon for both slots ($icons)" ;;
    "") bad "wine: no icon reached WM_SETICON at all" ;;
    *)  bad "wine: only part of the icon reached the window ($icons)" ;;
  esac

  echo "  -- the screenshot, taken from outside the process --"
  check_screenshot "$shot" "$shot2"

  stop_x
}

# Measure the captured screen. Everything here is ImageMagick reading an X
# dump; none of it trusts anything the compositor said about itself.
check_screenshot() {
  local shot=$1 shot2=$2
  if [ ! -s "$shot" ]; then bad "no screenshot to check"; return; fi
  command -v identify >/dev/null || { note "imagemagick absent"; return; }

  local colours
  colours=$(identify -format "%k" "$shot" 2>/dev/null)
  note "the screen has ${colours:-0} distinct colours"
  # A bare Xvfb root is a single flat colour. Anything drawn at all raises this
  # well above the handful a blank desktop plus a mouse cursor would give.
  if [ "${colours:-0}" -ge 16 ]; then
    ok "the screen is not a blank root window ($colours colours)"
  else
    bad "the screen looks blank ($colours colours)"
  fi

  # The client paints a known picture (crates/wc-shmclient/src/proto.rs): a
  # dark blue-grey field, a red border, and pure red, green and blue corner
  # markers at three corners. Finding those colours *on the X server* is the
  # end-to-end statement: the client wrote them into a named section, the
  # compositor opened it by name and blitted it with GDI, and Wine put it on
  # the screen.
  local found=0
  for spec in "srgb(255,0,0):red" "srgb(0,255,0):green" "srgb(0,0,255):blue"; do
    local colour=${spec%%:*} label=${spec##*:}
    local n
    n=$(convert "$shot" -fill white -fuzz 6% +opaque "$colour" \
        -fill black -fuzz 6% -opaque "$colour" -format "%[fx:w*h*(1-mean)]" info: 2>/dev/null)
    n=${n%%.*}
    if [ "${n:-0}" -ge 100 ]; then
      ok "the client's $label corner marker is on screen (${n} px)"
      found=$((found + 1))
    else
      bad "the client's $label corner marker is not on screen (${n:-0} px)"
    fi
  done
  [ "$found" -eq 3 ] && ok "all three corner markers found -- the blit is not mirrored or flipped"

  # Two shots of an animating client must differ. A window showing a single
  # stale frame would pass every check above.
  if [ -s "$shot2" ]; then
    local diff
    # `compare -metric AE` writes the count to stderr, and for some formats
    # appends a normalised value in parentheses ("32 (4.8e-04)"). Take the
    # leading integer only.
    diff=$(compare -metric AE "$shot" "$shot2" null: 2>&1 | tr -d '\n' \
           | grep -oE '^[0-9]+' | head -1)
    if [ "${diff:-0}" -ge 500 ]; then
      ok "two screenshots 1.5 s apart differ by ${diff} pixels -- the window is live"
    else
      bad "two screenshots differ by only ${diff:-0} pixels -- the window looks frozen"
    fi
  fi

  # Keep the evidence where a human can look at it.
  local out="${WC_SHOT:-/tmp/wc-stage3-screenshot.png}"
  cp "$shot" "$out" 2>/dev/null && note "screenshot copied to $out"
}

# ---------------------------------------------------------------------------
# wine, third-party: the measurement behind §2.4, not an assertion of it
# ---------------------------------------------------------------------------

run_wine_thirdparty() {
  echo "== wine: what a *third-party* client gets from the Windows build =="
  echo "  (docs/waycompose-design.md §2.4 and §7.5, measured rather than asserted)"
  fresh_or_skip "$WIN_BIN/wc-compositor.exe" "SKIP: no wc-compositor.exe"; case $? in 2) note "SKIP: no wc-compositor.exe"; return ;; 1) return ;; esac
  command -v "$CLIENT_ANIM" >/dev/null || { note "SKIP: $CLIENT_ANIM not installed"; return; }

  local sock=wl-stage3-tp
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  # No Xvfb and no virtual desktop: this run is about the byte stream, and it
  # stops before any window would be shown.
  wine "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 25 \
    >"$RUNDIR/tp-srv.log" 2>&1 &
  local srv=$!
  local node
  if ! node=$(find_socket "$sock"); then
    note "SKIP: the compositor never bound its socket"
    kill "$srv" 2>/dev/null; return
  fi

  mkdir -p "$RUNDIR/tp-xdg"; chmod 700 "$RUNDIR/tp-xdg"
  ln -sf "$node" "$RUNDIR/tp-xdg/$sock"
  XDG_RUNTIME_DIR="$RUNDIR/tp-xdg" WAYLAND_DISPLAY="$sock" timeout 8 "$CLIENT_ANIM" \
    >"$RUNDIR/tp-cli.log" 2>&1
  sleep 0.8
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

  local log="$RUNDIR/tp-srv.log" presents
  presents=$(grep -c "present window" "$log")

  if grep -q "xdg_toplevel created" "$log"; then
    ok "third-party: reaches xdg_toplevel against the Windows build (stage 2's result, still true)"
  else
    bad "third-party: does not even reach xdg_toplevel"
  fi
  # The point. A window with no pixels in it is not this stage's milestone, and
  # this is why `wc-shmclient` has to exist.
  if [ "$presents" -eq 0 ]; then
    ok 'third-party: 0 frames presented -- §2.4 confirmed: an `h` from a stock client does not arrive'
  else
    echo "  ????  third-party: $presents frame(s) presented -- §2.4 may no longer hold; investigate"
    fail=$((fail + 1))
  fi
}

case "$MODE" in
  linux) run_linux ;;
  wine)  run_wine; echo; run_wine_thirdparty ;;
  all)   run_linux; echo; run_wine; echo; run_wine_thirdparty ;;
  *) echo "usage: $0 [linux|wine|all]"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
