#!/usr/bin/env bash
# The GPU pool grows past its boot device and gives memory back
# (waycompose-design.md §108.13), end to end, in a NixOS VM, with every grown
# region handed to the client by the broker (§108.18):
#
#   sv-gpu-server pool-guest   lavapipe, a 64 MiB boot pool, growing from a
#                              sealed memfd that stands in for the Windows
#                              guest's RAM (scattered, stamped 2 MiB units)
#   wc-arena-host sessiond     the broker daemon, as its own system user: the
#     --broker                 host's peer locates each region with the 2 MiB
#                              probe, maps it, and publishes it to the client
#                              user as udmabufs
#   sv-hostd ivshmem-serve     the cold path, unchanged
#   clients (an ordinary user) sv-pool-stress (fills the pool with 64 MiB
#                              blocks, checks every block, frees), vulkaninfo
#                              and Zink frames on the grown pool
#
# The VM is needed because only the broker's account may open /dev/udmabuf:
# the user running this script is a client and never gets it.
#
#   sv-pool-grow-e2e.sh [budget MiB, default 768] [EVIDENCE-DIR]
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BUDGET=${1:-768}
EV=${2:-/mnt/datasets/builds/evidence/windows-guest-ram}
CG=${WC_CG:-/mnt/datasets/builds/wcrun/cg}
mkdir -p "$EV"
cd "$ROOT"
"$CG" nix develop -c cargo build --release -p sv-gpu-server -p sv-hostd -p sv-icd -p wc-arena-host
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/sv-pool-grow-bins-XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
cp target/release/{sv-gpu-server,sv-hostd,wc-arena-host,libsv_icd.so} "$STAGE/"
LOG="$EV/pool-grow-$(date +%Y%m%d-%H%M%S).log"
echo "sv-pool-grow-e2e: the test's log is $LOG"
rc=0
"$CG" nix build --impure -L --no-link --print-out-paths \
  --expr "import $ROOT/nix/vmtest-pool-grow.nix { bins = $STAGE; budgetMiB = $BUDGET; }" 2>&1 | tee "$LOG" || rc=$?
[ "$rc" = 0 ] && echo "sv-pool-grow-e2e: PASS" || echo "sv-pool-grow-e2e: FAIL ($rc)"
exit "$rc"
