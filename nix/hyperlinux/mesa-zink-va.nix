# Unpatched upstream Mesa. Only Zink's Wayland/EGL and VA frontend are built;
# the Linux Vulkan implementation is the separate ELF sv-icd package.
{ mesa, fetchFromGitLab, lib,
  meson, ninja, pkg-config, bison, flex, file, python3Packages, glslang,
  wayland-scanner, patchelf, vulkan-loader }:
let
  base = mesa.override {
    galliumDrivers = [ "zink" ];
    vulkanDrivers = [];
    vulkanLayers = [];
    eglPlatforms = [ "wayland" ];
    withValgrind = false;
  };
  unrelated = d: lib.any (p: lib.hasPrefix p (lib.getName d))
    [ "clang" "llvm" "mesa-libclc" "spirv-llvm-translator" ];
  omitFlag = f: lib.any (p: lib.hasPrefix p f) [ "-Dclang-libdir="
    "-Dgallium-rusticl=" "-Dgallium-rusticl-enable-drivers=" "-Dteflon="
    "-Dintel-rt=" "-Dmesa-clc=" "-Dprecomp-compiler=" ];
in base.overrideAttrs (old: {
  pname = "mesa-zink-va";
  version = "26.3.0-devel-42c325ea";
  src = fetchFromGitLab {
    domain = "gitlab.freedesktop.org";
    owner = "mesa";
    repo = "mesa";
    rev = "42c325ea02518276f08c5b44b487e37affc9fe69";
    hash = "sha256-R2WnUKhY6sg/tqGtA99DoClCOkQ3gwY1rxDVUcdFVbA=";
  };
  # nixpkgs patches OpenCL discovery. This package builds upstream Zink/VA only.
  patches = [];
  outputs = [ "out" ];
  mesonFlags = builtins.filter (f: !omitFlag f) old.mesonFlags ++ [
    "-Dgallium-va=enabled" "-Dgallium-rusticl=false" "-Dteflon=false"
    "-Dintel-rt=disabled" "-Dllvm=disabled" "-Dglx=disabled" "-Dxlib-lease=disabled"
  ];
  buildInputs = builtins.filter (d: !unrelated d) old.buildInputs;
  nativeBuildInputs = [ meson ninja pkg-config bison flex file patchelf
    python3Packages.python python3Packages.packaging python3Packages.pycparser
    python3Packages.mako python3Packages.ply python3Packages.pyyaml
    (lib.getBin glslang) wayland-scanner ];
  disallowedRequisites = [];
  env = builtins.removeAttrs (old.env or {}) [ "MESON_PACKAGE_CACHE_DIR" ];
  preConfigure = "";
  postInstall = "";
  postFixup = ''
    for js in "$out"/share/glvnd/egl_vendor.d/*.json; do
      substituteInPlace "$js" --replace-fail '"libEGL_' '"'"$out/lib/libEGL_"
    done
    rm -f "$out/lib/pkgconfig/dri.pc"
    # Zink uses the unchanged guest Vulkan loader to reach the Linux ELF ICD.
    for so in "$out"/lib/libgallium*.so; do
      patchelf --add-rpath ${vulkan-loader}/lib "$so"
    done
  '';
})
