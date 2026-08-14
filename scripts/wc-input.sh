#!/usr/bin/env bash
# Prove the stage-6 milestone: **a third-party Wayland client that can be typed
# into and clicked on**, with the keystrokes coming from Win32.
#
#   scripts/wc-input.sh linux    the seat and the keymap, against Weston clients
#   scripts/wc-input.sh pointer  §37: *where* a click lands, as a number
#   scripts/wc-input.sh wine     real WM_KEYDOWN, through the proxy, into a client
#   scripts/wc-input.sh arena    the same, with the keymap cut from the arena
#                                and no shared directory at all (stage 10)
#   scripts/wc-input.sh all      all of them
#
# Run from inside `nix develop`.
#
# WHY THE EVIDENCE IS IN TWO LEGS, AGAIN
#
# The same split docs/waycompose-design.md §8.5 drew for pixels, for the same structural
# reason and with a different line through it. Neither leg is sufficient and
# saying which half each carries matters more than either one passing.
#
#   * **linux.** `weston-eventdemo` — a Weston demo client that prints every
#     input event it receives, **including the Unicode character its own
#     libxkbcommon produced from our keycode and our keymap** — connects
#     directly. This is the strongest statement available about the seat: not
#     "an event was sent" but "a program that has never heard of this project
#     decoded it and got the right letter". What it cannot say anything about is
#     the *translation*, because there is no `WM_KEYDOWN` here.
#
#   * **wine.** `wc-compositor.exe` in a Wine virtual desktop, `wc-proxy`
#     beside it, `weston-eventdemo` against the proxy, and `probe-input.exe`
#     injecting with `SendInput` — so a keystroke starts as a scan code in the
#     desktop's input queue and ends as a character in a Linux client. This is
#     the half that exercises `crates/wc-compositor/src/input.rs`, the focus
#     messages, `TrackMouseEvent` and `SetCapture`.
#
# THE FD RULE, WHICH THIS STAGE TESTS IN THE OTHER DIRECTION
#
# Stage 4 moved a client-created object (a `wl_shm` pool) to the compositor.
# `wl_keyboard.keymap` is the reverse: the **compositor** creates the object and
# the client opens it. A keymap in a `Local\wc-…` section is unopenable by a
# Linux process, which is why §9.10 item 5 said "implemented, unexercised" — so
# the wine leg asserts on *which kind of object* carried the keymap, not merely
# that a keymap arrived. A run that smuggled a descriptor across would produce
# identical characters and fail that check.
#
# WHAT WINE DOES AND DOES NOT PROVE. Unchanged and still the most important
# sentence in the file: Wine is not Windows, and it has been wrong four times on
# this project. Input injection is a *worse* case than most, because `SendInput`
# reaches into the window manager and Wine's is not Windows'. A pass says the
# chain is self-consistent and structurally possible; `scripts/wc-guest.sh` is
# the arbiter, and §11.11 lists what has to be re-checked there.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }

# STAGE 42. `fresh` / `fresh_or_skip`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"

# STAGE 41. No QEMU on this box, so no guest RAM to locate: this harness is the
# Wine fixture and its `arena` mode is a file. `wc_substrate_declare
# wine-fixture` is what hands it the `SV_ALLOW_FILE_ARENA=1` that `wc-proxy` and
# `wc-compositor` now require, and it refuses to hand that out to any harness
# that has a guest.
. "$(dirname "$0")/wc-substrate.sh"
wc_substrate_declare wine-fixture \
  "no QEMU on this box: a Wine run has no guest RAM to locate" || exit 2

