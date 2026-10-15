#!/usr/bin/env bash
# Run the compositor's tests on **real Windows**, in the testhost guest.
#
#   scripts/wc-guest.sh deploy   cross-compile the C probes and copy everything over
#   scripts/wc-guest.sh run      run every check and print the table
#   scripts/wc-guest.sh stage4   only blocks 7-10, the host half (needs WC_HOST)
#   scripts/wc-guest.sh arena    only block 10, the guest-RAM arena (needs WC_HOST)
#   scripts/wc-guest.sh firefox  only block 11, Firefox over guest RAM (needs WC_HOST_FIREFOX)
#   scripts/wc-guest.sh damage   only block 12, the damage path WITH a hypervisor in it (§46)
#   scripts/wc-guest.sh clean    remove everything this script put on the guest
#   scripts/wc-guest.sh all      deploy, run, clean
#
# Run from inside `nix develop`.
#
# WHY THIS SCRIPT EXISTS
#
# `wc-handshake.sh`, `wc-xdg.sh` and `wc-xkb.sh` all pass under Wine 11.14
# staging, and every one of them says in its own header that Wine is not
# Windows. This is the script that stops saying it.
#
# THE PROBLEM WINE WAS SOLVING FOR US, AND HOW IT IS SOLVED HERE
#
# Under Wine a Windows AF_UNIX socket is backed by a real host Unix socket node,
# so `wayland-info` and `weston-multi-resource` -- native Linux C programs built
# on libwayland, which have never heard of this project -- connect straight to a
# mingw-built .exe. That is what made stages 1 and 2 evidence rather than
# self-agreement.
#
# On the guest that is gone: `afunix.sys` creates a node on NTFS that no Linux
# process can open. Three ways out were available.
#
#   1. Run our own `wc-probe.exe` as the client. Cheap, and it is done below --
#      but it is a matched pair of our own binaries, so a symmetric bug in our
#      own wire encoding would pass.
#   2. Port a Wayland client to Windows. There is no third-party one, and
#      porting libwayland is a project, not a test.
#   3. Move the bytes instead of the client. `scripts/probe-relay.c` accepts a
#      TCP connection on the guest's loopback and forwards it into the
#      compositor's AF_UNIX socket; `ssh -L <socket>:127.0.0.1:<port>` puts the
#      Linux end in an `XDG_RUNTIME_DIR`, where a stock libwayland client opens
#      it without knowing it is not a compositor.
#
# (3) is what the interesting checks use, and the reason it does not weaken them
# is in `probe-relay.c`'s header: the Wayland wire protocol over the socket is a
# byte stream, and on Windows an `h` argument is encoded *in band*, so there is
# no ancillary data for a TCP hop to lose. The compositor's socket, afunix.sys,
# the framing, wepoll and the whole server are on the real side of the pipe.
#
# THE SECOND MACHINE, WHICH STAGE 4 NEEDS AND STAGES 1-3 DID NOT
#
# Everything above moves *protocol bytes*, and a TCP hop moves those. Stage 4
# moves *pixels*, and a pixel has to arrive in memory both sides can name. The
# only such memory on this deployment is virtiofsd's export -- `/srv/winshare`
# on the VM host, `Z:` in the guest -- and that export exists between the guest
# and its own host, not between the guest and whichever workstation is driving
# the tests. So the stage-4 block runs the proxy and the unmodified client on
# **`WC_HOST`**: an ssh target for a Linux machine that shares a filesystem with
# the guest. Everything else still runs from here.
#
# This is not a weakening. The proxy's job is to be a Wayland server to a local
# client and a client of the Windows compositor, and it does that on whichever
# Linux box the shared region is reachable from. What it does mean is that a
# run with `WC_HOST` unset cannot test stage 4 at all, and says so rather than
# testing something adjacent.
#
# The rule is untouched by any of this: no descriptor crosses. The client's fd
# is consumed by the proxy on `WC_HOST` and what reaches the guest is a path or
# an `(arena, offset, len)` triple -- which the checks below verify from the
# compositor's own count of what it opened, not from the fact that pixels
# appeared.
#
# TWO OPERATIONAL LESSONS, BOTH PAID FOR
#
# * A Windows process backgrounded inside an ssh session keeps that session
#   open, and dies when the session is torn down. So every long-running guest
#   process gets its own ssh connection, held open from here, and killing it is
#   the cleanup. Backgrounding inside one session and then connecting from
#   another looks exactly like a firewall problem and is not one.
# * The agent to use is `~/.1password/agent.sock`. The sockets under
#   `~/.ssh/agent/s.*` are forwarded from a login session, go stale on every
#   reconnect, and fail as `agent refused operation` -- which looks exactly like
#   a locked vault and is not one. The guest's sshd also caps at six key
#   attempts and the agent offers six keys, so one identity is pinned.
#
# A THIRD, PAID FOR IN STAGE 5. The guest's user is an administrator, and
# Windows' stock `sshd_config` ends with `Match Group administrators` /
# `AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys`. So
# `~/.ssh/authorized_keys` is read by nobody, and a key appended there is
# offered, refused, and looks exactly like a key problem. `WC_HOST`'s key goes
# in the ProgramData file.
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
# Exported as resolved: `wc_topology_declare cross-vm` (wc-topology.sh) checks
# WC_GUEST, and a default that lived only in GUEST made it refuse cross-vm
# unless the caller had happened to set the variable this line defaults.
export WC_GUEST="$GUEST"
# Named rather than hard-coded, because more than one experiment runs against
# this guest at a time and a shared directory means one deploy overwrites the
# other's binaries in the middle of its run — which presents as the other run
# testing code nobody wrote.
GUESTNAME="${WC_GUESTNAME:-wctest}"
GUESTDIR="/c/Users/<guest-user>/$GUESTNAME"
GUESTDIR_WIN="C:\\Users\\<guest-user>\\$GUESTNAME"
APPDATA_WIN='C:\Users\<guest-user>\AppData\Local\waycompose'
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
KEY="${WC_GUEST_KEY:-/tmp/wc-guest-key.pub}"
PORT_BASE="${WC_PORT_BASE:-57271}"

# The stage-4 half. Unset WC_HOST and blocks 7-9 skip with their reason.
HOST="${WC_HOST:-}"                                  # ssh target of the VM host
HOST_DIR="${WC_HOST_DIR:-/tmp/wc-stage4}"            # where its binaries land
SHARE="${WC_HOST_SHARE:-/srv/winshare/wc}"           # the export, as the host spells it
SHARE_WIN="${WC_GUEST_SHARE:-Z:\\wc}"                # the export, as the guest spells it
HOST_GUEST="${WC_HOST_GUEST:-<guest-user>@<guest-nat-ip>}"       # how the host reaches the guest
HOST_KEY="${WC_HOST_KEY:-\$HOME/.ssh/wc_guest}"      # and with which key (expanded there)
HOST_WESTON="${WC_HOST_WESTON:-}"                    # dir holding weston-terminal, or ""

# STAGE 7. The guest-RAM arena, which is a fact about the hypervisor rather than
# about the filesystem, so it needs a different three things.
#
# WHY ROOT. `wc-arena-host` reads QEMU's QMP monitor and `/proc/<qemu-pid>/fd`,
# and `wc-proxy` opens the backing object at `/proc/<qemu-pid>/fd/<n>`. On this
# machine QEMU is a *system* service running as root, so both of those are
# root-only. That is a property of how testhost starts the VM and not of the
# design: shared_vulkan's own boundary hands the same name to unprivileged ICD
# processes, and it works there because QEMU runs as the developer. Recorded in
# docs/waycompose-design.md §12.9 rather than worked around, because the workaround would be
# passing a descriptor between the two processes and the rule says no.
QMP="${WC_QMP:-/run/windows-vm/qmp.sock}"            # QEMU's monitor, on WC_HOST
VM_HINT="${WC_VM_HINT:-windows-vm}"                  # how to recognise its process
ROOTC="${WC_ROOTC:-\$HOME/rootc}"                    # how WC_HOST becomes root
ARENA_PAGES="${WC_ARENA_PAGES:-16384}"               # 64 MiB, in 4096-byte pages
# STAGE 10: how much of that the compositor keeps for the objects it creates for
# clients. See the driver, and docs/design.md §14.
ARENA_OUTBOUND="${WC_ARENA_OUTBOUND:-1048576}"       # 1 MiB, the outbound window

# STAGE 12. The client docs/goal-firefox.md is about, on WC_HOST, as an absolute
# path: it is not on that machine's PATH, and a `firefox` that resolved to
# something else would be a different measurement. Block 11 skips without it.
HOST_FIREFOX="${WC_HOST_FIREFOX:-}"
# Set by the one block that needs them and empty everywhere else, so that every
# existing leg passes exactly what it passed before.
S4_FIREFOX=""; S4_OUTPUT=""; S4_VERIFY=""

STAGE=$(mktemp -d /tmp/wc-guest-XXXXXX)
RUNDIR=$(mktemp -d /tmp/wc-guest-run-XXXXXX)
chmod 700 "$RUNDIR"
# WC_KEEP honoured, which it was not. Two guest runs of block 12 failed and
# neither could be diagnosed, because this trap removed $RUNDIR -- and with it
# s4-dmg.log, the only place the driver's own S4-FAIL line is written -- before
# anything could read it. A harness that discards its evidence on the way out
# costs a run per question on a machine other agents are queued for.
trap 'if [ -n "${WC_KEEP:-}" ]; then echo "  ....  kept: $RUNDIR"; else rm -rf "$STAGE" "$RUNDIR"; fi' EXIT

pass=0; fail=0; skip=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }
nope() { echo "  SKIP  $*"; skip=$((skip + 1)); }

export SSH_AUTH_SOCK="$AGENT"
export MSYS_NO_PATHCONV=1
SSHOPT=(-o IdentityAgent="$AGENT" -o IdentitiesOnly=yes -i "$KEY"
        -o BatchMode=yes -o LogLevel=ERROR)

# One identity, because the agent offers six and Windows sshd tries six.
pin_key() {
  [ -s "$KEY" ] && return 0
  ssh-add -L 2>/dev/null | head -1 > "$KEY"
  [ -s "$KEY" ] || { echo "error: no identity from the agent at $AGENT"; exit 2; }
}

g()  { ssh "${SSHOPT[@]}" "$GUEST" "$@"; }
gq() { ssh "${SSHOPT[@]}" "$GUEST" "$@" 2>&1; }
h()  { ssh "${SSHOPT[@]}" "$HOST" "$@" 2>&1; }

# STAGE 42. `fresh` and `deployed_is`. Sourced after the reporters and `g`/`h`.
. "$(dirname "$0")/wc-binaries.sh"
# §51/§27. `do_damage` declares its substrate and its topology and asserts both.
# These were never sourced here: block 12 was written, reviewed and merged
# without ever running, so the first real run died on `command not found`
# before it reached anything it was built to measure. §14.9, once more.
. "$(dirname "$0")/wc-substrate.sh"
. "$(dirname "$0")/wc-topology.sh"

# Kill every process running out of one directory on the guest, and nothing
# else. See the call site in `do_deploy` for why an image name will not do.
#
# The prefix is matched with a trailing backslash so that `wctest` cannot match
# `wctest7`, which is a real directory belonging to a real other experiment.
kill_in_dir() {
  local dir="$1"
  g "export MSYS2_ARG_CONV_EXCL='*'; powershell.exe -NoProfile -Command \
     \"\\\$p = '${dir}\\\\'; Get-CimInstance Win32_Process | \
      Where-Object { \\\$_.ExecutablePath -and \\\$_.ExecutablePath.StartsWith(\\\$p) } | \
      ForEach-Object { Stop-Process -Id \\\$_.ProcessId -Force -ErrorAction SilentlyContinue }\"" \
    >/dev/null 2>&1
  true
}

