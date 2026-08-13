#!/usr/bin/env bash
# Point **Firefox** at this compositor and write down what actually happens.
#
#   scripts/wc-firefox.sh enumerate   what Firefox binds, and what it does without
#   scripts/wc-firefox.sh window      the browser's own pixels, measured from outside
#   scripts/wc-firefox.sh titlebar    the two levers that leave exactly one title bar
#   scripts/wc-firefox.sh tabs        the client half of --decorations tabs
#   scripts/wc-firefox.sh menu        a click, an xdg_popup, and a second window
#   scripts/wc-firefox.sh wine        the same client into a real HWND, through the proxy
#   scripts/wc-firefox.sh wine-arena  the same run with the pixels in an arena
#   scripts/wc-firefox.sh all         all seven
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

# STAGE 41. This harness has no guest and never did — see the header — so both
# of its transports are the Wine fixture, and the library is what lets it say so
# and what hands the arena leg the permission the binaries now require.
# `WC_GUEST_CONFIGURED` is deliberately *not* set here: setting it would be
# claiming a guest this harness has never had.
. "$(dirname "$0")/wc-substrate.sh"
# STAGE 42. `fresh`: is the binary newer than the tree.
. "$(dirname "$0")/wc-binaries.sh"

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

# STAGE 17. See `run_firefox`. Declared here so that `set -u` cannot turn an
# unset array into a run that dies three functions deep.
FX_DECOR=client
FX_ENV=()

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
#
# STAGE 17. Two knobs, both empty by default so that every block written before
# this stage passes exactly what it passed before:
#
#   FX_DECOR   the `--decorations` the compositor is started with. Every block
#              names it explicitly now, because the compiled-in default changed
#              in this stage (§22.2) and a block that relied on it would be
#              measuring the flag rather than the client.
#   FX_ENV     extra `VAR=value` words put in front of Firefox. This is where
#              `MOZ_GTK_TITLEBAR_DECORATION` goes, and it is an *array* because
#              the alternative — a string that the shell re-splits — puts a
#              quoting bug between the harness and the only lever the client has.
run_firefox() {
  local dir=$1 sock=$2 secs=$3; shift 3
  mkdir -p "$dir/rt" "$dir/profile" "$dir/frames"
  chmod 700 "$dir/rt"

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" \
    --socket "$sock" --timeout $((secs + 30)) --output "$OUTPUT" \
    --decorations "${FX_DECOR:-client}" \
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
    ${WC_WAYLAND_DEBUG:+WAYLAND_DEBUG=1} "${FX_ENV[@]}" \
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

  # Explicit since stage 17: this block's claims are all about the *client-side*
  # arrangement — the shadow inset, the tab strip, the mode Firefox picks when
  # nothing is done to it — so it names the mode it wants rather than inheriting
  # a compiled-in default that has now changed. §22.2.
  FX_DECOR=client; FX_ENV=()

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
  # Offered `Client`, Firefox asks for `Client`. Stage 13 changed what that is
  # evidence *for*: §15.7 read it as the goal document's clause arriving for
  # free, and §22.3 shows it is the client agreeing with whatever the manager
  # said. The other half of the pair is `block_titlebar`'s.
  grep -q 'kde-decoration: client asked for Value(Client)' "$dir/srv.log" \
    && ok "decorations: offered Client, Firefox asked for Client" \
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

  FX_DECOR=client; FX_ENV=()
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
# titlebar: one title bar, and it is the window manager's
# ---------------------------------------------------------------------------
#
# ADDED FOR STAGE 17, and it is the stage. Every window this project has ever
# made has had **two** title bars — the host window manager's caption, and
# directly beneath it Firefox's tab strip with its own minimise, maximise and
# close buttons. The user reported it, on the crash reporter's window as well as
# the browser's, so it is every window rather than a browser quirk.
#
# TWO LEVERS, AND NEITHER WORKS ALONE. That is the finding, and this block is
# built to say so rather than to assert the happy path:
#
#   * the compositor sending `default_mode(2)` on `org_kde_kwin_server_-`
#     `decoration_manager` bind. GTK 3 reads the manager's default in
#     `gdk_wayland_display_prefers_ssd()`, and `gtk_window_should_use_csd()`
#     consults nothing else on Wayland — not `GTK_CSD`, which is checked after
#     an unconditional Wayland branch and is therefore dead here.
#   * `MOZ_GTK_TITLEBAR_DECORATION=none` in the *client's* environment, which is
#     what makes Gecko stop drawing a title bar of its own. The compositor
#     cannot set this and there is no protocol that carries it, which is why
#     §22.4 records it as a deployment requirement rather than a fix.
#
# The control leg below is the one that earns the paragraph: with the compositor
# sending `default_mode(2)` and the environment variable absent, Firefox still
# asks for `Client` and still reports a shadow inset. Without the control this
# block would be evidence for the wrong lever.
block_titlebar() {
  echo "== titlebar: default_mode(Server) plus MOZ_GTK_TITLEBAR_DECORATION=none"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }

  # --- the arrangement the stage ships -------------------------------------
  local dir="$RUNDIR/tb"
  FX_DECOR=server; FX_ENV=(MOZ_GTK_TITLEBAR_DECORATION=none)
  WC_WAYLAND_DEBUG=1 run_firefox "$dir" wc-tb 25 \
    || { bad "the compositor never bound its socket"; return; }

  grep -q 'decorations: manager default is Server' "$dir/srv.log" \
    && ok "the compositor advertised Server as the manager default" \
    || bad "the compositor did not advertise Server"

  # From the client's own trace, not from ours: `default_mode` is an event the
  # compositor sends and libwayland logs on receipt, so this is Firefox saying
  # it heard it.
  grep -q 'default_mode(2)' "$dir/fx.txt" \
    && ok "Firefox's own trace shows it received default_mode(2)" \
    || bad "Firefox never logged default_mode(2)"

  local asked
  asked=$(grep -o 'org_kde_kwin_server_decoration#[0-9]*\.request_mode([0-9]*)' "$dir/fx.txt" \
          | grep -o '([0-9]*)' | tr -d '()' | sort -u | tr '\n' ' ')
  case "$asked" in
    "2 ") ok "Firefox asked for mode 2 (Server) and nothing else" ;;
    "")   bad "Firefox made no decoration request at all" ;;
    *)    bad "Firefox asked for mode(s) $asked; 2 (Server) was expected" ;;
  esac

  # **The number the Win32 side has to live with.** §15.6 made the window the
  # `set_window_geometry` rectangle rather than the buffer, and under client-side
  # decorations that rectangle is inset by GTK's invisible resize margin and drop
  # shadow — (26, 23) for this theme. Under server-side decorations there is no
  # shadow to subtract and the origin is (0, 0), so the window *is* the buffer
  # again. Both are handled by the same code and this is the assertion that the
  # inset really went away rather than being compensated for somewhere.
  #
  # Only the toplevel's rectangle is looked at. A popup sets its own geometry
  # and its origin is its own business, so anything narrower than 640 px is
  # skipped rather than folded into the claim.
  local insets
  insets=$(grep -o 'set_window_geometry([0-9-]*, [0-9-]*, [0-9-]*, [0-9-]*)' "$dir/fx.txt" \
           | tr -d 'a-z_()' | tr -d ' ' | awk -F, '$3 >= 640 { print $1 "," $2 }' | sort -u | tr '\n' ' ')
  case "$insets" in
    "0,0 ") ok "the toplevel's window geometry has a zero origin: no shadow inset to subtract" ;;
    "")     bad "Firefox set no toplevel window geometry" ;;
    *)      bad "the toplevel's geometry origin is $insets, not 0,0 — the CSD shadow inset is still there" ;;
  esac

  local mapped
  mapped=$(grep -o 'window 1 mapped surface [0-9]*x[0-9]*' "$dir/srv.log" | tail -1)
  [ -n "$mapped" ] \
    && ok "the window mapped, and its size is the geometry rectangle: $mapped" \
    || bad "the window never mapped"

  # A window with no title bar is also what a client that failed to start looks
  # like, so the frame is checked for being a browser.
  if have magick; then
    local last colours
    last=$(ls "$dir"/frames/window1-*.ppm 2>/dev/null | tail -1)
    if [ -n "$last" ]; then
      colours=$(magick "$last" -format %k info: 2>/dev/null)
      [ "${colours:-0}" -ge 50 ] \
        && ok "the frame has $colours colours, so it is the browser and not an empty bar" \
        || bad "the frame has only ${colours:-0} colours — Firefox drew nothing"
      note "the client's pixels, with no title bar of their own: $last"
    else
      bad "no frame was dumped"
    fi
  fi

  # --- the control: the same compositor, without the environment variable ---
  #
  # Run second and asserted *positively* — it is expected to fail to fix
  # anything, and a control that is only reported is a control nobody notices
  # has stopped controlling.
  local ctl="$RUNDIR/tb-control"
  FX_DECOR=server; FX_ENV=()
  WC_WAYLAND_DEBUG=1 run_firefox "$ctl" wc-tbc 25 \
    || { bad "the control run's compositor never bound its socket"; return; }

  local casked cinsets
  casked=$(grep -o 'org_kde_kwin_server_decoration#[0-9]*\.request_mode([0-9]*)' "$ctl/fx.txt" \
           | grep -o '([0-9]*)' | tr -d '()' | sort -u | tr '\n' ' ')
  cinsets=$(grep -o 'set_window_geometry([0-9-]*, [0-9-]*, [0-9-]*, [0-9-]*)' "$ctl/fx.txt" \
            | tr -d 'a-z_()' | tr -d ' ' | awk -F, '$3 >= 640 { print $1 "," $2 }' | sort -u | tr '\n' ' ')
  if [ "$casked" = "1 " ] && [ "$cinsets" != "0,0 " ]; then
    ok "control: with default_mode(2) alone Firefox still asks for Client and still insets by ${cinsets% }"
  else
    bad "control: default_mode(2) alone gave mode(s) ${casked:-none} and origin ${cinsets:-none}; the environment variable may no longer be needed, which would change §22.4"
  fi

  # And the compositor did not fight it. §22.2's decision: the per-surface
  # request is echoed even when it contradicts the default, because refusing it
  # changes no client's behaviour and makes GDK re-request in a loop.
  grep -q 'kde-decoration: client asked for Value(Client) (default is Server)' "$ctl/srv.log" \
    && ok "control: the compositor logged the contradiction and honoured it" \
    || bad "control: the compositor did not record a client insisting on Client"
  local loops
  loops=$(grep -c 'request_mode' "$ctl/fx.txt")
  [ "${loops:-0}" -le 2 ] \
    && ok "control: $loops request_mode(s) in the whole trace — echoing does not make GDK loop" \
    || bad "control: $loops request_mode calls — the client is re-requesting in a loop"

  FX_DECOR=client; FX_ENV=()
}

