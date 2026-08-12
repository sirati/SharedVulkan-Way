#!/usr/bin/env bash
# The benchmark design.md §13 step 1 asks for, run twice: once through this
# project's forwarded Vulkan and once against the real driver with no forwarding
# at all.
#
#   scripts/sv-bench.sh [icd-manifest-basename]   default: radeon_icd.x86_64.json
#
# The control run is the whole point. A forwarding design's numbers are
# meaningless in isolation — "1.4 ms a frame" is either terrible or the GPU's own
# floor depending on what the driver alone costs — and §0.6 item 7 is the
# standing argument for measuring the unforwarded case before believing anything
# about the forwarded one.
#
# It also reports `glxgears`. A microbenchmark that improves while glxgears does
# not is not an improvement, and glxgears is the one number here that comes from
# a program nobody wrote for this project.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The other harnesses hard-code `target/debug` and this one follows them by
# default, so the numbers it prints are the numbers those harnesses produce.
# `SV_BENCH_BIN=target/release` is the second data point: an unoptimised build of
# 126,205 lines of generated marshalling is not the same experiment.
BIN="$ROOT/${SV_BENCH_BIN:-target/debug}"
ICD="${1:-radeon_icd.x86_64.json}"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
RUNDIR="$(mktemp -d /tmp/sv-bench.XXXXXX)"
MANIFEST="$BIN/sv_icd.json"
SOCK="$RUNDIR/gpu.sock"

[ -f "$MANIFEST" ] || { echo "build first: cargo build -p sv-icd" >&2; exit 1; }
[ -x "$BIN/sv-gpu-server" ] || { echo "build first: cargo build -p sv-gpu-server" >&2; exit 1; }

GPUPID=""
cleanup() {
  [ -n "$GPUPID" ] && kill "$GPUPID" 2>/dev/null
  rm -rf "$RUNDIR"
}
trap cleanup EXIT

start_gpu_side() {
  VK_DRIVER_FILES="$ICDDIR/$ICD" "$BIN/sv-gpu-server" serve \
    --socket "$SOCK" --arena $((256 << 20)) --clients 1 \
    > "$RUNDIR/gpu.log" 2>&1 &
  GPUPID=$!
  for _ in $(seq 1 400); do
    [ -S "$SOCK" ] && return 0
    kill -0 $GPUPID 2>/dev/null || return 1
    sleep 0.05
  done
  return 1
}
stop_gpu_side() {
  [ -n "$GPUPID" ] && kill "$GPUPID" 2>/dev/null
  wait "$GPUPID" 2>/dev/null
  GPUPID=""
}

BENCH="$RUNDIR/vk-bench"
if ! cc -O2 -o "$BENCH" "$ROOT/scripts/vk-bench.c" \
     -I"${VULKAN_SDK:-/usr}/include" -ldl 2>"$RUNDIR/cc.log"; then
  echo "SKIP — vk-bench did not build:"
  sed 's/^/  /' "$RUNDIR/cc.log" | head -20
  exit 1
fi

echo "=== control: the real driver, no forwarding ==="
VK_DRIVER_FILES="$ICDDIR/$ICD" "$BENCH" 2>&1 | tee "$RUNDIR/native.log" | sed 's/^/  /'

echo
echo "=== through shared_vulkan ==="
start_gpu_side || { echo "  the GPU side never came up:"; sed 's/^/    /' "$RUNDIR/gpu.log"; exit 1; }
env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" "$BENCH" 2>&1 \
  | tee "$RUNDIR/forwarded.log" | sed 's/^/  /'
stop_gpu_side

echo
echo "=== ratio, forwarded / native ==="
awk '
  FILENAME == ARGV[1] && $1 == "bench" { native[$2] = $4 }
  FILENAME == ARGV[2] && $1 == "bench" {
    n = native[$2]
    if (n > 0) printf "  %-14s %10.3f us -> %10.3f us   x%.1f\n", $2, n, $4, $4 / n
    else       printf "  %-14s %10s    -> %10.3f us\n", $2, "-", $4
  }' "$RUNDIR/native.log" "$RUNDIR/forwarded.log"

# --- glxgears, which is the number that is allowed to settle an argument ------
echo
echo "=== glxgears ==="
if ! command -v Xvfb >/dev/null || ! command -v glxgears >/dev/null; then
  echo "  SKIP — Xvfb or glxgears is missing; enter 'nix develop'"
else
  XDISPLAY=":$((90 + RANDOM % 8))"
  Xvfb "$XDISPLAY" -screen 0 300x300x24 -nolisten tcp > "$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  sleep 1

  # glxgears prints "N frames in 5.0 seconds = F FPS" every five seconds. Take
  # the last one, so the report is not the first interval's warm-up.
  gears_fps() {
    local log="$1"
    grep -oE '= *[0-9.]+ FPS' "$log" | tail -1 | grep -oE '[0-9.]+'
  }
  run_gears() {
    local tag="$1"; shift
    env DISPLAY="$XDISPLAY" "$@" glxgears -geometry 300x300+0+0 \
      > "$RUNDIR/$tag.log" 2>&1 &
    local pid=$!
    sleep 16
    kill $pid 2>/dev/null; sleep 0.3; kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null
  }

  start_gpu_side || { echo "  the GPU side never came up"; kill "$XPID" 2>/dev/null; exit 1; }
  run_gears sv env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" \
    MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_KOPPER_DRI2=1 \
    vblank_mode=0
  stop_gpu_side
  # The control is zink on the same RADV, so what the comparison isolates is the
  # forwarding and not the choice of Gallium driver.
  run_gears zink env MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink \
    LIBGL_KOPPER_DRI2=1 vblank_mode=0
  run_gears soft env LIBGL_ALWAYS_SOFTWARE=1 vblank_mode=0

  printf "  shared_vulkan : %s FPS\n" "$(gears_fps "$RUNDIR/sv.log")"
  printf "  zink on RADV  : %s FPS\n" "$(gears_fps "$RUNDIR/zink.log")"
  printf "  llvmpipe      : %s FPS\n" "$(gears_fps "$RUNDIR/soft.log")"
  kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null
fi