# ------------------------------------------------- the stage-4 driver --------
#
# One experiment, run on WC_HOST: the Windows compositor on the guest, the relay
# beside it, an `ssh -L` unix socket into that relay, `wc-proxy` on the far end
# of it, and an unmodified libwayland client on the proxy. It prints its logs
# with markers and the caller below asserts on them.
#
# Written out from here rather than kept as a second file in `scripts/`. It has
# exactly one caller, it must agree with that caller about every marker it
# prints, and a stale copy left on a host from a previous run that silently
# disagrees is a failure mode with no symptom -- the run would pass while
# testing the old thing.
write_stage4_driver() { cat >"$1" <<'DRIVER'
#!/usr/bin/env bash
# wc-stage4 -- generated by scripts/wc-guest.sh; do not edit on the host.
#   wc-stage4 <transport> <client> <port> <share> <share_win> <guest> <key> <weston> <secs>
#             [<qmp> <vm-hint> <rootc> <arena-pages> <guest-dir> <arena-outbound>
#              <firefox> <output>]
set -uo pipefail
TRANSPORT=$1 CLIENT=$2 PORT=$3 SHARE=$4 SHARE_WIN=$5 G=$6 GKEY=$7 WESTON=$8 SECS=$9
QMP=${10:-} VM_HINT=${11:-} ROOTC=${12:-} PAGES=${13:-16384} GDIR=${14:-wctest}
# STAGE 10. How many bytes of a guest-RAM arena are the compositor's to hand to
# clients. One MiB against 64. What actually crosses for a `us` keymap is 35572
# bytes -- smithay recompiles the 71249-byte source through xkbcommon before
# serialising it, so the compositor's own log line and the client's are two
# different numbers and both are right -- which is 28 keymaps of headroom for
# 1.5% of the region.
ARENA_OUTBOUND=${15:-1048576}
# STAGE 12. The client `docs/goal-firefox.md` is about, as an absolute path,
# because it is not on this host's PATH and a `firefox` that resolved to
# something else would be a different measurement. Empty for every other client.
FIREFOX=${16:-}
# The size of the one `wl_output`. It matters for exactly one client and it
# matters a lot: with no output at all GTK sizes Firefox's window against an
# empty monitor list and produces 552x172 (docs/waycompose-design.md §15.3), and with a
# different one the window size in the assertions moves. Fixed here so that the
# guest's numbers and §15.6's workstation numbers are the same numbers.
OUTPUT=${17:-}
# §46. Compare the region against the client's own bytes after every copy, and
# count the rows that differ. **Off unless asked for**, because it reads the
# whole buffer every commit -- the cost `--copy damage` exists to avoid -- and
# because it is an instrument for correctness and must never be used to time
# anything on a topology whose latency is the point (§27.6).
#
# An 18th positional rather than an environment variable, because everything
# else this script needs is passed the same way and a flag that arrives by a
# different route is a flag that is set in one place and read in another.
VERIFY=${18:-}
VERIFY_ARG=""
[ -n "$VERIFY" ] && VERIFY_ARG="--verify-damage"
# Unique per invocation. Reusing a name across runs cost an hour: killing the
# ssh session that started a compositor kills the compositor, but the socket
# node it bound survives it, and the next `bind` then fails with "Could not
# write to XDG_RUNTIME_DIR" -- which reads as a permissions problem and is a
# leftover file.
SOCK="wg-s4-$$"
HERE=$(cd "$(dirname "$0")" && pwd)
ARENA_SIZE=$((64 * 1024 * 1024))
GK=(-i "$GKEY" -o IdentitiesOnly=yes -o BatchMode=yes -o LogLevel=ERROR)

RUN=$(mktemp -d /tmp/wc-s4-XXXXXX); chmod 755 "$RUN"
trap 'rm -rf "$RUN"' EXIT

rm -rf "$SHARE/frames" "$SHARE/live" "$SHARE/keymaps"
mkdir -p "$SHARE/frames" "$SHARE/live" "$SHARE/keymaps"
chmod 777 "$SHARE/frames" "$SHARE/live" "$SHARE/keymaps"; rm -f "$SHARE/s4.log"

# STAGE 6, and this block would fail without it. The compositor now has a
# keyboard, so a client that binds `wl_seat` receives a `wl_keyboard.keymap` —
# an `h` in the *server-to-client* direction. Published as a `Local\wc-…`
# section it is unopenable by a Linux process and `wc-proxy` ends the connection
# with a diagnosis, which is what happened to `wc-proxy.sh` the moment stage 6
# landed: `weston-terminal` binds a seat and `weston-presentation-shm` does not,
# so exactly one of the two clients here starts failing. See docs/design.md
# §11.2.
#
# **This is also the measurement §11.11 item 3 asks for, and it is not free.**
# §10.3 found the export incoherent for two *live* mappings and coherent for a
# closed write — and a keymap is a closed write, because `write_section` unmaps
# before returning. What §10.3 did not measure is this direction: its
# closed-write row is host-writes/guest-reads, and a keymap is
# guest-writes/host-reads. So a failure here is a *result* rather than a
# regression, and the proxy's error message names the descriptor kind so it can
# be told apart from anything else.
#
# **STAGE 10 removes this for one of the three transports, and that is the
# point of the stage.** With `guest-ram` the compositor reserves a window of the
# arena it already provided and cuts the keymap out of it, so the object has no
# name on any filesystem and `--shared-dir` is not passed at all. The other two
# transports keep it: `shared-file` has no arena to cut from, and the `arena`
# transport's region is a file on the very export whose incoherence this is
# trying to get away from, so pointing at it here would prove nothing.
KEYMAP_EXTRA=(--shared-dir "$SHARE_WIN\\keymaps" --shared-peer-prefix "$SHARE/keymaps/")

case "$TRANSPORT" in
  arena)
    # STAGE 41 licensed this branch as a CONTROL for §12.6's incoherence: a file
    # arena on the export, run beside the guest-RAM legs. It has not been
    # runnable since the compositor began refusing a file arena outright (§28.2,
    # `file_arena_verdict`): `SV_ALLOW_FILE_ARENA` was exported here, on the VM
    # host, while the compositor it was meant for runs on the guest over an ssh
    # that never sees this environment. So the leg asked for a substrate the
    # compositor will not serve, and failed as "no xdg_toplevel", which reads as
    # a protocol failure. Guest RAM is the only arena on a real VM; the export's
    # incoherence stays block 7's to measure, with no compositor in the way.
    # Refused by name rather than deleted, so a caller that asks is told why.
    echo "S4-FAIL: the file arena is not a substrate on a real VM (§28.2); use guest-ram"
    exit 2 ;;
  shared-file)
    COMP_EXTRA=()
    PROXY_EXTRA=(--transport shared-file --pool-dir "$SHARE/live" --peer-prefix "$SHARE_WIN\\live\\") ;;
  guest-ram)
    # STAGE 7. Nothing is provided out of band here, because the *guest*
    # provides it: the compositor commits and pins the region inside the VM and
    # this side goes and finds it. So there is no file to truncate, no path to
    # tell the compositor about, and PROXY_EXTRA cannot be filled in yet -- the
    # plan does not exist until the compositor has printed its run id.
    ARENA_SIZE=$((PAGES * 4096))
    # STAGE 41. Actively removed rather than merely not set, so that a value
    # inherited from the shell of somebody who ran the control an hour ago
    # cannot license a file if this leg's locate fails. This leg has no file to
    # fall back to and must not acquire one.
    unset SV_ALLOW_FILE_ARENA
    COMP_EXTRA=(--arena-guest-ram "0:$ARENA_SIZE" --arena-outbound "0:$ARENA_OUTBOUND")
    # STAGE 10: no shared directory. The keymap comes out of the arena, so
    # there is nothing for the compositor to publish and nothing for this side
    # to open. If the run still passes with this empty, the last reason the
    # guest needed a shared filesystem is gone.
    KEYMAP_EXTRA=()
    PROXY_EXTRA=() ;;
  *) echo "S4-FAIL: unknown transport $TRANSPORT"; exit 2 ;;
esac

# STAGE 12. How the client is started, decided once and used by both branches
# below — the plain one and the root-transient-unit one — because the two must
# run the same command and a second spelling of it is a second thing to get
# wrong.
#
# Firefox needs more of an environment than a weston demo does, and each piece
# earns its place. `env -u DISPLAY` because with an X server reachable
# `MOZ_ENABLE_WAYLAND=1` is a *preference* and Firefox will quietly use X11
# instead — a silent fallback, and this run has nothing else that would notice.
# `HOME` because the host-side half runs as root in a transient unit with no
# home of its own, and a Firefox that cannot write one starts and exits. A
# fresh `--profile` and `--no-remote` because an already-running Firefox would
# take the request and open a tab in *it*, which looks exactly like a client
# that connected and drew nothing.
if [ "$CLIENT" = firefox ]; then
  if [ -z "$FIREFOX" ]; then echo "S4-FAIL: no firefox path given"; exit 2; fi
  # `$RUN/xdg` rather than `$RUN`, and it is the same refusal as the home
  # directory's: Firefox checks the ownership of `$XDG_RUNTIME_DIR` too, and
  # `$RUN` belongs to the user who invoked this script while the client runs as
  # root. So the client gets a runtime directory of its own and is pointed at
  # the proxy's socket by **absolute path** — libwayland reads a `WAYLAND_DISPLAY`
  # that starts with `/` as one, which is the whole reason the two can be
  # separated.
  mkdir -p "$RUN/profile" "$RUN/home" "$RUN/xdg"
  CLIENT_ENV="env -u DISPLAY HOME=$RUN/home XDG_RUNTIME_DIR=$RUN/xdg \
WAYLAND_DISPLAY=$RUN/wl-proxy MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland"
  CLIENT_CMD="$FIREFOX --no-remote --profile $RUN/profile about:blank"
else
  CLIENT_ENV=""
  CLIENT_CMD="$WESTON/$CLIENT"
fi
OUT_EXTRA=()
[ -n "$OUTPUT" ] && OUT_EXTRA=(--output "$OUTPUT")

# Leftover socket nodes only. This used to `taskkill //F //IM wc-compositor.exe`,
# which is correct for one experiment on one guest and wrong the moment two are
# running: an image-name kill takes out somebody else's compositor as well as
# any of ours, and the symptom lands in *their* run rather than in this one.
# What made the kill necessary was socket reuse, and $$-unique names plus
# --timeout already deal with that.
ssh "${GK[@]}" "$G" "rm -f /c/Users/<guest-user>/AppData/Local/waycompose/wg-s4-* 2>/dev/null; true" >/dev/null 2>&1

# The export's own leftovers, and this one produced a wrong answer rather than a
# missing one. The frame list is written into $SHARE by the guest and read back
# here by name. Nothing truncated it between runs, so a run whose list was
# SHORTER than its predecessor's read its own entries followed by the tail of
# the previous run's -- 16397 frames for a 16384-page arena, jumping backwards
# from page 16384 to 16373, and wc-arena-host rightly refused it as a list with
# a hole. A stale scroll.log is worse still: it would report a scroll from the
# run before this one as if it were this one's.
rm -f "$SHARE/arena.frames" "$SHARE/scroll.log" "$SHARE/s4.log" 2>/dev/null || true

# How long the compositor lives. Everything else in this script is timed from
# it, and `guest-ram` needs the extra because the *locate* happens between the
# compositor starting and the client running: a sweep of 13 GiB of guest RAM is
# 2.4 s (§12.5) and the round trip through `rootc` and `systemd-run` around it is
# not. A compositor that timed out mid-run presents as a client that connected
# and then found the socket closed, which is a long way from the clock.
COMP_SECS=$((SECS + 45))
[ "$TRANSPORT" = guest-ram ] && COMP_SECS=$((SECS + 90))

# The scroll probe rides the compositor's OWN ssh, and that is the whole fix.
# Windows OpenSSH gives every connection its own session and window station, and
# window enumeration only ever sees the calling thread's desktop. A probe over a
# second ssh therefore polled an empty desktop for its full timeout and reported
#   probe-scroll: no visible window of class 'WaycomposeToplevel' after 150 s
# while the compositor, in the other session, had a real HWND with 45 frames
# blitted into it. Same ssh, same station, and FindWindowW can see it. This is
# the third harness to meet the session boundary; see wc-input.sh.
SCROLL_PRE=""
if [ -n "$VERIFY" ]; then
  SCROLL_PRE="(./probe-scroll.exe --class WaycomposeToplevel --wait $((SECS)) --notches ${WC_NOTCHES:-60} --gap ${WC_GAP:-150} --settle 6000 --log '$SHARE_WIN\\scroll.log' >scroll.out 2>&1 &) ; "
  echo "S4-SCROLL: probe-scroll.exe rides the compositor's own session"
fi
ssh "${GK[@]}" "$G" "export MSYS_NO_PATHCONV=1; cd $GDIR && $SCROLL_PRE ./wc-compositor.exe \
  --socket $SOCK --timeout $COMP_SECS --dump-frames '$SHARE_WIN\\frames' \
  --arena-frames '$SHARE_WIN\\arena.frames' \
  --log '$SHARE_WIN\\s4.log' ${OUT_EXTRA[*]@Q} ${COMP_EXTRA[*]@Q} ${KEYMAP_EXTRA[*]@Q}" >"$RUN/comp.out" 2>&1 &
COMP=$!
sleep 8

# ---------------------------------------------------------------- locate -----
# STAGE 7's ordering, and it is the whole reason this transport needs a step the
# other two do not. The guest allocates and stamps; only then can the host find
# the pages. The compositor prints its run id after the stamps are written, so
# waiting for that line is waiting for the region to be findable -- a sweep
# started earlier returns zero pages, which reads exactly like the arena not
# being shared at all.
if [ "$TRANSPORT" = guest-ram ]; then
  RUNID=""; FOUNDPAGES=""
  for _ in $(seq 1 30); do
    line=$(grep -o "arena 0 provided from guest RAM: run 0x[0-9a-f]*, [0-9]* bytes, [0-9]* pages, locked=[a-z]*" "$SHARE/s4.log" 2>/dev/null | tail -1)
    if [ -n "$line" ]; then
      RUNID=$(echo "$line" | grep -o "run 0x[0-9a-f]*" | cut -d' ' -f2)
      FOUNDPAGES=$(echo "$line" | grep -o "[0-9]* pages" | cut -d' ' -f1)
      echo "S4-GUESTARENA: $line"
      break
    fi
    sleep 1
  done
  if [ -z "$RUNID" ]; then
    echo "S4-FAIL: the compositor never reported a guest-RAM arena"
    sed -n '1,20p' "$SHARE/s4.log" 2>/dev/null
    kill $COMP 2>/dev/null; exit 1
  fi
  # THE FRAME LIST, AND WHY IT IS NOT OPTIONAL IN PRACTICE
  #
  # The compositor wrote the page frame behind every page of its arena into the
  # share, and translating that is what the locate should do. The sweep it
  # replaces is not merely slow (0.064 s against 2.308 s), it is **unreliable on
  # a loaded machine**: five consecutive runs of this harness gave `16377 of
  # 16384 pages`, `1 page index claimed twice`, `28 page index(es) claimed
  # twice`. Those duplicates are not stale stamps -- the run id hashes the
  # millisecond with the pid, and the tool counts a previous run's stamps
  # separately as `other_runs` -- they are two pages carrying *this* run's id at
  # one page index, found by a search that reads 13 GiB of contents which are
  # allowed to change while it reads them.
  #
  # Reading the kernel's page-frame database instead makes that entire class of
  # failure inexpressible, because no stamp is read at all.
  #
  # Conditional on the file existing rather than required, because the guest may
  # be a build without the query or an account without the privilege, and a
  # harness that refused to run at all would be a worse regression than a slow
  # locate. The locate line below says which locator ran.
  FRAMES_ARG=""
  if [ -s "$SHARE/arena.frames" ]; then
    FRAMES_ARG="--frames '$SHARE/arena.frames'"
    echo "S4-FRAMES: $(wc -l <"$SHARE/arena.frames") frames from the guest; locating by translation"
  else
    echo "S4-FRAMES: none; falling back to the content sweep (slow, and the duplicate-index failure is possible)"
  fi
  # Root, and only for this: QEMU is a system service here, so its QMP socket
  # and its /proc/<pid>/fd are root-only. See the header.
  echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$HERE/wc-arena-host --qmp '$QMP' --vm '$VM_HINT' --run $RUNID --pages $FOUNDPAGES \
  --outbound $ARENA_OUTBOUND $FRAMES_ARG --plan '$RUN/arena.plan' >'$RUN/locate.log' 2>&1
