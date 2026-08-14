#!/usr/bin/env bash
# Run the **Wayland Conformance Suite** against this compositor, and score it.
#
#   scripts/wc-wlcs.sh globals            what the compositor advertises, from a
#                                         third-party client, into the file WLCS
#                                         is told about
#   scripts/wc-wlcs.sh list               the suite's 1,228 tests, by suite
#   scripts/wc-wlcs.sh headless [FILTER]  the fast gate: the Linux build
#   scripts/wc-wlcs.sh wine [FILTER]      the Win32 leg: wc-compositor.exe under
#                                         Wine, reached through wc-proxy
#   scripts/wc-wlcs.sh compare            where the two legs disagree, which is
#                                         the only honest measurement of how
#                                         much Wine differs
#   scripts/wc-wlcs.sh score [LOG]        re-print the scoreboard for a run
#   scripts/wc-wlcs.sh all                everything, in that order
#
# Run from inside `nix develop`. FILTER is a gtest filter, e.g.
# 'XdgToplevelStableTest.*' — the whole suite takes tens of minutes and a
# filtered run takes seconds, so use one while you are fixing something.
#
# WHY THIS SUITE, AND WHAT A NUMBER FROM IT MEANS
#
# docs/waycompose-design.md §26 is the argument; the short version is that WLCS is the only
# thing that exists which is a *conformance* suite rather than one compositor's
# own regression tests, and that it drives our compositor through an integration
# module (`crates/wc-wlcs`) rather than testing a reference implementation.
#
# THE THING TO READ BEFORE BELIEVING A PASS
#
# The two legs cover different halves and neither covers everything:
#
#   * **headless.** `wc-compositor` built for Linux, with the `headless`
#     platform and `--control` standing where Windows stands. Everything from
#     `platform::FromUi` upwards is the shipping code — the protocol, the
#     surface tree, focus, serials, the shell. Everything below it is not:
#     no `HWND`, no `WM_MOUSEMOVE`, no GDI, and no fd shim, because a Linux
#     client's `SCM_RIGHTS` is delivered by the kernel.
#
#   * **wine.** `wc-compositor.exe`, a real Windows binary, in a real Win32
#     message loop, with `wc-proxy` in front of it translating every `h`
#     argument in band (docs/waycompose-design.md §9.4). That is the half the first leg
#     cannot reach, and it is the half the fd rule lives in. What it cannot do
#     is drive a pointer or place a window: `--control` is `#[cfg(unix)]` and
#     there is no AF_UNIX listener in the Windows binary, so this leg runs the
#     tests that need neither, and says so rather than pretending.
#
# Neither leg has a hypervisor in it. §26.5 says what the third leg would be and
# what it needs first; it is not implemented here, and a scoreboard from this
# script must not be quoted as one from the shipping configuration.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"
SHIM="$LINUX_BIN/libwc_wlcs.so"
OUT="$ROOT/target/wlcs"

pass=0; fail=0; skip=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
nay() { echo "  SKIP  $*"; skip=$((skip + 1)); }
note(){ echo "  ....  $*"; }

# STAGE 42. `fresh` / `fresh_or_skip`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"

