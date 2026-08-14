#!/usr/bin/env bash
#
# The compositor **on this host**, a real third-party client **inside the VM**.
#
#   scripts/wc-vm-client.sh linux      a Linux compositor here, the client in the guest
#   scripts/wc-vm-client.sh wine       wc-compositor.exe under Wine here, ditto
#   scripts/wc-vm-client.sh all
#
# docs/waycompose-design.md §67.9.
#
# THE THREE ARRANGEMENTS, AND WHICH TWO THIS FILE IS
#
# The user asked for three, and they are the three combinations of "which side
# is Wine" that exist while the Windows laptop is unavailable:
#
#   1. linux -> linux vm        `wc-vm-client.sh linux`      -- this file
#   2. linux -> wine linux vm   `wc-wine-guest.sh`           -- the other file
#   3. wine linux -> linux vm   `wc-vm-client.sh wine`       -- this file
#
# Arrangement 3 is the one docs/waycompose-design.md §26.5 specified as "leg 3"
# and nobody had run: *"Wine plays the Windows side, and a Linux VM guest runs
# the client."* Arrangement 1 is its control, and it is the reason both live in
# one file with one flag between them: §13.8 is a record of comparing two
# numbers taken on two trees with two instruments and being wrong, and the
# whole value of leg 1 here is that it differs from leg 3 in *one* thing.
#
# WHY THIS IS WORTH MORE THAN ANOTHER LOCAL WINE RUN
#
# docs/waycompose-design.md §26.5's correction: every Wine run in this document
# before it was one machine with no VM boundary, and several consequences of
# *having no hypervisor* were written up as "Wine lied". Here the client is in
# another kernel. It cannot see the host's filesystem except where 9p says so,
# it cannot open a host Unix socket, and its `SCM_RIGHTS` cannot cross -- which
# is the point, because that is exactly the constraint the real deployment has
# and the local Wine legs do not.
#
# HOW THE BYTES CROSS, AND WHY IT IS NOT A SOCKET
#
# The guest has no network at all (`nix/guest.nix` sets
# `networkingOptions = []`), and a Unix socket node exported over 9p is a host
# kernel object that a guest `connect()` cannot reach. What the guest does have
# is the virtio-serial cold path, whose host end is a Unix socket QEMU listens
# on. So:
#
#   client -> wc-proxy -> /tmp/up.sock  (guest)
#                            | socat
#                     /dev/virtio-ports/org.sirati.svcold.0
#                            = the hypervisor boundary =
#                       $RUNDIR/cold.sock  (host)
#                            | socat
#                     the compositor's AF_UNIX socket
#
# Two `socat`s and one virtio-serial port. The Wayland wire protocol is a byte
# stream and this forwards it unaltered, which is `probe-relay.c`'s argument for
# the same move on the real guest -- and here it is *more* honest than the local
# legs, because on those the "boundary" was two processes on one kernel.
#
# **One client at a time, and that is a property of the port rather than a
# simplification.** virtio-serial carries one stream; `wc-proxy` opens one
# upstream connection per client. A second client would interleave into the same
# pipe and corrupt both. The harness runs one and says so rather than appearing
# to support more.
#
# WHAT CROSSES THAT IS NOT BYTES
#
# `wl_shm` pools and the keymap are the two things that are *not* on the wire,
# and both use 9p rather than the port:
#
#   * The client's pools are written by `wc-proxy` into `/sv/pools` in the guest,
#     which is `$RUNDIR/pools` on the host, which the compositor opens by the
#     path `--peer-prefix` gives it. On the `wine` leg that is a `winepath -w`
#     of the host directory; on the `linux` leg it is the host directory itself.
#   * The keymap goes the other way: the compositor writes it into
#     `$RUNDIR/keymaps` and tells the client to open `/sv/keymaps/...`, which is
#     the same bytes seen from the other side of 9p.
#
# **This is a file transport and therefore a fixture, not the deployment.** The
# deployment's answer is the arena in guest RAM (docs/waycompose-design.md §12),
# and `wc_substrate_declare wine-fixture` is refused on a box with a guest --
# correctly, since this box has one. So the substrate is declared `guest-ram`'s
# absence explicitly below rather than claimed.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
RUNDIR=${SV_RUNDIR:-/tmp/wc-vm-client}
KEEP=0
MODE=${1:-all}
case "$MODE" in
  linux|wine|all) ;;
  *) echo "usage: $0 [linux|wine|all] [--keep]"; exit 2 ;;
esac
[ "${2:-}" = "--keep" ] && KEEP=1

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }
say() { echo "[$(date +%H:%M:%S)] $*"; }

