#!/usr/bin/env bash
# Run the compositor's tests on **real Windows**, in the testhost guest.
#
#   scripts/wc-guest.sh deploy   cross-compile the C probes and copy everything over
#   scripts/wc-guest.sh run      run every check and print the table
#   scripts/wc-guest.sh stage4   only blocks 7-10, the host half (needs WC_HOST)
#   scripts/wc-guest.sh arena    only block 10, the guest-RAM arena (needs WC_HOST)
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
# docs/design.md §12.9 rather than worked around, because the workaround would be
# passing a descriptor between the two processes and the rule says no.
QMP="${WC_QMP:-/run/windows-vm/qmp.sock}"            # QEMU's monitor, on WC_HOST
VM_HINT="${WC_VM_HINT:-windows-vm}"                  # how to recognise its process
ROOTC="${WC_ROOTC:-\$HOME/rootc}"                    # how WC_HOST becomes root
ARENA_PAGES="${WC_ARENA_PAGES:-16384}"               # 64 MiB, in 4096-byte pages
# STAGE 10: how much of that the compositor keeps for the objects it creates for
# clients. See the driver, and docs/design.md §13.
ARENA_OUTBOUND="${WC_ARENA_OUTBOUND:-1048576}"       # 1 MiB, the outbound window

STAGE=$(mktemp -d /tmp/wc-guest-XXXXXX)
RUNDIR=$(mktemp -d /tmp/wc-guest-run-XXXXXX)
chmod 700 "$RUNDIR"
trap 'rm -rf "$STAGE" "$RUNDIR"' EXIT

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
#             [<qmp> <vm-hint> <rootc> <arena-pages> <guest-dir> <arena-outbound>]
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
    # Provided out of band, before either side starts, which is what an arena
    # is. Truncated to zero first: a region left full of the previous run's
    # pixels is a way for a frozen frame to look like a live one.
    : >"$SHARE/arena"; truncate -s $ARENA_SIZE "$SHARE/arena"; chmod 666 "$SHARE/arena"
    COMP_EXTRA=(--arena "0:$ARENA_SIZE:$SHARE_WIN\\arena")
    PROXY_EXTRA=(--transport arena --arena-path "$SHARE/arena" --arena-id 0 --arena-size $ARENA_SIZE) ;;
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
    COMP_EXTRA=(--arena-guest-ram "0:$ARENA_SIZE" --arena-outbound "0:$ARENA_OUTBOUND")
    # STAGE 10: no shared directory. The keymap comes out of the arena, so
    # there is nothing for the compositor to publish and nothing for this side
    # to open. If the run still passes with this empty, the last reason the
    # guest needed a shared filesystem is gone.
    KEYMAP_EXTRA=()
    PROXY_EXTRA=() ;;
  *) echo "S4-FAIL: unknown transport $TRANSPORT"; exit 2 ;;
esac

# Leftover socket nodes only. This used to `taskkill //F //IM wc-compositor.exe`,
# which is correct for one experiment on one guest and wrong the moment two are
# running: an image-name kill takes out somebody else's compositor as well as
# any of ours, and the symptom lands in *their* run rather than in this one.
# What made the kill necessary was socket reuse, and $$-unique names plus
# --timeout already deal with that.
ssh "${GK[@]}" "$G" "rm -f /c/Users/<guest-user>/AppData/Local/waycompose/wg-s4-* 2>/dev/null; true" >/dev/null 2>&1

ssh "${GK[@]}" "$G" "export MSYS_NO_PATHCONV=1; cd $GDIR && ./wc-compositor.exe \
  --socket $SOCK --timeout $((SECS + 45)) --dump-frames '$SHARE_WIN\\frames' \
  --log '$SHARE_WIN\\s4.log' ${COMP_EXTRA[*]@Q} ${KEYMAP_EXTRA[*]@Q}" >"$RUN/comp.out" 2>&1 &
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
  # Root, and only for this: QEMU is a system service here, so its QMP socket
  # and its /proc/<pid>/fd are root-only. See the header.
  echo "export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$HERE/wc-arena-host --qmp '$QMP' --vm '$VM_HINT' --run $RUNID --pages $FOUNDPAGES \
  --outbound $ARENA_OUTBOUND --plan '$RUN/arena.plan' >'$RUN/locate.log' 2>&1
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

