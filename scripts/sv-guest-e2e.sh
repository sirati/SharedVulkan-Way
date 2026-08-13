#!/usr/bin/env bash
#
# The cross-VM topology, on a machine that can be rebooted.
#
#   scripts/sv-guest-e2e.sh
#
# Four processes, two kernels:
#
#   guest : sv-gpu-server guest --socket /dev/virtio-ports/org.sirati.svcold.0
#           allocates the arena out of *its own RAM*, pins it, stamps every page,
#           imports it into lavapipe, and publishes an ExtentDesc down the port.
#   host  : sv-hostd qemu-serve
#           reads `info mtree -f` over QMP, sweeps QEMU's memfd for the stamp,
#           coalesces, stitches, checks that extent offset n holds page n, and
#           then serves the fd + runs to ICD processes.
#   host  : vulkaninfo, through the real loader and our ICD.
#
# The check is the one `sv-end-to-end.sh` makes, across a hypervisor: the
# application must report the *guest's* driver name, which here is lavapipe.
# That string means the loader accepted our ICD, it encoded a command, the
# command crossed a ring in memory that belongs to another kernel, a driver in
# the VM answered, and the reply came back.
#
# Everything about this is disposable. The VM has no disk state, boots in a few
# seconds from the host's store, and can be killed at any point.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RUNDIR=${SV_RUNDIR:-/tmp/sv-guest}
ARENA=$((32 << 20))
KEEP=0
# §36.6. With --bell the guest attaches a doorbell on the second virtio-serial
# port and the ICD attaches to the host end of the same chardev; without it,
# every cross-VM wait runs out its 20 ms slice. The flag exists so the two can
# be measured against each other in one script rather than by editing it.
BELL=

while [ $# -gt 0 ]; do
  case "$1" in
    --arena) ARENA=$2; shift 2 ;;
    --keep) KEEP=1; shift ;;
    --bell) BELL=1; shift ;;
    *) echo "unknown argument $1"; exit 2 ;;
  esac
done

say() { echo "[$(date +%H:%M:%S)] $*"; }

exec 9>/tmp/.sv-guest-e2e.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

rm -rf "$RUNDIR"; mkdir -p "$RUNDIR"
export SV_RUNDIR="$RUNDIR"

VM=$(nix build "$ROOT#svGuest" --no-link --print-out-paths) || exit 1
say "guest image: $VM"

cleanup() {
  [ "$KEEP" = 1 ] && return
  kill "${QEMU_PID:-}" "${HOSTD_PID:-}" 2>/dev/null
  wait "${QEMU_PID:-}" 2>/dev/null
}
trap cleanup EXIT

say "booting the guest (2 GiB of memfd-backed RAM, QMP + virtio-serial)"
# The VM script wants a writable working directory for its ephemeral disk.
( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) > "$RUNDIR/guest-console.log" 2>&1 &
QEMU_PID=$!

say "waiting for QMP"
for _ in $(seq 1 120); do
  [ -S "$RUNDIR/qmp.sock" ] && break
  kill -0 $QEMU_PID 2>/dev/null || { echo "qemu died:"; tail -20 "$RUNDIR/guest-console.log"; exit 1; }
  sleep 1
done
[ -S "$RUNDIR/qmp.sock" ] || { echo "no QMP socket"; tail -30 "$RUNDIR/guest-console.log"; exit 1; }

say "waiting for the guest to reach a shell"
for _ in $(seq 1 180); do
  grep -q 'login:\|# $\|Welcome to NixOS' "$RUNDIR/guest-console.log" 2>/dev/null && break
  sleep 1
done

# The host half comes up first and waits on the cold path, so the guest can
# publish the moment it is ready.
say "starting sv-hostd qemu-serve"
"$ROOT/target/debug/sv-hostd" qemu-serve \
  --qmp "$RUNDIR/qmp.sock" --cold "$RUNDIR/cold.sock" \
  --socket "$RUNDIR/icd.sock" --vm sv-guest --clients 1 \
  > "$RUNDIR/hostd.log" 2>&1 &
