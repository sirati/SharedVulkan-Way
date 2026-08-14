#!/usr/bin/env bash
# Prove the stage-4 milestone: an **unmodified** third-party Wayland client
# painting into a real Windows window, across a boundary no descriptor crosses.
#
#   scripts/wc-proxy.sh window     the milestone, both clients, both transports
#   scripts/wc-proxy.sh cost       what each copy scope costs, measured
#   scripts/wc-proxy.sh all        both
#
# Run from inside `nix develop`.
#
# WHAT THIS CLOSES, AND WHY IT IS ONE EXPERIMENT RATHER THAN TWO
#
# docs/waycompose-design.md §8.5 had to split its evidence in half and said so:
#
#   * **Linux** ran `weston-terminal` and `weston-presentation-shm` — real
#     libwayland clients that cannot share a bug with this project — through the
#     whole buffer path, and could not produce an `HWND` because there is no
#     Win32 there.
#   * **Wine** ran `wc-shmclient.exe`, which speaks our dialect and is ours, and
#     got pixels into a window with Windows' own caption. "A real weakening",
#     as that section put it.
#
# The reason the halves could not be joined is §2.4: an `h` argument is
# out-of-band on Unix and in-band on Windows, so a stock libwayland client's
# `wl_shm.create_pool` writes no descriptor bytes and the Windows server's
# parser desynchronises. Measured twice — `wc-window.sh`'s third block under
# Wine, and `weston-flower` against the real guest in §7.8.
#
# `wc-proxy` is what joins them. It is a Wayland server to the local client,
# ancillary data and all, and a client of the Windows compositor in our dialect.
# So this harness runs **one** experiment: `weston-terminal` and
# `weston-presentation-shm`, unmodified, against `wc-compositor.exe`, and
# photographs the result from outside every process involved.
#
# THE RULE THIS HARNESS ALSO CHECKS
#
# No descriptor and no handle may cross, ever, even here where both sides are on
# one machine. So the harness does not only check that pixels arrived: it checks
# *what crossed*, by asserting on the compositor's own record of which kind of
# object it opened (`wc_handle::map_counts`, printed per window). A run that got
# the pixels there by passing something would fail these even though the picture
# would look identical.
#
# EVERY NUMBER THIS HARNESS PRINTS IS EITHER ASSERTED OR SAID TO BE INFORMATIONAL
#
# §31. This harness reported **37 passed, 0 failed** through a 51x copy-cost
# regression, because the cost table was printed for a human to read and the
# harness had no opinion about it. A number a harness prints and does not check
# is a number nobody is watching, and the only honest states for one are
# "asserted", "bounded", or "informational, and here is why".
#
# The audit, in full, so that the next number added here has somewhere to go:
#
#   ASSERTED   frames presented (per client floor), map counts by kind,
#              descriptors in and out, dumped-frame geometry and colour count,
#              light rows top against bottom, primaries found, wedge span,
#              screenshot colour count, commits, copies per commit, KiB per
#              copy against the client's own buffer extent, MiB per commit
#              across the scope hierarchy, and the number of rows in the table.
#   BOUNDED    screenshot difference, but only for the client that animates;
#              for a static client there is no live-ness claim to make.
#   INFORMATIONAL
#              microseconds per copy. It is a *rate* on a machine running Xvfb,
#              Wine, a compositor and a client at once; the same tree measured
#              37 us and 103 us for the same 244 KiB copy in one run of this
#              file. Asserting on it would be asserting on the load. What the
#              cost gate gets instead is bytes, which are a property of the code.
#
# TIMING, AND WHY THIS FILE WAITS FOR THINGS RATHER THAN SLEEPING ON THEM
#
# The other half of §31. This harness once failed 6 and then passed 39/0 twice
# on the same tree, and a red that is not reliably a regression is worse than no
# red at all: it teaches everyone to re-run until green. A scoreboard whose
# noise floor is one test cannot report a one-test regression.
#
# Every fixed `sleep` that guarded a *state* is now a bounded wait for that
# state, and every wait prints when it had to wait — a retry that leaves no
# trace turns a slow run into a fast-looking one. `sleep` survives in exactly
# two places, both of which wait for time rather than for a state: the gap
# before `wine explorer` (§8.9's wineserver shutdown, which nothing observable
# reports) and the interval between the two screenshots (which *is* the
# measurement).
#
# The sharpest instance was not in this file at all. `find_socket` waits for the
# compositor's socket *node*, and `bind` creates the node while `listen` is what
# makes it accept; `wc-proxy` connected upstream once and reported "cannot reach
# the compositor" if it landed in between. That is the same defect `wc-wlcs`'s
# `connect_client` was fixed for, and it is now fixed in the proxy itself, with
# the retry counted and printed — see `wc_proxy::proxy::connect_upstream`.
#
# WHAT WINE DOES AND DOES NOT PROVE. Unchanged from stages 1-3 and still the
# most important sentence: Wine is not Windows. A pass says our code paths are
# self-consistent and structurally possible. Wine has been wrong four times on
# this project, so a *negative* here is not a result and `scripts/wc-guest.sh`
# is the arbiter. What a pass buys is that the whole chain — third-party client,
# host proxy, in-band descriptor, named shared memory opened by the peer, GDI,
# an HWND — runs end to end without a descriptor crossing.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }

# STAGE 47. **A third outcome, because this harness had no way to say "the
# machine was too loaded to measure".**
#
# Every gate here could previously end in exactly two places: `bad`, which says
# the *code* is wrong, or `note`, which says nothing and is counted by nobody.
# A gate whose shortfall can be caused by load alone therefore had to lie in one
# direction or the other — blame the tree for the box, or go silent. §35's rule
# is that a number nobody asserts on is unwatched; the corollary this stage adds
# is that **an outcome the vocabulary cannot express gets misfiled**.
#
# `inconclusive` is counted and printed in the summary, so a run that could not
# measure something says so in the same line as its passes and failures, and a
# reader can tell "nothing was wrong" from "we could not tell".
#
# It is deliberately *not* a way to make a failing gate quiet. It is used only
# where the condition is a property of the machine — sample size under load —
# and never where the property being tested is the code's.
inconc=0
skip() { echo "  ????  $*"; inconc=$((inconc + 1)); }

# The load average, for the messages that need to distinguish slow from broken.
# One decimal, and the 1-minute figure, because the legs here are 6 seconds long
# and the 5-minute average would describe a different run.
loadnow() { awk '{printf "%.1f", $1}' /proc/loadavg 2>/dev/null || echo "?"; }

# STAGE 41. No QEMU on this box, so no guest RAM to locate: this harness is the
# Wine fixture and its `arena` transport is a file. `wc_substrate_declare
# wine-fixture` is what hands it the `SV_ALLOW_FILE_ARENA=1` that `wc-proxy` and
# `wc-compositor` now require, and it refuses to hand that out to any harness
# that has a guest.
. "$(dirname "$0")/wc-substrate.sh"

