#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo"
exec /mnt/datasets/builds/wcrun/cg nix develop -c python3 scripts/wc-cygwin-local-build.py "$@"
