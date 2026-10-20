#!/usr/bin/env bash
set -euo pipefail
S=/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland
export HOME="$S/host-home" XDG_RUNTIME_DIR="$S/host-runtime"
export XDG_CONFIG_HOME="$S/host-config" XDG_CACHE_HOME="$S/host-cache"
export XDG_DATA_HOME="$S/host-data" XDG_STATE_HOME="$S/host-state"
mkdir -m 700 -p "$HOME" "$XDG_RUNTIME_DIR" "$XDG_CONFIG_HOME" "$XDG_CACHE_HOME" "$XDG_DATA_HOME" "$XDG_STATE_HOME"
exec systemd-run --user --scope --quiet --unit=linux-bar-rdd-product-wayland-compositor-20261020 --slice=builds-10g.slice -p MemoryMax=10G -p MemorySwapMax=0 \
 timeout --signal=TERM --kill-after=10 480 "$S/snapshot-v3/assets/wc-compositor" \
 --provide-qemu-bar "0:28:wayland-bar:$S/qmp.sock" --wayland-rings "socket:$S/wcbell.sock" \
 --arena-outbound 0:8388608 --output 800x600 --socket product-fixture --timeout 450
