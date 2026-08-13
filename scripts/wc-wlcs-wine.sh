#!/usr/bin/env bash
# One WLCS server, on the Wine leg: `wc-compositor.exe` in a Wine virtual
# desktop with `wc-proxy` in front of it.
#
# **Not run by hand.** `scripts/wc-wlcs.sh wine` sets `WC_WLCS_LAUNCH` to this
# script, and `crates/wc-wlcs` runs it once per test with `XDG_RUNTIME_DIR`
# pointing at that test's own run directory and `WC_WLCS_SOCKET` naming the
# socket WLCS will connect to. This script's job is to make that socket exist
# and then stay in the foreground until it is killed.
#
# WHY THIS LEG EXISTS
#
# The headless leg tests the protocol and nothing under it. This one tests the
# two things headless structurally cannot:
#
#   * **Win32.** A real `HWND` per toplevel, a real `GetMessage` pump, a real
#     `WM_ACTIVATE` — so the activation WLCS waits for after a map arrives from
#     Windows rather than from `crate::control` standing in for it. That is a
#     check on the stand-in as much as on the compositor.
#   * **The fd shim.** Every `h` argument crosses in band (docs/waycompose-design.md §2.3)
#     because Wine's AF_UNIX carries no ancillary data, so a `wl_shm` pool from
#     a stock libwayland client — which is what WLCS is — reaches the Windows
#     binary only if `wc-proxy` translated it. §7.5 is the measurement that
#     says this cannot work without the proxy, which makes every passing test
#     here a test of the proxy too.
#
# WHAT IT DOES NOT COVER, SAID BEFORE ANY NUMBER IS QUOTED
#
#   * **There is no hypervisor in it.** Compositor and client are one machine
#     and one filesystem. §26.5 is about the topology that would fix that.
#   * **Wine is not Windows.** Four discrepancies are on record (§13.6), and
#     §13.6 withdrew one of them after finding the cause was a session-0 window
#     station with no display. Treat this leg as evidence that the code paths
#     are self-consistent and structurally possible, not as evidence about
#     `win32u.sys`.
#   * **No pointer and no window placement.** `--control` is `#[cfg(unix)]`,
#     so `WC_WLCS_NO_CONTROL` is set for this leg and every test needing either
#     is filtered out by `scripts/wc-wlcs.sh` rather than run and failed.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

DIR="${XDG_RUNTIME_DIR:?the shim must supply XDG_RUNTIME_DIR}"
SOCK="${WC_WLCS_SOCKET:?the shim must supply WC_WLCS_SOCKET}"
export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"

mkdir -p "$DIR/keymaps" "$DIR/pools"

# A desktop name unique to this server. docs/waycompose-design.md §8.9: two runs with the
# same name make the second `wine explorer` attach to the first one's
# still-dying desktop, after which `CreateWindowExW` fails for every visible
# window and the compositor silently makes none. That failure looks exactly
# like "Wine cannot do this", which is the reason the name is derived from the
# run directory rather than fixed.
TAG=$(basename "$DIR")
WSOCK="wl-$TAG"

winpath() { winepath -w "$1" 2>/dev/null || echo "$1"; }

# The Windows binary. `--shared-dir` is required and not optional: WLCS binds
# `wl_seat`, a seat sends `wl_keyboard.keymap`, and a keymap published as a
# `Local\wc-…` section cannot be opened by a Linux process — the proxy ends the
# connection with a diagnosis (§9.10 item 5). This is the compositor-to-client
# direction of the fd rule and every test here exercises it.
wine explorer "/desktop=$TAG,1280x1024" \
  "$WIN_BIN/wc-compositor.exe" \
  --socket "$WSOCK" --timeout 600 --output 1280x1024 \
  --shared-dir "$(winpath "$DIR/keymaps")" \
  --shared-peer-prefix "$DIR/keymaps/" \
  --log "$(winpath "$DIR/srv.log")" \
  >"$DIR/loader.log" 2>&1 &
SRV=$!

cleanup() {
  [ -n "${PROXY:-}" ] && kill "$PROXY" 2>/dev/null
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT TERM INT

# Where Wine put the compositor's socket. It is inside the prefix, under the
# user's AppData, because that is what `XDG_RUNTIME_DIR` resolves to for a
# Windows process — so it has to be found rather than constructed.
node=""
for _ in $(seq 400); do
  node=$(find "$WINEPREFIX/drive_c/users" -name "$WSOCK" -type s 2>/dev/null | head -1)
  [ -n "$node" ] && break
  kill -0 "$SRV" 2>/dev/null || { echo "the compositor exited; see $DIR/loader.log" >&2; exit 1; }
  sleep 0.1
done
[ -n "$node" ] || { echo "no socket named $WSOCK appeared under the wine prefix" >&2; exit 1; }

# The proxy, listening where WLCS will look. `--copy pool` is the default scope
# and `shared-file` the default transport; the arena is deliberately not used
# here, because an arena is the *hypervisor* transport and there is no
# hypervisor on this leg (§26.5). A file-backed arena would be the Wine-only
# fixture the project rule forbids treating as a real run.
"$LINUX_BIN/wc-proxy" \
  --upstream "$node" --socket "$SOCK" --runtime-dir "$DIR" \
  --transport shared-file --pool-dir "$DIR/pools" \
  --peer-prefix "$(winpath "$DIR/pools")\\" \
  --accept-timeout 600 --timeout 600 \
  >"$DIR/proxy.log" 2>&1 &
PROXY=$!

for _ in $(seq 300); do
  [ -S "$DIR/$SOCK" ] && break
  sleep 0.05
done
[ -S "$DIR/$SOCK" ] || { echo "the proxy never bound $DIR/$SOCK" >&2; exit 1; }

# Stay in the foreground. The shim SIGTERMs this pid when the test ends, and
# the trap takes the other two down with it.
wait "$PROXY"
