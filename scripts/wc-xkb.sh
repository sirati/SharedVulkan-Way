#!/usr/bin/env bash
# Prove that libxkbcommon works in a Windows binary — by linking and running,
# not by checking.
#
#   scripts/wc-xkb.sh
#
# Run from inside `nix develop`.
#
# WHY THIS IS A SEPARATE SCRIPT FROM THE COMPOSITOR TESTS
#
# docs/design.md §5.4 item 2 named this the prerequisite for the smithay
# decision being *actionable*, and named it as unmeasured. The reason it was
# unmeasured is worth repeating because it is the whole point of this script:
# smithay depends on the `xkbcommon` crate **non-optionally**, that crate is
# `#[link(name = "xkbcommon")]`, and `cargo check` never invokes the linker.
# Every "smithay compiles for Windows" number before this stage was therefore
# silent about whether a Windows *binary* could be produced at all.
#
# Four things are established here, in order of what they cost to be wrong:
#
#   1. libxkbcommon cross-compiles for x86_64-w64-mingw32.
#      nix/libxkbcommon-mingw.nix, and one upstream bug had to be patched to get
#      there (see that file, and nix/libxkbcommon-mingw-basetsd.patch).
#   2. A C program links against it and runs. scripts/probe-xkb.c.
#   3. The Rust crate binds to it and runs. crates/wc-xkb-probe. Different
#      question from (2): the fork's `cfg(unix)` gating has to be right and
#      cargo's linker invocation has to resolve the static library.
#   4. `xkb_keymap_new_from_names` *fails*, deliberately, because there is no
#      xkeyboard-config tree on Windows. See scripts/gen-keymap.sh.
#
# WINE IS NOT WINDOWS. Unchanged from stage 1 and still the most important line
# in the file. libxkbcommon is pure userspace computation over a string — no
# syscalls beyond `fopen`, no sockets, no timers — which makes this one of the
# few results here where Wine is close to conclusive. But "close to" is not
# "is", and the guest still has to confirm it.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"

: "${WINEPREFIX:=/tmp/wc-wineprefix}"
: "${WINEDEBUG:=-all}"
export WINEPREFIX WINEDEBUG

pass=0
fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }

command -v wine >/dev/null || { echo "error: wine not on PATH"; exit 2; }
[ -n "${WC_XKB_MINGW:-}" ] || { echo "error: WC_XKB_MINGW unset -- run inside \`nix develop\`"; exit 2; }

RUNDIR=$(mktemp -d /tmp/wc-xkb-XXXXXX)
trap 'rm -rf "$RUNDIR"' EXIT

# The keymap the Windows side will compile, generated here on Linux from real
# RMLVO names. This is the production path, not a test fixture.
"$ROOT/scripts/gen-keymap.sh" us "$RUNDIR/us.xkb" >/dev/null || {
  bad "could not generate a keymap"; exit 1
}
ok "generated a us keymap on Linux: $(wc -c < "$RUNDIR/us.xkb") bytes"

echo
echo "== the C library, as a Windows binary =="
if x86_64-w64-mingw32-gcc -O1 -o "$RUNDIR/probe-xkb.exe" "$ROOT/scripts/probe-xkb.c" \
     -I"$WC_XKB_MINGW_DEV/include" -L"$WC_XKB_MINGW/lib" -lxkbcommon 2>"$RUNDIR/cc.log"; then
  # Statically linked, so the .exe carries the keymap engine. A dynamic build
  # would need the DLL beside it and the test would silently be about a search
  # path instead of about the library.
  ok "linked probe-xkb.exe ($(stat -c %s "$RUNDIR/probe-xkb.exe") bytes, static)"
else
  bad "could not link probe-xkb.exe"; sed 's/^/        /' "$RUNDIR/cc.log"
fi

if [ -f "$RUNDIR/probe-xkb.exe" ]; then
  ( cd "$RUNDIR" && wine ./probe-xkb.exe us.xkb ) >"$RUNDIR/c.log" 2>&1
  if [ $? -eq 0 ] && grep -q "all checks passed" "$RUNDIR/c.log"; then
    ok "probe-xkb.exe: every check passed"
    grep -E "^  \[|keycode |get_as_string" "$RUNDIR/c.log" | sed 's/^/      /'
  else
    bad "probe-xkb.exe failed"; sed 's/^/        /' "$RUNDIR/c.log"
  fi
fi

echo
echo "== the Rust crate, as a Windows binary =="
if [ -f "$WIN_BIN/wc-xkb-probe.exe" ]; then
  cp "$RUNDIR/us.xkb" "$RUNDIR/k.xkb"
  ( cd "$RUNDIR" && wine "$WIN_BIN/wc-xkb-probe.exe" k.xkb ) >"$RUNDIR/rs.log" 2>&1
  if [ $? -eq 0 ] && grep -q "all checks passed" "$RUNDIR/rs.log"; then
    ok "wc-xkb-probe.exe: every check passed"
    grep -E "^  \[|keycode |get_as_string" "$RUNDIR/rs.log" | sed 's/^/      /'
  else
    bad "wc-xkb-probe.exe failed"; sed 's/^/        /' "$RUNDIR/rs.log"
  fi
else
  bad "no $WIN_BIN/wc-xkb-probe.exe -- cargo build --target x86_64-pc-windows-gnu first"
fi

echo
echo "== the whole point: smithay's binary links it too =="
# `wc-compositor.exe` existing at all is the load-bearing fact. smithay depends
# on `xkbcommon` non-optionally, so this .exe cannot have been produced without
# libxkbcommon resolving at link time.
if [ -f "$WIN_BIN/wc-compositor.exe" ]; then
  ok "wc-compositor.exe links ($(stat -c %s "$WIN_BIN/wc-compositor.exe") bytes) -- smithay's non-optional xkbcommon dependency resolved"
else
  bad "no wc-compositor.exe"
fi

echo
echo "== $pass passed, $fail failed =="
[ "$fail" -eq 0 ]
