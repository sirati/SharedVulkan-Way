set -eu
exec 2>&1
# Hide endpoint paths from the app to require the genuine render-node relay.
# The hostd still owns the listening sockets and registers each with root CUSE.
chmod 000 /run/private-cuse/user/gpu
runuser -u alice -- env VK_DRIVER_FILES=/nix/store/v01q3bm5nar7ygkrym0zhm79cii8gj69-sv-icd-driver-1c3869c3/share/vulkan/icd.d/sv_icd.json LD_LIBRARY_PATH=/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0/lib SV_SOCKET=/run/private-cuse/user/gpu/gpu.sock SV_BELL=/run/private-cuse/user/bell.sock SV_DRM_NODE=/dev/dri/renderD190 timeout 90 /tmp/shared/vk-probe --fill-buffer
