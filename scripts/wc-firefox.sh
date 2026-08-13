#!/usr/bin/env bash
# Point **Firefox** at this compositor and write down what actually happens.
#
#   scripts/wc-firefox.sh enumerate   what Firefox binds, and what it does without
#   scripts/wc-firefox.sh window      the browser's own pixels, measured from outside
#   scripts/wc-firefox.sh menu        a click, an xdg_popup, and a second window
#   scripts/wc-firefox.sh wine        the same client into a real HWND, through the proxy
#   scripts/wc-firefox.sh all         all four
#
# Run from inside `nix develop`.
#
# WHY THIS HARNESS IS DIFFERENT FROM THE OTHERS
#
# `wc-proxy.sh` and `wc-input.sh` prove that a mechanism works, using clients
# chosen because they are small and third-party. This one is the opposite
# errand: the client is fixed by docs/goal-firefox.md and the question is what
# **it** needs. Its closing line is the brief —
#
#   "Firefox needs a specific set. Enumerate what it actually binds and
#    implement that, rather than the 137 the research classified."
#
# — and the only honest way to enumerate that is to run it. So the assertions
# here are mostly of the form "Firefox did X", and where a thing is absent the
# harness records **which of the three answers** it got, because they are not
# the same answer: a client can *fail*, it can *fall back*, or it can *hang*.
#
# WHAT IS DELIBERATELY NOT CLAIMED
#
# Nothing here runs on the real guest, and the `wine` block is Wine. Wine has
# been wrong four times on this project — the number is design.md §13.6's, which
# withdrew the SetProcessDpiAwarenessContext entry with the measurement attached,
# and §14.12 item 6 records why three different counts were in circulation. A
# pass says the code paths are self-consistent; a negative is not a result, and
# §13.6's lesson is the sharper one — a negative from Wine, or from the guest,
# is only evidence if the question was asked where the code will really run.
# Everything in `enumerate`,
# `window` and `menu` runs against the **Linux** build of the compositor, where
# Firefox is a stock libwayland client talking to a stock `wayland-server` —
# which is exactly the right place to ask what a client needs, and says nothing
# about Win32.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

pass=0; fail=0; skip=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
non() { echo "  SKIP  $*"; skip=$((skip + 1)); }
note(){ echo "  ....  $*"; }

RUNDIR=$(mktemp -d /tmp/wc-firefox-XXXXXX)
KEEP=${WC_KEEP:-}
cleanup() {
  [ -n "${XPID:-}" ] && { kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null; }
  [ -n "$KEEP" ] || rm -rf "$RUNDIR"
}
trap cleanup EXIT

FIREFOX=${WC_FIREFOX:-firefox}
# The same prefix `wc-proxy.sh` and `wc-input.sh` use, and deliberately so: a
# fresh prefix costs ten minutes of `wineboot` before anything can be measured,
# and `WINEPREFIX` in the environment overrides it for anyone who needs
# isolation. If two harnesses run at once they share a `wineserver`, which is
# survivable — the distinct `/desktop=` name below is the part that is not
# optional (§8.9).
export WINEPREFIX="${WINEPREFIX:-/tmp/wc-wineprefix}"
export WINEDEBUG="${WINEDEBUG:--all}"

# The output the compositor advertises, and therefore the desktop Firefox
# believes it is on. Any size will do; a fixed one makes the window size in the
# assertions below reproducible.
OUTPUT=1600x1000

have() { command -v "$1" >/dev/null 2>&1; }

# ---------------------------------------------------------------------------
# plumbing
# ---------------------------------------------------------------------------

# run_firefox <dir> <socket> <seconds> [extra compositor args...]
#
# Starts the Linux compositor on its own socket in its own runtime dir, runs
# Firefox against it with a **fresh profile**, and leaves everything behind.
#
# `--no-remote` and a private profile, because a Firefox already running on the
# user's desktop would otherwise take the request and open a tab there instead
# — which looks exactly like a client that connected and drew nothing.
run_firefox() {
  local dir=$1 sock=$2 secs=$3; shift 3
  mkdir -p "$dir/rt" "$dir/profile" "$dir/frames"
  chmod 700 "$dir/rt"

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" \
    --socket "$sock" --timeout $((secs + 30)) --output "$OUTPUT" \
    --dump-frames "$dir/frames" "$@" >"$dir/srv.log" 2>&1 &
  local srv=$!
  for _ in $(seq 1 100); do
    [ -S "$dir/rt/$sock" ] && break
    sleep 0.1
  done
  if [ ! -S "$dir/rt/$sock" ]; then
    echo "NOSOCKET" >"$dir/why"
    kill "$srv" 2>/dev/null
    return 1
  fi

  # `env -u DISPLAY` matters more than it looks: with an X server reachable,
  # `MOZ_ENABLE_WAYLAND=1` is a *preference* and Firefox will quietly use X11 if
  # the Wayland connection disappoints it. Removing `DISPLAY` turns a silent
  # fallback into a visible failure, which is the whole point of the exercise.
  env -u DISPLAY \
    MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
    XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY="$sock" \
    ${WC_WAYLAND_DEBUG:+WAYLAND_DEBUG=1} \
    timeout "$secs" "$FIREFOX" --no-remote --profile "$dir/profile" about:blank \
    >"$dir/fx.log" 2>&1
  echo $? >"$dir/fx.rc"
  sleep 1
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null
  # libwayland's trace is coloured, and every grep below would otherwise have to
  # know about ANSI escapes.
  sed 's/\x1b\[[0-9;]*m//g' "$dir/fx.log" >"$dir/fx.txt"
  return 0
}

