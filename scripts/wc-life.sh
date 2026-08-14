#!/usr/bin/env bash
# docs/waycompose-design.md §53: does a window stop existing?
#
#   scripts/wc-life.sh wine     the churn, with the external witness
#   scripts/wc-life.sh up       the same, keeping the run directory
#
# Run from inside `nix develop`.
#
# WHAT IS MISSING, AND WHY IT IS A CATEGORY RATHER THAN A DETAIL
#
# Every window gate in this tree asserts that something **appeared**.
# `wc-window.sh` waits for a toplevel, `wc-taskbar.sh` counts buttons,
# `wc-icon.sh` reads an icon back, `wc-titlebar.sh` measures a caption. Three
# harnesses mention destruction and all three do it the same way: they grep the
# compositor's own log for `xdg_toplevel destroyed -> window N`.
#
# **That is the code under test reporting its own success.** A compositor that
# dropped its map entry, printed that line and never called `DestroyWindow`
# would keep every gate in this tree green while leaking an `HWND`, its icons
# and its `IDropTarget` registration once per client — for a whole session, on a
# compositor whose windows come and go with a client's menus. The first symptom
# would be a user's desktop running out of handles.
#
# So the missing thing is not an assertion, it is a *kind* of assertion:
# nothing here has ever asked an outside party whether something **stopped**.
# `scripts/probe-life.c` is that outside party — a separate process that never
# speaks to the compositor and asks the window manager instead.
#
# THE GATE HAS TO BE ABLE TO FAIL, AND IS MADE TO PROVE IT
#
# "The window is gone" is the easiest assertion in the world to write wrongly:
# `IsWindow` on a handle that was never valid also says gone, and a count of
# windows belonging to a process that has exited is also zero. Either would
# pass forever. So every cycle below asks the **same instrument about the same
# handle twice** — once while the client is running, where it must answer
# `alive`, and once after, where it must answer `gone`. A run in which the
# positive half ever fails is a run whose negative half proved nothing, and it
# is reported as a failure rather than skipped.
#
# WHAT THIS CANNOT MEASURE HERE, MEASURED RATHER THAN ASSUMED
#
# The obvious instrument is `GetGuiResources` — the kernel's own per-process
# USER and GDI handle counts. **Wine returns 0 for it.** That was measured
# against a `notepad` owning three windows, not inferred from the platform, and
# it matters because a harness that asserted `user == baseline` on this platform
# would be comparing zero to zero and passing forever. `probe-life.exe` reports
# `counter=stub` when it sees that contradiction and this harness *skips* the
# handle assertion by name rather than making it and calling it a pass.
#
# What is left is `EnumWindows` over the compositor's PID, which works and is
# the assertion that matters anyway: a leaked `HWND` is still enumerable.
set -uo pipefail

MODE="${1:-wine}"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/x86_64-pc-windows-gnu/debug"
LINUX_BIN="${CARGO_TARGET_DIR:-$ROOT/target}/debug"
PROBE="${CARGO_TARGET_DIR:-$ROOT/target}/probe-life.exe"

pass=0; fail=0; skip=0
ok()   { echo "  PASS  $*"; pass=$((pass + 1)); }
bad()  { echo "  FAIL  $*"; fail=$((fail + 1)); }
note() { echo "  ----  $*"; }
nope() { echo "  SKIP  $*"; skip=$((skip + 1)); }

. "$(dirname "$0")/wc-binaries.sh"
. "$(dirname "$0")/wc-substrate.sh"
. "$(dirname "$0")/wc-topology.sh"

