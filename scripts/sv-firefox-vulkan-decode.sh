#!/usr/bin/env bash
#
# **Firefox decodes H.264 on a forwarded Vulkan device.** docs/video-decode.md §7.7.
#
#   scripts/sv-firefox-vulkan-decode.sh [secs]
#
#   SVFD_FIREFOX   the browser to run (default: `firefox` on PATH). The decode
#                  needs the tier-1 readback patch, nix/firefox-vulkan-readback.patch;
#                  an unpatched 157 is the control, and fails the way §7.7 says.
#   SVFD_READBACK  1 (default) sets media.hardware-video-decoding-vulkan.readback.enabled
#   SVFD_VULKAN    1 (default) turns Firefox's Vulkan decoder on at all
#   SVFD_ICD       the driver the GPU side opens (default: the dev shell's RADV)
#   SVFD_GECKODRIVER, SVFD_JQ   about:support is read through Marionette with
#                  geckodriver and jq (`nix build nixpkgs#geckodriver nixpkgs#jq.bin`);
#                  without them the decode checks still run and the support
#                  check fails
#   SVFD_BIN       sv-icd / sv-gpu-server build (default: target/release — the
#                  debug ICD overflows Firefox's Renderer stack in `lower_pnext`)
#   SVFD_CLIP      an existing clip to play instead of the generated one
#   SVFD_ARENA_MIB the GPU side's arena in MiB (default 96; the guest's slot holds 64)
#   SVFD_GPU_LAYERS  VK_INSTANCE_LAYERS for the GPU side only, e.g.
#                  VK_LAYER_SV_intel_shape:VK_LAYER_KHRONOS_validation
#                  (docs/video-decode.md §7.10), with SVFD_GPU_LAYER_PATH added
#                  to the loader's search path. Firefox never sees these.
#
# The topology is sv-firefox-wayland.sh's — the testhost one with the VM taken
# out — with Xvfb in weston's place (see below): no DRI3, no dmabuf, the GPU side on
# this machine's RADV, and Firefox reaching it only through `sv-icd` — Zink for
# WebRender, ffmpeg's Vulkan hwaccel for the video. What it adds is a video.
#
# Evidence, in the order it is worth:
#
#  1. the RDD process's own log: `[VULKAN] readback: … copied to host memory`
#     per frame, which only the patched decoder prints and only after a
#     `vkCmdDecodeVideoKHR` on the forwarded device produced the frame;
#  2. the page's own count of frames presented, from
#     `HTMLVideoElement.getVideoPlaybackQuality()`, written into its title;
#  3. the GPU side saying it served the sessions;
#  4. about:support, screenshotted — its codec table says what Firefox's probe
#     believes, not what decoded, which is why it is last.
set -uo pipefail

SECS=${1:-45}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SVFD_BIN:-$ROOT/target/release}
RUN=$(mktemp -d /tmp/svfd-XXXXXX)
FIREFOX=${SVFD_FIREFOX:-$(command -v firefox)}
READBACK=${SVFD_READBACK:-1}
VULKAN=${SVFD_VULKAN:-1}
DPPX=${SVFD_DPPX:-0.5}
if [ -z "${SVFD_ICD:-}" ]; then
  IFS=: read -ra _dirs <<<"${XDG_DATA_DIRS:-}"
  for d in "${_dirs[@]}"; do
    [ -f "$d/vulkan/icd.d/radeon_icd.x86_64.json" ] && { SVFD_ICD="$d/vulkan/icd.d/radeon_icd.x86_64.json"; break; }
  done
fi
ICD=${SVFD_ICD:-/run/opengl-driver/share/vulkan/icd.d/radeon_icd.x86_64.json}
say() { echo "[$(date +%H:%M:%S)] $*"; }

KIDS=()
cleanup() { kill "${KIDS[@]}" 2>/dev/null; }
trap cleanup EXIT INT TERM

mkdir -p "$RUN/xdg" "$RUN/profile" "$RUN/home" "$RUN/shots"
chmod 700 "$RUN/xdg"
export XDG_RUNTIME_DIR="$RUN/xdg"

# ------------------------------------------------------------------ the video ---
# H.264 High in MP4, the codec and container every Firefox build can demux, and
# the one the guest's Intel decodes (§1). Ten seconds, looped.
say "an H.264 clip and a page that plays it"
if [ -n "${SVFD_CLIP:-}" ]; then
  cp "$SVFD_CLIP" "$RUN/clip.mp4" || { echo "FAIL: no clip at $SVFD_CLIP"; exit 1; }