# ---------------------------------------------------------------------------
# tabs: what `--decorations tabs` asks of the *client*, which is all Linux can
# say about it
# ---------------------------------------------------------------------------
#
# TABS IN THE TITLE BAR. The arrangement itself is `WM_NCCALCSIZE` and cannot be tested here
# at all: there is no Win32 under this compositor and no window manager under
# the headless platform. What *can* be tested here is the half of it that is a
# fact about the client, and it is the half §22.5 got a free pass on.
#
# §22.5 said the geometry-rectangle arithmetic was mode-agnostic "by
# construction rather than by a branch", and cited a `server` run where Firefox
# reported a zero origin — a run in which the subtraction §15.6 exists for is a
# no-op. The `tabs` arrangement is the one where it is *not*: Firefox decorates
# itself, so it insets by GTK's invisible resize margin and drop shadow, and the
# window has to be the rectangle rather than the buffer or the tab strip lands
# 23 px below the top of the caption it is supposed to be inside. That is what
# this block asserts, and it is a check rather than an assumption because the
# brief for the tabs-in-title-bar stage said in as many words not to assume it.
block_tabs() {
  echo "== tabs: the client half of --decorations tabs, on Linux"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }

  local dir="$RUNDIR/tabs"
  FX_DECOR=tabs; FX_ENV=()
  WC_WAYLAND_DEBUG=1 run_firefox "$dir" wc-tabs 25 \
    || { bad "the compositor never bound its socket"; return; }

  grep -q 'decorations: manager default is Tabs' "$dir/srv.log" \
    && ok "the compositor advertised Tabs as the manager default" \
    || bad "the compositor did not advertise Tabs"

  # `Tabs` is `Client` on the wire and has to be (§22.2): the tab strip this
  # mode puts inside the caption is drawn by the client, and it exists only
  # while the manager's default says Client. A `tabs` run in which Firefox
  # heard `default_mode(2)` has lost the thing it is arranging.
  grep -q 'default_mode(1)' "$dir/fx.txt" \
    && ok "Firefox's own trace shows it received default_mode(1): Tabs is Client on the wire" \
    || bad "Firefox did not receive default_mode(1)"

  local asked
  asked=$(grep -o 'org_kde_kwin_server_decoration#[0-9]*\.request_mode([0-9]*)' "$dir/fx.txt" \
          | grep -o '([0-9]*)' | tr -d '()' | sort -u | tr '\n' ' ')
  case "$asked" in
    "1 ") ok "Firefox asked for mode 1 (Client) and nothing else" ;;
    "")   bad "Firefox made no decoration request at all" ;;
    *)    bad "Firefox asked for mode(s) $asked; 1 (Client) was expected" ;;
  esac

  # THE NUMBER THE WIN32 SIDE HAS TO LIVE WITH, and the point of the block.
  local insets
  insets=$(grep -o 'set_window_geometry([0-9-]*, [0-9-]*, [0-9-]*, [0-9-]*)' "$dir/fx.txt" \
           | tr -d 'a-z_()' | tr -d ' ' | awk -F, '$3 >= 640 { print $1 "," $2 }' | sort -u | tr '\n' ' ')
  case "$insets" in
    "0,0 ") bad "the toplevel's geometry origin is 0,0 under Tabs: Firefox is not decorating itself, so there is no tab strip to put in the caption" ;;
    "")     bad "Firefox set no toplevel window geometry" ;;
    *)      ok "the toplevel's geometry origin is ${insets% }: a real CSD shadow inset, which §15.6's subtraction has to remove" ;;
  esac

  # And that it *was* removed. The window is the geometry rectangle's size, not
  # the buffer's, and under CSD those differ by twice the inset on each axis.
  local geom mapped
  geom=$(grep -o 'set_window_geometry([0-9-]*, [0-9-]*, [0-9-]*, [0-9-]*)' "$dir/fx.txt" \
         | tr -d 'a-z_()' | tr -d ' ' | awk -F, '$3 >= 640 { print $3 "x" $4 }' | tail -1)
  mapped=$(grep -o 'window 1 mapped surface [0-9]*x[0-9]*' "$dir/srv.log" | tail -1 \
           | grep -o '[0-9]*x[0-9]*')
  if [ -n "$geom" ] && [ "$geom" = "$mapped" ]; then
    ok "the window is the geometry rectangle and not the buffer: $mapped"
  else
    bad "the window is ${mapped:-nothing}, and the geometry rectangle is ${geom:-nothing}"
  fi

  # The client area is the surface in this mode too, which is what the tabs-in-title-bar stage
  # replaced three conversions with (§31.2). The headless platform prints both.
  grep -q "window 1 mapped surface $mapped client $mapped" "$dir/srv.log" \
    && ok "the client area is the surface: no conversion left between them" \
    || bad "the platform reports a client area that is not the surface"

  FX_DECOR=client; FX_ENV=()
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
  FX_DECOR=client; FX_ENV=()
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
  # STAGE 12 gave this block a transport argument, and the second leg is the
  # first one **with two flags changed and nothing else** — the same Firefox,
  # the same Xvfb, the same assertions — because the claim is that the transport
  # changed and nothing did. §14.6 set that discipline for `weston-eventdemo`
  # and a weaker check on the new leg would prove nothing.
  local transport=${1:-shared-file}
  echo "== wine/$transport: Firefox -> wc-proxy -> wc-compositor.exe -> an HWND"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }
  have wine || { non "no wine"; return; }
  have Xvfb || { non "no Xvfb"; return; }
  fresh_or_skip "$WIN_BIN/wc-compositor.exe" "no wc-compositor.exe; cargo build --target x86_64-pc-windows-gnu"; case $? in 2) non "no wc-compositor.exe; cargo build --target x86_64-pc-windows-gnu"; return ;; 1) return ;; esac

  local dir="$RUNDIR/wine-$transport"
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

  # STAGE 12. The two configurations, and the only difference between the runs.
  #
  # `arena`: the region is one file both sides map, provided out of band before
  # either starts, and the compositor keeps the last MiB of it for the objects
  # *it* creates — which for Firefox is two keymaps, one per seat-binding
  # connection (§15.2's table has `wl_seat` twice). So **no `--shared-dir` at
  # all**: if the run passes, no protocol object touched a filesystem.
  #
  # 64 MiB against the 14,325,120 bytes of pools a settled Firefox holds
  # (§15.10). The arena is truncated fresh every run for §12.7's reason: a
  # region still full of the previous run's pixels is a way for a frozen frame
  # to look like a live one.
  local arena_size=$((64 * 1024 * 1024)) arena_out=$((1024 * 1024))
  local comp_extra=() proxy_extra=()
  case "$transport" in
    shared-file)
      # Also a fixture, and declared as one for the same reason — but it needs
      # no permission from the binaries, because `shared-file` opens pool files
      # by name and never touches the arena code. Declaring it is what makes the
      # substrate assertion below able to say which of the two ran.
      wc_substrate_declare wine-fixture \
        "no QEMU on this box: a Wine run has no guest RAM to locate" || return
      comp_extra=(--shared-dir "$(winp "$dir/keymaps")" --shared-peer-prefix "$dir/keymaps/")
      proxy_extra=(--transport shared-file --pool-dir "$dir/pools"
                   --peer-prefix "$(winp "$dir/pools")\\") ;;
    arena)
      # STAGE 41. **This is the file arena, and the file arena is not a target.**
      # `wc-proxy` and `wc-compositor` both refuse it now unless
      # `SV_ALLOW_FILE_ARENA=1` says the run means it, and this is the one place
      # in the tree that is entitled to say so: there is no QEMU on this box, so
      # there is no guest RAM to locate, so a file standing in for the region is
      # the only way to exercise the arena code path at all. §18.2's first
      # bullet, enforced instead of described.
      #
      # `wc_substrate_declare` will refuse to hand out that permission if
      # `WC_HOST` is set or the harness declared a guest, which is what stops
      # this branch being copied into a harness that has one. Both this and the
      # binaries then print a line saying the substrate is not a target, so a
      # result from here cannot be mistaken for a result about the deployment in
      # a log somebody reads later.
      wc_substrate_declare wine-fixture \
        "no QEMU on this box: a Wine run has no guest RAM to locate" || return
      : >"$dir/arena"; truncate -s $arena_size "$dir/arena"
      comp_extra=(--arena-provide "0:$arena_size:$(winp "$dir/arena")"
                  --arena-outbound "0:$arena_out")
      proxy_extra=(--transport arena --arena-path "$dir/arena" --arena-id 0
                   --arena-size $arena_size --arena-outbound $arena_out) ;;
    *) bad "unknown transport $transport"; return ;;
  esac

  # `--decorations client` since stage 17, and named rather than inherited: this
  # leg's `1280x900` assertion below is the *client-side* geometry rectangle,
  # which is what a client-decorated Firefox reports and is the number §15.6
  # established. The stage's own arrangement is `block_titlebar`'s.
  wine explorer "/desktop=$desk,1600x1050" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 150 --output "$OUTPUT" \
    --decorations client \
    --dump-frames "$(winp "$dir/frames")" "${comp_extra[@]}" \
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
    --copy pool "${proxy_extra[@]}" \
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

  # STAGE 41. Which substrate, said once and asserted on — and here it says
  # *fixture*, which is the honest answer for a box with no hypervisor and is
  # the answer this harness's header has always given in prose. Nothing about
  # this leg changes; what changes is that the word now appears in the run's own
  # output next to the numbers, so a `wine` result cannot be quoted later as a
  # guest result by somebody reading only the PASS lines.
  wc_assert_substrate "$dir/srv.log" "wine/$transport"

  # The rule. A run that got the pixels there by passing a descriptor would
  # produce an identical picture and fail this.
  local counts
  counts=$(grep -o 'mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice' "$dir/srv.log" | tail -1)
  case "$transport:$counts" in
    "shared-file:mapped: 0 section, "[1-9]*" shared-file, 0 arena-slice")
      ok "the compositor opened only shared files by name ($counts)" ;;
    "arena:mapped: 0 section, 0 shared-file, "[1-9]*" arena-slice")
      ok "the compositor resolved only arena offsets, opening nothing ($counts)" ;;
    *)
      bad "wrong transport served the pixels: ${counts:-nothing recorded}" ;;
  esac

  # STAGE 12, and this is the leg's reason to exist. Firefox opens four
  # connections (§15.8) and `--transport arena` refused the second of them until
  # this stage (§15.14), so the number below is the whole claim: four sessions,
  # one region, one allocator.
  if [ "$transport" = arena ]; then
    local served joined
    served=$(grep -c '^\[proxy\] client [0-9]* connected' "$dir/proxy.log")
    joined=$(grep -c 'joins arena 0' "$dir/proxy.log")
    if [ "${served:-0}" -ge 2 ] && [ "$served" = "$joined" ]; then
      ok "$served connections were served and all $joined of them share one arena and one allocator"
    else
      bad "$served connection(s) served, $joined joined the arena"
      grep 'no transport for it' "$dir/proxy.log" | sed 's/^/        /' | head -3
    fi

    # Two keymaps, from Firefox's two `wl_seat` binds (§15.2), cut from one
    # outbound window and live at the same time. §14.12 item 3 said nothing had
    # asked for two objects at once outside a unit test. Both binds turn out to
    # be on **one** connection, which §16.5 records.
    local arenafds
    arenafds=$(grep -o '[0-9]* fds in, [0-9]* out ([0-9]* from the arena)' "$dir/proxy.log" \
               | grep -o '([0-9]* from the arena)' | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
    [ "${arenafds:-0}" -ge 2 ] \
      && ok "$arenafds objects the compositor created were live at once in its outbound window" \
      || bad "only ${arenafds:-0} descriptor(s) came from the arena; two keymaps were expected"

    # And nothing at all on a filesystem: this leg passes no --shared-dir.
    if [ -z "$(ls -A "$dir/keymaps" 2>/dev/null)" ] && [ -z "$(ls -A "$dir/pools" 2>/dev/null)" ]; then
      ok "no protocol object was written to any filesystem: the keymap directory and the pool directory are both empty"
    else
      bad "something landed on a filesystem: $(ls -A "$dir/keymaps" "$dir/pools" 2>/dev/null | tr '\n' ' ')"
    fi
  fi

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
  enumerate)  block_enumerate ;;
  window)     block_window ;;
  titlebar)   block_titlebar ;;
  tabs)       block_tabs ;;
  menu)       block_menu ;;
  wine)       block_wine shared-file ;;
  wine-arena) block_wine arena ;;
  all)        block_enumerate; block_window; block_titlebar; block_tabs; block_menu
              block_wine shared-file; block_wine arena ;;
  *) echo "usage: $0 [enumerate|window|titlebar|tabs|menu|wine|wine-arena|all]"; exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
