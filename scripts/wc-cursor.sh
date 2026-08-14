#!/usr/bin/env bash
# Prove the stage-12 milestone: **the pointer looks like what the client asked
# for, and there is exactly one of it**.
#
#   scripts/wc-cursor.sh linux   the protocol: is `wp_cursor_shape_v1` there,
#                                and can the proxy carry it
#   scripts/wc-cursor.sh wine    a real HCURSOR, built from a real client's
#                                cursor surface, read back out of Win32
#   scripts/wc-cursor.sh all     both
#
# Run from inside `nix develop`.
#
# WHY THE EVIDENCE IS IN TWO LEGS
#
# The same split every stage here draws, and the line is in a different place
# than usual because the two halves of this feature are genuinely different
# problems (docs/waycompose-design.md §23):
#
#   * **linux.** `wp_cursor_shape_v1` is a *name* on the wire and nothing else,
#     so what can go wrong with it is protocol-shaped: the global is not
#     advertised, or the proxy cannot walk the interface and refuses the
#     connection. Both are visible to `wayland-info` — libwayland's own client,
#     which has never heard of this project — and neither needs a pointer or a
#     Windows. The *mapping* from a name to `IDC_IBEAM` is arithmetic and is
#     unit-tested in `crates/wc-compositor/src/cursor.rs`, on the machine that
#     builds it, so it is not re-tested here.
#
#   * **wine.** `wl_pointer.set_cursor` is *pixels*, and what can go wrong with
#     it is Win32-shaped: the `HCURSOR` is not built, or is built and never
#     installed, or is installed and immediately overwritten by
#     `DefWindowProc`'s class cursor on the next mouse move. None of that is
#     visible from the Wayland side — a client that sets a cursor is told
#     nothing about what happened to it — so this leg reads the answer out of
#     `GetCursorInfo` with `scripts/probe-cursor.c`.
#
# WHY A HANDLE AND NOT A SCREENSHOT
#
# A cursor is composited by the window server above every window, so it is
# usually absent from a screen capture altogether; where it is present it is a
# few antialiased pixels. `GetCursorInfo` returns the handle, and comparing a
# handle against `LoadCursorW(NULL, IDC_*)` is identity rather than
# resemblance. See the header of `probe-cursor.c`.
#
# THE INVARIANT THIS EXISTS TO DEFEND
#
# **One visible cursor, and it is a real Win32 cursor.** The failure that
# prompted the stage was the client's cursor surface being composited into the
# window as ordinary content, so the user saw two. This harness cannot see a
# second cursor drawn into a window — that is `wc-window.sh`'s pixel
# comparison — but it can see the half that is this stage's: that the Win32
# cursor is the client's, so nothing else has to be.
#
# WHAT WINE DOES AND DOES NOT PROVE. Wine is not Windows and has been wrong
# four times on this project (docs/waycompose-design.md §13.6), and a cursor is exactly
# the kind of thing it emulates rather than implements: `SetCursor`,
# `CreateIconIndirect` with a 32-bit ARGB bitmap, and `GetCursorInfo` are all
# Wine's own reimplementations. A pass here says the compositor installed the
# right handle and that the chain is self-consistent. Whether Windows *draws*
# an ARGB cursor with a premultiplied alpha channel correctly is a real-guest
# question; `scripts/wc-guest.sh` is the arbiter and §23.7 lists it.

set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

pass=0; fail=0
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
  "the compositor runs under Wine on this box" || exit 1

RUNDIR=$(mktemp -d /tmp/wc-cursor-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"

# Which Win32 a compositor log says its run was on -- docs/waycompose-design.md §67.
#
# The compositor prints `wc-compositor: win32 is <tag>` as the first line of
# every log, before it has done anything else, precisely so this is answerable
# without racing it. A harness that cannot answer it is a harness whose Windows
# assertions might be about Wine, or whose Wine assertions might be about the
# Linux build -- and this file used to hold two allowances that were granted to
# *every* leg because one of them needed it. That is what docs/waycompose-design.md §67.3 is about.
win32_of() {
  local line
  line=$(grep -o 'wc-compositor: win32 is [a-z]*' "$1" 2>/dev/null | head -1)
  case "$line" in
    *' wine')    echo wine ;;
    *' windows') echo windows ;;
    *' none')    echo none ;;
    *)           echo unknown ;;
  esac
}

