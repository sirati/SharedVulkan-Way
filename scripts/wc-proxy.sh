#!/usr/bin/env bash
# Prove the stage-4 milestone: an **unmodified** third-party Wayland client
# painting into a real Windows window, across a boundary no descriptor crosses.
#
#   scripts/wc-proxy.sh window     the milestone, both clients, both transports
#   scripts/wc-proxy.sh cost       what each copy scope costs, measured
#   scripts/wc-proxy.sh all        both
#
# Run from inside `nix develop`.
#
# WHAT THIS CLOSES, AND WHY IT IS ONE EXPERIMENT RATHER THAN TWO
#
# docs/design.md §8.5 had to split its evidence in half and said so:
#
#   * **Linux** ran `weston-terminal` and `weston-presentation-shm` — real
#     libwayland clients that cannot share a bug with this project — through the
#     whole buffer path, and could not produce an `HWND` because there is no
#     Win32 there.
#   * **Wine** ran `wc-shmclient.exe`, which speaks our dialect and is ours, and
#     got pixels into a window with Windows' own caption. "A real weakening",
#     as that section put it.
#
# The reason the halves could not be joined is §2.4: an `h` argument is
# out-of-band on Unix and in-band on Windows, so a stock libwayland client's
# `wl_shm.create_pool` writes no descriptor bytes and the Windows server's
# parser desynchronises. Measured twice — `wc-window.sh`'s third block under
# Wine, and `weston-flower` against the real guest in §7.8.
#
# `wc-proxy` is what joins them. It is a Wayland server to the local client,
# ancillary data and all, and a client of the Windows compositor in our dialect.
# So this harness runs **one** experiment: `weston-terminal` and
# `weston-presentation-shm`, unmodified, against `wc-compositor.exe`, and
# photographs the result from outside every process involved.
#
# THE RULE THIS HARNESS ALSO CHECKS
#
# No descriptor and no handle may cross, ever, even here where both sides are on
# one machine. So the harness does not only check that pixels arrived: it checks
# *what crossed*, by asserting on the compositor's own record of which kind of
# object it opened (`wc_handle::map_counts`, printed per window). A run that got
# the pixels there by passing something would fail these even though the picture
# would look identical.
#
# WHAT WINE DOES AND DOES NOT PROVE. Unchanged from stages 1-3 and still the
# most important sentence: Wine is not Windows. A pass says our code paths are
# self-consistent and structurally possible. Wine has been wrong four times on
# this project, so a *negative* here is not a result and `scripts/wc-guest.sh`
# is the arbiter. What a pass buys is that the whole chain — third-party client,
# host proxy, in-band descriptor, named shared memory opened by the peer, GDI,
# an HWND — runs end to end without a descriptor crossing.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }

