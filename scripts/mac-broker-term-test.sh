#!/usr/bin/env bash
# Stub regressions for the management-only shutdown observer. No VM is used.
set -euo pipefail
source "$(dirname "$0")/lib/mac-broker-term.sh"
run_case() (
  local scenario=$1 expected=$2
  # Command substitution runs in subshells; derive transitions from the clock.
  term_now() { echo "$clock"; }
  clock=0
  sleep() { clock=$((clock + 1000)); }
  vm() {
    case $* in
      'sudo -n kill -TERM 100')
        [ "$scenario" = late ] && clock=13000
        return 0 ;;
      *'launchctl print'*)
        case $scenario in
          query_error) return 255 ;;
          unchanged) echo 100 ;;
          invalid) echo bad ;;
          *) echo 200 ;;
        esac ;;
      'sudo -n /bin/ps -axo pid=')
        case $scenario in
          alive) printf '1\n100\n' ;;
          error) return 255 ;;
          sudo_error) return 1 ;;
          malformed) echo unexpected ;;
          dead_replacement) echo 1 ;;
          *) printf '1\n200\n' ;;
        esac ;;
      *) echo "unexpected command: $*" >&2; return 99 ;;
    esac
  }
  local out result=0
  out=$(term_wait_restart 100) || result=$?
  if [ "$expected" = pass ]; then
    [ "$result" = 0 ] && [[ $out = *'relaunched by launchd: pid 200'* ]] || { echo "$out"; return 1; }
  else
    [ "$result" != 0 ] && [[ $out = *"$expected"* ]] || { echo "$out"; return 1; }
  fi
  echo "PASS: $scenario"
)
run_case alive 'shutdown plus 2 s observation margin'
run_case success pass
run_case error 'liveness probe failed'
run_case sudo_error 'liveness probe failed'
run_case query_error 'launchd query failed'
run_case late 'shutdown plus 2 s observation margin'
run_case malformed 'liveness probe failed'
run_case unchanged 'did not report a replacement'
run_case invalid 'invalid replacement'
run_case dead_replacement 'did not report a replacement'
