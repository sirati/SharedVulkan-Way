#!/usr/bin/env bash
# §44: a drag that actually enters, in both directions.
#
#   scripts/wc-drag.sh wine     the whole chain, both directions
#   scripts/wc-drag.sh up       bring the chain up and leave it up (KEEP=1)
#
# Run from inside `nix develop`.
#
# WHAT §43 LEFT, AND WHY THIS IS A DIFFERENT HARNESS RATHER THAN A FLAG
#
# docs/waycompose-design.md §43.9 item 1: "**No drag has ever entered.**
# `DragEnter` has not run outside a unit test. It needs a desktop with a Windows
# application on it and a mouse button held across two windows". §29.8 and §43
# both name the missing thing as a *venue* and both point at the guest.
#
# The venue is not the only thing that was missing, and this is the correction:
# **what a drag needs is a display with an input path, two applications on it,
# and something willing to hold the button down.** The existing Wine legs have
# an Xvfb and one application. Xvfb has XTEST, which is a real input path — the
# events arrive at Wine through the same X input queue a human's would — so the
# two things actually absent were the *second application* and the *gesture*.
# `scripts/probe-drag.c` is the first and `drag_x` below is the second.
#
# WHERE THIS RUN IS HONEST AND WHERE IT IS NOT
#
# It is honest about OLE: two processes, a real `ole32`, a real `DoDragDrop`
# modal loop, a real cross-process `IDropTarget` marshalled by Wine the way
# Windows marshals it, and a real button held across two windows.
#
# It is **not** a boundary. §29.8's correction stands word for word: every
# process here is on one kernel, and this leg says nothing about `afunix.sys`
# or about a hypervisor. The guest is still the venue for that, and §44 records
# why this run did not happen there (the guest's session was locked, and Windows
# discards `SendInput` from a locked workstation — measured, not assumed).
#
# THE ONE THING THAT MUST NOT BE TRUSTED HERE
#
# A drag that "worked" because nothing failed is exactly the gate §35 is about.
# So every assertion below reads a line that a *named* process printed:
# `DragEnter`/`DragOver`/`Drop` come from the compositor's own log, the bytes
# that arrived come from `probe-drag.exe`'s log or from `weston-dnd`'s stdout,
# and the harness fails if the line is absent. Nothing is inferred from the
# absence of an error.
set -uo pipefail

MODE="${1:-wine}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
PROBE="${CARGO_TARGET_DIR:-$ROOT/target}/probe-drag.exe"

pass=0; fail=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }

. "$(dirname "$0")/wc-binaries.sh"
. "$(dirname "$0")/wc-substrate.sh"

# §51. Whether a hypervisor is in the path, declared rather than left unknown.
# This harness is same-box, so `wc_latency_claim` will refuse any assertion here
# whose truth depends on cross-VM timing -- which is the point: §27.6 drew one
# from a harness exactly like this one.
. "$(dirname "$0")/wc-topology.sh"
wc_topology_declare same-box \
  "both ends of the drag are processes on this box" || exit 1

