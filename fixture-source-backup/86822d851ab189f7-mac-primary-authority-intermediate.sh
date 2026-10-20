#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
client=/nix/store/yj9jq7krddmqrjcllprzghbcr1wbp6l7-hl-primary-output-authority-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-primary-output-authority-client
/usr/bin/codesign --verify --strict /nix/store/n6n7x2c2ylbxplb6vr1yfls370wi6bnx-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
for mode in 0 1 2 3 4 5; do
 echo "BEGIN old-primary mode=$mode"
 set +e
 /nix/store/p2l1vsyzrhyshnnvjmx96xawmm857pmd-hyperlinux-probe-runner "$client" "$mode" > "$fixture/old-$mode.log" 2>&1
 result=$?
 set -e
 cat "$fixture/old-$mode.log"
 [[ "$result" == 1 ]]
 /usr/bin/grep -Eq 'result=[01] errno=0' "$fixture/old-$mode.log"
 echo "END old-primary mode=$mode expected-negative"
done
for mode in 0 1 2 3 4 5; do
 echo "BEGIN intermediate-strict-primary mode=$mode"
 /nix/store/bjv6cgra6wakg4l5jpz9c570pckqkckj-hyperlinux-probe-runner "$client" "$mode"
 echo "END intermediate-strict-primary mode=$mode passed"
done
