#!/usr/bin/env bash
#
# **The render node's relay, end to end, without root** (waycompose-design.md
# §112).
#
#   scripts/sv-relay-e2e.sh [pool MiB]          default 256
#
# sv-pool-e2e.sh's topology (pool-guest on lavapipe, sv-hostd ivshmem-serve
# with the mailbox cold path, the bell hub with socat for QEMU's chardev), but
# the clients can reach neither SV_SOCKET nor SV_BELL: both sit behind a
# directory made mode 000 after the services bound them, so connect() fails
# with EACCES as it does in Firefox's sandbox. They have SV_DRM_NODE instead:
#
#   sv-drm fuse-node   the node's handler behind a FUSE file (/dev/cuse is
#                      root's), with --services as `sv-drm serve` has it; the
#                      clients run as its children, which Yama's scope 1 needs
#                      for a non-root pidfd_getfd
#   sv-hostd, bell-hub register `sv-gpu:<SV_SOCKET>` and `sv-bell:<SV_BELL>`
#                      with --drm-services
#
# The ICD's own code runs unchanged: open the node, socketpair, ioctl, then the
# cold path by descriptor, and every pool region by RegionRequest. It passes
# when every client passes as in sv-pool-e2e, every attach was by descriptor
# through the node, the doorbell came the same way, and a client without
# SV_DRM_NODE is refused.
set -uo pipefail
command -v socat >/dev/null || { echo "need socat (run inside nix develop)"; exit 2; }

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${SVPE_BIN:-$ROOT/target/release}
POOL_MIB=${1:-256}
LVP=${SVPE_LVP:-${SV_MESA:-/run/opengl-driver}/share/vulkan/icd.d/lvp_icd.x86_64.json}
VKLOADER=${SVPE_VKLOADER:-/nix/store/i5p83jmafxzw98q980k8qwhdby5yf0i4-vulkan-loader-1.4.350.0}
export LD_LIBRARY_PATH="$VKLOADER/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
RUN=$(mktemp -d "${SVPE_RUNROOT:-/mnt/datasets/builds/gad-runs}/relay-XXXXXX")
say() { echo "[$(date +%H:%M:%S)] $*"; }
KIDS=()
cleanup() {
  chmod 700 "$RUN/locked" 2>/dev/null
  kill "${KIDS[@]}" 2>/dev/null
  wait 2>/dev/null
  fusermount3 -u "$RUN/node" 2>/dev/null || /run/wrappers/bin/fusermount3 -u "$RUN/node" 2>/dev/null
}
trap cleanup EXIT

truncate -s "${POOL_MIB}M" "$RUN/gpu"
mkdir -p "$RUN/real" "$RUN/locked" "$RUN/node" "$RUN/drm"
ln -s "$RUN/real" "$RUN/locked/in"
SV_SOCK="$RUN/locked/in/icd.sock"   # what the clients are told, and cannot reach
SV_HUB="$RUN/locked/in/hub/sv.sock"
SERVICES="$RUN/drm/services.sock"

socat "UNIX-LISTEN:$RUN/svbell.sock" "UNIX-LISTEN:$RUN/guestbell.sock" 2>"$RUN/socat-bell.log" &
KIDS+=($!)
for _ in $(seq 1 50); do [ -S "$RUN/svbell.sock" ] && break; sleep 0.1; done
"$BIN/sv-hostd" bell-hub --port "$RUN/svbell.sock=$SV_HUB" --drm-services "$SERVICES" >"$RUN/hub.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 50); do [ -S "$RUN/guestbell.sock" ] && [ -S "$SV_HUB" ] && break; sleep 0.1; done
[ -S "$RUN/guestbell.sock" ] && [ -S "$SV_HUB" ] || { echo "FAIL: the doorbell stand-in did not connect"; exit 1; }

