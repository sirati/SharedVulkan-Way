#!/usr/bin/env bash
set -euo pipefail
remote=testhost.example
ssh_options=(-o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=1 -o IdentityAgent=~/.1password/agent.sock)
evidence=/mnt/datasets/builds/evidence/hyperlinux-map-guards
export NIX_SSHOPTS='-o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=1 -o IdentityAgent=~/.1password/agent.sock'
nix copy --to "ssh://$remote" /nix/store/63kk0l28bmc638xz986kl4ffp0q1iqi4-hl-waycompose-arm64-apple-darwin-0.3.0-rc2 /nix/store/z8fx6mccmaxkbifld3p9bbddaphy8lsi-hyperlinux-probe-runner
ssh "${ssh_options[@]}" "$remote" 'codesign --verify --strict /nix/store/63kk0l28bmc638xz986kl4ffp0q1iqi4-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl'
fixture=$(ssh "${ssh_options[@]}" "$remote" 'mktemp -d /tmp/hlr.XXXXXX')
[[ $fixture =~ ^/tmp/hlr\.[a-zA-Z0-9]{6}$ ]]
printf '%s\n' "$fixture" > "$evidence/ro-trace-native-fixture.txt"
cleanup() {
 ssh "${ssh_options[@]}" "$remote" "rm -f '$fixture/shared-access-diagnostic'; rmdir '$fixture/home' '$fixture/tmp' '$fixture'; test ! -e '$fixture'"
}
trap cleanup EXIT
scp "${ssh_options[@]}" "$evidence/shared-access-diagnostic" "$remote:$fixture/shared-access-diagnostic"
set +e
ssh "${ssh_options[@]}" "$remote" "mkdir -p '$fixture/home' '$fixture/tmp' && /usr/bin/perl -e 'alarm 35; exec @ARGV' /usr/bin/env HL_WC_MAP_TRACE=1 HOME='$fixture/home' HL_NATIVE_FIXTURE='$fixture' /nix/store/z8fx6mccmaxkbifld3p9bbddaphy8lsi-hyperlinux-probe-runner '$fixture/shared-access-diagnostic'"
result=$?
set -e
printf '%s\n' "$result" > "$evidence/ro-trace-native-exit.txt"
exit "$result"