# §51/§52. A hypervisor is genuinely in this path -- the client is in another
# kernel and the bytes cross a virtio-serial port to get here. As in
# `wc-wine-guest.sh`, this script *is* the VM host rather than reaching one
# somebody else configured, so it establishes the two facts the guard checks.
. "$(dirname "$0")/wc-topology.sh"
export WC_HOST="${WC_HOST:-localhost}"
export WC_GUEST_CONFIGURED="${WC_GUEST_CONFIGURED:-svGuest}"
wc_topology_declare cross-vm \
  "the client and wc-proxy run inside .#svGuest; the compositor runs on this host" || exit 1
note "substrate: a FILE over 9p, and it is a fixture. The deployment's substrate is"
note "the arena in guest RAM (docs/waycompose-design.md §12); nothing here measures it."

exec 9>/tmp/.wc-vm-client.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

command -v socat >/dev/null || { echo "socat is not on PATH -- run inside nix develop"; exit 2; }
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"
[ -x "$LINUX_BIN/wc-proxy" ] || { echo "no $LINUX_BIN/wc-proxy -- cargo build first"; exit 2; }

WAYLAND_INFO=$(command -v wayland-info) || { echo "wayland-info is not on PATH"; exit 2; }
WAYLAND_INFO=$(readlink -f "$WAYLAND_INFO")

rm -rf "$RUNDIR"; mkdir -p "$RUNDIR/pools" "$RUNDIR/keymaps"
export SV_RUNDIR="$RUNDIR"
cp "$LINUX_BIN/wc-proxy" "$RUNDIR/wc-proxy"

VM=$(nix build "$ROOT#svGuest" --no-link --print-out-paths) || exit 1
say "guest image: $VM"

QEMU_PID=""; XPID=""; HOSTRELAY=""; COMP=""
cleanup() {
  for p in $HOSTRELAY $COMP; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  # By pid, and only what this script started.
  [ -n "$QEMU_PID" ] && { kill "$QEMU_PID" 2>/dev/null; wait "$QEMU_PID" 2>/dev/null; }
  [ -n "$XPID" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ "$KEEP" = 1 ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# One leg. `$1` is `linux` or `wine`, and it is the only thing that differs.
# ---------------------------------------------------------------------------
run_leg() {
  local kind=$1
  echo
  echo "== the client is in the VM; the compositor is $kind on this host =="

  local sock=wg-vm cdir="$RUNDIR/$kind"
  mkdir -p "$cdir"

  # --- the compositor, on this host --------------------------------------
  local node peer_pools peer_keymaps
  if [ "$kind" = wine ]; then
    [ -f "$WIN_BIN/wc-compositor.exe" ] || { bad "no wc-compositor.exe"; return; }
    command -v wine >/dev/null || { bad "wine not on PATH"; return; }
    export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}" WINEDEBUG="${WINEDEBUG:--all}"
    rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null
    # Xvfb, because winex11.drv wants a display for a real window. The
    # identity line does not need one (docs/waycompose-design.md §67.6) but the
    # rest of the platform is the point of this leg.
    Xvfb :101 -screen 0 1280x1024x24 -nolisten tcp >"$cdir/xvfb.log" 2>&1 & XPID=$!
    export DISPLAY=:101
    sleep 2
    peer_pools="$(winepath -w "$RUNDIR/pools" 2>/dev/null)\\"
    wine "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 120 --layout us \
      --shared-dir "$(winepath -w "$RUNDIR/keymaps" 2>/dev/null)" \
      --shared-peer-prefix "/sv/keymaps/" \
      --log "$(winepath -w "$cdir/comp.log" 2>/dev/null)" >"$cdir/loader.log" 2>&1 &
    COMP=$!
    # Wine backs a Windows AF_UNIX socket with a real host socket node, which is
    # what makes this leg possible at all and is docs/waycompose-design.md §4.3.
    local n=""
    for _ in $(seq 1 250); do
      n=$(find "$WINEPREFIX/drive_c" -type s -name "$sock" 2>/dev/null | head -1)
      [ -n "$n" ] && break
      sleep 0.2
    done
    node=$n
  else
    peer_pools="$RUNDIR/pools/"
    XDG_RUNTIME_DIR="$cdir" "$LINUX_BIN/wc-compositor" --socket "$sock" --timeout 120 --layout us \
      --shared-dir "$RUNDIR/keymaps" --shared-peer-prefix "/sv/keymaps/" \
      >"$cdir/comp.log" 2>&1 &
    COMP=$!
    local n=""
    for _ in $(seq 1 250); do
      [ -S "$cdir/$sock" ] && { n="$cdir/$sock"; break; }
      sleep 0.2
    done
    node=$n
  fi

  [ -n "$node" ] || { bad "the compositor never created its socket"; sed 's/^/        /' "$cdir/comp.log" 2>/dev/null | head -10; return; }
  ok "the compositor is listening on this host: ${node##*/}"

  # §67: which Win32 is this, said by the compositor itself.
  local plat
  plat=$(grep -o 'wc-compositor: win32 is [a-z]*' "$cdir/comp.log" 2>/dev/null | head -1 | tr -d '\r')
  plat=${plat##* }
  case "$kind:$plat" in
    wine:wine)   ok "and it says which Win32 it is: $(grep -o 'win32 is .*' "$cdir/comp.log" | head -1 | tr -d '\r')" ;;
    linux:none)  ok "and it says it is the Linux build: $(grep -o 'win32 is .*' "$cdir/comp.log" | head -1)" ;;
    *:)          bad "the compositor never said which Win32 it was on (docs/waycompose-design.md §67)" ;;
    *)           bad "the $kind leg ran on '$plat' (docs/waycompose-design.md §67)" ;;
  esac

  # --- the guest half ----------------------------------------------------
  #
  # Written before the boot on purpose: `sv-payload` waits up to two minutes for
  # this file to appear, so putting it there first removes a race rather than
  # relying on winning one.
  cat > "$RUNDIR/guest-cmd.sh" <<GUEST
