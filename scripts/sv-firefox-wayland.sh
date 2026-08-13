#!/usr/bin/env bash
#
# **`VK_KHR_wayland_surface`, on a machine that has a GPU.** The control that
# `docs/firefox-accel.md` names, run as an experiment rather than quoted as one.
#
#   scripts/sv-firefox-wayland.sh [probe|firefox] [secs]
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
BIN=$ROOT/target/release
RUN=$(mktemp -d /tmp/svwl-XXXXXX)
FIREFOX=${SVWL_FIREFOX:-$(command -v firefox)}
EGLINFO=${SVWL_EGLINFO:-/nix/store/29kh8r89yqgg1ndmfybh1wafjzichp2g-mesa-demos-9.0.0/bin/eglinfo}
DPPX=${SVWL_DPPX:-0.4}
# This machine has Mesa and its ICDs in `/run/opengl-driver` but no
# `libvulkan.so.1` on the default search path, so both the GPU side and Zink
# fail to find a loader at all. Named rather than searched for, because a
# different loader is a different measurement.
VKLOADER=${SVWL_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
say() { echo "[$(date +%H:%M:%S)] $*"; }

KIDS=()
cleanup() { kill "${KIDS[@]}" 2>/dev/null; }
trap cleanup EXIT INT TERM

mkdir -p "$RUN/xdg" "$RUN/profile" "$RUN/home" "$RUN/shots"
chmod 700 "$RUN/xdg"
export XDG_RUNTIME_DIR="$RUN/xdg"

cat >"$RUN/profile/user.js" <<PREFS
user_pref("gfx.webrender.all", true);
user_pref("gfx.webrender.software", false);
user_pref("webgl.force-enabled", true);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("layout.css.devPixelsPerPx", "$DPPX");
PREFS

# ------------------------------------------------------------- compositor ---
# `--renderer=pixman` is the whole point: a GL renderer would advertise
# `zwp_linux_dmabuf` and Mesa would take a path the real topology does not have.
say "weston, headless, pixman renderer — wl_shm and nothing else"
weston --backend=headless --renderer=pixman --width=1600 --height=1000 \
  --socket=svwl --idle-time=0 --debug >"$RUN/weston.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 40); do [ -S "$RUN/xdg/svwl" ] && break; sleep 0.5; done
[ -S "$RUN/xdg/svwl" ] || { echo "FAIL: weston never listened"; cat "$RUN/weston.log"; exit 1; }

# --------------------------------------------------------------- GPU side ---
say "sv-gpu-server on this machine's RADV"
"$BIN/sv-gpu-server" serve --clients 16 --socket "$RUN/gpu.sock" --idle $((SECS + 60)) \
  >"$RUN/gpu.log" 2>&1 &
KIDS+=($!)
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
  (cd "$RUN/shots" && timeout 20 weston-screenshooter >/dev/null 2>&1)
  wait $P 2>/dev/null
  head -40 "$RUN/probe.log"
  say "---- the ICD ----"; grep -iE "sv-icd|wl_shm|wayland|surface" "$RUN/probe.log" | head -20
  say "---- screenshots ----"; ls "$RUN/shots" 2>/dev/null
  echo "run dir: $RUN"
  exit 0
fi

if [ "$MODE" = x11 ]; then
  # Xwayland on top of the shm-only weston, and Firefox on X11 inside it. The
  # X server gets a *clean* GL environment — it is itself a GL program and an X
  # server that cannot initialise its own GL offers no fbconfig to anybody.
  say "rootful Xwayland on the shm-only weston, then Firefox on X11"
  env -u GALLIUM_DRIVER -u MESA_LOADER_DRIVER_OVERRIDE -u VK_DRIVER_FILES \
      -u SV_SOCKET -u EGL_PLATFORM LIBGL_ALWAYS_SOFTWARE=1 \
    Xwayland :78 -geometry 1600x1000 -noreset >"$RUN/xwayland.log" 2>&1 &
  KIDS+=($!)
  for _ in $(seq 1 60); do [ -S /tmp/.X11-unix/X78 ] && break; sleep 0.5; done
  [ -S /tmp/.X11-unix/X78 ] || { echo "FAIL: Xwayland never listened"; tail -5 "$RUN/xwayland.log"; exit 1; }
  export DISPLAY=:78 EGL_PLATFORM=x11 MOZ_ENABLE_WAYLAND=0 GDK_BACKEND=x11
  # **The override this whole mode exists to test.** `platform_x11.c` refuses
  # Zink outright when the X server has no DRI3 multibuffer support — which an
  # Xwayland with no DRM device never has — *unless* `LIBGL_KOPPER_DRI2` is
  # set, in which case it skips the render-device checks and goes straight to
  # `dri2_initialize_x11_kopper`. Kopper then presents through
  # `VK_KHR_xcb_surface`, which this ICD has had since §0.4.
  export LIBGL_KOPPER_DRI2=${SVWL_KOPPER_DRI2:-1}
fi

if [ "$MODE" != x11 ]; then
  # With no X server reachable `MOZ_ENABLE_WAYLAND` is a preference rather than
  # a choice; unsetting DISPLAY is what makes it one.
  unset DISPLAY
  export MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland
fi

say "firefox ($MODE), $SECS s, about:support at devPixelsPerPx=$DPPX"
env HOME="$RUN/home" ${SVWL_EXTRA_ENV:-} \
  timeout "$SECS" "$FIREFOX" --no-remote --profile "$RUN/profile" about:support \
  >"$RUN/ff.log" 2>&1 &
FF=$!
KIDS+=($FF)

sleep $((SECS - 15))
say "screenshot through weston's own screenshooter"
(cd "$RUN/shots" && timeout 20 weston-screenshooter >/dev/null 2>&1)
wait $FF 2>/dev/null

say "---- firefox stderr (the GFX1 lines are the arbiter's negative half) ----"
grep -iE "GFX1|EGL|webrender|sv-icd|zink" "$RUN/ff.log" | head -30 | sed 's/^/  /'
say "---- the GPU side ----"; grep -iE "attached|serving" "$RUN/gpu.log" | head -6 | sed 's/^/  /'
say "---- screenshots ----"; ls -la "$RUN/shots" 2>/dev/null | tail -3
echo "run dir: $RUN"
