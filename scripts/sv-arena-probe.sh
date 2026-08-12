#!/usr/bin/env bash
# Run the two-process arena probe against every Vulkan driver on this machine.
#
#   scripts/sv-arena-probe.sh [--containers]
#
# The question it answers is the one the GPU-allocated arena rests on: an
# unrelated process maps a descriptor the driver exported, writes a pattern
# through that mapping, the GPU runs a shader over it, and the process reads the
# result back. See crates/sv-gpu-server/src/bin/sv-arena-probe.rs.
#
# WHY IT SELECTS A DRIVER BY MANIFEST AND NOT BY INDEX. Physical-device *order*
# is the loader's, and it differs between the host and a container — a sandbox
# without /dev/nvidia* simply does not see the NVIDIA device, so "device 1"
# means different hardware on the two sides of the boundary. Pointing
# VK_DRIVER_FILES at one manifest makes each run unambiguous and is also how the
# GPU side would be pinned in production.
#
# WHAT --containers DEMONSTRATES that two plain processes cannot: the app side
# runs in a sandbox with **no /dev/dri and no /dev/nvidia at all**. It has no
# GPU, cannot open one, and still ends up with a CPU pointer to memory the GPU
# is computing into. That is design.md §1's invariant and the project's whole
# premise, as a process boundary rather than an argument.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
SOCKDIR="$(mktemp -d /tmp/sv-arena-probe.XXXXXX)"
trap 'rm -rf "$SOCKDIR"' EXIT

CONTAINERS=0
[ "${1:-}" = "--containers" ] && CONTAINERS=1

[ -x "$BIN/sv-arena-probe" ] || { echo "build first: cargo build -p sv-gpu-server" >&2; exit 1; }
if [ "$CONTAINERS" = 1 ]; then
  command -v bwrap >/dev/null || { echo "bubblewrap (bwrap) is not on PATH" >&2; exit 1; }
fi

# The GPU side gets the render nodes and, for NVIDIA, its character devices.
# ORDER MATTERS and getting it wrong is silent: bwrap applies these in sequence,
# so `--dev /dev` mounts a fresh devtmpfs *over* anything bound underneath it.
# With the device binds first the render node disappears and the driver falls
# back to nothing, which surfaces three layers away as "no physical device".
gpu_container() {
  local extra=()
  [ -e /dev/dri ] && extra+=(--dev-bind /dev/dri /dev/dri)
  for n in /dev/nvidia*; do [ -e "$n" ] && extra+=(--dev-bind "$n" "$n"); done
  bwrap \
    --ro-bind /nix /nix \
    --ro-bind "$BIN" /opt/sv \
    --bind "$SOCKDIR" /run/sv \
    --ro-bind /run/opengl-driver /run/opengl-driver \
    --ro-bind /sys /sys \
    --proc /proc --dev /dev "${extra[@]}" \
    --tmpfs /tmp --unshare-pid --unshare-ipc --unshare-uts \
    --hostname gpu --die-with-parent \
    -- "$@"
}

# The app side gets no device nodes whatsoever. It shares one directory with the
# GPU side and that directory contains one socket.
app_container() {
  bwrap \
    --ro-bind /nix /nix \
    --ro-bind "$BIN" /opt/sv \
    --bind "$SOCKDIR" /run/sv \
    --proc /proc --dev /dev \
    --tmpfs /tmp --unshare-pid --unshare-ipc --unshare-uts \
    --hostname app --die-with-parent \
    -- "$@"
}

overall=0
for icd in nvidia_icd.json radeon_icd.x86_64.json lvp_icd.x86_64.json; do
  [ -f "$ICDDIR/$icd" ] || continue
  echo
  echo "=============================================================="
  echo "$icd"
  echo "=============================================================="
  sock="$SOCKDIR/${icd%%.*}.sock"
  csock="/run/sv/${icd%%.*}.sock"
  gbin="$BIN/sv-arena-probe"
  abin="$BIN/sv-arena-probe"
  if [ "$CONTAINERS" = 1 ]; then
    gbin=/opt/sv/sv-arena-probe
    abin=/opt/sv/sv-arena-probe
  else
    csock="$sock"
  fi

  export VK_DRIVER_FILES="$ICDDIR/$icd"
  if [ "$CONTAINERS" = 1 ]; then
    gpu_container "$gbin" gpu --socket "$csock" --device 0 2>&1 | sed 's/^/  /' &
  else
    "$gbin" gpu --socket "$csock" --device 0 2>&1 | sed 's/^/  /' &
  fi
  gpid=$!

  # The app side must not inherit VK_DRIVER_FILES: it never opens Vulkan, and
  # leaving it set would hide an accidental dependency on one.
  if [ "$CONTAINERS" = 1 ]; then
    (unset VK_DRIVER_FILES; app_container "$abin" app --socket "$csock") 2>&1 | sed 's/^/  /'
  else
    (unset VK_DRIVER_FILES; "$abin" app --socket "$csock") 2>&1 | sed 's/^/  /'
  fi
  arc=${PIPESTATUS[0]}
  wait $gpid; grc=$?

  # lavapipe is *expected* to fail here and it is not a regression: it has no
  # dma-buf export and its opaque-fd export is not the same pages, so
  # `sv_gpu_server::arena::Provider::for_driver` sends it down the memfd path
  # instead. A pass here would mean the capability table is wrong.
  if [ "$icd" = "lvp_icd.x86_64.json" ]; then
    if [ $grc -ne 0 ]; then
      echo "  ==> EXPECTED FAIL — lavapipe cannot export; it uses the memfd provider"
    else
      echo "  ==> UNEXPECTED PASS — lavapipe exported an arena; Provider::for_driver is stale"
      overall=1
    fi
    continue
  fi

  if [ $arc -eq 0 ] && [ $grc -eq 0 ]; then
    echo "  ==> PASS"
  else
    echo "  ==> FAIL (gpu=$grc app=$arc)"
    overall=1
  fi
done

echo
[ $overall -eq 0 ] && echo "every driver behaved as its capability table says" \
                   || echo "at least one driver did not"
exit $overall
