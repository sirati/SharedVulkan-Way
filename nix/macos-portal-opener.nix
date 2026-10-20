# Cross-link the native opener on vmhost. sdk is the existing private Apple SDK,
# never a target machine fetch. rustToolchain must include ARM64 Darwin std.
{ buildPkgs, rustToolchain, sdk }:
assert buildPkgs.stdenv.buildPlatform.isLinux;
let
  platform = buildPkgs.makeRustPlatform { cargo = rustToolchain; rustc = rustToolchain; };
  linker = buildPkgs.writeShellScript "wc-arm64-macos-linker" ''
    export PATH=${buildPkgs.llvmPackages.lld}/bin:$PATH
    exec ${buildPkgs.llvmPackages.clang-unwrapped}/bin/clang \
      --target=arm64-apple-macosx14.0 -isysroot ${sdk} -fuse-ld=lld "$@"
  '';
in platform.buildRustPackage {
  pname = "wc-macos-portal-host-tools";
  version = "0.1.0";
  src = buildPkgs.lib.fileset.toSource {
    root = ../.;
    fileset = buildPkgs.lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../crates ../vendor ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  buildPhase = ''
    runHook preBuild
    export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=${linker}
    export SDKROOT=${sdk}
    export MACOSX_DEPLOYMENT_TARGET=14.0
    cargo build --offline --frozen --release --jobs "$NIX_BUILD_CORES" --target aarch64-apple-darwin \
      -p wc-hostplat --bin wc-portal-open
    runHook postBuild
  '';
  doCheck = false; # Never run the target executable on vmhost.
  installPhase = ''
    mkdir -p $out/bin
    cp target/aarch64-apple-darwin/release/wc-portal-open $out/bin/
  '';
  dontStrip = true; # Linux strip does not understand the Mach-O executable.
}