echo \"rc=\$?\" >>'$RUN/locate.log'
chmod a+r '$RUN/locate.log' '$RUN/arena.plan' 2>/dev/null; true" | eval "$ROOTC" >/dev/null 2>&1
  echo "S4-LOCATE-BEGIN"; cat "$RUN/locate.log" 2>/dev/null; echo "S4-LOCATE-END"
  if ! grep -q "^rc=0$" "$RUN/locate.log" 2>/dev/null; then
    echo "S4-FAIL: wc-arena-host did not locate the arena"
    kill $COMP 2>/dev/null; exit 1
  fi
  PROXY_EXTRA=(--transport arena --arena-plan "$RUN/arena.plan" --arena-id 0)
fi

ssh "${GK[@]}" "$G" "export MSYS_NO_PATHCONV=1; cd $GDIR && ./probe-relay.exe $PORT \
  'C:\\Users\\<guest-user>\\AppData\\Local\\waycompose\\$SOCK' --timeout $((SECS + 30))" >"$RUN/relay.out" 2>&1 &
RELAY=$!
sleep 4

ssh "${GK[@]}" -N -L "$RUN/up.sock:127.0.0.1:$PORT" "$G" >"$RUN/fwd.out" 2>&1 &
FWD=$!
sleep 4

# §46. The scroll, injected **on the guest**, because that is where the
# compositor's window is. Started here rather than after the client because
# `probe-scroll.exe` polls for a window of class `WaycomposeToplevel` for
# `--wait` seconds and then scrolls it: launching it early costs nothing and
# removes a race against however long Firefox takes to map on a given day.
#
# Backgrounded and never waited on. Its exit status is not the measurement --
# the measurement is the proxy's stale count -- and a probe that failed to find
# the window would show up as a run with no scroll in it, which the commit floor
# catches. Blocking the harness on it would trade a detectable failure for a
# hang.
# Launched above, inside the compositor's ssh. Not a separate connection any
# more, so there is no pid here to hold and nothing to wait on.
SCROLL=0

PROXY=0
if [ ! -S "$RUN/up.sock" ]; then
  echo "S4-FAIL: the ssh -L forward produced no socket"
elif [ "$TRANSPORT" != guest-ram ]; then
  XDG_RUNTIME_DIR="$RUN" "$HERE/wc-proxy" --upstream "$RUN/up.sock" --socket wl-proxy \
    --runtime-dir "$RUN" "${PROXY_EXTRA[@]}" --copy damage $VERIFY_ARG \
    --accept-timeout 20 --timeout $((SECS + 5)) >"$RUN/proxy.log" 2>&1 &
  PROXY=$!
  sleep 2
  XDG_RUNTIME_DIR="$RUN" WAYLAND_DISPLAY=wl-proxy $CLIENT_ENV timeout "$SECS" $CLIENT_CMD \
    >"$RUN/cli.log" 2>&1
  echo "S4-CLIENT-RC: $?"
else
  # The proxy has to open /proc/<qemu-pid>/fd/<n>, which is root-only on a
  # machine where QEMU is a system service, so the proxy and its client run in
  # one transient unit rather than here. `systemd-run --collect` rather than a
  # long-lived root shell, because a root call that blocks for a minute
  # frequently comes back with no output at all -- the unit's log survives it.
  cat >"$RUN/hostside.sh" <<HOSTSIDE
#!/usr/bin/env bash
export PATH=/run/current-system/sw/bin:/run/wrappers/bin
export XDG_RUNTIME_DIR="$RUN"
# This unit is root — §12.9, because /proc/<qemu-pid>/fd is — and Firefox
# refuses to start as root when \$HOME belongs to somebody else:
#   "Running Firefox as root in a regular user's session is not supported."
# It is a real check against a real mistake and the answer is to give the run
# its own home rather than to argue with it. The alternative, dropping to the
# invoking user for the client alone, would put the Wayland socket and the
# client on two sides of a permission boundary for no gain: the socket is in
# this same root-owned directory.
chown -R 0:0 "$RUN/home" "$RUN/profile" "$RUN/xdg" 2>/dev/null
chmod 700 "$RUN/xdg" 2>/dev/null
"$HERE/wc-proxy" --upstream "$RUN/up.sock" --socket wl-proxy \\
  --runtime-dir "$RUN" ${PROXY_EXTRA[*]@Q} --copy damage $VERIFY_ARG \\
  --accept-timeout 20 --timeout $((SECS + 5)) >"$RUN/proxy.log" 2>&1 &
P=\$!
sleep 2
WAYLAND_DISPLAY=wl-proxy $CLIENT_ENV timeout $SECS $CLIENT_CMD >"$RUN/cli.log" 2>&1
echo "S4-CLIENT-RC: \$?" >"$RUN/cli.rc"
sleep 3
kill \$P 2>/dev/null; wait 2>/dev/null
chmod -R a+rwX "$RUN" 2>/dev/null
touch "$RUN/hostside.done"
HOSTSIDE
  chmod 755 "$RUN/hostside.sh"
  UNIT="wc-s7-$$"
  # `systemd-run` gives the unit systemd's own PATH, which on NixOS contains no
  # `bash` and no `env` — so a `#!/usr/bin/env bash` script exits 127 before its
  # first line and the only trace is `env: 'bash': No such file or directory` in
  # the journal. Naming the interpreter is the fix; the shebang stays for anyone
  # running the file by hand.
  BASH=$(command -v bash)
  echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
systemd-run --unit=$UNIT --collect '$BASH' '$RUN/hostside.sh' >/dev/null 2>&1
echo started" | eval "$ROOTC" >/dev/null 2>&1
  for _ in $(seq 1 $((SECS + 40))); do
    [ -f "$RUN/hostside.done" ] && break
    sleep 1
  done
  if [ -f "$RUN/cli.rc" ]; then
    cat "$RUN/cli.rc"
  else
    echo "S4-FAIL: the host-side unit never finished"
    # Its own journal, because a unit that never started leaves nothing in $RUN
    # and the failure is otherwise invisible from here.
    echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
journalctl -u $UNIT --no-pager 2>&1 | tail -15" | eval "$ROOTC" 2>&1 | sed 's/^/  unit: /'
  fi
fi

sleep 3
# `kill 0` is not a no-op: it signals the whole process group, which is this
# script. PROXY is 0 whenever the proxy was not started here -- which used to be
# only the error path and is now the normal one for guest-ram -- so the zero has
# to be filtered rather than passed through. The symptom was a driver that
# printed the client's exit code and then nothing at all.
for p in $PROXY $FWD $RELAY $COMP $SCROLL; do [ "$p" -gt 0 ] 2>/dev/null && kill "$p" 2>/dev/null; done
wait 2>/dev/null
sleep 1

