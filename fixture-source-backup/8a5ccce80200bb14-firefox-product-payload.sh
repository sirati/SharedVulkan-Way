#!/usr/bin/env bash
# uid1000 only. Requires a real ready pool/bell and CUSE relay, supplied by fixture owner.
set -euo pipefail
exec 9>&-
[ "$(id -u)" = 1000 ] || { echo 'HOLD: Firefox must run as guest uid1000'; exit 3; }
: "${SV_SOCKET:?real guest GPU service required}" "${SV_BELL:?real guest bell required}" "${SV_DRM_NODE:?CUSE node required}"
[ -c "$SV_DRM_NODE" ] || { echo 'HOLD: CUSE node absent'; exit 3; }
if env | grep -E '^MOZ_(DISABLE_.*SANDBOX|FORCE_DISABLE_E10S_SANDBOX|CONTENT_SANDBOX_LEVEL|RDD_SANDBOX_LEVEL|GPU_SANDBOX_LEVEL)='; then exit 2; fi
unset LIBGL_ALWAYS_SOFTWARE SV_DEVICE_TYPE SVFS_DEVICE_TYPE
INPUT=/tmp/shared
BASE=${RDD_FIXTURE_BASE:-/tmp/product-rdd-fixture}
: "${WC_BROKER:?actual guest registry required}" "${PRODUCT_WAYLAND_REGION:?broker:ID required}" "${PRODUCT_WAYLAND_RUN:?original host header run required}" "${PRODUCT_WAYLAND_BELL:?real wake-only bell endpoint required}"
case "$PRODUCT_WAYLAND_REGION" in broker:*) ;; *) echo 'HOLD: original broker-leased BAR required'; exit 3;; esac
# The publisher/header receipt is decimal; wc-proxy --arena-run parses hex.
[[ "$PRODUCT_WAYLAND_RUN" =~ ^(0|[1-9][0-9]*)$ ]] || { echo 'HOLD: canonical decimal header run required'; exit 3; }
PRODUCT_WAYLAND_PROXY_RUN=$(printf '%016x' "$PRODUCT_WAYLAND_RUN") || { echo 'HOLD: header run exceeds u64'; exit 3; }
mkdir -p "$BASE"
cp "$INPUT/clip-H264.mp4" "$BASE/clip-H264.mp4"
RUN=$(mktemp -d "$BASE/firefox-XXXXXX")
printf '%s\n' "$RUN" > "$BASE/latest-firefox-run.txt"
mkdir -p "$RUN"/{home,rt,profile,config,data,state,cache}
chmod 700 "$RUN/rt"
export HOME="$RUN/home" XDG_RUNTIME_DIR="$RUN/rt" XDG_CONFIG_HOME="$RUN/config" XDG_DATA_HOME="$RUN/data" XDG_STATE_HOME="$RUN/state" XDG_CACHE_HOME="$RUN/cache"
FF=/nix/store/a6d0xgdw14rjpw84frvyxcqhyf8qmbwy-firefox-157.0.1/bin/firefox
MESA=/nix/store/c5vd2s8ffwsv0vxyn2mgmlpm3vhnfc5l-mesa-zink-va-26.3.0-devel-42c325ea
DRIVER=/nix/store/v01q3bm5nar7ygkrym0zhm79cii8gj69-sv-icd-driver-1c3869c3
GD=/nix/store/wmgsg5l24dvnhv6qkh04gb5lmd0r7v9p-geckodriver-0.37.1/bin/geckodriver
JQ=/nix/store/m3y7hqmy20nd1ps6qy8410a326255bf0-jq-1.8.2-bin/bin/jq
# BEGIN fixture WebDriver client
WD_SEQUENCE=0
wd_driver_start() {
 local raw
 [ -r "/proc/${GD_PID:?}/stat" ] || return 1
 raw=$(cat "/proc/$GD_PID/stat") || return 1
 local fields=()
 read -ra fields <<< "${raw##*) }"
 printf '%s\n' "${fields[19]}"
}
wd_owned_driver() {
 [ -n "${GD_PID:-}" ] && [ -n "${GD_START:-}" ] || return 1
 [ "$(readlink -f "/proc/$GD_PID/exe" 2>/dev/null)" = "$(readlink -f "$GD")" ] || return 1
 [ "$(wd_driver_start)" = "$GD_START" ]
}
wd_request() {
 local method=$1 path=$2 body=$3 label=$4 seconds=$5 request_dir curl_rc http_code
 local curl_args=()
 wd_owned_driver || { echo 'FAIL: owned geckodriver identity unavailable' >&2; return 1; }
 mkdir -p "$RUN/webdriver" || return 1
 request_dir="$RUN/webdriver/$label"
 mkdir "$request_dir" || { echo "FAIL: request evidence already exists: $label" >&2; return 1; }
 printf '%s\n' "$method" > "$request_dir/method.txt" || return 1
 printf '%s\n' "$path" > "$request_dir/path.txt" || return 1
 printf '%s\n' "$body" > "$request_dir/request.json" || return 1
 printf '%s %s\n' "$GD_PID" "$GD_START" > "$request_dir/owned-driver.txt" || return 1
 curl_args=(-sS --connect-timeout 2 --max-time "$seconds" -X "$method" -H 'Content-Type: application/json'
   --dump-header "$request_dir/headers.txt" --output "$request_dir/response.json"
   --write-out '%{http_code}\n' "http://127.0.0.1:${WD_PORT:-2829}$path")
 [ -z "$body" ] || curl_args+=(--data-binary "@$request_dir/request.json")
 if curl "${curl_args[@]}" > "$request_dir/http-status.txt" 2> "$request_dir/curl.stderr"; then curl_rc=0; else curl_rc=$?; fi
 printf '%s\n' "$curl_rc" > "$request_dir/curl-exit.txt" || return 1
 [ ! -f "$request_dir/response.json" ] || cat "$request_dir/response.json"
 [ "$curl_rc" = 0 ] || return 1
 http_code=$(cat "$request_dir/http-status.txt")
 [[ "$http_code" =~ ^2[0-9][0-9]$ ]] || return 1
 wd_owned_driver || return 1
 "$JQ" -e 'type == "object" and has("value") and (.value | type != "object" or (has("error") | not))' "$request_dir/response.json" >/dev/null
}
wd_wait_ready() {
 local end=$((SECONDS + 10)) attempt=0
 while [ "$SECONDS" -lt "$end" ]; do
  attempt=$((attempt + 1))
  if wd_request GET /status '' "ready-$attempt" 2 > "$RUN/webdriver-ready-$attempt.json"; then
   if "$JQ" -e '.value.ready == true' "$RUN/webdriver-ready-$attempt.json" >/dev/null; then return 0; fi
  fi
  wd_owned_driver || return 1
  sleep .2
 done
 echo 'FAIL: geckodriver readiness deadline' >&2
 return 1
}
wd_create_owned_session() {
 # The evidence directory is a permanent one-request latch, including timeout/error.
 local request_dir="$RUN/webdriver/session-create" candidate
 mkdir -p "$RUN/webdriver" || return 1
 [ ! -e "$request_dir" ] || { echo 'FAIL: session creation already attempted; no retry' >&2; return 1; }
 if wd_request POST /session '{"capabilities":{}}' session-create 75 > "$RUN/session-first-response.json"; then
  cp "$RUN/geckodriver.log" "$RUN/geckodriver-after-session-create.log" || return 1
  candidate=$("$JQ" -er '.value.sessionId | select(type == "string" and test("^[A-Za-z0-9_-]{1,128}$"))' "$request_dir/response.json") || return 1
  # Retain ownership before any later command or diagnostic can fail.
  sid=$candidate
  (set -C; printf '%s\n' "$sid" > "$RUN/owned-session-id.txt") || return 1
  "$JQ" -n --arg sessionId "$sid" --arg pid "$GD_PID" --arg start "$GD_START" \
   --arg responseSha256 "$(sha256sum "$request_dir/response.json" | cut -d' ' -f1)" \
   '{sessionId:$sessionId, driverPid:$pid, driverStart:$start, firstResponseSha256:$responseSha256, source:"single owned geckodriver POST /session response"}' > "$RUN/session-owner.json" || return 1
  (set -C; cat "$request_dir/response.json" > "$RUN/session.json")
 else
  cp "$RUN/geckodriver.log" "$RUN/geckodriver-after-session-create.log" || return 1
  printf '%s\n' 'Session creation failed or timed out. It will not be retried; original response/status and driver log are retained.' > "$RUN/session-create-failed.txt"
  return 1
 fi
}
wd() {
 WD_SEQUENCE=$((WD_SEQUENCE + 1))
 wd_request POST "$1" "$2" "command-$WD_SEQUENCE" "${3:-15}"
}
# END fixture WebDriver client

