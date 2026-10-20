#!/usr/bin/env bash
# Prepare an exact source overlay only; never build, install or load a module.
set -euo pipefail
[[ $# == 1 && ! -e $1 ]] || { echo "usage: $0 NEW_SOURCE_DIRECTORY" >&2; exit 2; }
repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$1/drivers/dma-buf"
dest=$(realpath "$1")
commit=725bd2f3c81d54edccb66fead01d4c0c222e2231
curl -fL --retry 3 "https://raw.githubusercontent.com/gregkh/linux/$commit/drivers/dma-buf/udmabuf.c" -o "$dest/drivers/dma-buf/udmabuf.c"
echo "0ee30a3bba56b1b4d2bf3669d782699d6845fab7ec0f313a27889a802910b91b  $dest/drivers/dma-buf/udmabuf.c" | sha256sum --check
git -C "$dest" apply --unidiff-zero --check "$repo/drivers/linux-udmabuf/protected.patch"
git -C "$dest" apply --unidiff-zero "$repo/drivers/linux-udmabuf/protected.patch"
cp "$repo"/drivers/linux-udmabuf/udmabuf-protected* "$dest/drivers/dma-buf/"
echo "$commit" > "$dest/UPSTREAM_COMMIT"
echo "Prepared Linux 6.18.55 udmabuf overlay in $dest; no module built or loaded."