RUNDIR=$(mktemp -d /tmp/wc-input-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"

# What `weston-eventdemo` must be asked for. Every log is off by default, and a
# run without these flags is silent and looks like a client that received
# nothing — which is exactly the failure this harness exists to detect, so the
# flags are in one place rather than repeated.
EVENTDEMO_FLAGS=(--log-key --log-button --log-motion --log-axis --log-focus)

# ---------------------------------------------------------------------------
# linux: the seat, against clients that cannot share our bugs
# ---------------------------------------------------------------------------

run_linux() {
  echo "== the seat, against unmodified Weston clients =="
  fresh "$LINUX_BIN/wc-compositor" "cargo build -p wc-compositor" || return

  local dir="$RUNDIR/linux"
  mkdir -p "$dir/rt"; chmod 700 "$dir/rt"

  # --- wayland-info: is the seat well-formed at all ----------------------
  #
  # First, because everything below is meaningless if the advertisement is
  # wrong, and because `wayland-info` is a tool from wayland-utils whose whole
  # job is to say what a compositor claims.
  if command -v wayland-info >/dev/null; then
    XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" --socket wl-in6-info --once --timeout 15 \
      >"$dir/info-srv.log" 2>&1 &
    local srv=$!
    sleep 1
    ( cd "$dir" && XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-in6-info \
      timeout 10 wayland-info ) >"$dir/info.log" 2>&1
    wait "$srv" 2>/dev/null

    grep -q "interface: 'wl_seat'" "$dir/info.log" \
      && ok "wayland-info sees a wl_seat" \
      || { bad "wayland-info found no wl_seat"; sed 's/^/        /' "$dir/info.log" | head -20; }
    # STAGE 20 added `touch`, which this leg always has because
    # `headless::has_touch` is true — the scripted gesture is a real source of
    # the events. On Windows the same line reads `pointer keyboard` on a machine
    # with no digitiser, which is why the wire check further down accepts both.
    grep -q "capabilities: pointer keyboard touch" "$dir/info.log" \
      && ok "the seat advertises all three capabilities: $(grep -o 'capabilities: .*' "$dir/info.log")" \
      || bad "wrong capabilities: $(grep -o 'capabilities: .*' "$dir/info.log")"
    # repeat_info is a v4 event and its absence is silent — a client simply
    # never repeats, which reads as a stuck keyboard rather than as a missing
    # event.
    grep -q "keyboard repeat rate: 30" "$dir/info.log" \
      && grep -q "keyboard repeat delay: 500" "$dir/info.log" \
      && ok "repeat_info arrived: $(grep -o 'repeat rate: [0-9]*' "$dir/info.log"), delay 500 ms" \
      || bad "no repeat_info: $(grep -o 'repeat.*' "$dir/info.log" | tr '\n' ' ')"
    grep -q "name: seat0" "$dir/info.log" \
      && ok "the seat is named seat0" || bad "the seat has no name"
  else
    note "SKIP wayland-info (not installed)"
  fi

  # --- weston-eventdemo: what a real client actually decodes --------------
  if ! command -v weston-eventdemo >/dev/null; then
    note "SKIP weston-eventdemo (not installed) -- this is the leg that matters"
    return
  fi

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" --socket wl-in6 --simulate-wm --once \
    --timeout 25 --expect-presents 1 >"$dir/srv.log" 2>&1 &
  local srv=$!
  sleep 1
  # `WAYLAND_DEBUG=1` as well as the client's own logging: libwayland then
  # decodes and prints every event it received, which is a second, independent
  # reader of the same bytes. The client's own printout says "I understood
  # this"; libwayland's says "this is what was on the wire".
  ( cd "$dir" && XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-in6 WAYLAND_DEBUG=1 \
    timeout 20 weston-eventdemo "${EVENTDEMO_FLAGS[@]}" ) >"$dir/cli.log" 2>&1
  local rc=$?
  wait "$srv" 2>/dev/null
  local srv_rc=$?

  [ "$srv_rc" = 0 ] && ok "the compositor exited 0 with the client gone" \
                    || bad "the compositor exited $srv_rc"
  check_client_events "$dir/cli.log" eventdemo

  # STAGE 20, and it belongs to this leg rather than to `check_client_events`:
  # only `--simulate-wm` drives a touch stream, so only this leg may insist one
  # happened. A capability advertised with nothing behind it is the exact
  # failure this stage set out not to commit.
  grep -q "wl_touch@[0-9]*\.down(" "$dir/cli.log.clean" \
    && ok "eventdemo: --simulate-wm drove a wl_touch stream a client decoded" \
    || bad "eventdemo: touch is advertised and no wl_touch.down ever reached a client"

  # --- the layout change, proved from the client's side ------------------
  #
  # On Windows this is `WM_INPUTLANGCHANGE`; here `--simulate-wm` drives it.
  # The check is the one thing about it a client can observe: **the same evdev
  # keycode producing a different character**. Keycode 21 is the position of `Y`
  # on an American board and of `Z` on a German one, so `us` must give `y` (121)
  # and `de` must give `z` (122).
  #
  # A second `wl_keyboard.keymap` event alone would not do: a compositor that
  # re-sent the *same* keymap would produce one, and so would a client that
  # ignored it. Only the character changing says the new keymap crossed the
  # boundary, was opened from its name, and was compiled by the client's own
  # libxkbcommon.
  local probes
  probes=$(grep -o "^key key: 21, unicode: [0-9]*, state: pressed" "$dir/cli.log.clean" \
           | awk -F'unicode: ' '{print $2}' | awk -F',' '{print $1}' | tr '\n' ' ')
  if [ "$probes" = "121 122 " ]; then
    ok "eventdemo: keycode 21 gave 'y' then 'z' across the layout change — a second keymap really crossed"
  else
    bad "eventdemo: keycode 21 gave [$probes], expected 121 then 122"
  fi
  # **§31: the sizes are checked, not recited.** This was `ok "keymaps
  # delivered, in bytes: $sizes"`, fired unconditionally — it passed on two
  # keymaps, on one, and on none, because the only thing it did with the numbers
  # was print them.
  #
  # Two things they can say. There must be **two** of them, because the test
  # above changes the layout and a second keymap is the whole subject; and each
  # must be a plausible keymap rather than a truncated one. Measured at 35,572
  # and 37,532 bytes for `us` and `de` on this machine, so 1 KiB is a floor with
  # thirty-five times the margin, and a zero-length or header-only file — which
  # is what a failed `mmap` or a short write on the server side produces, and
  # which the client would then fail to compile with no size in the message —
  # is what it excludes.
  local sizes count small
  sizes=$(grep -o "wl_keyboard@[0-9]*\.keymap(1, fd [0-9]*, [0-9]*)" "$dir/cli.log.clean" \
          | grep -o "[0-9]*)" | tr -d ')' | tr '\n' ' ')
  count=$(echo "$sizes" | wc -w)
  small=$(echo "$sizes" | tr ' ' '\n' | awk '$1 != "" && $1 < 1024' | wc -l)
  if [ "$count" -ne 2 ]; then
    bad "eventdemo: $count keymap(s) delivered, expected 2 (one per layout): [$sizes]"
  elif [ "$small" -ne 0 ]; then
    bad "eventdemo: $small of the keymaps is under 1 KiB and cannot be a real one: [$sizes]"
  else
    ok "eventdemo: two keymaps delivered, both a plausible size, in bytes: $sizes"
  fi
}

# STAGE 20. What a `wl_touch` stream must look like on the wire.
#
# Read out of libwayland's own decode rather than out of anything the client
# printed, because `weston-eventdemo` has no touch logging flag: the toytoolkit
# binds `wl_touch` and handles the events, and says nothing about them. So the
# arbiter here is one step weaker than it is for the keyboard — "libwayland
# decoded these events and the client did not disconnect in protocol error" and
# not "the client told us what it made of them". That is the honest strength of
# this leg and is stated in docs/waycompose-design.md §24.6.
#
# Silent when no touch events were driven, because both legs call this and only
# the one with `--simulate-wm` drives any; the caller that expected them asserts
# that separately.
check_touch_events() {
  local log=$1 tag=$2 caps=$3
  # `-v " -> "` drops the client's own requests, so `wl_touch.release` at
  # shutdown is not mistaken for an event the compositor sent.
  local seq
  seq=$(grep -v ' -> ' "$log" | grep -o "wl_touch@[0-9]*\.[a-z_]*(" \
        | sed 's/.*\.//; s/(//' | tr '\n' ' ')
  if [ -z "$seq" ]; then
    [ "$caps" = 7 ] && note "$tag: touch advertised, no touch events driven on this leg"
    return 0
  fi
  [ "$caps" = 7 ] || bad "$tag: wl_touch events arrived but capabilities said $caps"

  # **Two contacts, and both in one frame.** The single fact that distinguishes
  # a touch implementation from a mouse pretending to be one.
  case "$seq" in
    "down down frame"*) ok "$tag: two contacts landed together and closed one frame" ;;
    *) bad "$tag: the stream did not open with two contacts and a frame: ${seq:0:60}" ;;
  esac

  # **Nothing is left unframed.** A client is entitled to hold everything since
  # the last frame, so a trailing event with no frame after it is a touch the
  # client may never act on. `cancel` is the documented exception — the protocol
  # says outright that no frame is required after it.
  local unframed
  unframed=$(printf '%s' "$seq" | sed 's/.*frame //')
  case "$unframed" in
    "" | "cancel ") ok "$tag: every touch event is inside a closed frame" ;;
    *) bad "$tag: touch events after the last frame: '$unframed'" ;;
  esac

  # **The ids did not swap.** Two fingers moving apart: contact 0 must end left
  # of where it started and contact 1 right of it. A compositor that renumbered
  # contacts, or keyed them on Windows' pointer ids and let one be reused, makes
  # the two converge — which is visible here and in no other check.
  local x0f x0l x1f x1l
  x0f=$(grep -o "\.motion([0-9]*, 0, [0-9]*\." "$log" | head -1 | awk -F', ' '{print $3}' | tr -d '.')
  x0l=$(grep -o "\.motion([0-9]*, 0, [0-9]*\." "$log" | tail -1 | awk -F', ' '{print $3}' | tr -d '.')
  x1f=$(grep -o "\.motion([0-9]*, 1, [0-9]*\." "$log" | head -1 | awk -F', ' '{print $3}' | tr -d '.')
  x1l=$(grep -o "\.motion([0-9]*, 1, [0-9]*\." "$log" | tail -1 | awk -F', ' '{print $3}' | tr -d '.')
  if [ -n "$x0f" ] && [ -n "$x1f" ] && [ "$x0l" -lt "$x0f" ] && [ "$x1l" -gt "$x1f" ]; then
    ok "$tag: the contacts kept their ids across the pinch (0: $x0f->$x0l, 1: $x1f->$x1l)"
  else
    bad "$tag: the contacts did not diverge (0: $x0f->$x0l, 1: $x1f->$x1l) — ids were swapped or reused"
  fi

  grep -q "\.up([0-9]*, [0-9]*, 0)" "$log" && grep -q "\.up([0-9]*, [0-9]*, 1)" "$log" \
    && ok "$tag: both contacts were released by id" \
    || bad "$tag: a contact was never released; its id is stuck down in the client"

  # `cancel` is the event a compositor is likeliest never to send, because
  # nothing a user does produces one. It is also the one smithay was dropping
  # (vendor/PATCHES.md, stage 20), and this line is what caught it.
  grep -q "wl_touch@[0-9]*\.cancel()" "$log" \
    && ok "$tag: wl_touch.cancel reached the client" \
    || bad "$tag: no cancel; a client would hold the cancelled contact for ever"

  # The conditional half. One contact in the scripted gesture has a measured
  # patch and the rest have none, so exactly one of each must appear — a
  # compositor that manufactured an ellipse for every contact would show six.
  local shapes orients
  shapes=$(grep -c "wl_touch@[0-9]*\.shape(" "$log")
  orients=$(grep -c "wl_touch@[0-9]*\.orientation(" "$log")
  if [ "$shapes" = 1 ] && [ "$orients" = 1 ]; then
    ok "$tag: shape and orientation sent for the one contact that has geometry, and no other"
  else
    bad "$tag: $shapes shape and $orients orientation events; expected exactly one of each"
  fi
}

