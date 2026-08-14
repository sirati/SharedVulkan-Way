#!/usr/bin/env bash
# Prove that the stage-1 server completes a wl_display handshake.
#
#   scripts/wc-handshake.sh linux     server and clients as Linux binaries
#   scripts/wc-handshake.sh wine      server as a Windows binary under Wine
#   scripts/wc-handshake.sh all       both
#
# Run from inside `nix develop`.
#
# WHY THREE CLIENTS, AND WHY THAT MATTERS
#
# The claim being tested is "the server completes a handshake". A matched pair
# of our own binaries is weak evidence for that, because a symmetric bug in our
# own wire encoding would make client and server agree with each other and pass.
# So the harness uses three clients of decreasing relatedness:
#
#   1. wc-probe          our own, but sharing no code with the server beyond the
#                        socket — every byte is written out by hand from the
#                        protocol spec in its source.
#   2. wayland-info      libwayland-client, written in C by other people, which
#                        has never heard of this project. Linux only: there is
#                        no third-party Wayland client for Windows and there
#                        will not be one until somebody ports a toolkit.
#   3. wayland-info -> the *Windows* server
#                        the interesting one. Wine backs a Windows AF_UNIX
#                        socket with a real host Unix socket node, so a native
#                        Linux client can connect straight to a server running
#                        as a Windows binary. This is the strongest evidence
#                        available without the real machine: a C client that
#                        knows nothing about us, talking to our mingw-built
#                        .exe, over the real wire format.
#
# WHAT WINE DOES AND DOES NOT PROVE. Wine is not Windows. A pass here says our
# code paths are self-consistent and that nothing in them is structurally
# impossible; it says nothing about afunix.sys, about `SIO_AF_UNIX_GETPEERPID`
# (which Wine does not implement at all), or about wepoll's real behaviour.
# Those need the guest.
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

ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }

# STAGE 42. `fresh` / `fresh_or_skip`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"

# A run directory of our own, so a stale socket from a previous run cannot be
# mistaken for a live server and so nothing lands in the user's real
# XDG_RUNTIME_DIR.
RUNDIR=$(mktemp -d /tmp/wc-handshake-XXXXXX)
trap 'rm -rf "$RUNDIR"' EXIT

run_linux() {
  echo "== linux: server and clients as Linux binaries =="
  fresh "$LINUX_BIN/wc-server" "cargo build -p wc-server" || return

  # --- our own probe -------------------------------------------------------
  XDG_RUNTIME_DIR="$RUNDIR" "$LINUX_BIN/wc-server" --socket wayland-test --timeout 20 >"$RUNDIR/srv1.log" 2>&1 &
  local srv=$!
  sleep 0.5
  XDG_RUNTIME_DIR="$RUNDIR" "$LINUX_BIN/wc-probe" "$RUNDIR/wayland-test" >"$RUNDIR/probe.log" 2>&1
  local rc=$?
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  if [ $rc -eq 0 ] && grep -q "PASS" "$RUNDIR/probe.log"; then
    ok "wc-probe completed the handshake ($(grep -c 'global ' "$RUNDIR/probe.log") globals)"
  else
    bad "wc-probe rc=$rc"; sed 's/^/        /' "$RUNDIR/probe.log"
  fi

  # --- a real third-party client -------------------------------------------
  if ! command -v wayland-info >/dev/null; then
    echo "  SKIP  wayland-info not on PATH"
    return
  fi
  XDG_RUNTIME_DIR="$RUNDIR" "$LINUX_BIN/wc-server" --socket wayland-test2 --timeout 20 >"$RUNDIR/srv2.log" 2>&1 &
  srv=$!
  sleep 0.5
  XDG_RUNTIME_DIR="$RUNDIR" WAYLAND_DISPLAY=wayland-test2 wayland-info >"$RUNDIR/info.log" 2>&1
  rc=$?
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  if [ $rc -eq 0 ] && grep -q "wl_compositor" "$RUNDIR/info.log" && grep -q "wl_shm" "$RUNDIR/info.log"; then
    ok "wayland-info (libwayland C) enumerated our globals"
    grep -E "interface:" "$RUNDIR/info.log" | sed 's/^/        /'
  else
    bad "wayland-info rc=$rc"; sed 's/^/        /' "$RUNDIR/info.log"
  fi
}

