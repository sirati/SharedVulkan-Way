#!/usr/bin/env bash
#
# **Stock Firefox on the forwarded GPU, through the dma-buf render node**
# (waycompose-design.md §112).
#
#   scripts/sv-firefox-stock.sh [secs]          (inside `nix develop`, after
#       cargo build --release -p sv-icd -p sv-gpu-server -p sv-hostd -p wc-proxy -p sv-drm
#       cargo build --release -p wc-compositor --target x86_64-pc-windows-gnu)
#
# The topology of a real session, on one machine:
#
#   wc-compositor.exe (Wine)    the guest's compositor; maps the sections
#   wc-proxy --dmabuf-node N    Firefox's Wayland server; dma-bufs become sections
#   sv-gpu-server pool-guest    the GPU side, on this machine's RADV (not the RTX 5090)
#   sv-hostd ivshmem-serve      the cold path the ICDs attach through
#   sv-drm ns                   the namespace where libdrm sees N as a render node
#   firefox                     nixpkgs' own, unpatched: WebRender on Zink over
#                               sv-icd; VA-API decode (Zink's VA driver over
#                               the forwarded Vulkan Video) in its sandboxed RDD
#
# N is the render node the proxy names and the ICD reports. Here it is a
# stand-in: by default the AMD iGPU's real node (Zink only matches it to sv-icd
# by VK_EXT_physical_device_drm; nothing is allocated on it), because a CUSE
# node needs root. SVFS_NODE names a real `sv-drm serve` node instead. The
# NVIDIA card's node is hidden from Firefox (`sv-drm ns --hide`).
#
# What it checks, each from data rather than a picture:
#   1. about:support: WebRender, not software; the adapter is Zink on the
#      forwarded device; the RDD sandbox is on;
#   2. WebGL: a page draws a triangle, reads it back, reports the renderer;
#   3. H.264 (and H.265 with SVFS_CODECS) decoded by VA-API in the RDD
#      process: its `VA-API frame` lines, one per decoded frame, and the page's own frame count with no error;
#   4. zero copy: every commit of Firefox's surfaces reached the compositor
#      as a section (`px.commit ... copied=0 zero_copy=1`), the proxy copied
#      nothing for them;
#   5. the GPU side's account: sessions, sections, cross-session imports.
#
#   SVFS_FIREFOX   the browser (default: nixpkgs' firefox on PATH)
#   SVFS_ICD       the GPU side's driver (default: the dev shell's RADV)
#   SVFS_PCI       forwarded GPU PCI ids VVVV:DDDD, required for CUSE nodes
#   SVFS_NODE      the render node (default /dev/dri/renderD129 if it is AMD)
#   SVFS_CODECS    "H264" (default); "H264 HEVC" once Zink's VA driver has H.265
#   SVFS_MESA      the GPU sessions' Mesa (a store path of nix/mesa-zink-va.nix);
#                  without it there is no VA-API driver and no decode check passes
#   SVFS_BIN       Linux build dir (default target/release)
set -uo pipefail

# Reject inherited sandbox overrides before launching any application.
if env | grep -E '^MOZ_(DISABLE_.*SANDBOX|FORCE_DISABLE_E10S_SANDBOX|CONTENT_SANDBOX_LEVEL|RDD_SANDBOX_LEVEL|GPU_SANDBOX_LEVEL)=' >/dev/null; then
  echo "FAIL: the invoking environment overrides a Firefox sandbox"
  exit 2
fi

SECS=${1:-40}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SVFS_BIN:-$ROOT/target/release}
WIN_BIN=${SVFS_WIN_BIN:-$ROOT/target/x86_64-pc-windows-gnu/release}
SCRATCH=${SVFS_SCRATCH:-/mnt/datasets/builds/scratch/stock-firefox}
FIREFOX=${SVFS_FIREFOX:-$(command -v firefox)}
CODECS=${SVFS_CODECS:-H264}
say() { echo "[$(date +%H:%M:%S)] $*"; }

for t in wine Xvfb socat ffmpeg curl; do
  command -v "$t" >/dev/null || { echo "need $t (run inside nix develop)"; exit 2; }
done
for b in sv-gpu-server sv-hostd wc-proxy sv-drm libsv_icd.so; do
  [ -e "$BIN/$b" ] || { echo "no $BIN/$b (cargo build --release first)"; exit 2; }
