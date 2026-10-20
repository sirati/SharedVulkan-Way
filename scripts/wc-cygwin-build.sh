#!/usr/bin/env bash
# No target compiler is executed. The official Cygwin GCC package supplies
# target CRT objects/headers; native clang and lld cross-compile on vmhost.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
exec /mnt/datasets/builds/wcrun/cg nix shell --inputs-from "$repo" \
  nixpkgs#llvmPackages.clang-unwrapped nixpkgs#lld nixpkgs#wayland-scanner nixpkgs#python3 \
  -c python3 "$repo/scripts/wc-cygwin-build.py" "$@"
