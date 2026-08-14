#!/usr/bin/env bash
#
# The compositor **on this host**, a real third-party client **inside the VM**.
#
#   scripts/wc-vm-client.sh ring          can the wire ride the ring here? (the answer is no)
#   scripts/wc-vm-client.sh linux-relay   a Linux compositor here, the client in the guest
#   scripts/wc-vm-client.sh wine-relay    wc-compositor.exe under Wine here, ditto
#   scripts/wc-vm-client.sh all
#
# docs/waycompose-design.md §67.9 built it; §67.10 withdrew two of its three
# legs as a transport result and added the `ring` leg. Read §67.10 first.
#
# ═══════════════════════════════════════════════════════════════════════════
# THE RULE, AND WHY TWO OF THESE THREE LEGS ARE NOT A TRANSPORT RESULT
# ═══════════════════════════════════════════════════════════════════════════
#
# The user's standing rule:
#
#   > all communication is to go through the ring. alternative channels are
#   > permitted only for wakeup from idle
#
# The `*-relay` legs below carry the **Wayland wire itself** over the
# virtio-serial cold path, through two `socat`s. That is the protocol stream on
# an alternative channel and **the rule forbids it.** The doorbell is the one
# sanctioned use of virtio-serial (§36.6) because waking a parked consumer *is*
# wakeup from idle; a byte relay carrying the whole conversation is not.
#
# They are kept, and labelled, because what they establish is not about the
# transport at all: that the guest has no network, that a Unix socket node
# exported over 9p is a host kernel object a guest `connect()` cannot reach, and
# that both halves really are in different kernels — which the kernel assertion
# proves in both directions. That is the venue, and it is real. **No result from
# a `*-relay` leg is a statement about how the deployment moves bytes**, and the
# banner each one prints says so rather than leaving it to this comment.
#
# `ring` is the leg that tries to obey the rule. It fails, and the failure is
# the point — see §67.10 and `run_ring` below.
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
  ring|linux-relay|wine-relay|all) ;;
  *) echo "usage: $0 [ring|linux-relay|wine-relay|all] [--keep]"; exit 2 ;;
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

# `WC_VM_POOLTEST=1` swaps the registry walk for a client that sends an `h`.
#
# The default client, `wayland-info`, binds globals and disconnects; it never
# creates a `wl_shm` pool, so the in-band-descriptor path (§2.3) is never taken.
# `wc-pooltest.c` exists precisely to create and destroy pools and nothing else,
# which makes it the smallest client that reaches the argument this arrangement
# stops short of. Built here, on the host, because /nix/store is shared and the
# guest runs the binary by absolute path like everything else.
POOLTEST=""
if [ "${WC_VM_POOLTEST:-}" = 1 ]; then
  POOLTEST="$RUNDIR/wc-pooltest"
fi

rm -rf "$RUNDIR"; mkdir -p "$RUNDIR/pools" "$RUNDIR/keymaps"
export SV_RUNDIR="$RUNDIR"
cp "$LINUX_BIN/wc-proxy" "$RUNDIR/wc-proxy"

# Which client the guest runs, as an absolute path it can reach.
CLIENT_CMD="$WAYLAND_INFO"
if [ -n "$POOLTEST" ]; then
  gcc -O2 -o "$POOLTEST" "$ROOT/scripts/wc-pooltest.c" -lwayland-client \
    || { echo "could not build wc-pooltest"; exit 2; }
  CLIENT_CMD="/sv/wc-pooltest 4"
  say "client: wc-pooltest, which sends an h, instead of wayland-info"
fi

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

