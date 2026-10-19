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

. "$(dirname "${BASH_SOURCE[0]}")/site.sh"
set -uo pipefail

HOST=$SITE_HOST
GUEST=$SITE_GUEST
HOST_ALIAS=$SITE_HOST_KEYALIAS
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

# One run at a time, enforced rather than assumed.
#
# Two copies of this script racing is not a theoretical worry: the rendezvous is
# a directory of files with fixed names, so the second copy overwrites
# `host-gpu-fill.txt` with *its* stamp while the first is still waiting to
# verify against its own. The guest then reports "0 of 7864320 words" and the
# run looks like the arena failing, when what failed was the harness. That
# happened on the first real run of this script and it is exactly the shape of
# the false negative already recorded in measured-gpu-side.md — a number that
# comes out at zero deserving the same suspicion as one that comes out perfect.
exec 9>/tmp/.gpu-workload-run.lock
flock -n 9 || { echo "another gpu-workload-run.sh is already running"; exit 1; }

say() { echo "[$(date +%H:%M:%S)] $*"; }

# The guest's stdout comes back over the held-open ssh channel, so it lands here
# rather than in a file inside the guest.
GUESTLOG=${GUESTLOG:-/tmp/gpu-workload-guest.log}
: > "$GUESTLOG"

# Root on the host is reached by piping into ~/rootc, and every such call has to
# set PATH first because it starts from nothing.
# The script goes to `rootc` on *stdin*, not the output of running it as the
# user. Writing it the other way round — `{ $1; } | ~/rootc` — reads like the
# same thing and is not: it runs the command unprivileged and feeds whatever it
# printed to root, so `/proc/<qemu>/fd/*` comes back empty and a redirect into
# /srv/winshare fails with EACCES, both silently. It was that way until the
# first real run of this script, which found it immediately.
hroot() {
  # …and the script is piped in rather than heredoc'd inside the ssh argument,
  # so that `$p` and `$(readlink …)` in the caller's script are evaluated by the
  # root shell that will run them and not by this one.
  { printf 'export PATH=/run/current-system/sw/bin:/run/wrappers/bin\n'
    printf '%s\n' "$1"
  } | "${SSH_HOST[@]}" "$HOST" "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; ~/rootc"
}
huser() { "${SSH_HOST[@]}" "$HOST" "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; $1"; }

# `hroot` is only good for commands that finish in under half a second, and the
# way it fails is silent. `rootc` pipes the script into socat; once the script
# has been sent socat sees EOF and closes the connection after its default 0.5 s,
# which SIGHUPs the root shell and every child. Measured:
#
#     hroot 'date; sleep 3; date; echo survived'   -> prints the first date only
#
# The ~2 s arena scan is therefore killed part-way *every time*, leaving an empty
# `map.txt`, a `gpufill` that writes nothing, and a guest that reports "0 of
# 7864320 words" — which reads exactly like the arena not working. Every
# arena-host step below goes through `hrootl` instead: detach with nohup,
# redirect to a file, poll for a completion marker, fetch the output afterwards.
hrootl() {
  local tag=$1; shift
  hroot "rm -f /tmp/sv-$tag.log /tmp/sv-$tag.rc; nohup setsid bash -c '{ $*; } > /tmp/sv-$tag.log 2>&1; echo \$? > /tmp/sv-$tag.rc' > /dev/null 2>&1 < /dev/null &" > /dev/null
  local rc=""
  for _ in $(seq 1 600); do
    rc=$(hroot "cat /tmp/sv-$tag.rc 2>/dev/null" | tr -d '\r' | head -1)
    [ -n "$rc" ] && break
    sleep 1
  done
  hroot "cat /tmp/sv-$tag.log"
  return "${rc:-1}"
}

# ---------------------------------------------------------------------------
say "finding QEMU's memory-backend-memfd"
# The backing object is the fd whose /proc link names the memfd. Located rather
# than remembered: the pid and the fd number both change on every VM start, and
# a stale number would read another process's memory in silence (R21's shape).
FD=$(hroot 'p=$(pgrep -f "qemu-system-x86_64.*windows" | head -1); \
  for f in /proc/$p/fd/*; do \
    case "$(readlink $f)" in *memfd:memory-backend-memfd*) echo "$f"; break;; esac; \
  done' | tr -d '\r' | grep -m1 '^/proc/')
