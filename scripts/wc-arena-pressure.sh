#!/usr/bin/env bash
# STAGE 16. What an unmodified Firefox does when the arena is too small.
#
#   scripts/wc-arena-pressure.sh            this tree's proxy, starved arena
#   scripts/wc-arena-pressure.sh <proxy>    somebody else's proxy binary
#   scripts/wc-arena-pressure.sh both       this tree's and master's, compared
#
# # Why this exists, and why it is not `wc-firefox.sh` with a smaller number
#
# docs/waycompose-design.md §20.2 is a correction to §17.8. §17.8 said that an arena with
# no room for a pool would leave "the client's window stops updating" — a
# compositor that looks frozen. It does not. The pool goes unannounced, the
# client's next `wl_shm_pool.create_buffer` is forwarded to a compositor that
# has never heard of that object, and a request on an unknown object is a
# protocol error: the connection is dropped and the client dies. That is what
# happened to a user watching a YouTube video, and the log said so in three
# lines that nobody had read together.
#
# A test can assert that no message naming an unbacked pool is emitted — and
# `session::tests` does. What a test cannot do is show a *real* Firefox living
# through it, because the thing that kills Firefox is a real compositor's real
# protocol error. So this starves the arena on purpose and watches for the
# three strings that were in the original report.
#
# The arena is 24 MiB (`WC_ARENA_MIB` overrides) against a Firefox on a
# 1600x1000 desktop, whose largest single pool is 18,837,504 bytes and whose
# total demand across four connections is over 30 MB. **The size is chosen so
# that every individual pool fits and the set of them does not**, which is the
# case the user actually hit: a video whose buffers are each perfectly
# reasonable and which collectively exhaust the region.
#
# Starving it further (`WC_ARENA_MIB=12`) is a different and much harsher test:
# there the largest pool is bigger than the whole arena, so the client's main
# surface can never be given memory at all and no policy can save it. That case
# is worth running and is not worth asserting on -- see §20.7.
# STAGE 44. `pipefail` added: these were the only two harnesses without it, and
# uniformity is the point rather than a live bug — neither currently has a
# pipeline whose head is the thing under test, so this closes a latent hazard
# rather than an open one. Without it a failing `cargo`/`ssh` at the head of a
# pipeline is invisible; with it the status is at least *available*, which is
# what `built_ok` and `${PIPESTATUS[0]}` then make usable.
set -uo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
LINUX_BIN="$here/target/debug"
WIN_BIN="$here/target/x86_64-pc-windows-gnu/debug"
FIREFOX=${FIREFOX:-$(command -v firefox)}
export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"
OUTPUT=${OUTPUT:-1600x1000}
pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass+1)); }
bad() { echo "  FAIL  $*"; fail=$((fail+1)); }
note(){ echo "  ....  $*" >&2; }

# STAGE 41. No QEMU on this box, so no guest RAM to locate: this harness is the
# Wine fixture. `wc_substrate_declare wine-fixture` is what hands it the
# `SV_ALLOW_FILE_ARENA=1` that `wc-proxy` and `wc-compositor` now require, and
# it refuses to hand that out to any harness that has a guest.
. "$(dirname "$0")/wc-substrate.sh"

# §51. Whether a hypervisor is in the path, declared rather than left unknown.
# This harness is same-box, so `wc_latency_claim` will refuse any assertion here
# whose truth depends on cross-VM timing -- which is the point: §27.6 drew one
# from a harness exactly like this one.
. "$(dirname "$0")/wc-topology.sh"
wc_topology_declare same-box \
  "the proxy and its clients run on this box; the arena is a file here" || exit 1
# STAGE 42. `fresh`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"
wc_substrate_declare wine-fixture \
  "no QEMU on this box: a Wine run has no guest RAM to locate" || exit 2

winp() { printf 'Z:%s\n' "$(echo "$1" | tr '/' '\\')"; }

