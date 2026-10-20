# Cross-build integration for the existing Hyper Linux Darwin derivation.
# `hlCross` MUST use a Linux build platform and ARM64 Darwin host platform.
# The caller supplies its Apple SDK/cross-signing toolchain, never a Mac builder.
{ lib, hlCross, backend }:
let runtimeInputs = import ./runtime-inputs.nix; in
assert lib.assertMsg (hlCross.stdenv.buildPlatform.isLinux &&
  hlCross.stdenv.hostPlatform.system == "aarch64-darwin")
  "Hyper Linux must cross-build on Linux, not on the real Mac or its guest";
hlCross.overrideAttrs (old: {
  pname = "hl-waycompose";
  src = import ./source.nix;
  patches = (old.patches or [ ]) ++ runtimeInputs.patches;
  postPatch = (old.postPatch or "") + "\n" +
    lib.concatMapStringsSep "\n" (source: "cp ${source} src/${builtins.baseNameOf source}") runtimeInputs.sources;
  WC_HYPERLINUX_LIB = "${backend}/lib/libwc_hyperlinux.a";
  # Preserve upstream signing after the final linked binary is installed.
  dontStrip = true;
  postFixup = (old.postFixup or "") + ''
    codesign --entitlements entitlements.plist -f -s - "$out/bin/hl"
  '';
})
