#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
runner=/nix/store/5238ihxsxqapz5aqzb9q9c3l5xr66l2a-hyperlinux-probe-runner
client=/nix/store/s1jznxziy7rbxh7jnxi2babhvls11n9r-hl-control-lifecycle-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-control-lifecycle-client
/usr/bin/codesign --verify --strict /nix/store/64j6q3lz78l9py8ca9qgxv4nnh3kg7jy-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl
run() {
 echo "BEGIN $*"
 set +e
 "$runner" "$client" "$@" > "$fixture/result" 2>&1
 status=$?
 set -e
 cat "$fixture/result"
 [[ "$status" == 0 ]] || return "$status"
 case "$1" in
 --worker-syscalls) /usr/bin/grep -q CONTROL_WORKER_SYSCALLS_DENIED "$fixture/result";;
 --worker-*) /usr/bin/grep -q CONTROL_WORKER_SIGSEGV "$fixture/result";;
 --fork-syscalls) /usr/bin/grep -q CONTROL_FORK_SYSCALLS_DENIED "$fixture/result";/usr/bin/grep -q CONTROL_FORK_PARENT_REAPED "$fixture/result";;
 --fork-*) /usr/bin/grep -q CONTROL_FORK_SIGSEGV "$fixture/result";/usr/bin/grep -q CONTROL_FORK_PARENT_REAPED "$fixture/result";;
 --exec-*) /usr/bin/grep -q CONTROL_EXEC_PREPNR_PRESERVED "$fixture/result";;
 esac
 echo "END $* passed"
}
for address in 0x100000 0x3fe000 0x3ff000 0xffff000000 0xfffffff000; do
 for action in syscalls read write exec; do run --fork-$action "$address";done
done
