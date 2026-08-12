#!/usr/bin/env bash
# The acceptance criterion of this stage: **an OpenGL program running on Zink on
# top of our forwarded Vulkan.**
#
#   scripts/sv-zink-gl.sh [icd-manifest-basename]   default: radeon_icd.x86_64.json
#
# Three checks, in increasing order of what they prove:
#
#  1. `eglinfo` on the surfaceless platform reports a **renderer string naming
#     the GPU side's device** — "zink Vulkan 1.4(… RADV …)". That is Zink
#     starting: it enumerated our physical device through the real loader,
#     queried the features and formats it needs across the ring, created a
#     device, and built its screen. Zink refuses loudly and by name when a
#     feature is missing, so a renderer string is a statement that none was.
#
#  2. `gl-probe` **renders a frame and reads it back**, and the frame is judged
#     on its pixels rather than on the call returning: coverage, colour count and
#     the warm/cool balance that only a working depth test produces. A forwarder
#     that silently drops every draw passes every other kind of check.
#
#  3. The frame is compared against the **same program on llvmpipe**. Two
#     entirely different rasterisers agreeing to within a rounding error is much
#     stronger evidence than either one looking plausible on its own.
#
#  4. `gl4-probe` uses the three capabilities that were absent while GL was
#     3.2 — tessellation, an instance-rate vertex attribute, and an indirect
#     draw whose count comes from a parameter buffer. `gl-probe` is written
#     against GLSL 1.50 on purpose and passes identically at 3.2 and 4.6, so it
#     cannot tell them apart; this one can, and it judges each capability on the
#     pixels it produces rather than on the version string.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
ICD="${1:-radeon_icd.x86_64.json}"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
RUNDIR="$(mktemp -d /tmp/sv-zink-gl.XXXXXX)"
MANIFEST="$BIN/sv_icd.json"
SOCK="$RUNDIR/gpu.sock"

[ -f "$MANIFEST" ] || { echo "build first: cargo build -p sv-icd" >&2; exit 1; }
[ -x "$BIN/sv-gpu-server" ] || { echo "build first: cargo build -p sv-gpu-server" >&2; exit 1; }
command -v eglinfo >/dev/null || { echo "eglinfo is missing; enter 'nix develop'" >&2; exit 1; }

fail=0
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

# Every GL program runs with the same environment: our ICD is the *only* Vulkan
# implementation it can reach (no VK_DRIVER_FILES for a real driver), and zink
# is the only Gallium driver it may load.
gl_env() {
  env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" \
      MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink \
      "$@"
}

renderer_of() { grep -m1 -oE 'zink Vulkan [^)]*\)+' "$1"; }

echo "GPU side ICD: $ICD"

# --- 1. Zink starts, on the surfaceless platform ------------------------------
echo
echo "--- eglinfo, surfaceless ---"
start_gpu_side || { echo "  the GPU side never came up:"; sed 's/^/    /' "$RUNDIR/gpu.log"; exit 1; }
gl_env env EGL_PLATFORM=surfaceless eglinfo -B > "$RUNDIR/eglinfo.log" 2>&1
R=$(renderer_of "$RUNDIR/eglinfo.log")
GL=$(grep -m1 'OpenGL core profile version' "$RUNDIR/eglinfo.log" | sed 's/.*: //')
stop_gpu_side
if [ -z "$R" ]; then
  echo "  FAIL — Zink did not start. Its own reason:"
  grep -E 'MESA|zink' "$RUNDIR/eglinfo.log" | head -10 | sed 's/^/    /'
  fail=1
else
  echo "  renderer : $R"
  echo "  version  : $GL"
  echo "  PASS — Zink is running on our forwarded Vulkan"
fi

# --- 2. a real GL program renders, and the pixels are checked -----------------
#
# WHY NOT glxgears. It is a GLX program and GLX needs an X server; this machine
# is headless, and Xvfb's GLX has no DRI3, so Zink's window-system path
# (`zink_kopper`) cannot create a screen on it — it wants a swapchain, which
# design.md §10 deliberately keeps outside the protocol. A windowed test would
# be measuring the one thing this stage does not implement. `gl-probe` is the
# same workload without the window: an indexed, depth-tested, textured, lit mesh
# through a real graphics pipeline, read back and judged.
echo
echo "--- gl-probe: a rendered frame, and its pixels ---"
PROBE="$RUNDIR/gl-probe"
if ! cc -O1 -o "$PROBE" "$ROOT/scripts/gl-probe.c" -lEGL -lGL -lm 2>"$RUNDIR/cc.log"; then
  echo "  SKIP — gl-probe did not build:"
  sed 's/^/    /' "$RUNDIR/cc.log" | head -10