# What the compositor presented, counted from the files it dumped rather than
# from anything it said. `distinct` against `total` is the whole of the
# staleness measurement: a client that redraws every frame and a compositor
# that sees every redraw make those two numbers close.
tot=$(ls "$SHARE/frames"/*.ppm 2>/dev/null | wc -l)
if [ "$tot" -gt 0 ]; then
  md5sum "$SHARE/frames"/*.ppm | cut -d' ' -f1 | sort > "$RUN/h"
  dis=$(uniq -c "$RUN/h" | wc -l)
  top=$(uniq -c "$RUN/h" | sort -rn | head -1 | awk '{print $1}')
  geo=$(head -2 "$(ls "$SHARE/frames"/*.ppm | head -1)" | tail -1)
else
  dis=0; top=0; geo=none
fi
echo "S4-FRAMES: total=$tot distinct=$dis top=$top first=$geo"

echo "S4-PROXY-BEGIN";  cat "$RUN/proxy.log"; echo "S4-PROXY-END"
echo "S4-COMP-BEGIN";   cat "$SHARE/s4.log" 2>/dev/null; echo "S4-COMP-END"
echo "S4-RELAY-BEGIN";  cat "$RUN/relay.out"; echo "S4-RELAY-END"
# §57. The scroll probe's own log, fetched from the guest. Without this the
# only evidence the wheel moved is that the probe did not crash, and a locked
# Windows session makes SendInput return 0 while the probe exits happily.
if [ -n "$VERIFY" ]; then
  echo "S4-SCROLLLOG-BEGIN"
  # Read from the export, not over ssh: the probe now writes into $SHARE_WIN so
  # the driver can read it locally, and one fewer connection is one fewer
  # session to get confused about.
  cat "$SHARE/scroll.log" 2>/dev/null
  echo "S4-SCROLLLOG-END"
fi
echo "S4-CLIENT-BEGIN"; head -20 "$RUN/cli.log"; echo "S4-CLIENT-END"
DRIVER
}

# ---------------------------------------------------------------- deploy -----
do_deploy() {
  echo "== deploy =="
  [ -n "${WC_XKB_MINGW:-}" ] || { echo "error: WC_XKB_MINGW unset -- run inside \`nix develop\`"; exit 2; }
  local cc=x86_64-w64-mingw32-gcc

  for p in accept relay ioctl unixproc clock; do
    if $cc -O1 -o "$STAGE/probe-$p.exe" "$ROOT/scripts/probe-$p.c" -lws2_32 2>"$STAGE/cc.log"; then
      echo "  built probe-$p.exe"
    else
      echo "  FAILED to build probe-$p.exe"; sed 's/^/        /' "$STAGE/cc.log"; exit 1
    fi
  done

  # §46's scroll probe, which needs `-mwindows` (it has no console) and no
  # sockets. It is deployed here rather than by `wc-damage.sh` because the
  # cross-VM leg's compositor runs *on the guest*, so the thing that scrolls its
  # window has to be on the guest too -- see `do_damage`.
  if $cc -O1 -mwindows -o "$STAGE/probe-scroll.exe" "$ROOT/scripts/probe-scroll.c" 2>"$STAGE/cc.log"; then
    echo "  built probe-scroll.exe"
  else
    echo "  FAILED to build probe-scroll.exe"; sed 's/^/        /' "$STAGE/cc.log"; exit 1
  fi
  if $cc -O1 -o "$STAGE/probe-xkb.exe" "$ROOT/scripts/probe-xkb.c" \
       -I"$WC_XKB_MINGW_DEV/include" -L"$WC_XKB_MINGW/lib" -lxkbcommon 2>"$STAGE/cc.log"; then
    echo "  built probe-xkb.exe"
  else
    echo "  FAILED to build probe-xkb.exe"; sed 's/^/        /' "$STAGE/cc.log"; exit 1
  fi

  # The keymap is generated on Linux from real RMLVO names, because there is no
  # xkeyboard-config tree on Windows. That is the production path, not a
  # fixture -- see scripts/gen-keymap.sh and docs/design.md §7.2.
  "$ROOT/scripts/gen-keymap.sh" us "$STAGE/us.xkb" >/dev/null || {
    echo "  FAILED to generate a keymap"; exit 1; }
  echo "  generated us.xkb ($(wc -c < "$STAGE/us.xkb") bytes)"

  # probe-filemap.exe has no `-lws2_32` and is the coherency half of block 7.
  if $cc -O1 -o "$STAGE/probe-filemap.exe" "$ROOT/scripts/probe-filemap.c" 2>"$STAGE/cc.log"; then
    echo "  built probe-filemap.exe"
  else
    echo "  FAILED to build probe-filemap.exe"; sed 's/^/        /' "$STAGE/cc.log"; exit 1
  fi

  # STAGE 42. **Existence is not freshness.** §35.4 measured a harness reporting
  # the previous tree's behaviour after `cargo test` declined to relink, and
  # §35.6 item 6 named this harness among the seventeen with no guard. Five
  # binaries here, so `fresh_all` says it once when they are all fine and names
  # each offender when they are not.
  local how_win="cargo build --target x86_64-pc-windows-gnu"
  local wins=() e
  for e in wc-server.exe wc-probe.exe wc-compositor.exe wc-xkb-probe.exe wc-poolcost.exe; do
    wins+=("$WIN_BIN/$e")
  done
  fresh_all "$how_win" "${wins[@]}" || exit 1
  for e in wc-server.exe wc-probe.exe wc-compositor.exe wc-xkb-probe.exe wc-poolcost.exe; do
    cp "$WIN_BIN/$e" "$STAGE/"
  done

  # A `.exe` still running holds its own file open, and `scp` over it fails as
  # `dest open ...: Failure` -- which reads as a permissions or disk problem and
  # is neither. A compositor left behind by a killed ssh session is the usual
  # cause; so is one that survived a guest reboot, which has been seen.
  # Only the copies in *this* deploy's directory.
  #
  # `taskkill //IM wc-compositor.exe` was the original, and stage 7 narrowed it
  # to "only when `WC_GUESTNAME` is unset". That guard is the wrong way round:
  # the default name is exactly what an agent that has not thought about sharing
  # the guest uses, so the blanket kill fired in precisely the case where two
  # runs collide. Stage 8 watched it happen from the other side — a compositor
  # in `wcdesk` photographed and driven into a resize loop by a run that
  # believed it owned the machine (docs/waycompose-design.md §13.9).
  #
  # An image name is not an identity here; the image *path* is, because every
  # experiment deploys its own copy into its own directory. `taskkill` cannot
  # filter on it and `Win32_Process.ExecutablePath` can.
  kill_in_dir "$GUESTDIR_WIN"

  g "mkdir -p $GUESTDIR" || exit 1
  scp "${SSHOPT[@]}" "$STAGE"/*.exe "$STAGE/us.xkb" "$GUEST:$GUESTNAME/" >/dev/null || exit 1
  echo "  copied $(ls "$STAGE" | grep -c . ) files to $GUEST:$GUESTDIR"
  # STAGE 42. The long comment above is about `scp` *failing* on a running
  # `.exe`; this is the case where it appears to succeed and the old bytes stay.
  # `scp` returning 0 cannot tell them apart, and on the GPU path a stale copy
  # presents as a transport fault — `finish_attach` refuses a wire-version skew
  # with "This is a version skew between two builds, not a transport fault."
  deployed_all "$STAGE" "$GUESTDIR" g "re-run $0 deploy with nothing of ours running on the guest" \
    wc-server.exe wc-probe.exe wc-compositor.exe wc-xkb-probe.exe wc-poolcost.exe || exit 1

  [ -n "$HOST" ] || { echo "  WC_HOST unset -- nothing deployed for stage 4"; return; }

  # The stage-4 half, onto the machine that shares a filesystem with the guest.
  # `probe-filemap-peer` is built here and copied, which is fine on a NixOS pair
  # that shares a store and is the first thing to suspect if it will not run
  # there; the check below is what turns that into a message rather than a
  # mystery.
  if gcc -O1 -o "$STAGE/probe-filemap-peer" "$ROOT/scripts/probe-filemap-peer.c" 2>"$STAGE/cc.log"; then
    echo "  built probe-filemap-peer (native)"
  else
    echo "  FAILED to build probe-filemap-peer"; sed 's/^/        /' "$STAGE/cc.log"; exit 1
  fi
  fresh "$LINUX_BIN/wc-proxy" "cargo build -p wc-proxy" || exit 1
  write_stage4_driver "$STAGE/wc-stage4"

  # `probe-filemap.c`'s pattern, generated here so that both the generator and
  # the verifier are this project's, and shipped rather than reproduced on the
  # host -- a second implementation of `(k * 7 + 3) & 0xff` is a second thing
  # that can be wrong.
  { for i in $(seq 0 4095); do printf '%o\n' $(( (i * 7 + 3) % 256 )); done; } \
    | while read -r o; do printf "\\$o"; done > "$STAGE/flush.bin"
  [ "$(wc -c < "$STAGE/flush.bin")" = 4096 ] || { echo "  FAILED to generate flush.bin"; exit 1; }

  # STAGE 7. The locator, built from its own manifest because it is not a
  # workspace member: it path-depends on shared-vulkan-base, which must not be
  # in the Windows cross gate's dependency graph. See its Cargo.toml.
  local locator="$ROOT/crates/wc-arena-host/target/debug/wc-arena-host"
  if cargo build --manifest-path "$ROOT/crates/wc-arena-host/Cargo.toml" >"$STAGE/cc.log" 2>&1; then
    echo "  built wc-arena-host (links shared_vulkan's sv-hostd)"
  else
    echo "  FAILED to build wc-arena-host"; sed 's/^/        /' "$STAGE/cc.log"; exit 1
  fi
  # The crate was restored as an ORDINARY WORKSPACE MEMBER, so cargo now puts it
  # in the workspace target dir. This line still named the crate-local tree it
  # built into when it had one, and the deploy died on a path rather than on a
  # build. Both layouts are tried, after the build, because an unbuilt binary is
  # absent from either and resolving first would always take the fallback.
  [ -x "$locator" ] || locator="$ROOT/target/debug/wc-arena-host"
  [ -x "$locator" ] || { echo "error: no $locator"; exit 1; }

  h "mkdir -p $HOST_DIR $SHARE" >/dev/null
  scp "${SSHOPT[@]}" "$STAGE/probe-filemap-peer" "$STAGE/wc-stage4" "$STAGE/flush.bin" \
      "$LINUX_BIN/wc-proxy" "$locator" "$HOST:$HOST_DIR/" >/dev/null || exit 1
  h "chmod +x $HOST_DIR/*" >/dev/null
  echo "  copied wc-proxy, wc-arena-host, probe-filemap-peer and the driver to $HOST:$HOST_DIR"
  local vers; vers=$(h "$HOST_DIR/probe-filemap-peer 2>&1 | head -1")
  case "$vers" in
    *usage*) echo "  probe-filemap-peer runs on $HOST" ;;
    *) echo "  WARNING: probe-filemap-peer will not run on $HOST: $vers" ;;
  esac
}

# ------------------------------------------------------------------- run -----

# Run a native libwayland client on this machine against a server on the guest,
# through probe-relay.exe and an ssh unix-socket forward. See the header.
#   relayed <server.exe> <socket-name> <port> <client...> ; logs land in $RUNDIR
relayed() {
  local server="$1" sock="$2" port="$3"; shift 3
  local chunk="${CHUNK:-0}" delay="${DELAY:-0}"
  local comp relay fwd

  ssh "${SSHOPT[@]}" "$GUEST" \
    "cd $GUESTDIR && ./$server --socket $sock --timeout 45" \
    >"$RUNDIR/$sock.server.log" 2>&1 &
  comp=$!
  sleep 5
  ssh "${SSHOPT[@]}" "$GUEST" \
    "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && ./probe-relay.exe $port '$APPDATA_WIN\\$sock' --timeout 35 --chunk $chunk --delay $delay" \
    >"$RUNDIR/$sock.relay.log" 2>&1 &
  relay=$!
  sleep 3
  ssh "${SSHOPT[@]}" -N -L "$RUNDIR/$sock:127.0.0.1:$port" "$GUEST" &
  fwd=$!
  sleep 3

  if [ -S "$RUNDIR/$sock" ]; then
    XDG_RUNTIME_DIR="$RUNDIR" WAYLAND_DISPLAY="$sock" timeout 15 "$@" \
      >"$RUNDIR/$sock.client.log" 2>&1
  else
    echo "no forward socket" >"$RUNDIR/$sock.client.log"
  fi
  # **Wait for the server to say the client left, not for a clock.** This was
  # `sleep 2`, and the fragmented leg failed all four milestones in one run and
  # passed them in the next, with a server log that -- read afterwards -- held
  # every one of them. The client finishes writing long before the server has
  # read it: with `--chunk 3 --delay 15` the relay drips 372 bytes at about
  # 200 B/s through a 15.6 ms timer tick, and then the server's log still has to
  # come back over this ssh. Killing that ssh two seconds after the client exits
  # reads a log that is still arriving.
  #
  # Both servers print `client … disconnected` when the relay closes its side,
  # and the relay closes only after it has forwarded everything the client
  # sent, so that line is the last milestone of the exchange. Bounded, so a
  # server that never says it fails the checks below rather than hanging.
  local _
  for _ in $(seq 1 60); do
    grep -q "disconnected" "$RUNDIR/$sock.server.log" 2>/dev/null && break
    sleep 0.5
  done
  kill $comp $relay $fwd 2>/dev/null
  wait $comp $relay $fwd 2>/dev/null
}

# The four milestones wc-xdg.sh asserts, against a guest log.
check_milestones() {
  local log="$1" label="$2"
  grep -q "wl_surface created"          "$log" && ok "$label: wl_surface created"            || bad "$label: no wl_surface"
  grep -q "xdg_toplevel created"        "$log" && ok "$label: xdg_toplevel created  <-- the stage-2 milestone" || bad "$label: no xdg_toplevel"
  grep -q "sent xdg_toplevel configure" "$log" && ok "$label: compositor sent a configure"   || bad "$label: no configure"
  grep -q "wl_surface commit"           "$log" && ok "$label: client committed the surface"  || bad "$label: no commit"
}

do_run() {
  echo "== 1. the transport, on afunix.sys =="
  # docs/waycompose-design.md §7.8 items 1 and 2. Both roles are separate processes: a pid
  # is only interesting when it is not our own.
  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && rm -f acc-*.log && (./probe-accept.exe server '$GUESTDIR_WIN\\acc.sock' > acc-srv.log 2>&1 &) && sleep 2 && ./probe-accept.exe client '$GUESTDIR_WIN\\acc.sock' > acc-cli.log 2>&1; sleep 1; cat acc-srv.log acc-cli.log" >"$RUNDIR/acc.log"
  if grep -q "RESULT: GETPEERPID returned the client's real pid" "$RUNDIR/acc.log"; then
    ok "SIO_AF_UNIX_GETPEERPID returns the peer's real pid  <-- never executed anywhere before"
    grep -E "GETPEERPID ok" "$RUNDIR/acc.log" | sed 's/^/        /'
  else
    bad "SIO_AF_UNIX_GETPEERPID"; sed 's/^/        /' "$RUNDIR/acc.log"
  fi
  if grep -q "uds_windows verdict: WOULD ACCEPT" "$RUNDIR/acc.log"; then
    ok "accept() reports a well-formed peer address; stock uds_windows would accept"
    grep -E "accept peer address" "$RUNDIR/acc.log" | sed 's/^/        /'
  else
    bad "accept() peer address"; grep -E "accept peer address|verdict" "$RUNDIR/acc.log" | sed 's/^/        /'
  fi

  # The control that makes the above mean something: an ioctl in the same
  # vendor range that certainly does not exist must still fail.
  gq "cd $GUESTDIR && ./probe-ioctl.exe" >"$RUNDIR/ioctl.log"
  if grep -q "WSAIoctl bogus vendor ioctl        ERR" "$RUNDIR/ioctl.log"; then
    ok "control: a bogus vendor ioctl still fails, so the range is not blanket-implemented"
  else
    bad "control: the bogus vendor ioctl did not fail"
  fi

  echo
  echo "== 2. the stage-1 handshake, both ends as Windows binaries =="
  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && rm -f hs-*.log && (./wc-server.exe --socket wg-hs --timeout 25 > hs-srv.log 2>&1 &) && sleep 3 && WAYLAND_DISPLAY=wg-hs ./wc-probe.exe > hs-probe.log 2>&1; WAYLAND_DISPLAY=wg-hs ./wc-probe.exe --shm > hs-shm.log 2>&1; sleep 1; echo '--probe--'; cat hs-probe.log; echo '--shm--'; cat hs-shm.log; echo '--server--'; cat hs-srv.log" >"$RUNDIR/hs.log"
  grep -q "PASS — handshake completed" "$RUNDIR/hs.log" \
    && ok "wc-probe.exe completed the handshake against wc-server.exe" \
    || { bad "handshake"; sed 's/^/        /' "$RUNDIR/hs.log"; }
  if grep -q 'SHM MAPPED' "$RUNDIR/hs.log" && grep -q 'head="WAYCOMPOSE-SHM-OK"' "$RUNDIR/hs.log"; then
    ok "an \`h\` argument crossed: the server opened the client's section by name and read both markers"
    grep -E "descriptor:|SHM MAPPED" "$RUNDIR/hs.log" | sed 's/^/        /'
  else
    bad "the fd shim did not deliver the section"; sed 's/^/        /' "$RUNDIR/hs.log"
  fi

  echo
  echo "== 3. a real third-party C client, into the Windows server =="
  if command -v wayland-info >/dev/null; then
    relayed wc-server.exe wg-info $((PORT_BASE + 0)) wayland-info
    if grep -q "wl_compositor" "$RUNDIR/wg-info.client.log" && grep -q "wl_shm" "$RUNDIR/wg-info.client.log"; then
      ok "wayland-info (libwayland C) enumerated the globals of the Windows server"
      grep -E "interface:" "$RUNDIR/wg-info.client.log" | sed 's/^/        /'
    else
      bad "wayland-info"; sed 's/^/        /' "$RUNDIR/wg-info.client.log"
    fi
  else
    nope "wayland-info not on PATH"
  fi

  echo
  echo "== 4. smithay, on real Windows =="
  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && rm -f cp-*.log && (./wc-compositor.exe --socket wg-cp --timeout 20 > cp-comp.log 2>&1 &) && sleep 4 && WAYLAND_DISPLAY=wg-cp ./wc-probe.exe > cp-probe.log 2>&1; WAYLAND_DISPLAY=wg-cp ./wc-probe.exe --shm > cp-shm.log 2>&1; cat cp-probe.log cp-shm.log" >"$RUNDIR/cp.log"
  # The handshake, and the five globals this block is named for -- by name, not
  # by count. It grepped for "5 globals advertised", which was the whole list
  # when stage 5 wrote it and has been wrong since the compositor grew a sixth:
  # it advertises 16 today, so the check failed on a compositor that was doing
  # more than it asked, and printed the list that proved it. A count asserts
  # nothing about which globals are there; the names below are what smithay
  # provides and what every later block depends on.
  local n_globals missing="" gl
  n_globals=$(grep -o "PASS — handshake completed, [0-9]* globals advertised" "$RUNDIR/cp.log" \
              | head -1 | grep -o "[0-9]* globals" | tr -dc 0-9)
  for gl in wl_compositor wl_subcompositor wl_shm xdg_wm_base wl_seat; do
    grep -qE "global [0-9]+: $gl v[0-9]+" "$RUNDIR/cp.log" || missing="$missing $gl"
  done
  if [ -z "$n_globals" ]; then
    bad "smithay: wc-probe.exe did not complete the handshake against wc-compositor.exe"
    sed 's/^/        /' "$RUNDIR/cp.log"
  elif [ -n "$missing" ]; then
    bad "smithay: the handshake completed but these globals are missing:$missing"
    grep -E "  global " "$RUNDIR/cp.log" | sort -u | sed 's/^/        /'
  else
    ok "wc-compositor.exe completed the handshake and advertises smithay's five core globals (of $n_globals)"
  fi
  grep -q "the server processed create_pool without error" "$RUNDIR/cp.log" \
    && ok "smithay's ShmState accepted a create_pool carrying a real named section" \
    || bad "smithay create_pool"

  if command -v weston-multi-resource >/dev/null; then
    relayed wc-compositor.exe wg-xdg $((PORT_BASE + 1)) weston-multi-resource
    check_milestones "$RUNDIR/wg-xdg.server.log" "weston-multi-resource"

    # docs/waycompose-design.md §7.8 item 3: three bytes at a time guarantees the server
    # reads a partial message and has to wait for the rest.
    CHUNK=3 DELAY=15 relayed wc-compositor.exe wg-frag $((PORT_BASE + 2)) weston-multi-resource
    check_milestones "$RUNDIR/wg-frag.server.log" "fragmented (3-byte writes, 15 ms apart)"
  else
    nope "weston-multi-resource not on PATH"
  fi

  echo
  echo "== 5. libxkbcommon, as a Windows binary =="
  gq "cd $GUESTDIR && ./probe-xkb.exe us.xkb" >"$RUNDIR/xkbc.log"
  grep -q "all checks passed" "$RUNDIR/xkbc.log" \
    && ok "probe-xkb.exe (C): every check passed, including the deliberate new_from_names failure" \
    || { bad "probe-xkb.exe"; sed 's/^/        /' "$RUNDIR/xkbc.log"; }
  gq "cd $GUESTDIR && ./wc-xkb-probe.exe us.xkb" >"$RUNDIR/xkbr.log"
  grep -q "all checks passed" "$RUNDIR/xkbr.log" \
    && ok "wc-xkb-probe.exe (the Rust crate): every check passed" \
    || { bad "wc-xkb-probe.exe"; sed 's/^/        /' "$RUNDIR/xkbr.log"; }

  echo
  echo "== 6. the machine itself =="
  gq "cd $GUESTDIR && ./probe-clock.exe" >"$RUNDIR/clock.log"
  sed 's/^/        /' "$RUNDIR/clock.log"
  note "Sleep(1) above is the number to read: the poll loop's 5 ms sleep cannot"
  note "be shorter than the platform timer tick. Under Wine it is ~1 ms."
  gq "cd $GUESTDIR && for e in wc-server.exe wc-compositor.exe; do s=\$(date +%s%N); ./\$e --socket ld\$e --timeout 1 >/dev/null 2>&1; f=\$(date +%s%N); echo \"\$e: \$(( (f-s)/1000000 )) ms wall, of which 1000 ms is the idle timeout\"; done" >"$RUNDIR/load.log"
  sed 's/^/        /' "$RUNDIR/load.log"
  note "docs/waycompose-design.md §7.8 item 6: the 56 MB debug binary costs nothing measurable to load."

  do_coherency
  do_poolcost
  do_stage4
  do_guestarena
}

# =============================================================================
# Stage 4 on the real boundary. docs/waycompose-design.md §9.11, all five items.
# =============================================================================

# ------------------------------------------------------------------ 7 --------
# §9.11 item 1. Is the shared filesystem coherent -- does a store one side makes
# through its own mapping become visible to the other with no flush and no
# reopen? That decides whether `SharedFile` exists on the real boundary, and
# `probe-filemap.c`'s header says why a one-way check is not enough.
do_coherency() {
  echo
  echo "== 7. the shared filesystem, and whether a mapping of it is coherent =="
  if [ -z "$HOST" ]; then
    nope "WC_HOST unset: no Linux machine here shares a filesystem with the guest"
    note "Set WC_HOST, WC_HOST_SHARE and WC_GUEST_SHARE to the two spellings of the export."
    return
  fi

  # THE CONTROL, AND IT IS THE POINT OF THE BLOCK. A probe that reports
  # "not coherent" is worth nothing unless it can report "coherent", so the same
  # binary runs first against a local NTFS path with two Windows processes: one
  # holds a mapping and has not closed it, the other opens and verifies. If that
  # says MISMATCH the measurement below is broken, not the mount.
  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && rm -f ctl.bin && (./probe-filemap.exe '$GUESTDIR_WIN\\ctl.bin' 65536 > ctl-w.log 2>&1 &) && sleep 2 && ./probe-filemap.exe --open '$GUESTDIR_WIN\\ctl.bin' 65536; sleep 5; echo '--writer--'; cat ctl-w.log" >"$RUNDIR/coh-ctl.log"
  if grep -q "^\[fm\] MATCH" "$RUNDIR/coh-ctl.log"; then
    ok "control: on local NTFS a second process sees the first's live stores, unflushed"
  else
    bad "control: NTFS was not coherent either -- the probe is broken, not the mount"
    sed 's/^/        /' "$RUNDIR/coh-ctl.log" | head -8
  fi

  # The measurement. The Windows side creates and fills through its mapping and
  # waits for 0xAB; the Linux side verifies, stores 0xAB and waits for 0xCD.
  h "mkdir -p $SHARE; : >$SHARE/coh.bin; truncate -s 65536 $SHARE/coh.bin; chmod 666 $SHARE/coh.bin" >/dev/null
  ( gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && ./probe-filemap.exe '$SHARE_WIN\\coh.bin' 65536" >"$RUNDIR/coh-win.log" ) &
  local w=$!
  sleep 2
  h "$HOST_DIR/probe-filemap-peer $SHARE/coh.bin 65536" >"$RUNDIR/coh-lin.log"
  wait $w 2>/dev/null

  local w2l l2w
  grep -q "SAW the peer's write" "$RUNDIR/coh-win.log" && l2w=yes || l2w=no
  grep -q "peer\] pattern from the Windows side: MATCH" "$RUNDIR/coh-lin.log" && w2l=yes || w2l=no
  # **§31: what is asserted here is that the probes ran**, and the coherency
  # answer is recorded rather than scored.
  #
  # Both arms of this used to call `ok`, so the check could not fail: whichever
  # way the mount behaved, the scoreboard gained a pass. That is the right
  # *instinct* — an incoherent export really is a result rather than a defect,
  # §9.10 item 2 predicted it, and block 9 prices it — but a result belongs in a
  # `note`. A pass is a claim that something was checked, and "the mount did
  # one of the two things a mount can do" is not one.
  #
  # What can genuinely fail is the pair of probes producing no determination at
  # all, which is what an empty log looks like and which the old code reported
  # as "NOT coherent" — a broken probe wearing a finding's clothes.
  if ! [ -s "$RUNDIR/coh-win.log" ] || ! [ -s "$RUNDIR/coh-lin.log" ]; then
    bad "the coherency probes produced no output on one or both sides; there is no measurement here"
  elif [ "$w2l" = yes ] && [ "$l2w" = yes ]; then
    ok "both coherency probes reported, and the export is coherent in both directions: \`SharedFile\` is a live transport here"
  else
    ok "both coherency probes reported a determination"
    note "measured: the export is NOT coherent while mapped (guest->host $w2l, host->guest $l2w)"
    note "This is a result, not a failure: virtio-fs without DAX gives each side its own"
    note "pages, exactly as docs/design.md §9.10 item 2 predicted. Block 9 measures what"
    note "it costs a real client."
  fi
  sed 's/^/        /' "$RUNDIR/coh-lin.log" | head -3
  grep -E "SAW|NEVER" "$RUNDIR/coh-win.log" | sed 's/^/        /'

  # The weaker property, which is a different question and has a different
  # answer. `flush.bin` holds the same pattern, written and closed on the host
  # before the guest ever looks; the guest then opens it for the first time. So
  # this is host-writes-then-closes, guest-opens-fresh -- the direction the
  # pixels travel, without either side holding a mapping across the other's
  # write. That is the property a filesystem is obliged to have and the one
  # above is not, and separating them is what stops "not coherent" being read
  # as "the mount does not work".
  h "cp $HOST_DIR/flush.bin $SHARE/flush.bin; chmod 666 $SHARE/flush.bin; wc -c < $SHARE/flush.bin" >"$RUNDIR/flush-mk.log"
  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && ./probe-filemap.exe --open '$SHARE_WIN\\flush.bin' 4096" >"$RUNDIR/flush.log"
  if grep -q "^\[fm\] MATCH" "$RUNDIR/flush.log"; then
    ok "a closed write does cross: on a fresh open the guest maps the host's bytes exactly"
  else
    bad "even a closed write did not cross"; sed 's/^/        /' "$RUNDIR/flush.log"
  fi
}

# ------------------------------------------------------------------ 8 --------
# §9.11 item 2, which the section calls "the measurement that turns a preference
# into a decision": what a pool costs the receiver, per transport, across the
# hypervisor. `wc-poolcost` measures the shipping path -- `Handle::map` -- and
# not an imitation of it.
do_poolcost() {
  echo
  echo "== 8. what a pool costs the compositor, per transport =="
  if [ -z "$HOST" ]; then
    nope "WC_HOST unset: no shared filesystem, so no pool to open across one"
    return
  fi

  # §51. **This block's claims are about time across the boundary**, which is
  # what makes the declaration load-bearing here rather than bookkeeping: the
  # export is virtio-fs, so "the same open costs 1046 us over the export and
  # 24 on NTFS" is precisely a cross-VM latency claim. It is legitimate because
  # there is a hypervisor in this path -- and the identical sentence written in
  # a Wine harness would be §27.6 again.
  wc_topology_declare cross-vm \
    "the pools are opened by the compositor on the guest, across the export from $HOST" || return

  local n=32 size=1048576 arena=$((64 * 1024 * 1024))
  # The pool files are created **by the peer**, on the far side of the mount,
  # because that is who creates them in the real path and a file this process
  # made would be warm in the wrong cache.
  h "rm -rf $SHARE/pools; mkdir -p $SHARE/pools; for i in \$(seq 0 $((n - 1))); do truncate -s $size \$(printf $SHARE/pools/pc-%06d.bin \$i); done; : >$SHARE/arena; truncate -s $arena $SHARE/arena; chmod -R a+rw $SHARE/pools $SHARE/arena" >/dev/null

  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && ./wc-poolcost.exe --iters $n --size $size --sections --files '$SHARE_WIN\\pools' --arena '$SHARE_WIN\\arena:$arena'" >"$RUNDIR/cost-share.log"
  sed 's/^/        /' "$RUNDIR/cost-share.log"

  # The control that separates the hypervisor from Windows: the identical loop
  # over a local NTFS directory. Without it, a slow `CreateFileW` could be
  # Windows being slow rather than the mount, and the decision would rest on an
  # attribution nothing had checked.
  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && mkdir -p pools && ./wc-poolcost.exe --prepare '$GUESTDIR_WIN\\pools' --iters $n --size $size >/dev/null && ./wc-poolcost.exe --iters $n --size $size --files '$GUESTDIR_WIN\\pools'" >"$RUNDIR/cost-ntfs.log"
  sed 's/^/        /' "$RUNDIR/cost-ntfs.log"

  local sf ar ntfs
  sf=$(awk '/shared-file/ {print int($3)}' "$RUNDIR/cost-share.log" | head -1)
  ar=$(awk '/arena-slice/ {print int($3)}' "$RUNDIR/cost-share.log" | head -1)
  ntfs=$(awk '/shared-file/ {print int($3)}' "$RUNDIR/cost-ntfs.log" | head -1)

  if [ -n "$sf" ] && [ -n "$ntfs" ] && [ "$ntfs" -gt 0 ] && [ "$sf" -gt $((ntfs * 5)) ]; then
    ok "the mount, not Windows: the same open costs ${sf} us over the export and ${ntfs} us on NTFS"
  else
    bad "the export's open (${sf:-?} us) is not distinguishable from NTFS's (${ntfs:-?} us)"
  fi

  # The decision, stated as a ratio so it can come out either way. On one
  # machine §9.5 measured these the same; if they are the same here too, the
  # arena has no cost argument and this must say so.
  #
  # **§31: only one of the two outcomes is a pass.** Both arms called `ok`, so
  # this check could not fail; the second arm is a finding — "there is no cost
  # argument for the arena" — and a finding is a `note`. What is asserted is
  # that the arena slice is never the *slower* of the two, which is the one
  # answer that would mean the arena is doing something wrong rather than
  # something unnecessary, and which nothing here was watching for.
  if [ -n "$sf" ] && [ -n "$ar" ]; then
    if [ "$sf" -gt $((ar * 20 + 20)) ]; then
      ok "the decision: a pool costs ${sf} us as a shared file and ${ar} us as an arena slice (~$((sf / (ar + 1)))x)"
    elif [ "$ar" -le "$sf" ]; then
      ok "the arena slice is no more expensive than the shared file (${ar} us vs ${sf} us)"
      note "measured: the two transports cost the same here -- no cost argument for either"
    else
      bad "an arena slice costs ${ar} us against a shared file's ${sf} us; resolving an offset must not be dearer than opening a name"
    fi
  else
    bad "the cost measurement produced no numbers"
  fi
}

# ------------------------------------------------------------------ 9 --------
# §9.11 items 3, 4 and 5: the proxy reaching the guest through the relay, what a
# real client's pools do when they get there, and whether the run still reports
# the transport that actually served the pixels.
do_stage4() {
  echo
  echo "== 9. stage 4 across the real boundary: proxy -> ssh -L -> relay -> afunix =="
  if [ -z "$HOST" ]; then
    nope "WC_HOST unset: the proxy and the client need the shared filesystem"
    note "§9.11 item 3's shape is implemented and untested without one."
    return
  fi
  if [ -z "$HOST_WESTON" ]; then
    nope "WC_HOST_WESTON unset: no unmodified third-party client on $HOST"
    note "Our own binaries on both ends would be a matched pair; see the header."
    return
  fi

  # `term` -- weston-terminal over a FILE arena on the export -- is gone. The
  # compositor refuses a file arena on a real VM (§28.2, wc-substrate.sh), so
  # the leg failed as "no xdg_toplevel" on every run; `termg` below is the same
  # client over the substrate that replaced it, and `termf` keeps the export
  # as a pool transport, which the compositor still serves.
  s4_run termf shared-file weston-terminal         $((PORT_BASE + 12)) 12
  # STAGE 7. `weston-presentation-shm` over a *file* arena is what §10.7
  # measured -- 645 frames, 18 distinct, one image 462 of them -- and it is not
  # re-run here. §10.3 established that the incoherence is the mount's and not
  # the transport's, so re-measuring it every time would be re-measuring
  # virtio-fs. What is run instead is the same client over the region that
  # replaces it, and the frame check below is unchanged.
  s4_run pres  guest-ram   weston-presentation-shm $((PORT_BASE + 13)) 20
  s4_run termg guest-ram   weston-terminal         $((PORT_BASE + 14)) 12
}

# s4_run <tag> <transport> <client> <port> <seconds>
s4_run() {
  local tag=$1 transport=$2 client=$3 port=$4 secs=$5
  local log="$RUNDIR/s4-$tag.log"
  # `$HOST_WESTON` is quoted, and that is not tidying. It is empty for any leg
  # that runs Firefox rather than weston, and unquoted an empty value passes NO
  # argument at all -- so every positional after it shifted left by one, PAGES
  # received GDIR's "wctest", and the driver died on
  # `ARENA_SIZE=$((PAGES * 4096))` with "wctest: unbound variable", because bash
  # arithmetic reads a bare word as a variable name. Three guest runs of block 12
  # failed on this before the run directory was kept long enough to read.
  #
  # `${ROOTC@Q}` for the same family of reason. The driver runs it with
  # `eval "$ROOTC"`, so it has to arrive as one argument whatever it holds:
  # `sudokey run -- bash` is three words, and a value with its own quotes
  # broke the `\"…\"` this used to be. Single-quoted, the default
  # `$HOME/rootc` is still expanded -- by the eval, on the host, as before.
  echo "  -- $client / $transport --"
  h "$HOST_DIR/wc-stage4 $transport $client $port $SHARE '$SHARE_WIN' $HOST_GUEST \"$HOST_KEY\" '$HOST_WESTON' $secs '$QMP' '$VM_HINT' ${ROOTC@Q} $ARENA_PAGES $GUESTNAME $ARENA_OUTBOUND '$S4_FIREFOX' '$S4_OUTPUT' '$S4_VERIFY'" >"$log"

  if grep -q "S4-FAIL" "$log"; then
    bad "$tag: $(grep S4-FAIL "$log")"
    return
  fi

  # STAGE 7. The locate step, before anything the compositor says: every page of
  # the arena has to be where the guest said it was, or the pixels below are
  # landing in somebody else's memory and looking fine while they do it.
  if [ "$transport" = guest-ram ]; then
    local loc
    loc=$(grep -o "verify: [0-9]*/[0-9]* pages of the stitched view carry their own arena index" "$log")
    case "$loc" in
      "") bad "$tag: the locator printed no verify line"; sed -n '/S4-LOCATE-BEGIN/,/S4-LOCATE-END/p' "$log" | head -12 ;;
      *) local got want
         got=${loc#verify: }; got=${got%%/*}
         want=$(echo "$loc" | grep -o "/[0-9]* pages" | tr -dc 0-9)
         if [ "$got" = "$want" ] && [ "${got:-0}" -gt 0 ]; then
           ok "$tag: all $got pages of the guest's arena were found in QEMU's backend, each at its own index"
           # §51. The evidence the assertions read. Without this the locate
           # happened, printed its line into the driver log, and `wc_assert_substrate`
           # still said UNPROVEN -- correctly, because it can only see variables.
           # The gate was right and the wiring was missing.
           WC_SUBSTRATE_LOCATED=$got
           WC_SUBSTRATE_LOCATOR="wc-arena-host over QMP on $HOST"
         else
           bad "$tag: $loc"
         fi ;;
    esac
    local runs
    runs=$(grep -o "coalesced into [0-9]* run(s) covering [0-9]* MiB" "$log" | tail -1)
    [ -n "$runs" ] && note "$tag: $runs -- the guest allocator's fragmentation, as mmap calls"
    local stitch
    stitch=$(grep -o "stitched [0-9]* MiB into one view with [0-9]* mmap(MAP_FIXED) call(s) in [0-9.]*[a-z]*" "$log" | tail -1)
    [ -n "$stitch" ] && note "$tag: $stitch"
  fi

  grep -q "xdg_toplevel created" "$log" \
    && ok "$tag: an unmodified libwayland client reached xdg_toplevel on the guest's compositor" \
    || { bad "$tag: no xdg_toplevel"; sed -n '/S4-COMP-BEGIN/,/S4-COMP-END/p' "$log" | head -12; return; }

  grep -q "window .* created (HWND" "$log" \
    && ok "$tag: a real Windows HWND was created for it  <-- §8.11 item 1, on the guest" \
    || bad "$tag: no HWND"

  local presents
  presents=$(grep -o "([0-9]* frames presented)" "$log" | grep -o "[0-9]*" | tail -1)
  presents=${presents:-0}
  [ "$presents" -ge 2 ] \
    && ok "$tag: $presents frames of the client's pixels were blitted into it" \
    || bad "$tag: only $presents frame(s) presented"

  # §9.11 item 5. The compositor's own count of what it opened, which is the
  # only thing between "the pixels arrived" and "the pixels arrived the way we
  # think they did".
  local counts
  counts=$(grep -o "mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice" "$log" | tail -1)
  case "$transport:$counts" in
    "shared-file:mapped: 0 section, "[1-9]*" shared-file, 0 arena-slice")
      ok "$tag: the compositor opened only shared files by name ($counts)" ;;
    "arena:mapped: 0 section, 0 shared-file, "[1-9]*" arena-slice"| \
    "guest-ram:mapped: 0 section, 0 shared-file, "[1-9]*" arena-slice")
      ok "$tag: the compositor resolved only arena offsets, opening nothing ($counts)" ;;
    *)
      bad "$tag: wrong transport served the pixels: ${counts:-nothing recorded}" ;;
  esac

  # The descriptor rule, in the form stage 6 gave it. `wc-proxy.sh` was updated
  # when the keymap landed and this was not, because nobody ran the guest
  # blocks between stage 6 and stage 7 -- so `0 out` was still asserted here and
  # three of these runs failed on it the first time the two were run together.
  #
  # The number is no longer zero and that is not a violation. `wl_keyboard.keymap`
  # is an `h` in the *server-to-client* direction: the compositor publishes a
  # file on the share, the **name** crosses the boundary, and this proxy opens
  # that name locally and hands the client a descriptor it manufactured. Nothing
  # was relayed. What enforces the rule is that the proxy's upstream writer has
  # no fd path at all -- construction, not a counter -- and what this asserts is
  # the part a counter can see: every descriptor going out was made here.
  # docs/waycompose-design.md §14.7 is the full argument: which two processes that
  # descriptor exists between, why that pair is not the boundary, and what this
  # number would have to be for the rule to be broken.
  # STAGE 42. **The twenty lines above argue about which number would mean the
  # rule was broken, and the code then asserted only that a number exists.** Any
  # `"N fds in, M out"` passed, including one that showed descriptors being
  # relayed — which is the single thing this project must never do and the whole
  # subject of §14.7. A gate on the boundary claim of the stage that could not
  # fail.
  #
  # What is asserted now is the number the comment names. The proxy prints, for
  # example, `12 fds in, 2 out (2 from the arena)`, and the rule is that **every
  # descriptor going out was made here**: over guest RAM they come from the
  # arena, so `out` and `from the arena` must be equal. A descriptor that was
  # relayed rather than manufactured shows up as `out` exceeding the number
  # accounted for, which is exactly the shape §14.7 says to watch.
  local fds out fromarena
  fds=$(grep -o "[0-9]* fds in, [0-9]* out[^;]*" "$log" | tail -1)
  if [ -z "$fds" ]; then
    bad "$tag: the proxy reported no descriptor counts"
  else
    out=$(echo "$fds" | grep -o "[0-9]* out" | tr -dc 0-9)
    fromarena=$(echo "$fds" | grep -o "([0-9]* from the arena)" | tr -dc 0-9)
    if [ "$transport" = guest-ram ] && [ -n "$fromarena" ]; then
      if [ "${out:-0}" -eq "${fromarena:-0}" ]; then
        ok "$tag: all ${out:-0} descriptor(s) that went out were made here from the arena, none relayed ($fds)  <-- §14.7"
      else
        bad "$tag: ${out:-0} descriptor(s) went out but only ${fromarena:-0} came from the arena -- the rest were not accounted for ($fds)"
      fi
    elif [ "${out:-0}" -eq 0 ]; then
      ok "$tag: no descriptor went out at all ($fds)  <-- §14.7"
    else
      # The shared-file and file-arena controls manufacture theirs from a name
      # rather than from the arena, so there is no per-descriptor accounting
      # line to compare against. Stated as informational rather than dressed up
      # as a verdict this leg cannot reach.
      note "$tag: ${out:-0} descriptor(s) out, made here from a name; this transport prints no arena accounting ($fds)"
    fi
  fi

  # STAGE 10, and it is the claim of the stage on the deployment that matters.
  # Over guest RAM the compositor was started with **no `--shared-dir`**, so a
  # keymap can only have come out of its own window of the arena — three
  # integers, no name, no filesystem. A client that binds a seat is what makes
  # this assertable; `weston-presentation-shm` does not, so the check is on the
  # count being consistent with the client rather than on it being non-zero.
  if [ "$transport" = guest-ram ]; then
    case "$fds" in
      *"from the arena"*)
        ok "$tag: the keymap crossed as a slice of the guest's own RAM ($fds)  <-- §14" ;;
      *)
        note "$tag: no descriptor came from the arena ($fds) -- expected for a client that binds no seat" ;;
    esac
    if [ -z "$(ls -A "$SHARE/keymaps" 2>/dev/null)" ]; then
      ok "$tag: nothing was published to the export at all -- the share carried no protocol object"
    else
      bad "$tag: something landed in $SHARE/keymaps: $(ls -A "$SHARE/keymaps" | tr '\n' ' ')"
    fi
    grep -q "outbound window: offset" "$log" \
      && ok "$tag: $(grep -o 'arena 0 outbound window: offset [0-9]*, [0-9]* bytes' "$log" | head -1)" \
      || bad "$tag: the compositor reserved no outbound window"
  fi
  # And the measurement §11.11 item 3 asked for, on the deployment that matters:
  # the keymap is written by the *guest* and read by the *host*, which is the
  # opposite direction from §10.3's closed-write row and over a mount that
  # caches on the writing side. A name the compositor published and this side
  # could not open is that re-check coming back negative.
  if grep -q "cannot open" "$log"; then
    bad "$tag: the compositor named something this side could not open"
    grep "cannot open" "$log" | sed 's/^/        /' | head -3
  else
    ok "$tag: every name the compositor sent was openable on this side  <-- §11.11 item 3"
  fi

  # §9.11 item 4: the pool growth the deferral design rests on. What matters is
  # not the count -- see below, it is not a constant -- but that every resize
  # was absorbed before anything else was asked of the pool.
  #
  # **The fields are read by name, not by position.** This matched the exact
  # sequence `N pools (R released, S resizes, O overflows)`, and stage 16 put
  # `reclaimed` and `republished` between the first two and appended
  # `refusals` and `attaches deferred` after the last -- so it matched nothing
  # and printed an empty FAIL on three legs that had no overflow at all. The
  # old `*"0 overflows)"` also accepted `10 overflows)`.
  local pools overflows
  pools=$(grep -o "[0-9]* pools ([^)]*)" "$log" | tail -1)
  overflows=$(echo "$pools" | grep -o "[0-9]* overflows" | tr -dc 0-9)
  if [ -z "$pools" ] || [ -z "$overflows" ]; then
    bad "$tag: the proxy printed no pool counts with an overflow field (${pools:-nothing})"
  elif [ "$overflows" -eq 0 ]; then
    ok "$tag: $pools -- every resize arrived before the pool was used"
  else
    bad "$tag: $overflows overflow(s): $pools"
  fi

  # And what the frames themselves say. `distinct` well below `total` means the
  # compositor presented the same image many times: for a client that redraws
  # every frame that is a stale mapping, and it is invisible in every check
  # above.
  local fr tot dis top
  fr=$(grep "^S4-FRAMES:" "$log")
  tot=$(echo "$fr" | grep -o "total=[0-9]*" | cut -d= -f2)
  dis=$(echo "$fr" | grep -o "distinct=[0-9]*" | cut -d= -f2)
  top=$(echo "$fr" | grep -o "top=[0-9]*" | cut -d= -f2)
  note "$tag: ${tot:-0} frames dumped, ${dis:-0} of them distinct, the commonest repeated ${top:-0} times"
  # **§31: the floor under this, which applies to every client.** The staleness
  # test below runs only for `weston-presentation-shm`, so for every other
  # client these three numbers were printed in a `note` and compared with
  # nothing — including the case that makes the whole block meaningless, which
  # is `total=0`. A run whose frame dump produced no frames at all printed
  # "0 frames dumped, 0 of them distinct" and passed, with the staleness check
  # it exists for silently not running.
  if [ "${tot:-0}" -lt 1 ] || [ "${dis:-0}" -lt 1 ]; then
    bad "$tag: ${tot:-0} frames dumped and ${dis:-0} distinct; the frame instrument produced nothing to judge"
  else
    ok "$tag: the frame dump produced ${tot} frames, ${dis} of them distinct -- there is something here to judge"
  fi
  if [ "$client" = weston-presentation-shm ] && [ "${tot:-0}" -gt 20 ]; then
    # This client redraws every frame, so on a coherent region almost every
    # dumped frame differs. `top` is the number that catches a frozen mapping.
    if [ "${top:-0}" -lt $((tot / 4)) ]; then
      ok "$tag: the window is live -- no single image accounts for more than a quarter of $tot frames"
    else
      bad "$tag: the mapping is stale -- one image is $top of $tot frames from a client that redraws every one"
      note "Nothing above catches this: the HWND, the frame count and the transport line are"
      note "all correct while it happens. Over a file on virtio-fs it is expected and is"
      note "block 7's result arriving as pixels -- the mount is not coherent while mapped, so"
      note "the compositor reads its own cached copy (docs/waycompose-design.md §10.3, §10.7). Over"
      note "guest-ram it is not expected and means one of two things: the host is writing"
      note "into memory that is not the compositor's (check block 10's COH- lines, and the"
      note "verify line above), or the guest's pages moved after they were located, which"
      note "is what locked=false in the S4-GUESTARENA line would predict."
    fi
  fi
}

