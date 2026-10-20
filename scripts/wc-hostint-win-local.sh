#!/usr/bin/env bash
#
# §106 on the Windows session path, on one machine (from wc-no-ssh.sh's boot
# and requests leg): the Linux build of the compositor stands in for the
# guest, with --boot-* and the --hostint flags wc-launch.exe passes, a file
# stands where the ivshmem device would be, and the VM host's half is the
# product's own (sv-hostd bell-hub, wc-arena-host sessiond --ivshmem,
# scripts/wc-session). The compositor's host-integration opener is the test
# fixture's (`hostint::start_logging`): it logs what Windows would open, after
# the real `Service` has checked it.
#
# Checks: the proxy claims OWNER_HOSTINT and the compositor opens it; the
# session's xdg-data/xdg-config are written; in an application's own
# environment, `wc-host-open` opens a URL on the host (logged, `rep ok`, exit
# 0), refuses file:///etc/passwd locally (exit 2), gets "no such host
# application" for an unknown token, and sends a file in the share as a host
# path; the control `open` of a file:/// URL in the share reaches the
# application's %u and %f, and one outside the share is refused; no ssh.
#
#   scripts/wc-hostint-win-local.sh
#
# Run inside `nix develop` after
# `cargo build -p wc-compositor -p wc-proxy -p wc-arena-host -p sv-hostd -p wc-launch`.
# Evidence: $WC_EVIDENCE (default /mnt/datasets/builds/evidence/host-integration/win-local).
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${CARGO_TARGET_DIR:-$ROOT/target}/debug
SOCAT=$(command -v socat || ls /nix/store/*-socat-1.*/bin/socat 2>/dev/null | head -1)
EVID=${WC_EVIDENCE:-/mnt/datasets/builds/evidence/host-integration/win-local}
D=$(mktemp -d /mnt/datasets/builds/wcrun/wc-hi-win-XXXXXX)
STAMP=$(date +%Y%m%d-%H%M%S)
mkdir -p "$EVID"
exec > >(tee "$EVID/run-$STAMP.log") 2>&1
FAIL=0
PIDS=()

