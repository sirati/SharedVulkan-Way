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
#               VM  : sv-guest gpu guest --label L: a wired (or superpage) window
#                     of guest RAM, stamped, with the Hello offered in its first
#                     page (sv_transport::coldpage); no socket, no port;
#               here: sv-hostd qemu-serve --cold mem:L sweeping QEMU's memfd
#                     (QMP for the region table) for the live offer, replying in
#                     the same page, and scripts/vk-present-probe.c through
#                     sv-icd: a VK_EXT_headless_surface swapchain whose frames
#                     are compared byte for byte with what the probe drew.
#             The GPU path crosses the VM boundary only through guest RAM.
#             ssh is the bootstrap: it copies the binary and starts the server
#             with the run's label (the one value the host is told, and it is
#             told by being chosen here). Nothing of the session goes over it,
#             which the script checks: the server's sockets in the VM (lsof) and
#             sv-hostd's here.
#   compute : the same path with sv-compute-e2e as the client: a compute shader
#             dispatched over the rings, run by lavapipe in the VM, its output
#             read back on this side through the arena and checked.
#   window  : the same GPU session, and vkcube --wsi wayland as the client of
#             scripts/mac-window.sh: drawn by lavapipe in the VM, presented by
#             sv-icd as Wayland buffers through wc-proxy's arena to the macOS
#             compositor, a window on the VM's desktop. WC_SECS (40),
#             WC_SHOT_AFTER (15) and MAC_GPU_CUBE_FRAMES (vkcube --c, 400) apply.
#             The Wayland connection is mac-window.sh's (§103.13/§103.14).
#
# Env: MACVM_INSTANCE (wayland-host), MAC_GPU_ARENA_MIB (64),
#      VK_PRESENT_PROBE_FRAMES / _W / _H (passed through; 3 frames of 256x256),
#      SV_PRESENT_SYNC (passed through), MAC_GPU_BIN (the macOS sv-guest),
#      MAC_GPU_WAIT_OFFER=1 starts sv-hostd only once the server has offered
#      (watching its log over ssh), so the cold path is timed alone, as the ssh
#      path's was; MAC_GPU_BELL (the server's --bell, e.g. dev:cu.NAME),
#      WC_KEEP=1 keeps the run dir.
#      MAC_GPU_VIA=session (the default, §103.15): the server is started the
#      way a session starts it. The harness sets gpu-server/gpu-icd in the
#      installed launcher's wc-launch.ini (scripts/mac-launch-install.sh first)
#      and starts its main process in the user's GUI session. The main process
#      starts `sv-guest gpu guest --label <session>`, its boot record says so,
#      and the host's wc-mac-sessiond runs `wc-session gpu`, which starts
#      sv-hostd qemu-serve. The probe uses that session's icd.sock. No ssh
#      starts anything of the session: the harness's ssh only copies the binary,
#      edits the ini and opens the main process, as a user would. MAC_GPU_VIA=ssh
#      is the §103.6 bootstrap (ssh starts the server; this script starts
#      sv-hostd).
# Uses /tmp/wcgpu and ~/wc/gpu in the VM, so it does not meet
# scripts/mac-window.sh's /tmp/wcrt.
# QMP takes one client: sv-hostd retries while another harness holds it.
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
# The VM's account comes from the VM store, not from this repository.
VMUSER=$(sed -n 's/.*"user": "\([^"]*\)".*/\1/p' "$idir/instance.json")
mvm=/mnt/datasets/macvm/bin/macos-vm
arena=$(( ${MAC_GPU_ARENA_MIB:-64} * 1024 * 1024 ))
local_bin=${MAC_GPU_BIN:-$root/target/x86_64-apple-darwin/debug/sv-guest}
port=$(sed -n 's/.*"ssh_port": \([0-9]*\).*/\1/p' "$idir/instance.json")
ssh_opts=(-i /mnt/datasets/macvm/support/id_ed25519 -p "$port" -o UserKnownHostsFile="$idir/known_hosts"
          -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes)
