#!/usr/bin/env bash
#
# §105 on one machine: a session with no ssh anywhere, end to end. A file
# stands where the ivshmem device would be, a socat pair where QEMU's
# virtio-serial port would be, and everything else is the product's own code:
#
#   the doorbell hub   sv-hostd bell-hub, holding the "QEMU" end
#   the service        wc-arena-host sessiond --ivshmem, as the user unit runs it
#   the installation   a copy of scripts/wc-session with an apps.ini and a key
#   the guest          the Linux build of the compositor, with --boot-* and a
#                      control socket, as wc-launch.exe starts it
#
#   scripts/wc-no-ssh.sh     legs: boot and requests; the sequence number and
#                            replay; a wrong key; the hub restarted under a
#                            session (the rescan); the lease when the
#                            compositor dies; and no ring port at all (polled)
#
# Run from inside `nix develop` after
# `cargo build -p wc-compositor -p wc-proxy -p wc-arena-host -p sv-hostd -p wc-launch`.
# It starts transient user units (wc-launch-wcns*-…), as wc-session does on the
# VM host, and stops them again. What it cannot show is the hypervisor: a
# virtio-serial byte and a BAR are the guest runs' claim.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/debug
SOCAT=$(command -v socat || ls /nix/store/*-socat-1.*/bin/socat 2>/dev/null | head -1)
CLIENT=$(command -v wayland-info || true)
D=$(mktemp -d /mnt/datasets/builds/wcrun/wc-no-ssh-XXXXXX)
FAIL=0
PIDS=()