RUNDIR=$(mktemp -d /tmp/wc-life-XXXXXX)
XPID=""; SRV=""; PROXY=""; CLIENT=""
cleanup() {
  for p in $CLIENT $PROXY $SRV; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  sleep 1
  wineserver -k 2>/dev/null
  [ -n "$XPID" ] && kill "$XPID" 2>/dev/null
  if [ -n "${KEEP:-}" ]; then echo "kept: $RUNDIR"; else rm -rf "$RUNDIR"; fi
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"
TAG="${WC_LIFE_TAG:-life$$}"
DESK=1280x1024
CYCLES="${WC_LIFE_CYCLES:-8}"

wined() { wine explorer "/desktop=wc-$TAG,$DESK" "$@"; }
wl()    { XDG_RUNTIME_DIR="$DIR/rt" WAYLAND_DISPLAY=wl-proxy "$@"; }

# The probe, run inside the compositor's virtual desktop, answering into a file.
#
# **Both halves of that are forced.** `EnumWindows` enumerates the *calling
# thread's* desktop, so the probe has to be launched with
# `wine explorer /desktop=<the compositor's>` or it sees none of the windows in
# question — and `wine explorer` launches the program without connecting its
# stdout to the caller, so a probe that printed its answer produced nothing at
# all here. The first version of this harness did exactly that and reported
# "no window titled 'ClickDot'" for a client that was running happily with its
# window mapped, which is a false negative that reads like a compositor bug.
#
# `tr -d '\r'` because it is a Windows program writing a file a shell reads:
# every line ends in a carriage return and `[ "$x" = gone ]` silently never
# matches. That one cost a run in §50 as well.
# **An empty answer is a failed probe, not a "no".** `wine explorer` does not
# always get the program started -- the watch loop saw one invocation in twelve
# come back with nothing while the window it was asked about was plainly there.
# If that empty string reached the churn loop it would be compared against
# `gone`, fail to match, and the harness would report the *right* answer for the
# wrong reason today and the wrong answer tomorrow. So the retry is here, and a
# probe that never answers returns non-zero rather than an empty string that
# reads like a verdict.
life() {
  local out="$RUNDIR/probe.out" try _
  for try in 1 2 3; do
    rm -f "$out"
    wined "$PROBE" "$@" --out "$(winepath -w "$out")" >/dev/null 2>&1
    for _ in $(seq 1 60); do [ -s "$out" ] && break; sleep 0.1; done
    if [ -s "$out" ]; then tr -d '\r' < "$out"; return 0; fi
  done
  return 1
}

start_x() {
  Xvfb ":$1" -screen 0 "${DESK}x24" -nolisten tcp >"$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$1"
  for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && return 0; sleep 0.1; done
  return 1
}

find_socket() {
  local name=$1 node=""
  for _ in $(seq 1 250); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$name" 2>/dev/null | head -1)
    [ -n "$node" ] && { echo "$node"; return 0; }
    sleep 0.2
  done
  return 1
}

build_probe() {
  command -v x86_64-w64-mingw32-gcc >/dev/null \
    || { bad "no x86_64-w64-mingw32-gcc on PATH (run inside nix develop)"; return 1; }
  x86_64-w64-mingw32-gcc -O1 -g -o "$PROBE" "$ROOT/scripts/probe-life.c" -luser32 -lgdi32 \
    || { bad "probe-life.c did not build"; return 1; }
}

start_chain() {
  DIR="$RUNDIR/run"
  mkdir -p "$DIR/rt" "$DIR/pools" "$DIR/keymaps"
  chmod 700 "$DIR/rt"
  SOCK="wl-$TAG"
  # The idle timeout has to outlast the longest stretch of this run with **no
  # client connected**, and that is not the gap between cycles: the "is it gone
  # yet" poll spends up to forty seconds asking, each `life` call being a fresh
  # `wine explorer`. Set to 30 s it fired mid-churn, the compositor exited, and
  # the next three cycles reported windows that never appeared and a
  # registration "stranded in OLE's table" -- a clean bill of blame for the
  # compositor, produced by a harness that had shut it down.
  wined "$WIN_BIN/wc-compositor.exe" --socket "$SOCK" --timeout "${WC_LIFE_IDLE:-120}" \
    --shared-dir "$(winepath -w "$DIR/keymaps")" --shared-peer-prefix "$DIR/keymaps/" \
    --log "$(winepath -w "$DIR/srv.log")" >"$DIR/loader.log" 2>&1 &
  SRV=$!
  local node
  node=$(find_socket "$SOCK") || return 1
  "$LINUX_BIN/wc-proxy" --upstream "$node" --socket wl-proxy --runtime-dir "$DIR/rt" \
    --transport shared-file --pool-dir "$DIR/pools" \
    --peer-prefix "$(winepath -w "$DIR/pools")\\" \
    --no-icons --accept-timeout 540 --timeout 540 >"$DIR/proxy.log" 2>&1 &
  PROXY=$!
  sleep 1
}

# A client whose window stays up until this harness takes it down.
#
# **The choice of client is the first thing this harness got wrong**, and it is
# worth recording because the symptom looked like the compositor's fault.
# `weston-terminal` was the obvious pick and it is the wrong one: it runs a
# shell in a pty, and on a non-interactive launch that shell exits at once and
# takes the terminal with it. The compositor's log showed the whole life of
# window 1 — created, mapped, titled, focused, destroyed — inside five seconds,
# and the harness then said no window existed to measure. Giving it a `--shell`
# script that blocks did not help; the terminal still went, leaving the blocked
# script behind as an orphan.
#
# `weston-clickdot` has no pty and no child: it maps a toplevel, draws, and runs
# until something kills it. **A window whose lifetime the harness does not
# control is not a window the harness can ask lifetime questions about** — which
# is the whole subject here, so getting it wrong was getting the experiment
# backwards rather than getting a detail wrong.
CLIENT_BIN="${WC_LIFE_CLIENT:-weston-clickdot}"
CLIENT_TITLE="${WC_LIFE_TITLE:-ClickDot}"

# **Not `wl "$WESTON/$CLIENT_BIN" &`**, and the difference is the whole
# experiment. `wl` is a shell function, so backgrounding a call to it puts a
# *subshell* in the background and `$!` is the subshell's pid; the client is its
# child. `kill "$CLIENT"` then kills the subshell and leaves the client running,
# with its window mapped, for ever.
#
# The first churn run did exactly that, and what makes it worth a comment rather
# than a fix is what the harness said about it: five cycles, five windows still
# alive afterwards, the count climbing 3 → 8, and **the compositor claiming zero
# destructions** — a perfect description of a compositor that never destroys
# anything, produced by a harness that never closed a client. The assertion was
# right and its subject was wrong. An environment prefix on a simple command
# backgrounds the command itself, so `$!` is the client.
start_client() {
  XDG_RUNTIME_DIR="$DIR/rt" WAYLAND_DISPLAY=wl-proxy "$WESTON/$CLIENT_BIN" \
    >"$DIR/c$1.log" 2>&1 &
  CLIENT=$!
}

# And the kill is checked, for the same reason: "the window is still there"
# after a kill that did nothing is a true statement about the wrong world.
stop_client() {
  [ -n "$CLIENT" ] || return 0
  kill "$CLIENT" 2>/dev/null
  local _
  for _ in $(seq 1 40); do kill -0 "$CLIENT" 2>/dev/null || break; sleep 0.25; done
  if kill -0 "$CLIENT" 2>/dev/null; then
    bad "the client (pid $CLIENT) would not die, so nothing below is about a closed client"
    kill -9 "$CLIENT" 2>/dev/null
    CLIENT=""
    return 1
  fi
  CLIENT=""
  return 0
}

# The compositor's window for the client, by title, as an HWND. Empty if none.
client_hwnd() {
  life windows --pid "$COMP_PID" | awk -F"\t" -v t="$CLIENT_TITLE" 'index($3, t) { print $1; exit }'
}

all_windows() { life count --pid "$COMP_PID" | grep -oE 'all=[0-9]+' | cut -d= -f2; }

run_wine() {
  echo "== docs/waycompose-design.md §53: does a window stop existing? =="

  command -v Xvfb >/dev/null || { bad "Xvfb not on PATH"; return; }
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }

  fresh "$WIN_BIN/wc-compositor.exe" "cargo build --workspace --target x86_64-pc-windows-gnu" || return
  fresh "$LINUX_BIN/wc-proxy" "cargo build --workspace" || return
  build_probe || return

  wc_substrate_declare wine-fixture \
    "the Wine leg has no QEMU and no guest; this run is about window handles, not pixels" || return
  # §51. Everything is one kernel, and nothing here is a time.
  wc_topology_declare same-box \
    "wc-compositor.exe under Wine, wc-proxy and the clients are processes on this workstation" || return

  WESTON=$(ls -d /nix/store/*weston-15*/bin 2>/dev/null | head -1)
  [ -x "$WESTON/$CLIENT_BIN" ] || { bad "$CLIENT_BIN not found"; return; }

  start_x 98 || { bad "could not start Xvfb"; return; }
  ok "Xvfb up on $DISPLAY"
  start_chain || { bad "the chain did not come up"; sed -n '1,20p' "$RUNDIR/run/loader.log"; return; }
  ok "wc-compositor.exe is listening, wc-proxy is in front of it"

  # One client first, to learn the compositor's PID from a window it owns. The
  # PID cannot be asked for later: when the assertion matters there are no
  # windows left to ask about.
  start_client 0
  sleep 5
  COMP_PID=$(life pidof --title "$CLIENT_TITLE")
  if [ -z "$COMP_PID" ] || [ "$COMP_PID" = 0 ]; then
    bad "no window titled '$CLIENT_TITLE' on the Wine desktop; nothing to measure"
    life windows --pid 0
    return
  fi
  ok "the compositor owns the client's window, and its pid is $COMP_PID"

  local counter
  counter=$(life count --pid "$COMP_PID" | grep -oE 'counter=[a-z]+' | cut -d= -f2)
  note "handle counters: $(life count --pid "$COMP_PID")"
  if [ "$counter" = live ]; then
    ok "GetGuiResources answers on this platform, so USER/GDI handles are measurable"
  else
    nope "GetGuiResources is a $counter here (Wine returns 0), so no USER/GDI assertion is made -- see the header"
  fi

  stop_client
  sleep 3
  BASE=$(all_windows)
  note "baseline with no client: $BASE windows belong to the compositor"

  # ---------------------------------------------------------------- churn ---
  local i h alive gone now up
  local positives=0 negatives=0 grew=0 rose=0
  for i in $(seq 1 "$CYCLES"); do
    start_client "$i"
    h=""
    for _ in $(seq 1 40); do h=$(client_hwnd); [ -n "$h" ] && break; sleep 0.5; done
    if [ -z "$h" ]; then bad "cycle $i: the client's window never appeared"; break; fi

    # THE POSITIVE HALF. Same question, same instrument, opposite answer — and
    # if this ever fails then the negative half below proves nothing, because
    # an instrument that always says `gone` would pass it.
    alive=$(life alive --hwnd "$h")
    [ "$alive" = alive ] && positives=$((positives + 1)) \
      || bad "cycle $i: window $h was not reported alive while its client was running (got '$alive')"

    # THE COUNTER, PROVED TO MOVE. The baseline here is **0** — the compositor's
    # own controller window is message-only and `EnumWindows` does not enumerate
    # those — so "0 windows after N cycles, the same as before" would be a
    # comparison of zero to zero, which is the shape of gate this whole harness
    # exists to avoid. Recording the count *while the client is up* is what
    # makes the final equality mean something: the number is shown to rise and
    # then shown to come back.
    up=$(all_windows)
    [ "${up:-0}" -gt "$BASE" ] && rose=$((rose + 1)) \
      || bad "cycle $i: the compositor owned $up windows with a client mapped, not more than the baseline $BASE"

    stop_client
    for _ in $(seq 1 40); do [ "$(life alive --hwnd "$h")" = gone ] && break; sleep 0.5; done

    gone=$(life alive --hwnd "$h")
    [ "$gone" = gone ] && negatives=$((negatives + 1)) \
      || bad "cycle $i: window $h still exists after its client went away"

    now=$(all_windows)
    [ "$now" -gt "$BASE" ] && { grew=$((grew + 1)); note "cycle $i: $now windows, baseline $BASE"; }
  done

  [ "$rose" = "$CYCLES" ] \
    && ok "the window count rose above the baseline of $BASE on every one of $CYCLES cycles, so it is a counter that moves" \
    || bad "the count only rose on $rose of $CYCLES cycles -- a count that never moves cannot show a leak"

  [ "$positives" = "$CYCLES" ] \
    && ok "every one of $CYCLES windows was observed to exist while its client ran (the gate can fail)" \
    || bad "only $positives of $CYCLES windows were ever seen alive -- the negative results below are worthless"

  [ "$negatives" = "$CYCLES" ] \
    && ok "and every one of them stopped existing, checked by a process that is not the compositor" \
    || bad "only $negatives of $CYCLES windows stopped existing"

  now=$(all_windows)
  [ "$now" = "$BASE" ] \
    && ok "after $CYCLES create/destroy cycles the compositor owns $now windows, the same as before ($BASE)" \
    || bad "the compositor owns $now windows after $CYCLES cycles, up from $BASE -- an HWND per client is leaking"

  [ "$grew" = 0 ] || note "the count was above baseline at the end of $grew cycle(s)"

  # ------------------------------------------------- the claim, cross-checked ---
  # This is the point of the whole harness. The compositor says it destroyed
  # them; the external count says whether that word meant anything.
  local claimed
  # The specific line, not any line containing the word: `xdg_toplevel destroyed`
  # and `window N destroyed` are both printed for one window, so a loose grep
  # doubles the count and an assertion of `>=` never notices.
  claimed=$(grep -c "^wc-compositor: window [0-9]* destroyed$" "$DIR/srv.log")
  note "the compositor's own log claims $claimed destruction(s)"
  if [ "$claimed" = "$((CYCLES + 1))" ] && [ "$now" = "$BASE" ]; then
    ok "the compositor claimed $claimed destructions for $((CYCLES + 1)) windows and the window manager agrees none are left"
  else
    bad "the compositor claimed $claimed destruction(s) and $now windows remain against a baseline of $BASE"
  fi

  # ------------------------------------------- the other per-window resource ---
  #
  # docs/waycompose-design.md §62. A window is not the only thing `ToUi::Destroy` gives back: it also
  # revokes the window's OLE drop-target registration, and **that one has no
  # external witness at all**. OLE's registration table is not enumerable from
  # another process, and a stranded entry has no symptom until an `HWND` value
  # is reused — which docs/waycompose-design.md §62.3 measures as not happening within a run, so the
  # trick that would have caught it is unavailable here.
  #
  # What is available is the compositor's own count, printed at exit, made
  # falsifiable by `revoke` now reading the `HRESULT` it used to discard. This
  # asserts the pair balances *and* that it is not a pair of zeros.
  echo "--- waiting for the compositor to reach its idle timeout and report ---"
  local _ exitline dtline
  for _ in $(seq 1 $(( ${WC_LIFE_IDLE:-120} + 40 ))); do
    grep -q "drop targets at exit" "$DIR/srv.log" 2>/dev/null && break
    sleep 1
  done
  exitline=$(grep -o 'windows at exit:.*' "$DIR/srv.log" | tail -1)
  dtline=$(grep -o 'drop targets at exit:.*' "$DIR/srv.log" | tail -1)
  if [ -z "$dtline" ]; then
    bad "the compositor never printed its drop-target accounting"
  else
    note "$exitline"
    note "$dtline"
    local reg rev bad_rev
    reg=$(echo "$dtline" | grep -oE 'claims [0-9]+' | grep -oE '[0-9]+')
    rev=$(echo "$dtline" | grep -oE '[0-9]+ revoked' | grep -oE '[0-9]+')
    bad_rev=$(echo "$dtline" | grep -oE '[0-9]+ revoke\(s\) failed' | grep -oE '[0-9]+')
    # Non-vacuity first, for the reason the whole harness exists: 0 == 0 is not
    # a result. The churn above proved windows were made; each of them
    # registered, so this number has to be the same one.
    if [ "${reg:-0}" -eq 0 ]; then
      bad "no window was ever registered as a drop target, so the balance below is 0 == 0"
    else
      ok "$reg window(s) were registered as drop targets, so there was something to give back"
      [ "$reg" = "$rev" ] \
        && ok "and all $rev were revoked before their windows were destroyed (§43.7's ordering, counted)" \
        || bad "$reg registered but only $rev revoked -- a registration is stranded in OLE's table"
      [ "${bad_rev:-0}" = 0 ] \
        && ok "no RevokeDragDrop failed" \
        || bad "$bad_rev RevokeDragDrop call(s) failed"
    fi
  fi

  wc_assert_topology "$DIR/srv.log" "wc-life"
  mkdir -p "$ROOT/docs/life"
  cp "$DIR/srv.log" "$ROOT/docs/life/compositor.log" 2>/dev/null
  note "log in $ROOT/docs/life/compositor.log"
}

