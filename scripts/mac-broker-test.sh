#!/usr/bin/env bash
# §111.9: the macOS broker (the LaunchDaemon org.waycompose.broker) in the
# macOS VM, after scripts/mac-launch-install.sh.
#
#   scripts/mac-broker-test.sh PHASE...
#
#   state : the daemon (launchd's view, its pid), the port nodes (root's,
#           0600), the socket (0666), the keys (root's, 0700/0600), and that
#           the VM's user cannot open a port itself.
#   rtt   : a main process (as mac-gpu.sh starts it, no face), and 2000
#           wl_display_roundtrip through the session's proxy from this side.
#   gpu   : MAC_GPU_VIA=session scripts/mac-gpu.sh present, 300 frames of
#           256x256, no dumps: the GPU server started through `--gpu-exec`
#           (tied to the session, svbell moved in by the broker).
#   kill N: the §103.15 panic case, N times: Console's session under a
#           drawing load from this side (foot running `seq`), its main
#           process killed with SIGKILL; the broker's log must show the
#           windows held as orphans until the release word, and the guest's
#           panic count and uptime must stay.
#   term  : SIGTERM to the daemon while it holds orphans: it exits within
#           10 s, and launchd starts it again (KeepAlive).
#   users : a second macOS user (sysadminctl, deleted at the end): its
#           processes cannot open the first user's regions, cannot take the
#           ports, and the boot record the broker signs for it names it.
#
# Logs go to $EVID (default /mnt/datasets/builds/evidence/session-fixes/broker-mac),
# never into the repository. The VM is reached over the VM manager's own ssh,
# as every harness here does; the product never is.
set -uo pipefail
inst=${MACVM_INSTANCE:-wayland-host}
idir=/mnt/datasets/macvm/instances/$inst
VMUSER=$(sed -n 's/.*"user": "\([^"]*\)".*/\1/p' "$idir/instance.json")
EVID=${EVID:-/mnt/datasets/builds/evidence/session-fixes/broker-mac}
TAG=${TAG:-broker}
SCR=${SCR:-/mnt/datasets/builds/wcrun/broker-mac-scratch}
mkdir -p "$EVID"
vm() { timeout "${VMT:-60}" /mnt/datasets/macvm/bin/macos-vm ssh "$inst" "$@"; }
say() { echo "[broker-test] $*"; }
G='Library/Application Support/waycompose'
SYS='/Library/Application Support/waycompose'
broker_log() { vm "sudo -n cat /Library/Logs/waycompose/broker.log" 2>/dev/null; }
panics() { vm 'ls /Library/Logs/DiagnosticReports/*.panic 2>/dev/null | wc -l' | tr -d ' '; }

# The main process as mac-gpu.sh starts it; prints the session id.
start_main() {
  vm "rm -f ~/Library/Logs/waycompose/main.log; nohup \"\$HOME/$G/bin/wc-launch\" --main >/dev/null 2>&1 </dev/null &"
  local sid=""
  for _ in $(seq 1 300); do
    sid=$(vm "sed -n 's/.*main process: session \(wcm-[0-9]*\).*/\1/p' ~/Library/Logs/waycompose/main.log 2>/dev/null | tail -1")
    [ -n "$sid" ] && [ -S "/tmp/wc-launch/$sid/wl-proxy" ] && break
    sleep 0.1
  done
  echo "$sid"
}

phase_state() {
  local out="$EVID/$TAG-state.txt"
  {
    echo "== launchd"; vm "sudo -n launchctl print system/org.waycompose.broker | grep -E 'state|pid =|program|keepalive|last exit'"
    echo "== ports"; vm "ls -l /dev/cu.org.sirati.*"
    echo "== socket"; vm "ls -ld /var/run/waycompose /var/run/waycompose/broker.sock"
    echo "== keys"; vm "sudo -n ls -la \"$SYS/keys\" \"$SYS/bin\""
    echo "== the VM's user opens a port itself"; vm "head -c0 /dev/cu.org.sirati.wcbell.0 && echo OPENED || echo refused: \$?" 2>&1
    echo "== the VM's user reads a key"; vm "cat \"$SYS/keys/wcm.key\" >/dev/null 2>&1 && echo READ || echo refused"
    echo "== stats"; vm "\"$SYS/bin/wc-broker\" ctl 'hello state' stats"
  } 2>&1 | tee "$out"
}

