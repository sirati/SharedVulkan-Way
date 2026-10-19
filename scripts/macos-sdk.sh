#!/usr/bin/env bash
# Copy the macOS SDK and MacPorts' static libxkbcommon out of the macOS VM
# (§103.9), for scripts/macos-linker.sh. Run once per VM template.
set -euo pipefail
inst=${MACVM_INSTANCE:-wayland-host}
dest=${1:-/mnt/datasets/builds/macos-sdk}
mkdir -p "$dest/xkb"
vm() { /mnt/datasets/macvm/bin/macos-vm ssh "$inst" "$@"; }
vm 'cd /Library/Developer/CommandLineTools/SDKs && tar cf - MacOSX14.5.sdk' | tar xf - -C "$dest" 2>/dev/null
vm 'cat /opt/local/lib/libxkbcommon.a' > "$dest/xkb/libxkbcommon.a"
echo "SDK in $dest/MacOSX14.5.sdk ($(du -sh "$dest/MacOSX14.5.sdk" | cut -f1))"
