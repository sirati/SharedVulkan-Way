#!/usr/bin/env bash
# §103.6, §103.11: a Linux Vulkan client on this workstation, rendered by
# sv-gpu-server inside the macOS VM, frames back through guest RAM.
#
#   scripts/mac-gpu.sh [probe|present|compute|window|both]     (default: both)
#
#   probe   : in the VM, `sv-guest gpu probe` against lavapipe, against MoltenVK
#             alone (no Metal: the server must say so and exit 1, not crash),
#             and against both (the loader skips MoltenVK, the server takes
#             lavapipe). Prints the three verdicts.
#   present : the whole GPU path, headless:
#               VM  : sv-guest gpu guest --port P: a wired (or superpage) window
#                     of guest RAM, stamped, published on 127.0.0.1:P;
#               here: an ssh forward of P to a Unix socket (the cold path),
#                     sv-hostd qemu-serve locating the stamps in QEMU's memfd over
#                     QMP, and scripts/vk-present-probe.c through sv-icd:
#                     a VK_EXT_headless_surface swapchain whose frames are
#                     compared byte for byte with what the probe drew.
#   compute : the same path with sv-compute-e2e as the client: a compute shader
#             dispatched over the rings, run by lavapipe in the VM, its output
#             read back on this side through the arena and checked.
#   window  : the same GPU session, and vkcube --wsi wayland as the client of
#             scripts/mac-window.sh: drawn by lavapipe in the VM, presented by
#             sv-icd as Wayland buffers through wc-proxy's arena to the macOS
#             compositor, a window on the VM's desktop. WC_SECS (40),
#             WC_SHOT_AFTER (15) and MAC_GPU_CUBE_FRAMES (vkcube --c, 400) apply.
#
# Env: MACVM_INSTANCE (wayland-host), MAC_GPU_PORT (57460), MAC_GPU_ARENA_MIB (64),
#      VK_PRESENT_PROBE_FRAMES / _W / _H (passed through; 3 frames of 256x256),
#      SV_PRESENT_SYNC (passed through), MAC_GPU_BIN (the macOS sv-guest),
#      WC_KEEP=1 keeps the run dir.
# Uses /tmp/wcgpu in the VM, so it does not meet scripts/mac-window.sh's /tmp/wcrt.
# QMP takes one client: do not run this while another harness is locating.
set -uo pipefail
ulimit -c 0
what=${1:-both}
# Presents inside vkQueuePresentKHR by default. With the headless target's
# pipelined presenter, the dump of frame N holds frame N+1's pixels (the last
# frame is right). That is the same on Linux with RADV and no VM at all
# (sv-gpu-server serve, measured 2026-10-19), so it is an sv-icd matter and not
# this path's; SV_PRESENT_SYNC=0 shows it.
export SV_PRESENT_SYNC=${SV_PRESENT_SYNC:-1}
# MAC_GPU_DUMP=0: no frame dumps and no comparison, for a timed run with
# VK_PRESENT_PROBE_FRAMES/_W/_H.
dump=${MAC_GPU_DUMP:-1}
inst=${MACVM_INSTANCE:-wayland-host}
root=$(cd "$(dirname "$0")/.." && pwd)
idir=/mnt/datasets/macvm/instances/$inst
mvm=/mnt/datasets/macvm/bin/macos-vm
gport=${MAC_GPU_PORT:-57460}
arena=$(( ${MAC_GPU_ARENA_MIB:-64} * 1024 * 1024 ))
local_bin=${MAC_GPU_BIN:-$root/target/x86_64-apple-darwin/debug/sv-guest}
port=$(sed -n 's/.*"ssh_port": \([0-9]*\).*/\1/p' "$idir/instance.json")
ssh_opts=(-i /mnt/datasets/macvm/support/id_ed25519 -p "$port" -o UserKnownHostsFile="$idir/known_hosts"
          -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes)
