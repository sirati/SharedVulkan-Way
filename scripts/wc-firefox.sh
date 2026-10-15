#!/usr/bin/env bash
# Point **Firefox** at this compositor and write down what actually happens.
#
#   scripts/wc-firefox.sh enumerate   what Firefox binds, and what it does without
#   scripts/wc-firefox.sh needs       what it binds when a page asks it to
#   scripts/wc-firefox.sh window      the browser's own pixels, measured from outside
#   scripts/wc-firefox.sh titlebar    the two levers that leave exactly one title bar
#   scripts/wc-firefox.sh auto        Firefox under the compiled-in default, vs server
#   scripts/wc-firefox.sh tabs        the client half of --decorations tabs
#   scripts/wc-firefox.sh tabs-default  §82: tabs in the caption with no flag, the launcher's profile
#   scripts/wc-firefox.sh wine-tabs   §82: the same in a Wine HWND: hit test, drag, double-click
#   scripts/wc-firefox.sh menu        a click, an xdg_popup, and a second window
#   scripts/wc-firefox.sh wine        the same client into a real HWND, through the proxy
#   scripts/wc-firefox.sh wine-arena  the same run with the pixels in an arena
#   scripts/wc-firefox.sh all         all eight
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
# been wrong four times on this project — the number is docs/waycompose-design.md §13.6's, which
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

