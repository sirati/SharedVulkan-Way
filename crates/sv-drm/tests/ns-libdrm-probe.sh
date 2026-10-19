#!/usr/bin/env bash
# What libdrm 2.4.134 makes of the faux sysfs entry `sv-drm ns` lays down.
#   crates/sv-drm/tests/ns-libdrm-probe.sh path/to/sv-drm
# Builds tests/drm_probe.c against nixpkgs' libdrm and runs it in the namespace
# with /dev/null bound as /dev/dri/renderD128. libdrm matches /dev/dri entries
# by st_rdev, so the device number given to `ns` is the bound node's (1:3); in a
# real run it is the CUSE node's own MAJ:128 and no test node is needed.
set -euo pipefail
svdrm=$(realpath "${1:?usage: $0 path/to/sv-drm}")
here=$(cd "$(dirname "$0")" && pwd)
out=${TMPDIR:-/tmp}/sv-drm-probe.$$
dev=$(nix build --no-link --print-out-paths nixpkgs#libdrm.dev)
lib=$(nix build --no-link --print-out-paths nixpkgs#libdrm.out)
nix shell nixpkgs#gcc -c gcc -O1 -Wall -o "$out" "$here/drm_probe.c" \
  -I"$dev/include" -I"$dev/include/libdrm" -L"$lib/lib" -ldrm -Wl,-rpath,"$lib/lib"
trap 'rm -f "$out"' EXIT
"$svdrm" ns --dev 1:3 --minor 128 --test-node /dev/null -- "$out" 1 3
