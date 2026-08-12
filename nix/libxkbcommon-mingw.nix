# libxkbcommon, cross-built for x86_64-w64-mingw32.
#
# WHY THIS FILE EXISTS
#
# smithay depends on the `xkbcommon` crate **non-optionally**, and that crate is
# `#[link(name = "xkbcommon")]`. `cargo check` never links, so the stage-1
# measurement of smithay (docs/design.md §5.1) proved nothing about whether a
# Windows *binary* can be produced. This derivation is what closes that gap.
#
# nixpkgs' own `libxkbcommon` cannot be used for mingw as packaged, for two
# independent reasons, both measured:
#
#   1. `meta.platforms = unix`, so it is refused for a Windows host outright.
#      That one is cosmetic — `NIXPKGS_ALLOW_UNSUPPORTED_SYSTEM=1` gets past it.
#   2. Its `mesonFlags` reference `libx11.out` and its `buildInputs` include
#      `libxcb`, so the X11 half is not optional. `libx11` genuinely does not
#      cross-compile to mingw here: `xorgproto` for mingw installs no
#      `include/X11`, and libx11's configure dies with
#      "…/include/X11 doesn't exist or isn't a directory". That is a hard stop,
#      not a flag.
#
# So the X11, Wayland, docs, registry and tool halves are switched off. What
# remains — the keymap compiler, the state machine and the compose table — is
# the entire surface smithay uses, and it has no Unix dependency of its own.
#
# Static rather than shared. `-Ddefault_library=static` yields `libxkbcommon.a`
# instead of `libxkbcommon-0.dll` + import library, which means a produced
# `.exe` carries its keymap engine inside it. A compositor that has to ship a
# DLL alongside itself is a deployment problem we can decline to have, and it
# also makes the Wine test honest: nothing is resolved from a search path we
# arranged.
{
  lib,
  pkgsCross,
  meson,
  ninja,
  pkg-config,
  bison,
}:
pkgsCross.mingwW64.libxkbcommon.overrideAttrs (old: {
  pname = "libxkbcommon-mingw";

  buildInputs = [];
  nativeBuildInputs = [meson ninja pkg-config bison];
  outputs = ["out" "dev"];

  # MEASURED UPSTREAM BUG, and the reason the research's "libxkbcommon is the
  # one bright spot" needed qualifying.
  #
  # `src/utils.h` includes `<BaseTsd.h>` unconditionally under `_WIN32`, but the
  # only thing it needs from that header is `SSIZE_T`, used one line later
  # inside `#ifdef _MSC_VER`. mingw-w64 ships the header as `basetsd.h`, lower
  # case, and only that spelling. On Windows the filesystem is case-insensitive
  # so MSVC and clang-cl never notice; cross-compiling from Linux the include
  # simply is not found, and *every* translation unit that reaches `utils.h`
  # fails — 250-odd of them.
  #
  # This is exactly the gap the research flagged when it said non-MSVC Windows
  # support was "claimed-fixed but not CI-covered" (res §3): libxkbcommon's
  # Windows CI is MSVC and clang-cl *on Windows*, so the mingw-from-Linux path
  # has no coverage and this regression survives.
  #
  # The fix is to put the include where the symbol is used. It is two hunks,
  # needs no `#ifdef` that is not already there, and is upstreamable as-is —
  # which is why it is kept as a patch file with a commit message rather than a
  # `substituteInPlace`. See nix/libxkbcommon-mingw-basetsd.patch.
  patches = (old.patches or []) ++ [./libxkbcommon-mingw-basetsd.patch];

  mesonFlags = [
    "-Ddefault_library=static"
    "-Denable-x11=false"
    "-Denable-wayland=false"
    "-Denable-docs=false"
    "-Denable-xkbregistry=false"
    "-Denable-tools=false"
    "-Denable-bash-completion=false"

    # These three are compiled-in absolute paths that only matter to
    # `xkb_keymap_new_from_names`, which walks the xkeyboard-config data tree at
    # runtime. That tree does not exist on Windows and these paths are therefore
    # deliberately *wrong-looking*: they name a location we would have to
    # populate on purpose, so that a `new_from_names` call which silently
    # started working off a Linux store path would be a visible bug rather than
    # an accident. See docs/design.md §7.2 for how keymaps are really obtained.
    "-Dxkb-config-root=C:/ProgramData/waycompose/xkb"
    "-Dxkb-config-extra-path=C:/ProgramData/waycompose/xkb-extra"
    "-Dx-locale-root=C:/ProgramData/waycompose/locale"
  ];

  doCheck = false; # the test binaries are .exe; there is no runner in this shell

  meta =
    old.meta
    // {
      platforms = lib.platforms.all;
      description = "libxkbcommon cross-built for x86_64-w64-mingw32, static, no X11/Wayland";
    };
})