done
[ -f "$WIN_BIN/wc-compositor.exe" ] || { echo "no $WIN_BIN/wc-compositor.exe"; exit 2; }
[ -x "$FIREFOX" ] || { echo "no firefox (SVFS_FIREFOX)"; exit 2; }

# The GPU side's driver: RADV, never NVIDIA.
if [ -z "${SVFS_ICD:-}" ]; then
  IFS=: read -ra _dirs <<<"${XDG_DATA_DIRS:-}"
  for d in "${_dirs[@]}"; do
    [ -f "$d/vulkan/icd.d/radeon_icd.x86_64.json" ] && { SVFS_ICD="$d/vulkan/icd.d/radeon_icd.x86_64.json"; break; }
  done
fi
ICD=${SVFS_ICD:?no RADV manifest; set SVFS_ICD}

# The render node.
NODE=${SVFS_NODE:-}
HIDE=()
if [ -z "$NODE" ]; then
  for n in /sys/class/drm/renderD*; do
    [ "$(cat "$n/device/vendor" 2>/dev/null)" = 0x1002 ] && { NODE=/dev/dri/$(basename "$n"); break; }
  done
fi
[ -c "${NODE:-}" ] || { echo "no render node (SVFS_NODE)"; exit 2; }
for n in /sys/class/drm/renderD* /sys/class/drm/card[0-9]; do
  [ "$(cat "$n/device/vendor" 2>/dev/null)" = 0x10de ] && HIDE+=(--hide "/dev/dri/$(basename "$n")")
done
DEV=$(stat -c '%Hr:%Lr' "$NODE")
MINOR=${NODE##*renderD}
PCI=()
if [ -n "${SVFS_PCI:-}" ]; then
  PCI=(--pci "$SVFS_PCI")
elif [ -r "/sys/class/drm/renderD$MINOR/device/vendor" ] && [ -r "/sys/class/drm/renderD$MINOR/device/device" ]; then
  vendor=$(cat "/sys/class/drm/renderD$MINOR/device/vendor")
  device=$(cat "/sys/class/drm/renderD$MINOR/device/device")
  PCI=(--pci "${vendor#0x}:${device#0x}")
else
  echo "need SVFS_PCI=VVVV:DDDD for the forwarded render node"
  exit 2
fi

mkdir -p "$SCRATCH"
RUN=$(mktemp -d "$SCRATCH/run-XXXXXX")
mkdir -p "$RUN/keymaps" "$RUN/rt" "$RUN/home" "$RUN/profile" "$RUN/pools" "$RUN/frames" "$RUN/xdg/config" "$RUN/xdg/data" "$RUN/xdg/state" "$RUN/xdg/cache"
chmod 700 "$RUN/rt"
export WINEPREFIX="${SVFS_WINEPREFIX:-$SCRATCH/wp}"
export WINEDEBUG="${WINEDEBUG:--all}"
KIDS=()
cleanup() {
  kill "${KIDS[@]}" 2>/dev/null
  wineserver -k 2>/dev/null
  rm -f "${POOL:-}"
}
trap cleanup EXIT INT TERM

if [ ! -f "$WINEPREFIX/system.reg" ] || [ ! -d "$WINEPREFIX/drive_c/windows" ]; then
  say "booting a Wine prefix in $WINEPREFIX"
  wineboot -i >/dev/null 2>&1; wineserver -w
fi

# ------------------------------------------------------------------ pages ---
say "clips and pages"
for c in $CODECS; do
  case $c in
    H264) enc=(-c:v libx264 -profile:v high -pix_fmt yuv420p) ;;
    HEVC) enc=(-c:v libx265 -pix_fmt yuv420p -tag:v hvc1 -x265-params log-level=error) ;;
    *) echo "unknown codec $c"; exit 2 ;;
  esac
  ffmpeg -y -loglevel error -f lavfi -i "testsrc2=size=1280x720:rate=30:duration=10" \
    "${enc[@]}" -preset fast -b:v 3M -movflags +faststart "$RUN/clip-$c.mp4" \
    || { echo "FAIL: ffmpeg could not make the $c clip"; exit 1; }
