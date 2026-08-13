# wc-substrate.sh -- sourced, never run. The one place that decides, and then
# says out loud, which memory a run's pixels actually crossed.
#
#   . "$(dirname "$0")/wc-substrate.sh"
#
# WHY THIS FILE EXISTS
#
# The arena is **guest RAM**. That is the deployment, it is what §12 and §18
# built, and it has been restated to this project three times. The file-backed
# arena -- `--arena <id>:<size>:<path>` on the compositor and `--arena-path` on
# the proxy -- is a **Wine-only fixture**: a Wine run on one Linux box has no
# QEMU, therefore no guest RAM to locate, and therefore no way to exercise the
# arena code path at all without a file to stand in for the region. That is the
# whole of its licence.
#
# It is not a fallback. §10.3 measured the export incoherent for two live
# mappings -- 65280 of 65536 bytes wrong, in both directions -- and the one time
# a harness degraded to it silently, tearing reached the user's screen with
# every check still green. So the property this file enforces is narrow and
# absolute:
#
#   **A run that can see the guest cannot end up on a file.**
#
# Three mechanisms, because one of them alone has failed before:
#
#   1. `wc_substrate_fixture_guard` refuses the file substrate by name, at the
#      top of the run, unless the run demonstrably has no guest to measure.
#   2. `wc_locate_arena` has no fallback: if the compositor never reports a
#      guest-RAM arena, or `wc-arena-host` cannot find it, the run stops. It
#      does not truncate a file on the export and carry on.
#   3. `wc_assert_substrate` makes the harness **assert** on the compositor's
#      own `mapped:` counters rather than print them. §35's lesson, stated as
#      code: a number a harness prints without asserting on is a number nobody
#      is watching.
#
# WHAT THE CALLER MUST ALREADY HAVE
#
#   ok/bad/note        the three reporters. Fallbacks are defined below so that
#                      sourcing this file cannot itself break a harness.
#   g/h                ssh to the guest and to the VM host. Only the locate
#                      needs them.
#   HOST HOST_DIR QMP VM_HINT ROOTC       the locate's four settings, spelled
#                      as `wc-guest.sh` and `wc-desktop.sh` spell them.

# The three reporters, if the caller has not defined them. Deliberately not
# overriding: every harness counts its own passes and failures.
type ok   >/dev/null 2>&1 || ok()   { echo "  PASS  $*"; }
type bad  >/dev/null 2>&1 || bad()  { echo "  FAIL  $*"; }
type note >/dev/null 2>&1 || note() { echo "  ----  $*"; }

# ---------------------------------------------------------------- settings ---
# One spelling of the guest-RAM arena's two numbers, because `wc-guest.sh`,
# `wc-desktop.sh`, `wc-icon.sh` and `wc-taskbar.sh` all need them and a second
# spelling is a second thing to get wrong.
WC_ARENA_PAGES="${WC_ARENA_PAGES:-16384}"            # 64 MiB, in 4096-byte pages
WC_ARENA_BYTES=$((WC_ARENA_PAGES * 4096))
WC_ARENA_OUTBOUND="${WC_ARENA_OUTBOUND:-1048576}"    # 1 MiB, the outbound window

# Where the compositor writes the frame list `wc-arena-host --frames` reads, on
# the guest, if it writes one at all. See `wc_locate_arena`.
WC_ARENA_FRAMES="${WC_ARENA_FRAMES:-}"

# Set by `wc_substrate_declare`; read by `wc_assert_substrate`.
WC_SUBSTRATE=""
WC_SUBSTRATE_LOCATED=""

