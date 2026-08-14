#!/usr/bin/env bash
# wc-keymap.sh -- the `h` dialect in the **compositor to client** direction.
#
#   scripts/wc-keymap.sh          both legs, and fail if either is wrong
#   scripts/wc-keymap.sh fixed    only the leg that must pass
#   scripts/wc-keymap.sh control  only the leg that must still fail
#
# Run from inside `nix develop`. `cargo build -p wc-compositor -p wc-proxy` first.
#
# THE OTHER HALF OF `wc-pool.sh`
#
# `wc-pool.sh` is §72.3's acceptance test and runs client -> proxy -> compositor:
# a `wl_shm.create_pool` whose `h` the compositor has to *read*. This is the
# return path, where the compositor has to *write* one -- `wl_keyboard.keymap`,
# the only core event carrying an `h`. §79 built the reading half and said so;
# §80 is the writing half.
#
# The client is `wayland-info`, deliberately: it is not ours, it is in the dev
# shell already, and it binds `wl_seat` and creates a `wl_keyboard`, which is
# exactly what makes a compositor send a keymap. `wc-pooltest.c` binds nothing
# but `wl_shm` and so never exercises this direction at all -- which is why the
# pool leg passing was never evidence about it.
#
# WHAT THE ASSERTION IS, AND WHY IT IS NOT THE EXIT CODE
#
# `wayland-info` exits 0 either way: it prints the globals it was told about and
# stops. What differs is whether the *keyboard* answered. `wl_keyboard.repeat_info`
# is sent immediately after `wl_keyboard.keymap`, so if the keymap desynchronised
# the stream the repeat lines never arrive -- and that absence is the symptom, in
# a run whose exit code says everything was fine. Asserting on the exit code
# would have called the control leg a pass.
#
# The control leg's failure is *logged* here, unlike the pool direction's:
#
#   [proxy] client 1 failed: compositor: wl_keyboard.keymap (event):
#           argument 1 claims 35572 bytes, 0 available
#
# The proxy reads its upstream in band, so it takes the length word of an
# out-of-band `h` -- which occupies no body bytes at all -- and finds nothing
# behind it. That is the same defect as §72.3's, seen from the end that happens
# to have a length check in front of it.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=$ROOT/target/debug
MODE=${1:-both}
FAILED=0

for b in wc-compositor wc-proxy; do
  [ -x "$BIN/$b" ] || { echo "wc-keymap: no $BIN/$b -- cargo build -p wc-compositor -p wc-proxy" >&2; exit 2; }
done
command -v wayland-info >/dev/null || { echo "wc-keymap: no wayland-info on PATH" >&2; exit 2; }

run_leg() {
  leg=$1
  rundir=$(mktemp -d /tmp/wc-keymap-XXXX)
  cdir=$rundir/c; pdir=$rundir/p
  mkdir -p "$cdir" "$pdir" "$rundir/pools" "$rundir/keymaps"

  inband=""
  [ "$leg" = fixed ] && inband="--in-band"

  XDG_RUNTIME_DIR="$cdir" "$BIN/wc-compositor" --socket wg-km $inband \
    --timeout 40 --layout us --shared-dir "$rundir/keymaps" \
    --shared-peer-prefix "$rundir/keymaps/" >"$rundir/comp.log" 2>&1 &
  comp=$!
  for _ in $(seq 1 100); do [ -S "$cdir/wg-km" ] && break; sleep 0.2; done
  [ -S "$cdir/wg-km" ] || { echo "  FAIL: compositor never listened"; kill $comp 2>/dev/null; return 1; }

  XDG_RUNTIME_DIR="$pdir" "$BIN/wc-proxy" --upstream "$cdir/wg-km" \
    --socket wl-proxy --runtime-dir "$pdir" --transport shared-file \
    --pool-dir "$rundir/pools" --peer-prefix "$rundir/pools/" \
    --timeout 35 >"$rundir/proxy.log" 2>&1 &
  proxy=$!
  for _ in $(seq 1 100); do [ -S "$pdir/wl-proxy" ] && break; sleep 0.2; done
  [ -S "$pdir/wl-proxy" ] || { echo "  FAIL: proxy never listened"; kill $comp $proxy 2>/dev/null; return 1; }

  XDG_RUNTIME_DIR="$pdir" WAYLAND_DISPLAY=wl-proxy timeout 20 wayland-info >"$rundir/info.log" 2>&1
  rc=$?
  sleep 0.5
  kill $proxy $comp 2>/dev/null; wait 2>/dev/null

  # The keyboard answered iff `repeat_info` arrived, which follows `keymap`.
  repeat=$(grep -c 'keyboard repeat rate' "$rundir/info.log" 2>/dev/null)
  # One named object per keymap, minted into --shared-dir on first in-band use.
  minted=$(find "$rundir/keymaps" -type f 2>/dev/null | wc -l)
  echo "  wayland-info exit=$rc, repeat_info lines=$repeat, keymap objects minted=$minted"

  if [ "$leg" = fixed ]; then
    if [ "$rc" -eq 0 ] && [ "$repeat" -ge 1 ] && [ "$minted" -ge 1 ]; then
      echo "  PASS: the keymap crossed as a named object and the keyboard answered"
    else
      echo "  FAIL: expected exit 0, repeat_info, and a minted object"
      grep -iE 'failed|error' "$rundir/proxy.log" | head -3
      return 1
    fi
  else
    # The proxy must still reject it, and must still reject it *for this reason*.
    if [ "$repeat" -eq 0 ] && [ "$minted" -eq 0 ] \
       && grep -q 'wl_keyboard.keymap (event)' "$rundir/proxy.log"; then
      echo "  PASS: still rejected at wl_keyboard.keymap, still no keyboard"
    else
      echo "  FAIL: the control leg should lose the keyboard at the keymap event"
      grep -iE 'failed|error' "$rundir/proxy.log" | head -3
      return 1
    fi
  fi
  rm -rf "$rundir"
  return 0
}

echo "== §80: a keymap from a Linux compositor to a client behind wc-proxy =="

if [ "$MODE" = both ] || [ "$MODE" = fixed ]; then
  echo "-- fixed: the compositor is told its socket peer is a proxy (--in-band)"
  run_leg fixed || FAILED=1
fi
if [ "$MODE" = both ] || [ "$MODE" = control ]; then
  echo "-- control: the same topology, without telling it"
  run_leg control || FAILED=1
fi

if [ "$FAILED" -eq 0 ]; then echo "wc-keymap: OK"; else echo "wc-keymap: FAILED"; fi
exit $FAILED