done
cat >"$RUN/play.html" <<'HTML'
<!doctype html><title>starting</title>
<video id=v autoplay muted loop width=640></video>
<pre id=s></pre>
<script>
const codec = location.hash.slice(1) || 'H264';
const v = document.getElementById('v'), s = document.getElementById('s');
v.src = 'clip-' + codec + '.mp4';
setInterval(() => {
  const q = v.getVideoPlaybackQuality();
  const line = `${codec} frames ${q.totalVideoFrames} dropped ${q.droppedVideoFrames} ` +
               `t=${v.currentTime.toFixed(1)} err=${v.error ? v.error.code + ' ' + v.error.message : 'none'}`;
  document.title = line; s.textContent = line;
  console.log("svfs " + line);
}, 500);
</script>
HTML
cat >"$RUN/webgl.html" <<'HTML'
<!doctype html><title>webgl</title><canvas id=c width=256 height=256></canvas><pre id=s></pre>
<script>
function run() {
  const gl = document.getElementById('c').getContext('webgl2', {preserveDrawingBuffer: true});
  if (!gl) return 'no-context';
  const ext = gl.getExtension('WEBGL_debug_renderer_info');
  const renderer = ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
  const sh = (t, src) => { const s = gl.createShader(t); gl.shaderSource(s, src); gl.compileShader(s); return s; };
  const p = gl.createProgram();
  gl.attachShader(p, sh(gl.VERTEX_SHADER, '#version 300 es\nin vec2 a; void main(){gl_Position=vec4(a,0,1);}'));
  gl.attachShader(p, sh(gl.FRAGMENT_SHADER, '#version 300 es\nprecision mediump float; out vec4 o; void main(){o=vec4(1,0.5,0,1);}'));
  gl.linkProgram(p); gl.useProgram(p);
  const b = gl.createBuffer(); gl.bindBuffer(gl.ARRAY_BUFFER, b);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1,-1, 1,-1, -1,1]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0); gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  gl.clearColor(0, 0, 1, 1); gl.clear(gl.COLOR_BUFFER_BIT); gl.drawArrays(gl.TRIANGLES, 0, 3);
  const px = new Uint8Array(4 * 256 * 256); gl.readPixels(0, 0, 256, 256, gl.RGBA, gl.UNSIGNED_BYTE, px);
  let orange = 0, blue = 0;
  for (let i = 0; i < px.length; i += 4) {
    if (px[i] > 240 && px[i+1] > 110 && px[i+1] < 145 && px[i+2] < 15) orange++;
    else if (px[i] < 15 && px[i+1] < 15 && px[i+2] > 240) blue++;
  }
  return `renderer=${renderer} orange=${orange} blue=${blue}`;
}
const r = run(); document.title = 'webgl ' + r; document.getElementById('s').textContent = r;
console.log('svfs webgl ' + r);
</script>
HTML

# ---------------------------------------------------------------- compositor ---
DISP=$((100 + RANDOM % 400))
Xvfb ":$DISP" -screen 0 1920x1200x24 -nolisten tcp >"$RUN/xvfb.log" 2>&1 &
KIDS+=($!)
export DISPLAY=":$DISP"
for _ in $(seq 1 100); do [ -S "/tmp/.X11-unix/X$DISP" ] && break; sleep 0.1; done
SOCK="wcf$$"
wine explorer "/desktop=wc-ff-$$,1920x1200" "$WIN_BIN/wc-compositor.exe" --socket "$SOCK" \
  --timeout $((SECS * 4 + 120)) --shared-dir "$(winepath -w "$RUN/keymaps" 2>/dev/null)" --shared-peer-prefix "$RUN/keymaps/" --log "$(winepath -w "$RUN/srv.log" 2>/dev/null)" >"$RUN/loader.log" 2>&1 &
KIDS+=($!)
NODE_SOCK=""
for _ in $(seq 1 600); do
  NODE_SOCK=$(find "$WINEPREFIX/drive_c" -type s -name "$SOCK" 2>/dev/null | head -1)
  [ -n "$NODE_SOCK" ] && break
  sleep 0.2
done
[ -n "$NODE_SOCK" ] || { echo "FAIL: the compositor never made its socket"; tail "$RUN/loader.log"; exit 1; }