say() { echo "[$(date +%H:%M:%S)] $*"; }
bad() { echo "FAIL: $*"; FAIL=1; }
ok() { echo "ok: $*"; }
cleanup() {
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  for u in $(systemctl --user list-units --plain --no-legend 'wc-launch-*' 2>/dev/null | awk '{print $1}' | grep -E "wchi[0-9]*-$$-"); do
    systemctl --user stop "$u" 2>/dev/null
  done
  pkill -f "UNIX-LISTEN:$D/host.sock" 2>/dev/null
  pkill -f "UNIX-LISTEN:$D/guest.sock" 2>/dev/null
  pkill -f "tail -F $D/run" 2>/dev/null
  wait 2>/dev/null
  mkdir -p "$EVID/run-$STAMP.files"
  cp "$D"/*.log "$EVID/run-$STAMP.files/" 2>/dev/null
  cp -r "$D"/xdg-copy "$EVID/run-$STAMP.files/" 2>/dev/null
  [ "$FAIL" = 0 ] && rm -rf "$D"
}
trap cleanup EXIT

echo "wc-hostint-win-local $STAMP, tree $(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo '+dirty')"
[ -x "$SOCAT" ] || { echo "no socat"; exit 2; }
for b in wc-compositor wc-proxy wc-arena-host sv-hostd wc-launch wc-host-open; do
  [ -x "$BIN/$b" ] || { echo "no $BIN/$b: build it first"; exit 2; }
done

# The share, as the guest's Z:\ would be, and its drop directory.
SHARE=$D/share
mkdir -p "$SHARE/waycompose-open" "$D/testhome"
echo "<p>shared</p>" >"$SHARE/x.html"
echo "notes" >"$SHARE/doc.txt"

# Two applications that only record what they were started with: one takes
# %u, one %f. Each writes its argv and its environment.
APPD=$D/app
mkdir -p "$APPD/bin" "$APPD/share/applications"
for k in u f; do
  printf '#!/bin/sh\nprintf "%%s\\n" "$@" >%q/argv-%s.$$\nenv >%q/env-%s\n' "$D" "$k" "$D" "$k" >"$APPD/bin/argv$k"
  chmod +x "$APPD/bin/argv$k"
  printf '[Desktop Entry]\nType=Application\nName=Argv %s\nExec=argv%s %%%s\nMimeType=text/html;text/plain;\n' "$k" "$k" "$k" \
    >"$APPD/share/applications/argv$k.desktop"
done

# The installation, as deploy makes it.
INST=$D/inst
mkdir -p "$INST/bin" "$D/installs" "$D/state" "$D/run" "$D/crt" "$D/hub"
chmod 700 "$D/crt" "$D/hub"
cp "$ROOT/scripts/wc-session" "$INST/wc-session"
for b in wc-proxy wc-arena-host wc-launch sv-hostd wc-host-open; do ln -s "$BIN/$b" "$INST/bin/$b"; done
cat >"$INST/apps.ini" <<EOF
[argvu]
exec = $APPD/bin/argvu
titlebar = none
desktop = argvu
gpu = off

[argvf]
exec = $APPD/bin/argvf
titlebar = none
desktop = argvf
gpu = off
EOF
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' >"$INST/session.key"
chmod 600 "$INST/session.key"
cp "$INST/session.key" "$D/guest.key"
ln -s "$INST" "$D/installs/wchi"

export WC_LAUNCH_RUN="$D/run" WC_LAUNCH_ARENA="$D/arena" WC_LAUNCH_WCBELL="$D/host.sock"
export WC_LAUNCH_BELL_HUB="$D/hub" WC_LAUNCH_HUB_UNMANAGED=1 WC_LAUNCH_SESSION_SECS=120
export WC_LAUNCH_AUDIO=off WC_LAUNCH_TEST_HOME="$D/testhome"

rm -f "$D/guest.sock" "$D/host.sock"
printf '#!/usr/bin/env bash\nwhile true; do %q - UNIX-LISTEN:%q,unlink-early 2>/dev/null; done\n' "$SOCAT" "$D/host.sock" >"$D/qemu-host-end.sh"
chmod +x "$D/qemu-host-end.sh"
"$SOCAT" UNIX-LISTEN:"$D/guest.sock" EXEC:"$D/qemu-host-end.sh" &
PIDS+=("$!")
for _ in $(seq 1 50); do [ -S "$D/guest.sock" ] && break; sleep 0.1; done
"$BIN/sv-hostd" bell-hub --port "$D/host.sock=$D/hub/wl.sock" >>"$D/hub.log" 2>&1 &
HUB=$!; PIDS+=("$HUB")
for _ in $(seq 1 50); do [ -S "$D/hub/wl.sock" ] && break; sleep 0.1; done
"$BIN/wc-arena-host" sessiond --ivshmem "$D/arena" --installs "$D/installs" --state "$D/state" --live "$D/live" \
  --bell "$D/hub/wl.sock" --poll-ms 60000 >>"$D/sessiond.log" 2>&1 &
SVC=$!; PIDS+=("$SVC")

wait_line() { # FILE REGEX SECS
  local i; for i in $(seq 1 $(($3 * 10))); do grep -qE "$2" "$1" 2>/dev/null && return 0; sleep 0.1; done; return 1
}
request() { printf '%s\n' "$1" | timeout 30 "$SOCAT" -t 30 - UNIX-CONNECT:"$D/ctl.sock" 2>&1; }
b64() { printf '%s' "$1" | base64 -w0; }

say "the guest, with the --hostint flags wc-launch.exe passes ([hostint] share and drop)"
truncate -s $((128 << 20)) "$D/arena"
XDG_RUNTIME_DIR="$D/crt" WC_IVSHMEM_STANDIN="$D/arena" "$BIN/wc-compositor" \
  --arena-ivshmem 0 --arena-outbound 0:1048576 --wayland-rings "$D/guest.sock" \
  --socket "wchi$$" --in-band --timeout 120 \
  --boot-session "wchi-$$" --boot-install wchi --boot-key "$D/guest.key" \
  --control-socket "$D/ctl.sock" --session-audio 0 \
  --hostint --hostint-share "Z:\\=$SHARE" --hostint-drop "Z:\\waycompose-open=$SHARE/waycompose-open" \
  >"$D/comp.log" 2>&1 &
COMP=$!; PIDS+=("$COMP")
wait_line "$D/comp.log" 'boot: located' 30 && ok "booted: $(grep -m1 -o 'boot: located.*' "$D/comp.log" | cut -c1-80)" || { bad "no boot"; exit 1; }
wait_line "$D/comp.log" 'control socket' 20 || bad "no control socket"

R=$(request "raw run argvu 96 30")
case "$R" in ok-raw*'run accepted'*) ok "run argvu accepted" ;; *) bad "run: $R" ;; esac
SD="$D/run/wchi-$$"
PLOG="$SD/proxy.log"
wait_line "$PLOG" 'hostint: host says hello' 20 && ok "proxy: $(grep -m1 -o 'hostint: host says hello.*' "$PLOG")" || bad "the proxy heard no hostint hello: $(grep -i hostint "$PLOG" | tail -3)"
(tail -F "$PLOG" 2>/dev/null >"$D/proxy.log" &)
wait_line "$D/comp.log" 'hostint: on ring slot [0-9]+' 10 && ok "compositor: $(grep -m1 -o 'hostint: on ring slot [0-9]*.*' "$D/comp.log")" || bad "the compositor opened no hostint slot"
wait_line "$PLOG" 'hostint: 1 share' 10 && ok "proxy: $(grep -m1 -o 'hostint: 1 share.*' "$PLOG")" || bad "no share from the host"
grep -qx "$SHARE" "$SD/hostint/roots" 2>/dev/null && ok "roots: $SHARE" || bad "roots: $(cat "$SD/hostint/roots" 2>&1)"
wait_line "$PLOG" 'host application\(s\) as desktop entries' 10
if [ -d "$SD/hostint/xdg-data/applications" ] && [ -f "$SD/hostint/xdg-config/mimeapps.list" ]; then
  ok "xdg-data/applications and xdg-config/mimeapps.list written ($(ls "$SD/hostint/xdg-data/applications" | wc -l) entr(ies))"
  mkdir -p "$D/xdg-copy" && cp -r "$SD/hostint/xdg-data" "$SD/hostint/xdg-config" "$D/xdg-copy/"
else
  bad "no xdg-data/xdg-config under $SD/hostint"
fi

wait_line "$D/env-u" 'WC_HOSTINT_SOCK=' 10 || bad "argvu never ran (no env)"
ENVU=$D/env-u
envv() { sed -n "s/^$1=//p" "$ENVU" | head -1; }
SOCK=$(envv WC_HOSTINT_SOCK); OPENER=$(envv WC_HOST_OPEN); APATH=$(envv PATH)
[ -n "$SOCK" ] && ok "the application's WC_HOSTINT_SOCK=$SOCK" || bad "no WC_HOSTINT_SOCK in the application's environment"
[ "$(envv HOME)" = "$D/testhome/home" ] && ok "WC_LAUNCH_TEST_HOME: HOME=$(envv HOME)" || bad "HOME is $(envv HOME)"
grep -q "BROWSER=xdg-open" "$ENVU" && ok "BROWSER=xdg-open" || bad "no BROWSER"
XO=$(PATH="$APATH" command -v xdg-open)
grep -q 'wc-host-open' "$XO" 2>/dev/null && ok "xdg-open is the shim: $XO" || bad "xdg-open is $XO"
# Exactly the application's environment.
in_app() { env -i $(grep -E '^(PATH|HOME|WC_HOSTINT_SOCK|WC_HOST_OPEN|XDG_[A-Z_]*)=' "$ENVU" | tr '\n' ' ') "$@"; }

say "wc-host-open from the application's environment"
in_app xdg-open https://example.org >"$D/o1.txt" 2>&1; RC=$?
[ $RC = 0 ] && ok "xdg-open https://example.org: exit 0" || bad "xdg-open https: exit $RC: $(cat "$D/o1.txt")"
wait_line "$D/comp.log" 'hostint: would open url https://example.org/' 5 && ok "compositor: $(grep -m1 -o 'would open url https://example.org/.*' "$D/comp.log")" || bad "the compositor did not open it"
wait_line "$PLOG" 'hostint: rep [0-9]+: Ok' 5 && ok "proxy: $(grep -m1 -oE 'hostint: rep [0-9]+: Ok.*' "$PLOG")" || bad "no rep ok"

in_app "$OPENER" file:///etc/passwd >"$D/o2.txt" 2>&1; RC=$?
[ $RC = 2 ] && ok "file:///etc/passwd refused locally, exit 2: $(head -c 120 "$D/o2.txt")" || bad "file:///etc/passwd: exit $RC: $(cat "$D/o2.txt")"
grep -q 'etc/passwd' "$D/comp.log" && bad "/etc/passwd reached the host side"

in_app "$OPENER" --with zz https://x >"$D/o3.txt" 2>&1; RC=$?
grep -q 'no such host application' "$D/o3.txt" && [ $RC = 1 ] && ok "--with zz: exit 1, $(head -c 120 "$D/o3.txt")" || bad "--with zz: exit $RC: $(cat "$D/o3.txt")"

in_app "$OPENER" "$SHARE/doc.txt" >"$D/o4.txt" 2>&1; RC=$?
[ $RC = 0 ] && wait_line "$D/comp.log" 'would open path Z:\\doc.txt' 5 \
  && ok "a file in the share went as a host path: $(grep -m1 -o 'would open path .*' "$D/comp.log")" \
  || bad "share file: exit $RC $(cat "$D/o4.txt"); $(grep 'would open path' "$D/comp.log")"

say "the session's control open: a file in the share reaches %u and %f; one outside is refused"
URL="file://$SHARE/x.html"
for k in u f; do
  R=$(request "raw open argv$k $(b64 "$URL") 96 30")
  case "$R" in ok-raw*'open accepted'*) ;; *) bad "open argv$k: $R" ;; esac
done
sleep 3
GU=$(cat "$D"/argv-u.* 2>/dev/null | grep -F "$URL")
[ -n "$GU" ] && ok "argvu (%u) got $GU" || bad "argvu did not get the URL: $(cat "$D"/argv-u.* 2>/dev/null | tr '\n' ' ')"
GF=$(cat "$D"/argv-f.* 2>/dev/null | grep -Fx "$SHARE/x.html")
[ -n "$GF" ] && ok "argvf (%f) got the path $GF" || bad "argvf did not get the path: $(cat "$D"/argv-f.* 2>/dev/null | tr '\n' ' ')"
NU=$(ls "$D"/argv-u.* 2>/dev/null | wc -l)
R=$(request "raw open argvu $(b64 "file:///etc/hostname") 96 30")
sleep 3
if [ "$(ls "$D"/argv-u.* 2>/dev/null | wc -l)" = "$NU" ] && ! cat "$D"/argv-u.* | grep -q /etc/hostname; then
  ok "a file outside the share was not opened ($(echo "$R" | head -c 60); proxy: $(grep -o 'not in a folder shared with the host[^"]*' "$PLOG" | head -1 | cut -c1-80))"
else
  bad "a file outside the share reached the application"
fi

pgrep -x ssh -P "$SVC,$HUB,$COMP" >/dev/null && bad "a process of this run started ssh"
ps -o pid=,comm= --ppid "$SVC" | grep -qw ssh && bad "the service has an ssh child"
pgrep -af ssh | grep -F "$D" && bad "an ssh with this run's paths"
grep -q 'SSH FALLBACK' "$PLOG" "$D/comp.log" && bad "an SSH FALLBACK line appeared"
ok "no ssh process in this run"

R=$(request "down"); case "$R" in ok*) ok "down" ;; *) bad "down: $R" ;; esac
echo "logs: $D (copied to $EVID/run-$STAMP.files)"
[ "$FAIL" = 0 ] && { echo "wc-hostint-win-local: OK"; exit 0; }
echo "wc-hostint-win-local: FAILED"; exit 1
