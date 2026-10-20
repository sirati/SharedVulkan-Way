#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
runner=/nix/store/65p4a2v4vn1xjpkdpdc4xpi7qdgzjayr-hyperlinux-probe-runner
client=/nix/store/hzdymqflnafy2535z2wngd6qdlsml7p1-hl-external-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-external-fault-client
/usr/bin/codesign --verify --strict /nix/store/0xmh6qpd9dapcmr6jp3wbqgy0b5v7r06-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
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

"$runner" "$client" pending
for mode in --basic --backend-denial --trap --tsync --stacked --tsync-divergent --kill-thread --arguments --wait-status; do
 echo "BEGIN seccomp $mode"
 "$runner" /nix/store/030c4g4f12ibljhchpinq58f0baky0fn-hl-seccomp-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seccomp-client "$mode"
 echo "END seccomp $mode passed"
done
