#!/usr/bin/env bash
# The acceptance test: an application, the real Vulkan loader, our ICD, a ring,
# and a real driver at the far end.
#
#   scripts/sv-end-to-end.sh [icd-manifest-basename]     default: radeon_icd.x86_64.json
#
# Two checks, in order of what they prove:
#
#  1. `vulkaninfo` reports the **GPU side's driver name**, not design.md §9's
#     synthesised "shared_vulkan forwarded device". That is the whole stage in
#     one string: the loader accepted us, our ICD encoded
#     vkGetPhysicalDeviceProperties, it crossed a ring into another process, a
#     real driver answered, and the answer came back through the generated
#     decoder into the application's struct.
#
#  2. The same, under VK_LAYER_KHRONOS_validation, because the dispatchable-
#     handle contract of §6.4 is what validation exercises and a segfault there
#     is the classic silent failure.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
ICD="${1:-radeon_icd.x86_64.json}"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
RUNDIR="$(mktemp -d /tmp/sv-e2e.XXXXXX)"
trap 'rm -rf "$RUNDIR"' EXIT

MANIFEST="$BIN/sv_icd.json"
[ -f "$MANIFEST" ] || { echo "build first: cargo build -p sv-icd" >&2; exit 1; }
[ -x "$BIN/sv-gpu-server" ] || { echo "build first: cargo build -p sv-gpu-server" >&2; exit 1; }

SOCK="$RUNDIR/gpu.sock"
fail=0

start_gpu_side() {
  # `--clients 1` and then it serves rings until the app side sets SHUTDOWN or
  # goes quiet, so the script never has to remember to kill it.
  VK_DRIVER_FILES="$ICDDIR/$ICD" "$BIN/sv-gpu-server" serve \
    --socket "$SOCK" --arena $((64 << 20)) --clients 1 \
    > "$RUNDIR/gpu.log" 2>&1 &
  GPUPID=$!
  for _ in $(seq 1 300); do
    [ -S "$SOCK" ] && return 0
    kill -0 $GPUPID 2>/dev/null || return 1
    sleep 0.05
  done
  return 1
}

run_case() {
  local name="$1"; shift
  echo
  echo "--- $name ---"
  start_gpu_side || { echo "  the GPU side never came up:"; sed 's/^/    /' "$RUNDIR/gpu.log"; return 1; }

  # The app side is the real loader, our ICD, and vulkaninfo on top. Note what
  # is NOT set: it has no VK_DRIVER_FILES for a real driver, so the only
  # implementation it can reach is ours.
  env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" "$@" \
    > "$RUNDIR/app.log" 2>&1
  local rc=$?
  wait $GPUPID 2>/dev/null

  if [ $rc -ne 0 ]; then
    echo "  FAIL (exit $rc)"
    echo "  --- app ---"; tail -25 "$RUNDIR/app.log" | sed 's/^/    /'
    echo "  --- gpu ---"; tail -25 "$RUNDIR/gpu.log" | sed 's/^/    /'
    return 1
  fi

  local reported
  reported=$(grep -m1 "deviceName" "$RUNDIR/app.log" | sed 's/.*= //')
  local driver
  driver=$(grep -m1 "^\[gpu\] " "$RUNDIR/gpu.log" | sed 's/^\[gpu\] //; s/ (.*//')
  echo "  GPU side driver : $driver"
  echo "  app side sees   : $reported"
  if [ -z "$reported" ]; then
    echo "  FAIL — vulkaninfo reported no device"
    return 1
  fi
  if [ "$reported" = "shared_vulkan forwarded device" ]; then
    echo "  FAIL — that is our synthesised table, so nothing was forwarded"
    return 1
  fi
  echo "  PASS — the application is reading the GPU side's driver"
  return 0
}

echo "GPU side ICD: $ICD"
run_case "vulkaninfo through the loader" vulkaninfo --summary || fail=1
run_case "vulkaninfo under the validation layer" \
  env VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation vulkaninfo --summary || fail=1

echo
[ $fail -eq 0 ] && echo "end-to-end: PASS" || echo "end-to-end: FAIL"
exit $fail