# Did the connection outlive the assertions made about it?
#
# See the call site for why this exists. Every check here is a *negative*: it
# looks for the evidence of a broken session that the positive assertions cannot
# see, because a positive assertion is true the moment it is taken and says
# nothing about the moment after.
assert_session_survived() {
  local kind=$1 cdir=$2
  local cl="$RUNDIR/client.log" pl="$RUNDIR/proxy.log" sl="$cdir/comp.log"
  local broke=0

  # 1. A `wl_display` error is the protocol's own way of saying the session is
  #    over. libwayland prints it and keeps going, which is why the client's
  #    exit status is not a substitute for reading this.
  if grep -qE "wl_display#[0-9]+: error|error [0-9]+: " "$cl" 2>/dev/null; then
    bad "the client hit a protocol error, so the session died: \
$(grep -m1 -E "wl_display#[0-9]+: error|error [0-9]+: " "$cl" | tr -d '\r')"
    broke=1
  fi

  # 2. The proxy's account of the same connection.
  local why
  why=$(grep -m1 -E "client [0-9]+ failed|could not publish|which this side cannot open|stopped: the compositor closed" "$pl" 2>/dev/null | tr -d '\r')
  if [ -n "$why" ]; then
    bad "the proxy reports the session did not survive: ${why#\[proxy\] }"
    broke=1
    # Every distinct complaint, not just the first: §70.5's run had one keymap
    # refusal and six pool refusals, and reporting one of the seven would make a
    # systematic failure look like an incident.
    grep -cE "could not publish" "$pl" 2>/dev/null | grep -qv '^0$' \
      && note "pool publish refusals: $(grep -c 'could not publish' "$pl")"
    # **And the cause, measured rather than guessed** (§67.10). `SharedFile::create`
    # ends in `Handle::map_mut`, an `mmap(MAP_SHARED)` on the pool file. `/sv` is
    # 9p mounted `rw,relatime,access=client,msize=16384,trans=virtio` -- no cache
    # mode -- and a shared mapping there returns EINVAL. Measured in the guest
    # with a five-line probe: MAP_SHARED on /sv fails with errno 22, MAP_PRIVATE
    # succeeds, and MAP_SHARED on tmpfs succeeds. So this is the 9p share
    # refusing the mapping, not the dialect, not Wine and not the compositor --
    # which is also why it is identical on both legs, the one pair that differs
    # in exactly the dialect.
    if grep -q "could not publish.*os error 22" "$pl" 2>/dev/null; then
      note "the EINVAL is 9p: mmap(MAP_SHARED) on /sv returns errno 22 (measured;"
      note "MAP_PRIVATE there and MAP_SHARED on tmpfs both succeed). The file pool"
      note "transport cannot cross this share at all -- see §67.10."
    fi
  fi

  # 3. And the compositor's, so this is not one side agreeing with itself.
  if grep -qE "protocol error|client .* error" "$sl" 2>/dev/null; then
    bad "the compositor reports a protocol error: $(grep -m1 -E 'protocol error|client .* error' "$sl" | tr -d '\r')"
    broke=1
  fi

  [ "$broke" = 0 ] && ok "the session survived: no protocol error, no failed client, no refused pool"
  return 0
}