# Everything a `weston-eventdemo` log must contain, whichever side it ran on.
# One function, called by both legs, so that the wine leg is held to exactly the
# standard the linux leg is and a weaker check cannot creep into the harder
# case.
# `keyboard` as a third argument means the pointer assertions are skipped, which
# is only ever right for the `--post` diagnostic — see `probe-input.c` for why
# a posted mouse message cannot be faithful.
check_client_events() {
  local raw=$1 tag=$2 scope=${3:-all}
  local log="$raw.clean"

  if [ ! -s "$raw" ]; then
    bad "$tag: the client logged nothing at all"
    return 1
  fi
  # `WAYLAND_DEBUG=1` output is coloured and spells object ids with `#` in
  # libwayland 1.22 and later and with `@` before it. Normalising here rather
  # than writing greps that tolerate both keeps the assertions readable and
  # means a future spelling change is one line.
  sed -e 's/\x1b\[[0-9;]*m//g' -e 's/#\([0-9]\)/@\1/g' "$raw" >"$log"

  # --- the wire, as libwayland decoded it -------------------------------
  # `capabilities` is a bitmask: 1 pointer, 2 keyboard, 4 touch. **Both 3 and 7
  # are correct answers and which one is right is a fact about the machine**,
  # not about this code: stage 20 made the touch bit conditional on there being
  # a digitiser (`SM_MAXIMUMTOUCHES` on Windows), so a laptop without a
  # touchscreen must say 3 and saying 7 there would be the compositor promising
  # events it cannot produce. The check therefore reports which was seen rather
  # than demanding one, and fails on anything else — a missing keyboard bit is
  # a 1, and that is the failure worth catching here.
  local caps
  caps=$(grep -o "wl_seat@[0-9]*\.capabilities([0-9]*)" "$log" | head -1 \
         | grep -o "([0-9]*)" | tr -d '()')
  case "$caps" in
    3) ok "$tag: wl_seat.capabilities(3) = pointer|keyboard; no touch digitiser on this machine" ;;
    7) ok "$tag: wl_seat.capabilities(7) = pointer|keyboard|touch" ;;
    "") bad "$tag: no capabilities event on the wire" ;;
    *) bad "$tag: wl_seat.capabilities($caps), which is neither pointer|keyboard nor pointer|keyboard|touch" ;;
  esac
  # format 1 is XKB_V1; the size is the keymap's, and the fd is a real
  # descriptor libwayland received. On the wine leg that fd was manufactured on
  # this side of the boundary from a *name*, which is the whole point.
  local keymap
  keymap=$(grep -o "wl_keyboard@[0-9]*\.keymap(1, fd [0-9]*, [0-9]*)" "$log" | head -1)
  [ -n "$keymap" ] \
    && ok "$tag: the client received a keymap: $keymap" \
    || bad "$tag: no wl_keyboard.keymap reached the client"
  grep -q "wl_keyboard@[0-9]*\.repeat_info(30, 500)" "$log" \
    && ok "$tag: repeat_info(30, 500) — the client owns the repeat timer, not us" \
    || bad "$tag: no repeat_info on the wire"
  grep -q "wl_keyboard@[0-9]*\.enter(" "$log" \
    && ok "$tag: wl_keyboard.enter — focus followed the platform" \
    || bad "$tag: the keyboard never entered a surface"
  if [ "$scope" != keyboard ]; then
    grep -q "wl_pointer@[0-9]*\.enter(" "$log" \
      && ok "$tag: wl_pointer.enter" || bad "$tag: the pointer never entered"
    # The leave is the one that needs `TrackMouseEvent` on Win32 and is the
    # classic omission, so it is checked on the wire rather than inferred.
    grep -q "wl_pointer@[0-9]*\.leave(" "$log" \
      && ok "$tag: wl_pointer.leave — the pointer left, so the platform noticed" \
      || bad "$tag: no wl_pointer.leave (on Win32 this means TrackMouseEvent never armed)"
    grep -q "wl_pointer@[0-9]*\.frame()" "$log" \
      && ok "$tag: wl_pointer.frame — events are grouped as the protocol requires" \
      || bad "$tag: no frame events; clients batching on frame would stall"
  fi

  check_touch_events "$log" "$tag" "$caps"

  # --- what the client made of it ---------------------------------------
  #
  # The decisive check. `weston-eventdemo` prints the Unicode codepoint **its
  # own libxkbcommon** produced by running the keymap we sent over the keycode
  # we sent. A wrong keycode, a wrong keymap, a wrong evdev/xkb offset or a
  # keymap that failed to cross all produce a different letter or none, and
  # none of them produce an error anywhere.
  #
  # Measured while writing this, and worth keeping: the first version of the
  # compositor handed smithay an evdev keycode where it wanted an XKB one, and
  # this line printed "]9aaqt" for a run that typed "hello". Nothing else in
  # the harness noticed.
  local typed
  typed=$(grep -o "^key key: [0-9]*, unicode: [0-9]*, state: pressed" "$log" \
          | awk -F'unicode: ' '{print $2}' | awk -F',' '{print $1}' \
          | while read -r u; do
              [ "$u" -ge 32 ] && [ "$u" -lt 127 ] && printf "%b" "$(printf '\\%03o' "$u")"
            done)
  case "$typed" in
    hello*) ok "$tag: the client decoded the keystrokes as \"$typed\"" ;;
    *)      bad "$tag: the client decoded \"$typed\", which does not start with \"hello\""
            grep "^key key:" "$log" | head -8 | sed 's/^/        /' ;;
  esac

  # **The modifier.** `Z` is `KEY_Z` with shift held, and it is the one place
  # the compositor's *own* xkb state is load-bearing: it tracks which modifiers
  # are down, tells the client with `wl_keyboard.modifiers`, and the client
  # applies them. A compositor that forwarded the keycodes and never sent a
  # `modifiers` event would produce a lower-case `z` here — and every other
  # assertion in this file would still pass.
  case "$typed" in
    *Z*) ok "$tag: shift + KEY_Z decoded as an upper-case Z — the modifier crossed" ;;
    *)   bad "$tag: no upper-case Z in \"$typed\"; the shift did not reach the client"
         grep "^key key:" "$log" | tail -8 | sed 's/^/        /' ;;
  esac
  # And on the wire: a `modifiers` event with a non-zero *depressed* mask. Bit 0
  # is Shift in the mask the keymap defines, which is why this is checked as
  # "not zero" rather than against a number the keymap chose.
  local mods
  mods=$(grep -o "wl_keyboard@[0-9]*\.modifiers([0-9]*, [0-9]*," "$log" \
         | awk -F', ' '{print $2}' | sort -u | tr '\n' ' ')
  case "$mods" in
    *[1-9]*) ok "$tag: wl_keyboard.modifiers carried a depressed mask (values seen: $mods)" ;;
    *)       bad "$tag: every modifiers event had an empty mask (values seen: $mods)" ;;
  esac

  if [ "$scope" = keyboard ]; then
    note "$tag: pointer assertions skipped — --post cannot inject a faithful mouse"
    return 0
  fi

  # BTN_LEFT is 0x110 = 272, and it is a Linux evdev button code rather than
  # anything Win32 has a name for. A compositor forwarding Win32's own numbering
  # would send 1 here and every client would ignore it.
  grep -q "^button time: [0-9]*, button: 272, state: pressed" "$log" \
    && ok "$tag: a left click arrived as evdev BTN_LEFT (272)" \
    || { bad "$tag: no BTN_LEFT press"; grep "^button" "$log" | head -4 | sed 's/^/        /'; }
  grep -q "^button time: [0-9]*, button: 272, state: released" "$log" \
    && ok "$tag: and its release" || bad "$tag: the press had no release"

  grep -q "^motion time:" "$log" \
    && ok "$tag: pointer motion: $(grep -m1 '^motion time:' "$log")" \
    || bad "$tag: no motion"

  # **The sign of the scroll**, which is the assertion this whole block exists
  # for. Windows counts a notch rolled *away from the user* as `+WHEEL_DELTA`;
  # Wayland's vertical axis is positive **downwards**, in the same space as
  # motion, and rolling away scrolls the content up. So the compositor must
  # invert it, and the correct value here is **negative**.
  #
  # A `+10` would be a mouse wheel that scrolls backwards in every application
  # — the most reported and least diagnosable input bug there is — and no
  # "did scrolling happen" check catches it. Both legs produce this same
  # gesture so that neither platform can be wrong on its own.
  local axis
  axis=$(grep -m1 "^axis time: [0-9]*, axis: vertical" "$log")
  case "$axis" in
    *"value: -10.000000") ok "$tag: a notch away from the user became -10 (up), the right sign" ;;
    "")                   bad "$tag: no vertical axis event" ;;
    *)                    bad "$tag: wrong axis value: $axis" ;;
  esac
  # STAGE 42. The sign check immediately above is a proper `case` with two `bad`
  # arms; this one had a `note` on its failing side and so could not fail. A
  # smooth scroll where a discrete detent was sent is a real defect and this is
  # the only thing watching for it.
  local discrete
  discrete=$(grep -m1 '^axis discrete' "$log")
  case "$discrete" in
    "axis discrete axis: 0 value: -1")
      ok "$tag: and it was one discrete detent, not a smooth scroll" ;;
    "")
      bad "$tag: no discrete axis event at all -- the scroll arrived as a smooth axis" ;;
    *)
      bad "$tag: wrong discrete axis event: $discrete" ;;
  esac
  return 0
}

