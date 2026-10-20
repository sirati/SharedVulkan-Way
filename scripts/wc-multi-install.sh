#!/usr/bin/env bash
#
# §107 on one machine: THREE installations' sessions at once against ONE
# compositor, the way the Windows guest will run them. As scripts/wc-no-ssh.sh
# does, a file stands where the ivshmem device would be and a socat pair where
# QEMU's virtio-serial port would be; everything else is the product's code:
#
#   the doorbell hub   sv-hostd bell-hub
#   the service        wc-arena-host sessiond --ivshmem, one for every installation
#   the installations  wcma, wcmb, wcmc (and wcmd for the wrong key): a copy of
#                      scripts/wc-session each, with its own apps.ini and key
#   the guest          ONE Linux compositor with --user-control, booting each
#                      session on a `boot` line, as every wc-launch.exe will
#
#   legs: three boots at once; a client in each, with the arena's regions per
#   session; the nested-compositor globals for one session only; a wrong key refused while the others run; one session's proxy
#   killed, the others unaffected, its regions reused by the next boot; the
#   compositor killed and started again; `down` for all and the compositor's
#   own end; a BAR too small for a third session's region, refused cleanly; one pool over a
#   second region (§110); host integration on beside the arena (§106); one
#   installation closed and another opened while the compositor keeps running,
#   on the slots the first gave back (§107.11).
#
# WC_MULTI_ONLY_REOPEN=1 runs leg 10 alone.
#
# Run from inside `nix develop` after
# `cargo build -p wc-compositor -p wc-proxy -p wc-arena-host -p sv-hostd -p wc-launch`.
# Evidence (logs) are left in $WC_MULTI_EVIDENCE if set.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/debug
SOCAT=$(command -v socat || ls /nix/store/*-socat-1.*/bin/socat 2>/dev/null | head -1)
SHM=$(command -v weston-presentation-shm || ls /nix/store/*-weston-1*/bin/weston-presentation-shm 2>/dev/null | head -1)
INFO=$(command -v wayland-info || true)
D=$(mktemp -d /mnt/datasets/builds/wcrun/wc-multi-XXXXXX)
FAIL=0
PIDS=()
LINGER=4

say() { echo "[$(date +%H:%M:%S.%3N)] $*"; }
bad() { echo "FAIL: $*"; FAIL=1; }
ok() { echo "ok: $*"; }
ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  for u in $(systemctl --user list-units --plain --no-legend 'wc-launch-*' 2>/dev/null | awk '{print $1}' | grep -E "wcm[a-d]-$$-"); do
    systemctl --user stop "$u" 2>/dev/null
  done
  pkill -f "UNIX-LISTEN:$D/host.sock" 2>/dev/null
  pkill -f "tail -n [+]1 -F $D/run" 2>/dev/null
  pkill -f "UNIX-LISTEN:$D/guest.sock" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

[ -x "$SOCAT" ] || { echo "no socat"; exit 2; }
[ -x "$SHM" ] || { echo "no weston-presentation-shm"; exit 2; }
# Built here, a no-op when they are current: a binary older than the sources
# runs yesterday's flags against today's harness and fails like the code's
# fault (a compositor that refused an argument, then a leg with no pid).
if command -v cargo >/dev/null; then
  (cd "$ROOT" && cargo build -q $(for b in wc-compositor wc-proxy wc-arena-host sv-hostd; do printf -- '--bin %s ' "$b"; done)) || { echo "cargo build failed"; exit 2; }
fi
for b in wc-compositor wc-proxy wc-arena-host sv-hostd; do
  [ -x "$BIN/$b" ] || { echo "no $BIN/$b: build it first"; exit 2; }
done

mkdir -p "$D/installs" "$D/state" "$D/run" "$D/crt" "$D/hub"
chmod 700 "$D/crt" "$D/hub"
# installation NAME: the host's directory, as deploy makes it, and the guest's key.
installation() {
  local n=$1 I=$D/inst-$1
  mkdir -p "$I/bin"
  cp "$ROOT/scripts/wc-session" "$I/wc-session"
  printf '%s\n' "$D/test-home" >"$I/test-home"
  for b in wc-proxy wc-arena-host wc-launch sv-hostd; do ln -sf "$BIN/$b" "$I/bin/$b"; done
  cat >"$I/apps.ini" <<EOF
[shm]
exec = $SHM
titlebar = none
desktop = shm
gpu = off

[info]
exec = ${INFO:-$SHM}
titlebar = none
desktop = info
gpu = off
EOF
  head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$I/session.key"
  chmod 600 "$I/session.key"
  cp "$I/session.key" "$D/guest-$n.key"
  ln -sfn "$I" "$D/installs/$n"
}
for n in wcma wcmb wcmc wcmd; do installation "$n"; done
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$D/wrong.key"