phase_rtt() {
  local sid; sid=$(start_main)
  [ -n "$sid" ] || { say "no session"; return 1; }
  sleep 3
  say "session $sid; 2000 round trips"
  WAYLAND_DISPLAY="/tmp/wc-launch/$sid/wl-proxy" "$SCR/rtt" 2000 | tee "$EVID/$TAG-rtt.txt"
  # The main process's CPU over 30 s idle, as §103.15 measured it.
  vm "top -l 2 -s 30 -pid ${sid#wcm-} -stats pid,cpu,command | tail -1" | tee -a "$EVID/$TAG-rtt.txt"
  vm "kill ${sid#wcm-}"; sleep 4
}

phase_gpu() {
  MAC_GPU_DUMP=0 VK_PRESENT_PROBE_FRAMES=300 VK_PRESENT_PROBE_W=256 VK_PRESENT_PROBE_H=256 \
    MAC_GPU_VIA=session "$(dirname "$0")/mac-gpu.sh" present 2>&1 | tee "$EVID/$TAG-gpu.txt" | grep -E "timing|ring waits|PASS|FAIL|TIMING"
  sleep 5
  say "the GPU server after its session's release:"
  broker_log | grep -E "ends with pool|pool .* is set|ending it|\(gpu\) exited" | tail -6 | tee -a "$EVID/$TAG-gpu.txt"
}