HOSTD_PID=$!

say "starting the GPU side inside the guest"
# Left in the shared directory, not typed at the console. `nix/guest.nix`'s
# `sv-payload` unit waits for `/sv/guest-cmd.sh` and runs it — "a file appearing
# in a directory is a rendezvous both sides already understand" — and the console
# cannot serve as one, because the harness is reading the boot log through the
# same stdio it would have to type into.
#
# The binary is **copied** into the shared directory rather than named by its
# path in this worktree: the guest sees `/nix/store` over 9p and `$RUNDIR` as
# `/sv`, and nothing else. It is dynamically linked against the store, so a copy
# runs there unchanged.
cp "$ROOT/target/debug/sv-gpu-server" "$RUNDIR/sv-gpu-server"
GUEST_CMD="/sv/sv-gpu-server guest --socket /dev/virtio-ports/org.sirati.svcold.0 --arena $ARENA${BELL:+ --bell org.sirati.svbell.0}"
printf '%s\n' "$GUEST_CMD" > "$RUNDIR/guest-cmd.sh"
say "  $GUEST_CMD"

[ "$KEEP" = 1 ] && { say "--keep: leaving the VM up. kill $QEMU_PID when done."; exit 0; }

# The app side. This is the check the header describes and the script did not
# make: `sv-hostd` served nobody, so the run ended when the daemon's client count
# was never reached. Wait for the daemon to be listening — it prints the name the
# ICD is to open, which is the §1a handoff in one line — and then run the real
# loader against our ICD.
say "waiting for sv-hostd to offer the extent to ICD clients"
for _ in $(seq 1 300); do
  grep -q "listening on .* for .* ICD client" "$RUNDIR/hostd.log" 2>/dev/null && break
  kill -0 $HOSTD_PID 2>/dev/null || break
  sleep 1
done

say "vulkaninfo through the real loader, our ICD, and a GPU side in the VM${BELL:+, with a doorbell}"
# Timed, because §36.6 is a latency claim and this is the only place in the tree
# where a real application's commands cross a real hypervisor. Every one of them
# is a round trip the app side parks for, so the wall clock here *is* the sum of
# those waits plus the work — which is exactly the quantity the doorbell changes.
APP_T0=$(date +%s.%N)
env VK_DRIVER_FILES="$ROOT/target/debug/sv_icd.json" SV_SOCKET="$RUNDIR/icd.sock" \
  ${BELL:+SV_BELL="$RUNDIR/bell.sock"} \
  vulkaninfo --summary > "$RUNDIR/app.log" 2>&1
APP_RC=$?
APP_MS=$(awk "BEGIN{printf \"%.0f\", ($(date +%s.%N) - $APP_T0) * 1000}")
wait $HOSTD_PID
HOSTD_RC=$?

say "----- guest -----"
sed 's/^/  /' "$RUNDIR/guest.log" 2>/dev/null
say "----- sv-hostd -----"
sed 's/^/  /' "$RUNDIR/hostd.log"

REPORTED=$(grep -m1 "deviceName" "$RUNDIR/app.log" | sed 's/.*= //')
say "----- app side -----"
echo "  app side sees: ${REPORTED:-<nothing>}"

echo
# The guest runs lavapipe, so the application must report llvmpipe. Anything
# else means it answered from the app side rather than from the VM.
if [ $APP_RC -eq 0 ] && [ $HOSTD_RC -eq 0 ] && [[ "$REPORTED" == *llvmpipe* ]]; then
  echo "cross-VM: PASS — the application read a driver that lives in another kernel"
  echo "  app side took ${APP_MS} ms${BELL:+ (doorbell attached)}${BELL:- (no doorbell: every wait runs out its 20 ms slice)}"
  exit 0
fi
echo "cross-VM: FAIL (app=$APP_RC hostd=$HOSTD_RC device='${REPORTED:-none}')"
tail -20 "$RUNDIR/app.log" | sed 's/^/  /'
exit 1