export WAYLAND_DISPLAY=rdd-test
kids=()
descendants() {
 local child
 for child in $(pgrep -P "$1" 2>/dev/null); do echo "$child"; descendants "$child"; done
}
capture_browser() {
 [ -n "${FFPID:-}" ] || return 0
 for pid in $(descendants "$FFPID"); do
  [ -r "/proc/$pid/status" ] || continue
  grep -E 'libgdk-3\.so|libwc_native_shm\.so' "/proc/$pid/maps" >> "$RUN/gtk-maps.txt" 2>/dev/null || true
  if grep -q '^Name:[[:space:]]*RDD Process$' "/proc/$pid/status" 2>/dev/null; then
   cat "/proc/$pid/status" > "$RUN/rdd-$pid-status.txt" 2>/dev/null || true
   cat "/proc/$pid/maps" > "$RUN/rdd-$pid-maps.txt" 2>/dev/null || true
  fi
 done
}
browser_live() {
 [ -r "/proc/$1/status" ] && ! grep -q '^State:[[:space:]]*Z' "/proc/$1/status"
}
graceful_browser_shutdown() {
 [ -n "${FFPID:-}" ] || return 0
 browser_owned=("$FFPID" $(descendants "$FFPID"))
 printf '%s\n' "${browser_owned[@]}" > "$RUN/shutdown-browser-pids.txt"
 if [ -n "${sid:-}" ]; then
  # Existing-browser DeleteSession only detaches (geckodriver marionette.rs).
  # Schedule nsIAppStartup's ordinary quit after returning a command receipt.
  wd "/session/$sid/moz/context" '{"context":"chrome"}' 5 > "$RUN/shutdown-context.json" 2> "$RUN/shutdown-context.err" || true
  wd "/session/$sid/execute/sync" '{"args":[],"script":"window.setTimeout(() => Services.startup.quit(Ci.nsIAppStartup.eAttemptQuit), 500); return {quitRequested:true, flag: \"eAttemptQuit\"};"}' 5 > "$RUN/shutdown-request.json" 2> "$RUN/shutdown-request.err" || true
 fi
 for _ in $(seq 1 200); do
  browser_remaining=()
  for pid in "${browser_owned[@]}"; do browser_live "$pid" && browser_remaining+=("$pid"); done
  [ "${#browser_remaining[@]}" -gt 0 ] || break
  sleep .1
 done
 printf '%s\n' "${browser_remaining[@]}" > "$RUN/shutdown-browser-remaining.txt"
 if [ "${#browser_remaining[@]}" -gt 0 ] || ! "$JQ" -e ' .value.quitRequested == true and .value.flag == "eAttemptQuit"' "$RUN/shutdown-request.json" >/dev/null 2>&1; then
  printf '%s\n' 'UNCERTAIN: browser graceful shutdown receipt or observed exit missing; forced CPU cleanup requires GPU provider and original backing retention' > "$RUN/gpu-completion-uncertain.txt"
  return 1
 fi
 printf '%s\n' 'Browser wrapper and captured descendants exited before forced cleanup. GPU provider retirement still requires its own completion receipt.' > "$RUN/shutdown-observed.txt"
}
cleanup() {
 original_rc=$?
 capture_browser
 graceful_browser_shutdown || { [ "$original_rc" != 0 ] || original_rc=1; }
 owned=()
 for pid in "${kids[@]}"; do owned+=($(descendants "$pid")); owned+=("$pid"); done
 printf '%s\n' "${owned[@]}" > "$RUN/cleanup-owned-pids.txt"
 for pid in "${owned[@]}"; do kill "$pid" 2>/dev/null || true; done
 for _ in $(seq 1 20); do
  live=()
  for pid in "${owned[@]}"; do kill -0 "$pid" 2>/dev/null && live+=("$pid"); done
  [ "${#live[@]}" -gt 0 ] || break
  sleep .1
 done
 for pid in "${live[@]:-}"; do [ -z "$pid" ] || kill -KILL "$pid" 2>/dev/null || true; done
 wait 2>/dev/null || true
 : > "$RUN/cleanup-survivors.txt"
 for pid in "${owned[@]}"; do
  [ -r "/proc/$pid/status" ] || continue
  grep -q '^State:[[:space:]]*Z' "/proc/$pid/status" && continue
  echo "$pid" >> "$RUN/cleanup-survivors.txt"
 done
 if [ -s "$RUN/cleanup-survivors.txt" ]; then
  echo 'FAIL: owned fixture children remain after bounded cleanup' >&2
  [ "$original_rc" != 0 ] || original_rc=1
 fi
 trap - EXIT
 exit "$original_rc"
}
trap cleanup EXIT
GTK=/nix/store/73dikpdicjxl5jsmabrax8y7pip7da3v-gtk3-native-shm-3.24.52
export LD_LIBRARY_PATH="$GTK/lib:/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export WC_NATIVE_SHM_STRICT=1 WC_NATIVE_SHM_FIXTURE=0
export WAYLAND_DISPLAY=wl-product
export WC_FRAME_TRACE="$RUN/proxy-trace.txt"
WC_NATIVE_SHM=1 "$INPUT/bin/wc-proxy" --runtime-dir "$XDG_RUNTIME_DIR" --socket "$WAYLAND_DISPLAY" \
 --transport arena --arena-ivshmem "$PRODUCT_WAYLAND_REGION" --arena-run "$PRODUCT_WAYLAND_PROXY_RUN" \
 --arena-id 0 --wayland-rings --wayland-bell "$PRODUCT_WAYLAND_BELL" \
 --dmabuf-node "$SV_DRM_NODE" --no-clipboard --no-icons --accept-timeout 60 --timeout 260 > "$RUN/proxy.log" 2>&1 &
