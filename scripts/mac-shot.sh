#!/usr/bin/env bash
# The macOS VM's screen, from QEMU's side (QMP screendump), so no guest
# permission (screen recording / TCC) is involved. Usage: mac-shot.sh OUT.png
set -euo pipefail
inst=${MACVM_INSTANCE:-wayland-host}
q=/mnt/datasets/macvm/instances/$inst/qmp.sock
out=$(realpath -m "$1")
printf '{"execute":"qmp_capabilities"}\n{"execute":"screendump","arguments":{"filename":"%s","format":"png"}}\n' "$out" \
  | socat -t 5 - "UNIX-CONNECT:$q" > /dev/null
for i in $(seq 1 20); do [ -s "$out" ] && exit 0; sleep 0.2; done
echo "mac-shot: no screendump at $out" >&2; exit 1