say() { echo "[$(date +%H:%M:%S)] $*"; }
bad() { echo "FAIL: $*"; FAIL=1; }
ok() { echo "ok: $*"; }
cleanup() {
  # By pid and by this run's unit names only: other agents run processes here.
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  for u in $(systemctl --user list-units --plain --no-legend 'wc-launch-*' 2>/dev/null | awk '{print $1}' | grep -E "wcns[0-9]*-$$-"); do
    systemctl --user stop "$u" 2>/dev/null
  done
  pkill -f "UNIX-LISTEN:$D/host.sock" 2>/dev/null
  pkill -f "tail -F $D/run" 2>/dev/null
  pkill -f "UNIX-LISTEN:$D/guest.sock" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

[ -x "$SOCAT" ] || { echo "no socat"; exit 2; }
[ -n "$CLIENT" ] || { echo "no wayland-info"; exit 2; }
for b in wc-compositor wc-proxy wc-arena-host sv-hostd wc-launch; do
  [ -x "$BIN/$b" ] || { echo "no $BIN/$b: build it first"; exit 2; }
done

# The installation, as deploy makes it.
INST=$D/inst
mkdir -p "$INST/bin" "$D/installs" "$D/state" "$D/run" "$D/crt" "$D/hub"
chmod 700 "$D/crt" "$D/hub"
cp "$ROOT/scripts/wc-session" "$INST/wc-session"
for b in wc-proxy wc-arena-host wc-launch sv-hostd; do ln -s "$BIN/$b" "$INST/bin/$b"; done
cat >"$INST/apps.ini" <<EOF
[probe]
exec = $CLIENT
titlebar = none
desktop = probe
gpu = off
EOF
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$INST/session.key"
chmod 600 "$INST/session.key"
cp "$INST/session.key" "$D/guest.key"
ln -s "$INST" "$D/installs/wcns"

export WC_LAUNCH_RUN="$D/run" WC_LAUNCH_ARENA="$D/arena" WC_LAUNCH_WCBELL="$D/host.sock"
export WC_LAUNCH_BELL_HUB="$D/hub" WC_LAUNCH_HUB_UNMANAGED=1 WC_LAUNCH_AUDIO=off WC_LAUNCH_SESSION_SECS=120

start_port() {
  # QEMU's chardev and the guest's port. The guest's end is held for as long
  # as the guest holds it; the host's end takes one connection at a time and a
  # new one after it closes, as QEMU's chardev server does. Bytes from the
  # guest while no host end is connected are lost, as QEMU drops them.
  rm -f "$D/guest.sock" "$D/host.sock"
  printf '#!/usr/bin/env bash\nwhile true; do %q - UNIX-LISTEN:%q,unlink-early 2>/dev/null; done\n' "$SOCAT" "$D/host.sock" >"$D/qemu-host-end.sh"
  chmod +x "$D/qemu-host-end.sh"
  "$SOCAT" UNIX-LISTEN:"$D/guest.sock" EXEC:"$D/qemu-host-end.sh" &
  PORT=$!; PIDS+=("$PORT")
  for _ in $(seq 1 50); do [ -S "$D/guest.sock" ] && break; sleep 0.1; done
}
start_hub() {
  "$BIN/sv-hostd" bell-hub --port "$D/host.sock=$D/hub/wl.sock" >>"$D/hub.log" 2>&1 &
  HUB=$!; PIDS+=("$HUB")
  for _ in $(seq 1 50); do [ -S "$D/hub/wl.sock" ] && break; sleep 0.1; done
}
start_service() {
  "$BIN/wc-arena-host" sessiond --ivshmem "$D/arena" --installs "$D/installs" --state "$D/state" \
    ${1:+--bell "$1"} --poll-ms "${2:-1000}" >>"$D/sessiond.log" 2>&1 &
  SVC=$!; PIDS+=("$SVC")
}
# compositor TAG KEY [PORT]
start_compositor() {
  local tag=$1 key=$2 port=${3:-$D/guest.sock}
  rm -f "$D/arena" "$D/ctl-$tag.sock"
  truncate -s $((128 << 20)) "$D/arena"
  XDG_RUNTIME_DIR="$D/crt" WC_IVSHMEM_STANDIN="$D/arena" "$BIN/wc-compositor" \
    --arena-ivshmem 0 --arena-outbound 0:1048576 --wayland-rings "$port" \
    --socket "wcns$$-$tag" --in-band --timeout 120 \
    --boot-session "wcns-$$-$tag" --boot-install wcns --boot-key "$key" \
    --control-socket "$D/ctl-$tag.sock" >"$D/comp-$tag.log" 2>&1 &
  COMP=$!; PIDS+=("$COMP")
}
wait_line() { # FILE REGEX SECS
  local i; for i in $(seq 1 $(($3 * 10))); do grep -qE "$2" "$1" 2>/dev/null && return 0; sleep 0.1; done; return 1
}
request() { # TAG LINE
  printf '%s\n' "$2" | timeout 30 "$SOCAT" -t 30 - UNIX-CONNECT:"$D/ctl-$1.sock" 2>&1
}

say "leg 1: boot, a request, the jump-list verb, the lease"
# The run directory goes with `down`; its proxy log is kept here.
keep_log() { (tail -F "$1" 2>/dev/null >"$2" &) ; }
start_port; start_hub; start_service "$D/hub/wl.sock" 60000
start_compositor a "$D/guest.key"
T0=$(date +%s%N)
if wait_line "$D/comp-a.log" 'boot: located' 30; then
  T1=$(date +%s%N)
  ok "located by hello in $(( (T1 - T0) / 1000000 )) ms after the compositor started (service timer 60 s, so the hello did it)"
else
  bad "no boot: $(grep -E 'boot:' "$D/comp-a.log" | tail -2)"; tail -5 "$D/sessiond.log"
fi
wait_line "$D/comp-a.log" 'control socket' 20 || bad "no control socket"
R=$(request a "raw run probe 96 30")
case "$R" in ok-raw*'run ok'*) ok "run over the control ring: $(echo "$R" | tail -1)" ;; *) bad "run: $R" ;; esac
sleep 2
PLOG="$D/run/wcns-$$-a/proxy.log"
keep_log "$PLOG" "$D/proxy-a.log"
grep -q 'connected, upstream ring slot' "$PLOG" && ok "the application's connection is on a ring slot" || bad "no ring connection in $PLOG"
grep -q 'no socket, no TCP, no ssh' "$PLOG" && ok "proxy: $(grep -o 'wayland rings: .*' "$PLOG" | cut -c1-110)" || bad "proxy did not attach no-ssh rings"
grep -q 'SSH FALLBACK' "$PLOG" "$D/comp-a.log" && bad "an SSH FALLBACK line appeared"
grep -q 'session control on ring slot' "$PLOG" && ok "control on its own ring slot" || bad "no control ring"
R=$(request a "raw status")
case "$R" in ok-raw*) ok "status round trip" ;; *) bad "status: $R" ;; esac
R=$(request a "run probe; rm -rf ~ 96 30")
case "$R" in err*refused*) ok "a request with shell words is refused: ${R:0:60}" ;; *) bad "not refused: $R" ;; esac
# Timing: 20 status round trips.
T0=$(date +%s%N); for _ in $(seq 1 20); do request a "raw status" >/dev/null; done; T1=$(date +%s%N)
ok "control round trip (status verb included) $(( (T1 - T0) / 20000000 )) ms mean"
ps -o pid=,comm= --ppid "$SVC" | grep -qw ssh && bad "the service has an ssh child"
pgrep -u "$(id -u)" -f "ssh .*wc-session" >/dev/null && bad "an ssh wc-session process exists"

