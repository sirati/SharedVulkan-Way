#!/usr/bin/env bash
#
# **The dma-buf path end to end, on one machine** (waycompose-design.md §104).
#
#   scripts/sv-dmabuf-e2e.sh
#
# The topology of scripts/sv-pool-e2e.sh — `sv-gpu-server pool-guest` on
# lavapipe (which imports a host pointer into a shared mapping as Intel does),
# socat for the tunnel, `sv-hostd ivshmem-serve` — with `SV_DRM_NODE` set for
# the clients. `/dev/null` stands in for the render node: the gate only wants a
# character device, and its st_rdev is what the ICD reports.
#
#   sv-dmabuf-e2e   Zink's calls, minus GL: export, token, import, clear,
#                   SYNC_FD, and the pixels read from /dev/shm by name
#   gl-probe        with SV_DRM_NODE set, twice: the sv-pool-e2e command, and
#                   on Zink over sv-icd (see below): nothing regressed
#   egl-dmabuf      EGL_EXT_image_dma_buf_import on Zink over sv-icd: a
#                   GBM-less KWin frame (see scripts/egl-dmabuf.c)
#
#   SVDE_EGL_NODE=/dev/dri/renderD129 SVDE_DEVICE_TYPE=integrated \
#     scripts/sv-dmabuf-e2e.sh      KWin's way of finding the device: Zink
#                                   matches the render node to this ICD by its
#                                   VkPhysicalDeviceDrmPropertiesEXT (the node
#                                   is only opened and probed, never used)
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SVDE_BIN:-$ROOT/target/release}
LVP=${SVDE_LVP:-${SV_MESA:-/run/opengl-driver}/share/vulkan/icd.d/lvp_icd.x86_64.json}
VKLOADER=${SVDE_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
RUN=$(mktemp -d "${SVDE_RUNROOT:-/mnt/datasets/builds/gad-runs}/dmabuf-XXXXXX")
PORT=${SVDE_PORT:-$((48000 + RANDOM % 1000))}
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

export VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$RUN/icd.sock"

say "sv-dmabuf-e2e, SV_DRM_NODE=$NODE"
SV_DRM_NODE=$NODE timeout 120 "$BIN/sv-dmabuf-e2e" >"$RUN/dmabuf.log" 2>&1; ra=$?
cat "$RUN/dmabuf.log"

cc -O1 -o "$RUN/gl-probe" "$ROOT/scripts/gl-probe.c" -lEGL -lGL -lm 2>"$RUN/cc.log" || { echo "FAIL: gl-probe"; cat "$RUN/cc.log"; exit 1; }
# Two ways to put Zink on this ICD without a render node it can match:
#  - plain surfaceless (the command sv-pool-e2e.sh runs): Mesa 26.2 opens the
#    machine's render nodes, Zink matches none of them to a device and EGL
#    falls back to llvmpipe — it passes, but it is not Zink, with or without
#    SV_DRM_NODE;
#  - LIBGL_ALWAYS_SOFTWARE=1: the fd-less screen, which takes a CPU-type
#    device, which this ICD over lavapipe is. That one is Zink on sv-icd.
# SVDE_EGL_NODE=/dev/dri/renderD<n> SVDE_DEVICE_TYPE=integrated is the third,
# KWin's: Zink matches the node to this ICD by VkPhysicalDeviceDrmPropertiesEXT.
say "gl-probe, surfaceless, SV_DRM_NODE set (the sv-pool-e2e command)"
SV_DRM_NODE=$NODE MESA_LOADER_DRIVER_OVERRIDE=zink ZINK_DEBUG=cpu EGL_PLATFORM=surfaceless timeout 180 \
  "$RUN/gl-probe" >"$RUN/gl.log" 2>&1; rb=$?
grep -E "renderer|gl-probe:" "$RUN/gl.log"
if [ -n "${SVDE_EGL_NODE:-}" ]; then
  ZENV=(SV_DRM_NODE="$SVDE_EGL_NODE" SV_DEVICE_TYPE="${SVDE_DEVICE_TYPE:-integrated}")
else
  ZENV=(SV_DRM_NODE="$NODE" LIBGL_ALWAYS_SOFTWARE=1)
fi
say "gl-probe on Zink over sv-icd: ${ZENV[*]}"
env "${ZENV[@]}" MESA_LOADER_DRIVER_OVERRIDE=zink EGL_PLATFORM=surfaceless timeout 180 \
  "$RUN/gl-probe" >"$RUN/gl-zink.log" 2>&1; rz=$?
grep -E "renderer|gl-probe:" "$RUN/gl-zink.log"

rc=skip
if cc -O1 -o "$RUN/egl-dmabuf" "$ROOT/scripts/egl-dmabuf.c" -lEGL -lGLESv2 2>>"$RUN/cc.log"; then
  say "egl-dmabuf: EGL_EXT_image_dma_buf_import on Zink over sv-icd: ${ZENV[*]}"
  env "${ZENV[@]}" MESA_LOADER_DRIVER_OVERRIDE=zink EGL_PLATFORM=surfaceless timeout 180 \
    "$RUN/egl-dmabuf" >"$RUN/egl.log" 2>&1; rc=$?
  grep -vE "^sv-icd: VK_|^MESA-EGL: warning: failed to open /dev/dri/card" "$RUN/egl.log" | tail -12
else
  echo "egl-dmabuf did not build:"; tail -5 "$RUN/cc.log"
fi
sleep 3

FAIL=0
check() { if eval "$2"; then echo "  ok   $1"; else echo "  FAIL $1"; FAIL=1; fi; }
echo "---- verdict ----"
check "sv-dmabuf-e2e (exit $ra)" '[ $ra = 0 ] && grep -q PASS "$RUN/dmabuf.log"'
check "gl-probe, surfaceless, SV_DRM_NODE set (exit $rb)" '[ $rb = 0 ] && grep -q "gl-probe: PASS" "$RUN/gl.log"'
check "gl-probe on Zink over sv-icd (exit $rz)" '[ $rz = 0 ] && grep -q "renderer.*zink" "$RUN/gl-zink.log"'
if [ "$rc" = skip ]; then echo "  skip egl-dmabuf"; else
  check "egl-dmabuf: Zink rendered into an imported dma-buf, pixels in the section (exit $rc)" '[ $rc = 0 ]'
  check "the import went through svImportShared" 'grep -q "imported (type" "$RUN/gpu.log"'
fi
check "no lost device anywhere" '! grep -qi "DEVICE_LOST\|lost device" "$RUN"/*.log'
check "no section left in /dev/shm by this GPU process (pid $GPID)" 'grep -q "sv-shared-$GPID-" "$RUN/gpu.log" && ! ls /dev/shm/sv-shared-$GPID-* >/dev/null 2>&1'
echo "---- the GPU side's account ----"
grep -E "shared section|svAllocShared|svImportShared|shared image" "$RUN/gpu.log" | cut -c1-300 | tail -20
echo "run dir: $RUN"
[ $FAIL = 0 ] && { say "sv-dmabuf-e2e: PASS"; rm -f "$RUN/gpu"; } || say "sv-dmabuf-e2e: FAIL"
exit $FAIL