# ---------------------------------------------------------------------------
# wine: real Win32 messages, through the proxy, into a Linux client
# ---------------------------------------------------------------------------

find_socket() {
  local name=$1 node=""
  for _ in $(seq 1 250); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$name" 2>/dev/null | head -1)
    [ -n "$node" ] && { echo "$node"; return 0; }
    sleep 0.2
  done
  return 1
}

start_x() {
  Xvfb ":$1" -screen 0 1280x1024x24 -nolisten tcp >"$RUNDIR/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$1"
  for _ in $(seq 1 100); do
    xwd -root -silent >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

# STAGE 10 gave this leg a parameter, and it is the only difference between the
# two runs of it: **what carried the keymap**.
#
#   run_wine shared-file   the stage-6 answer. `--shared-dir` puts the keymap in
#                          a directory both sides can name, and the client opens
#                          a path.
#   run_wine arena         the stage-10 answer. The compositor cuts the keymap
#                          out of its own window of the arena and sends three
#                          integers; **there is no shared directory at all**.
#
# Everything else is held identical on purpose — the same client, the same
# injected keystrokes, the same `check_client_events` — because the claim being
# made is that the transport changed and nothing else did. A weaker check on the
# new leg would prove nothing, and a stronger one would be measuring the check
# rather than the transport.
run_wine() {
  local mode=${1:-shared-file}
  echo "== real WM_KEYDOWN, across the boundary, into an unmodified client (keymap by $mode) =="
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build -p wc-compositor --target x86_64-pc-windows-gnu" || return
  fresh "$LINUX_BIN/wc-proxy" "cargo build -p wc-proxy" || return
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }
  command -v Xvfb >/dev/null || { bad "Xvfb not on PATH"; return; }
  command -v weston-eventdemo >/dev/null || { note "SKIP (weston-eventdemo not installed)"; return; }
  command -v x86_64-w64-mingw32-gcc >/dev/null || { bad "no mingw gcc -- run inside nix develop"; return; }

  local dir="$RUNDIR/wine-$mode"
  mkdir -p "$dir/rt" "$dir/pools" "$dir/keymaps"
  chmod 700 "$dir/rt"

  # **`-mwindows`, and it is load-bearing rather than tidiness.** A console
  # subsystem binary launched inside a Wine virtual desktop gets a
  # `WineConsoleClass` window, and that window **takes the foreground** — so
  # `SetForegroundWindow(compositor)` is refused, every injected keystroke goes
  # to the console, and the compositor logs nothing. Measured exactly that: the
  # probe found the window, attached its input queue, and reported
  # `foreground=… class 'WineConsoleClass'`. Building for the windows subsystem
  # means no console exists to steal it, which is why `probe-input.c` writes to
  # `--log` rather than to a redirect.
  if x86_64-w64-mingw32-gcc -O1 -mwindows -o "$dir/probe-input.exe" "$ROOT/scripts/probe-input.c" \
       -luser32 2>"$dir/cc.log"; then
    ok "linked probe-input.exe ($(stat -c %s "$dir/probe-input.exe") bytes)"
  else
    bad "could not link probe-input.exe"; sed 's/^/        /' "$dir/cc.log"; return
  fi

  start_x 95 || { bad "could not start Xvfb"; return; }
  ok "Xvfb up on $DISPLAY"

  local sock="wl-in6-$mode"
  local desktop="wc-in-$mode"
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  # The two answers to "where does an object the compositor made for a client
  # go", as flags.
  #
  #   shared-file: `--shared-dir` is the stage-6 flag — a directory both sides
  #     can name, with its peer spelling. Without it the keymap is a
  #     `Local\wc-…` section and `wc-proxy` refuses the connection with a
  #     diagnosis, which is itself checked by a unit test.
  #
  #   arena: `--arena-provide` opens the region read-write on the compositor's
  #     side (under Wine there is no hypervisor, so the substrate is a file —
  #     see `wc_handle::arena::provide_file`) and `--arena-outbound` says how
  #     many of its bytes are the compositor's to hand out. **No `--shared-dir`
  #     at all**, which is the point of the leg: nothing the client opens has a
  #     name on any filesystem.
  local comp_flags=() proxy_flags=()
  # 32 MiB, and the number was measured rather than chosen. `weston-eventdemo`
  # asks for a **4190208-byte** pool for its cursor theme, on top of its own
  # 800 KiB surface, and resizes it repeatedly; a 4 MiB arena ran out, the
  # proxy reported `no run of 4190208 bytes`, and the client exited before a
  # single keystroke was injected. That is docs/waycompose-design.md §12.13 item 5 —
  # "64 MiB is a guess" — arriving in the smallest possible client, and it is
  # worth knowing that the guess has to be generous.
  local arena_bytes=$((32 * 1024 * 1024)) out_bytes=$((1024 * 1024))
  if [ "$mode" = arena ]; then
    # The proxy creates the region if it is not there, and it must exist before
    # the compositor opens it read-write — so it is made here, once, by the side
    # that owns its size.
    head -c "$arena_bytes" /dev/zero >"$dir/arena"
    comp_flags=(--arena-provide "0:$arena_bytes:$(winepath -w "$dir/arena" 2>/dev/null || echo "$dir/arena")"
                --arena-outbound "0:$out_bytes")
    proxy_flags=(--transport arena --arena-path "$dir/arena" --arena-id 0
                 --arena-size "$arena_bytes" --arena-outbound "$out_bytes")
  else
    local keymaps_win
    keymaps_win=$(winepath -w "$dir/keymaps" 2>/dev/null || echo "$dir/keymaps")
    comp_flags=(--shared-dir "$keymaps_win" --shared-peer-prefix "$dir/keymaps/")
    proxy_flags=(--transport shared-file --pool-dir "$dir/pools"
                 --peer-prefix "$(winepath -w "$dir/pools" 2>/dev/null || echo "$dir/pools")\\")
  fi

  # See wc-proxy.sh: a distinct desktop name and a gap, both measured rather
  # than chosen.
  sleep 3
  wine explorer "/desktop=$desktop,1280x1024" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 70 --layout us \
    "${comp_flags[@]}" \
    --log "$(winepath -w "$dir/srv.log" 2>/dev/null || echo "$dir/srv.log")" \
    >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node
  if ! node=$(find_socket "$sock"); then
    bad "the Windows compositor never created its socket node"
    sed 's/^/        /' "$dir/srv.log" 2>/dev/null | head -10
    kill "$srv" 2>/dev/null
    return
  fi
  ok "wc-compositor.exe is listening"

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    "${proxy_flags[@]}" \
    --trace --accept-timeout 25 --timeout 30 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 0.8

  # `stdbuf -oL`, and it is not cosmetic. `weston-eventdemo`'s own `printf`
  # output is fully buffered when stdout is a file, and this leg has to stop the
  # client by killing it — so without line buffering the log contains the
  # `WAYLAND_DEBUG` lines (stderr, unbuffered) and **none** of the client's own
  # decoded characters, which is the half that matters. Measured: a run where
  # every wire assertion passed and every "what did the client make of it"
  # assertion failed, for no reason that was in the compositor.
  ( cd "$dir" && XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy WAYLAND_DEBUG=1 \
    timeout 40 stdbuf -oL -eL weston-eventdemo "${EVENTDEMO_FLAGS[@]}" ) >"$dir/cli.log" 2>&1 &
  local cli=$!

  # **The probe runs inside the same virtual desktop, and that is not a
  # detail.** `FindWindowW` enumerates the windows of *one* desktop, and
  # `wine explorer /desktop=wc-input` creates a second one; a probe launched on
  # the default desktop finds no window of class `WaycomposeToplevel`, injects
  # into the wrong input queue, and reports a compositor that ignores input.
  # Measured: exactly that, on the first run of this harness.
  #
  # Naming the same desktop attaches to the running one rather than making
  # another, which is the case wc-proxy.sh's warning about desktop names is
  # about — reuse is a problem only when the previous one is winding down.
  #
  # `--log` for the reason in `probe-input.c`: a program launched by
  # `wine explorer` has its console inside the desktop.
  # `WC_PROBE_POST=1` swaps `SendInput` for `PostMessage`. Kept reachable from
  # the harness rather than only from the probe, because the value of the
  # fallback is that the two modes can be *compared*: if a guest run ever
  # disagrees between them, the difference is the platform's injection layer and
  # not the compositor's translation. Both modes are run here, so the fallback
  # is measured rather than asserted.
  local probe_log_win probe_mode=()
  [ -n "${WC_PROBE_POST:-}" ] && probe_mode=(--post)
  probe_log_win=$(winepath -w "$dir/probe.log" 2>/dev/null || echo "$dir/probe.log")
  wine explorer "/desktop=$desktop,1280x1024" \
    "$dir/probe-input.exe" --class WaycomposeToplevel --wait 25 --type helloZ \
    "${probe_mode[@]}" --log "$probe_log_win" >"$dir/probe-loader.log" 2>&1
  local probe_rc=$?
  sleep 2

  xwd -root -silent -out "$dir/s.xwd" 2>/dev/null \
    && magick "$dir/s.xwd" "$dir/s.png" 2>/dev/null

  # **Stop the compositor first and let the client notice.** Killing the client
  # instead ends it with a signal, and a program killed by a signal does not
  # run its `atexit` handlers or flush stdio. Taking the display away makes
  # libwayland's dispatch fail and `weston-eventdemo` exit through `exit()`,
  # which flushes — so the decoded characters are in the log because the client
  # chose to leave rather than because it was told to stop existing.
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  wait "$cli" 2>/dev/null
  kill "$cli" 2>/dev/null

  # --- the injection itself ---------------------------------------------
  if [ "$probe_rc" = 0 ]; then
    ok "probe-input.exe found the window and injected: $(grep -o 'target HWND [^,]*' "$dir/probe.log")"
  else
    bad "probe-input.exe exited $probe_rc"
    sed 's/^/        /' "$dir/probe.log" | head -10
  fi
  grep -q "foreground=.*focus=" "$dir/probe.log" \
    && note "$(grep -o 'foreground=.* focus=.*' "$dir/probe.log" | head -1)"

  # --- §37: where the click landed, on the Win32 side of the seam --------
  #
  # The pointer leg above measures everything from `FromUi` upwards. This
  # measures the half it cannot reach: a real `SendInput`, a real
  # `WM_LBUTTONDOWN`, `win32.rs`'s `to_surface`, and only then the same Wayland
  # path. Both are needed, and the number they must produce is the same one —
  # the client-area point the probe aimed at, plus the inset the client itself
  # declared.
  #
  # `to_surface` is the identity, and §31 is what turned that from an accident
  # of `extend_top` being zero into a fact about **both** decoration
  # arrangements: `WM_NCCALCSIZE` folds the caption in by moving the client
  # rectangle's own top edge, so the client origin is the surface origin whether
  # or not the fold happened. This check is what would notice if that stopped
  # being true — nothing on the Win32 input path branches on it, so nothing else
  # would.
  # The probe names both points it visited because Wine reorders them; see
  # `probe-input.c`. The click must be at one of them, converted — a compositor
  # that dropped or invented the inset would be at neither.
  local sent inset_x inset_y
  sed -e 's/\x1b\[[0-9;]*m//g' "$dir/cli.log" >"$dir/cli.geom" 2>/dev/null
  sent=$(grep -o "client points [0-9]*,[0-9]* and [0-9]*,[0-9]*" "$dir/probe.log" | head -1 \
         | sed 's/client points //; s/ and / /')
  inset_x=$(grep -o "set_window_geometry([-0-9]*, [-0-9]*," "$dir/cli.geom" | head -1 \
            | sed 's/.*(\([-0-9]*\), \([-0-9]*\),/\1/')
  inset_y=$(grep -o "set_window_geometry([-0-9]*, [-0-9]*," "$dir/cli.geom" | head -1 \
            | sed 's/.*(\([-0-9]*\), \([-0-9]*\),/\2/')
  if [ -z "$sent" ] || [ -z "$inset_x" ]; then
    note "wine: no click coordinate to compare (probe said [$sent], inset [$inset_x,$inset_y])"
  else
    local got want=""
    got=$(grep -o "^button time: [0-9]*, button: 272, state: pressed, x: [-0-9]*, y: [-0-9]*" \
          "$dir/cli.log" | head -1 | sed 's/.*x: \([-0-9]*\), y: \([-0-9]*\)/\1,\2/')
    local p
    for p in $sent; do
      want="$want $(( ${p%,*} + inset_x )),$(( ${p#*,} + inset_y ))"
    done
    case " $want " in
      *" $got "*)
        ok "wine: a SendInput click at client point ${sent%% *} reached the client at surface $got — WM_LBUTTONDOWN, to_surface, and the inset, all agreeing" ;;
      *)
        bad "wine: the click reached the client at [$got]; the points injected were $sent and the inset is $inset_x,$inset_y, so it should have been one of$want" ;;
    esac
  fi

  # --- the compositor's own view ----------------------------------------
  #
  # These are the lines that say the *Win32 half* ran: a scan code became an
  # evdev keycode, `WM_SETFOCUS` became a focus, `WM_MOUSELEAVE` arrived. The
  # client's log below says the Wayland half ran. Both are needed and neither
  # is the other's evidence.
  grep -q "seat0 capabilities: pointer, keyboard" "$dir/srv.log" \
    && ok "wine: the Windows binary compiled a keymap and advertised a keyboard" \
    || { bad "wine: no keyboard capability"; grep -i "seat0\|keymap" "$dir/srv.log" | sed 's/^/        /' | head -5; }
  grep -q "has keyboard focus" "$dir/srv.log" \
    && ok "wine: WM_SETFOCUS became keyboard focus" \
    || bad "wine: the window never took keyboard focus"
  # No key should have been dropped. The compositor prints the scan code of any
  # it cannot translate, so this is an assertion about the table rather than
  # about the absence of a log line.
  if grep -q "no evdev keycode for scan" "$dir/srv.log"; then
    bad "wine: some scan codes had no evdev keycode"
    grep "no evdev keycode" "$dir/srv.log" | sed 's/^/        /' | head -5
  else
    ok "wine: every injected scan code translated"
  fi

  # --- the rule: what carried the keymap ---------------------------------
  #
  # A keymap is an `h`, and this is the direction stage 4 never exercised.
  # `--shared-dir` is what makes the object a file both sides can name; without
  # it the descriptor is a Windows section and unopenable here. Asserted from
  # the proxy's own count, because a run that got the keymap across by passing
  # something would look identical from the client's side.
  #
  # **Why a non-zero `out` is not the stage-4 rule breaking**, since this is the
  # assertion a reader will stop at: that descriptor exists between `wc-proxy`
  # and `weston-eventdemo`, both Linux processes on this machine, over a socket
  # that never leaves it. The hop that crosses the boundary is the *other*
  # socket, and `proxy.rs` writes that one with a plain `write` that has no fd
  # argument -- so its count is not zero, it is unrepresentable. docs/design.md
  # §14.7 states it in full, including what the number would have to be for the
  # rule to be violated.
  local fds
  fds=$(grep -o "[0-9]* fds in, [0-9]* out[^;]*" "$dir/proxy.log" | tail -1)
  case "$fds" in
    *", "[1-9]*" out"*) ok "wine: the proxy manufactured the client's keymap fd locally ($fds)" ;;
    "")                 bad "wine: the proxy reported no descriptor counts" ;;
    *)                  bad "wine: the proxy materialised nothing: $fds" ;;
  esac

  # STAGE 10: **which** mechanism carried it, which is the whole difference
  # between the two runs of this leg and is invisible from the client's side —
  # the characters are identical either way. The proxy counts the descriptors it
  # made from an arena slice separately from the ones it opened by name, for the
  # reason docs/waycompose-design.md §8.4 gives: a stage that added a second provider with
  # no way to tell which one ran would not really have added it.
  if [ "$mode" = arena ]; then
    case "$fds" in
      *"from the arena"*) ok "wine: the keymap crossed as an arena slice ($fds)" ;;
      *) bad "wine: the compositor had an outbound window and nothing came from the arena: $fds" ;;
    esac
    grep -q "are the compositor's to hand to clients" "$dir/proxy.log" \
      && ok "wine: $(grep -o "arena 0: the last .*" "$dir/proxy.log" | head -1)" \
      || bad "wine: the proxy did not reserve the compositor's window"
    grep -q "outbound window: offset" "$dir/srv.log" \
      && ok "wine: $(grep -o 'arena 0 outbound window: offset [0-9]*, [0-9]* bytes' "$dir/srv.log" | head -1)" \
      || bad "wine: the compositor reserved no outbound window"
    # And the claim the stage exists for: nothing the client opened has a name
    # on any filesystem. There is no `--shared-dir` on this leg's command line
    # and the directory the other leg uses stays empty.
    if [ -z "$(ls -A "$dir/keymaps" 2>/dev/null)" ]; then
      ok "wine: no keymap file was created anywhere — the object has no name at all"
    else
      bad "wine: something landed in $dir/keymaps: $(ls -A "$dir/keymaps" | tr '\n' ' ')"
    fi
  else
    case "$fds" in
      *"from the arena"*) bad "wine: this leg has no arena and something came from one: $fds" ;;
      *) ok "wine: the keymap crossed as a file both sides could name ($fds)" ;;
    esac
    [ -n "$(ls -A "$dir/keymaps" 2>/dev/null)" ] \
      && ok "wine: the keymap is a file in the shared directory ($(ls -A "$dir/keymaps" | head -1))" \
      || bad "wine: --shared-dir was given and nothing was published into it"
  fi
  if grep -q "wl_keyboard@[0-9]*\.keymap" "$dir/proxy.log"; then
    ok "wine: the proxy walked wl_keyboard.keymap in the server direction"
  else
    note "wine: no traced keymap event ($(grep -c '<-' "$dir/proxy.log") server messages traced)"
  fi
  # And nothing crossed the other way. Same check stage 4 makes, restated here
  # because this stage adds a second thing that could have leaked.
  case "$fds" in
    "0 fds in"*|*"0 fds in"*) note "wine: the client sent no descriptors this run" ;;
  esac

  # --- and what the client received --------------------------------------
  check_client_events "$dir/cli.log" wine "${WC_PROBE_POST:+keyboard}"
}

