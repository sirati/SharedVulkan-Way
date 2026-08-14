#!/usr/bin/env bash
# Does the region hold what the client drew, while the client is **scrolling**?
#
#   scripts/wc-damage.sh scroll     Firefox scrolling about:support, at --copy damage
#   scripts/wc-damage.sh arena      the same, over the arena -- the deployment's transport
#   scripts/wc-damage.sh control    the same run at --copy buffer, which cannot fail
#   scripts/wc-damage.sh all        all three
#
# Run from inside `nix develop`.
#
# THE QUESTION, AND WHY A SCREENSHOT CANNOT ANSWER IT
#
# The user's clause is "screen must not tear or stop rendering of scrolling no
# blank slides (e.g. in about:support)". docs/waycompose-design.md §18 found and fixed one
# cause of the first symptom — an incoherent virtio-fs export — by dumping the
# compositor's own frames and finding them torn identically to the screen. That
# technique cannot find *this* one, and the reason is worth stating: a stale
# band in the region is a **plausible picture**. It is real content, correctly
# blitted, from two frames ago. No screenshot, no frame dump and no colour
# histogram distinguishes it from a page that simply had not scrolled yet, and
# §10.7's "count the distinct frames" would count it as a distinct frame,
# because it is one.
#
# So the oracle is the client's own memory. `wc-proxy --verify-damage` compares,
# after every copy, the region's bytes for the attached buffer against the
# client's bytes for the same buffer, and counts the rows that differ. The
# comparison is exact and race-free by protocol: between `wl_surface.commit` and
# `wl_buffer.release` the client may not touch the buffer it attached, and that
# buffer is required to hold the whole current surface content. So a
# disagreement has exactly one possible author, which is the proxy.
#
# WHAT THE TWO BLOCKS ARE FOR, AND WHY BOTH
#
# `scroll` runs at `--copy damage`, which is what `wc-guest.sh` and
# `wc-desktop.sh` run on the real guest. `control` runs the identical harness at
# `--copy buffer`, which copies the whole extent every commit and therefore
# *cannot* be stale. A run where both report zero says the damage path is right.
# A run where `scroll` reports stale rows and `control` reports none says the
# fault is damage tracking's and not the transport's, the arena's or Wine's —
# which is the distinction that took §18 a whole stage to draw the first time.
#
# It also says something about the existing harnesses: `wc-firefox.sh`'s wine
# leg runs `--copy pool`, and `wc-proxy.sh cost` runs all three but with
# `weston-presentation-shm`, which damages its whole buffer every frame. Neither
# could ever have caught this. The scope that ships was the scope nothing tested
# with a client that scrolls.
#
# WHAT WINE DOES NOT PROVE. Unchanged and still the most important sentence:
# Wine is not Windows, it has been wrong four times on this project (§13.6), and
# a *negative* here is not a result. What this harness measures, though, is
# unusually insulated from that: `--verify-damage` runs entirely inside
# `wc-proxy`, which is a Linux process, comparing two mappings this side owns.
# Wine's only role is to be an upstream that consumes the stream. A stale row
# counted here is a real stale row on the guest too.
#
# THE INSTRUMENT IS NOT A CLOCK. `--verify-damage` reads the whole buffer every
# commit, which is precisely the cost `--copy damage` exists to avoid. §13.8
# retracted a 2x throughput claim that turned out to be 1.03x because the
# instrument -- a PPM written over virtiofs -- was inside the loop being timed.
# So: nothing in this file times anything, and no number produced with
# `--verify-damage` on may be used to argue about cadence.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

pass=0; fail=0; skip=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
non() { echo "  SKIP  $*"; skip=$((skip + 1)); }
note(){ echo "  ....  $*"; }

# STAGE 42. `fresh` / `fresh_or_skip`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"
# And which memory the pixels cross, declared and then asserted rather than
# assumed. §27 measured its whole result over `shared-file` and said nothing
# about it, which is exactly the omission `wc_assert_substrate` exists for.
. "$(dirname "$0")/wc-substrate.sh"