# `grep`, not `tail -1`: `rootc` prints the remote exit code on its own line and
# strips it again, which leaves a trailing empty line, so the last line of any
# `rootc` output is blank. `tail -1` therefore returned "" for a lookup that had
# in fact succeeded.
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
# The ssh session is **held open for the whole run**, in the background of this
# script, rather than detaching the probe with `nohup … &` and returning.
#
# Detaching was tried first and does not work here. Windows OpenSSH runs the
# command under a job object and tears the job down when the channel closes;
# msys `nohup` sets a signal disposition, which is not what a job-object kill
# respects. The observable symptom is exactly what it looks like when the probe
# crashes on startup: `guest.log` is created by the redirect and stays zero
# bytes, and nothing is ever in `tasklist`. Holding the channel open costs one
# idle ssh connection and removes the whole class of problem.
"${SSH_GUEST[@]}" "$GUEST" \
  "export MSYS_NO_PATHCONV=1; cd /z/probe 2>/dev/null || cd '$GUEST_WORKDIR'; \
   ./gpu-probe.exe $GARGS 2>&1" </dev/null > "$GUESTLOG" 2>&1 &
GUEST_SSH_PID=$!
trap 'kill $GUEST_SSH_PID 2>/dev/null' EXIT

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
# Unfiltered. Filtering the scan's output through `grep -E 'located|…'` hides
# the case that actually happened on the first real run: `arena-host` would not
# execute at all — a glibc binary built on another NixOS machine names an
# interpreter under a store path that does not exist here, and the error is
# `cannot execute: required file not found`. The grep swallowed it, `map.txt`
# was never written, `gpufill` wrote nothing, and the guest reported "0 of
# 7864320 words", which reads like the arena failing. Print what the tool says.
hrootl scan "/root/arena-host scan --fd $FD --run $RUN --out $SHARE/map.txt"
# And check the artefact rather than the exit status, because every step after
# this one silently does nothing without it.
PAGES_MAPPED=$(huser "grep -c . $SHARE/map.txt 2>/dev/null" | tr -d '\r' | head -1)
[ "${PAGES_MAPPED:-0}" -gt 0 ] 2>/dev/null || {
  echo "the scan produced no page map — nothing below this point would measure anything"; exit 1; }
say "map.txt has $PAGES_MAPPED lines"

STAMP=$(printf '%016x' $(( (RANDOM<<48) ^ (RANDOM<<32) ^ (RANDOM<<16) ^ RANDOM )))
say "host fills the input half, stamp=$STAMP"
hrootl gpufill "/root/arena-host gpufill --fd $FD --map $SHARE/map.txt --stamp $STAMP"
hroot "echo stamp=$STAMP > $SHARE/host-gpu-fill.txt"

# ---------------------------------------------------------------------------
say "waiting for the guest to dispatch and report (guest-gpu.txt)"
for _ in $(seq 1 300); do
  huser "test -s $SHARE/guest-gpu.txt" && break
  sleep 1
done
huser "cat $SHARE/guest-gpu.txt" || { echo "no guest-gpu.txt — see the guest log"; }

say "the Linux host now checks the same bytes through the memfd, independently"
hrootl gpuverify "/root/arena-host gpuverify --fd $FD --map $SHARE/map.txt --stamp $STAMP --run $RUN"

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
  hrootl gpuchurn "/root/arena-host gpuchurn --fd $FD --map $SHARE/map.txt --stampa $A --stampb $B \
         --seconds 10"
  for _ in $(seq 1 60); do
    huser "test -s $SHARE/guest-churn-done.txt" && break
    sleep 1
  done
  huser "cat $SHARE/guest-churn-done.txt"
fi

hroot "echo done=1 > $SHARE/host-done.txt"
say "waiting for the guest to exit and release the window"
# Wait on the process rather than on a fixed sleep: the window is 64 MiB of
# somebody's locked RAM and the only thing that gives it back is the probe
# exiting, so it is worth knowing that it did.
wait $GUEST_SSH_PID
trap - EXIT
say "----- the guest's log -----"
cat "$GUESTLOG"
say "done"
