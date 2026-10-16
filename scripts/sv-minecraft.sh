#!/usr/bin/env bash
#
# **Minecraft through the forwarder**, on the deployed topology with the VM taken
# out (docs/minecraft.md, waycompose-design.md §96).
#
#   scripts/sv-minecraft.sh setup                      Prism, the game, Fabric, VulkanMod
#   scripts/sv-minecraft.sh run stock|vulkanmod [secs] play it, and judge what happened
#   scripts/sv-minecraft.sh ship SSH-HOST              Phase 2: the game, ready to launch, on a VM host
#
# Run from inside `nix develop` (Xvfb, Wine, ImageMagick and the dev shell's
# RADV come from there). Prism, Java, xdotool, GLFW and Python come from
# nixpkgs and are fetched by the script.
#
# THE TOPOLOGY
#
#   wc-compositor.exe   under Wine on an Xvfb, as scripts/sv-firefox-wayland.sh
#                       runs it: the Windows compositor, with real window
#                       messages, real `ClipCursor` and real raw input
#   wc-proxy            the game's Wayland server: `wl_shm` and nothing that
#                       carries a descriptor, every buffer into an arena
#   sv-gpu-server       the GPU side, on this machine's AMD iGPU (RADV) and
#                       nothing else: the loader is only ever shown radeon_icd
#   the game            Minecraft 1.21.11 in **demo mode**, a native Wayland
#                       GLFW client. `stock` renders OpenGL through Zink on our
#                       ICD; `vulkanmod` renders Vulkan on our ICD directly
#
# Input comes from `xdotool` on the Xvfb, so it enters as X events, becomes
# Win32 messages inside Wine, and reaches the game as Wayland input from our
# compositor: the same path as a person's mouse on the guest, minus the VM.
#
# NO ACCOUNT. Prism's own demo launch is used: no Microsoft account is created,
# none is logged into, and the game is started with `--demo`, which is Mojang's
# demo mode and what Prism passes when no account owns the game. `setup` drives
# Prism's four dialogs for that (setup wizard: Finish without logging in; "No
# Accounts": No; "Play demo?": Play Demo; the player name: OK) and records the
# launch Prism makes; `run` replays that launch, so a measurement does not
# depend on clicking through dialogs.
#
# ENVIRONMENT
#
#   SVMC_DATA      where Prism's data dir and the captures live  (/tmp/svmc)
#   SVMC_VALIDATE  1: VK_LAYER_KHRONOS_validation on the GPU side, VUIDs counted
#   SVMC_VSYNC     1 to leave the game's vsync on (default off, to measure FPS)
#   SVMC_FULLSCREEN 1: press F11 during the run and judge the window
#   SVMC_SOUND     1: give the game this desktop's PulseAudio, at master volume 0
#   SVMC_MEM       the game's memory cap (6G); the heap is Prism's 3 GiB
set -uo pipefail

CMD=${1:-}
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DATA=${SVMC_DATA:-/tmp/svmc}
PRISM_DIR=$DATA/prism
CAP=$DATA/capture
MC_VERSION=1.21.11
FABRIC_LOADER=0.19.5
# The latest VulkanMod release for 1.21.11 at the time of writing, with the
# sha1 Modrinth publishes for it.
VULKANMOD_URL='https://cdn.modrinth.com/data/JYQhtZtO/versions/hWKRTXbQ/VulkanMod-0.6.8%2B1.21.11.jar'
VULKANMOD_JAR='VulkanMod-0.6.8+1.21.11.jar'
VULKANMOD_SHA1=f273dc13efe240532a36ee55fc2e3d88b642a76d

