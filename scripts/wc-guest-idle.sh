#!/usr/bin/env bash
# wc-guest-idle.sh: is <guest-user>'s desktop ours? (docs/waycompose-design.md §101)
# Installed as /tmp/wcrun/guest-idle.sh.
#
# The rule: the desktop is ours when nobody has touched the keyboard or mouse for
# GUEST_IDLE_SECS (default 120) seconds. The answer comes from wc-activity.exe in
# the guest's session (scripts/wc-activity.c), which records only input Windows
# does not flag as injected, so an agent's SendInput never counts. Input injected
# through QEMU (guest-keys.sh, unlock-guest.sh) does look real there; those
# scripts log each injection to /tmp/wcrun/qmp-inject.log, and input from half a
# second before to two seconds after a logged injection is ours too.
#
#   exit 0  idle for at least GUEST_IDLE_SECS; prints "idle N s"
#   exit 2  somebody used it N s ago;           prints "active N s ago"
#   exit 1  unknown: no sentinel, or its heartbeat is stale. Counts as NOT idle.
set -u
LIMIT=${GUEST_IDLE_SECS:-120}
LOG=${GUEST_INJECT_LOG:-/tmp/wcrun/qmp-inject.log}
G="ssh -o BatchMode=yes -o ConnectTimeout=8 -o LogLevel=ERROR <guest-user>@<guest-ip>"
t0=$(date +%s%3N)
# GUEST_ACTIVITY_TEXT: the sentinel's file and a `guest_now` line, for testing.
if [ -n "${GUEST_ACTIVITY_TEXT:-}" ]; then out=$GUEST_ACTIVITY_TEXT
else out=$($G 'cat /c/Users/<guest-user>/AppData/Local/waycompose/activity.txt 2>/dev/null; echo "guest_now $(date +%s%3N)"' 2>/dev/null | tr -d '\r'); fi
t1=$(date +%s%3N)
[ -n "$out" ] || { echo "unknown: cannot reach the guest"; exit 1; }
now=$(awk '$1=="guest_now"{print $2}' <<<"$out")
hb=$(awk '$1=="heartbeat_wall_ms"{print $2}' <<<"$out")
hooks=$(awk '$1=="hooks"{print $2}' <<<"$out")
started=$(awk '$1=="started_wall_ms"{print $2}' <<<"$out")
[ -n "$hb" ] && [ -n "$now" ] || { echo "unknown: wc-activity is not running (no activity.txt)"; exit 1; }
[ $(( now - hb )) -le 15000 ] || { echo "unknown: wc-activity's heartbeat is $(( (now - hb) / 1000 )) s old"; exit 1; }
[ "${hooks:-0}" = 1 ] || { echo "unknown: wc-activity has no hooks"; exit 1; }
# The guest's clock against this one, from the middle of the ssh round trip.
off=$(( now - (t0 + t1) / 2 ))
# Injections, as guest milliseconds.
inj=$( [ -f "$LOG" ] && awk -v off="$off" '{print $1 + off}' "$LOG" | tail -200 )
# The newest input that is not ours. The sentinel keeps the last 64 (coalesced
# to 250 ms); if every one of them is ours, the last real input is older than
# the oldest, and that is the conservative answer.
last=$(awk -v inj="$inj" 'BEGIN{n=split(inj, I, "\n")} $1=="input"{t=$2; ours=0; for(i=1;i<=n;i++) if (I[i]!="" && t>=I[i]-500 && t<=I[i]+2000) ours=1; if(!ours){print t; exit} oldest=t} END{}' <<<"$out")
if [ -z "$last" ]; then
  # No real input in the ring. Not full (64): none since the sentinel started,
  # which is the conservative answer, since it cannot know what came before it ran.
  # Full: the real one is older than the oldest entry kept.
  count=$(awk '$1=="inputs"{print $2}' <<<"$out")
  if [ "${count:-0}" -ge 64 ]; then last=$(awk '$1=="input"{t=$2} END{print t}' <<<"$out"); else last=$started; fi
fi
age=$(( (now - last) / 1000 ))
if [ "$age" -ge "$LIMIT" ]; then echo "idle $age s"; exit 0; fi
echo "active $age s ago"; exit 2
