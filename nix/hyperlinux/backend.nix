{ lib, rustPlatform, stdenv }:
assert lib.assertMsg (stdenv.buildPlatform.isLinux && stdenv.hostPlatform.system == "aarch64-darwin")
  "Build the hl native Rust backend on Linux for ARM64 Darwin";
rustPlatform.buildRustPackage {
  pname = "wc-hyperlinux-backend";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [ ../../Cargo.toml ../../Cargo.lock ../../crates ../../vendor ../../include ];
  };
  cargoLock.lockFile = ../../Cargo.lock;
  cargoBuildFlags = [ "-p" "wc-hyperlinux" "--lib" ];
  doCheck = false; # The target binary cannot execute on the build machine.
  installPhase = ''
    mkdir -p "$out/lib" "$out/include"
    cp target/aarch64-apple-darwin/release/libwc_hyperlinux.a "$out/lib/"
    cp include/wc_hyperlinux.h "$out/include/"
  '';
}
