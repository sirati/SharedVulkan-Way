#!/bin/bash
set -euo pipefail
binary=/nix/store/r030wny595nr4v24zppp5864w6lfr32l-hl-readonly-host-exec-check-arm64-apple-darwin-0.1.0/bin/hl-readonly-host-exec-check
/usr/bin/codesign --verify --strict "$binary"
"$binary"