RUNDIR=$(mktemp -d /tmp/wc-wlcs-run-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${CPID:-}" ] && { kill "$CPID" 2>/dev/null; wait "$CPID" 2>/dev/null; }
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

mkdir -p "$OUT"
EXTFILE="$OUT/extensions.txt"

# ---------------------------------------------------------------------------
# The exclusion table
# ---------------------------------------------------------------------------
#
# WLCS turns a global that is absent *and* undeclared into a skip rather than a
# failure (`in_process_server.cpp`, `bind_if_supported`), and it prints
# `Missing extension: <name>>= <version>` on the line before it. So the suite
# already says *which* protocol every skip is about; what it cannot say is
# **why we do not have it**, and the difference between "cannot be done on
# Windows" and "nobody has done it yet" is the entire value of a compliance
# claim.
#
# So the table below is keyed by **extension**, not by suite, and the score
# function reads the extension out of the log. That is not a tidiness
# preference. Nearly every WLCS suite is parameterised over shell types —
# `SurfaceInputRegions/SurfaceInputCombinations` runs each case against
# `wl_shell`, `zxdg_shell_v6`, `xdg_wm_base` and a subsurface — so a
# suite-keyed table has to describe a suite whose skips have three different
# causes with one sentence, and the first draft of this file did exactly that
# and left thirteen suites unexplained.
#
# Two categories, and the labels are deliberately different words:
#
#   JUSTIFIED  the protocol cannot be implemented here, or implementing it
#              would contradict what this project is for. Not a debt.
#   GAP        implementable on Windows, simply not implemented. A debt, and
#              the number that should go down.
#
# A third category is worth naming for its absence: the protocols this project
# judged *impossible* — `zwp_linux_dmabuf_v1`, `wl_drm`, `wp_presentation` —
# have no tests in WLCS at all. See §26.6: the suite does not ask that
# question, so passing it is not evidence about the answer.
reason_for() {
  case "$1" in
    zwlr_layer_shell_v1)
      echo "JUSTIFIED a layer surface is a panel, a wallpaper or a lock screen, and on this desktop Windows owns all three" ;;
    zxdg_shell_v6)
      echo "JUSTIFIED superseded by xdg-shell stable in 2017 and required by no client this project targets" ;;
    wl_shell)
      echo "JUSTIFIED removed from the protocol; xdg-shell replaced it" ;;
    zwp_primary_selection_device_manager_v1|gtk_primary_selection_device_manager)
      echo "JUSTIFIED Windows has no middle-click selection, so a compositor-side one would be a clipboard no Windows application can see" ;;
    zwlr_foreign_toplevel_manager_v1)
      echo "JUSTIFIED it exists so a Linux dock can list windows, and the dock here is the Windows taskbar" ;;
    zwp_pointer_constraints_v1)
      echo "GAP ClipCursor and SetCursorPos exist on Win32; nothing binds them yet" ;;
    zwp_relative_pointer_manager_v1)
      echo "GAP WM_INPUT raw motion exists on Win32; nothing binds it yet" ;;
    zwp_text_input_manager_v2|zwp_text_input_manager_v3|zwp_input_method_v1|zwp_input_method_manager_v2)
      echo "GAP Windows has an IME (TSF) and none of it is wired up" ;;
    zwlr_virtual_pointer_manager_v1)
      echo "GAP not implemented" ;;
    wp_viewporter)
      echo "GAP the blitter could scale and crop; it does not" ;;
    wp_fractional_scale_manager_v1)
      echo "GAP not implemented, and it is the pair to wl_compositor v6's preferred_buffer_scale" ;;
    zxdg_output_manager_v1)
      echo "GAP not implemented" ;;
    # WLCS's own self-checks, which deliberately ask for extensions that exist
    # nowhere in order to prove its skip machinery works. Counting these as
    # our exclusions would be counting somebody else's test of themselves.
    xdg_not_really_an_extension|wlcs_non_existent_extension|"(no extension named)")
      echo "SELFTEST WLCS proving its own skip machinery; not about this compositor" ;;
    *) echo "" ;;
  esac
}

