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
keyop() { ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" "$1" admin auto "$2" "$3"; }
provision() { keyop key-prepare "$1" "$2" && keyop key-confirm "$1" "$2"; }
run() { ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" install-staged admin auto "$@" 2>&1; }

# Current ownership and mode cannot establish where an unknown key came from.
mkdir -p "$T/var/lib/waycompose/key-trust/wct2"
chmod 0700 "$T/var/lib/waycompose/key-trust" "$T/var/lib/waycompose/key-trust/wct2"
printf '%064d' 0 >"$T/var/lib/waycompose/key-trust/wct2/session.key"
chmod 0600 "$T/var/lib/waycompose/key-trust/wct2/session.key"
out=$(keyop key-prepare unused wct2 2>&1); rc=$?
[ "$rc" != 0 ] && ok "unknown preseed with protected metadata is refused" || bad "unknown preseed adopted"
[ ! -e "$T/var/lib/waycompose/key-trust/wct2/generated-here" ] && ok "unknown key gets no generation provenance" || bad "unknown key relabelled"
rm -f "$T/var/lib/waycompose/key-trust/wct2/session.key"
printf '%s\n' root-generated-candidate-v1 >"$T/var/lib/waycompose/key-trust/wct2/generated-here"
printf '%s\n' root-generated-broker-confirmed-v1 >"$T/var/lib/waycompose/key-trust/wct2/broker-confirmed"
chmod 0600 "$T/var/lib/waycompose/key-trust/wct2/generated-here" "$T/var/lib/waycompose/key-trust/wct2/broker-confirmed"
out=$(keyop key-prepare unused wct2 2>&1); rc=$?
[ "$rc" != 0 ] && [ ! -e "$T/var/lib/waycompose/key-trust/wct2/session.key" ] && ok "lost candidate cannot inherit a stale broker confirmation" || bad "lost candidate silently regenerated"
rm -rf "$T/var/lib/waycompose/key-trust/wct2"

S=$(stage wct1 "$(printf 'a%.0s' $(seq 64))")
out=$(run "$S" wct1); rc=$?
[ "$rc" != 0 ] && ok "legacy key without protected provenance is refused" || bad "legacy key accepted"
mkdir -p "$T/home/admin/.config/waycompose/installs"
ln -s "$S" "$T/home/admin/.config/waycompose/installs/wct1"
keyop key-prepare "$S" wct1 || exit 1
[ ! -e "$S/session.key" ] && ok "retired legacy verifier is removed before broker provisioning" || bad "retired verifier survived"
cp "$T/var/lib/waycompose/key-trust/wct1/session.key" "$T/pending-key"
out=$(run "$S" wct1); rc=$?
[ "$rc" != 0 ] && ok "unconfirmed broker candidate is refused" || bad "unconfirmed candidate accepted"
keyop key-prepare "$S" wct1 || exit 1
cmp -s "$T/pending-key" "$T/var/lib/waycompose/key-trust/wct1/session.key" && ok "retry preserves protected candidate" || bad "retry replaced candidate"
keyop key-confirm "$S" wct1 || exit 1
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

cp "$I/session.key" "$T/expected-key"
[ "$(cat "$I/session.key")" != "$(printf 'a%.0s' $(seq 64))" ] && ok "formerly exposed key was rotated" || bad "legacy key retained"
chmod 0755 "$T/var/lib/waycompose/key-trust/wct1"
out=$(run "$S" wct1); rc=$?
[ "$rc" != 0 ] && ok "unprotected provenance directory is refused" || bad "unprotected provenance accepted"
chmod 0700 "$T/var/lib/waycompose/key-trust/wct1"
# A second deploy: a new stage with another key; the installed key stays.
S=$(stage wct1 "$(printf 'b%.0s' $(seq 64))")
run "$S" wct1 >/dev/null
cmp -s "$I/session.key" "$T/expected-key" && ok "a redeploy keeps the installed key" || bad "the key changed"
[ ! -e "$I.new" ] && [ ! -e "$I.old" ] && ok "no leftovers" || bad "leftovers next to $I"

S=$(stage wcivs "$(printf 'c%.0s' $(seq 64))")
provision "$S" wcivs || exit 1
out=$(run "$S" wcivs); echo "$out" | grep -q "its sessions as desk" && ok "a user installation runs as its mapped user" || bad "wcivs's user"

S=$(stage nobody "$(printf 'd%.0s' $(seq 64))")
out=$(run "$S" nobody); rc=$?
[ "$rc" != 0 ] && echo "$out" | grep -q "maps no user for nobody" && ok "an unmapped installation is refused" || bad "unmapped: rc=$rc $out"
[ ! -e "$T/var/lib/waycompose/installs/nobody" ] && ok "and nothing of it is installed" || bad "it was installed"

out=$(ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" install-staged admin auto "$T/elsewhere" wct2 2>&1); rc=$?
[ "$rc" != 0 ] && ok "a stage that is not one is refused" || bad "a bad stage: $out"

# Protected rollback restores the prior applications while keeping current trust.
printf '#!/bin/sh\nprintf "restored application ran with original config\\n"\n' >"$T/var/lib/waycompose/installs/wcivs/bin/fixture-app"
printf 'changed\n' >"$S/apps.ini"
run "$S" wcivs >/dev/null || exit 1
ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" rollback admin desk unused wcivs >/dev/null || exit 1
app_out=$(sh "$T/var/lib/waycompose/installs/wcivs/bin/fixture-app")
[ "$app_out" = "restored application ran with original config" ] && ok "protected rollback restores a runnable application" || bad "rollback application failed"
! grep -q '^changed' "$T/var/lib/waycompose/installs/wcivs/apps.ini" && ok "protected rollback restores application configuration" || bad "rollback configuration failed"
cmp -s "$T/var/lib/waycompose/installs/wcivs/session.key" "$T/var/lib/waycompose/key-trust/wcivs/session.key" && ok "protected rollback keeps the trusted verifier key" || bad "rollback restored old trust"
# The first migration must retain a working pre-candidate application, even
# when the new wc-session immediately fails. This never runs a desktop app.
S=$(stage wct2 "$(printf 'e%.0s' $(seq 64))")
ln -s "$S" "$T/home/admin/.config/waycompose/installs/wct2"
provision "$S" wct2 || exit 1
mkdir -p "$T/build-bin"
printf '#!/bin/sh\nexit 91\n' >"$T/build-bin/wc-session"
printf 'fixture\n' >"$T/build-bin/wc-arena-host"
printf 'fixture\n' >"$T/build-bin/sv-hostd"
ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" apply admin wctest "$T/build-bin" wct2 >"$T/first-apply.txt" 2>&1 || { cat "$T/first-apply.txt"; exit 1; }
bash "$T/var/lib/waycompose/installs/wct2/wc-session" apps >/dev/null 2>&1; rc=$?
[ "$rc" = 91 ] && ok "first-migration candidate failure is reproduced" || bad "candidate unexpectedly worked"
ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" rollback admin wctest unused wct2 >"$T/first-rollback.txt" 2>&1 || { cat "$T/first-rollback.txt"; exit 1; }
app_out=$(bash "$T/var/lib/waycompose/installs/wct2/wc-session" apps)
[ "$app_out" = "apps ok" ] && ok "first-migration rollback restores pre-candidate executable" || bad "first migration had no working rollback"
grep -q '/var/lib/waycompose/installs/wct2/bin/app-firefox' "$T/var/lib/waycompose/installs/wct2/apps.ini" && ok "first-migration rollback keeps protected system paths" || bad "rollback revived admin application paths"
cmp -s "$T/var/lib/waycompose/installs/wct2/session.key" "$T/var/lib/waycompose/key-trust/wct2/session.key" && ok "first-migration rollback keeps rotated trust" || bad "first migration restored exposed key"

# A shared verifier must not restart over another install's exposed key.
mkdir -p "$T/var/lib/waycompose/installs/legacy-unmigrated"
printf 'retired fixture\n' >"$T/var/lib/waycompose/installs/legacy-unmigrated/session.key"
out=$(ROOT="$T" bash "$ROOT_DIR/scripts/wc-users-migrate.sh" services-restart admin desk unused wcivs 2>&1); rc=$?
[ "$rc" != 0 ] && ok "shared verifier restart refuses any unmigrated installation" || bad "shared verifier accepted untrusted install"
rm -rf "$T/var/lib/waycompose/installs/legacy-unmigrated"
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

out=$(scope 1 BROKER=key); rc=$?
[ "$rc" = 0 ] && ok "a test may provision its own broker key without installing the service" || bad "test key: rc=$rc $out"
out=$(scope 0 PORT_ACCESS=key); rc=$?
[ "$rc" = 2 ] && ok "key mode is refused for other shared settings" || bad "port key: rc=$rc $out"

broker_probe() { # MODE SERVICE-STATE [LAYOUT]
  ( eval "$(sed -n '/^broker_deploy_check() {/,/^}/p' "$ROOT_DIR/scripts/wc-launch.sh")"
    BROKER=$1 LAYOUT=${3:-user}
    # The fake guest returns the service state, never opens a session.
    state_reply=$2
    g() { printf '%s\n' "$state_reply"; }
    broker_deploy_check ) 2>&1
}
for state in Running Stopped; do
  out=$(broker_probe skip "$state"); rc=$?
  [ "$rc" = 2 ] && echo "$out" | grep -q 'keep this installation.*key privileged' && ok "skip refuses an existing $state broker before leaking its key" || bad "skip $state: rc=$rc $out"
done
out=$(broker_probe key Running system); rc=$?
[ "$rc" = 0 ] && ok "key-only deploy accepts a running broker" || bad "key Running: rc=$rc $out"
for state in absent Stopped broken; do
  out=$(broker_probe key "$state" system); rc=$?
  [ "$rc" = 2 ] && ok "key-only deploy refuses broker state $state" || bad "key $state: rc=$rc $out"
done
out=$(broker_probe skip absent system); rc=$?
[ "$rc" = 2 ] && ok "the system layout never recreates a desktop-readable key, even if its broker is absent" || bad "system skip absent: rc=$rc $out"
out=$(broker_probe key Running system); rc=$?
[ "$rc" = 0 ] && ok "the system layout accepts key mode with the running broker" || bad "system key: rc=$rc $out"
out=$(broker_probe install absent user); rc=$?
[ "$rc" = 2 ] && ok "legacy user layout refuses privileged broker provisioning in preflight" || bad "legacy privileged provisioning accepted"
out=$(broker_probe skip absent); rc=$?
[ "$rc" = 0 ] && ok "pre-broker skip still works" || bad "skip absent: rc=$rc $out"

rm -rf "$T"
[ "$FAIL" = 0 ] && { echo "wc-users-migrate-test: OK"; exit 0; }
echo "wc-users-migrate-test: FAILED"; exit 1