printf 'guest-%s: %s\n' kernel "\$(uname -sr)"
mkdir -p /tmp/rt && chmod 700 /tmp/rt
# The guest end of the pipe: a Unix socket wc-proxy can dial, pumped into the
# virtio-serial port whose other end is on the host.
# No \`fork\`: this accepts exactly one connection and pumps it. See --no-icons
# below for why one is the right number rather than a limitation being worked
# around.
${SOCAT_ABS} UNIX-LISTEN:/tmp/up.sock GOPEN:/dev/virtio-ports/org.sirati.svcold.0 \
  >/sv/guest-relay.log 2>&1 &
sleep 1
# **--no-icons, and it is the virtio port's arithmetic rather than a
# preference.** wc-proxy opens a *second* upstream connection to resolve app ids
# to icon pixels. A virtio-serial port is one stream: two connections
# interleave into it, the compositor sees a corrupt second message flow and
# closes, and the first connection then dies of a broken pipe. Measured exactly
# that way on the first run of this harness -- "icon: the compositor closed the
# icon connection" followed by "client 1 failed: Broken pipe". The icon feed is
# not what this leg is testing, and the honest fix is to not open the second
# connection rather than to multiplex a port that cannot carry it.
XDG_RUNTIME_DIR=/tmp/rt /sv/wc-proxy --upstream /tmp/up.sock --socket wl-proxy \
  --runtime-dir /tmp/rt --transport shared-file --no-icons \
  --pool-dir /sv/pools --peer-prefix '${peer_pools}' \
  --accept-timeout 40 --timeout 60 >/sv/proxy.log 2>&1 &
sleep 2
XDG_RUNTIME_DIR=/tmp/rt WAYLAND_DISPLAY=wl-proxy timeout 40 ${WAYLAND_INFO} \
  >/sv/client.log 2>&1
