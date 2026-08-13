#!/usr/bin/env bash
# Stage 13: copy in a Linux client, paste in a Windows application, and back.
#
#   scripts/wc-clip.sh wine     the whole chain, under Wine, both directions
#   scripts/wc-clip.sh all      the same
#
# Run from inside `nix develop`. Everything lands in docs/clipboard/.
#
# WHY THIS HARNESS IS ONE LEG AND NOT TWO
#
# Every other stage splits its evidence in half (docs/waycompose-design.md §8.5): a Linux
# leg with a real third-party client and no Windows, and a Wine leg with a real
# window and a client of ours. **The clipboard cannot be split that way**, and
# the reason is structural rather than a shortage of effort.
#
# The `h` dialect is `cfg(windows)` in the vendored `wayland-backend`
# (`src/rs/wire.rs`). So the *Linux* build of wc-compositor speaks libwayland's
# ordinary out-of-band descriptors, and wc-proxy — whose whole job is to write
# the in-band dialect upstream — cannot talk to it at all. A Linux leg would
# therefore have to be a client talking straight to the compositor, with no
# proxy in it, and the proxy is the half of this stage that matters: it is where
# the client's pipe is terminated and where the request that used to kill the
# connection is answered.
#
# So the chain runs where it is real:
#
#   wl-copy / wl-paste        wc-proxy                 wc-compositor.exe
#   (wl-clipboard 2.3,        (terminates the pipe,    (Win32 clipboard)
#    somebody else's code)     ships bytes)                  |
#        |  set_selection ------> pull through a pipe         |
#        |                        chunk/data/publish -------> SetClipboardData
#        |                                                          |
#   probe-clip.exe get  <-------------------------------------------+
#                       (a separate Windows process, not ours)
#
# WHAT MAKES THIS EVIDENCE RATHER THAN A LOG
#
# Three of the four programs in that picture are not ours. `wl-copy` and
# `wl-paste` are wl-clipboard, written by somebody who has never heard of this
# project, and they drive the exact requests under test — `create_data_source`,
# `offer`, `set_selection` on one side, and `wl_data_offer.receive` on the
# other. `probe-clip.exe` is ours but it is a *different process* that uses
# nothing from the compositor: it calls `OpenClipboard` and `GetClipboardData`
# the way Notepad does. A compositor that convinced itself it had set the
# clipboard cannot fake that.
#
# WINE IS NOT WINDOWS, AND HAS BEEN WRONG FOUR TIMES (§13.6)
#
# What this leg establishes is that the code paths are self-consistent against a
# real `user32` clipboard implementation with a real second process reading it.
# What it does not establish is the guest. The specific things worth re-checking
# there are listed in docs/waycompose-design.md §28.9.
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
OUT="$ROOT/docs/clipboard"

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }

RUNDIR=$(mktemp -d /tmp/wc-clip-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${SRV:-}" ] && { kill "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; }
  [ -n "${PROXY:-}" ] && { kill "$PROXY" 2>/dev/null; wait "$PROXY" 2>/dev/null; }
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"

# A tag nobody else's harness matches on. §16.8's rule: this guest and this
# prefix are shared, an image name is not an identity, and a socket name is.
TAG="${WC_CLIP_TAG:-clip$$}"

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

find_socket() {
  local name=$1 node=""
  for _ in $(seq 1 250); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$name" 2>/dev/null | head -1)
    [ -n "$node" ] && { echo "$node"; return 0; }
    sleep 0.2
  done
  return 1
}

# The Windows application, in the same wineserver as the compositor and
# therefore looking at the same clipboard.
clip() {
  ( cd "$RUNDIR" && wine "$RUNDIR/probe-clip.exe" "$@" ) 2>>"$RUNDIR/probe.log"
}

# `clip get`, retried until it answers or the deadline passes.
#
# **A poll rather than a longer sleep, and it is not flakiness management.** A
# copy is asynchronous through four processes: the client answers
# `wl_data_source.send` on its own event loop, wc-proxy drains the pipe on its
# poll turn, the bytes cross to the compositor, and the compositor's UI thread
# reaches its command queue and calls `SetClipboardData`. There is no message
# that says "the clipboard now holds it" — `WM_CLIPBOARDUPDATE` says so to the
# compositor and nothing forwards that to a shell script. A fixed sleep encodes
# a guess about four schedulers; asking until the answer arrives encodes the
# actual question, and the deadline is what turns "never" into a failure.
clip_get_until() {
  local want=$1 deadline=$((SECONDS + ${2:-20})) got=""
  while [ "$SECONDS" -lt "$deadline" ]; do
    got=$(clip get)
    [ "$got" = "$want" ] && { printf '%s' "$got"; return 0; }
    sleep 1
  done
  printf '%s' "$got"
  return 1
}

