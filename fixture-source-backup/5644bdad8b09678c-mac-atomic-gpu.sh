#!/bin/bash
set -euo pipefail
fixture=$(mktemp -d /tmp/wc-hl-vulkan.XXXXXXXX)
server= client=
cleanup() {
 for p in "$client" "$server"; do if [[ -n "$p" ]]; then kill "$p" 2>/dev/null || true; fi; done
 for p in "$client" "$server"; do if [[ -n "$p" ]]; then wait "$p" 2>/dev/null || true; fi; done
 for f in server direct relay; do if [[ -f "$fixture/$f.log" ]]; then echo "BEGIN $f"; cat "$fixture/$f.log"; echo "END $f"; fi; done
 rm -rf -- "$fixture"
}
trap cleanup EXIT
hl=/nix/store/q60kkfcyzicx2bb55hak175h80p4dgdn-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
host=/nix/store/r01q0savjdpd60fnhppxlk9kr78fk1rh-waycompose-hyperlinux-native-arm64-apple-darwin-0.1.0/bin/sv-guest
/usr/bin/codesign --verify --strict "$hl"
/usr/bin/codesign --verify --strict "$host"
mkdir -p "$fixture"/{home,runtime,root/lib,shm,tmp}
chmod 700 "$fixture/runtime"
ln -s /nix/store/hx9wvdx2l59iahwf9nmrx7rwkm6cymgs-glibc-2.44-25/lib/ld-linux-aarch64.so.1 "$fixture/root/lib/ld-linux-aarch64.so.1"
export TMPDIR=/tmp HOME="$fixture/home" XDG_RUNTIME_DIR="$fixture/runtime"
export SV_VULKAN_LOADER=/nix/store/0sc5lczip91wvwmzj3xxk990q4plbclr-vulkan-loader-1.4.350.0/lib/libvulkan.1.dylib
export VK_DRIVER_FILES=/nix/store/3a3np3x8b0j413inmgvqls46qpvp7d7l-kosmickrisp-aarch64-darwin-26.2.4/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json
export VK_ICD_FILENAMES="$VK_DRIVER_FILES"
unset DISPLAY XAUTHORITY WAYLAND_DISPLAY DYLD_LIBRARY_PATH DYLD_INSERT_LIBRARIES
"$host" gpu hl-local --runtime "$fixture/runtime" --socket "$fixture/runtime/gpu" --bell "$fixture/runtime/bell" > "$fixture/server.log" 2>&1 & server=$!
for ((i=0;i<50;i++)); do [[ -S "$fixture/runtime/gpu.relay" ]] && break; kill -0 "$server"; sleep .1; done
[[ -S "$fixture/runtime/gpu.relay" ]]
export XDG_RUNTIME_DIR=/run/user/501
export VK_DRIVER_FILES=/nix/store/0brz15lh7d12gg6fmi1mbbn9dfvlpf6k-sv-icd-hyperlinux-aarch64-unknown-linux-gnu-0.1.0/share/vulkan/icd.d/sv_icd.json
export VK_ICD_FILENAMES="$VK_DRIVER_FILES" SV_DRM_NODE=/dev/dri/renderD128 WC_HL_RELAY="$fixture/runtime/gpu.relay"
hl_args=( "$hl" --timeout 12 --audio-backend null --fs-mode=rooted --isolated --sysroot "$fixture/root" --guest-home /home/user --guest-cwd /home/user --bind "$HOME:/home/user" --bind "$fixture/shm:/dev/shm" --bind "$fixture/tmp:/tmp" --bind "$fixture/runtime:/run/user/501" --bind "$fixture:$fixture" --bind /nix/store:/nix/store:ro )
for mode in direct relay; do
 if [[ "$mode" == direct ]]; then export SV_SOCKET=/run/user/501/gpu SV_BELL=/run/user/501/bell; else export SV_SOCKET=/unmapped/gpu SV_BELL=/unmapped/bell; fi
 "${hl_args[@]}" /nix/store/2nnzxk5h3wy9n2nd1cyjd7jwavyx9lk6-vulkan-tools-1.4.357.0/bin/vulkaninfo --summary > "$fixture/$mode.log" 2>&1 & client=$!
 wait "$client"; client=
 /usr/bin/grep -E 'deviceName.*Apple|deviceName.*A18|deviceName.*Kosmic' "$fixture/$mode.log"
done
echo 'Final public ARM64 ELF ICD direct and render-node relay passed'
