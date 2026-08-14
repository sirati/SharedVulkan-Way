#!/usr/bin/env bash
#
# The Windows-guest doorbell round trip. waycompose-design.md §36.19's open
# question: does `WaitOnAddress`'s whole-millisecond argument put a floor under a
# *delivered* wake, or only under a missed one?
#
#   scripts/sv-bell-guest.sh
#
# Every figure in §36.6.3 and §36.15 is the *Linux* development guest. This is
# the deployment guest -- where the 216.9 ms present was measured, and the only
# place `WaitOnAddress` actually runs. §36.19 predicts the floor does not apply,
# from a two-second-slice measurement on the Windows binary under Wine; this is
# the prediction's test.
#
# ROUTES, both verified, and which does what
#
#   guest   <guest-user>@<guest-ip>   direct from here, via the 1Password agent
#   host    <host-user>@<host-ip>   testhost
#
# The guest half is driven **directly** -- no nested ssh -- because the
# 1Password agent holds that key. The host half must run *on* testhost
# regardless of routing: the chardev socket it measures is a file there.
#
# WHAT THIS DOES TO A MACHINE SOMEBODY IS USING
#
# One hot-add over QMP: a chardev and a virtserialport on the controller that
# already exists. **Addition only** -- nothing removed, nothing existing
# modified, no restart, desktop session untouched. §36.16 established a Windows
# guest with a live desktop notices nothing. The port is transient by
# construction, which is a feature here: it dies with the QEMU process, so a
# reboot undoes this whatever happens. Making it permanent is one line in
# testhost's NixOS config and needs a restart, which is the user's to time.
#
# TWO THINGS IT REFUSES TO ASSUME
#
# 1. **The QMP chardev serves exactly one client.** A second opener hangs
#    silently, so this refuses rather than joining the queue.
# 2. **The guest's Windows session may be locked**, which silently discards
#    injected input. This injects none -- it is ssh driving a virtio-serial
#    handle -- but "should not matter" is not evidence, so the guest half must
#    be *seen echoing* before any latency is quoted. A silent discard then fails
#    the handshake instead of producing a plausible number.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
VMHOST=${SV_HOST:-<host-user>@<host-ip>}
GUEST=${SV_GUEST:-<guest-user>@<guest-ip>}
AGENT=${SV_SSH_AGENT:-$HOME/.1password/agent.sock}
PORT=org.sirati.svbell.0
SOCK=/run/windows-vm/vport-$PORT.sock
COUNT=${SV_BELL_COUNT:-2000}

S=(-o IdentityAgent="$AGENT" -o BatchMode=yes -o ConnectTimeout=15 -o LogLevel=ERROR)
say()   { echo "[$(date +%H:%M:%S)] $*"; }
vmhost()  { ssh "${S[@]}" "$VMHOST" "$@"; }
guest() { ssh "${S[@]}" "$GUEST" "$@"; }

# The virtio-serial interfaces the guest has now. The difference across the
# hot-add names the new one **by construction** -- §36.16's method, and the
# reason no `vport0pN` ordinal is ever guessed: `&0&02` is almost certainly the
# WiFi port, and a virtio-serial port takes exactly one opener.
guest_ports() {
  guest 'powershell -NoProfile -Command "Get-ChildItem \"HKLM:\SYSTEM\CurrentControlSet\Control\DeviceClasses\" -EA SilentlyContinue | Where-Object { $_.PSChildName -match \"6fde7521\" } | ForEach-Object { Get-ChildItem $_.PSPath -EA SilentlyContinue | ForEach-Object { $_.PSChildName } }"' 2>/dev/null | tr -d '\r' | grep VIOSerialPort | sort
}

