#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture" HL_WC_MAP_TRACE=1
mkdir -p "$HOME" "$fixture/tmp"
/usr/bin/codesign --verify --strict /nix/store/1va70zx270ygbbh642mhc00vpn586yyz-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
/nix/store/gqml6waymgl56jmwfh4s4131rl9s915q-hyperlinux-probe-runner /nix/store/3wz8p3rg2nh6j3qgi5zissc199bsc38w-hl-shared-access-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-shared-access-client --guest-map-fault
