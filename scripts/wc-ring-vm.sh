#!/usr/bin/env bash
#
# §36 across a real hypervisor: a Wayland client on the host, a compositor in a VM.
#
#   scripts/wc-ring-vm.sh
#
#   guest : wc-compositor --wayland-cold /dev/virtio-ports/org.sirati.svcold.0
#           commits a control region out of *its own RAM*, stamps every page,
#           publishes an ExtentDesc, and polls the ring directory for clients.
#   host  : sv-hostd qemu-serve — sweeps QEMU's memfd for the stamp, stitches,
#           and offers the region.
#   host  : wc-proxy --upstream-ring-attach — attaches, and claims a ring per
#           client.
#   host  : wayland-info, unmodified, through the proxy.
#
# The check: a Wayland client on this kernel drives a compositor in another one,
# and the only thing between them is memory. No socket, no TCP, no ssh and no
# probe-relay on the hop that crosses.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RUNDIR=${SV_RUNDIR:-/tmp/wc-ring-vm}
CLIENT=${WC_RING_CLIENT:-wayland-info}
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

say() { echo "[$(date +%H:%M:%S)] $*"; }
exec 9>/tmp/.wc-ring-vm.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

rm -rf "$RUNDIR"; mkdir -p "$RUNDIR/rt" "$RUNDIR/pools"
export SV_RUNDIR="$RUNDIR"

VM=$(nix build "$ROOT#svGuest" --no-link --print-out-paths) || exit 1

cleanup() {
  [ "$KEEP" = 1 ] && return
  # By pid. Never a pattern kill: other agents run on this machine.
  for p in "${PROXY:-}" "${HOSTD:-}" "${QEMU:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  wait 2>/dev/null
}
trap cleanup EXIT

say "booting the guest"
( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) > "$RUNDIR/console.log" 2>&1 &
QEMU=$!
for _ in $(seq 1 120); do [ -S "$RUNDIR/qmp.sock" ] && break; sleep 1; done
[ -S "$RUNDIR/qmp.sock" ] || { echo "FAIL: no QMP"; tail -20 "$RUNDIR/console.log"; exit 1; }

# The host half first, so it is already waiting on the cold path when the guest
# publishes — the guest speaks first and a listener that is not there yet is a
# handshake that hangs.
say "sv-hostd, waiting for the guest's ring region"
"$ROOT/target/debug/sv-hostd" qemu-serve \
  --qmp "$RUNDIR/qmp.sock" --cold "$RUNDIR/cold.sock" \
  --socket "$RUNDIR/wl.sock" --vm sv-guest --clients 1 \
  > "$RUNDIR/hostd.log" 2>&1 &
HOSTD=$!

cp "$ROOT/target/debug/wc-compositor" "$RUNDIR/wc-compositor"
cat > "$RUNDIR/guest-cmd.sh" <<'GUEST'
# The socket listener binds before the ring region is published, and it needs a
# runtime dir; a systemd unit is not a login shell and has none.
export XDG_RUNTIME_DIR=/tmp
/sv/wc-compositor --wayland-cold /dev/virtio-ports/org.sirati.svcold.0 \
  --socket wcring --timeout 90
GUEST
say "compositor in the guest, publishing its region"

for _ in $(seq 1 240); do
  grep -q "listening on .* for .* ICD client" "$RUNDIR/hostd.log" 2>/dev/null && break
  kill -0 $HOSTD 2>/dev/null || break
  sleep 1
done
if ! grep -q "listening on .* for .* ICD client" "$RUNDIR/hostd.log" 2>/dev/null; then
  echo "FAIL: sv-hostd never offered the region"
  say "----- guest -----"; sed 's/^/  /' "$RUNDIR/guest.log" 2>/dev/null
  say "----- hostd -----"; sed 's/^/  /' "$RUNDIR/hostd.log"
  exit 1
fi

say "proxy, attaching to the guest's RAM"
XDG_RUNTIME_DIR="$RUNDIR/rt" "$ROOT/target/debug/wc-proxy" \
  --upstream-ring-attach "$RUNDIR/wl.sock" --socket wl-proxy \
  --runtime-dir "$RUNDIR/rt" --pool-dir "$RUNDIR/pools" \
  --peer-prefix "$RUNDIR/pools" --no-icons \
  --accept-timeout 30 --timeout 30 > "$RUNDIR/proxy.log" 2>&1 &
PROXY=$!
for _ in $(seq 1 150); do grep -q '^\[proxy\] listening on' "$RUNDIR/proxy.log" && break; sleep 0.2; done
grep -q '^\[proxy\] listening on' "$RUNDIR/proxy.log" || {
  echo "FAIL: the proxy never listened"; sed 's/^/  /' "$RUNDIR/proxy.log"; exit 1; }

say "$CLIENT on this kernel, compositor in the other one"
XDG_RUNTIME_DIR="$RUNDIR/rt" WAYLAND_DISPLAY=wl-proxy timeout 30 "$CLIENT" \
  > "$RUNDIR/cli.log" 2>&1
CLI_RC=$?

echo
say "----- guest -----"; grep -E "guest RAM|located|accepting|ring slot" "$RUNDIR/guest.log" 2>/dev/null | sed 's/^/  /'
say "----- sv-hostd -----"; grep -E "offers run|located|stitched|listening|attached" "$RUNDIR/hostd.log" | sed 's/^/  /'
say "----- proxy -----"; grep -E "upstream is|ring slot|msgs" "$RUNDIR/proxy.log" | sed 's/^/  /'
IFACES=$(grep -cE "^interface: " "$RUNDIR/cli.log")
say "----- client -----"; echo "  rc=$CLI_RC, $IFACES interfaces"

echo
FAIL=0
[ "$CLI_RC" -eq 0 ] || { echo "FAIL: client exited $CLI_RC"; FAIL=1; }
[ "$IFACES" -ge 10 ] || { echo "FAIL: only $IFACES interfaces"; FAIL=1; }
grep -q "upstream is the guest's own RAM" "$RUNDIR/proxy.log" || { echo "FAIL: the proxy did not attach to guest RAM"; FAIL=1; }
grep -q "accepted on ring slot" "$RUNDIR/guest.log" 2>/dev/null || { echo "FAIL: the compositor accepted no ring"; FAIL=1; }

if [ $FAIL -eq 0 ]; then
  echo "cross-VM ring: PASS — a Wayland client drove a compositor in another kernel, over memory"
  exit 0
fi
echo "cross-VM ring: FAIL (logs in $RUNDIR)"
exit 1