# ---------------------------------------------------------------------------
# One leg. `$1` is `linux` or `wine`, and it is the only thing that differs.
# ---------------------------------------------------------------------------
run_leg() {
  local kind=$1
  echo
  echo "== the client is in the VM; the compositor is $kind on this host =="
  note "TRANSPORT: this leg carries the Wayland wire over virtio-serial through two"
  note "socats. The rule is that all communication goes through the ring and an"
  note "alternative channel is only for wakeup from idle, so **this is not a"
  note "sanctioned transport and no result below is a statement about one.** What"
  note "it establishes is the venue: two kernels, no network, no shared socket."
  note "See the 'ring' leg for the transport question and §67.10 for the blocker."

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
XDG_RUNTIME_DIR=/tmp/rt WAYLAND_DISPLAY=wl-proxy timeout 40 ${CLIENT_CMD} \
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

  # --- did the session SURVIVE ------------------------------------------
  #
  # **This block exists because both this harness and §70.5's reported a pass
  # over a session that had already died.** Theirs stopped at `window … created`;
  # this one stopped at the registry walk. In both cases every assertion was true
  # when it was made and the connection was broken a moment later, which is §35's
  # shape exactly and is the failure this whole section is about.
  #
  # It is deliberately three sources rather than one, because the interesting
  # failures were each visible in only one of them:
  #
  #   * the **client's** own log -- a `wl_display` protocol error is fatal to the
  #     session and libwayland prints it to stderr; `wc-pooltest` then went on to
  #     print "4 pools created and destroyed" and **exit 0**, so neither its
  #     output nor its exit status noticed;
  #   * the **proxy's** log -- `client N failed`, a pool that could not be
  #     published, a keymap the far side cannot open;
  #   * the **compositor's** log -- the far end of the same connection.
  #
  # Reported before the content assertions, so a run that broke is not read as a
  # run that worked.
  assert_session_survived "$kind" "$cdir"

  # --- what the client saw, from inside another kernel --------------------
  local cl="$RUNDIR/client.log"

  # The `h`-sending client, when asked for. **The expected result is a hang,
  # not a pass**, and the assertion is written that way on purpose: `wc-proxy`
  # writes the in-band-descriptor dialect (§2.3) upstream unconditionally
  # because the far side has always been the Windows build, so against a *Linux*
  # compositor the first `wl_shm.create_pool` desynchronises the reader with
  # nothing logged on either side. A harness that called that a failure of this
  # arrangement would be blaming the venue for a dialect bug; a harness that
  # called it a pass would be worse. It reports which of the two happened.
  if [ -n "$POOLTEST" ]; then
    local made
    made=$(grep -o "create_pool=[0-9]*" "$cl" 2>/dev/null | head -1)
    # **What it printed, and no cause attached.** An earlier version of this
    # block said "expected against a Linux compositor: wc-proxy writes the
    # in-band-h dialect upstream" -- on *both* legs, including the `wine` one
    # where that pairing is correct and §70.5 confirms the stream is right at
    # both ends. That is docs/waycompose-design.md §63.3's error and §67.5's:
    # a real explanation attached to the wrong path. The two legs differ in one
    # thing, so let the two logs say which of them broke and how, and leave the
    # attribution to whoever reads both.
    note "the h-sending client on the $kind leg: $(tail -1 "$cl" 2>/dev/null | tr -d '\r')"
    note "and its own exit status was 0 -- see the session check above, which is the"
    note "only thing in this harness that noticed anything was wrong."
    grep -E "could not publish|cannot open|failed" "$RUNDIR/proxy.log" 2>/dev/null \
      | sed 's/^/        /' | head -4
    return
  fi

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

  # **And this leg stops exactly where the interesting argument starts.**
  # `wayland-info` walks the registry and disconnects; it never sends an `h`.
  # `wc-proxy` writes the in-band-descriptor dialect (§2.3) upstream
  # unconditionally, because the far side has always been the Windows build --
  # so on the `linux` leg the first `wl_shm.create_pool` desynchronises the
  # compositor's reader, with nothing logged on either side and the connection
  # left open. That is why a registry walk passes here and is not evidence that
  # a drawing client would. `scripts/wc-pooltest.c` is the client that reaches
  # it; `WC_VM_POOLTEST=1` runs it instead, and the assertion is inverted --
  # the desync is expected until the dialect is chosen rather than assumed.
  # **Corrected.** This used to say the registry walk left the in-band path
  # "advertised and not exercised". It does not: `wayland-info` sends no `h`,
  # but the seat sends `wl_keyboard.keymap` unprompted, which carries one in the
  # server-to-client direction -- and on the `linux` leg that is exactly where
  # the dialect mismatch surfaces. The old sentence was true about the client's
  # requests and wrong about the connection, and nothing noticed because nothing
  # was checking the session. See docs/waycompose-design.md §67.10.
  note "wayland-info sends no \`h\` itself, but the seat's wl_keyboard.keymap carries"
  note "one the other way, so this walk does reach the dialect. §67.10."

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

# ---------------------------------------------------------------------------
# The leg that obeys the rule, and the two refusals it collects.
# ---------------------------------------------------------------------------
#
# The wire must ride the ring. For this arrangement — a compositor on this host,
# a client in the guest — the control region has to be memory *both kernels can
# map*, which on this rig means the guest's own RAM through the memfd backend.
# The compositor has exactly two ways to offer a control region and neither
# reaches that:
#
#   --wayland-cold PORT   publishes a region out of guest RAM. `#[cfg(target_os
#                         = "linux")]` on `RingLink::publish`, and refused at
#                         parse on any other build by `check_platform_flags`.
#   --wayland-ring NAME   a named region: a file on Linux, a **named section**
#                         on Windows. A section is a same-machine object; the
#                         compositor's own startup already says the neighbouring
#                         keymap section is "unopenable by one across the
#                         boundary", and the ring section is the same kind of
#                         object.
#
# So there is no combination that gets a *Windows-build* compositor's control
# region into a form the far side of a hypervisor can map. Both are measured
# here rather than argued, because a blocker asserted from reading the source is
# the thing §67.5 is about.
run_ring() {
  echo
  echo "== can the wire ride the ring in this arrangement? =="

  # NOT `$RUNDIR/ring`: that is the path handed to --wayland-ring below, and using
  # it as this leg's scratch directory made the compositor fail to create the
  # region -- reported as "did not create a file-backed ring", which was true and
  # about the wrong step.
  local d="$RUNDIR/ringleg"; mkdir -p "$d"
  export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}" WINEDEBUG="${WINEDEBUG:--all}"

  # 1. the cross-VM publish, on the Windows build.
  wine "$WIN_BIN/wc-compositor.exe" --socket ring-a \
    --wayland-cold /dev/virtio-ports/org.sirati.svcold.0 --timeout 3 \
    >"$d/cold.log" 2>&1
  local rc=$?
  if [ "$rc" != 0 ] && grep -q "needs the Linux guest-RAM provider" "$d/cold.log"; then
    ok "the Windows build refuses --wayland-cold at parse (exit $rc), and says why:"
    echo "        $(grep -o 'wc-compositor: --wayland-cold.*' "$d/cold.log" | head -1 | cut -c1-150)..."
  else
    bad "expected --wayland-cold to be refused on the Windows build; exit $rc"
    sed 's/^/        /' "$d/cold.log" | head -5
  fi

  # 2. the named region, on the Windows build. This one *works* — and being a
  #    named section is exactly why it does not help.
  timeout 30 wine "$WIN_BIN/wc-compositor.exe" --socket ring-b \
    --wayland-ring 'Local\wc-vm-ring' --timeout 4 >"$d/ring.log" 2>&1
  if grep -q 'accepting Wayland rings in' "$d/ring.log"; then
    ok "--wayland-ring is accepted and creates a section: \
$(grep -o 'accepting Wayland rings in.*' "$d/ring.log" | head -1 | tr -d '\r')"
    note "and a named section is a same-machine object. The compositor says so about"
    note "the keymap in the very next line of its own log: \"unopenable by one across"
    note "the boundary\". Nothing in the guest can open this region either."
  else
    bad "--wayland-ring did not start a ring directory"
    sed 's/^/        /' "$d/ring.log" | head -8
  fi

  # 3. The obvious fallback, and it does not exist either.
  #
  # If a Windows-build section cannot cross, the next idea is a **file**-backed
  # control region on the one filesystem both sides can see: `--wayland-ring
  # $RUNDIR/ring` on the host, `--upstream-ring /sv/ring` in the guest. It is
  # worth measuring rather than reasoning about, because the reasoning goes
  # through a fact established elsewhere and §67.5 is about exactly that habit.
  #
  # It fails for the reason the pool publish fails: `SharedRegion::map_fd` is an
  # `mmap(MAP_SHARED)`, `/sv` is 9p with no cache mode, and a shared mapping
  # there is EINVAL. So the same measurement rules out the file *pool* and the
  # file *control region* together, and the only substrate that crosses this
  # boundary is guest RAM through the memfd backend — which is the deployment's
  # answer, the ring's, and the thing `--wayland-cold` does for Linux only.
  local rd="$d/filering"; mkdir -p "$rd"
  # `XDG_RUNTIME_DIR` is not optional here and its absence cost a run: without it
  # the socket bind fails before the ring is ever created, and the leg reported
  # "did not create a file-backed ring" — which is true, and about the wrong
  # step. §63.3, in miniature, inside this file.
  XDG_RUNTIME_DIR="$rd" "$LINUX_BIN/wc-compositor" --socket ring-c --wayland-ring "$RUNDIR/ring" \
    --timeout 60 >"$rd/comp.log" 2>&1 &
  local rcomp=$!
  local ok_ring=0
  for _ in $(seq 1 100); do
    grep -q "accepting Wayland rings" "$rd/comp.log" 2>/dev/null && { ok_ring=1; break; }
    sleep 0.2
  done
  if [ "$ok_ring" != 1 ]; then
    note "the Linux compositor did not create a file-backed ring; skipping the 9p probe"
    kill "$rcomp" 2>/dev/null
  else
    cat > "$RUNDIR/guest-cmd.sh" <<GUEST
printf 'guest-%s: %s\n' kernel "\$(uname -sr)"
mkdir -p /tmp/rt && chmod 700 /tmp/rt
ls -l /sv/ring 2>&1 | head -1
# The pool flags are here only to get past argument validation: --transport
# shared-file demands a --peer-prefix whether or not a pool is ever created, and
# without them the proxy exits before it touches the ring -- which the first run
# of this probe reported as an attach, from a log that never mentioned one.
mkdir -p /tmp/pools
XDG_RUNTIME_DIR=/tmp/rt timeout 25 /sv/wc-proxy --upstream-ring /sv/ring \
  --socket wl-proxy --runtime-dir /tmp/rt --accept-timeout 5 --timeout 8 \
  --pool-dir /tmp/pools --peer-prefix /tmp/pools/ \
  >/sv/ringproxy.log 2>&1
printf 'ringproxy-%s %s\n' rc "\$?"
printf 'WCVM-%s\n' DONE
GUEST
    rm -f "$RUNDIR/guest.log" "$RUNDIR/ringproxy.log" "$RUNDIR/nixos.qcow2"
    say "booting the guest to try --upstream-ring on the 9p share"
    ( cd "$RUNDIR" && "$VM/bin/run-nixos-vm" -nographic ) >"$rd/console.log" 2>&1 &
    QEMU_PID=$!
    for _ in $(seq 1 150); do
      grep -q "^WCVM-DONE" "$RUNDIR/guest.log" 2>/dev/null && break
      kill -0 "$QEMU_PID" 2>/dev/null || break
      sleep 2
    done
    local rp; rp=$(head -3 "$RUNDIR/ringproxy.log" 2>/dev/null | tr -d '\r' | tr '\n' ' ')
    # Three outcomes, and they are told apart on *what the log says about the
    # ring* rather than on whether anything went wrong. The first version of
    # this check matched any error at all, so a proxy that exited on argument
    # validation -- before it had touched the region -- was read as a refusal of
    # the mapping. That is the same mistake as the `$RUNDIR/ring` collision
    # above, twice in one function, and it is why both are written down.
    if grep -qiE "mmap failed|os error 22|errno 22|invalid argument" "$RUNDIR/ringproxy.log" 2>/dev/null; then
      ok "a file-backed ring on the 9p share is refused too: \
$(grep -iE 'mmap failed|errno 22|os error 22|invalid argument' "$RUNDIR/ringproxy.log" | head -1 | tr -d '\r')"
      note "same cause as the pool: mmap(MAP_SHARED) on /sv is EINVAL (measured"
      note "directly in the guest; MAP_PRIVATE there and MAP_SHARED on tmpfs both work)."
    elif grep -qiE "claimed|attached|upstream ring" "$RUNDIR/ringproxy.log" 2>/dev/null; then
      bad "unexpected: the guest attached to a file-backed ring over 9p: ${rp:0:130}"
    else
      note "the ring probe was inconclusive -- the proxy stopped before the region:"
      note "  ${rp:0:130}"
    fi
    kill "$rcomp" 2>/dev/null
    [ -n "$QEMU_PID" ] && { kill "$QEMU_PID" 2>/dev/null; wait "$QEMU_PID" 2>/dev/null; }
    QEMU_PID=""
  fi

  note "CONCLUSION: this arrangement cannot be made ring-compliant today, and the"
  note "missing piece is one function -- a guest-RAM publish for the Windows build."
  note "The compositor already commits and stamps guest RAM on Windows for an *arena*"
  note "(--arena-guest-ram, via awe-probe); ringlink's publish has never been wired"
  note "to it. See docs/waycompose-design.md §67.10."
}

SOCAT_ABS=$(readlink -f "$(command -v socat)")
case "$MODE" in
  ring)        run_ring ;;
  linux-relay) run_leg linux; end_leg ;;
  wine-relay)  run_leg wine;  end_leg ;;
  all)         run_ring; run_leg linux; end_leg; run_leg wine; end_leg ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ "$fail" = 0 ]
