#!/usr/bin/env bash
# Same-kernel protocol test on vmhost. This does not execute hl or prove Darwin.
set -euo pipefail
: "${HL_GPU_SERVER:?sv-gpu-server from this build}"
: "${HL_GUEST_ICD:?manifest selecting current build Linux ELF ICD}"
: "${HL_REAL_ICD:?native software Vulkan driver manifest}"
: "${HL_VULKANINFO:?vulkaninfo executable}"
run=$(mktemp -d "${HL_PROBE_ROOT:-/mnt/datasets/builds/scratch}/hl-gpu.XXXXXXXX")
pid=
cleanup() {
  if [[ -n "$pid" ]]; then kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; fi
  if [[ -n "${HL_GPU_EVIDENCE:-}" ]]; then
    mkdir -p "$HL_GPU_EVIDENCE"
    cp "$run"/*.log "$HL_GPU_EVIDENCE/" 2>/dev/null || true
  fi
  rm -rf -- "$run"
}
trap cleanup EXIT
chmod 700 "$run"
mkdir -p "$run"/{home,config,data,state}
export HOME="$run/home" XDG_CONFIG_HOME="$run/config" XDG_DATA_HOME="$run/data" XDG_STATE_HOME="$run/state" XDG_RUNTIME_DIR="$run"
unset DISPLAY XAUTHORITY
env VK_DRIVER_FILES="$HL_REAL_ICD" VK_ICD_FILENAMES="$HL_REAL_ICD" \
  "$HL_GPU_SERVER" hl-local --runtime "$run" --socket "$run/gpu" --bell "$run/bell" >"$run/server.log" 2>&1 &
pid=$!
for ((i=0; i<200; i++)); do
  [[ -S "$run/gpu" && -S "$run/bell" ]] && break
  kill -0 "$pid" 2>/dev/null || { cat "$run/server.log"; exit 1; }
  sleep .02
done
[[ -S "$run/gpu" && -S "$run/bell" ]]
for i in 1 2; do
  env VK_DRIVER_FILES="$HL_GUEST_ICD" VK_ICD_FILENAMES="$HL_GUEST_ICD" \
    SV_SOCKET="$run/gpu" SV_BELL="$run/bell" \
    "$HL_VULKANINFO" --summary >"$run/client-$i.log" 2>&1
  grep -q 'deviceName.*llvmpipe' "$run/client-$i.log"
done
echo 'two native Linux ICD sessions reached the local shared GPU endpoint'
