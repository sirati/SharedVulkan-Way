#!/usr/bin/env bash
#
# §89 on one machine: the launcher's Wayland rings, end to end, with a file
# where the ivshmem device would be and a socat pair where the virtio-serial
# port would be.
#
#   scripts/wc-rings.sh            the rings, 40 clients in a row (more than the
#                                  region has slots, so recycling is exercised),
#                                  then the fallback: no port, every connection
#                                  on the socket, said in capitals at both ends
#
# Run from inside `nix develop` after `cargo build -p wc-compositor -p wc-proxy`.
#
# The compositor is the Linux build. It maps the stand-in file as its ivshmem
# device (WC_IVSHMEM_STANDIN, wc_handle's unix platform, which exists for this
# script only), keeps the region's tail for the rings, and publishes the header
# with them in it; the proxy checks the header's run id, attaches the rings and
# holds the "host" end of the doorbell. What this cannot show is the hypervisor:
# a virtio-serial byte and a BAR are a guest's claim (§89's guest runs).
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/debug
CLIENT=${WC_RINGS_CLIENT:-wayland-info}
N=${WC_RINGS_N:-40}
SOCAT=$(command -v socat || ls /nix/store/*-socat-1.*/bin/socat 2>/dev/null | head -1)
D=$(mktemp -d /tmp/wc-rings-XXXXXX)
mkdir -p "$D/rt" "$D/crt"
chmod 700 "$D/rt" "$D/crt"
FAIL=0

say() { echo "[$(date +%H:%M:%S)] $*"; }
bad() { echo "FAIL: $*"; FAIL=1; }
ok() { echo "ok: $*"; }
cleanup() {
  # By pid, never by pattern: other agents run processes on this machine.
  for p in ${PROXY:-} ${SRV:-} ${RELAY:-}; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
}
trap cleanup EXIT

[ -x "$SOCAT" ] || { echo "no socat"; exit 2; }
command -v "$CLIENT" >/dev/null || { echo "no $CLIENT"; exit 2; }

# One leg: $1 = rings | fallback.
leg() {
  local mode=$1
  rm -f "$D"/*.sock "$D/arena"
  truncate -s $((128 << 20)) "$D/arena"
  local port="$D/guest.sock"
  if [ "$mode" = rings ]; then
    # QEMU's chardev is one socket the host side connects to; the guest's
    # port is the other end. socat accepts the compositor first (it opens its
    # port at start-up), then the proxy, and relays bytes between them.
    "$SOCAT" UNIX-LISTEN:"$D/guest.sock" UNIX-LISTEN:"$D/host.sock" &
    RELAY=$!
    for _ in $(seq 1 50); do [ -S "$D/guest.sock" ] && break; sleep 0.1; done
  else
    port="$D/no-such-port.sock"
  fi

  say "$mode: compositor"
  XDG_RUNTIME_DIR="$D/crt" WC_IVSHMEM_STANDIN="$D/arena" "$BIN/wc-compositor" \
    --arena-ivshmem 0 --arena-outbound 0:1048576 --wayland-rings "$port" \
    --socket "wcrings$$" --in-band --timeout 120 >"$D/srv-$mode.log" 2>&1 &
  SRV=$!
  local run=""
  for _ in $(seq 1 150); do
    run=$(grep -o 'header published: run 0x[0-9a-f]*' "$D/srv-$mode.log" | awk '{print $4}')
    [ -n "$run" ] && break
    sleep 0.2
  done
  [ -n "$run" ] || { bad "$mode: no header published"; cat "$D/srv-$mode.log"; return; }

  if [ "$mode" = rings ]; then
    for _ in $(seq 1 50); do [ -S "$D/host.sock" ] && break; sleep 0.1; done
  fi
  say "$mode: proxy, run $run"
  XDG_RUNTIME_DIR="$D/rt" "$BIN/wc-proxy" \
    --upstream "$D/crt/wcrings$$" --socket wl-proxy --runtime-dir "$D/rt" \
    --transport arena --arena-ivshmem "$D/arena" --arena-run "$run" --arena-id 0 \
    --wayland-rings --wayland-bell "$D/host.sock" --no-icons \
    --accept-timeout 60 --timeout 60 >"$D/proxy-$mode.log" 2>&1 &
  PROXY=$!
  for _ in $(seq 1 150); do grep -q '^\[proxy\] listening on' "$D/proxy-$mode.log" && break; sleep 0.2; done
  grep -q '^\[proxy\] listening on' "$D/proxy-$mode.log" || {
    bad "$mode: the proxy never listened"; cat "$D/proxy-$mode.log"; return; }

  local good=0 i
  for i in $(seq 1 "$N"); do
    if XDG_RUNTIME_DIR="$D/rt" WAYLAND_DISPLAY=wl-proxy timeout 20 "$CLIENT" >"$D/cli.log" 2>&1 \
       && grep -q '^interface: ' "$D/cli.log"; then
      good=$((good + 1))
    else
      echo "  client $i failed:"; tail -3 "$D/cli.log" | sed 's/^/    /'
    fi
  done
  sleep 1
  local ringed sock fell
  ringed=$(grep -c 'connected, upstream ring slot' "$D/proxy-$mode.log")
  sock=$(grep -c 'connected, upstream .* connected' "$D/proxy-$mode.log")
  fell=$(grep -c 'SSH FALLBACK' "$D/proxy-$mode.log")
  echo "  $mode: $good of $N clients listed interfaces; $ringed on a ring, $sock on the socket, $fell fallback line(s)"
  [ "$good" = "$N" ] || bad "$mode: $((N - good)) client(s) failed"
  if [ "$mode" = rings ]; then
    [ "$ringed" = "$N" ] && ok "every connection on a ring ($N > 31 slots: recycled)" \
      || bad "$ringed of $N connections on a ring"
    [ "$fell" = 0 ] || bad "a fallback with the port up"
    grep -q 'wayland rings: 32 connection slots' "$D/srv-$mode.log" \
      && ok "compositor: $(grep -o 'wayland rings: .*' "$D/srv-$mode.log")" \
      || bad "the compositor did not keep rings"
    local max
    max=$(grep -o 'upstream ring slot [0-9]*' "$D/proxy-$mode.log" | awk '{print $4}' | sort -n | tail -1)
    ok "highest slot used: $max (of 31)"
    grep -E "rings' doorbell:" "$D/proxy-$mode.log" | sed 's/^/  /'
  else
    grep -q 'WAYLAND RINGS UNAVAILABLE' "$D/srv-$mode.log" \
      && ok "compositor said: $(grep -o 'WAYLAND RINGS UNAVAILABLE.*' "$D/srv-$mode.log" | cut -c1-90)" \
      || bad "the compositor did not say it had no rings"
    [ "$fell" -ge 1 ] && ok "proxy said: $(grep -m1 -o 'SSH FALLBACK.*' "$D/proxy-$mode.log" | cut -c1-90)" \
      || bad "the proxy fell back silently"
    [ "$sock" = "$N" ] || bad "$sock of $N on the socket"
  fi
  kill "$PROXY" "$SRV" ${RELAY:-} 2>/dev/null
  wait "$PROXY" "$SRV" ${RELAY:-} 2>/dev/null
  PROXY= SRV= RELAY=
}

leg rings
leg fallback
echo "logs: $D"
[ "$FAIL" = 0 ] && { echo "wc-rings: OK"; exit 0; }
echo "wc-rings: FAILED"; exit 1