# The interfaces the client bound, one per line as `name vN`.
binds() {
  grep -o 'bind([0-9]*, "[^"]*", [0-9]*' "$1" \
    | sed 's/bind([0-9]*, "\([^"]*\)", \([0-9]*\)/\1 v\2/' | sort -u
}

# ---------------------------------------------------------------------------
# enumerate: the point of the stage
# ---------------------------------------------------------------------------

block_enumerate() {
  echo "== enumerate: what Firefox binds, and what it does when a thing is missing"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }

  local dir="$RUNDIR/enum"
  WC_WAYLAND_DEBUG=1 run_firefox "$dir" wc-enum 25 || { bad "the compositor never bound its socket"; return; }
  local b="$dir/binds.txt"
  binds "$dir/fx.txt" >"$b"
  note "bound: $(tr '\n' ' ' <"$b")"

  # The five that are not optional. Each was arrived at by taking it away and
  # watching what broke; the failures are recorded in docs/design.md §13.
  local iface
  for iface in wl_compositor wl_shm wl_subcompositor wl_seat wl_output \
               wl_data_device_manager xdg_wm_base; do
    grep -q "^$iface " "$b" \
      && ok "Firefox bound $iface" \
      || bad "Firefox did not bind $iface"
  done

  # The decoration question, measured rather than assumed. Both protocols are
  # advertised; exactly one is used. docs/design.md §13.7.
  if grep -q '^org_kde_kwin_server_decoration_manager ' "$b"; then
    ok "decorations: Firefox bound KDE's org_kde_kwin_server_decoration_manager"
  else
    bad "decorations: Firefox bound no KDE decoration manager"
  fi
  if grep -q '^zxdg_decoration_manager_v1 ' "$b"; then
    bad "decorations: Firefox bound xdg-decoration, which contradicts §13.7"
  else
    ok "decorations: xdg-decoration was advertised and ignored, as §13.7 says"
  fi
  grep -q 'kde-decoration: client asked for Value(Client)' "$dir/srv.log" \
    && ok "decorations: Firefox asked for client-side, which is what the goal needs" \
    || bad "decorations: Firefox did not ask for client-side decorations"

  # dmabuf. The strategically important one: no `zwp_linux_dmabuf_v1` is
  # advertised by this compositor, and the question is whether Firefox fails,
  # falls back, or hangs. It falls back — to `wl_shm` — and this is the check
  # that says so.
  if grep -q '^zwp_linux_dmabuf_v1 ' "$b"; then
    bad "dmabuf: something advertised zwp_linux_dmabuf_v1; this run is not the fallback case"
  else
    grep -q 'create_pool' "$dir/fx.txt" \
      && ok "dmabuf: absent, and Firefox fell back to wl_shm rather than failing" \
      || bad "dmabuf: absent, and Firefox drew nothing at all"
  fi

  # `xdg_surface.set_window_geometry` — the request stage 8 found nothing
  # honoured. Firefox sends it, so it is a requirement and not a nicety.
  grep -q 'set_window_geometry' "$dir/fx.txt" \
    && ok "Firefox sends xdg_surface.set_window_geometry" \
    || bad "Firefox never sent set_window_geometry"

  # Subsurfaces, which is where all of the browser's pixels live.
  grep -q 'get_subsurface' "$dir/fx.txt" \
    && ok "Firefox creates a wl_subsurface for its content" \
    || bad "Firefox created no subsurface"

  # What a pool costs, which is the number docs/design.md §12.13 item 5 asks
  # for. Reported rather than asserted: it is a property of the client and of
  # what it was doing, and a threshold here would be a threshold on Firefox's
  # mood.
  local pools bytes largest
  pools=$(grep -c 'create_pool' "$dir/fx.txt")
  bytes=$(grep -o 'create_pool([^)]*)' "$dir/fx.txt" | sed 's/.*, \([0-9]*\))/\1/' \
          | awk '{s+=$1} END {print s+0}')
  largest=$(grep -o 'create_pool([^)]*)' "$dir/fx.txt" | sed 's/.*, \([0-9]*\))/\1/' \
          | sort -n | tail -1)
  note "wl_shm: $pools pools, $bytes bytes created in total, largest $largest"
  note "wl_shm: $(grep -c 'wl_shm_pool#[0-9]*\.destroy' "$dir/fx.txt") of them destroyed"
}