else
  # The reference first, on llvmpipe, so that "correct" is a comparison rather
  # than a claim. Two entirely different rasterisers agreeing to within a
  # rounding error is far stronger evidence than either one looking plausible.
  LIBGL_ALWAYS_SOFTWARE=1 GL_PROBE_PPM="$RUNDIR/ref.ppm" \
    "$PROBE" > "$RUNDIR/ref.log" 2>&1
  REFOK=$?

  start_gpu_side || { echo "  the GPU side never came up"; exit 1; }
  GL_PROBE_PPM="$RUNDIR/zink.ppm" gl_env "$PROBE" > "$RUNDIR/probe.log" 2>&1
  RC=$?
  stop_gpu_side

  sed -n 's/^/  /p' "$RUNDIR/probe.log" | grep -E 'renderer|version|coverage|colours|warm' || true
  if [ $RC -ne 0 ]; then
    echo "  FAIL — gl-probe returned $RC"
    grep -vE '^(renderer|version|coverage|colours|warm|wrote)' "$RUNDIR/probe.log" \
      | head -12 | sed 's/^/    /'
    echo "  --- gpu ---"; tail -8 "$RUNDIR/gpu.log" | sed 's/^/    /'
    fail=1
  else
    echo "  PASS — a depth-tested, textured, lit mesh was rendered and read back"
  fi

  if [ $REFOK -eq 0 ] && [ $RC -eq 0 ] && command -v pnmpsnr >/dev/null; then
    PSNR=$(pnmpsnr "$RUNDIR/ref.ppm" "$RUNDIR/zink.ppm" 2>&1 | grep -oE 'Y: *[0-9.]+' | head -1)
    echo "  against llvmpipe : $PSNR dB (luma)"
    DB=$(echo "$PSNR" | grep -oE '[0-9]+' | head -1)
    if [ "${DB:-0}" -lt 30 ]; then
      echo "  FAIL — the frame does not agree with the software reference"
      fail=1
    fi
  fi
fi

# --- 3. the GL 4.x capabilities, each judged on its own pixels ----------------
echo
echo "--- gl4-probe: tessellation, instance divisor, indirect count ---"
PROBE4="$RUNDIR/gl4-probe"
if ! cc -O1 -o "$PROBE4" "$ROOT/scripts/gl4-probe.c" -lEGL -lGL -lm 2>"$RUNDIR/cc4.log"; then
  echo "  SKIP — gl4-probe did not build:"
  sed 's/^/    /' "$RUNDIR/cc4.log" | head -10
else
  LIBGL_ALWAYS_SOFTWARE=1 EGL_PLATFORM=surfaceless GL4_PROBE_PPM="$RUNDIR/ref4.ppm" \
    "$PROBE4" > "$RUNDIR/ref4.log" 2>&1
  REF4=$?

  start_gpu_side || { echo "  the GPU side never came up"; exit 1; }
  GL4_PROBE_PPM="$RUNDIR/zink4.ppm" gl_env "$PROBE4" > "$RUNDIR/probe4.log" 2>&1
  RC4=$?
  stop_gpu_side

  sed -n 's/^/  /p' "$RUNDIR/probe4.log" | grep -E 'version|coverage|disc|instances|marks' || true
  if [ $RC4 -ne 0 ]; then
    echo "  FAIL — gl4-probe returned $RC4"
    grep -E 'gl4-probe:' "$RUNDIR/probe4.log" | head -12 | sed 's/^/    /'
    echo "  --- gpu ---"; tail -8 "$RUNDIR/gpu.log" | sed 's/^/    /'
    fail=1
  else
    echo "  PASS — all three GL 4.x capabilities produced the geometry they should"
  fi

  if [ $REF4 -eq 0 ] && [ $RC4 -eq 0 ] && command -v pnmpsnr >/dev/null; then
    PSNR4=$(pnmpsnr "$RUNDIR/ref4.ppm" "$RUNDIR/zink4.ppm" 2>&1 | grep -oE 'Y: *[0-9.]+' | head -1)
    echo "  against llvmpipe : $PSNR4 dB (luma)"
    DB4=$(echo "$PSNR4" | grep -oE '[0-9]+' | head -1)
    if [ "${DB4:-0}" -lt 30 ]; then
      echo "  FAIL — the frame does not agree with the software reference"
      fail=1
    fi
  fi
fi

echo
[ $fail -eq 0 ] && echo "zink over shared_vulkan: PASS" || echo "zink over shared_vulkan: FAIL"
exit $fail