export WC_LAUNCH_TEST_HOME="$D/test-home"
export WC_LAUNCH_RUN="$D/run" WC_LAUNCH_ARENA="$D/arena" WC_LAUNCH_WCBELL="$D/host.sock"
export WC_LAUNCH_BELL_HUB="$D/hub" WC_LAUNCH_HUB_UNMANAGED=1 WC_LAUNCH_SESSION_SECS=600
# §113: the compositor stands for the VM whose process tells a stalled guest
# from a dead one; start_compositor writes its pid.
export WC_LAUNCH_VM_PIDFILE="$D/vm.pid"
# §110: the file leg 7 makes as the GPU device; wc-session passes it to the
# proxy as region 1 once it exists.
export WC_LAUNCH_GPU_REGION="$D/gpu"
PACAT=$(ls /nix/store/*-pulseaudio-1*/bin/pacat 2>/dev/null | head -1)
[ -n "$PACAT" ] || export WC_LAUNCH_AUDIO=off

start_port() {
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
  "$BIN/wc-arena-host" sessiond --ivshmem "$D/arena" --installs "$D/installs" --state "$D/state" --live "$D/live" \
    --bell "$D/hub/wl.sock" --poll-ms 60000 >>"$D/sessiond.log" 2>&1 &
  SVC=$!; PIDS+=("$SVC")
}
# compositor TAG [MIB]: the user's one compositor, with no session of its own.
start_compositor() {
  local tag=$1 mib=${2:-128} region=${3:-}
  rm -f "$D/arena" "$D/user.ctl"
  truncate -s $((mib << 20)) "$D/arena"
  XDG_RUNTIME_DIR="$D/crt" WC_IVSHMEM_STANDIN="$D/arena" "$BIN/wc-compositor" \
    --arena-ivshmem 0 --arena-outbound 0:1048576 --wayland-rings "$D/guest.sock" \
    --socket "wcm$$-$tag" --in-band --timeout 600 --exit-when-empty $LINGER \
    --user-control "$D/user.ctl" --singleton "wc-multi-$$" ${region:+--pool-region-file "$region"} \
    ${COMP_EXTRA:-} >"$D/comp-$tag.log" 2>&1 &
  COMP=$!; PIDS+=("$COMP")
  echo "$COMP" >"$D/vm.pid"
  wait_line "$D/comp-$tag.log" 'user control socket' 20 || bad "compositor $tag: no user control socket"
}
wait_line() { # FILE REGEX SECS
  local i; for i in $(seq 1 $(($3 * 20))); do grep -qE "$2" "$1" 2>/dev/null && return 0; sleep 0.05; done; return 1
}
user() { printf '%s\n' "$1" | timeout 100 "$SOCAT" -t 100 - UNIX-CONNECT:"$D/user.ctl" 2>&1; }
# boot TAG INSTALL KEY: as wc-launch.exe asks, one line on the user's socket.
boot() {
  local tag=$1 inst=$2 key=$3 extra=${4:-}
  user "$(printf 'boot\tsession=%s\tinstall=%s\tkey=%s\tcontrol=%s\taudio=0%s' "$inst-$$-$tag" "$inst" "$key" "$D/ctl-$tag.sock" "$extra")"
}
request() { printf '%s\n' "$2" | timeout 30 "$SOCAT" -t 30 - UNIX-CONNECT:"$D/ctl-$1.sock" 2>&1; }
keep_log() { (tail -n +1 -F "$1" 2>/dev/null >"$2" &) ; }
proxy_pid() { systemctl --user show -p MainPID --value "wc-launch-$1" 2>/dev/null; }
sessions() { user sessions; }
held_by() { # COMPLOG INDEX: MiB granted to session INDEX, as the compositor logged it
  grep -o "arena: session $2 granted region [^:]*: [0-9]* MiB at [0-9]* MiB" "$1" | awk '{s+=$7} END {print s+0}'
}

if [ -n "${WC_MULTI_ONLY_ONE_POOL:-}${WC_MULTI_ONLY_REOPEN:-}" ]; then start_port; start_hub; start_service; COMP=; fi

if [ -z "${WC_MULTI_ONLY_ONE_POOL:-}${WC_MULTI_ONLY_REOPEN:-}" ]; then
say "leg 1: one compositor, three installations booted at once"
start_port; start_hub; start_service
start_compositor 1
T0=$(date +%s%N)
for t in a b c; do
  ( R=$(boot "$t" "wcm$t" "$D/guest-wcm$t.key" "$([ $t = c ] && printf '\tnested=1')"); echo "$R" >"$D/boot-$t.reply"; echo "$(ms "$T0")" >"$D/boot-$t.ms" ) &
done
wait $(jobs -p | grep -v -e "$PORT" -e "$HUB" -e "$SVC" -e "$COMP") 2>/dev/null
for t in a b c; do
  R=$(cat "$D/boot-$t.reply" 2>/dev/null)
  case "$R" in ok\ located*) ok "wcm$t booted in $(cat "$D/boot-$t.ms") ms: ${R:0:60}" ;; *) bad "wcm$t: $R" ;; esac
