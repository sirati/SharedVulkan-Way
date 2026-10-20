#!/bin/bash
# Bounded authenticated NV12 IOSurface fixture. The controller is supplied by
# the mac-stock-va owner because it owns the Mach section service and GPU
# export/import. No launchd/global service is started here.
set -euo pipefail
: "${WC_NV12_CONTROLLER:?set the signed fixture controller path}"
: "${WC_NV12_RUNTIME:?set the existing authenticated broker runtime directory}"
: "${WC_NV12_SERVICE:?set the authenticated Mach section-service endpoint}"
: "${WC_NV12_OWNER:?set owner-generation token from the existing broker}"
fixture=${HL_PROBE_ROOT:?}
run="$fixture/nv12-$$"; mkdir -m 700 "$run"
cleanup(){ rm -rf "$run"; }
trap cleanup EXIT
# Controller must create the original joined two-plane IOSurface, register it
# with the existing owner generation, and retain it until GPU retirement.
# Geometry is checked by wc-handle::nv12_surface/send_nv12; this wrapper does
# not accept caller-supplied plane metadata.
export WC_NV12_RUNTIME WC_NV12_SERVICE WC_NV12_OWNER WC_NV12_FIXTURE_DIR="$run"
"$WC_NV12_CONTROLLER" --authenticated-owner "$WC_NV12_OWNER" \
  --service "$WC_NV12_SERVICE" --runtime "$WC_NV12_RUNTIME" \
  --format NV12 --width 64 --height 64 --frames 3 \
  --require-original-iosurface --require-two-plane-prime \
  --retain-until-retirement --no-cpu-copy >"$run/controller.log" 2>&1
cat "$run/controller.log"
grep -q 'AUTHENTICATED_OWNER_OK' "$run/controller.log"
grep -q 'NV12_TWO_PLANE_PRIME_OK' "$run/controller.log"
grep -q 'ORIGINAL_IOSURFACE_RETAINED_UNTIL_RETIREMENT' "$run/controller.log"
grep -q 'NO_CPU_COPY' "$run/controller.log"
grep -q 'NV12_SAMPLE_PASS' "$run/controller.log"
echo 'headless authenticated NV12 IOSurface/PRIME fixture passed'
