#!/usr/bin/env bash
# **The Vulkan Conformance Test Suite, forwarded — and the same suite not
# forwarded, which is the only thing that makes the first number mean anything.**
#
#   scripts/sv-cts.sh [options] [icd-manifest-basename]   default: radeon_icd.x86_64.json
#
#     --fraction N     run 1 test in every N of the suite (default 2000)
#     --jobs J         concurrent deqp-vk processes, each its own session (default 8)
#     --include RE     restrict to tests whose name matches this ERE, repeatable
#     --arena BYTES    per-session arena (default 256 MiB)
#     --timeout SEC    hard ceiling on one deqp-vk invocation (default 3600)
#     --stall SEC      kill an invocation that has printed nothing for this long (default 120)
#     --chunk N        tests per deqp-vk invocation (default 200)
#     --keep           do not delete the run directory
#
# Every other harness in this tree judges one workload. This one judges the
# *API*: "a lot of functions are still missing, they must all be impl" is a
# claim about surface area, and surface area is exactly what a workload cannot
# measure — `sv-zink-gl.sh` renders a correct frame through 239 forwarded
# commands without saying a word about the other 501.
#
# ## Five checks, in this order: the control, concurrency, the sweep, the join,
# ## and the gap. That order is part of the experiment — see check 2.
#
# ## Four verdicts, and only three of them are about us
#
# deqp reports Pass, Fail, NotSupported and a handful of others; this script
# adds Crash, which is what a chunk that died mid-test looks like from outside.
# NotSupported is the interesting one and it is **not a failure**: a test that
# asks for `VK_KHR_ray_tracing_pipeline` and is told the device does not have it
# has been answered correctly. What the skip count measures is the *profile*,
# which is design.md §11's declared scope — check 5 turns that into data.
#
# ## The control is not optional
#
# design.md §0.13 retracted a headline that was measuring Xvfb, and §0.6 item 7
# spent an afternoon on a triangle that was back-face culled on every
# implementation including the unforwarded one. So this runs the *same tests, in
# the same order, on the same driver, in the same shell*, once through the
# forwarder and once directly, and reports the join. A test that fails both ways
# is RADV's business or the suite's; a test that passes directly and fails
# forwarded is ours, and that list — not the pass rate — is the output with any
# information in it.
#
# ## Three things about deqp that cost time to find
#
#  - **The mustpass lists beside the binary do not match the binary.** The
#    `vk-default` lists in the `vulkan-cts` derivation name tests it does not
#    have — 362 of a 1451-test sample, all format combinations like
#    `astc_10x10_srgb_block.b8g8r8_srgb.linear_optimal_nearest` where the binary
#    generates only the `optimal_optimal_*` variants. The caselist here comes
#    from `--deqp-runmode=stdout-caselist`, the binary describing itself, and is
#    cached under the store hash it was dumped from so a flake bump invalidates
#    it by name rather than by anyone remembering to.
#  - **`deqp-runner` was tried and removed.** See the note in `flake.nix`: 0.18.0
#    does not know deqp's `DeviceLost` status and discards a whole group when it
#    sees one, which on this machine reported *zero passes* for unforwarded
#    RADV. The chunking below is the part of it this needs, and it treats a
#    status it has never seen as data rather than as an error.
#  - **A chunk that dies takes one test with it and no more.** deqp-vk announces
#    `Test case 'X'..` before running X, so the test with no verdict after the
#    process ends is the one that killed it. It is recorded as `Crash` and the
#    chunk is re-run without it — which is the whole of crash isolation, and it
#    matters because a forwarder's characteristic failure *is* the segfault.
#
# ## What `--jobs J` costs on the GPU side
#
# The ICD connects once per *process* (`sv_icd::instance`, a `OnceLock`), so a
# chunk is a session and a long run opens hundreds of them one after another,
# up to J at a time. Each has its own arena (design.md §0.12 item 3), which is
# why the default here is 256 MiB and not the 1 GiB the other harnesses use.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d

FRACTION=2000
JOBS=8
ARENA=$((256 << 20))
TIMEOUT=3600
STALL=120
CHUNK=200
KEEP=0
INCLUDE=""
ICD=radeon_icd.x86_64.json

