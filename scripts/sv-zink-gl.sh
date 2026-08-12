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
#
#  5. **`glxgears`, in a real X window**, which is the program §13's acceptance
#     criterion names and which the previous stage could not run at all: Zink's
#     windowed path is `zink_kopper` and it wants a `VK_KHR_swapchain`, which
#     design.md §10 stage 2 now provides. The frame is captured with `xwd` and
#     compared against the same program on llvmpipe — not pixel for pixel, since
#     glxgears animates from the wall clock, but on the histogram statistics
#     that are invariant under the gears' rotation.
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
# WHY THIS AND NOT ONLY glxgears. `glxgears` now runs (check 5 below), but it
# animates, so it can only be judged on statistics. `gl-probe` renders one
# deterministic frame and can be compared against llvmpipe pixel for pixel,
# which is a much sharper instrument for the *rendering* half. The two check
# different things and both are worth keeping: an indexed, depth-tested,
# textured, lit mesh through a real graphics pipeline, read back and judged.
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

# --- 4. a windowed GL program, in a real X window -----------------------------
#
# The check the previous stage could not make. design.md §10 stage 2 exists, so
# `zink_kopper` has a swapchain to create and `glxgears` — which is the program
# §13's acceptance criterion actually names — runs against a window rather than
# refusing to start.
#
# WHY THE JUDGEMENT IS STRUCTURAL AND NOT A PSNR. `glxgears` animates from the
# wall clock, so two runs never produce the same frame and there is nothing to
# compare byte for byte. What *is* invariant under rotation is the shape of the
# histogram: the three gears keep their areas and their colours whatever angle
# they are at. So the same five numbers are taken from our capture and from
# llvmpipe's, and they have to agree — which is the same argument §0.4 makes for
# `gl-probe`, applied to a moving picture.
#
# `LIBGL_KOPPER_DRI2` is set because Xvfb's GLX has no DRI3 and Mesa's GLX
# refuses the visual before zink is reached without it (§0.4 saw the same wall
# from the other side). Kopper itself needs no DRI3: its whole point is that the
# swapchain is Vulkan's rather than the X server's.
echo
echo "--- glxgears: a window, and what ends up in it ---"
if ! command -v Xvfb >/dev/null || ! command -v xwd >/dev/null || ! command -v glxgears >/dev/null; then
  echo "  SKIP — Xvfb, xwd or glxgears is missing; enter 'nix develop'"
else
  XDISPLAY=":$((90 + RANDOM % 8))"
  GEARS=300
  Xvfb "$XDISPLAY" -screen 0 ${GEARS}x${GEARS}x24 -nolisten tcp > "$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  sleep 1

  # The root window *is* the gears window, so a root capture needs no cropping
  # and "coverage" means coverage of the frame the application drew.
  gears_capture() {
    local tag="$1"; shift
    env DISPLAY="$XDISPLAY" "$@" glxgears -geometry ${GEARS}x${GEARS}+0+0 \
      > "$RUNDIR/$tag.log" 2>&1 &
    local pid=$!
    sleep 5
    DISPLAY="$XDISPLAY" xwd -root -silent -out "$RUNDIR/$tag.xwd" 2>/dev/null
    kill $pid 2>/dev/null; sleep 0.3; kill -9 $pid 2>/dev/null; wait $pid 2>/dev/null
    [ -s "$RUNDIR/$tag.xwd" ] && xwdtopnm < "$RUNDIR/$tag.xwd" > "$RUNDIR/$tag.ppm" 2>/dev/null
  }

  # colours, non-black coverage, and the share of each gear's hue. `ppmhist`
  # gives (r g b lum count) rows; everything else is arithmetic.
  gears_stats() {
    ppmhist "$1" 2>/dev/null | awk '
      /^[ \t]*[0-9]/ {
        r=$1; g=$2; b=$3; n=$5; total+=n; colours++
        if (r+g+b > 0) { lit+=n
          if (r > g && r > b) red+=n
          else if (g > r && g > b) green+=n
          else if (b > r && b > g) blue+=n }
      }
      END { if (total==0) total=1
            printf "%d %.3f %.3f %.3f %.3f\n", colours, lit/total, red/total, green/total, blue/total }'
  }

  start_gpu_side || { echo "  the GPU side never came up"; kill "$XPID" 2>/dev/null; exit 1; }
  gears_capture zink env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" \
    MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_KOPPER_DRI2=1
  stop_gpu_side
  gears_capture soft env LIBGL_ALWAYS_SOFTWARE=1

  if [ ! -s "$RUNDIR/zink.ppm" ]; then
    echo "  FAIL — nothing was captured. glxgears said:"
    head -8 "$RUNDIR/zink.log" | sed 's/^/    /'
    fail=1
  else
    read -r ZC ZLIT ZR ZG ZB <<< "$(gears_stats "$RUNDIR/zink.ppm")"
    echo "  ours     : $ZC colours, $ZLIT lit, gears r/g/b $ZR / $ZG / $ZB"
    if [ -s "$RUNDIR/soft.ppm" ]; then
      read -r SC SLIT SR SG SB <<< "$(gears_stats "$RUNDIR/soft.ppm")"
      echo "  llvmpipe : $SC colours, $SLIT lit, gears r/g/b $SR / $SG / $SB"
    fi
    # A dropped frame leaves the window at the X server's background, which is
    # one colour and no coverage; a present that showed something but not the
    # gears has no three-hue split. Both are what this catches.
    ok=$(awk -v c="$ZC" -v lit="$ZLIT" -v r="$ZR" -v g="$ZG" -v b="$ZB" \
      -v sr="${SR:-0}" -v sg="${SG:-0}" -v sb="${SB:-0}" -v slit="${SLIT:-0}" '
      BEGIN {
        if (c < 50)   { print "too few colours: " c; exit }
        if (lit < 0.05) { print "almost nothing was drawn: " lit; exit }
        if (r < 0.01 || g < 0.01 || b < 0.01) { print "a gear is missing: " r " " g " " b; exit }
        if (slit > 0) {
          if (lit < slit * 0.5 || lit > slit * 2.0) { print "coverage disagrees with llvmpipe"; exit }
          if (r  < sr * 0.4 || r  > sr * 2.5)       { print "the red gear disagrees";  exit }
          if (g  < sg * 0.4 || g  > sg * 2.5)       { print "the green gear disagrees"; exit }
          if (b  < sb * 0.4 || b  > sb * 2.5)       { print "the blue gear disagrees";  exit }
        }
        print "ok"
      }')
    if [ "$ok" = "ok" ]; then
      echo "  PASS — three gears, in a window, over a forwarded swapchain"
    else
      echo "  FAIL — $ok"
      grep -E 'MESA|Error|error' "$RUNDIR/zink.log" | head -6 | sed 's/^/    /'
      fail=1
    fi
  fi
  kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null
fi

echo
[ $fail -eq 0 ] && echo "zink over shared_vulkan: PASS" || echo "zink over shared_vulkan: FAIL"
exit $fail