say() { echo "[$(date +%H:%M:%S)] $*"; }
die() { echo "FAIL: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "$1 is missing; run this inside 'nix develop'"; }

# nixpkgs, from the registry, so the Prism here is the one deployed to the host.
nixpath() { nix build --no-link --print-out-paths "nixpkgs#$1" 2>/dev/null | head -1; }
PRISM=$(nixpath prismlauncher) || true
JDK=$(nixpath jdk21) || true
XDOTOOL=$(nixpath xdotool) || true
GLFW=$(nixpath glfw3-minecraft) || true
[ -x "$PRISM/bin/prismlauncher" ] && [ -x "$JDK/bin/java" ] && [ -x "$XDOTOOL/bin/xdotool" ] \
  && [ -e "$GLFW/lib/libglfw.so.3" ] || die "could not fetch prismlauncher, jdk21, xdotool and glfw3-minecraft"
export PATH="$XDOTOOL/bin:$PATH"

KIDS=()
cleanup() {
  kill "${KIDS[@]}" 2>/dev/null
  [ -n "${WINEPREFIX:-}" ] && WINEPREFIX="$WINEPREFIX" wineserver -k 2>/dev/null
}
trap cleanup EXIT INT TERM

# A display of our own. A display number is a shared identity on this machine
# (firefox-accel.md, "The Wayland WSI", §6), and **`-displayfd` is not enough
# here**: it takes the lowest free number, which on this workstation is `:0` —
# the number the login greeter's X server had, and the one any stray client
# that assumes "the" display talks to. So the number is chosen from a range
# nobody's default is in, and Xvfb's own lock decides a race.
start_xvfb() {
  need Xvfb
  local n
  for n in $(shuf -i 150-199); do
    [ -e "/tmp/.X$n-lock" ] || [ -e "/tmp/.X11-unix/X$n" ] && continue
    Xvfb ":$n" -screen 0 1600x1000x24 -nolisten tcp >"$1/xvfb.log" 2>&1 &
    local xp=$!
    for _ in $(seq 1 50); do [ -S "/tmp/.X11-unix/X$n" ] && break; kill -0 "$xp" 2>/dev/null || break; sleep 0.1; done
    if [ -S "/tmp/.X11-unix/X$n" ] && kill -0 "$xp" 2>/dev/null; then
      KIDS+=($xp); XN=$n; echo ":$n" >"$1/display"; return 0
    fi
  done
  die "Xvfb did not start on any of :150-:199"
}
shot() { DISPLAY=":$XN" xwd -root -silent | magick xwd:- "$1" 2>/dev/null; }

# ------------------------------------------------------------------- setup ---
# Prism's data dir, two instances, and one demo launch of each through Prism
# itself — which is what downloads the game, Fabric and their libraries — with
# Prism's wrapper command recording the launch it made.
setup() {
  mkdir -p "$PRISM_DIR/instances" "$CAP"
  cat >"$PRISM_DIR/prismlauncher.cfg" <<EOF
[General]
ConfigVersion=1.2
Language=en_US
ApplicationTheme=system
IconTheme=pe_colored
JavaPath=$JDK/bin/java
LastHostname=$(hostname)
IgnoreJavaWizard=true
AutomaticJavaDownload=false
AutomaticJavaSwitch=false
UserAskedAboutAutomaticJavaDownload=true
MinMemAlloc=1024
MaxMemAlloc=3072
LastOfflinePlayerName=DemoPlayer
EOF
  local i
  for i in stock vulkanmod; do
    mkdir -p "$PRISM_DIR/instances/mc-$i/minecraft"
    # `UseNativeGLFW`: Prism's "use system GLFW", which on nixpkgs is
    # glfw3-minecraft. LWJGL's own GLFW crashes in `glfwInit` on a Wayland
    # compositor with no `wl_seat` (a pixman weston) and draws libdecor frames;
    # this one is what nixpkgs ships for Minecraft on Wayland (docs/minecraft.md).
    cat >"$PRISM_DIR/instances/mc-$i/instance.cfg" <<EOF
[General]
ConfigVersion=1.3
InstanceType=OneSix
name=mc-$i
iconKey=default
OverrideNativeWorkarounds=true
UseNativeGLFW=true
OverrideCommands=true
WrapperCommand=$CAP/wrap-$i.sh
EOF
    # The wrapper: argv, environment, working directory and the launch script
    # Prism writes on stdin, then the launch itself, unchanged.
    cat >"$CAP/wrap-$i.sh" <<EOF
#!/bin/sh
D=$CAP/$i
mkdir -p "\$D"
printf '%s\n' "\$@" >"\$D/argv"
env >"\$D/env"
pwd >"\$D/cwd"
tee "\$D/stdin" | "\$@"
EOF
    chmod +x "$CAP/wrap-$i.sh"
  done
  printf '{"components":[{"important":true,"uid":"net.minecraft","version":"%s"}],"formatVersion":1}\n' \
    "$MC_VERSION" >"$PRISM_DIR/instances/mc-stock/mmc-pack.json"
  printf '{"components":[{"important":true,"uid":"net.minecraft","version":"%s"},{"uid":"net.fabricmc.intermediary","version":"%s","dependencyOnly":true},{"uid":"net.fabricmc.fabric-loader","version":"%s"}],"formatVersion":1}\n' \
    "$MC_VERSION" "$MC_VERSION" "$FABRIC_LOADER" >"$PRISM_DIR/instances/mc-vulkanmod/mmc-pack.json"
  local mods="$PRISM_DIR/instances/mc-vulkanmod/minecraft/mods"
  mkdir -p "$mods"
  if ! echo "$VULKANMOD_SHA1  $mods/$VULKANMOD_JAR" | sha1sum -c --status 2>/dev/null; then
    curl -fsSL -o "$mods/$VULKANMOD_JAR" "$VULKANMOD_URL" || die "could not download VulkanMod from Modrinth"
    echo "$VULKANMOD_SHA1  $mods/$VULKANMOD_JAR" | sha1sum -c --status || die "VulkanMod's sha1 is not Modrinth's"
  fi

  local run; run=$(mktemp -d "$DATA/setup-XXXX")
  # **No browser, and no session bus, for Prism.** Prism's login is OAuth in a
  # browser, opened with `xdg-open` (or, over D-Bus, the desktop's portal);
  # nothing here may ever reach it. The stub refuses and says so in the log,
  # and a watchdog below kills Prism outright if it so much as starts a
  # Microsoft login. This happened once while the dialogs were being
  # automated — the device-code flow started on its own, polled
  # login.microsoftonline.com and tried to open a browser — and nothing was
  # entered anywhere, but "nothing was entered" is not a property to rely on.
  mkdir -p "$run/nobrowser"
  printf '#!/bin/sh\necho "refused: $0 $*" >>"%s/nobrowser/log"\nexit 1\n' "$run" >"$run/nobrowser/xdg-open"
  chmod +x "$run/nobrowser/xdg-open"
  for b in firefox chromium google-chrome sensible-browser www-browser; do
    ln -sf xdg-open "$run/nobrowser/$b"
  done
  say "Prism on an Xvfb of its own per instance; the game is launched once each, in software, to download it"
  for i in stock vulkanmod; do
    start_xvfb "$run"
    local xvfb=${KIDS[-1]}
    rm -rf "${CAP:?}/$i"
    # Prism's UI on X, the game on Xvfb in software (llvmpipe) for this one
    # launch: nothing here may reach a GPU, and the Vulkan loader is shown no
    # driver at all, so VulkanMod's first start fails cleanly after the
    # download rather than finding the workstation's NVIDIA card.
    systemd-run --user --scope --quiet -p MemoryMax="${SVMC_MEM:-6G}" \
      env DISPLAY=":$XN" QT_QPA_PLATFORM=xcb WAYLAND_DISPLAY= LIBGL_ALWAYS_SOFTWARE=1 \
      __GLX_VENDOR_LIBRARY_NAME=mesa VK_DRIVER_FILES=/nonexistent.json \
      PATH="$run/nobrowser:$PATH" BROWSER="$run/nobrowser/xdg-open" \
      DBUS_SESSION_BUS_ADDRESS=disabled: \
      "$PRISM/bin/prismlauncher" -d "$PRISM_DIR" --launch "mc-$i" >"$run/prism-$i.log" 2>&1 &
    local pp=$!
    KIDS+=($pp)
    ( while kill -0 "$pp" 2>/dev/null; do
        if grep -q "AuthFlow\|login.microsoftonline.com" "$run/prism-$i.log" 2>/dev/null; then
          echo "[watchdog] Prism started a Microsoft login; killing it" >>"$run/prism-$i.log"
          kill -9 "$pp"; exit 0
        fi
        sleep 0.2
      done ) &
    KIDS+=($!)
    DISPLAY=":$XN" demo_dialogs 120 || die "Prism's demo dialogs did not appear; see $run/prism-$i.log"
    grep -q "\[watchdog\]" "$run/prism-$i.log" && die "Prism started a Microsoft login and was killed; see $run/prism-$i.log"
    local t
    for t in $(seq 1 600); do [ -s "$CAP/$i/stdin" ] && grep -q '^launch$' "$CAP/$i/stdin" && break; sleep 1; done
    [ -s "$CAP/$i/stdin" ] || die "Prism never launched mc-$i (download failed?); see $run/prism-$i.log"
    grep -q -- '^param --demo$' "$CAP/$i/stdin" || die "Prism's launch of mc-$i is not a demo launch"
    say "mc-$i: Prism launched it in demo mode; launch recorded in $CAP/$i"
    sleep 20
    kill_game "mc-$i"
    kill "$pp" 2>/dev/null; wait "$pp" 2>/dev/null
    kill "$xvfb" 2>/dev/null; wait "$xvfb" 2>/dev/null
  done
  say "setup done: $PRISM_DIR"
}

# Click through Prism's demo launch (see the header). **By clicking the
# buttons, not by keys.** The setup wizard's focused widget is "Add Microsoft
# account", so any Space or Return that reaches it — including one left over in
# the X server from the previous dialog — starts a Microsoft login; that is how
# the device-code flow started in this script's first version. A click on
# "Finish" can only be "Finish". Offsets are from each dialog's bottom-right
# corner, measured on Prism 11.1.1's own layout.
demo_dialogs() {
  local end=$(( $(date +%s) + $1 ))
  click() {
    local w; w=$(xdotool search --onlyvisible --name "$1" 2>/dev/null | tail -1)
    [ -n "$w" ] || return 1
    local X Y WIDTH HEIGHT SCREEN WINDOW
    eval "$(xdotool getwindowgeometry --shell "$w")"
    xdotool mousemove $((X + WIDTH - $2)) $((Y + HEIGHT - $3)) click 1
    say "prism: '$1' <- click $4"; sleep 1.5
  }
  while [ "$(date +%s)" -lt "$end" ]; do
    click "Quick Setup" 52 27 Finish && continue
    click "No Accounts" 52 27 No && continue
    click "Play demo" 159 27 "Play Demo" && continue
    click "Player name" 142 32 OK && return 0
    sleep 0.5
  done
  return 1
}

# The game's JVM for one instance, and only ours: the instance directory is in
# its command line, and `pgrep -x java` matches a process name, never a shell.
kill_game() {
  local p
  for p in $(pgrep -x java 2>/dev/null); do
    tr '\0' ' ' <"/proc/$p/cmdline" 2>/dev/null | grep -q -- "$PRISM_DIR/instances/$1/" && kill "$p"
  done
}

# --------------------------------------------------------------------- run ---
run() {
  local inst=$1 secs=${2:-150}
  need wine; need magick; need xwd
  [ -s "$CAP/$inst/stdin" ] || die "no recorded launch for $inst; run '$0 setup' first"
  local BIN=$ROOT/target/release WINBIN=${SVMC_WINBIN:-$ROOT/target/x86_64-pc-windows-gnu/debug}
  [ -x "$BIN/sv-gpu-server" ] && [ -x "$BIN/wc-proxy" ] && [ -f "$BIN/sv_icd.json" ] \
    && [ -x "$WINBIN/wc-compositor.exe" ] \
    || die "build first: cargo build --release -p sv-icd -p sv-gpu-server -p wc-proxy; cargo build --target x86_64-pc-windows-gnu -p wc-compositor"
  local RUN; RUN=$(mktemp -d "$DATA/run-$inst-XXXX")
  local GAMEDIR; GAMEDIR=$(cat "$CAP/$inst/cwd")
  mkdir -p "$RUN/xdg" "$RUN/keymaps" "$RUN/trace"; chmod 700 "$RUN/xdg"
  say "run $inst for $secs s in $RUN"

  # The game's options: no accessibility onboarding, no pause when the window
  # loses focus (the harness is not a person looking at it), a modest view
  # distance, and vsync off so the frame rate is the pipeline's.
  local O="$GAMEDIR/options.txt"
  touch "$O"
  setopt() { grep -q "^$1:" "$O" && sed -i "s/^$1:.*/$1:$2/" "$O" || echo "$1:$2" >>"$O"; }
  setopt onboardAccessibility false
  setopt pauseOnLostFocus false
  setopt tutorialStep none
  setopt renderDistance 8
  setopt fullscreen false
  setopt maxFps 260
  setopt enableVsync "$([ "${SVMC_VSYNC:-0}" = 1 ] && echo true || echo false)"
  setopt soundCategory_master 0.0
  # The narrator off, and its hotkey: Mojang's Linux narrator speaks through
  # `pa_simple` and aborts the JVM (`Assertion 'p' failed … pa_simple_write`)
  # when there is no PulseAudio to speak to, which a session without audio
  # is. Found when a stray keystroke had turned it on.
  setopt narrator 0
  setopt narratorHotkey false

  # ---- the compositor, under Wine, and the proxy in front of it
  start_xvfb "$RUN"
  export WINEPREFIX=${SVMC_WINEPREFIX:-$DATA/wineprefix} WINEDEBUG=-all
  mkdir -p "$WINEPREFIX"
  local WCARENA=$((256 << 20)) WSOCK="svmc-$$"
  truncate -s "$WCARENA" "$RUN/wc-arena"
  DISPLAY=":$XN" SV_ALLOW_FILE_ARENA=1 WC_FRAME_TRACE="$(winepath -w "$RUN/trace")\\%p.txt" \
    wine explorer "/desktop=svmc$$,1600x1000" "$WINBIN/wc-compositor.exe" \
    --socket "$WSOCK" --timeout $((secs + 300)) \
    --arena "0:$WCARENA:$(winepath -w "$RUN/wc-arena")" \
    --shared-dir "$(winepath -w "$RUN/keymaps")" --shared-peer-prefix "$RUN/keymaps/" \
    --log "$(winepath -w "$RUN/compositor.log")" >"$RUN/wine.log" 2>&1 &
  KIDS+=($!)
  local NODE=""
  for _ in $(seq 1 300); do
    NODE=$(find "$WINEPREFIX/drive_c" -type s -name "$WSOCK" 2>/dev/null | head -1)
    [ -n "$NODE" ] && break; sleep 0.2
  done
  [ -n "$NODE" ] || die "wc-compositor.exe never listened; see $RUN/wine.log"
  SV_ALLOW_FILE_ARENA=1 WC_FRAME_TRACE="$RUN/trace/%p.txt" "$BIN/wc-proxy" --upstream "$NODE" \
    --socket svmc --runtime-dir "$RUN/xdg" --copy damage \
    --transport arena --arena-path "$RUN/wc-arena" --arena-id 0 --arena-size "$WCARENA" \
    --accept-timeout 120 --timeout $((secs + 300)) >"$RUN/proxy.log" 2>&1 &
  KIDS+=($!)

  # ---- the GPU side: the dev shell's RADV (the system one needs a newer glibc
  # than sv-gpu-server is built with), shown nothing else
  local GPU_ICD=${SVMC_GPU_ICD:-${SV_MESA:-/run/opengl-driver}/share/vulkan/icd.d/radeon_icd.x86_64.json}
  local -a GPU_ENV=(VK_DRIVER_FILES="$GPU_ICD")
  if [ "${SVMC_VALIDATE:-0}" = 1 ]; then
    local VVL; VVL=$(nixpath vulkan-validation-layers)
    GPU_ENV+=(VK_LAYER_PATH="$VVL/share/vulkan/explicit_layer.d" VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation)
  fi
  ( env "${GPU_ENV[@]}" SV_TRACE=1 "$BIN/sv-gpu-server" serve --clients 16 --arena $((512 << 20)) \
      --socket "$RUN/gpu.sock" --idle $((secs + 120)) 2>&1 &
    echo $! >"$RUN/gpu.pid"; wait ) \
    | gawk -v out="$RUN/gpu-trace.txt" -v keep="${SVMC_KEEP_PIPELINE_TRACE:-0}" -v keepf="$RUN/gpu-pipelines.txt" '
        BEGIN { t0 = systime(); last = t0 }
        /^\[trace\]/ && keep && /Pipeline|VK_ERROR|result/ { print >> keepf }
        /^\[trace\]/ { n[$2]++
          if (systime() - last >= 10) {
            printf "t=%ds submit=%d present=%d draw=%d\n", systime() - t0,
              n["vkQueueSubmit"] + n["vkQueueSubmit2"], n["vkQueuePresentKHR"],
              n["vkCmdDraw"] + n["vkCmdDrawIndexed"] + n["vkCmdDrawIndirect"] + n["vkCmdDrawIndexedIndirect"] >> out
            fflush(out); delete n; last = systime() }
          next }
        /^\[blob\]/ { next }
        { print; fflush() }' >"$RUN/gpu.log" &
  KIDS+=($!)
  for _ in $(seq 1 80); do [ -S "$RUN/gpu.sock" ] && [ -S "$RUN/xdg/svmc" ] && break; sleep 0.25; done
  [ -S "$RUN/gpu.sock" ] || die "the GPU side never listened; see $RUN/gpu.log"
  [ -S "$RUN/xdg/svmc" ] || die "wc-proxy never listened; see $RUN/proxy.log"
  KIDS+=($(cat "$RUN/gpu.pid"))

  # ---- the game, as Prism launched it, with the session's environment
  local KM; KM=$(nix build --no-link --print-out-paths --impure --expr \
    "(builtins.getFlake \"nixpkgs\").legacyPackages.x86_64-linux.callPackage $ROOT/nix/mesa-kopper-wayland.nix {}" \
    2>/dev/null | grep -v -- '-[a-z_]*$' | head -1)
  [ -n "$KM" ] || die "could not build nix/mesa-kopper-wayland.nix"
  local LDP; LDP=$(sed -n 's/^LD_LIBRARY_PATH=//p' "$CAP/$inst/env")
  local -a GAME_ENV=(
    XDG_RUNTIME_DIR="$RUN/xdg" WAYLAND_DISPLAY=svmc
    # Our ICD, and only ours.
    VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$RUN/gpu.sock"
    # Prism's own library path (OpenAL, GLFW, the Vulkan loader, libglvnd).
    LD_LIBRARY_PATH="$LDP"
    # GL: Mesa's EGL from the patched Mesa, and Zink, through kopper with no
    # DRM device (docs/design.md §0.28) — what gpu.env sets for Firefox.
    __GLX_VENDOR_LIBRARY_NAME=mesa
    __EGL_VENDOR_LIBRARY_FILENAMES="$KM/share/glvnd/egl_vendor.d/50_mesa.json"
    MESA_LOADER_DRIVER_OVERRIDE=zink GALLIUM_DRIVER=zink LIBGL_KOPPER_DRI2=1
    WC_FRAME_TRACE="$RUN/trace/%p.txt"
  )
  if [ "${SVMC_SOUND:-0}" = 1 ] && [ -S "/run/user/$(id -u)/pulse/native" ]; then
    GAME_ENV+=(PULSE_SERVER="unix:/run/user/$(id -u)/pulse/native")
    GAME_ENV+=(ALSOFT_LOGLEVEL=3)
  fi
  # `SVMC_APP_VALIDATE=1`: the validation layer inside the game as well, over
  # our ICD. It sees what Zink or VulkanMod asked for, before the wire; the
  # GPU side's layer sees what the driver was handed. A VUID in both is the
  # application's; one only on the GPU side is ours.
  if [ "${SVMC_APP_VALIDATE:-0}" = 1 ]; then
    local AVVL; AVVL=$(nixpath vulkan-validation-layers)
    GAME_ENV+=(VK_LAYER_PATH="$AVVL/share/vulkan/explicit_layer.d" VK_INSTANCE_LAYERS=VK_LAYER_KHRONOS_validation)
  fi
  # `SVMC_GAME_ENV`: extra `NAME=value` words for the game alone (`SV_DEBUG=1`,
  # `ZINK_DEBUG=…`). Word-split, never evaluated.
  local -a EXTRA_ENV; read -r -a EXTRA_ENV <<<"${SVMC_GAME_ENV:-}"
  GAME_ENV+=("${EXTRA_ENV[@]}")
  local ARGS=()
  [ -d "$GAMEDIR/saves/Demo_World" ] && ARGS=(--quickPlaySingleplayer Demo_World)
  # **No `/dev/dri` for the game.** Mesa's EGL probes every DRM device it
  # can open before it settles on the kopper path, and on this workstation the
  # first one is the NVIDIA card other people's jobs run on (`pci id for fd
  # 16: 10de:2b85`). It found no driver and let go, but the harness must not
  # open that device at all; an empty `/dev/dri` is also exactly what the
  # deployed host has, which has no GPU.
  need bwrap
  local -a ARGV; mapfile -t ARGV <"$CAP/$inst/argv"
  ( cd "$GAMEDIR" && { grep -v '^launch$' "$CAP/$inst/stdin"; for a in "${ARGS[@]}"; do echo "param $a"; done; echo launch; } \
      | systemd-run --user --scope --quiet -p MemoryMax="${SVMC_MEM:-6G}" \
          bwrap --dev-bind / / --tmpfs /dev/dri -- \
          env -u DISPLAY "${GAME_ENV[@]}" timeout "$secs" "${ARGV[@]}" ) >"$RUN/game.log" 2>&1 &
  local G=$!
  KIDS+=($G)

  # ---- a person, by xdotool
  play "$RUN" "$inst" "$GAMEDIR" "$secs" >"$RUN/play.log" 2>&1 &
  local P=$!
  KIDS+=($P)
  wait "$G"; local rc=$?
  kill "$P" 2>/dev/null
  verdict "$RUN" "$inst" "$GAMEDIR" "$rc"
}

# What a person does: wait for the window, get into the world, open F3, look
# around, walk, take Minecraft's own screenshot (F2), pause, and quit.
play() {
  local RUN=$1 inst=$2 GAMEDIR=$3 secs=$4
  export DISPLAY=":$XN"
  mark() { echo "$(date +%s.%N) $*" >>"$RUN/marks.txt"; echo "[play] $*"; }
  local t
  for t in $(seq 1 120); do grep -q "window 1 mapped" "$RUN/compositor.log" 2>/dev/null && break; sleep 1; done
  mark mapped
  if [ ! -d "$GAMEDIR/saves/Demo_World" ]; then
    # The title screen: "Play Demo World", in the window at the origin of the
    # Wine desktop (caption 30 px, border 4).
    sleep 20; shot "$RUN/title.png"; xdotool mousemove 429 265 click 1; mark "play demo world"
  fi
  # In the world: the server says so in the game's own log.
  for t in $(seq 1 300); do grep -q "joined the game" "$GAMEDIR/logs/latest.log" 2>/dev/null && break; sleep 1; done
  sleep 8
  shot "$RUN/p0-demo-screen.png"
  # "Continue Playing!" on the demo's welcome screen.
  xdotool mousemove 548 381 click 1; sleep 2; mark continue
  xdotool key F3; sleep 3; mark f3
  shot "$RUN/p1-f3.png"
  for t in $(seq 1 20); do xdotool mousemove_relative -- 15 0; sleep 0.05; done
  sleep 1; shot "$RUN/p2-looked-right.png"; mark "looked right 300 px"
  for t in $(seq 1 10); do xdotool mousemove_relative -- 0 -15; sleep 0.05; done
  sleep 1; shot "$RUN/p3-looked-up.png"; mark "looked up 150 px"
  xdotool keydown w; sleep 2; xdotool keyup w; sleep 1; mark "walked"
  xdotool key space; sleep 1
  xdotool key F2; sleep 2; mark "F2 screenshot"
  shot "$RUN/p4-walked.png"
  if [ "${SVMC_FULLSCREEN:-0}" = 1 ]; then
    xdotool key F11; sleep 4; shot "$RUN/p5-fullscreen.png"; mark fullscreen
    xdotool key F11; sleep 4; shot "$RUN/p6-windowed.png"; mark windowed
  fi
  # The frame rate is measured over this stretch: the game running, nobody
  # touching it.
  mark "steady begin"; sleep 30; mark "steady end"
  xdotool key Escape; sleep 2; shot "$RUN/p7-paused.png"; mark paused
  # Quit the way a person on Windows does: the window's close button, which
  # is `WM_CLOSE`, then `xdg_toplevel.close`, then Minecraft saving the world
  # and exiting by itself.
  # The button is found from where the compositor says the window is (Wine's
  # virtual desktop has no X window per Win32 window to ask): the last
  # "(window WxH at X,Y)" it logged, and the caption's close box 15 px in from
  # the right edge and 16 down, which is Wine's default theme.
  local geo; geo=$(grep -o "(window [0-9]*x[0-9]* at -\?[0-9]*,-\?[0-9]*)" "$RUN/compositor.log" | tail -1 | tr -dc "0-9x,\- \n" )
  local ww wy wx
  ww=$(echo "$geo" | sed -E 's/ *([0-9]+)x.*/\1/')
  wx=$(echo "$geo" | sed -E 's/.* (-?[0-9]+),.*/\1/')
  wy=$(echo "$geo" | sed -E 's/.*,(-?[0-9]+).*/\1/')
  xdotool mousemove $((wx + ww - 15)) $((wy + 16)) click 1
  mark "closed the window"
}

verdict() {
  local RUN=$1 inst=$2 GAMEDIR=$3 rc=$4
  local fail=0
  ok()  { echo "  ok   $*"; }
  bad() { echo "  FAIL $*"; fail=1; }
  local C="$RUN/compositor.log" L="$GAMEDIR/logs/latest.log"
  say "---- the verdict ($inst) ----"
  grep -q "sv-icd: connected to the GPU side" "$RUN/game.log" \
    && ok "the game loaded our ICD and reached the GPU side" \
    || bad "the game never connected to the GPU side through sv-icd"
  grep -q "serving rings" "$RUN/gpu.log" && ok "the GPU side served the session on $(sed -n 's/.*serving rings \[[0-9, ]*\] on //p' "$RUN/gpu.log" | head -1)" \
    || bad "the GPU side served nothing"
  # Presents are counted where they happen: our ICD implements
  # `vkQueuePresentKHR` itself (a GPU-side copy into the arena, docs/design.md
  # §0.6), so the GPU side never sees the call and only the ICD's own trace can
  # count it.
  local presents; presents=$(cat "$RUN"/trace/*.txt 2>/dev/null | grep -c " icd.present ")
  [ "${presents:-0}" -gt 100 ] && ok "$presents frames presented by the game through our ICD" \
    || bad "only ${presents:-0} presents through our ICD"
  grep -q "window 1 mapped" "$C" && ok "the window mapped on wc-compositor.exe: $(grep -m1 'window 1 mapped' "$C" | sed 's/.*mapped //')" \
    || bad "no window mapped"
  grep -q "follows its client to 854x480" "$C" && ok "the window follows the game's own geometry (854x480)"
  grep -q "holds the pointer: Locked" "$C" && ok "the game locked the pointer" || bad "the game never locked the pointer"
  local moved; moved=$(sed -n 's/.*released.*; [0-9]* raw-input reports, \([0-9]*\) moved the view/\1/p' "$C" | awk '{s+=$1} END {print s+0}')
  [ "${moved:-0}" -gt 0 ] && ok "$moved raw-input motions reached the game while locked" \
    || bad "no raw-input motion reached the game"
  grep -q "pointer hold released" "$C" && ok "every hold was released: $(grep -c 'pointer hold released' "$C") release(s), $(grep -c 'in force: ClipCursor' "$C") clip(s)" \
    || bad "a hold was never released"
  grep -q "65548: Wayland: The compositor does not support pointer locking" "$RUN/game.log" \
    && bad "GLFW still says the compositor cannot lock the pointer" \
    || ok "GLFW did not complain about pointer locking"
  if grep -q "Wayland\|GLFW" "$RUN/game.log" && grep -q "X11:" "$RUN/game.log"; then
    bad "GLFW used X11"
  fi
  local shots; shots=$(find "$GAMEDIR/screenshots" -newer "$RUN/gpu.pid" -name '*.png' 2>/dev/null | head -1)
  [ -n "$shots" ] && ok "Minecraft's own F2 screenshot: $shots" || bad "F2 produced no screenshot"
  if [ "$rc" -eq 0 ] && grep -q "Stopping!" "$L"; then
    ok "closing the window quit the game cleanly (exit 0, \"Stopping!\")"
  else
    bad "the game did not quit cleanly when its window was closed (exit $rc)"
  fi
  if grep -qE "SIGSEGV|hs_err_pid|Exception in thread \"Render thread\"" "$RUN/game.log"; then
    bad "the game crashed: $(grep -m1 -E 'SIGSEGV|Problematic frame|Exception in thread' "$RUN/game.log")"
  else
    ok "no crash in the game's output"
  fi
  if [ "${SVMC_VALIDATE:-0}" = 1 ]; then
    local vuids; vuids=$(grep -oE 'VUID-[A-Za-z0-9_-]+' "$RUN/gpu.log" | sort | uniq -c | sort -rn)
    [ -z "$vuids" ] && ok "the validation layer on the GPU side reported no VUID" \
      || { echo "  ---- VUIDs on the GPU side ----"; echo "$vuids" | head -20; bad "$(echo "$vuids" | wc -l) distinct VUID(s) on the GPU side"; }
  fi
  if [ "${SVMC_APP_VALIDATE:-0}" = 1 ]; then
    local avuids; avuids=$(grep -oE 'VUID-[A-Za-z0-9_-]+' "$RUN/game.log" | sort | uniq -c | sort -rn)
    [ -z "$avuids" ] && ok "the validation layer in the game reported no VUID" \
      || { echo "  ---- VUIDs in the game ----"; echo "$avuids" | head -20; bad "$(echo "$avuids" | wc -l) distinct VUID(s) in the game"; }
  fi
  if [ "${SVMC_SOUND:-0}" = 1 ]; then
    if grep -q 'Initialized backend "pulse"' "$RUN/game.log" && grep -q "OpenAL initialized on device" "$L"; then
      ok "sound: OpenAL on PulseAudio, device '$(grep -m1 'OpenAL initialized' "$L" | sed 's/.*device //')' (this workstation's default sink)"
    else
      bad "sound did not reach PulseAudio: $(grep -m1 -i 'sound\|openal' "$L")"
    fi
  fi
  if [ "${SVMC_FULLSCREEN:-0}" = 1 ]; then
    grep -q "fullscreen on its monitor" "$C" && grep -q "left fullscreen" "$C" \
      && ok "F11: $(grep -m1 'fullscreen on its monitor' "$C" | sed 's/wc-compositor: //'), and back" \
      || bad "F11 did not make the window fullscreen and back: $(grep -i fullscreen "$C" | tr '\n' ';')"
  fi
  # Frame pacing, from the ICD's own present timestamps in the trace.
  local PY; PY=$(nixpath python3)/bin/python3
  if [ -x "$PY" ]; then
    "$PY" "$ROOT/scripts/sv-minecraft-fps.py" "$RUN/trace" "$RUN/marks.txt" >"$RUN/fps.txt" 2>&1
    sed 's/^/  /' "$RUN/fps.txt"
  fi
  say "---- the GPU side, per 10 s ----"; sed 's/^/  /' "$RUN/gpu-trace.txt" | tail -8
  echo "run dir: $RUN"
  [ "$fail" -eq 0 ] && say "sv-minecraft $inst: PASS" || say "sv-minecraft $inst: FAIL"
  return "$fail"
}

# ------------------------------------------------------------ bundle, ship ---
# Phase 2 (waycompose-design.md §96): what the VM host gets. `setup`'s data
# directory and recorded launches, moved to SVMC_HOST_DIR (~/wc-mc),
# with Prism configured for that host, the options a person wants (the
# narrator off, the sound on, vsync on, a view distance for a laptop), the
# launcher `scripts/minecraft-demo`, and desktop entries and Minecraft's own
# icon in share/, where `wc-session`'s `app` and `icon` verbs look.
HOST_DIR=${SVMC_HOST_DIR:-~/wc-mc}
bundle() {
  [ -s "$CAP/stock/stdin" ] && [ -s "$CAP/vulkanmod/stdin" ] || die "run '$0 setup' first"
  local host=${1:?bundle HOSTNAME} B="$DATA/bundle/wc-mc"
  rm -rf "$DATA/bundle"; mkdir -p "$B/bin" "$B/capture" "$B/share/applications"
  rsync -a --exclude 'logs/' --exclude 'screenshots/' --exclude 'hs_err_pid*' --exclude 'core*' \
    --exclude 'crash-reports/' --exclude 'debug/' "$PRISM_DIR" "$B/"
  local v f kv k val
  for v in stock vulkanmod; do
    mkdir -p "$B/capture/$v"
    for f in argv stdin cwd; do sed "s|$PRISM_DIR|$HOST_DIR/prism|g" "$CAP/$v/$f" >"$B/capture/$v/$f"; done
    # Only the library path of Prism's wrapper: the rest of the recorded
    # environment was this workstation's.
    grep '^LD_LIBRARY_PATH=' "$CAP/$v/env" >"$B/capture/$v/env"
    sed -i -e '/^WrapperCommand=/d' -e 's/^OverrideCommands=true/OverrideCommands=false/' \
      "$B/prism/instances/mc-$v/instance.cfg"
    local O="$B/prism/instances/mc-$v/minecraft/options.txt"
    for kv in narrator:0 narratorHotkey:false onboardAccessibility:false renderDistance:6 \
              simulationDistance:6 enableVsync:true maxFps:120 soundCategory_master:1.0 \
              pauseOnLostFocus:true fullscreen:false tutorialStep:none; do
      k=${kv%%:*}; val=${kv#*:}
      grep -q "^$k:" "$O" && sed -i "s/^$k:.*/$k:$val/" "$O" || echo "$k:$val" >>"$O"
    done
  done
  sed -i -e "s/^LastHostname=.*/LastHostname=$host/" -e 's/^MaxMemAlloc=.*/MaxMemAlloc=1024/' \
    -e 's/^MinMemAlloc=.*/MinMemAlloc=256/' "$B/prism/prismlauncher.cfg"
  install -m 755 "$ROOT/scripts/minecraft-demo" "$B/bin/minecraft-demo"
  # Minecraft's own icon, from the game's assets (`icons/icon_*.png`).
  local idx="$PRISM_DIR/assets/indexes/29.json" sz h
  for sz in 256x256 128x128 48x48; do
    h=$(grep -oE "\"icons/icon_$sz.png\": \{\"hash\": \"[0-9a-f]+\"" "$idx" | grep -oE '[0-9a-f]{40}')
    [ -n "$h" ] || die "no icons/icon_$sz.png in $idx"
    mkdir -p "$B/share/icons/hicolor/$sz/apps"
    cp "$PRISM_DIR/assets/objects/${h:0:2}/$h" "$B/share/icons/hicolor/$sz/apps/wc-minecraft.png"
  done
  printf '[Desktop Entry]\nType=Application\nName=Minecraft (demo)\nComment=Minecraft %s in demo mode, OpenGL through Zink on the guest'"'"'s GPU\nExec=minecraft-demo stock\nIcon=wc-minecraft\nCategories=Game;\n' \
    "$MC_VERSION" >"$B/share/applications/wc-minecraft.desktop"
  printf '[Desktop Entry]\nType=Application\nName=Minecraft (demo, VulkanMod)\nComment=Minecraft %s in demo mode with VulkanMod, Vulkan on the guest'"'"'s GPU\nExec=minecraft-demo vulkanmod\nIcon=wc-minecraft\nCategories=Game;\n' \
    "$MC_VERSION" >"$B/share/applications/wc-minecraft-vulkanmod.desktop"
  grep -rl "$PRISM_DIR" "$B/capture" && die "a recorded launch still names $PRISM_DIR"
  say "bundle for $host in $B ($(du -sh "$B" | cut -f1))"
}

# Copy it there: the store paths the launch names (`nix copy`; this user is
# trusted on the VM host, so the locally built Prism wrapper needs no
# signature), then the bundle. Nothing is built on the host.
ship() {
  local target=${1:?ship SSH-HOST}
  local host; host=$(ssh -o BatchMode=yes "$target" hostname) || die "cannot reach $target"
  bundle "$host"
  nix copy --no-check-sigs --to "ssh-ng://$target" "$PRISM" "$JDK" "$GLFW" || die "nix copy to $target"
  rsync -a --delete "$DATA/bundle/wc-mc/" "$target:$HOST_DIR/" || die "rsync to $target"
  ssh "$target" "bash -n $HOST_DIR/bin/minecraft-demo && test -x $(head -1 "$CAP/stock/argv")" \
    || die "the launcher or its Java is not usable on $target"
  say "shipped to $target:$HOST_DIR"
}

case "$CMD" in
  setup) setup ;;
  run) shift; run "$@" ;;
  bundle) shift; bundle "${1:-$(hostname)}" ;;
  ship) shift; ship "$@" ;;
  *) sed -n '3,9p' "$0"; exit 2 ;;
esac
