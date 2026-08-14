#!/usr/bin/env bash
#
# The damage path, with a **real hypervisor** in it, on this workstation.
#
#   scripts/wc-vm-damage.sh linux    client here, compositor in a Linux VM
#   scripts/wc-vm-damage.sh boot     boot the guest and stop, for poking at
#
# docs/waycompose-design.md §70.
#
# WHY THIS EXISTS
#
# The user's clause — "screen must not tear or stop rendering of scrolling no
# blank slides (e.g. in about:support)" — has one symptom still unverified, and
# §51.6's block 12 was built to verify it and has never run: it needs
# `testhost`, which is in use. The instruction that unblocked this was to
# bring the boundary here instead, and to be precise about which boundary:
#
#   1. Linux -> Linux VM          <- this file
#   2. Linux -> Wine in a Linux VM
#   3. Wine-Linux -> Linux VM
#
# **They are not interchangeable and a result from one is not a result from
# another**, so this file runs exactly one of them and says which.
#
# WHAT IS ACTUALLY DIFFERENT FROM `wc-damage.sh`
#
# `wc-damage.sh` is thorough and has no VM in it at all, which is why §27.6 was
# withdrawn: a conclusion about a cross-VM present drawn from an experiment that
# contained no hypervisor. Here the compositor runs in a NixOS guest with its
# own kernel, the arena is that guest's own RAM located through QEMU's monitor,
# and the Wayland stream crosses a virtio-serial port. `wc_topology_declare
# cross-vm` is therefore *available* here and refused there, which is the first
# real exercise of §51's gate against a leg that can actually pass it.
#
# ═══════════════════════════════════════════════════════════════════════════
# THE TRANSPORT HERE IS A FIXTURE AND THE RULE FORBIDS IT AS A RESULT
# ═══════════════════════════════════════════════════════════════════════════
#
# The user's standing rule:
#
#   > all communication is to go through the ring. alternative channels are
#   > permitted only for wakeup from idle
#
# This file carries the **Wayland wire itself** over a virtio-serial port
# through two `socat`s. That is the protocol stream on an alternative channel,
# and it is exactly what `wc-vm-client.sh` labels forbidden. **No result from
# this file is a statement about how the deployment moves bytes.**
#
# It is kept for the half that is not about the transport: the **arena locate**
# -- `--arena-guest-ram` in the guest, `wc-arena-host` reading QEMU's monitor
# from outside, and `wc-proxy` mapping the result. That is the path `wc-guest.sh`
# uses on testhost and the one §51.6's block 12 depends on, and nothing else
# on this workstation exercises it. Legs 1 and 3 in `wc-vm-client.sh` and leg 2
# in `wc-wine-guest.sh` cover the three arrangements the user asked for; this
# file is not a fourth and does not claim to be.
#
# **And the wire does not work here.** Three runs reported `2 client msgs,
# 0 server msgs`: the client reaches the proxy, the compositor answers, and the
# answers do not come back. Moving the socket off 9p and attaching the host end
# of the port first both changed nothing. That is not diagnosed further, because
# fixing it would be building a forbidden channel properly.
#
# HOW THE PROTOCOL CROSSES, AND WHY NOT SSH
#
# `nix/guest.nix` gives the guest no sshd and no networking on purpose — "no X,
# no sshd, no users, no persistent disk". The three ways in are 9p, QMP and
# virtio-serial. So the Wayland socket crosses on a **third virtio-serial port,
# added to the running guest over QMP `device_add`**, which is not a trick this
# file invented: guest.nix's own note on the doorbell says the same addition can
# be made to a running guest that way, and doing it here means `nix/guest.nix`
# is not modified by this harness at all.
#
# `socat` bridges the port to a Unix socket at each end. It is named by its
# absolute store path, which works inside the guest for the same reason
# `wc-wine-guest.sh`'s Wine does: `/nix/store` is shared over 9p, so a store
# binary runs there unbuilt and uncopied.
#
# THE CLINCHING CHECK
#
# Borrowed from `wc-wine-guest.sh` and it is the one that makes everything else
# evidence rather than decoration: **the guest reports its own kernel, and it is
# not this workstation's.** A run that measured the host while believing it
# measured the VM would otherwise be indistinguishable from a run that worked.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/target/debug"

pass=0; fail=0; skip=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
non() { echo "  SKIP  $*"; skip=$((skip + 1)); }
note(){ echo "  ....  $*"; }

