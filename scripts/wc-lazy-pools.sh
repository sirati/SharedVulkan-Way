#!/usr/bin/env bash
#
# §109 on one machine: real clients whose wl_shm pools are larger than the
# arena, through the whole no-ssh path (scripts/wc-no-ssh.sh's setup: a file for
# the ivshmem device, socat for QEMU's port, the real hub, service and proxy,
# the Linux compositor).
#
#   foot             a 512 MiB pool (its default), in a 64 MiB BAR
#   weston-terminal  its pool doublings
#
# Each must map a window, stay alive, and leave the proxy's log with no
# `Invalid new_id`, no protocol error and no "held back". The proxy's stats
# line says how much of the arena the buffers held.
#
# Run from inside `nix develop` after
# `cargo build -p wc-compositor -p wc-proxy -p wc-arena-host -p sv-hostd -p wc-launch`.
# WC_FOOT may name foot's binary (default: `nix build nixpkgs#foot`).
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/debug
SOCAT=$(command -v socat || ls /nix/store/*-socat-1.*/bin/socat 2>/dev/null | head -1)
FOOT=${WC_FOOT:-$(nix build nixpkgs#foot --no-link --print-out-paths 2>/dev/null)/bin/foot}
WTERM=$(command -v weston-terminal || ls /nix/store/*-weston-1*/bin/weston-terminal 2>/dev/null | head -1)
D=$(mktemp -d /mnt/datasets/builds/wcrun/wc-lazy-pools-XXXXXX)
FAIL=0
PIDS=()
BAR_MIB=${WC_BAR_MIB:-64}
APPS=()

say() { echo "[$(date +%H:%M:%S)] $*"; }
bad() { echo "FAIL: $*"; FAIL=1; }
ok() { echo "ok: $*"; }
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  for u in $(systemctl --user list-units --plain --no-legend 'wc-launch-*' 2>/dev/null | awk '{print $1}' | grep -E "wclp-$$-"); do
    systemctl --user stop "$u" 2>/dev/null
  done
  pkill -f "UNIX-LISTEN:$D/host.sock" 2>/dev/null
  pkill -f "UNIX-LISTEN:$D/guest.sock" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

[ -x "$SOCAT" ] || { echo "no socat"; exit 2; }
[ -x "$FOOT" ] || { echo "no foot ($FOOT)"; exit 2; }
[ -x "$WTERM" ] || { echo "no weston-terminal"; exit 2; }
for b in wc-compositor wc-proxy wc-arena-host sv-hostd wc-launch; do
  [ -x "$BIN/$b" ] || { echo "no $BIN/$b: build it first"; exit 2; }
done

INST=$D/inst
mkdir -p "$INST/bin" "$D/installs" "$D/state" "$D/run" "$D/crt" "$D/hub"
chmod 700 "$D/crt" "$D/hub"
cp "$ROOT/scripts/wc-session" "$INST/wc-session"
for b in wc-proxy wc-arena-host wc-launch sv-hostd; do ln -s "$BIN/$b" "$INST/bin/$b"; done
cat >"$INST/apps.ini" <<EOF
[foot]
exec = $FOOT
titlebar = none
desktop = foot
gpu = off

[wterm]
exec = $WTERM
titlebar = none
desktop = wterm
gpu = off
EOF
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$INST/session.key"
chmod 600 "$INST/session.key"
cp "$INST/session.key" "$D/guest.key"
ln -s "$INST" "$D/installs/wclp"

export WC_LAUNCH_RUN="$D/run" WC_LAUNCH_ARENA="$D/arena" WC_LAUNCH_WCBELL="$D/host.sock"
export WC_LAUNCH_BELL_HUB="$D/hub" WC_LAUNCH_HUB_UNMANAGED=1 WC_LAUNCH_SESSION_SECS=300 WC_LAUNCH_AUDIO=off

rm -f "$D/guest.sock" "$D/host.sock"
printf '#!/usr/bin/env bash\nwhile true; do %q - UNIX-LISTEN:%q,unlink-early 2>/dev/null; done\n' "$SOCAT" "$D/host.sock" >"$D/qemu-host-end.sh"
chmod +x "$D/qemu-host-end.sh"
"$SOCAT" UNIX-LISTEN:"$D/guest.sock" EXEC:"$D/qemu-host-end.sh" & PIDS+=($!)
for _ in $(seq 1 50); do [ -S "$D/guest.sock" ] && break; sleep 0.1; done
"$BIN/sv-hostd" bell-hub --port "$D/host.sock=$D/hub/wl.sock" >>"$D/hub.log" 2>&1 & PIDS+=($!)
for _ in $(seq 1 50); do [ -S "$D/hub/wl.sock" ] && break; sleep 0.1; done
"$BIN/wc-arena-host" sessiond --ivshmem "$D/arena" --installs "$D/installs" --state "$D/state" \
  --bell "$D/hub/wl.sock" --poll-ms 60000 >>"$D/sessiond.log" 2>&1 & PIDS+=($!)

truncate -s $((BAR_MIB << 20)) "$D/arena"
XDG_RUNTIME_DIR="$D/crt" WC_IVSHMEM_STANDIN="$D/arena" "$BIN/wc-compositor" \
  --arena-ivshmem 0 --arena-outbound 0:1048576 --wayland-rings "$D/guest.sock" \
  --socket "wclp$$" --in-band --timeout 300 \
  --boot-session "wclp-$$-a" --boot-install wclp --boot-key "$D/guest.key" \
  --control-socket "$D/ctl.sock" >"$D/comp.log" 2>&1 & PIDS+=($!)

wait_line() { local i; for i in $(seq 1 $(($3 * 10))); do grep -qE "$2" "$1" 2>/dev/null && return 0; sleep 0.1; done; return 1; }
request() { printf '%s\n' "$1" | timeout 30 "$SOCAT" -t 30 - UNIX-CONNECT:"$D/ctl.sock" 2>&1; }

wait_line "$D/comp.log" 'boot: located' 30 || { bad "no boot: $(grep boot: "$D/comp.log" | tail -2)"; }
wait_line "$D/comp.log" 'control socket' 20 || bad "no control socket"
PLOG="$D/run/wclp-$$-a/proxy.log"

for app in foot wterm; do
  say "$app, in a ${BAR_MIB} MiB BAR"
  before=$(grep -c ' mapped ' "$D/comp.log")
  R=$(request "raw run $app 96 30")
  case "$R" in ok-raw*'run accepted'*) ;; *) bad "run $app: $R" ;; esac
  i=0; while [ $i -lt 300 ] && [ "$(grep -c ' mapped ' "$D/comp.log")" -le "$before" ]; do sleep 0.1; i=$((i+1)); done
  [ "$(grep -c ' mapped ' "$D/comp.log")" -gt "$before" ] && ok "$app mapped a window in $((i / 10)).$((i % 10)) s" || bad "$app mapped no window"
  sleep 4
  pid=$(pgrep -f "$( [ $app = foot ] && echo "$FOOT" || echo "$WTERM")" | head -1)
  APPS+=("$pid")
  [ -n "$pid" ] && ok "$app is still running (pid $pid)" || bad "$app died: $(tail -3 "$D/run/wclp-$$-a/app.log" 2>/dev/null)"
  grep -m1 -o "pool [0-9]*: [0-9]* bytes from the client[^(]*" "$PLOG" | sed "s/^/  /"
