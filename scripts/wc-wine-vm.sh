#!/usr/bin/env bash
# docs/waycompose-design.md §66: Wine *inside* the Linux VM guest.
#
#   scripts/wc-wine-vm.sh          boot the guest, run the probe in it, report
#   scripts/wc-wine-vm.sh --keep   and leave the run directory behind
#
# Run from inside `nix develop`.
#
# WHICH TOPOLOGY THIS IS
#
# The user asked for three, each with a real hypervisor in the path: Linux to
# Linux VM, Linux to **Wine in a Linux VM**, and Wine-Linux to Linux VM. This is
# the second. `scripts/wc-ring-vm.sh` is the first and runs the *Linux*
# compositor in the guest; nothing before this ran a Windows binary inside one.
#
# THE QUESTION IT IS FOR
#
# §53.3 measured `GetGuiResources` returning 0 under Wine on this workstation
# and skipped the USER/GDI leak assertion for that reason. The skip could not
# settle what the zero *was*: a fact about Wine, or a fact about the venue. That
# matters beyond tidiness — if Wine stubs it everywhere, then no arrangement of
# virtual machines can turn that SKIP into an assertion and only a real Windows
# kernel can, which decides whether the queued testhost run is worth the
# user's machine.
#
# So the probe is self-contained (`probe-life.exe selftest`): it makes four
# windows and asks about its own process, printing both numbers next to the
# count of windows they describe, so `user=0 windows=5` is legible as a stub
# rather than as an absence.
#
# HOW THE GUEST GETS A WINDOWS BINARY WITHOUT ONE BEING INSTALLED
#
# `nix/guest.nix` shares the host's `/nix/store` over 9p, and says so: "the
# guest half is handed in from the host's store, which the guest can see because
# /nix/store is shared. So there is nothing to install: the harness runs an
# absolute path." Wine and Xvfb are store paths, so the guest can run them
# without the image growing a package. `probe-life.exe` is not in the store, so
# it is copied into the shared run directory, which the guest sees as `/sv`.
#
# **That sharing is also the sharpest caveat and it is stated up front.** The
# Wine the guest runs is byte-for-byte the Wine this host runs — the same store
# path over 9p. A `stub` answer from inside the VM is therefore evidence about
# *that Wine build*, which is exactly the question asked, and is **not** evidence
# that a different Wine would stub. See §66.4.
#
# WHAT STOPS THIS CLAIMING THE VM WHILE MEASURING THE HOST
#
# The guest prints its own `uname -a` and `/proc/version` beside the answer, and
# this harness fails if they match the host's. A run that measured the host
# would otherwise look exactly like a run that measured the guest, which is the
# §51 mistake in a new place.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RUNDIR="${SV_RUNDIR:-/tmp/wc-wine-vm}"
PROBE="${CARGO_TARGET_DIR:-$ROOT/target}/probe-life.exe"
KEEP=0
[ "${1:-}" = "--keep" ] && KEEP=1

pass=0; fail=0; skip=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }
nope() { echo "  SKIP  $*"; skip=$((skip + 1)); }

. "$(dirname "$0")/wc-substrate.sh"
. "$(dirname "$0")/wc-topology.sh"

VMPID=""
cleanup() {
  [ -n "$VMPID" ] && { kill "$VMPID" 2>/dev/null; sleep 1; kill -9 "$VMPID" 2>/dev/null; }
  [ "$KEEP" = 1 ] && echo "kept: $RUNDIR"
}
trap cleanup EXIT

echo "== docs/waycompose-design.md §66: Wine inside the Linux VM =="

command -v x86_64-w64-mingw32-gcc >/dev/null || { bad "no mingw gcc (run inside nix develop)"; exit 1; }
x86_64-w64-mingw32-gcc -O1 -g -o "$PROBE" "$ROOT/scripts/probe-life.c" -luser32 -lgdi32 \
  || { bad "probe-life.c did not build"; exit 1; }

WINEDIR=$(dirname "$(readlink -f "$(command -v wine)")")
XVFBDIR=$(dirname "$(readlink -f "$(command -v Xvfb)")")
[ -x "$WINEDIR/wine" ] || { bad "no wine on PATH"; exit 1; }
[ -x "$XVFBDIR/Xvfb" ] || { bad "no Xvfb on PATH"; exit 1; }

# `cross-vm` is refused by wc_topology_declare unless WC_HOST names a machine
# running QEMU and a guest is configured — a guard written for testhost, where
# the hypervisor is somebody else's. Here *this* process starts the hypervisor,
# so the declaration is made against that fact and the guard is satisfied by
# pointing both at this box rather than by weakening it.
export WC_HOST="${WC_HOST:-localhost}"
export WC_GUEST_CONFIGURED=1
wc_topology_declare cross-vm \
  "this harness boots \$VM/bin/run-nixos-vm itself; the probe runs on the guest's kernel" || exit 1

rm -rf "$RUNDIR"; mkdir -p "$RUNDIR"
export SV_RUNDIR="$RUNDIR"
cp "$PROBE" "$RUNDIR/probe-life.exe"
uname -a > "$RUNDIR/host-uname.txt"