while [ $# -gt 0 ]; do
  case "$1" in
    --fraction) FRACTION="$2"; shift 2 ;;
    --jobs)     JOBS="$2";     shift 2 ;;
    --include)  INCLUDE="${INCLUDE:+$INCLUDE|}$2"; shift 2 ;;
    --arena)    ARENA="$2";    shift 2 ;;
    --timeout)  TIMEOUT="$2";  shift 2 ;;
    --stall)    STALL="$2";    shift 2 ;;
    --chunk)    CHUNK="$2";    shift 2 ;;
    --keep)     KEEP=1;        shift ;;
    -h|--help)  sed -n '2,15p' "$0"; exit 0 ;;
    *)          ICD="$1";      shift ;;
  esac
done

command -v deqp-vk >/dev/null || { echo "deqp-vk is missing; enter 'nix develop'" >&2; exit 1; }
MANIFEST="$BIN/sv_icd.json"
[ -f "$MANIFEST" ]          || { echo "build first: cargo build -p sv-icd" >&2; exit 1; }
[ -x "$BIN/sv-gpu-server" ] || { echo "build first: cargo build -p sv-gpu-server" >&2; exit 1; }
[ -f "$ICDDIR/$ICD" ]       || { echo "no such ICD manifest: $ICDDIR/$ICD" >&2; exit 1; }

RUNDIR="$(mktemp -d /tmp/sv-cts.XXXXXX)"
fail=0
GPUPID=""
# **Every GPU side this run started, killed, including the ones in subshells.**
#
# This is not tidiness. A `sv-gpu-server` that outlives its deqp-vk keeps the
# driver's context alive with work queued on it, and an interrupted run left
# four of them behind: `amdgpu: ring gfx_0.0.0 timeout, signaled seq=875426,
# emitted seq=875428` on every *subsequent* invocation, including plain
# unforwarded `dEQP-VK.api.smoke.*`, which went from 6/6 to 2/4 and aborted.
# The machine looked as though CTS had broken RADV; killing the four leaked
# processes restored it to 6/6 immediately. So the pid file is the difference
# between a run that ends and a machine that has to be noticed.
cleanup() {
  [ -n "$GPUPID" ] && kill "$GPUPID" 2>/dev/null
  [ -s "$RUNDIR/gpupids" ] && xargs -r kill 2>/dev/null < "$RUNDIR/gpupids"
  [ "$KEEP" -eq 1 ] && { echo "run directory kept: $RUNDIR"; return; }
  rm -rf "$RUNDIR"
}
trap cleanup EXIT

say() { printf '\n=== %s ===\n' "$*"; }
ok()  { printf '  ok   %s\n' "$*"; }
bad() { printf '  FAIL %s\n' "$*"; fail=1; }

# ---------------------------------------------------------------------------
say "0. the suite, the driver, and the caselist"
# ---------------------------------------------------------------------------
# The version is part of every number below, so it is printed rather than
# assumed: a conformance figure without the suite version attached is not one.
CTS_STORE="$(dirname "$(dirname "$(readlink -f "$(command -v deqp-vk)")")")"
CTS_VERSION="$(basename "$CTS_STORE" | sed 's/^[a-z0-9]*-//')"
echo "  suite        $CTS_VERSION"
echo "  GPU-side ICD $ICD"
echo "  sampling     1 in $FRACTION,  jobs $JOBS,  chunk $CHUNK,  arena $((ARENA >> 20)) MiB/session"

CASELIST="${SV_CTS_CASELIST:-/tmp/sv-cts-caselist-$(basename "$CTS_STORE").txt}"
if [ ! -s "$CASELIST" ]; then
  echo "  dumping the binary's own test hierarchy (about a minute)…"
  deqp-vk --deqp-runmode=stdout-caselist 2>/dev/null \
    | sed -n 's/^TEST: //p' > "$CASELIST.tmp" || true
  [ -s "$CASELIST.tmp" ] || { bad "deqp-vk would not enumerate its own tests"; exit 1; }
  mv "$CASELIST.tmp" "$CASELIST"
fi
TOTAL=$(wc -l < "$CASELIST")
echo "  caselist     $TOTAL tests"
[ "$TOTAL" -gt 100000 ] || bad "the caselist is implausibly short — deqp-vk enumerated $TOTAL"

