#!/usr/bin/env bash
# Build helper: run cargo inside `nix develop`, never the cargo on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."
exec nix develop --command bash -c "$*"
