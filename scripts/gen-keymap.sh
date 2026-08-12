#!/usr/bin/env bash
# Generate an XKB keymap *string* on Linux, for a Windows binary to compile.
#
#   scripts/gen-keymap.sh [layout] [outfile]
#
# Run from inside `nix develop`.
#
# WHY THIS EXISTS AS A BUILD STEP RATHER THAN A RUNTIME CALL
#
# `xkb_keymap_new_from_names()` — the call every Linux compositor makes, and the
# one smithay's `XkbConfig::compile_keymap` makes — resolves RMLVO names against
# the xkeyboard-config data tree on disk, found via `XKB_CONFIG_ROOT`. That tree
# is ~2,000 files of X11 configuration data. It does not exist on Windows, is
# not shipped with anything on Windows, and shipping it ourselves would mean
# carrying an X11 data package into a compositor that has no X11.
#
# Measured, under Wine, by both `scripts/probe-xkb.c` and `wc-xkb-probe.exe`:
#
#     xkbcommon: ERROR: [XKB-632] Failed to add any default include path
#                       (system path: C:/ProgramData/waycompose/xkb)
#     xkbcommon: ERROR: [XKB-595] Cannot load XKB rules "evdev"
#
# and that failure is arranged deliberately — `nix/libxkbcommon-mingw.nix`
# compiles in a config root nobody creates, so that a `new_from_names` which
# silently started working off a Linux store path would be a visible bug.
#
# `xkb_keymap_new_from_string()` needs none of it. So the split is: names are
# resolved here, on Linux, at build time; the Windows side only ever compiles a
# string. This is also what the research recommends (res §5, "generate the
# keymap strings at build time on Linux for the bounded set of layouts you care
# about and embed them as string literals"), and it is what Weston's RDP backend
# does for the same reason.
#
# The cost, stated: the set of layouts is fixed when the binary is built. A
# Windows compositor answering `WM_INPUTLANGCHANGE` must map the new HKL to one
# of the layouts it shipped. FreeRDP's `xkb_layout_ids.c` — surfaced in Weston
# as `rdp_keyboards[]` — is the KLID-to-RMLVO table for that, and it is stage-4
# work.
set -uo pipefail

LAYOUT="${1:-us}"
OUT="${2:-}"

command -v xkbcli >/dev/null || {
  echo "error: xkbcli not on PATH -- run inside \`nix develop\`" >&2
  exit 2
}

if [ -n "$OUT" ]; then
  xkbcli compile-keymap --layout "$LAYOUT" > "$OUT" || exit 1
  echo "wrote $OUT ($(wc -c < "$OUT") bytes, layout=$LAYOUT)"
else
  xkbcli compile-keymap --layout "$LAYOUT"
fi
