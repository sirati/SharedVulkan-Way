#!/usr/bin/env bash
#
# §111.10 on one machine, without root: the session service maps a boot
# record's identity to a Linux user (users.conf), starts the session through
# that user's agent, and the session's processes reach the BAR file and the
# doorbell hub only through the broker the service embeds. The server side is
# the Linux compositor with its record signed by a server-role broker
# (`wc-broker serve`), as the guest's privileged service will sign it.
#
# Everything runs as the invoking user, so "the mapped user" is this user too;
# the NixOS VM test (waycompose-users.nix in the host configuration) is
# where two Linux users and a system account are real. What this shows:
#
#   leg 1  an identity users.conf maps: located, the session's proxy has the
#          arena as `broker:ID` and the hub as `broker:wl-hub`, an application
#          runs, the proxy's fd is the BAR file the broker opened; the lease
#          ends the session, the agent says `ended`, the regions are unpublished
#   leg 2  an installation the identity may not start: refused, said in the page
#   leg 3  an identity no line maps: refused, said in the page
#   leg 4  a record with no identity (a key file, §107): refused; with
#          --accept-unidentified and a `-` line, it runs (the transitional rule)
#   leg 5  an agent socket that is not the mapped user's is refused (uid check),
#          shown by mapping to an account whose uid is not this one's
#
# Run from inside `nix develop` after
# `cargo build -p wc-compositor -p wc-proxy -p wc-arena-host -p sv-hostd -p wc-launch -p wc-broker`.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/${WC_BROKER_HOST_PROFILE:-debug}
SOCAT=$(command -v socat || ls /nix/store/*-socat-1.*/bin/socat 2>/dev/null | head -1)
CLIENT=$(command -v wayland-info || true)
D=$(mktemp -d /mnt/datasets/builds/wcrun/broker-host-XXXXXX)
FAIL=0
PIDS=()
ME=$(id -un)
MYUID=$(id -u)

say() { echo "[$(date +%H:%M:%S)] $*"; }
bad() { echo "FAIL: $*"; FAIL=1; }
ok() { echo "ok: $*"; }
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  for u in $(systemctl --user list-units --plain --no-legend 'wc-launch-*' 2>/dev/null | awk '{print $1}' | grep -E "wcb[a-z]*-$$-"); do
    systemctl --user stop "$u" 2>/dev/null
  done
  pkill -f "UNIX-LISTEN:$D/" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

[ -x "$SOCAT" ] || { echo "no socat"; exit 2; }
[ -n "$CLIENT" ] || { echo "no wayland-info"; exit 2; }
for b in wc-compositor wc-proxy wc-arena-host sv-hostd wc-launch wc-broker; do
  [ -x "$BIN/$b" ] || { echo "no $BIN/$b: build it first"; exit 2; }
done

# §111.10's layout: installation definitions in one system directory (here
# $D/installs), keys readable by the service only; the server's keys in the
# server broker's directory, as the guest keeps its copy.
mkdir -p "$D/installs" "$D/state" "$D/run" "$D/crt" "$D/hub" "$D/keys" "$D/agents/$ME"
chmod 700 "$D/crt" "$D/hub" "$D/keys"
installation() { # NAME
  local i="$D/installs/$1"
  mkdir -p "$i/bin"
  cp "$ROOT/scripts/wc-session" "$i/wc-session"
  for b in wc-proxy wc-arena-host wc-launch sv-hostd; do ln -s "$BIN/$b" "$i/bin/$b"; done
  # `~/` is the mapped user's home (§111.10); the probe needs none.
  printf '[probe]\nexec = %s\ntitlebar = none\ndesktop = probe\ngpu = off\n' "$CLIENT" >"$i/apps.ini"
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$i/session.key"
  chmod 600 "$i/session.key"
  cp "$i/session.key" "$D/keys/$1.key"
}
installation wcbh
installation wcbx

export WC_LAUNCH_RUN="$D/run" WC_LAUNCH_WCBELL="$D/host.sock"
export WC_LAUNCH_BELL_HUB="$D/hub" WC_LAUNCH_HUB_UNMANAGED=1 WC_LAUNCH_SESSION_SECS=120 WC_LAUNCH_AUDIO=off