# ---------------------------------------------------------------------------
# pointer: where the click landed, as a number the client says out loud
# ---------------------------------------------------------------------------
#
# ADDED FOR §37, and it is the check whose absence let the bug ship. Every
# pointer assertion above this line says an event *arrived* — a motion happened,
# a button was 272, the axis had the right sign. None of them says **where**,
# and the bug was entirely in the where.
#
# THE CONVERSION UNDER TEST, IN ONE LINE
#
#   surface point = window pixel + the client's own `set_window_geometry` origin
#
# because `compose_tree` blits the tree at `-(gx, gy)`, so the window's top-left
# pixel *is* surface point `(gx, gy)`. The pointer and the pixels must use the
# same number; before §37 the pointer used zero and the pixels used `(gx, gy)`,
# so a click landed `(gx, gy)` up and to the left of the thing under the cursor.
#
# WHY THE EXPECTED NUMBER IS NOT WRITTEN DOWN HERE
#
# It is read out of the client's own `xdg_surface.set_window_geometry` request,
# off the wire, in the same run. A hard-coded 32 would pass against a toolkit
# that changed its shadow margin and would say nothing about a client with a
# different inset; taking it from the wire makes this an assertion about the
# *conversion* rather than about Weston's taste in shadows.
#
# TWO CLIENTS, BECAUSE THEY ANSWER DIFFERENT QUESTIONS
#
#   * `weston-eventdemo` prints the coordinate it received. That is the
#     measurement: a number to compare against a number.
#   * `weston-terminal` is the *symptom* the user reported — it draws its own
#     title bar, and a click that lands in it answers with `xdg_toplevel.move`.
#     So this leg clicks twice, once on the bar and once eight pixels below it,
#     and asserts that exactly one of them starts a move. Before §37 it was the
#     wrong one: the drawn bar did nothing and the row below it dragged the
#     window, which is what "it snaps the mouse there" was.
#
# Linux only, because `--click` is (see `lib.rs`): on Windows a click comes from
# a mouse. What this leg cannot say anything about is `WM_MOUSEMOVE` and
# `FrameStyle` — the same division of evidence §13.6 drew, and the guest run in
# docs/waycompose-design.md §37 is the other half.