# ---------------------------------------------------------------------------
# the run
# ---------------------------------------------------------------------------

start_chain() {
  local dir="$RUNDIR/run"
  mkdir -p "$dir/rt" "$dir/pools" "$dir/keymaps" "$dir/frames"
  chmod 700 "$dir/rt"
  SOCK="wl-$TAG"

  # `--shared-dir` because the compositor has a keyboard and every client here
  # binds a seat: without it the keymap is a `Local\wc-…` section and wc-proxy
  # ends the connection with a diagnosis (§9.10 item 5, and wc-proxy.sh's note).
  sleep 2
  wine explorer "/desktop=wc-$TAG,1280x1024" \
    "$WIN_BIN/wc-compositor.exe" --socket "$SOCK" --timeout 120 \
    --shared-dir "$(winepath -w "$dir/keymaps" 2>/dev/null || echo "$dir/keymaps")" \
    --shared-peer-prefix "$dir/keymaps/" \
    --log "$(winepath -w "$dir/srv.log" 2>/dev/null || echo "$dir/srv.log")" \
    >"$dir/loader.log" 2>&1 &
  SRV=$!

  local node
  if ! node=$(find_socket "$SOCK"); then
    return 1
  fi
  NODE=$node

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --transport shared-file --pool-dir "$dir/pools" \
    --peer-prefix "$(winepath -w "$dir/pools" 2>/dev/null || echo "$dir/pools")\\" \
    --no-icons --accept-timeout 90 --timeout 90 >"$dir/proxy.log" 2>&1 &
  PROXY=$!
  sleep 1
  DIR=$dir
  return 0
}

wl() {
  XDG_RUNTIME_DIR="$DIR/rt" WAYLAND_DISPLAY=wl-proxy "$@"
}