start_port() {
  rm -f "$D/guest.sock" "$D/host.sock"
  printf '#!/usr/bin/env bash\nwhile true; do %q - UNIX-LISTEN:%q,unlink-early 2>/dev/null; done\n' "$SOCAT" "$D/host.sock" >"$D/qemu-host-end.sh"
  chmod +x "$D/qemu-host-end.sh"
  "$SOCAT" UNIX-LISTEN:"$D/guest.sock" EXEC:"$D/qemu-host-end.sh" &
  PORT=$!
  PIDS+=("$!")
  for _ in $(seq 1 50); do [ -S "$D/guest.sock" ] && break; sleep 0.1; done
}
start_hub() {
  "$BIN/sv-hostd" bell-hub --port "$D/host.sock=$D/hub/wl.sock" >>"$D/hub.log" 2>&1 &
  PIDS+=("$!")
  for _ in $(seq 1 50); do [ -S "$D/hub/wl.sock" ] && break; sleep 0.1; done
}
start_server_broker() {
  "$BIN/wc-broker" serve --socket "$D/server-broker.sock" --keys "$D/keys" >>"$D/server-broker.log" 2>&1 &
  PIDS+=("$!")
  for _ in $(seq 1 50); do [ -S "$D/server-broker.sock" ] && break; sleep 0.1; done
}
start_agent() {
  "$BIN/wc-arena-host" agent --listen "$D/agents/$ME/agent.sock" --installs "$D/installs" \
    --daemon-user "$ME" --broker "$D/broker.sock" >>"$D/agent.log" 2>&1 &
  AGENT=$!; PIDS+=("$AGENT")
  for _ in $(seq 1 50); do [ -S "$D/agents/$ME/agent.sock" ] && break; sleep 0.1; done
}
start_service() { # USERS-CONF [EXTRA...]
  local conf=$1; shift
  local before; before=$(grep -c "\[wc-broker\] run .*: serving" "$D/sessiond.log" 2>/dev/null)
  [ -n "${SVC:-}" ] && { kill "$SVC" 2>/dev/null; wait "$SVC" 2>/dev/null; }
  "$BIN/wc-arena-host" sessiond --ivshmem "$D/arena" --installs "$D/installs" --state "$D/state" --live "$D/live" \
    --bell "$D/hub/wl.sock" --poll-ms 60000 --users "$conf" --server testvm --agents "$D/agents" \
    --broker "$D/broker.sock" --gpu-region "$D/gpu" --hub-wl "$D/hub/wl.sock" "$@" >>"$D/sessiond.log" 2>&1 &
  SVC=$!; PIDS+=("$SVC")
  for _ in $(seq 1 100); do [ "$(grep -c "\[wc-broker\] run .*: serving" "$D/sessiond.log" 2>/dev/null)" -gt "${before:-0}" ] && break; sleep 0.1; done
  sleep 0.5
}
# compositor TAG INSTALL KEYSPEC
start_compositor() {
  local tag=$1 inst=$2 key=$3
  # The guest's port takes one holder, as virtio-serial does: a new one each time.
  [ -n "${PORT:-}" ] && { kill "$PORT" 2>/dev/null; wait "$PORT" 2>/dev/null; start_port; }
  rm -f "$D/arena" "$D/ctl-$tag.sock"
  truncate -s $((128 << 20)) "$D/arena"
  XDG_RUNTIME_DIR="$D/crt" WC_IVSHMEM_STANDIN="$D/arena" "$BIN/wc-compositor" \
    --arena-ivshmem 0 --arena-outbound 0:1048576 --wayland-rings "$D/guest.sock" \
    --socket "wcb$$-$tag" --in-band --timeout 120 \
    --boot-session "$inst-$$-$tag" --boot-install "$inst" --boot-key "$key" \
    --control-socket "$D/ctl-$tag.sock" --session-audio 0 >"$D/comp-$tag.log" 2>&1 &
  COMP=$!; PIDS+=("$COMP")
}
# A stopped compositor's waiting record stays in the file: take the file away
# too, or the next service serves the old record.
stop_compositor() { kill "$COMP" 2>/dev/null; wait "$COMP" 2>/dev/null; rm -f "$D/arena"; }
wait_line() { # FILE REGEX SECS
  local i; for i in $(seq 1 $(($3 * 10))); do grep -qE "$2" "$1" 2>/dev/null && return 0; sleep 0.1; done; return 1
}
request() { printf '%s\n' "$2" | timeout 30 "$SOCAT" -t 30 - UNIX-CONNECT:"$D/ctl-$1.sock" 2>&1; }
boot_result() { # TAG -> the compositor's boot line
  wait_line "$D/comp-$1.log" 'boot: (FAILED|located)' 30
  grep -m1 -oE 'boot: (FAILED|located).*' "$D/comp-$1.log" | cut -c1-200
}