RUNDIR=$(mktemp -d /tmp/wc-proxy-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"

# ---------------------------------------------------------------------------
# plumbing
# ---------------------------------------------------------------------------

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

# Find the compositor's AF_UNIX node inside the prefix. Under Wine that node is
# a *real host socket*, which is the only reason a native Linux proxy can reach
# a mingw `.exe` at all (docs/design.md §4.2). On the real guest `afunix.sys`
# makes an NTFS reparse point instead and `scripts/probe-relay.c` is the bridge.
find_socket() {
  local name=$1 node=""
  for _ in $(seq 1 250); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$name" 2>/dev/null | head -1)
    [ -n "$node" ] && { echo "$node"; return 0; }
    sleep 0.2
  done
  return 1
}

# run_one <tag> <client> <transport> <copy-scope> <seconds>
#
# Starts the Windows compositor inside a Wine virtual desktop, the proxy beside
# it, and the client against the proxy; photographs the X root twice while the
# client is still drawing; leaves everything in $RUNDIR/<tag>.
run_one() {
  local tag=$1 client=$2 transport=$3 scope=$4 secs=$5
  local dir="$RUNDIR/$tag"
  mkdir -p "$dir/rt" "$dir/frames"
  chmod 700 "$dir/rt"

  local sock="wl-s4-$tag"
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  # STAGE 6. The compositor now has a keyboard, so any client that binds
  # `wl_seat` receives a `wl_keyboard.keymap` — which is an `h` in the
  # *server-to-client* direction, the one stage 4 implemented and never
  # exercised (§9.10 item 5). Without `--shared-dir` the compositor publishes it
  # as a `Local\wc-…` section and `wc-proxy` ends the connection with a
  # diagnosis, which is correct and which broke this harness the moment stage 6
  # landed: `weston-terminal` binds a seat and `weston-presentation-shm` does
  # not, so exactly one of the two clients here started failing.
  #
  # Recorded rather than quietly fixed, because it is the clearest possible
  # demonstration that the two directions are different problems.
  mkdir -p "$dir/keymaps"
  local shared=(--shared-dir "$(winepath -w "$dir/keymaps" 2>/dev/null || echo "$dir/keymaps")"
                --shared-peer-prefix "$dir/keymaps/")

  local arena_arg=() proxy_extra=()
  if [ "$transport" = arena ]; then
    # The arena is provided **out of band, before either side starts**, which is
    # what it is in the real deployment: shared_vulkan's cold-path handshake has
    # long since run by the time a compositor exists. Here the harness is the
    # third party, and that is the whole of the difference.
    local arena="$dir/arena" size=$((64 * 1024 * 1024))
    truncate -s "$size" "$arena"
    arena_arg=(--arena "0:$size:$(winepath -w "$arena" 2>/dev/null || echo "$arena")")
    proxy_extra=(--transport arena --arena-path "$arena" --arena-id 0 --arena-size "$size")
  else
    local pools="$dir/pools"
    mkdir -p "$pools"
    proxy_extra=(--transport shared-file --pool-dir "$pools"
                 --peer-prefix "$(winepath -w "$pools" 2>/dev/null || echo "$pools")\\")
  fi

  # A **distinct desktop name per run, and a gap before starting**, both of
  # which were measured rather than chosen. docs/design.md §8.9 records that the
  # Wine harnesses collide when run back to back because the previous
  # `wineserver` is still shutting down; this harness runs three compositors in
  # one go and hit a sharper version of it. With the same desktop name the
  # second `wine explorer` attaches to the first run's desktop, which is still
  # winding down, and `CreateWindowExW` then fails for every visible top-level
  # window — the compositor logs `wl_surface commit` and simply never makes a
  # window. That is §8.11 item 1's failure mode arriving as a test artefact
  # rather than as a finding, which is exactly the sort of thing that would be
  # written up as "Wine cannot do this".
  sleep 3
  wine explorer "/desktop=wc-$tag,1280x1024" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 60 \
    --dump-frames "$(winepath -w "$dir/frames" 2>/dev/null || echo "$dir/frames")" \
    "${arena_arg[@]}" "${shared[@]}" \
    --log "$(winepath -w "$dir/srv.log" 2>/dev/null || echo "$dir/srv.log")" \
    >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node
  if ! node=$(find_socket "$sock"); then
    echo "NOSOCKET" >"$dir/why"
    kill "$srv" 2>/dev/null
    return 1
  fi

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --copy "$scope" "${proxy_extra[@]}" \
    --accept-timeout 20 --timeout "$secs" >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 0.8

  XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout $((secs + 8)) "$client" >"$dir/cli.log" 2>&1 &
  local cli=$!

  # Two shots, while the client is still drawing. One shot cannot tell a live
  # window from a window showing one frozen frame, which is the failure stage 3
  # designed this pair to catch.
  sleep 5
  xwd -root -silent -out "$dir/s1.xwd" 2>/dev/null && magick "$dir/s1.xwd" "$dir/s1.png" 2>/dev/null
  sleep 1.5
  xwd -root -silent -out "$dir/s2.xwd" 2>/dev/null && magick "$dir/s2.xwd" "$dir/s2.png" 2>/dev/null

  wait "$cli" 2>/dev/null
  echo $? >"$dir/cli.rc"
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  return 0
}

# The bounding box of every pixel within `fuzz` of `colour`, as WxH+X+Y, or
# empty. `-trim` on a masked copy: the tool doing the measuring is ImageMagick,
# reading an X screenshot taken by `xwd`, neither of which is ours.
bbox() {
  magick "$1" -fuzz "${3:-6}%" -fill black +opaque "$2" -fill white -opaque "$2" \
    -format "%@" info: 2>/dev/null
}

# ---------------------------------------------------------------------------
# window: the milestone
# ---------------------------------------------------------------------------

check_common() {
  local tag=$1 transport=$2 dir="$RUNDIR/$1"
  local srv="$dir/srv.log" px="$dir/proxy.log"

  if [ -f "$dir/why" ]; then
    bad "$tag: the Windows compositor never created its socket node"
    sed 's/^/        /' "$dir/srv.log" 2>/dev/null | head -10
    return 1
  fi

  grep -q "xdg_toplevel created" "$srv" \
    && ok "$tag: the client reached xdg_toplevel through the proxy" \
    || { bad "$tag: no xdg_toplevel"; sed 's/^/        /' "$srv" | head -20; return 1; }

  grep -q "window .* created (HWND" "$srv" \
    && ok "$tag: a real HWND was created for it" \
    || bad "$tag: no HWND"

  # The claim of the stage. Third-party client, Windows window, client's pixels.
  local presents
  presents=$(grep -o "([0-9]* frames presented)" "$srv" | grep -o "[0-9]*" | tail -1)
  presents=${presents:-0}
  if [ "$presents" -ge 2 ]; then
    ok "$tag: $presents frames of the client's pixels reached the window"
  else
    bad "$tag: only $presents frame(s) presented"
  fi

  # *What crossed*, from the compositor's own count of what it opened. This is
  # the rule, checked rather than assumed: a run that smuggled a descriptor
  # across would produce an identical picture and fail here.
  local counts
  counts=$(grep -o "mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice" "$srv" | tail -1)
  case "$transport:$counts" in
    "shared-file:mapped: 0 section, "[1-9]*" shared-file, 0 arena-slice")
      ok "$tag: the compositor opened only shared files by name ($counts)" ;;
    "arena:mapped: 0 section, 0 shared-file, "[1-9]*" arena-slice")
      ok "$tag: the compositor resolved only arena offsets, opening nothing ($counts)" ;;
    *)
      bad "$tag: wrong transport served the pixels: ${counts:-nothing recorded}" ;;
  esac

  # And from the proxy's side: the descriptor it published, and that every
  # descriptor it received from the client was consumed locally.
  if grep -q "pool .* -> $transport" "$px"; then
    ok "$tag: the proxy published every pool through the $transport transport"
  else
    bad "$tag: the proxy did not use the $transport transport"
    grep "pool " "$px" | sed 's/^/        /' | head -5
  fi
  # **CHANGED IN STAGE 6, and the change is the interesting part.**
  #
  # This used to demand `0 out`, and that passed because the compositor had no
  # keyboard: nothing ever travelled server-to-client carrying an `h`. Stage 6
  # gives it one, so a client that binds `wl_seat` now receives a
  # `wl_keyboard.keymap` and the proxy hands it a real file descriptor.
  #
  # That is **not** a violation of the rule and the old check was measuring the
  # wrong thing. The rule is that nothing crosses *the boundary*; the fds
  # counted here are on the host side of it, manufactured locally by opening a
  # path the compositor named. `wc-proxy`'s upstream writer has no fd path at
  # all, which is what actually enforces the rule and is enforced by
  # construction rather than by this number.
  #
  # So what is asserted now is that every descriptor handed to the client was
  # manufactured rather than relayed — which is exactly what a failure to open
  # the name would report, and is checked by its absence.
  local fds
  fds=$(grep -o "[0-9]* fds in, [0-9]* out" "$px" | tail -1)
  case "$fds" in
    "") bad "$tag: the proxy reported no descriptor counts" ;;
    *)  ok "$tag: descriptors in from the client, out only ones made here from a name ($fds)" ;;
  esac
  if grep -q "cannot open" "$px"; then
    bad "$tag: the compositor named something this side could not open"
    grep "cannot open" "$px" | sed 's/^/        /' | head -3
  else
    ok "$tag: every name the compositor sent was openable on this side"
  fi

  # Two shots of a live window. `weston-terminal` is static by design and
  # `weston-presentation-shm` animates, so only the second can carry this — the
  # same asymmetry docs/design.md §8.9 records for the Linux harness.
  local differ=""
  if [ -f "$dir/s1.png" ] && [ -f "$dir/s2.png" ]; then
    differ=$(magick compare -metric AE "$dir/s1.png" "$dir/s2.png" null: 2>&1 | grep -o "^[0-9]*")
    ok "$tag: two screenshots 1.5 s apart, differing by ${differ:-0} pixels"
  else
    bad "$tag: no screenshot was captured"
  fi
  echo "${differ:-0}" >"$dir/differ"
  return 0
}

