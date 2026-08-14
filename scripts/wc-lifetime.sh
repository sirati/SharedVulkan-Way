# wc-lifetime.sh -- sourced, never run. The assertion this tree did not have:
# **that something stopped existing.**
#
#   . "$(dirname "$0")/wc-lifetime.sh"
#   wc_assert_no_window_leaks <compositor-log> [tag]
#
# WHY THIS FILE EXISTS
#
# Every gate in this project asserts that something **appeared**. A window is
# created, an `HWND` exists, pixels arrive, a title is set, an icon is on the
# taskbar. Not one of them asserted that anything went away, and the two are not
# the same claim: a compositor that creates a window per client and destroys
# none of them passes every check in this tree, for a whole session, while
# leaking an `HWND` per client. §53 is the primary treatment of this category --
# an outside process that asks the window manager -- and this file is its cheap
# half: the same question answered by the compositor itself, on every platform
# and every run, including ones where no witness process can be deployed.
#
# It is a real risk rather than a hypothetical one, and §21 is the precedent.
# smithay calls `IdleInhibitHandler::uninhibit` from exactly one place: the
# client's explicit `destroy` request. There is **no `destroyed` hook**, so a
# client that crashes leaves an inhibitor nothing will ever revoke. Window
# teardown runs through a different path and is believed sound -- but "believed
# sound" is the state the idle inhibitor was in before somebody looked, and the
# reason it was found was that a display which never sleeps is visible. A leaked
# `HWND` is not visible. Nothing in a log, nothing on screen, nothing in a
# screenshot; the process simply holds a handle until it exits.
#
# WHAT THE COMPOSITOR PRINTS, AND WHY IT IS FIVE NUMBERS
#
#   wc-compositor: windows at exit: 3 created, 3 destroyed, 0 still held,
#                  0 still alive, 0 client(s) still connected
#
#   created / destroyed   what the compositor believes it did.
#   still held            entries left in its own map.
#   still alive           **what Windows says**, from `EnumWindows` filtered to
#                         this process and the `WaycomposeToplevel` class.
#   clients connected     whether anybody was still there. The qualifier.
#
# The fourth is the point and the fifth is what makes it usable.
#
# The first three are the compositor's own account of its own behaviour, and a
# bug in the teardown path is exactly the circumstance in which that account is
# wrong. `DestroyWindow` returning false, a window owned by the wrong thread, a
# handle already replaced -- each leaves `created` and `destroyed` balanced, the
# map empty, and a window on the desktop. Only the fourth number can see that,
# and it is the one asked of somebody else.
#
# The fifth exists because the first real run of this check **failed on a run
# with no bug in it**: the compositor timed out while `weston-terminal` was
# still connected, so of course its window was still standing. A window whose
# client is alive is not a leak; the leak is a window with nobody behind it.
# Without this number the check would have fired on every harness in the tree
# that stops a compositor with `--timeout` rather than `--once`, and would have
# been switched off within a day as noise.
#
# On the headless platform the last two are necessarily the same number, because
# a platform with no window manager has no second opinion to offer. That is
# stated in the output rather than hidden: the Linux leg checks the bookkeeping
# and the Wine leg checks the bookkeeping *and* the platform.
#
# WHAT THE CALLER MUST ALREADY HAVE
#
#   ok/bad/note   the reporters. Fallbacks below, so sourcing cannot break a
#                 harness that spells them differently.

type ok   >/dev/null 2>&1 || ok()   { echo "  PASS  $*"; }
type bad  >/dev/null 2>&1 || bad()  { echo "  FAIL  $*"; }
type note >/dev/null 2>&1 || note() { echo "  ----  $*"; }

# wc_assert_no_window_leaks <compositor-log> [tag]
#
# Returns non-zero if the log has no line, so a caller can tell "the compositor
# is too old to say" from "the compositor said something bad". A harness that
# treats a missing line as a pass is back where this file started.
wc_assert_no_window_leaks() {
  local log="$1" tag="${2:-}"
  local line
  line=$(grep -o 'windows at exit: [0-9]* created, [0-9]* destroyed, [0-9]* still held, [0-9]* still alive, [0-9]* client(s) still connected' "$log" 2>/dev/null | tail -1)

  if [ -z "$line" ]; then
    # **Not a pass.** A compositor that exited without saying is a compositor
    # this assertion did not run against, and §35's rule is that a check which
    # cannot fail is not a check -- of which "a check that silently did not
    # run" is the purest case.
    bad "${tag:+$tag: }the compositor never reported its window accounting; nothing here checked for a leak"
    return 1
  fi

  local made gone held live clients
  made=$(echo "$line" | awk '{print $4}')
  gone=$(echo "$line" | awk '{print $6}')
  held=$(echo "$line" | awk '{print $8}')
  live=$(echo "$line" | awk '{print $11}')
  clients=$(echo "$line" | awk '{print $14}')

  # **A window whose client is still connected is not a leak**, and this line is
  # here because the first run of this assertion said it was. A compositor that
  # exits on an idle timeout while somebody is still using it is entitled to
  # still have their window; the leak is a window with *nobody behind it*. Every
  # harness in this tree that stops a compositor by `--timeout` rather than by
  # `--once` would have failed on that, and the check would have been withdrawn
  # within a day as noise.
  if [ "${clients:-0}" -gt 0 ]; then
    note "${tag:+$tag: }$clients client(s) were still connected when the compositor exited, so $held window(s) standing is not a leak -- teardown was never reached"
    return 0
  fi

  # A run in which no window was ever made cannot demonstrate that windows are
  # cleaned up. Said out loud rather than passed, because "0 created, 0 leaked"
  # is exactly what a broken harness reports.
  if [ "${made:-0}" -eq 0 ]; then
    note "${tag:+$tag: }no window was created in this run, so nothing here says anything about teardown"
    return 0
  fi

  local ok_all=1
  if [ "${held:-0}" -ne 0 ] || [ "${live:-0}" -ne 0 ]; then
    bad "${tag:+$tag: }WINDOW LEAK: $made created, $gone destroyed, $held still held, $live still alive"
    ok_all=0
  fi
  if [ "${made:-0}" -ne "${gone:-0}" ]; then
    # Reported separately from the leak above, because they are different
    # faults. Unequal counts with nothing alive means the compositor lost track
    # of a window it did destroy; equal counts with something alive means it
    # believes it destroyed one it did not. The second is the dangerous one and
    # the first is the one that predicts it.
    bad "${tag:+$tag: }the compositor created $made windows and destroyed $gone; the two must agree by the time it exits"
    ok_all=0
  fi
  [ "$ok_all" = 1 ] || return 0

  ok "${tag:+$tag: }every one of the $made window(s) this run created was destroyed, and Windows agrees none survives"
  return 0
}