# ---------------------------------------------------------------------------
# What the compositor actually advertises
# ---------------------------------------------------------------------------
#
# Generated from `wayland-info` — libwayland, written by somebody else — rather
# than from a table in our source, because the table is what tells WLCS which
# missing globals are expected, and a table that says more than the compositor
# does turns a failure into a skip. That is the one way this harness could lie
# about the number it prints, so the number is taken from outside.
globals() {
  echo "== globals: what a third-party client sees"
  if [ ! -x "$LINUX_BIN/wc-compositor" ]; then
    bad "$LINUX_BIN/wc-compositor is not built (cargo build --workspace)"
    return
  fi
  export XDG_RUNTIME_DIR="$RUNDIR"
  chmod 700 "$RUNDIR"
  "$LINUX_BIN/wc-compositor" --socket wc-globals --timeout 30 \
    > "$RUNDIR/globals.log" 2>&1 &
  CPID=$!
  for _ in $(seq 100); do [ -S "$RUNDIR/wc-globals" ] && break; sleep 0.1; done
  if [ ! -S "$RUNDIR/wc-globals" ]; then
    bad "the compositor never listened; see $RUNDIR/globals.log"
    return
  fi
  {
    echo "# Generated by scripts/wc-wlcs.sh from wayland-info against"
    echo "# $LINUX_BIN/wc-compositor at $(date -Is)."
    echo "# This is what WLCS is told we support. Nothing may be added to it by"
    echo "# hand: a name here that is not in the registry turns a real failure"
    echo "# into a skip. See crates/wc-wlcs/src/imp.rs."
    WAYLAND_DISPLAY=wc-globals wayland-info 2>/dev/null \
      | sed -n "s/^interface: '\([a-z_0-9]*\)', *version: *\([0-9]*\).*/\1 \2/p"
  } > "$EXTFILE"
  kill "$CPID" 2>/dev/null; wait "$CPID" 2>/dev/null; CPID=""

  local n
  n=$(grep -cv '^#' "$EXTFILE")
  if [ "$n" -lt 5 ]; then
    bad "wayland-info reported only $n globals; something did not run"
    return
  fi
  ok "$n globals, written to $EXTFILE"
  grep -v '^#' "$EXTFILE" | while read -r name version; do
    note "  $name v$version"
  done

  # The shim carries a fallback copy for hand-runs. If it has drifted from what
  # the compositor advertises, a hand-run is silently testing a different claim
  # from the one this harness makes — which is precisely the shape of failure
  # docs/waycompose-design.md §13.6 is about, so it is a FAIL rather than a note.
  local table drift=0
  table=$(sed -n 's/^ *("\([a-z_0-9]*\)", \([0-9]*\)),$/\1 \2/p' \
    "$ROOT/crates/wc-wlcs/src/imp.rs")
  while read -r line; do
    grep -qxF "$line" "$EXTFILE" || { note "fallback table has: $line"; drift=1; }
  done <<< "$table"
  while read -r line; do
    [ -z "$line" ] && continue
    grep -qxF "$line" <<< "$table" || { note "registry has: $line"; drift=1; }
  done < <(grep -v '^#' "$EXTFILE")
  if [ "$drift" = 0 ]; then
    ok "crates/wc-wlcs/src/imp.rs's fallback table matches the registry"
  else
    bad "the fallback table in crates/wc-wlcs/src/imp.rs has drifted; update it"
  fi
}

# ---------------------------------------------------------------------------
# The suite
# ---------------------------------------------------------------------------
list() {
  echo "== list: what WLCS contains"
  [ -f "$SHIM" ] || { bad "$SHIM is not built (cargo build -p wc-wlcs)"; return; }
  local total
  "$WC_WLCS_RUNNER" "$SHIM" --gtest_list_tests > "$RUNDIR/tests.txt" 2>&1
  total=$(grep -c '^  ' "$RUNDIR/tests.txt")
  [ "$total" -gt 0 ] || { bad "the runner listed nothing"; return; }
  ok "$total tests in $(grep -cv '^  ' "$RUNDIR/tests.txt") suites"
  awk '/^[^ ]/{s=$1; next} {c[s]++} END{for(k in c) printf "%6d %s\n", c[k], k}' \
    "$RUNDIR/tests.txt" | sort -rn | head -20 | while read -r n s; do
    note "$(printf '%5d' "$n")  $s"
  done
}

# One WLCS invocation, with the log kept and the scoreboard printed.
#   $1 leg name   $2 gtest filter
run_suite() {
  local leg=$1 filter=$2
  local log="$OUT/$leg.log" json="$OUT/$leg.json"
  echo "  ....  running; this is not fast. log: $log"
  WC_WLCS_EXTENSIONS="$EXTFILE" \
    "$WC_WLCS_RUNNER" "$SHIM" \
      --gtest_filter="$filter" \
      --gtest_output="json:$json" \
      > "$log" 2>&1
  local rc=$?
  # gtest exits non-zero when tests fail, which is the expected state of a
  # compliance baseline. The run is only *broken* if it produced no verdicts.
  grep -qE '^\[==========\].*run\.' "$log" || {
    bad "$leg: the runner produced no verdicts (exit $rc); see $log"
    tail -5 "$log" | sed 's/^/        /'
    return 1
  }
  score "$log" "$leg"
}