find_socket() {
  local name=$1 node=""
  for _ in $(seq 1 250); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$name" 2>/dev/null | head -1)
    [ -n "$node" ] && { echo "$node"; return 0; }
    sleep 0.2
  done
  return 1
}

start_x() {
  Xvfb ":$1" -screen 0 1280x1024x24 -nolisten tcp >"$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$1"
  for _ in $(seq 1 100); do
    xwd -root -silent >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

# ---------------------------------------------------------------------------
# linux: is the protocol there, and can the proxy carry it
# ---------------------------------------------------------------------------

run_linux() {
  echo "== wp_cursor_shape_v1, advertised and proxied =="
  fresh "$LINUX_BIN/wc-compositor" "cargo build -p wc-compositor" || return
  command -v wayland-info >/dev/null || { note "SKIP (wayland-info not installed)"; return; }

  local dir="$RUNDIR/linux"
  mkdir -p "$dir/rt"; chmod 700 "$dir/rt"

  # --- direct: does the compositor advertise it at all --------------------
  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" --socket wl-cur-info --once --timeout 15 \
    >"$dir/direct-srv.log" 2>&1 &
  local srv=$!
  sleep 1
  ( cd "$dir" && XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-cur-info \
    timeout 10 wayland-info ) >"$dir/direct.log" 2>&1
  wait "$srv" 2>/dev/null

  # §67, the negative case, and it is worth an assertion for the same reason the
  # wine leg's is: this leg is *supposed* to have no Win32, and a `linux` run
  # that had somehow reached a Windows binary would pass every check below it
  # while proving nothing about the platform it names.
  case "$(win32_of "$dir/direct-srv.log")" in
    none)    ok "this leg is the Linux build and says so: \
$(grep -o 'win32 is .*' "$dir/direct-srv.log" | head -1)" ;;
    unknown) bad "the compositor never said which Win32 it was on (docs/waycompose-design.md §67)" ;;
    *)       bad "the linux leg found a Win32 -- it is not running the binary it thinks (docs/waycompose-design.md §67)" ;;
  esac

  if grep -q "interface: 'wp_cursor_shape_manager_v1'" "$dir/direct.log"; then
    ok "the global is advertised: $(grep -o "interface: 'wp_cursor_shape_manager_v1'.*" "$dir/direct.log" | head -1)"
  else
    bad "no wp_cursor_shape_manager_v1 in the registry"
    grep -o "interface: '[a-z_0-9]*'" "$dir/direct.log" | sed 's/^/        /' | head -30
  fi
  # Version 2, not 1. Version 2 is what adds `dnd_ask` and `all_resize` to the
  # shape enum, and a client that binds 2 against a compositor advertising 1
  # is a protocol error rather than a graceful downgrade.
  # `wayland-info` pads its columns, so the version is not a fixed number of
  # spaces after the comma.
  grep -Eq "wp_cursor_shape_manager_v1', +version: +2," "$dir/direct.log" \
    && ok "advertised at version 2" \
    || bad "wrong version: $(grep -Eo "wp_cursor_shape_manager_v1', +version: +[0-9]+" "$dir/direct.log")"

  # `repeat_info` while we are here. It is the other "we advertised a number we
  # invented" in this stage, and on this platform the answer is the documented
  # Windows default rather than the build host's X settings — see
  # `platform/headless.rs`.
  grep -q "keyboard repeat rate: 30" "$dir/direct.log" \
    && grep -q "keyboard repeat delay: 500" "$dir/direct.log" \
    && ok "headless repeat_info is Windows' documented default, 30/s after 500 ms" \
    || bad "repeat_info wrong: $(grep -o 'keyboard repeat.*' "$dir/direct.log" | tr '\n' ' ')"

  # --- through the proxy: can it walk the new interface -------------------
  #
  # This is the assertion that matters for a staging protocol. The proxy parses
  # the XML to know each message's argument shape, and an interface it has
  # never heard of is refused with a diagnosis rather than desynchronised
  # (docs/design.md §9.3) — so a run where `crates/wc-proxy/protocols/` was not
  # updated fails *here*, loudly, and not on the guest a week later.
  fresh_or_skip "$LINUX_BIN/wc-proxy" "SKIP the proxy leg (no wc-proxy built)"; case $? in 2) note "SKIP the proxy leg (no wc-proxy built)"; return ;; 1) return ;; esac
  mkdir -p "$dir/prt" "$dir/pools" "$dir/keymaps"; chmod 700 "$dir/prt"

  # `--shared-dir`, and it is required rather than tidy. Without somewhere to
  # put the keymap the compositor sends `wl_keyboard.keymap` as an in-band
  # descriptor the proxy cannot materialise, and the connection dies with
  # `argument 1 claims 35572 bytes, 0 available`. That is a **race** against the
  # registry burst, not a hard failure: `wayland-info` sometimes prints its
  # globals first and sometimes does not, so without this the leg passes about
  # half the time. Measured, on the run that first showed it.
  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" --socket wl-cur-up --once --timeout 25 \
    --shared-dir "$dir/keymaps" --shared-peer-prefix "$dir/keymaps/" \
    >"$dir/up-srv.log" 2>&1 &
  srv=$!
  sleep 1
  local node="$dir/rt/wl-cur-up"
  XDG_RUNTIME_DIR="$dir/prt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-cur-proxy --runtime-dir "$dir/prt" \
    --transport shared-file --pool-dir "$dir/pools" --peer-prefix "$dir/pools/" \
    --accept-timeout 20 --timeout 25 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 0.8
  ( cd "$dir" && XDG_RUNTIME_DIR="$dir/prt" WAYLAND_DISPLAY=wl-cur-proxy \
    timeout 15 wayland-info ) >"$dir/proxied.log" 2>&1
  kill "$srv" "$proxy" 2>/dev/null; wait "$srv" "$proxy" 2>/dev/null

  if grep -q "interface: 'wp_cursor_shape_manager_v1'" "$dir/proxied.log"; then
    ok "a libwayland client sees it through wc-proxy"
  else
    bad "the global did not survive the proxy"
    sed 's/^/        /' "$dir/proxied.log" | tail -20
    grep -i "unknown\|no message\|refus" "$dir/proxy.log" | sed 's/^/        /' | head -5
  fi
  # And nothing went wrong walking the session. `wayland-info` enumerates
  # without binding the globals it does not know, so this leg says the
  # *advertisement* crosses; that the proxy can walk cursor-shape's own
  # messages is `cursor_shape_is_walkable` in `crates/wc-proxy/src/spec.rs`,
  # which asserts the two opcodes and their argument kinds against the same XML
  # the compositor is generated from. Stating which claim comes from where
  # beats a harness that looks like it proved more than it did.
  if grep -qi "no known interface\|has no message\|unknown opcode" "$dir/proxy.log"; then
    bad "the proxy could not walk something in this session"
    grep -i "no known interface\|has no message\|unknown opcode" "$dir/proxy.log" \
      | sed 's/^/        /' | head -5
  else
    ok "the proxy walked the whole session with no unknown interface"
  fi
}

