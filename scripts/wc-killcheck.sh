#!/usr/bin/env bash
# wc-killcheck.sh -- no harness may kill by a name the whole machine shares.
#
#   scripts/wc-killcheck.sh          check; non-zero if a new unscoped kill exists
#   scripts/wc-killcheck.sh --list   show every kill in scripts/ and its verdict
#
# WHY
#
# An over-broad `pkill -9 -f weston`, meant for one harness's leftovers, matched
# the desktop session hosting 1Password and killed it — taking the machine's SSH
# agent (no guest access) and its commit-signing key with it, for the rest of the
# session. §59. The user's instruction was "that must never happen again", and a
# rule nobody runs is the same shape as the defect it prevents (§54), so this is
# behind `cargo test --workspace`.
#
# THE ACCEPTANCE RULE, which is the sweep's finding encoded and nothing broader:
#
#   **A pattern is allowed only when it names something only THIS RUN could have
#   produced.**
#
# In practice that means the pattern interpolates a shell variable naming a
# directory, socket or port this run owns — `^$HOST_DIR/wc-proxy`,
# `$DIR/sv-hostd`, `qemu-system.*$RUNDIR`. A *literal* cannot be run-unique: two
# concurrent runs of the same harness share every literal in it.
#
# What is refused is a bare program name — `weston`, `qemu`, `firefox` — which
# names something the whole machine shares. That is the shape that caused the
# outage, and it is the only shape this gate exists to stop.
#
# NEW OCCURRENCES ONLY. Eleven of the thirteen kills in `scripts/` were already
# correctly anchored when this was written; the gate does not demand a rewrite of
# code that is right. The two that are not run-unique are listed below with the
# reason each is tolerated, which is the baseline. Anything not matching the rule
# and not in the baseline is new, and fails.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT" || exit 2
MODE="${1:-check}"

# ---------------------------------------------------------------- baseline ---
# Tolerated literals, each with the argument for it. A baseline entry is a
# *decision*, so it carries its reason here rather than being a bare exemption:
# an allowlist without reasons becomes the place rules go to die.
#
#   http.server 8931   a TCP port. Only one process can hold a port, so there is
#                      never a second match to hit by accident. Scoped to
#                      something the run owns, if not to the run itself.
#
#   wc-launch/firefox  a *path fragment*, and the one judgement call here. It
#                      cannot match a user's ordinary Firefox, because an
#                      ordinary Firefox has no `wc-launch/firefox` in its command
#                      line — so it is far better than a bare name and nothing
#                      like the outage. But it is **harness-unique, not
#                      run-unique**: two concurrent `wc-launch.sh` runs would
#                      match each other. They would also collide on the shared
#                      profile directory that pattern points at, so the kill is
#                      not the first thing that breaks. Tolerated on that basis
#                      and recorded as the one entry that would become correct
#                      by giving each run its own profile directory.
baseline_ok() {
  case "$1" in
    *"http.server 8931"*) return 0 ;;
    *"wc-launch/firefox"*) return 0 ;;
  esac
  return 1
}

# Run-unique by construction: the pattern interpolates a variable. A literal
# cannot be unique to a run, so this is the whole of the positive rule.
run_scoped() { case "$1" in *'$'*) return 0 ;; esac; return 1; }

bad_n=0; ok_n=0; base_n=0
: > /tmp/wc-killcheck-$$.bad
trap 'rm -f /tmp/wc-killcheck-'"$$"'.bad' EXIT

# `pkill`/`killall` anywhere in scripts/, including inside the here-documents the
# harnesses ship to the guest and the VM host -- those run on another machine,
# which is where an over-broad match does the most damage.
while IFS= read -r hit; do
  file=${hit%%:*}; rest=${hit#*:}; line=${rest%%:*}; text=${rest#*:}
  case "$file" in */wc-killcheck.sh) continue ;; esac
  # **Talking about the defect is not the defect**, and this gate flagged its
  # own documentation on its first run — `proc_kill`'s refusal message quotes
  # the very command that caused the outage, so the string `pkill -f weston`
  # appears in `wc-binaries.sh` inside a `note`. That is the third time a
  # checker in this tree has caught its own prose (§48.3, §54.1), and the
  # answer is the same each time: narrow the rule, do not exempt the file.
  #
  # So: comments are skipped, and so are *reporter calls*, whose whole job is to
  # print text at a person. A line whose first word is `note`/`bad`/`echo` and
  # friends is saying something, not doing it. This is deliberately a rule about
  # one line's leading token rather than an exemption for a path, because
  # `wc-binaries.sh` is exactly the file most likely to grow a real kill.
  head=$(printf '%s' "$text" | sed 's/^[[:space:]]*//')
  case "$head" in
    '#'*) continue ;;
    note\ *|bad\ *|ok\ *|skip\ *|echo\ *|printf\ *) continue ;;
  esac

  if run_scoped "$text"; then
    ok_n=$((ok_n + 1))
  elif baseline_ok "$text"; then
    base_n=$((base_n + 1))
    [ "$MODE" = --list ] && echo "  BASELINE $file:$line: $(printf '%s' "$text" | sed 's/^[[:space:]]*//')"
  else
    bad_n=$((bad_n + 1))
    printf '  %s:%s: %s\n' "$file" "$line" "$(printf '%s' "$text" | sed 's/^[[:space:]]*//')" \
      >> /tmp/wc-killcheck-$$.bad
  fi
done < <(grep -rnE '(^|[^[:alnum:]_-])(pkill|killall)([^[:alnum:]_-]|$)' scripts/ 2>/dev/null)

echo "== kills in scripts/ =="
echo "  $ok_n scoped to something this run owns (a \$variable in the pattern)"
echo "  $base_n tolerated by the baseline, each with its reason in this script"
echo "  $bad_n name something the machine shares"

if [ "$bad_n" -gt 0 ]; then
  echo
  echo "-- REFUSED: a kill that can match a process this run did not start"
  cat /tmp/wc-killcheck-$$.bad
  cat <<'EOT'

   An over-broad `pkill -9 -f weston` killed the session hosting 1Password on
   this machine, taking the SSH agent and the commit-signing key with it (§59).

   Use one of these instead, in this order of preference:

     proc_kill <pid> <what>              you started it, so you have its pid
     proc_kill_pidfile <file> <what>     it outlives the shell; have it write one
     kill_exact <name> [sig]             matches the executable name, not the
                                         command line, so it cannot match the
                                         shell running it

   If a pattern is genuinely the only option, it must name something only THIS
   RUN could have produced -- interpolate the run's own directory, socket or
   port, as `pkill -f "^$HOST_DIR/wc-proxy"` does. A literal cannot be
   run-unique: two concurrent runs share every literal in the file.

   All three helpers are in scripts/wc-binaries.sh.
EOT
fi

[ "$bad_n" -eq 0 ]
