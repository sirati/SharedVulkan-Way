# wc-pw-node (§106.3): the sound ring's devices as native PipeWire nodes, each
# the driver of its graph and paced by the ring's credit. A small C program
# against libpipewire, built here so that its RUNPATH names the store's
# libpipewire: the VM host need not have PipeWire installed, only this
# closure, which the deploy GC-roots at bin/wc-pw-node.
{
  stdenv,
  pkg-config,
  pipewire,
}:
stdenv.mkDerivation {
  pname = "wc-pw-node";
  version = "1";
  src = ../crates/wc-proxy/pw-node;
  nativeBuildInputs = [pkg-config];
  buildInputs = [pipewire];
  buildPhase = ''
    runHook preBuild
    $CC -O2 -Wall -Wextra -Werror -std=gnu11 -o wc-pw-node wc-pw-node.c \
      $(pkg-config --cflags --libs libpipewire-0.3)
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    install -Dm755 wc-pw-node $out/bin/wc-pw-node
    runHook postInstall
  '';
  meta.mainProgram = "wc-pw-node";
}