done
IDX_A=$(awk '{print $3}' "$D/boot-a.reply"); IDX_B=$(awk '{print $3}' "$D/boot-b.reply"); IDX_C=$(awk '{print $3}' "$D/boot-c.reply")
[ "$IDX_A" != "$IDX_B" ] && [ "$IDX_B" != "$IDX_C" ] && [ "$IDX_A" != "$IDX_C" ] && ok "three session indices: $IDX_A $IDX_B $IDX_C" || bad "indices $IDX_A $IDX_B $IDX_C"
grep -c 'boot record for session' "$D/sessiond.log" | grep -qx 3 && ok "the service took three records from one header page, one after the other" || bad "service: $(grep -c 'boot record' "$D/sessiond.log") records"
for t in a b c; do wait_line "$D/comp-1.log" "control socket $D/ctl-$t.sock" 20 || bad "no control socket for $t"; done

say "leg 1b: a client in each session; the arena's regions per session"
for t in a b c; do
  R=$(request "$t" "raw run shm 96 30")
  case "$R" in ok-raw*'run accepted'*) ;; *) bad "run in $t: $R" ;; esac
done
for t in a b c; do
  eval "I=\$IDX_$(echo $t | tr a-c A-C)"
  wait_line "$D/comp-1.log" "arena: session $I granted region" 30 && ok "session $I (wcm$t): $(grep -m1 -o "arena: session $I granted region.*" "$D/comp-1.log" | cut -c1-110)" || bad "session $I got no region"
  wait_line "$D/comp-1.log" "accepted on ring slot [0-9]* of session $I" 10 || bad "no client of session $I"
  keep_log "$D/run/wcm$t-$$-$t/proxy.log" "$D/proxy-$t.log"
done
sleep 3
grep -q 'REFUSED' "$D/comp-1.log" && bad "a refusal: $(grep -m1 REFUSED "$D/comp-1.log")"
S=$(sessions); ok "sessions: ${S#ok }"
for t in a b c; do
  grep -q "session [0-9]* of a shared compositor" "$D/proxy-$t.log" && ok "proxy $t: $(grep -m1 -o 'session [0-9]* of a shared compositor.*' "$D/proxy-$t.log" | cut -c1-100)" || bad "proxy $t did not run as a shared compositor's session"
done
for t in a b c; do R=$(request "$t" "raw status"); case "$R" in ok-raw*) ;; *) bad "status $t: $R" ;; esac; done
ok "every session answers its own control ring"

say "leg 1d: isolated control requests: 20 ms and 1 s apart, the host's proxy parked between them, mean under 1 ms"
# A request after a pause finds the proxy's control thread parked on its lane
# with a 500 ms slice, so only the doorbell (compositor -> port -> hub -> the
# proxy's doorman -> the lane's futex) answers it in time. Back to back (the
# probe's default) the proxy is still spinning and the doorbell is never used.
cc -O2 -o "$D/probe-ctl-rtt" "$ROOT/scripts/probe-ctl-rtt.c" 2>/dev/null || bad "cannot build the rtt probe"
for spec in "a 20 50" "b 20 50" "c 1000 6"; do
  set -- $spec
  R=$(GAP_MS=$2 "$D/probe-ctl-rtt" "$D/ctl-$1.sock" "$3" rtt 2>&1)
  M=$(echo "$R" | sed -n 's/.*mean \([0-9.]*\) us.*/\1/p')
  if [ -n "$M" ] && awk -v m="$M" 'BEGIN { exit !(m < 1000) }'; then ok "wcm$1, $2 ms apart: $R"; else bad "wcm$1, $2 ms apart: ${R:-no answer} (mean must be < 1000 us)"; fi
done

if [ -n "$INFO" ]; then
  say "leg 1c: the nested-compositor globals only for the session that asked (wcmc booted with nested=1)"
  for t in a c; do request "$t" "raw run info 96 30" >/dev/null; done
  for t in a c; do wait_line "$D/run/wcm$t-$$-$t/app.log" 'wl_seat|wl_compositor' 15 || bad "no wayland-info output in $t"; done
  sleep 1
  grep -q wp_viewporter "$D/run/wcmc-$$-c/app.log" && ok "wcmc's client sees wp_viewporter" || bad "wcmc's client has no wp_viewporter"
  grep -q wp_viewporter "$D/run/wcma-$$-a/app.log" && bad "wcma's client sees wp_viewporter" || ok "wcma's client does not ($(grep -c 'interface:' "$D/run/wcma-$$-a/app.log") globals against $(grep -c 'interface:' "$D/run/wcmc-$$-c/app.log"))"
fi

say "leg 2: a wrong key for a fourth installation is refused while the three run"
R=$(boot d wcmd "$D/wrong.key")
case "$R" in err*REFUSED*'does not verify'*) ok "wrong key: ${R:0:110}" ;; *) bad "wrong key not refused: $R" ;; esac
for t in a b c; do R=$(request "$t" "raw status"); case "$R" in ok-raw*) ;; *) bad "after the refusal, status $t: $R" ;; esac; done
ok "the three sessions still answer"

