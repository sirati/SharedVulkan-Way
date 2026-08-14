# wc-binaries.sh -- sourced, never run. Two questions every harness that runs a
# binary should ask before it measures anything, and almost none of them did.
#
#   . "$(dirname "$0")/wc-binaries.sh"
#
#   1. Is the binary I am about to measure newer than the tree?      `fresh`
#   2. Is the copy on the far side the binary I just built?          `deployed_is`
#
# WHY THIS FILE EXISTS
#
# §35.4 measured the first one: `cargo test` does not relink `target/debug/
# wc-proxy`, so a harness run after a source change can measure the *previous*
# tree and report it as this one's behaviour. `wc-proxy.sh` and `wc-clip.sh`
# grew a `fresh()` for it; §35.6 item 6 listed seventeen harnesses that did not,
# and said the reason it was copied rather than shared is that "these scripts
# have no common file". That reason expired when `wc-substrate.sh` landed, so
# this is the shared file, and `fresh` lives here now.
#
# The second question is the one that has cost the most time on this project,
# because its symptom is a lie. On the cross-VM topology the guest side is
# *copied*, so rebuilding here changes only this side; the two then disagree at
# the handshake and the run reports a **transport fault**. `finish_attach` says
# so itself, in the refusal it raises:
#
#   "This is a version skew between two builds, not a transport fault. The usual
#    cause is a stale binary: on the cross-VM topology the GPU side is *copied*
#    to the guest, so rebuilding here changes only this side. Rebuild and
#    re-deploy **both** sides from the same tree."
#
# WHY IDENTITY AND NOT A VERSION NUMBER
#
# The obvious check is "assert both sides' `WIRE_VERSION` agree". It cannot be
# written: `WIRE_VERSION` is a `pub const u32` that the compiler inlines into
# the code that compares it, no binary carries it as data, and nothing prints it
# at startup. `grep`ping a `.exe` for it finds nothing because there is nothing
# to find.
#
# So the check is **byte identity of the deployed copy against the local one**,
# which is strictly stronger: it catches a `WIRE_VERSION` skew, a protocol-hash
# skew, and every other way a stale copy can differ, without needing any of them
# to be observable from outside. A harness that has copied a binary and then
# proved the copy is the same bytes has established what the version assertion
# was reaching for.
#
# A NOTE ABOUT READING PE BINARIES, which cost a detour to rediscover:
# **`strings` does not work on the mingw `.exe`s this project builds** — it
# reports nothing for symbols that are demonstrably present. `grep -a -c` on the
# raw file does. Anything below that needs to look inside a Windows binary uses
# `grep -a`, and anything added later should too.
#
# WHAT THE CALLER MUST ALREADY HAVE
#
#   ok/bad/note   the reporters. Fallbacks below, so sourcing cannot break a
#                 harness that spells them differently.
#   ROOT          the worktree root, which every harness already computes.

type ok   >/dev/null 2>&1 || ok()   { echo "  PASS  $*"; }
type bad  >/dev/null 2>&1 || bad()  { echo "  FAIL  $*"; }
type note >/dev/null 2>&1 || note() { echo "  ----  $*"; }

# ------------------------------------------------------------------ fresh ---
# Is this binary newer than every source file that could have changed it?
#
#   fresh <path> <what-to-do-about-it>
#
# Returns non-zero and says which file is newer. The message names the *first*
# newer file rather than counting them, because one path is enough to act on and
# a count is not.
#
# `-print -quit` stops at the first hit: this runs over a generated protocol
# crate with tens of thousands of files in it, and the full walk is not free.
fresh() {
  local bin=$1 how=$2 newer
  if [ ! -x "$bin" ] && [ ! -f "$bin" ]; then
    bad "no $bin -- $how"
    return 1
  fi
  newer=$(find "$ROOT/crates" "$ROOT/vendor" "$ROOT/Cargo.lock" \
            \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) \
            -newer "$bin" -print -quit 2>/dev/null)
  if [ -n "$newer" ]; then
    bad "$(basename "$bin") is older than ${newer#"$ROOT"/} -- $how, or this run measures the previous tree"
    return 1
  fi
  ok "$(basename "$bin") is newer than every source file in the tree"
  return 0
}