# ----------------------------------------------------------------- GPU side ---
PORT=$((30000 + RANDOM % 20000))
# SVFS_GPU=serve: `sv-gpu-server serve`, whose arena is a dma-buf the
# driver exported (the development path; RADV here, for a local look).
# The default is the deployed shape, pool-guest behind sv-hostd.
if [ "${SVFS_GPU:-pool}" = serve ]; then
  VK_DRIVER_FILES="$ICD" "$BIN/sv-gpu-server" serve --clients 16 --arena $((256 << 20)) \
    --socket "$RUN/icd.sock" --idle $((SECS * 4 + 120)) >"$RUN/gpu.log" 2>&1 &
  GPID=$!
  KIDS+=($GPID)
  for _ in $(seq 1 100); do [ -S "$RUN/icd.sock" ] && break; sleep 0.1; done
else
# On tmpfs: RADV imports a host pointer only into shmem pages (amdgpu
# userptr), and the pool is imported whole as the sessions' arena.
POOL=/dev/shm/svfs-$$-gpu
truncate -s 1G "$POOL"
say "pool-guest on $(basename "$ICD"), port $PORT"
VK_DRIVER_FILES="$ICD" "$BIN/sv-gpu-server" pool-guest --pool-file "$POOL" --port "$PORT" \
  --sessions 8 --idle $((SECS * 4 + 120)) >"$RUN/gpu.log" 2>&1 &
GPID=$!
KIDS+=($GPID)
for _ in $(seq 1 100); do grep -q "publishing the extent" "$RUN/gpu.log" && break; sleep 0.1; done
grep -q "publishing the extent" "$RUN/gpu.log" || { echo "FAIL: the GPU side never listened"; cat "$RUN/gpu.log"; exit 1; }
socat "UNIX-LISTEN:$RUN/cold.sock,fork,reuseaddr" "TCP:127.0.0.1:$PORT" 2>"$RUN/socat.log" &
KIDS+=($!)
"$BIN/sv-hostd" ivshmem-serve --region "$POOL" --cold "$RUN/cold.sock" --socket "$RUN/icd.sock" \
  >"$RUN/hostd.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 50); do [ -S "$RUN/icd.sock" ] && break; sleep 0.1; done
fi
printf '{"file_format_version":"1.0.1","ICD":{"library_path":"%s","api_version":"1.4.0"}}\n' \
  "$BIN/libsv_icd.so" >"$RUN/sv_icd.json"

# -------------------------------------------------------------------- proxy ---
WC_FRAME_TRACE="$RUN/proxy-trace.txt" XDG_RUNTIME_DIR="$RUN/rt" "$BIN/wc-proxy" \
  --upstream "$NODE_SOCK" --socket wl-proxy --runtime-dir "$RUN/rt" \
  --transport shared-file --pool-dir "$RUN/pools" \
  --peer-prefix "$(winepath -w "$RUN/pools" 2>/dev/null)\\" \
  --dmabuf-node "$NODE" \
  --accept-timeout 60 --timeout $((SECS * 4 + 90)) >"$RUN/proxy.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 200); do grep -q '^\[proxy\] listening on' "$RUN/proxy.log" && break; sleep 0.1; done

# ------------------------------------------------------------------ firefox ---
MPORT=$((20000 + RANDOM % 9000))
cat >"$RUN/profile/user.js" <<PREFS
user_pref("marionette.port", $MPORT);
PREFS
# Lavapipe only: Firefox blocklists a renderer string naming llvmpipe, so a
# local stand-in GPU side needs WebRender forced on to exercise the
# hardware path at all. Never set for a real GPU (the guest proof).
[ "${SVFS_LAVAPIPE_FORCE_WR:-0}" = 1 ] && echo 'user_pref("gfx.webrender.all", true);' >>"$RUN/profile/user.js"
FFENV=(
  HOME="$RUN/home" XDG_CONFIG_HOME="$RUN/xdg/config" XDG_DATA_HOME="$RUN/xdg/data"
  XDG_STATE_HOME="$RUN/xdg/state" XDG_CACHE_HOME="$RUN/xdg/cache"
  XDG_RUNTIME_DIR="$RUN/rt" WAYLAND_DISPLAY=wl-proxy MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland
  VK_DRIVER_FILES="$RUN/sv_icd.json" SV_SOCKET="$RUN/icd.sock" SV_DRM_NODE="$NODE"
  MESA_LOADER_DRIVER_OVERRIDE=zink
  ${SVFS_DEVICE_TYPE:+SV_DEVICE_TYPE=$SVFS_DEVICE_TYPE}
  MOZ_LOG="PlatformDecoderModule:5,FFmpegVideo:5,Dmabuf:4,WidgetWayland:3" MOZ_LOG_FILE="$RUN/moz.log"
)
# The GPU sessions' Mesa (nix/mesa-zink-va.nix): Zink for EGL and GBM, and
# its VA-API driver, the only one Firefox can find (LIBVA_DRIVER_NAME=zink).
if [ -n "${SVFS_MESA:-}" ]; then
  FFENV+=(__EGL_VENDOR_LIBRARY_FILENAMES="$SVFS_MESA/share/glvnd/egl_vendor.d/50_mesa.json"
    LIBGL_DRIVERS_PATH="$SVFS_MESA/lib/dri" LIBVA_DRIVER_NAME=zink LIBVA_DRIVERS_PATH="$SVFS_MESA/lib/dri"
    GBM_BACKENDS_PATH="$SVFS_MESA/lib/gbm")
