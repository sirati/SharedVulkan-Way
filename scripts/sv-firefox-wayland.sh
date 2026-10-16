#!/usr/bin/env bash
#
# **`VK_KHR_wayland_surface`, on a machine that has a GPU.** The control that
# `docs/firefox-accel.md` names, run as an experiment rather than quoted as one.
#
#   scripts/sv-firefox-wayland.sh [probe|firefox|x11|soak|soak-x11] [secs]
#
# The topology is the testhost one with the VM taken out and nothing else
# changed, which is exactly what makes it a control:
#
#   weston --renderer=pixman   a compositor that offers `wl_shm` and **no**
#                              dmabuf, because the cross-VM one cannot offer one
#   sv-gpu-server serve        the GPU side, on this machine's real RADV
#   firefox                    Zink -> sv-icd -> RADV, presenting through
#                              `VK_KHR_wayland_surface` into `wl_shm`
#
# What it isolates: everything about the cross-VM transport is gone, so a
# failure here is the WSI's and a success here says the WSI is not what fails
# there. `docs/firefox-accel.md` §5 measured the *other* half of this — real
# RADV with its own Wayland WSI on a pixman weston — and found no EGLConfig
# failure, so the only variable left is whose WSI it is.
set -uo pipefail

MODE=${1:-firefox}
SECS=${2:-60}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# Which build. `target/debug` is what every deployment script ships (wc-guest.sh,
# wc-firefox.sh), and the two profiles do not have the same stack frames: the
# Firefox crash series of §0.24 is a debug-build fault (§0.27).
BIN=${SVWL_BIN:-$ROOT/target/release}
# The GPU side on **one named device**. Unpinned, the loader offers every ICD on
# the machine, and on this workstation that includes the leased NVIDIA card
# other people's jobs run on. RADV here is the AMD iGPU (renderD129). The dev
# shell's Mesa rather than /run/opengl-driver's: the system RADV is linked
# against a newer glibc than the one `sv-gpu-server` is built with, and the
# loader refuses it with ERROR_INCOMPATIBLE_DRIVER.
GPU_ICD=${SVWL_GPU_ICD:-${SV_MESA:-/run/opengl-driver}/share/vulkan/icd.d/radeon_icd.x86_64.json}
# Mesa's EGL only, for the same reason: glvnd would otherwise load NVIDIA's
# vendor library first and open the leased card from Firefox itself.
export __EGL_VENDOR_LIBRARY_FILENAMES=${SVWL_EGL_VENDOR:-/run/opengl-driver/share/glvnd/egl_vendor.d/50_mesa.json}
# **The Mesa the Wayland modes need** (docs/design.md §0.28). Stock Mesa will not
# put Zink on a Wayland compositor that offers only `wl_shm` — it takes the DRM
# door, finds no device, and loads llvmpipe — so Firefox as a Wayland client of
# this weston, or of wc-proxy, renders in software whatever this ICD offers.
# `nix/mesa-kopper-wayland.nix` is the system's own Mesa plus the override
# platform_x11.c already has (`LIBGL_KOPPER_DRI2`). Built against the registry's
# nixpkgs, which is the one the system Firefox comes from, so both load one
# libwayland-client. `SVWL_KOPPER_MESA=none` measures stock Mesa instead.
KOPPER_MESA=${SVWL_KOPPER_MESA:-}
if [ -z "$KOPPER_MESA" ] && { [ "$MODE" = firefox ] || [ "$MODE" = soak ] || [ "$MODE" = scroll ] || [ "$MODE" = two-windows ] || [ "$MODE" = probe ]; }; then
  KOPPER_MESA=$(nix build --no-link --print-out-paths --impure --expr \
    "(builtins.getFlake \"nixpkgs\").legacyPackages.x86_64-linux.callPackage $ROOT/nix/mesa-kopper-wayland.nix {}" \
    2>/dev/null | grep -v -- '-[a-z_]*$' | head -1)
  [ -n "$KOPPER_MESA" ] || { echo "FAIL: could not build nix/mesa-kopper-wayland.nix (SVWL_KOPPER_MESA=none to run stock Mesa)"; exit 1; }
fi
if [ -n "$KOPPER_MESA" ] && [ "$KOPPER_MESA" != none ]; then
  export __EGL_VENDOR_LIBRARY_FILENAMES=$KOPPER_MESA/share/glvnd/egl_vendor.d/50_mesa.json
