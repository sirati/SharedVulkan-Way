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
# Is this binary built from the sources it depends on, as they are now?
#
#   fresh <path> <what-to-do-about-it>
#
# WHY THIS IS NOT "NEWER THAN THE TREE" ANY MORE
#
# It was, and its normal state became a false positive -- which is worse than
# no check, because a guard that cries wolf trains eighteen harnesses to ignore
# it, and then the stale binary it exists to catch walks straight through.
#
# Two independent defects, both measured rather than reasoned about:
#
# 1. **The scope was the whole tree.** `wc-proxy` depends on 66 `.rs` files and
#    `wc-compositor` on 272, out of 481 in `crates/` and `vendor/`. Their
#    dependency graphs are *disjoint*, so touching a compositor source marked
#    the proxy stale, rebuilding the proxy did not clear it, and rebuilding the
#    compositor made the proxy stale again. One crate at a time never converges;
#    the only sequence that worked was to touch both and build both, which is a
#    ritual rather than a check.
#
# 2. **mtime is not content.** `git stash pop`, `git checkout` and the reflink
#    seeding this project's worktrees use all rewrite mtimes on files whose
#    bytes are unchanged. Every one of those invalidated every binary.
#
# So the scope comes from **cargo's own depfile** -- `<bin>.d`, the exact set
# cargo compiled -- and the verdict is **content**, which is the same standard
# `deployed_is` already holds the far side to. A hash of the dependency set is
# recorded beside the binary when it is verified; if the hash still matches, the
# sources are byte-for-byte what they were and the binary is current no matter
# what happened to any mtime.
#
# mtime keeps one job, and it is the one it is good at: when the content *has*
# changed, deciding whether the binary or the source moved last.
#
# A binary with no depfile falls back to the old whole-tree walk and says so.
# That is the honest answer for a binary cargo did not build -- a copied `.exe`,
# say -- rather than passing it on a check that was never run.

# The sources cargo says this binary was built from, one per line, rebased onto
# this worktree.
#
# The rebasing is not decoration: `sv-worktree.sh` seeds `target/` by reflink
# from a base checkout, so a freshly seeded worktree's depfiles name *the base
# tree's* paths. Comparing this worktree's binary against another worktree's
# sources is exactly the class of mistake this file exists to stop.
_fresh_deps() {
  local bin=$1 dep
  # Cargo names the depfile after the *crate*, so a binary's platform suffix is
  # not part of it: `wc-proxy.exe` -> `wc-proxy.d`, `libsv_icd.so` -> `libsv_icd.d`.
  # Stripping only `.exe` sent every `.so` down the whole-tree fallback, where it
  # was blamed on the first unrelated file the walk happened to reach.
  dep="${bin%.exe}"; dep="${dep%.so}"; dep="${dep%.dylib}.d"
  [ -f "$dep" ] || return 1
  # A Makefile rule: "target: dep dep dep", possibly over several lines.
  sed -e 's/\\$//' -e 's/^[^:]*://' "$dep" 2>/dev/null \
    | tr ' ' '\n' \
    | sed '/^[[:space:]]*$/d' \
    | awk -v root="$ROOT" '{ if (match($0, /\/(crates|vendor)\//)) print root substr($0, RSTART) }' \
    | sort -u
}

# One hash over the *contents* of that set. The file list is sorted, so the
# hash is stable across filesystem ordering; the paths go in as well as the
# bytes, so adding an empty file is a change.
_fresh_hash() {
  local bin=$1 list
  list=$(_fresh_deps "$bin") || return 1
  [ -n "$list" ] || return 1
  printf '%s\n' "$list" | xargs -r sha256sum 2>/dev/null | sha256sum | cut -d' ' -f1
}