truncate -s $((16 << 20)) "$D/gpu"
printf 'testvm %s %s installs=wcbh\n' "$ME" "$ME" >"$D/users-ok.conf"
printf 'testvm someone-else %s installs=wcbh,wcbx\n' "$ME" >"$D/users-unmapped.conf"
printf 'testvm %s %s installs=wcbh\ntestvm - %s installs=wcbh\n' "$ME" "$ME" "$ME" >"$D/users-transitional.conf"
start_port; start_hub; start_server_broker; start_agent

say "leg 1: a mapped identity: the session runs through the agent, its devices through the broker"
start_service "$D/users-ok.conf"
start_compositor a wcbh "broker:$D/server-broker.sock"
T0=$(date +%s%N)
R=$(boot_result a)
case "$R" in
  *located*) ok "located in $(( ($(date +%s%N) - T0) / 1000000 )) ms: ${R:0:120}" ;;
  *) bad "leg 1 boot: $R"; tail -n 5 "$D/sessiond.log" "$D/agent.log" ;;
esac
grep -q "the broker signed the record for os=linux user=$ME id=$MYUID" "$D/comp-a.log" \
  && ok "the server broker signed for os=linux user=<me> id=<my uid>" || bad "no signed identity in the compositor's log"
grep -q "asked by os=linux user=$ME id=$MYUID, runs as $ME (uid $MYUID)" "$D/sessiond.log" \
  && ok "service: the identity verified and mapped to <me>" || bad "service did not map: $(grep 'boot record' "$D/sessiond.log" | tail -1)"
wait_line "$D/comp-a.log" 'control socket' 20 || bad "no control socket"
R=$(request a "raw run probe 96 30")
case "$R" in ok-raw*'run accepted'*) ok "an application runs: $(echo "$R" | tail -1 | cut -c1-80)" ;; *) bad "run: $R" ;; esac
sleep 2
PLOG="$D/run/wcbh-$$-a/proxy.log"
PSH="$D/run/wcbh-$$-a/proxy.sh"
grep -qE -- '--arena-ivshmem broker:[0-9]+' "$PSH" && ok "the proxy maps the arena as $(grep -oE -- 'broker:[0-9]+' "$PSH" | head -1)" || bad "the proxy was not given a broker region: $(grep -o -- '--arena-ivshmem [^ ]*' "$PSH")"
grep -q -- '--wayland-bell broker:wl-hub' "$PSH" && ok "the proxy's doorbell is broker:wl-hub" || bad "the proxy's bell is not the broker's"
grep -q 'connected, upstream ring slot' "$PLOG" && ok "the application's connection is on a ring slot" || bad "no ring connection in $PLOG"
PPID_=$(systemctl --user show -p MainPID --value "wc-launch-wcbh-$$-a" 2>/dev/null)
if [ -n "$PPID_" ] && [ "$PPID_" != 0 ]; then
  P=$(pgrep -P "$PPID_" -x wc-proxy || echo "$PPID_")
  if ls -l /proc/"$P"/fd 2>/dev/null | grep -q " $D/arena\$"; then
    ok "proxy $P holds the BAR file through the fd the broker opened (Uid $(awk '/^Uid:/{print $2}' /proc/"$P"/status))"
  else
    bad "proxy $P has no fd of $D/arena"
  fi
