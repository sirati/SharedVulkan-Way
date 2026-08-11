#!/usr/bin/env bash
#
# Drive the whole GPU-workload measurement: guest, Linux host, and the
# rendezvous between them.
#
# Step 0's harness is documented as commands to type by hand, which was right
# when each phase wanted looking at. This one is a script because the phases are
# no longer interesting individually — the interesting thing is whether the GPU
# computed the right answer — and because the run has to be repeatable on a
# machine somebody is using, where a half-finished sequence leaves 64 MiB of
# their RAM locked until a probe times out.
#
# It runs on the build machine and reaches both sides over ssh. Nothing is
# built here; publish the binaries first (see --help).
#
# WHAT IT DOES TO A MACHINE SOMEBODY IS USING: allocates the window, dispatches
# short compute work on the GPU that drives the panel, and releases everything.
# The workload is bounded and the fence has a 2 s timeout. It never restarts the
# guest and never touches the display configuration.

set -uo pipefail

HOST=<host-user>@<host-ip>
GUEST=<guest-user>@<guest-ip>
HOST_ALIAS=testhost.lan
SHARE=/srv/winshare/probe
# Forward slashes on purpose. The guest shell is git-bash, and `Z:\probe` inside
# a double-quoted string that crosses two shells loses the backslash and becomes
# `Z:probe` — a valid-looking relative path that the probe would then fail to
# write to for a reason that has nothing to do with anything being measured.
# Windows accepts `Z:/probe` everywhere the probe uses it.
GUEST_WORKDIR='Z:/probe'
PAGES=16384
MODE=large
CONCURRENT=0
ITERS=7
AGENT=""
KEY=""

usage() {
  cat <<'EOF'
usage: gpu-workload-run.sh --agent <ssh-agent-socket> [options]

  --agent <path>   ssh agent socket that can authenticate to both machines.
                   They rotate; find a live one with
                     for s in ~/.ssh/agent/s.*; do \
                       n=$(SSH_AUTH_SOCK=$s ssh-add -l 2>/dev/null | wc -l); \
                       [ "$n" -gt 0 ] && echo "$s"; done
  --key <file>     public key to pin for the guest. Windows sshd caps at six
                   authentication attempts and the agent offers six keys, so an
                   unpinned connection can fail for a reason that looks like a
                   wrong key. Defaults to the first key the agent lists.
  --pages N        window in 4 KiB pages (default 16384 = 64 MiB)
  --mode M         large | locked | awe | awe2  (default large)
  --iters N        timing repetitions (default 7)
  --concurrent     also run the host-writes-while-the-GPU-reads phase
  --help

publish first, from the dev shell:
  cargo build -p gpu-probe --release --target x86_64-pc-windows-gnu
  cargo build -p arena-host --release
  scp target/x86_64-pc-windows-gnu/release/gpu-probe.exe target/release/arena-host <host>:/tmp/
  # then, as root on the host:
  install -m0755 /tmp/gpu-probe.exe /srv/winshare/probe/
  install -m0755 /tmp/arena-host /root/arena-host
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --agent) AGENT=$2; shift 2 ;;
    --key) KEY=$2; shift 2 ;;
    --pages) PAGES=$2; shift 2 ;;
    --mode) MODE=$2; shift 2 ;;
    --iters) ITERS=$2; shift 2 ;;
    --concurrent) CONCURRENT=1; shift ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown argument $1"; usage; exit 2 ;;
  esac
done
[ -n "$AGENT" ] || { echo "--agent is required"; usage; exit 2; }

SSH_HOST=(ssh -o IdentityAgent="$AGENT" -o HostKeyAlias="$HOST_ALIAS"
          -o HostKeyAlgorithms=rsa-sha2-512,rsa-sha2-256 -o BatchMode=yes)

if [ -z "$KEY" ]; then
  KEY=$(mktemp /tmp/gpu-run-key.XXXXXX.pub)
  SSH_AUTH_SOCK="$AGENT" ssh-add -L | head -1 > "$KEY"
fi
SSH_GUEST=(ssh -o IdentityAgent="$AGENT" -o IdentitiesOnly=yes -i "$KEY" -o BatchMode=yes)

say() { echo "[$(date +%H:%M:%S)] $*"; }

# Root on the host is reached by piping into ~/rootc, and every such call has to
# set PATH first because it starts from nothing.
hroot() { "${SSH_HOST[@]}" "$HOST" "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; { $1; } | ~/rootc"; }
huser() { "${SSH_HOST[@]}" "$HOST" "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; $1"; }

# ---------------------------------------------------------------------------
say "finding QEMU's memory-backend-memfd"
# The backing object is the fd whose /proc link names the memfd. Located rather
# than remembered: the pid and the fd number both change on every VM start, and
# a stale number would read another process's memory in silence (R21's shape).
FD=$(hroot 'p=$(pgrep -f "qemu-system-x86_64.*windows" | head -1); \
  for f in /proc/$p/fd/*; do \
    case "$(readlink $f)" in *memfd:memory-backend-memfd*) echo "$f"; break;; esac; \
  done' | tr -d '\r' | tail -1)
