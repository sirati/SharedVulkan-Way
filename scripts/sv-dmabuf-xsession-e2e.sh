#!/usr/bin/env bash
#
# **A dma-buf handed from one application to another through one GPU
# process** (waycompose-design.md §104.11): a Plasma client and KWin.
#
#   scripts/sv-dmabuf-xsession-e2e.sh
#
# The topology of scripts/sv-dmabuf-e2e.sh — `sv-gpu-server pool-guest` on
# lavapipe, socat for the tunnel, `sv-hostd ivshmem-serve` — with two
# application processes, each its own ICD session in the one GPU process:
#
#   producer   allocates an exportable image, keeps the GPU busy, clears the
#              image to X, exports its sync fd (at once) and passes the token
#              fd over a unix socket while the GPU is still working
#   consumer   imports it (a section another session made), acquires it as
#              Zink does, copies it on its own device: X, every pixel — the
#              GPU side made its submit wait for the producer's work; then
#              draws Y into it and hands back its fence record and number;
#              the producer waits on that record as the compositor would and
#              reads Y by name, and on its own device
#   lifetime   the producer frees first, the section survives; the consumer
#              frees, the section is gone
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SVDE_BIN:-$ROOT/target/release}
LVP=${SVDE_LVP:-${SV_MESA:-/run/opengl-driver}/share/vulkan/icd.d/lvp_icd.x86_64.json}
VKLOADER=${SVDE_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
RUN=$(mktemp -d "${SVDE_RUNROOT:-/mnt/datasets/builds/gad-runs}/xsession-XXXXXX")
PORT=${SVDE_PORT:-$((49000 + RANDOM % 1000))}
NODE=${SVDE_NODE:-/dev/null}
say() { echo "[$(date +%H:%M:%S)] $*"; }
KIDS=()
cleanup() { kill "${KIDS[@]}" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT
ulimit -c 0

truncate -s 256M "$RUN/gpu"
say "pool-guest on lavapipe, port $PORT, run dir $RUN"
VK_DRIVER_FILES="$LVP" "$BIN/sv-gpu-server" pool-guest --pool-file "$RUN/gpu" --port "$PORT" \
  --sessions 8 --idle 20 >"$RUN/gpu.log" 2>&1 &
GPID=$!
KIDS+=($GPID)
for _ in $(seq 1 100); do grep -q "publishing the extent" "$RUN/gpu.log" && break; sleep 0.1; done
grep -q "publishing the extent" "$RUN/gpu.log" || { echo "FAIL: the GPU side never listened"; cat "$RUN/gpu.log"; exit 1; }
socat "UNIX-LISTEN:$RUN/cold.sock,fork,reuseaddr" "TCP:127.0.0.1:$PORT" 2>"$RUN/socat.log" &
KIDS+=($!)
"$BIN/sv-hostd" ivshmem-serve --region "$RUN/gpu" --cold "$RUN/cold.sock" --socket "$RUN/icd.sock" \
  >"$RUN/hostd.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 50); do [ -S "$RUN/icd.sock" ] && break; sleep 0.1; done

export VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$RUN/icd.sock" SV_DRM_NODE="$NODE"

say "producer and consumer, two sessions of GPU process $GPID"
timeout 180 "$BIN/sv-dmabuf-xsession" producer "$RUN/x.sock" >"$RUN/producer.log" 2>&1 &
PP=$!
timeout 180 "$BIN/sv-dmabuf-xsession" consumer "$RUN/x.sock" >"$RUN/consumer.log" 2>&1; rc=$?
wait $PP; rp=$?
grep -v "^sv-icd: VK_" "$RUN/producer.log"
grep -v "^sv-icd: VK_" "$RUN/consumer.log"
sleep 2

FAIL=0
check() { if eval "$2"; then echo "  ok   $1"; else echo "  FAIL $1"; FAIL=1; fi; }
echo "---- verdict ----"
check "producer (exit $rp)" '[ $rp = 0 ] && grep -q "\[producer\] PASS" "$RUN/producer.log"'
check "consumer (exit $rc)" '[ $rc = 0 ] && grep -q "\[consumer\] PASS" "$RUN/consumer.log"'
check "two sessions, two fence records" '[ "$(grep -c "fence record /dev/shm/sv-fence-$GPID-" "$RUN/gpu.log")" -ge 2 ]'
check "the consumer imported the producer's section" 'grep -q "imported (type" "$RUN/gpu.log"'
check "no GPU-side wait gave up" '! grep -q "gave up after" "$RUN/gpu.log"'
check "the cross-session wait ran on the GPU side" 'grep -q "first wait on another session" "$RUN/gpu.log"'
check "no lost device anywhere" '! grep -qi "DEVICE_LOST\|lost device" "$RUN"/*.log'
check "no section left in /dev/shm by this GPU process (pid $GPID)" '! ls /dev/shm/sv-shared-$GPID-* >/dev/null 2>&1'
echo "---- the GPU side's account ----"
grep -E "shared section|fence record|sync fd export|another session|svFence|svImportShared|svAllocShared|session [0-9]+:" "$RUN/gpu.log" | cut -c1-300 | tail -20
echo "run dir: $RUN"
[ $FAIL = 0 ] && { say "sv-dmabuf-xsession-e2e: PASS"; rm -f "$RUN/gpu"; } || say "sv-dmabuf-xsession-e2e: FAIL"
exit $FAIL