fi
# `SVWL_RUNROOT`: where the run directory goes. Large; keep it off a small /tmp.
RUN=$(mktemp -d "${SVWL_RUNROOT:-/tmp}/svwl-XXXXXX")
FIREFOX=${SVWL_FIREFOX:-$(command -v firefox)}
EGLINFO=${SVWL_EGLINFO:-/nix/store/29kh8r89yqgg1ndmfybh1wafjzichp2g-mesa-demos-9.0.0/bin/eglinfo}
DPPX=${SVWL_DPPX:-0.4}
# Per-session arena on the GPU side. 64 MiB is `sv-gpu-server`'s own default and
# what this harness always ran with; Firefox in use outgrows it (docs/design.md
# §0.27), and 256 MiB is `sv-icd`'s default and `sv-cts.sh`'s.
ARENA=${SVWL_ARENA:-$((64 << 20))}
# `SVWL_POOL=BYTES`: every GPU session leases from one pool of this size instead
# of getting `ARENA` of its own (waycompose-design.md §97), as on the guest.
XDISPLAY=${SVWL_XDISPLAY:-:78}
# This machine has Mesa and its ICDs in `/run/opengl-driver` but no
# `libvulkan.so.1` on the default search path, so both the GPU side and Zink
# fail to find a loader at all. Named rather than searched for, because a
# different loader is a different measurement.
VKLOADER=${SVWL_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
say() { echo "[$(date +%H:%M:%S)] $*"; }

KIDS=()
cleanup() {
  kill "${KIDS[@]}" 2>/dev/null
  # Wine's server outlives the processes it served; this prefix is this
  # harness's own, so stopping it stops nobody else's.
  [ "${SERVER:-weston}" = wc-proxy ] && WINEPREFIX="$WINEPREFIX" wineserver -k 2>/dev/null
}
trap cleanup EXIT INT TERM

mkdir -p "$RUN/xdg" "$RUN/profile" "$RUN/home" "$RUN/shots"
chmod 700 "$RUN/xdg"
export XDG_RUNTIME_DIR="$RUN/xdg"
# `SVWL_FRAME_TRACE=1`: every process of the frame writes its half of one
# timeline (waycompose-design.md §88), read by `scripts/wc-frame-timeline.py`.
if [ "${SVWL_FRAME_TRACE:-0}" = 1 ]; then
  mkdir -p "$RUN/trace"
  export WC_FRAME_TRACE="$RUN/trace/%p.txt"
fi

# **The two prefs that override Firefox's blocklist**, and a switch to leave them
# out. Firefox's gfxInfo calls Zink without a DRM node `mesa/software-unknown`
# and blocklists hardware WebRender for it (docs/design.md §0.27.8), so with
# them a run measures what the driver can do and without them it measures what
# a profile nobody configured gets. `SVWL_FORCE=0` is the second (§0.28), and
# `SVWL_FORCE=wr` is exactly what `wc-session` writes on a host with gpu.env:
# `gfx.webrender.all` and nothing else.
FORCE=${SVWL_FORCE:-1}
: >"$RUN/profile/user.js"
case "$FORCE" in
  1) printf 'user_pref("gfx.webrender.all", true);\nuser_pref("webgl.force-enabled", true);\n' >>"$RUN/profile/user.js" ;;
  wr) printf 'user_pref("gfx.webrender.all", true);\n' >>"$RUN/profile/user.js" ;;
  0) ;;
  *) echo "FAIL: SVWL_FORCE is 1, wr or 0"; exit 1 ;;