else
  FFENV+=(LIBVA_DRIVERS_PATH=/nonexistent LIBVA_DRIVER_NAME=none)
fi
# An explicit client Mesa must not be overwritten by the GPU-side driver.
[ -n "${SVFS_MESA:-}${__EGL_VENDOR_LIBRARY_FILENAMES:-}" ] || {
  m=$(dirname "$(dirname "$(dirname "$(dirname "$ICD")")")")
  [ -f "$m/share/glvnd/egl_vendor.d/50_mesa.json" ] && FFENV+=(__EGL_VENDOR_LIBRARY_FILENAMES="$m/share/glvnd/egl_vendor.d/50_mesa.json")
}
unset DISPLAY

say "firefox $("$FIREFOX" --version 2>/dev/null), $SECS s, node $NODE ($DEV) ${HIDE[*]}"
env "${FFENV[@]}" timeout $((SECS + 30)) "$BIN/sv-drm" ns --dev "$DEV" --minor "$MINOR" "${PCI[@]}" "${HIDE[@]}" -- \
  "$FIREFOX" --no-remote --marionette --remote-allow-system-access --profile "$RUN/profile" \
  "file://$RUN/webgl.html" >"$RUN/ff.log" 2>&1 &
FF=$!
KIDS+=($FF)
sleep $((SECS - 15))

GECKODRIVER=${SVFS_GECKODRIVER:-$(command -v geckodriver)}
JQ=${SVFS_JQ:-$(command -v jq)}
if [ -n "$GECKODRIVER" ] && [ -n "$JQ" ]; then
  GDPORT=$((MPORT + 1))
  "$GECKODRIVER" --connect-existing --marionette-port "$MPORT" --port "$GDPORT" >"$RUN/geckodriver.log" 2>&1 &
  KIDS+=($!)
  sleep 1
  wd() { curl -s --max-time 30 -X POST -H 'Content-Type: application/json' "http://127.0.0.1:$GDPORT$1" -d "$2"; }
  for _ in 1 2 3; do
    wd /session '{"capabilities":{}}' >"$RUN/wd-session.json"
    grep -q sessionId "$RUN/wd-session.json" && break
    sleep 2
  done
  sid=$(sed -n 's/.*"sessionId":"\([^"]*\)".*/\1/p' "$RUN/wd-session.json")
  if [ -n "$sid" ]; then
    # Collect results through WebDriver without changing media or console prefs.
    payload=$("$JQ" -nc --arg url "file://$RUN/webgl.html" '{url:$url}')
    wd "/session/$sid/url" "$payload" >"$RUN/navigation-webgl.json"
    wd "/session/$sid/execute/sync" '{"args":[],"script":"return document.getElementById(\"s\").textContent"}' >"$RUN/webgl.json"
    "$JQ" -r '"svfs webgl " + .value' "$RUN/webgl.json" >>"$RUN/page.log"
    for c in $CODECS; do
      payload=$("$JQ" -nc --arg url "file://$RUN/play.html#$c" '{url:$url}')
      wd "/session/$sid/url" "$payload" >"$RUN/navigation-$c.json"
      sleep 10
      wd "/session/$sid/execute/sync" '{"args":[],"script":"return document.getElementById(\"s\").textContent"}' >"$RUN/video-$c.json"
      "$JQ" -r '"svfs " + .value' "$RUN/video-$c.json" >>"$RUN/page.log"
    done
    wd "/session/$sid/moz/context" '{"context":"chrome"}' >/dev/null
    wd "/session/$sid/execute/async" '{"args":[],"script":"const done = arguments[arguments.length-1]; const {Troubleshoot} = ChromeUtils.importESModule(\"resource://gre/modules/Troubleshoot.sys.mjs\"); Troubleshoot.snapshot().then(s => done(JSON.stringify({prefOverrides:[\"gfx.webrender.all\",\"media.hardware-video-decoding.enabled\",\"media.hardware-video-decoding.force-enabled\",\"media.hardware-video-decoding-vulkan.enabled\",\"media.ffmpeg.vaapi.enabled\",\"security.sandbox.content.level\",\"security.sandbox.rdd.level\",\"media.rdd-process.enabled\"].filter(k=>Services.prefs.prefHasUserValue(k)), defaultContentSandboxLevel: Services.prefs.getDefaultBranch(\"\").getIntPref(\"security.sandbox.content.level\"), media: {codecSupportInfo: s.media.codecSupportInfo}, sandbox: s.sandbox, graphics: {featureLog: s.graphics.featureLog, adapterDescription: s.graphics.adapterDescription, driverVendor: s.graphics.driverVendor, windowLayerManagerType: s.graphics.windowLayerManagerType, webgl2Renderer: s.graphics.webgl2Renderer, failures: s.graphics.failures}})), e => done(String(e)));"}' \
      >"$RUN/support.json"
  fi