headless() {
  local filter=${1:-*}
  echo "== headless: the Linux build, --control standing in for Win32"
  [ -f "$SHIM" ] || { bad "$SHIM is not built (cargo build -p wc-wlcs)"; return; }
  fresh "$LINUX_BIN/wc-compositor" "$LINUX_BIN/wc-compositor is not built" || return
  [ -f "$EXTFILE" ] || { bad "no $EXTFILE; run 'globals' first"; return; }
  export WC_WLCS_COMPOSITOR="$LINUX_BIN/wc-compositor"
  export WC_WLCS_RUNDIR="$RUNDIR"
  unset WC_WLCS_ARGS
  run_suite headless "$filter"
}

# The Wine leg. A real Windows binary, a real message loop, and wc-proxy doing
# the `h` translation — which is the whole half the headless leg cannot reach.
#
# It runs a **subset**, and the subset is not a judgement about which tests
# matter: `--control` is `#[cfg(unix)]`, so there is no way to move a pointer or
# place a window on this leg, and every test that needs one would fail for a
# reason that says nothing about Win32. Those are excluded by filter and listed
# as such. See §26.5.
wine_leg() {
  local filter=${1:-}
  echo "== wine: wc-compositor.exe, through wc-proxy"
  [ -f "$SHIM" ] || { bad "$SHIM is not built (cargo build -p wc-wlcs)"; return; }
  if [ ! -f "$WIN_BIN/wc-compositor.exe" ]; then
    bad "$WIN_BIN/wc-compositor.exe is not built"
    note "cargo build --workspace --target x86_64-pc-windows-gnu"
    note "a clippy run builds metadata, not an executable"
    return
  fi
  fresh "$LINUX_BIN/wc-proxy" "$LINUX_BIN/wc-proxy is not built" || return
  [ -f "$EXTFILE" ] || { bad "no $EXTFILE; run 'globals' first"; return; }
  command -v wine >/dev/null || { bad "no wine in this shell"; return; }
  command -v Xvfb >/dev/null || { bad "no Xvfb in this shell"; return; }

  # An X server, because there is no way around one. Measured, and it is why
  # `xvfb` is a dependency rather than a convenience: with no `DISPLAY`, Wine's
  # `CreateWindowExW` fails for **every** visible top-level window
  # (scripts/probe-window.c, docs/waycompose-design.md §8.5) — so a headless Wine cannot
  # run this leg at all, and every test would fail for a reason that is about
  # the rig.
  #
  # One server for the whole leg, not one per test: it costs a second to start
  # and there is one per test already in `wine explorer`.
  local display=":$(( 90 + (RANDOM % 8) ))"
  Xvfb "$display" -screen 0 1280x1024x24 -nolisten tcp >"$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  sleep 1
  if ! kill -0 "$XPID" 2>/dev/null; then
    bad "Xvfb $display did not start; see $RUNDIR/xvfb.log"
    return
  fi
  export DISPLAY="$display"
  ok "Xvfb on $display"

  # The filter. Every test that needs a pointer or a window position is
  # excluded, because on this leg there is nothing to answer either — see the
  # header, and `crate::control` for why. Excluded rather than run and failed:
  # a failure whose cause is the harness is worse than no result, and this
  # project has been bitten by exactly that (§13.6).
  local cannot='-*PointerCrossing*:*RegionSurfaceInputCombinations*:*SurfaceInputCombinations*:*ToplevelInputCombinations*:*TouchTest*:*XdgPopupPositionerTest*:*SubsurfaceTest*:*SubsurfaceMultilevelTest*:*XdgPopupTest*:SelfTest.*:*interactive*:*pointer*:*touch*:*window_under_cursor*'
  # And the whole justified/gap set as well, which the headless leg has already
  # established. **This is about the cost, and it is worth being explicit.**
  # WLCS builds a server before it discovers an extension is missing, so a
  # skipped test on this leg still starts a Wine virtual desktop, a Windows
  # binary and a proxy — about three seconds — and 373 of them would be twenty
  # minutes of wineserver churn on a machine ten worktrees share (§16.8), to
  # re-learn a fact that does not depend on the platform. A missing global is
  # missing in the same source on both legs.
  local elsewhere='*LayerSurface*:*LayerShell*:*V6*:WlShellSubsurfaces*:ForeignToplevel*:PrimarySelection*:GtkPrimarySelection*:TextInput*:VirtualPointer*:WpViewporter*:FractionalScale*:XdgOutputV1*:PointerConstraints*:RelativePointer*'
  local f="${filter:-*}$cannot:$elsewhere"
  note "filter: $f"

  export WC_WLCS_LAUNCH="$ROOT/scripts/wc-wlcs-wine.sh"
  export WC_WLCS_NO_CONTROL=1
  export WC_WLCS_RUNDIR="$RUNDIR"
  run_suite wine "$f"
  unset WC_WLCS_LAUNCH WC_WLCS_NO_CONTROL
}