# ------------------------------------------------------------- the refusal ---
# The file substrate is reachable only from a run that has no guest.
#
# "Has no guest" is not a preference and is not asked politely: it is checked
# against the two facts that would make a guest reachable -- a VM host to run
# the locate on (`WC_HOST`) and a guest to ssh into (`WC_GUEST`, when the
# harness that sourced this file uses one). A harness that sets either is a
# harness that can measure the real thing, and is therefore not allowed to
# measure a file instead.
#
# `WC_ARENA_FIXTURE=wine` is required on top of that. Not because the check
# above is insufficient, but because it makes the fixture something a person
# typed rather than something a default decided, which is the difference
# between the two runs that produced §10.3's numbers and the one that did not.
#
#   wc_substrate_fixture_guard <why-this-harness-wants-a-file>
#
# Returns 0 if the fixture is permitted. Otherwise prints the reason and
# returns 1; every caller treats that as fatal.
wc_substrate_fixture_guard() {
  local why="${1:-}"
  local blocked=""

  if [ -n "${WC_HOST:-}" ]; then
    blocked="WC_HOST=$WC_HOST names a VM host, so this run can reach guest RAM"
  elif [ -n "${WC_GUEST_CONFIGURED:-}" ]; then
    blocked="this harness runs against the guest at ${WC_GUEST:-a guest}"
  fi

  if [ -n "$blocked" ]; then
    bad "REFUSED: the file-backed arena is a Wine-only fixture, and $blocked"
    note "The arena is guest RAM. The file is the substrate §10.3 measured"
    note "incoherent -- 65280 of 65536 bytes wrong, both directions -- and a"
    note "silent degrade to it is how tearing reached a user's screen once."
    note "Unset WC_HOST to run the Wine fixture, or let this run use guest RAM."
    [ -n "$why" ] && note "what asked for it: $why"
    return 1
  fi

  if [ "${WC_ARENA_FIXTURE:-}" != wine ]; then
    bad "REFUSED: the file-backed arena needs WC_ARENA_FIXTURE=wine set explicitly"
    note "There is no guest here to measure, so the fixture is legitimate -- but"
    note "it has to be asked for, so that no run reports a file result by default."
    [ -n "$why" ] && note "what asked for it: $why"
    return 1
  fi

  note "substrate: a file-backed arena, as the Wine fixture (WC_ARENA_FIXTURE=wine)"
  return 0
}

# What a run declares it is about to measure on, before it measures anything.
#
#   wc_substrate_declare guest-ram
#   wc_substrate_declare wine-fixture "the Wine leg has no QEMU"
#
# Returns non-zero if the declaration is refused, and the caller must stop.
wc_substrate_declare() {
  local kind="$1" why="${2:-}"
  case "$kind" in
    guest-ram)
      WC_SUBSTRATE=guest-ram
      WC_SUBSTRATE_LOCATED=""
      return 0 ;;
    wine-fixture)
      wc_substrate_fixture_guard "$why" || return 1
      WC_SUBSTRATE=wine-fixture
      return 0 ;;
    *)
      bad "wc_substrate_declare: unknown substrate $kind"; return 1 ;;
  esac
}

# The compositor's flags for a guest-RAM arena.
#
# There is deliberately no `--shared-dir` here and its absence is the point
# rather than an omission: with a guest-RAM arena the keymap is cut out of the
# compositor's own outbound window (§14), so no `h` argument is published on any
# filesystem and the export is off the pixel path entirely. A harness that
# passed `--shared-dir` alongside this would be back on the export for the one
# object that §11.2 could not move.
wc_arena_comp_args() {
  echo "--arena-guest-ram 0:$WC_ARENA_BYTES --arena-outbound 0:$WC_ARENA_OUTBOUND"
}