esac
cat >>"$RUN/profile/user.js" <<PREFS
user_pref("gfx.webrender.software", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("layout.css.devPixelsPerPx", "$DPPX");
PREFS
# `SVWL_VULKAN_DECODE=1`: the three prefs `wc-session` writes on the deployed
# install (waycompose-design.md §95.10). With them Firefox starts more processes
# that open a GPU session at start-up, which is what two slots ran out of.
if [ "${SVWL_VULKAN_DECODE:-0}" = 1 ]; then
  cat >>"$RUN/profile/user.js" <<PREFS
user_pref("media.hardware-video-decoding-vulkan.enabled", true);
user_pref("media.hardware-video-decoding-vulkan.force-enabled", true);
user_pref("media.hardware-video-decoding-vulkan.readback.enabled", true);
PREFS
fi

# ------------------------------------------------------------- compositor ---
# Two servers, one socket name. `weston` is the control this file was written
# for. `wc-proxy` is the deployed topology with the VM taken out: Firefox is a
# client of `wc-proxy`, which offers `wl_shm` and nothing that carries a
# descriptor, and which passes every buffer on through an arena to
# `wc-compositor.exe` — here under Wine on an Xvfb, as `scripts/wc-proxy.sh`
# runs it, rather than on the guest's desktop (docs/design.md §0.28).
SERVER=${SVWL_SERVER:-weston}
XN=""
# A picture of whatever the server shows. weston's own screenshooter for
# weston; the X root of the Xvfb the Wine desktop is on for wc-proxy.
shot() {
  if [ "$SERVER" = wc-proxy ]; then
    local f="$RUN/shots/xroot-$(date +%H-%M-%S)"
    DISPLAY=":$XN" timeout 20 xwd -root -silent -out "$f.xwd" 2>/dev/null \
      && magick "$f.xwd" "$f.png" 2>/dev/null; rm -f "$f.xwd"
  else
    (cd "$RUN/shots" && timeout 20 weston-screenshooter >/dev/null 2>&1)
  fi
}
if [ "$SERVER" = wc-proxy ]; then
  WINBIN=${SVWL_WINBIN:-$ROOT/target/x86_64-pc-windows-gnu/debug}
  [ -x "$WINBIN/wc-compositor.exe" ] && [ -x "$BIN/wc-proxy" ] \
    || { echo "FAIL: needs $WINBIN/wc-compositor.exe and $BIN/wc-proxy"; exit 1; }
  # Its own prefix, so that this never shares a wineserver with
  # `wc-proxy.sh`'s or anybody else's.
  export WINEPREFIX=${SVWL_WINEPREFIX:-/tmp/svwl-wineprefix-$(id -u)} WINEDEBUG=-all
  # Wine refuses to create a prefix whose parent it does not own, and /tmp is
  # root's; it is content to populate a directory that already exists.
  mkdir -p "$WINEPREFIX"
  # **Xvfb picks its own display number**, and that is a fix rather than a
  # style. This used `90 + $$ % 9`, and on a machine other people's harnesses
  # share, that display was sometimes already somebody's: our Xvfb exited with
  # "Server is already active for display 90", the readiness probe below
  # succeeded against *their* server, and when their run ended Wine lost its X
  # connection, wc-compositor.exe died, wc-proxy closed Firefox's socket and
  # GDK took Firefox down with "Error reading events from display: Broken
  # pipe" — twice, at 156 s and at 60 s, and it read as a crash of the path
  # under test. A display number is a shared identity (firefox-accel.md, "The
  # Wayland WSI", §6); `-displayfd` makes it this run's.
  Xvfb -displayfd 7 -screen 0 1600x1000x24 -nolisten tcp >"$RUN/xvfb.log" 2>&1 7>"$RUN/xvfb.display" &
  XPID=$!
  KIDS+=($XPID)
  for _ in $(seq 1 100); do [ -s "$RUN/xvfb.display" ] && break; sleep 0.1; done
  XN=$(tr -dc 0-9 <"$RUN/xvfb.display")
  { [ -n "$XN" ] && kill -0 "$XPID" 2>/dev/null; } \
    || { echo "FAIL: Xvfb did not start"; cat "$RUN/xvfb.log"; exit 1; }
  say "wc-compositor.exe under Wine on Xvfb :$XN (pid $XPID), wc-proxy in front of it — wl_shm and an arena"
  # The compositor's arena. 256 MiB rather than the launcher's 64, because
  # Firefox at 1280x900 keeps three 4.6 MB buffers per surface in flight and
  # this run is about the GPU side, not about how wc-proxy copes when tight.
  WCARENA=$((256 << 20))
  truncate -s "$WCARENA" "$RUN/wc-arena"
  mkdir -p "$RUN/keymaps"
  WSOCK="svwl-$$"
  # A file arena is a fixture, and wc-compositor refuses one unless told this
  # is a fixture run: there is no guest RAM on a box with no hypervisor.
  # The compositor is a Windows program and opens a Windows path.
  [ -n "${WC_FRAME_TRACE:-}" ] && CTRACE="WC_FRAME_TRACE=$(winepath -w "$RUN/trace")\\%p.txt"
  DISPLAY=":$XN" SV_ALLOW_FILE_ARENA=1 env ${CTRACE:-} wine explorer "/desktop=svwl$$,1600x1000" "$WINBIN/wc-compositor.exe" \
    --socket "$WSOCK" --timeout $((SECS + 400)) \
    --arena "0:$WCARENA:$(winepath -w "$RUN/wc-arena")" \
    --shared-dir "$(winepath -w "$RUN/keymaps")" --shared-peer-prefix "$RUN/keymaps/" \
    --log "$(winepath -w "$RUN/compositor.log")" >"$RUN/wine.log" 2>&1 &
  KIDS+=($!)
  NODE=""
  for _ in $(seq 1 300); do
    NODE=$(find "$WINEPREFIX/drive_c" -type s -name "$WSOCK" 2>/dev/null | head -1)
    [ -n "$NODE" ] && break; sleep 0.2
  done
  [ -n "$NODE" ] || { echo "FAIL: wc-compositor.exe never listened"; tail -5 "$RUN/wine.log"; exit 1; }
  SV_ALLOW_FILE_ARENA=1 "$BIN/wc-proxy" --upstream "$NODE" --socket svwl --runtime-dir "$RUN/xdg" --copy damage \
    --transport arena --arena-path "$RUN/wc-arena" --arena-id 0 --arena-size "$WCARENA" \
    --accept-timeout 120 --timeout $((SECS + 400)) ${SVWL_PROXY_EXTRA:-} >"$RUN/proxy.log" 2>&1 &
  KIDS+=($!)
  for _ in $(seq 1 80); do [ -S "$RUN/xdg/svwl" ] && break; sleep 0.25; done
  [ -S "$RUN/xdg/svwl" ] || { echo "FAIL: wc-proxy never listened"; cat "$RUN/proxy.log"; exit 1; }
  [ "$MODE" = x11 ] || [ "$MODE" = soak-x11 ] && { echo "FAIL: the X11 modes need weston"; exit 1; }
else
# `--renderer=pixman` is the whole point: a GL renderer would advertise
# `zwp_linux_dmabuf` and Mesa would take a path the real topology does not have.
say "weston, headless, pixman renderer — wl_shm and nothing else"
weston --backend=headless --renderer=pixman --width=1600 --height=1000 \
  --socket=svwl --idle-time=0 --debug >"$RUN/weston.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 40); do [ -S "$RUN/xdg/svwl" ] && break; sleep 0.5; done
[ -S "$RUN/xdg/svwl" ] || { echo "FAIL: weston never listened"; cat "$RUN/weston.log"; exit 1; }
fi

# --------------------------------------------------------------- GPU side ---
say "sv-gpu-server on this machine's RADV"
# `SVWL_GPU_ENV`: extra `NAME=value` words for the GPU side alone, e.g. a Vulkan
# layer that should see what the driver is handed and not what Firefox sends
# (docs/design.md §0.29 ran `VK_LAYER_LUNARG_api_dump` this way). Word-split,
# never evaluated.
read -r -a GPU_ENV <<<"${SVWL_GPU_ENV:-}"
if [ "$MODE" = soak ] || [ "$MODE" = soak-x11 ] || [ "$MODE" = two-windows ]; then
  # **What the GPU side executed, per half minute.** `SV_TRACE` prints every
  # command; a five-minute browser run is millions of lines, so they are
  # counted here rather than kept. A window with queue submissions in it is a
  # window in which something in Firefox rendered through this driver, which
  # about:support cannot say: its GPU rows come from the `glxtest` probe, not
  # from the context WebRender actually made.
  ( env "${GPU_ENV[@]}" VK_DRIVER_FILES="$GPU_ICD" SV_TRACE=1 "$BIN/sv-gpu-server" serve --clients 16 ${SVWL_SLOTS:+--slots $SVWL_SLOTS} ${SVWL_POOL:+--pool $SVWL_POOL} \
      --arena "$ARENA" --socket "$RUN/gpu.sock" --idle $((SECS + 240)) 2>&1 &
    echo $! >"$RUN/gpu.pid"; wait ) \
    | gawk -v out="$RUN/gpu-trace.txt" '
        BEGIN { t0 = systime(); last = t0 }
        /^\[trace\]/ { n[$2]++; total++
          if (systime() - last >= 30) {
            printf "t=%ds total=%d submit=%d present=%d draw=%d pools=+%d/-%d cbs=+%d/-%d mem=+%d/-%d\n", systime() - t0, total,
              n["vkQueueSubmit"] + n["vkQueueSubmit2"], n["vkQueuePresentKHR"],
              n["vkCmdDraw"] + n["vkCmdDrawIndexed"] + n["vkCmdDrawIndirect"],
              n["vkCreateCommandPool"], n["vkDestroyCommandPool"],
              n["vkAllocateCommandBuffers"], n["vkFreeCommandBuffers"],
              n["vkAllocateMemory"], n["vkFreeMemory"] >> out
            fflush(out); delete n; total = 0; last = systime() }
          next }
        # One line per recorded command, and the counts above already say
        # what the blobs carried; a five-minute run wrote 36 MB of these.
        /^\[blob\]/ { next }
        { print; fflush() }' >"$RUN/gpu.log" &
  KIDS+=($!)
  for _ in $(seq 1 40); do [ -s "$RUN/gpu.pid" ] && break; sleep 0.25; done
  KIDS+=($(cat "$RUN/gpu.pid"))
else
env "${GPU_ENV[@]}" VK_DRIVER_FILES="$GPU_ICD" "$BIN/sv-gpu-server" serve --clients 16 ${SVWL_SLOTS:+--slots $SVWL_SLOTS} ${SVWL_POOL:+--pool $SVWL_POOL} --arena "$ARENA" --socket "$RUN/gpu.sock" --idle $((SECS + 60)) \
  >"$RUN/gpu.log" 2>&1 &
KIDS+=($!)
fi
for _ in $(seq 1 40); do [ -S "$RUN/gpu.sock" ] && break; sleep 0.5; done
[ -S "$RUN/gpu.sock" ] || { echo "FAIL: the GPU side never listened"; cat "$RUN/gpu.log"; exit 1; }

# ------------------------------------------------------------------ client ---
# `VK_DRIVER_FILES` names our ICD only, so a Zink that reaches a device reached
# *this* one. No `LIBGL_KOPPER_DISABLE`: taking the kopper path is the change
# being measured, and if the WSI is missing this hangs rather than falling back
# quietly, which is the property that makes the result mean something.
export WAYLAND_DISPLAY=svwl
export MESA_LOADER_DRIVER_OVERRIDE=zink
${SVWL_GALLIUM:+export GALLIUM_DRIVER=$SVWL_GALLIUM}
${SVWL_FORCE_SW:+export LIBGL_ALWAYS_SOFTWARE=1}
export VK_DRIVER_FILES="$BIN/sv_icd.json"
export SV_SOCKET="$RUN/gpu.sock"

if [ "$MODE" = probe ]; then
  say "eglinfo on the Wayland platform"
  # `SVWL_PROBE_CMD` is how a GL *program* — one that makes a window and
  # presents to it — takes the place of a query that only enumerates configs.
  # `eglinfo` proves the driver is chosen; only a program with a swapchain
  # proves the WSI puts pixels anywhere.
  CMD=${SVWL_PROBE_CMD:-$EGLINFO -B}
  EGL_PLATFORM=wayland timeout 40 $CMD >"$RUN/probe.log" 2>&1 &
  P=$!
  sleep 20
  shot
  wait $P 2>/dev/null
  head -40 "$RUN/probe.log"
  say "---- the ICD ----"; grep -iE "sv-icd|wl_shm|wayland|surface" "$RUN/probe.log" | head -20
  say "---- screenshots ----"; ls "$RUN/shots" 2>/dev/null
  echo "run dir: $RUN"
  exit 0
fi

if [ "$MODE" = x11 ] || [ "$MODE" = soak-x11 ]; then
  # **Reap this display number first.** An Xwayland outlives the harness that
  # started it whenever the run does not reach its teardown, and the next run
  # then connects to a server whose compositor is gone: the X client gets a
  # swapchain, presents once, and kopper polls the surface forever. That looks
  # exactly like the stall this mode exists to measure, and it cost two
  # measurements here before it was recognised.
  #
  # **`pkill -f` is not usable for this** and the reason is `waycompose-design.md`
  # §16.8 a third time: the pattern would have to contain the display number,
  # and the command line it matches against includes *this script's own*, which
  # also contains it. The script kills itself, exit 143, and the run reports
  # nothing at all. A process **name** is an identity `pgrep -x` matches
  # exactly and can never match a shell; the display number then comes from
  # that process's own argv rather than from a pattern over every process.
  for p in $(pgrep -x Xwayland 2>/dev/null); do
    tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q -- " $XDISPLAY " && kill "$p"
  done
  sleep 1
  # Xwayland on top of the shm-only weston, and Firefox on X11 inside it. The
  # X server gets a *clean* GL environment — it is itself a GL program and an X
  # server that cannot initialise its own GL offers no fbconfig to anybody.
  say "rootful Xwayland on the shm-only weston, then Firefox on X11"
  env -u GALLIUM_DRIVER -u MESA_LOADER_DRIVER_OVERRIDE -u VK_DRIVER_FILES \
      -u SV_SOCKET -u EGL_PLATFORM LIBGL_ALWAYS_SOFTWARE=1 \
    Xwayland $XDISPLAY -geometry 1600x1000 -noreset >"$RUN/xwayland.log" 2>&1 &
  KIDS+=($!)
  for _ in $(seq 1 60); do [ -S "/tmp/.X11-unix/X${XDISPLAY#:}" ] && break; sleep 0.5; done
  [ -S "/tmp/.X11-unix/X${XDISPLAY#:}" ] || { echo "FAIL: Xwayland never listened"; tail -5 "$RUN/xwayland.log"; exit 1; }
  export DISPLAY=$XDISPLAY EGL_PLATFORM=x11 MOZ_ENABLE_WAYLAND=0 GDK_BACKEND=x11
  # **The override this whole mode exists to test.** `platform_x11.c` refuses
  # Zink outright when the X server has no DRI3 multibuffer support — which an
  # Xwayland with no DRM device never has — *unless* `LIBGL_KOPPER_DRI2` is
  # set, in which case it skips the render-device checks and goes straight to
  # `dri2_initialize_x11_kopper`. Kopper then presents through
  # `VK_KHR_xcb_surface`, which this ICD has had since §0.4.
  export LIBGL_KOPPER_DRI2=${SVWL_KOPPER_DRI2:-1}
fi

if [ "$MODE" != x11 ] && [ "$MODE" != soak-x11 ]; then
  # With no X server reachable `MOZ_ENABLE_WAYLAND` is a preference rather than
  # a choice; unsetting DISPLAY is what makes it one.
  unset DISPLAY
  export MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland
  # The patched Mesa's override, read by its Wayland platform exactly as the
  # X11 platform reads it: take the kopper path with no DRM device. Inert in
  # stock Mesa, whose Wayland platform does not read it.
  [ "${KOPPER_MESA:-none}" != none ] && export LIBGL_KOPPER_DRI2=${SVWL_KOPPER_DRI2:-1}
  say "Mesa for the Wayland platform: ${KOPPER_MESA:-none} (LIBGL_KOPPER_DRI2=${LIBGL_KOPPER_DRI2:-unset})"
fi

if [ "$MODE" = soak ] || [ "$MODE" = soak-x11 ] || [ "$MODE" = scroll ] || [ "$MODE" = two-windows ]; then
  # **Firefox in use, for SECS, on the forwarded driver** (docs/design.md
  # §0.27). `sv-firefox-soak.py` drives it through Marionette: a tour of local
  # pages and real sites, wheel-scrolled, with a WebGL page whose frames are
  # read back, and the compositor asked after every page. The verdict below is
  # about what survived and what was used, not about the script returning.
  PY=${SVWL_PYTHON:-python3}
  command -v "$PY" >/dev/null || { echo "FAIL: no $PY; run under 'nix shell nixpkgs#python3'"; exit 1; }
  MPORT=${SVWL_MARIONETTE_PORT:-$((28000 + $$ % 1000))}
  # **`media.cubeb.force_null_context`, because about:support can wedge the
  # main thread without it**, and that has nothing to do with graphics
  # (docs/design.md §0.28.9). Its load handler asks `GetCurrentMaxAudioChannels`,
  # which goes through audioipc to the audio process; this Firefox has the
  # harness's own XDG_RUNTIME_DIR and so no audio server, and that IPC did not
  # answer in 6 of 22 short runs — two cores taken mid-stall have the main thread
  # parked in `audioipc::rpccore::Proxy::call`. `media.cubeb.sandbox=false` does
  # not help: CubebUtils turns the sandbox back on whenever the content sandbox
  # is. The null context is Firefox's own switch for a session with no audio,
  # and this is one.
  cat >>"$RUN/profile/user.js" <<PREFS
user_pref("marionette.port", $MPORT);
user_pref("webgl.sanitize-unmasked-renderer", false);
user_pref("media.cubeb.force_null_context", true);
user_pref("layout.css.devPixelsPerPx", "1.0");
PREFS
  mkdir -p "$RUN/pages"
  {
    echo '<!doctype html><title>sv long page</title><style>'
    echo 'body{font:16px sans-serif;margin:2em} .card{margin:1em 0;padding:1em;border-radius:12px;'
    echo 'background:linear-gradient(135deg,#fde,#def);box-shadow:0 4px 12px #0003;transform:rotate(-0.5deg)}'
    echo '.spin{width:40px;height:40px;background:#c33;animation:s 2s linear infinite;display:inline-block}'
    echo '@keyframes s{to{transform:rotate(360deg)}}</style>'
    for i in $(seq 1 200); do
      echo "<div class=card><div class=spin></div> <b>Card $i</b> — Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.</div>"
    done
  } >"$RUN/pages/long.html"
  cat >"$RUN/pages/canvas.html" <<'HTML'
<!doctype html><title>sv canvas 2d</title><canvas id=c width=900 height=600></canvas>
<div style="height:3000px;background:linear-gradient(#fff,#88f)"></div>
<script>
const x = c.getContext('2d'); let t = 0;
(function f(){ t++; x.fillStyle='hsl('+(t%360)+',60%,80%)'; x.fillRect(0,0,900,600);
  for (let i=0;i<200;i++){ x.beginPath(); x.arc(450+300*Math.sin(t/50+i), 300+200*Math.cos(t/37+i*1.3), 10, 0, 7);
  x.fillStyle='hsl('+((i*7+t)%360)+',80%,50%)'; x.fill(); } requestAnimationFrame(f); })();
</script>
HTML
  # The page from the guest session that ran out of buffers (waycompose-design.md
  # §95): its title changes ten times a second, so the tab strip and the window
  # title redraw continuously, and a little text below it does too.
  cat >"$RUN/pages/retitle.html" <<'HTML'
<!doctype html><title>sv retitle</title><style>body{font:16px sans-serif;margin:2em}</style>
<h1 id=h>0</h1><div style="height:3000px;background:linear-gradient(#fff,#f88)"></div>
<script>let n = 0; setInterval(() => { n++; document.title = 'sv retitle ' + n; h.textContent = n; }, 100);</script>
HTML
  # `SVWL_VIDEO=1` (two-windows): the first window plays a 720p H.264 clip in a
  # loop for the whole run instead of retitling, and says how playback goes in
  # its title (waycompose-design.md §97: two windows plus a video; §98: the
  # recipe that made Firefox crash at quit).
  if [ "${SVWL_VIDEO:-0}" = 1 ]; then
    ffmpeg -y -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=10" \
      -c:v libx264 -profile:v high -preset fast -b:v 3M -pix_fmt yuv420p -movflags +faststart \
      "$RUN/pages/clip.mp4" || { echo "FAIL: ffmpeg could not make the clip"; exit 1; }
    cat >"$RUN/pages/video.html" <<'HTML'
<!doctype html><title>sv video</title>
<video id=v src="clip.mp4" autoplay muted loop width=960></video>
<script>
const v = document.getElementById('v');
setInterval(() => {
  const q = v.getVideoPlaybackQuality();
  document.title = `sv video frames ${q.totalVideoFrames} dropped ${q.droppedVideoFrames} err=${v.error ? v.error.code : 'none'}`;
}, 100);
</script>
HTML
    export SOAK_VIDEO=1
  fi
  cat >"$RUN/pages/webgl.html" <<'HTML'
<!doctype html><title>sv webgl</title><canvas id=c width=800 height=600></canvas>
<div style="height:3000px;background:linear-gradient(#fff,#8f8)"></div>
<script>
window.__sv = {frames: 0};
try {
  const gl = c.getContext('webgl2') || c.getContext('webgl');
  if (!gl) throw new Error('no WebGL context');
  const dbg = gl.getExtension('WEBGL_debug_renderer_info');
  __sv.renderer = dbg ? gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
  __sv.version = gl.getParameter(gl.VERSION);
  const sh = (t, s) => { const o = gl.createShader(t); gl.shaderSource(o, s); gl.compileShader(o); return o; };
  const p = gl.createProgram();
  gl.attachShader(p, sh(gl.VERTEX_SHADER, 'attribute vec2 a; uniform float t; void main(){ float c=cos(t), s=sin(t); gl_Position=vec4(mat2(c,-s,s,c)*a,0.,1.); }'));
  gl.attachShader(p, sh(gl.FRAGMENT_SHADER, 'precision mediump float; void main(){ gl_FragColor=vec4(1.,.5,.1,1.); }'));
  gl.linkProgram(p); gl.useProgram(p);
  gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([0,.9,-.8,-.6,.8,-.6]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0); gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  const ut = gl.getUniformLocation(p, 't'), px = new Uint8Array(4);
  (function f(){ gl.clearColor(.1,.2,.3,1); gl.clear(gl.COLOR_BUFFER_BIT);
    gl.uniform1f(ut, __sv.frames/60); gl.drawArrays(gl.TRIANGLES, 0, 3);
    gl.readPixels(400, 300, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px); __sv.pixel = Array.from(px);
    __sv.frames++; requestAnimationFrame(f); })();
} catch (e) { __sv.error = String(e); }
</script>
HTML
  say "firefox (soak), $SECS s through Marionette on port $MPORT"
  # `MOZ_CRASHREPORTER_NO_DELETE_DUMP=1` in Firefox's own environment: without
  # it the crash reporter submits a crash's minidump and deletes it, and the
  # verdict's count of minidumps below says none (waycompose-design.md §98).
  env HOME="$RUN/home" MOZ_CRASHREPORTER_NO_DELETE_DUMP=1 ${SVWL_EXTRA_ENV:-} \
    "$FIREFOX" --no-remote --marionette -remote-allow-system-access --profile "$RUN/profile" about:blank \
    >"$RUN/ff.log" 2>&1 &
  FF=$!
  KIDS+=($FF)
  STARTED=$(date +%s)
  # A screenshot a minute, so the record has pictures from the whole run, and
  # whether the parent process has our ICD mapped at that moment.
  ( while kill -0 $FF 2>/dev/null; do sleep 60; shot
      grep -c "libsv_icd.so" "/proc/$FF/maps" 2>/dev/null | sed "s/^/t=$(( $(date +%s) - STARTED ))s mappings=/" >>"$RUN/icd-mapped.txt"
    done ) &
  KIDS+=($!)
  # `SVWL_STALL_MS=N`: stop the GPU side for N ms every `SVWL_STALL_EVERY`
  # seconds (default 20), from 30 s on. A `SIGSTOP` stops every thread of it,
  # its heartbeat too, which is what a guest VM that is not scheduled looks
  # like from the host (waycompose-design.md §95: the iGPU's D3->D0, a host
  # with 244 MB free). Each stall is a line in stalls.txt.
  if [ -n "${SVWL_STALL_MS:-}" ] && [ -s "$RUN/gpu.pid" ]; then
    ( GP=$(cat "$RUN/gpu.pid"); sleep 30
      while kill -0 $FF 2>/dev/null; do
        kill -STOP "$GP"; sleep "$(printf '%d.%03d' $((SVWL_STALL_MS / 1000)) $((SVWL_STALL_MS % 1000)))"; kill -CONT "$GP"
        echo "t=$(( $(date +%s) - STARTED ))s stopped the GPU side for ${SVWL_STALL_MS} ms" >>"$RUN/stalls.txt"
        sleep "${SVWL_STALL_EVERY:-20}"
      done ) &
    KIDS+=($!)
  fi
  [ "$MODE" = scroll ] && export SOAK_MODE=scroll
  [ "$MODE" = two-windows ] && export SOAK_MODE=two-windows
  if [ "$MODE" = scroll ]; then
    CPUFILE="$RUN/cpu.txt"
    ( sleep 25; "$PY" "$ROOT/scripts/wc-cpu-sample.py" "${SVWL_CPU_SECS:-20}" "$CPUFILE" >/dev/null 2>&1 ) &
  fi
  timeout $((SECS + 180)) "$PY" "$ROOT/scripts/sv-firefox-soak.py" "$MPORT" "$SECS" "$RUN" \
    >"$RUN/soak.log" 2>&1
  DRIVER=$?
  if [ "$MODE" = scroll ]; then
    # **Only the frame rate while scrolling** (waycompose-design.md §88): no
    # tour, so the timeline is the scroll and nothing else.
    kill $FF 2>/dev/null; wait $FF 2>/dev/null
    say "---- scroll ----"; grep -E '"kind": "(scroll-fps|compositor|fatal)"' "$RUN/soak.jsonl"
    if [ -n "${WC_FRAME_TRACE:-}" ] && [ -f "$ROOT/scripts/wc-frame-timeline.py" ]; then
      "$PY" "$ROOT/scripts/wc-frame-timeline.py" "$RUN/trace" --harness "$RUN/harness.trace" --soak "$RUN/soak.jsonl" ${CPUFILE:+--cpu "$CPUFILE"} >"$RUN/timeline.txt" 2>&1
      cat "$RUN/timeline.txt"
    fi
    echo "run dir: $RUN"
    grep -q '"kind": "done"' "$RUN/soak.jsonl"; exit $?
  fi
  ALIVE=0; kill -0 $FF 2>/dev/null && ALIVE=1
  LIVED=$(( $(date +%s) - STARTED ))
  # `two-windows` ends by closing both windows (§95), so the browser should
  # now be quitting on its own: give it 30 s, and keep its exit status.
  QUIT=
  if [ "$MODE" = two-windows ] && grep -q '"kind": "closed"' "$RUN/soak.jsonl"; then
    for _ in $(seq 1 $(( ${SVWL_QUIT_WAIT:-30} * 2 ))); do kill -0 $FF 2>/dev/null || break; sleep 0.5; done
    if kill -0 $FF 2>/dev/null; then QUIT="still running ${SVWL_QUIT_WAIT:-30} s after its last window closed"
    else wait $FF; QUIT="exit $?"; fi
    ALIVE=1
  fi
  shot
  kill $FF 2>/dev/null; wait $FF 2>/dev/null

  fail=0
  ok()  { echo "  ok   $*"; }
  bad() { echo "  FAIL $*"; fail=1; }
  J="$RUN/soak.jsonl"
  say "---- the verdict ----"
  [ "$DRIVER" -eq 0 ] && grep -q '"kind": "done"' "$J" \
    && ok "the tour ran to the end ($(grep -c '"kind": "load"' "$J") page loads, $(grep -c '"kind": "scroll"' "$J") scrolls)" \
    || bad "the driver stopped early (exit $DRIVER): $(grep '"kind": "fatal"' "$J" | tail -1)"
  [ "$ALIVE" -eq 1 ] && ok "Firefox was alive after ${LIVED} s" || bad "Firefox was gone after ${LIVED} s"
  [ "$LIVED" -ge "$SECS" ] && ok "it lived at least the $SECS s asked for" || bad "it lived ${LIVED} s of $SECS"
  if [ -n "$QUIT" ]; then
    # A crash's minidump is moved out of the profile into the crash reporter's
    # own directory under `$HOME` (`Crash Reports/pending`), so both are counted
    # (waycompose-design.md §98).
    dumps=$(find "$RUN/profile/minidumps" "$RUN/home" -name '*.dmp' 2>/dev/null | wc -l)
    [ "$QUIT" = "exit 0" ] && [ "$dumps" -eq 0 ] \
      && ok "closing the last window quit Firefox cleanly (exit 0, no minidump)" \
      || bad "closing the last window: $QUIT, $dumps minidump(s) in the profile"
  fi
  if coredumpctl list --no-legend --since "@$STARTED" 2>/dev/null | grep -qi firefox; then
    bad "a Firefox process dumped core during the run: $(coredumpctl list --no-legend --since "@$STARTED" 2>/dev/null | grep -i firefox | head -3)"
  else
    ok "no Firefox process dumped core during the run"
  fi
  comps=$(grep '"kind": "compositor"' "$J" | sed 's/.*"value": "\([^"]*\)".*/\1/' | sort | uniq -c | tr -s ' ')
  if [ -n "$comps" ] && ! grep '"kind": "compositor"' "$J" | grep -qv '"value": "WebRender"'; then
    ok "Firefox called every compositor sample WebRender, not WebRender (Software):$comps"
  else
    bad "Firefox did not call the compositor WebRender throughout:${comps:- no samples}"
  fi
  # Every full half-minute window of the GPU side's trace, after the first,
  # must have queue submissions in it: rendering through this driver for the
  # whole run, not only at start-up.
  # At least one such window, too: a run that died inside its first minute has
  # none after the first, and "every one of none" is not a pass.
  if [ "$(tail -n +2 "$RUN/gpu-trace.txt" 2>/dev/null | wc -l)" -ge 1 ] \
     && ! tail -n +2 "$RUN/gpu-trace.txt" | grep -q "submit=0 "; then
    ok "the GPU side executed submissions in every 30 s window: $(tail -n +2 "$RUN/gpu-trace.txt" | awk '{print $3}' | tr '\n' ' ')"
  else
    bad "a 30 s window with no queue submission on the GPU side, or no trace: $(tr '\n' ';' <"$RUN/gpu-trace.txt" 2>/dev/null)"
  fi
  mapped=$(cat "$RUN/icd-mapped.txt" 2>/dev/null | tr '\n' ' ')
  echo "$mapped" | grep -q "mappings=0" || [ -z "$mapped" ] \
    && bad "the parent process did not have libsv_icd.so mapped at every sample: ${mapped:-no samples}" \
    || ok "libsv_icd.so mapped in the parent process at every sample: $mapped"
  if [ "$MODE" != two-windows ]; then
    wg=$(grep '"kind": "webgl"' "$J" | tail -1)
    if echo "$wg" | grep -qi 'zink' && echo "$wg" | grep -q '"frames": [1-9]'; then
      ok "WebGL drew on Zink: $wg"
    else
      bad "WebGL did not draw on Zink: ${wg:-no sample}"
    fi
  fi
  # waycompose-design.md §95: what the guest session printed before WebRender
  # gave up, and what this ICD now says about it whatever SV_DEBUG says.
  FAILS="ZINK: vk[A-Za-z]+ failed|couldn't allocate memory|OutOfMemory|DEVICE LOST"
  SAID="^sv-icd: (vkAllocateMemory: |vkCreateBuffer: the GPU|transport: .*; returning|blob reserve exhausted|a call failed in this ICD)"
  fails=$(grep -cE "$FAILS" "$RUN/ff.log")
  [ "$fails" -eq 0 ] && ok "no Vulkan call failed in Firefox's log" \
    || bad "$fails Vulkan failure line(s) in Firefox's log: $(grep -E "$FAILS|$SAID" "$RUN/ff.log" | sort | uniq -c | sort -rn | head -6 | tr -s ' ' | tr '\n' ';')"
  last=$(grep -E "^sv-icd: t=[0-9.]+s arena" "$RUN/ff.log" | tail -1)
  [ -n "$last" ] && echo "  arena, last SV_DEBUG line: ${last#sv-icd: }"
  [ -s "$RUN/stalls.txt" ] && echo "  GPU side stopped $(wc -l <"$RUN/stalls.txt") time(s) for ${SVWL_STALL_MS} ms"
  say "---- about:support ----"; grep '"kind": "about-support"' "$J" | tail -1
  echo "run dir: $RUN"
  [ "$fail" -eq 0 ] && say "sv-firefox-wayland soak: PASS" || say "sv-firefox-wayland soak: FAIL"
  exit "$fail"
fi

say "firefox ($MODE), $SECS s, about:support at devPixelsPerPx=$DPPX"
env HOME="$RUN/home" ${SVWL_EXTRA_ENV:-} \
  timeout "$SECS" "$FIREFOX" --no-remote --profile "$RUN/profile" about:support \
  >"$RUN/ff.log" 2>&1 &
FF=$!
KIDS+=($FF)

sleep $((SECS - 15))
say "screenshot of what $SERVER shows"
shot
wait $FF 2>/dev/null

say "---- firefox stderr (the GFX1 lines are the arbiter's negative half) ----"
grep -iE "GFX1|EGL|webrender|sv-icd|zink" "$RUN/ff.log" | head -30 | sed 's/^/  /'
say "---- the GPU side ----"; grep -iE "attached|serving" "$RUN/gpu.log" | head -6 | sed 's/^/  /'
say "---- screenshots ----"; ls -la "$RUN/shots" 2>/dev/null | tail -3
echo "run dir: $RUN"

# ------------------------------------------------------------------ verdict --
# **§35. This script had no `ok`, no `bad`, no scoreboard and no exit code.**
#
# It started weston, `sv-gpu-server` and Firefox, waited, grepped four logs and
# dumped them — and then exited with whatever the `ls -la` above returned, which
# is 0 whenever the directory exists. `wait $FF` discards Firefox's own status on
# the line before. So the run could not fail: every number that would let anyone
# judge it was printed for a human to read, which is the maximal form of the
# defect §35 is about, and the one that is easiest to miss because there is no
# `ok` call to look wrong.
#
# Three things are asserted, and all three are properties of the code rather
# than of the machine — no timing, no rate, nothing this contended box can move:
#
#  * **the GPU side served the session.** Without it Firefox reached no device
#    through us and everything below is about something else.
#  * **WebRender is on the GPU.** Firefox's `GFX1` lines are how it reports
#    falling back, and the whole point of this file is the difference between
#    accelerated and not. A fallback is silent in every other respect.
#  * **a screenshot exists and is not empty.** The screenshooter timing out
#    leaves the directory in place, which is exactly what the old exit status
#    was reading as success.
fail=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; fail=1; }
say "---- the verdict ----"