run_wine() {
  echo "== wine: server as a Windows binary =="
  fresh "$WIN_BIN/wc-server.exe" "cargo build -p wc-server --target x86_64-pc-windows-gnu" || return
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }

  # Windows AF_UNIX binds to a real filesystem path, and our `socket_dir()`
  # puts the socket under `%LOCALAPPDATA%\waycompose`. Do not try to override
  # LOCALAPPDATA from the Unix environment: Wine builds the Windows environment
  # block itself and its own value wins, so the socket lands under the prefix's
  # user profile regardless. Discover the node instead of predicting it — that
  # is also a better test, since it checks the path the code really chose.
  local sockname=wayland-win
  local cdrive="$WINEPREFIX/drive_c"
  rm -f "$cdrive"/users/*/AppData/Local/waycompose/"$sockname"* 2>/dev/null

  wine "$WIN_BIN/wc-server.exe" --socket "$sockname" --timeout 25 >"$RUNDIR/wsrv.log" 2>&1 &
  local srv=$!
  # Wine's first run may initialise the prefix; give it room.
  local node=""
  for _ in $(seq 1 150); do
    node=$(find "$cdrive" -type s -name "$sockname" 2>/dev/null | head -1)
    [ -n "$node" ] && break
    sleep 0.2
  done
  if [ -z "$node" ]; then
    bad "the Windows server never created its socket node"
    sed 's/^/        /' "$RUNDIR/wsrv.log"
    kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
    return
  fi
  # `-type s` is doing real work in that find: it is what shows the node is a
  # genuine Unix-domain socket on the host filesystem and not a placeholder
  # file. That is how a Linux client can reach it at all.
  ok "the Windows binary bound an AF_UNIX socket; host node: ${node#"$cdrive"}"

  # --- our probe, also as a Windows binary ---------------------------------
  if [ -f "$WIN_BIN/wc-probe.exe" ]; then
    WAYLAND_DISPLAY="$sockname" wine "$WIN_BIN/wc-probe.exe" >"$RUNDIR/wprobe.log" 2>&1
    if grep -q "PASS" "$RUNDIR/wprobe.log"; then
      ok "wc-probe.exe completed the handshake against wc-server.exe (both Windows binaries)"
      grep -E "global |wl_callback.done" "$RUNDIR/wprobe.log" | sed 's/^/        /'
    else
      bad "wc-probe.exe did not complete the handshake"
      sed 's/^/        /' "$RUNDIR/wprobe.log"
    fi
  fi

  # --- the fd shim, end to end --------------------------------------------
  # The probe creates a real section object, writes two markers into it, and
  # sends only the *descriptor*. If the server prints the markers back it can
  # only have opened the object by name — which is the whole claim of the shim.
  if [ -f "$WIN_BIN/wc-probe.exe" ]; then
    WAYLAND_DISPLAY="$sockname" wine "$WIN_BIN/wc-probe.exe" --shm >"$RUNDIR/wshm.log" 2>&1
    sleep 0.5
    if grep -q 'SHM MAPPED' "$RUNDIR/wsrv.log" \
       && grep -q 'head="WAYCOMPOSE-SHM-OK"' "$RUNDIR/wsrv.log" \
       && grep -q 'tail="tail"' "$RUNDIR/wsrv.log"; then
      ok "an \`h\` argument crossed: the server mapped the client's section and read both markers"
      grep -E "descriptor:|SHM MAPPED" "$RUNDIR/wsrv.log" | sed 's/^/        /'
    else
      bad "the fd shim did not deliver the section"
      sed 's/^/        /' "$RUNDIR/wshm.log"
      grep -E "wl_shm|SHM" "$RUNDIR/wsrv.log" | sed 's/^/        /'
    fi
  fi

  # --- the interesting one: a real C client, from Linux, into the .exe -----
  if command -v wayland-info >/dev/null; then
    mkdir -p "$RUNDIR/xdg"
    ln -sf "$node" "$RUNDIR/xdg/wayland-win" 2>/dev/null
    XDG_RUNTIME_DIR="$RUNDIR/xdg" WAYLAND_DISPLAY=wayland-win wayland-info >"$RUNDIR/winfo.log" 2>&1
    local rc=$?
    if [ $rc -eq 0 ] && grep -q "wl_compositor" "$RUNDIR/winfo.log"; then
      ok "wayland-info (libwayland C, native Linux) enumerated the globals of the Windows server"
      grep -E "interface:" "$RUNDIR/winfo.log" | sed 's/^/        /'
    else
      bad "wayland-info against the Windows server rc=$rc"
      sed 's/^/        /' "$RUNDIR/winfo.log"
    fi
  fi

  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  echo "  --- server log ---"
  sed 's/^/        /' "$RUNDIR/wsrv.log"
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