say "leg 2: the hub restarted under the session; requests still answered (the rescan)"
kill "$HUB"; wait "$HUB" 2>/dev/null
sleep 0.5; start_hub
sleep 1.5
T0=$(date +%s%N); R=$(request a "raw status"); T1=$(date +%s%N)
case "$R" in ok-raw*) ok "after a hub restart: answered in $(( (T1 - T0) / 1000000 )) ms" ;; *) bad "after the hub restart: $R" ;; esac
grep -c 'reconnected' "$PLOG" >/dev/null && ok "proxy: $(grep -m1 -o '\[doorbell\] reconnected.*' "$PLOG")"

say "leg 3: the lease: the compositor dies, the proxy ends within seconds, the service takes the session down"
kill -9 "$COMP"; wait "$COMP" 2>/dev/null
T0=$(date +%s)
wait_line "$D/proxy-a.log" 'LEASE LOST' 15 && ok "proxy: LEASE LOST after $(( $(date +%s) - T0 )) s" || bad "the proxy outlived its lease"
wait_line "$D/sessiond.log" "session wcns-$$-a ended" 15 && ok "service: $(grep -o "session wcns-$$-a ended.*" "$D/sessiond.log" | cut -c1-90)" || bad "the service never took the session down"

say "leg 4: a record replayed, and one signed with the wrong key"
kill "$PORT" 2>/dev/null; start_port
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$D/wrong.key"
start_compositor b "$D/wrong.key"
wait_line "$D/comp-b.log" 'boot: (FAILED|located)' 30
grep -q 'boot: FAILED: REFUSED' "$D/comp-b.log" && ok "wrong key: $(grep -o 'REFUSED.*' "$D/comp-b.log" | cut -c1-100)" || bad "a wrong key was not refused: $(grep boot: "$D/comp-b.log")"
kill "$COMP" 2>/dev/null; wait "$COMP" 2>/dev/null
kill "$PORT" 2>/dev/null; start_port
SEQ=$(cat "$D/state/wcns.seq"); echo 1 >"$D/state/wcns.seq.tmp"; mv "$D/state/wcns.seq.tmp" "$D/state/wcns.seq"
[ "$SEQ" -gt 1000000000000000 ] && ok "the accepted sequence number was stored ($SEQ)" || bad "sequence number $SEQ"
echo "$((SEQ + 10000000000))" >"$D/state/wcns.seq"
start_compositor c "$D/guest.key"
wait_line "$D/comp-c.log" 'boot: (FAILED|located)' 30
grep -q 'replayed' "$D/comp-c.log" && ok "an old sequence number is refused as a replay" || bad "replay not refused: $(grep boot: "$D/comp-c.log")"
kill "$COMP" 2>/dev/null; wait "$COMP" 2>/dev/null
echo "$SEQ" >"$D/state/wcns.seq"

say "leg 5: no ring port at all: polled, said loudly, still no ssh"
kill "$PORT" 2>/dev/null
# A VM without the port has no chardev for a hub to hold either.
kill "$HUB" 2>/dev/null; wait "$HUB" 2>/dev/null; rm -f "$D/hub/"*.sock "$D/host.sock"
kill "$SVC" 2>/dev/null; wait "$SVC" 2>/dev/null
start_service "" 200
start_compositor d "$D/guest.key" "$D/no-such-port.sock"
if wait_line "$D/comp-d.log" 'boot: located' 30; then
  grep -q 'NO DOORBELL' "$D/comp-d.log" && ok "compositor: $(grep -m1 -o 'NO DOORBELL.*' "$D/comp-d.log" | cut -c1-80)"
  wait_line "$D/comp-d.log" 'control socket' 20
  R=$(request d "raw run probe 96 30")
  case "$R" in ok-raw*'run ok'*) ok "run on polled rings" ;; *) bad "run polled: $R" ;; esac
  grep -q 'NO DOORBELL' "$D/run/wcns-$$-d/proxy.log" && ok "proxy: polled, said so" || bad "the proxy did not say it polls"
else
  bad "polled boot: $(grep boot: "$D/comp-d.log")"
fi
R=$(request d "down")
case "$R" in ok*) ok "down over the control ring" ;; *) bad "down: $R" ;; esac
wait_line "$D/sessiond.log" "session wcns-$$-d ended" 15 && ok "the service took the session down" || bad "no down"

echo "logs: $D"
[ "$FAIL" = 0 ] && { echo "wc-no-ssh: OK"; rm -rf "$D"; exit 0; }
echo "wc-no-ssh: FAILED"; exit 1