# Bring the chain up, start one client, and say what is true every second for
# half a minute. Kept rather than deleted: "the window was not there" and "the
# window was there and went" are different diagnoses with the same symptom, and
# the first version of this harness could not tell them apart.
watch_one() {
  build_probe || return
  WESTON=$(ls -d /nix/store/*weston-15*/bin 2>/dev/null | head -1)
  start_x 98 || { bad "no Xvfb"; return; }
  start_chain || { bad "no chain"; cat "$RUNDIR/run/loader.log"; return; }
  start_client 0
  local i pid state
  for i in $(seq 1 30); do
    sleep 1
    kill -0 "$CLIENT" 2>/dev/null && state=running || state=DEAD
    pid=$(life pidof --title "$CLIENT_TITLE")
    echo "t=${i}s client=$state window_owner_pid=${pid:-none} srv=$(grep -c 'window 1 destroyed' "$DIR/srv.log")"
  done
  echo "--- client log ---"; cat "$DIR/c0.log"
  echo "--- proxy tail ---"; tail -6 "$DIR/proxy.log"
  echo "--- srv tail ---"; tail -6 "$DIR/srv.log"
}

case "$MODE" in
  up)       export KEEP=1; run_wine ;;
  watch)    export KEEP=1; watch_one ;;
  wine|all) run_wine ;;
  *) echo "usage: wc-life.sh [wine|up]"; exit 2 ;;
esac

echo
echo "  $pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
