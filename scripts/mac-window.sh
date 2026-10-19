#!/usr/bin/env bash
# §103.11: a Linux client on this workstation, its window on the macOS VM's desktop.
#
#   scripts/mac-window.sh [CLIENT ARGS...]      (default: weston-terminal)
#
# The path, end to end, with nothing a product would not also have:
#   * wc-compositor runs in the VM, in the logged-in user's GUI session
#     (`launchctl asuser`), with its arena in 2 MiB superpages of the VM's RAM
#     (`--arena-guest-ram`, pinning=superpages) and Wayland on a Unix socket in
#     the in-band dialect, because no descriptor crosses (design.md §1a);
#   * wc-arena-host finds the arena's stamped pages in QEMU's memfd through QMP
#     and /proc/<qemu>/fd, as the user; no root, no file;
#   * an OpenSSH Unix-socket forward carries the Wayland bytes (§103.4);
#   * wc-proxy maps the plan and serves the client.
# Env: MACVM_INSTANCE (wayland-host), WC_SECS (run length, 120), WC_ARENA_MIB (64),
#      WC_SHOT_AFTER (seconds, then a QMP screenshot), WC_DURING (a command before it),
#      WC_MAC_BIN (the compositor in the VM), WC_KEEP=1 keeps the run dir.
set -uo pipefail
ulimit -c 0
inst=${MACVM_INSTANCE:-wayland-host}
root=$(cd "$(dirname "$0")/.." && pwd)
idir=/mnt/datasets/macvm/instances/$inst
mvm=/mnt/datasets/macvm/bin/macos-vm
secs=${WC_SECS:-120}
arena_mib=${WC_ARENA_MIB:-64}
arena=$((arena_mib * 1024 * 1024))
out=$((1024 * 1024))
# The compositor is cross-built here (scripts/macos-linker.sh) and copied in.
bin=/Users/c3r/wc/bin/wc-compositor
local_bin=${WC_MAC_BIN:-$root/target/x86_64-apple-darwin/debug/wc-compositor}
run=/mnt/datasets/builds/wcrun/mac-$(date +%Y%m%d-%H%M%S)
mkdir -p "$run/rt" && chmod 700 "$run/rt"
port=$(sed -n 's/.*"ssh_port": \([0-9]*\).*/\1/p' "$idir/instance.json")
ssh_opts=(-i /mnt/datasets/macvm/support/id_ed25519 -p "$port" -o UserKnownHostsFile="$idir/known_hosts"
          -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes)