[ -n "$FD" ] || { echo "could not find the memfd — is the VM running?"; exit 1; }
say "memfd is $FD"

say "clearing the rendezvous directory"
hroot "rm -f $SHARE/guest-status.txt $SHARE/guest-gpu.txt $SHARE/host-gpu-fill.txt \
       $SHARE/host-done.txt $SHARE/host-churn.txt $SHARE/guest-churn-ready.txt \
       $SHARE/guest-churn-done.txt $SHARE/map.txt" >/dev/null

# ---------------------------------------------------------------------------
say "starting gpu-probe in the guest ($PAGES pages, mode $MODE)"
GARGS="--workdir $GUEST_WORKDIR --pages $PAGES --mode $MODE --iters $ITERS"
[ "$CONCURRENT" = 1 ] && GARGS="$GARGS --concurrent"
# Detached, because the guest half waits for us between phases and an ssh
# session that dies would take it with it. `< /dev/null` matters as much as the
# nohup: a child still holding the session's stdin keeps it open, and ssh then
# refuses to return.
"${SSH_GUEST[@]}" "$GUEST" \
  "export MSYS_NO_PATHCONV=1; cd /z/probe 2>/dev/null || cd '$GUEST_WORKDIR'; \
   nohup ./gpu-probe.exe $GARGS > guest.log 2>&1 < /dev/null &" \
  >/dev/null 2>&1 &

say "waiting for guest-status.txt"
for _ in $(seq 1 120); do
  RUN=$(huser "grep -m1 '^run_id=' $SHARE/guest-status.txt 2>/dev/null" | tr -d '\r' | cut -d= -f2)
  [ -n "$RUN" ] && break
  sleep 1
done
[ -n "$RUN" ] || { echo "the guest never wrote guest-status.txt"; exit 1; }
say "run id $RUN"
huser "cat $SHARE/guest-status.txt | head -8"

# ---------------------------------------------------------------------------
say "locating the guest's pages in the memfd (this reads all of guest RAM, ~2 s)"
hroot "/root/arena-host scan --fd $FD --run $RUN --out $SHARE/map.txt 2>&1 | \
       grep -E 'located|contiguous|note:'"

STAMP=$(printf '%016x' $(( (RANDOM<<48) ^ (RANDOM<<32) ^ (RANDOM<<16) ^ RANDOM )))
say "host fills the input half, stamp=$STAMP"
hroot "/root/arena-host gpufill --fd $FD --map $SHARE/map.txt --stamp $STAMP 2>&1 | grep gpufill"
hroot "echo stamp=$STAMP > $SHARE/host-gpu-fill.txt"

# ---------------------------------------------------------------------------
say "waiting for the guest to dispatch and report (guest-gpu.txt)"
for _ in $(seq 1 300); do
  huser "test -s $SHARE/guest-gpu.txt" && break
  sleep 1
done
huser "cat $SHARE/guest-gpu.txt" || { echo "no guest-gpu.txt — see the guest log"; }

say "the Linux host now checks the same bytes through the memfd, independently"
hroot "/root/arena-host gpuverify --fd $FD --map $SHARE/map.txt --stamp $STAMP --run $RUN 2>&1 | \
       grep -v '^memfd'"

# ---------------------------------------------------------------------------
if [ "$CONCURRENT" = 1 ]; then
  say "waiting for the guest to be ready for the churn"
  for _ in $(seq 1 120); do
    huser "test -s $SHARE/guest-churn-ready.txt" && break
    sleep 1
  done
  A=$(printf '%016x' $(( (RANDOM<<32) ^ RANDOM ^ 0xA000000 )))
  B=$(printf '%016x' $(( (RANDOM<<32) ^ RANDOM ^ 0xB000000 )))
  say "host churns the input between $A and $B for 10 s while the GPU reads it"
  hroot "{ echo stampa=$A; echo stampb=$B; echo seconds=10; } > $SHARE/host-churn.txt"
  hroot "/root/arena-host gpuchurn --fd $FD --map $SHARE/map.txt --stampa $A --stampb $B \
         --seconds 10 2>&1 | grep -E 'gpuchurn|WARNING'"
  for _ in $(seq 1 60); do
    huser "test -s $SHARE/guest-churn-done.txt" && break
    sleep 1
  done
  huser "cat $SHARE/guest-churn-done.txt"
fi

hroot "echo done=1 > $SHARE/host-done.txt"
say "waiting for the guest to exit and release the window"
sleep 6
say "----- the guest's log -----"
huser "cat $SHARE/guest.log 2>/dev/null" || \
  "${SSH_GUEST[@]}" "$GUEST" "cat /z/probe/guest.log" 2>/dev/null
say "done"
