# Cross-build integration for the existing Hyper Linux Darwin derivation.
# `hlCross` MUST use a Linux build platform and ARM64 Darwin host platform.
# The caller supplies its Apple SDK/cross-signing toolchain, never a Mac builder.
{ lib, hlCross, backend }:
assert lib.assertMsg (hlCross.stdenv.buildPlatform.isLinux &&
  hlCross.stdenv.hostPlatform.system == "aarch64-darwin")
  "Hyper Linux must cross-build on Linux, not on the real Mac or its guest";
hlCross.overrideAttrs (old: {
  pname = "hl-waycompose";
  src = import ./source.nix;
  patches = (old.patches or [ ]) ++ [ ./host-backed-mappings.patch ./initial-scripts.patch ./seccomp.patch ./external-guest-fault.patch ./async-signal-context.patch ./deferred-signal-irq.patch ];
  postPatch = (old.postPatch or "") + ''
    cp ${./hl_shared.c} src/hl_shared.c
    cp ${./hl_shared.h} src/hl_shared.h
    cp ${./hl_seals.c} src/hl_seals.c
    cp ${./hl_seals.h} src/hl_seals.h
    cp ${./hl_drm.c} src/hl_drm.c
    cp ${./hl_drm.h} src/hl_drm.h
    cp ${./hl_socket.c} src/hl_socket.c
    cp ${./hl_socket.h} src/hl_socket.h
    cp ${./hl_script.h} src/hl_script.h
      cp ${./hl_copy.c} src/hl_copy.c
      cp ${./hl_copy.h} src/hl_copy.h
      cp ${./hl_atomic.c} src/hl_atomic.c
      cp ${./hl_atomic.h} src/hl_atomic.h
    cp ${./hl_seccomp.c} src/hl_seccomp.c
    cp ${./hl_seccomp.h} src/hl_seccomp.h
    cp ${./hl_seccomp_bpf.c} src/hl_seccomp_bpf.c
    cp ${./hl_seccomp_bpf.h} src/hl_seccomp_bpf.h
    cp ${../../include/wc_hyperlinux.h} src/wc_hyperlinux.h
  '';
  WC_HYPERLINUX_LIB = "${backend}/lib/libwc_hyperlinux.a";
  # Preserve upstream signing after the final linked binary is installed.
  dontStrip = true;
  postFixup = (old.postFixup or "") + ''
    codesign --entitlements entitlements.plist -f -s - "$out/bin/hl"
  '';
})