# The same question, for a block that *skips* when the binary is not built
# rather than failing.
#
#   fresh_or_skip <path> <what-to-do-about-it>
#     0  built and fresh        -- run the block
#     2  not built at all       -- skip it, as the harness always did
#     1  built but STALE        -- fail, because this is the dangerous case
#
# **The three answers are not two.** Several harnesses have blocks that skip
# when a binary is missing, which is right: not every machine builds the Windows
# half, and a skip says so honestly. But those same blocks then ran whatever
# *was* there, however old — and a stale binary is strictly worse than a missing
# one, because a missing one stops the block and a stale one produces a number.
# §35.4's measurement is exactly that: the run reported the previous tree and
# nothing said so. So absence keeps its skip and staleness becomes a failure.
fresh_or_skip() {
  local bin=$1 how=$2
  if [ ! -x "$bin" ] && [ ! -f "$bin" ]; then
    return 2
  fi
  fresh "$bin" "$how" >/dev/null 2>&1 && return 0
  fresh "$bin" "$how"
  return 1
}

# The same question about several binaries, with one line of output per failure
# and one summary line when they all pass -- because a harness that checks six
# binaries should not spend six lines saying so on the happy path.
#
#   fresh_all <what-to-do-about-it> <path>...
fresh_all() {
  local how=$1; shift
  local bad_n=0 n=0 b
  for b in "$@"; do
    n=$((n + 1))
    fresh "$b" "$how" >/dev/null 2>&1 || { fresh "$b" "$how" >/dev/null; bad_n=$((bad_n + 1)); }
  done
  if [ "$bad_n" -eq 0 ]; then
    ok "all $n binaries are newer than every source file in the tree"
    return 0
  fi
  # Re-run the failures so their reasons are printed, now that we know there
  # are some. Cheap: the walk stops at the first newer file.
  for b in "$@"; do fresh "$b" "$how" >/dev/null 2>&1 || fresh "$b" "$how"; done
  return 1
}

# ------------------------------------------------------------ deployed_is ---
# Is the copy on the far side the bytes I just built?
#
#   deployed_is <local-path> <remote-path> <runner> <what-to-do-about-it>
#
# `runner` is the name of the caller's own ssh helper -- `g` for the guest, `h`
# for the VM host -- so this makes no assumptions about how a harness reaches
# either, and works for both boundaries with one implementation.
#
# The remote path is spelled the way the *remote shell* spells it: a POSIX path
# under MSYS on the guest (`/c/Users/<guest-user>/...`), not a Windows one.
#
# A mismatch here is the failure `finish_attach` would otherwise report as a
# transport fault twenty seconds later, so the message says which it is.
deployed_is() {
  local local_path=$1 remote_path=$2 runner=$3 how=$4
  local want got

  if [ ! -f "$local_path" ]; then
    bad "no $local_path to compare against -- $how"
    return 1
  fi
  want=$(sha256sum "$local_path" 2>/dev/null | cut -d' ' -f1)
  got=$("$runner" "sha256sum '$remote_path' 2>/dev/null | cut -d' ' -f1" 2>/dev/null | tr -d '\r' | tr -d '[:space:]')

  if [ -z "$got" ]; then
    bad "$(basename "$local_path"): nothing at $remote_path on the far side -- $how"
    return 1
  fi
  if [ "$want" != "$got" ]; then
    bad "$(basename "$local_path") on the far side is NOT the binary this tree built"
    note "here  ${want:0:16}…   there ${got:0:16}…"
    note "This is a stale deployed copy, not a transport fault -- which is exactly how"
    note "it will present if the run continues: the two sides disagree at the handshake"
    note "and \`finish_attach\` raises a wire-version or protocol-hash mismatch. $how"
    return 1
  fi
  ok "$(basename "$local_path") on the far side is byte-identical to this tree's build"
  return 0
}

