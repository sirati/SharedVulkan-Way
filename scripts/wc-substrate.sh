# wc-substrate.sh -- sourced, never run. The one place a harness declares which
# memory its pixels will cross, locates that memory, and then **asserts** that
# it was the memory they crossed.
#
#   . "$(dirname "$0")/wc-substrate.sh"
#
# WHY THIS FILE EXISTS
#
# The arena is **guest RAM**. That is the deployment, it is what §12 and §18
# built, and it had been restated to this project three times while five guest
# harnesses could still reach a file. The file-backed arena -- `--arena` and
# `--arena-provide` on the compositor, `--transport arena --arena-path` on the
# proxy -- is a **Wine-only fixture**: a Wine run on one Linux box has no QEMU,
# therefore no guest RAM to locate, and therefore no way to exercise the arena
# code path at all without a file to stand in for the region. That is the whole
# of its licence, and §18.2 wrote it down.
#
# WHAT ENFORCES IT, AND WHAT THIS FILE DELIBERATELY DOES NOT DUPLICATE
#
# **The binaries refuse it.** `wc-proxy`'s `arena` branch and `wc-compositor`'s
# `--arena`/`--arena-provide` both exit with the reason unless
# `SV_ALLOW_FILE_ARENA=1` is set, and a run that sets it prints a warning saying
# the substrate is not a target -- so a fixture can never be mistaken for a
# result in a log somebody reads later. That is the enforcement and there is no
# second copy of it here: a rule implemented twice is a rule that can disagree
# with itself.
#
# What this file adds is the two things a refusal cannot do:
#
#   1. **Hand out the escape hatch, or decline to.** `wc_substrate_declare`
#      exports `SV_ALLOW_FILE_ARENA=1` for a leg that really is a fixture, and
#      refuses to export it for a leg that can see a guest. A guest leg never
#      sets it, so a guest leg cannot start on a file even by accident.
#   2. **Say which substrate a run used, in the run's own output, as an
#      assertion.** `wc_assert_substrate` checks the compositor's own `mapped:`
#      counters instead of printing them. §35's lesson stated as code: a number
#      a harness prints without asserting on is a number nobody is watching.
#      `wc-icon.sh` and `wc-taskbar.sh` printed that line for four stages while
#      every pixel in the run crossed the fixture.
#
# And it holds the guest-RAM locate itself (`wc_locate_arena`), which is the
# positive half -- the thing a harness needs in order to have somewhere to go
# once the file is refused.
#
# WHAT THE CALLER MUST ALREADY HAVE
#
#   ok/bad/note        the three reporters. Fallbacks are defined below so that
#                      sourcing this file cannot itself break a harness.
#   g/h                ssh to the guest and to the VM host. The locate needs
#                      both; nothing else here does.
#   HOST HOST_DIR QMP VM_HINT ROOTC SSHOPT     the locate's settings, spelled as
#                      `wc-guest.sh` and `wc-desktop.sh` spell them, so that one
#                      export in a shell configures every harness.

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

# Set by `wc_substrate_declare`; read by `wc_assert_substrate`.
WC_SUBSTRATE=""
WC_SUBSTRATE_LOCATED=""
WC_SUBSTRATE_LOCATOR=""

# ------------------------------------------------------------ the decision ---
# What a run declares it is about to measure on, before it measures anything.
#
#   wc_substrate_declare guest-ram
#   wc_substrate_declare wine-fixture "no QEMU on this box, so no guest RAM to locate"
#
# Returns non-zero if the declaration is refused, and every caller treats that
# as fatal.
#
# THE FIXTURE CASE, WHICH IS THE ONE WORTH READING
#
# `SV_ALLOW_FILE_ARENA=1` is the binaries' escape hatch and it works: set it and
# `wc-proxy` and `wc-compositor` will open a file, with a warning. So the one
# way a guest run can still end up on the wrong substrate is a harness that
# exports it. This function is where that decision is made once, for all of
# them, and it will not export it for a run that can see a guest.
#
# "Can see a guest" is not asked politely and is not a preference. It is checked
# against the two facts that would make one reachable: a VM host to run the
# locate on (`WC_HOST`), and a guest to ssh into (`WC_GUEST_CONFIGURED`, which a
# harness that ssh's into one sets next to its `WC_GUEST` default). A harness
# with either is a harness that can measure the real thing, and is therefore not
# given the means to measure a file instead.
wc_substrate_declare() {
  local kind="$1" why="${2:-}"
  case "$kind" in
    guest-ram)
      WC_SUBSTRATE=guest-ram
      WC_SUBSTRATE_LOCATED=""
      WC_SUBSTRATE_LOCATOR=""
      # Not merely "not set": actively removed from this run's environment, so
      # that a value inherited from the shell of somebody who ran a fixture an
      # hour ago cannot license a file on a guest run.
      unset SV_ALLOW_FILE_ARENA
      return 0 ;;
    wine-fixture)
      if [ -n "${WC_HOST:-}" ]; then
        bad "REFUSED to declare a fixture: WC_HOST=$WC_HOST names a VM host, so this run can reach guest RAM"
        note "The file arena stands in for guest RAM on a box that has none. This box has one."
        return 1
      fi
      if [ -n "${WC_GUEST_CONFIGURED:-}" ]; then
        bad "REFUSED to declare a fixture: this harness runs against the guest at ${WC_GUEST:-a guest}"
        return 1
      fi
      # The escape hatch, handed out here and nowhere else. The binaries will
      # print their own warning on top of this one; two lines saying the same
      # thing is the correct amount for a substrate §10.3 measured incoherent
      # for live mappings -- 65280 of 65536 bytes wrong, both directions.
      export SV_ALLOW_FILE_ARENA=1
      WC_SUBSTRATE=wine-fixture
      note "substrate: the FILE arena, as a fixture. It is not a target and no result from"
      note "it is a result about the deployment. Reason: ${why:-no reason given}"
      return 0 ;;
    *)
      bad "wc_substrate_declare: unknown substrate $kind"; return 1 ;;
  esac
}

