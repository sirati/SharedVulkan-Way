#!/usr/bin/env bash
# Prove the stage-6 milestone: **a third-party Wayland client that can be typed
# into and clicked on**, with the keystrokes coming from Win32.
#
#   scripts/wc-input.sh linux    the seat and the keymap, against Weston clients
#   scripts/wc-input.sh wine     real WM_KEYDOWN, through the proxy, into a client
#   scripts/wc-input.sh arena    the same, with the keymap cut from the arena
#                                and no shared directory at all (stage 10)
#   scripts/wc-input.sh all      all of them
#
# Run from inside `nix develop`.
#
# WHY THE EVIDENCE IS IN TWO LEGS, AGAIN
#
# The same split docs/design.md §8.5 drew for pixels, for the same structural
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
  [ -x "$LINUX_BIN/wc-compositor" ] || { bad "no $LINUX_BIN/wc-compositor -- cargo build first"; return; }

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
    grep -q "capabilities: pointer keyboard" "$dir/info.log" \
      && ok "the seat advertises both capabilities: $(grep -o 'capabilities: .*' "$dir/info.log")" \
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
  local sizes
  sizes=$(grep -o "wl_keyboard@[0-9]*\.keymap(1, fd [0-9]*, [0-9]*)" "$dir/cli.log.clean" \
          | grep -o "[0-9]*)" | tr -d ')' | tr '\n' ' ')
  ok "eventdemo: keymaps delivered, in bytes: $sizes"
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
  grep -q "wl_seat@[0-9]*\.capabilities(3)" "$log" \
    && ok "$tag: libwayland decoded wl_seat.capabilities(3) = pointer|keyboard" \
    || bad "$tag: no capabilities event on the wire"
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
  grep -q "^axis discrete axis: 0 value: -1" "$log" \
    && ok "$tag: and it was one discrete detent, not a smooth scroll" \
    || note "$tag: no discrete axis line ($(grep -m1 '^axis discrete' "$log"))"
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
  [ -f "$WIN_BIN/wc-compositor.exe" ] || { bad "no $WIN_BIN/wc-compositor.exe"; return; }
  [ -x "$LINUX_BIN/wc-proxy" ] || { bad "no $LINUX_BIN/wc-proxy"; return; }
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
  # single keystroke was injected. That is docs/design.md §12.13 item 5 —
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
  # reason docs/design.md §8.4 gives: a stage that added a second provider with
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
# term: type into a terminal and watch the characters appear
# ---------------------------------------------------------------------------
#
# `weston-eventdemo` proves the events are right; this proves they are *useful*.
# A terminal is the demonstration because it redraws only when its content
# changes — docs/design.md §8.7 measured that, and it is what makes "frames
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
  [ -f "$WIN_BIN/wc-compositor.exe" ] || { bad "no $WIN_BIN/wc-compositor.exe"; return; }
  [ -x "$LINUX_BIN/wc-proxy" ] || { bad "no $LINUX_BIN/wc-proxy"; return; }
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

  wine explorer "/desktop=wc-term,1280x1024" \
    "$dir/probe-input.exe" --class WaycomposeToplevel --wait 20 --no-mouse \
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
  linux) run_linux ;;
  wine)  run_wine shared-file ;;
  arena) run_wine arena ;;
  term)  run_term ;;
  all)   run_linux; echo; run_wine shared-file; echo; run_wine arena; echo
         kill "${XPID:-0}" 2>/dev/null; unset XPID; unset DISPLAY
         run_term ;;
  *) echo "usage: $0 [linux|wine|arena|term|all]"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ -n "$KEEP" ] && echo "artefacts kept in $RUNDIR"
[ "$fail" -eq 0 ]