else
  ffmpeg -y -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=10" \
    -c:v libx264 -profile:v high -preset fast -b:v 3M -pix_fmt yuv420p -movflags +faststart \
    "$RUN/clip.mp4" || { echo "FAIL: ffmpeg could not make the clip"; exit 1; }
fi
cat >"$RUN/play.html" <<'HTML'
<!doctype html><title>starting</title>
<video id=v src="clip.mp4" autoplay muted loop width=640></video>
<pre id=s></pre>
<script>
const v = document.getElementById('v'), s = document.getElementById('s');
setInterval(() => {
  const q = v.getVideoPlaybackQuality();
  const line = `frames ${q.totalVideoFrames} dropped ${q.droppedVideoFrames} ` +
               `t=${v.currentTime.toFixed(1)} err=${v.error ? v.error.code + ' ' + v.error.message : 'none'}`;
  document.title = line; s.textContent = line;
  console.log("svfd " + line);
}, 500);
</script>
HTML

cat >"$RUN/profile/user.js" <<PREFS
user_pref("gfx.webrender.all", true);
user_pref("gfx.webrender.software", false);
user_pref("media.hardware-video-decoding-vulkan.enabled", $([ "$VULKAN" = 1 ] && echo true || echo false));
user_pref("media.hardware-video-decoding-vulkan.force-enabled", $([ "$VULKAN" = 1 ] && echo true || echo false));
user_pref("media.hardware-video-decoding-vulkan.readback.enabled", $([ "$READBACK" = 1 ] && echo true || echo false));
user_pref("media.suspend-background-video.enabled", false);
user_pref("media.autoplay.default", 0);
user_pref("devtools.console.stdout.content", true);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.sessionstore.resume_from_crash", false);
user_pref("datareporting.policy.dataSubmissionEnabled", false);
user_pref("toolkit.telemetry.enabled", false);
user_pref("layout.css.devPixelsPerPx", "$DPPX");
PREFS