pointer_probe() {
  # $1 tag, $2 client command, $3 click x, $4 click y -> sets PP_GX/PP_GY/PP_LOG
  local tag=$1 client=$2 cx=$3 cy=$4
  local dir="$RUNDIR/pointer-$tag"
  mkdir -p "$dir/rt"; chmod 700 "$dir/rt"
  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" --socket "wl-ptr-$tag" --once \
    --timeout 14 --click "$cx,$cy,272,2500" >"$dir/srv.log" 2>&1 &
  local srv=$!
  sleep 1
  # `stdbuf -oL` because the client's own printf is block-buffered into a pipe,
  # and a client killed by `timeout` never flushes it — which reads as a client
  # that received nothing, i.e. as the failure this leg exists to detect.
  ( cd "$dir" && XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY="wl-ptr-$tag" WAYLAND_DEBUG=1 \
    stdbuf -oL -eL timeout 12 $client ) >"$dir/cli.log" 2>&1
  wait "$srv" 2>/dev/null
  sed -e 's/\x1b\[[0-9;]*m//g' "$dir/cli.log" >"$dir/cli.clean"
  PP_LOG="$dir/cli.clean"
  PP_GX=$(grep -o "set_window_geometry([-0-9]*, [-0-9]*," "$PP_LOG" | head -1 \
          | sed 's/.*(\([-0-9]*\), \([-0-9]*\),/\1/')
  PP_GY=$(grep -o "set_window_geometry([-0-9]*, [-0-9]*," "$PP_LOG" | head -1 \
          | sed 's/.*(\([-0-9]*\), \([-0-9]*\),/\2/')
}