# ------------------------------------------------------------------ 11 -------
# STAGE 12. Firefox, on the real guest, over the guest's own RAM.
#
# This block exists to close two boxed items at once, and they are the same gap
# seen from opposite ends. §14.12's is that nothing in the compositor-side
# allocator had ever run over guest RAM — the Wine arena is a *file*. §15.14's
# is that Firefox had never touched an arena at all, because `--transport arena`
# refused the second connection and Firefox opens four. §15.15 item 2 says one
# guest run with Firefox on the far end closes both, and this is that run.
#
# **Why it is not just `s4_run ff guest-ram firefox` and nothing else.** The
# checks `s4_run` already makes are the right ones and they are made here
# unchanged — the transport line, the descriptor counts, the empty export, the
# outbound window. What this block adds is the three facts that are only
# interesting because the client is *this* client: that four connections were
# served over one region, that the compositor cut an object for more than one of
# them out of one window, and that the frame at the end of it is a browser
# rather than a black rectangle. The last is §15.5's measurement: Firefox's
# toplevel surface is transparent and every pixel of the browser is in a
# subsurface, so "a window appeared" and "the pixels arrived" are different
# claims and a colour count is what separates them.
do_firefox() {
  echo
  echo "== 11. Firefox over the guest's own RAM =="
  if [ -z "$HOST" ]; then
    nope "WC_HOST unset: the proxy has to run where QEMU is"
    return
  fi
  if [ -z "$HOST_FIREFOX" ]; then
    nope "WC_HOST_FIREFOX unset: no Firefox on $HOST"
    note "It is not on that machine's PATH, and this block will not guess a path for it."
    return
  fi

  # 1600x1000 so that the window size is §15.6's 1280x900 and the two runs are
  # comparable. The guest's real desktop size is §15.15 item 3 and not this
  # stage's errand.
  S4_FIREFOX="$HOST_FIREFOX"; S4_OUTPUT=1600x1000
  s4_run ff guest-ram firefox $((PORT_BASE + 15)) "${WC_FIREFOX_SECS:-60}"
  S4_FIREFOX=""; S4_OUTPUT=""

  local log="$RUNDIR/s4-ff.log"
  [ -s "$log" ] || { bad "the Firefox leg produced no log"; return; }

  # The stage's own claim. Until now `--transport arena` refused the second
  # connection **by name** (§15.14), so this number being four is the whole of
  # what changed in the proxy.
  local served joined
  served=$(grep -c '^\[proxy\] client [0-9]* connected' "$log")
  joined=$(grep -c 'joins arena 0' "$log")
  if [ "${served:-0}" -ge 2 ] && [ "$served" = "$joined" ]; then
    ok "$served Wayland connections were served and all $joined share one region and one allocator"
  else
    bad "$served connection(s) served, $joined joined the arena"
    grep -E 'no transport for it|already serving' "$log" | sed 's/^/        /' | head -3
  fi

  # §14.12 item 3: nothing had ever asked the outbound window for two live
  # objects outside a unit test. Firefox binds `wl_seat` twice — GTK's at v5 and
  # Gecko's at v8 (§15.2) — and gets a keymap for each, so two gifts are live in
  # one window at once, on the guest, out of pinned guest RAM. Measured, the two
  # binds are on **one** connection rather than one each, which is not what
  # §15.2's "two connections that matter" reads like; see §16.5.
  local arenafds
  arenafds=$(grep -o '([0-9]* from the arena)' "$log" | grep -o '[0-9]*' \
             | awk '{s+=$1} END {print s+0}')
  [ "${arenafds:-0}" -ge 2 ] \
    && ok "$arenafds objects the compositor created were live at once in its window of guest RAM  <-- §14.12 item 3" \
    || bad "only ${arenafds:-0} descriptor(s) came from the arena; two keymaps were expected"

  # The geometry rectangle, on the guest. §15.6 measured this on this
  # workstation and §15.14 item 1 says nothing in §15 has run anywhere else.
  local mapped
  mapped=$(grep -o 'window 1 mapped [0-9]*x[0-9]*' "$log" | tail -1)
  case "$mapped" in
    "window 1 mapped 1280x900") ok "the HWND is the geometry rectangle, on real Windows: $mapped" ;;
    "") bad "the window never mapped" ;;
    *)  bad "unexpected window size: $mapped (expected 1280x900 on a 1600x1000 output)" ;;
  esac

  # What the arena actually held, which is §15.10's table arriving on the
  # deployment it is about.
  local pools
  pools=$(grep -o '[0-9]* pools ([^)]*)' "$log" | tail -1)
  [ -n "$pools" ] && note "Firefox's pools through the arena: $pools"
  grep -o 'client [0-9]*: joins arena 0 ([0-9]* connection(s) on it now, [0-9]* of [0-9]* bytes out)' "$log" \
    | tail -1 | sed 's/^/  ----  /'

  # And the pixels. Fetched rather than trusted: the colour count is run here,
  # on a file the compositor wrote inside the VM, because §15.5's failure —
  # 1,268,064 pixels of #000000 with three different frame hashes — passes every
  # other check in this block.
  if ! have_magick; then
    note "no imagemagick here: the frame was not counted"
    return
  fi
  local last
  last=$(h "ls $SHARE/frames/window1-*.ppm 2>/dev/null | tail -1" | tr -d '\r')
  if [ -z "$last" ]; then
    bad "the compositor dumped no frame of window 1"
    return
  fi
  h "base64 <$last" | base64 -d >"$RUNDIR/ff-last.ppm" 2>/dev/null
  local colours
  colours=$(magick "$RUNDIR/ff-last.ppm" -format %k info: 2>/dev/null)
  colours=${colours:-0}
  if [ "$colours" -ge 50 ]; then
    ok "the last frame the compositor composited inside the VM has $colours distinct colours, so it is a rendered browser and not a black rectangle"
  else
    bad "the last frame has only $colours distinct colours -- §15.5's symptom, the subsurface is missing"
  fi
  local top bottom
  top=$(magick "$RUNDIR/ff-last.ppm" -format '%[pixel:p{640,20}]' info: 2>/dev/null)
  bottom=$(magick "$RUNDIR/ff-last.ppm" -format '%[pixel:p{640,700}]' info: 2>/dev/null)
  note "pixel at 640,20 = $top; at 640,700 = $bottom"
  [ -n "$top" ] && [ "$top" != "$bottom" ] \
    && ok "the chrome and the content area are different colours, so the window is not a flat fill" \
    || bad "the window is a flat fill"
  [ -n "${WC_KEEP:-}" ] && { cp "$RUNDIR/ff-last.ppm" /tmp/wc12-firefox-guest.ppm; note "kept /tmp/wc12-firefox-guest.ppm"; }
  true
}

