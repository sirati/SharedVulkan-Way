#!/bin/bash
set -euo pipefail
fixture=$(mktemp -d /tmp/wc-hl-vulkan.XXXXXXXX)
server= client=
cleanup() {
 for p in "$client" "$server"; do if [[ -n "$p" ]]; then kill "$p" 2>/dev/null || true; fi; done
 for p in "$client" "$server"; do if [[ -n "$p" ]]; then wait "$p" 2>/dev/null || true; fi; done
 for f in server zink; do if [[ -f "$fixture/$f.log" ]]; then echo "BEGIN $f"; cat "$fixture/$f.log"; echo "END $f"; fi; done
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
"$host" gpu hl-local --runtime "$fixture/runtime" --socket "$fixture/runtime/gpu" --bell "$fixture/runtime/bell" --sessions 1 > "$fixture/server.log" 2>&1 & server=$!
for ((i=0;i<50;i++)); do [[ -S "$fixture/runtime/gpu.relay" ]] && break; kill -0 "$server"; sleep .1; done
[[ -S "$fixture/runtime/gpu.relay" ]]
export XDG_RUNTIME_DIR=/run/user/501
export VK_DRIVER_FILES=/nix/store/0brz15lh7d12gg6fmi1mbbn9dfvlpf6k-sv-icd-hyperlinux-aarch64-unknown-linux-gnu-0.1.0/share/vulkan/icd.d/sv_icd.json
export VK_ICD_FILENAMES="$VK_DRIVER_FILES" SV_DRM_NODE=/dev/dri/renderD128 WC_HL_RELAY="$fixture/runtime/gpu.relay"
hl_args=( "$hl" --timeout 12 --audio-backend null --fs-mode=rooted --isolated --sysroot "$fixture/root" --guest-home /home/user --guest-cwd /home/user --bind "$HOME:/home/user" --bind "$fixture/shm:/dev/shm" --bind "$fixture/tmp:/tmp" --bind "$fixture/runtime:/run/user/501" --bind "$fixture:$fixture" --bind /nix/store:/nix/store:ro )
export SV_SOCKET=/run/user/501/gpu SV_BELL=/run/user/501/bell
export EGL_PLATFORM=surfaceless MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink
export LIBGL_DRIVERS_PATH=/nix/store/1za29y8csjim0lvvc9j8xya3rbagj6p6-mesa-zink-va-aarch64-unknown-linux-gnu-26.3.0-devel-42c325ea/lib/dri
export __EGL_VENDOR_LIBRARY_FILENAMES=/nix/store/1za29y8csjim0lvvc9j8xya3rbagj6p6-mesa-zink-va-aarch64-unknown-linux-gnu-26.3.0-devel-42c325ea/share/glvnd/egl_vendor.d/50_mesa.json
export LD_LIBRARY_PATH=/nix/store/1za29y8csjim0lvvc9j8xya3rbagj6p6-mesa-zink-va-aarch64-unknown-linux-gnu-26.3.0-devel-42c325ea/lib
/usr/bin/python3 - "$fixture" "${hl_args[@]}" /nix/store/pj47q83qh0mk9192a8g7mxmdaiislv4c-hl-zink-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-zink-client <<'INNER' > "$fixture/zink.log" 2>&1
import subprocess,sys,socket,struct
fixture=sys.argv[1]
client=subprocess.Popen(sys.argv[2:]+['--hold'],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
seen=False
try:
 for line in client.stdout:
  print(line,end='',flush=True)
  if line.strip()!='HOLD_READY': continue
  seen=True
  for index in range(12):
   with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as extra:
    extra.settimeout(2);extra.connect(fixture+'/runtime/gpu')
    header=extra.recv(4,socket.MSG_WAITALL);assert len(header)==4
    length=struct.unpack('<I',header)[0];assert length<4096
    body=extra.recv(length,socket.MSG_WAITALL)
    assert b'all native GPU session slots' in body,body
  print('twelve excess native GPU sessions refused while the original owner remained active',flush=True)
 assert client.wait()==0
 assert seen
finally:
 if client.poll() is None: client.kill();client.wait()
INNER
echo 'Stock Zink rendering continued after native admission saturation'
