# Construct the whole native session package on a Linux build machine.
# All arguments are explicit; this never invokes a Darwin-native compiler.
{ pkgs, buildPkgs, rustToolchain, sdk }:
assert buildPkgs.stdenv.buildPlatform.isLinux;
assert pkgs.stdenv.hostPlatform.system == "aarch64-darwin";
let
  pulseAudio = import ./macos-pulseaudio.nix { inherit pkgs buildPkgs sdk; };
  hostTools = import ./macos-portal-opener.nix { inherit buildPkgs rustToolchain sdk; };
in (import ./macos-session-services.nix { inherit pkgs buildPkgs hostTools pulseAudio; }) // {
  inherit hostTools pulseAudio;
}
