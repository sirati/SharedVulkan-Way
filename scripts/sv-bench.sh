#!/usr/bin/env bash
# The benchmark docs/design.md §13 step 1 asks for, run twice: once through this
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
#
# **THIS FILE IS AN INSTRUMENT, NOT A GATE, AND §31 MAKES THAT EXPLICIT RATHER
# THAN IMPLICIT.**
#
# Every headline number here — microseconds a call, the forwarded/native ratio,
# frames a second — is a *rate*, measured on a shared machine whose GPU other
# people are using. §0.13 retracted a headline for measuring Xvfb, and the
# lesson §31 draws from that is not "assert on the rate anyway" but "say which
# numbers a harness has an opinion about". This one has an opinion about
# exactly two things, and they are both properties of the code rather than of
# the load:
#
#   * every measurement it prints was actually produced — a benchmark that
#     silently measured nothing is worse than one that failed;
#   * forwarded glxgears beats **llvmpipe**. Mesa's fallback to software is
#     silent and reports GL 4.6 while doing it (`sv-guest-host.sh` names the
#     same trap), so "a number came out" does not mean a GPU produced it. An
#     order of magnitude separates the two, so this is a bound no amount of
#     load can cross.
#
# Everything else is printed for a person to read, and that is now a stated
# decision rather than an omission. It used to have no `ok`/`bad` at all and
# exited 0 whatever it measured.
set -uo pipefail

fail=0

# §51. Same-box, declared so the distinction above is machine-checkable rather
# than only argued. The glxgears bound IS a throughput claim, and it is a sound
# one, because what it concludes is **which device answered** -- an order of
# magnitude between a GPU and llvmpipe -- and not how long anything took to
# cross a boundary. There is no boundary here, and `wc_latency_claim` would
# refuse one if this file asked.
. "$(dirname "$0")/wc-topology.sh"
wc_topology_declare same-box \
  "Xvfb, glxgears and the ICD all run on this machine; the control is the same machine with no forwarding" >/dev/null || exit 1

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

# STAGE 44. §35.6 item 6 and §42.5 item 4: this harness had no freshness guard,
# and it is one of the three that produce numbers this project quotes. Stronger
# than freshness alone, because `sv_icd.json` is a *generated file holding an
# absolute path*: `scripts/sv-worktree.sh` seeds a worktree by reflinking
# `target/`, so a fresh worktree's manifest names the **base tree's**
# `libsv_icd.so`. That file exists, so `[ -f "$MANIFEST" ]` was satisfied while
# the Vulkan loader handed this suite another checkout's driver — and a
# benchmark that measures the wrong tree is worse than one that does not run.
#
# `SV_BENCH_BIN=target/release` is a supported second data point (see `BIN`
# above), and `manifest_is_ours` only requires the path to be *inside this
# worktree*, so it does not constrain which profile is measured.
. "$(dirname "${BASH_SOURCE[0]}")/wc-binaries.sh"
manifest_is_ours "$MANIFEST" "cargo build -p sv-icd, in this worktree" || exit 1
fresh "$BIN/libsv_icd.so"  "cargo build -p sv-icd" || exit 1
fresh "$BIN/sv-gpu-server" "cargo build -p sv-gpu-server" || exit 1

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

  SV_FPS=$(gears_fps "$RUNDIR/sv.log")
  ZINK_FPS=$(gears_fps "$RUNDIR/zink.log")
  SOFT_FPS=$(gears_fps "$RUNDIR/soft.log")
  printf "  shared_vulkan : %s FPS\n" "$SV_FPS"
  printf "  zink on RADV  : %s FPS\n" "$ZINK_FPS"
  printf "  llvmpipe      : %s FPS\n" "$SOFT_FPS"
  kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null

  # See the header: the rates are informational. These two are not.
  if [ -z "$SV_FPS" ] || [ -z "$ZINK_FPS" ] || [ -z "$SOFT_FPS" ]; then
    echo "  FAIL one of the three glxgears runs produced no FPS line; there is no comparison here"
    fail=1
  elif awk -v s="$SV_FPS" -v l="$SOFT_FPS" 'BEGIN { exit !(s > l) }'; then
    echo "  ok   forwarded glxgears beats llvmpipe ($SV_FPS vs $SOFT_FPS FPS) — a real device is on the other end"
  else
    echo "  FAIL forwarded glxgears ($SV_FPS FPS) does not beat llvmpipe ($SOFT_FPS FPS); this is software rendering wearing a GPU's name"
    fail=1
  fi
fi

exit "$fail"