# The sample. Deterministic — every Nth line of a list the binary produced in
# its own order — so two runs of this script select the same tests and the
# control and the forwarded pass select the *same* tests as each other, which is
# the whole point of having a control.
SAMPLE="$RUNDIR/sample.txt"
if [ -n "$INCLUDE" ]; then
  grep -E "$INCLUDE" "$CASELIST" | awk -v n="$FRACTION" 'NR % n == 1' > "$SAMPLE"
else
  awk -v n="$FRACTION" 'NR % n == 1' "$CASELIST" > "$SAMPLE"
fi
NSAMPLE=$(wc -l < "$SAMPLE")
echo "  selected     $NSAMPLE tests"
[ "$NSAMPLE" -gt 0 ] || { bad "the selection is empty"; exit 1; }

# `stdout → NAME,VERDICT,REASON`. deqp names a test before running it and prints
# the verdict as the first word of the next indented line, so the test left
# without one when the stream ends is the test that ended it.
#
# The reason is the parenthesised text, and it is the most useful field in the
# whole run: `NotSupported (VK_EXT_host_image_copy is not supported)` is CTS
# naming, in its own words, one entry of the gap check 5 lists from the
# registry's side. The source location is stripped so that two tests refusing
# for the same reason aggregate, and commas become semicolons because this is a
# CSV and the reason is prose.
PARSE='
/^Test case '"'"'/ { cur = $0
  sub(/^Test case '"'"'/, "", cur); sub(/'"'"'\.\.$/, "", cur); next }
