#!/usr/bin/env bash
set -euo pipefail
E=/mnt/datasets/builds/evidence/linux-bar-rdd/product-wayland
S=/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland
export LD_LIBRARY_PATH=/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0/lib
export VK_DRIVER_FILES=/run/opengl-driver/share/vulkan/icd.d/nvidia_icd.json
exec systemd-run --user --scope --quiet --unit=linux-bar-rdd-product-wayland-gpu-20261020 --slice=builds-10g.slice -p MemoryMax=10G -p MemorySwapMax=0 \
 "$S/snapshot-v3/assets/sv-gpu-server" pool-guest --pool-file /dev/shm/linux-bar-rdd-product-wayland-gpu --bell "socket:$S/svbell.sock" --sessions 16 --idle 20 --exit-idle 120
