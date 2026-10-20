#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
runner=/nix/store/5238ihxsxqapz5aqzb9q9c3l5xr66l2a-hyperlinux-probe-runner
client=/nix/store/9vpd2hdq7qa0pj6z8hvjk208vpn0vwf4-hl-external-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-external-fault-client
/usr/bin/codesign --verify --strict /nix/store/64j6q3lz78l9py8ca9qgxv4nnh3kg7jy-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
"$runner" /nix/store/gdjsyzh9byy1bs9ig0rx3ixdqs12gmvz-hl-socket-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-socket-client
"$runner" /nix/store/3wz8p3rg2nh6j3qgi5zissc199bsc38w-hl-shared-access-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-shared-access-client --guest-map-fault > "$fixture/truncate.log" 2>&1
cat "$fixture/truncate.log"
/usr/bin/grep -q GUEST_SIGBUS "$fixture/truncate.log"
"$runner" "$client" handled
for mode in default blocked ignored; do
 set +e
 "$runner" "$client" "$mode" > "$fixture/$mode.log" 2>&1
 status=$?
 set -e
 cat "$fixture/$mode.log"
 [[ "$status" == 135 ]]
 echo "external fault $mode guest termination135 passed"
done
