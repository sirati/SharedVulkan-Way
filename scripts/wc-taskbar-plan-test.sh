#!/usr/bin/env bash
# Exercise deploy's real taskbar helper with a first-install guest stub.
set -euo pipefail
ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
eval "$(sed -n '/^taskbar_ids() {/,/^}/p' "$ROOT_DIR/scripts/wc-launch.sh")"
APPDATA_MSYS=/missing SESSION=fresh DIR_MSYS=/install HOSTDIR=/stage
WC_LAUNCH_TASKBAR_ID=auto WC_LAUNCH_TEST_HOME=
h() { return 1; }
note() { :; }
ok() { printf 'ok: %s\n' "$*"; }
bad() { printf 'bad: %s\n' "$*" >&2; exit 1; }
g() {
  case "$1" in
    *'cat /missing/wc-launch-fresh.log 2>/dev/null | wc -l'*) printf '0\r\n' ;;
    *'--taskbar-ids'*) : ;;
    'tail -n +1 '*) printf '[123] taskbar-id: fresh install own\n[124] taskbar-id: wrote ini\n' ;;
    *) printf 'unexpected guest command: %s\n' "$1" >&2; return 1 ;;
  esac
}
out=$(taskbar_ids)
[[ "$out" == *'ok: taskbar identities'* ]]
printf '%s\n' 'wc-taskbar-plan-test: OK'
