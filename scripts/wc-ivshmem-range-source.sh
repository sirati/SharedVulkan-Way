#!/usr/bin/env bash
# Prepare pinned upstream driver sources plus our protected range overlay.
# This fetches source only. It never builds, signs, installs or loads a driver.
set -euo pipefail
upstream=d92b9c073ec1108f51846d0bd109c5796b46fd36
root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:?usage: wc-ivshmem-range-source.sh EMPTY-DESTINATION}
if [[ -e $dest ]]; then
    echo "destination already exists: $dest" >&2
    exit 1
fi
git init -q "$dest"
git -C "$dest" remote add origin https://github.com/virtio-win/kvm-guest-drivers-windows.git
git -C "$dest" fetch --quiet --depth=1 origin "$upstream"
git -C "$dest" checkout --quiet --detach FETCH_HEAD
[[ $(git -C "$dest" rev-parse HEAD) == "$upstream" ]]
# vcxproj is upstream CRLF/BOM. git apply handles its whitespace in the patch.
git -C "$dest" apply --check --ignore-space-change "$root/drivers/windows-ivshmem/protected-ranges.patch"
git -C "$dest" apply --ignore-space-change "$root/drivers/windows-ivshmem/protected-ranges.patch"
cp "$root/drivers/windows-ivshmem/Range.c" "$root/drivers/windows-ivshmem/Range.h" "$root/drivers/windows-ivshmem/RangeBounds.h" "$dest/ivshmem/"
echo "prepared pinned driver source at $dest; no build or installation performed"
