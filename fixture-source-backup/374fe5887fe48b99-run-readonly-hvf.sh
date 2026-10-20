#!/usr/bin/env bash
set -euo pipefail
remote=testhost.example
ssh_options=(-o BatchMode=yes -o ConnectTimeout=10 -o ConnectionAttempts=1 -o IdentityAgent=~/.1password/agent.sock)
evidence=/mnt/datasets/builds/evidence/hyperlinux-map-guards
fixture=$(ssh "${ssh_options[@]}" "$remote" 'mktemp -d /tmp/hla.XXXXXX')
[[ $fixture =~ ^/tmp/hla\.[a-zA-Z0-9]{6}$ ]]
printf '%s\n' "$fixture" > "$evidence/readonly-hvf-fixture.txt"
cleanup() {
 ssh "${ssh_options[@]}" "$remote" "rm -f '$fixture/readonly-hvf-check'; rmdir '$fixture'; test ! -e '$fixture'"
}
trap cleanup EXIT
scp "${ssh_options[@]}" "$evidence/readonly-hvf-check" "$remote:$fixture/readonly-hvf-check"
ssh "${ssh_options[@]}" "$remote" "test \"\$(shasum -a 256 '$fixture/readonly-hvf-check' | cut -d' ' -f1)\" = f7e6525a6ee3d7679d7fb23109148e97630b4e3680d5903430fdf85cc9b5f1e3 && codesign --verify --strict '$fixture/readonly-hvf-check' && ulimit -c 0 && /usr/bin/perl -e 'alarm 20; exec @ARGV' '$fixture/readonly-hvf-check'"
