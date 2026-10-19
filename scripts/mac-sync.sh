#!/usr/bin/env bash
# Copy this worktree's sources into the macOS VM (§103.9) and optionally run a
# command there in the copy. Usage: scripts/mac-sync.sh [INSTANCE] [-- CMD...]
set -euo pipefail
inst=${MACVM_INSTANCE:-wayland-host}
root=$(cd "$(dirname "$0")/.." && pwd)
mvm=/mnt/datasets/macvm/bin/macos-vm
port=$(sed -n 's/.*"ssh_port": \([0-9]*\).*/\1/p' /mnt/datasets/macvm/instances/$inst/instance.json)
user=$(sed -n 's/.*"user": "\([^"]*\)".*/\1/p' /mnt/datasets/macvm/instances/$inst/instance.json)
ssh_cmd="ssh -i /mnt/datasets/macvm/support/id_ed25519 -p $port -o UserKnownHostsFile=/mnt/datasets/macvm/instances/$inst/known_hosts -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes"
rsync -a --delete -e "$ssh_cmd" \
  --exclude target --exclude .git --exclude '.kache*' --exclude 'docs/' --exclude '*.png' \
  "$root/" "$user@127.0.0.1:src/svb/"
# The Vulkan registry the flake pins: sv-proto's generated code and its
# protocol hash must be the Linux side's, byte for byte.
vkreg=$(ls -d /nix/store/*-vulkan-headers-1.4.350.0/share/vulkan/registry 2>/dev/null | head -1)
[ -n "$vkreg" ] && rsync -a -e "$ssh_cmd" "$vkreg/" "$user@127.0.0.1:src/vkreg/"
if [ "${1:-}" = "--" ]; then shift; exec $mvm ssh "$inst" "cd ~/src/svb && export PATH=\$HOME/.cargo/bin:/opt/local/bin:\$PATH PKG_CONFIG_PATH=/opt/local/lib/pkgconfig VK_XML=\$HOME/src/vkreg/vk.xml LIBRARY_PATH=\$HOME/src/xkb-static && $*"; fi
