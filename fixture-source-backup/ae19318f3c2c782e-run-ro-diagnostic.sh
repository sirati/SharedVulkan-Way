#!/usr/bin/env bash
set -euo pipefail
remote=testhost.example
ssh_options=(-o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=1 -o IdentityAgent=~/.1password/agent.sock)
evidence=/mnt/datasets/builds/evidence/hyperlinux-map-guards
fixture=$(ssh "${ssh_options[@]}" "$remote" 'mktemp -d /tmp/hlr.XXXXXX')
[[ $fixture =~ ^/tmp/hlr\.[a-zA-Z0-9]{6}$ ]]
printf '%s\n' "$fixture" > "$evidence/ro-native-fixture.txt"
cleanup() {
 ssh "${ssh_options[@]}" "$remote" "rm -f '$fixture/shared-access-diagnostic'; rmdir '$fixture/home' '$fixture/tmp' '$fixture'; test ! -e '$fixture'"
}
trap cleanup EXIT
scp "${ssh_options[@]}" "$evidence/shared-access-diagnostic" "$remote:$fixture/shared-access-diagnostic"
set +e
ssh "${ssh_options[@]}" "$remote" "mkdir -p '$fixture/home' '$fixture/tmp' && /usr/bin/perl -e 'alarm 35; exec @ARGV' /usr/bin/env HOME='$fixture/home' HL_NATIVE_FIXTURE='$fixture' /nix/store/1r34s0vjia847g1wzx5yh4n0pscvfrjz-hyperlinux-probe-runner '$fixture/shared-access-diagnostic'"
result=$?
set -e
printf '%s\n' "$result" > "$evidence/ro-native-exit.txt"
exit "$result"
