#!/usr/bin/env bash
#
# **The guest's pool path, end to end, on one machine** (waycompose-design.md
# §97).
#
#   scripts/sv-pool-e2e.sh [pool MiB]          default 256, testhost's device
#
# Every process of the deployed topology but the VM:
#
#   sv-gpu-server pool-guest   the guest's GPU process, serving every session
#                              from one pool, with a file standing in for the
#                              ivshmem BAR and lavapipe for Intel (both import
#                              a host pointer into a shared mapping; RADV
#                              refuses one, docs/design.md §5.4)
# Zink is told to take a CPU device (`ZINK_DEBUG=cpu`): the forwarded device
# is lavapipe here, which Zink otherwise skips ("failed to choose pdev").
#
#   sv-hostd ivshmem-serve     the VM host's side: claims each session's offer
#                              in the pool's mailboxes (--cold pool), checks its
#                              stamps and the pool header's nonce, hands the ICD
#                              the file by name. No socket joins the two sides
#                              (SVPE_COLD=tcp: the TCP debug path through socat)
#   sv-hostd bell-hub, socat   with SVPE_BELL=1: the doorbell, socat standing in
#                              for QEMU's chardev
#   clients                    two vulkaninfo, two gl-probe frames on Zink,
#                              and two sv-pool-stress fills of the pool at once
#
# It passes when every client passes, the stress fills end in
# VK_ERROR_OUT_OF_DEVICE_MEMORY (never a lost device), nothing a process mapped
# was touched by the other, and the pool is back to empty (its header granule,
# and the offer waiting in a mailbox, whose control region it holds) when
# every session has ended.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SVPE_BIN:-$ROOT/target/release}
POOL_MIB=${1:-256}
# The dev shell's Mesa: the system one is linked against a newer glibc than
# `sv-gpu-server`, and the loader refuses it (`sv-firefox-wayland.sh` says so too).
LVP=${SVPE_LVP:-${SV_MESA:-/run/opengl-driver}/share/vulkan/icd.d/lvp_icd.x86_64.json}
VKLOADER=${SVPE_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
RUN=$(mktemp -d "${SVPE_RUNROOT:-/mnt/datasets/builds/gad-runs}/pool-XXXXXX")
PORT=${SVPE_PORT:-$((47000 + RANDOM % 1000))}
say() { echo "[$(date +%H:%M:%S)] $*"; }
KIDS=()
cleanup() { kill "${KIDS[@]}" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

truncate -s "${POOL_MIB}M" "$RUN/gpu"
# SVPE_COLD=pool (the default, the deployed path): the cold path is the pool's
# mailboxes, and nothing but the pool file joins the two sides.
# SVPE_COLD=tcp: the TCP debug path, with socat for the old ssh forward.
COLD=${SVPE_COLD:-pool}
GUESTARGS=(); HOSTARGS=(--cold pool)
if [ "$COLD" = tcp ]; then
  GUESTARGS=(--port "$PORT"); HOSTARGS=(--cold "$RUN/cold.sock")
fi
# SVPE_BELL=1: a doorbell, as deployed: socat stands in for QEMU's chardev (the
# hub's end and the guest's end of one port), `sv-hostd bell-hub` holds it,
# and every ICD and sv-hostd are its clients.
if [ "${SVPE_BELL:-0}" = 1 ]; then
  mkdir -p "$RUN/hub"
  socat "UNIX-LISTEN:$RUN/svbell.sock" "UNIX-LISTEN:$RUN/guestbell.sock" 2>"$RUN/socat-bell.log" &
  KIDS+=($!)
  for _ in $(seq 1 50); do [ -S "$RUN/svbell.sock" ] && break; sleep 0.1; done
  "$BIN/sv-hostd" bell-hub --port "$RUN/svbell.sock=$RUN/hub/sv.sock" >"$RUN/hub.log" 2>&1 &
  KIDS+=($!)
  for _ in $(seq 1 50); do [ -S "$RUN/guestbell.sock" ] && [ -S "$RUN/hub/sv.sock" ] && break; sleep 0.1; done
  GUESTARGS+=(--bell "socket:$RUN/guestbell.sock")
  HOSTARGS+=(--cold-bell "$RUN/hub/sv.sock")
  export SV_BELL="$RUN/hub/sv.sock"
fi
say "pool-guest on lavapipe, a ${POOL_MIB} MiB pool in $RUN/gpu, cold path $COLD, bell ${SVPE_BELL:-0}"
VK_DRIVER_FILES="$LVP" "$BIN/sv-gpu-server" pool-guest --pool-file "$RUN/gpu" "${GUESTARGS[@]}" \
  --sessions 8 --idle 20 >"$RUN/gpu.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 100); do grep -q "publishing the extent" "$RUN/gpu.log" && break; sleep 0.1; done
grep -q "publishing the extent" "$RUN/gpu.log" || { echo "FAIL: the GPU side never published"; cat "$RUN/gpu.log"; exit 1; }
if [ "$COLD" = tcp ]; then
  socat "UNIX-LISTEN:$RUN/cold.sock,fork,reuseaddr" "TCP:127.0.0.1:$PORT" 2>"$RUN/socat.log" &
  KIDS+=($!)
fi
"$BIN/sv-hostd" ivshmem-serve --region "$RUN/gpu" "${HOSTARGS[@]}" --socket "$RUN/icd.sock" \
  >"$RUN/hostd.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 50); do [ -S "$RUN/icd.sock" ] && break; sleep 0.1; done

export VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$RUN/icd.sock"
cc -O1 -o "$RUN/gl-probe" "$ROOT/scripts/gl-probe.c" -lEGL -lGL -lm 2>"$RUN/cc.log" || { echo "FAIL: gl-probe"; cat "$RUN/cc.log"; exit 1; }
cc -O1 -o "$RUN/stress" "$ROOT/scripts/sv-pool-stress.c" -lvulkan 2>>"$RUN/cc.log" || { echo "FAIL: sv-pool-stress"; cat "$RUN/cc.log"; exit 1; }

say "clients: two vulkaninfo, two gl-probe frames on Zink, at once"
timeout 120 vulkaninfo --summary >"$RUN/vulkaninfo.log" 2>&1 & A=$!
timeout 120 vulkaninfo >"$RUN/vulkaninfo-full.log" 2>&1 & B=$!
MESA_LOADER_DRIVER_OVERRIDE=zink ZINK_DEBUG=cpu EGL_PLATFORM=surfaceless timeout 180 "$RUN/gl-probe" >"$RUN/gl1.log" 2>&1 & C=$!
MESA_LOADER_DRIVER_OVERRIDE=zink ZINK_DEBUG=cpu EGL_PLATFORM=surfaceless timeout 180 "$RUN/gl-probe" >"$RUN/gl2.log" 2>&1 & D=$!
wait $A; ra=$?; wait $B; rb=$?; wait $C; rc=$?; wait $D; rd=$?

say "two fills of the pool at once: 8 MiB blocks held 8 s, and 2 MiB blocks"
timeout 180 "$RUN/stress" 8 8 big >"$RUN/stress-big.log" 2>&1 & E=$!
sleep 3
timeout 180 "$RUN/stress" 2 0 small >"$RUN/stress-small.log" 2>&1 & F=$!
wait $E; re=$?; wait $F; rf=$?
say "after both: one more fill, alone, to see the pool came back"
timeout 180 "$RUN/stress" 8 0 after >"$RUN/stress-after.log" 2>&1; rg=$?

# Every session ends within the GPU side's own idle limit after its process.
sleep 3

FAIL=0
check() { if eval "$2"; then echo "  ok   $1"; else echo "  FAIL $1"; FAIL=1; fi; }
echo "---- verdict ----"
check "two vulkaninfo through the pool (exit $ra, $rb)" '[ $ra = 0 ] && [ $rb = 0 ] && grep -q llvmpipe "$RUN/vulkaninfo.log"'
check "gl-probe frames judged good, on Zink over the pool (exit $rc, $rd)" '[ $rc = 0 ] && [ $rd = 0 ] && grep -qi "zink" "$RUN/gl1.log" "$RUN/gl2.log"'
check "the 8 MiB fill ended in OUT_OF_DEVICE_MEMORY, untouched (exit $re)" '[ $re = 0 ] && grep -q "ok=1" "$RUN/stress-big.log"'
check "the 2 MiB fill beside it did too (exit $rf)" '[ $rf = 0 ] && grep -q "ok=1" "$RUN/stress-small.log"'
check "a fill after both got the pool back (exit $rg)" '[ $rg = 0 ] && grep -q "ok=1" "$RUN/stress-after.log"'
check "no lost device anywhere" '! grep -qi "DEVICE_LOST\|lost device" "$RUN"/*.log'
check "every session gave its leases back" '[ "$(grep -c "lease(s) and the control region given back" "$RUN/gpu.log")" -ge 7 ]'
if [ "$COLD" = tcp ]; then
  check "the pool ended empty" 'grep "finished\|ended" "$RUN/gpu.log" | tail -1 | grep -q "pool 0.1/"'
else
  # The one account left is the waiting offer's, holding its control region only.
  check "the pool ended empty but for the waiting offer" 'L=$(grep "finished\|ended" "$RUN/gpu.log" | tail -1); [ "$(grep -o "; session " <<<"$L" | wc -l)" = 1 ] && grep -q "\[1 control [0-9.]*\]" <<<"$L"'
  check "no socket to the guest: every ICD attached through a mailbox" '[ "$(grep -c "attached:.*(mailbox" "$RUN/hostd.log")" -ge 7 ] && ! grep -q "TCP" "$RUN/hostd.log" "$RUN/gpu.log"'
fi
echo "---- the cold path, per ICD (sv-hostd) ----"
grep -o "attached: .*" "$RUN/hostd.log" | sed 's/.*run 0x[0-9a-f]*, //' | cut -c1-200
echo "---- the pool's own account ----"
grep -E "pool (of|high water)|refused|finished|given back" "$RUN/gpu.log" | cut -c1-400 | tail -25
grep RESULT "$RUN"/stress-*.log
echo "run dir: $RUN"
[ $FAIL = 0 ] && { say "sv-pool-e2e: PASS"; rm -f "$RUN/gpu"; } || say "sv-pool-e2e: FAIL"
exit $FAIL