# ---------------------------------------------------------------------------
# wine: a real HCURSOR, read back out of Win32
# ---------------------------------------------------------------------------
#
# `weston-eventdemo` is the client, and it is the right one for a reason that
# is not convenience: Weston's toytoolkit loads an **XCursor theme** and calls
# `wl_pointer.set_cursor` with a themed surface — that is the 4190208-byte pool
# `wc-input.sh` had to size an arena for. So the pixels that arrive are a real
# cursor theme's, drawn by somebody else, at whatever size that theme ships,
# with a hotspot that is not 0,0. Nothing in this project chose any of it.
run_wine() {
  echo "== a client's cursor surface becomes a Win32 HCURSOR =="
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build -p wc-compositor --target x86_64-pc-windows-gnu" || return
  fresh "$LINUX_BIN/wc-proxy" "cargo build -p wc-proxy" || return
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }
  command -v Xvfb >/dev/null || { bad "Xvfb not on PATH"; return; }
  command -v weston-eventdemo >/dev/null || { note "SKIP (weston-eventdemo not installed)"; return; }
  command -v x86_64-w64-mingw32-gcc >/dev/null || { bad "no mingw gcc -- run inside nix develop"; return; }

  local dir="$RUNDIR/wine"
  mkdir -p "$dir/rt" "$dir/pools" "$dir/keymaps"
  chmod 700 "$dir/rt"

  # `-mwindows` for the reason wc-input.sh measured: a console-subsystem binary
  # inside a Wine virtual desktop gets a `WineConsoleClass` window that takes
  # the foreground, and then nothing injected reaches the compositor.
  if x86_64-w64-mingw32-gcc -O1 -Wall -mwindows -o "$dir/probe-cursor.exe" \
       "$ROOT/scripts/probe-cursor.c" -luser32 -lgdi32 2>"$dir/cc.log"; then
    ok "linked probe-cursor.exe ($(stat -c %s "$dir/probe-cursor.exe") bytes)"
  else
    bad "could not link probe-cursor.exe"; sed 's/^/        /' "$dir/cc.log"; return
  fi

  start_x 96 || { bad "could not start Xvfb"; return; }
  ok "Xvfb up on $DISPLAY"

  local sock="wl-cur" desktop="wc-cursor"
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  local keymaps_win pools_win
  keymaps_win=$(winepath -w "$dir/keymaps" 2>/dev/null || echo "$dir/keymaps")
  pools_win=$(winepath -w "$dir/pools" 2>/dev/null || echo "$dir/pools")

  sleep 3
  wine explorer "/desktop=$desktop,1280x1024" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 90 --layout us \
    --shared-dir "$keymaps_win" --shared-peer-prefix "$dir/keymaps/" \
    --log "$(winepath -w "$dir/srv.log" 2>/dev/null || echo "$dir/srv.log")" \
    >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node
  if ! node=$(find_socket "$sock"); then
    bad "the Windows compositor never created its socket node"
    sed 's/^/        /' "$dir/srv.log" 2>/dev/null | head -10
    kill "$srv" 2>/dev/null
    return
  fi
  ok "wc-compositor.exe is listening"

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --transport shared-file --pool-dir "$dir/pools" --peer-prefix "$pools_win\\" \
    --accept-timeout 30 --timeout 60 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 0.8

  ( cd "$dir" && XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout 60 stdbuf -oL -eL weston-eventdemo --log-motion --log-focus ) \
    >"$dir/cli.log" 2>&1 &
  local cli=$!

  # The probe runs inside the same virtual desktop: `FindWindowW` enumerates one
  # desktop, and `wine explorer /desktop=` makes a second one.
  #
  # `--dwell 1500` is the assertion that costs nothing and catches the most
  # likely regression. A cursor installed only from `WM_SETCURSOR` is correct
  # while the pointer moves and reverts as soon as it stops; reading it again
  # after a second and a half of stillness is how that shows up.
  local probe_log_win
  probe_log_win=$(winepath -w "$dir/probe.log" 2>/dev/null || echo "$dir/probe.log")
  wine explorer "/desktop=$desktop,1280x1024" \
    "$dir/probe-cursor.exe" --class WaycomposeToplevel --wait 30 \
    --settle 1500 --dwell 1500 --log "$probe_log_win" >"$dir/probe-loader.log" 2>&1
  local probe_rc=$?

  # **Wait for the probe's own last line, not for the launcher** — the same trap
  # `wc-damage.sh` documents at length. `wine explorer /desktop=NAME` attaches
  # to a desktop that already exists (the compositor made this one), so having
  # handed the program over the launcher returns at once and `$probe_rc` is the
  # launcher's, not the probe's.
  #
  # This harness got away with it until §67 for an uncomfortable reason: the
  # probe used to *die* at the first `GetCursorInfo`, about a second in, so
  # everything it was ever going to write was already written by the time the
  # log was read. Giving it a Wine path that lets it survive turned a crash into
  # a race, and the two failures look identical from here — a missing reading.
  # That is why the missing-reading cases below are failures rather than notes.
  local probe_done=1
  for _ in $(seq 1 120); do
    grep -q 'probe-cursor: done' "$dir/probe.log" 2>/dev/null && { probe_done=0; break; }
    sleep 0.5
  done
  [ "$probe_done" = 0 ] || note "probe-cursor.exe never printed its last line \
(launcher exit $probe_rc); the readings below are whatever it managed"
  sleep 1

  xwd -root -silent -out "$dir/s.xwd" 2>/dev/null \
    && magick "$dir/s.xwd" "$dir/s.png" 2>/dev/null

  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  wait "$cli" 2>/dev/null
  kill "$cli" 2>/dev/null

  [ -s "$dir/probe.log" ] || { bad "probe-cursor.exe wrote nothing (exit $probe_rc)"; \
    sed 's/^/        /' "$dir/probe-loader.log" | head -10; return; }
  sed 's/^/        /' "$dir/probe.log"

  # --- did the pointer actually move -------------------------------------
  #
  # docs/waycompose-design.md §57, applied to this probe. Every reading below
  # that names a position -- the client area, the dwell, both frame points -- is
  # a claim about where the pointer *is*, and `SendInput` can return 0 with
  # `ERROR_ACCESS_DENIED` without blocking or throwing. A refused move does not
  # make those readings fail; it makes them describe wherever the pointer
  # already was, and on this Xvfb they would still find the compositor's cursor
  # and pass. §57 caught `probe-scroll.c` measuring a browser nobody had
  # scrolled the same way.
  #
  # **The failure message names no cause, and docs/waycompose-design.md §63.3 is
  # why.** "The session is locked" is a real measured cause for this return
  # value and it is also the one that held a run up for a day while being about
  # a path the launcher does not travel. A harness that prints a cause it did
  # not establish hands the next reader a conclusion instead of a question.
  local refused
  refused=$(grep -o "probe-cursor: moves-refused [0-9]* of [0-9]*" "$dir/probe.log" | head -1 | tr -d '\r')
  case "$refused" in
    "")               bad "the probe never said whether its moves were accepted \
(docs/waycompose-design.md §57)" ;;
    *"refused 0 of "*) ok "every pointer move was accepted: ${refused#*moves-}" ;;
    *)                bad "the window server refused pointer moves, so the readings below are \
about wherever the pointer already was: ${refused#*moves-} -- which session and desktop is the probe \
on? (docs/waycompose-design.md §63.3)" ;;
  esac

  # --- which Win32 did this actually run on ------------------------------
  #
  # docs/waycompose-design.md §67, and it is first because every assertion below is a claim about a
  # platform. Asserted rather than noted: a `wine` leg that quietly ran the
  # Linux build, or that found a real Windows, has measured something -- but not
  # the thing its name says, and the readings would still all pass.
  local plat
  plat=$(win32_of "$dir/srv.log")
  case "$plat" in
    wine)
      ok "the compositor says which Win32 it is on: \
$(grep -o 'win32 is .*' "$dir/srv.log" | head -1)" ;;
    unknown)
      bad "the compositor never said which Win32 it was on -- this leg cannot be attributed (docs/waycompose-design.md §67)" ;;
    *)
      bad "the wine leg ran on '$plat', so nothing below is about Wine (docs/waycompose-design.md §67)" ;;
  esac

  # --- the compositor's own view -----------------------------------------
  #
  # First, because if this line is missing the Win32 assertions below are about
  # something that never left the Wayland thread.
  if grep -q "wc-compositor: cursor -> .* pixels, hotspot" "$dir/srv.log"; then
    ok "the compositor built a cursor from the client's surface: \
$(grep -o 'cursor -> .*' "$dir/srv.log" | tail -1)"
  else
    bad "the compositor never turned a cursor surface into pixels"
    grep -i "cursor" "$dir/srv.log" | sed 's/^/        /' | head -10
  fi

  # --- what Win32 is actually showing ------------------------------------
  #
  # Two independent readings, and both are asserted on because they fail in
  # different ways. `client-area-msg` is `WM_SETCURSOR` sent by hand plus
  # `GetCursor` on the attached queue: the compositor's window procedure and
  # the thread's cursor, with the window server not involved.
  # `client-area` is `GetCursorInfo`, which is what a *user* would see and
  # which under Wine goes through `XQueryPointer`.
  #
  # `tr -d '\r'` on every one of them, and it is load-bearing rather than
  # tidiness: `probe-cursor.exe` reopens its own `--log` with `freopen(…, "w")`,
  # which on the mingw runtime is a *text* stream, so every line it writes ends
  # `\r\n`. A `case` pattern anchored at the end — `*"returned 1")` — therefore
  # never matches, and the first version of the assertions below reported a
  # correct `returned 1` as a failure. The pre-existing patterns here are all
  # `*…*` and were immune, which is exactly why nobody had hit it.
  local msg_ret msg_line frame_ret client_line still_line edge_line
  msg_ret=$(grep "client-area-msg WM_SETCURSOR" "$dir/probe.log" | head -1 | tr -d '\r')
  msg_line=$(grep "probe-cursor: client-area-msg cursor=" "$dir/probe.log" | head -1 | tr -d '\r')
  frame_ret=$(grep "frame-msg WM_SETCURSOR" "$dir/probe.log" | head -1 | tr -d '\r')
  client_line=$(grep "probe-cursor: client-area cursor=" "$dir/probe.log" | head -1 | tr -d '\r')
  still_line=$(grep "probe-cursor: client-area-still cursor=" "$dir/probe.log" | head -1 | tr -d '\r')
  edge_line=$(grep "probe-cursor: frame-right-edge cursor=" "$dir/probe.log" | head -1 | tr -d '\r')

  # The hit-test guard, sent by hand, and **asserted on now — on both legs**.
  #
  # §23.7 item 2 recorded this as a Wine limitation: a cross-process
  # `SendMessageW(WM_SETCURSOR)` returning 0 for *both* hit-test codes, with
  # "the cursor on the queue does not change for `HTRIGHT` either" as the tell
  # that the message never reached the window procedure. It listed the two
  # sharp assertions below as things only a real guest could make.
  #
  # **That was never Wine.** §67.5: the harness was killing the compositor one
  # second after the launcher returned, while this probe was still in its
  # `--settle` sleep, so both `SendMessageW`s were being sent to a window that
  # had already been destroyed — and `SendMessageW` to a dead window returns 0.
  # With the wait above in place the same wine-11.14 returns `1` for `HTCLIENT`
  # and `0` for `HTRIGHT`, and the frame's cursor handle differs from the client
  # area's, which is the tell §23.7 was looking for.
  #
  # So the allowance is withdrawn rather than made conditional. That is the
  # better outcome by some distance: a Wine path here would have frozen a
  # harness bug into the design as a property of Wine, and the guest leg was
  # paying for it too — the guard was, in §23.7's own words, "defended by
  # reading and not by measurement" on *both* platforms.
  case "$msg_ret" in
    *"returned 1") ok "WM_SETCURSOR(HTCLIENT) returned 1: the compositor claimed the client area" ;;
    "")            bad "no WM_SETCURSOR(HTCLIENT) reading at all" ;;
    *)             bad "WM_SETCURSOR(HTCLIENT) returned ${msg_ret#*returned }, so the client's \
cursor is not installed over the client area" ;;
  esac
  case "$frame_ret" in
    *"returned 0") ok "WM_SETCURSOR(HTRIGHT) returned 0: the frame is still Windows'" ;;
    "")            bad "no WM_SETCURSOR(HTRIGHT) reading at all" ;;
    *)             bad "WM_SETCURSOR(HTRIGHT) returned ${frame_ret#*returned }, so the \
compositor claimed the resize border too" ;;
  esac

  # This one *is* asserted on. `GetCursor` reads the cursor of the calling
  # thread's input queue, and `AttachThreadInput` put this thread on the
  # compositor's — so a handle that matches none of the sixteen stock cursors
  # is the compositor's own `CreateIconIndirect` handle and nothing else.
  #
  # `GetIconInfo` on it fails with 1402 `ERROR_INVALID_CURSOR_HANDLE` and that
  # is correct rather than a fault: a cursor built by `CreateIconIndirect`
  # belongs to the process that built it. What it is made of is therefore
  # reported by the compositor itself — see the round-trip assertion below.
  case "$msg_line" in
    *"cursor=custom"*)
      ok "the compositor's input queue holds a non-stock cursor: ${msg_line#*cursor=}" ;;
    *"cursor=stock"*)
      bad "the compositor's queue still holds a system cursor: ${msg_line#*cursor=}" ;;
    *)
      bad "no reading of the compositor's queue: ${msg_line:-missing}" ;;
  esac

  # What that handle is made of, from the only process that can ask.
  local rt
  rt=$(grep -o "cursor round-trip: .*" "$dir/srv.log" | head -1)
  case "$rt" in
    "")            bad "the compositor never read its own HCURSOR back" ;;
    *"fIcon=0"*)   ok "Win32 accepted it as a cursor, not an icon: $rt" ;;
    *)             bad "the handle came back as an icon: $rt" ;;
  esac
  # 32 bits per pixel in the colour bitmap. Anything else means the alpha was
  # dropped, and a cursor without alpha is a black rectangle with a shape in it.
  case "$rt" in
    *"at 32 bpp"*) ok "the colour bitmap is 32-bit, so the alpha channel survived" ;;
    "")            : ;;
    *)             bad "the alpha channel did not survive: $rt" ;;
  esac
  # The hotspot the client asked for, as Win32 read it back. Compared against
  # the compositor's own `cursor ->` line rather than against a constant: the
  # number belongs to whatever XCursor theme is installed, and hard-coding it
  # would make this harness fail on a machine with a different theme.
  local asked got
  asked=$(grep -o "cursor -> .* hotspot [0-9]*,[0-9]*" "$dir/srv.log" | tail -1 | grep -o '[0-9]*,[0-9]*$')
  got=$(echo "$rt" | grep -o 'hotspot=[0-9]*,[0-9]*' | cut -d= -f2)
  if [ -n "$asked" ] && [ "$asked" = "$got" ]; then
    ok "the hotspot survived unchanged: the client asked for $asked and ICONINFO holds $got"
  elif [ -n "$rt" ]; then
    bad "the hotspot changed: asked $asked, ICONINFO holds $got"
  fi

  # `GetCursorInfo` — what a *user* would see, as against the queue reading
  # above. Both legs make this reading and both assert on it.
  #
  # Before §67 the probe *died* here under Wine and this harness printed a note
  # and carried on, so the Wine leg silently lost this reading and the two below
  # it: three assertions that looked like they were being made. §67.5 has the
  # cause and it was this harness, not Wine. An empty reading is therefore a
  # failure on either platform now, because on either platform something has to
  # have gone wrong to produce one.
  case "$client_line" in
    *"cursor=custom"*)
      ok "and GetCursorInfo agrees: ${client_line#*cursor=}" ;;
    "")
      bad "GetCursorInfo produced nothing -- did probe-cursor.exe die? \
(docs/waycompose-design.md §67.5)" ;;
    *)
      bad "over the client area Win32 is showing ${client_line#*cursor=}" ;;
  esac
  case "$still_line" in
    *"cursor=custom"*) ok "and it is still there after 1.5 s of no mouse movement" ;;
    "")                bad "no still reading -- the probe stopped before it (docs/waycompose-design.md §67.5)" ;;
    *)                 bad "the cursor reverted once the pointer stopped moving: $still_line" ;;
  esac

  # The other half of "exactly one cursor": the system frame keeps its own.
  # The `WM_SETCURSOR(HTRIGHT)` assertion above is the sturdy version of this;
  # this one is the same claim measured by parking the *real* pointer on the
  # border, which is why it is worth having twice.
  #
  # **THIS IS THE ONE PLACE IN THIS FILE WHERE WINE IS GENUINELY BROKEN** and
  # therefore the one place with a Wine branch (§67.4). `probe-cursor.c` names a
  # cursor by comparing its handle against `LoadCursorW(NULL, IDC_*)`, which is
  # proof of identity on Windows because stock cursors are shared per-session
  # handles. Under Wine they are **per process**: measured on wine-11.14, the
  # sixteen handles in one process are 0x1004e … 0x1006c, a dense run from a
  # per-process base that a second process allocates from again. The probe reads
  # the cursor off the *compositor's* queue, so the two numbers come from two
  # namespaces and comparing them can both miss a match and manufacture one.
  # Everything therefore reads `cursor=custom` under Wine whatever it is, and
  # the probe says so with `stock-identity=unavailable` rather than letting the
  # word `custom` be mistaken for a measurement.
  #
  # The substitute is the weaker test the guard is actually about — the frame's
  # handle must **differ** from the client area's — and it is weaker in a
  # specific, stateable way: it proves the compositor did not claim the border,
  # and says nothing about which system cursor is there. Windows keeps the
  # assertion by name, where the name means something.
  local edge_h client_h
  edge_h=$(echo "$edge_line" | grep -o 'handle=[0-9a-fA-F]*')
  client_h=$(echo "$msg_line" | grep -o 'handle=[0-9a-fA-F]*')
  if [ -z "$edge_line" ]; then
    bad "no border reading -- the probe stopped before it (docs/waycompose-design.md §67.4)"
  elif grep -q "stock-identity=unavailable" "$dir/probe.log"; then
    case "$edge_line" in
      *"cursor=stock"*)
        ok "the window's right border shows ${edge_line#*cursor=}" ;;
      *)
        if [ -n "$edge_h" ] && [ "$edge_h" != "$client_h" ]; then
          ok "the border's cursor is not the client's ($edge_h vs $client_h); \
Wine cannot say which system cursor it is (docs/waycompose-design.md §67.4)"
        else
          bad "the client's cursor leaked onto the window frame: $edge_line"
        fi ;;
    esac
  else
    case "$edge_line" in
      *"cursor=stock IDC_SIZEWE"*)
        ok "the window's right border still shows the system resize arrow" ;;
      *"cursor=stock"*)
        note "the border shows ${edge_line#*cursor=} -- a stock cursor, so the guard held" ;;
      *)
        bad "the client's cursor leaked onto the window frame: $edge_line" ;;
    esac
  fi
}

case "${1:-all}" in
  linux) run_linux ;;
  wine)  run_wine ;;
  all)   run_linux; echo; run_wine ;;
  *) echo "usage: $0 [linux|wine|all]"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ -n "$KEEP" ] && echo "artefacts kept in $RUNDIR"
[ "$fail" -eq 0 ]