. "$(dirname "$0")/wc-binaries.sh"
. "$(dirname "$0")/wc-substrate.sh"
. "$(dirname "$0")/wc-topology.sh"
. "$(dirname "$0")/wc-lifetime.sh"

RUNDIR=${WC_VM_RUNDIR:-/tmp/wc-vm-damage}
# One run at a time. Two of these would boot two guests whose command lines
# both contain the same RUNDIR, which is the one thing the hint above cannot
# survive.
exec 9>"/tmp/.wc-vm-damage.lock"
flock -n 9 || { echo "another wc-vm-damage run is in progress"; exit 1; }
KEEP=${WC_KEEP:-}
QEMU_PID=""; PROXY=""; RELAY_HOST=""; CLI=""

cleanup() {
  for p in $CLI $PROXY $RELAY_HOST $QEMU_PID; do
    [ -n "$p" ] && kill "$p" 2>/dev/null
  done
  # **The QEMU by its run directory, not by $QEMU_PID alone.** `run-nixos-vm` is
  # a wrapper script, so the pid this shell knows is the wrapper's and killing
  # it can leave the emulator behind -- which is how three of them accumulated
  # and made `find_qemu_pid` refuse. Matched on RUNDIR so this can only ever
  # reach this run's guest and never another agent's.
  pkill -f "qemu-system.*$RUNDIR" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

ARENA_MIB=${WC_ARENA_MIB:-64}
ARENA_SIZE=$((ARENA_MIB * 1024 * 1024))
ARENA_OUT=$((1024 * 1024))
SECS=${WC_SECS:-90}

# Guest-side names. `wl.0` is this harness's own port; the two that guest.nix
# defines are left alone.
WLPORT=org.sirati.wcwl.0

boot_guest() {
  mkdir -p "$RUNDIR"; rm -rf "${RUNDIR:?}"/*; mkdir -p "$RUNDIR"
  export SV_RUNDIR="$RUNDIR"

  local vm
  vm=$(nix build "$ROOT#svGuest" --no-link --print-out-paths 2>"$RUNDIR/nixbuild.log") || {
    bad "nix build .#svGuest failed"; sed 's/^/        /' "$RUNDIR/nixbuild.log" | tail -6; return 1; }
  ok "guest image built: ${vm##*/}"

  ( cd "$RUNDIR" && "$vm/bin/run-nixos-vm" -nographic ) >"$RUNDIR/console.log" 2>&1 &
  QEMU_PID=$!

  local i
  for i in $(seq 1 120); do
    [ -S "$RUNDIR/qmp.sock" ] && break
    kill -0 "$QEMU_PID" 2>/dev/null || { bad "qemu died during boot"; tail -20 "$RUNDIR/console.log" | sed 's/^/        /'; return 1; }
    sleep 1
  done
  [ -S "$RUNDIR/qmp.sock" ] || { bad "no QMP socket after 120s"; return 1; }
  ok "the guest is up and QMP is listening"

  for i in $(seq 1 180); do
    grep -q 'login:\|Welcome to NixOS\|reached target' "$RUNDIR/console.log" 2>/dev/null && break
    sleep 1
  done
  return 0
}

# qmp <json>  -- one command, one reply. The greeting has to be consumed and
# `qmp_capabilities` sent before anything else is accepted, so every call pays
# that; a persistent connection would be faster and would also mean this
# function could not be called from anywhere.
qmp() {
  printf '%s\n%s\n' '{"execute":"qmp_capabilities"}' "$1" \
    | socat - "UNIX-CONNECT:$RUNDIR/qmp.sock" 2>/dev/null
}

add_wayland_port() {
  # A chardev and a port on the controller guest.nix already created. The
  # controller's id is `svserial`, which is why this does not need one of its
  # own.
  qmp "{\"execute\":\"chardev-add\",\"arguments\":{\"id\":\"wcwl\",\"backend\":{\"type\":\"socket\",\"data\":{\"addr\":{\"type\":\"unix\",\"data\":{\"path\":\"$RUNDIR/wl.sock\"}},\"server\":true,\"wait\":false}}}}" >"$RUNDIR/qmp-chardev.log"
  qmp "{\"execute\":\"device_add\",\"arguments\":{\"driver\":\"virtserialport\",\"bus\":\"svserial.0\",\"chardev\":\"wcwl\",\"name\":\"$WLPORT\",\"id\":\"wcwlport\"}}" >"$RUNDIR/qmp-device.log"
  local i
  for i in $(seq 1 40); do
    [ -S "$RUNDIR/wl.sock" ] && return 0
    sleep 0.25
  done
  bad "the Wayland virtio port never produced a host socket"
  sed 's/^/        /' "$RUNDIR/qmp-chardev.log" "$RUNDIR/qmp-device.log" 2>/dev/null | head -6
  return 1
}

