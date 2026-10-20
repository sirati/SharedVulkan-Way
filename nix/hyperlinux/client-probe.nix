{ stdenv, lib, programName ? "hl-shared-client", source ? ./shared-client.c,
  buildInputs ? [], linkFlags ? [] }:
assert lib.assertMsg (stdenv.buildPlatform.isLinux && stdenv.hostPlatform.system == "aarch64-linux")
  "Cross-build the ARM64 Linux hl client on Linux";
stdenv.mkDerivation {
  pname = programName;
  version = "0.1.0";
  dontUnpack = true;
  inherit buildInputs;
  buildPhase = ''
    $CC -std=c11 -O2 -Wall -Wextra -Werror ${source} -o ${programName} ${lib.escapeShellArgs linkFlags}
  '';
  installPhase = ''
    mkdir -p "$out/bin"
    cp ${programName} "$out/bin/"
  '';
  meta.mainProgram = programName;
}
