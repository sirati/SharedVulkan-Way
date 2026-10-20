#!/usr/bin/env bash
# ARM64 Darwin links on the Linux build machine. Never invokes a Mac compiler.
set -euo pipefail
sdk=${WC_MACOS_SDK:-/mnt/datasets/builds/macos-sdk/MacOSX14.5.sdk}
: "${WC_MACOS_CLANG:?run inside nix develop}"
: "${WC_MACOS_LLD_BIN:?run inside nix develop}"
[[ -d "$sdk" ]] || { echo 'macOS SDK is missing' >&2; exit 1; }
PATH="$WC_MACOS_LLD_BIN:$PATH" exec "$WC_MACOS_CLANG" \
  --target=arm64-apple-macosx14.0 -isysroot "$sdk" -fuse-ld=lld "$@"
