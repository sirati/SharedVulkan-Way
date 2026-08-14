#!/usr/bin/env bash
# Does anything ever stop existing?
#
#   scripts/wc-teardown.sh linux    the bookkeeping, against the Linux build
#   scripts/wc-teardown.sh wine     the bookkeeping *and* Windows' own answer
#   scripts/wc-teardown.sh all      both
#
# Run from inside `nix develop`.
#
# WHAT THIS IS FOR
#
# Task #35, and it is a missing **category** rather than a missing check. Every
# gate in this tree asserts that something appeared: a window was created, an
# `HWND` exists, pixels arrived, a title was set, an icon reached the taskbar.
# Nothing asserted that anything went away. A compositor that made a window per
# client and destroyed none of them would pass every one of them, for a whole
# session, while leaking an `HWND` per client — and unlike a leaked idle
# inhibitor (§21, a display that never sleeps) a leaked window handle is
# invisible: nothing in a log, nothing on screen, nothing in a screenshot.
#
# WHAT IT MEASURED, AND THE ANSWER IS "NO BUG"
#
# The teardown path works. A client killed with `SIGTERM` — no `destroy`
# request, no clean disconnect — still produces `window 1 destroyed` before
# `client disconnected`, on both platforms. So this harness is not a bug fix; it
# is the assertion that would have caught it, and the reason it exists is that
# nothing would have.
#
# THE TWO LEGS ARE NOT THE SAME CLAIM
#
# `linux` checks the compositor's own arithmetic: created against destroyed,
# and the map emptied. That is worth having and it is self-reported.
#
# `wine` additionally asks **Windows** how many `WaycomposeToplevel` windows
# this process still owns, with `EnumWindows` filtered by process id. That is
# the half that can catch a teardown bug the compositor cannot see: a
# `DestroyWindow` that returned false, a window owned by the wrong thread, a
# handle already replaced. Each of those leaves the arithmetic balanced, the map
# empty, and a window on the desktop, and only somebody outside the process can
# say so.
#
# WHY EVERY LEG LETS THE COMPOSITOR TIME OUT INSTEAD OF KILLING IT
#
# The report is printed on the way out, from the thread that owns the windows.
# A compositor killed with a signal prints nothing, and prints nothing about
# teardown for the good reason that it did not do any. Every other harness in
# this tree stops its compositor with `kill`, which is why none of them can
# carry this assertion without being restructured — and why this is a harness of
# its own rather than four lines added to five existing ones.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

pass=0; fail=0; skip=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
non() { echo "  SKIP  $*"; skip=$((skip + 1)); }
note(){ echo "  ....  $*"; }

. "$(dirname "$0")/wc-binaries.sh"
. "$(dirname "$0")/wc-lifetime.sh"
# §51. No hypervisor in either leg: both the Linux build and the Wine one run
# on this box. Nothing here is a claim about how long teardown takes, only about
# whether it happened.
. "$(dirname "$0")/wc-topology.sh"
wc_topology_declare same-box \
  "the compositor, the client and the window manager are all on this box" || exit 1

RUNDIR=$(mktemp -d /tmp/wc-teardown-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"
WC_X_DISPLAY=${WC_X_DISPLAY:-71}
have() { command -v "$1" >/dev/null 2>&1; }

# The client's life and the compositor's, in seconds. The client must finish
# **first** and the compositor must then reach its idle timeout, or the run
# measures a compositor that stopped while somebody was still using it — which
# is a legitimate state with windows standing in it, and not a leak. That
# ordering is the whole design of the timings below.
CLIENT_SECS=${WC_CLIENT_SECS:-5}
IDLE_SECS=${WC_IDLE_SECS:-6}

# ---------------------------------------------------------------------------
# linux: the arithmetic
# ---------------------------------------------------------------------------
block_linux() {
  echo "== linux: every window a client made is gone when the client is =="
  fresh_or_skip "$LINUX_BIN/wc-compositor" "cargo build -p wc-compositor"
  case $? in 2) non "no wc-compositor; cargo build"; return ;; 1) return ;; esac
  have weston-terminal || { non "weston-terminal is not installed"; return; }

  local dir="$RUNDIR/linux"
  mkdir -p "$dir"; chmod 700 "$dir"

  XDG_RUNTIME_DIR="$dir" "$LINUX_BIN/wc-compositor" \
    --socket wl-td --timeout "$IDLE_SECS" --simulate-wm >"$dir/srv.log" 2>&1 &
  local srv=$!
  sleep 2

  XDG_RUNTIME_DIR="$dir" WAYLAND_DISPLAY=wl-td \
    timeout "$CLIENT_SECS" weston-terminal >"$dir/cli.log" 2>&1
  ok "the client ran and exited, leaving the compositor up"

  # **Waited for, not killed.** The exit report is the measurement.
  wait "$srv" 2>/dev/null

  grep -q "window [0-9]* created" "$dir/srv.log" \
    && ok "a window was created for it" \
    || { bad "no window was ever created; this run proves nothing about teardown"
         sed 's/^/        /' "$dir/srv.log" | tail -8; return; }

  grep -q "disconnected" "$dir/srv.log" \
    && ok "the compositor saw the client go" \
    || note "no disconnect line; the client may have outlived the compositor"

  wc_assert_no_window_leaks "$dir/srv.log" linux
}