fi
# Capture the actual RDD's kernel sandbox state while it is still alive.
# Limit the walk to this browser's descendants, not other users' browsers.
descendants() {
  local child
  for child in $(pgrep -P "$1" 2>/dev/null); do
    echo "$child"
    descendants "$child"
  done
}
: >"$RUN/rdd-status.txt"
for pid in $(descendants "$FF"); do
  if [ "$(cat "/proc/$pid/comm" 2>/dev/null)" = "RDD Process" ]; then
    grep -E '^(Name|Pid|NoNewPrivs|Seccomp):' "/proc/$pid/status" >>"$RUN/rdd-status.txt"
  fi
done
# What the compositor put on its desktop, for a reader to judge.
(xwd -root -silent -display ":$DISP" | magick xwd:- "$RUN/screen.png") 2>/dev/null
wait $FF 2>/dev/null

# ------------------------------------------------------------------ verdict ---
fail=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; fail=1; }
logs() { cat "$RUN"/moz.log* 2>/dev/null; }
say "---- the verdict ($RUN) ----"
"$JQ" -r '.value' "$RUN/support.json" >"$RUN/support.data.json" 2>/dev/null
sup() { "$JQ" -r "$1" "$RUN/support.data.json" 2>/dev/null; }

lm=$(sup '.graphics.windowLayerManagerType')
adapter=$(sup '.graphics.adapterDescription')
case "$lm" in
  WebRender) ok "WebRender on the GPU (adapter: $adapter)" ;;
  *) bad "compositing is '${lm:-unknown}', not hardware WebRender (adapter: $adapter)"; sup '.graphics.failures[]?' | head -5 | sed 's/^/        /' ;;
esac
case "$adapter" in *zink*|*Zink*) ok "the adapter is Zink on the forwarded device" ;; *) bad "the adapter is not Zink: $adapter" ;; esac

wgl=$(grep -a -o 'svfs webgl .*' "$RUN/page.log" | tail -1)
o=$(sed -n 's/.*orange=\([0-9]*\).*/\1/p' <<<"$wgl"); b=$(sed -n 's/.*blue=\([0-9]*\).*/\1/p' <<<"$wgl")
wr=$(sup '.graphics.webgl2Renderer')
if [[ "$wr" == *zink* ]] && [ "${o:-0}" -gt 30000 ] && [ "${b:-0}" -gt 30000 ]; then
  ok "WebGL draws on the forwarded device: $wr; ${wgl#svfs webgl }"
else
  bad "WebGL: $wr; ${wgl:-<the page never reported>}"
fi