# One run: a Windows compositor, the named proxy, a starved arena, and Firefox.
# Echoes a one-line verdict and leaves its logs behind.
run_one() {
  local proxy_bin=$1 label=$2 dir=$3
  local sock="wl-press-$$-$label" desk="press$$$label"
  local arena_size=${WC_ARENA_MIB:-24}; arena_size=$((arena_size * 1024 * 1024)); local arena_out=$((1024 * 1024))
  mkdir -p "$dir/rt" "$dir/frames" "$dir/profile"
  chmod 700 "$dir/rt"
  : >"$dir/arena"; truncate -s $arena_size "$dir/arena"

  wine explorer "/desktop=$desk,1600x1050" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 90 --output "$OUTPUT" \
    --dump-frames "$(winp "$dir/frames")" \
    --arena-provide "0:$arena_size:$(winp "$dir/arena")" \
    --arena-outbound "0:$arena_out" \
    --arena-grow-dir "$(winp "$dir")" \
    --log "$(winp "$dir/srv.log")" >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node=""
  for _ in $(seq 1 300); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$sock" 2>/dev/null | head -1)
    [ -n "$node" ] && break
    sleep 0.2
  done
  if [ -z "$node" ]; then
    echo "NOSOCKET"; kill "$srv" 2>/dev/null; return
  fi

  XDG_RUNTIME_DIR="$dir/rt" "$proxy_bin" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --copy pool --transport arena --arena-path "$dir/arena" --arena-id 0 \
    --arena-size $arena_size --arena-outbound $arena_out ${WC_PROXY_EXTRA:-} \
    --accept-timeout 40 --timeout 60 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 1

  env -u DISPLAY MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
    XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout 45 "$FIREFOX" --no-remote --profile "$dir/profile" about:blank \
    >"$dir/fx.log" 2>&1 &
  local cli=$!
  sleep 35

  kill "$cli" 2>/dev/null; wait "$cli" 2>/dev/null
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  echo "RAN"
}