LVP='$HOME/lvp/share/vulkan/icd.d/lvp_icd.x86_64.json'
MVK=/opt/local/share/vulkan/icd.d/MoltenVK_icd.json
vm() { "$mvm" ssh "$inst" "$@"; }
say() { echo "[mac-gpu] $*"; }
run=/mnt/datasets/builds/wcrun/macgpu-$(date +%Y%m%d-%H%M%S)
mkdir -p "$run/f" /mnt/datasets/builds/wcrun/macgpu-last
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  vm "pkill -f 'sv-guest gpu guest --port $gport ' 2>/dev/null; true" >/dev/null 2>&1
  cp "$run"/*.log /mnt/datasets/builds/wcrun/macgpu-last/ 2>/dev/null
  [ "${WC_KEEP:-0}" = 1 ] || rm -rf "$run"
}
trap cleanup EXIT

[ -x "$local_bin" ] || { say "no macOS build at $local_bin (cargo build --target x86_64-apple-darwin -p sv-guest)"; exit 2; }
for b in sv-hostd libsv_icd.so sv_icd.json; do
  [ -e "$root/target/debug/$b" ] || { say "build first: cargo build -p sv-icd -p sv-hostd"; exit 2; }
done
vm 'mkdir -p ~/wc/bin'
rsync -a -e "ssh ${ssh_opts[*]}" "$local_bin" c3r@127.0.0.1:wc/bin/sv-guest || exit 1
fail=0

if [ "$what" = probe ] || [ "$what" = both ]; then
  say "probe: lavapipe"
  vm "ulimit -c 0; cd /tmp; VK_DRIVER_FILES=$LVP ~/wc/bin/sv-guest gpu probe 2>&1; echo rc=\$?" > "$run/probe-lvp.log" 2>&1
  grep -E '^\[gpu\]|rc=' "$run/probe-lvp.log" | sed 's/^/  /'
  grep -q 'rc=0' "$run/probe-lvp.log" || fail=1
  say "probe: MoltenVK alone (this VM has no Metal; expect a clean refusal)"
  vm "ulimit -c 0; cd /tmp; MVK_CONFIG_LOG_LEVEL=1 VK_DRIVER_FILES=$MVK ~/wc/bin/sv-guest gpu probe 2>&1; echo rc=\$?" > "$run/probe-mvk.log" 2>&1
  grep -E 'mvk-error|sv-gpu-server:|rc=' "$run/probe-mvk.log" | sed 's/^/  /'
  grep -q 'rc=1' "$run/probe-mvk.log" && grep -q 'portability enumeration enabled' "$run/probe-mvk.log" || fail=1
  say "probe: MoltenVK and lavapipe (the loader skips MoltenVK)"
  vm "ulimit -c 0; cd /tmp; MVK_CONFIG_LOG_LEVEL=1 VK_LOADER_DEBUG=warn VK_DRIVER_FILES=$MVK:$LVP ~/wc/bin/sv-guest gpu probe 2>&1; echo rc=\$?" > "$run/probe-both.log" 2>&1
  grep -E 'Skipping|^\[gpu\] [^t]|rc=' "$run/probe-both.log" | sed 's/^/  /'
  grep -q 'rc=0' "$run/probe-both.log" || fail=1
fi

if [ "$what" = present ] || [ "$what" = both ] || [ "$what" = compute ] || [ "$what" = window ]; then
  idle=30
  if [ "$what" = window ]; then
    # vkcube dlopens libwayland-client; mac-window.sh runs its client outside
    # the dev shell, so the library's directory is passed along.
    read -r probe wllib < <(cd "$root" && nix develop -c bash -c \
      'echo "$(command -v vkcube) $(pkg-config --variable=libdir wayland-client)"' 2>/dev/null | tail -1)
    [ -x "$probe" ] || { say "no vkcube in the dev shell"; exit 2; }
    dump=0; idle=120
  elif [ "$what" = compute ]; then
    probe="$root/target/debug/sv-compute-e2e"; dump=0
    [ -x "$probe" ] || { say "build first: cargo build -p sv-gpu-server --bin sv-compute-e2e"; exit 2; }
  else
    probe="$run/vk-present-probe"
    ( cd "$root" && nix develop -c bash -c "cc -O1 -o '$probe' scripts/vk-present-probe.c -I\${VULKAN_SDK:-/usr}/include -ldl" ) \
      > "$run/cc.log" 2>&1 || { say "vk-present-probe did not build"; tail -5 "$run/cc.log"; exit 1; }
  fi
  vm "pkill -f 'sv-guest gpu guest --port $gport ' 2>/dev/null; rm -rf /tmp/wcgpu; mkdir -m 700 /tmp/wcgpu; cd /tmp/wcgpu; ulimit -c 0; \
      VK_DRIVER_FILES=$LVP nohup ~/wc/bin/sv-guest gpu guest --port $gport --arena $arena --idle $idle \
      > /tmp/wcgpu/gpu.log 2>&1 < /dev/null &"
  for _ in $(seq 1 60); do vm 'grep -q "publishing the extent" /tmp/wcgpu/gpu.log 2>/dev/null' && break; sleep 0.5; done
  vm 'cat /tmp/wcgpu/gpu.log' > "$run/gpu.log"
  grep -q 'publishing the extent' "$run/gpu.log" || { say "the GPU side did not start"; tail -5 "$run/gpu.log"; exit 1; }
  say "VM: $(grep -o 'guest-RAM window: .*' "$run/gpu.log")"
  ssh "${ssh_opts[@]}" -N -o StreamLocalBindUnlink=yes -L "$run/cold.sock:127.0.0.1:$gport" c3r@127.0.0.1 > "$run/fwd.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 1 40); do [ -S "$run/cold.sock" ] && break; sleep 0.25; done
  "$root/target/debug/sv-hostd" qemu-serve --qmp "$idir/qmp.sock" --vm "c3r-macos-$inst" \
    --cold "$run/cold.sock" --socket "$run/icd.sock" --clients 1 > "$run/hostd.log" 2>&1 &
  pids+=($!)
  for _ in $(seq 1 120); do [ -S "$run/icd.sock" ] && break; sleep 0.5; done
  grep -E 'located|stitched' "$run/hostd.log" | sed 's/^/  /'
  [ -S "$run/icd.sock" ] || { say "sv-hostd did not locate the extent"; tail -5 "$run/hostd.log"; exit 1; }
  t0=$(date +%s.%N)
  dumpenv=()
  [ "$dump" = 1 ] && dumpenv=(SV_PRESENT_DUMP="$run/f/got-" VK_PRESENT_PROBE_EXPECT="$run/f/want-")
  if [ "$what" = window ]; then
    WC_SECS=${WC_SECS:-40} WC_SHOT_AFTER=${WC_SHOT_AFTER:-15} "$root/scripts/mac-window.sh" \
      env VK_DRIVER_FILES="$root/target/debug/sv_icd.json" SV_SOCKET="$run/icd.sock" LD_LIBRARY_PATH="$wllib" \
      "$probe" --wsi wayland --width 400 --height 300 --c "${MAC_GPU_CUBE_FRAMES:-400}" > "$run/app.log" 2>&1
    rc=$?
    cp /mnt/datasets/builds/wcrun/mac-last/client.log "$run/client.log" 2>/dev/null
    sed 's/^/  /' "$run/app.log"
    grep -E '^\[icd\]|sv-icd:|rror' "$run/client.log" 2>/dev/null | head -8 | sed 's/^/  client: /'
  else
    ( cd "$run" && nix develop "$root" -c env VK_DRIVER_FILES="$root/target/debug/sv_icd.json" SV_SOCKET="$run/icd.sock" \
        "${dumpenv[@]}" timeout 180 "$probe" ) > "$run/app.log" 2>&1
    rc=$?
  fi
  t1=$(date +%s.%N)
  grep -E '^\[icd\]|^swapchain|^frame|^timing|PASS|FAIL|pass|fail|device|words|checked' "$run/app.log" | head -20 | sed 's/^/  /'
  same=0; differ=0
  for w in "$run"/f/want-*.ppm; do
    [ -e "$w" ] || continue
    if cmp -s "$w" "${w/want-/got-}"; then same=$((same + 1)); else differ=$((differ + 1)); fi
  done
  say "probe rc=$rc in $(awk "BEGIN{printf \"%.2f\", $t1 - $t0}") s; frames identical: $same, differing: $differ"
  if [ "$dump" = 1 ]; then
    [ "$rc" -eq 0 ] && [ "$same" -gt 0 ] && [ "$differ" -eq 0 ] || fail=1
  else
    [ "$rc" -eq 0 ] || fail=1
  fi
  sleep 1
  vm 'cat /tmp/wcgpu/gpu.log' > "$run/gpu.log"
  grep -E 'accepted|imported|dispatched' "$run/gpu.log" | sed 's/^/  VM: /'
fi
[ $fail -eq 0 ] && say PASS || say FAIL
exit $fail
