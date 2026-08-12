#!/usr/bin/env bash
# The **app-allocated** arena provider, end to end.
#
#   scripts/sv-memfd-path.sh [icd-manifest-basename]    default: lvp_icd.x86_64.json
#
# `sv-hostd` owns a memfd; `sv-gpu-server attach` imports it with
# `VK_EXT_external_memory_host`; `sv-compute-e2e` drives the whole stack through
# the real loader. This is the topology `testhost` needs — there the GPU side
# is inside a VM, no descriptor can cross that boundary, and the arena is guest
# RAM `sv-hostd` locates — with the locating step degenerating to "create a
# memfd and hand it out" on one machine (design.md §3).
#
# It is also the only path lavapipe can serve: lavapipe has no dma-buf export
# and its opaque-fd export is not the same pages, but it will adopt a host
# pointer. That makes this the configuration that needs no GPU at all, which is
# what §13 steps 1–6 are built around.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
ICD="${1:-lvp_icd.x86_64.json}"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
RUNDIR="$(mktemp -d /tmp/sv-memfd.XXXXXX)"
trap 'rm -rf "$RUNDIR"' EXIT

for b in sv-hostd sv-gpu-server sv-compute-e2e; do
  [ -x "$BIN/$b" ] || { echo "build first: cargo build --workspace" >&2; exit 1; }
done
SOCK="$RUNDIR/hostd.sock"

# `--live` so the daemon sizes the control region for the geometry the GPU side
# publishes. Without it the two disagree and `Control::create` refuses.
"$BIN/sv-hostd" serve --live --socket "$SOCK" --clients 2 --arena $((64 << 20)) \
  > "$RUNDIR/hostd.log" 2>&1 &
HOSTD=$!
for _ in $(seq 1 300); do [ -S "$SOCK" ] && break; sleep 0.05; done

VK_DRIVER_FILES="$ICDDIR/$ICD" "$BIN/sv-gpu-server" attach --socket "$SOCK" \
  > "$RUNDIR/gpu.log" 2>&1 &
GPU=$!
# The app side retries `NotReady` for ten seconds, so it may start immediately;
# a short pause only keeps the log readable.
sleep 1

env VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$SOCK" "$BIN/sv-compute-e2e"
RC=$?
sleep 0.5
kill $GPU $HOSTD 2>/dev/null

echo "--- sv-hostd ---"; sed 's/^/  /' "$RUNDIR/hostd.log"
echo "--- GPU side ---"; sed 's/^/  /' "$RUNDIR/gpu.log"
echo
[ $RC -eq 0 ] && echo "memfd provider: PASS" || echo "memfd provider: FAIL"
exit $RC