# §51. Whether a hypervisor is in the path, declared rather than left unknown.
# This harness is same-box, so `wc_latency_claim` will refuse any assertion here
# whose truth depends on cross-VM timing -- which is the point: §27.6 drew one
# from a harness exactly like this one.
. "$(dirname "$0")/wc-topology.sh"
wc_topology_declare same-box \
  "every block runs on this box: the Linux compositor directly, and wc-compositor.exe under Wine" || exit 1
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
#
#              **`FX_DECOR=none` passes no `--decorations` at all**, added by
#              §53 for the one block that has to measure the compiled-in
#              default rather than a named value. That is the exact opposite of
#              every other block's requirement and it is why it needs a
#              sentinel: §49 changed the default to `auto` and argued it is
#              indistinguishable from `server` for a client that negotiates,
#              and *every* block here names `FX_DECOR`, so no run had ever
#              exercised the argument. §49.9 listed it as unmeasured; this is
#              the block that measures it.
#   FX_ENV     extra `VAR=value` words put in front of Firefox. This is where
#              `MOZ_GTK_TITLEBAR_DECORATION` goes, and it is an *array* because
#              the alternative — a string that the shell re-splits — puts a
#              quoting bug between the harness and the only lever the client has.
run_firefox() {
  local dir=$1 sock=$2 secs=$3; shift 3
  mkdir -p "$dir/rt" "$dir/profile" "$dir/frames"
  chmod 700 "$dir/rt"

  # `none` means "name nothing", so the compositor uses its compiled-in
  # default. See the FX_DECOR paragraph above.
  local decor=()
  [ "${FX_DECOR:-client}" = none ] || decor=(--decorations "${FX_DECOR:-client}")

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-compositor" \
    --socket "$sock" --timeout $((secs + 30)) --output "$OUTPUT" \
    "${decor[@]}" \
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
    timeout "$secs" "$FIREFOX" --no-remote --profile "$dir/profile" "${FX_URL:-about:blank}" \
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
  # advertised; exactly one is used. docs/waycompose-design.md §13.7.
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

  # What a pool costs, which is the number docs/waycompose-design.md §12.13 item 5 asks
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
# --------------------------------------------------------------------- auto --
# STAGE 53. Firefox under the compiled-in default, which nothing had ever run.
#
# §49 made `--decorations auto` the default and argued it is *byte-for-byte*
# `server` for a client that negotiates: `Decorations::kde()` answers
# `KdeDefaultMode::Server` for `Auto` and `Server` alike, `xdg()` answers
# `ServerSide` for both, and `fold_caption` never folds a `Negotiated` window.
# Firefox binds KDE's manager, so it is `Negotiated` on every run.
#
# That argument was unmeasured, and §49.9 said so. It could not be measured by
# accident either: every block in this file names `FX_DECOR`, deliberately, for
# the reason the settings block gives — so the compiled-in default is the one
# thing this harness was constructed never to exercise.
#
# The claim is an *equivalence*, so the block is a pair. `auto` and `server` are
# run minutes apart with the same environment, and the assertions are on the
# things Firefox can see: what it was told, what it asked for, and where it put
# --------------------------------------------------------------------- needs -
# STAGE 56. What Firefox binds when it is asked to do something.
#
# `enumerate` runs `about:blank`, and its list has been read ever since as "the
# protocols Firefox needs" — including by the task that asks for them to be
# implemented fully. **It is a floor, not the set.** A browser binds a protocol
# when a page makes it necessary, and `about:blank` makes almost nothing
# necessary: no text field, so no `text-input`; no pointer lock, so no
# `pointer-constraints` and no `relative-pointer`; no video, so nothing that
# would want `viewporter`.
#
# That matters for the gap list rather than being a curiosity. WLCS's skips are
# grouped by the protocol that caused them, and the four largest gaps —
# `wp_viewporter` 23, `zwp_pointer_constraints_v1` 15, `zwp_text_input_manager_v2`
# 12, `zwp_text_input_manager_v3` 11 — are *all absent* from the `about:blank`
# list. Read as "the subset that matters", that list says those 61 skips are
# somebody else's problem. This block exists to find out whether that is true.
#
# The page is local and offline: this harness must not depend on a network, and
# a remote page would make the measurement a fact about a website.
block_needs() {
  echo "== needs: what Firefox binds when a page asks it to"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }

  local dir="$RUNDIR/needs"
  mkdir -p "$dir"
  # Each element is here to provoke exactly one protocol, and the comment says
  # which. Nothing here is interactive: Firefox binds a manager when it decides
  # the capability may be used, which for these is at layout or focus time.
  cat >"$dir/needs.html" <<'HTML'
<!doctype html><meta charset=utf-8><title>needs</title>
<body>
<!-- text-input v2/v3: a focused editable field is what makes an IME relevant -->
<input id=t autofocus placeholder=type>
<textarea rows=2>editable</textarea>
<div contenteditable>editable div</div>
<!-- pointer-constraints + relative-pointer: pointer lock is requested on click,
     but Gecko binds the managers when a document that *can* lock is loaded -->
<canvas id=c width=200 height=120></canvas>
<!-- viewporter / fractional-scale: a scaled video surface is the usual reason -->
<video id=v width=160 height=90 muted autoplay loop></video>
<script>
  document.getElementById('t').focus();
  const c = document.getElementById('c');
  c.addEventListener('click', () => c.requestPointerLock());
  // Touch the APIs so Gecko instantiates the machinery behind them.
  void (document.pointerLockElement, navigator.keyboard, window.devicePixelRatio);
</script>
HTML

  FX_DECOR=client; FX_ENV=(); FX_URL="file://$dir/needs.html"
  WC_WAYLAND_DEBUG=1 run_firefox "$dir" wc-needs 30 \
    || { bad "the compositor never bound its socket"; FX_URL=; return; }
  FX_URL=

  # Bound interfaces, from libwayland's own decode of the client's requests.
  local bound
  bound=$(grep -o 'wl_registry#[0-9]*\.bind([0-9]*, "[a-z_0-9]*"' "$dir/fx.txt" \
          | sed 's/.*"\(.*\)"/\1/' | sort -u)
  [ -n "$bound" ] || { bad "Firefox bound nothing at all — it did not start"; return; }
  note "bound with a page: $(echo "$bound" | tr '\n' ' ')"

  # The comparison that is the point. `enumerate`'s baseline is recomputed here
  # rather than read from that block's output, so this block stands alone.
  local base="$RUNDIR/needs-base"
  FX_DECOR=client; FX_ENV=()
  WC_WAYLAND_DEBUG=1 run_firefox "$base" wc-needs-base 25 \
    || { bad "the compositor never bound its socket (about:blank)"; return; }
  local blank extra
  blank=$(grep -o 'wl_registry#[0-9]*\.bind([0-9]*, "[a-z_0-9]*"' "$base/fx.txt" \
          | sed 's/.*"\(.*\)"/\1/' | sort -u)
  extra=$(comm -23 <(echo "$bound") <(echo "$blank") | tr '\n' ' ')

  if [ -n "${extra// /}" ]; then
    ok "a page made Firefox bind protocols about:blank never asks for: $extra"
  else
    ok "a page made Firefox bind nothing extra — about:blank's list really is the set"
  fi

  # And the question the gap list needs answered, named protocol by named
  # protocol, so the answer is usable rather than a sentence.
  local p
  for p in zwp_text_input_manager_v3 zwp_text_input_manager_v2 \
           zwp_pointer_constraints_v1 zwp_relative_pointer_manager_v1 \
           wp_viewporter wp_fractional_scale_manager_v1 zxdg_output_manager_v1; do
    if echo "$bound" | grep -qx "$p"; then
      note "ON FIREFOX'S PATH: $p — a WLCS gap this client actually binds"
    else
      note "not bound even with a page: $p"
    fi
  done

  # --- the third leg, and it is what stops the negative being misread --------
  #
  # "Firefox did not bind it" has two causes and they lead opposite ways: the
  # client does not need the protocol, or the client's support for it is behind
  # a preference that is off. Reporting the first when it is the second would
  # tell whoever implements these gaps that they are not on the goal's path,
  # which is the most expensive kind of wrong answer a measurement can give.
  #
  # So the prefs are turned **on** and the page is run again. Gecko's Wayland
  # text-input and pointer-lock switches are the ones that matter here; a pref
  # that does not exist in this build is simply ignored by Firefox, so naming a
  # stale one costs nothing but the line.
  local pref="$RUNDIR/needs-pref"
  mkdir -p "$pref/profile"
  cat >"$pref/profile/user.js" <<'JS'
user_pref("widget.wayland.text-input-v3.enabled", true);
user_pref("widget.wayland-text-input-v3.enabled", true);
user_pref("widget.wayland.pointer-constraints.enabled", true);
user_pref("widget.wayland.fractional-scale.enabled", true);
user_pref("full-screen-api.allow-trusted-requests-only", false);
JS
  FX_DECOR=client; FX_ENV=(); FX_URL="file://$dir/needs.html"
  WC_WAYLAND_DEBUG=1 run_firefox "$pref" wc-needs-pref 30 \
    || { bad "the compositor never bound its socket (prefs on)"; FX_URL=; return; }
  FX_URL=
  local withpref gained
  withpref=$(grep -o 'wl_registry#[0-9]*\.bind([0-9]*, "[a-z_0-9]*"' "$pref/fx.txt" \
             | sed 's/.*"\(.*\)"/\1/' | sort -u)
  gained=$(comm -23 <(echo "$withpref") <(echo "$bound") | tr '\n' ' ')
  if [ -n "${gained// /}" ]; then
    bad "with the Wayland prefs on, Firefox binds more: $gained — the earlier negative was a preference, not a need"
  else
    ok "with the Wayland prefs on as well, Firefox binds nothing further: the negative is about the client, not a switch"
  fi
}

