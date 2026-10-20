#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" HL_NATIVE_FIXTURE="$fixture"
mkdir -p "$HOME" "$fixture/tmp"
client=/nix/store/46b2dlp16iyqq8msxr3c7gdvsj99c3v4-hl-async-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-async-fault-client
for binary in /nix/store/64j6q3lz78l9py8ca9qgxv4nnh3kg7jy-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl /nix/store/sggc9sksnmjzf4hwl3rg4rqkv749hg90-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl; do /usr/bin/codesign --verify --strict "$binary";done
set +e
/nix/store/5238ihxsxqapz5aqzb9q9c3l5xr66l2a-hyperlinux-probe-runner --timeout 8 "$client" > "$fixture/old" 2>&1
old=$?
set -e
cat "$fixture/old"
echo "old runtime exit=$old"
[[ "$old" == 91 ]]
/nix/store/mmjchb81vdcibjyrad8al6vvjsz0s8ni-hyperlinux-probe-runner --timeout 8 "$client" > "$fixture/new" 2>&1
cat "$fixture/new"
/usr/bin/grep -q 'both rt_sigreturn paths passed' "$fixture/new"
echo 'identical exact-main-TID fixture fails old runtime and passes corrected runtime'
