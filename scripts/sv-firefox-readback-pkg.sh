#!/usr/bin/env bash
#
# **The readback-patched Firefox as a store path**, so that it can be copied to
# the VM host with `nix copy` like everything else there (docs/video-decode.md
# §7.9 step 6, waycompose-design.md §86).
#
#   scripts/sv-firefox-readback-pkg.sh OBJDIR [STOCK_FIREFOX]
#
# OBJDIR is a `./mach build` tree of firefox-unwrapped 157.0.1 with
# `nix/firefox-vulkan-readback.patch` applied (video-decode.md §7.7 says how it
# was built). STOCK_FIREFOX is the nixpkgs *wrapped* Firefox of the same
# version, default `nixpkgs#firefox`; its wrapper script is reused with one
# line changed, so the patched build gets exactly the environment the stock
# one gets (ffmpeg 9 with Vulkan, libpulseaudio, GTK's schemas, the nixpkgs
# policies and autoconfig).
#
# Why a derivation and not `nix-store --add`: a path added that way has no
# references, so `nix copy` would send the browser without the hundred-odd
# libraries its RUNPATH names. Copying the tree in a builder makes nix scan the
# output, and the closure is then real.
#
# Prints the store path. Run it inside the build slice; it copies ~1.3 GB twice.
set -euo pipefail
OBJ=${1:?usage: $0 OBJDIR [STOCK_FIREFOX]}
STOCK=${2:-$(nix eval --raw nixpkgs#firefox.outPath)}
[ -x "$OBJ/dist/bin/firefox" ] || { echo "no $OBJ/dist/bin/firefox" >&2; exit 1; }
[ -x "$STOCK/bin/firefox" ] || nix build --no-link "$STOCK" >&2
grep -q 'readback' "$OBJ/dist/bin/libxul.so" \
  || { echo "$OBJ/dist/bin/libxul.so has no readback string: not the patched build" >&2; exit 1; }

# dist/bin is a development tree: most of it is symlinks into the source and
# object directories. Dereferenced here, once, so the store path stands alone.
STAGE=$(mktemp -d /tmp/ff-readback-XXXXXX)
trap 'rm -rf "$STAGE"' EXIT
cp -rL "$OBJ/dist/bin" "$STAGE/firefox" 2>/dev/null || true
[ -f "$STAGE/firefox/libxul.so" ] || { echo "copy failed" >&2; exit 1; }

nix build --no-link --print-out-paths --impure --expr "
let pkgs = (builtins.getFlake \"nixpkgs\").legacyPackages.x86_64-linux;
    dist = builtins.path { path = $STAGE/firefox; name = \"firefox-readback-dist\"; };
    stock = builtins.storePath \"$STOCK\";
in pkgs.runCommand \"firefox-readback-157.0.1\" {} ''
  mkdir -p \$out/lib \$out/bin
  cp -r \${dist} \$out/lib/firefox
  chmod -R u+w \$out/lib/firefox
  # The nixpkgs additions the wrapped Firefox carries: enterprise policies and
  # the autoconfig hook. (The wrapper below still names the stock path, for
  # MOZ_SYSTEM_DIR, so it is in the closure either way.)
  cp -rL \${stock}/lib/firefox/distribution \$out/lib/firefox/ 2>/dev/null || true
  mkdir -p \$out/lib/firefox/defaults/pref
  cp -L \${stock}/lib/firefox/defaults/pref/autoconfig.js \$out/lib/firefox/defaults/pref/ 2>/dev/null || true
  cp -L \${stock}/lib/firefox/mozilla.cfg \$out/lib/firefox/ 2>/dev/null || true
  ln -s ../lib/firefox/firefox \$out/bin/.firefox-wrapped
  # The marker `wc-session run` looks for beside the browser before it turns
  # Vulkan decode on (waycompose-design.md §86): only this build can read a
  # decoded frame back instead of exporting it as a dma-buf.
  touch \$out/lib/firefox/.wc-vulkan-readback
  # The stock wrapper, retargeted: the same environment, this build's binary.
  sed -e 's|\${stock}/bin/.firefox-wrapped|'\$out'/bin/.firefox-wrapped|' \
      \${stock}/bin/firefox > \$out/bin/firefox
  chmod 555 \$out/bin/firefox
  grep -q \"\$out/bin/.firefox-wrapped\" \$out/bin/firefox
''"
