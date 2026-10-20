{
  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  inputs.rust-overlay = { url = "github:oxalica/rust-overlay"; flake = false; };
  outputs = inputs: let p = import ./default.nix { inherit inputs; }; in {
    packages.x86_64-linux = {
      default = p.kosmickrisp;
      inherit (p) kosmickrisp mtltextureTest nv12Test nv12Sampling nv12SamplingTest;
    };
  };
}