block_wine() {
  echo "== the clipboard, both directions, through the whole chain"
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }
  command -v Xvfb >/dev/null || { bad "Xvfb not on PATH"; return; }
  command -v wl-copy >/dev/null || { bad "wl-copy not on PATH -- see flake.nix"; return; }
  [ -x "$WIN_BIN/wc-compositor.exe" ] || { bad "no $WIN_BIN/wc-compositor.exe"; return; }
  [ -x "$LINUX_BIN/wc-proxy" ] || { bad "no $LINUX_BIN/wc-proxy"; return; }

  x86_64-w64-mingw32-gcc -O1 -o "$RUNDIR/probe-clip.exe" "$ROOT/scripts/probe-clip.c" \
    2>"$RUNDIR/probe-build.log" \
    && ok "probe-clip.exe built (the Windows application, and not the code under test)" \
    || { bad "probe-clip.exe did not build: $(tail -2 "$RUNDIR/probe-build.log")"; return; }

  start_x 96 || { bad "could not start Xvfb"; return; }
  ok "Xvfb up on $DISPLAY"

  if ! start_chain; then
    bad "the compositor's socket never appeared"
    sed -n '1,20p' "$RUNDIR/run/loader.log" 2>/dev/null
    return
  fi
  ok "wc-compositor.exe is listening, wc-proxy is in front of it"

  grep -q "watching the system clipboard" "$DIR/srv.log" 2>/dev/null \
    && ok "the compositor registered a WM_CLIPBOARDUPDATE listener" \
    || note "no clipboard listener line (AddClipboardFormatListener may have failed under Wine)"

  # -- direction A: a Linux client copies, a Windows application pastes -------
  local text="waycompose stage 13 -- copied on Linux"
  ( wl timeout 40 wl-copy -- "$text" >"$DIR/copy.log" 2>&1 ) &
  local copier=$!
  local got rc
  got=$(clip_get_until "$text" 25)
  rc=$?
  wait "$copier" 2>/dev/null
  if [ "$rc" -eq 0 ] && [ "$got" = "$text" ]; then
    ok "a Windows application pasted what a Linux client copied"
    note "CF_UNICODETEXT: $(printf '%s' "$got" | head -c 60)"
  else
    bad "probe-clip.exe get -> rc=$rc, ${got:-<nothing>}"
    note "proxy: $(grep -c . "$DIR/proxy.log") lines; last clipboard line:"
    grep -i clipboard "$DIR/proxy.log" | tail -3
    grep -i clipboard "$DIR/srv.log" | tail -3
  fi

  # The `\n` a Unix client sends must be `\r\n` on the Windows clipboard, or
  # Notepad renders one long line. Checked on the bytes rather than trusted.
  ( wl timeout 40 wl-copy -- "$(printf 'one\ntwo')" >/dev/null 2>&1 ) &
  copier=$!
  # **One read, not two.** The bytes are examined from the same `clip get` that
  # the poll accepted; asking a second time would be a second race against
  # whatever writes the clipboard next, and it failed exactly that way once.
  got=$(clip_get_until "$(printf 'one\r\ntwo')" 25 | od -c | head -2)
  wait "$copier" 2>/dev/null
  if printf '%s' "$got" | grep -q '\\r'; then
    ok "line endings became CRLF on the way to the Windows clipboard"
  else
    bad "no CR in the pasted bytes: $got"
  fi

  # -- direction B: a Windows application copies, a Linux client pastes ------
  local back="waycompose stage 13 -- copied in a Windows application"
  if clip set "$back"; then
    ok "probe-clip.exe put text on the Windows clipboard"
  else
    bad "probe-clip.exe set failed"
  fi
  sleep 2
  local pasted
  pasted=$(wl timeout 25 wl-paste --no-newline 2>"$DIR/paste.log")
  rc=$?
  if [ "$rc" -eq 0 ] && [ "$pasted" = "$back" ]; then
    ok "a Linux client pasted what a Windows application copied"
  else
    bad "wl-paste -> rc=$rc, ${pasted:-<nothing>}"
    note "$(tail -3 "$DIR/paste.log")"
    grep -iE "clipboard|selection|data" "$DIR/srv.log" | tail -6
  fi

  # -- the defect, from outside ----------------------------------------------
  #
  # Before this stage `wl_data_offer.receive` returned `Error::UnsupportedFd`,
  # wc-proxy failed the drain, and the client's connection was closed. The
  # observable difference is in the proxy's own log: a client that pasted used
  # to end with "failed:", and now ends with "stopped:".
  if grep -q "client .* failed:" "$DIR/proxy.log"; then
    bad "a connection failed: $(grep 'failed:' "$DIR/proxy.log" | head -2)"
  else
    ok "no client connection failed -- a paste is answered, not fatal"
  fi
  grep -q "carries a descriptor this proxy cannot" "$DIR/proxy.log" \
    && bad "the UnsupportedFd path was reached" \
    || ok "nothing hit the descriptor-with-no-transport path"

  # -- html ------------------------------------------------------------------
  if clip set-html "<b>rich</b> text"; then
    sleep 2
    local html
    html=$(wl timeout 25 wl-paste --type text/html --no-newline 2>>"$DIR/paste.log")
    if printf '%s' "$html" | grep -q "<b>rich</b> text"; then
      ok "text/html crossed with the CF_HTML header stripped"
      note "got: $(printf '%s' "$html" | head -c 60)"
    else
      note "text/html did not come back (${html:0:60}); see docs/waycompose-design.md §28.9"
    fi
  fi

  # -- the counters ----------------------------------------------------------
  # One line from a connection that copied and one from a connection that
  # pasted, because the two have different fd counts and §28.4 is an argument
  # about exactly those numbers.
  local line
  line=$(grep "clipboard: 1 copied" "$DIR/proxy.log" | head -1)
  [ -n "$line" ] && note "a copy: ${line#*] }"
  line=$(grep "clipboard: 0 copied .*1 pasted" "$DIR/proxy.log" | head -1)
  [ -n "$line" ] && note "a paste: ${line#*] }"

  mkdir -p "$OUT"
  cp "$DIR/srv.log" "$OUT/compositor.log" 2>/dev/null
  cp "$DIR/proxy.log" "$OUT/proxy.log" 2>/dev/null
  note "logs in $OUT/"
}

case "$MODE" in
  wine|all) block_wine ;;
  *) echo "usage: $0 [wine|all]"; exit 2 ;;
esac

echo
echo "  $pass passed, $fail failed"
[ "$fail" -eq 0 ]