done
grep -E 'Invalid new_id|protocol error|held back|is absorbed' "$PLOG" "$D/comp.log" && bad "a desync or a held-back pool"
grep -q 'Invalid new_id' "$D/run/wclp-$$-a/app.log" 2>/dev/null && bad "a client saw Invalid new_id"
POOLS=$(grep -o 'pool [0-9]*: [0-9]* bytes from the client' "$PLOG" | awk '{s+=$3; if ($3>m) m=$3} END {printf "%d pools, %.0f MiB asked, largest %.0f MiB", NR, s/1048576, m/1048576}')
ok "pools: $POOLS"
# The clients first, so each connection prints its stats line.
kill "${APPS[@]}" 2>/dev/null
sleep 3
cp "$PLOG" "$D/proxy.log" 2>/dev/null
grep -o 'client [0-9]* [0-9]* commits.*' "$D/proxy.log" | grep -v ' 0 commits' | sed -E 's/^(client [0-9]*) ([0-9]* commits).* ([0-9]* pools \([^)]*\)).*(buffers: [^;]*).*/  \1: \2; \3; \4/'
request "down" >/dev/null

echo "logs: $D"
[ "$FAIL" = 0 ] && { echo "wc-lazy-pools: OK"; rm -rf "$D"; exit 0; }
echo "wc-lazy-pools: FAILED"; exit 1