fresh() {
  local bin=$1 how=$2 newer want stamp
  if [ ! -x "$bin" ] && [ ! -f "$bin" ]; then
    bad "no $bin -- $how"
    return 1
  fi

  want=$(_fresh_hash "$bin")
  if [ -z "$want" ]; then
    # No depfile: cargo did not build this here. The whole-tree walk is the only
    # thing left, and it is named as the weaker check it is.
    newer=$(find "$ROOT/crates" "$ROOT/vendor" "$ROOT/Cargo.lock" \
              \( -name '*.rs' -o -name 'Cargo.toml' -o -name 'Cargo.lock' \) \
              -newer "$bin" -print -quit 2>/dev/null)
    if [ -n "$newer" ]; then
      bad "$(basename "$bin") is older than ${newer#"$ROOT"/} -- $how, or this run measures the previous tree"
      return 1
    fi
    note "$(basename "$bin") has no depfile; checked against the whole tree instead"
    return 0
  fi

  stamp="${bin%.exe}.srcsha"
  if [ -f "$stamp" ] && [ "$(cat "$stamp" 2>/dev/null)" = "$want" ]; then
    ok "$(basename "$bin") was built from these exact sources"
    return 0
  fi

  # The content differs from the last verified state, so now ask which moved
  # last -- over this binary's own dependencies and nothing else.
  newer=$(_fresh_deps "$bin" | while read -r f; do
            [ -n "$f" ] && [ "$f" -nt "$bin" ] && { printf '%s\n' "$f"; break; }
            :
          done)
  if [ -n "$newer" ]; then
    bad "$(basename "$bin") is older than ${newer#"$ROOT"/} -- $how, or this run measures the previous tree"
    return 1
  fi
  printf '%s\n' "$want" > "$stamp" 2>/dev/null
  ok "$(basename "$bin") is newer than every source it depends on"
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

# ----------------------------------------------------------- proc_running ---
# Is a process of this name running -- **excluding the command that is asking**?
#
#   proc_running <exact-executable-name>
#     0  yes, something else is running it
#     1  no
#
# WHY THIS EXISTS, AND IT IS NOW THREE TIMES
#
# `pgrep -f` and `pkill -f` match against the **whole command line**, and the
# command line doing the asking contains the pattern it is asking about. So a
# guard written as
#
#     ssh host 'pgrep -f qemu-serve >/dev/null && refuse'
#
# matches its own shell, every time, and refuses unconditionally. On this
# project that has now happened three times:
#
#   1. a `pkill -f` intended for one process took down the shell issuing it;
#   2. a QMP guard refused a socket that was free for an hour, because the ssh
#      command carrying the check contained `qemu-serve` -- and it was written
#      *after* lesson 1;
#   3. a `ps | grep` looking for stray pollers found only its own `grep`.
#
# The third is the harmless one and the most instructive: the check was for
# exactly this defect and exhibited it.
#
# THE FIX THAT GENERALISES is to match on something the invoking command cannot
# contain. In order of preference:
#
#   - a **pidfile** the process itself wrote. Unambiguous, and survives a rename.
#   - **`pgrep -x <name>`**, which matches the executable name rather than the
#     command line. A shell running `pgrep -x sv-hostd` is named `bash`, so it
#     cannot match. This is what the function below does.
#   - failing both, `pgrep -f <pat>` with **`$$` and its ancestors excluded** --
#     which is fiddly, easy to get wrong under `ssh`, and the reason the other
#     two are preferred.
#
# A guard that cannot distinguish itself from what it guards against is not a
# guard; it is a coin that always lands the same way.
proc_running() {
  local name=$1 pids
  pids=$(pgrep -x "$name" 2>/dev/null) || return 1
  [ -n "$pids" ]
}

# ------------------------------------------------------------- proc_kill ----
# Stop a process this run started, by **pid or pidfile — never a pattern**.
#
#   proc_kill <pid> <what-it-is>
#   proc_kill_pidfile <path> <what-it-is>
#
# WHY THIS EXISTS, AND IT IS NOT HYPOTHETICAL
#
# `proc_running` above matches an exact executable name because a `pgrep -f`
# guard matches the shell running it. **The kill side of that argument was never
# written, and today it cost the machine.** An over-broad
#
#     pkill -9 -f weston
#
# — intended for one harness's leftovers — matched the desktop session hosting
# 1Password, killed it, and took with it the SSH agent that carries the keys for
# `testhost` and the guest, and the key that signs commits. Two machines
# unreachable and signing broken, from a cleanup line in an unrelated run. It is
# the fourth instance of this defect in one session; the first three were guards
# matching their own shells, which is embarrassing but self-contained. This one
# matched *another agent's process*, which is not.
#
# **A pattern is the wrong instrument for stopping a process you started.** You
# know its pid — you started it. The pattern is a way of *rediscovering* a pid
# you already had, and it rediscovers other people's too. The rule:
#
#   * If you started it in this shell, you have `$!`. Use it.
#   * If it outlives the shell, have it write a pidfile, and use that.
#   * If you have neither, you are not stopping *your* process — you are
#     stopping whatever matches, and you cannot know what that is.
#
# `pkill -f` is still correct in one shape, which several harnesses here use:
# anchored on an absolute path into **this run's own directory**
# (`pkill -f '^$HOST_DIR/wc-proxy'`). That is a pattern no other agent's command
# line can contain, because the directory is unique to this run. What is refused
# below is a bare program name, which is what `weston` was.
proc_kill() {
  local pid=$1 what=${2:-a process} sig=${3:-TERM}

  # A pid is digits. Anything else is a pattern by another name, and the whole
  # point of this function is that it will not take one -- including the empty
  # string, which `kill ""` turns into an error and `kill $(pgrep …)` with no
  # match turns into "kill the whole process group".
  case "$pid" in
    ''|*[!0-9]*)
      bad "proc_kill: refusing to kill '$pid' ($what): that is not a pid"
      note "Pass a pid or use proc_kill_pidfile. A name or pattern matches processes"
      note "this run did not start -- an over-broad 'pkill -f weston' took out the"
      note "SSH agent and commit signing on this machine today."
      return 1 ;;
  esac

  [ "$pid" -gt 1 ] 2>/dev/null || { bad "proc_kill: refusing pid $pid ($what)"; return 1; }
  kill -s "$sig" "$pid" 2>/dev/null || return 0   # already gone is success
  # Give it a moment to go, then insist. `kill -0` is the check, not `pgrep`.
  local i=0
  while [ "$i" -lt 40 ] && kill -0 "$pid" 2>/dev/null; do sleep 0.05; i=$((i + 1)); done
  if kill -0 "$pid" 2>/dev/null; then
    kill -s KILL "$pid" 2>/dev/null
    note "$what (pid $pid) ignored SIG$sig and was killed"
  fi
  return 0
}

# The same, for a process that outlives the shell that started it.
#
# The pidfile is removed **only when the pid in it was ours to stop**: a stale
# pidfile whose pid has been recycled by an unrelated process is the same defect
# as a pattern, one indirection along. `proc_kill` refuses a non-numeric body,
# which is the common form of a truncated or half-written pidfile.
proc_kill_pidfile() {
  local file=$1 what=${2:-a process} pid
  [ -f "$file" ] || return 0
  pid=$(tr -dc '0-9' < "$file" 2>/dev/null)
  if [ -z "$pid" ]; then
    note "$file holds no pid; leaving it rather than guessing what to stop"
    return 1
  fi
  proc_kill "$pid" "$what" && rm -f "$file"
}
