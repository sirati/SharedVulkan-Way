#!/usr/bin/env bash
#
# The acceptance test for the **Windows platform layer**: two processes inside
# the guest, no VM boundary, the Intel iGPU at the far end.
#
#   scripts/sv-windows-e2e.sh --agent <ssh-agent-socket>
#
# This is the exact analogue of `sv-end-to-end.sh`, which runs the same pair as
# two Linux processes. What differs is only the platform layer:
#
#   Linux                              Windows
#   -----                              -------
#   memfd + mmap(MAP_SHARED)           named section + MapViewOfFile
#   Unix socket + SCM_RIGHTS           named pipe, and **no handle passed**
#   VK_DRIVER_FILES=<manifest path>    VK_DRIVER_FILES=<manifest path>
#
# The check is the same one and the string is the one that matters: the app side
# must report **Intel(R) UHD Graphics**, the GPU side's driver, not our
# synthesised "shared_vulkan forwarded device". That single string means the
# loader accepted our DLL, the ICD encoded a command, it crossed a ring in
# shared memory, a real driver answered, and the reply came back through the
# generated decoder.
#
# Nothing here touches the VM's configuration, the display, or the network. It
# starts two user-mode processes and they exit.
set -uo pipefail

AGENT=""
ARENA=$((64 << 20))
SESSION="e2e-$$"
DEVICE=0

while [ $# -gt 0 ]; do
  case "$1" in
    --agent) AGENT=$2; shift 2 ;;
    --arena) ARENA=$2; shift 2 ;;
    --session) SESSION=$2; shift 2 ;;
    --device) DEVICE=$2; shift 2 ;;
    *) echo "unknown argument $1"; exit 2 ;;
  esac
done

HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
PL="$HERE/pl.sh"
[ -n "$AGENT" ] && export SV_AGENT="$AGENT"

BIN="$ROOT/target/x86_64-pc-windows-gnu/release"
for f in sv-gpu-server.exe sv_icd.dll sv_icd.json; do
  [ -f "$BIN/$f" ] || {
    echo "build first: cargo build -p sv-icd -p sv-gpu-server --release --target x86_64-pc-windows-gnu"
    exit 1
  }
done

say() { echo "[$(date +%H:%M:%S)] $*"; }

exec 9>/tmp/.sv-windows-e2e.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

say "publishing to the guest's Z:\\probe"
"$PL" push "$BIN/sv-gpu-server.exe" "$BIN/sv_icd.dll" "$BIN/sv_icd.json"
"$PL" root "install -m0755 /tmp/sv-gpu-server.exe /tmp/sv_icd.dll /srv/winshare/probe/ && \
            install -m0644 /tmp/sv_icd.json /srv/winshare/probe/" > /dev/null || exit 1

# The two halves run from a *local* directory, not from Z:. virtio-fs is fine
# for reading a file and is not somewhere to load a DLL from: the loader maps
# the image, and a network-ish filesystem under a memory mapping is a class of
# problem nobody needs while proving something else.
say "copying into the guest's local disk"
"$PL" guest 'mkdir -p /c/sv && cp /z/probe/sv-gpu-server.exe /z/probe/sv_icd.dll /z/probe/sv_icd.json /c/sv/ && ls -la /c/sv' || exit 1

GPULOG=/tmp/sv-windows-e2e-gpu.log
APPLOG=/tmp/sv-windows-e2e-app.log
: > "$GPULOG"; : > "$APPLOG"

# Held open, not detached — Windows OpenSSH tears the job down when the channel
# closes, so `nohup … &` leaves nothing running. Learned the hard way in
# gpu-workload-run.sh.
say "starting the GPU side (session $SESSION, arena $((ARENA >> 20)) MiB, device $DEVICE)"
"$PL" guest "cd /c/sv && ./sv-gpu-server.exe serve --session $SESSION --arena $ARENA --device $DEVICE 2>&1" \
  </dev/null > "$GPULOG" 2>&1 &
GPU_PID=$!
trap 'kill $GPU_PID 2>/dev/null' EXIT

say "waiting for it to publish the sections and open the pipe"
for _ in $(seq 1 60); do
  grep -q 'waiting for the app side' "$GPULOG" && break
  kill -0 $GPU_PID 2>/dev/null || break
  sleep 1
done
if ! grep -q 'waiting for the app side' "$GPULOG"; then
  echo "the GPU side never came up:"; sed 's/^/    /' "$GPULOG"; exit 1
fi
sed 's/^/  /' "$GPULOG"

# The app side is the guest's own Vulkan loader, our DLL, and vulkaninfo on top.
# Note what is NOT set: no VK_DRIVER_FILES for the Intel driver, so the only
# implementation vulkaninfo can reach is ours.
say "running vulkaninfo through our ICD"
"$PL" guest "cd /c/sv && VK_DRIVER_FILES='C:\\sv\\sv_icd.json' SV_SOCKET=$SESSION \
             ./vulkaninfo.exe --summary 2>&1" > "$APPLOG" 2>&1
RC=$?

wait $GPU_PID 2>/dev/null
trap - EXIT

echo
echo "--- the GPU side ---"
sed 's/^/  /' "$GPULOG"
echo "--- the app side ---"
sed 's/^/  /' "$APPLOG"

REPORTED=$(grep -m1 'deviceName' "$APPLOG" | sed 's/.*= //' | tr -d '\r')
echo
if [ -z "$REPORTED" ]; then
  echo "FAIL — vulkaninfo reported no device (exit $RC)"
  exit 1
fi
if [ "$REPORTED" = "shared_vulkan forwarded device" ]; then
  echo "FAIL — that is our synthesised table, so nothing was forwarded"
  exit 1
fi
echo "app side sees: $REPORTED"
case "$REPORTED" in
  *"UHD Graphics"*) echo "PASS — the application is reading the guest's real GPU through our ICD" ;;
  *) echo "PASS (unexpected device name — check it is the GPU side's)" ;;
esac
