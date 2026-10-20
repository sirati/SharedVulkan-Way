#!/usr/bin/env bash
# The guest-RAM broker in a NixOS VM test (waycompose-design.md §108.18):
# the broker daemon as its own system user over a nested QEMU's RAM memfd,
# alice and bob as clients. Builds the two static binaries, then the test.
#
#   wc-guestram-vmtest.sh [EVIDENCE-DIR]
#
# The test's log goes to EVIDENCE-DIR (default
# /mnt/datasets/builds/evidence/windows-guest-ram), never into the repository.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
EV=${1:-/mnt/datasets/builds/evidence/windows-guest-ram}
CG=${WC_CG:-/mnt/datasets/builds/wcrun/cg}
T=x86_64-unknown-linux-musl
mkdir -p "$EV"
cd "$ROOT"
"$CG" nix develop -c cargo build --release --target $T -p wc-arena-host
"$CG" nix develop -c cargo build --release --target $T -p sv-ram --bin sv-ram-vmtest
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/wc-guestram-bins-XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
cp "target/$T/release/wc-arena-host" "target/$T/release/sv-ram-vmtest" "$STAGE/"
LOG="$EV/vmtest-$(date +%Y%m%d-%H%M%S).log"
echo "wc-guestram-vmtest: the test's log is $LOG"
rc=0
"$CG" nix build --impure -L --no-link --print-out-paths \
  --expr "import $ROOT/nix/vmtest-guest-ram.nix { bins = $STAGE; }" 2>&1 | tee "$LOG" || rc=$?
[ "$rc" = 0 ] && echo "wc-guestram-vmtest: OK" || echo "wc-guestram-vmtest: FAILED ($rc)"
exit "$rc"
