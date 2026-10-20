#!/usr/bin/env bash
set -euo pipefail
remote=testhost.example
ssh_options=(-o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=1 -o IdentityAgent=~/.1password/agent.sock)
evidence=/mnt/datasets/builds/evidence/hyperlinux-map-guards
exec 9>/mnt/datasets/builds/wcrun/neo-runtime.lock
flock 9
fixture=$(ssh "${ssh_options[@]}" "$remote" 'mktemp -d /tmp/hlrx.XXXXXX')
[[ $fixture =~ ^/tmp/hlrx\.[a-zA-Z0-9]{6}$ ]]
printf '%s\n' "$fixture" > "$evidence/readonly-hvf-rx-fixture.txt"
cleanup() {
 ssh "${ssh_options[@]}" "$remote" "rm -f '$fixture/readonly-hvf-rx-check'; rmdir '$fixture'; test ! -e '$fixture'"
}
trap cleanup EXIT
scp "${ssh_options[@]}" "$evidence/readonly-hvf-rx-check" "$remote:$fixture/readonly-hvf-rx-check"
ssh "${ssh_options[@]}" "$remote" "test \"\$(shasum -a 256 '$fixture/readonly-hvf-rx-check' | cut -d' ' -f1)\" = f8153830d50ac4dfa164a7de46f6b3f35b6770826eb1085d199a5ebd08806dcd && codesign --verify --strict '$fixture/readonly-hvf-rx-check' && ulimit -c 0 && /usr/bin/perl -e 'alarm 20; exec @ARGV' '$fixture/readonly-hvf-rx-check'"