# ------------------------------------------------------------- built_ok -----
# Did the *build* succeed, given a status captured from `${PIPESTATUS[0]}`?
#
#   cargo build … 2>&1 | grep -E '^error' | head -5
#   built_ok "${PIPESTATUS[0]}" "the Windows half" || exit 1
#
# **Why this needs a helper at all, and why `$?` will not do.** The harnesses
# filter build output through a pipeline so that a wall of cargo noise becomes
# five lines. With `set -o pipefail` — which every harness here sets — that
# pipeline's status is the last non-zero of any stage, and `grep` **exits 1 when
# it matches nothing**. Finding no errors is the success case. So:
#
#   cargo succeeds, grep finds nothing   PIPESTATUS=(0 1 0)   pipeline rc=1
#   cargo FAILS,    grep finds nothing   PIPESTATUS=(1 1 0)   pipeline rc=1
#
# The pipeline returns 1 either way. A harness that tested `$?` would learn
# nothing, and one that tested it *naively* would treat every successful build
# as a failure. `${PIPESTATUS[0]}` is the only thing that separates them, and it
# must be read on the very next line because any command in between replaces it.
#
# §42.3 stated this wrongly — it said the pipeline "gives grep's exit status, so
# cargo's is discarded", which is what happens *without* `pipefail`. Every
# harness in this tree has `pipefail`, so the real mechanism is the one above:
# the status is not discarded, it is **indistinguishable**. The conclusion was
# right and the reason was not; §44.2 records the correction.
built_ok() {
  local st=$1 what=$2
  if [ "${st:-1}" -ne 0 ]; then
    bad "the build of $what failed (cargo exited $st) -- the previous binaries are still on disk"
    note "Nothing below this point is a measurement of the current tree. The five"
    note "lines above are cargo's own first errors."
    return 1
  fi
  return 0
}

