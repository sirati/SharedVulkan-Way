#!/usr/bin/env bash
# wc-pool.sh -- §72.3's acceptance test: a pool reaching a **Linux** compositor
# through wc-proxy.
#
#   scripts/wc-pool.sh            both legs, and fail if either is wrong
#   scripts/wc-pool.sh fixed      only the leg that must pass
#   scripts/wc-pool.sh control    only the leg that must still hang
#
# Run from inside `nix develop`. Builds nothing: `cargo build -p wc-compositor
# -p wc-proxy` first.
#
# WHAT THIS IS THE TEST *OF*
#
# §72.3: `wc-proxy` writes its upstream stream in the in-band-`h` dialect, and
# until §79 no code on Linux could read it. A rewritten `wl_shm.create_pool`
# desynchronised the compositor's reader, **the client's next round trip never
# returned, and nothing was logged on either side.** `scripts/wc-pooltest.c` is
# this tree's own client, written for another purpose, and it reproduces it --
# which is why the defect is not in the client that found it.
#
# §72.3 called that "the regression test, and it has to go from failing to
# passing". This is that test. It replaced the tripwire
# `the_upstream_dialect_is_still_assumed_rather_than_chosen`, which asserted the
# defect's *absence of a fix* from inside the unit suite and was deleted by §79
# because its premise stopped being true.
#
# WHY THE CONTROL LEG IS HALF THE TEST
#
# The fixed leg alone would pass just as well if the compositor had quietly
# started ignoring `h` arguments. The control leg runs the identical topology
# with one flag removed and asserts the failure is **still exactly the one §72.3
# describes**: a timeout, not an error, with nothing in either log. That is what
# makes "it works now" mean "the dialect is chosen" rather than "something
# changed".
#
# WHY THERE IS NO VM HERE
#
# `scripts/wc-vm-client.sh --upstream` runs the same three processes with the
# client in another kernel, and that leg is about the *venue*. The dialect is a
# property of the wire, not of the kernel: the proxy writes the same bytes and
# the compositor reads them with the same parser either way. This harness is
# therefore the whole of the dialect question and none of the transport one --
# it deliberately claims nothing about crossing a hypervisor.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=$ROOT/target/debug
MODE=${1:-both}
FAILED=0

for b in wc-compositor wc-proxy; do
  [ -x "$BIN/$b" ] || { echo "wc-pool: no $BIN/$b -- cargo build -p wc-compositor -p wc-proxy" >&2; exit 2; }
done

CLIENT=$(mktemp -u /tmp/wc-pooltest-XXXX)
gcc -O2 -o "$CLIENT" "$ROOT/scripts/wc-pooltest.c" -lwayland-client 2>/dev/null \
  || { echo "wc-pool: could not build wc-pooltest.c (needs libwayland-client)" >&2; exit 2; }

# One leg. $1 is `fixed` or `control`; the only difference is whether the
# compositor is told that the peer on its socket is a proxy.
run_leg() {
  leg=$1
  rundir=$(mktemp -d /tmp/wc-pool-XXXX)
  cdir=$rundir/c; pdir=$rundir/p
  mkdir -p "$cdir" "$pdir" "$rundir/pools" "$rundir/keymaps"

  inband=""
  [ "$leg" = fixed ] && inband="--in-band"

  XDG_RUNTIME_DIR="$cdir" "$BIN/wc-compositor" --socket wg-pool $inband \
    --timeout 45 --layout us --shared-dir "$rundir/keymaps" \
    --shared-peer-prefix "$rundir/keymaps/" >"$rundir/comp.log" 2>&1 &
  comp=$!
  for _ in $(seq 1 100); do [ -S "$cdir/wg-pool" ] && break; sleep 0.2; done
  [ -S "$cdir/wg-pool" ] || { echo "  FAIL: compositor never listened"; kill $comp 2>/dev/null; return 1; }

  XDG_RUNTIME_DIR="$pdir" "$BIN/wc-proxy" --upstream "$cdir/wg-pool" \
    --socket wl-proxy --runtime-dir "$pdir" --transport shared-file \
    --pool-dir "$rundir/pools" --peer-prefix "$rundir/pools/" \
    --timeout 40 >"$rundir/proxy.log" 2>&1 &
  proxy=$!
  for _ in $(seq 1 100); do [ -S "$pdir/wl-proxy" ] && break; sleep 0.2; done
  [ -S "$pdir/wl-proxy" ] || { echo "  FAIL: proxy never listened"; kill $comp $proxy 2>/dev/null; return 1; }

  XDG_RUNTIME_DIR="$pdir" WAYLAND_DISPLAY=wl-proxy timeout 20 "$CLIENT" 4 >"$rundir/client.log" 2>&1
  rc=$?
  sleep 0.5
  kill $proxy $comp 2>/dev/null; wait 2>/dev/null

  pools=$(grep -c 'bytes from the client ->' "$rundir/proxy.log" 2>/dev/null)
  echo "  client exit=$rc, pools published by the proxy=$pools"

  if [ "$leg" = fixed ]; then
    # Four rounds, each one create_pool + create_buffer + destroy. The client
    # exiting 0 means every `wl_display_roundtrip` came back, which is the
    # single thing §72.3 says did not happen.
    if [ "$rc" -eq 0 ] && [ "$pools" -eq 4 ]; then
      echo "  PASS: 4 pools crossed and every round trip returned"
    else
      echo "  FAIL: expected exit 0 and 4 pools"
      sed -n '1,20p' "$rundir/comp.log"
      return 1
    fi
  else
    # 124 is `timeout`'s. Anything else -- a clean exit, or a protocol error --
    # would mean the failure mode moved, and the fixed leg would stop being
    # evidence about *this* defect.
    if [ "$rc" -eq 124 ] && [ "$pools" -eq 1 ]; then
      echo "  PASS: still hangs after the first pool, still silent -- §72.3 unchanged"
    else
      echo "  FAIL: the control leg should hang after one pool (got exit=$rc, pools=$pools)"
      return 1
    fi
  fi
  rm -rf "$rundir"
  return 0
}

echo "== §72.3 acceptance: a pool through wc-proxy to a Linux wc-compositor =="

if [ "$MODE" = both ] || [ "$MODE" = fixed ]; then
  echo "-- fixed: the compositor is told its socket peer is a proxy (--in-band)"
  run_leg fixed || FAILED=1
fi
if [ "$MODE" = both ] || [ "$MODE" = control ]; then
  echo "-- control: the same topology, without telling it"
  run_leg control || FAILED=1
fi

rm -f "$CLIENT"
if [ "$FAILED" -eq 0 ]; then echo "wc-pool: OK"; else echo "wc-pool: FAILED"; fi
exit $FAILED