for c in $CODECS; do
  page=$(grep -a -o "svfs $c frames [0-9]* dropped [0-9]* t=[0-9.]* err=.*" "$RUN/page.log" | tail -1)
  frames=$(awk '{print $4}' <<<"$page")
  if [ -n "$page" ] && [ "${frames:-0}" -gt 60 ] && grep -q 'err=none' <<<"$page"; then
    ok "$c: the page presented $frames frames with no media error (${page#svfs })"
  else
    bad "$c: the page did not play cleanly: ${page:-<it never reported>}"
  fi
  listed=$("$JQ" -r --arg c "$c" '.media.codecSupportInfo // "" | split("\n")[] | select(startswith($c + " "))' "$RUN/support.data.json" 2>/dev/null)
  [[ "$listed" == *HWDEC* ]] && ok "about:support: $listed" || bad "about:support does not list $c hardware decoding ($listed)"
done
n=$(logs | grep -a -c "VA-API frame pts=")
grep -a -q "VA-API FFmpeg init successful" "$RUN"/moz.log* 2>/dev/null && ok "the RDD process opened VA-API on the render node (Zink's VA driver)" || bad "VA-API did not initialise in the RDD process"
[ "${n:-0}" -gt 100 ] && ok "$n frames decoded by VA-API on the forwarded device and handed over as dma-bufs" \
  || { bad "only ${n:-0} VA-API-decoded frames were exported"; logs | grep -a -iE 'VULKAN.*(fail|error)|Failed to' | head -8 | sed 's/^/        /'; }

overrides=$(sup '.prefOverrides[]?')
if [ "${SVFS_LAVAPIPE_FORCE_WR:-0}" = 1 ]; then
  overrides=$(grep -vx 'gfx.webrender.all' <<<"$overrides")
fi
if [ "$(sup '.prefOverrides | type')" = array ] && [ -z "$overrides" ]; then
  if [ "${SVFS_LAVAPIPE_FORCE_WR:-0}" = 1 ]; then
    ok "no overrides beyond the explicit lavapipe-only WebRender test preference"
  else
    ok "graphics, decoder and sandbox preferences retain their defaults"
  fi
else
  bad "graphics, decoder or sandbox preferences were overridden: ${overrides:-unknown}"
fi
level=$(sup '.sandbox.effectiveContentSandboxLevel')
default_level=$(sup '.defaultContentSandboxLevel')
if [[ "$level" =~ ^[0-9]+$ ]] && [ "$level" -gt 0 ] && [ "$level" = "$default_level" ]; then
  ok "Firefox reports an active content sandbox at level $level"
else
  bad "Firefox content sandbox level ${level:-unknown} differs from its default ${default_level:-unknown}"
fi
if grep -qE '^Seccomp:[[:space:]]+2$' "$RUN/rdd-status.txt" &&
   grep -qE '^NoNewPrivs:[[:space:]]+1$' "$RUN/rdd-status.txt"; then
  ok "the browser's RDD process has seccomp filtering and NoNewPrivs enabled"
else
  bad "no sandboxed RDD process was observed in this browser's process tree"
fi

stats=$(grep -m1 "commits, " "$RUN/proxy.log")
commits=$(sed -n 's/.*dmabuf_commits=\([0-9]*\).*/\1/p' <<<"$stats")
zc=$(grep -c "px.commit .*zero_copy=1" "$RUN/proxy-trace.txt" 2>/dev/null)
# `copied` is a byte count. Empty/damage-only commits with copied=0 do not
# become copies merely because they carry no zero_copy marker.
cp=$(grep -Ec "px.commit .*copied=[1-9][0-9]*" "$RUN/proxy-trace.txt" 2>/dev/null)
say "proxy: $stats"
[ "${cp:-0}" -eq 0 ] || bad "the proxy copied $cp Firefox commits"
[ "${zc:-0}" -gt 100 ] && ok "$zc of Firefox's commits reached the compositor as sections, zero copy ($cp copied ones)" \
  || bad "zero-copy commits: ${zc:-0}, copied: ${cp:-0}"

say "---- the GPU side's account ----"
grep -E "attached|shared section|svImportShared|imported \(type|fence record|sync fd export" "$RUN/gpu.log" | cut -c1-200 | sort | uniq -c | sort -rn | head -12
grep -qi "DEVICE_LOST\|lost device" "$RUN"/*.log && bad "a lost device somewhere"
echo "run dir: $RUN"
[ "$fail" -eq 0 ] && say "sv-firefox-stock: PASS" || say "sv-firefox-stock: FAIL"
exit "$fail"