# ---------------------------------------------------------------------------
# wine: and Windows' own answer
# ---------------------------------------------------------------------------
block_wine() {
  echo "== wine: the same, plus what Windows says is still on the desktop =="
  have wine || { non "no wine"; return; }
  have Xvfb || { non "no Xvfb"; return; }
  fresh_or_skip "$WIN_BIN/wc-compositor.exe" "cargo build --target x86_64-pc-windows-gnu"
  case $? in 2) non "no wc-compositor.exe"; return ;; 1) return ;; esac
  fresh_or_skip "$WIN_BIN/wc-shmclient.exe" "cargo build --target x86_64-pc-windows-gnu"
  case $? in 2) non "no wc-shmclient.exe"; return ;; 1) return ;; esac

  local dir="$RUNDIR/wine"
  mkdir -p "$dir/xdg"; chmod 700 "$dir/xdg"

  Xvfb ":$WC_X_DISPLAY" -screen 0 1024x768x24 -nolisten tcp >"$dir/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$WC_X_DISPLAY"
  local up=""
  for _ in $(seq 1 100); do
    xwd -root -silent >/dev/null 2>&1 && { up=1; break; }
    sleep 0.1
  done
  [ -n "$up" ] || { bad "Xvfb never came up"; return; }

  # §8.9: a desktop name never used before, and a gap.
  sleep 3
  local desk="wc-teardown-$$-$RANDOM" sock="wl-td-$$"
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  # `--log`, not a redirect: `wine explorer /desktop=` gives its child a console
  # *inside* the desktop, so a redirect on the outside captures the loader and
  # nothing else. wc-window.sh's header records the run this cost.
  wine explorer "/desktop=$desk,1024x768" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout "$IDLE_SECS" \
    --log "$(winepath -w "$dir/srv.log" 2>/dev/null || echo "$dir/srv.log")" \
    >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node=""
  for _ in $(seq 1 300); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$sock" 2>/dev/null | head -1)
    [ -n "$node" ] && break
    sleep 0.2
  done
  [ -n "$node" ] || { bad "the compositor never bound its socket"; kill "$srv" 2>/dev/null; return; }
  ok "wc-compositor.exe is listening"

  ln -sf "$node" "$dir/xdg/$sock"
  # Frames chosen so the client finishes well before the compositor's idle
  # timeout: it has to be the one that goes first.
  WAYLAND_DISPLAY="$sock" timeout 30 wine "$WIN_BIN/wc-shmclient.exe" \
    --frames "${WC_FRAMES:-120}" --width 320 --height 240 \
    --title "waycompose teardown" >"$dir/cli.log" 2>&1
  ok "the client ran and exited, leaving the compositor up"

  wait "$srv" 2>/dev/null
  sleep 1

  grep -q "window [0-9]* created" "$dir/srv.log" \
    && ok "a window was created for it" \
    || { bad "no window was created; this run proves nothing about teardown"
         sed 's/^/        /' "$dir/srv.log" | tail -10; return; }

  wc_assert_no_window_leaks "$dir/srv.log" wine

  # The line itself, quoted, because the numbers are the finding and a reader
  # should not have to take the verdict's word for them.
  local line
  line=$(grep -o 'windows at exit:.*' "$dir/srv.log" | tail -1)
  [ -n "$line" ] && note "wine: $line"
}

case "${1:-all}" in
  linux) block_linux ;;
  wine)  block_wine ;;
  all)   block_linux; echo; block_wine ;;
  *) echo "usage: $0 [linux|wine|all]"; exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