# its window geometry. Asserting only "auto looks right" would pass a compositor
# that had quietly made `server` wrong too.
block_auto() {
  echo "== auto: Firefox under the compiled-in default, and against server"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }

  local a="$RUNDIR/auto" s="$RUNDIR/autoref"

  # Leg 1: name nothing. This is what a deployment that sets no flag gets.
  FX_DECOR=none; FX_ENV=(MOZ_GTK_TITLEBAR_DECORATION=none)
  WC_WAYLAND_DEBUG=1 run_firefox "$a" wc-auto 25 \
    || { bad "the compositor never bound its socket (auto)"; return; }

  grep -q 'decorations: manager default is Auto' "$a/srv.log" \
    && ok "auto: no --decorations was passed and the compositor ran Auto" \
    || { bad "auto: the compositor did not report Auto as its default"; \
         grep -o 'manager default is .*' "$a/srv.log" | sed 's/^/        /'; }

  # The whole of §49.4, from the client's side: Auto is Server on the wire.
  grep -q 'default_mode(2)' "$a/fx.txt" \
    && ok "auto: Firefox received default_mode(2) — Auto advertises Server" \
    || bad "auto: Firefox never logged default_mode(2)"

  # And the whole of §49.3: a client that negotiates is never folded.
  grep -q 'negotiated its decorations' "$a/srv.log" \
    && ok "auto: the compositor classified Firefox as Negotiated, so it is never folded" \
    || { bad "auto: Firefox was not classified as Negotiated"; \
         grep -oE 'window [0-9]+ \(.*' "$a/srv.log" | head -2 | sed 's/^/        /'; }
  grep -q 'folding the caption' "$a/srv.log" \
    && bad "auto: a Firefox window had its caption folded — auto is not server for it" \
    || ok "auto: no window's caption was folded, which is what server would have done"

  # Leg 2: the same run with `server` named, so the equivalence is measured
  # rather than argued. Everything Firefox can observe must match.
  FX_DECOR=server; FX_ENV=(MOZ_GTK_TITLEBAR_DECORATION=none)
  WC_WAYLAND_DEBUG=1 run_firefox "$s" wc-autoref 25 \
    || { bad "the compositor never bound its socket (server)"; return; }

  # `fx_facts` is what a client could tell the two runs apart by: the mode it
  # was told, the modes it asked for, and its toplevel geometry origin.
  fx_facts() {
    local t=$1
    printf 'told=%s asked=%s origin=%s' \
      "$(grep -o 'default_mode([0-9]*)' "$t" | sort -u | tr '\n' ',')" \
      "$(grep -o 'request_mode([0-9]*)' "$t" | sort -u | tr '\n' ',')" \
      "$(grep -o 'set_window_geometry([0-9-]*, [0-9-]*, [0-9-]*, [0-9-]*)' "$t" \
         | tr -d 'a-z_()' | tr -d ' ' | awk -F, '$3 >= 640 { print $1 "," $2 }' | sort -u | tr '\n' ',')"
  }
  local fa fs
  fa=$(fx_facts "$a/fx.txt")
  fs=$(fx_facts "$s/fx.txt")
  note "auto:   $fa"
  note "server: $fs"
  if [ "$fa" = "$fs" ]; then
    ok "auto and server are indistinguishable to Firefox — §49.4's argument, measured"
  else
    bad "auto and server differ where Firefox can see it"
  fi
  case "$fa" in
    *origin=0,0,*) ok "auto: the toplevel geometry origin is 0,0 — server-side decorations, no shadow inset" ;;
    *)             bad "auto: the toplevel geometry origin is not 0,0" ;;
  esac
}

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

# ---------------------------------------------------------------------------
# §82: tabs in the title bar, under the defaults
# ---------------------------------------------------------------------------
#
# The arrangement a deployment gets with no flag on the compositor and the
# launcher's default `titlebar = tabs`: Firefox told MOZ_GTK_TITLEBAR_DECORATION
# =client, its own window buttons turned off, and a userChrome.css that makes
# its tab strip the caption's height and leaves the caption buttons' corner
# empty. The profile is written by `wc-launch --write-firefox-profile`, the same
# code a launch runs, so this measures the launcher's output and not a copy.

