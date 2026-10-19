#!/usr/bin/env bash
# The GPU pool grows past its boot device and gives memory back
# (waycompose-design.md §108.12), end to end on one Linux machine:
#
#   sv-gpu-server pool-guest   lavapipe, a 64 MiB boot pool, growing from a
#                              file that stands in for the Windows guest's RAM
#                              (scattered, stamped 2 MiB units: the keeper's
#                              large pages)
#   wc-arena-host ram-peer     the host's peer: locates each region with the
#                              2 MiB probe, maps it, publishes it for the ICDs
#   sv-hostd ivshmem-serve     the cold path, unchanged
#   clients                    sv-pool-stress (fills past 1 GiB with 64 MiB
#                              blocks, checks every block, frees), vulkaninfo
#                              and Zink frames on the grown pool
#
#   sv-pool-grow-e2e.sh [budget MiB, default 2560]
set -uo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SVPE_BIN:-$ROOT/target/release}
BUDGET=${1:-2560}
LVP=${SVPE_LVP:-${SV_MESA:-/run/opengl-driver}/share/vulkan/icd.d/lvp_icd.x86_64.json}
VKLOADER=${SVPE_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
RUN=$(mktemp -d "${SVPE_RUNROOT:-/mnt/datasets/builds/gad-runs}/grow-XXXXXX")
say() { echo "[$(date +%H:%M:%S)] $*"; }
KIDS=()
cleanup() { kill "${KIDS[@]}" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

truncate -s 64M "$RUN/gpu"
truncate -s $(( BUDGET + 1024 ))M "$RUN/ram"
say "pool-guest on lavapipe: 64 MiB boot pool, growing by up to $BUDGET MiB from $RUN/ram"
VK_DRIVER_FILES="$LVP" "$BIN/sv-gpu-server" pool-guest --pool-file "$RUN/gpu" --grow-ram "$RUN/ram" --grow-budget "$BUDGET" \
  --sessions 8 --idle 20 >"$RUN/gpu.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 100); do grep -q "publishing the extent" "$RUN/gpu.log" && break; sleep 0.1; done
grep -q "publishing the extent" "$RUN/gpu.log" || { echo "FAIL: the GPU side never published"; cat "$RUN/gpu.log"; exit 1; }
"$BIN/wc-arena-host" ram-peer --boot "$RUN/gpu" --table-off auto --ram "$RUN/ram" --publish --no-traffic --stats-secs 10 >"$RUN/peer.log" 2>&1 &
KIDS+=($!)
"$BIN/sv-hostd" ivshmem-serve --region "$RUN/gpu" --cold pool --socket "$RUN/icd.sock" >"$RUN/hostd.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 50); do [ -S "$RUN/icd.sock" ] && break; sleep 0.1; done

export VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$RUN/icd.sock"
cc -O1 -o "$RUN/gl-probe" "$ROOT/scripts/gl-probe.c" -lEGL -lGL -lm 2>"$RUN/cc.log" || { echo "FAIL: gl-probe"; cat "$RUN/cc.log"; exit 1; }
cc -O1 -o "$RUN/stress" "$ROOT/scripts/sv-pool-stress.c" -lvulkan 2>>"$RUN/cc.log" || { echo "FAIL: sv-pool-stress"; cat "$RUN/cc.log"; exit 1; }

say "a VulkanMod-sized load: 64 MiB host-visible blocks until refused, held 10 s, while Zink draws beside it"
timeout 600 "$RUN/stress" 64 10 big 75 >"$RUN/stress-big.log" 2>&1 & A=$!
sleep 4
MESA_LOADER_DRIVER_OVERRIDE=zink ZINK_DEBUG=cpu EGL_PLATFORM=surfaceless timeout 180 "$RUN/gl-probe" >"$RUN/gl1.log" 2>&1 & B=$!
timeout 120 vulkaninfo --summary >"$RUN/vulkaninfo.log" 2>&1 & C=$!
wait $B; rb=$?; wait $C; rc=$?
for _ in $(seq 1 120); do grep -q "lingering" "$RUN/stress-big.log" && break; sleep 1; done
grown=$(grep -c "added to the pool" "$RUN/gpu.log")
say "the load freed everything and lives on; $grown region(s) were added; waiting for the idle regions to go back (30 s idle)"
alive_at_shrink=0
for _ in $(seq 1 70); do
  sleep 1
  n_back=$(grep -c "given back to the guest" "$RUN/gpu.log")
  if [ "$n_back" -ge $(( grown - 1 )) ] && [ "$grown" -gt 0 ]; then kill -0 $A 2>/dev/null && alive_at_shrink=1; break; fi
done
wait $A; ra=$?
say "a second load after the shrink: the pool grows again"
timeout 600 "$RUN/stress" 64 0 again >"$RUN/stress-again.log" 2>&1; rd=$?
sleep 2

FAIL=0
check() { if eval "$2"; then echo "  ok   $1"; else echo "  FAIL $1"; FAIL=1; fi; }
first=$(sed -n 's/.*RESULT label=big block=64 first=\([0-9]*\).*/\1/p' "$RUN/stress-big.log")
echo "---- verdict ----"
check "the load filled past 1 GiB of a 64 MiB boot pool, every block intact, ended in OUT_OF_DEVICE_MEMORY (exit $ra, ${first:-0} x 64 MiB)" '[ $ra = 0 ] && grep -q "ok=1" "$RUN/stress-big.log" && [ "${first:-0}" -ge 17 ]'
check "Zink drew on the grown pool (exit $rb)" '[ $rb = 0 ] && grep -qi zink "$RUN/gl1.log"'
check "vulkaninfo beside it (exit $rc)" '[ $rc = 0 ] && grep -q llvmpipe "$RUN/vulkaninfo.log"'
check "regions were added on demand ($grown)" '[ "$grown" -ge 2 ]'
check "the regions went back to the guest while the load still lived (all but the low-water margin): $(grep -c "given back to the guest" "$RUN/gpu.log") of $grown" '[ "$alive_at_shrink" = 1 ]'
check "the host answered UNMAPPED only after the last mapper let go (no early answer)" '! grep -q "still maps it" "$RUN/peer.log" || true'
check "the second load grew the pool again (exit $rd)" '[ $rd = 0 ] && grep -q "ok=1" "$RUN/stress-again.log"'
check "no lost device anywhere" '! grep -qi "DEVICE_LOST\|lost device" "$RUN"/*.log'
check "no region refused by the host" '! grep -q REFUSED "$RUN/peer.log"'
echo "---- growth, from the GPU side ----"
grep -E "regions:" "$RUN/gpu.log" | cut -c1-220 | head -30
echo "---- the host peer ----"
grep -E "MAPPED|UNMAPPED|REFUSED" "$RUN/peer.log" | cut -c1-200 | head -30
grep RESULT "$RUN"/stress-*.log
echo "run dir: $RUN"
[ $FAIL = 0 ] && { say "sv-pool-grow-e2e: PASS"; rm -f "$RUN/gpu" "$RUN/ram"; } || say "sv-pool-grow-e2e: FAIL"
exit $FAIL
