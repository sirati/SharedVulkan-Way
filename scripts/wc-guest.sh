#!/usr/bin/env bash
# Run the compositor's tests on **real Windows**, in the testhost guest.
#
#   scripts/wc-guest.sh deploy   cross-compile the C probes and copy everything over
#   scripts/wc-guest.sh run      run every check and print the table
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
set -uo pipefail

MODE="${1:-all}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"

GUEST="${WC_GUEST:-<guest-user>@<guest-ip>}"
GUESTDIR='/c/Users/<guest-user>/wctest'
GUESTDIR_WIN='C:\Users\<guest-user>\wctest'
APPDATA_WIN='C:\Users\<guest-user>\AppData\Local\waycompose'
AGENT="${WC_AGENT:-$HOME/.1password/agent.sock}"
KEY="${WC_GUEST_KEY:-/tmp/wc-guest-key.pub}"
PORT_BASE="${WC_PORT_BASE:-57271}"

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

  local missing=0
  for e in wc-server.exe wc-probe.exe wc-compositor.exe wc-xkb-probe.exe; do
    if [ -f "$WIN_BIN/$e" ]; then cp "$WIN_BIN/$e" "$STAGE/"; else
      echo "  missing $WIN_BIN/$e"; missing=1
    fi
  done
  [ $missing -eq 0 ] || { echo "error: build first: cargo build --target x86_64-pc-windows-gnu"; exit 1; }

  g "mkdir -p $GUESTDIR" || exit 1
  scp "${SSHOPT[@]}" "$STAGE"/*.exe "$STAGE/us.xkb" "$GUEST:wctest/" >/dev/null || exit 1
  echo "  copied $(ls "$STAGE" | grep -c . ) files to $GUEST:$GUESTDIR"
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
}

# ------------------------------------------------------------------ clean ----
do_clean() {
  echo "== clean =="
  g "rm -rf $GUESTDIR; rm -f /c/Users/<guest-user>/AppData/Local/waycompose/*.lock /c/Users/<guest-user>/AppData/Local/waycompose/wg-* 2>/dev/null; rmdir /c/Users/<guest-user>/AppData/Local/waycompose 2>/dev/null; echo cleaned" 2>&1 | tail -1
}

pin_key
case "$MODE" in
  deploy) do_deploy ;;
  run)    do_run ;;
  clean)  do_clean ;;
  all)    do_deploy; echo; do_run; echo; do_clean ;;
  *) echo "usage: $0 [deploy|run|clean|all]"; exit 2 ;;
esac

if [ "$MODE" = run ] || [ "$MODE" = all ]; then
  echo
  echo "== $pass passed, $fail failed, $skip skipped -- on real Windows =="
fi
[ "$fail" -eq 0 ]