vm() { "$mvm" ssh "$inst" "$@"; }
say() { echo "[mac-window] $*"; }
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null; done
  vm 'pkill -f "wc-compositor --socket wl-mac" 2>/dev/null; true' >/dev/null 2>&1
  [ "${WC_KEEP:-0}" = 1 ] || { cp "$run"/*.log /mnt/datasets/builds/wcrun/mac-last/ 2>/dev/null; rm -rf "$run"; }
}
mkdir -p /mnt/datasets/builds/wcrun/mac-last
trap cleanup EXIT

bh="$root/target/debug"
for b in wc-arena-host wc-proxy; do [ -x "$bh/$b" ] || { say "build $b first (cargo build -p $b)"; exit 2; }; done

say "run dir $run"
[ -x "$local_bin" ] || { say "no macOS build at $local_bin (cargo build --target x86_64-apple-darwin -p wc-compositor)"; exit 2; }
vm 'mkdir -p ~/wc/bin'
rsync -a -e "ssh ${ssh_opts[*]}" "$local_bin" c3r@127.0.0.1:wc/bin/wc-compositor
vm 'pkill -f "wc-compositor --socket wl-mac" 2>/dev/null; rm -rf /tmp/wcrt; mkdir -m 700 /tmp/wcrt'
# The GUI session: a process started by sshd is outside the user's Aqua
# bootstrap namespace and cannot reach the WindowServer.
vm "sudo -n launchctl asuser 501 sudo -n -u c3r env XDG_RUNTIME_DIR=/tmp/wcrt WC_MAC_TRACE=${WC_MAC_TRACE:-1} WC_MAC_CMD_KEY=\${WC_MAC_CMD_KEY:-control} \
  $bin --socket wl-mac --in-band --timeout $((secs + 30)) --exit-when-empty 20 \
  --arena-guest-ram 0:$arena --arena-outbound 0:$out > /tmp/wcrt/comp.log 2>&1 &"
for i in $(seq 1 60); do vm 'grep -q "listening on" /tmp/wcrt/comp.log 2>/dev/null' && break; sleep 0.5; done
vm 'cat /tmp/wcrt/comp.log' > "$run/comp.log"
grep -q "listening on" "$run/comp.log" || { say "the compositor did not start"; tail -20 "$run/comp.log"; exit 1; }
runid=$(grep -o "run 0x[0-9a-f]*" "$run/comp.log" | head -1 | cut -d' ' -f2)
pages=$(grep -o "[0-9][0-9]* pages, locked" "$run/comp.log" | head -1 | cut -d' ' -f1)
say "compositor up: arena run $runid, $pages pages, $(grep -o 'pinning=[a-z-]*' "$run/comp.log" | head -1)"

t0=$(date +%s.%N)
"$bh/wc-arena-host" --qmp "$idir/qmp.sock" --vm "$idir" --run "$runid" --pages "$pages" --outbound "$out" \
  --plan "$run/arena.plan" > "$run/locate.log" 2>&1
t1=$(date +%s.%N)
[ -s "$run/arena.plan" ] || { say "the arena was not located"; tail -8 "$run/locate.log"; exit 1; }
say "located in $(awk "BEGIN{printf \"%.2f\", $t1 - $t0}") s: $(grep -o 'verify: [0-9]*/[0-9]* pages' "$run/locate.log" | tail -1), $(grep -c '^run' "$run/arena.plan" 2>/dev/null) runs"

ssh "${ssh_opts[@]}" -N -o StreamLocalBindUnlink=yes -L "$run/up.sock:/tmp/wcrt/wl-mac" c3r@127.0.0.1 &
pids+=($!)
for i in $(seq 1 40); do [ -S "$run/up.sock" ] && break; sleep 0.25; done

XDG_RUNTIME_DIR="$run/rt" "$bh/wc-proxy" --upstream "$run/up.sock" --socket wl-proxy --runtime-dir "$run/rt" \
  --transport arena --arena-plan "$run/arena.plan" --arena-id 0 --copy damage \
  --accept-timeout 40 --timeout "$secs" > "$run/proxy.log" 2>&1 &
pids+=($!)
for i in $(seq 1 40); do [ -S "$run/rt/wl-proxy" ] && break; sleep 0.25; done
[ -S "$run/rt/wl-proxy" ] || { say "the proxy did not start"; tail -10 "$run/proxy.log"; exit 1; }

client=("${@:-weston-terminal}")
[ $# -eq 0 ] && client=(weston-terminal)
say "client: ${client[*]} for ${secs}s"
XDG_RUNTIME_DIR="$run/rt" WAYLAND_DISPLAY=wl-proxy timeout "$secs" "${client[@]}" > "$run/client.log" 2>&1 &
pids+=($!)
if [ -n "${WC_SHOT_AFTER:-}" ]; then
  sleep "$WC_SHOT_AFTER"
  # WC_DURING: a command run once the window is up, before the screenshot
  # (input through scripts/mac-input.sh, a clipboard check, ...).
  if [ -n "${WC_DURING:-}" ]; then say "during: $WC_DURING"; bash -c "$WC_DURING" > "$run/during.log" 2>&1; sleep 1; fi
  "$root/scripts/mac-shot.sh" "$run/shot.png" && cp "$run/shot.png" /mnt/datasets/builds/wcrun/mac-last/shot.png \
    && say "screenshot: /mnt/datasets/builds/wcrun/mac-last/shot.png"
fi
wait "${pids[-1]}"
vm 'cat /tmp/wcrt/comp.log' > "$run/comp.log"
say "compositor said: $(grep -c . "$run/comp.log") lines; windows: $(grep -c 'created (macos' "$run/comp.log"), mapped: $(grep -c ' mapped ' "$run/comp.log")"
grep -E 'mapped|error|panic' "$run/comp.log" | head -10