run_pointer() {
  echo "== where the click landed =="
  fresh "$LINUX_BIN/wc-compositor" "cargo build -p wc-compositor" || return

  # --- eventdemo: the measurement ----------------------------------------
  if command -v weston-eventdemo >/dev/null; then
    local cx=200 cy=150
    pointer_probe demo "weston-eventdemo --log-motion --log-button" $cx $cy
    if [ -z "$PP_GX" ]; then
      bad "pointer: the client never set a window geometry -- nothing to compare against"
    else
      note "pointer: eventdemo declared an inset of $PP_GX,$PP_GY"
      local want_x=$((cx + PP_GX)) want_y=$((cy + PP_GY))
      local got
      got=$(grep -o "^button time: [0-9]*, button: 272, state: pressed, x: [-0-9]*, y: [-0-9]*" "$PP_LOG" \
            | head -1 | sed 's/.*x: \([-0-9]*\), y: \([-0-9]*\)/\1 \2/')
      if [ "$got" = "$want_x $want_y" ]; then
        ok "pointer: a click at window pixel $cx,$cy reached the client at surface $got, which is the pixel under the cursor"
      else
        bad "pointer: a click at window pixel $cx,$cy reached the client at [$got], expected $want_x $want_y -- off by the window-geometry inset"
      fi
    fi
  else
    note "SKIP weston-eventdemo (not installed) -- this is the leg that measures"
  fi

  # --- flower: the control, and it is not decoration ----------------------
  #
  # `weston-flower` declares **no** window geometry, so its origin is `(0, 0)`
  # and the conversion must be the identity. Without it this leg cannot tell a
  # fix from an over-correction: a compositor that added a constant, or added
  # the inset twice, or added the *parent's* inset to a popup, would pass every
  # check above and move every click in a client that has no inset at all.
  #
  # Read off the wire rather than out of the client, because flower prints
  # nothing: it draws a flower and has no input logging. `wl_pointer.enter`
  # carries the surface-local point, and libwayland decodes it under
  # `WAYLAND_DEBUG`.
  #
  # Flower does send `set_window_geometry`, with an origin of `(0, 0)` — so the
  # control is "the origin is zero", not "there is no geometry", and it is
  # asserted rather than assumed: a run where flower grew an inset would be
  # controlling for nothing and has to say so instead of passing quietly.
  if command -v weston-flower >/dev/null; then
    pointer_probe flower "weston-flower" 100 60
    local ent
    # `[#@]` because libwayland spells object ids with `#` and this file's older
    # checks read a copy that `check_client_events` has already rewritten to
    # `@`. Matching one spelling here found nothing and reported a client that
    # never received a pointer, which is the most misleading possible way for a
    # control to fail.
    ent=$(grep -o "wl_pointer[#@][0-9]*\.enter([0-9]*, wl_surface[#@][0-9]*, [-0-9.]*, [-0-9.]*)" "$PP_LOG" \
          | head -1 | sed 's/.*, \([-0-9.]*\), \([-0-9.]*\))/\1 \2/')
    if [ -n "${PP_GX:-}" ] && [ "$PP_GX,$PP_GY" != "0,0" ]; then
      note "pointer: flower declared an inset of $PP_GX,$PP_GY; the control is only meaningful at 0,0"
    else
      case "$ent" in
        "100.00000000 60.00000000")
          ok "pointer: a client whose origin is 0,0 is untouched — the click arrived at exactly 100,60" ;;
        "") bad "pointer: weston-flower never received a pointer enter" ;;
        *) bad "pointer: a client whose origin is 0,0 had its click moved to [$ent], expected 100 60" ;;
      esac
    fi
  else
    note "SKIP weston-flower (not installed) -- no zero-inset control this run"
  fi

  # --- terminal: the symptom the user reported ---------------------------
  if command -v weston-terminal >/dev/null; then
    # 12 px down is inside a toytoolkit title bar; 40 px down is below it for
    # any inset up to 32, which is what Weston uses. Both are read back against
    # the inset the client actually declared rather than assumed.
    pointer_probe bar "weston-terminal" 200 12
    local on_bar=no; grep -q "xdg_toplevel#[0-9]*\.move(" "$PP_LOG" && on_bar=yes
    pointer_probe below "weston-terminal" 200 40
    local below_bar=no; grep -q "xdg_toplevel#[0-9]*\.move(" "$PP_LOG" && below_bar=yes

    [ "$on_bar" = yes ] \
      && ok "pointer: clicking the title bar weston-terminal drew starts a move" \
      || bad "pointer: clicking the drawn title bar did nothing -- the click landed above the window"
    [ "$below_bar" = no ] \
      && ok "pointer: clicking below the title bar does not start a move" \
      || bad "pointer: clicking 40 px down started a move -- this is the reported bug, the click landed $PP_GY px high"
  else
    note "SKIP weston-terminal (not installed) -- this is the leg with the symptom"
  fi
}

# ---------------------------------------------------------------------------
# term: type into a terminal and watch the characters appear
# ---------------------------------------------------------------------------
#
# `weston-eventdemo` proves the events are right; this proves they are *useful*.
# A terminal is the demonstration because it redraws only when its content
# changes — docs/waycompose-design.md §8.7 measured that, and it is what makes "frames
# appeared after the keys were injected" a statement about the keys rather than
# about an animation. Stage 4 established `weston-terminal` through the proxy;
# stage 6 types into it.
#
# Nothing here reads text off the screen. There is no OCR in this environment
# and inventing one would make the harness's weakest link a font renderer. What
# is asserted instead is the property a terminal has and an animation does not:
# **it was still between the two screenshots, and it moved only because it was
# typed at.**