say "leg 3: session b's proxy killed; a and c unaffected; its regions reused by the next boot"
OLD_B=$(grep -o "arena: session $IDX_B granted region [^:]*: [0-9]* MiB at [0-9]* MiB" "$D/comp-1.log" | head -1 | awk '{print $10}')
P=$(proxy_pid "wcmb-$$-b")
CLIENTS_BEFORE=$(grep -c 'disconnected' "$D/comp-1.log")
T0=$(date +%s%N)
# A unit that is not there shows MainPID 0, and `kill -9 0` is this script's
# whole process group: the harness and whatever started it.
[ "${P:-0}" -gt 0 ] && kill -9 "$P" 2>/dev/null && ok "killed session $IDX_B's proxy (pid $P)" || bad "no proxy pid for b"
wait_line "$D/comp-1.log" "index $IDX_B, installation wcmb\) ENDED" 20 && ok "compositor ended session $IDX_B in $(ms "$T0") ms: $(grep -m1 -o "index $IDX_B, installation wcmb) ENDED.*" "$D/comp-1.log" | cut -c1-90)" || bad "session $IDX_B was never ended"
wait_line "$D/comp-1.log" "arena: session $IDX_B's [0-9]* region" 10 && ok "$(grep -m1 -o "arena: session $IDX_B's.*" "$D/comp-1.log")" || bad "its regions did not come back"
for t in a c; do R=$(request "$t" "raw status"); case "$R" in ok-raw*) ;; *) bad "after b's death, status $t: $R" ;; esac; done
ok "a and c still answer"
S=$(sessions); case "$S" in *wcmb*) bad "b still listed: $S" ;; *) ok "sessions now: ${S#ok }" ;; esac
LOST=$(( $(grep -c 'disconnected' "$D/comp-1.log") - CLIENTS_BEFORE ))
ok "clients disconnected with session $IDX_B: $LOST (its own)"
R=$(boot b2 wcmb "$D/guest-wcmb.key")
case "$R" in ok\ located*) IDX_B2=$(echo "$R" | awk '{print $3}'); ok "wcmb booted again: index $IDX_B2 (indices are never reused)" ;; *) bad "wcmb again: $R" ;; esac
R=$(request b2 "raw run shm 96 30")
wait_line "$D/comp-1.log" "arena: session ${IDX_B2:-x} granted region" 30
NEW_B=$(grep -o "arena: session ${IDX_B2:-x} granted region [^:]*: [0-9]* MiB at [0-9]* MiB" "$D/comp-1.log" | head -1 | awk '{print $10}')
[ -n "$NEW_B" ] && [ "$NEW_B" = "$OLD_B" ] && ok "the new session's first region is at $NEW_B MiB, where the dead one's was" || bad "reuse: old at ${OLD_B:-?} MiB, new at ${NEW_B:-?} MiB"
keep_log "$D/run/wcmb-$$-b2/proxy.log" "$D/proxy-b2.log"

say "leg 3b (§113): the whole guest stalls for 8 s (SIGSTOP) under three sessions; all of them ride it out"
ENDED_N=$(grep -c ') ENDED' "$D/comp-1.log")
sleep 2 # b2 booted just now: its lease starts with its first ping
kill -STOP "$COMP"; sleep 8; kill -CONT "$COMP"
sleep 3
for t in a c b2; do
  grep -q 'LEASE LOST' "$D/proxy-$t.log" && bad "proxy $t ended over a stall: $(grep -o 'LEASE LOST.*' "$D/proxy-$t.log" | cut -c1-110)"
  wait_line "$D/proxy-$t.log" 'the guest is back' 5 || bad "proxy $t never said it held a stalled guest"
  R=$(request "$t" "raw status"); case "$R" in ok-raw*) ;; *) bad "after the stall, status $t: $R" ;; esac
done
[ "$(grep -c ') ENDED' "$D/comp-1.log")" = "$ENDED_N" ] && ok "a, c and b2 rode out an 8 s stall: $(grep -m1 -o 'the guest is back after [0-9]* ms' "$D/proxy-a.log"); $(grep -m1 -o 'this process did not run for [0-9]* ms' "$D/comp-1.log" || echo 'no probe crossed it')" \
  || bad "the compositor ended a session over the stall: $(grep ') ENDED' "$D/comp-1.log" | tail -1 | cut -c1-140)"

say "leg 4: the compositor killed under three sessions, then started again"
sleep 2
kill -9 "$COMP"; wait "$COMP" 2>/dev/null
T0=$(date +%s%N)
for t in a c b2; do
  wait_line "$D/proxy-$t.log" 'LEASE LOST' 15 && ok "proxy $t: LEASE LOST after $(ms "$T0") ms" || bad "proxy $t outlived its lease"
done
for s in "wcma-$$-a" "wcmc-$$-c" "wcmb-$$-b2"; do
  wait_line "$D/sessiond.log" "session $s ended" 20 || bad "the service never took $s down"
