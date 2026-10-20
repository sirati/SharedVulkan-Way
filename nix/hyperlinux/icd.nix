{ lib, rustPlatform, stdenv, vulkan-headers }:
assert lib.assertMsg (stdenv.buildPlatform.isLinux && stdenv.hostPlatform.system == "aarch64-linux")
  "The injected ICD must be Linux ARM64 ELF built on Linux";
rustPlatform.buildRustPackage {
  pname = "sv-icd-hyperlinux";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [ ../../Cargo.toml ../../Cargo.lock ../../crates ../../vendor ../../include ];
  };
  cargoLock.lockFile = ../../Cargo.lock;
  cargoBuildFlags = [ "-p" "sv-icd" "--lib" ];
  VK_XML = "${vulkan-headers}/share/vulkan/registry/vk.xml";
  doCheck = false; # Native unit tests belong to the build-machine check suite.
  installPhase = ''
    mkdir -p "$out/lib" "$out/share/vulkan/icd.d"
    cp target/${stdenv.hostPlatform.rust.rustcTarget}/release/libsv_icd.so "$out/lib/"
    # Rewrite the development manifest's absolute scratch path to the installed
    # Linux ELF library. No Darwin .dylib may enter this discovery directory.
    sed "s|\"library_path\":.*|\"library_path\": \"$out/lib/libsv_icd.so\",|" \
      target/${stdenv.hostPlatform.rust.rustcTarget}/release/sv_icd.json \
      > "$out/share/vulkan/icd.d/sv_icd.json"
  '';
}
