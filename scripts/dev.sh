#!/usr/bin/env bash
# Run a command inside the pinned dev shell, from the worktree root.
#
# Exists so that every cargo/rustc invocation in this project goes through
# `nix develop` and therefore through the pinned toolchain and the pinned
# $VK_XML, rather than whatever happens to be on PATH.
set -euo pipefail
cd "$(dirname "$0")/.."
exec nix develop --command "$@"
