{ inputs }:
let
  pkgs = import inputs.nixpkgs {
    system = "x86_64-linux";
    overlays = [ (import inputs.rust-overlay) ];
    config.allowUnfree = true;
  };
  target = pkgs.lib.systems.elaborate "aarch64-darwin";
  sdkMetadata = builtins.fromJSON (
    builtins.readFile "${inputs.nixpkgs}/pkgs/by-name/ap/apple-sdk/metadata/versions.json"
  );
  sdk =
    (pkgs.callPackage "${inputs.nixpkgs}/pkgs/by-name/ap/apple-sdk/common/fetch-sdk.nix" { })
      sdkMetadata."26";
  compiler =
    pkgs.runCommand "darwin-cross-clang"
      {
        passthru = {
          targetPrefix = "aarch64-apple-darwin-";
          isClang = true;
          bintools = pkgs.llvmPackages.bintools;
        };
      }
      ''
        mkdir -p "$out/bin"
        for entry in cc clang gcc c++; do
          driver=clang
          [ "$entry" != c++ ] || driver=clang++
          cat > "$out/bin/aarch64-apple-darwin-$entry" <<SCRIPT
        #!${pkgs.runtimeShell}
        exec ${pkgs.llvmPackages.clang-unwrapped}/bin/$driver --target=arm64-apple-macosx14.0 -isysroot ${sdk} -B${pkgs.llvmPackages.lld}/bin -fuse-ld=lld "\$@"
        SCRIPT
          chmod +x "$out/bin/aarch64-apple-darwin-$entry"
        done
        ln -s ${pkgs.llvmPackages.llvm}/bin/llvm-ar "$out/bin/aarch64-apple-darwin-ar"
        ln -s ${pkgs.llvmPackages.llvm}/bin/llvm-ranlib "$out/bin/aarch64-apple-darwin-ranlib"
      '';
  stdenv = pkgs.stdenv.override {
    cc = compiler;
    allowedRequisites = null;
    hostPlatform = target;
    targetPlatform = target;
  };
  assemblyStdenv = pkgs.stdenvNoCC.override {
    hostPlatform = target;
    targetPlatform = target;
    allowedRequisites = null;
  };
  builders = pkgs.callPackage "${inputs.nixpkgs}/pkgs/build-support/trivial-builders" {
    stdenvNoCC = assemblyStdenv;
  };
  rust = pkgs.rust-bin.stable.latest.default.override {
    targets = [ "aarch64-apple-darwin" ];
  };
  rustPlatform = pkgs.makeRustPlatform {
    inherit stdenv;
    rustc = rust;
    cargo = rust;
  };
  makeWrapper = pkgs.makeSetupHook {
    name = "make-darwin-shell-wrapper";
    propagatedBuildInputs = [ pkgs.dieHook ];
    substitutions.shell = "/bin/bash";
  } "${inputs.nixpkgs}/pkgs/build-support/setup-hooks/make-wrapper.sh";
in
{
  inherit
    pkgs
    target
    sdk
    compiler
    stdenv
    rust
    rustPlatform
    makeWrapper
    builders
    ;
}
