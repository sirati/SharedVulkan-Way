#!/usr/bin/env bash
# Where Direct3D stands: DXVK, under Wine, on our forwarded Vulkan.
#
#   scripts/sv-dxvk-d3d11.sh [dxvk-version]     default: 2.6.2
#
# docs/design.md §9.2 declines to advertise `bufferDeviceAddress` and predicts that
# DXVK and vkd3d-proton will therefore "fail device creation cleanly and
# loudly". This script exists because that prediction turned out to be true of
# one DXVK and false of another, and because the thing that actually stops
# Direct3D 11 here is neither of them.
#
# What it establishes, in order:
#
#  1. **DXVK loads against our ICD at all**, which needs three instance
#     extensions Wine puts on every host instance it creates regardless of what
#     the application asked for — see `sv_icd::advertise::INSTANCE_EXTENSIONS`.
#     Without them `vkCreateInstance` fails with `VK_ERROR_EXTENSION_NOT_PRESENT`
#     inside winevulkan and DXVK reports "Failed to create Vulkan 1.1 instance",
#     which names nothing useful.
#
#  2. **What DXVK makes of the device.** On 2.6.2 it enumerates our physical
#     device, reads §9.1's memory table, and computes a maximum Direct3D feature
#     level of **12_0** — so every feature it requires for D3D11 is present, and
#     `bufferDeviceAddress` is not among them. On 2.7 and later it refuses,
#     because 2.7 rewrote the requirement list.
#
#  3. **A frame.** `VK_KHR_swapchain` used to be what stopped this — DXVK marks
#     it *required* even for a device that will never present — and design.md
#     §10 stage 2 now advertises and implements it, so 2.6.2 creates the device
#     and the probe draws. The frame is judged on its pixels, like every other
#     harness here.
#
# Two things this stage found and neither is about DXVK. The **triangle in
# `d3d11-probe.c` was wound the wrong way** and was being back-face culled by
# D3D11's default rasterizer state, on every implementation including RADV with
# no forwarding at all; that was invisible while DXVK refused the device. And
# `--clients` is still exact (below), which is a property of `sv-gpu-server`
# rather than of this test.
#
# Wine, DXVK and the mingw cross-compiler are all in the dev shell or the store.
# The .exe deliberately has no window: see `scripts/d3d11-probe.c`.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
VER="${1:-2.6.2}"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
RUNDIR="$(mktemp -d /tmp/sv-dxvk.XXXXXX)"
MANIFEST="$BIN/sv_icd.json"

[ -f "$MANIFEST" ] || { echo "build first: cargo build -p sv-icd" >&2; exit 1; }
[ -x "$BIN/sv-gpu-server" ] || { echo "build first: cargo build -p sv-gpu-server" >&2; exit 1; }
command -v x86_64-w64-mingw32-gcc >/dev/null || {
  echo "no mingw cross compiler; enter 'nix develop'" >&2; exit 1; }
WINE="$(command -v wine || true)"
[ -n "$WINE" ] || WINE="$(ls -d /nix/store/*-wine-wow-*/bin/wine 2>/dev/null | head -1)"
[ -x "$WINE" ] || { echo "SKIP — no wine" >&2; exit 0; }

CACHE="${SV_DXVK_CACHE:-$HOME/.cache/shared-vulkan}"
mkdir -p "$CACHE"
TARBALL="$CACHE/dxvk-$VER.tar.gz"
if [ ! -f "$TARBALL" ]; then
  echo "fetching DXVK $VER"
  curl -sfL --retry 6 --retry-all-errors -o "$TARBALL" \
    "https://github.com/doitsujin/dxvk/releases/download/v$VER/dxvk-$VER.tar.gz" || {
      echo "SKIP — could not fetch DXVK $VER" >&2; rm -f "$TARBALL"; exit 0; }
fi