done
ok "the service took all three down"
kill "$PORT" 2>/dev/null; start_port
start_compositor 2
for t in a b c; do
  T0=$(date +%s%N); R=$(boot "${t}r" "wcm$t" "$D/guest-wcm$t.key")
  case "$R" in ok\ located*) ok "after the restart wcm$t booted in $(ms "$T0") ms (index $(echo "$R" | awk '{print $3}'))" ;; *) bad "after the restart wcm$t: $R" ;; esac
done
for t in a b c; do
  R=$(request "${t}r" "raw run shm 96 30"); case "$R" in ok-raw*'run accepted'*) ;; *) bad "run ${t}r: $R" ;; esac
done
for t in a b c; do keep_log "$D/run/wcm$t-$$-${t}r/proxy.log" "$D/proxy-${t}r.log"; done
sleep 3
N=$(grep -c 'granted region' "$D/comp-2.log")
[ "$N" -ge 3 ] && ok "the new compositor granted $N region(s); $(sessions)" || bad "after the restart: $N regions"
grep -q 'run 0x' "$D/comp-2.log" && ok "new run: $(grep -m1 -o 'header published: run 0x[0-9a-f]*' "$D/comp-2.log")"

say "leg 5: down for every session; the compositor ends after its linger"
for t in a b c; do
  R=$(request "${t}r" "raw down"); case "$R" in ok*) ;; *) bad "down ${t}r: $R" ;; esac
done
for t in a b c; do
  wait_line "$D/comp-2.log" "installation wcm$t\) ENDED" 15 || bad "the compositor did not end wcm$t's session"
done
ok "three sessions ended by down: $(grep -o 'ENDED after [0-9]* s: [^(]*' "$D/comp-2.log" | sort | uniq -c | tr '\n' ';')"
T0=$(date +%s%N)
wait "$COMP" 2>/dev/null; RC=$?
grep -q 'no session has been up for the linger' "$D/comp-2.log" && ok "the compositor ended $(ms "$T0") ms after the last down (linger ${LINGER} s), exit $RC" || bad "the compositor did not end by itself"
FREE=$(grep -o 'back in the pool ([0-9]* MiB free' "$D/comp-2.log" | tail -1)
ok "last return: $FREE"

say "leg 6: a BAR too small for three sessions' regions"
kill "$PORT" 2>/dev/null; start_port
start_compositor 3 44
for t in a b c; do
  R=$(boot "${t}s" "wcm$t" "$D/guest-wcm$t.key"); case "$R" in ok\ located*) ;; *) bad "small BAR boot $t: $R" ;; esac
  request "${t}s" "raw run shm 96 30" >/dev/null
  sleep 2
done
sleep 3
GR=$(grep -c 'granted region' "$D/comp-3.log"); RF=$(grep -c 'REFUSED [0-9]* bytes' "$D/comp-3.log")
ok "$(grep -m1 -o 'sessions: one compositor.*per session' "$D/comp-3.log" | cut -c1-120)"
[ "$GR" -ge 2 ] && [ "$RF" -ge 1 ] && ok "granted $GR, refused $RF (after the 16 MiB step, the third proxy asked for exactly its pool and was refused that too): $(grep -m1 -o 'REFUSED [0-9]* bytes.*' "$D/comp-3.log" | cut -c1-120)" || bad "granted $GR, refused $RF"
ok "regions: $(grep -o 'granted region [^(]*' "$D/comp-3.log" | tr '\n' ';')"
for t in a b; do R=$(request "${t}s" "raw status"); case "$R" in ok-raw*) ;; *) bad "small BAR, status $t: $R" ;; esac; done
ok "the sessions that have regions still answer: $(sessions)"
for t in a b c; do request "${t}s" "raw down" >/dev/null; done

fi
if [ -z "${WC_MULTI_ONLY_REOPEN:-}" ]; then
say "leg 7: one pool (§110): a 32 MiB BAR with a 256 MiB region beside it standing in for the GPU device"
sleep $((LINGER + 1))
kill "$COMP" 2>/dev/null; wait "$COMP" 2>/dev/null
kill "$PORT" 2>/dev/null; start_port
truncate -s $((256 << 20)) "$D/gpu"
start_compositor 4 32 "$D/gpu"
wait_line "$D/comp-4.log" 'pool region 1 ' 10 && ok "$(grep -m1 -o 'sessions: pool region 1.*' "$D/comp-4.log" | cut -c1-120)" || bad "no pool region 1"
BEFORE=$(grep -c ' mapped ' "$D/comp-4.log")
for t in a b c; do
  R=$(boot "${t}p" "wcm$t" "$D/guest-wcm$t.key"); case "$R" in ok\ located*) ;; *) bad "one-pool boot $t: $R" ;; esac
  request "${t}p" "raw run shm 96 30" >/dev/null
  keep_log "$D/run/wcm$t-$$-${t}p/proxy.log" "$D/proxy-${t}p.log"
  keep_log "$D/run/wcm$t-$$-${t}p/app.log" "$D/app-${t}p.log"