block_linux() {
  echo "== topology 1: client here, compositor and arena in a Linux VM =="

  fresh_or_skip "$BIN/wc-compositor" "cargo build -p wc-compositor"
  case $? in 2) non "no wc-compositor"; return ;; 1) return ;; esac
  fresh_or_skip "$BIN/wc-proxy" "cargo build -p wc-proxy"
  case $? in 2) non "no wc-proxy"; return ;; 1) return ;; esac
  fresh_or_skip "$BIN/wc-arena-host" "cargo build -p wc-arena-host"
  case $? in 2) non "no wc-arena-host"; return ;; 1) return ;; esac
  command -v socat >/dev/null || { non "socat is not on PATH"; return; }
  local SOCAT; SOCAT=$(command -v socat)

  # **Declared before anything runs, and both refuse rather than default.**
  # `guest-ram` is the only substrate this leg may use and there is no file to
  # fall back to; `cross-vm` needs a host and a guest to be nameable at all.
  # WC_HOST is this machine because the VM runs here — which is the honest
  # answer and the one §52.8 item 1 warned would need saying.
  export WC_HOST="${WC_HOST:-localhost}"
  export WC_GUEST_CONFIGURED=1
  wc_substrate_declare guest-ram || return
  wc_topology_declare cross-vm \
    "the compositor and its arena are in a NixOS guest on this workstation; the client is not" || return

  boot_guest || return
  add_wayland_port || return

  # The compositor, inside the guest. Copied into the shared directory for the
  # reason `sv-guest-e2e.sh` gives: the guest sees `/nix/store` and `/sv`, and
  # this binary is in neither until it is put there.
  # **The host end of the port, attached BEFORE the guest opens it.** A
  # virtio-serial port tells the guest whether a host is connected, and a guest
  # that opens it first writes into a port with nobody on the other end. The
  # first two runs of this leg reported "2 client msgs, 0 server msgs" -- the
  # client reached the proxy, the compositor answered, and the answers went
  # nowhere. Starting this side first costs nothing and removes the race.
  socat "UNIX-LISTEN:$RUNDIR/upstream.sock,fork" "UNIX-CONNECT:$RUNDIR/wl.sock" \\
    >"$RUNDIR/relay-host.log" 2>&1 &
  RELAY_HOST=$!
  sleep 1

  cp "$BIN/wc-compositor" "$RUNDIR/wc-compositor"

  # `guest-cmd.sh` is the rendezvous `nix/guest.nix`'s `sv-payload` unit waits
  # on. Two processes: the compositor on a Unix socket in /sv, and socat
  # bridging that socket to the virtio port. The compositor is headless here and
  # that is not a compromise — `--verify-damage` compares two mappings inside
  # the proxy on *this* side, so what the guest does with the pixels afterwards
  # is not what is being measured.
  cat >"$RUNDIR/guest-cmd.sh" <<GUESTEOF
# **Guest-local, not the 9p share.** The compositor binds an AF_UNIX socket
# here and 9p cannot represent one: bound under /sv it appears on the host as a
# plain file, and the guest-side socat has nothing to connect to. The first run
# of this leg got "2 client msgs, 0 server msgs" from that -- the client reached
# the proxy and the compositor never answered. Only the logs belong on /sv.
export XDG_RUNTIME_DIR=/run/wcrt
mkdir -p /run/wcrt && chmod 700 /run/wcrt
uname -sr > /sv/guest-kernel.txt
/sv/wc-compositor --socket wl-vm --timeout $SECS \\
  --arena-guest-ram "0:$ARENA_SIZE" --arena-outbound "0:$ARENA_OUT" \\
  --arena-frames /sv/arena.frames \\
  > /sv/comp.log 2>&1 &