# -------------------------------------------------------------- compositor ---
# An X server rather than a Wayland compositor, and the reason is this machine
# rather than the design: Mesa's Wayland EGL here reaches for a DRM node and
# then refuses Zink ("ZINK: failed to choose pdev"), with or without this
# branch, while Zink through the same ICD on Xvfb comes up on the forwarded RADV
# (docs/video-decode.md §7.7). Xvfb has no DRI3, so kopper presents through
# `VK_KHR_xcb_surface`, which the ICD answers itself — the shape firefox-accel.md
# measured Firefox rendering with.
XDISPLAY=${SVFD_DISPLAY:-:$((90 + RANDOM % 9))}
say "Xvfb $XDISPLAY, 1600x2400"
Xvfb "$XDISPLAY" -screen 0 1600x2400x24 -nolisten tcp >"$RUN/xvfb.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 40); do [ -S "/tmp/.X11-unix/X${XDISPLAY#:}" ] && break; sleep 0.5; done
[ -S "/tmp/.X11-unix/X${XDISPLAY#:}" ] || { echo "FAIL: Xvfb never listened"; cat "$RUN/xvfb.log"; exit 1; }

# ---------------------------------------------------------------- GPU side ---
# One session per Firefox process that loads the ICD — glxtest, the Vulkan
# probe, WebRender's Zink, the RDD decoder — each with its own arena.
say "sv-gpu-server on $(basename "$(dirname "$(dirname "$(dirname "$ICD")")")")'s RADV"
VK_DRIVER_FILES="$ICD" VK_INSTANCE_LAYERS="${SVFD_GPU_LAYERS:-}" VK_ADD_LAYER_PATH="${SVFD_GPU_LAYER_PATH:-}" \
  "$BIN/sv-gpu-server" serve --clients 16 --arena $((${SVFD_ARENA_MIB:-96} << 20)) \
  --socket "$RUN/gpu.sock" --idle $((SECS + 60)) >"$RUN/gpu.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 40); do [ -S "$RUN/gpu.sock" ] && break; sleep 0.5; done
[ -S "$RUN/gpu.sock" ] || { echo "FAIL: the GPU side never listened"; cat "$RUN/gpu.log"; exit 1; }

# ------------------------------------------------------------------- client ---
export DISPLAY=$XDISPLAY MOZ_ENABLE_WAYLAND=0 GDK_BACKEND=x11 EGL_PLATFORM=x11 LIBGL_KOPPER_DRI2=1
unset WAYLAND_DISPLAY
export MESA_LOADER_DRIVER_OVERRIDE=zink
export VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$RUN/gpu.sock"
# The RDD sandbox brokers `connect()` to the Wayland and X11 sockets only and
# `/dev/shm` to Firefox's own prefix, so the ICD in the decoder process could
# reach neither our socket nor our control region (docs/video-decode.md §7.7).
export MOZ_DISABLE_RDD_SANDBOX=1
# No VA-API driver may answer, so that a hardware decode can only be ours.
export LIBVA_DRIVERS_PATH=/nonexistent LIBVA_DRIVER_NAME=none
export MOZ_LOG="PlatformDecoderModule:5,FFmpegVideo:5,sync" MOZ_LOG_FILE="$RUN/moz.log"

# Marionette on a port of our own, so that about:support's data can be read as
# text — `Troubleshoot.snapshot()`, the object the page renders — rather than
# only photographed.
MPORT=${SVFD_MARIONETTE_PORT:-$((20000 + RANDOM % 20000))}
GDPORT=$((MPORT + 1))
echo "user_pref(\"marionette.port\", $MPORT);" >>"$RUN/profile/user.js"

say "firefox, $SECS s: the clip in one tab, about:support in the other"
env HOME="$RUN/home" timeout "$SECS" "$FIREFOX" --no-remote --marionette --remote-allow-system-access --profile "$RUN/profile" \
  "file://$RUN/play.html" "about:support" >"$RUN/ff.log" 2>&1 &
FF=$!
KIDS+=($FF)
sleep $((SECS - 25))

GECKODRIVER=${SVFD_GECKODRIVER:-$(command -v geckodriver)}
if [ -n "$GECKODRIVER" ]; then
  say "about:support's data, through Marionette"
  "$GECKODRIVER" --connect-existing --marionette-port "$MPORT" --port "$GDPORT" >"$RUN/geckodriver.log" 2>&1 &
  KIDS+=($!)
  sleep 1
  wd() { curl -s --max-time 30 -X POST -H 'Content-Type: application/json' "http://127.0.0.1:$GDPORT$1" -d "$2"; }
  # The session handshake has timed out under load (the decoder and the
  # compositor busy), so it is retried rather than trusted to the first attempt.
  for _ in 1 2 3; do
    wd /session '{"capabilities":{}}' >"$RUN/wd-session.json"
    grep -q sessionId "$RUN/wd-session.json" && break
    sleep 2
  done
  sid=$(sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p' "$RUN/wd-session.json")
  if [ -n "$sid" ]; then
    wd "/session/$sid/moz/context" '{"context":"chrome"}' >/dev/null
    wd "/session/$sid/execute/async" '{"args":[],"script":"const done = arguments[arguments.length-1]; const {Troubleshoot} = ChromeUtils.importESModule(\"resource://gre/modules/Troubleshoot.sys.mjs\"); Troubleshoot.snapshot().then(s => done(JSON.stringify({media: {codecSupportInfo: s.media.codecSupportInfo}, graphics: {featureLog: s.graphics.featureLog, adapterDescription: s.graphics.adapterDescription, driverVendor: s.graphics.driverVendor, windowLayerManagerType: s.graphics.windowLayerManagerType}})), e => done(String(e)));"}' \
      >"$RUN/support.json"
    # Screenshots as Firefox renders them (WebDriver, not the X framebuffer):
    # the video tab's viewport, and about:support's Media and graphics
    # "decisions" tables, each as its own element shot.
    JQ=${SVFD_JQ:-$(command -v jq)}
    wget_() { curl -s "http://127.0.0.1:$GDPORT$1"; }
    shot() { "$JQ" -r '.value' | base64 -d >"$1" 2>/dev/null; }
    if [ -n "$JQ" ]; then
      wd "/session/$sid/moz/context" '{"context":"content"}' >/dev/null
      for h in $(wget_ "/session/$sid/window/handles" | "$JQ" -r '.value[]'); do
        wd "/session/$sid/window" "{\"handle\":\"$h\"}" >/dev/null
        url=$(wget_ "/session/$sid/url" | "$JQ" -r '.value')
        case "$url" in
          about:support*)
            for sel in media-info-tbody graphics-decisions-tbody graphics-features-tbody; do
              eid=$(wd "/session/$sid/element" "{\"using\":\"css selector\",\"value\":\"#$sel\"}" |
                    "$JQ" -r '.value | to_entries[0].value // empty')
              [ -n "$eid" ] && wget_ "/session/$sid/element/$eid/screenshot" | shot "$RUN/shots/support-$sel.png"
            done ;;
          file://*play.html)
            wget_ "/session/$sid/screenshot" | shot "$RUN/shots/video-tab.png" ;;
        esac
      done
    fi
  fi
fi
say "screenshot of the X root window"
xwininfo -root -tree -display "$XDISPLAY" >"$RUN/xtree.txt" 2>&1
(xwd -root -silent -display "$XDISPLAY" | convert xwd:- "$RUN/shots/screen.png") 2>/dev/null
wait $FF 2>/dev/null

# ------------------------------------------------------------------ verdict --
fail=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; fail=1; }
logs() { cat "$RUN"/moz.log* 2>/dev/null; }
say "---- the verdict ----"