# §51. Whether a hypervisor is in the path, declared rather than left unknown.
# This harness is same-box, so `wc_latency_claim` will refuse any assertion here
# whose truth depends on cross-VM timing -- which is the point: §27.6 drew one
# from a harness exactly like this one.
. "$(dirname "$0")/wc-topology.sh"
wc_topology_declare same-box \
  "weston's clients, the proxy and wc-compositor.exe all run on this box" || exit 1
# STAGE 42. `fresh` / `fresh_or_skip` / `deployed_is`, shared rather than copied.
. "$(dirname "$0")/wc-binaries.sh"
wc_substrate_declare wine-fixture \
  "no QEMU on this box: a Wine run has no guest RAM to locate" || exit 2

RUNDIR=$(mktemp -d /tmp/wc-proxy-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"

# ---------------------------------------------------------------------------
# plumbing
# ---------------------------------------------------------------------------

start_x() {
  Xvfb ":$1" -screen 0 1280x1024x24 -nolisten tcp >"$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$1"
  for _ in $(seq 1 100); do
    xwd -root -silent >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

# fresh <binary> <what-to-run>
#
# **Refuse to measure a binary older than the tree it is supposed to be built
# from.** This check exists because writing this file walked straight into what
# it is guarding: the §27 fix was reverted to reproduce the 51x regression, the
# source was put back, `cargo test --workspace` was run — which builds test
# binaries and does *not* relink `target/debug/wc-proxy` — and the next harness
# run reported the regression against a clean tree. Twelve failures, and every
# one of them a fact about a stale executable.
#
# That is the same defect this whole section is about, one level down: the
# harness had an opinion about the numbers and no opinion about *what produced
# them*. `[ -x "$LINUX_BIN/wc-proxy" ]` is a check that the file exists, which
# is not the same claim at all. §18.8 says the neighbouring thing about clippy
# — it builds metadata, not an executable — and this is its other half.
# STAGE 42. `fresh` now lives in `scripts/wc-binaries.sh`, sourced above, and
# this copy is deleted rather than kept in sync. §35.6 item 6 said it was
# "copied rather than shared because these scripts have no common file"; that
# stopped being true when `wc-substrate.sh` landed, and two copies of a check
# are two checks that can disagree — which is the same argument §41.4 makes for
# not implementing the file-arena refusal twice.
#
# The shared file also carries `fresh_or_skip`, which this harness does not need
# and the seventeen in §35.6 item 6's list mostly did: a block that skips when a
# binary is absent must still fail when one is present and stale.

# wait_for <file> <extended-regex> <seconds> <what>
#
# Wait for a state to be *reported* rather than sleeping long enough that it
# probably happened. Bounded, and **the wait is printed whenever it was not
# instant**, which is the difference between a harness that is load-independent
# and one that merely looks it: a run that needed 4 of its 20 seconds says so,
# so a machine drifting towards the cliff is visible before it goes over.
wait_for() {
  local f=$1 re=$2 secs=$3 what=$4
  local i=0 n=$((secs * 20))
  while [ "$i" -lt "$n" ]; do
    if [ -f "$f" ] && grep -qE "$re" "$f" 2>/dev/null; then
      [ "$i" -gt 10 ] && note "waited $(awk -v i="$i" 'BEGIN{printf "%.1f", i/20}')s for $what"
      return 0
    fi
    sleep 0.05
    i=$((i + 1))
  done
  note "gave up after ${secs}s waiting for $what"
  return 1
}

# Find the compositor's AF_UNIX node inside the prefix. Under Wine that node is
# a *real host socket*, which is the only reason a native Linux proxy can reach
# a mingw `.exe` at all (docs/design.md §4.2). On the real guest `afunix.sys`
# makes an NTFS reparse point instead and `scripts/probe-relay.c` is the bridge.
find_socket() {
  local name=$1 node=""
  for _ in $(seq 1 250); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$name" 2>/dev/null | head -1)
    [ -n "$node" ] && { echo "$node"; return 0; }
    sleep 0.2
  done
  return 1
}

# run_one <tag> <client> <transport> <copy-scope> <seconds> [min-screenshot-diff]
#
# Starts the Windows compositor inside a Wine virtual desktop, the proxy beside
# it, and the client against the proxy; photographs the X root twice while the
# client is still drawing; leaves everything in $RUNDIR/<tag>.
#
# `min-screenshot-diff` is how many pixels the two shots must differ by for the
# pair to carry a live-ness claim. 0 (the default) means the client is static
# and the difference is informational. A non-zero value gets a **bounded,
# printed retry**: a client that animates can still be photographed twice inside
# one frame on a loaded machine, and that is a property of the load rather than
# of the code.
run_one() {
  local tag=$1 client=$2 transport=$3 scope=$4 secs=$5 mindiff=${6:-0} shots=${7:-1}
  local dir="$RUNDIR/$tag"
  mkdir -p "$dir/rt" "$dir/frames"
  chmod 700 "$dir/rt"

  local sock="wl-s4-$tag"
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  # STAGE 6. The compositor now has a keyboard, so any client that binds
  # `wl_seat` receives a `wl_keyboard.keymap` — which is an `h` in the
  # *server-to-client* direction, the one stage 4 implemented and never
  # exercised (§9.10 item 5). Without `--shared-dir` the compositor publishes it
  # as a `Local\wc-…` section and `wc-proxy` ends the connection with a
  # diagnosis, which is correct and which broke this harness the moment stage 6
  # landed: `weston-terminal` binds a seat and `weston-presentation-shm` does
  # not, so exactly one of the two clients here started failing.
  #
  # Recorded rather than quietly fixed, because it is the clearest possible
  # demonstration that the two directions are different problems.
  mkdir -p "$dir/keymaps"
  local shared=(--shared-dir "$(winepath -w "$dir/keymaps" 2>/dev/null || echo "$dir/keymaps")"
                --shared-peer-prefix "$dir/keymaps/")

  local arena_arg=() proxy_extra=()
  if [ "$transport" = arena ]; then
    # The arena is provided **out of band, before either side starts**, which is
    # what it is in the real deployment: shared_vulkan's cold-path handshake has
    # long since run by the time a compositor exists. Here the harness is the
    # third party, and that is the whole of the difference.
    local arena="$dir/arena" size=$((64 * 1024 * 1024))
    truncate -s "$size" "$arena"
    arena_arg=(--arena "0:$size:$(winepath -w "$arena" 2>/dev/null || echo "$arena")")
    proxy_extra=(--transport arena --arena-path "$arena" --arena-id 0 --arena-size "$size")
  else
    local pools="$dir/pools"
    mkdir -p "$pools"
    proxy_extra=(--transport shared-file --pool-dir "$pools"
                 --peer-prefix "$(winepath -w "$pools" 2>/dev/null || echo "$pools")\\")
  fi

  # A **distinct desktop name per run, and a gap before starting**, both of
  # which were measured rather than chosen. docs/waycompose-design.md §8.9 records that the
  # Wine harnesses collide when run back to back because the previous
  # `wineserver` is still shutting down; this harness runs three compositors in
  # one go and hit a sharper version of it. With the same desktop name the
  # second `wine explorer` attaches to the first run's desktop, which is still
  # winding down, and `CreateWindowExW` then fails for every visible top-level
  # window — the compositor logs `wl_surface commit` and simply never makes a
  # window. That is §8.11 item 1's failure mode arriving as a test artefact
  # rather than as a finding, which is exactly the sort of thing that would be
  # written up as "Wine cannot do this".
  sleep 3
  wine explorer "/desktop=wc-$tag,1280x1024" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 60 \
    --dump-frames "$(winepath -w "$dir/frames" 2>/dev/null || echo "$dir/frames")" \
    "${arena_arg[@]}" "${shared[@]}" \
    --log "$(winepath -w "$dir/srv.log" 2>/dev/null || echo "$dir/srv.log")" \
    >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node
  if ! node=$(find_socket "$sock"); then
    echo "NOSOCKET" >"$dir/why"
    kill "$srv" 2>/dev/null
    return 1
  fi

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --copy "$scope" "${proxy_extra[@]}" \
    --accept-timeout 20 --timeout "$secs" >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  # WAS `sleep 0.8`, and the sleep was the bug. `Listener::bind` binds and
  # listens in one call and then prints this line, so the line *is* the state
  # the client needs; 0.8 s was a guess that happened to be long enough on an
  # idle machine.
  wait_for "$dir/proxy.log" '^\[proxy\] listening on' 20 "$tag: the proxy to listen"

  XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout $((secs + 8)) "$client" >"$dir/cli.log" 2>&1 &
  local cli=$!

  # WAS `sleep 5`. The thing worth photographing is a window, and the compositor
  # says when it has made one; on a loaded machine five seconds was sometimes
  # before that and the screenshot assertions then measured the load. Bounded by
  # the client's own run length, so a client that never makes a window costs
  # nothing extra and fails in `check_common` where the diagnosis lives.
  #
  # Only for the legs that photograph. The cost legs never look at a screenshot
  # — `check_common` is not called for them — so waiting for a window there
  # bought nothing and cost 16 s six times over, with a "gave up" line each
  # time that trains a reader to ignore the notes. A wait nobody needs is
  # noise, and noise is how a real one gets missed.
  if [ "$shots" = 1 ]; then
  wait_for "$dir/srv.log" 'window .* created \(HWND' $((secs + 10)) "$tag: the window to be created" || true
  # A beat for the client to paint into the window that now exists. This one
  # stays a `sleep` on purpose and it is a different kind of thing from the five
  # seconds it replaced: startup was the load-dependent part and is now waited
  # for explicitly, so all this covers is one client's paint. Nothing asserted
  # on the shots needs it to be long enough — the colour-count floor clears by
  # 7x either way — it is here so the photograph is of a settled window rather
  # than of one appearing.
  sleep 2

  # Two shots, while the client is still drawing. One shot cannot tell a live
  # window from a window showing one frozen frame, which is the failure stage 3
  # designed this pair to catch. The 1.5 s between them is a real sleep and
  # stays one: it is the measurement's own interval, not a wait for a state.
  local attempt=0 differ=0
  while :; do
    xwd -root -silent -out "$dir/s1.xwd" 2>/dev/null && magick "$dir/s1.xwd" "$dir/s1.png" 2>/dev/null
    sleep 1.5
    xwd -root -silent -out "$dir/s2.xwd" 2>/dev/null && magick "$dir/s2.xwd" "$dir/s2.png" 2>/dev/null
    differ=$(magick compare -metric AE "$dir/s1.png" "$dir/s2.png" null: 2>&1 | grep -o "^[0-9]*")
    differ=${differ:-0}
    [ "$mindiff" -eq 0 ] && break
    [ "$differ" -ge "$mindiff" ] && break
    attempt=$((attempt + 1))
    [ "$attempt" -ge 3 ] && break
    note "$tag: the two shots differ by $differ, under the $mindiff this client owes; retrying the pair ($attempt/3)"
  done
  echo "$differ" >"$dir/differ"
  echo "$mindiff" >"$dir/mindiff"
  fi

  wait "$cli" 2>/dev/null
  echo $? >"$dir/cli.rc"
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  return 0
}

# The bounding box of every pixel within `fuzz` of `colour`, as WxH+X+Y, or
# empty. `-trim` on a masked copy: the tool doing the measuring is ImageMagick,
# reading an X screenshot taken by `xwd`, neither of which is ours.
bbox() {
  magick "$1" -fuzz "${3:-6}%" -fill black +opaque "$2" -fill white -opaque "$2" \
    -format "%@" info: 2>/dev/null
}

# ---------------------------------------------------------------------------
# window: the milestone
# ---------------------------------------------------------------------------

# check_common <tag> <transport> [min-frames-presented]
#
# `min-frames-presented` is the client's own floor, not one number for both
# clients. It used to be 2 for everything, which is `weston-terminal`'s
# *structural* count — it draws once at map and once on the configure — and is
# therefore no bound at all on `weston-presentation-shm`, which presented 784.
# A window frozen after two frames passed the old check happily.
check_common() {
  local tag=$1 transport=$2 minframes=${3:-2} dir="$RUNDIR/$1"
  local srv="$dir/srv.log" px="$dir/proxy.log"

  if [ -f "$dir/why" ]; then
    bad "$tag: the Windows compositor never created its socket node"
    sed 's/^/        /' "$dir/srv.log" 2>/dev/null | head -10
    return 1
  fi

  grep -q "xdg_toplevel created" "$srv" \
    && ok "$tag: the client reached xdg_toplevel through the proxy" \
    || { bad "$tag: no xdg_toplevel"; sed 's/^/        /' "$srv" | head -20; return 1; }

  grep -q "window .* created (HWND" "$srv" \
    && ok "$tag: a real HWND was created for it" \
    || bad "$tag: no HWND"

  # The claim of the stage. Third-party client, Windows window, client's pixels.
  local presents
  presents=$(grep -o "([0-9]* frames presented)" "$srv" | grep -o "[0-9]*" | tail -1)
  presents=${presents:-0}
  # STAGE 48. **Zero is broken; short is inconclusive.** §35.9 item 3 flagged
  # this floor as having no margin — `weston-terminal`'s present count was 2, 2
  # and 5 across the runs measured — and asked what to do if a fourth
  # observation came in at 1. This is the answer, and it turns on the floor's
  # *basis* rather than on its value.
  #
  # The basis is structural, not a rate: `weston-terminal` draws once at map and
  # once on the configure, so 2 is what the client owes by construction and no
  # amount of load changes what it owes. What load can change is whether both
  # draws land inside this leg's 8-second window. So the two cases are genuinely
  # different facts and were reported as one:
  #
  #   0 frames  nothing of the client's pixels ever reached the window. That is
  #             the claim of the stage failing, and no load explains it — the
  #             client connected, mapped and committed to get this far.
  #   1..n-1    fewer than the structural count, inside a bounded window, on a
  #             machine that now reaches load average 175. That is a statement
  #             about the box, and §47.3's third outcome is where it belongs.
  #
  # This is deliberately *not* applied to `weston-presentation-shm`, whose floor
  # is 500 against the 784 it presents: that floor is a rate and a short count
  # there is a real regression in a client that draws continuously. The
  # distinction is the same one §47.2 draws between bytes and time — a
  # structural count can be bounded tightly, a rate cannot.
  if [ "$presents" -ge "$minframes" ]; then
    ok "$tag: $presents frames of the client's pixels reached the window (owed $minframes)"
  elif [ "$presents" -eq 0 ]; then
    bad "$tag: no frame of the client's pixels ever reached the window (owed $minframes)"
  elif [ "$minframes" -le 4 ]; then
    skip "$tag: $presents of the $minframes structural frames arrived inside the window at load $(loadnow) -- the client owes them by construction, so this is the window and not the code"
  else
    bad "$tag: only $presents frame(s) presented, against the $minframes this client owes"
  fi

  # *What crossed*, from the compositor's own count of what it opened. This is
  # the rule, checked rather than assumed: a run that smuggled a descriptor
  # across would produce an identical picture and fail here.
  local counts
  counts=$(grep -o "mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice" "$srv" | tail -1)
  case "$transport:$counts" in
    "shared-file:mapped: 0 section, "[1-9]*" shared-file, 0 arena-slice")
      ok "$tag: the compositor opened only shared files by name ($counts)" ;;
    "arena:mapped: 0 section, 0 shared-file, "[1-9]*" arena-slice")
      ok "$tag: the compositor resolved only arena offsets, opening nothing ($counts)" ;;
    *)
      bad "$tag: wrong transport served the pixels: ${counts:-nothing recorded}" ;;
  esac

  # And from the proxy's side: the descriptor it published, and that every
  # descriptor it received from the client was consumed locally.
  if grep -q "pool .* -> $transport" "$px"; then
    ok "$tag: the proxy published every pool through the $transport transport"
  else
    bad "$tag: the proxy did not use the $transport transport"
    grep "pool " "$px" | sed 's/^/        /' | head -5
  fi
  # **CHANGED IN STAGE 6, and the change is the interesting part.**
  #
  # This used to demand `0 out`, and that passed because the compositor had no
  # keyboard: nothing ever travelled server-to-client carrying an `h`. Stage 6
  # gives it one, so a client that binds `wl_seat` now receives a
  # `wl_keyboard.keymap` and the proxy hands it a real file descriptor.
  #
  # That is **not** a violation of the rule and the old check was measuring the
  # wrong thing. The rule is that nothing crosses *the boundary*; the fds
  # counted here are on the host side of it, manufactured locally by opening a
  # path the compositor named. `wc-proxy`'s upstream writer has no fd path at
  # all, which is what actually enforces the rule and is enforced by
  # construction rather than by this number.
  #
  # So what is asserted now is that every descriptor handed to the client was
  # manufactured rather than relayed — which is exactly what a failure to open
  # the name would report, and is checked by its absence.
  #
  # **CHANGED IN §31.** The check above was `case "$fds" in "") bad;; *) ok;;`,
  # which is to say: the two numbers were printed and the only thing asserted
  # was that the line existed. Both of them mean something.
  #
  #  * `in` is descriptors the *client* sent over `SCM_RIGHTS`. It must be at
  #    least one, because a `wl_shm` client cannot make a pool without sending
  #    the memfd — and if it were zero the whole premise of this harness, that a
  #    stock libwayland client's ancillary data is being handled, would be
  #    untested while the run still passed. Measured: 2 for `weston-terminal`
  #    (two pools) and 1 for `weston-presentation-shm`.
  #  * `out` is descriptors this side *manufactured* by opening a name the
  #    compositor sent — one `wl_keyboard.keymap` per seat bind. Measured: 1 for
  #    `weston-terminal`, 0 for `weston-presentation-shm`, which binds no seat.
  #    What matters is that it does not scale with anything: 784 frames
  #    presented against 0 fds out is the shape of "manufactured, not relayed".
  #    The bound is 4 rather than 1 so that a second seat or a keymap reload is
  #    not a failure, and it is 4 rather than unbounded so that a per-commit
  #    descriptor would be.
  local fds fds_in fds_out
  fds=$(grep -o "[0-9]* fds in, [0-9]* out" "$px" | tail -1)
  fds_in=$(echo "$fds" | grep -o "^[0-9]*")
  fds_out=$(echo "$fds" | grep -o "[0-9]* out$" | grep -o "^[0-9]*")
  if [ -z "$fds" ]; then
    bad "$tag: the proxy reported no descriptor counts"
  else
    [ "${fds_in:-0}" -ge 1 ] \
      && ok "$tag: the client sent $fds_in descriptor(s) over SCM_RIGHTS, so the ancillary path really ran" \
      || bad "$tag: the client sent no descriptors at all, so this run proves nothing about ancillary data"
    [ "${fds_out:-0}" -le 4 ] \
      && ok "$tag: $fds_out descriptor(s) out, each made here from a name -- not proportional to $presents frames" \
      || bad "$tag: $fds_out descriptors handed to the client; that is too many to be keymaps and looks relayed"
  fi
  if grep -q "cannot open" "$px"; then
    bad "$tag: the compositor named something this side could not open"
    grep "cannot open" "$px" | sed 's/^/        /' | head -3
  else
    ok "$tag: every name the compositor sent was openable on this side"
  fi

  # Two shots of a live window. `weston-terminal` is static by design and
  # `weston-presentation-shm` animates, so only the second can carry a live-ness
  # claim — the same asymmetry docs/waycompose-design.md §8.9 records for the Linux
  # harness.
  #
  # **CHANGED IN §31, twice over.**
  #
  # The old line was `ok "two screenshots ... differing by $differ pixels"`,
  # fired unconditionally. It passed at 0 pixels and it would have passed at any
  # other number: the screenshot pair was a picture taken for a human, in a
  # harness that runs headless. Two things are asserted in its place.
  #
  #  * **The screenshot has a window in it.** An Xvfb root with nothing on it is
  #    one colour; these are 336 (`weston-terminal`) and 120
  #    (`weston-presentation-shm`) distinct colours. A run where Wine drew
  #    nothing, or where the desktop came up empty, produced a screenshot the
  #    old check accepted and this one does not. Nothing else here looked at the
  #    *screen* — the frame dumps are the compositor's own view, from inside.
  #  * **The difference between the two, but only where there is a claim to
  #    make.** For a client that animates it is a bound with a bounded retry
  #    behind it (see `run_one`); for a static client it is informational and
  #    labelled as such, because "0 pixels changed" is the correct answer for
  #    `weston-terminal` and asserting on it either way would be inventing a
  #    claim to have one.
  local differ mindiff colours
  differ=$(cat "$dir/differ" 2>/dev/null || echo 0)
  mindiff=$(cat "$dir/mindiff" 2>/dev/null || echo 0)
  if [ -f "$dir/s1.png" ] && [ -f "$dir/s2.png" ]; then
    colours=$(magick identify -format "%k" "$dir/s1.png" 2>/dev/null)
    [ "${colours:-0}" -ge 16 ] \
      && ok "$tag: the screenshot has a window in it -- ${colours} distinct colours, against 1 for a bare root" \
      || bad "$tag: the screenshot has only ${colours:-0} distinct colours; the desktop looks empty"
    if [ "$mindiff" -gt 0 ]; then
      [ "$differ" -ge "$mindiff" ] \
        && ok "$tag: the window is live -- $differ pixels changed between the two shots (owed $mindiff)" \
        || bad "$tag: only $differ pixels changed across three tries; the window may be showing one frozen frame"
    else
      note "$tag: two screenshots 1.5 s apart differ by $differ pixels (informational: this client is static)"
    fi
  else
    bad "$tag: no screenshot was captured"
  fi
  return 0
}

