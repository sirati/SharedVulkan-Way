#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
/usr/bin/codesign --verify --strict /nix/store/a5pminja7yhsvzh5yh5wsb4f562h4ql3-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
/nix/store/p2l1vsyzrhyshnnvjmx96xawmm857pmd-hyperlinux-probe-runner /nix/store/17pnljin9pbzq0mq7q0i4nxyp93jghmx-hl-seqpacket-cleanup-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seqpacket-cleanup-client
