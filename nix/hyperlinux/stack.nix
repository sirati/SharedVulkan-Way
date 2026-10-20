# A complete ARM64 Darwin launcher closure, assembled and cross-built on Linux.
{ inputs, sessionPackage ? null }:
let
  t = import ./cross-toolchain.nix { inherit inputs; };
  pkgs = t.pkgs;
  nativeSource = inputs.nativeNixpkgs or inputs.nixpkgs;
  native = import nativeSource { system = "aarch64-darwin"; config.allowUnfree = true; };
  guest = import inputs.nixpkgs {
    localSystem = "x86_64-linux";
    crossSystem = "aarch64-linux";
    crossOverlays = [ (_final: prev: {
      # GCC16's target libgomp trips format-truncation under its own -Werror.
      # Keep the target compiler source unchanged and make warnings nonfatal.
      gcc16 = prev.gcc16.override {
        cc = prev.gcc16.cc.overrideAttrs (old: {
          configureFlags = (old.configureFlags or []) ++ [ "--disable-werror" ];
        });
      };
    }) ];
    config.allowUnfree = true;
  };
  # Use the same target libc source/version for stock apps and custom libraries.
  # Both stock ARM64 Firefox and glibc are available from the binary cache.
  guestApps = import inputs.nixpkgs { system = "aarch64-linux"; config.allowUnfree = true; };
  lib = pkgs.lib;
  project = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [ ../../Cargo.toml ../../Cargo.lock ../../crates ../../vendor ../../include ];
  };
  rustEnv = {
    SDKROOT = "${t.sdk}";
    MACOSX_DEPLOYMENT_TARGET = "14.0";
    CC_aarch64_apple_darwin = "${t.compiler}/bin/aarch64-apple-darwin-cc";
    CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER = "${t.compiler}/bin/aarch64-apple-darwin-cc";
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "${pkgs.stdenv.cc}/bin/cc";
    VK_XML = "${pkgs.vulkan-headers}/share/vulkan/registry/vk.xml";
    PKG_CONFIG_ALLOW_CROSS = "1";
    PKG_CONFIG_PATH = "${native.libxkbcommon.dev}/lib/pkgconfig";
    CARGO_TARGET_AARCH64_APPLE_DARWIN_RUSTFLAGS = "-L native=${native.libxkbcommon}/lib";
  };
  backend = t.rustPlatform.buildRustPackage {
    pname = "wc-hyperlinux-backend"; version = "0.1.0";
    src = project; cargoLock.lockFile = ../../Cargo.lock;
    cargoBuildFlags = [ "-p" "wc-hyperlinux" "--lib" "--bin" "wc-hl-shared-peer" ];
    env = rustEnv; doCheck = false; dontStrip = true;
    installPhase = ''
      mkdir -p "$out/lib" "$out/bin"
      cp target/aarch64-apple-darwin/release/libwc_hyperlinux.a "$out/lib/"
      cp target/aarch64-apple-darwin/release/wc-hl-shared-peer "$out/bin/"
      ${pkgs.removeReferencesTo}/bin/remove-references-to -t ${t.rust} "$out/bin/wc-hl-shared-peer"
      ${pkgs.rcodesign}/bin/rcodesign sign "$out/bin/wc-hl-shared-peer"
    '';
  };
  hostTools = t.rustPlatform.buildRustPackage {
    pname = "waycompose-hyperlinux-native"; version = "0.1.0";
    src = project; cargoLock.lockFile = ../../Cargo.lock;
    cargoBuildFlags = [ "-p" "sv-guest" "--bin" "sv-guest" ];
    nativeBuildInputs = [ pkgs.pkg-config ];
    env = rustEnv; doCheck = false; dontStrip = true;
    postFixup = ''
      ${pkgs.removeReferencesTo}/bin/remove-references-to -t ${t.rust} "$out/bin/sv-guest"
      ${pkgs.rcodesign}/bin/rcodesign sign "$out/bin/sv-guest"
    '';
  };
  # Existing native KosmicKrisp provider, with its authorized MTLTexture import
  # patch. This is separate from the unmodified Linux Mesa Zink/VA package.
  kosmickrisp = (import ./kosmickrisp-cross.nix { inherit inputs; }).kosmickrisp;
  hl = t.stdenv.mkDerivation {
    pname = "hl-waycompose"; version = "0.3.0-rc2";
    src = import ./source.nix;
    patches = [ ./host-backed-mappings.patch ];
    nativeBuildInputs = [ pkgs.gnumake pkgs.python3 pkgs.rcodesign ];
    dontConfigure = true; dontStrip = true; dontPatchELF = true; dontPatchShebangs = true;
    postPatch = ''
      cp ${./hl_shared.c} src/hl_shared.c
      cp ${./hl_shared.h} src/hl_shared.h
      cp ${./hl_seals.c} src/hl_seals.c
      cp ${./hl_seals.h} src/hl_seals.h
      cp ${./hl_drm.c} src/hl_drm.c
      cp ${./hl_drm.h} src/hl_drm.h
      cp ${./hl_socket.c} src/hl_socket.c
      cp ${./hl_socket.h} src/hl_socket.h
      cp ${../../include/wc_hyperlinux.h} src/wc_hyperlinux.h
    '';
    buildPhase = ''
      runHook preBuild
      mkdir -p _build
      for unit in shim vdso_guest; do
        ${t.compiler}/bin/aarch64-apple-darwin-cc -c src/$unit.S -o _build/$unit.o
        symbol=shim_bin header=shim_blob.h
        if [ "$unit" = vdso_guest ]; then symbol=vdso_guest_bin; header=vdso_guest_bin.h; fi
        ${pkgs.python3}/bin/python3 ${../../scripts/macho-text-blob.py} \
          _build/$unit.o _build/$unit.bin _build/$header "$symbol"
      done
      mkdir -p "$NIX_BUILD_TOP/sign-tools"
      cp ${../../scripts/codesign-hyperlinux.sh} "$NIX_BUILD_TOP/sign-tools/codesign"
      substituteInPlace "$NIX_BUILD_TOP/sign-tools/codesign" \
        --replace-fail '#!/usr/bin/env bash' '#!${pkgs.runtimeShell}'
      chmod +x "$NIX_BUILD_TOP/sign-tools/codesign"
      ln -s ${t.compiler}/bin/aarch64-apple-darwin-cc "$NIX_BUILD_TOP/sign-tools/clang"
      export HL_RCODESIGN=${pkgs.rcodesign}/bin/rcodesign
      export PATH="$NIX_BUILD_TOP/sign-tools:$PATH"
      make hl WC_HYPERLINUX_LIB=${backend}/lib/libwc_hyperlinux.a
      runHook postBuild
    '';
    installPhase = ''
      mkdir -p "$out/bin"
      cp _build/hl "$out/bin/hl"
      ${pkgs.removeReferencesTo}/bin/remove-references-to -t ${t.rust} "$out/bin/hl"
      ${pkgs.rcodesign}/bin/rcodesign sign --entitlements-xml-file entitlements.plist "$out/bin/hl"
    '';
  };
  session = if sessionPackage != null then { package = sessionPackage; } else
    import ../macos-session-package.nix {
      pkgs = native; buildPkgs = pkgs; rustToolchain = t.rust; sdk = t.sdk;
    };
  compositor = import ./compositor.nix {
    inherit pkgs;
    nativeBash = native.bashNonInteractive;
    nativeHostTools = hostTools;
    nativeDriverManifest = "${kosmickrisp}/share/vulkan/icd.d/kosmickrisp_mesa_icd.aarch64.json";
    nativeVulkanLoader = "${native.vulkan-loader}/lib/libvulkan.1.dylib";
  };
  mkApp = import ./mk-app.nix {
    inherit pkgs hl compositor;
    guestPkgs = guestApps;
    guestIcd = guest.callPackage ./icd.nix { };
    guestMesa = guest.callPackage ./mesa-zink-va.nix { };
    nativeBash = native.bashNonInteractive;
    sessionServices = session.package;
  };
  firefox = mkApp { name = "hyperlinux-firefox"; program = guestApps.firefox; };
  probeClient = guest.callPackage ./client-probe.nix { };
  renderClient = guest.callPackage ./client-probe.nix {
    programName = "hl-render-client"; source = ./render-client.c;
  };
  accessClient = guest.callPackage ./client-probe.nix {
    programName = "hl-shared-access-client"; source = ./shared-access-client.c;
  };
  socketClient = guest.callPackage ./client-probe.nix {
    programName = "hl-socket-client"; source = ./socket-client.c;
  };
  probeRoot = pkgs.runCommand "hyperlinux-probe-sysroot" {} ''
    mkdir -p "$out/lib"
    ln -s ${guest.glibc}/lib/ld-linux-aarch64.so.1 "$out/lib/ld-linux-aarch64.so.1"
  '';
  probeRunner = pkgs.writeTextFile {
    name = "hyperlinux-probe-runner"; executable = true;
    text = ''
      #!${native.bashNonInteractive}/bin/bash
      set -euo pipefail
      fixture="''${HL_NATIVE_FIXTURE:-$(dirname "$2")}"
      exec ${hl}/bin/hl --timeout 30 --audio-backend null --fs-mode=rooted --isolated --sysroot ${probeRoot} \
        --guest-home /home/user --guest-cwd /home/user \
        --bind "$HOME:/home/user" --bind "$fixture:$fixture" \
        --bind "/nix/store:/nix/store:ro" "$@"
    '';
  };
  probe = pkgs.writeTextFile {
    name = "hyperlinux-shared-probe"; executable = true;
    destination = "/bin/hyperlinux-shared-probe";
    text = ''
      #!${native.bashNonInteractive}/bin/bash
      set -euo pipefail
      export HL_SHARED_PEER=${backend}/bin/wc-hl-shared-peer
      export HL_SHARED_CLIENT=${probeClient}/bin/hl-shared-client
      export HL_RUNNER=${probeRunner}
      export HL_PROBE_ROOT="''${HL_PROBE_ROOT:-/tmp}"
      ${native.bashNonInteractive}/bin/bash ${../../scripts/hl-shared-probe.sh}
      fixture=$(mktemp -d "$HL_PROBE_ROOT/hl-render.XXXXXXXX")
      trap 'rm -rf -- "$fixture"' EXIT
      mkdir -p "$fixture"/{home,config,data,state,runtime}
      chmod 700 "$fixture/runtime"
      export HOME="$fixture/home" XDG_CONFIG_HOME="$fixture/config" XDG_DATA_HOME="$fixture/data"
      export XDG_STATE_HOME="$fixture/state" XDG_RUNTIME_DIR="$fixture/runtime" HL_NATIVE_FIXTURE="$fixture"
      "$HL_RUNNER" ${renderClient}/bin/hl-render-client
      "$HL_RUNNER" ${socketClient}/bin/hl-socket-client
      "$HL_RUNNER" ${accessClient}/bin/hl-shared-access-client
    '';
  };
in {
  inherit hl backend hostTools kosmickrisp session compositor firefox probe;
}
