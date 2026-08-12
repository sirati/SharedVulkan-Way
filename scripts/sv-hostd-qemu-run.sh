#!/usr/bin/env bash
#
# Point `sv-hostd qemu` at a real guest, end to end.
#
# The guest stamps a window and waits; `sv-hostd` discovers QEMU, reads the
# region table off `info mtree -f`, locates the window by content, coalesces,
# stitches it into one contiguous view and checks that extent offset *n* holds
# the page the guest called *n*. Then the guest is released.
#
# Nothing about this is Windows-specific — it is the same command against a
# Linux guest — but the Windows guest is the one that exists today, and running
# it there first means the Linux-guest rig has a known-good reference.
set -uo pipefail

AGENT=""
KEY=/tmp/sv-k1.pub
PAGES=16384
MODE=large
QMP=/run/windows-vm/qmp.sock
VM_HINT=windows

while [ $# -gt 0 ]; do
  case "$1" in
    --agent) AGENT=$2; shift 2 ;;
    --key) KEY=$2; shift 2 ;;
    --pages) PAGES=$2; shift 2 ;;
    --mode) MODE=$2; shift 2 ;;
    --qmp) QMP=$2; shift 2 ;;
    *) echo "unknown argument $1"; exit 2 ;;
  esac
done

HERE=$(cd "$(dirname "$0")" && pwd)
PL="$HERE/pl.sh"
[ -n "$AGENT" ] && export SV_AGENT="$AGENT"

say() { echo "[$(date +%H:%M:%S)] $*"; }

exec 9>/tmp/.sv-hostd-qemu-run.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

say "clearing the rendezvous and starting the guest half ($PAGES pages, $MODE)"
"$PL" root "rm -f /srv/winshare/probe/guest-status.txt /srv/winshare/probe/host-gpu-fill.txt \
            /srv/winshare/probe/host-done.txt /srv/winshare/probe/guest-gpu.txt" > /dev/null

# Held open, not detached: Windows OpenSSH tears the job down when the channel
# closes, so `nohup … &` leaves nothing running (see gpu-workload-run.sh).
GUESTLOG=/tmp/sv-hostd-qemu-guest.log
: > "$GUESTLOG"
"$PL" guest "cd /z/probe && ./gpu-probe.exe --workdir Z:/probe --pages $PAGES --mode $MODE --no-perf 2>&1" \
  </dev/null > "$GUESTLOG" 2>&1 &
GUEST_PID=$!
trap 'kill $GUEST_PID 2>/dev/null' EXIT

say "waiting for the guest to stamp"
RUN=""
for _ in $(seq 1 120); do
  RUN=$("$PL" host "grep -m1 '^run_id=' /srv/winshare/probe/guest-status.txt 2>/dev/null" \
        | tr -d '\r' | cut -d= -f2)
  [ -n "$RUN" ] && break
  sleep 1
done
[ -n "$RUN" ] || { echo "the guest never stamped — see $GUESTLOG"; exit 1; }
say "run id $RUN"

say "sv-hostd qemu: discover, translate, locate, coalesce, stitch, verify"
"$PL" rootlong hostd \
  "/root/sv-hostd qemu --qmp $QMP --vm $VM_HINT --run $RUN \
     --control-pages 0 --arena-pages $PAGES --large --verify"
RC=$?

say "releasing the guest"
"$PL" root "echo done=1 > /srv/winshare/probe/host-done.txt" > /dev/null
kill $GUEST_PID 2>/dev/null
trap - EXIT
exit $RC
