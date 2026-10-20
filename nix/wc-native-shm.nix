# The shipped GTK allocator's mapping/lease helper. Firefox and Mesa are inputs,
# not patched sources. Tests run through the workspace's disposable fixtures.
{ lib, rustPlatform, stdenv }:
rustPlatform.buildRustPackage {
  pname = "wc-native-shm";
  version = "0.1.0";
  src = lib.cleanSourceWith {
    src = ../.;
    filter = path: type:
      lib.cleanSourceFilter path type
      && !(builtins.elem (baseNameOf path) [ "target" ".claude" ".kache-store" ".direnv" ]);
  };
  cargoLock.lockFile = ../Cargo.lock;
  cargoBuildFlags = [ "-p" "wc-native-shm" ];
  doCheck = false;
  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib $out/include
    cp target/${stdenv.hostPlatform.rust.rustcTarget}/release/libwc_native_shm.so $out/lib/
    cp crates/wc-native-shm/include/wc-native-shm.h $out/include/
    runHook postInstall
  '';
}
