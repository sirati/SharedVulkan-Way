#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
/usr/bin/codesign --verify --strict /nix/store/sggc9sksnmjzf4hwl3rg4rqkv749hg90-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
/nix/store/mmjchb81vdcibjyrad8al6vvjsz0s8ni-hyperlinux-probe-runner /nix/store/5x0lm4winiqg7450na12yji6znmyiyyj-hl-fatal-bus-wait-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-fatal-bus-wait-client /nix/store/hzdymqflnafy2535z2wngd6qdlsml7p1-hl-external-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-external-fault-client