run_term() {
  echo "== typing into weston-terminal, in a Windows window =="
  fresh "$WIN_BIN/wc-compositor.exe" "cargo build -p wc-compositor --target x86_64-pc-windows-gnu" || return
  fresh "$LINUX_BIN/wc-proxy" "cargo build -p wc-proxy" || return
  command -v weston-terminal >/dev/null || { note "SKIP (weston-terminal not installed)"; return; }
  command -v magick >/dev/null || { bad "imagemagick not on PATH"; return; }
  command -v x86_64-w64-mingw32-gcc >/dev/null || { bad "no mingw gcc"; return; }

  local dir="$RUNDIR/term"
  mkdir -p "$dir/rt" "$dir/pools" "$dir/keymaps" "$dir/frames"
  chmod 700 "$dir/rt"

  x86_64-w64-mingw32-gcc -O1 -mwindows -o "$dir/probe-input.exe" \
    "$ROOT/scripts/probe-input.c" -luser32 2>"$dir/cc.log" \
    || { bad "could not link probe-input.exe"; sed 's/^/        /' "$dir/cc.log"; return; }

  [ -n "${DISPLAY:-}" ] || start_x 96 || { bad "could not start Xvfb"; return; }

  local sock="wl-in6-term"
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null
  local win_path
  win_path() { winepath -w "$1" 2>/dev/null || echo "$1"; }

  sleep 3
  wine explorer "/desktop=wc-term,1280x1024" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 80 --layout us \
    --shared-dir "$(win_path "$dir/keymaps")" --shared-peer-prefix "$dir/keymaps/" \
    --dump-frames "$(win_path "$dir/frames")" \
    --log "$(win_path "$dir/srv.log")" >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node
  if ! node=$(find_socket "$sock"); then
    bad "term: the compositor never created its socket node"; kill "$srv" 2>/dev/null; return
  fi

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --transport shared-file --pool-dir "$dir/pools" \
    --peer-prefix "$(win_path "$dir/pools")\\" \
    --accept-timeout 25 --timeout 45 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 0.8

  XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout 45 weston-terminal >"$dir/cli.log" 2>&1 &
  local cli=$!

  # Let the terminal settle. It draws its window, its prompt, and then stops —
  # which is exactly the state that makes the comparison below mean something.
  sleep 8
  local before_frames before_shot
  before_frames=$(find "$dir/frames" -name '*.ppm' | wc -l)
  xwd -root -silent -out "$dir/before.xwd" 2>/dev/null \
    && magick "$dir/before.xwd" "$dir/before.png" 2>/dev/null

  # A second shot at the same quiescent moment, to establish that the window is
  # *still*. Without it, "the picture changed after typing" would be satisfied
  # by a blinking cursor, and every terminal has one.
  sleep 2
  xwd -root -silent -out "$dir/still.xwd" 2>/dev/null \
    && magick "$dir/still.xwd" "$dir/still.png" 2>/dev/null

  # **Clicked first, and that is §37's change to this leg.** It used to pass
  # `--no-mouse`, so it typed into a window nobody had clicked in — which is not
  # what a user does and is why it stayed green while the user could not type.
  #
  # The click is at client-area `(200, 40)`: below the title bar `weston-terminal`
  # draws for itself, for any inset up to its 32. Before §37 that click arrived
  # 32 px high, inside the bar, and the terminal answered `xdg_toplevel.move` —
  # which this compositor hands to Windows as `SC_MOVE`, and `SC_MOVE` with no
  # mouse point is the keyboard move mode: it warps the cursor to the caption
  # and swallows every key until Escape. So a run with the bug loses the
  # keystrokes *and* the repaints, and this leg's existing "the picture changed"
  # and "new frames appeared" checks both fail — the user's two symptoms, from
  # one cause, on one instrument.
  wine explorer "/desktop=wc-term,1280x1024" \
    "$dir/probe-input.exe" --class WaycomposeToplevel --wait 20 \
    --at 200,40 --no-leave \
    --type "echo waycompose types" --log "$(win_path "$dir/probe.log")" \
    >"$dir/probe-loader.log" 2>&1
  local probe_rc=$?

  sleep 4
  local after_frames
  after_frames=$(find "$dir/frames" -name '*.ppm' | wc -l)
  xwd -root -silent -out "$dir/after.xwd" 2>/dev/null \
    && magick "$dir/after.xwd" "$dir/after.png" 2>/dev/null

  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  kill "$cli" 2>/dev/null; wait "$cli" 2>/dev/null

  [ "$probe_rc" = 0 ] && ok "term: probe-input.exe typed into the window" \
                      || { bad "term: probe-input.exe exited $probe_rc"; sed 's/^/        /' "$dir/probe.log" 2>/dev/null | head -6; }

  grep -q 'title -> "Wayland Terminal"' "$dir/srv.log" \
    && ok "term: it is weston-terminal, with its own title in the Windows caption" \
    || bad "term: no title from the client"

  # §37. Named separately from the picture checks below because it is the
  # *cause* and they are the effect: if the click started a move, everything
  # after it is a statement about a compositor in a modal loop.
  grep -q "handed a move to Windows" "$dir/srv.log" \
    && bad "term: the click 40 px down the client area started a window move — it landed inside the bar the client drew, and SC_MOVE is now eating the keys" \
    || ok "term: the click 40 px down the client area started no window move, so the keys below went to the terminal"

  # The still pair, first. A terminal that is redrawing on a timer would fail
  # here and every claim below it would be worthless.
  local still_diff typed_diff
  if [ -f "$dir/before.png" ] && [ -f "$dir/still.png" ]; then
    still_diff=$(magick compare -metric AE "$dir/before.png" "$dir/still.png" null: 2>&1 | grep -o "^[0-9]*")
    still_diff=${still_diff:-0}
    [ "$still_diff" -lt 4000 ] \
      && ok "term: the window is quiescent — $still_diff pixels changed in 2 s with nothing typed" \
      || bad "term: the window changed by $still_diff pixels with no input; the comparison below is meaningless"
  else
    bad "term: no quiescent screenshots"
  fi

  # **The threshold, and why it is 150 rather than a round bigger number.**
  # `echo waycompose types` is one line of an 8x17 monospace font, and only the
  # *lit* pixels of the glyphs differ from the black field behind them.
  # Measured: **337** changed pixels for those 21 characters, against 0 while
  # the window sat idle. A first attempt at this check demanded 500 and failed
  # a run whose screenshot plainly showed the text — which is worth recording,
  # because the failure was in the harness's idea of how many pixels a letter
  # is and not in anything the compositor did.
  if [ -f "$dir/still.png" ] && [ -f "$dir/after.png" ]; then
    typed_diff=$(magick compare -metric AE "$dir/still.png" "$dir/after.png" null: 2>&1 | grep -o "^[0-9]*")
    typed_diff=${typed_diff:-0}
    if [ "$typed_diff" -ge 150 ] && [ "$typed_diff" -gt $(( ${still_diff:-0} * 3 )) ]; then
      ok "term: typing changed $typed_diff pixels against $still_diff while idle — the characters are on screen"
    else
      bad "term: typing changed only $typed_diff pixels (idle was $still_diff)"
    fi
  else
    bad "term: no screenshot after typing"
  fi

  # The frames are the compositor's own count of how many times the client
  # committed. A terminal that received the keys redraws once per character or
  # thereabouts; one that did not receive them commits nothing at all.
  if [ "$after_frames" -gt "$before_frames" ]; then
    ok "term: the client committed $((after_frames - before_frames)) new frames after the keys ($before_frames -> $after_frames)"
  else
    bad "term: no new frames after typing ($before_frames -> $after_frames)"
  fi

  if grep -q "no evdev keycode for scan" "$dir/srv.log"; then
    bad "term: some scan codes had no evdev keycode"
  else
    ok "term: every scan code of \"echo waycompose types\" translated"
  fi
  # The deliverable of the stage. `docs/stage6-typing.png` is this image,
  # cropped — kept in the repository for the same reason `stage3-window.png` is:
  # a table of numbers is not the same as seeing the characters.
  note "term: the screenshot after typing is $dir/after.png (WC_KEEP=1 to keep it)"
}

case "${1:-all}" in
  linux)   run_linux ;;
  pointer) run_pointer ;;
  wine)    run_wine shared-file ;;
  arena)   run_wine arena ;;
  term)    run_term ;;
  all)   run_linux; echo; run_pointer; echo; run_wine shared-file; echo
         run_wine arena; echo
         kill "${XPID:-0}" 2>/dev/null; unset XPID; unset DISPLAY
         run_term ;;
  *) echo "usage: $0 [linux|pointer|wine|arena|term|all]"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ -n "$KEEP" ] && echo "artefacts kept in $RUNDIR"
[ "$fail" -eq 0 ]
