#!/usr/bin/env bash
# Stress legs for guest-RAM regions (waycompose-design.md §108.11), run against
# a LOCAL Windows test VM only (winvm instance, default "wcram"), never a real
# guest. The VM runs the keeper as a SYSTEM task; the host runs
# `wc-arena-host ram-peer` against the VM's bootstrap file and RAM memfd.
# Background instruments in the guest (canary, jitter) are SYSTEM tasks too.
#
#   wc-guestram-stress.sh gib                  GiB regions, three sessions, alloc/free/realloc
#   wc-guestram-stress.sh pressure [SECS]      the same while a hog keeps Windows short of memory
#   wc-guestram-stress.sh crash [N]            kill the provider N times while the host writes
#   wc-guestram-stress.sh sessions [N]         N sessions at once (default 16)
#   wc-guestram-stress.sh long [SECS]          everything, mixed, for SECS (default 3900)
#
# Every leg prints PASS/FAIL lines; logs go to $WC_EVIDENCE (default
# /mnt/datasets/builds/evidence/windows-guest-ram). The harness talks to the
# guest over winvm's management ssh; the product path (keeper, provider, table,
# doorbell) uses none.
set -uo pipefail
W=${WINVM:-$HOME/winvm/bin/winvm}
VM=${WC_RAM_VM:-wcram}
EV=${WC_EVIDENCE:-/mnt/datasets/builds/evidence/windows-guest-ram}
BELL=org.sirati.svbell.0
EXE='C:\wcram\sv-guest.exe'
mkdir -p "$EV"
stamp() { date +%H%M%S; }
gs() { "$W" ssh "$VM" "$@" 2>&1 | tr -d '\r'; }
pass() { echo "PASS  $*"; }
fail() { echo "FAIL  $*"; FAILED=1; }
FAILED=0

keeper_held() { gs "$EXE ram stats" | awk '/^[0-9]/{print $1; exit}'; }
canary_bad() { gs 'Get-Content C:\wcram\canary.log' | grep -c CORRUPTION; }
jitter_worst() { gs 'Get-Content C:\wcram\jitter.log' | awk '/STALL/{v=$4+0; if (v>m) m=v} END{print m+0}'; }
health() {
  local t0 t1; t0=$(date +%s%N)
  if [ "$(gs 'echo ok')" = ok ]; then t1=$(date +%s%N); echo "ok $(( (t1 - t0) / 1000000 )) ms"; else echo DOWN; fi
}

run_test() { # LOG ARGS... : a provider in the foreground
  local log=$1; shift
  timeout 7200 "$W" ssh "$VM" "$EXE ram test --bell $BELL $*" > "$log" 2>&1
}

check_test() { # LOG NAME
  local log=$1 name=$2
  if grep -q 'DONE' "$log" && ! grep -qE 'errors=[1-9]|bad words [1-9]|still counters [1-9]' "$log"; then
    pass "$name: $(grep DONE "$log" | sed 's/.*DONE //')"
  else
    fail "$name: see $log"; grep -E 'errors=|bad words [1-9]|still counters [1-9]|failed' "$log" | tail -5
  fi
}

after() { # NAME: the invariants every leg ends with
  local name=$1 held bad
  held=$(keeper_held); bad=$(canary_bad)
  [ "${held:-x}" = 0 ] && pass "$name: the keeper holds nothing" || fail "$name: the keeper still holds ${held:-?} region(s)"
  [ "${bad:-1}" = 0 ] && pass "$name: canary clean" || fail "$name: canary saw $bad corruption line(s)"
  echo "      guest $(health), worst sleep(1 ms) so far $(jitter_worst) ms"
}

leg_gib() {
  local log=$EV/gib-$(stamp).log
  run_test "$log" --sessions 3 --cycles 8 --sizes 256M,1G,2G --hold-ms 1000
  check_test "$log" gib; after gib
}

leg_pressure() {
  local secs=${1:-300} log=$EV/pressure-$(stamp).log
  gs "schtasks /create /tn wcram-hog /tr 'cmd /c $EXE ram hog --mib 2048 --secs $secs > C:\\wcram\\hog.log 2>&1' /sc once /st 00:00 /ru SYSTEM /f; schtasks /run /tn wcram-hog" >/dev/null
  run_test "$log" --sessions 3 --secs "$secs" --sizes 256M,512M,1G --hold-ms 2000
  check_test "$log" pressure
  echo "      hog: $(gs 'Get-Content C:\wcram\hog.log -Tail 1')"
  echo "      grows refused for the budget: $(grep -c 'failed: budget' "$log"), for no large pages (1450): $(grep -c 'errno 1450' "$log")"
  after pressure
}

leg_crash() {
  local n=${1:-5} i log pid
  for i in $(seq "$n"); do
    log=$EV/crash-$i-$(stamp).log
    "$W" ssh "$VM" "$EXE ram test --bell $BELL --sessions 2 --cycles 0 --sizes 256M,512M --hold-ms 4000 --pid-file C:\\wcram\\prov.pid" > "$log" 2>&1 &
    local sshpid=$!
    sleep $((4 + RANDOM % 6))
    pid=$(gs 'Get-Content C:\wcram\prov.pid')
    gs "taskkill /F /PID $pid" > /dev/null
    wait $sshpid 2>/dev/null
    local orphans; orphans=$(gs "$EXE ram stats" | awk '/^[0-9]/{print $3}')
    echo "      crash $i: provider $pid killed; the keeper holds ${orphans:-?} orphan(s); the host keeps writing into them for 5 s"
    sleep 5
    # The next provider of the pool adopts the orphans; with no sessions it
    # only waits for the host to let go, frees them, and exits.
    run_test "$log.adopt" --sessions 0
    if grep -q 'orphan(s) adopted' "$log.adopt" && grep -q DONE "$log.adopt"; then
      pass "crash $i: $(grep -o '[0-9]* orphan(s) adopted' "$log.adopt"), freed after the host's UNMAPPED"
    else
      fail "crash $i: adoption, see $log.adopt"
    fi
    after "crash $i"
  done
}

leg_sessions() {
  local n=${1:-16} log=$EV/sessions-$(stamp).log
  run_test "$log" --sessions "$n" --cycles 6 --sizes 64M,32M,128M,96M --hold-ms 500
  check_test "$log" "sessions x$n"; after "sessions x$n"
}

leg_long() {
  local secs=${1:-3900} t_end log i=0
  t_end=$(( $(date +%s) + secs ))
  echo "      long run until $(date -d @$t_end +%T)"
  while [ "$(date +%s)" -lt "$t_end" ]; do
    i=$((i + 1))
    case $((i % 4)) in
      1) leg_gib ;;
      2) leg_sessions 8 ;;
      3) leg_crash 1 ;;
      0) leg_pressure 240 ;;
    esac
  done
}

case ${1:-} in
  gib) leg_gib ;;
  pressure) leg_pressure "${2:-300}" ;;
  crash) leg_crash "${2:-5}" ;;
  sessions) leg_sessions "${2:-16}" ;;
  long) leg_long "${2:-3900}" ;;
  *) sed -n '2,20p' "$0"; exit 2 ;;
esac
[ $FAILED = 0 ] && echo "wc-guestram-stress: OK" || { echo "wc-guestram-stress: FAILED"; exit 1; }
