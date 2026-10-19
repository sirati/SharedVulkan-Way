#!/usr/bin/env bash
# wc-guest-idle.sh: is <guest-user>'s desktop ours? (docs/waycompose-design.md §101)
# Installed as /tmp/wcrun/guest-idle.sh.
#
# The rule: the desktop is ours when nobody has touched the keyboard or mouse for
# GUEST_IDLE_SECS (default 120) seconds, and nobody is connected to it through
# TeamViewer. Two signals:
#
# - Input, from wc-activity.exe (v2) in the guest's session (scripts/wc-activity.c).
#   Every input counts, injected or not, because a person working through
#   TeamViewer arrives as injected input. Not counted: what our tools inject
#   (they tag it, and the sentinel skips it); input from half a second before to
#   two seconds after an injection logged on the host (/tmp/wcrun/qmp-inject.log:
#   QEMU's keys look like hardware) or on the guest
#   (%LOCALAPPDATA%\waycompose\inject.log: for a tool of ours that cannot tag).
# - TeamViewer's own log: while its last `ConnectionGuard:` line says "incoming
#   remote control in sessions", somebody is connected, whatever the input says.
#   Only "no restrictions" after it counts as the session gone.
#
#   exit 0  idle for at least GUEST_IDLE_SECS; prints "idle N s"
#   exit 2  in use: "active N s ago", or "in use: TeamViewer remote control ..."
#   exit 1  unknown: no sentinel (or an old one), or its heartbeat is stale.
#           Counts as NOT idle.
#
# A sentinel that is not running (no file, or a heartbeat over 15 s old) is
# started once through its scheduled task (`schtasks /Run`: no window, no
# input), and the file is read again. Something outside can end it: on
# 2026-10-19 an Office update's Restart Manager did. Still stale after that:
# unknown, as before. GUEST_IDLE_NO_RESTART=1 skips the restart.
. "$(dirname "${BASH_SOURCE[0]}")/site.sh"
set -u
LIMIT=${GUEST_IDLE_SECS:-120}
LOG=${GUEST_INJECT_LOG:-/tmp/wcrun/qmp-inject.log}
G="ssh -o BatchMode=yes -o ConnectTimeout=8 -o LogLevel=ERROR $SITE_GUEST"
# GUEST_ACTIVITY_TEXT: the sentinel's file and a `guest_now` line, for testing.
# GUEST_ACTIVITY_TEXT_AFTER: what the second read returns after a restart.
fetch() {
t0=$(date +%s%3N)
if [ -n "${GUEST_ACTIVITY_TEXT:-}" ]; then out=$GUEST_ACTIVITY_TEXT
else out=$($G 'W=/c/Users/'"$SITE_GUEST_USER"'/AppData/Local/waycompose; cat $W/activity.txt 2>/dev/null
  tail -n 200 $W/inject.log 2>/dev/null | sed "s/^/inj /"
  T="/c/Program Files/TeamViewer"
  if [ -d "$T" ]; then
    l=$(grep -a "ConnectionGuard:" "$T/TeamViewer15_Logfile.log" 2>/dev/null | tail -1)
    [ -n "$l" ] || l=$(grep -a "ConnectionGuard:" "$T/TeamViewer15_Logfile_OLD.log" 2>/dev/null | tail -1)
    echo "tv ${l:-none}"
  fi
  echo "guest_now $(date +%s%3N)"' 2>/dev/null | tr -d '\r'); fi
t1=$(date +%s%3N)
}
fetch
[ -n "$out" ] || { echo "unknown: cannot reach the guest"; exit 1; }
# Not running: start its task once, then read again.
hb0=$(awk '$1=="heartbeat_wall_ms"{print $2}' <<<"$out")
now0=$(awk '$1=="guest_now"{print $2}' <<<"$out")
if [ "${GUEST_IDLE_NO_RESTART:-0}" != 1 ] && [ -n "$now0" ] && { [ -z "$hb0" ] || [ $(( now0 - hb0 )) -gt 15000 ]; }; then
  if [ -n "${GUEST_ACTIVITY_TEXT:-}" ]; then
    GUEST_ACTIVITY_TEXT=${GUEST_ACTIVITY_TEXT_AFTER:-$GUEST_ACTIVITY_TEXT}
  else
    $G 'MSYS2_ARG_CONV_EXCL="*" schtasks /Run /TN wc-activity' >/dev/null 2>&1
    sleep 4
  fi
  echo "wc-activity was not running; started its task once" >&2
  fetch
  [ -n "$out" ] || { echo "unknown: cannot reach the guest"; exit 1; }
fi
now=$(awk '$1=="guest_now"{print $2}' <<<"$out")
hb=$(awk '$1=="heartbeat_wall_ms"{print $2}' <<<"$out")
hooks=$(awk '$1=="hooks"{print $2}' <<<"$out")
started=$(awk '$1=="started_wall_ms"{print $2}' <<<"$out")
[ -n "$hb" ] && [ -n "$now" ] || { echo "unknown: wc-activity is not running (no activity.txt)"; exit 1; }
[ $(( now - hb )) -le 15000 ] || { echo "unknown: wc-activity's heartbeat is $(( (now - hb) / 1000 )) s old"; exit 1; }
[ "${hooks:-0}" = 1 ] || { echo "unknown: wc-activity has no hooks"; exit 1; }
# v1 dropped injected input, which is how TeamViewer's arrives.
ver=$(awk '$1=="wc-activity"{print $2}' <<<"$out")
[ "${ver:-0}" -ge 2 ] || { echo "unknown: wc-activity is version ${ver:-?}, which cannot see remote input"; exit 1; }
tv=$(awk '$1=="tv"{sub(/^tv /,""); print}' <<<"$out")
case "$tv" in
  *"incoming remote control in sessions"*)
    echo "in use: TeamViewer remote control since $(cut -c1-19 <<<"$tv")"; exit 2 ;;
esac
# The guest's clock against this one, from the middle of the ssh round trip.
off=$(( now - (t0 + t1) / 2 ))
# Injections, as guest milliseconds: QEMU's (host clock, hardware-kind input)
# and the guest's own log (guest clock, injected-kind input).
qinj=$( [ -f "$LOG" ] && awk -v off="$off" '{print $1 + off}' "$LOG" | tail -200 )
ginj=$(awk '$1=="inj"{print $2}' <<<"$out")
# The newest input that is not ours. The sentinel keeps the last 64 (coalesced
# to 250 ms); if every one of them is ours, see below.
last=$(awk -v q="$qinj" -v g="$ginj" 'BEGIN{nq=split(q, Q, "\n"); ng=split(g, J, "\n")}
  $1=="input"{t=$2; k=$3; ours=0
    if (k=="k" || k=="m") { for(i=1;i<=nq;i++) if (Q[i]!="" && t>=Q[i]-500 && t<=Q[i]+2000) ours=1 }
    else { for(i=1;i<=ng;i++) if (J[i]!="" && t>=J[i]-500 && t<=J[i]+2000) ours=1 }
    if(!ours){print t; exit}}' <<<"$out")
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