OUTTB="$ROOT/docs/tabs-in-titlebar"
LAUNCH_BIN="$LINUX_BIN/wc-launch"

# The geometry rectangle's top strip: how many rows from the top at column 700
# are the colour of row 0. Firefox's tab strip is one flat colour there (it is
# past the last tab) and the navigation bar under it is another.
strip_rows() {
  magick "$1" -crop 1x80+700+0 +repage txt:- 2>/dev/null \
    | awk 'NR>1 {print $3}' | uniq -c | head -1 | awk '{print $1}'
}

# The share of the top-right `$2`x`$3` corner that is its most common colour,
# in percent. An empty corner is ~99.5 (the window's rounded corner is the
# rest); one with Firefox's tab-list button or close button in it is under 98.
corner_share() {
  local img=$1 w=$2 h=$3 width
  width=$(magick identify -format %w "$img" 2>/dev/null)
  magick "$img" -crop "${w}x${h}+$((width - w))+0" +repage -format %c histogram:info:- 2>/dev/null \
    | sort -rn | awk '{n=$1+0; t+=n; if (NR==1) m=n} END {if (t) printf "%.1f", 100*m/t}'
}

block_tabs_default() {
  echo "== tabs-default: Firefox's tabs in the caption with no compositor flag (Linux, headless)"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }
  have magick || { non "no imagemagick"; return; }
  fresh_or_skip "$LAUNCH_BIN" "cargo build -p wc-launch"; case $? in 2) non "no wc-launch"; return ;; 1) return ;; esac

  local t="$RUNDIR/tdef-tabs" n="$RUNDIR/tdef-none" c="$RUNDIR/tdef-bare"
  mkdir -p "$t/profile" "$n/profile" "$c/profile"
  "$LAUNCH_BIN" --write-firefox-profile tabs 96 "$t/profile" >"$t/launch.txt" 2>&1 \
    || { bad "wc-launch would not write the tabs profile: $(cat "$t/launch.txt")"; return; }
  "$LAUNCH_BIN" --write-firefox-profile none 96 "$n/profile" >"$n/launch.txt" 2>&1 \
    || { bad "wc-launch would not write the none profile"; return; }
  note "$(cat "$t/launch.txt")"

  # Leg 1: the default. No --decorations; what the launcher writes and sets.
  FX_DECOR=none; FX_ENV=(MOZ_GTK_TITLEBAR_DECORATION=client)
  WC_WAYLAND_DEBUG=1 run_firefox "$t" wc-tdef 25 \
    || { bad "the compositor never bound its socket (tabs)"; return; }
  # Leg 2: the opt-out, `titlebar = none`.
  FX_DECOR=none; FX_ENV=(MOZ_GTK_TITLEBAR_DECORATION=none)
  WC_WAYLAND_DEBUG=1 run_firefox "$n" wc-tdefn 25 \
    || { bad "the compositor never bound its socket (none)"; return; }
  # Leg 3, the control: the env var and nothing else — a Firefox started by
  # something other than the launcher. The fold must still happen and the
  # corner must not be clear, or the sheet is not what clears it.
  FX_DECOR=none; FX_ENV=(MOZ_GTK_TITLEBAR_DECORATION=client)
  WC_WAYLAND_DEBUG=1 run_firefox "$c" wc-tdefc 25 \
    || { bad "the compositor never bound its socket (control)"; return; }
  FX_DECOR=client; FX_ENV=()

  grep -q 'decorations: manager default is Auto' "$t/srv.log" \
    && ok "tabs: the compositor ran its default, Auto" \
    || bad "tabs: the compositor did not run Auto"
  grep -q 'org_kde_kwin_server_decoration#[0-9]*\.request_mode(1)' "$t/fx.txt" \
    && ok "tabs: Firefox asked for Client (request_mode(1))" \
    || bad "tabs: Firefox did not ask for Client"
  grep -q 'window 1 (firefox) asked to decorate itself (Client) and was told yes, so this window.s caption is folded' "$t/srv.log" \
    && ok "tabs: the compositor folded the caption because Firefox asked for Client (§82)" \
    || { bad "tabs: no fold under the default"; grep 'window 1 (' "$t/srv.log" | sed 's/^/        /'; }
  grep -q 'window 1 mapped surface .* (the system caption folded in)' "$t/srv.log" \
    && ok "tabs: the platform mapped it with the caption folded in" \
    || bad "tabs: the platform did not fold"

  grep -q 'request_mode(2)' "$n/fx.txt" \
    && ok "none: Firefox asked for Server, so the opt-out still opts out" \
    || bad "none: Firefox did not ask for Server"
  if grep -q 'caption folded in' "$n/srv.log"; then
    bad "none: the caption was folded for a Firefox that opted out"
  else
    ok "none: the caption was kept: Windows' title bar, the tab strip below it"
  fi
  [ -f "$n/profile/chrome/userChrome.css" ] \
    && bad "none: the opt-out profile has a userChrome.css" \
    || ok "none: the opt-out profile has no userChrome.css"

  local lt ln lc
  lt=$(ls "$t"/frames/window1-*.ppm 2>/dev/null | tail -1)
  ln=$(ls "$n"/frames/window1-*.ppm 2>/dev/null | tail -1)
  lc=$(ls "$c"/frames/window1-*.ppm 2>/dev/null | tail -1)
  if [ -z "$lt" ] || [ -z "$lc" ] || [ -z "$ln" ]; then
    bad "a leg dumped no frames: tabs=${lt:-none} none=${ln:-none} control=${lc:-none}"
    return
  fi
  local rows_t rows_c share_t share_c
  rows_t=$(strip_rows "$lt"); rows_c=$(strip_rows "$lc")
  share_t=$(corner_share "$lt" 146 31); share_c=$(corner_share "$lc" 146 31)
  note "tab strip rows: tabs=$rows_t control=$rows_c; corner 146x31 dominant colour: tabs=$share_t% control=$share_c%"
  [ "$rows_t" = 31 ] \
    && ok "tabs: the tab strip is 31 rows, the caption's height at 100% (control: $rows_c)" \
    || bad "tabs: the tab strip is ${rows_t:-?} rows; the caption is 31"
  awk -v s="${share_t:-0}" 'BEGIN { exit !(s >= 99) }' \
    && ok "tabs: the caption-button corner is empty ($share_t% one colour)" \
    || bad "tabs: something of Firefox's is in the caption-button corner ($share_t% one colour)"
  awk -v s="${share_c:-100}" 'BEGIN { exit !(s < 99) }' \
    && ok "control: without the profile Firefox's own widgets are in that corner ($share_c%), so the profile is what empties it" \
    || bad "control: the corner is empty without the profile too ($share_c%); the measurement cannot tell"

  mkdir -p "$OUTTB"
  magick "$lt" -crop 1280x80+0+0 +repage "$OUTTB/linux-tabs-default-top.png" 2>/dev/null
  magick "$lc" -crop 1280x80+0+0 +repage "$OUTTB/linux-tabs-noprofile-top.png" 2>/dev/null
  magick "$ln" -crop 1280x80+0+0 +repage "$OUTTB/linux-none-top.png" 2>/dev/null
  note "frame-dump crops (Linux headless compositor, not Windows): ${OUTTB#"$ROOT"/}/linux-{tabs-default,tabs-noprofile,none}-top.png"
}

