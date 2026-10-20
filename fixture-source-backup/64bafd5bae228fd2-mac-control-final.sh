#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
hl=/nix/store/1va70zx270ygbbh642mhc00vpn586yyz-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
runner=/nix/store/gqml6waymgl56jmwfh4s4131rl9s915q-hyperlinux-probe-runner
control=/nix/store/jnh7bhvhbvycgqi7zfsf9mwv9w820m2l-hl-control-access-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-control-access-client
access=/nix/store/3wz8p3rg2nh6j3qgi5zissc199bsc38w-hl-shared-access-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-shared-access-client
/usr/bin/codesign --verify --strict "$hl"
# Read actual initialized primary size from verbose startup before computing
# its protected page-table boundary. No guessed hardware IPA width.
"$hl" --verbose --timeout 5 --audio-backend null --fs-mode=rooted --isolated --sysroot /nix/store/imbmjaqwlgy9pj80xbsfn017bb5wvhfc-hyperlinux-probe-sysroot --guest-home /home/user --guest-cwd /home/user --bind "$HOME:/home/user" --bind "$fixture:$fixture" --bind "$fixture/tmp:/tmp" --bind /nix/store:/nix/store:ro "$control" --syscalls 0x100000 > "$fixture/layout.log" 2>&1
cat "$fixture/layout.log"
pool=$(/usr/bin/python3 - "$fixture/layout.log" <<'PY'
import re,sys
s=open(sys.argv[1]).read();match=re.search(r'IPA size: \d+ bits \((\d+)GB primary\)',s);assert match,s
print(hex((int(match[1])<<30)-(16<<20)))
PY
)
tail=$(/usr/bin/python3 - "$pool" <<'PY'
import sys
print(hex(int(sys.argv[1],16)+(16<<20)-4096))
PY
)
run() {
 echo "BEGIN $*"
 set +e
 "$runner" "$@" > "$fixture/result.log" 2>&1
 status=$?
 set -e
 cat "$fixture/result.log"
 [[ "$status" == 0 ]] || return "$status"
 case "$*" in *--cpu-*) /usr/bin/grep -q CONTROL_GUEST_SIGSEGV "$fixture/result.log";; *--guest-*) /usr/bin/grep -Eq 'GUEST_SIGBUS|GUEST_SIGSEGV' "$fixture/result.log";; esac
 echo "END $* passed"
}
for address in 0x100000 0x3c0000 0x3ff000 "$pool" "$tail"; do
 for mode in --syscalls --cpu-read --cpu-write --cpu-exec; do run "$control" "$mode" "$address"; done
done
for address in 0xe000 0xf000; do run "$control" --syscalls "$address";run "$control" --cpu-write "$address";done
run "$control" --boundary "$pool"
for mode in --guest-readonly-write --guest-readonly-exec --guest-map-fault; do run "$access" "$mode";done
