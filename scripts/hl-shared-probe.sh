#!/usr/bin/env bash
# Native execution verifies the test. HL_RUNNER executes the same Linux client
# under hl on an ARM64 Mac. Native success is never an HVF runtime claim.
set -euo pipefail
: "${HL_SHARED_PEER:?native wc-hl-shared-peer executable}"
: "${HL_SHARED_CLIENT:?Linux shared-client executable}"
scratch=$(mktemp -d "${HL_PROBE_ROOT:-/mnt/datasets/builds/scratch}/hl-shared.XXXXXXXX")
peer_pid=
cleanup() {
  if [[ -n "$peer_pid" ]]; then kill "$peer_pid" 2>/dev/null || true; wait "$peer_pid" 2>/dev/null || true; fi
  rm -rf -- "$scratch"
}
trap cleanup EXIT
mkdir -p "$scratch"/{home,config,data,state,runtime}
chmod 700 "$scratch/runtime"
export HOME="$scratch/home" XDG_CONFIG_HOME="$scratch/config" XDG_DATA_HOME="$scratch/data" XDG_STATE_HOME="$scratch/state" XDG_RUNTIME_DIR="$scratch/runtime"
"$HL_SHARED_PEER" "$scratch/backing" "$scratch/bell" >"$scratch/peer.log" 2>&1 &
peer_pid=$!
# This bounded readiness wait is setup, not transport polling.
for ((i=0; i<100; i++)); do
  [[ -S "$scratch/bell" ]] && break
  kill -0 "$peer_pid" 2>/dev/null || { cat "$scratch/peer.log"; exit 1; }
  sleep .05
done
[[ -S "$scratch/bell" ]]
if [[ -n "${HL_RUNNER:-}" ]]; then
  "$HL_RUNNER" "$HL_SHARED_CLIENT" "$scratch/backing" "$scratch/bell"
else
  "$HL_SHARED_CLIENT" "$scratch/backing" "$scratch/bell"
fi
wait "$peer_pid"
peer_pid=
echo 'shared backing, doorbells, close, protection, partial unmap, fork and exec passed'