LVP='$HOME/lvp/share/vulkan/icd.d/lvp_icd.x86_64.json'
MVK=/opt/local/share/vulkan/icd.d/MoltenVK_icd.json
BIN='~/wc/gpu/sv-guest'
# The run's label: the offer the host takes is the one carrying it.
label=mg$$
vm() { "$mvm" ssh "$inst" "$@"; }
say() { echo "[mac-gpu] $*"; }
now() { date +%s.%N; }
dt() { awk "BEGIN{printf \"%.3f\", $2 - $1}"; }
run=/mnt/datasets/builds/wcrun/macgpu-$(date +%Y%m%d-%H%M%S)
mkdir -p "$run/f" ${WC_EVIDENCE:-/mnt/datasets/builds/evidence/macos-host}/macgpu-last
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  vm "pkill -f 'sv-guest gpu guest --label $label ' 2>/dev/null; true" >/dev/null 2>&1
  cp "$run"/*.log ${WC_EVIDENCE:-/mnt/datasets/builds/evidence/macos-host}/macgpu-last/ 2>/dev/null
  [ "${WC_KEEP:-0}" = 1 ] || rm -rf "$run"
}
trap cleanup EXIT

[ -x "$local_bin" ] || { say "no macOS build at $local_bin (cargo build --target x86_64-apple-darwin -p sv-guest)"; exit 2; }
for b in sv-hostd libsv_icd.so sv_icd.json; do
  [ -e "$root/target/debug/$b" ] || { say "build first: cargo build -p sv-icd -p sv-hostd"; exit 2; }
done
vm 'mkdir -p ~/wc/gpu'
rsync -a -e "ssh ${ssh_opts[*]}" "$local_bin" $VMUSER@127.0.0.1:wc/gpu/sv-guest || exit 1
fail=0

if [ "$what" = probe ] || [ "$what" = both ]; then
  say "probe: lavapipe"
  vm "ulimit -c 0; cd /tmp; VK_DRIVER_FILES=$LVP $BIN gpu probe 2>&1; echo rc=\$?" > "$run/probe-lvp.log" 2>&1
  grep -E '^\[gpu\]|rc=' "$run/probe-lvp.log" | sed 's/^/  /'
  grep -q 'rc=0' "$run/probe-lvp.log" || fail=1
  say "probe: MoltenVK alone (this VM has no Metal; expect a clean refusal)"
  vm "ulimit -c 0; cd /tmp; MVK_CONFIG_LOG_LEVEL=1 VK_DRIVER_FILES=$MVK $BIN gpu probe 2>&1; echo rc=\$?" > "$run/probe-mvk.log" 2>&1
  grep -E 'mvk-error|sv-gpu-server:|rc=' "$run/probe-mvk.log" | sed 's/^/  /'
  grep -q 'rc=1' "$run/probe-mvk.log" && grep -q 'portability enumeration enabled' "$run/probe-mvk.log" || fail=1
  say "probe: MoltenVK and lavapipe (the loader skips MoltenVK)"
  vm "ulimit -c 0; cd /tmp; MVK_CONFIG_LOG_LEVEL=1 VK_LOADER_DEBUG=warn VK_DRIVER_FILES=$MVK:$LVP $BIN gpu probe 2>&1; echo rc=\$?" > "$run/probe-both.log" 2>&1
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
  bell=()
  [ -n "${MAC_GPU_BELL:-}" ] && bell=(--bell "$MAC_GPU_BELL")
  icd="$run/icd.sock"; gpulog=/tmp/wcgpu/gpu.log
  tl=$(now)
  if [ "${MAC_GPU_VIA:-session}" = session ]; then
    G='Library/Application Support/waycompose'
    [ "$what" = window ] && { say "window: use MAC_GPU_VIA=ssh (mac-window.sh runs its own compositor)"; exit 2; }
    systemctl --user is-active --quiet wc-mac-sessiond || { say "no wc-mac-sessiond: run scripts/mac-launch-install.sh first"; exit 2; }
    vm "cat \"\$HOME/$G/wc-launch.ini\"" > "$run/ini.orig" || exit 1
    awk -v u="/Users/$VMUSER" -v mib="${MAC_GPU_ARENA_MIB:-64}" '
      /^gpu-(server|icd|arena-mib) =/ { next }
      { print }
      $0 == "[session]" {
        print "gpu-server = " u "/wc/gpu/sv-guest"
        print "gpu-icd = " u "/lvp/share/vulkan/icd.d/lvp_icd.x86_64.json"
        print "gpu-arena-mib = " mib
      }' "$run/ini.orig" > "$run/ini.new"
    vm "cat > \"\$HOME/$G/wc-launch.ini\"" < "$run/ini.new" || exit 1
    gpulog='~/Library/Logs/waycompose/gpu.log'
    # The main process, as a face would start it. No face connects in this
    # test, so the bootstrap namespace it registers in does not matter.
    vm "pkill -f 'wc-launch --main' 2>/dev/null; rm -f ~/Library/Logs/waycompose/main.log; \
        nohup \"\$HOME/$G/bin/wc-launch\" --main >/dev/null 2>&1 < /dev/null &"
    tv=$(now); th0=$tv
    sid=""
    for _ in $(seq 1 300); do
      sid=$(vm "sed -n 's/.*main process: session \(wcm-[0-9]*\).*/\1/p' ~/Library/Logs/waycompose/main.log 2>/dev/null | tail -1")
      [ -n "$sid" ] && [ -S "/tmp/wc-launch/$sid/icd.sock" ] && break
      sleep 0.1
    done
    th1=$(now)
    icd="/tmp/wc-launch/$sid/icd.sock"
    [ -s "/tmp/wc-launch/$sid/bell.path" ] && export SV_BELL="$(cat "/tmp/wc-launch/$sid/bell.path")"
    hostd=$(systemctl --user show -p MainPID --value "wc-launch-gpu-$sid" 2>/dev/null)
    journalctl --user -u wc-mac-sessiond --since "-60s" --no-pager -o cat | grep -E "$sid" | sed 's/^/  /'
    [ -S "$icd" ] || { say "the session ($sid) never served a GPU"; vm "cat ~/Library/Logs/waycompose/main.log; tail -5 $gpulog"; exit 1; }
    say "TIMING main process opened to the session's GPU served: $(dt "$tv" "$th1") s (boot record, locate, proxy, sv-hostd's claim)"
    # The main process goes (its lease ends the session on the host), and
    # the ini is what it was.
    cleanup_session() {
      vm "pkill -f 'wc-launch --main'; true" >/dev/null 2>&1
      vm "cat > \"\$HOME/$G/wc-launch.ini\"" < "$run/ini.orig" >/dev/null 2>&1
    }
    trap 'cleanup_session; cleanup' EXIT
  else
  # The bootstrap: start the server with this run's label. Its cold path is
  # its own first page from here on.
  vm "rm -rf /tmp/wcgpu; mkdir -m 700 /tmp/wcgpu; cd /tmp/wcgpu; ulimit -c 0; \
      VK_DRIVER_FILES=$LVP nohup $BIN gpu guest --label $label --arena $arena --idle $idle ${bell[*]} \
      > /tmp/wcgpu/gpu.log 2>&1 < /dev/null &"
  tv=$(now)
  if [ "${MAC_GPU_WAIT_OFFER:-0}" = 1 ]; then
    for _ in $(seq 1 60); do vm 'grep -q "offered the extent" /tmp/wcgpu/gpu.log 2>/dev/null' && break; sleep 0.2; done
  fi
  th0=$(now)
  "$root/target/debug/sv-hostd" qemu-serve --qmp "$idir/qmp.sock" --vm "$idir" \
    --cold "mem:$label" --socket "$run/icd.sock" --clients 1 > "$run/hostd.log" 2>&1 &
  hostd=$!
  pids+=($hostd)
  for _ in $(seq 1 12000); do [ -S "$run/icd.sock" ] && break; kill -0 $hostd 2>/dev/null || break; sleep 0.01; done
  th1=$(now)
  grep -E "claimed|located|stitched|published|read again" "$run/hostd.log" | sed 's/^/  /'
  [ -S "$run/icd.sock" ] || { say "sv-hostd did not take the offer"; tail -5 "$run/hostd.log"; vm 'cat /tmp/wcgpu/gpu.log' | tail -5; exit 1; }
  if [ "${MAC_GPU_WAIT_OFFER:-0}" = 1 ]; then
    say "TIMING cold path (sweep, claim, check, Accept, control header), the server already offering: $(dt "$th0" "$th1") s"
  else
    say "TIMING server started over ssh in $(dt "$tl" "$tv") s; sv-hostd from start to serving: $(dt "$th0" "$th1") s"
  fi
  fi
  # Every socket sv-hostd holds, by kind (its descriptors' inodes against
  # /proc/net): QMP was read once for the region table and closed; what is
  # left is the ICD's listening socket. No TCP.
  ino=$(ls -l /proc/$hostd/fd 2>/dev/null | sed -n 's/.*socket:\[\([0-9]*\)\]/\1/p')
  ntcp=0; nunix=0
  for i in $ino; do
    awk -v i="$i" '$10 == i {f=1} END {exit !f}' /proc/net/tcp /proc/net/tcp6 && ntcp=$((ntcp + 1))
    awk -v i="$i" '$7 == i {f=1} END {exit !f}' /proc/net/unix && nunix=$((nunix + 1))
  done
  say "sv-hostd holds $nunix Unix socket(s) and $ntcp TCP socket(s): $(ls -l /proc/$hostd/fd 2>/dev/null | grep -c socket) socket(s) in all"
  [ "$ntcp" = 0 ] || fail=1
  # The server in the VM, the same question, while it serves.
  [ "${MAC_GPU_VIA:-session}" = session ] && label=${sid:0:16}
  vm "p=\$(pgrep -f 'sv-guest gpu guest --label $label ' | head -1); echo pid=\$p; lsof -nP -a -p \$p -i 2>/dev/null | tail -n +2 | wc -l | sed 's/^ */inet=/'; lsof -nP -a -p \$p -U 2>/dev/null | tail -n +2 | wc -l | sed 's/^ */unix=/'" > "$run/vmsock.log" 2>&1
  inet=$(sed -n 's/^inet=//p' "$run/vmsock.log"); unix=$(sed -n 's/^unix=//p' "$run/vmsock.log")
  if grep -q '^pid=[0-9]' "$run/vmsock.log"; then
    say "VM: the serving server holds $inet network socket(s) and $unix Unix socket(s)"
    [ "$inet" = 0 ] || fail=1
  else
    say "VM: no server process to check"; fail=1
  fi
  t0=$(now)
  dumpenv=()
  [ "$dump" = 1 ] && dumpenv=(SV_PRESENT_DUMP="$run/f/got-" VK_PRESENT_PROBE_EXPECT="$run/f/want-")
  if [ "$what" = window ]; then
    WC_SECS=${WC_SECS:-40} WC_SHOT_AFTER=${WC_SHOT_AFTER:-15} "$root/scripts/mac-window.sh" \
      env VK_DRIVER_FILES="$root/target/debug/sv_icd.json" SV_SOCKET="$icd" LD_LIBRARY_PATH="$wllib" \
      "$probe" --wsi wayland --width 400 --height 300 --c "${MAC_GPU_CUBE_FRAMES:-400}" > "$run/app.log" 2>&1
    rc=$?
    cp ${WC_EVIDENCE:-/mnt/datasets/builds/evidence/macos-host}/mac-last/client.log "$run/client.log" 2>/dev/null
    sed 's/^/  /' "$run/app.log"
    grep -E '^\[icd\]|sv-icd:|rror' "$run/client.log" 2>/dev/null | head -8 | sed 's/^/  client: /'
  else
    ( cd "$run" && nix develop "$root" -c env VK_DRIVER_FILES="$root/target/debug/sv_icd.json" SV_SOCKET="$icd" \
        "${dumpenv[@]}" timeout 180 stdbuf -oL "$probe" ) 2>&1 \
      | while IFS= read -r l; do echo "$(now) $l" >> "$run/app.ts"; echo "$l"; done > "$run/app.log"
    rc=${PIPESTATUS[0]}
    first=$(awk '/surface support/{print $1; exit}' "$run/app.ts" 2>/dev/null)
    f0=$(awk '/frame 0 presented/{print $1; exit}' "$run/app.ts" 2>/dev/null)
    [ -n "$first" ] && [ -n "$f0" ] && say "TIMING first frame $(dt "$first" "$f0") s after the probe's first line; $(dt "$tl" "$f0") s after the server was started (nix develop included)"
  fi
  t1=$(now)
  grep -E '^\[icd\]|^swapchain|^frame|^timing|PASS|FAIL|pass|fail|device|words|checked' "$run/app.log" | head -20 | sed 's/^/  /'
  same=0; differ=0
  for w in "$run"/f/want-*.ppm; do
    [ -e "$w" ] || continue
    if cmp -s "$w" "${w/want-/got-}"; then same=$((same + 1)); else differ=$((differ + 1)); fi
  done
  say "probe rc=$rc in $(dt "$t0" "$t1") s; frames identical: $same, differing: $differ"
  if [ "$dump" = 1 ]; then
    [ "$rc" -eq 0 ] && [ "$same" -gt 0 ] && [ "$differ" -eq 0 ] || fail=1
  else
    [ "$rc" -eq 0 ] || fail=1
  fi
  sleep 1
  vm "cat $gpulog" > "$run/gpu.log"
  grep -E 'offered|claimed|accepted|imported|published|dispatched' "$run/gpu.log" | sed 's/^/  VM: /'
fi
[ $fail -eq 0 ] && say PASS || say FAIL
exit $fail
