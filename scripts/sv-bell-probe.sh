#!/usr/bin/env bash
#
# How long does a doorbell take to cross a real hypervisor?
#
#   scripts/sv-bell-probe.sh [--count N]
#
# waycompose-design.md §36.6. The doorbell's own measurement — 9.26 ms for an
# eleven-wait present against 137.8 ms for the 20 ms slice — was taken with a
# `socketpair` standing in for the virtio-serial port. That stands in correctly
# for the *mechanism* and not at all for the *cost*: a socketpair write is a
# function call into the same kernel and a virtio-serial write is a vmexit.
#
# This runs the same ping-pong across a real VM boundary, on the development
# guest, which is the venue for exactly this: `nix build .#svGuest` boots a
# NixOS VM on this workstation whose RAM is a memfd and whose virtio-serial
# ports are the same mechanism testhost carries. Nobody else is using it and
# it can be killed at any point.
#
# Two ports on one controller: `org.sirati.svcold.0` is the cold path and
# `org.sirati.svbell.0` is the doorbell. They are separate because the cold path
# is framed and the doorbell deliberately is not (§36.6.3).
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RUNDIR=${SV_RUNDIR:-/tmp/sv-bell}
COUNT=2000
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --count) COUNT=$2; shift 2 ;;
    --keep) KEEP=1; shift ;;
    *) echo "unknown argument $1"; exit 2 ;;
  esac
done

say() { echo "[$(date +%H:%M:%S)] $*"; }

exec 9>/tmp/.sv-bell-probe.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

rm -rf "$RUNDIR"; mkdir -p "$RUNDIR"
export SV_RUNDIR="$RUNDIR"

VM=$(nix build "$ROOT#svGuest" --no-link --print-out-paths) || exit 1
say "guest image: $VM"

cleanup() {
  [ "$KEEP" = 1 ] && return
  # By pid, never by image name: this workstation runs other VMs and other
  # agents' processes, and a pattern kill is how you take one of theirs down.
  [ -n "${QEMU_PID:-}" ] && kill "$QEMU_PID" 2>/dev/null
  [ -n "${QEMU_PID:-}" ] && wait "$QEMU_PID" 2>/dev/null
}
trap cleanup EXIT

say "booting the guest (QMP + two virtio-serial ports)"
( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) > "$RUNDIR/guest-console.log" 2>&1 &
QEMU_PID=$!

say "waiting for the doorbell chardev"
for _ in $(seq 1 120); do
  [ -S "$RUNDIR/bell.sock" ] && break
  kill -0 $QEMU_PID 2>/dev/null || { echo "qemu died:"; tail -20 "$RUNDIR/guest-console.log"; exit 1; }
  sleep 1
done
[ -S "$RUNDIR/bell.sock" ] || { echo "no bell.sock"; tail -30 "$RUNDIR/guest-console.log"; exit 1; }

# The binary is copied into the shared directory rather than named by its path
# in this worktree: the guest sees /nix/store over 9p and $RUNDIR as /sv, and
# nothing else. It is dynamically linked against the store, so a copy runs there
# unchanged. Same reasoning as sv-guest-e2e.sh.
cp "$ROOT/target/debug/sv-bell-probe" "$RUNDIR/sv-bell-probe"
cat > "$RUNDIR/guest-cmd.sh" <<'GUEST'
/sv/sv-bell-probe guest --port /dev/virtio-ports/org.sirati.svbell.0 --seconds 300
GUEST
say "guest half: sv-bell-probe guest --port /dev/virtio-ports/org.sirati.svbell.0"

say "waiting for the guest half to open the port"
for _ in $(seq 1 180); do
  grep -q "guest echoing on" "$RUNDIR/guest.log" 2>/dev/null && break
  kill -0 $QEMU_PID 2>/dev/null || break
  sleep 1
done
if ! grep -q "guest echoing on" "$RUNDIR/guest.log" 2>/dev/null; then
  echo "the guest half never started"
  say "----- guest -----"; sed 's/^/  /' "$RUNDIR/guest.log" 2>/dev/null
  tail -30 "$RUNDIR/guest-console.log"
  exit 1
fi

say "measuring $COUNT round trips"
"$ROOT/target/debug/sv-bell-probe" host --port "$RUNDIR/bell.sock" --count "$COUNT"
RC=$?

say "----- guest -----"
sed 's/^/  /' "$RUNDIR/guest.log" 2>/dev/null
exit $RC
