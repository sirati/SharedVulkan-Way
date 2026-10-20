#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
/usr/bin/codesign --verify --strict /nix/store/64j6q3lz78l9py8ca9qgxv4nnh3kg7jy-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
/nix/store/5238ihxsxqapz5aqzb9q9c3l5xr66l2a-hyperlinux-probe-runner /nix/store/hzdymqflnafy2535z2wngd6qdlsml7p1-hl-external-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-external-fault-client pending
binary=/nix/store/m03i9m9b20xhszx5jhykb4g6g6iy4ir1-hl-readonly-host-exec-check-arm64-apple-darwin-0.1.0/bin/hl-readonly-host-exec-check
/usr/bin/codesign --verify --strict "$binary"
"$binary"