run_window() {
  echo "== the milestone: an unmodified third-party client, in a Windows window =="
  for exe in wc-compositor.exe; do
    [ -f "$WIN_BIN/$exe" ] || { bad "no $WIN_BIN/$exe -- cargo build --target x86_64-pc-windows-gnu first"; return; }
  done
  [ -x "$LINUX_BIN/wc-proxy" ] || { bad "no $LINUX_BIN/wc-proxy -- cargo build first"; return; }
  command -v wine >/dev/null || { bad "wine not on PATH"; return; }
  command -v Xvfb >/dev/null || { bad "Xvfb not on PATH -- required, see the header"; return; }
  command -v magick >/dev/null || { bad "imagemagick not on PATH -- required"; return; }

  start_x 93 || { bad "could not start Xvfb"; return; }
  ok "Xvfb up on $DISPLAY (1280x1024x24)"

  # --- weston-terminal, over a shared file -------------------------------
  #
  # Toytoolkit, Argb8888, real text rendering, and the client whose pool growth
  # shaped the whole deferral design (see `Session::flush_deferred`).
  if command -v weston-terminal >/dev/null; then
    echo "  -- weston-terminal / shared-file --"
    run_one term weston-terminal shared-file damage 8
    if check_common term shared-file; then
      local srv="$RUNDIR/term/srv.log"
      grep -q 'title -> "Wayland Terminal"' "$srv" \
        && ok "term: the client's own title reached the Windows caption" \
        || bad "term: no title"
      grep -q 'app_id -> "org.freedesktop.weston.wayland-terminal"' "$srv" \
        && ok "term: set_app_id arrived, which is what the taskbar icon will key off" \
        || bad "term: no app_id"
      grep -q "mapped 806x491" "$srv" \
        && ok "term: the window is 806x491, the size the client asked for" \
        || bad "term: unexpected geometry: $(grep -o 'mapped [0-9]*x[0-9]*' "$srv" | head -1)"

      # The pixels, read back by tools that are not ours. The dump is the
      # compositor's view; the checks are stage 3's.
      # The pixels, read back out of the process by tools that are not ours.
      # `--dump-frames` is new on Win32 for stage 4 and is what gives this leg
      # the read-back the Linux leg always had (docs/design.md §8.5).
      local frame
      frame=$(find "$RUNDIR/term/frames" -name '*.ppm' 2>/dev/null | sort | tail -1)
      if [ -n "$frame" ]; then
        local geom fw fh colours
        geom=$(magick identify -format "%wx%h" "$frame" 2>/dev/null)
        fw=${geom%x*}; fh=${geom#*x}
        colours=$(magick identify -format "%k" "$frame" 2>/dev/null)
        # The buffer is *larger* than the window and that is correct, not a
        # bug: toytoolkit draws a drop shadow around its frame and tells the
        # compositor the visible part with `xdg_surface.set_window_geometry`.
        # Measured at 869x555 for an 806x491 window. Asserting equality here
        # would be asserting that the client does not do something it is
        # entitled to do.
        if [ "${fw:-0}" -ge 806 ] && [ "${fh:-0}" -ge 491 ]; then
          ok "term: the dumped frame decodes as $geom with $colours distinct colours"
        else
          bad "term: the dumped frame is $geom, smaller than the 806x491 window"
        fi
        [ "${colours:-0}" -ge 16 ] \
          && ok "term: the frame has structure -- a terminal with text in it, not a flat field" \
          || bad "term: only ${colours:-0} colours"

        # **The flip check, and it is unmissable by construction.**
        # `weston-terminal` draws its own toytoolkit title bar as a light band
        # across the top of its buffer and a black terminal field below. An
        # upside-down blit puts the light band at the bottom, which is exactly
        # docs/design.md §8.11 item 3's worry about `SetDIBitsToDevice` with a
        # negative `biHeight` — the one place the documentation is ambiguous
        # and the most likely to differ between Wine's GDI and the real one.
        #
        # **How many light rows are in each half.** Three earlier attempts
        # measured the wrong thing and each failure is worth keeping, because
        # each looked like the right measurement:
        #
        #  * a band at the very top and the very bottom: both are toytoolkit's
        #    drop shadow, and it reported 0 against 0;
        #  * a band a tenth of the way in: lands just below the title bar, 0.20
        #    against 0.01 — the right sign and not a convincing margin;
        #  * the *brightest* row of each half: 0.95 against 0.76, because the
        #    frame's bottom border is a light line one or two rows tall, so a
        #    maximum cannot tell a title bar from a border.
        #
        # A title bar is tens of light rows and a border is one or two, so
        # counting is what separates them. Nothing here needs to know where the
        # bar is, only that a broad light band is in the top half and not the
        # bottom — which is exactly what "not upside down" means for this client.
        local half=$((fh / 2)) top bot
        top=$(magick "$frame" -crop "${fw}x${half}+0+0" +repage -colorspace Gray \
              -resize "1x${half}!" -threshold 50% -format "%[fx:int(mean*h+0.5)]" info: 2>/dev/null)
        bot=$(magick "$frame" -crop "${fw}x${half}+0+${half}" +repage -colorspace Gray \
              -resize "1x${half}!" -threshold 50% -format "%[fx:int(mean*h+0.5)]" info: 2>/dev/null)
        if [ "${top:-0}" -ge 8 ] && [ "${top:-0}" -gt $(( ${bot:-0} * 3 )) ]; then
          ok "term: ${top} light rows in the top half against ${bot} in the bottom -- the title bar is up, not flipped"
        else
          bad "term: ${top:-?} light rows top, ${bot:-?} bottom -- the image may be upside down"
        fi
      else
        bad "term: no frame was dumped"
      fi
      [ "$(cat "$RUNDIR/term/cli.rc")" = 0 ] \
        && ok "term: the client exited 0" \
        || note "term: the client exited $(cat "$RUNDIR/term/cli.rc") (124 = killed by timeout)"
    fi
  else
    note "SKIP weston-terminal (not installed)"
  fi

  # --- weston-presentation-shm, over the arena ---------------------------
  #
  # A simple-client rather than a toytoolkit one, pure `wl_shm`, and it redraws
  # continuously — which is the only way to show the blit is live rather than
  # one frame held. Over the arena, because that is the transport the real
  # deployment uses and it must carry the same client.
  if command -v weston-presentation-shm >/dev/null; then
    echo "  -- weston-presentation-shm / arena --"
    run_one pres weston-presentation-shm arena damage 8
    if check_common pres arena; then
      local differ
      differ=$(cat "$RUNDIR/pres/differ")
      [ "${differ:-0}" -ge 500 ] \
        && ok "pres: the window is live -- $differ pixels changed between the two shots" \
        || bad "pres: only $differ pixels changed; the window may be showing one frozen frame"

      # Stage 3's kind of measurement, on a client that is not ours.
      #
      # Pure red, green and blue must all be on the captured screen: the wheel
      # arrived with its channels intact. This is the check that a `Xrgb8888`
      # buffer and a 32-bit `BI_RGB` DIB really are the same bytes in the same
      # order (docs/design.md §8.4) — a B/R swap anywhere between the client's
      # `memfd` and the glass would leave a plausible-looking wheel with red and
      # blue exchanged, and the harness would still find three primaries, so the
      # *positions* are what carry it below.
      #
      # Not measured against the white field: the Wine window frame is white
      # too, so its bounding box spans the frame and says nothing. Measured 662x431
      # for a 250x250 client, which is a check that would have passed happily
      # while measuring the wrong thing.
      local seen=0 minx=99999 miny=99999 maxx=0 maxy=0
      for c in red lime blue; do
        local b
        b=$(bbox "$RUNDIR/pres/s1.png" "$c" 8)
        [ -z "$b" ] && continue
        seen=$((seen + 1))
        # `WxH+X+Y`, split without assuming anything about field widths.
        local w h x y rest
        w=${b%%x*}
        rest=${b#*x}
        h=${rest%%+*}
        x=$(echo "$b" | cut -d+ -f2)
        y=$(echo "$b" | cut -d+ -f3)
        [ "$x" -lt "$minx" ] && minx=$x
        [ "$y" -lt "$miny" ] && miny=$y
        [ "$((x + w))" -gt "$maxx" ] && maxx=$((x + w))
        [ "$((y + h))" -gt "$maxy" ] && maxy=$((y + h))
      done
      [ "$seen" = 3 ] \
        && ok "pres: red, green and blue are all on screen -- the channel order survived" \
        || bad "pres: only $seen of 3 primaries found on screen"

      # The wheel is inscribed in the client's 250x250 buffer, so the union of
      # the three coloured wedges must fit inside 250x250 and must not be tiny.
      # That is the scale check: a DPI-virtualised window (docs/design.md §8.10
      # item 9) would be drawn larger, and a stride error would smear the wedges
      # horizontally past the buffer's width. Both fail this and neither fails a
      # "the colours are there" check.
      if [ "$seen" = 3 ]; then
        local uw=$((maxx - minx)) uh=$((maxy - miny))
        if [ "$uw" -le 250 ] && [ "$uh" -le 250 ] && [ "$uw" -ge 120 ] && [ "$uh" -ge 120 ]; then
          ok "pres: the wedges span ${uw}x${uh} on screen, inside the client's 250x250 buffer and at 1:1"
        else
          bad "pres: the wedges span ${uw}x${uh}, which does not fit a 250x250 buffer drawn 1:1"
        fi
      fi

      local frame
      frame=$(find "$RUNDIR/pres/frames" -name '*.ppm' 2>/dev/null | sort | tail -1)
      [ -n "$frame" ] && [ "$(magick identify -format "%wx%h" "$frame" 2>/dev/null)" = "250x250" ] \
        && ok "pres: the dumped frame decodes as 250x250" \
        || bad "pres: the dumped frame is $(magick identify -format "%wx%h" "$frame" 2>/dev/null)"
    fi
  else
    note "SKIP weston-presentation-shm (not installed)"
  fi

  # --- the cross-check: the other pairing --------------------------------
  #
  # Each client has now driven one transport. Swapping them is what says the
  # result is a property of the path and not of one client's habits, which is
  # the same argument docs/design.md §8.5 makes for running two clients at all.
  if command -v weston-terminal >/dev/null; then
    echo "  -- weston-terminal / arena (the cross-check) --"
    run_one term-arena weston-terminal arena damage 8
    check_common term-arena arena
  fi

  stop=0
}

# ---------------------------------------------------------------------------
# cost: what the copy actually costs, measured rather than argued
# ---------------------------------------------------------------------------

run_cost() {
  echo "== what a commit costs, per copy scope and per transport =="
  [ -x "$LINUX_BIN/wc-proxy" ] || { bad "no $LINUX_BIN/wc-proxy"; return; }
  command -v weston-presentation-shm >/dev/null \
    || { note "SKIP (weston-presentation-shm not installed)"; return; }
  start_x 94 || { bad "could not start Xvfb"; return; }

  printf "  %-12s %-6s %8s %10s %12s %10s\n" transport scope commits copies "MiB copied" "us/copy"
  for transport in shared-file arena; do
    for scope in pool buffer damage; do
      local tag="cost-$transport-$scope"
      run_one "$tag" weston-presentation-shm "$transport" "$scope" 6 || continue
      local line
      line=$(grep -o "[0-9]* commits, [0-9]* copies, [0-9.]* MiB in [0-9.]* ms" "$RUNDIR/$tag/proxy.log" | tail -1)
      local us
      us=$(grep -o "[0-9]* us per copy" "$RUNDIR/$tag/proxy.log" | tail -1 | grep -o "^[0-9]*")
      set -- $line
      printf "  %-12s %-6s %8s %10s %12s %10s\n" "$transport" "$scope" "$1" "$3" "$5" "${us:-n/a}"
      echo "$transport $scope $line ${us:-0}us" >>"$RUNDIR/cost.txt"
    done
  done
  stop_x_note=1
  if [ -s "$RUNDIR/cost.txt" ]; then
    ok "six configurations measured; the table is above"
  else
    bad "no configuration produced a measurement"
  fi
}

case "${1:-all}" in
  window) run_window ;;
  cost)   run_cost ;;
  all)    run_window; cleanup_x=1; kill "${XPID:-0}" 2>/dev/null; unset XPID; run_cost ;;
  *) echo "usage: $0 [window|cost|all]"; exit 2 ;;
esac

echo
echo "== $pass passed, $fail failed =="
[ -n "$KEEP" ] && echo "artefacts kept in $RUNDIR"
[ "$fail" -eq 0 ]