done
sleep 5
G0=$(grep -c 'granted region .* at [0-9]* MiB (it holds' "$D/comp-4.log"); G1=$(grep -c 'of pool region 1' "$D/comp-4.log")
MAPPED=$(( $(grep -c ' mapped ' "$D/comp-4.log") - BEFORE ))
[ "$G1" -ge 1 ] && ok "BAR grants $G0, pool-region-1 grants $G1: $(grep -m1 -o 'granted region [^ ]*: [0-9]* MiB at [0-9]* MiB of pool region 1' "$D/comp-4.log")" || bad "nothing was leased from region 1 (BAR $G0)"
OFFS=$(grep -o 'MiB at [0-9]* MiB of pool region 1' "$D/comp-4.log" | sort | uniq -d)
[ -z "$OFFS" ] && ok "region 1 grants at $(grep -o 'at [0-9]* MiB of pool region 1' "$D/comp-4.log" | awk '{print $2}' | tr '\n' ' ')MiB: no two sessions share a byte" || bad "two live grants at one offset: $OFFS"
[ "$MAPPED" -ge 3 ] && ok "$MAPPED window(s) mapped, three installations on a 32 MiB BAR" || bad "only $MAPPED window(s) mapped"
grep -q 'REFUSED [0-9]* bytes' "$D/comp-4.log" && bad "a refusal with region 1 beside: $(grep -m1 -o 'REFUSED [0-9]* bytes.*' "$D/comp-4.log" | cut -c1-150)"
for t in a b c; do R=$(request "${t}p" "raw status"); case "$R" in ok-raw*) ;; *) bad "one pool, status $t: $R" ;; esac; done
for t in a b c; do request "${t}p" "raw down" >/dev/null; done
wait_line "$D/comp-4.log" 'installation wcmc\) ENDED' 15
wait_line "$D/comp-4.log" 'session [0-9]'"'"'s [0-9]* region' 10
ok "after down: $(grep -c 'back in the pool' "$D/comp-4.log") session(s) returned their regions; $(grep -o 'back in the pool ([0-9]* MiB free' "$D/comp-4.log" | tail -1)"

if [ -n "$INFO" ]; then
  say "leg 8 (§107.10): short-lived clients and their feeds; the session ends by its linger, nothing left connected"
  sleep $((LINGER + 1))
  kill "$COMP" 2>/dev/null; wait "$COMP" 2>/dev/null
  kill "$PORT" 2>/dev/null; start_port
  start_compositor 5
  R=$(boot l wcma "$D/guest-wcma.key"); case "$R" in ok\ located*) IDX_L=$(echo "$R" | awk '{print $3}') ;; *) bad "linger boot: $R" ;; esac
  # wayland-info connects, reads the globals and leaves at once: the race
  # that left a feed behind, with whatever the compositor had sent it unread.
  for i in 1 2 3 4 5; do request l "raw run info 96 30" >/dev/null; sleep 0.3; done
  wait_line "$D/comp-5.log" "accepted on ring slot [0-9]* of session ${IDX_L:-x}, a feed" 15 && ok "feeds are known as feeds: $(grep -c "of session ${IDX_L:-x}, a feed" "$D/comp-5.log") of $(grep -c "of session ${IDX_L:-x}" "$D/comp-5.log") connections" || bad "no feed was marked as one"
  T0=$(date +%s%N)
  wait_line "$D/comp-5.log" "index ${IDX_L:-x}, installation wcma\) ENDED.*last client left" $((LINGER + 20)) \
    && ok "session ${IDX_L:-x} ended by its linger $(ms "$T0") ms after the last run: $(grep -m1 -o "index ${IDX_L:-x}, installation wcma) ENDED.*" "$D/comp-5.log" | cut -c1-100)" \
    || bad "session ${IDX_L:-x} did not end by its linger ($(sessions))"
  grep -q 'outlived it; closed' "$D/comp-5.log" && ok "a feed the proxy left behind was closed: $(grep -m1 -o 'ring slot [0-9]*: a feed.*' "$D/comp-5.log")"
fi
# The guest-RAM leg (§108: the one pool growing from guest RAM, the proxies mapping
# windows of grown regions) needs the broker, the one account that may make
# udmabufs of guest RAM, which the user running this harness never is. It is
# in the NixOS VM tests: scripts/sv-pool-grow-e2e.sh (the GPU pool) and
# scripts/wc-guestram-vmtest.sh (the broker, its users and its rules).
say "the guest-RAM leg (§108): in the VM tests, scripts/sv-pool-grow-e2e.sh and scripts/wc-guestram-vmtest.sh"

