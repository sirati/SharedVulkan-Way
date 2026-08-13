#!/usr/bin/env bash
# Prove that the stage-2 smithay compositor reaches `xdg_toplevel`.
#
#   scripts/wc-xdg.sh linux     compositor as a Linux binary
#   scripts/wc-xdg.sh wine      compositor as a Windows binary under Wine
#   scripts/wc-xdg.sh all       both
#
# Run from inside `nix develop`.
#
# WHAT IS BEING CLAIMED, AND WHY THIS CLIENT
#
# The claim is "smithay works as a Windows binary", and the way to be wrong
# about it is to test with something that shares our assumptions. So the client
# is `weston-multi-resource`: a C program, using libwayland-client and the
# upstream `xdg-shell` XML, from the Weston project, which has never heard of
# waycompose. It cannot have a bug that is symmetric with ours.
#
# It is a step up from stage 1's `wayland-info` in exactly the way this stage
# needs. `wayland-info` binds `wl_registry` and enumerates — it proves the
# handshake and nothing past it. This client binds `wl_compositor` and
# `xdg_wm_base`, creates a `wl_surface`, wraps it in an `xdg_surface`, requests
# an `xdg_toplevel`, commits, and waits for our `configure`. Every one of those
# crosses smithay's own protocol implementation rather than ours.
#
# WHY THIS CLIENT AND NOT `weston-flower`, WHICH IS THE OBVIOUS CHOICE
#
# Because of request *order*, and the reason is documented rather than
# incidental. `weston-flower` is a toytoolkit client, and toytoolkit allocates
# its `wl_shm` pool during `display_create()` — before any surface exists. A
# `wl_shm.create_pool` carries an `h` argument, and on Windows an `h` is encoded
# in-band as a length-prefixed descriptor rather than passed out-of-band by
# `SCM_RIGHTS` (docs/design.md §2.3). A stock libwayland client sends the fd out
# of band; Wine's AF_UNIX carries no ancillary data; so the descriptor is simply
# absent from the byte stream and the server's parser desynchronises at exactly
# that request.
#
# That is docs/design.md §2.4 — "the Windows byte stream is not libwayland's
# byte stream" — arriving on schedule. Measured: against the Windows binary,
# `weston-flower` sends `create_surface`, `get_xdg_surface`, `get_toplevel` and
# `commit`, and the server sees none of them, because they follow the
# `create_pool` that broke framing.
#
# `weston-multi-resource` is a simple-client rather than a toytoolkit one, and
# it creates the surface and the toplevel *first*, allocating shm only after the
# first `configure`. So the whole xdg-shell exchange completes before the first
# `h` argument is reached. This is not the test being weakened to fit — it is
# the same protocol, driven by the same library, in an order that stays inside
# what this stage claims to support. Both clients are run on Linux, where
# `SCM_RIGHTS` works and neither has the problem, and `weston-flower`'s Linux
# pass is what shows the ordering is the only difference.
#
# The four assertions below are ordered so that a failure localises:
#
#   1. the compositor bound a socket             — transport
#   2. `wl_surface created`                      — wl_compositor, smithay's
#   3. `xdg_toplevel created`                    — xdg-shell, smithay's
#   4. `wl_surface commit`                       — role, pending state, commit
#
# and (3) is the milestone this stage was defined by.
#
# WHAT WINE DOES AND DOES NOT PROVE. Unchanged from stage 1, and still the most
# important sentence in this file: Wine is not Windows. A pass says our code
# paths are self-consistent and structurally possible. It says nothing about
# `afunix.sys`, about wepoll under load, or about how Windows schedules any of
# it. Those need the guest.
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

# STAGE 42. `fresh` / `fresh_or_skip`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"

RUNDIR=$(mktemp -d /tmp/wc-xdg-XXXXXX)
chmod 700 "$RUNDIR"
trap 'rm -rf "$RUNDIR"' EXIT

# The client used everywhere. See the header for why it is this one and not
# `weston-flower`.
CLIENT=weston-multi-resource
command -v "$CLIENT" >/dev/null || CLIENT=""
# Run on Linux only, as the control that shows the ordering above is the sole
# difference between the two.
CLIENT_TOY=weston-flower
command -v "$CLIENT_TOY" >/dev/null || CLIENT_TOY=""

# Assert on the compositor's own log. The compositor prints one greppable line
# per protocol milestone precisely so that this can be a sequence check rather
# than an exit-code check: a compositor that accepted the client and then did
# nothing would still exit 0.
check_log() {
  local log="$1" label="$2"
  if grep -q "wl_surface created" "$log"; then
    ok "$label: the client created a wl_surface"
  else
    bad "$label: no wl_surface"
  fi
  if grep -q "xdg_toplevel created" "$log"; then
    ok "$label: the client created an xdg_toplevel  <-- the stage-2 milestone"
  else
    bad "$label: no xdg_toplevel"
  fi
  if grep -q "sent xdg_toplevel configure" "$log"; then
    ok "$label: the compositor sent a configure"
  else
    bad "$label: no configure sent"
  fi
  if grep -q "wl_surface commit" "$log"; then
    ok "$label: the client committed the surface"
  else
    bad "$label: no commit"
  fi
}