printf 'client-%s %s\n' rc "\$?"
printf 'WCVM-%s\n' DONE
GUEST

  # **A fresh boot per leg, and it is not laziness.** `nix/guest.nix`'s
  # `sv-payload` unit is `Type = oneshot` with `RemainAfterExit`, so it runs the
  # payload exactly once per boot -- deliberately, so a failure is visible in the
  # unit state rather than restarted into a loop. Reusing one boot for both legs
  # would silently run the first leg's payload twice and report the second leg's
  # result from the first leg's client.
  rm -f "$RUNDIR/guest.log" "$RUNDIR/client.log" "$RUNDIR/proxy.log" \
        "$RUNDIR/guest-relay.log" "$RUNDIR/nixos.qcow2"
  say "booting the guest"
  ( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) >"$cdir/guest-console.log" 2>&1 &
  QEMU_PID=$!

  # --- the host half of the relay, after the boot ------------------------
  #
  # `cold.sock` is created by QEMU, so it does not exist until the guest is
  # started; an earlier version of this script connected to it first and got
  # "No such file or directory" from a `socat` that then exited, leaving a leg
  # that failed for a reason two steps away from the one it reported.
  local waited=0
  for _ in $(seq 1 120); do
    [ -S "$RUNDIR/cold.sock" ] && break
    kill -0 "$QEMU_PID" 2>/dev/null || break
    sleep 0.5; waited=$((waited + 1))
  done
  [ -S "$RUNDIR/cold.sock" ] || { bad "QEMU never created cold.sock"; tail -15 "$cdir/guest-console.log"; return; }
  socat "UNIX-CONNECT:$RUNDIR/cold.sock" "UNIX-CONNECT:$node" >"$cdir/relay.log" 2>&1 &
  HOSTRELAY=$!
  sleep 0.5
  kill -0 "$HOSTRELAY" 2>/dev/null \
    && ok "the host relay bridges the virtio-serial port to the compositor's socket" \
    || { bad "the host relay died"; sed 's/^/        /' "$cdir/relay.log"; return; }

  say "waiting for the guest half"
  for _ in $(seq 1 180); do
    grep -q "^WCVM-DONE" "$RUNDIR/guest.log" 2>/dev/null && break
    kill -0 "$QEMU_PID" 2>/dev/null || break
    sleep 2
  done

  if ! grep -q "^WCVM-DONE" "$RUNDIR/guest.log" 2>/dev/null; then
    bad "the guest half never finished"
    sed 's/^/        /' "$RUNDIR/guest.log" 2>/dev/null | tail -15
    sed 's/^/        /' "$RUNDIR/guest-relay.log" 2>/dev/null | tail -5
    sed 's/^/        /' "$RUNDIR/proxy.log" 2>/dev/null | tail -10
    return
  fi
  ok "the guest ran the payload: $(grep -m1 '^guest-kernel: ' "$RUNDIR/guest.log")"

  # --- the two kernels, and this leg's version of the check ---------------
  #
  # `wc-wine-guest.sh` asserts that the compositor's `wine_get_host_version` is
  # the *guest's* kernel, because there the compositor is in the VM. Here it is
  # the mirror image: the compositor is on this host, so its kernel must be the
  # *host's* and must differ from the one the payload reported. Neither
  # assertion can be satisfied by a run that put both halves in the same place,
  # which is the failure both are guarding against -- and between them the pair
  # says which side of the boundary each half was on rather than asserting it.
  if [ "$kind" = wine ]; then
    local ghost chost
    ghost=$(grep -m1 '^guest-kernel: ' "$RUNDIR/guest.log" | sed 's/^guest-kernel: //' | tr -d '\r')
    chost=$(grep -o 'win32 is wine .* (host [^)]*)' "$cdir/comp.log" | head -1 \
            | sed -n 's/.*(host \(.*\))/\1/p' | tr -d '\r')
    if [ -n "$chost" ] && [ -n "$ghost" ] && [ "$chost" != "$ghost" ]; then
      ok "and Wine is on this host, not in the VM: compositor '$chost', client '$ghost'"
    elif [ "$chost" = "$ghost" ]; then
      # Not impossible and not a pass: the guest tracks the host's nixpkgs, so
      # the day the two kernels match this check stops being able to separate
      # them. Said out loud rather than passing quietly.
      note "both kernels are '$chost', so this run cannot tell the two sides apart"
    else
      bad "no kernel from one of the two sides: compositor '$chost', client '$ghost'"
    fi
  fi

  # --- what the client saw, from inside another kernel --------------------
  local cl="$RUNDIR/client.log"
  if grep -q "interface: 'wl_compositor'" "$cl" 2>/dev/null; then
    ok "a stock libwayland client in the VM enumerated the compositor's globals: \
$(grep -c "interface: '" "$cl") interfaces"
  else
    bad "the client in the VM saw no globals"
    sed 's/^/        /' "$cl" 2>/dev/null | tail -10
    sed 's/^/        /' "$RUNDIR/proxy.log" 2>/dev/null | tail -10
    return
  fi
  for want in wl_shm xdg_wm_base wl_seat; do
    grep -q "interface: '$want'" "$cl" \
      && ok "  ... including $want" \
      || bad "  ... but not $want"
  done

  # The compositor's own account of the same client, so this is not one log
  # agreeing with itself.
  grep -q "client" "$cdir/comp.log" 2>/dev/null \
    && ok "and the compositor logged the connection from its side" \
    || note "the compositor's log says nothing about a client"
}

# Everything one leg started, torn down before the next one starts. By pid, and
# only this script's -- the workstation runs other agents' VMs.
end_leg() {
  for p in $HOSTRELAY $COMP; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  [ -n "$QEMU_PID" ] && { kill "$QEMU_PID" 2>/dev/null; wait "$QEMU_PID" 2>/dev/null; }
  [ -n "$XPID" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  HOSTRELAY=""; COMP=""; QEMU_PID=""; XPID=""
  sleep 2
}

SOCAT_ABS=$(readlink -f "$(command -v socat)")
case "$MODE" in
  linux) run_leg linux; end_leg ;;
  wine)  run_leg wine;  end_leg ;;
  all)   run_leg linux; end_leg; run_leg wine; end_leg ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ "$fail" = 0 ]