# ---------------------------------------------------------------------------
# window: the browser's own pixels
# ---------------------------------------------------------------------------

block_window() {
  echo "== window: Firefox's chrome, measured from outside the compositor"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }
  have magick   || { non "no imagemagick"; return; }

  local dir="$RUNDIR/win"
  run_firefox "$dir" wc-win 30 || { bad "the compositor never bound its socket"; return; }

  grep -q 'xdg_toplevel created' "$dir/srv.log" \
    && ok "Firefox reached xdg_toplevel" \
    || { bad "no xdg_toplevel"; sed 's/^/        /' "$dir/srv.log" | tail -10; return; }
  grep -q 'window 1 title -> "Mozilla Firefox"' "$dir/srv.log" \
    && ok "xdg_toplevel.set_title arrived as \"Mozilla Firefox\"" \
    || bad "no title, or not the expected one"
  grep -q 'window 1 app_id -> "firefox"' "$dir/srv.log" \
    && ok "xdg_toplevel.set_app_id arrived as \"firefox\"" \
    || bad "no app_id"

  # The window geometry rectangle, honoured. Firefox's buffer is 1332x952 on a
  # 1600x1000 output and its window is 1280x900: the difference is GTK's
  # invisible resize margin and drop shadow, which a compositor that blitted
  # the buffer would show. Asserted as "the window is smaller than the buffer",
  # rather than on the exact numbers, because the margin is a theme's.
  local mapped
  mapped=$(grep -o 'window 1 mapped surface [0-9]*x[0-9]*' "$dir/srv.log" | tail -1)
  case "$mapped" in
    "window 1 mapped surface 1280x900") ok "the window is the geometry rectangle: $mapped" ;;
    "") bad "the window never mapped" ;;
    *)  bad "unexpected window size: $mapped (expected the geometry rectangle, 1280x900)" ;;
  esac

  local presents
  presents=$(grep -c 'present window=1' "$dir/srv.log")
  [ "$presents" -ge 2 ] \
    && ok "$presents frames of Firefox's pixels reached the window" \
    || bad "only $presents frame(s) presented"

  # **The measurement that says the scene graph works.** Before it, this frame
  # was 1,268,064 pixels of #000000 — Firefox's toplevel surface is GTK's
  # client-side-decoration surface and is transparent, and every pixel of the
  # browser is in a subsurface. A colour count is what tells the two apart, and
  # it is done by ImageMagick reading a file the compositor wrote.
  local last colours
  last=$(ls "$dir"/frames/window1-*.ppm 2>/dev/null | tail -1)
  if [ -z "$last" ]; then
    bad "no frame was dumped"
    return
  fi
  colours=$(magick "$last" -format %k info: 2>/dev/null)
  colours=${colours:-0}
  if [ "$colours" -ge 50 ]; then
    ok "the frame has $colours distinct colours, so it is a rendered UI and not a black rectangle"
  else
    bad "the frame has only $colours distinct colours — the subsurface is missing"
  fi

  # And that the top of the window is chrome rather than page: Firefox's tab
  # strip is a grey band, the content below it is white. Two samples, far
  # apart, is the cheapest statement that the window is not a flat fill.
  local top bottom
  top=$(magick "$last" -format '%[pixel:p{640,20}]' info: 2>/dev/null)
  bottom=$(magick "$last" -format '%[pixel:p{640,700}]' info: 2>/dev/null)
  note "pixel at 640,20 = $top; at 640,700 = $bottom"
  [ -n "$top" ] && [ "$top" != "$bottom" ] \
    && ok "the chrome and the content area are different colours" \
    || bad "the window is a flat fill"
}

# ---------------------------------------------------------------------------
# menu: the thing stage 6 could not do
# ---------------------------------------------------------------------------