RUNDIR=$(mktemp -d /tmp/wc-damage-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

FIREFOX=${WC_FIREFOX:-firefox}
export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"
OUTPUT=1600x1000
# Its own display number, and its own desktop name, because §16.8's rule is that
# identity on a shared machine is never a class of thing. A harness that reused
# `wc-firefox.sh`'s :77 would take that harness's Xvfb down with it.
WC_X_DISPLAY=${WC_X_DISPLAY:-79}

have() { command -v "$1" >/dev/null 2>&1; }

# The page. `about:support` is the one the requirement names, and it is a good
# choice independently of that: several screens of dense tables, so a scroll
# repaints a large fraction of the window with content that is different from
# what was there, which is the load the bug needs. `about:blank` would scroll
# nothing and pass everything.
PAGE=${WC_PAGE:-about:support}

# ---------------------------------------------------------------------------
# one run: Firefox -> wc-proxy (--verify-damage) -> wc-compositor.exe, scrolling
# ---------------------------------------------------------------------------
#
# scroll_run <tag> <copy-scope>
#
# Returns non-zero only when the run could not be *made*; a run that produced
# numbers returns 0 and the assertions are the caller's.
scroll_run() {
  local tag=$1 scope=$2 transport=${3:-shared-file}
  local dir="$RUNDIR/$tag"
  mkdir -p "$dir/rt" "$dir/pools" "$dir/keymaps" "$dir/profile"
  chmod 700 "$dir/rt"

  winp() { local p=$1; echo "Z:${p//\//\\}"; }

  # **Which memory, declared before anything starts.** Both legs are fixtures on
  # this box for the same reason — a Wine run has no QEMU, so there is no guest
  # RAM to locate — and `wc_substrate_declare` refuses to say so if `WC_HOST` or
  # a configured guest is in the environment, which is what stops this harness
  # measuring a file on a machine that has the real thing.
  local arena_size=$((64 * 1024 * 1024)) arena_out=$((1024 * 1024))
  local comp_extra=() proxy_extra=()
  case "$transport" in
    shared-file)
      wc_substrate_declare wine-fixture \
        "no QEMU on this box: a Wine run has no guest RAM to locate" || return 1
      comp_extra=(--shared-dir "$(winp "$dir/keymaps")" --shared-peer-prefix "$dir/keymaps/")
      proxy_extra=(--transport shared-file --pool-dir "$dir/pools"
                   --peer-prefix "$(winp "$dir/pools")\\") ;;
    arena)
      # The transport the **deployment** uses, which is the reason this leg was
      # added rather than the substrate rule being satisfied on paper. The arena
      # packs every pool into one region behind an allocator that moves offsets,
      # reclaims cold pools (§16) and hands regions back per frame (dynamic
      # arena); `shared-file` gives each pool its own file at offset 0 and never
      # does any of that. The arrears bookkeeping is keyed on buffer id and is
      # believed transport-agnostic — believed is the operative word, and this
      # branch is what turns it into measured.
      wc_substrate_declare wine-fixture \
        "no QEMU on this box: a Wine run has no guest RAM to locate" || return 1
      : >"$dir/arena"; truncate -s $arena_size "$dir/arena"
      comp_extra=(--arena-provide "0:$arena_size:$(winp "$dir/arena")"
                  --arena-outbound "0:$arena_out")
      proxy_extra=(--transport arena --arena-path "$dir/arena" --arena-id 0
                   --arena-size $arena_size --arena-outbound $arena_out) ;;
    *) bad "$tag: unknown transport $transport"; return 1 ;;
  esac

  Xvfb ":$WC_X_DISPLAY" -screen 0 1600x1100x24 -nolisten tcp >"$dir/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$WC_X_DISPLAY"
  local up=""
  for _ in $(seq 1 100); do
    xwd -root -silent >/dev/null 2>&1 && { up=1; break; }
    sleep 0.1
  done
  [ -n "$up" ] || { bad "$tag: Xvfb never came up"; return 1; }

  # §8.9: a desktop name that has never been used, and a gap before starting.
  sleep 3
  local desk="wc-damage-$$-$RANDOM-$tag"
  local sock="wl-dmg-$$-$tag"
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  wine explorer "/desktop=$desk,1600x1050" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 200 --output "$OUTPUT" \
    "${comp_extra[@]}" \
    --log "$(winp "$dir/srv.log")" >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node=""
  for _ in $(seq 1 300); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$sock" 2>/dev/null | head -1)
    [ -n "$node" ] && break
    sleep 0.2
  done
  if [ -z "$node" ]; then
    bad "$tag: the Windows compositor never created its socket node"
    sed 's/^/        /' "$dir/srv.log" 2>/dev/null | head -10
    kill "$srv" 2>/dev/null
    return 1
  fi

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --copy "$scope" --verify-damage \
    "${proxy_extra[@]}" \
    --accept-timeout 40 --timeout 200 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 1

  env -u DISPLAY MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
    XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout 120 "$FIREFOX" --no-remote --profile "$dir/profile" "$PAGE" \
    >"$dir/fx.log" 2>&1 &
  local cli=$!

  # The scroll itself, from outside every process involved — and **inside the
  # same virtual desktop**, which `wc-input.sh` records as the thing that cost
  # it a run: `FindWindowW` enumerates one desktop's windows, so a probe on the
  # default desktop finds no `WaycomposeToplevel`, injects into the wrong input
  # queue, and reports a compositor that ignores input. Naming the desktop that
  # is already running attaches to it rather than making a second.
  wine explorer "/desktop=$desk,1600x1050" \
    "$dir/probe-scroll.exe" --class WaycomposeToplevel --wait 60 \
    --notches "${WC_NOTCHES:-120}" --gap "${WC_GAP:-150}" \
    --log "$(winp "$dir/scroll.log")" >"$dir/scroll.out" 2>&1

  # **Wait for the probe's own last line, not for the launcher.** `wine
  # explorer /desktop=NAME` attaches to a desktop that already exists — this one
  # does, the compositor made it — and having handed the program to it, the
  # launcher returns at once. `wc-input.sh` never noticed because its probe
  # takes about a second; this one scrolls for twenty, and the first version of
  # this harness tore the whole run down two seconds in and then complained that
  # the client had not scrolled enough. The visible symptom was a *low commit
  # count*, which is exactly the symptom this file is investigating, so it is
  # worth being blunt: a harness that measures a stall it caused itself is worse
  # than no harness.
  local scroll_rc=1
  for _ in $(seq 1 300); do
    grep -q 'notches towards the user' "$dir/scroll.log" 2>/dev/null && { scroll_rc=0; break; }
    sleep 0.5
  done

  sleep 2
  xwd -root -silent -out "$dir/s.xwd" 2>/dev/null && magick "$dir/s.xwd" "$dir/s.png" 2>/dev/null

  kill "$cli" 2>/dev/null; wait "$cli" 2>/dev/null
  sleep 1
  # SIGTERM and then let it print. The summary line is the measurement, so a
  # `kill -9` here would throw away the whole run.
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; XPID=

  echo "$scroll_rc" >"$dir/scroll.rc"
  return 0
}