if grep -q 'attached' "$RUN/gpu.log"; then
  ok "the GPU side served $(grep -c 'attached' "$RUN/gpu.log") session(s)"
else
  bad "the GPU side served no session; Firefox reached no device through us"
fi

dev=$(logs | grep -o 'VulkanDeviceHolder: created shared VkDevice for [^"]*' | head -1)
if [ -n "$dev" ]; then ok "the RDD process made its decode device: ${dev#*for }"; else bad "no Vulkan decode device was created"; fi

n=$(logs | grep -a -c -E '\[VULKAN\] readback: [0-9]+x[0-9]+ frame copied to host memory')
if [ "$n" -gt 0 ]; then
  ok "$n frames decoded on the forwarded device and read back"
else
  bad "no frame was read back from a Vulkan decode"
  logs | grep -iE 'VULKAN|vulkan|FATAL|Failed to' | head -8 | sed 's/^/        /'
fi

# The page's own report, through `devtools.console.stdout.content`.
page=$(grep -a -o 'svfd frames [0-9]* dropped [0-9]* t=[0-9.]* err=.*' "$RUN/ff.log" | tail -1)
frames=$(echo "$page" | awk '{print $3}')
if [ -n "$page" ] && [ "${frames:-0}" -gt 0 ] && echo "$page" | grep -q 'err=none'; then
  ok "the page presented $frames frames with no media error (${page#svfd })"
else
  bad "the page did not play cleanly: ${page:-<it never reported>}"
fi

# about:support, as data. The codec table is what Firefox's probe believes the
# device can do; the feature log says whether the Vulkan decoder was allowed.
JQ=${SVFD_JQ:-$(command -v jq)}
if [ -s "$RUN/support.json" ] && [ -n "$JQ" ]; then
  "$JQ" -r '.value' "$RUN/support.json" >"$RUN/support.data.json" 2>/dev/null
  echo "  about:support → Media → codecSupportInfo:"
  echo "  about:support → Graphics: $("$JQ" -r '"\(.graphics.adapterDescription) [\(.graphics.driverVendor)], \(.graphics.windowLayerManagerType)"' "$RUN/support.data.json" 2>/dev/null)"
  "$JQ" -r '.media.codecSupportInfo // "<absent>"' "$RUN/support.data.json" 2>/dev/null | sed 's/^/        /'
  echo "  about:support → Graphics → features:"
  "$JQ" -r '.graphics.featureLog.features[]? | select(.name|test("VIDEO")) |
            "        \(.name): \(.status) \((.log // []) | map(.message // .failureId // "") | join("; "))"' \
    "$RUN/support.data.json" 2>/dev/null
  if "$JQ" -e '.media.codecSupportInfo | test("H264 [^\\n]*HWDEC")' "$RUN/support.data.json" >/dev/null 2>&1; then
    ok "about:support lists H.264 hardware decoding"
  else
    bad "about:support does not list H.264 hardware decoding"
  fi
else
  bad "no about:support data (Marionette/geckodriver/jq)"
fi

shot=$(find "$RUN/shots" -type f -size +1k 2>/dev/null | head -1)
if [ -n "$shot" ]; then ok "screenshot: $shot"; else bad "no screenshot"; fi
echo "run dir: $RUN"
[ "$fail" -eq 0 ] && say "sv-firefox-vulkan-decode: PASS" || say "sv-firefox-vulkan-decode: FAIL"
exit "$fail"