RUNDIR=$(mktemp -d /tmp/wc-drag-XXXXXX)
XPID=""; SRV=""; PROXY=""; CLIENT=""; CLIENT2=""; PROBEPID=""
cleanup() {
  for p in $CLIENT $CLIENT2 $PROBEPID $PROXY $SRV; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  sleep 1
  wineserver -k 2>/dev/null
  [ -n "$XPID" ] && kill "$XPID" 2>/dev/null
  if [ -n "${KEEP:-}" ]; then echo "kept: $RUNDIR"; else rm -rf "$RUNDIR"; fi
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-drag-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"
TAG="${WC_DRAG_TAG:-drag$$}"
DESK=1280x1024
XDOTOOL="${XDOTOOL:-xdotool}"

# ------------------------------------------------------------- the display ---

start_x() {
  Xvfb ":$1" -screen 0 "${DESK}x24" -nolisten tcp >"$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$1"
  for _ in $(seq 1 100); do
    xdpyinfo >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
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

# Every Wine process joins the *same* virtual desktop, which is what puts the
# compositor's windows and probe-drag's window on one desktop with one pointer.
# Passing /desktop= to each `wine` invocation separately gives each its own.
wined() { wine explorer "/desktop=wc-$TAG,$DESK" "$@"; }

wl() { XDG_RUNTIME_DIR="$RUNDIR/run/rt" WAYLAND_DISPLAY=wl-proxy "$@"; }

# ------------------------------------------------------------- the gesture ---
#
# A press, a *creep*, and a release. The creep is the part that is not padding:
# OLE decides a gesture is a drag only after the pointer has moved past
# `SM_CXDRAG` while the button is down, and it calls `IDropTarget::DragOver`
# once per mouse move — so a single jump from A to B is a click that lands on B,
# and a run of small steps is a drag. `--sync` on each move keeps the X server
# and Wine in step; without it xdotool outruns the wineserver and the whole
# gesture arrives as two events.
drag_x() {
  local x1=$1 y1=$2 x2=$3 y2=$4 steps=${5:-40} delay=${6:-0.03}
  local i x y
  $XDOTOOL mousemove --sync "$x1" "$y1"; sleep 0.4
  $XDOTOOL mousedown 1; sleep 0.3
  for i in $(seq 1 "$steps"); do
    x=$((x1 + (x2 - x1) * i / steps))
    y=$((y1 + (y2 - y1) * i / steps))
    $XDOTOOL mousemove --sync "$x" "$y"
    sleep "$delay"
  done
  # Settle on the target: several DragOver calls at the drop point, so that a
  # feedback that arrived late still has a call left to be reported on.
  for i in 0 1 0 1 0 1 0 1; do
    $XDOTOOL mousemove --sync $((x2 + i)) "$y2"
    sleep 0.07
  done
  sleep 0.4
  $XDOTOOL mouseup 1
  sleep 0.8
}

# The Win32 windows on the Wine desktop. X cannot see them: they are all inside
# one X window (Wine's virtual desktop), so this asks Win32.
wins() {
  rm -f "$RUNDIR/wins.txt"
  wined "$PROBE" --mode list --log "$(winepath -w "$RUNDIR/wins.txt")" >/dev/null 2>&1
  for _ in $(seq 1 40); do [ -s "$RUNDIR/wins.txt" ] && break; sleep 0.25; done
  # `tr -d '\r'`, and it is not tidiness. probe-drag.exe writes this with
  # `fprintf` on Windows, where a `\n` in text mode becomes CRLF — so the last
  # field of every line ends in a carriage return, and an `awk` matching a title
  # against `"Wayland Drag and Drop Demo"` silently matches nothing. It reads
  # exactly like the window not being there, which is a diagnosis one would
  # otherwise chase into the compositor.
  tr -d '\r' < "$RUNDIR/wins.txt" 2>/dev/null
}

# probe-drag.exe is built by this harness rather than by cargo, because it is a
# C program for the Windows target and the workspace has no place for one. It is
# rebuilt every run: `fresh` compares a binary against *every* source file in the
# tree, so a probe built before an unrelated edit is "stale" and would stop the
# run — and the fix for that is to make the build cheap, not to weaken the guard.
build_probe() {
  # The cross gcc is on the dev shell's PATH. `scripts/probe-build.sh` hard-codes
  # a store path for it and that path no longer exists — worth not copying.
  local gcc=x86_64-w64-mingw32-gcc
  command -v "$gcc" >/dev/null || { bad "no $gcc on PATH (run inside nix develop)"; return 1; }
  "$gcc" -O1 -g -o "$PROBE" "$ROOT/scripts/probe-drag.c" \
    -lole32 -luuid -lgdi32 -luser32 || { bad "probe-drag.c did not build"; return 1; }
  return 0
}

place() {
  wined "$PROBE" --mode place --hwnd "$1" --x "$2" --y "$3" --w "$4" --h "$5" 2>/dev/null
}

# ---------------------------------------------------------------- the chain ---

start_chain() {
  local dir="$RUNDIR/run"
  mkdir -p "$dir/rt" "$dir/pools" "$dir/keymaps"
  chmod 700 "$dir/rt"
  SOCK="wl-$TAG"

  RUST_BACKTRACE=1 wined "$WIN_BIN/wc-compositor.exe" --socket "$SOCK" --timeout 300 \
    --shared-dir "$(winepath -w "$dir/keymaps" 2>/dev/null || echo "$dir/keymaps")" \
    --shared-peer-prefix "$dir/keymaps/" \
    --log "$(winepath -w "$dir/srv.log" 2>/dev/null || echo "$dir/srv.log")" \
    >"$dir/loader.log" 2>&1 &
  SRV=$!

  local node
  node=$(find_socket "$SOCK") || return 1
  NODE=$node

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --transport shared-file --pool-dir "$dir/pools" \
    --peer-prefix "$(winepath -w "$dir/pools" 2>/dev/null || echo "$dir/pools")\\" \
    --no-icons --accept-timeout 240 --timeout 240 >"$dir/proxy.log" 2>&1 &
  PROXY=$!
  sleep 1
  DIR=$dir
  return 0
}

# ------------------------------------------------------------------- the run ---

run_wine() {
  echo "== §44: a drag that enters, under Wine on Xvfb =="

  command -v Xvfb    >/dev/null || { bad "Xvfb not on PATH"; return; }
  command -v wine    >/dev/null || { bad "wine not on PATH"; return; }
  command -v $XDOTOOL >/dev/null || { bad "xdotool not on PATH (nix shell nixpkgs#xdotool)"; return; }

  # §42's guard: measure this tree, not the one before it.
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build --workspace --target x86_64-pc-windows-gnu" || return
  fresh "$LINUX_BIN/wc-proxy" "cargo build --workspace" || return
  build_probe || return
  fresh "$PROBE" "this harness rebuilds it every run; see build_probe" || return

  # Both binaries are used from the paths they were built at, on one machine, so
  # there is no far side for `deployed_is` to check. Said rather than skipped:
  # the reason there is no deployment check is that there is no deployment, and
  # that is the same fact as this leg not being a boundary.
  note "no deployed_is check: nothing is copied across a boundary in this leg (see the header)"

  wc_substrate_declare wine-fixture \
    "the Wine leg has no QEMU and no guest; the pixel substrate is not what this run measures" || return

  local W=$(ls -d /nix/store/*weston-15*/bin 2>/dev/null | head -1)
  [ -x "$W/weston-dnd" ] || { bad "weston-dnd not found"; return; }

  start_x 97 || { bad "could not start Xvfb"; return; }
  ok "Xvfb up on $DISPLAY (${DESK})"

  start_chain || { bad "the chain did not come up"; sed -n '1,20p' "$RUNDIR/run/loader.log"; return; }
  ok "wc-compositor.exe is listening on $NODE, wc-proxy is in front of it"

  # The Wayland client: weston's own drag-and-drop demo. Third-party, and it is
  # both halves — it starts drags carrying `text/plain;charset=utf-8` and it
  # accepts drops of the same. `--self-only` is *not* passed, which is what lets
  # it take a drop that came from another client (here: from Windows).
  wl "$W/weston-dnd" >"$RUNDIR/run/dnd.log" 2>&1 &
  CLIENT=$!
  sleep 3

  # THE DROP TARGET, AND WHY IT IS A TERMINAL
  #
  # `weston-editor` was the obvious choice and it exits on this compositor with
  # "No text input manager global" — it wants `zwp_text_input_manager`, which is
  # not implemented here. `weston-terminal` is the better witness anyway, and
  # not as a consolation: its drop handler calls
  # `input_receive_drag_data_to_fd(input, "text/plain;charset=utf-8", master)`,
  # which writes the received bytes **straight into the pty**. So whatever the
  # drag delivered arrives at a program's standard input, and the assertion can
  # be on bytes rather than on a picture. `dropcatch.sh` is that program: it
  # takes the pty out of canonical mode so no Return is needed, and copies what
  # it is given to a file this harness reads.
  cat >"$RUNDIR/dropcatch.sh" <<'CATCH'
#!/bin/sh
# The "shell" weston-terminal runs. Raw mode so that a drop with no trailing
# newline still arrives -- in canonical mode the tty would hold the bytes until
# somebody pressed Return, which looks exactly like a drop that delivered
# nothing.
stty raw -echo 2>/dev/null
exec cat > "$WC_DROP_OUT"
CATCH
  chmod +x "$RUNDIR/dropcatch.sh"
  WC_DROP_OUT="$RUNDIR/dropped.txt" wl "$W/weston-terminal" --shell="$RUNDIR/dropcatch.sh" \
    >"$RUNDIR/run/term.log" 2>&1 &
  CLIENT2=$!
  sleep 4

  wined "$PROBE" --mode both --title "probe-drag" --log "$(winepath -w "$RUNDIR/run/probe.log")" \
    --text "$OUTTEXT" --x $PX --y $PY --w $PW --h $PH --seconds 240 \
    >"$RUNDIR/run/probe.out" 2>&1 &
  PROBEPID=$!
  for _ in $(seq 1 60); do
    grep -q PROBE-READY "$RUNDIR/run/probe.log" 2>/dev/null && break
    sleep 0.5
  done
  grep -q PROBE-READY "$RUNDIR/run/probe.log" 2>/dev/null \
    && ok "probe-drag.exe is up and registered" \
    || { bad "probe-drag.exe never reported ready"; cat "$RUNDIR/run/probe.out"; return; }

  # Put the client's window where this harness expects it. The compositor puts
  # it at the origin, which is where Wine also puts a console window, and a drop
  # that lands on somebody else's window is a drop the compositor never sees.
  CLIENT_HWND=$(wins | awk -F'\t' '$4 == "Wayland Drag and Drop Demo" { print $1; exit }')
  [ -n "$CLIENT_HWND" ] || { bad "the client's window is not on the Wine desktop"; wins; return; }
  place "$CLIENT_HWND" "$CX" "$CY" "$CW" "$CH" >/dev/null
  sleep 1

  TERM_HWND=$(wins | awk -F'\t' '$4 ~ /Wayland Terminal|weston-terminal|dropcatch/ { print $1; exit }')
  [ -n "$TERM_HWND" ] || { bad "weston-terminal's window is not on the Wine desktop"; wins; return; }
  place "$TERM_HWND" "$EX" "$EY" "$EW" "$EH" >/dev/null
  sleep 1

  PROBE_HWND=$(wins | awk -F'\t' '$4 == "probe-drag" { print $1; exit }')
  [ -n "$PROBE_HWND" ] || { bad "probe-drag's window is not on the Wine desktop"; wins; return; }

  echo "--- windows on the Wine desktop ---"
  wins
  import -window root "$RUNDIR/before.png" 2>/dev/null

  # =====================================================================
  # DIRECTION IN -- a native Windows application into a Wayland client.
  # =====================================================================
  #
  # probe-drag.exe enters DoDragDrop from its own WM_LBUTTONDOWN, so the press
  # is what starts it; everything after that is OLE's modal loop calling the
  # compositor's IDropTarget across a process boundary.
  echo
  echo "== direction IN: probe-drag.exe (Windows) -> weston-dnd (Wayland) =="
  drag_x $((PX + PW / 2)) $((PY + PH / 2)) $((EX + EW / 2)) $((EY + EH / 2)) 45 0.03

  grep -q "a Windows drag entered window" "$DIR/srv.log" \
    && ok "IDropTarget::DragEnter ran: $(grep -m1 'a Windows drag entered window' "$DIR/srv.log")" \
    || bad "DragEnter never ran -- the compositor's log has no 'a Windows drag entered window'"

  grep -q "DragOver on window" "$DIR/srv.log" \
    && ok "IDropTarget::DragOver ran ($(grep -c 'DragOver on window' "$DIR/srv.log") log line(s))" \
    || bad "DragOver never ran"

  grep -q "a Windows drag was dropped on window" "$DIR/srv.log" \
    && ok "IDropTarget::Drop ran: $(grep -m1 'a Windows drag was dropped on window' "$DIR/srv.log")" \
    || bad "Drop never ran"

  # §43.4's staleness, as the number of gesture frames it actually costs.
  echo "--- §43.4: the stale cursor, measured ---"
  grep -E "DragOver on window|the client under the drag answered" "$DIR/srv.log" | head -12

  # The bytes, at the far end of the whole chain: Windows IDataObject ->
  # IDropTarget -> smithay start_dnd -> wl_data_device -> weston-terminal ->
  # the pty -> a file. Nothing of ours is in the last four hops.
  for _ in $(seq 1 40); do [ -s "$RUNDIR/dropped.txt" ] && break; sleep 0.25; done
  if [ -s "$RUNDIR/dropped.txt" ]; then
    ok "the Wayland client received the bytes: $(od -c "$RUNDIR/dropped.txt" | head -2 | tr '\n' ' ')"
    if grep -qF "$OUTTEXT" "$RUNDIR/dropped.txt"; then
      ok "and they are the bytes the Windows application offered ($OUTTEXT)"
    else
      bad "the bytes that arrived are not the ones offered: $(cat "$RUNDIR/dropped.txt")"
    fi
  else
    bad "nothing reached the Wayland client's pty"
  fi

  echo "--- what the Wayland clients made of it ---"
  cat "$DIR/dnd.log" "$DIR/term.log" 2>/dev/null | tail -20
  echo "--- wc-proxy ---"
  grep -iE "drag|dnd|offer" "$DIR/proxy.log" 2>/dev/null | tail -20

  import -window root "$RUNDIR/after-in.png" 2>/dev/null

  # =====================================================================
  # DIRECTION OUT -- a Wayland client into a native Windows application.
  # =====================================================================
  #
  # The press lands on one of weston-dnd's flowers *inside the compositor's
  # window*, so the pointer event goes Windows -> compositor -> wc-proxy ->
  # client, the client decides it is a drag and calls wl_data_device.start_drag,
  # wc-proxy intercepts it and pulls the bytes, and the compositor posts
  # WM_WC_DRAG to itself and enters DoDragDrop -- all while the button is
  # still down and moving. probe-drag.exe's IDropTarget is the far end.
  echo
  echo "== direction OUT: weston-dnd (Wayland) -> probe-drag.exe (Windows) =="
  drag_x $((CX + 218)) $((CY + 185)) $((PX + PW / 2)) $((PY + PH / 2)) 45 0.03

  echo "--- probe-drag.exe's log ---"
  cat "$DIR/probe.log" 2>/dev/null
  echo "--- compositor, the source side ---"
  grep -iE "drag|DoDragDrop" "$DIR/srv.log" 2>/dev/null | tail -25

  grep -q "TARGET-DragEnter" "$DIR/probe.log" \
    && ok "a drag out of the Wayland client entered the Windows application" \
    || bad "probe-drag.exe never saw a DragEnter from the Wayland client"

  grep -q "TARGET-Drop" "$DIR/probe.log" \
    && ok "the Windows application received the drop" \
    || bad "probe-drag.exe never saw a Drop"

  # Not `<<<.*>>>` on one line: weston-dnd's own payload ends in a newline, so
  # the closing marker is on the next one and the anchored form matched nothing
  # on a drop that had in fact delivered.
  if grep -q "DROP-TEXT: <<<" "$DIR/probe.log" && ! grep -q "DROP-TEXT: no CF_UNICODETEXT" "$DIR/probe.log"; then
    ok "and the bytes arrived: $(grep -m1 'DROP-TEXT' "$DIR/probe.log")"
  else
    bad "the drop carried no CF_UNICODETEXT"
  fi

  import -window root "$RUNDIR/after-out.png" 2>/dev/null

  # Keep the evidence where §29.7 keeps the clipboard's, because a claim in the
  # design document that cites a log should be able to cite the log.
  mkdir -p "$ROOT/docs/drag"
  cp "$DIR/srv.log"   "$ROOT/docs/drag/compositor.log" 2>/dev/null
  cp "$DIR/proxy.log" "$ROOT/docs/drag/proxy.log"      2>/dev/null
  cp "$DIR/probe.log" "$ROOT/docs/drag/probe-drag.log" 2>/dev/null
  cp "$RUNDIR/after-out.png" "$ROOT/docs/drag/desktop.png" 2>/dev/null
  note "logs in $ROOT/docs/drag/, screenshots in $RUNDIR/{before,after-in,after-out}.png"
}

OUTTEXT="${WC_DRAG_TEXT:-waycompose drag payload 4711}"

# Where the two windows go. Fixed rather than discovered, because every
# coordinate in the gesture is relative to them and a run whose windows moved is
# a run whose drag went somewhere else.
CX=120; CY=150; CW=356; CH=403          # weston-dnd, the drag *source* client
EX=120; EY=620; EW=420; EH=300          # weston-editor, the drop *target* client
PX=780; PY=80;  PW=420; PH=380          # probe-drag's own window

# TWO CLIENTS, AND WHY IT IS NOT ONE
#
# The first run of this harness used `weston-dnd` for both directions and the
# drag *in* was refused: the compositor's log said `the client under the drag
# accepts nothing`, once per `DragOver`, for a drag offering
# `text/plain;charset=utf-8`. That is weston-dnd behaving correctly rather than
# a defect — it is a demo of dragging *its own* `application/x-wayland-dnd-flower`
# items and its drop handler wants that type — but it makes it a drag source and
# not a text drop target. `weston-editor` is the other half: it sets a drop
# handler for `text/plain;charset=utf-8` and puts what it receives in the
# document, where a screenshot can see it. Both are weston's, both are
# third-party, and each is used for the direction it is actually about.

case "$MODE" in
  build-probe)
    echo "x86_64-w64-mingw32-gcc -O1 -g -o $PROBE scripts/probe-drag.c -lole32 -luuid -lgdi32 -luser32" ;;
  up)       export KEEP=1; run_wine ;;
  wine|all) run_wine ;;
  *) echo "usage: wc-drag.sh [wine|up]"; exit 2 ;;
esac

echo
echo "  $pass passed, $fail failed"
[ "$fail" -eq 0 ]