run_window() {
  echo "== the milestone: an unmodified third-party client, in a Windows window =="
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build --workspace --target x86_64-pc-windows-gnu" || return
  fresh "$LINUX_BIN/wc-proxy" "cargo build --workspace" || return
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }
  command -v Xvfb >/dev/null || { bad "Xvfb not on PATH -- required, see the header"; return; }
  command -v magick >/dev/null || { bad "imagemagick not on PATH -- required"; return; }

  start_x 93 || { bad "could not start Xvfb"; return; }
  ok "Xvfb up on $DISPLAY (1280x1024x24)"

  # --- weston-terminal, over a shared file -------------------------------
  #
  # Toytoolkit, Argb8888, real text rendering, and the client whose pool growth
  # shaped the whole deferral design (see `Session::flush_deferred`).
  if command -v weston-terminal >/dev/null; then
    echo "  -- weston-terminal / shared-file --"
    # 2 frames: `weston-terminal` paints when it is mapped and when it is
    # configured, and then stops. Measured at 2, 2 and 5 across runs, so 2 is
    # its floor rather than its value — a count, not a rate, which is why it
    # survives a loaded machine, and why it is no bound at all for the client
    # below.
    run_one term weston-terminal shared-file damage 8
    if check_common term shared-file 2; then
      local srv="$RUNDIR/term/srv.log"
      grep -q 'title -> "Wayland Terminal"' "$srv" \
        && ok "term: the client's own title reached the Windows caption" \
        || bad "term: no title"
      grep -q 'app_id -> "org.freedesktop.weston.wayland-terminal"' "$srv" \
        && ok "term: set_app_id arrived, which is what the taskbar icon will key off" \
        || bad "term: no app_id"
      # **CHANGED IN STAGE 9, and the change is the point.** This used to expect
      # 806x491, which is the size of the client's *buffer*. `weston-terminal`
      # draws a 32-pixel drop shadow and resize margin around its frame and
      # tells the compositor which part of that is the window with
      # `xdg_surface.set_window_geometry` — 742x427 at +32,+32. Until stage 9
      # nothing honoured it, so the window was the buffer: offset by 32 px on
      # two sides, showing the client's own title bar under Windows' caption,
      # with the opposite edges clipped away. Stage 8 measured that; §13.6 has
      # both halves. The old number passing was the bug.
      grep -q "mapped 742x427" "$srv" \
        && ok "term: the window is 742x427, the client's window-geometry rectangle" \
        || bad "term: unexpected geometry: $(grep -o 'mapped [0-9]*x[0-9]*' "$srv" | head -1)"

      # The pixels, read back by tools that are not ours. The dump is the
      # compositor's view; the checks are stage 3's.
      # The pixels, read back out of the process by tools that are not ours.
      # `--dump-frames` is new on Win32 for stage 4 and is what gives this leg
      # the read-back the Linux leg always had (docs/waycompose-design.md §8.5).
      local frame
      frame=$(find "$RUNDIR/term/frames" -name '*.ppm' 2>/dev/null | sort | tail -1)
      if [ -n "$frame" ]; then
        local geom fw fh colours
        geom=$(magick identify -format "%wx%h" "$frame" 2>/dev/null)
        fw=${geom%x*}; fh=${geom#*x}
        colours=$(magick identify -format "%k" "$frame" 2>/dev/null)
        # **CHANGED IN STAGE 9 for the same reason as the line above.** The dump
        # is the window's shadow, and the window's shadow is now the composited
        # scene cropped to the geometry rectangle rather than a copy of the
        # client's buffer. So it is exactly the window size, and asserting
        # equality is now the right thing where before it would have been wrong.
        if [ "$geom" = "742x427" ]; then
          ok "term: the dumped frame decodes as $geom with $colours distinct colours"
        else
          bad "term: the dumped frame is $geom, not the 742x427 window"
        fi
        [ "${colours:-0}" -ge 16 ] \
          && ok "term: the frame has structure -- a terminal with text in it, not a flat field" \
          || bad "term: only ${colours:-0} colours"

        # **The flip check, and it is unmissable by construction.**
        # `weston-terminal` draws its own toytoolkit title bar as a light band
        # across the top of its buffer and a black terminal field below. An
        # upside-down blit puts the light band at the bottom, which is exactly
        # docs/waycompose-design.md §8.11 item 3's worry about `SetDIBitsToDevice` with a
        # negative `biHeight` — the one place the documentation is ambiguous
        # and the most likely to differ between Wine's GDI and the real one.
        #
        # **How many light rows are in each half.** Three earlier attempts
        # measured the wrong thing and each failure is worth keeping, because
        # each looked like the right measurement:
        #
        #  * a band at the very top and the very bottom: both are toytoolkit's
        #    drop shadow, and it reported 0 against 0;
        #  * a band a tenth of the way in: lands just below the title bar, 0.20
        #    against 0.01 — the right sign and not a convincing margin;
        #  * the *brightest* row of each half: 0.95 against 0.76, because the
        #    frame's bottom border is a light line one or two rows tall, so a
        #    maximum cannot tell a title bar from a border.
        #
        # A title bar is tens of light rows and a border is one or two, so
        # counting is what separates them. Nothing here needs to know where the
        # bar is, only that a broad light band is in the top half and not the
        # bottom — which is exactly what "not upside down" means for this client.
        local half=$((fh / 2)) top bot
        top=$(magick "$frame" -crop "${fw}x${half}+0+0" +repage -colorspace Gray \
              -resize "1x${half}!" -threshold 50% -format "%[fx:int(mean*h+0.5)]" info: 2>/dev/null)
        bot=$(magick "$frame" -crop "${fw}x${half}+0+${half}" +repage -colorspace Gray \
              -resize "1x${half}!" -threshold 50% -format "%[fx:int(mean*h+0.5)]" info: 2>/dev/null)
        if [ "${top:-0}" -ge 8 ] && [ "${top:-0}" -gt $(( ${bot:-0} * 3 )) ]; then
          ok "term: ${top} light rows in the top half against ${bot} in the bottom -- the title bar is up, not flipped"
        else
          bad "term: ${top:-?} light rows top, ${bot:-?} bottom -- the image may be upside down"
        fi
      else
        bad "term: no frame was dumped"
      fi
      # **§32, and this one was found by the merge rather than by the audit.**
      #
      # This was `ok` on 0 and `note` on anything else, so it could not fail —
      # and on the merged tree it took the `note` arm (124, the timeout) and the
      # scoreboard went from 54 to 53 with nothing reported. A check whose
      # failing arm is informational does not merely fail to catch things; it
      # *moves the total silently*, which is worse, because the total is what a
      # person compares between runs.
      #
      # 0 and 124 are both correct. `weston-terminal` runs until something stops
      # it, so `timeout` killing it is the ordinary ending, and which of the two
      # happens depends on whether the compositor's own `--timeout` closed the
      # connection first. Anything else — a segfault at 139, a protocol error at
      # 1 — is the client dying on us, which is exactly what this line was
      # supposed to notice and could not.
      local rc
      rc=$(cat "$RUNDIR/term/cli.rc")
      case "$rc" in
        0|124) ok "term: the client exited $rc (0 = clean, 124 = outlived the harness's timeout)" ;;
        *)     bad "term: the client exited $rc -- neither a clean exit nor the timeout; it died" ;;
      esac
    fi
  else
    note "SKIP weston-terminal (not installed)"
  fi

  # --- weston-presentation-shm, over the arena ---------------------------
  #
  # A simple-client rather than a toytoolkit one, pure `wl_shm`, and it redraws
  # continuously — which is the only way to show the blit is live rather than
  # one frame held. Over the arena, because that is the transport the real
  # deployment uses and it must carry the same client.
  if command -v weston-presentation-shm >/dev/null; then
    echo "  -- weston-presentation-shm / arena --"
    # 30 frames, and 500 pixels of screenshot difference. Measured: 784 frames
    # and 24,864 pixels in an 8-second run, so both floors are more than an
    # order of magnitude clear of the truth and more than an order of magnitude
    # above the frozen case. The live-ness bound moved into `check_common` with
    # `run_one`'s bounded retry behind it: two shots can land inside one frame
    # on a loaded machine, and that is the load, not the code.
    run_one pres weston-presentation-shm arena damage 8 500
    if check_common pres arena 30; then
      # Stage 3's kind of measurement, on a client that is not ours.
      #
      # Pure red, green and blue must all be on the captured screen: the wheel
      # arrived with its channels intact. This is the check that a `Xrgb8888`
      # buffer and a 32-bit `BI_RGB` DIB really are the same bytes in the same
      # order (docs/waycompose-design.md §8.4) — a B/R swap anywhere between the client's
      # `memfd` and the glass would leave a plausible-looking wheel with red and
      # blue exchanged, and the harness would still find three primaries, so the
      # *positions* are what carry it below.
      #
      # Not measured against the white field: the Wine window frame is white
      # too, so its bounding box spans the frame and says nothing. Measured 662x431
      # for a 250x250 client, which is a check that would have passed happily
      # while measuring the wrong thing.
      local seen=0 minx=99999 miny=99999 maxx=0 maxy=0
      for c in red lime blue; do
        local b
        b=$(bbox "$RUNDIR/pres/s1.png" "$c" 8)
        [ -z "$b" ] && continue
        seen=$((seen + 1))
        # `WxH+X+Y`, split without assuming anything about field widths.
        local w h x y rest
        w=${b%%x*}
        rest=${b#*x}
        h=${rest%%+*}
        x=$(echo "$b" | cut -d+ -f2)
        y=$(echo "$b" | cut -d+ -f3)
        [ "$x" -lt "$minx" ] && minx=$x
        [ "$y" -lt "$miny" ] && miny=$y
        [ "$((x + w))" -gt "$maxx" ] && maxx=$((x + w))
        [ "$((y + h))" -gt "$maxy" ] && maxy=$((y + h))
      done
      [ "$seen" = 3 ] \
        && ok "pres: red, green and blue are all on screen -- the channel order survived" \
        || bad "pres: only $seen of 3 primaries found on screen"

      # The wheel is inscribed in the client's 250x250 buffer, so the union of
      # the three coloured wedges must fit inside 250x250 and must not be tiny.
      # That is the scale check: a DPI-virtualised window (docs/waycompose-design.md §8.10
      # item 9) would be drawn larger, and a stride error would smear the wedges
      # horizontally past the buffer's width. Both fail this and neither fails a
      # "the colours are there" check.
      if [ "$seen" = 3 ]; then
        local uw=$((maxx - minx)) uh=$((maxy - miny))
        if [ "$uw" -le 250 ] && [ "$uh" -le 250 ] && [ "$uw" -ge 120 ] && [ "$uh" -ge 120 ]; then
          ok "pres: the wedges span ${uw}x${uh} on screen, inside the client's 250x250 buffer and at 1:1"
        else
          bad "pres: the wedges span ${uw}x${uh}, which does not fit a 250x250 buffer drawn 1:1"
        fi
      fi

      local frame
      frame=$(find "$RUNDIR/pres/frames" -name '*.ppm' 2>/dev/null | sort | tail -1)
      [ -n "$frame" ] && [ "$(magick identify -format "%wx%h" "$frame" 2>/dev/null)" = "250x250" ] \
        && ok "pres: the dumped frame decodes as 250x250" \
        || bad "pres: the dumped frame is $(magick identify -format "%wx%h" "$frame" 2>/dev/null)"
    fi
  else
    note "SKIP weston-presentation-shm (not installed)"
  fi

  # --- the cross-check: the other pairing --------------------------------
  #
  # Each client has now driven one transport. Swapping them is what says the
  # result is a property of the path and not of one client's habits, which is
  # the same argument docs/waycompose-design.md §8.5 makes for running two clients at all.
  if command -v weston-terminal >/dev/null; then
    echo "  -- weston-terminal / arena (the cross-check) --"
    run_one term-arena weston-terminal arena damage 8
    check_common term-arena arena 2
  fi

  stop=0
}

