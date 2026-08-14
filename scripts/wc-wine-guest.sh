#!/usr/bin/env bash
#
# The Wayland server, as a Windows binary, under Wine, **inside a VM**.
#
#   scripts/wc-wine-guest.sh [--keep]
#
# docs/waycompose-design.md §67.6.
#
# WHY THIS EXISTS
#
# The requirement docs/waycompose-design.md §67 answers is "we also need to be
# able to test against a vm running only wine". Every Wine run in this document
# before §26 was one machine with no VM boundary at all, and §26.5 is blunt
# about what that cost: several consequences of *having no hypervisor* were
# written up as "Wine lied". So a Wine leg that is a genuine second venue — its
# own kernel, its own wineserver, its own X-less session — is worth more than
# another local one, and this is the cheapest honest form of it.
#
# `nix build .#svGuest` boots a NixOS VM on this workstation with a
# `memory-backend-memfd` and a QMP socket. It has no Wine of its own and does
# not need one: `/nix/store` is shared into it over 9p, so the same
# `wine-wow64-staging-11.14` the dev shell uses runs there by absolute path,
# unchanged and unbuilt. `nix/guest.nix` is not modified by this file, which is
# the point — the guest stays the minimal thing it was designed to be.
#
# WHAT IT ASSERTS, AND WHAT IT DELIBERATELY DOES NOT
#
# One thing, sharply: **the compositor says which Win32 it is on, and it is the
# guest's Wine and not this workstation's.** `wine_get_host_version` reports the
# kernel Wine is running on, so the line printed inside the VM names the guest's
# release and the line printed here names the workstation's, and the two differ.
# That is what makes the platform line evidence rather than decoration — a run
# cannot claim to have measured the VM while actually having measured the host.
#
# It does **not** run a client, and therefore not a toplevel, a window or a
# pixel. What it does reach is further than expected and worth writing down:
# with no X in the guest at all (`graphics = false`) the whole platform still
# comes up — `SetProcessDpiAwarenessContext`, the clipboard listener, the
# keyboard layout, the message pump, the socket, the high-resolution timer and
# a clean idle-timeout exit. So Wine's null display driver is enough for
# everything short of a real `HWND` with pixels in it, which is more than
# `flake.nix`'s note about needing Xvfb implies for this particular use.
#
# The identity line is printed *before* `platform::start` regardless
# (docs/waycompose-design.md §67.3), and that ordering is the load-bearing
# part: what platform a run is on must not depend on the platform having come
# up, or the one run that most needs attributing — the one that failed to
# start — is the one that cannot be.
#
# A leg that renders in the guest needs an X server and a client in there too.
# docs/waycompose-design.md §67.8 names it as work rather than pretending this covers it.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RUNDIR=${SV_RUNDIR:-/tmp/wc-wine-guest}
KEEP=0

while [ $# -gt 0 ]; do
  case "$1" in
    --keep) KEEP=1; shift ;;
    *) echo "unknown argument $1"; exit 2 ;;
  esac
done

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }
say() { echo "[$(date +%H:%M:%S)] $*"; }

# docs/waycompose-design.md §51 and §52. Whether a hypervisor is in the path,
# declared rather than assumed.
#
# **This one is genuinely `cross-vm`, and it is the only Wine leg in the tree
# that is.** Every other Wine harness here declares `same-box` and is right to:
# §26.5's correction is that every Wine run in this document before it was one
# machine with no boundary at all. This script boots the VM itself and runs the
# compositor inside it, so the boundary is real and the declaration is checkable
# against the run's own output — docs/waycompose-design.md §67.6's two kernels are that check.
#
# `WC_HOST` and `WC_GUEST_CONFIGURED` are set here rather than expected from the
# environment, and the difference matters: the other cross-VM harnesses *reach*
# a guest somebody else configured, and are refused if there is none. This
# harness **is** the VM host — it starts QEMU on this box a few lines below —
# so the two facts `wc_topology_declare` checks are facts it establishes rather
# than facts it inherits. Setting them is stating that, not evading the guard.
. "$(dirname "$0")/wc-topology.sh"
export WC_HOST="${WC_HOST:-localhost}"
export WC_GUEST_CONFIGURED="${WC_GUEST_CONFIGURED:-svGuest}"
wc_topology_declare cross-vm \
  "this script boots .#svGuest and runs wc-compositor.exe under Wine inside it" || exit 1