export WINEPREFIX="$RUNDIR/prefix"
export WINEDEBUG=-all
export DISPLAY=
mkdir -p "$WINEPREFIX"
"$WINE" wineboot -u > "$RUNDIR/wineboot.log" 2>&1
# Keep the server and its service processes alive across the run. Without this
# a cold prefix starts services *while* the probe is starting, each of them
# creates a Vulkan instance of its own, and they exhaust the GPU side's client
# slots before the application gets one.
"${WINE}server" -p > /dev/null 2>&1
"$WINE" cmd /c exit > /dev/null 2>&1
tar xzf "$TARBALL" -C "$RUNDIR"
cp -f "$RUNDIR/dxvk-$VER/x64/"*.dll "$WINEPREFIX/drive_c/windows/system32/" || {
  echo "could not install DXVK into the prefix" >&2; exit 1; }

PROBE="$RUNDIR/d3d11-probe.exe"
if ! x86_64-w64-mingw32-gcc -O1 -o "$PROBE" "$ROOT/scripts/d3d11-probe.c" \
     -ld3d11 -ldxgi -luuid -lole32 2>"$RUNDIR/cc.log"; then
  echo "  SKIP — d3d11-probe did not build:"; sed 's/^/    /' "$RUNDIR/cc.log" | head -10; exit 0
fi

# `--clients` has to be **exact**, and Wine makes that awkward: `sv-gpu-server
# serve` accepts precisely the number it was told *before* it starts serving, so
# one too few and a helper process takes the application's slot (the loader then
# reports only `VK_ERROR_INCOMPATIBLE_DRIVER`), and one too many and the server
# waits for a client that never comes while the application's first command
# times out. On a cold prefix Wine starts its services alongside the application
# and two instances get created; on a warm one, only the application's. Warming
# the prefix above is what makes "1" a fact rather than a guess.
SOCK="$RUNDIR/gpu.sock"
VK_DRIVER_FILES="$ICDDIR/radeon_icd.x86_64.json" "$BIN/sv-gpu-server" serve \
  --socket "$SOCK" --arena $((256 << 20)) --clients 1 > "$RUNDIR/gpu.log" 2>&1 &
GPUPID=$!
trap '[ -n "${GPUPID:-}" ] && kill "$GPUPID" 2>/dev/null; rm -rf "$RUNDIR"' EXIT
for _ in $(seq 1 400); do [ -S "$SOCK" ] && break; sleep 0.05; done

echo "DXVK $VER, Wine $("$WINE" --version 2>/dev/null), our ICD"
echo
VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" \
  WINEDLLOVERRIDES="d3d11,dxgi,d3d10core,d3d9=n" DXVK_LOG_LEVEL=info \
  "$WINE" "$PROBE" > "$RUNDIR/probe.log" 2>&1
RC=$?
kill "$GPUPID" 2>/dev/null; wait "$GPUPID" 2>/dev/null; GPUPID=""

grep -E "^adapter |^info: +Driver|^info: +Memory Type|Maximum supported feature level|Using feature level|^err:|not supported|^feature level|^coverage|^colours|^PASS|^FAIL" \
  "$RUNDIR/probe.log" | sed 's/^/  /'

echo
if grep -q "^PASS" "$RUNDIR/probe.log"; then
  echo "Direct3D 11 over DXVK $VER over shared_vulkan: PASS"
  exit 0
fi
if grep -q "Required Vulkan extension VK_KHR_swapchain not supported" "$RUNDIR/probe.log"; then
  echo "Direct3D 11 over DXVK $VER: BLOCKED on VK_KHR_swapchain — which docs/design.md §10"
  echo "stage 2 implements, so this is a regression in the WSI rather than a known gap."
  exit 1
fi
echo "Direct3D 11 over DXVK $VER: refused, for the reason above."
echo
echo "A refusal is the **expected** answer for DXVK >= 2.7, which rewrote its"
echo "requirement list and demands bufferDeviceAddress, descriptorIndexing and"
echo "VK_KHR_pipeline_library (docs/design.md §0.5, §9.2). For 2.6.x it is a failure."
exit 1