cur != "" && /^  [A-Za-z]+ \(/ {
  r = ""
  if (match($0, /\(.*\)/)) r = substr($0, RSTART + 1, RLENGTH - 2)
  sub(/ at [^ ]*:[0-9]+$/, "", r)
  gsub(/,/, ";", r)
  print cur "," $1 "," r; cur = ""; next
}
END { if (cur != "") print cur "," crash "," }
'

# `start_gpu_side <socket> <log>` — one GPU side, waited for.
start_gpu_side() {
  local sock="$1" log="$2"
  VK_DRIVER_FILES="$ICDDIR/$ICD" "$BIN/sv-gpu-server" serve \
    --socket "$sock" --arena "$ARENA" --clients 1000000 > "$log" 2>&1 &
  local pid=$!
  echo "$pid" >> "$RUNDIR/gpupids"
  local i
  for ((i = 0; i < 900; i++)); do
    [ -S "$sock" ] && { echo "$pid"; return 0; }
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
  echo ""
  return 1
}

# `run_chunk <listfile> <resultfile> <logfile> <gpulog> <env…>` — one deqp-vk
# invocation, re-invoked around whatever kills it until the list is exhausted or
# nothing more can be attributed.
#
# # Why the forwarded path gets a **fresh GPU side per round**
#
# Measured, and it is a finding rather than a precaution. CTS deliberately
# provokes device loss, and one forwarded submit was enough to get
# `radv/amdgpu: The CS has been cancelled … this context is guilty of a hard
# recovery` out of RADV. A GPU side opens **one** `VkDevice` at startup and
# every session shares it (design.md §0.12), so after that every later session
# — belonging to processes that had nothing to do with the offending test — got
# `vkQueueSubmit answered VkResult(-4)` and took its ring fatal. The first run
# of this script measured 15 passes against the control's 96 for exactly that
# reason: not 81 forwarding bugs, one, contaminating everything after it.
#
# The control has no such property, because each deqp-vk process there creates
# its own `VkDevice` and a lost device dies with it. So a shared GPU side is not
# the same experiment, and making the two comparable means restarting ours where
# deqp restarts its device. Check 5 is where several sessions share one on
# purpose.
run_chunk() {
  local list="$1" results="$2" log="$3" gpulog="$4"; shift 4
  local todo="$list.todo"
  cp "$list" "$todo"
  local round=0
  # One culprit is retired per round in the worst case, so the bound is the
  # chunk size and not a number somebody liked the look of.
  local maxr=$((CHUNK + 2))
  while [ -s "$todo" ] && [ "$round" -lt "$maxr" ]; do
    round=$((round + 1))
    local sock="" pid="" extra=()
    if [ -n "$gpulog" ]; then
      sock="$RUNDIR/chunk-$$-$round.sock"
      pid="$(start_gpu_side "$sock" "$gpulog.$round")"
      if [ -z "$pid" ]; then
        awk '{ print $0 ",NoGpuSide" }' "$todo" >> "$results"
        break
      fi
      extra=("SV_SOCKET=$sock")
    fi
    # # A stall watchdog, not a deadline
    #
    # The first version gave the whole invocation `--timeout` seconds and that
    # is the wrong instrument for the thing it has to catch. A chunk of 200
    # forwarded tests legitimately takes ten minutes, and a *hang* is one test
    # that never returns — so a deadline generous enough for the chunk costs
    # fifteen minutes per hang, and one tight enough to catch the hang kills
    # honest work. Two hangs at fraction 1-in-2000
    # (`dEQP-VK.memory.allocation.random.79` and a `timeline_semaphore.device_host`
    # case) turned a 25-minute run into an open-ended one.
    #
    # What distinguishes the two is *progress*, and deqp announces every test on
    # stdout before running it. So the watchdog is "no new output for `--stall`
    # seconds", which is bounded by the slowest single test rather than by the
    # chunk. `stdbuf -oL` is what makes it work at all: a redirected stdout is
    # block-buffered, so without it the file grows in 4 KiB steps and a stall
    # looks the same as ordinary buffering.
    stdbuf -oL -eL env "$@" "${extra[@]}" deqp-vk \
      --deqp-caselist-file="$todo" \
      --deqp-log-filename=/dev/null \
      --deqp-surface-width=64 --deqp-surface-height=64 \
      --deqp-shadercache=disable \
      > "$log" 2>&1 &
    local dp=$! last=-1 quiet=0 elapsed=0 stalled=0
    while kill -0 "$dp" 2>/dev/null; do
      sleep 5
      elapsed=$((elapsed + 5))
      local now
      now=$(stat -c %s "$log" 2>/dev/null || echo 0)
      if [ "$now" -eq "$last" ]; then quiet=$((quiet + 5)); else quiet=0; last=$now; fi
      if [ "$quiet" -ge "$STALL" ] || [ "$elapsed" -ge "$TIMEOUT" ]; then
        stalled=1
        kill -9 "$dp" 2>/dev/null
        break
      fi
    done
    wait "$dp" 2>/dev/null
    local rc=$?
    [ "$stalled" -eq 1 ] && rc=124
    if [ -n "$pid" ]; then
      kill "$pid" 2>/dev/null
      wait "$pid" 2>/dev/null
      cat "$gpulog.$round" >> "$gpulog"
      rm -f "$gpulog.$round" "$sock"
    fi
    local crash=Crash
    [ "$rc" -eq 124 ] && crash=Timeout
    awk -v crash="$crash" "$PARSE" "$log" >> "$results"
    # `FILENAME`, not `NR==FNR`. The idiomatic two-file awk is wrong whenever the
    # *first* file is empty — `NR==FNR` is then true for every record of the
    # second, so it is all swallowed into the lookup table and nothing is
    # printed. Both places that join two files here can legitimately see an
    # empty first file (a round that produced no verdicts; a run with nothing to
    # isolate), and the symptom is not an error but an empty result: the control
    # reported "produced no results" for a pass that had just recorded 164 of
    # them.
    touch "$results"
    awk -F, -v k="$results" \
      'FILENAME==k { seen[$1]=1; next } !($0 in seen)' "$results" "$todo" > "$todo.next"
    # A round that attributed nothing cannot make progress by retrying the same
    # list. That is a deqp-vk killed before it named a test — out of memory, or
    # a signal — and the file is in the binary's own hierarchy order, so the
    # first entry is the one it was about to run. Charge it and continue.
    if [ "$(wc -l < "$todo.next")" -eq "$(wc -l < "$todo")" ]; then
      head -1 "$todo" | awk -v c="$crash" '{ print $0 "," c "," }' >> "$results"
      tail -n +2 "$todo" > "$todo.next"
    fi
    mv "$todo.next" "$todo"
  done
  # Nothing leaves without a verdict. A test the loop gave up on is `NoResult`,
  # which is a row in the scoreboard rather than a row that quietly is not there
  # — a missing row reads as a smaller denominator and flatters the pass rate.
  [ -s "$todo" ] && awk '{ print $0 ",NoResult," }' "$todo" >> "$results"
  rm -f "$todo" "$todo.next"
}

# `run_suite <tag> <gpu|direct> <env…>` — the whole sample, sharded across
# `--jobs` workers. Round-robin sharding rather than contiguous, so every worker
# gets a mix of cheap and expensive groups and the wall time is not the slowest
# test group.
run_suite() {
  local tag="$1" kind="$2"; shift 2
  local out="$RUNDIR/$tag"
  mkdir -p "$out"
  local j
  local pids=()
  for ((j = 0; j < JOBS; j++)); do
    awk -v j="$j" -v n="$JOBS" 'NR % n == j' "$SAMPLE" > "$out/shard-$j.txt"
  done
  for ((j = 0; j < JOBS; j++)); do
    [ -s "$out/shard-$j.txt" ] || continue
    (
      local gpulog=""
      [ "$kind" = gpu ] && gpulog="$out/gpu-$j.log"
      split -l "$CHUNK" -d -a 4 "$out/shard-$j.txt" "$out/chunk-$j-"
      for c in "$out/chunk-$j-"*; do
        run_chunk "$c" "$out/results-$j.csv" "$c.log" "$gpulog" "$@"
      done
    ) &
    pids+=($!)
  done
  # **Named pids, not a bare `wait`.** The GPU side of check 2 is a background
  # job of this same shell, and a bare `wait` waits for that too — so the
  # forwarded pass finished every test and then hung until the server's idle
  # timeout, which reads exactly like a forwarder that has deadlocked. Half an
  # hour to find, one word to fix.
  wait "${pids[@]}"
  cat "$out"/results-*.csv 2>/dev/null | sort -u -t, -k1,1 > "$out/batch.csv"

  # ## The isolation phase, and why the batch verdict is not the answer
  #
  # deqp holds **one** `VkDevice` for a whole invocation. A test that loses it
  # takes every test after it in that process down as `DeviceLost`, and those
  # are not measurements of anything — the first forwarded run of this script
  # reported twelve, and `dEQP-VK.ubo.3_level_array.std140.mat3x2.…` was one of
  # them, which passes on its own every time.
  #
  # The first attempt at a fix was a heuristic — keep the first `DeviceLost` of
  # a round as the culprit, retry the rest — and it was **wrong**, which the
  # next run said plainly: sixteen `DeviceLost` verdicts across sixteen rounds,
  # the same innocent test among them. The culprit does not have to report
  # `DeviceLost` itself; it can report `Pass` and leave the device wedged for
  # whoever comes next. No ordering heuristic can find it.
  #
  # So every non-benign batch verdict is **re-run alone, one test per process
  # and (forwarded) one GPU side per test**, and the isolated verdict is the one
  # that counts. It is exact, it is bounded by the number of such verdicts, and
  # it costs nothing when there are none — which is the control's case, where it
  # runs anyway so that both columns are produced the same way.
  awk -F, '$2!="Pass" && $2!="NotSupported" && $2!="QualityWarning" { print $1 }' \
    "$out/batch.csv" > "$out/suspect.txt"
  local nsus
  nsus=$(wc -l < "$out/suspect.txt")
  if [ "$nsus" -gt 0 ]; then
    echo "  isolating $nsus non-benign verdict(s), one process each…" >&2
    : > "$out/isolated.csv"
    local pids2=()
    for ((j = 0; j < JOBS; j++)); do
      awk -v j="$j" -v n="$JOBS" 'NR % n == j' "$out/suspect.txt" > "$out/sus-$j.txt"
      [ -s "$out/sus-$j.txt" ] || continue
      (
        local gpulog=""
        [ "$kind" = gpu ] && gpulog="$out/gpu-iso-$j.log"
        local i=0
        while read -r t; do
          i=$((i + 1))
          printf '%s\n' "$t" > "$out/one-$j-$i.txt"
          run_chunk "$out/one-$j-$i.txt" "$out/iso-$j.csv" "$out/one-$j-$i.log" \
            "$gpulog" "$@"
        done < "$out/sus-$j.txt"
      ) &
      pids2+=($!)
    done
    wait "${pids2[@]}"
    cat "$out"/iso-*.csv 2>/dev/null | sort -u -t, -k1,1 > "$out/isolated.csv"
  else
    : > "$out/isolated.csv"
  fi
  # The isolated verdict wins where there is one. Keyed on `FILENAME` for the
  # reason `run_chunk` gives at length: `isolated.csv` is empty in the ordinary
  # case and `NR==FNR` would then discard the entire batch.
  awk -F, -v k="$out/isolated.csv" \
    'FILENAME==k { v[$1]=$0; next } { print ($1 in v) ? v[$1] : $0 }' \
    "$out/isolated.csv" "$out/batch.csv" | sort -u -t, -k1,1 > "$out/results.csv"
  # Kept so the difference between the two is visible rather than asserted.
  join -t, -j1 <(cut -d, -f1,2 "$out/batch.csv") <(cut -d, -f1,2 "$out/isolated.csv") \
    2>/dev/null | awk -F, '$2!=$3' > "$out/collateral.csv"
}

# The scoreboard for one pass. Verdicts are printed as deqp spells them, plus
# the two this script adds; anything unrecognised is printed rather than folded
# into a bucket, because a status nobody has seen is information.
score() {
  awk -F, '
    { c[$2]++; n++ }
    END {
      order = "Pass Fail QualityWarning CompatibilityWarning NotSupported ResourceError InternalError DeviceLost Waiver Crash Timeout NoResult"
      split(order, o, " ")
      for (i in o) known[o[i]] = 1
      for (i = 1; i <= 12; i++) if (c[o[i]]) printf "  %-22s %6d\n", o[i], c[o[i]]
      for (k in c) if (!(k in known)) printf "  %-22s %6d   (unrecognised)\n", k, c[k]
      printf "  %-22s %6d\n", "total", n
    }' "$1"
}

# ---------------------------------------------------------------------------
say "1. the control: the same tests, the same driver, no forwarding"
# ---------------------------------------------------------------------------
# `VK_DRIVER_FILES` names the *real* RADV here and our manifest in check 2, so
# the two passes differ in exactly one value. design.md §0.13 item 3 is the run
# where leaving it unset for one side and set for the other silently changed
# what was being measured.
CTL_START=$SECONDS
run_suite control direct "VK_DRIVER_FILES=$ICDDIR/$ICD"
CTL_SECS=$((SECONDS - CTL_START))
CTL="$RUNDIR/control/results.csv"
if [ -s "$CTL" ]; then
  score "$CTL"
  echo "  wall                   ${CTL_SECS}s"
else
  bad "the control produced no results"
fi

# ---------------------------------------------------------------------------
say "2. several sessions on one GPU side, at once"
# ---------------------------------------------------------------------------
# Check 3 gives every chunk its own GPU side, for the device-loss reason given
# there, and that removes the property design.md §0.12 is about: **one driver,
# one queue, several clients**. This puts it back, on a group the forwarder is
# known to pass, so that a failure here is concurrency and not coverage.
#
# It is also the test that would notice the `vkQueueSubmit` race §0.12 item 4
# records — three ring threads per session times N sessions, all reaching one
# `VkQueue`. That race is closed in the generated dispatch as of this branch;
# this is the instrument that would say if it were not.
#
# # It runs **before** the sweep, and that is not cosmetic
#
# It used to run last, and it reported six `DeviceLost` at `vkWaitForFences`
# across six clients on a run whose sweep had just lost eight devices and put
# RADV through as many ring resets. Repeated on a quiescent GPU, the identical
# six clients gave **36/36 Pass**. So the failure was the *previous check's*
# damage, not concurrency, and a check placed after a phase that resets the GPU
# is measuring the phase. Ordering is part of the experiment.
#
# The general form of this is the one design.md §0 keeps restating: a true
# observation attached to the wrong cause. It survived one run in each
# direction — 48/48 the first time, 12/18 the second — which is exactly long
# enough to be believed.
STRESS="dEQP-VK.api.smoke.*"
grep -E '^dEQP-VK\.(api\.smoke|compute\.basic)\.' "$CASELIST" > "$RUNDIR/stress.txt"
NSTRESS=$(wc -l < "$RUNDIR/stress.txt")
echo "  $NSTRESS tests ($STRESS and compute.basic), $JOBS concurrent clients, one GPU side"
SPID="$(start_gpu_side "$RUNDIR/stress.sock" "$RUNDIR/stress-gpu.log")"
if [ -z "$SPID" ]; then
  bad "the GPU side never came up for the concurrency check"
else
  GPUPID="$SPID"
  spids=()
  for ((j = 0; j < JOBS; j++)); do
    ( timeout "$TIMEOUT" env "VK_DRIVER_FILES=$MANIFEST" "SV_SOCKET=$RUNDIR/stress.sock" \
        deqp-vk --deqp-caselist-file="$RUNDIR/stress.txt" --deqp-log-filename=/dev/null \
        --deqp-surface-width=64 --deqp-surface-height=64 --deqp-shadercache=disable \
        > "$RUNDIR/stress-$j.log" 2>&1
      awk "$PARSE" "$RUNDIR/stress-$j.log" > "$RUNDIR/stress-$j.csv" ) &
    spids+=($!)
  done
  wait "${spids[@]}"
  kill "$SPID" 2>/dev/null; wait "$SPID" 2>/dev/null; GPUPID=""
  cat "$RUNDIR"/stress-*.csv > "$RUNDIR/stress.csv" 2>/dev/null
  score "$RUNDIR/stress.csv"
  BAD=$(awk -F, '$2!="Pass" && $2!="NotSupported"' "$RUNDIR/stress.csv" | wc -l)
  SSESS=$(grep -c 'attached (control region' "$RUNDIR/stress-gpu.log")
  if [ "$BAD" -eq 0 ] && [ "$SSESS" -ge "$JOBS" ]; then
    ok "$SSESS concurrent sessions on one device, one queue, nothing lost"
  else
    bad "$BAD non-pass verdict(s) across $SSESS concurrent session(s)"
    awk -F, '$2!="Pass" && $2!="NotSupported"' "$RUNDIR/stress.csv" | head -10 | sed 's/^/    /'
    grep FATAL "$RUNDIR/stress-gpu.log" | head -5 | sed 's/^/    /'
  fi
fi
# ---------------------------------------------------------------------------
say "3. the forwarded run: the same tests through sv-icd and sv-gpu-server"
# ---------------------------------------------------------------------------
FWD_START=$SECONDS
run_suite forwarded gpu "VK_DRIVER_FILES=$MANIFEST"
FWD_SECS=$((SECONDS - FWD_START))
FWD="$RUNDIR/forwarded/results.csv"
if [ -s "$FWD" ]; then
  score "$FWD"
  echo "  wall                   ${FWD_SECS}s"
  [ "$CTL_SECS" -gt 0 ] && awk -v f="$FWD_SECS" -v c="$CTL_SECS" \
    'BEGIN { printf "  %-22s %.2fx the control (isolation included)\n", "cost", f/c }'
  COLL=$(wc -l < "$RUNDIR/forwarded/collateral.csv" 2>/dev/null || echo 0)
  [ "$COLL" -gt 0 ] && echo \
    "  $COLL verdict(s) changed when the test was re-run alone — batch collateral, not results"
else
  bad "the forwarded run produced no results"
fi

# **Which implementation actually ran.** Without this the whole script can pass
# while measuring RADV twice — the failure mode design.md §0.13 item 3 describes
# from the other direction. Both ends have to say so: the ICD prints that it
# connected, and the GPU side counts the sessions it served.
cat "$RUNDIR/forwarded"/gpu-*.log > "$RUNDIR/gpu.log" 2>/dev/null
SESSIONS=$(grep -c 'attached (control region' "$RUNDIR/gpu.log")
if [ "$SESSIONS" -gt 0 ]; then
  ok "the GPU side served $SESSIONS session(s) — the forwarded run went through the forwarder"
else
  bad "the GPU side served no sessions; the forwarded run did not use this ICD"
fi

# What the GPU side itself said. A ring that goes fatal is a forwarding defect
# by definition — it is this project's code declining to continue — so the
# distinct shapes are counted and printed rather than left in a log nobody
# opens. The ids are stripped so that a hundred instances of one bug read as
# one line.
if grep -q FATAL "$RUNDIR/gpu.log"; then
  echo "  the GPU side took a ring fatal, by shape:"
  grep FATAL "$RUNDIR/gpu.log" \
    | sed 's/^\[gpu\] //; s/ring [0-9]*/ring N/; s/seqno [0-9]*/seqno N/; s/id [0-9]*/id X/' \
    | sort | uniq -c | sort -rn | head -12 | sed 's/^/    /'
fi
LOST=$(grep -c 'hard recovery' "$RUNDIR/gpu.log")
[ "$LOST" -gt 0 ] && echo "  RADV reported $LOST hard GPU recovery/recoveries under the forwarded run"

# ---------------------------------------------------------------------------
say "4. the join: which tests the forwarder alone loses"
# ---------------------------------------------------------------------------
# The only column with information in it. A test failing on both sides is not
# ours, and a pass rate that does not separate the two says nothing.
if [ -s "$CTL" ] && [ -s "$FWD" ]; then
  # Two fields for the join, the third kept for the reason breakdown below.
  join -t, -j1 <(cut -d, -f1,2 "$CTL") <(cut -d, -f1,2 "$FWD") > "$RUNDIR/joined.csv"
  printf '  %-34s %6d\n' "compared"                     "$(wc -l < "$RUNDIR/joined.csv")"
  printf '  %-34s %6d\n' "same verdict both ways"       \
    "$(awk -F, '$2==$3' "$RUNDIR/joined.csv" | wc -l)"
  awk -F, '$2=="Pass" && $3!="Pass" { print $3"  "$1 }' "$RUNDIR/joined.csv" \
    | sort > "$RUNDIR/ours.txt"
  awk -F, '$2!="Pass" && $3=="Pass" { print $2"  "$1 }' "$RUNDIR/joined.csv" \
    | sort > "$RUNDIR/theirs.txt"
  REG=$(wc -l < "$RUNDIR/ours.txt")
  printf '  %-34s %6d\n' "pass on RADV, not forwarded"  "$REG"
  printf '  %-34s %6d\n' "pass forwarded, not on RADV"  "$(wc -l < "$RUNDIR/theirs.txt")"

  if [ "$REG" -gt 0 ]; then
    echo "  what the forwarder loses:"
    head -40 "$RUNDIR/ours.txt" | sed 's/^/    /'
    [ "$REG" -gt 40 ] && echo "    … and $((REG - 40)) more"
    echo "  by test group:"
    awk -F, '$2=="Pass" && $3!="Pass" { split($1, a, "."); print a[2]"."a[3] }' \
      "$RUNDIR/joined.csv" | sort | uniq -c | sort -rn | head -15 | sed 's/^/    /'
    cp "$RUNDIR/ours.txt" "${SV_CTS_REGRESSIONS:-/tmp/sv-cts-regressions.txt}"
    echo "  written to ${SV_CTS_REGRESSIONS:-/tmp/sv-cts-regressions.txt}"

    # **The gap, in CTS's own words.** A test that RADV passes and the forwarded
    # device declines is the profile refusing something real, and deqp says why
    # in the parentheses it prints. This is the other half of check 5: that one
    # lists what the profile does not carry, from the registry; this lists what
    # was actually asked for, from the suite. The intersection is the work list.
    awk -F, -v k="$CTL" 'FILENAME==k { if ($2=="Pass") ctl[$1]=1; next }
             ($1 in ctl) && $2=="NotSupported" && $3 != "" { print $3 }' \
      "$CTL" "$FWD" | sort | uniq -c | sort -rn > "$RUNDIR/why.txt"
    if [ -s "$RUNDIR/why.txt" ]; then
      echo "  why the forwarded device declined them, in deqp's words:"
      head -20 "$RUNDIR/why.txt" | sed 's/^/    /'
    fi
  else
    ok "the forwarder loses nothing the control passes"
  fi
else
  bad "cannot join: one of the two runs is missing"
fi


# ---------------------------------------------------------------------------
say "5. the gap, as data: what the profile does not carry"
# ---------------------------------------------------------------------------
# design.md §11 declares the scope and `vk_registry::profiles::zink` is that
# declaration, but until this there was no way to ask "which commands are out?"
# other than to subtract one list from another in your head. A skip in check 1
# is what a missing command looks like from the suite's side, so the two belong
# next to each other.
if [ -x "$BIN/sv-codegen" ]; then
  "$BIN/sv-codegen" gap 2>/dev/null | head -8 | sed 's/^/  /'
  echo "  (full list: target/debug/sv-codegen gap)"
else
  echo "  (sv-codegen not built; run: cargo build -p sv-codegen)"
fi

echo
if [ "$fail" -eq 0 ]; then echo "sv-cts: PASS"; else echo "sv-cts: FAIL"; fi
exit "$fail"
