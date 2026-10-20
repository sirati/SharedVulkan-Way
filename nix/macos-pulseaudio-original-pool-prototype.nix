# Inactive experimental provider. No session/package selects this expression.
{ pkgs, buildPkgs, sdk }:
let
  accepted = import ./macos-pulseaudio.nix { inherit pkgs buildPkgs sdk; };
in accepted.overrideAttrs (old: {
  pname = "wc-pulseaudio-original-pool-protocol-prototype";
  patches = old.patches ++ [
    ./patches/pulseaudio/original-pool-lease-backend.patch
    ./patches/pulseaudio/original-pool-forwarding.patch
  ];
})