phase_kill() {
  local n=${1:-5} out="$EVID/$TAG-kill.txt"
  local before; before=$(panics)
  : >"$out"
  for i in $(seq 1 "$n"); do
    vm "pkill -f 'Linux/Console.app' 2>/dev/null; true"; sleep 2
    vm "rm -f ~/Library/Logs/waycompose/main.log; open ~/Applications/Linux/Console.app"
    local sid=""
    for _ in $(seq 1 200); do
      sid=$(vm "sed -n 's/.*main process: session \(wcm-[0-9]*\).*/\1/p' ~/Library/Logs/waycompose/main.log 2>/dev/null | tail -1")
      [ -n "$sid" ] && [ -S "/tmp/wc-launch/$sid/wl-proxy" ] && break
      sleep 0.2
    done
    [ -n "$sid" ] || { say "[$i] no session" | tee -a "$out"; continue; }
    sleep 4
    # The load: a terminal from this side scrolling for ever, its pixels
    # written into the session's arena by the proxy. Throwaway HOME/XDG.
    local th; th=$(mktemp -d "$SCR/foot-XXXX")
    HOME=$th XDG_CONFIG_HOME=$th/c XDG_DATA_HOME=$th/d XDG_STATE_HOME=$th/s XDG_CACHE_HOME=$th/k \
      WAYLAND_DISPLAY="/tmp/wc-launch/$sid/wl-proxy" timeout 60 "$SCR/foot/bin/foot" -o tweak.max-shm-pool-size-mb=0 \
      sh -c 'while :; do seq 1 100000; done' >/dev/null 2>&1 &
    local fp=$!
    sleep 6
    local mainpid=${sid#wcm-}
    local t0; t0=$(date +%s.%N)
    vm "kill -9 $mainpid; echo killed $mainpid" | sed "s/^/  [$i] /" | tee -a "$out"
    sleep 15
    kill "$fp" 2>/dev/null; wait "$fp" 2>/dev/null; rm -rf "$th"
    broker_log | grep -E "process $mainpid \(main\) exited|pool $sid|ending it" | tail -4 | sed "s/^/  [$i] broker: /" | tee -a "$out"
    journalctl --user -u wc-mac-sessiond --since "-40s" --no-pager -o cat | grep -E "$sid.*(released|not released|ended)" | tail -2 | sed "s/^/  [$i] host: /" | tee -a "$out"
    vm "uptime" | sed "s/^/  [$i] /" | tee -a "$out"
  done
  local after; after=$(panics)
  say "panics before $before after $after" | tee -a "$out"
}

phase_term() {
  local out="$EVID/$TAG-term.txt"
  {
    local p0; p0=$(vm "sudo -n launchctl print system/org.waycompose.broker | sed -n 's/.*pid = //p'")
    echo "daemon pid $p0"
    # Orphans to hold: a main process killed, the host given no time.
    local sid; sid=$(start_main); sleep 2
    vm "kill -9 ${sid#wcm-}"; sleep 0.5
    vm "\"$SYS/bin/wc-broker\" ctl 'hello term' stats"
    local t0; t0=$(date +%s.%N)
    vm "sudo -n kill -TERM $p0"
    for _ in $(seq 1 300); do vm "kill -0 $p0 2>/dev/null" || break; sleep 0.1; done
    echo "the daemon ended $(awk -v a="$t0" -v b="$(date +%s.%N)" 'BEGIN{printf "%.2f", b-a}') s after SIGTERM"
    sleep 3
    echo "relaunched by launchd: pid $(vm "sudo -n launchctl print system/org.waycompose.broker | sed -n 's/.*pid = //p'")"
    broker_log | grep -E "SIGTERM|orphan|exiting" | tail -4
  } 2>&1 | tee "$out"
}

phase_users() {
  local out="$EVID/$TAG-users.txt" U2=wctest2
  local pw; pw=$(head -c 18 /dev/urandom | base64 | tr -d '/+=')
  {
    vm "id $U2 >/dev/null 2>&1 || { sudo -n sysadminctl -addUser $U2 -password '$pw' -home /Users/$U2 >/dev/null 2>&1; sudo -n createhomedir -c -u $U2 >/dev/null 2>&1; }; id $U2"
    # The first user's session.
    local sid; sid=$(start_main); sleep 3
    echo "user 1's session $sid"
    local ids; ids=$(broker_log | sed -n "s/.*region \([0-9]*\) of pool $sid: .*/\1/p" | tr '\n' ' ')
    echo "its regions: $ids"
    local as2="sudo -n -u $U2 -H env HOME=/Users/$U2 \"$SYS/bin/wc-broker\" ctl"
    echo "== user 2 opens user 1's regions, finds its pool, takes the ports"
    for id in $ids; do vm "$as2 'open $id'"; done
    vm "$as2 'find $sid' 'device wcbell' 'device svbell'"
    echo "== user 2 opens the port nodes and user 1's keys itself"
    vm "sudo -n -u $U2 sh -c 'head -c0 /dev/cu.org.sirati.wcbell.0 && echo OPENED || echo refused'" 2>&1
    vm "sudo -n -u $U2 sh -c 'cat \"$SYS/keys/wcm.key\" >/dev/null 2>&1 && echo READ || echo refused'"
    echo "== user 2's own region and its signature"
    vm "$as2 'hello u2' 'alloc u2pool 8192' 'sign wcm u2-1 0 1 2 0 3 2 0 1' stats"
    echo "== user 2's session (its own main process; the doorbell port is user 1's, so it polls and the host finds it by its sweep)"
    vm "sudo -n mkdir -p \"/Users/$U2/$G/bin\"; sudo -n cp \"\$HOME/$G/bin/wc-launch\" \"\$HOME/$G/wc-launch.ini\" \"/Users/$U2/$G/\"; \
        sudo -n mv \"/Users/$U2/$G/wc-launch\" \"/Users/$U2/$G/bin/\"; sudo -n chown -R $U2 \"/Users/$U2/Library\"; \
        sudo -n -u $U2 -H sh -c 'cd; nohup \"\$HOME/$G/bin/wc-launch\" --main >/dev/null 2>&1 </dev/null &'"
    sleep 25
    vm "sudo -n -u $U2 cat \"/Users/$U2/Library/Logs/waycompose/main.log\" 2>/dev/null | tail -8"
    journalctl --user -u wc-mac-sessiond --since "-60s" --no-pager -o cat | grep -E "asked by|refused" | tail -4
    echo "== user 1 opens user 2's regions"
    local ids2; ids2=$(broker_log | grep "of uid:" | sed -n "s/.*region \([0-9]*\) of pool \([^:]*\): .* of uid:\([0-9]*\)/\1 \3/p" | awk -v me="$(vm 'id -u')" '$2!=me{print $1}' | tr '\n' ' ')
    for id in $ids2; do vm "\"$SYS/bin/wc-broker\" ctl 'open $id'"; done
    vm "sudo -n pkill -u $U2 -f 'wc-launch --main'; kill ${sid#wcm-}"; sleep 8
    vm "sudo -n sysadminctl -deleteUser $U2 >/dev/null 2>&1; id $U2 2>&1 | head -1"
  } 2>&1 | tee "$out"
}

for p in "$@"; do
  case $p in
    state) phase_state ;;
    rtt) phase_rtt ;;
    gpu) phase_gpu ;;
    kill) phase_kill "${KILLS:-5}" ;;
    term) phase_term ;;
    users) phase_users ;;
    *) echo "unknown phase $p"; exit 2 ;;
  esac
done
