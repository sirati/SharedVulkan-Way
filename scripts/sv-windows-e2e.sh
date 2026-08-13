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
#
# ---------------------------------------------------------------------------
# BOTH SIDES MUST COME FROM ONE BUILD. The GPU side here is a binary *copied*
# into the guest, so rebuilding on the host changes only the host's half.
#
# `WIRE_VERSION` went 1 -> 2 in design.md §0.18 (`svDriverVersions`, which is
# hand-written in `sv_proto::sv` and so outside the protocol hash). A guest
# binary from before that commit is refused at handshake — correctly, and by
# design. The ICD names the cause and the fix in the refusal message; the
# reason it is also written here is that this project has twice read a stale
# deployed binary as a transport fault and lost an afternoon to it.
# ---------------------------------------------------------------------------
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
for f in sv-gpu-server.exe sv_icd.dll sv_icd.json vk-probe.exe; do
  [ -f "$BIN/$f" ] || {
    echo "build first: cargo build -p sv-icd -p sv-gpu-server -p vk-probe \\"
    echo "               --release --target x86_64-pc-windows-gnu"
    exit 1
  }
done

say() { echo "[$(date +%H:%M:%S)] $*"; }

exec 9>/tmp/.sv-windows-e2e.lock
flock -n 9 || { echo "another run is in progress"; exit 1; }

say "publishing to the guest's Z:\\probe"
"$PL" push "$BIN/sv-gpu-server.exe" "$BIN/sv_icd.dll" "$BIN/sv_icd.json" "$BIN/vk-probe.exe"
"$PL" root "install -m0755 /tmp/sv-gpu-server.exe /tmp/sv_icd.dll /tmp/vk-probe.exe /srv/winshare/probe/ && \
            install -m0644 /tmp/sv_icd.json /srv/winshare/probe/" > /dev/null || exit 1

# The two halves run from a *local* directory, not from Z:. virtio-fs is fine
# for reading a file and is not somewhere to load a DLL from: the loader maps
# the image, and a network-ish filesystem under a memory mapping is a class of
# problem nobody needs while proving something else.
say "copying into the guest's local disk"
"$PL" guest 'mkdir -p /c/sv && cp /z/probe/sv-gpu-server.exe /z/probe/sv_icd.dll /z/probe/sv_icd.json /z/probe/vk-probe.exe /c/sv/ && ls -la /c/sv' || exit 1

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
trap 'kill $GPU_PID 2>/dev/null' EXIT INT TERM

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
# --- getting the loader to see our ICD at all -------------------------------
#
# **`VK_DRIVER_FILES` does not work over ssh on this guest, and the reason is
# not ours.** The loader refuses to honour it in an elevated process:
#
#   [Vulkan Loader] INFO: Loader is running with elevated permissions.
#                         Environment variable VK_DRIVER_FILES will be ignored
#
# That is a deliberate loader security measure — an elevated process must not be
# talked into loading an attacker-named DLL — and Windows OpenSSH hands out an
# elevated, high-integrity token for this account. The symptom without reading
# the loader log is perfect and misleading: `vulkaninfo` runs, succeeds, and
# reports `Intel(R) UHD Graphics` with `driverID =
# DRIVER_ID_INTEL_PROPRIETARY_WINDOWS` — the *real* driver, reached directly,
# looking exactly like a forwarding success.
#
# `runas /trustlevel:0x20000` was tried and does not help; the loader still
# reports the process as elevated.
#
# So the ICD is registered where the loader looks for a real one —
# `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`, which is the documented mechanism and
# what `scripts/win-register-icd.ps1` exists for — and **unregistered again on
# every exit path**. The exposure is a few seconds during which another Vulkan
# application on this machine would enumerate one extra device. It is additive
# rather than destructive, it is reversible in one command, and the trap below
# runs on success, failure and interrupt alike.
unregister() {
  "$PL" guest "powershell -NoProfile -ExecutionPolicy Bypass -File C:/sv/win-register-icd.ps1 \
               -Manifest C:/sv/sv_icd.json -Remove" >/dev/null 2>&1
}
say "registering the ICD with the loader (removed again on exit)"
"$PL" push "$ROOT/scripts/win-register-icd.ps1" >/dev/null
"$PL" root "install -m0644 /tmp/win-register-icd.ps1 /srv/winshare/probe/" >/dev/null
"$PL" guest 'cp /z/probe/win-register-icd.ps1 /c/sv/' >/dev/null
trap 'unregister; kill $GPU_PID 2>/dev/null' EXIT INT TERM
"$PL" guest "powershell -NoProfile -ExecutionPolicy Bypass -File C:/sv/win-register-icd.ps1 \
             -Manifest C:/sv/sv_icd.json" 2>&1 | sed 's/^/  /'