# --------------------------------------------------------- manifest_is_ours -
# Does this ICD manifest point at a driver in *this* worktree?
#
#   manifest_is_ours <path-to-sv_icd.json> <what-to-do-about-it>
#
# **THE WORST INSTANCE OF THIS WHOLE CLASS, AND IT IS BUILT INTO THE WORKFLOW.**
# §44. `scripts/sv-worktree.sh` seeds a new worktree by reflinking the base
# tree's `target/`, which is deliberate and good — a cold rebuild of the
# generated protocol crate is enormous. But `target/debug/sv_icd.json` is a
# *generated file containing an absolute path*, and the reflinked copy says:
#
#   "library_path": "/mnt/datasets/builds/shared-vulkan-base/target/debug/libsv_icd.so"
#
# That file exists. So in a fresh worktree, before anything is rebuilt, the
# Vulkan loader is pointed at **the base tree's driver** and every one of
# `sv-cts.sh`, `sv-bench.sh` and `sv-present.sh` measures a different checkout
# than the one it is running in — silently, with `[ -f "$MANIFEST" ]` perfectly
# satisfied, because the file really is there.
#
# This is strictly worse than the stale-binary case §35.4 measured: that is the
# *previous* state of this tree, which at least shares its history. This is
# another tree entirely, whose contents no one in this run has any claim about.
# And it is the default state of every worktree on its first run, which is
# exactly when an agent is most likely to run the suite to get a baseline.
#
# `fresh` cannot catch it: the manifest's mtime is the reflink's, which is
# usually newer than the sources. The assertable fact is the path itself.
manifest_is_ours() {
  local manifest=$1 how=$2 lib
  if [ ! -f "$manifest" ]; then
    bad "no ICD manifest at $manifest -- $how"
    return 1
  fi
  lib=$(grep -o '"library_path"[[:space:]]*:[[:space:]]*"[^"]*"' "$manifest" \
        | sed 's/.*"\([^"]*\)"$/\1/')
  if [ -z "$lib" ]; then
    bad "the ICD manifest $manifest names no library_path -- $how"
    return 1
  fi
  case "$lib" in
    "$ROOT"/*)
      if [ ! -f "$lib" ]; then
        bad "the ICD manifest points into this tree at $lib, but nothing is there -- $how"
        return 1
      fi
      ok "the ICD manifest points at this tree's own driver (${lib#"$ROOT"/})"
      return 0 ;;
    *)
      bad "the ICD manifest points OUTSIDE this worktree: $lib"
      note "This run would load another checkout's driver and report the result as"
      note "this tree's. It is the reflink seed from scripts/sv-worktree.sh: the"
      note "manifest is generated and holds an absolute path, so a fresh worktree"
      note "inherits the base tree's. $how"
      return 1 ;;
  esac
}

# ------------------------------------------------------------- picture_is ---
# Is this capture a picture of the thing it claims to be?
#
#   picture_is <png> <kind> <caption>
#     kind = tray    a strip across the screen: wide, short, bounded above
#            window  a client's window: not a sliver
#            screen  a whole desktop
#
# WHY THIS IS SHARED RATHER THAN WRITTEN A FIFTH TIME
#
# §41.8 fixed one of these, in `wc-taskbar.sh`, and the audit that followed found
# the *unfixed* original still in four siblings — `wc-icon.sh`, `wc-desktop.sh`,
# `wc-popup.sh` and `wc-titlebar.sh` — where between them they gate **twelve**
# checked-in photographs. In all four the geometry is interpolated into the
# success string and compared with nothing:
#
#   ok "$caption -> docs/stage11/$png ($(identify -format '%wx%h' "$png"))"
#
# so a 1x1 capture, or an all-black one, converts fine and reads as a PASS.
# These are the stages' headline evidence, which is the worst place for a gate
# that cannot fail.
#
# AND THE TRAP THAT COMES WITH FIXING THEM. §41.8 was the opposite defect: a
# `w < 200 || h < 200` bound written for a window and applied to a taskbar, which
# on this guest is **1280x48**, so it could never pass. `wc-icon.sh`'s tray shots
# and `wc-desktop.sh`'s `--class Shell_TrayWnd` shots are that same strip. A
# single bound for all callers would reintroduce exactly the bug §41.8 removed,
# which is why `kind` is a required argument here and not a default.
#
# The bounds are weak, absolute, and about *shape* rather than exact size: what
# they exclude is a capture that cannot be of the named thing at all. The colour
# floor is §35.3's — 16 against the 1 a blank surface gives.
picture_is() {
  local png=$1 kind=$2 caption=$3
  local geo w hgt colours why=""

  if [ ! -s "$png" ]; then
    bad "$caption: no image at $png"
    return 1
  fi
  geo=$(identify -format '%wx%h' "$png" 2>/dev/null)
  colours=$(identify -format '%k' "$png" 2>/dev/null)
  w=${geo%x*}; hgt=${geo#*x}

  case "$kind" in
    tray)
      [ "${w:-0}" -lt 400 ] && why="${geo:-nothing} is too narrow to be a taskbar spanning the screen"
      [ -z "$why" ] && [ "${hgt:-0}" -lt 16 ]  && why="${geo:-nothing} is too short to hold a button"
      # Bounded above, which turns "is it big enough" into "is it the right
      # window" -- a full-desktop capture arriving here means the class
      # selector picked something other than Shell_TrayWnd.
      [ -z "$why" ] && [ "${hgt:-0}" -gt 200 ] && why="${geo:-nothing} is too tall to be Shell_TrayWnd -- the capture selected the wrong window" ;;
    window)
      [ "${w:-0}" -lt 100 ] || [ "${hgt:-0}" -lt 100 ] \
        && why="${geo:-nothing} is too small to be a client's window" ;;
    screen)
      [ "${w:-0}" -lt 640 ] || [ "${hgt:-0}" -lt 400 ] \
        && why="${geo:-nothing} is too small to be a desktop" ;;
    *) bad "picture_is: unknown kind $kind"; return 1 ;;
  esac

  if [ -n "$why" ]; then
    bad "$caption: $why"
    return 1
  fi
  if [ "${colours:-0}" -lt 16 ]; then
    bad "$caption: $geo with only ${colours:-0} distinct colours -- a blank surface"
    return 1
  fi
  ok "$caption ($geo, $colours distinct colours, checked as $kind)"
  return 0
}

# The same for a list, against one remote directory.
#
#   deployed_all <local-dir> <remote-dir> <runner> <how> <basename>...
#
# Used by the deploy steps, which copy a fixed set into one directory. Note the
# *renaming* deployers -- `wc-icon.sh` and `wc-taskbar.sh` install
# `wc-compositor.exe` as `wc-s11-compositor.exe` and `wc-s32-compositor.exe`
# (§13.9: an image name is not an identity on a shared guest) -- so those pass
# `<local>:<remote>` pairs rather than bare names.
deployed_all() {
  local ldir=$1 rdir=$2 runner=$3 how=$4; shift 4
  local rc=0 spec l r
  for spec in "$@"; do
    case "$spec" in
      *:*) l=${spec%%:*}; r=${spec#*:} ;;
      *)   l=$spec; r=$spec ;;
    esac
    deployed_is "$ldir/$l" "$rdir/$r" "$runner" "$how" || rc=1
  done
  return $rc
}