block_menu() {
  echo "== menu: a right-click, an xdg_popup, and a second window"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }

  # 0x111 is `BTN_RIGHT`. A right-click into the content area opens the page
  # context menu, which is chosen over the hamburger button on purpose: it does
  # not depend on hitting a 24-pixel target, so it does not become a test of
  # this harness's arithmetic.
  local dir="$RUNDIR/menu"
  run_firefox "$dir" wc-menu 32 --click 640,500,273,12000 \
    || { bad "the compositor never bound its socket"; return; }

  grep -q '\[click\] window 1 button 0x111' "$dir/srv.log" \
    && ok "the right-click was delivered to the toplevel" \
    || { bad "the click never went out"; return; }

  grep -q 'xdg_popup created' "$dir/srv.log" \
    && ok "Firefox answered it with an xdg_popup" \
    || { bad "no xdg_popup — the click arrived and nothing happened, as in stage 6"
         sed 's/^/        /' "$dir/srv.log" | tail -10; return; }

  local placed
  placed=$(grep -o 'xdg_popup created (#1) -> window 2 at [0-9-]*,[0-9-]* [0-9]*x[0-9]*' "$dir/srv.log")
  [ -n "$placed" ] \
    && ok "the positioner placed it: ${placed#*-> }" \
    || bad "the popup has no position"

  grep -q 'window 2 created' "$dir/srv.log" \
    && ok "a second window was made for it" \
    || bad "the popup got no window"

  local presents
  presents=$(grep -c 'present window=2' "$dir/srv.log")
  [ "${presents:-0}" -ge 1 ] \
    && ok "the menu drew $presents frame(s) of its own pixels" \
    || bad "the popup window never presented anything"

  if have magick; then
    local last colours
    last=$(ls "$dir"/frames/window2-*.ppm 2>/dev/null | tail -1)
    if [ -n "$last" ]; then
      colours=$(magick "$last" -format %k info: 2>/dev/null)
      [ "${colours:-0}" -ge 20 ] \
        && ok "the menu's frame has $colours colours, so it has text in it" \
        || bad "the menu's frame is nearly blank ($colours colours)"
    else
      bad "no frame was dumped for the popup"
    fi
  fi
}

# ---------------------------------------------------------------------------
# wine: the same client, into a real HWND, through the proxy
# ---------------------------------------------------------------------------

