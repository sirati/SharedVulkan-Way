#!/usr/bin/env bash
#
# The §36 milestone: an unmodified Wayland client, through a shared-memory ring.
#
#   scripts/wc-ring.sh
#
# `wayland-info` connects to `wc-proxy`, which forwards to `wc-compositor` over
# a ring slot in a shared_vulkan control region. **There is no socket, no TCP
# and no ssh on the upstream hop** — the bytes are memcpy'd into a mapped
# region and the connection itself is a CAS on the ring directory.
#
# What this is not: a VM. Both halves are Linux processes on this machine, so
# the ring is shared memory between two processes rather than across a
# hypervisor. The ring code is identical either way — `SharedRegion` does not
# know which it is — and what is proven here is the protocol path: claim,
# accept, chunk, reassemble, EOF. §36.8 tracks what still separates this from a
# guest window.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
CLIENT=${WC_RING_CLIENT:-wayland-info}
D=$(mktemp -d /tmp/wc-ring-XXXXXX)
mkdir -p "$D/rt" "$D/pools"

say() { echo "[$(date +%H:%M:%S)] $*"; }
cleanup() {
  # By pid. Never `pkill -f`: this machine runs other agents' processes and a
  # pattern kill has already taken one of them down once.
  [ -n "${PROXY:-}" ] && kill "$PROXY" 2>/dev/null
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

say "compositor, creating the ring region"
"$ROOT/target/debug/wc-compositor" \
  --wayland-ring "$D/ctl" --socket "wcring$$" --timeout 30 >"$D/srv.log" 2>&1 &
SRV=$!
for _ in $(seq 1 150); do grep -q "accepting Wayland rings" "$D/srv.log" && break; sleep 0.2; done
grep -q "accepting Wayland rings" "$D/srv.log" || {
  echo "FAIL: the compositor did not create the ring region"; cat "$D/srv.log"; exit 1; }

say "proxy, attaching to it"
XDG_RUNTIME_DIR="$D/rt" "$ROOT/target/debug/wc-proxy" \
  --upstream-ring "$D/ctl" --socket wl-proxy --runtime-dir "$D/rt" \
  --pool-dir "$D/pools" --peer-prefix "$D/pools" --no-icons \
  --accept-timeout 20 --timeout 20 >"$D/proxy.log" 2>&1 &
PROXY=$!
for _ in $(seq 1 150); do grep -q '^\[proxy\] listening on' "$D/proxy.log" && break; sleep 0.2; done
grep -q '^\[proxy\] listening on' "$D/proxy.log" || {
  echo "FAIL: the proxy never listened"; cat "$D/proxy.log"; exit 1; }

say "$CLIENT, which knows nothing about any of this"
XDG_RUNTIME_DIR="$D/rt" WAYLAND_DISPLAY=wl-proxy timeout 25 "$CLIENT" >"$D/cli.log" 2>&1
CLI_RC=$?

echo
echo "----- compositor -----"; grep -E "listening|accepting|ring slot" "$D/srv.log" | sed 's/^/  /'
echo "----- proxy -----"; grep -E "upstream is|ring slot|msgs" "$D/proxy.log" | sed 's/^/  /'
IFACES=$(grep -cE "^interface: " "$D/cli.log")
echo "----- client -----"; echo "  rc=$CLI_RC, $IFACES interfaces enumerated"

echo
# The assertions, and each is a different claim. The interface count says the
# protocol round-tripped; the two log lines say it did so over a ring and that
# both ends agree which; the fd count is §14.7's, on the hop that crosses.
FAIL=0
[ "$CLI_RC" -eq 0 ] || { echo "FAIL: the client exited $CLI_RC"; FAIL=1; }
[ "$IFACES" -ge 10 ] || { echo "FAIL: only $IFACES interfaces; the client saw almost nothing"; FAIL=1; }
grep -q "connected, upstream ring slot" "$D/proxy.log" || { echo "FAIL: the proxy did not use a ring"; FAIL=1; }
grep -q "accepted on ring slot" "$D/srv.log" || { echo "FAIL: the compositor did not accept a ring"; FAIL=1; }
grep -qE "[0-9]+ fds in, [0-9]+ out" "$D/proxy.log" || { echo "FAIL: no fd count reported"; FAIL=1; }

if [ $FAIL -eq 0 ]; then
  echo "ring: PASS — an unmodified Wayland client drove a compositor with no socket upstream"
  exit 0
fi
echo "ring: FAIL (logs in $D)"
trap - EXIT; cleanup
exit 1