# The compositor's flags for a guest-RAM arena, given where it should publish
# its page frame list on the guest.
#
#   wc_arena_comp_args 'C:\Users\<guest-user>\wcicon\arena-icons.frames'
#
# There is deliberately no `--shared-dir` here and its absence is the point
# rather than an omission: with a guest-RAM arena the keymap is cut out of the
# compositor's own outbound window (§14), so no `h` argument is published on any
# filesystem and the export is off the pixel path entirely. A harness that
# passed `--shared-dir` alongside this would be back on the export for the one
# object §11.2 could not move.
#
# `--arena-frames` is not a file arena and cannot become one: what the file
# carries is *where the pages are*, not the pages. See `wc_locate_arena`.
wc_arena_comp_args() {
  local frames="${1:-}"
  local a="--arena-guest-ram 0:$WC_ARENA_BYTES --arena-outbound 0:$WC_ARENA_OUTBOUND"
  [ -n "$frames" ] && a="$a --arena-frames $frames"
  echo "$a"
}

# --------------------------------------------------------------- the locate --
# Find the region the compositor pinned inside the VM, and write a plan the
# proxy can open.
#
#   wc_locate_arena <compositor-log-on-the-guest> <frame-list-on-the-guest> \
#                   <plan-path-on-WC_HOST> <scratch-dir-here>
#
# Both guest paths are POSIX as the guest's ssh shell spells them; the frame
# list is the same file `--arena-frames` was given in Windows spelling.
#
# ORDERING IS THE WHOLE OF IT, and it is §12's: the guest allocates and stamps,
# and only then can the host find the pages. The compositor prints its run id
# *after* the region is committed, so waiting for that line is waiting for the
# region to be findable -- a locate started earlier finds nothing, which reads
# exactly like the arena not being shared at all.
#
# Read over ssh rather than out of the export, which is `wc-desktop.sh`'s choice
# and is deliberate: this is the one control path that decides whether the run
# is on guest RAM, and reading it through the export would make the check depend
# on the thing the check exists to stop using. It also means a harness needs no
# shared filesystem at all to run on guest RAM, which is the point of §14.
#
# Root, and only for this: QEMU is a system service on this host, so its QMP
# socket and its `/proc/<pid>/fd` are root-only (§12.9). **No descriptor crosses
# the VM boundary in either direction.** What comes back is a *plan*: a list of
# offsets into an object the host opens by name for itself.
#
# TWO LOCATORS, AND WHY THE SLOW ONE IS ONLY A NOTE
#
# The sweep reads the *contents* of guest RAM looking for the stamps the
# compositor wrote -- 2.308 s over 13 GiB -- and on a loaded guest it is a coin
# flip: five consecutive runs gave `16377 of 16384 pages`, `1 page index claimed
# twice`, one success, `28 page index(es) claimed twice`, `1 page index claimed
# twice`. Those duplicates are not stale stamps; the run id hashes the
# millisecond with the pid and a previous run's stamps are counted separately as
# `other_runs`. They are a search reading 13 GiB of contents that are allowed to
# change while it reads them.
#
# `--frames` translates the guest's own page-frame database instead -- 0.064 s,
# and **no stamp is read at all**, so neither a stale stamp nor a duplicate is
# expressible. It is used whenever the compositor managed to publish a list, and
# the run says which locator it got either way.
#
# Conditional rather than required, and that asymmetry is deliberate: a guest
# without `SuperfetchPfnQuery` or without the privilege is a *slow* run, not a
# broken one, and a harness that refused to start would be a worse regression
# than the one being fixed. Neither branch is a fallback to a file -- the arena
# is guest RAM in both, and they differ only in how its pages are named. A
# failure of both stops the run.
wc_locate_arena() {
  local guestlog="$1" guestframes="$2" plan="$3" scratch="$4"
  local line="" runid="" pages="" t

  if [ -z "${HOST:-}" ]; then
    bad "no WC_HOST: the guest-RAM locate needs the machine QEMU runs on, and there is no substitute"
    return 1
  fi

  for t in $(seq 1 40); do
    line=$(g "cat '$guestlog' 2>/dev/null" 2>/dev/null \
           | grep -o "arena 0 provided from guest RAM: run 0x[0-9a-f]*, [0-9]* bytes, [0-9]* pages, locked=[a-z]*.*" \
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
  # The FIRST "N pages": a `frames=none (… 19 of 16384 pages missing …)` tail
  # on the same line is a second match, and two lines in `--pages` broke the
  # root script with "16384: command not found" (§84).
  pages=$(echo "$line" | grep -o "[0-9]* pages" | head -1 | cut -d' ' -f1)

  # The frame list, if the compositor got one. `--arena-frames` puts `frames=`
  # on this very line for exactly this reason: one grep answers both "where is
  # the region" and "did the guest name its own pages", so a harness never has
  # to guess whether a file it is about to look for exists.
  local framesarg="" n=0
  WC_SUBSTRATE_LOCATOR=sweep
  case "$line" in
    *", frames=none"*)
      note "the guest could not name its own frames: $(echo "$line" | grep -o 'frames=none.*')" ;;
    *", frames="*)
      g "cat '$guestframes'" >"$scratch/arena.frames" 2>/dev/null
      # **`grep -c` with `|| echo 0` is a bug, and it was one here.** §42.
      # `grep -c` prints `0` *and* exits 1 when it matches nothing, so the
      # `|| echo 0` this line used to carry appended a *second* line and `n`
      # became the two-character-plus-newline string "0\n0". Every arithmetic
      # test on that then fails with `integer expected` and returns 2 — so the
      # `-ne "$pages"` guard below did not merely mis-answer, it was **skipped**,
      # the `elif` ran, an empty frame list was shipped to `wc-arena-host
      # --frames`, and the run announced "the guest named its own 0 page frames"
      # as a PASS. A coverage guard that cannot fire, in the check whose whole
      # job is to refuse a frame list that does not cover the region.
      #
      # `wc | tr` rather than `grep -c`, which is `wc-arena-pressure.sh`'s idiom
      # and documents this exact trap at its own line 115. `wc -l` on an empty
      # stream is a plain `0` with a zero exit, so there is nothing to `||`.
      n=$(grep -a '^pfn ' "$scratch/arena.frames" 2>/dev/null | wc -l | tr -d ' ')
      if [ "${n:-0}" -ne "$pages" ]; then
        note "the frame list has ${n:-0} frames against $pages pages; not translating a list that does not cover the region"
      elif scp -q ${SSHOPT[@]+"${SSHOPT[@]}"} "$scratch/arena.frames" "$HOST:$plan.frames" 2>/dev/null; then
        framesarg="--frames '$plan.frames'"
        WC_SUBSTRATE_LOCATOR=frames
      else
        note "the frame list could not be copied to $HOST; taking the sweep"
      fi ;;
  esac
  if [ "$WC_SUBSTRATE_LOCATOR" = frames ]; then
    ok "the guest named its own $n page frames: the locate reads the PFN database, not the extent's contents"
  else
    note "locating by content sweep: 2.3 s over 13 GiB, and the arm on which 'page index"
    note "claimed twice' is possible. Not a file, and not a fallback to one -- both arms"
    note "locate guest RAM and differ only in how its pages are named."
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
  # doing it, which is exactly the shape §35 is about. Over a frame list this
  # verify is the trusting locator checked against the stamps at the one moment
  # they still exist to check it against -- one pass over the region rather than
  # over all of guest RAM.
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

  ok "the arena is guest RAM: all $got pages located by $WC_SUBSTRATE_LOCATOR through QEMU's flat view, and there is no file"
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
# grepped that line and printed it. Printing it is not watching it.
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
      ok "${tag:+$tag: }SUBSTRATE guest-ram: $slices arena slice(s) over $WC_SUBSTRATE_LOCATED pages located by $WC_SUBSTRATE_LOCATOR, 0 sections, 0 shared files"
      return 0 ;;
    wine-fixture)
      if [ "${sections:-0}" -ne 0 ]; then
        bad "${tag:+$tag: }SUBSTRATE WRONG: $counts -- a named section cannot be opened by a Linux process"
        return 1
      fi
      ok "${tag:+$tag: }SUBSTRATE wine-fixture (NOT A TARGET): $counts -- a file standing in for guest RAM on a box with no QEMU"
      return 0 ;;
    *)
      bad "${tag:+$tag: }no substrate was declared before this run: $counts"
      return 1 ;;
  esac
}

# The export must not be able to serve pixels behind a harness's back. A
# leftover `arena` from a run of that harness before this change is openable,
# the right size, and completely silent -- so it is removed rather than trusted
# not to be used. §18's shape, generalised from `wc-desktop.sh`, and still worth
# having now that the binaries refuse: the refusal stops *this* tree opening it,
# and the removal stops an older `wc-proxy` somebody has lying around.
#
#   wc_purge_arena_file <share-dir-on-WC_HOST>
wc_purge_arena_file() {
  local share="$1"
  [ -n "${HOST:-}" ] || return 0
  h "rm -f '$share/arena'; rm -rf '$share/keymaps' '$share/pools'" >/dev/null 2>&1
  true
}
