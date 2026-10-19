#!/usr/bin/env bash
# The Mach-O linker for `cargo build --target x86_64-apple-darwin` on Linux
# (§103.9): clang as the driver, lld's ld64 flavour, and the macOS SDK copied
# out of the VM's Command Line Tools (scripts/macos-sdk.sh). The SDK is Apple's
# and is never committed; WC_MACOS_SDK names where it is.
set -euo pipefail
sdk=${WC_MACOS_SDK:-/mnt/datasets/builds/macos-sdk/MacOSX14.5.sdk}
libs=${WC_MACOS_LIBS:-/mnt/datasets/builds/macos-sdk/xkb}
[ -d "$sdk" ] || { echo "macos-linker: no SDK at $sdk; run scripts/macos-sdk.sh" >&2; exit 1; }
: "${WC_MACOS_CLANG:?run inside nix develop}" "${WC_MACOS_LLD_BIN:?run inside nix develop}"
PATH="$WC_MACOS_LLD_BIN:$PATH" exec "$WC_MACOS_CLANG" --target=x86_64-apple-macosx14.0 -isysroot "$sdk" \
  -fuse-ld=lld -L"$libs" "$@"