PROXY=0
if [ ! -S "$RUN/up.sock" ]; then
  echo "S4-FAIL: the ssh -L forward produced no socket"
elif [ "$TRANSPORT" != guest-ram ]; then
  XDG_RUNTIME_DIR="$RUN" "$HERE/wc-proxy" --upstream "$RUN/up.sock" --socket wl-proxy \
    --runtime-dir "$RUN" "${PROXY_EXTRA[@]}" --copy damage \
    --accept-timeout 20 --timeout $((SECS + 5)) >"$RUN/proxy.log" 2>&1 &
  PROXY=$!
  sleep 2
  XDG_RUNTIME_DIR="$RUN" WAYLAND_DISPLAY=wl-proxy timeout "$SECS" "$WESTON/$CLIENT" \
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
"$HERE/wc-proxy" --upstream "$RUN/up.sock" --socket wl-proxy \\
  --runtime-dir "$RUN" ${PROXY_EXTRA[*]@Q} --copy damage \\
  --accept-timeout 20 --timeout $((SECS + 5)) >"$RUN/proxy.log" 2>&1 &
P=\$!
sleep 2
WAYLAND_DISPLAY=wl-proxy timeout $SECS "$WESTON/$CLIENT" >"$RUN/cli.log" 2>&1
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
for p in $PROXY $FWD $RELAY $COMP; do [ "$p" -gt 0 ] 2>/dev/null && kill "$p" 2>/dev/null; done
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

  local missing=0
  for e in wc-server.exe wc-probe.exe wc-compositor.exe wc-xkb-probe.exe wc-poolcost.exe; do
    if [ -f "$WIN_BIN/$e" ]; then cp "$WIN_BIN/$e" "$STAGE/"; else
      echo "  missing $WIN_BIN/$e"; missing=1
    fi
  done
  [ $missing -eq 0 ] || { echo "error: build first: cargo build --target x86_64-pc-windows-gnu"; exit 1; }

  # A `.exe` still running holds its own file open, and `scp` over it fails as
  # `dest open ...: Failure` -- which reads as a permissions or disk problem and
  # is neither. A compositor left behind by a killed ssh session is the usual
  # cause; so is one that survived a guest reboot, which has been seen.
  # Only when this deploy owns the whole guest. With WC_GUESTNAME set there is a
  # second experiment on this machine and an image-name kill would take its
  # compositor down too; the cost is that a `.exe` of ours still running makes
  # its own scp fail, which is a message rather than a mystery.
  [ "$GUESTNAME" = wctest ] &&
    g "taskkill //F //IM wc-compositor.exe //IM wc-server.exe //IM probe-relay.exe >/dev/null 2>&1; true" >/dev/null 2>&1

  g "mkdir -p $GUESTDIR" || exit 1
  scp "${SSHOPT[@]}" "$STAGE"/*.exe "$STAGE/us.xkb" "$GUEST:$GUESTNAME/" >/dev/null || exit 1
  echo "  copied $(ls "$STAGE" | grep -c . ) files to $GUEST:$GUESTDIR"

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
  [ -x "$LINUX_BIN/wc-proxy" ] || { echo "error: no $LINUX_BIN/wc-proxy -- cargo build first"; exit 1; }
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
  sleep 2
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
  # docs/design.md §7.8 items 1 and 2. Both roles are separate processes: a pid
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
  if grep -q "5 globals advertised" "$RUNDIR/cp.log"; then
    ok "wc-compositor.exe advertises smithay's five globals"
    grep -E "  global " "$RUNDIR/cp.log" | sort -u | sed 's/^/        /'
  else
    bad "smithay globals"; sed 's/^/        /' "$RUNDIR/cp.log"
  fi
  grep -q "the server processed create_pool without error" "$RUNDIR/cp.log" \
    && ok "smithay's ShmState accepted a create_pool carrying a real named section" \
    || bad "smithay create_pool"

  if command -v weston-multi-resource >/dev/null; then
    relayed wc-compositor.exe wg-xdg $((PORT_BASE + 1)) weston-multi-resource
    check_milestones "$RUNDIR/wg-xdg.server.log" "weston-multi-resource"

    # docs/design.md §7.8 item 3: three bytes at a time guarantees the server
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
  note "docs/design.md §7.8 item 6: the 56 MB debug binary costs nothing measurable to load."

  do_coherency
  do_poolcost
  do_stage4
  do_guestarena
}

# =============================================================================
# Stage 4 on the real boundary. docs/design.md §9.11, all five items.
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
  if [ "$w2l" = yes ] && [ "$l2w" = yes ]; then
    ok "the export is coherent in both directions: \`SharedFile\` is a live transport here"
  else
    ok "measured: the export is NOT coherent while mapped (guest->host $w2l, host->guest $l2w)"
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
  if [ -n "$sf" ] && [ -n "$ar" ]; then
    if [ "$sf" -gt $((ar * 20 + 20)) ]; then
      ok "the decision: a pool costs ${sf} us as a shared file and ${ar} us as an arena slice (~$((sf / (ar + 1)))x)"
    else
      ok "measured: the two transports cost the same here (${sf} us vs ${ar} us) -- no cost argument for either"
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

  s4_run term  arena       weston-terminal         $((PORT_BASE + 10)) 12
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
  echo "  -- $client / $transport --"
  h "$HOST_DIR/wc-stage4 $transport $client $port $SHARE '$SHARE_WIN' $HOST_GUEST \"$HOST_KEY\" $HOST_WESTON $secs '$QMP' '$VM_HINT' \"$ROOTC\" $ARENA_PAGES $GUESTNAME $ARENA_OUTBOUND" >"$log"

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
  local fds
  fds=$(grep -o "[0-9]* fds in, [0-9]* out[^;]*" "$log" | tail -1)
  case "$fds" in
    "") bad "$tag: the proxy reported no descriptor counts" ;;
    *) ok "$tag: descriptors in from the client, out only ones made here from a name ($fds)" ;;
  esac

  # STAGE 10, and it is the claim of the stage on the deployment that matters.
  # Over guest RAM the compositor was started with **no `--shared-dir`**, so a
  # keymap can only have come out of its own window of the arena — three
  # integers, no name, no filesystem. A client that binds a seat is what makes
  # this assertable; `weston-presentation-shm` does not, so the check is on the
  # count being consistent with the client rather than on it being non-zero.
  if [ "$transport" = guest-ram ]; then
    case "$fds" in
      *"from the arena"*)
        ok "$tag: the keymap crossed as a slice of the guest's own RAM ($fds)  <-- §13" ;;
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
  local pools
  pools=$(grep -o "[0-9]* pools ([0-9]* released, [0-9]* resizes, [0-9]* overflows)" "$log" | tail -1)
  case "$pools" in
    *"0 overflows)") ok "$tag: $pools -- every resize arrived before the pool was used" ;;
    *) bad "$tag: $pools" ;;
  esac

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
      note "the compositor reads its own cached copy (docs/design.md §10.3, §10.7). Over"
      note "guest-ram it is not expected and means one of two things: the host is writing"
      note "into memory that is not the compositor's (check block 10's COH- lines, and the"
      note "verify line above), or the guest's pages moved after they were located, which"
      note "is what locked=false in the S4-GUESTARENA line would predict."
    fi
  fi
}

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
  g "rm -rf $GUESTDIR; rm -f /c/Users/<guest-user>/AppData/Local/waycompose/*.lock /c/Users/<guest-user>/AppData/Local/waycompose/wg-* 2>/dev/null; rmdir /c/Users/<guest-user>/AppData/Local/waycompose 2>/dev/null; echo cleaned" 2>&1 | tail -1
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
  clean)  do_clean ;;
  all)    do_deploy; echo; do_run; echo; do_clean ;;
  *) echo "usage: $0 [deploy|run|stage4|arena|clean|all]"; exit 2 ;;
esac

if [ "$MODE" = run ] || [ "$MODE" = all ] || [ "$MODE" = stage4 ]; then
  echo
  echo "== $pass passed, $fail failed, $skip skipped -- on real Windows =="
fi
[ "$fail" -eq 0 ]