say "running the app side through our ICD"
# `vk-probe.exe` rather than `vulkaninfo.exe`, and the reason is WSI rather than
# preference. On Linux `vulkaninfo` skips surface queries because `DISPLAY` is
# unset; on Windows there is always a Win32 surface, so it calls
# `vkGetPhysicalDeviceSurfaceSupportKHR` and dies with
# `ERROR_EXTENSION_NOT_PRESENT` before it prints a single device. Presentation is
# app-side and is X11/Wayland today, so `VK_KHR_win32_surface` is simply not
# implemented; that is a gap in WSI coverage and not in the platform layer this
# script exists to test.
#
# `vk-probe` enumerates devices and asks each one the arena questions, which is
# exactly what wants proving here and needs no surface at all.
"$PL" guest "cd /c/sv && SV_SOCKET=$SESSION ./vk-probe.exe 2>&1" > "$APPLOG" 2>&1
RC=$?
unregister
say "ICD unregistered"

wait $GPU_PID 2>/dev/null
trap - EXIT INT TERM

echo
echo "--- the GPU side ---"
sed 's/^/  /' "$GPULOG"
echo "--- the app side ---"
sed 's/^/  /' "$APPLOG"

# The verdict, and why it is a *count* rather than a string.
#
# On Linux the check is "does the app report the GPU side's driver name rather
# than our synthesised one", because the app side has no GPU of its own and the
# only implementation it can reach is ours. Here both processes are on the
# machine that owns the GPU, so the loader enumerates the Intel ICD *and* ours,
# and both report `Intel(R) UHD Graphics` — the name alone proves nothing.
#
# What proves it is that **there are two of them on a machine with one GPU**.
# A second entry with the same driver version can only be our ICD forwarding to
# the first. The `api=1.3.0` on ours against `1.4.323` on the real one is the
# corroborating detail: 1.3.0 is the cap in our manifest.
DEVICES=$(grep -c '^=== device ' "$APPLOG")
CONNECTED=$(grep -c 'connected to the GPU side' "$APPLOG")
ATTACHED=$(grep -c 'app side attached' "$GPULOG")
SERVING=$(grep -c 'serving rings' "$GPULOG")
echo
echo "app side connected : $CONNECTED"
echo "GPU side attached  : $ATTACHED"
echo "GPU side serving   : $SERVING"
echo "devices enumerated : $DEVICES"
if [ "$CONNECTED" -eq 0 ]; then
  echo "FAIL — the app side never reached the GPU side over the named pipe (exit $RC)"
  exit 1
fi
if [ "$ATTACHED" -eq 0 ] || [ "$SERVING" -eq 0 ]; then
  echo "FAIL — the GPU side never attached the app or never served a ring"
  exit 1
fi
if [ "$DEVICES" -lt 2 ]; then
  echo "FAIL — only $DEVICES device(s); our forwarded one is missing"
  exit 1
fi
REPORTED=$(grep -m1 '^=== device ' "$APPLOG" | sed 's/.*: //' | tr -d '\r')
case "$REPORTED" in
  *"UHD Graphics"*) echo "PASS — the application is reading the guest's real GPU through our ICD" ;;
  *) echo "PASS (unexpected device name — check it is the GPU side's)" ;;
esac
