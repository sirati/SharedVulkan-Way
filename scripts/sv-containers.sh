#!/usr/bin/env bash
# Run the two ends of shared_vulkan in two containers on this machine.
#
#   scripts/sv-containers.sh            build and run the whole thing
#   scripts/sv-containers.sh --no-build use whatever is in target/debug
#
# This is the Linux-host / Linux-guest topology, not a simulation of it. The two
# sides share two things and no more: a Unix socket, over which `sv-hostd` sends
# the *name* of the arena object (docs/design.md §1a — no descriptor crosses), and the
# `/dev/shm` namespace that name is resolved in. Everything else — PID namespace,
# IPC namespace, UTS namespace, /tmp — is separate, so a side that reached for
# anything it was not given would fail here rather than in a VM three stages
# later.
#
# THE /dev/shm BIND IS THE POINT, NOT PLUMBING. Before §1a the containers needed
# no shared memory namespace at all, because the descriptor arrived over the
# socket and a descriptor names nothing. A name is only worth anything to a peer
# that can resolve it, so the rule that removed the descriptor added a
# requirement: **the two sides must share a namespace in which the name means the
# same object.** Two containers share `/dev/shm` by bind mount; two machines
# share nothing, which is exactly why the cross-VM topology does not use a name
# at all — there the guest allocates and the host *locates by content* (§5.1),
# and that is the strongest form of the rule rather than an exception to it.
#
# WHY BUBBLEWRAP AND NOT PODMAN. The user asked for podman; this machine has
# neither podman nor docker installed, and bubblewrap 0.11.2 is already present.
# bwrap is the better fit anyway for what is being tested: it is rootless with no
# daemon, and its model *is* "a chroot directory with some bind mounts", which is
# how the request was phrased. Nothing here depends on bwrap specifically — the
# same three commands run under `podman run --userns=keep-id -v $SOCKDIR:/run/sv`
# if podman is ever installed, and the socket bind mount is the only requirement.
#
# WHAT IT PROVES, which the single-process tests cannot:
#   * a *name* really is enough across a process and namespace boundary, and the
#     two sides really do map the same physical pages at different addresses;
#   * the app side writes a pattern through its own mapping and sends only
#     (offset, len) — the GPU side's checksum over its own mapping agrees;
#   * the R26 canary survives a full session of allocation and is re-checked;
#   * the RingBarrier orders a queue-ring packet after two device-ring packets
#     across two address spaces.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOCKDIR="$(mktemp -d /tmp/sv-containers.XXXXXX)"
trap 'rm -rf "$SOCKDIR"' EXIT

die() { echo "error: $*" >&2; exit 1; }

command -v bwrap >/dev/null || die "bubblewrap (bwrap) is not on PATH; enter 'nix develop' first"

if [ "${1:-}" != "--no-build" ]; then
  echo "== building =="
  ( cd "$ROOT" && cargo build -p sv-hostd ) || die "build failed"
fi

BIN="$ROOT/target/debug"
[ -x "$BIN/sv-hostd" ] || die "$BIN/sv-hostd not built"
[ -x "$BIN/sv-loopback" ] || die "$BIN/sv-loopback not built"

# One container. The nix store is read-only because the binaries link against
# it; target/debug is read-only because a container has no business writing to
# the build tree; $SOCKDIR and /dev/shm are the only writable, *shared* things.
#
# ORDER MATTERS: `--dev /dev` mounts a fresh tmpfs over /dev, which would bury an
# earlier /dev/shm bind. The bind comes after it, and the failure if it does not
# is silent in the worst way — each container gets its *own* /dev/shm, the
# daemon's name resolves in neither, and the client reports "cannot open".
run_in_container() {
  local name="$1"; shift
  bwrap \
    --ro-bind /nix /nix \
    --ro-bind "$BIN" /opt/sv \
    --bind "$SOCKDIR" /run/sv \
    --proc /proc \
    --dev /dev \
    --bind /dev/shm /dev/shm \
    --tmpfs /tmp \
    --unshare-pid --unshare-ipc --unshare-uts \
    --hostname "$name" \
    --die-with-parent \
    --setenv SV_CONTAINER "$name" \
    -- "$@"
}

echo "== two containers, one named object =="
echo "   socket directory (shared): $SOCKDIR"

run_in_container hostd /opt/sv/sv-hostd serve --socket /run/sv/hostd.sock --clients 2 \
  2>&1 | sed 's/^/[hostd] /' &
HOSTD=$!

# Wait for the socket rather than sleeping a guessed amount.
for _ in $(seq 1 200); do
  [ -S "$SOCKDIR/hostd.sock" ] && break
  sleep 0.05
done
[ -S "$SOCKDIR/hostd.sock" ] || die "sv-hostd never created its socket"

run_in_container gpu /opt/sv/sv-loopback gpu --socket /run/sv/hostd.sock \
  2>&1 | sed 's/^/[gpu-side] /' &
GPU=$!

run_in_container app /opt/sv/sv-loopback app --socket /run/sv/hostd.sock \
  2>&1 | sed 's/^/[app-side] /'
APP_RC=$?

wait $GPU; GPU_RC=$?
wait $HOSTD; HOSTD_RC=$?

echo
if [ $APP_RC -eq 0 ] && [ $GPU_RC -eq 0 ] && [ $HOSTD_RC -eq 0 ]; then
  echo "PASS — the arena crossed two container boundaries and both sides agree"
  exit 0
fi
echo "FAIL — hostd=$HOSTD_RC gpu=$GPU_RC app=$APP_RC"
exit 1