run_linux() {
  echo "== linux: compositor as a Linux binary =="
  fresh "$LINUX_BIN/wc-compositor" "cargo build -p wc-compositor" || return
  [ -n "$CLIENT" ] || { echo "  SKIP  weston-flower not on PATH"; return; }

  XDG_RUNTIME_DIR="$RUNDIR" "$LINUX_BIN/wc-compositor" \
    --socket wayland-xdg --timeout 25 >"$RUNDIR/lin.log" 2>&1 &
  local srv=$!
  sleep 0.5

  XDG_RUNTIME_DIR="$RUNDIR" WAYLAND_DISPLAY=wayland-xdg timeout 5 "$CLIENT" \
    >"$RUNDIR/lin-client.log" 2>&1
  sleep 0.5
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

  check_log "$RUNDIR/lin.log" "linux"
  echo "  --- compositor log ---"
  sed 's/^/        /' "$RUNDIR/lin.log"

  # The toytoolkit control. On Linux `SCM_RIGHTS` works, so the request order
  # that defeats the Windows build is harmless here — and a pass proves the
  # Windows failure is about the fd encoding rather than about anything in the
  # xdg-shell path.
  [ -n "$CLIENT_TOY" ] || return
  echo
  echo "  -- control: $CLIENT_TOY (toytoolkit; allocates shm before any surface) --"
  XDG_RUNTIME_DIR="$RUNDIR" "$LINUX_BIN/wc-compositor" \
    --socket wayland-xdg-toy --timeout 25 >"$RUNDIR/lintoy.log" 2>&1 &
  local srv2=$!
  sleep 0.5
  XDG_RUNTIME_DIR="$RUNDIR" WAYLAND_DISPLAY=wayland-xdg-toy timeout 5 "$CLIENT_TOY" \
    >"$RUNDIR/lintoy-client.log" 2>&1
  sleep 0.5
  kill "$srv2" 2>/dev/null; wait "$srv2" 2>/dev/null
  if grep -q "xdg_toplevel created" "$RUNDIR/lintoy.log"; then
    ok "linux: $CLIENT_TOY also reached xdg_toplevel"
  else
    bad "linux: $CLIENT_TOY did not reach xdg_toplevel"
    sed 's/^/        /' "$RUNDIR/lintoy.log"
  fi
}

run_wine() {
  echo "== wine: compositor as a Windows binary =="
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build -p wc-compositor --target x86_64-pc-windows-gnu" || return
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }
  [ -n "$CLIENT" ] || { echo "  SKIP  weston-flower not on PATH"; return; }

  # As in stage 1: do not try to override LOCALAPPDATA from the Unix side. Wine
  # builds the Windows environment block itself and its value wins. Discover the
  # node instead of predicting it, which also checks the path the code chose.
  local sockname=wayland-xdgwin
  local cdrive="$WINEPREFIX/drive_c"
  rm -f "$cdrive"/users/*/AppData/Local/waycompose/"$sockname"* 2>/dev/null

  wine "$WIN_BIN/wc-compositor.exe" --socket "$sockname" --timeout 40 \
    >"$RUNDIR/win.log" 2>&1 &
  local srv=$!

  local node=""
  for _ in $(seq 1 200); do
    node=$(find "$cdrive" -type s -name "$sockname" 2>/dev/null | head -1)
    [ -n "$node" ] && break
    sleep 0.2
  done
  if [ -z "$node" ]; then
    bad "the Windows compositor never created its socket node"
    sed 's/^/        /' "$RUNDIR/win.log"
    kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
    return
  fi
  # `-type s` is doing real work here: it shows the node is a genuine host
  # Unix-domain socket rather than a placeholder file, which is what lets a
  # native Linux client reach a Windows binary at all.
  ok "wine: the Windows binary bound an AF_UNIX socket; host node: ${node#"$cdrive"}"

  # A native Linux C client, straight into the .exe.
  mkdir -p "$RUNDIR/xdg"
  chmod 700 "$RUNDIR/xdg"
  ln -sf "$node" "$RUNDIR/xdg/$sockname" 2>/dev/null
  XDG_RUNTIME_DIR="$RUNDIR/xdg" WAYLAND_DISPLAY="$sockname" timeout 8 "$CLIENT" \
    >"$RUNDIR/win-client.log" 2>&1
  sleep 1
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

  check_log "$RUNDIR/win.log" "wine"
  echo "  --- compositor log ---"
  sed 's/^/        /' "$RUNDIR/win.log"
}

case "$MODE" in
  linux) run_linux ;;
  wine)  run_wine ;;
  all)   run_linux; echo; run_wine ;;
  *) echo "usage: $0 [linux|wine|all]"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
