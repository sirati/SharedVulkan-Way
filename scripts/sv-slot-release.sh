#!/usr/bin/env bash
# waycompose-design.md §95.10. How a GPU session slot is given back, and what
# a client does when every slot is taken, against `sv-gpu-server serve --slots 1`
# on this machine's RADV. Run inside `nix develop`.
#
#   scripts/sv-slot-release.sh [BIN]     BIN defaults to target/release
#
#   exit     vulkaninfo exits normally; how long until its session ends
#   killed   a process holding a session is SIGKILLed (no atexit); a second
#            client asks at once and must be served within the 10 s it waits
#   full     the holder stays; the second client must give up after ~10 s in
#            total, not 10 s for each of the loader's loads of the ICD
set -u
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
B=${1:-$ROOT/target/release}
VKLOADER=${SVWL_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib"
VI=$(nix build --no-link --print-out-paths nixpkgs#vulkan-tools 2>/dev/null | head -1)/bin/vulkaninfo
PY=$(nix shell nixpkgs#python3 -c bash -c 'command -v python3')
fail=0
ms() { date +%s%3N; }

server() {
  R=$(mktemp -d /tmp/svslot-XXXXXX)
  VK_DRIVER_FILES=$SV_MESA/share/vulkan/icd.d/radeon_icd.x86_64.json \
    "$B/sv-gpu-server" serve --clients 8 --slots 1 --socket "$R/gpu.sock" --idle 60 >"$R/gpu.log" 2>&1 &
  SP=$!
  for _ in $(seq 40); do [ -S "$R/gpu.sock" ] && break; sleep 0.25; done
}
holder() {
  cat >"$R/hold.py" <<'PYEOF'
import ctypes, os, time
v = ctypes.CDLL(os.environ["LD_LIBRARY_PATH"].split(":")[0] + "/libvulkan.so.1")
n = ctypes.c_uint32(0)
v.vkEnumerateInstanceExtensionProperties(None, ctypes.byref(n), None)
time.sleep(60)
PYEOF
  VK_DRIVER_FILES=$B/sv_icd.json SV_SOCKET=$R/gpu.sock "$PY" "$R/hold.py" >/dev/null 2>&1 &
  HP=$!
  for _ in $(seq 100); do grep -q attached "$R/gpu.log" && break; sleep 0.05; done
}
second() {
  local t0; t0=$(ms)
  VK_DRIVER_FILES=$B/sv_icd.json SV_SOCKET=$R/gpu.sock "$VI" --summary >"$R/second.out" 2>&1
  RC=$?; T=$(( $(ms) - t0 ))
}

server
t0=$(ms); VK_DRIVER_FILES=$B/sv_icd.json SV_SOCKET=$R/gpu.sock "$VI" --summary >/dev/null 2>&1; t1=$(ms)
for _ in $(seq 200); do grep -q finished "$R/gpu.log" && break; sleep 0.05; done
t2=$(ms)
gap=$(( t2 - t1 ))
echo "exit:   vulkaninfo took $(( t1 - t0 )) ms; its session ended within $gap ms of its exit"
[ "$gap" -lt 1000 ] || { echo "  FAIL the session outlived its process by $gap ms"; fail=1; }
kill $SP 2>/dev/null; wait $SP 2>/dev/null

server; holder; kill -KILL $HP; wait $HP 2>/dev/null; second
echo "killed: second client exit $RC after $T ms: $(grep -E '^sv-icd: got' "$R/second.out")"
[ "$RC" -eq 0 ] || { echo "  FAIL the second client was not served"; fail=1; }
kill $SP 2>/dev/null; wait $SP 2>/dev/null

server; holder; second
echo "full:   second client exit $RC after $T ms"
{ [ "$RC" -ne 0 ] && [ "$T" -lt 15000 ]; } || { echo "  FAIL expected a refusal within 15 s"; fail=1; }
kill $HP $SP 2>/dev/null; wait 2>/dev/null
[ $fail -eq 0 ] && echo "sv-slot-release: PASS" || echo "sv-slot-release: FAIL"
exit $fail
