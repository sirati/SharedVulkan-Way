#!/usr/bin/env bash
# §111.10: scripts/wc-users-migrate.sh install-staged, in a dry tree (ROOT=),
# as wc-launch.sh deploy's system layout uses it. No root, no unit touched.
#
#   a staged installation goes into ROOT/var/lib/waycompose/installs/NAME,
#   apps.ini's stage paths become the installation's and the admin's ~/;
#   its user is the one users.conf maps it to (wctest for a test install);
#   a second deploy keeps the installed key and drops the stage's copy;
#   an installation users.conf does not name is refused.
set -uo pipefail
ROOT_DIR=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d /mnt/datasets/builds/wcrun/wc-users-migrate-test-XXXXXX)
FAIL=0
ok() { echo "ok: $*"; }
bad() { echo "FAIL: $*"; FAIL=1; }

mkdir -p "$T/etc/waycompose" "$T/home/admin" "$T/home/wctest" "$T/home/desk"
cat >"$T/etc/waycompose/users.conf" <<'EOF'
# SERVER SERVER-USER LINUX-USER [installs=a,b] [id=ID]
vm guest desk installs=wcivs,wcmc
vm guest wctest installs=wct1,wct2
EOF
stage() { # NAME KEY
  local s="$T/home/admin/.cache/waycompose-stage/$1"
  mkdir -p "$s/bin"
  printf '#!/bin/sh\necho apps ok\n' >"$s/wc-session"
  printf 'x' >"$s/bin/wc-proxy"
  printf '%s' "$2" >"$s/session.key"
  cat >"$s/apps.ini" <<EOF
[firefox]
exec     = /home/admin/.cache/waycompose-stage/$1/bin/app-firefox/bin/firefox
profile  = /home/admin/.wc-launch/firefox-$1
EOF
  printf 'VK_ICD_FILENAMES=/home/admin/.cache/waycompose-stage/%s/bin/sv_icd.json\n' "$1" >"$s/gpu.env"
  echo "$s"
}
run() { ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" install-staged admin auto "$@" 2>&1; }

S=$(stage wct1 "$(printf 'a%.0s' $(seq 64))")
out=$(run "$S" wct1)
echo "$out" | sed 's/^/    /'
I="$T/var/lib/waycompose/installs/wct1"
echo "$out" | grep -q "its sessions as wctest" && ok "a test installation runs as wctest" || bad "the mapped user: $out"
[ -f "$I/wc-session" ] && ok "installed into $I" || bad "not installed"
grep -q "^exec     = /var/lib/waycompose/installs/wct1/bin/app-firefox/bin/firefox$" "$I/apps.ini" && ok "apps.ini names the installation's own paths" || bad "apps.ini: $(cat "$I/apps.ini")"
grep -q "^profile  = ~/.wc-launch/firefox-wct1$" "$I/apps.ini" && ok "the profile is the mapped user's ~/" || bad "profile: $(grep profile "$I/apps.ini")"
grep -q "/var/lib/waycompose/installs/wct1/bin/sv_icd.json" "$I/gpu.env" && ok "gpu.env follows" || bad "gpu.env: $(cat "$I/gpu.env")"
[ ! -e "$S/session.key" ] && ok "the stage's key is gone" || bad "the stage still has its key"
[ "$(stat -c %a "$I/session.key")" = 600 ] && ok "the key is 0600" || bad "key mode $(stat -c %a "$I/session.key")"

# A second deploy: a new stage with another key; the installed key stays.
S=$(stage wct1 "$(printf 'b%.0s' $(seq 64))")
run "$S" wct1 >/dev/null
[ "$(cat "$I/session.key")" = "$(printf 'a%.0s' $(seq 64))" ] && ok "a redeploy keeps the installed key" || bad "the key changed"
[ ! -e "$I.new" ] && [ ! -e "$I.old" ] && ok "no leftovers" || bad "leftovers next to $I"

S=$(stage wcivs "$(printf 'c%.0s' $(seq 64))")
out=$(run "$S" wcivs); echo "$out" | grep -q "its sessions as desk" && ok "a user installation runs as its mapped user" || bad "wcivs's user"

S=$(stage nobody "$(printf 'd%.0s' $(seq 64))")
out=$(run "$S" nobody); rc=$?
[ "$rc" != 0 ] && echo "$out" | grep -q "maps no user for nobody" && ok "an unmapped installation is refused" || bad "unmapped: rc=$rc $out"
[ ! -e "$T/var/lib/waycompose/installs/nobody" ] && ok "and nothing of it is installed" || bad "it was installed"

out=$(ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" install-staged admin auto "$T/elsewhere" wct2 2>&1); rc=$?
[ "$rc" != 0 ] && ok "a stage that is not one is refused" || bad "a bad stage: $out"

# wc-launch.sh deploy's guard: its two functions, with the host's commands run
# here against this tree.
guard() { # LAYOUT SESSION -> output, rc
  ( eval "$(sed -n '/^host_system_services() {/,/^}/p; /^host_layout_check() {/,/^}/p' "$ROOT_DIR/scripts/wc-launch.sh")"
    h() { local c=${1//\/etc\/waycompose/$T\/etc\/waycompose}; c=${c//systemctl cat wc-sessiond.service/false}; bash -c "$c"; }
    LAYOUT=$1 SESSION=$2 HOST=testhost HOST_ROOT=true DEBUG_SSH=0 HOSTDIR=/stage SYSTEM_BASE=/var/lib/waycompose/installs
    host_layout_check ) 2>&1
}
out=$(guard user wct1); rc=$?
[ "$rc" = 2 ] && echo "$out" | grep -q "runs waycompose as system services" && ok "an old-form deploy refuses on a system-services host" || bad "old form: rc=$rc $out"
out=$(guard system wct1); rc=$?
[ "$rc" = 0 ] && echo "$out" | grep -q "run as guest -> wctest" && ok "the system layout names the mapped user ($out)" || bad "system layout: rc=$rc $out"
out=$(guard system nobody); rc=$?
[ "$rc" = 2 ] && echo "$out" | grep -q "maps no user for installation nobody" && ok "the system layout refuses an unmapped installation" || bad "unmapped deploy: rc=$rc $out"
mv "$T/etc/waycompose/users.conf" "$T/etc/waycompose/users.conf.off"
out=$(guard user wct1); rc=$?
[ "$rc" = 0 ] && ok "the old form still works on a host without the system services" || bad "plain host: rc=$rc $out"
out=$(guard system wct1); rc=$?
[ "$rc" = 2 ] && ok "the system layout refuses a host without the system services" || bad "system on a plain host: rc=$rc"

# deploy's scope: shared state only by name, never from a test installation.
scope() { # HOSTDIR-HAS-TEST-HOME(0/1) ENV...
  local th=$1; shift
  ( eval "$(sed -n '/^deploy_scope_check() {/,/^}/p' "$ROOT_DIR/scripts/wc-launch.sh")"
    h() { [ "$th" = 1 ]; }
    HOSTDIR=/stage BROKER=skip PORT_ACCESS=skip
    for a in "$@"; do eval "$a"; done
    deploy_scope_check ) 2>&1
}
out=$(scope 0); rc=$?
[ "$rc" = 0 ] && echo "$out" | grep -q "this installation only" && ok "the defaults change nothing shared" || bad "defaults: rc=$rc $out"
out=$(scope 0 BROKER=install); rc=$?
[ "$rc" = 0 ] && echo "$out" | grep -q "WC_LAUNCH_BROKER" && ok "a user installation may ask for the broker by name" || bad "broker by name: rc=$rc $out"
out=$(scope 1 BROKER=install); rc=$?
[ "$rc" = 2 ] && echo "$out" | grep -q "test installation" && ok "a test installation (test-home) is refused the broker" || bad "test + broker: rc=$rc $out"
out=$(scope 0 WC_LAUNCH_TEST_HOME=/x PORT_ACCESS=install); rc=$?
[ "$rc" = 2 ] && ok "a test installation (WC_LAUNCH_TEST_HOME) is refused the port grant" || bad "test + port access: rc=$rc"
out=$(scope 1 WC_LAUNCH_SERVICES=install); rc=$?
[ "$rc" = 2 ] && ok "a test installation is refused the host's services" || bad "test + services: rc=$rc"
out=$(scope 0 BROKER=yes); rc=$?
[ "$rc" = 2 ] && ok "an unknown value is refused" || bad "unknown value: rc=$rc"

rm -rf "$T"
[ "$FAIL" = 0 ] && { echo "wc-users-migrate-test: OK"; exit 0; }
echo "wc-users-migrate-test: FAILED"; exit 1