# The three strings from the original report, plus what the run achieved.
verdict() {
  local dir=$1 label=$2
  local closed died refusals frames unknown reclaimed peak floor
  # `grep -c` prints 0 *and* exits 1 when it matches nothing, so the obvious
  # `|| echo 0` appends a second line and every arithmetic test then says
  # "integer expected". Count lines instead and let a missing file be zero.
  # Everything except the row itself goes to stderr, or `$(verdict ...)` eats it.
  count() { grep -aE "$1" "$2" 2>/dev/null | wc -l | tr -d ' '; }
  closed=$(count 'the compositor closed the connection' "$dir/proxy.log")
  died=$(count 'channel error|Compositor .* crashed' "$dir/fx.log")
  refusals=$(count 'could not publish' "$dir/proxy.log")
  # The compositor's own words for "you sent a request about an object I do not
  # have", which is §20.2's failure and the one thing this run can decide.
  unknown=$(count 'invalid object|Invalid object|no known interface' "$dir/srv.log")
  reclaimed=$(grep -aoE '[0-9]+ reclaimed' "$dir/proxy.log" 2>/dev/null \
    | grep -oE '^[0-9]+' | awk '{s+=$1} END {print s+0}')
  frames=$(ls "$dir/frames" 2>/dev/null | wc -l)
  # DYNAMIC ARENA. Peak resident arena bytes, which is the memory question and
  # is a different number from every MiB figure on the stats line -- those are
  # bytes copied, cumulative over every frame. A run that copied 1158 MiB held
  # nothing like it. Printing throughput without this invites exactly that
  # misreading, so both go in the report or neither does.
  peak=$(grep -aoE "peak resident [0-9.]+ MiB in [0-9]+ region\\(s\\)" "$dir/proxy.log" 2>/dev/null | tail -1)
  floor=$(grep -aoE "floor [0-9.]+ MiB" "$dir/proxy.log" 2>/dev/null | tail -1)
  [ -n "$peak" ] || peak="peak resident unknown (no arena line)"
  {
    note "$label: $refusals refusal(s), $reclaimed reclaimed, $frames frame(s), $unknown unknown-object error(s)"
    note "$label: $peak; $floor -- memory held, NOT bytes copied"
    grep -aoE 'message: "[^"]*"' "$dir/srv.log" 2>/dev/null | sort -u | head -3 \
      | sed 's/^/          /'
    grep -aE 'reclaiming|could not publish' "$dir/proxy.log" 2>/dev/null | tail -2 \
      | sed 's/^/          /'
    grep -aE '[0-9]+ commits,' "$dir/proxy.log" 2>/dev/null | tail -1 | sed 's/^/          /'
    grep -aE 'arena [0-9]+ (memory|churn):' "$dir/proxy.log" 2>/dev/null | sed 's/^/          /'
  } >&2
  echo "$label|$closed|$died|$refusals|$frames|$reclaimed|$unknown"
}
main() {
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build -p wc-compositor --target x86_64-pc-windows-gnu" || exit 2
  [ -n "$FIREFOX" ] || { echo "no firefox"; exit 2; }
  local root; root=$(mktemp -d /tmp/wc-press-XXXXXX)
  local which=${1:-this}

  echo "== a ${WC_ARENA_MIB:-24} MiB arena against a Firefox that wants over 30 MB of pools"
  local rows=()
  if [ "$which" = both ]; then
    local master=${WC_MASTER_PROXY:-/tmp/wc-master/target/debug/wc-proxy}
    if [ -x "$master" ]; then
      run_one "$master" master "$root/master" >/dev/null
      rows+=("$(verdict "$root/master" master | head -1)")
    else
      note "no master proxy at $master; skipping the before half"
    fi
    run_one "$LINUX_BIN/wc-proxy" this "$root/this" >/dev/null
    rows+=("$(verdict "$root/this" this | head -1)")
  else
    local bin="$LINUX_BIN/wc-proxy"
    [ "$which" != this ] && bin=$which
    run_one "$bin" this "$root/this" >/dev/null
    rows+=("$(verdict "$root/this" this | head -1)")
  fi

  for row in "${rows[@]}"; do
    IFS='|' read -r label closed died refusals frames reclaimed survived <<<"$row"
    # 1. The run has to be a real test of the response, or nothing below means
    #    anything.
    [ "$refusals" -gt 0 ] \
      && ok "$label: the arena really did run out ($refusals refusal(s)) -- the run is a test of the response" \
      || bad "$label: nothing was refused, so this run measures nothing; lower WC_ARENA_MIB"
    # 2. The policy this stage is about ran in anger: guest memory came back
    #    from a client that never asked to give it up. This is §20.5, and it is
    #    the assertion the stage exists for.
    [ "$reclaimed" -gt 0 ] \
      && ok "$label: $reclaimed pool(s) reclaimed without the client asking -- §20.5's policy ran" \
      || bad "$label: nothing was reclaimed under real pressure"
    # 3. The specific desync of §20.2 is gone: an unbacked pool's requests are
    #    absorbed, so the compositor never reports an unknown object. The unit
    #    tests assert the messages directly; this asserts the compositor's own
    #    verdict on them.
    [ "$survived" -eq 0 ] \
      && ok "$label: the compositor reported no request on an object it does not have" \
      || bad "$label: $survived unknown-object error(s) -- a message about an unbacked pool crossed"
    # 4. What is NOT asserted, and §20.7 says why: whether the connection
    #    survives to the end, and whether a window is ever painted. Under Wine
    #    in a bare ssh session this compositor cannot create a window at all
    #    ("Invalid window handle", os error 1400), so a run here composites
    #    nothing whatever the arena does, and a pass/fail on it would be
    #    measuring the window station rather than the allocator.
    note "$label: connection dropped: $closed; client reported a protocol error: $died; frames: $frames"
  done

  echo
  echo "== $pass passed, $fail failed =="
  [ "${WC_KEEP:-0}" = 1 ] && note "logs in $root" || rm -rf "$root"
  [ "$fail" -eq 0 ]
}

main "$@"