kids+=($!)
for _ in $(seq 1 150); do [ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || break; sleep .1; done
[ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] || { cat "$RUN/proxy.log"; exit 1; }
PROXYPID=${kids[0]}
readlink -f "/proc/$PROXYPID/exe" > "$RUN/proxy-live-exe.txt"
sha256sum "/proc/$PROXYPID/exe" > "$RUN/proxy-live-sha256.txt"
tr '\0' '\n' < "/proc/$PROXYPID/cmdline" > "$RUN/proxy-live-argv.txt"
cat "/proc/$PROXYPID/status" > "$RUN/proxy-live-status.txt"
cat "/proc/$PROXYPID/maps" > "$RUN/proxy-live-maps.txt"
cat > "$RUN/play.html" <<'HTML'
<!doctype html><video id=v autoplay muted loop width=640 src="../clip-H264.mp4"></video><pre id=s></pre><script>
setInterval(()=>{const q=v.getVideoPlaybackQuality();s.textContent=JSON.stringify({frames:q.totalVideoFrames,dropped:q.droppedVideoFrames,time:v.currentTime,error:v.error?.message||null});},500);
</script>
HTML
printf 'user_pref("marionette.port", 2828);\n' > "$RUN/profile/user.js"
DEV=$(stat -c '%t:%T' "$SV_DRM_NODE")
DEV="$((16#${DEV%:*})):$((16#${DEV#*:}))"
env MOZ_ENABLE_WAYLAND=1 GDK_BACKEND=wayland VK_DRIVER_FILES="$DRIVER/share/vulkan/icd.d/sv_icd.json" \
 MESA_LOADER_DRIVER_OVERRIDE=zink __EGL_VENDOR_LIBRARY_FILENAMES="$MESA/share/glvnd/egl_vendor.d/50_mesa.json" \
 LIBGL_DRIVERS_PATH="$MESA/lib/dri" LIBVA_DRIVER_NAME=zink LIBVA_DRIVERS_PATH="$MESA/lib/dri" GBM_BACKENDS_PATH="$MESA/lib/gbm" \
 VK_LOADER_DEBUG=error,warn,driver MOZ_SANDBOX_LOGGING=1 MOZ_GFX_DEBUG=1 MOZ_LOG='PlatformDecoderModule:5,FFmpegVideo:5,Dmabuf:4' MOZ_LOG_FILE="$RUN/moz.log" \
 "$INPUT/bin/sv-drm" ns --dev "$DEV" --minor 190 --pci 10de:2b85 -- \
 "$FF" --width 640 --height 480 --no-remote --marionette --remote-allow-system-access --profile "$RUN/profile" "file://$RUN/play.html" > "$RUN/firefox.log" 2>&1 &
FFPID=$!; kids+=($FFPID)
for _ in $(seq 1 40); do grep -q "Listening on port 2828" "$RUN/firefox.log" && break; sleep 1; done
for pid in $(descendants "$FFPID"); do
 [ -r "/proc/$pid/status" ] || continue
 readlink -f "/proc/$pid/exe" > "$RUN/browser-$pid-exe.txt" || true
 sha256sum "/proc/$pid/exe" > "$RUN/browser-$pid-sha256.txt" 2>/dev/null || true
 tr '\0' '\n' < "/proc/$pid/cmdline" > "$RUN/browser-$pid-argv.txt" 2>/dev/null || true
 cat "/proc/$pid/status" > "$RUN/browser-$pid-status.txt" 2>/dev/null || true
 cat "/proc/$pid/maps" > "$RUN/browser-$pid-maps.txt" 2>/dev/null || true
done
grep -q "Listening on port 2828" "$RUN/firefox.log" || { echo 'FAIL: actual Marionette startup deadline'; exit 1; }
"$GD" --log trace --connect-existing --marionette-port 2828 --port 2829 > "$RUN/geckodriver.log" 2>&1 &
GD_PID=$!; kids+=("$GD_PID")
GD_START=$(wd_driver_start)
wd_wait_ready || { echo "FAIL: owned WebDriver service not ready; evidence $RUN"; exit 1; }
wd_create_owned_session || { echo "FAIL: single WebDriver session creation; evidence $RUN"; exit 1; }
sleep 15
wd "/session/$sid/execute/sync" '{"args":[],"script":"return document.getElementById(\"s\").textContent"}' > "$RUN/video.json" || true
wd "/session/$sid/moz/context" '{"context":"chrome"}' > "$RUN/context.json" || true
    wd "/session/$sid/execute/async" '{"args":[],"script":"const done = arguments[arguments.length-1]; const {Troubleshoot} = ChromeUtils.importESModule(\"resource://gre/modules/Troubleshoot.sys.mjs\"); Troubleshoot.snapshot().then(s => done(JSON.stringify({prefOverrides:[\"gfx.webrender.all\",\"media.hardware-video-decoding.enabled\",\"media.hardware-video-decoding.force-enabled\",\"media.hardware-video-decoding-vulkan.enabled\",\"media.ffmpeg.vaapi.enabled\",\"security.sandbox.content.level\",\"security.sandbox.rdd.level\",\"media.rdd-process.enabled\"].filter(k=>Services.prefs.prefHasUserValue(k)), defaultContentSandboxLevel: Services.prefs.getDefaultBranch(\"\").getIntPref(\"security.sandbox.content.level\"), media: {codecSupportInfo: s.media.codecSupportInfo}, sandbox: s.sandbox, graphics: {featureLog: s.graphics.featureLog, adapterDescription: s.graphics.adapterDescription, driverVendor: s.graphics.driverVendor, windowLayerManagerType: s.graphics.windowLayerManagerType, webgl2Renderer: s.graphics.webgl2Renderer, failures: s.graphics.failures}})), e => done(String(e)));"}' \
      20 >"$RUN/support.json"
capture_browser
printf '%s\n' "$RUN" > "$BASE/latest-firefox-run.txt"
echo "Evidence $RUN"
cat "$RUN/video.json" "$RUN/support.json"
# This GPU payload is successful only when real GPU selection and cold VA initialization are observed.
# The software discovery payload deliberately has a narrower verdict.
"$JQ" -e '.value | fromjson | (.prefOverrides | length) == 0 and .sandbox.effectiveContentSandboxLevel == 6' "$RUN/support.json" >/dev/null
rdd_ok=0
for status in "$RUN"/rdd-*-status.txt; do
 [ -f "$status" ] || continue
 if grep -q '^Name:[[:space:]]*RDD Process$' "$status" && grep -q '^Uid:[[:space:]]*1000[[:space:]]' "$status" \
   && grep -q '^NoNewPrivs:[[:space:]]*1$' "$status" && grep -q '^Seccomp:[[:space:]]*2$' "$status"; then rdd_ok=1; fi
done
[ "$rdd_ok" = 1 ] || { echo 'FAIL: actual browser RDD sandbox not observed'; exit 1; }
"$JQ" -e '.value | fromjson | .frames > 0 and .error == null' "$RUN/video.json" >/dev/null || { echo 'FAIL: video page did not play cleanly'; exit 1; }
grep -ahc 'VA-API frame pts=' "$RUN"/moz.log* > "$RUN/va-export-frame-counts.txt" || true
"$JQ" -e '.value | fromjson | .graphics.windowLayerManagerType == "WebRender" and (.graphics.adapterDescription | contains("NVIDIA GeForce RTX 5090"))' "$RUN/support.json" >/dev/null || { echo 'FAIL: default GPU WebRender not observed'; exit 1; }
grep -aq 'VA-API FFmpeg init successful' "$RUN"/moz.log* || { echo 'FAIL: stock RDD cold VA initialization not observed'; exit 1; }
grep -Fq "$GTK/lib/libgdk-3.so" "$RUN/gtk-maps.txt" || { echo 'FAIL: matched original-allocation GTK not loaded'; exit 1; }
if grep -q 'px.commit .*copied=[1-9][0-9]*' "$RUN/proxy-trace.txt"; then echo 'FAIL: a product surface commit was copied'; exit 1; fi
grep -q 'px.commit .*zero_copy=1' "$RUN/proxy-trace.txt" || { echo 'FAIL: no product zero-copy commits'; exit 1; }
echo 'OBSERVED: stock default GPU WebRender and cold VA initialization; final exit also requires graceful browser shutdown. Physical presentation and decoded original frames need separate evidence' 