# Where the two legs disagree, which is the measurement neither leg can make on
# its own.
#
# Anecdotes about Wine surprising somebody are not evidence; a table of the
# tests where a real `HWND` and a synthetic one reach different verdicts is.
# A test that passes headless and fails under Wine is either a Win32 bug or a
# Wine bug and worth a name; one that does the reverse means `crate::control`
# is being kinder than Windows, which is the more dangerous direction and the
# reason this mode exists at all.
compare() {
  echo "== compare: the same tests, on both legs"
  local h="$OUT/headless.log" w="$OUT/wine.log"
  [ -f "$h" ] || { bad "no $h; run 'headless' first"; return; }
  [ -f "$w" ] || { bad "no $w; run 'wine' first"; return; }
  # **`sub(/,$/, "", n)` is load-bearing, and it was found the hard way.** A
  # parameterised test that passes or fails prints
  # `Suite.test/3, where GetParam() = …`; the same test skipped prints
  # `Suite.test/3 (22ms)` with no suffix. So the name carries a trailing comma
  # in two verdict kinds out of three, and a join on the raw field silently
  # drops **every parameterised test whose verdict differs between the legs** —
  # which is precisely the set this mode exists to find, and which is 700 of
  # the 1,225. It showed up as a one-line `comm` mismatch when the same test
  # went SKIP on one run and FAIL on the next.
  local verdicts='
    function name(s) { sub(/,$/, "", s); return s }
    /^\[ +OK +\] .*\([0-9]+ ?ms\)$/     { print name($4), "PASS" }
    /^\[ +FAILED +\] .*\([0-9]+ ?ms\)$/ { print name($4), "FAIL" }
    /^\[ +SKIP +\] .*\([0-9]+ ?ms\)$/   { print name($4), "SKIP" }'
  awk "$verdicts" "$h" | LC_ALL=C sort > "$RUNDIR/h.v"
  awk "$verdicts" "$w" | LC_ALL=C sort > "$RUNDIR/w.v"
  LC_ALL=C join "$RUNDIR/w.v" "$RUNDIR/h.v" > "$RUNDIR/both.v"

  # A test present in one log and not the other is not a disagreement, but it
  # is not nothing either: it means the two legs ran different sets and the
  # filter has drifted. Counted separately rather than folded into either.
  local only
  only=$(LC_ALL=C join -v1 "$RUNDIR/w.v" "$RUNDIR/h.v" | wc -l)
  [ "$only" = 0 ] || note "$only test(s) ran on the wine leg only"

  local n d
  n=$(wc -l < "$RUNDIR/both.v")
  d=$(awk '$2 != $3' "$RUNDIR/both.v" | wc -l)
  if [ "$n" = 0 ]; then
    bad "the two legs share no tests; check the wine filter"
    return
  fi
  note "$n tests ran on both legs"
  if [ "$d" = 0 ]; then
    ok "both legs agree on all $n"
    note "so on this subset the synthetic Win32 in crate::control and a real"
    note "message pump reach the same verdict, and wc-proxy's in-band h argument"
    note "carried every buffer the suite sent"
  else
    bad "$d of $n disagree"
    awk '$2 != $3 {printf "        %-70s wine=%s headless=%s\n", $1, $2, $3}' "$RUNDIR/both.v"
  fi
}

