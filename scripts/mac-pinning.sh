#!/usr/bin/env bash
# §103.3: do mlock-wired 4 KiB pages stay at one guest-physical address on
# macOS? §84.5's measurement, for XNU: one compositor with a guest-RAM arena
# and no client, located repeatedly, with memory pressure in the guest between
# the locates (an allocation larger than the free memory, so the compressor and
# swap run). Identical plans and full verifies mean no wired page moved.
#
#   scripts/mac-pinning.sh [ROUNDS] [PRESSURE_MIB]     (3, 2560)
set -uo pipefail
ulimit -c 0
inst=${MACVM_INSTANCE:-wayland-host}
root=$(cd "$(dirname "$0")/.." && pwd)
idir=/mnt/datasets/macvm/instances/$inst
# The VM's account comes from the VM store, not from this repository.
VMUSER=$(sed -n 's/.*"user": "\([^"]*\)".*/\1/p' "$idir/instance.json")
mvm=/mnt/datasets/macvm/bin/macos-vm
rounds=${1:-3}
pressure=${2:-2560}
small=${WC_PIN_SMALL:---arena-small-pages-may-corrupt-guest-memory}
arena=$((64 * 1024 * 1024)); out=$((1024 * 1024))
run=${WC_EVIDENCE:-/mnt/datasets/builds/evidence/macos-host}/mac-pin-$(date +%Y%m%d-%H%M%S); mkdir -p "$run"
vm() { "$mvm" ssh "$inst" "$@"; }
port=$(sed -n 's/.*"ssh_port": \([0-9]*\).*/\1/p' "$idir/instance.json")
ssh_opts=(-i /mnt/datasets/macvm/support/id_ed25519 -p "$port" -o UserKnownHostsFile="$idir/known_hosts" -o IdentitiesOnly=yes -o IdentityAgent=none -o BatchMode=yes)
# This run's socket and pressure program are the only things it may kill in
# the VM (scripts/wc-killcheck.sh).
sock=wl-pin-$$
press=/tmp/wc-press-$$
trap 'vm "pkill -f \"wc-compositor --socket $sock \"; pkill -f \"^$press\"" >/dev/null 2>&1' EXIT

rsync -a -e "ssh ${ssh_opts[*]}" "$root/target/x86_64-apple-darwin/debug/wc-compositor" $VMUSER@127.0.0.1:wc/bin/wc-compositor
cat > "$run/press.c" <<'E'
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(int c, char **v) { size_t n = (size_t)atoi(v[1]) << 20; char *p = malloc(n);
  for (int r = 0; r < 3; r++) { for (size_t i = 0; i < n; i += 4096) p[i] = (char)(i >> 12) + r; sleep(5); }
  free(p); return 0; }
E
rsync -a -e "ssh ${ssh_opts[*]}" "$run/press.c" $VMUSER@127.0.0.1:/tmp/press.c
vm "cd /tmp && clang -O1 -o $press press.c && mkdir -p -m 700 /tmp/wcpin && rm -f /tmp/wcpin/comp.log"
vm "XDG_RUNTIME_DIR=/tmp/wcpin ~/wc/bin/wc-compositor --socket $sock --timeout 900 $small \
  --arena-guest-ram 0:$arena --arena-outbound 0:$out > /tmp/wcpin/comp.log 2>&1 &"
for i in $(seq 1 40); do vm 'grep -q "listening on" /tmp/wcpin/comp.log 2>/dev/null' && break; sleep 0.5; done
vm 'cat /tmp/wcpin/comp.log' > "$run/comp.log"
runid=$(grep -o "run 0x[0-9a-f]*" "$run/comp.log" | head -1 | cut -d' ' -f2)
pages=$(grep -o "[0-9][0-9]* pages, locked" "$run/comp.log" | head -1 | cut -d' ' -f1)
echo "arena: $(grep -o 'pinning=[a-z-]*' "$run/comp.log" | head -1), run $runid, $pages pages"
[ -n "$runid" ] || { cat "$run/comp.log"; exit 1; }
for r in $(seq 0 $((rounds - 1))); do
  if [ "$r" -gt 0 ]; then
    vm "$press $pressure; vm_stat | grep -E 'Swapouts|Pages stored in compressor'" | tr '\n' ' ' > "$run/press$r.txt"
    echo "round $r pressure: $(cat "$run/press$r.txt")"
  fi
  "$root/target/debug/wc-arena-host" --qmp "$idir/qmp.sock" --vm "$idir" --run "$runid" --pages "$pages" \
    --outbound "$out" --plan "$run/plan$r" > "$run/locate$r.log" 2>&1
  echo "round $r: $(grep -o 'located [0-9]* of [0-9]* pages in [0-9.]*s' "$run/locate$r.log"), $(grep -o 'verify: [0-9]*/[0-9]* pages' "$run/locate$r.log" | tail -1), $(grep -o 'coalesced into [0-9]* run' "$run/locate$r.log")$(grep -o 'claimed twice' "$run/locate$r.log" | head -1)"
  grep '^r ' "$run/plan$r" > "$run/runs$r" 2>/dev/null
  if [ "$r" -gt 0 ]; then
    if cmp -s "$run/runs0" "$run/runs$r"; then echo "round $r: plan identical to round 0 (no page moved)"
    else echo "round $r: PLAN DIFFERS from round 0"; diff "$run/runs0" "$run/runs$r" | head -6; fi
  fi
done
echo "evidence: $run"