if grep -qiE "attached|serving" "$RUN/gpu.log" 2>/dev/null; then
  ok "the GPU side attached and served the session"
else
  bad "the GPU side never reported a session; Firefox did not reach a device through us"
fi

if grep -qiE "GFX1.*(fail|error|disabl|blocklist|blacklist)" "$RUN/ff.log" 2>/dev/null; then
  bad "Firefox reported a GFX1 failure and fell back; the run measured software"
  grep -iE "GFX1.*(fail|error|disabl|blocklist|blacklist)" "$RUN/ff.log" | head -3 | sed 's/^/        /'
elif grep -qiE "webrender" "$RUN/ff.log" 2>/dev/null; then
  ok "Firefox composited with WebRender and reported no GFX1 failure"
else
  bad "Firefox said nothing about WebRender either way; there is no evidence here to judge"
fi

shot=$(find "$RUN/shots" -type f -size +1k 2>/dev/null | head -1)
if [ -n "$shot" ]; then
  ok "a screenshot came back: $(basename "$shot"), $(wc -c <"$shot") bytes"
else
  bad "no screenshot over 1 KiB in $RUN/shots -- weston-screenshooter produced nothing"
fi

[ "$fail" -eq 0 ] && say "sv-firefox-wayland: PASS" || say "sv-firefox-wayland: FAIL"
exit "$fail"