# ---------------------------------------------------------------------------
# The scoreboard
# ---------------------------------------------------------------------------
#
# Parsed from gtest's console output rather than from its JSON, so that a run
# killed half way through still scores what it got. The JSON is written too, for
# anything that wants structure.
score() {
  local log=${1:-$OUT/headless.log} leg=${2:-$(basename "${1:-headless}" .log)}
  [ -f "$log" ] || { bad "no such log: $log"; return; }

  # gtest prints the same bracket tags twice: once per test with a duration,
  # and once again in the closing summary without one. Matching on the trailing
  # `(N ms)` is what tells them apart — a first attempt counted the summary too
  # and reported 1,437 verdicts for a suite of 1,225, which is the kind of
  # arithmetic that makes a scoreboard worthless.
  awk '
    /^\[ +OK +\] .*\([0-9]+ ?ms\)$/     { n = $4; sub(/\.[^.]*$/, "", n); p[n]++; P++; next }
    /^\[ +FAILED +\] .*\([0-9]+ ?ms\)$/ { n = $4; sub(/\.[^.]*$/, "", n); f[n]++; F++; next }
    /^\[ +SKIP +\] .*\([0-9]+ ?ms\)$/   { n = $4; sub(/\.[^.]*$/, "", n); s[n]++; S++; next }
    END {
      for (k in p) all[k]
      for (k in f) all[k]
      for (k in s) all[k]
      for (k in all) printf "%s %d %d %d\n", k, p[k], f[k], s[k]
      printf "TOTAL %d %d %d\n", P, F, S
    }' "$log" | sort > "$RUNDIR/score.txt"

  local tp tf ts
  read -r _ tp tf ts <<< "$(grep '^TOTAL ' "$RUNDIR/score.txt")"
  tp=${tp:-0}; tf=${tf:-0}; ts=${ts:-0}

  echo
  echo "  ---- $leg, against $(sed -n 's/^\[==========\] Running \([0-9]*\) tests.*/\1/p' "$log" | head -1) tests ----"
  printf "  %-52s %5s %5s %5s\n" SUITE pass fail skip
  grep -v '^TOTAL ' "$RUNDIR/score.txt" | while read -r suite p f s; do
    printf "  %-52s %5d %5d %5d\n" "$suite" "$p" "$f" "$s"
  done
  printf "  %-52s %5d %5d %5d\n" "-- all --" "$tp" "$tf" "$ts"
  echo

  # Why every skip happened, grouped by the extension WLCS named on the line
  # before it. A skip with no entry in the table is the dangerous case: it
  # looks like a pass in the totals and nobody decided anything about it.
  # `why` is reset at each `[ RUN ]`, so a test that skipped without printing a
  # reason cannot inherit the previous test's. WLCS's own
  # `SelfTest.xfail_failure_is_noted` is exactly that case — it prints "Reason
  # goes here" and skips — and letting it borrow a neighbour's extension would
  # be inventing an exclusion.
  awk '
    /^\[ +RUN +\]/                    { why = ""; next }
    /Missing extension: /             { e = $5; sub(/>=.*/, "", e); why = e; next }
    /^\[ +SKIP +\] .*\([0-9]+ ?ms\)$/ { c[why == "" ? "(no extension named)" : why]++; next }
    END { for (k in c) printf "%d %s\n", c[k], k }' "$log" | sort -rn > "$RUNDIR/skips.txt"

  local unexplained=0
  echo "  ---- why the skips, by the extension WLCS asked for ----"
  while read -r n ext; do
    [ -n "$ext" ] || continue
    local why; why=$(reason_for "$ext")
    if [ -z "$why" ]; then
      echo "  ????  $ext ($n) — nobody has said why"
      unexplained=$((unexplained + 1))
    else
      printf "  %-9s %-42s %4d\n            %s\n" "${why%% *}" "$ext" "$n" "${why#* }"
    fi
  done < "$RUNDIR/skips.txt"
  echo

  if [ "$tp" -gt 0 ]; then
    ok "$leg: $tp passed, $tf failed, $ts skipped"
  else
    bad "$leg: nothing passed"
  fi
  if [ "$unexplained" = 0 ]; then
    ok "$leg: every skip has a recorded reason"
  else
    bad "$leg: $unexplained missing extension(s) with no recorded reason"
  fi

  justified_failures "$log" "$leg"
}