for i in \$(seq 1 100); do [ -S /run/wcrt/wl-vm ] && break; sleep 0.2; done
$SOCAT UNIX-CONNECT:/run/wcrt/wl-vm /dev/virtio-ports/$WLPORT >/sv/socat.log 2>&1
GUESTEOF
  chmod +x "$RUNDIR/guest-cmd.sh"

  local i
  for i in $(seq 1 120); do
    [ -s "$RUNDIR/comp.log" ] && grep -q "listening on" "$RUNDIR/comp.log" 2>/dev/null && break
    sleep 1
  done
  if ! grep -q "listening on" "$RUNDIR/comp.log" 2>/dev/null; then
    bad "the compositor never bound its socket inside the guest"
    sed 's/^/        /' "$RUNDIR/comp.log" 2>/dev/null | tail -12
    tail -12 "$RUNDIR/console.log" | sed 's/^/        /'
    return
  fi
  ok "wc-compositor is listening inside the guest"

  # **The clinching check** (§39.6's, reused). The guest's kernel is not this
  # workstation's, so the run cannot have measured the host.
  local gk hk
  gk=$(cat "$RUNDIR/guest-kernel.txt" 2>/dev/null)
  hk=$(uname -sr)
  if [ -n "$gk" ] && [ "$gk" != "$hk" ]; then
    ok "the guest is a different kernel from this host: '$gk' inside, '$hk' outside"
  else
    bad "the guest reports '${gk:-nothing}' and this host reports '$hk' -- this run cannot prove it crossed a boundary"
    return
  fi

  # The arena, located through QEMU's monitor. This is the step that proves the
  # hypervisor: `wc-arena-host` reads `info mtree -f` over QMP and translates the
  # guest's own page frames, which cannot answer for a machine that is not
  # running one.
  local runid pages
  runid=$(grep -o "run 0x[0-9a-f]*" "$RUNDIR/comp.log" | head -1 | cut -d' ' -f2)
  pages=$(grep -o "[0-9]* pages" "$RUNDIR/comp.log" | head -1 | cut -d' ' -f1)
  if [ -z "$runid" ]; then
    bad "the compositor never reported a guest-RAM arena"
    sed 's/^/        /' "$RUNDIR/comp.log" | head -14
    return
  fi
  ok "the compositor allocated its arena from the guest's own RAM: run $runid, $pages pages"

  # **The frame list, and it is not an optimisation.** §7 measured the content
  # sweep at 2.308 s against 0.064 s, and -- the part that matters -- unreliable
  # on a loaded machine: five consecutive runs gave "16377 of 16384 pages",
  # "1 page index claimed twice", "28 page index(es) claimed twice". The first
  # run of this harness reproduced it exactly, at 16316 of 16384, because the
  # sweep reads 2 GiB of guest memory that the guest is allowed to change while
  # it is being read. Reading the kernel own page-frame database instead makes
  # that whole class of failure inexpressible, because no stamp is read at all.
  local frames_arg=()
  if [ -s "$RUNDIR/arena.frames" ]; then
    frames_arg=(--frames "$RUNDIR/arena.frames")
    ok "the guest published a frame list ($(wc -l <"$RUNDIR/arena.frames") frames); locating by translation, not by sweep"
  else
    note "no frame list from the guest; falling back to the content sweep, which §7 measured unreliable"
  fi
  # **The hint is this run's own directory, not the image name.** `guest.nix`
  # bakes `-name sv-guest,process=sv-guest` into every guest, so on a workstation
  # where several agents each have one, `--vm sv-guest` matches all of them and
  # `find_qemu_pid` refuses rather than guessing -- correctly, and it cost this
  # harness a run to see it. RUNDIR appears in this QEMU's command line twice
  # (the QMP path and the 9p share) and in nobody else's, which is §16.8's rule
  # applied where it bites: identity on a shared machine is a path or a pid,
  # never a name a hundred processes could carry.
  "$BIN/wc-arena-host" --qmp "$RUNDIR/qmp.sock" --vm "$RUNDIR" \
    --run "$runid" --pages "$pages" --outbound "$ARENA_OUT" "${frames_arg[@]}" \
    --plan "$RUNDIR/arena.plan" >"$RUNDIR/locate.log" 2>&1
  if [ ! -s "$RUNDIR/arena.plan" ]; then
    bad "wc-arena-host did not locate the guest's arena"
    sed 's/^/        /' "$RUNDIR/locate.log" | tail -6
    # §70.4: if the shortfall is the outbound window, say so, because the number
    # on its own reads like sweep noise and it is not.
    local badpages
    badpages=$(grep -o "verify: [0-9]*/[0-9]* pages" "$RUNDIR/locate.log" | tail -1)
    if [ -n "$badpages" ]; then
      note "the stamps that are missing are the OUTBOUND WINDOW's, at the tail of the"
      note "region: $badpages. Most of those pages read as zeroes rather than as somebody"
      note "else's data, which is a page that was dropped and re-faulted, not one that was"
      note "overwritten -- and a re-faulted page also has a new frame, which is why the"
      note "frame list is stale for exactly those pages and the sweep cannot find them"
      note "either. §70.4 has the detail; this is not the content sweep being unreliable."
    fi
    return| tail -14
    return
  fi
  local verify
  verify=$(grep -o "verify: [0-9]*/[0-9]* pages" "$RUNDIR/locate.log" | tail -1)
  ok "the arena was located in QEMU's own memory: ${verify:-plan written}"
  WC_SUBSTRATE_LOCATED=$pages
  WC_SUBSTRATE_LOCATOR="wc-arena-host over QMP"

  # The proxy's runtime dir. The relay itself was started before the guest
  # opened its end -- see above.
  mkdir -p "$RUNDIR/rt"; chmod 700 "$RUNDIR/rt"

  XDG_RUNTIME_DIR="$RUNDIR/rt" "$BIN/wc-proxy" \
    --upstream "$RUNDIR/upstream.sock" --socket wl-proxy --runtime-dir "$RUNDIR/rt" \
    --transport arena --arena-plan "$RUNDIR/arena.plan" --arena-id 0 \
    --copy damage --verify-damage \
    --accept-timeout 40 --timeout "$SECS" >"$RUNDIR/proxy.log" 2>&1 &
  PROXY=$!
  sleep 2

  if ! [ -S "$RUNDIR/rt/wl-proxy" ]; then
    bad "the proxy never bound its socket"
    sed 's/^/        /' "$RUNDIR/proxy.log" | tail -12
    return
  fi
  ok "wc-proxy is up, with the guest's arena mapped and --verify-damage on"

  local client="${WC_VM_CLIENT:-weston-terminal}"
  command -v "$client" >/dev/null || { non "$client is not installed"; return; }
  XDG_RUNTIME_DIR="$RUNDIR/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout "${WC_CLIENT_SECS:-25}" "$client" >"$RUNDIR/cli.log" 2>&1
  ok "the client ran against the proxy"

  sleep 2
  kill "$PROXY" 2>/dev/null; wait "$PROXY" 2>/dev/null; PROXY=""

  wc_assert_substrate "$RUNDIR/comp.log" vm
  wc_assert_topology "$RUNDIR/comp.log" vm

  # THE ASSERTION, over a boundary this time.
  local verified stale
  verified=$(grep -o 'verified [0-9]* commits' "$RUNDIR/proxy.log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  stale=$(grep -o '[0-9]* stale (worst [0-9]* rows)' "$RUNDIR/proxy.log" | awk -F' stale' '{print $1}' | awk '{s+=$1} END {print s+0}')
  if [ "${verified:-0}" -eq 0 ]; then
    bad "no commit was checked; the client drew nothing through the boundary"
    sed 's/^/        /' "$RUNDIR/proxy.log" | tail -10
    return
  fi
  if [ "${stale:-0}" -eq 0 ]; then
    ok "over a real VM boundary, the region matched the client on every one of $verified checked commits"
  else
    bad "$stale of $verified commits left the region stale -- and this is guest RAM, not a fixture"
    grep -o 'stale: .*' "$RUNDIR/proxy.log" | head -5 | sed 's/^/        /'
  fi

  # The gate §51 built, exercised from the side that can pass it.
  if wc_latency_claim "the present latency is measurable on this leg"; then
    ok "this leg may make a claim about cross-VM latency -- there is a hypervisor in it"
  else
    bad "a cross-vm leg was refused a latency claim; the declaration or the locate did not hold"
  fi
}

case "${1:-linux}" in
  linux) block_linux ;;
  boot)  boot_guest && add_wayland_port && { note "guest up, RUNDIR=$RUNDIR, qemu pid $QEMU_PID"; KEEP=1; sleep 600; } ;;
  *) echo "usage: $0 [linux|boot]"; exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ -n "$KEEP" ] && note "run kept in $RUNDIR"
[ "$fail" -eq 0 ]