# The Wine leg of the same arrangement, and the one place before the guest
# where the Win32 half runs: WM_NCCALCSIZE, the hit test, the fallback caption
# buttons, and a real drag through SendInput.
#
# WHAT WINE CAN AND CANNOT SAY. Wine's `dwmapi` composites nothing and answers
# DWMWA_CAPTION_BUTTON_BOUNDS with E_NOTIMPL (§67.4), so this leg sees the
# compositor's *fallback* buttons (DrawFrameControl), never DWM's, and its
# hit-test answers come from `caption::hit` rather than DwmDefWindowProc. The
# modal move and size loops are Wine's own. A pass here says the Win32 code paths
# are self-consistent and that the client half round-trips; it says nothing
# about DWM, snap layouts, or real Windows' metrics.
block_wine_tabs() {
  echo "== wine-tabs: the default arrangement in a Wine HWND — fold, hit test, drag, double-click"
  have "$FIREFOX" || { non "no firefox on PATH"; return; }
  have wine || { non "no wine"; return; }
  have Xvfb || { non "no Xvfb"; return; }
  have magick || { non "no imagemagick"; return; }
  fresh_or_skip "$WIN_BIN/wc-compositor.exe" "cargo build --target x86_64-pc-windows-gnu"; case $? in 2) non "no wc-compositor.exe"; return ;; 1) return ;; esac
  fresh_or_skip "$WIN_BIN/wc-desk.exe" "cargo build -p wc-desk --target x86_64-pc-windows-gnu"; case $? in 2) non "no wc-desk.exe"; return ;; 1) return ;; esac
  fresh_or_skip "$LAUNCH_BIN" "cargo build -p wc-launch"; case $? in 2) non "no wc-launch"; return ;; 1) return ;; esac

  local dir="$RUNDIR/wine-tabs"
  mkdir -p "$dir/rt" "$dir/frames" "$dir/pools" "$dir/keymaps" "$dir/profile"
  chmod 700 "$dir/rt"
  winp() { local p=$1; echo "Z:${p//\//\\}"; }
  "$LAUNCH_BIN" --write-firefox-profile tabs 96 "$dir/profile" >"$dir/launch.txt" 2>&1 \
    || { bad "wc-launch would not write the profile"; return; }

  wc_substrate_declare wine-fixture \
    "no QEMU on this box: a Wine run has no guest RAM to locate" || return

  Xvfb :$WC_X_DISPLAY -screen 0 1600x1100x24 -nolisten tcp >"$dir/xvfb.log" 2>&1 &
  XPID=$!
  export DISPLAY=":$WC_X_DISPLAY"
  local up=""
  for _ in $(seq 1 100); do
    xwd -root -silent >/dev/null 2>&1 && { up=1; break; }
    sleep 0.1
  done
  [ -n "$up" ] || { bad "Xvfb never came up"; return; }
  sleep 3
  local desk="wc-ftabs-$$-$RANDOM" sock=wl-ftabs
  rm -f "$WINEPREFIX/drive_c"/users/*/AppData/Local/waycompose/"$sock"* 2>/dev/null

  # No --decorations: the compiled-in default, which is the claim.
  wine explorer "/desktop=$desk,1600x1050" \
    "$WIN_BIN/wc-compositor.exe" --socket "$sock" --timeout 170 --output "$OUTPUT" \
    --shared-dir "$(winp "$dir/keymaps")" --shared-peer-prefix "$dir/keymaps/" \
    --log "$(winp "$dir/srv.log")" >"$dir/loader.log" 2>&1 &
  local srv=$!
  local node=""
  for _ in $(seq 1 300); do
    node=$(find "$WINEPREFIX/drive_c" -type s -name "$sock" 2>/dev/null | head -1)
    [ -n "$node" ] && break
    sleep 0.2
  done
  [ -n "$node" ] || { bad "the Windows compositor never created its socket node"; kill "$srv" 2>/dev/null; return; }

  XDG_RUNTIME_DIR="$dir/rt" "$LINUX_BIN/wc-proxy" \
    --upstream "$node" --socket wl-proxy --runtime-dir "$dir/rt" \
    --copy pool --transport shared-file --pool-dir "$dir/pools" \
    --peer-prefix "$(winp "$dir/pools")\\" \
    --accept-timeout 40 --timeout 160 >"$dir/proxy.log" 2>&1 &
  local proxy=$!
  sleep 1
  env -u DISPLAY MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland MOZ_GTK_TITLEBAR_DECORATION=client \
    XDG_RUNTIME_DIR="$dir/rt" WAYLAND_DISPLAY=wl-proxy \
    timeout 140 "$FIREFOX" --no-remote --profile "$dir/profile" about:blank \
    >"$dir/fx.log" 2>&1 &
  local cli=$!
  sleep 35

  # `wc-desk` inside the same virtual desktop (wc-input.sh's lesson: a second
  # desktop has no WaycomposeToplevel to find), its stdout to a file because a
  # program started by `wine explorer` has its console inside the desktop.
  #
  # **And it is waited for by a file, not by the exit of `wine explorer`.**
  # Naming a desktop that is already running hands the program to it and
  # returns at once, so the first version of this leg ran the hit test, the
  # top-edge drag, the tab-strip drag and the double-click *at the same time*:
  # the top-edge drag came out as a move, and three of the five gestures had no
  # "after" line. Measured, and the reason for `.done`.
  #
  # **And no point it touches is in the top-left 665x434.** `cmd /c` opens a
  # visible Wine console at 0,0 of the desktop, over the window, and a
  # `SendInput` press there goes to the console: the first top-edge drag of
  # this leg was aimed at x=600 and "did not resize" for exactly that reason.
  # Hit tests are messages and are not affected; gestures are aimed right of it.
  desk() {
    local out=$1; shift
    rm -f "$out" "$out.done"
    wine explorer "/desktop=$desk,1600x1050" cmd /c \
      "$(winp "$WIN_BIN/wc-desk.exe") $* --class WaycomposeToplevel > $(winp "$out") 2>&1 & echo done> $(winp "$out.done")" \
      >/dev/null 2>&1
    for _ in $(seq 1 300); do
      [ -f "$out.done" ] && break
      sleep 0.2
    done
    sleep 0.5
  }
  shot() {
    xwd -root -silent -out "$dir/$1.xwd" 2>/dev/null && magick "$dir/$1.xwd" "$dir/$1.png" 2>/dev/null
  }
  shot restored

  # 1. The hit test, at points fixed relative to an edge: the top edge, the
  #    top-left corner, empty tab strip, the three caption buttons (thirds of
  #    the fallback strip, which is 3 x SM_CXSIZE at the right), and the
  #    navigation bar under the strip.
  desk "$dir/hit.txt" hittest --at -300,2 --at 2,2 --at 800,15 \
    --at -50,15 --at -30,15 --at -12,15 --at 600,60
  sed 's/^/        /' "$dir/hit.txt"

  # 2. The top edge, dragged down by 60: the window must get shorter and its
  #    top must move, and nothing must reach the client.
  desk "$dir/top.txt" mouse --drag -300,2 -300,62
  # 3. Empty tab strip, dragged by (+120, +80): Firefox must ask for a move and
  #    the window must follow.
  desk "$dir/move.txt" mouse --drag 800,15 920,95
  shot moved
  # 4. A double-click on empty tab strip: Firefox's titlebar action, which is
  #    `set_maximized`.
  desk "$dir/dbl.txt" mouse --dblclick 800,15
  shot maximised
  # Maximised, the window sits above the monitor by its own border (§31.3),
  # so the first *visible* row is window-relative y = -top, not 2.
  local mtop
  mtop=$(grep -o "mouse.after-dbl *[0-9]*x[0-9]* at [0-9-]*,[0-9-]*" "$dir/dbl.txt" | sed "s/.*,//")
  mtop=${mtop:-0}; if [ "$mtop" -lt 0 ]; then mtop=$((0 - mtop)); else mtop=0; fi
  desk "$dir/hitmax.txt" hittest --at -300,$mtop --at -12,$((mtop + 15))
  for f in top move dbl; do sed 's/^/        /' "$dir/$f.txt"; done

  kill "$cli" 2>/dev/null; wait "$cli" 2>/dev/null
  sleep 1
  kill "$proxy" 2>/dev/null; wait "$proxy" 2>/dev/null
  kill "$srv" 2>/dev/null; wait "$srv" 2>/dev/null

  wc_assert_substrate "$dir/srv.log" "wine-tabs"
  grep -q 'window 1 (firefox) asked to decorate itself (Client) and was told yes' "$dir/srv.log" \
    && ok "the compositor folded Firefox's caption under its default, on Win32" \
    || { bad "no fold"; grep 'window 1 ' "$dir/srv.log" | head -8 | sed 's/^/        /'; }
  local folded
  folded=$(grep -o 'window 1 folds [0-9]* px of caption[^;]*' "$dir/srv.log" | head -1)
  [ -n "$folded" ] && ok "WM_NCCALCSIZE: $folded" || bad "no 'folds N px' line"
  grep -q "window 1 draws Windows' caption buttons itself (DrawFrameControl)" "$dir/srv.log" \
    && ok "DWM would not say (Wine), so the window drew the system's buttons itself: $(grep -o 'client rect [0-9,]* [0-9x]*' "$dir/srv.log" | grep -m1 . )" \
    || bad "no fallback caption buttons under Wine"
  grep -q 'window 1 leaves the caption-button corner clear' "$dir/srv.log" \
    && ok "the compositor's own corner check: Firefox left the button strip empty" \
    || { bad "the compositor saw Firefox's widgets under the buttons"; grep 'CAPTION BUTTONS\|corner clear' "$dir/srv.log" | sed 's/^/        /'; }

  h() { grep -o "hittest *$1 .* -> [A-Z]*" "$2" | grep -o '[A-Z]*$'; }
  [ "$(h -300,2 "$dir/hit.txt")" = HTTOP ] && ok "hit test: the top edge of the restored window is HTTOP" \
    || bad "hit test: the top edge is $(h -300,2 "$dir/hit.txt")"
  [ "$(h 2,2 "$dir/hit.txt")" = HTTOPLEFT ] && ok "hit test: the top-left corner is HTTOPLEFT" \
    || bad "hit test: the top-left corner is $(h 2,2 "$dir/hit.txt")"
  [ "$(h 800,15 "$dir/hit.txt")" = HTCLIENT ] && ok "hit test: empty tab strip is HTCLIENT — the client's to drag (§22.8)" \
    || bad "hit test: empty tab strip is $(h 800,15 "$dir/hit.txt")"
  [ "$(h -12,15 "$dir/hit.txt")" = HTCLOSE ] && ok "hit test: the right end of the strip is HTCLOSE" \
    || bad "hit test: the right end of the strip is $(h -12,15 "$dir/hit.txt")"
  case "$(h -50,15 "$dir/hit.txt") $(h -30,15 "$dir/hit.txt")" in
    *HTMAXBUTTON*) ok "hit test: HTMAXBUTTON is reachable — what Windows 11 raises snap layouts on" ;;
    *) bad "hit test: no HTMAXBUTTON at -50,15 or -30,15" ;;
  esac
  [ "$(h 600,60 "$dir/hit.txt")" = HTCLIENT ] && ok "hit test: below the strip is the client's" \
    || bad "hit test: below the strip is $(h 600,60 "$dir/hit.txt")"

  # The gestures, from the rectangles wc-desk printed.
  geom() { grep -o "mouse\.$1 *[0-9]*x[0-9]* at [0-9-]*,[0-9-]* zoomed=[a-z]*" "$2" | head -1 \
             | sed 's/mouse\.[a-z-]* *//'; }
  local tb ta mb ma db da
  tb=$(geom before "$dir/top.txt"); ta=$(geom after-drag "$dir/top.txt")
  mb=$(geom before "$dir/move.txt"); ma=$(geom after-drag "$dir/move.txt")
  db=$(geom before "$dir/dbl.txt"); da=$(geom after-dbl "$dir/dbl.txt")
  note "top edge: $tb -> $ta"
  note "tab strip drag: $mb -> $ma"
  note "double-click: $db -> $da"
  local th0 th1 ty0 ty1
  th0=$(echo "$tb" | sed 's/^[0-9]*x\([0-9]*\).*/\1/'); th1=$(echo "$ta" | sed 's/^[0-9]*x\([0-9]*\).*/\1/')
  ty0=$(echo "$tb" | sed 's/.*,\([0-9-]*\) .*/\1/'); ty1=$(echo "$ta" | sed 's/.*,\([0-9-]*\) .*/\1/')
  if [ -n "$th0" ] && [ -n "$th1" ] && [ "$th1" -lt "$th0" ] && [ "$ty1" -gt "$ty0" ]; then
    ok "the top edge resizes: ${th0}px tall at y=$ty0 -> ${th1}px at y=$ty1"
  else
    bad "the top edge did not resize ($tb -> $ta)"
  fi
  if grep -q 'window 1 handed a move to Windows' "$dir/srv.log"; then
    ok "Firefox asked for xdg_toplevel.move from a drag on empty tab strip, and it was handed to Windows"
  else
    bad "no xdg_toplevel.move reached the compositor from the tab-strip drag"
  fi
  local mx0 my0 mx1 my1
  mx0=$(echo "$mb" | sed 's/.* at \([0-9-]*\),.*/\1/'); mx1=$(echo "$ma" | sed 's/.* at \([0-9-]*\),.*/\1/')
  my0=$(echo "$mb" | sed 's/.*,\([0-9-]*\) .*/\1/'); my1=$(echo "$ma" | sed 's/.*,\([0-9-]*\) .*/\1/')
  # Within 25 px of the drag, not exactly it: GTK starts the move only once the
  # pointer has passed its drag threshold, so the window trails the pointer by
  # that much. The keyboard-mode bug this replaced moved it by (+286, +82).
  local mdx mdy
  mdx=$((${mx1:-0} - ${mx0:-0})); mdy=$((${my1:-0} - ${my0:-0}))
  if [ -n "$mx0" ] && [ -n "$mx1" ] && [ $((mdx > 120 ? mdx - 120 : 120 - mdx)) -le 25 ] \
       && [ $((mdy > 80 ? mdy - 80 : 80 - mdy)) -le 25 ]; then
    ok "the window followed the drag: $mx0,$my0 -> $mx1,$my1, (+$mdx,+$mdy) for a drag of (+120,+80)"
  else
    bad "the window did not follow the drag: ${mx0:-?},${my0:-?} -> ${mx1:-?},${my1:-?}"
  fi
  case "$da" in
    *zoomed=true) ok "a double-click on empty tab strip maximised the window" ;;
    *) bad "the double-click did not maximise ($da)" ;;
  esac
  [ "$(h -300,$mtop "$dir/hitmax.txt")" = HTCLIENT ] \
    && ok "maximised: no top resize band, the monitor's top row (window y=$mtop) is the tab strip" \
    || bad "maximised: the monitor's top row (window y=$mtop) is $(h -300,$mtop "$dir/hitmax.txt")"

  # The maximised capture, read the way `tabs-default` reads a frame dump:
  # the screen's top rows at x=700 must be the tab strip, 31 of them at the
  # launcher's 100% fit, starting at row 0. A shifted window (§82: the
  # geometry-origin shift this leg found) shows the navigation bar there.
  local mrows
  mrows=$(strip_rows "$dir/maximised.png")
  [ "${mrows:-0}" -ge 28 ] && [ "${mrows:-0}" -le 34 ] \
    && ok "maximised: the screen's first $mrows rows are Firefox's tab strip" \
    || bad "maximised: the screen's top is ${mrows:-?} rows of one colour, not the 31-row tab strip"

  mkdir -p "$OUTTB"
  local name
  for name in restored moved maximised; do
    [ -f "$dir/$name.png" ] && cp "$dir/$name.png" "$OUTTB/wine-tabs-$name.png"
  done
  note "Wine screenshots (Xvfb root, Wine's own frame, no DWM): ${OUTTB#"$ROOT"/}/wine-tabs-{restored,moved,maximised}.png"
  # Without the per-commit line, which is 13,000 of its 13,300 lines.
  grep -v "wl_surface commit$" "$dir/srv.log" >"$OUTTB/wine-tabs-compositor.log" 2>/dev/null
  cat "$dir/hit.txt" "$dir/top.txt" "$dir/move.txt" "$dir/dbl.txt" "$dir/hitmax.txt" \
    >"$OUTTB/wine-tabs-desk.txt" 2>/dev/null
}

WC_X_DISPLAY=${WC_X_DISPLAY:-77}

case "${1:-all}" in
  enumerate)  block_enumerate ;;
  window)     block_window ;;
  titlebar)   block_titlebar ;;
  auto)       block_auto ;;
  needs)      block_needs ;;
  tabs)       block_tabs ;;
  menu)       block_menu ;;
  wine)       block_wine shared-file ;;
  tabs-default) block_tabs_default ;;
  wine-tabs)  block_wine_tabs ;;
  wine-arena) block_wine arena ;;
  all)        block_enumerate; block_needs; block_window; block_titlebar; block_auto; block_tabs; block_menu
              block_wine shared-file; block_wine arena ;;
  *) echo "usage: $0 [enumerate|needs|window|titlebar|auto|tabs|menu|wine|wine-arena|all]"; exit 2 ;;
esac

echo
echo "$pass passed, $fail failed, $skip skipped"
[ "$fail" -eq 0 ]
