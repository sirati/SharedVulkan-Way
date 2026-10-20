# Native PulseAudio transport provider, compiled and signed only on Linux.
{ pkgs, buildPkgs, sdk }:
assert buildPkgs.stdenv.buildPlatform.isLinux;
assert pkgs.stdenv.hostPlatform.system == "aarch64-darwin";
let
  inherit (buildPkgs) lib;
  llvm = buildPkgs.llvmPackages;
  mesonList = values: "[ " + lib.concatMapStringsSep ", " (value: "'${value}'") values + " ]";
  targetInputs = pkgs.pulseaudio.buildInputs;
  targetLibraries = lib.unique (map (p: p.lib or p.out or p) targetInputs);
  compiler = buildPkgs.runCommand "wc-pulse-darwin-compiler" {} ''
    mkdir -p $out/bin
    cat > $out/bin/cc <<'CC'
    #!${buildPkgs.runtimeShell}
    export PATH=${llvm.lld}/bin:$PATH
    exec ${llvm.clang-unwrapped}/bin/clang --target=arm64-apple-macosx14.0 \
      -isysroot ${sdk} -fuse-ld=lld "$@"
    CC
    cat > $out/bin/c++ <<'CXX'
    #!${buildPkgs.runtimeShell}
    export PATH=${llvm.lld}/bin:$PATH
    exec ${llvm.clang-unwrapped}/bin/clang++ --target=arm64-apple-macosx14.0 \
      -isysroot ${sdk} -fuse-ld=lld -stdlib=libc++ -nostdinc++ \
      -isystem ${sdk}/usr/include/c++/v1 "$@"
    CXX
    chmod +x $out/bin/*
  '';
  crossFile = buildPkgs.writeText "wc-pulse-aarch64-darwin.ini" ''
    [binaries]
    c = '${compiler}/bin/cc'
    cpp = '${compiler}/bin/c++'
    ar = '${llvm.llvm}/bin/llvm-ar'
    strip = '${llvm.llvm}/bin/llvm-strip'
    pkg-config = '${buildPkgs.pkg-config}/bin/pkg-config'
    [built-in options]
    c_args = ${mesonList (map (p: "-I${lib.getDev p}/include") targetInputs)}
    cpp_args = ${mesonList (map (p: "-I${lib.getDev p}/include") targetInputs)}
    c_link_args = ${mesonList (map (p: "-L${p}/lib") targetLibraries)}
    cpp_link_args = ${mesonList (map (p: "-L${p}/lib") targetLibraries)}
    [properties]
    needs_exe_wrapper = true
    [host_machine]
    system = 'darwin'
    cpu_family = 'aarch64'
    cpu = 'aarch64'
    endian = 'little'
  '';
in buildPkgs.stdenv.mkDerivation {
  pname = "wc-pulseaudio-aarch64-darwin";
  inherit (pkgs.pulseaudio) version src;
  patches = pkgs.pulseaudio.patches ++ [ ./patches/pulseaudio/darwin-shared-fd.patch ./patches/pulseaudio/strict-original-pool.patch ];
  nativeBuildInputs = [ buildPkgs.meson buildPkgs.ninja buildPkgs.pkg-config buildPkgs.python3 buildPkgs.rcodesign buildPkgs.gnum4 buildPkgs.gettext ];
  buildInputs = targetInputs;
  strictDeps = true;
  dontUseMesonConfigure = true;
  configurePhase = ''
    runHook preConfigure
    mkdir -p "$NIX_BUILD_TOP/tools"
    ln -s ${llvm.llvm}/bin/llvm-install-name-tool "$NIX_BUILD_TOP/tools/install_name_tool"
    ln -s ${llvm.llvm}/bin/llvm-otool "$NIX_BUILD_TOP/tools/otool"
    export PATH="$NIX_BUILD_TOP/tools:$PATH"
    export SDKROOT=${sdk}
    export MACOSX_DEPLOYMENT_TARGET=14.0
    meson setup build --cross-file ${crossFile} --prefix "$out" --libdir lib \
      --buildtype release -Dtests=false -Dman=false -Ddoxygen=false \
      -Dalsa=disabled -Dasyncns=enabled -Davahi=disabled -Dbluez5=disabled \
      -Dbluez5-gstreamer=disabled -Ddatabase=simple -Delogind=disabled \
      -Dgsettings=disabled -Dgstreamer=disabled -Dgtk=disabled -Djack=disabled \
      -Dlirc=disabled -Dopenssl=disabled -Dorc=disabled -Dsystemd=disabled \
      -Dtcpwrap=disabled -Dudev=disabled -Dvalgrind=disabled -Dwebrtc-aec=enabled \
      -Dx11=disabled -Dconsolekit=disabled -Ddbus=disabled -Dglib=disabled -Doss-output=disabled
    cd build
    runHook postConfigure
  '';
  dontStrip = true;
  dontPatchELF = true;
  dontPatchShebangs = true;
  doCheck = false; # Target executables are never run on the build machine.
  postInstall = ''
    rm -f "$out/bin/qpaeq" # Packaged separately by nixpkgs; its GUI dependencies are absent.
    ${buildPkgs.python3}/bin/python3 - "$out" ${buildPkgs.rcodesign}/bin/rcodesign <<'PY'
    from pathlib import Path
    import subprocess
    import sys
    for path in Path(sys.argv[1]).rglob('*'):
        if path.is_file() and not path.is_symlink():
            with path.open('rb') as source:
                magic = source.read(4)
            if magic == b'\xcf\xfa\xed\xfe':
                subprocess.run([sys.argv[2], 'sign', str(path)], check=True)
    PY
  '';
  passthru = { inherit crossFile; };
}