# The payload. `/sv` is this directory inside the guest.
#
# **No backticks below this line.** The heredoc is unquoted so that $XVFBDIR and
# $WINEDIR expand into the payload -- and an unquoted heredoc also performs
# command substitution, so a backtick in a *comment* inside it is executed on
# the host and deleted from the payload. That is what happened: the harness
# printed `mscoree,mshtml=: command not found` and a wine usage message, and the
# guest received a comment with holes in it. Prose about the payload lives here,
# where it is safe.
#
# WINEDLLOVERRIDES disables the Mono and Gecko add-ons. Without it the first
# wine in a fresh prefix tries to install them, and with no network in this
# guest that is where an early run of this harness stopped -- the payload log
# ended at the Xvfb line with nothing after it, which reads exactly like wine
# being absent rather than wine waiting.
cat > "$RUNDIR/guest-cmd.sh" <<PAYLOAD
set -x
echo "GUEST-UNAME: \$(uname -a)"
echo "GUEST-PROCVERSION: \$(cat /proc/version)"
export HOME=/tmp/wh WINEPREFIX=/tmp/wp WINEDEBUG=-all WINEDLLOVERRIDES="mscoree,mshtml="
mkdir -p \$HOME \$WINEPREFIX
$XVFBDIR/Xvfb :7 -screen 0 800x600x24 -nolisten tcp &
sleep 4
export DISPLAY=:7
echo "GUEST-WINEBOOT-BEGIN"
$WINEDIR/wineboot --init 2>&1 | tail -3
echo "GUEST-WINEBOOT-END"
echo "GUEST-WINE-VERSION: \$($WINEDIR/wine --version 2>&1 | tail -1)"
echo "GUEST-SELFTEST-BEGIN"
$WINEDIR/wine /sv/probe-life.exe selftest 2>/dev/null
echo "GUEST-SELFTEST-END"
PAYLOAD

VM=$(nix build "$ROOT#svGuest" --no-link --print-out-paths 2>/dev/null) \
  || { bad "nix build .#svGuest failed"; exit 1; }
ok "guest image built: $VM"

( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) >"$RUNDIR/console.log" 2>&1 &
VMPID=$!
note "guest booting (pid $VMPID); waiting up to ${WC_WINE_VM_SECS:-900}s -- wineboot in a VM whose store is 9p is slow, and 600s timed out once mid-wineboot"

for _ in $(seq 1 "${WC_WINE_VM_SECS:-900}"); do
  # Anchored, and that is a fix: the `sv-payload` unit echoes the script it is
  # about to run into the same log, so an unanchored grep for a marker matches
  # the *source line* `echo "GUEST-SELFTEST-END"` before anything has run. The
  # first run of this harness "finished" in seconds for that reason.
  grep -qE "^GUEST-SELFTEST-END$" "$RUNDIR/guest.log" 2>/dev/null && break
  kill -0 "$VMPID" 2>/dev/null || break
  sleep 1
done

if [ ! -f "$RUNDIR/guest.log" ]; then
  bad "the guest never wrote /sv/guest.log -- it did not boot far enough to run anything"
  tail -20 "$RUNDIR/console.log" 2>/dev/null
  echo; echo "  $pass passed, $fail failed, $skip skipped"; exit 1
fi

echo "--- what the guest said ---"
grep -E "GUEST-|before |after |windows=|counter=" "$RUNDIR/guest.log" || cat "$RUNDIR/guest.log"

# It really was the guest's kernel.
guest_uname=$(grep -m1 "^GUEST-UNAME: " "$RUNDIR/guest.log" | sed 's/^GUEST-UNAME: //')
host_uname=$(cat "$RUNDIR/host-uname.txt")
if [ -z "$guest_uname" ]; then
  bad "the guest never reported its kernel, so nothing below can claim to be from it"
elif [ "$guest_uname" = "$host_uname" ]; then
  bad "the guest's uname is identical to this host's -- this run measured the host"
else
  ok "the probe ran on another kernel: '$guest_uname' against this host's '$host_uname'"
fi

# `tr -d` on each, and this is the **third** time in this session that a
# Windows program writing a line a shell reads has cost a run: probe-drag's
# window list in §50, probe-life's `gone` in §53, and now this. mingw's stdio
# turns every `\n` into CRLF, so `counter` is `stub\r`, `case ... in stub)`
# does not match, and the harness reports "unexpected verdict 'stub'" about a
# verdict it can plainly print. It is always the same bug and it always looks
# like a different one.
counter=$(grep -m1 "^counter=" "$RUNDIR/guest.log" | cut -d= -f2 | tr -d '\r')
windows=$(grep -m1 "^windows=" "$RUNDIR/guest.log" | cut -d= -f2 | tr -d '\r')
after=$(grep -m1 "^after " "$RUNDIR/guest.log" | tr -d '\r')

if [ -z "$counter" ]; then
  bad "the probe produced no verdict; the guest log is in $RUNDIR/guest.log"
else
  note "in the guest: $after windows=$windows"
  # Non-vacuity: a verdict about handle counts is only worth anything if the
  # process owned handles to count. The probe makes four windows.
  if [ "${windows:-0}" -lt 4 ]; then
    bad "the probe owned only ${windows:-0} window(s), so 'user=0' says nothing"
  else
    ok "the probe owned $windows windows in the guest, so a zero would be a stub and not an absence"
    case "$counter" in
      live) ok "GetGuiResources ANSWERS under Wine in the guest -- §53's SKIP can become an assertion there" ;;
      stub) nope "GetGuiResources still returns 0 under Wine in the guest: it is a fact about Wine, not the venue (§66.4)" ;;
      *)    bad "unexpected verdict '$counter'" ;;
    esac
  fi
fi

echo
echo "  $pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