# ---------------------------------------------------------------------------
# cost: what the copy actually costs, measured rather than argued
# ---------------------------------------------------------------------------

run_cost() {
  echo "== what a commit costs, per copy scope and per transport =="
  # The cost table is the number this harness is trusted for, so it is the last
  # place that may be computed from a binary older than the tree. See `fresh`.
  fresh "$LINUX_BIN/wc-proxy" "cargo build --workspace" || return
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build --workspace --target x86_64-pc-windows-gnu" || return
  command -v weston-presentation-shm >/dev/null \
    || { note "SKIP (weston-presentation-shm not installed)"; return; }
  start_x 94 || { bad "could not start Xvfb"; return; }

  # The client's own buffer, in KiB: `weston-presentation-shm` draws a 250x250
  # wheel at 4 bytes a pixel, which is 244.14 KiB and is asserted to be 250x250
  # by the window leg above. **No copy scope may ever move more than one extent
  # per copy**, because every scope is defined as copying a *subset* of a
  # buffer — except `pool`, whose subset is the whole pool and whose ceiling is
  # therefore the pool size, checked as the top of the hierarchy below instead.
  #
  # This is the number the 51x regression violated by 55x: 13 MiB per copy of a
  # 244 KiB buffer. It is an absolute, so unlike a comparison between two runs
  # it cannot be defeated by the two runs seeing different amounts of load.
  local extent_kib=244 extent_ceiling=250

  printf "  %-12s %-6s %8s %10s %12s %10s %10s\n" \
    transport scope commits copies "MiB copied" "KiB/copy" "us/copy"
  for transport in shared-file arena; do
    for scope in pool buffer damage; do
      local tag="cost-$transport-$scope"
      # STAGE 47. **Measure until the sample is big enough, rather than for a
      # fixed time and then complain.**
      #
      # The floor below is `commits >= 50`, and its reasoning is sound on an
      # idle box: 620 to 736 commits were measured in 6 seconds, so 50 is more
      # than ten times clear. But it is a floor on a *rate*, and this machine now
      # runs full-profile workspace builds at a load average north of 100 on 32
      # cores. At that point a 6-second window can genuinely fail to reach 50
      # commits, and the harness would report `bad` — announcing a defect in the
      # tree because the box was busy. **A threshold picked when the machine was
      # quiet cannot tell slow from broken.**
      #
      # So the window grows instead of the verdict changing: 6 s, then 18 s if
      # the sample was short. A slow machine pays time and still produces a
      # measurement; a broken one produces a short sample twice and is reported
      # as inconclusive with the load quoted, not as a failure. That is the
      # distinction the fixed window could not express.
      #
      # The retry is not flakiness-papering: the *cost* gates below are ratios
      # per commit, which are load-independent by construction (§31), so a
      # longer window changes the confidence and not the number.
      local secs=6 attempt=0 commits_seen=0 line=""
      while [ "$attempt" -lt 2 ]; do
        attempt=$((attempt + 1))
        run_one "$tag" weston-presentation-shm "$transport" "$scope" "$secs" 0 0 || break
        line=$(grep -o "[0-9]* commits, [0-9]* copies, [0-9.]* MiB in [0-9.]* ms" "$RUNDIR/$tag/proxy.log" | tail -1)
        commits_seen=$(printf '%s' "$line" | grep -o '^[0-9]*')
        [ "${commits_seen:-0}" -ge 50 ] && break
        [ "$attempt" -ge 2 ] && break
        note "$tag: ${commits_seen:-0} commits in ${secs}s at load $(loadnow) -- widening the window to 18s rather than calling it a failure"
        secs=18
      done
      [ -n "$line" ] || continue
      local us kib
      us=$(grep -o "[0-9]* us per copy" "$RUNDIR/$tag/proxy.log" | tail -1 | grep -o "^[0-9]*")
      kib=$(grep -o "[0-9.]* KiB and" "$RUNDIR/$tag/proxy.log" | tail -1 | grep -o "^[0-9.]*")
      set -- $line
      printf "  %-12s %-6s %8s %10s %12s %10s %10s\n" \
        "$transport" "$scope" "$1" "$3" "$5" "${kib:-n/a}" "${us:-n/a}"
      echo "$transport $scope $line ${us:-0}us ${kib:-0}kib" >>"$RUNDIR/cost.txt"
    done
  done

  # **The row count, not "is the file non-empty".** The old check said "six
  # configurations measured" whenever `cost.txt` had a single byte in it, so a
  # run where five of the six legs died printed the word "six" and passed. A
  # message that states a number the check does not verify is the same defect as
  # a number nobody asserts on, one level up.
  local rows
  rows=$(wc -l <"$RUNDIR/cost.txt" 2>/dev/null || echo 0)
  if [ "$rows" -eq 6 ]; then
    ok "all six configurations produced a measurement"
  else
    bad "$rows of 6 configurations produced a measurement; the table above is incomplete"
  fi

  # Per row: the shape of the run, and the cost of a copy.
  #
  # `commits >= 50` is a floor on *meaningfulness* rather than on speed — 620 to
  # 736 were measured for a 6-second run, so 50 is more than ten times clear of
  # the truth, and its job is only to stop a ratio being computed over three
  # commits of noise.
  #
  # `copies <= 2 x commits` is the 51x regression stated as what it actually
  # was: a buffer copied 55 times where it should have been copied once. Every
  # scope copies at most once per commit by construction, and 698/699, 725/726,
  # 735/736 were measured — one copy per commit, less the commit that attaches
  # nothing. Twice that is 27x below the regression and cannot be reached by any
  # amount of load.
  # `<transport> <scope> <N> commits, <M> copies, <X> MiB in <Y> ms <U>us <K>kib`
  local t s commits copies kibper
  while read -r t s commits _ copies _ _ _ _ _ _ _ kibper; do
    kibper=${kibper%kib}
    # STAGE 47. **A short sample is inconclusive, not failing.** This was
    # `bad "only N commits in 6 s -- too few to measure over"`, which is a true
    # sentence filed under the wrong verdict: too few commits says the machine
    # was busy, and nothing about whether the code copies correctly. With the
    # window now widening once before giving up, reaching this line means two
    # windows were short — still a fact about the box, and reported as one, with
    # the load quoted so it can be told from a real stall.
    #
    # The load-independent gates below (`copies <= 2 x commits`, KiB per copy,
    # and the per-commit ratios) are *not* skipped by this: they still run on
    # whatever sample there is, because they are properties of the code and a
    # thin sample cannot make a 55x regression look like 1x.
    local why="" thin=""
    [ "$commits" -lt 50 ] && thin="only $commits commits over two windows at load $(loadnow) -- too thin to draw a rate from"
    if [ -z "$why" ] && [ "$copies" -gt $((commits * 2)) ]; then
      why="$copies copies for $commits commits -- a buffer is being copied more than once per commit"
    fi
    if [ -z "$why" ] && [ "$s" != pool ] \
       && awk -v k="$kibper" -v c="$extent_ceiling" 'BEGIN { exit !(k > c) }'; then
      why="$kibper KiB per copy of a ${extent_kib} KiB buffer -- a scope that copies part of a buffer moved more than all of it"
    fi
    # The code gates win over the sample-size note: a run that is BOTH thin and
    # copying 55 times per commit is a failure, and saying "inconclusive" there
    # would be the skip hiding a defect. Only a thin sample with nothing else
    # wrong is inconclusive.
    if [ -n "$why" ]; then
      bad "$t/$s: $why"
    elif [ -n "$thin" ]; then
      skip "$t/$s: $thin (the per-copy and per-commit gates still ran and found nothing wrong)"
    else
      ok "$t/$s: $commits commits, $copies copies, $kibper KiB per copy -- once per commit, never more than the extent"
    fi
  done <"$RUNDIR/cost.txt"

  # Microseconds per copy is the one number here that is **informational, and
  # deliberately so**. It is a rate measured on a machine running Xvfb, Wine, a
  # Windows compositor, a client and this script at once; the same tree produced
  # 37 us and 103 us for the same 244 KiB copy in a single run of this file. A
  # bound loose enough to survive that would not catch anything, and a tight one
  # would fail on load. Bytes are a property of the code and are what the gate
  # above is built on; time is a property of the machine and is printed for a
  # person looking at a specific run.
  note "us/copy above is informational: it is a rate on a loaded machine, and the assertions are on bytes"

  # **The table is now checked and not only printed**, and that is §27's
  # doing. `--copy damage` is the scope the guest runs; `--copy buffer` copies
  # the whole extent every commit and is therefore the ceiling — no scope that
  # copies a *subset* of a buffer may cost more than copying all of it.
  #
  # `weston-presentation-shm` damages its whole buffer every frame, so the two
  # should land within noise of each other, and for six stages they did. Then a
  # damage-tracking change made `damage` copy 8583 MiB against `buffer`'s 128
  # for the same 700 commits — 51x — and this harness reported **37 passed, 0
  # failed**, because six numbers were printed for a human who was not looking.
  # A measurement nothing asserts on is a measurement that has stopped being a
  # test, which is the same lesson §14.9 drew about comments.
  #
  # **CHANGED AGAIN IN §31: per commit, and the whole hierarchy.**
  #
  # The §27 check compared the two runs' raw MiB totals. That is a ratio between
  # two separately-timed runs, and the client is timed rather than counted — so
  # the divisor was itself load-dependent, and the 1.5x ceiling had to be loose
  # enough to absorb that. Dividing by each run's own commit count cancels the
  # load out: 175.24/736 against 172.85/726 is 1.0002x where the raw totals gave
  # 1.01x, and the ceiling can come down to 1.25x without becoming noise. A gate
  # that is load-independent can afford to be tight, which is the whole reason
  # to make it load-independent.
  #
  # Not 1.0x: the two scopes copy the same pixels here but not the same *bytes*
  # — `copy_damage` skips the padding between rows when a rectangle is not
  # full-width.
  #
  # And the rest of the hierarchy, which nothing checked: `pool` copies the
  # entire pool every commit and is therefore the ceiling for every other scope
  # by definition. Measured at 14.3 MiB a commit against 0.238 for the other
  # two, a 60x gap — so the assertion has enormous margin, and its job is to
  # catch a `pool` scope that has quietly stopped copying the pool, which would
  # otherwise look like an improvement.
  per_commit() {
    awk -v t="$1" -v s="$2" '$1==t && $2==s { printf "%.6f", $7 / ($3 > 0 ? $3 : 1) }' "$RUNDIR/cost.txt"
  }
  local ratio
  for transport in shared-file arena; do
    local pc_pool pc_buffer pc_damage
    pc_pool=$(per_commit "$transport" pool)
    pc_buffer=$(per_commit "$transport" buffer)
    pc_damage=$(per_commit "$transport" damage)
    if [ -z "$pc_buffer" ] || [ -z "$pc_damage" ]; then
      bad "$transport: no buffer/damage pair to compare -- the cost gate did not run for this transport"
      continue
    fi
    ratio=$(awk -v d="$pc_damage" -v b="$pc_buffer" 'BEGIN { printf "%.3f", (b > 0 ? d / b : 999) }')
    if awk -v r="$ratio" 'BEGIN { exit !(r <= 1.25) }'; then
      ok "$transport: damage costs ${ratio}x buffer per commit -- the subset is not more expensive than the whole"
    else
      bad "$transport: damage costs ${ratio}x buffer per commit ($pc_damage vs $pc_buffer MiB); a scope that copies part of a buffer must not cost more than copying all of it"
    fi
    if [ -z "$pc_pool" ]; then
      bad "$transport: no pool measurement, so the top of the scope hierarchy is unchecked"
    elif awk -v p="$pc_pool" -v b="$pc_buffer" 'BEGIN { exit !(p >= b) }'; then
      ok "$transport: pool costs $pc_pool MiB a commit against buffer's $pc_buffer -- the whole pool is still the ceiling"
    else
      bad "$transport: pool costs $pc_pool MiB a commit, less than buffer's $pc_buffer; pool has stopped copying the pool"
    fi
  done
}

case "${1:-all}" in
  window) run_window ;;
  cost)   run_cost ;;
  all)    run_window; cleanup_x=1; kill "${XPID:-0}" 2>/dev/null; unset XPID; run_cost ;;
  *) echo "usage: $0 [window|cost|all]"; exit 2 ;;
esac

echo
# STAGE 47. The inconclusive count is in the summary, and only there when it is
# non-zero — a run that measured everything should not have to say so. It does
# **not** affect the exit status: "we could not measure it" is not a failure,
# and a harness that exited non-zero for load would teach everyone to re-run
# until green, which §35.1 names as worse than failing.
if [ "$inconc" -gt 0 ]; then
  echo "== $pass passed, $fail failed, $inconc inconclusive (load $(loadnow)) =="
  echo "   inconclusive = the machine was too loaded to draw a rate; the code gates still ran."
else
  echo "== $pass passed, $fail failed =="
fi
[ -n "$KEEP" ] && echo "artefacts kept in $RUNDIR"
[ "$fail" -eq 0 ]
