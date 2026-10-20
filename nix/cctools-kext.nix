# A small Linux-hosted classic Darwin linker for MH_KEXT_BUNDLE output.
# LLVM ld64.lld recognizes -kext but does not implement kext output.
{ lib, llvmPackages, fetchFromGitHub, zlib, libxml2, util-linux }:
llvmPackages.stdenv.mkDerivation {
  pname = "cctools-kext-linker";
  version = "973.0.1-609";
  src = fetchFromGitHub {
    owner = "tpoechtrager";
    repo = "cctools-port";
    rev = "df82634ce7c3eddba9722e881433fbe357396e16";
    hash = "sha256-HCx2ZEZZ6EuU+N8+YK8nKecaGKi1xVmCodlqc+uHuJI=";
  };
  sourceRoot = "source/cctools";
  buildInputs = [ zlib libxml2 util-linux ];
  configureFlags = [
    "--target=x86_64-apple-darwin"
    "--disable-tapi-support" "--disable-xar-support" "--disable-lto-support"
  ];
  enableParallelBuilding = true;
  # Only the linker is needed; no assembler, stripping or SDK installation.
  buildPhase = ''
    runHook preBuild
    make -C libstuff -j"$NIX_BUILD_CORES"
    make -C ld64 -j"$NIX_BUILD_CORES"
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    make -C ld64 install
    runHook postInstall
  '';
  meta = {
    description = "Linux-hosted classic Darwin linker for Intel macOS kernel extensions";
    license = lib.licenses.apple-psl20;
    platforms = [ "x86_64-linux" ];
  };
}
