#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
hl=/nix/store/mziqjfy14zv0gcb6qkir9sdd4l2l5b8v-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
runner=/nix/store/8f9jil9iv8f7w4rw56mq5285r4cg08y9-hyperlinux-probe-runner
client=/nix/store/3wz8p3rg2nh6j3qgi5zissc199bsc38w-hl-shared-access-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-shared-access-client
/usr/bin/codesign --verify --strict "$hl"
for mode in --guest-readonly-write --guest-readonly-exec; do
 echo "BEGIN $mode"
 "$runner" "$client" "$mode" > "$fixture/result" 2>&1
 cat "$fixture/result"
 /usr/bin/grep -q GUEST_SIGSEGV "$fixture/result"
 echo "END $mode guest denial passed"
done
