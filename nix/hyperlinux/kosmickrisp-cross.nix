# Mesa's KosmicKrisp (Vulkan on Metal) for aarch64-darwin, cross-built on
# x86_64-linux. The Mac never compiles: build here, `nix copy` the result.
#
#   nix build --impure --expr '(import ./darwin/kosmickrisp-cross.nix {
#     inputs = (builtins.getFlake "git+file://~/nix-config").inputs; }).kosmickrisp'
#
# Mesa's precompiled shaders need three build-machine tools: mesa_clc and
# vtn_bindgen2 come from nixpkgs' Linux mesa (its cross_tools output, same
# Mesa version); kk_clc is not in there, so it is built natively first. Both
# builds then run with mesa-clc=system and precomp-compiler=system, so no
# LLVM is needed for either host.
{ inputs }:
let
  t = import ./cross-toolchain.nix { inherit inputs; };
  pkgs = t.pkgs;
  inherit (pkgs) lib;

  mesa = pkgs.mesa;
  mesaTools = mesa.cross_tools;

  python = pkgs.python3.withPackages (ps: [
    ps.mako
    ps.packaging
    ps.pyyaml
  ]);

  commonMesonFlags = [
    "-Dgallium-drivers="
    "-Dvulkan-drivers=kosmickrisp"
    "-Dvulkan-layers="
    "-Dtools="
    "-Dopengl=false"
    "-Dgles1=disabled"
    "-Dgles2=disabled"
    "-Dglx=disabled"
    "-Degl=disabled"
    "-Dgbm=disabled"
    "-Dglvnd=disabled"
    "-Dllvm=disabled"
    "-Dspirv-tools=disabled"
    "-Dvalgrind=disabled"
    "-Dlibunwind=disabled"
    "-Dlmsensors=disabled"
    "-Dzlib=disabled"
    "-Dzstd=disabled"
    "-Dexpat=disabled"
    "-Dxmlconfig=disabled"
    "-Dbuild-tests=false"
    "-Dmesa-clc=system"
  ];

  # KosmicKrisp's internal shader compiler, for the build machine.
  kkClc = pkgs.stdenv.mkDerivation {
    pname = "kosmickrisp-kk_clc";
    inherit (mesa) version src;
    nativeBuildInputs = [
      pkgs.meson
      pkgs.ninja
      pkgs.pkg-config
      pkgs.bison
      pkgs.flex
      python
      mesaTools
    ];
    # Linux mesa wants libdrm for any Vulkan driver, even this one.
    buildInputs = [ pkgs.libdrm ];
    mesonAutoFeatures = "disabled";
    mesonFlags = commonMesonFlags ++ [
      "-Dplatforms="
      "-Dprecomp-compiler=enabled"
    ];
    ninjaFlags = [ "src/kosmickrisp/clc/kk_clc" ];
    installPhase = ''
      runHook preInstall
      install -Dm755 src/kosmickrisp/clc/kk_clc "$out/bin/kk_clc"
      runHook postInstall
    '';
  };

  llvm = pkgs.llvmPackages.llvm;
  cc = "${t.compiler}/bin/aarch64-apple-darwin-cc";
  cxx = "${t.compiler}/bin/aarch64-apple-darwin-c++";

  crossFile = pkgs.writeText "aarch64-darwin.ini" ''
    [binaries]
    c = '${cc}'
    cpp = '${cxx}'
    objc = '${cc}'
    ar = '${llvm}/bin/llvm-ar'
    strip = '${llvm}/bin/llvm-strip'
    pkg-config = '${pkgs.pkg-config}/bin/pkg-config'

    [built-in options]
    c_args = ['--target=arm64-apple-macosx15.0', '-Qunused-arguments']
    cpp_args = ['--target=arm64-apple-macosx15.0', '-Qunused-arguments']
    objc_args = ['--target=arm64-apple-macosx15.0', '-Qunused-arguments']
    c_link_args = ['--target=arm64-apple-macosx15.0']
    cpp_link_args = ['--target=arm64-apple-macosx15.0']
    objc_link_args = ['--target=arm64-apple-macosx15.0']

    [properties]
    needs_exe_wrapper = true
    # No target libraries come from pkg-config.
    pkg_config_libdir = '/var/empty'

    [host_machine]
    system = 'darwin'
    subsystem = 'macos'
    kernel = 'xnu'
    cpu_family = 'aarch64'
    cpu = 'aarch64'
    endian = 'little'
  '';

  kosmickrisp = pkgs.stdenv.mkDerivation {
    pname = "kosmickrisp-aarch64-darwin";
    inherit (mesa) version src;
    patches = [ ./patches/mesa/0001-kosmickrisp-import-mtltexture.patch ];

    nativeBuildInputs = [
      pkgs.meson
      pkgs.ninja
      pkgs.pkg-config
      pkgs.bison
      pkgs.flex
      python
      mesaTools
      kkClc
      pkgs.rcodesign
    ];

    dontUseMesonConfigure = true;
    configurePhase = ''
      runHook preConfigure
      # meson rewrites install names of installed Mach-O files.
      mkdir -p "$NIX_BUILD_TOP/bin"
      ln -s ${llvm}/bin/llvm-install-name-tool "$NIX_BUILD_TOP/bin/install_name_tool"
      ln -s ${llvm}/bin/llvm-otool "$NIX_BUILD_TOP/bin/otool"
      export PATH="$NIX_BUILD_TOP/bin:$PATH"
      export SDKROOT=${t.sdk}
      meson setup build \
        --cross-file ${crossFile} \
        --prefix "$out" \
        --libdir lib \
        --buildtype release \
        -Db_ndebug=true \
        -Dauto_features=disabled \
        -Dplatforms=macos \
        -Dprecomp-compiler=system \
        ${lib.concatStringsSep " " commonMesonFlags}
      cd build
      runHook postConfigure
    '';

    # Linux strip and patchelf do not apply to Mach-O.
    dontStrip = true;
    dontPatchELF = true;
    dontPatchShebangs = true;

    postInstall = ''
      # Mutating a Mach-O invalidates its linker-generated ad-hoc signature,
      # and arm64 macOS refuses unsigned code: sign what we ship last.
      rcodesign sign "$out/lib/libvulkan_kosmickrisp.dylib"
    '';

    passthru = { inherit kkClc crossFile; };
  };

in
{
  inherit kosmickrisp kkClc;
}