say "leg 9: host integration (§106) on beside the arena: every session's service slots are its own"
# The guest's compositor runs with --hostint (wc-launch.exe passes it), so each
# session has an OWNER_HOSTINT slot beside its OWNER_ARENA slot. On the guest the
# two once had one owner id, and a session's `grow` was answered with hostint's
# hello (`not a grant: "hello 1 windows"`): no client got a buffer.
sleep $((LINGER + 1))
kill "$COMP" 2>/dev/null; wait "$COMP" 2>/dev/null
kill "$PORT" 2>/dev/null; start_port
COMP_EXTRA=--hostint start_compositor 6
for t in a b c; do
  R=$(boot "${t}h" "wcm$t" "$D/guest-wcm$t.key"); case "$R" in ok\ located*) ;; *) bad "hostint boot $t: $R" ;; esac
  echo "$R" | awk '{print $3}' >"$D/idx-${t}h"
done
for t in a b c; do
  R=$(request "${t}h" "raw run shm 96 30"); case "$R" in ok-raw*'run accepted'*) ;; *) bad "hostint run ${t}h: $R" ;; esac
  keep_log "$D/run/wcm$t-$$-${t}h/proxy.log" "$D/proxy-${t}h.log"
done
sleep 5
for t in a b c; do
  I=$(cat "$D/idx-${t}h")
  wait_line "$D/comp-6.log" "arena: session $I granted region" 15 && ok "session $I (wcm$t) got its region with hostint on" || bad "session $I (wcm$t) got no region with hostint on"
  grep -q 'not a grant' "$D/proxy-${t}h.log" && bad "proxy ${t}h: $(grep -m1 -o 'not a grant.*' "$D/proxy-${t}h.log" | cut -c1-100)"
done
HS=$(grep -c 'hostint: on ring slot' "$D/comp-6.log")
[ "$HS" -ge 3 ] && ok "hostint served on $HS slot(s): $(grep -o 'hostint: on ring slot [0-9]*' "$D/comp-6.log" | awk '{print $5}' | tr '\n' ' ')" || bad "hostint on only $HS slot(s)"
MAPPED=$(grep -c ' mapped ' "$D/comp-6.log")
[ "$MAPPED" -ge 3 ] && ok "$MAPPED window(s) mapped with hostint on" || bad "only $MAPPED window(s) mapped with hostint on"
for t in a b c; do request "${t}h" "raw down" >/dev/null; done
fi

say "leg 10: reopen (§107.11): one installation opened and closed, then another opened, while the compositor keeps running"
# The guest's reopen round of 2026-10-19: a session's hostint thread outlived
# its session and kept reading its slot; once the slot was recycled, the next
# owner's bytes went to that thread (foot on vkcube's old slot, no window).
# Here session x stays up the whole leg, so the compositor never runs empty.
# Session y (its four service slots, no client) and then session o open, and
# both close. x's next client takes the three lowest slots they gave back, so
# the reopened session n starts one slot below where o did: n's first client
# lands on o's old hostint slot, which is what foot met on the guest. Every
# channel of n must work, and o's hostint thread must have stopped.
sleep $((LINGER + 1))
kill "$COMP" 2>/dev/null; wait "$COMP" 2>/dev/null
kill "$PORT" 2>/dev/null; start_port
COMP_EXTRA=--hostint start_compositor 7
L7=$D/comp-7.log
slots_of() { # INDEX: every ring slot session INDEX was given, as the compositor logged it
  grep -oE "slot [0-9]+ \(no ssh\), session $1 of|sound ring is slot [0-9]+ .*session $1 of|ring slot [0-9]+ of session $1[ ,]|session $1 asks for regions on ring slot [0-9]+|hostint: on ring slot [0-9]+ .*session $1 of" "$L7" \
    | grep -oE 'slot [0-9]+' | awk '{print $2}' | sort -n | uniq | tr '\n' ' '
}
R=$(boot xr wcmc "$D/guest-wcmc.key"); case "$R" in ok\ located*) IDX_X=$(echo "$R" | awk '{print $3}') ;; *) bad "reopen: boot x: $R" ;; esac
request xr "raw run shm 96 30" >/dev/null
wait_line "$L7" "accepted on ring slot [0-9]* of session ${IDX_X:-x} " 15 || bad "reopen: no client of x"
R=$(boot yr wcmd "$D/guest-wcmd.key"); case "$R" in ok\ located*) IDX_Y=$(echo "$R" | awk '{print $3}') ;; *) bad "reopen: boot y: $R" ;; esac
wait_line "$L7" "hostint: on ring slot [0-9]*" 1; sleep 1
R=$(boot or wcma "$D/guest-wcma.key"); case "$R" in ok\ located*) IDX_O=$(echo "$R" | awk '{print $3}') ;; *) bad "reopen: boot o: $R" ;; esac
request or "raw run shm 96 30" >/dev/null
wait_line "$L7" "accepted on ring slot [0-9]* of session ${IDX_O:-x} " 15 || bad "reopen: no client of o"
wait_line "$L7" "arena: session ${IDX_O:-x} granted region" 15 || bad "reopen: o got no region"
sleep 1
O_H=$(grep -o 'hostint: on ring slot [0-9]*' "$L7" | sed -n 3p | awk '{print $5}')
ok "session ${IDX_O:-?} (wcma) has slots $(slots_of "${IDX_O:-x}")and hostint on slot ${O_H:-?}"
ENDED_BEFORE=$(grep -c 'hostint: the channel ended' "$L7")
T0=$(date +%s%N)
for t in yr or; do R=$(request "$t" "raw down"); case "$R" in ok*) ;; *) bad "reopen: down $t: $R" ;; esac; done
wait_line "$L7" "index ${IDX_Y:-x}, installation wcmd\) ENDED" 15 || bad "reopen: y did not end"
wait_line "$L7" "index ${IDX_O:-x}, installation wcma\) ENDED" 15 && ok "sessions ${IDX_Y:-?} and ${IDX_O:-?} ENDED $(ms "$T0") ms after their downs" || bad "reopen: o did not end"
for _ in $(seq 1 60); do [ "$(grep -c 'hostint: the channel ended' "$L7")" -ge $((ENDED_BEFORE + 2)) ] && break; sleep 0.05; done
[ "$(grep -c 'hostint: the channel ended' "$L7")" -ge $((ENDED_BEFORE + 2)) ] \
  && ok "both hostint threads stopped within $(ms "$T0") ms of the downs: $(grep 'hostint: the channel ended' "$L7" | tail -1 | sed 's/^wc-compositor: //' | cut -c1-90)" \
  || bad "$(( $(grep -c 'hostint: the channel ended' "$L7") - ENDED_BEFORE )) of the 2 ended sessions' hostint threads said the channel ended"
