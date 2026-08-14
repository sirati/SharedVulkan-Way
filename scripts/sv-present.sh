#!/usr/bin/env bash
# The acceptance criterion of design.md §10 stage 2: **a swapchain that
# presents, judged on the pixels that come out of it.**
#
#   scripts/sv-present.sh [icd-manifest-basename]   default: radeon_icd.x86_64.json
#
# Two checks, and the first is the one that matters.
#
#  1. `vk-present-probe` creates a `VK_EXT_headless_surface`, a real
#     `VK_KHR_swapchain` on our forwarded device, and presents three frames of a
#     known pattern. Each frame is written into a **GPU-side** swapchain image,
#     copied by `vkQueuePresentKHR` into arena memory the app side has mapped,
#     and written out as a PPM by the ICD's headless display target. The probe
#     writes its own PPM of what it uploaded. The two must be **byte-identical**.
#
#     A tolerance would be the wrong instrument here: unlike a rendered frame,
#     a presented one has an exactly knowable value, so a single wrong byte is a
#     bug and `cmp` is the right check. See the header of
#     `scripts/vk-present-probe.c` for what the pattern is designed to catch —
#     a wrong stride, a channel swap, a vertical flip, and an acquire that hands
#     out the same image twice.
#
#  2. The same run under `VK_LAYER_KHRONOS_validation`, because a swapchain is a
#     pile of image layout transitions and validation is the only thing that
#     reads them. §0.5 got validation to zero errors for `gl4-probe`; this keeps
#     it there for the present path.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
ICD="${1:-radeon_icd.x86_64.json}"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
RUNDIR="$(mktemp -d /tmp/sv-present.XXXXXX)"
MANIFEST="$BIN/sv_icd.json"

# STAGE 44. §35.6 item 6 and §42.5 item 4: the last of the three that had no
# freshness guard. Stronger than freshness alone, because `sv_icd.json` is a
# *generated file holding an absolute path*: `scripts/sv-worktree.sh` seeds a
# worktree by reflinking `target/`, so a fresh worktree's manifest names the
# **base tree's** `libsv_icd.so`. That file exists, so `[ -f "$MANIFEST" ]` was
# satisfied while the Vulkan loader used another checkout's driver — and this
# harness compares rendered frames against golden images, so the one thing it
# must never be uncertain about is which tree drew them.
. "$(dirname "${BASH_SOURCE[0]}")/wc-binaries.sh"
manifest_is_ours "$MANIFEST" "cargo build -p sv-icd, in this worktree" || exit 1
fresh "$BIN/libsv_icd.so"  "cargo build -p sv-icd" || exit 1
fresh "$BIN/sv-gpu-server" "cargo build -p sv-gpu-server" || exit 1

fail=0
GPUPID=""
cleanup() {
  [ -n "$GPUPID" ] && kill "$GPUPID" 2>/dev/null
  rm -rf "$RUNDIR"
}
trap cleanup EXIT

start_gpu_side() {
  VK_DRIVER_FILES="$ICDDIR/$ICD" "$BIN/sv-gpu-server" serve \
    --socket "$RUNDIR/gpu.sock" --arena $((256 << 20)) --clients 1 \
    > "$RUNDIR/gpu.log" 2>&1 &
  GPUPID=$!
  for _ in $(seq 1 400); do
    [ -S "$RUNDIR/gpu.sock" ] && return 0
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

PROBE="$RUNDIR/vk-present-probe"
if ! cc -O1 -o "$PROBE" "$ROOT/scripts/vk-present-probe.c" \
     -I"${VULKAN_SDK:-/usr}/include" -ldl 2>"$RUNDIR/cc.log"; then
  echo "SKIP — vk-present-probe did not build:"
  sed 's/^/  /' "$RUNDIR/cc.log" | head -10
  exit 0
fi

# `run <tag> [extra env...]` — one probe run, into its own frame directory.
run() {
  local tag="$1"; shift
  local dir="$RUNDIR/$tag"
  mkdir -p "$dir"
  start_gpu_side || { echo "  the GPU side never came up:"; sed 's/^/    /' "$RUNDIR/gpu.log"; return 1; }
  env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$RUNDIR/gpu.sock" \
      SV_PRESENT_DUMP="$dir/got-" VK_PRESENT_PROBE_EXPECT="$dir/want-" \
      "$@" "$PROBE" > "$RUNDIR/$tag.log" 2>&1
  local rc=$?
  stop_gpu_side
  return $rc
}

echo "GPU side ICD: $ICD"

# --- 1. a swapchain, three frames, compared byte for byte --------------------
echo
echo "--- vk-present-probe: a headless swapchain, and the bytes that come back ---"
if ! run plain; then
  echo "  FAIL — the probe returned non-zero"
  sed 's/^/    /' "$RUNDIR/plain.log" | head -20
  echo "  --- gpu ---"; tail -8 "$RUNDIR/gpu.log" | sed 's/^/    /'
  fail=1
else
  grep -E '^(surface|caps|formats|present modes|swapchain|frame)' "$RUNDIR/plain.log" \
    | sed 's/^/  /'
  same=0
  differ=0
  for w in "$RUNDIR/plain"/want-*.ppm; do
    g="${w/want-/got-}"
    if [ -f "$g" ] && cmp -s "$w" "$g"; then
      same=$((same + 1))
    else
      differ=$((differ + 1))
      echo "  MISMATCH: $(basename "$w")"
      command -v pnmpsnr >/dev/null && pnmpsnr "$w" "$g" 2>&1 | sed 's/^/    /' | head -2
    fi
  done
  echo "  frames identical : $same, differing : $differ"
  if [ "$same" -eq 0 ] || [ "$differ" -ne 0 ]; then
    echo "  FAIL — a presented frame is not the frame that was drawn"
    fail=1
  else
    echo "  PASS — $same frames crossed the boundary and came back byte for byte"
  fi
fi

# --- 2. the same, under validation -------------------------------------------
echo
echo "--- the same run, under VK_LAYER_KHRONOS_validation ---"
if ! run validated VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation; then
  echo "  FAIL — the probe returned non-zero under validation"
  sed 's/^/    /' "$RUNDIR/validated.log" | head -20
  fail=1
else
  # The GPU side's own validation output arrives on the reverse ring and lands
  # in the app-side log; the app side's own layer writes there directly.
  errs=$(cat "$RUNDIR/validated.log" "$RUNDIR/gpu.log" 2>/dev/null | grep -c 'Validation Error')
  echo "  validation errors: $errs"
  if [ "$errs" -ne 0 ]; then
    grep -m4 -A2 'Validation Error' "$RUNDIR/validated.log" "$RUNDIR/gpu.log" 2>/dev/null \
      | sed 's/^/    /'
    echo "  FAIL — the present path is not clean under validation"
    fail=1
  else
    echo "  PASS — no validation errors"
  fi
fi

echo
[ $fail -eq 0 ] && echo "presentation over shared_vulkan: PASS" || echo "presentation over shared_vulkan: FAIL"
exit $fail