# ---------------------------------------------------------------- damage -----
#
# §46. THE CROSS-VM LEG, and the one this project's second symptom needs.
#
# `scripts/wc-damage.sh` measures the damage path exhaustively and declares
# `same-box`, because Firefox, the proxy and the compositor all run under Wine
# on one Linux box there. That is the right harness for the copy's correctness
# -- `--verify-damage` compares two mappings inside one Linux process and Wine
# is only an upstream that consumes the stream -- and it is the wrong harness
# for anything about the boundary, which is §27.6's retraction in one sentence.
#
# This block is the same measurement with the boundary in it: the compositor on
# the guest, the proxy on the VM host over an arena located in the guest's own
# RAM, Firefox scrolling `about:support`, and `probe-scroll.exe` injecting the
# wheel **on the guest**, where the window is.
#
# WHAT IT CAN AND CANNOT SETTLE
#
# It settles the correctness question on the real substrate: whether the arrears
# bookkeeping holds when the region is guest RAM reached through QEMU rather
# than a file this side mapped. Nothing in §46's argument predicts a difference
# and the arena leg found none, but "the arena code path on a file" and "guest
# RAM through a hypervisor" are two claims and only one of them has been made.
#
# It does **not** on its own settle the user's second symptom. A stale-row count
# of zero says the bytes are right; it says nothing about when they arrive. What
# makes this leg able to speak to that is `wc_topology_declare cross-vm`, which
# is what `wc_latency_claim` requires before any assertion about the present may
# be made at all -- so a latency assertion added here later will be permitted,
# and the same assertion added to `wc-damage.sh` will be refused.
#
# **UNRUN.** testhost was down when this was written and nothing here has
# executed. It is assembled entirely from paths this harness already exercises
# -- `s4_run ff guest-ram firefox` is §17's, the scroll probe is §46's, and the
# proxy flag is one argument -- but §14.9's rule is that "implemented" has meant
# "compiled" twice on this project, so it is marked rather than counted. The
# first run of it is a measurement; until then it is a hypothesis with a script
# attached.
do_damage() {
  echo
  echo "== 12. the damage path, with a hypervisor in it =="
  if [ -z "$HOST" ]; then
    nope "WC_HOST unset: the proxy has to run where QEMU is"
    return
  fi
  if [ -z "$HOST_FIREFOX" ]; then
    nope "WC_HOST_FIREFOX unset: no Firefox on $HOST"
    return
  fi

  # The declarations, before anything runs, and both of them refuse rather than
  # default. `guest-ram` is the only substrate this leg may use and
  # `wc_substrate_declare` will not hand out the file-arena escape hatch to a
  # harness that has a guest configured; `cross-vm` is refused unless there is
  # both a VM host and a guest, so it cannot be declared on the Wine box.
  wc_substrate_declare guest-ram || return
  wc_topology_declare cross-vm \
    "compositor on $GUEST, proxy on $HOST, arena in the guest's own RAM" || return

  S4_FIREFOX="$HOST_FIREFOX"; S4_OUTPUT=1600x1000; S4_VERIFY=1
  s4_run dmg guest-ram firefox $((PORT_BASE + 16)) "${WC_DAMAGE_SECS:-90}"
  S4_FIREFOX=""; S4_OUTPUT=""; S4_VERIFY=""

  local log="$RUNDIR/s4-dmg.log"
  [ -s "$log" ] || { bad "the damage leg produced no log"; return; }

  grep -q "S4-SCROLL:" "$log" \
    && ok "probe-scroll.exe was launched on the guest, where the compositor's window is" \
    || bad "no scroll was injected; this leg measures an idle browser"

  # **Launched is not injected**, and on this guest that distinction is the
  # difference between a result and a fiction. §57: a *locked* Windows session
  # puts the input desktop on `Winlogon`, and a process on `Default` cannot
  # reach it -- `SendInput` returns 0, sets `ERROR_ACCESS_DENIED`, and the probe
  # runs to completion having moved nothing. Every number downstream would then
  # be a real measurement of an unscrolled browser, which is §27.6's shape
  # exactly: an experiment that did not contain the thing it was about.
  #
  # The probe now counts refusals and exits 3 when everything was refused. This
  # reads its accounting rather than trusting its exit, because the exit code
  # travels through `ssh` and a backgrounded job and this line does not.
  local scrolled
  scrolled=$(sed -n '/S4-SCROLLLOG-BEGIN/,/S4-SCROLLLOG-END/p' "$log" \
             | grep -o '[0-9]* of [0-9]* notches towards the user accepted' | tail -1)
  if [ -z "$scrolled" ]; then
    bad "dmg: the scroll probe never reported an accepted count; the wheel cannot be assumed to have moved"
    sed -n '/S4-SCROLLLOG-BEGIN/,/S4-SCROLLLOG-END/p' "$log" | sed 's/^/        /' | head -8
  elif grep -q "REFUSED by SendInput" "$log"; then
    bad "dmg: SendInput refused notches -- $scrolled. If the guest session is LOCKED this is expected and the run is void: unlock it and re-run."
    sed -n '/S4-SCROLLLOG-BEGIN/,/S4-SCROLLLOG-END/p' "$log" | grep -E "REFUSED|LOCKED|scrolled nothing" | sed 's/^/        /'
    return
  else
    ok "dmg: the guest accepted the wheel -- $scrolled"
  fi

  # The substrate and the topology, from the run's own evidence. `wc_locate_arena`
  # has set WC_SUBSTRATE_LOCATED by now if a locate ran, and `wc_assert_topology`
  # refuses a cross-vm claim without one -- a hypervisor in the path is exactly
  # what makes guest RAM locatable, so the locate is the proof.
  local comp="$RUNDIR/s4-dmg-comp.log"
  sed -n '/S4-COMP-BEGIN/,/S4-COMP-END/p' "$log" >"$comp"
  wc_assert_substrate "$comp" "dmg"
  wc_assert_topology "$comp" "dmg"

  # THE ASSERTION, and it is `wc-damage.sh`'s, unchanged, over a boundary.
  local verified skipped unreached stale worst commits
  verified=$(grep -o 'verified [0-9]* commits' "$log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  skipped=$(grep -o '([0-9]* state-only, unverifiable' "$log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  unreached=$(grep -o '[0-9]* never reached the copy' "$log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  stale=$(grep -o '[0-9]* stale (worst [0-9]* rows)' "$log" | awk -F' stale' '{print $1}' | awk '{s+=$1} END {print s+0}')
  worst=$(grep -o 'worst [0-9]* rows' "$log" | grep -o '[0-9]*' | sort -n | tail -1)
  commits=$(grep -o '[0-9]* commits,' "$log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')

  if [ "${verified:-0}" -eq 0 ]; then
    bad "dmg: no commit was checked -- --verify-damage did not reach the proxy on the host"
    sed -n '/S4-PROXY-BEGIN/,/S4-PROXY-END/p' "$log" | tail -8 | sed 's/^/        /'
    return
  fi

  # The identity, as §46.3 defines it: every commit is checked, state-only, or
  # never reached the copy. Carried over verbatim because a coverage claim that
  # is only made on one topology is a coverage claim about that topology.
  local classified=$(( ${verified:-0} + ${skipped:-0} + ${unreached:-0} ))
  [ "$classified" -eq "${commits:-0}" ] \
    && ok "dmg: every one of $commits commits is accounted for -- $verified checked, $skipped attached no buffer, $unreached never reached the copy" \
    || bad "dmg: $classified of $commits commits classified; $(( ${commits:-0} - classified )) unaccounted for"

  if [ "${stale:-0}" -eq 0 ]; then
    ok "dmg: over a real VM boundary, the region matched the client on every one of $verified checked commits"
  else
    bad "dmg: $stale of $verified commits left the region stale, worst ${worst:-?} rows -- and this is guest RAM, not a fixture"
    grep -o 'stale: .*' "$log" | head -6 | sed 's/^/        /'
  fi

  # **What this leg is now permitted to say, and the Wine legs are not.** The
  # gate is called for its permission rather than its refusal here, which is the
  # whole point of the declaration: the same call in `wc-damage.sh` fails and
  # this one passes, and the difference is a hypervisor.
  if wc_latency_claim "the present latency is measurable on this leg"; then
    ok "dmg: this leg may make a claim about cross-VM latency -- there is a boundary in it"
    note "The claim itself is not made here. §46.7 item 5: the doorbell's 0.876 ms"
    note "against 20.29 ms is somebody else's measurement, and pairing it with a"
    note "scroll-smoothness number is the next run's errand, not this block's."
  fi
}

have_magick() { command -v magick >/dev/null 2>&1; }

# ------------------------------------------------------------------ 10 -------
# STAGE 7. What a pool costs over the region the deployment actually has, and
# whether two live mappings of it see each other's writes.
#
# The second half is the measurement §10.3 made about a file and could not make
# about anything else: `probe-filemap` found 65280 of 65536 bytes wrong in both
# directions with both sides mapped. This is the same shape with the same
# discipline -- one side writes while the other holds a live mapping, and the
# other answers -- over guest RAM instead of over the export. It is deliberately
# separate from block 9: if the frames go stale, this says whether the region or
# the pixel path is the reason, and without it the two are indistinguishable.
do_guestarena() {
  echo
  echo "== 10. the arena as a real region: guest RAM through QEMU's flat view =="
  if [ -z "$HOST" ]; then
    nope "WC_HOST unset: the locator has to run on the machine QEMU is on"
    return
  fi

  local size=1048576 n=32 pages=$ARENA_PAGES
  local bytes=$((pages * 4096))
  # Two offsets well past the pools the cost loop will cut, so the probe's bytes
  # and the measurement's bytes cannot be the same bytes.
  local hoff=$((60 * 1024 * 1024)) goff=$((60 * 1024 * 1024 + 4096))
  local gl="$RUNDIR/ga-guest.log" hl="$RUNDIR/ga-host.log"

  # The guest half: provide the arena, measure pools over it, then hold a live
  # mapping and watch. It prints GUESTARENA-READY the moment the stamps are
  # written, which is the earliest the host can find anything.
  gq "export MSYS_NO_PATHCONV=1; cd $GUESTDIR && ./wc-poolcost.exe --iters $n --size $size \
      --arena-guest-ram $bytes --watch $hoff:60 --reply $goff:WCGUESTREPLY" >"$gl" 2>&1 &
  local gpid=$!

  local runid="" found=""
  local i=0
  while [ $i -lt 40 ]; do
    runid=$(grep -o "GUESTARENA-READY: run=0x[0-9a-f]*" "$gl" 2>/dev/null | head -1 | grep -o "0x[0-9a-f]*")
    [ -n "$runid" ] && break
    sleep 1; i=$((i + 1))
  done
  if [ -z "$runid" ]; then
    bad "the guest never provided a guest-RAM arena"
    sed -n '1,15p' "$gl" | sed 's/^/        /'
    kill $gpid 2>/dev/null; wait 2>/dev/null
    return
  fi
  found=$(grep -o "pages=[0-9]*" "$gl" | head -1 | cut -d= -f2)
  ok "the guest committed and pinned $found pages of its own RAM as arena 0 (run $runid)"
  grep -o "locked=[a-z]*" "$gl" | head -1 | grep -q "locked=true" \
    && ok "the region is pinned: VirtualLock succeeded, so the pages cannot move under the host" \
    || bad "VirtualLock failed -- the pages are findable now and free to move afterwards"

  # The host half, as root: locate, verify, write, and wait for the answer.
  h "echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$HOST_DIR/wc-arena-host --qmp '$QMP' --vm '$VM_HINT' --run $runid --pages $found \
  --plan /tmp/wc-arena-b10.plan --poke $hoff:WCHOSTPOKE --watch $goff:60 2>&1
echo rc=\\\$?\" | $ROOTC" >"$hl" 2>&1

  wait $gpid 2>/dev/null

  local loc
  loc=$(grep -o "located [0-9]*/[0-9]* pages in [0-9.]*[a-z]*" "$hl")
  if [ -n "$loc" ]; then
    note "$loc  (a sweep of all of guest RAM, not of the arena)"
  fi
  local ver
  ver=$(grep -o "verify: [0-9]*/[0-9]* pages of the stitched view carry their own arena index" "$hl")
  case "$ver" in
    "verify: $found/$found"*) ok "every one of $found pages is at its own arena offset in QEMU's backend" ;;
    "") bad "the locator produced no verify line"; sed -n '1,20p' "$hl" | sed 's/^/        /' ;;
    *) bad "$ver" ;;
  esac
  local runs
  runs=$(grep -o "coalesced into [0-9]* run(s)" "$hl" | grep -o "[0-9]*" | head -1)
  [ -n "$runs" ] && note "the guest's $found pages are $runs discontiguous run(s) in guest-physical space"

  # The two directions, each measured rather than inferred.
  local gsaw hsaw
  gsaw=$(grep -o "COH-GUEST-SAW: offset=[0-9]* after_ms=[a-z0-9]*" "$gl")
  hsaw=$(grep -o "COH-HOST-SAW: offset=[0-9]* after_ms=[a-z0-9]*" "$hl")
  case "$gsaw" in
    *after_ms=never|"") bad "host -> guest: the guest never saw the host's write (${gsaw:-no line})" ;;
    *) ok "host -> guest: ${gsaw#COH-GUEST-SAW: } -- a live mapping saw a write from outside the VM" ;;
  esac
  case "$hsaw" in
    *after_ms=never|"") bad "guest -> host: the host never saw the guest's answer (${hsaw:-no line})" ;;
    *) ok "guest -> host: ${hsaw#COH-HOST-SAW: } -- and the same region carries the other direction" ;;
  esac

  # And the number §10.4's table was missing: what a pool costs over the real
  # region, against the same code over a file on the same mount.
  local ga
  ga=$(grep "arena-guest-ram" "$gl" | head -1)
  if [ -n "$ga" ]; then
    note "cost: ${ga#  }"
    local map
    map=$(echo "$ga" | grep -o "map *[0-9.]* us" | grep -o "[0-9.]*")
    ok "a pool over guest RAM resolves in ${map} us, opening nothing (the file arena's own number is block 8's)"
  else
    bad "no per-pool measurement over the guest-RAM arena"
    sed -n '1,20p' "$gl" | sed 's/^/        /'
  fi
}