# --------------------------------------------------------------- the locate --
# Find the region the compositor pinned inside the VM, and write a plan the
# proxy can open.
#
#   wc_locate_arena <compositor-log-path-on-the-guest> <plan-path-on-WC_HOST> <scratch-dir>
#
# Ordering is the whole of it, and it is §12's: the guest allocates and stamps,
# and only then can the host find the pages. The compositor prints its run id
# *after* the stamps are written, so waiting for that line is waiting for the
# region to be findable -- a sweep started earlier returns zero pages, which
# reads exactly like the arena not being shared at all.
#
# Read over ssh rather than out of the export, which is `wc-desktop.sh`'s choice
# and is deliberate: this is the one control path that decides whether the run
# is on guest RAM, and reading it through the export would make the check depend
# on the thing the check exists to stop using.
#
# Root, and only for this: QEMU is a system service on this host, so its QMP
# socket and its `/proc/<pid>/fd` are root-only (§12.9). No descriptor crosses
# the VM boundary either way; `wc-arena-host` writes a *plan*, which is a list
# of offsets into an object the host opens by name for itself.
#
# `--frames` VERSUS THE SWEEP, WHICH IS WHY THIS FUNCTION HAS TWO ARMS
#
# The sweep reads the *contents* of guest RAM looking for the stamps the
# compositor wrote, 2.308 s over 13 GiB. On a contended guest it fails, and the
# failures are the ones that cost five agents a night: `1 page index claimed
# twice`, `28 page index(es) claimed twice`, `16377 of 16384 pages`. Another
# process's page carried a stamp that looked like ours, or ours moved between
# the stamping and the sweep.
#
# `wc-arena-host --frames` reads the guest's own page-frame database instead --
# 0.064 s, and **no stamps are read at all**, so that entire class of failure
# becomes impossible. Both halves of it exist: `wc_compositor::grow::frames`
# queries `SuperfetchPfnQuery` on the guest and `frame_list_text` writes the
# format, and `sv_hostd::locate::PfnLocator` translates it on the host.
#
# What does not exist, as this was written, is the **call site**: nothing asks
# for the frame list at arena-publish time, so the compositor writes no file for
# this function to find. That wiring belongs to another worktree. So this
# function *probes* for the file rather than passing a flag the compositor would
# reject as unknown, uses `--frames` the moment one appears, and says which arm
# it took either way. When it takes the sweep it says so as a NOTE, not as a
# pass -- the sweep is the slow, fragile arm and a run that used it should look
# like one.
#
# Neither arm is a fallback to a file. Both locate guest RAM; they differ only
# in how the pages are named. A failure of both stops the run.
wc_locate_arena() {
  local guestlog="$1" plan="$2" scratch="$3"
  local line="" runid="" pages="" t

  if [ -z "${HOST:-}" ]; then
    bad "no WC_HOST: the guest-RAM locate needs the machine QEMU runs on, and there is no substitute"
    return 1
  fi

  for t in $(seq 1 40); do
    line=$(g "cat '$guestlog' 2>/dev/null" 2>/dev/null \
           | grep -o "arena 0 provided from guest RAM: run 0x[0-9a-f]*, [0-9]* bytes, [0-9]* pages, locked=[a-z]*" \
           | tail -1)
    [ -n "$line" ] && break
    sleep 2
  done
  if [ -z "$line" ]; then
    bad "the compositor never reported a guest-RAM arena -- there is no fallback, so this run stops"
    g "cat '$guestlog' 2>/dev/null" 2>/dev/null | head -20 | sed 's/^/        /'
    return 1
  fi
  note "$line"
  runid=$(echo "$line" | grep -o "run 0x[0-9a-f]*" | cut -d' ' -f2)
  pages=$(echo "$line" | grep -o "[0-9]* pages" | cut -d' ' -f1)

  # The `--frames` arm, if the guest wrote a frame list. Probed rather than
  # demanded: `WC_ARENA_FRAMES` names it when the caller knows, and otherwise
  # the two spellings next to the compositor's own log are tried, which is where
  # a publish-time writer would naturally put it.
  local framesarg="" gframes="" cand
  for cand in ${WC_ARENA_FRAMES:-} "$guestlog.frames" "${guestlog%.log}.frames"; do
    [ -n "$cand" ] || continue
    if g "test -s '$cand'" >/dev/null 2>&1; then gframes="$cand"; break; fi
  done
  if [ -n "$gframes" ]; then
    g "cat '$gframes'" >"$scratch/arena.frames" 2>/dev/null
    local n; n=$(grep -c '^pfn ' "$scratch/arena.frames" 2>/dev/null || echo 0)
    if [ "${n:-0}" -eq "$pages" ]; then
      scp -q ${SSHOPT[@]+"${SSHOPT[@]}"} "$scratch/arena.frames" "$HOST:$plan.frames" 2>/dev/null \
        && framesarg="--frames '$plan.frames'"
    fi
    if [ -n "$framesarg" ]; then
      ok "the guest named its own $n page frames: the locate reads the PFN database, not the extent's contents"
    else
      note "a frame list was found at $gframes but has ${n:-0} frames against $pages pages; taking the sweep"
    fi
  fi
  if [ -z "$framesarg" ]; then
    note "no frame list from the guest: taking the content sweep (2.3 s, and the arm that"
    note "fails on a contended guest with 'page index claimed twice'). wc-arena-host --frames"
    note "and PfnLocator both exist; the call site at arena-publish time does not yet."
  fi

  h "echo \"export PATH=/run/current-system/sw/bin:/run/wrappers/bin
$HOST_DIR/wc-arena-host --qmp '$QMP' --vm '$VM_HINT' --run $runid --pages $pages \
  --outbound $WC_ARENA_OUTBOUND $framesarg --plan '$plan' >'$plan.log' 2>&1
echo rc=\\\$? >>'$plan.log'
chmod a+r '$plan.log' '$plan' 2>/dev/null; true\" | eval $ROOTC" >/dev/null 2>&1
  h "cat '$plan.log' 2>/dev/null" >"$scratch/locate.log" 2>&1
  sed 's/^/        /' "$scratch/locate.log" | head -12

  if ! grep -q "^rc=0$" "$scratch/locate.log" 2>/dev/null; then
    bad "wc-arena-host did not locate the arena; refusing to fall back to a file on the export"
    return 1
  fi

  # Every page, at its own index. A locate that found most of them is a locate
  # that would put some of these pixels in somebody else's memory and look fine
  # doing it, which is exactly what §35 is about.
  local verify got want
  verify=$(grep -o "verify: [0-9]*/[0-9]* pages of the stitched view carry their own arena index" "$scratch/locate.log")
  if [ -z "$verify" ]; then
    bad "the locator printed no verify line: nothing here proves which pages were found"
    return 1
  fi
  got=${verify#verify: }; got=${got%%/*}
  want=$(echo "$verify" | grep -o "/[0-9]* pages" | tr -dc 0-9)
  if [ "$got" != "$want" ] || [ "${got:-0}" -eq 0 ]; then
    bad "$verify -- the arena is not wholly where the guest said it was"
    return 1
  fi

  ok "the arena is guest RAM: all $got pages located through QEMU's flat view, and there is no file"
  WC_SUBSTRATE_LOCATED=$got
  return 0
}

# --------------------------------------------------------- the assertion -----
# What the run was measured on, said in the run's own output and **checked**.
#
#   wc_assert_substrate <compositor-log-on-this-machine> <tag>
#
# The compositor prints, once per window, which of the three homes a `wl_shm`
# pool actually had:
#
#   mapped: N section, N shared-file, N arena-slice
#
# The counters come from `wc_handle::Handle::map`, the function that opened the
# object, rather than from anything the compositor could infer. Three harnesses
# grepped that line and printed it. Printing it is not watching it: `wc-icon.sh`
# and `wc-taskbar.sh` printed a green run for four stages while every pixel in
# it crossed the fixture.
#
# For `guest-ram` the assertion has two legs and needs both. The `mapped:` line
# proves nothing was opened by *name* -- no section, no file on the export. It
# cannot on its own tell a guest-RAM arena from a file one, because both are
# `arena-slice`. What separates them is that the locate ran and verified every
# page, which is `WC_SUBSTRATE_LOCATED`.
wc_assert_substrate() {
  local log="$1" tag="${2:-}"
  local counts
  counts=$(grep -o 'mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice' "$log" 2>/dev/null | tail -1)

  if [ -z "$counts" ]; then
    bad "${tag:+$tag: }the compositor never said which substrate served the pixels"
    return 1
  fi

  local sections files slices
  sections=$(echo "$counts" | grep -o '[0-9]* section' | tr -dc 0-9)
  files=$(echo "$counts" | grep -o '[0-9]* shared-file' | tr -dc 0-9)
  slices=$(echo "$counts" | grep -o '[0-9]* arena-slice' | tr -dc 0-9)

  case "$WC_SUBSTRATE" in
    guest-ram)
      if [ -z "$WC_SUBSTRATE_LOCATED" ]; then
        bad "${tag:+$tag: }SUBSTRATE UNPROVEN: no locate verified this arena, so '$counts' could be a file"
        return 1
      fi
      if [ "${sections:-0}" -ne 0 ] || [ "${files:-0}" -ne 0 ] || [ "${slices:-0}" -lt 1 ]; then
        bad "${tag:+$tag: }SUBSTRATE WRONG: $counts -- something was opened by name on a guest-RAM run"
        return 1
      fi
      ok "${tag:+$tag: }SUBSTRATE guest-ram: $slices arena slice(s) over $WC_SUBSTRATE_LOCATED located pages, 0 sections, 0 shared files"
      return 0 ;;
    wine-fixture)
      if [ "${sections:-0}" -ne 0 ]; then
        bad "${tag:+$tag: }SUBSTRATE WRONG: $counts -- a named section cannot be opened by a Linux process"
        return 1
      fi
      ok "${tag:+$tag: }SUBSTRATE wine-fixture: $counts -- a file standing in for guest RAM, on a box with no QEMU"
      return 0 ;;
    *)
      bad "${tag:+$tag: }no substrate was declared before this run: $counts"
      return 1 ;;
  esac
}

# The export must not be able to serve pixels behind a harness's back. A
# leftover `arena` from a run of that harness before this change is openable,
# the right size, and completely silent -- so it is removed rather than trusted
# not to be used. §18's shape, which this generalises from `wc-desktop.sh`.
#
#   wc_purge_arena_file <share-dir-on-WC_HOST>
wc_purge_arena_file() {
  local share="$1"
  [ -n "${HOST:-}" ] || return 0
  h "rm -f '$share/arena'; rm -rf '$share/keymaps' '$share/pools'" >/dev/null 2>&1
  true
}