say "pool-guest on lavapipe, a ${POOL_MIB} MiB pool, cold path pool, bell on"
VK_DRIVER_FILES="$LVP" "$BIN/sv-gpu-server" pool-guest --pool-file "$RUN/gpu" --bell "socket:$RUN/guestbell.sock" \
  --sessions 8 --idle 20 >"$RUN/gpu.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 100); do grep -q "publishing the extent" "$RUN/gpu.log" && break; sleep 0.1; done
grep -q "publishing the extent" "$RUN/gpu.log" || { echo "FAIL: the GPU side never published"; cat "$RUN/gpu.log"; exit 1; }
"$BIN/sv-hostd" ivshmem-serve --region "$RUN/gpu" --cold pool --cold-bell "$RUN/real/hub/sv.sock" \
  --socket "$SV_SOCK" --drm-services "$SERVICES" >"$RUN/hostd.log" 2>&1 &
KIDS+=($!)
for _ in $(seq 1 50); do [ -S "$SV_SOCK" ] && break; sleep 0.1; done
[ -S "$SV_SOCK" ] || { echo "FAIL: sv-hostd did not bind"; cat "$RUN/hostd.log"; exit 1; }
# From here on nobody can connect to either socket by its path.
chmod 000 "$RUN/locked"

cc -O1 -o "$RUN/gl-probe" "$ROOT/scripts/gl-probe.c" -lEGL -lGL -lm 2>"$RUN/cc.log" || { echo "FAIL: gl-probe"; cat "$RUN/cc.log"; exit 1; }
cc -O1 -o "$RUN/stress" "$ROOT/scripts/sv-pool-stress.c" -lvulkan 2>>"$RUN/cc.log" || { echo "FAIL: sv-pool-stress"; cat "$RUN/cc.log"; exit 1; }

cat >"$RUN/clients.sh" <<EOF
#!/usr/bin/env bash
set -u
RUN="$RUN"
# Both services registered with the node before any client starts.
for _ in \$(seq 1 100); do
  [ "\$(grep -c "registered by uid" "\$RUN/drm.log")" -ge 2 ] && break; sleep 0.1
done
export VK_DRIVER_FILES="$BIN/sv_icd.json" SV_SOCKET="$SV_SOCK" SV_BELL="$SV_HUB"
# Without the node: refused, as the ICD refuses an unreachable GPU side.
timeout 60 vulkaninfo --summary >"\$RUN/nonode.log" 2>&1; echo \$? >"\$RUN/nonode.rc"
export SV_DRM_NODE="$RUN/node/renderD128"
timeout 120 vulkaninfo --summary >"\$RUN/vulkaninfo.log" 2>&1 & A=\$!
timeout 120 vulkaninfo >"\$RUN/vulkaninfo-full.log" 2>&1 & B=\$!
MESA_LOADER_DRIVER_OVERRIDE=zink ZINK_DEBUG=cpu EGL_PLATFORM=surfaceless timeout 180 "\$RUN/gl-probe" >"\$RUN/gl1.log" 2>&1 & C=\$!
MESA_LOADER_DRIVER_OVERRIDE=zink ZINK_DEBUG=cpu EGL_PLATFORM=surfaceless timeout 180 "\$RUN/gl-probe" >"\$RUN/gl2.log" 2>&1 & D=\$!
wait \$A; echo \$? >"\$RUN/a.rc"; wait \$B; echo \$? >"\$RUN/b.rc"; wait \$C; echo \$? >"\$RUN/c.rc"; wait \$D; echo \$? >"\$RUN/d.rc"
timeout 180 "\$RUN/stress" 8 8 big >"\$RUN/stress-big.log" 2>&1 & E=\$!
sleep 3
timeout 180 "\$RUN/stress" 2 0 small >"\$RUN/stress-small.log" 2>&1 & F=\$!
wait \$E; echo \$? >"\$RUN/e.rc"; wait \$F; echo \$? >"\$RUN/f.rc"
timeout 180 "\$RUN/stress" 8 0 after >"\$RUN/stress-after.log" 2>&1; echo \$? >"\$RUN/g.rc"
EOF
chmod +x "$RUN/clients.sh"