# **No substrate is declared, and that is the honest answer rather than an
# omission.** `wc_substrate_declare` offers `guest-ram` and `wine-fixture`, and
# this run uses neither: there is no client, so no `wl_shm` pool and no arena
# crosses anything. Declaring `guest-ram` would name a mechanism this leg never
# touches. `wc_latency_claim` is correspondingly unavailable — this script makes
# no timing claim and must not grow one without a locate to back it.
note "substrate: NONE -- no client, so no pool and no arena. This leg measures identity, not transport."

# One run at a time: the guest's run directory holds its disk image, and a
# second boot against the same qcow2 dies on the write lock rather than doing
# anything interesting.
exec 9>/tmp/.wc-wine-guest.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

EXE="$ROOT/target/x86_64-pc-windows-gnu/debug/wc-compositor.exe"
[ -f "$EXE" ] || {
  echo "no $EXE"
  echo "cargo build -p wc-compositor --target x86_64-pc-windows-gnu"
  exit 2
}

# Resolved here rather than hard-coded, and passed into the guest as an absolute
# store path: the guest has no PATH of ours and no Wine in its closure.
WINE=$(command -v wine) || { echo "wine is not on PATH -- run inside nix develop"; exit 2; }
WINE=$(readlink -f "$WINE")
say "wine on the host: $WINE"

rm -rf "$RUNDIR"; mkdir -p "$RUNDIR"
export SV_RUNDIR="$RUNDIR"
cp "$EXE" "$RUNDIR/wc-compositor.exe"

VM=$(nix build "$ROOT#svGuest" --no-link --print-out-paths) || exit 1
say "guest image: $VM"