block_wine() {
  echo "== wine: Firefox -> wc-proxy -> wc-compositor.exe -> an HWND"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }
  have wine || { non "no wine"; return; }
  have Xvfb || { non "no Xvfb"; return; }
  [ -x "$WIN_BIN/wc-compositor.exe" ] || { non "no wc-compositor.exe; cargo build --target x86_64-pc-windows-gnu"; return; }

  local dir="$RUNDIR/wine"
  mkdir -p "$dir/rt" "$dir/frames" "$dir/pools" "$dir/keymaps" "$dir/profile"
  chmod 700 "$dir/rt"

  # A Linux path as Wine spells it. `winepath` would be the polite way and it
  # blocks for the length of a `wineserver` start; `Z:` is the drive Wine maps
  # the root onto and the substitution is exact.
  winp() { local p=$1; echo "Z:${p//\//\\}"; }

  Xvfb :$WC_X_DISPLAY -screen 0 1600x1100x24 -nolisten tcp >"$dir/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$WC_X_DISPLAY"
  local up=""
  for _ in $(seq 1 100); do
    xwd -root -silent >/dev/null 2>&1 && { up=1; break; }
    sleep 0.1
  done
  [ -n "$up" ] || { bad "Xvfb never came up"; return; }

  # §8.9's gap before starting, for the reason `wc-proxy.sh` records: a
  # `wineserver` that is still shutting down takes the next `wine explorer` into
  # a dying desktop, where `CreateWindowExW` fails for every visible window.
  # **A desktop name that has never been used before**, which is §8.9 and cost
  # two runs to rediscover here. A `wine explorer /desktop=NAME` that reuses the
  # name of a desktop still winding down attaches to the dying one, and then
  # `CreateWindowExW` fails for every visible top-level window — as
  # `X Error: BadWindow ... X_CreateWindow` in the loader log, and as a
  # compositor that logs `xdg_toplevel created` and simply never makes an HWND.
  # Read as a finding rather than as a harness detail, that is §8.11 item 1's
  # failure mode arriving as a test artefact.
  sleep 3
  local desk="wc-firefox-$$-$RANDOM"
  local sock=wl-fx
  # A previous run's node, which `afunix` will not bind over: the error is
  # `OS Error 433`, which says nothing about a stale file.
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null
  wine explorer "/desktop=$desk,1600x1050" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 150 --output "$OUTPUT" \
    --dump-frames "$(winp "$dir/frames")" \
    --shared-dir "$(winp "$dir/keymaps")" --shared-peer-prefix "$dir/keymaps/" \
    --log "$(winp "$dir/srv.log")" >"$dir/loader.log" 2>&1 &
  local srv=$!

  local node=""
  for _ in $(seq 1 300); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$sock" 2>/dev/null | head -1)
    [ -n "$node" ] && break
    sleep 0.2
  done
  if [ -z "$node" ]; then
    bad "the Windows compositor never created its socket node"
    sed 's/^/        /' "$dir/srv.log" 2>/dev/null | head -10
    kill "$srv" 2>/dev/null
    return
  fi
  ok "wc-compositor.exe is listening on an AF_UNIX node inside the prefix"

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --copy pool --transport shared-file --pool-dir "$dir/pools" \
    --peer-prefix "$(winp "$dir/pools")\\" \
    --accept-timeout 40 --timeout 120 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 1

  env -u DISPLAY MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland \
    XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout 70 "$FIREFOX" --no-remote --profile "$dir/profile" about:blank \
    >"$dir/fx.log" 2>&1 &
  local cli=$!

  # Two shots, well apart, for stage 3's reason: one cannot tell a live window
  # from a window showing one frozen frame.
  sleep 30
  xwd -root -silent -out "$dir/s1.xwd" 2>/dev/null && magick "$dir/s1.xwd" "$dir/s1.png" 2>/dev/null
  sleep 6
  xwd -root -silent -out "$dir/s2.xwd" 2>/dev/null && magick "$dir/s2.xwd" "$dir/s2.png" 2>/dev/null

  kill "$cli" 2>/dev/null; wait "$cli" 2>/dev/null
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

  grep -q 'xdg_toplevel created' "$dir/srv.log" 2>/dev/null \
    && ok "Firefox reached xdg_toplevel through the proxy" \
    || { bad "no xdg_toplevel on the Windows side"
         sed 's/^/        /' "$dir/srv.log" 2>/dev/null | tail -12
         sed 's/^/        /' "$dir/proxy.log" | tail -8; return; }

  grep -q 'window 1 created (HWND' "$dir/srv.log" \
    && ok "a real HWND was created for it" \
    || bad "no HWND"

  # The Win32 platform does not log a line per present — that is the headless
  # platform's `--dump-frames` bookkeeping. What it does log is the count when
  # the toplevel goes away, which is the same number from the same counter.
  local presents
  presents=$(grep -o '([0-9]* frames presented)' "$dir/srv.log" | grep -o '[0-9]*' | tail -1)
  [ "${presents:-0}" -ge 2 ] \
    && ok "$presents frames of Firefox's pixels reached the Windows window" \
    || bad "only ${presents:-0} frame(s) presented"

  # The window was sized from the geometry rectangle rather than from the
  # buffer. On Win32 this is the line `map_window` prints.
  local mapped
  mapped=$(grep -o 'window 1 mapped [0-9]*x[0-9]*' "$dir/srv.log" | tail -1)
  case "$mapped" in
    "window 1 mapped 1280x900") ok "the HWND is the geometry rectangle: $mapped" ;;
    "") bad "the window never mapped" ;;
    *)  bad "unexpected window size: $mapped (expected 1280x900)" ;;
  esac

  # The rule. A run that got the pixels there by passing a descriptor would
  # produce an identical picture and fail this.
  local counts
  counts=$(grep -o 'mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice' "$dir/srv.log" | tail -1)
  case "$counts" in
    "mapped: 0 section, "[1-9]*" shared-file, 0 arena-slice")
      ok "the compositor opened only shared files by name ($counts)" ;;
    *)
      bad "wrong transport served the pixels: ${counts:-nothing recorded}" ;;
  esac

  if have magick && [ -f "$dir/s2.png" ]; then
    local colours
    colours=$(magick "$dir/s2.png" -format %k info: 2>/dev/null)
    [ "${colours:-0}" -ge 200 ] \
      && ok "the X screenshot of the Wine desktop has $colours colours in it" \
      || bad "the screenshot is nearly empty ($colours colours)"
    note "screenshots: $dir/s1.png $dir/s2.png (WC_KEEP=1 to keep them)"
  fi
}

WC_X_DISPLAY=${WC_X_DISPLAY:-77}

case "${1:-all}" in
  enumerate) block_enumerate ;;
  window)    block_window ;;
  menu)      block_menu ;;
  wine)      block_wine ;;
  all)       block_enumerate; block_window; block_menu; block_wine ;;
  *) echo "usage: $0 [enumerate|window|menu|wine|all]"; exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