# ---------------------------------------------------------------------------
# Justified *failures*, which are not the same thing as justified skips
# ---------------------------------------------------------------------------
#
# ADDED FOR §45, because the scoreboard could not say something true.
#
# A justified **skip** is easy: WLCS turns a missing global into a skip and
# prints which one, so `reason_for` can key a reason to it and the run is
# self-describing. That machinery only reaches failures caused by an *absent
# protocol*.
#
# The case it cannot reach is a test that runs, is answered, and gets a
# different answer here on purpose. `RegionSurfaceInputCombinations` with
# WLCS's `XdgStableSurfaceBuilder(12, 5, 20, 6)` probes the four edges of a
# buffer that lie outside the window geometry rectangle. Wayland says window
# geometry does not clip input; on this platform it does, because the `HWND` is
# the geometry rectangle and Windows delivers no message outside a window. See
# §45.4 for why that is a decision rather than a defect. Nothing is missing
# from the registry, so WLCS has no mechanism to declare it, and it fails.
#
# **The wrong answers, both of which were considered.** Adding the extension to
# the table so the tests skip would be a lie of exactly the kind §26 exists to
# prevent — the protocol *is* implemented. Filtering the tests out of the run
# would be worse, because then nothing counts them at all. So they stay in the
# failed column, and this table says which of them are expected to be there.
#
# THE PART THAT MATTERS: THIS TABLE VALIDATES ITSELF
#
# §42 found twelve gates that were vacuous — a guard that could never fire
# looks exactly like a guard that always passes. A table of "expected
# failures" is the single easiest thing in a harness to make vacuous, in two
# different ways, and both are checked:
#
#   * **A listed test that PASSES is an error.** The reason has expired: either
#     somebody fixed it, or the pattern was wrong from the start. Silently
#     accepting a pass would let this table hide a *regression's* absence and,
#     worse, let a stale entry sit forever.
#   * **A listed pattern that matches NO test is an error.** That is the
#     vacuous case: a typo, or a test WLCS renamed, and the entry then excuses
#     nothing while looking like it excuses something.
#
# So the only way an entry stays quiet is if it matches at least one test and
# every test it matches actually failed.
justified_failure_patterns() {
  # <pattern><TAB><reason>. Matched against the full gtest name with `case`, so
  # `*` is a glob rather than a regex.
  #
  # **Written out one test at a time, deliberately.** The tempting entry is
  # `*RegionSurfaceInputCombinations.*/6`, and it is wrong twice over. The
  # parameter index means different things in different instantiations — only
  # `DefaultEdges` and `MultiRectEdges` are built over `all_surface_types`, so
  # only there does index ≡ 6, 7 (mod 12) select the inset builder; in
  # `FullSurface` and the rest, index 6 is an entirely different surface. And
  # even within one instantiation only the *positive* assertions fail: a test
  # that checks input is **not** seen passes trivially here, because nothing is
  # delivered at all. A pattern is a claim about which tests are excused, and a
  # loose one excuses tests nobody examined.
  #
  # The first draft of this table used the loose pattern. The self-check below
  # rejected it on its first run, naming ten tests that passed while being
  # listed as expected failures. That is the check earning its place before the
  # table it guards was even finished.
  local why='input outside the window geometry rectangle: the HWND *is* that rectangle, and Windows delivers no message outside a window (§45.4)'
  local t
  for t in 6 7 18 19 30 31 42 43; do
    printf 'DefaultEdges/RegionSurfaceInputCombinations.input_inside_region_seen/%s\t%s\n' "$t" "$why"
  done
  for t in 6 7 18 19 42 43 54 55; do
    printf 'MultiRectEdges/RegionSurfaceInputCombinations.input_inside_region_seen/%s\t%s\n' "$t" "$why"
  done
  for t in input_hits_parent_after_falling_through_subsurface \
           input_seen_after_dragged_off_surface \
           input_seen_after_surface_unmapped_and_remapped; do
    printf 'SurfaceInputRegions/SurfaceInputCombinations.%s/6\t%s\n' "$t" "$why"
    printf 'SurfaceInputRegions/SurfaceInputCombinations.%s/7\t%s\n' "$t" "$why"
  done
}