# Past the quarantine (ringlink::QUARANTINE, 10 s), so o's slots are free.
sleep 12
request xr "raw run shm 96 30" >/dev/null
sleep 2
R=$(boot nr wcmb "$D/guest-wcmb.key"); case "$R" in ok\ located*) IDX_N=$(echo "$R" | awk '{print $3}'); ok "wcmb opened as session $IDX_N while the compositor kept running" ;; *) bad "reopen: boot n: $R" ;; esac
wait_line "$L7" "session: control socket $D/ctl-nr.sock" 20 || bad "reopen: no control socket for n"
R=$(request nr "raw run shm 96 30"); case "$R" in ok-raw*'run accepted'*) ok "the reopened session's control ring answers" ;; *) bad "reopen: run in n: $R" ;; esac
keep_log "$D/run/wcmb-$$-nr/proxy.log" "$D/proxy-nr.log"
wait_line "$L7" "arena: session ${IDX_N:-x} granted region" 15 && ok "session ${IDX_N:-?} got its region" || bad "reopen: session ${IDX_N:-?} got no region"
wait_line "$L7" "accepted on ring slot [0-9]* of session ${IDX_N:-x} " 15 || bad "reopen: no client of n"
for _ in $(seq 1 100); do [ "$(grep -c ' mapped ' "$L7")" -ge 4 ] && break; sleep 0.1; done
MAPPED=$(grep -c ' mapped ' "$L7")
[ "$MAPPED" -ge 4 ] && ok "$MAPPED window(s) mapped: x's two, o's, and the reopened session's" || bad "only $MAPPED window(s) mapped (x 2, o 1, n 1 expected)"
wait_line "$D/proxy-nr.log" 'host says hello' 10 && ok "the reopened session's hostint heard the host's hello" || bad "reopen: n's hostint never heard the host"
N_SLOTS=$(slots_of "${IDX_N:-x}")
ON_H=$(grep -E "slot ${O_H:-x}[ ,(].*session ${IDX_N:-x}[ ,]|slot ${O_H:-x} of session ${IDX_N:-x}" "$L7" | tail -1 | sed 's/^wc-compositor: //; s/ (no socket.*//' | cut -c1-80)
case " $N_SLOTS" in *" ${O_H:-x} "*) ok "o's old hostint slot ${O_H} is the reopened session's now (${ON_H}; its slots ${N_SLOTS}), and it works" ;;
  *) ok "the reopened session's slots: ${N_SLOTS}(o's old hostint slot ${O_H:-?} went elsewhere)" ;; esac
for _ in 1 2 3; do R=$(request nr "raw status"); case "$R" in ok-raw*) ;; *) bad "reopen: status n: $R" ;; esac; done
grep -q "index ${IDX_N:-x}, installation wcmb) ENDED" "$L7" && bad "the reopened session ended by itself: $(grep -m1 -o "index ${IDX_N:-x}, installation wcmb) ENDED.*" "$L7")"
grep -q 'REFUSED' "$L7" && bad "a refusal: $(grep -m1 REFUSED "$L7")"
for t in xr nr; do request "$t" "raw down" >/dev/null; done

if [ -n "${WC_MULTI_EVIDENCE:-}" ]; then mkdir -p "$WC_MULTI_EVIDENCE" && cp "$D"/*.log "$WC_MULTI_EVIDENCE"/ 2>/dev/null; fi
echo "logs: $D"
[ "$FAIL" = 0 ] && { echo "wc-multi-install: OK"; rm -rf "$D"; exit 0; }
echo "wc-multi-install: FAILED"; exit 1