# ---------------------------------------------------------------------------
# the assertions, identical for both scopes
# ---------------------------------------------------------------------------
check_run() {
  local tag=$1 scope=$2 floor=$3 dir="$RUNDIR/$tag"

  grep -q 'xdg_toplevel created' "$dir/srv.log" 2>/dev/null \
    && ok "$tag: Firefox reached xdg_toplevel through the proxy" \
    || { bad "$tag: no xdg_toplevel on the Windows side"
         sed 's/^/        /' "$dir/srv.log" 2>/dev/null | tail -12
         sed 's/^/        /' "$dir/proxy.log" 2>/dev/null | tail -8
         return 1; }

  # Which memory actually served the pixels, from the compositor's own counters
  # rather than from what this harness asked for. A run whose region turned out
  # to be something other than the declared substrate is a run whose stale-row
  # count is about the wrong thing.
  wc_assert_substrate "$dir/srv.log" "$tag"

  local notches
  notches=$(grep -m1 'notches towards the user' "$dir/scroll.log" 2>/dev/null)
  if [ "$(cat "$dir/scroll.rc" 2>/dev/null || echo 1)" = 0 ] && [ -n "$notches" ]; then
    ok "$tag: ${notches#probe-scroll: }"
  else
    bad "$tag: probe-scroll.exe never finished its notches: $(tail -1 "$dir/scroll.log" 2>/dev/null)"
    return 1
  fi

  # The scroll has to have produced work, or a zero stale count means nothing.
  # This is the check that stops the harness passing vacuously, which is the way
  # a correctness harness usually dies: the client never drew and every count
  # was zero and every assertion was true.
  local verified stale worst
  verified=$(grep -o 'verified [0-9]* commits' "$dir/proxy.log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  stale=$(grep -o '[0-9]* stale (worst [0-9]* rows)' "$dir/proxy.log" | awk -F' stale' '{print $1}' | awk '{s+=$1} END {print s+0}')
  worst=$(grep -o 'worst [0-9]* rows' "$dir/proxy.log" | grep -o '[0-9]*' | sort -n | tail -1)
  worst=${worst:-0}

  local skipped commits unreached
  skipped=$(grep -o '([0-9]* state-only, unverifiable' "$dir/proxy.log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  unreached=$(grep -o '[0-9]* never reached the copy' "$dir/proxy.log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  # **§31: `skipped` is now part of an equation instead of part of a sentence.**
  #
  # It was interpolated into the pass message and compared with nothing, which
  # matters because it is the *coverage* of the check below: `verified` says how
  # much was looked at and `skipped` says how much was not, and a run where the
  # second dwarfs the first is a strong assertion over a small corner of the
  # traffic.
  #
  # What is asserted is an identity rather than a threshold, because an identity
  # needs no number chosen on a machine: every commit is verified, classified
  # state-only, or never reached the copy at all, so the three must add up to
  # the commit count. If they do not, commits are passing through the instrument
  # without being classified any way — the check has a blind spot, and the size
  # of the blind spot is the discrepancy. That is a bound on coverage that
  # cannot be defeated by load, by the client, or by how long the run lasted.
  #
  # **The third term is §46's, and it was added because this assertion
  # failed.** The identity was originally written with two terms, on the premise
  # that `commit` increments one of them every time. It does not: it returns
  # early for a surface that has never had a buffer attached, for a buffer or
  # pool already gone, and for a pool the compositor does not have — all
  # legitimate, none of them counted. The first arena run came out 7 short of
  # 3711 and this line failed, correctly. Seven commits do not matter; an
  # identity with an unnamed remainder is not an identity, so the remainder was
  # named in `Stats::verify_unreached` rather than tolerated with a threshold.
  commits=$(grep -o '[0-9]* commits,' "$dir/proxy.log" | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
  local classified=$(( ${verified:-0} + ${skipped:-0} + ${unreached:-0} ))
  if [ "$classified" -ne "${commits:-0}" ]; then
    bad "$tag: $classified of $commits commits were classified; $(( ${commits:-0} - classified )) went through the check unaccounted for"
  else
    ok "$tag: every one of $commits commits is accounted for -- $verified checked, $skipped attached no buffer, $unreached never reached the copy"
  fi
  if [ "${verified:-0}" -ge "$floor" ]; then
    ok "$tag: $verified commits were checked against the client's own bytes (${skipped:-0} more attached no buffer, so there was nothing to check)"
  else
    bad "$tag: only ${verified:-0} commits were checked, below the floor of $floor; the run did not scroll enough to mean anything"
    sed 's/^/        /' "$dir/proxy.log" | tail -6
    return 1
  fi

  # THE ASSERTION. Zero, and there is no tolerance to set: a single row of
  # two-frames-ago content is the symptom, and a harness that allowed "a few"
  # would have passed the bug it was written for.
  if [ "${stale:-0}" -eq 0 ]; then
    ok "$tag ($scope): the region matched the client on every one of $verified commits"
  else
    bad "$tag ($scope): $stale of $verified commits left the region stale, worst $worst rows"
    grep -o 'verified .*rows)' "$dir/proxy.log" | sed 's/^/        /'
  fi
}

# block_scroll <copy-scope> <commit-floor>
#
# The floor differs between the two scopes, and the honest reason is variance
# rather than cost. How much a Firefox draws in twenty seconds of scrolling
# under Xvfb and Wine, on a machine ten agents are sharing, is not reproducible:
# the checked-commit count across the runs behind §27 was 454,
# 936, 1360, 1509, 1628 and 4106 at `damage`, and 20 and 1035 at `buffer`. The
# first draft of this comment blamed the `buffer` scope's larger copy for its
# low count, on the strength of a single 51-commit run; the next `buffer` run
# was 1035 and the explanation was wrong. §13.8's retraction is the precedent
# and this is the same mistake caught earlier, so it is written down rather than
# quietly deleted.
#
# So `damage` — the block that is the gate — is held to 200, which every run of
# it has cleared by at least 2x, and `control` to 20, because it corroborates
# rather than gates and a flaky FAIL in a corroboration is worse than a weak
# floor.
block_scroll() {
  local scope=${1:-damage} floor=${2:-200} transport=${3:-shared-file} tag
  tag="$scope-$transport"
  echo "== $scope over $transport: Firefox scrolling $PAGE, checked against the client's own bytes"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }
  have wine || { non "no wine"; return; }
  have Xvfb || { non "no Xvfb"; return; }
  have x86_64-w64-mingw32-gcc || { non "no mingw cross compiler for probe-scroll.exe"; return; }
  fresh_or_skip "$WIN_BIN/wc-compositor.exe" "no wc-compositor.exe; cargo build --target x86_64-pc-windows-gnu"; case $? in 2) non "no wc-compositor.exe; cargo build --target x86_64-pc-windows-gnu"; return ;; 1) return ;; esac
  fresh_or_skip "$LINUX_BIN/wc-proxy" "no wc-proxy; cargo build"; case $? in 2) non "no wc-proxy; cargo build"; return ;; 1) return ;; esac
  # §18.8: `cargo clippy` builds metadata and not an executable, and a stale
  # binary cost a run. The proxy must at least *know* the flag this harness
  # depends on, or the run measures the previous build.
  #
  # The output is captured *first* rather than piped. `--help` exits non-zero —
  # it is a usage error path — and with `set -o pipefail` the pipeline then
  # reports that exit status no matter what `grep` found, so the piped spelling
  # of this check skipped a proxy that had the flag. Recorded because it cost a
  # run and because every other harness here uses the piped spelling on programs
  # that happen to exit zero.
  local usage
  usage=$("$LINUX_BIN/wc-proxy" --help 2>&1)
  if ! grep -q -- --verify-damage <<<"$usage"; then
    non "$LINUX_BIN/wc-proxy has no --verify-damage; rebuild it"
    return
  fi

  mkdir -p "$RUNDIR/$tag"
  if x86_64-w64-mingw32-gcc -O1 -mwindows -o "$RUNDIR/$tag/probe-scroll.exe" \
       "$ROOT/scripts/probe-scroll.c" >"$RUNDIR/$tag/cc.log" 2>&1; then
    ok "linked probe-scroll.exe ($(stat -c %s "$RUNDIR/$tag/probe-scroll.exe") bytes)"
  else
    bad "could not link probe-scroll.exe"; sed 's/^/        /' "$RUNDIR/$tag/cc.log"; return
  fi

  # **A short run is re-run, not excused.** The floor exists to say "this run
  # produced enough work to mean something", and Firefox under Xvfb and Wine on
  # a box ten agents share sometimes produces almost none: 23 checked commits
  # where the same leg gives 1600 on either side of it. §27.10 item 3 already
  # records that the counts span 20 to 4106.
  #
  # There are two wrong answers and one right one. Lowering the floor to 20
  # would make the gate pass on a run that scrolled nothing, which is the
  # vacuous-pass failure this harness was written to avoid. Leaving it to flake
  # blocks whoever runs the gate next, for a reason that is not about the code.
  # So the run is repeated, up to twice, and the *floor stays where it is* — a
  # retry changes how many chances the workload gets, not what counts as a
  # result. A leg that comes up short twice fails, and that is a real signal.
  local attempt
  for attempt in 1 2; do
    scroll_run "$tag" "$scope" "$transport" || return
    local got
    got=$(grep -o 'verified [0-9]* commits' "$RUNDIR/$tag/proxy.log" 2>/dev/null \
          | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
    [ "${got:-0}" -ge "$floor" ] && break
    if [ "$attempt" = 1 ]; then
      note "$tag: only ${got:-0} commits checked, below the floor of $floor -- the client barely drew. Re-running once."
      # A clean directory, so the second run's logs cannot be read as the
      # first's, and the probe relinked into it because it lived there.
      rm -rf "${RUNDIR:?}/$tag"
      mkdir -p "$RUNDIR/$tag"
      x86_64-w64-mingw32-gcc -O1 -mwindows -o "$RUNDIR/$tag/probe-scroll.exe" \
        "$ROOT/scripts/probe-scroll.c" >"$RUNDIR/$tag/cc.log" 2>&1 \
        || { bad "$tag: could not relink probe-scroll.exe for the retry"; return; }
    fi
  done
  check_run "$tag" "$scope" "$floor"
  [ -n "$KEEP" ] && note "run kept in $RUNDIR/$tag"
}

case "${1:-all}" in
  scroll)  block_scroll damage 200 shared-file ;;
  arena)   block_scroll damage 200 arena ;;
  control) block_scroll buffer 20 shared-file ;;
  all)     block_scroll damage 200 shared-file
           block_scroll damage 200 arena
           block_scroll buffer 20 shared-file ;;
  *) echo "usage: $0 [scroll|arena|control|all]"; exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
