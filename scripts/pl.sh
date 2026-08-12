#!/usr/bin/env bash
# testhost access helper. One place for the ssh incantations the machine needs,
# so that every call site is short.
#
#   pl.sh agent                 print the live ssh-agent socket
#   pl.sh host <cmd...>         run <cmd> on the NixOS host as sirati
#   pl.sh root <cmd...>         run <cmd> on the host as root, via ~/rootc
#   pl.sh guest <cmd...>        run <cmd> in the Windows guest (git-bash)
#   pl.sh push <file>...        scp files to the host's /tmp
#   pl.sh pull <remote> <local> scp a file back from the host
set -uo pipefail

AGENT_DIR=~/.ssh/agent
KEY=/tmp/sv-k1.pub

live_agent() {
  for s in "$AGENT_DIR"/s.*; do
    n=$(SSH_AUTH_SOCK=$s timeout 4 ssh-add -l 2>/dev/null | wc -l)
    if [ "$n" -gt 0 ]; then echo "$s"; return 0; fi
  done
  return 1
}

AG=${SV_AGENT:-$(live_agent)}
[ -n "$AG" ] || { echo "no live ssh agent socket" >&2; exit 1; }
export SSH_AUTH_SOCK="$AG"

HOST_OPTS=(-o IdentityAgent="$AG" -o HostKeyAlias=testhost.lan
           -o HostKeyAlgorithms=rsa-sha2-512,rsa-sha2-256 -o BatchMode=yes
           -o LogLevel=ERROR)
# The guest's sshd caps at six authentication attempts and the agent offers six
# keys, so an unpinned connection fails in a way that looks like a wrong key.
if [ ! -s "$KEY" ]; then ssh-add -L | head -1 > "$KEY"; fi
GUEST_OPTS=(-o IdentityAgent="$AG" -o IdentitiesOnly=yes -i "$KEY"
            -o BatchMode=yes -o LogLevel=ERROR)

HOST=<host-user>@<host-ip>
GUEST=<guest-user>@<guest-ip>

cmd=${1:?usage: pl.sh agent|host|root|guest|push|pull ...}; shift

case "$cmd" in
  agent) echo "$AG" ;;
  host)  ssh "${HOST_OPTS[@]}" "$HOST" "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; $*" ;;
  # Root is reached by piping into ~/rootc, and the PATH export has to be the
  # first line of the piped script because that shell starts from nothing.
  root)  ssh "${HOST_OPTS[@]}" "$HOST" "export PATH=/run/current-system/sw/bin:/run/wrappers/bin; { echo 'export PATH=/run/current-system/sw/bin:/run/wrappers/bin'; cat; } <<'SVEOF' | ~/rootc
$*
SVEOF" ;;
  guest) ssh "${GUEST_OPTS[@]}" "$GUEST" "export MSYS_NO_PATHCONV=1; $*" ;;
  # `rootc` is only good for *short* root commands, and the failure is silent.
  # It pipes the script into socat; once the script has been sent socat sees EOF
  # on its input and closes the connection after its default half-second, which
  # SIGHUPs the root shell and everything under it. Measured here:
  #
  #     pl.sh root 'date; sleep 3; date; echo survived'
  #     -> prints the first date and nothing else. No error, no exit code.
  #
  # So anything slower than about half a second — the ~2 s arena scan, a build,
  # a Vulkan run — is killed part-way and reports success by saying nothing.
  # `rootlong` is the pattern that works: detach with nohup, redirect to a file,
  # poll for a completion marker, then fetch the output in a second short call.
  rootlong)
    tag=$1; shift
    "$0" root "rm -f /tmp/sv-$tag.log /tmp/sv-$tag.rc; nohup setsid bash -c '{ $*; } > /tmp/sv-$tag.log 2>&1; echo \$? > /tmp/sv-$tag.rc' > /dev/null 2>&1 < /dev/null &" >/dev/null
    for _ in $(seq 1 600); do
      rc=$("$0" root "cat /tmp/sv-$tag.rc 2>/dev/null" | tr -d '\r' | head -1)
      [ -n "$rc" ] && break
      sleep 1
    done
    "$0" root "cat /tmp/sv-$tag.log"
    exit "${rc:-1}" ;;
  push)  scp "${HOST_OPTS[@]}" "$@" "$HOST:/tmp/" ;;
  pull)  scp "${HOST_OPTS[@]}" "$HOST:$1" "$2" ;;
  *) echo "unknown subcommand $cmd" >&2; exit 2 ;;
esac