say "refusing to share the QMP socket"
# By executable name, never by command line. See `proc_running` in
# wc-binaries.sh for why: the ssh command carrying this check contains the
# string it searches for, so a `pgrep -f` matched its own shell and refused a
# socket that was free.
if vmhost 'export PATH=/run/current-system/sw/bin; pgrep -x sv-hostd >/dev/null' 2>/dev/null; then
  echo "FAIL: an sv-hostd holds QMP. The chardev serves one client and a second"
  echo "      opener hangs silently. Wait for that run to finish."
  exit 1
fi

say "guest interfaces before"; BEFORE=$(guest_ports); echo "$BEFORE" | sed 's/^/    /'

if ! vmhost "test -S $SOCK" 2>/dev/null; then
  say "hot-adding $PORT (addition only, no restart)"
  vmhost "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; {
      printf '%s\\n' '{\"execute\":\"qmp_capabilities\"}'; sleep 0.4
      printf '%s\\n' '{\"execute\":\"chardev-add\",\"arguments\":{\"id\":\"svbell\",\"backend\":{\"type\":\"socket\",\"data\":{\"addr\":{\"type\":\"unix\",\"data\":{\"path\":\"$SOCK\"}},\"server\":true,\"wait\":false}}}}'; sleep 0.6
      printf '%s\\n' '{\"execute\":\"device_add\",\"arguments\":{\"driver\":\"virtserialport\",\"bus\":\"vioserial.0\",\"chardev\":\"svbell\",\"name\":\"$PORT\",\"id\":\"svbellport\"}}'; sleep 1.5
    } | sudo -n socat - UNIX-CONNECT:/run/windows-vm/qmp.sock 2>&1 | tr ',' '\\n' | grep -iE '\"error\"|desc' | head -4"
  sleep 3
else
  say "$PORT is already present"
fi
vmhost "test -S $SOCK" 2>/dev/null || { echo "FAIL: no $SOCK after the hot-add"; exit 1; }

say "guest interfaces after"; AFTER=$(guest_ports); echo "$AFTER" | sed 's/^/    /'
NEW=$(comm -13 <(printf '%s\n' "$BEFORE") <(printf '%s\n' "$AFTER") 2>/dev/null)
[ -n "$NEW" ] && say "new interface, known by construction: $NEW"

say "deploying the probe"
scp "${S[@]}" -q "$ROOT/target/x86_64-pc-windows-gnu/debug/sv-bell-probe.exe" \
    "$GUEST:C:/Users/<guest-user>/sv-bell-probe.exe" || { echo "FAIL: cannot deploy to the guest"; exit 1; }
scp "${S[@]}" -q "$ROOT/target/debug/sv-bell-probe" "$VMHOST:/tmp/sv-bell-probe-linux" || exit 1

# The echo must be seen before any latency is quoted -- see the header.
say "starting the guest half"
guest "C:/Users/<guest-user>/sv-bell-probe.exe guest --port $PORT --seconds 180" > /tmp/sv-bellguest.log 2>&1 &
GP=$!
for _ in $(seq 1 40); do grep -q "echoing on" /tmp/sv-bellguest.log 2>/dev/null && break; sleep 1; done
if ! grep -q "echoing on" /tmp/sv-bellguest.log 2>/dev/null; then
  echo "FAIL: the guest half never opened the port -- measuring nothing"
  sed 's/^/    /' /tmp/sv-bellguest.log 2>/dev/null | head -6
  kill $GP 2>/dev/null; exit 1
fi
say "guest half is echoing -- $(head -1 /tmp/sv-bellguest.log | tr -d '\r')"

say "measuring $COUNT round trips across the deployment boundary"
vmhost "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; sudo -n /tmp/sv-bell-probe-linux host --port $SOCK --count $COUNT"
rc=$?

kill $GP 2>/dev/null
guest 'rm -f /c/Users/<guest-user>/sv-bell-probe.exe' >/dev/null 2>&1
vmhost 'rm -f /tmp/sv-bell-probe-linux' >/dev/null 2>&1
rm -f /tmp/sv-bellguest.log
say "the port is transient and dies with the QEMU process; nothing to undo"
exit $rc