# ------------------------------------------------------------------ clean ----
do_clean() {
  echo "== clean =="
  # STAGE 12: the socket nodes are only swept when this experiment owns the
  # whole guest, and that is §12.8 again in the one place stage 7 did not look.
  # `rm -f .../wg-*` is not scoped to anything: every experiment derived from
  # this script names its sockets `wg-…`, so a clean here deleted another run's
  # live socket node — which presents in *their* log as a compositor that
  # cannot bind, exactly as the image-name `taskkill` did. The directory this
  # deploy owns is scoped by `WC_GUESTNAME`; the AppData directory is not
  # scoped by anything, so it is left alone unless nobody else can be using it.
  local sockets=""
  [ "$GUESTNAME" = wctest ] && sockets="rm -f /c/Users/<guest-user>/AppData/Local/waycompose/*.lock /c/Users/<guest-user>/AppData/Local/waycompose/wg-* 2>/dev/null; rmdir /c/Users/<guest-user>/AppData/Local/waycompose 2>/dev/null;"
  kill_in_dir "$GUESTDIR_WIN"
  g "rm -rf $GUESTDIR; $sockets echo cleaned" 2>&1 | tail -1
  [ -n "$HOST" ] || return 0
  # The export is somebody's real folder, so only what this script made goes.
  # `keymaps` is stage 6's and was not added here when it landed, so `clean` left
  # the share behind and `rmdir` failed silently on a directory that looked empty.
  h "rm -rf $HOST_DIR $SHARE/pools $SHARE/live $SHARE/frames $SHARE/keymaps; rm -f $SHARE/arena $SHARE/coh.bin $SHARE/flush.bin $SHARE/s4.log; rmdir $SHARE 2>/dev/null; echo cleaned $HOST" | tail -1
}

pin_key
case "$MODE" in
  deploy) do_deploy ;;
  run)    do_run ;;
  stage4) do_coherency; do_poolcost; do_stage4; do_guestarena ;;
  arena)  do_guestarena ;;
  firefox) do_firefox ;;
  damage)  do_damage ;;
  clean)  do_clean ;;
  all)    do_deploy; echo; do_run; echo; do_clean ;;
  *) echo "usage: $0 [deploy|run|stage4|arena|firefox|damage|clean|all]"; exit 2 ;;
esac

if [ "$MODE" = run ] || [ "$MODE" = all ] || [ "$MODE" = stage4 ] || [ "$MODE" = firefox ]; then
  echo
  echo "== $pass passed, $fail failed, $skip skipped -- on real Windows =="
fi
[ "$fail" -eq 0 ]