say "clients under sv-drm fuse-node: two vulkaninfo, two gl-probe frames on Zink, three pool fills"
"$BIN/sv-drm" fuse-node --mount "$RUN/node" --services "$SERVICES" -- "$RUN/clients.sh" >"$RUN/drm.log" 2>&1
# Every session ends within the GPU side's own idle limit after its process.
sleep 3
rc() { cat "$RUN/$1.rc" 2>/dev/null || echo missing; }
ra=$(rc a); rb=$(rc b); rc_=$(rc c); rd=$(rc d); re=$(rc e); rf=$(rc f); rg=$(rc g); rn=$(rc nonode)

FAIL=0
check() { if eval "$2"; then echo "  ok   $1"; else echo "  FAIL $1"; FAIL=1; fi; }
echo "---- verdict ----"
check "both services registered with the node" '[ "$(grep -c "registered by uid" "$RUN/drm.log")" -ge 2 ]'
check "without SV_DRM_NODE the ICD is refused (exit $rn)" '[ "$rn" != 0 ] && grep -q "cannot reach .*Permission denied" "$RUN/nonode.log"'
check "two vulkaninfo through the relay (exit $ra, $rb)" '[ "$ra" = 0 ] && [ "$rb" = 0 ] && grep -q llvmpipe "$RUN/vulkaninfo.log"'
check "gl-probe frames judged good, on Zink through the relay (exit $rc_, $rd)" '[ "$rc_" = 0 ] && [ "$rd" = 0 ] && grep -qi "zink" "$RUN/gl1.log" "$RUN/gl2.log"'
check "the 8 MiB fill ended in OUT_OF_DEVICE_MEMORY, untouched (exit $re)" '[ "$re" = 0 ] && grep -q "ok=1" "$RUN/stress-big.log"'
check "the 2 MiB fill beside it did too (exit $rf)" '[ "$rf" = 0 ] && grep -q "ok=1" "$RUN/stress-small.log"'
check "a fill after both got the pool back (exit $rg)" '[ "$rg" = 0 ] && grep -q "ok=1" "$RUN/stress-after.log"'
check "no lost device anywhere" '! grep -qi "DEVICE_LOST\|lost device" "$RUN"/*.log'
check "every attach was by descriptor through the node (>= 7)" '[ "$(grep -c "(through the render node, by descriptor) attached:" "$RUN/hostd.log")" -ge 7 ] && ! grep -q "(socket" "$RUN/hostd.log"'
check "the ICDs said so, and opened no name" 'grep -q "attached by descriptor through the render node" "$RUN/vulkaninfo.log" "$RUN/stress-big.log"'
check "pool regions came by RegionRequest" '[ "$(grep -c "[1-9][0-9]* region descriptor(s) given" "$RUN/hostd.log")" -ge 7 ]'
check "the doorbell came through the node" 'grep -q "the doorbell comes through the render node" "$RUN/stress-big.log" "$RUN/vulkaninfo.log"'
check "every session gave its leases back" '[ "$(grep -c "lease(s) and the control region given back" "$RUN/gpu.log")" -ge 7 ]'
echo "---- the node ----"
grep -E "registered|passed to|errno|fuse-node" "$RUN/drm.log" | sort | uniq -c | sort -rn | head -12
echo "---- the cold path, per ICD (sv-hostd) ----"
grep -oE "ICD client [0-9]+ \(.*|region descriptor.*" "$RUN/hostd.log" | cut -c1-160 | head -12
grep RESULT "$RUN"/stress-*.log
echo "run dir: $RUN"
[ $FAIL = 0 ] && { say "sv-relay-e2e: PASS"; rm -f "$RUN/gpu"; } || say "sv-relay-e2e: FAIL"
exit $FAIL