QEMU_PID=""
cleanup() {
  # By pid, and only the one this script started. This workstation runs other
  # VMs and other agents' processes, and a pattern kill is how one of theirs
  # gets taken down.
  [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null
  [ -n "$QEMU_PID" ] && wait "$QEMU_PID" 2>/dev/null
  [ "$KEEP" = 1 ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

# The payload. `--log` rather than a pipe because the interesting line is
# printed before anything else and a pipe through `head` would buffer it, and
# into `C:\` because a Windows binary spells the shared directory differently
# from the shell that launched it.
# `printf 'WCWG-%s\n' DONE` rather than `echo WCWG-DONE`, and every grep below
# is anchored with `^`. The unit in `nix/guest.nix` prints `cat
# /sv/guest-cmd.sh` before running it, so the script's own source text is in
# `guest.log` too — and the first version of this file matched its own sentinel
# in that echo and declared the run finished about two seconds after boot. A
# marker that a payload can print but cannot quote is the fix.
cat > "$RUNDIR/guest-cmd.sh" <<GUEST
export WINEPREFIX=/tmp/pfx WINEDEBUG=-all HOME=/tmp
printf 'guest-%s: %s\n' kernel "\$(uname -sr)"
# First boot of a prefix on two cores over 9p is slow and silent; doing it as a
# separate step means a timeout here is distinguishable from a compositor that
# never started.
$WINE wineboot -u >/sv/wineboot.log 2>&1
printf 'wineboot-%s %s\n' rc "\$?"
$WINE /sv/wc-compositor.exe --socket wg-wine --once --timeout 5 \
  --log 'C:\comp.log' >/sv/comp-stdout.log 2>&1
printf 'compositor-%s %s\n' rc "\$?"
cp /tmp/pfx/drive_c/comp.log /sv/comp.log 2>/dev/null || echo "no comp.log was written"
printf 'WCWG-%s\n' DONE
GUEST

say "booting the guest"
( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) >"$RUNDIR/guest-console.log" 2>&1 &
QEMU_PID=$!

say "waiting for the guest half (wineboot on two cores takes a while)"
for _ in $(seq 1 300); do
  grep -q "^WCWG-DONE" "$RUNDIR/guest.log" 2>/dev/null && break
  kill -0 "$QEMU_PID" 2>/dev/null || break
  sleep 2
done

echo
echo "== the Wayland server, as a Windows binary, under Wine, inside a VM =="

if ! grep -q "^WCWG-DONE" "$RUNDIR/guest.log" 2>/dev/null; then
  bad "the guest half never finished"
  sed 's/^/        /' "$RUNDIR/guest.log" 2>/dev/null | tail -20
  tail -20 "$RUNDIR/guest-console.log" | sed 's/^/        /'
  echo; echo "== $pass passed, $fail failed =="
  exit 1
fi
ok "the guest ran the payload: $(grep -m1 '^guest-kernel: ' "$RUNDIR/guest.log")"

[ -s "$RUNDIR/comp.log" ] || {
  bad "the compositor wrote no log inside the guest"
  sed 's/^/        /' "$RUNDIR/guest.log" | tail -20
  sed 's/^/        /' "$RUNDIR/comp-stdout.log" 2>/dev/null | tail -10
  echo; echo "== $pass passed, $fail failed =="
  exit 1
}
sed 's/^/        /' "$RUNDIR/comp.log"

# --- the assertion this script exists for --------------------------------
GUEST_LINE=$(grep -o 'wc-compositor: win32 is .*' "$RUNDIR/comp.log" | head -1 | tr -d '\r')
case "$GUEST_LINE" in
  *"win32 is wine"*) ok "inside the VM the compositor says: ${GUEST_LINE#wc-compositor: }" ;;
  "")                bad "the compositor never said which Win32 it was on (docs/waycompose-design.md §67)" ;;
  *)                 bad "inside the VM the compositor says: ${GUEST_LINE#wc-compositor: }" ;;
esac

# The host's own answer, for the comparison that makes the line evidence.
HOST_LINE=$("$WINE" "$EXE" --socket wg-wine-host --once --timeout 2 2>&1 \
  | grep -o 'wc-compositor: win32 is .*' | head -1 | tr -d '\r')
[ -n "$HOST_LINE" ] && say "on this workstation the same binary says: ${HOST_LINE#wc-compositor: }"

# `wine_get_host_version` reports the kernel Wine sits on, so these two lines
# name two different kernels — which is the whole claim: this run measured the
# VM, and it can prove it did.
GUEST_KERNEL=$(echo "$GUEST_LINE" | sed -n 's/.*(host \([^)]*\)).*/\1/p')
HOST_KERNEL=$(echo "$HOST_LINE"  | sed -n 's/.*(host \([^)]*\)).*/\1/p')
if [ -n "$GUEST_KERNEL" ] && [ -n "$HOST_KERNEL" ] && [ "$GUEST_KERNEL" != "$HOST_KERNEL" ]; then
  ok "and it is the guest's Wine, not this workstation's: '$GUEST_KERNEL' inside, '$HOST_KERNEL' out"
elif [ -n "$GUEST_KERNEL" ] && [ "$GUEST_KERNEL" = "$HOST_KERNEL" ]; then
  # Not a failure: the guest tracks the host's nixpkgs, so the day the two
  # kernels match this assertion stops being able to tell them apart. Said out
  # loud rather than passing quietly, because a check that cannot fail is worse
  # than no check.
  echo "  ....  both kernels are '$GUEST_KERNEL', so this run cannot tell the two venues apart"
else
  bad "no host kernel in one of the two lines: guest '$GUEST_KERNEL', host '$HOST_KERNEL'"
fi

echo
echo "== $pass passed, $fail failed =="
[ "$fail" = 0 ]
