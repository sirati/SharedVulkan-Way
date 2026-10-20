# Pass the guest's cross package set to build these probes on the build host.
# The resulting executable interpreter and loader match that package set.
{ pkgs }:
pkgs.stdenv.mkDerivation {
  pname = "waycompose-gpu-probes";
  version = "1";
  dontUnpack = true;
  strictDeps = true;
  buildInputs = [ pkgs.vulkan-headers pkgs.vulkan-loader ];
  buildPhase = ''
    runHook preBuild
    $CC -Wall -Wextra -Werror ${./probe-vk-formats.c} -lvulkan -o probe-vk-formats
    $CC -Wall -Wextra -Werror ${./probe-vk-image-usage.c} -lvulkan -o probe-vk-image-usage
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp probe-vk-formats probe-vk-image-usage $out/bin/
    runHook postInstall
  '';
}