justified_failures() {
  local log=$1 leg=$2
  local verdicts='
    function name(s) { sub(/,$/, "", s); return s }
    /^\[ +OK +\] .*\([0-9]+ ?ms\)$/     { print name($4), "PASS" }
    /^\[ +FAILED +\] .*\([0-9]+ ?ms\)$/ { print name($4), "FAIL" }'
  awk "$verdicts" "$log" | LC_ALL=C sort -u > "$RUNDIR/jf.v"
  [ -s "$RUNDIR/jf.v" ] || return 0

  rm -f "$RUNDIR/jf.by-reason"
  echo "  ---- failures this platform expects, and why ----"
  local bad_table=0 total=0
  while IFS=$'\t' read -r pattern reason; do
    [ -n "$pattern" ] || continue
    local matched=0 wrongly_passed=""
    while read -r test verdict; do
      case "$test" in
        $pattern)
          matched=$((matched + 1))
          [ "$verdict" = PASS ] && wrongly_passed="$wrongly_passed $test"
          ;;
      esac
    done < "$RUNDIR/jf.v"

    if [ "$matched" = 0 ]; then
      # The vacuous case. An entry that matches nothing excuses nothing.
      echo "  ????  '$pattern' matches no test in this run — stale entry"
      bad_table=$((bad_table + 1))
      continue
    fi
    if [ -n "$wrongly_passed" ]; then
      echo "  ????  '$pattern' is listed as expected-to-fail but these PASSED:"
      for t in $wrongly_passed; do echo "          $t"; done
      echo "        the reason has expired; remove the entry"
      bad_table=$((bad_table + 1))
      continue
    fi
    total=$((total + matched))
    # Grouped by reason rather than one line per pattern: the table is written
    # one test at a time so that each entry is an auditable claim, but a reader
    # wants the *causes* and how many tests each accounts for.
    printf '%d\t%s\n' "$matched" "$reason" >> "$RUNDIR/jf.by-reason"
  done < <(justified_failure_patterns)
  if [ -s "$RUNDIR/jf.by-reason" ]; then
    awk -F'\t' '{n[$2] += $1} END {for (r in n) printf "  %-9s %4d  %s\n", "EXPECTED", n[r], r}' \
      "$RUNDIR/jf.by-reason" | sort -k2 -rn
  fi
  echo

  if [ "$bad_table" = 0 ]; then
    ok "$leg: $total of the failures are expected on this platform, and the rest are debts"
  else
    bad "$leg: $bad_table entr(y/ies) in the expected-failure table no longer describe this run"
  fi
}

case "${1:-all}" in
  globals)  globals ;;
  list)     list ;;
  headless) headless "${2:-*}" ;;
  wine)     wine_leg "${2:-}" ;;
  compare)  compare ;;
  score)    score "${2:-$OUT/headless.log}" ;;
  all)      globals; list; headless "${2:-*}"; wine_leg; compare ;;
  *) echo "usage: $0 {globals|list|headless [FILTER]|wine [FILTER]|compare|score [LOG]|all}"; exit 2 ;;
esac

echo
echo "  $pass passed, $fail failed, $skip skipped"
[ "$fail" = 0 ]