fi
R=$(request a "raw status"); case "$R" in ok-raw*) ok "status round trip" ;; *) bad "status: $R" ;; esac
# The broker is set-up only: the control ring's round trip is wc-no-ssh.sh's.
cc -O2 -o "$D/probe-ctl-rtt" "$ROOT/scripts/probe-ctl-rtt.c" 2>/dev/null && {
  sleep 2
  R=$("$D/probe-ctl-rtt" "$D/ctl-a.sock" 10000 rtt 2>&1) && ok "control round trip (compare wc-no-ssh.sh): $R" || bad "rtt probe: $R"
}

say "leg 1b: the lease ends the session; the agent says ended; the regions go"
kill -9 "$COMP"; wait "$COMP" 2>/dev/null; rm -f "$D/arena"
wait_line "$D/sessiond.log" "session wcbh-$$-a ended" 25 && ok "service: $(grep -o "session wcbh-$$-a ended.*" "$D/sessiond.log" | cut -c1-100)" || bad "the service never heard the session end"
grep -q "region [0-9]*: the broker's handle is closed" "$D/sessiond.log" && ok "the session's regions were unpublished" || bad "no region was unpublished"

say "leg 2: an installation this identity may not start"
start_compositor b wcbx "broker:$D/server-broker.sock"
R=$(boot_result b)
case "$R" in *FAILED*'may not start installation wcbx'*) ok "refused: ${R:0:150}" ;; *) bad "leg 2: $R" ;; esac
stop_compositor

say "leg 3: an identity users.conf does not map"
start_service "$D/users-unmapped.conf"
start_compositor c wcbh "broker:$D/server-broker.sock"
R=$(boot_result c)
case "$R" in *FAILED*'is not mapped to any Linux user'*) ok "refused: ${R:0:150}" ;; *) bad "leg 3: $R" ;; esac
stop_compositor

say "leg 4: a record without identity: refused; accepted with --accept-unidentified and a '-' line"
start_service "$D/users-ok.conf"
start_compositor d wcbh "$D/keys/wcbh.key"
R=$(boot_result d)
case "$R" in *FAILED*'carries no identity, and this host requires one'*) ok "refused: ${R:0:130}" ;; *) bad "leg 4: $R" ;; esac
stop_compositor
start_service "$D/users-transitional.conf" --accept-unidentified
start_compositor e wcbh "$D/keys/wcbh.key"
R=$(boot_result e)
case "$R" in *located*) ok "transitional: located (no identity, the '-' line's user)" ;; *) bad "leg 4b: $R" ;; esac
grep -q "TRANSITIONAL" "$D/sessiond.log" && ok "the service says it runs the transitional rule"
R=$(request e "down" 2>/dev/null)
wait_line "$D/sessiond.log" "session wcbh-$$-e ended" 25 && ok "down: the agent ended it" || bad "leg 4b: no end"
stop_compositor

say "leg 5: an agent socket that is not the mapped user's"
# Map to an account that exists and is not this one (nobody), and put this
# user's agent socket where nobody's should be: the service must refuse it.
mkdir -p "$D/agents/nobody"; ln -sf "$D/agents/$ME/agent.sock" "$D/agents/nobody/agent.sock"
printf 'testvm %s nobody installs=wcbh\n' "$ME" >"$D/users-nobody.conf"
start_service "$D/users-nobody.conf"
start_compositor f wcbh "broker:$D/server-broker.sock"
R=$(boot_result f)
case "$R" in *FAILED*"socket, not uid"*) ok "refused: ${R:0:150}" ;; *) bad "leg 5: $R" ;; esac
stop_compositor

echo "logs: $D"
[ "$FAIL" = 0 ] && { echo "wc-broker-host: OK"; rm -rf "$D"; exit 0; }
echo "wc-broker-host: FAILED"; exit 1
