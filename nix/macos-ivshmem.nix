# Use the existing darwin/cross-toolchain.nix result as `toolchain`.
# This package builds on Linux only. It neither installs nor activates a driver.
{ toolchain }:
let
  t = toolchain;
  source = ../drivers/macos-ivshmem;
  linker = t.pkgs.callPackage ./cctools-kext.nix {};
in {
  # The SDK-independent access checks are executed natively, then cross-linked
  # with the same compiler/SDK used by the project's macOS packages.
  abi-check = t.pkgs.runCommand "wc-pci-abi-check" {
    nativeBuildInputs = [ t.pkgs.stdenv.cc ];
    dontFixup = true;
  } ''
    mkdir -p "$out/bin"
    c++ -std=c++17 -Wall -Wextra -Werror ${source}/tests/ranges.cpp -o ranges
    ./ranges
    ${t.pkgs.llvmPackages.clang-unwrapped}/bin/clang++ --target=x86_64-apple-macosx14.0 \
      -isysroot ${t.sdk} -std=c++17 -fsyntax-only ${source}/tests/mach-abi.cpp
    ${t.compiler}/bin/aarch64-apple-darwin-c++ -std=c++17 -fsyntax-only ${source}/tests/mach-abi.cpp
    ${t.compiler}/bin/aarch64-apple-darwin-c++ -std=c++17 -Wall -Wextra -Werror \
      ${source}/tests/ranges.cpp -o "$out/bin/wc-pci-abi-check"
  '';
  # Intel only. The existing Darwin toolchain's native clang and classic ld64 build a
  # kernel binary from public IOKit SDK headers. No signing/loading here.
  kext = t.pkgs.runCommand "wc-ivshmem-kext-unsigned" {
    nativeBuildInputs = [ t.pkgs.bash t.pkgs.python3 ];
    dontFixup = true;
  } ''
    export MACOS_SDK=${t.sdk}
    export MACOS_SDK_VERSION=${t.sdk.version}
    export DARWIN_CLANGXX=${t.pkgs.llvmPackages.clang-unwrapped}/bin/clang++
    export DARWIN_LD=${linker}/bin/x86_64-apple-darwin-ld
    export WC_PCI_KEXT_OUT="$TMPDIR/kext"
    bash ${source}/build-kext.sh
    python3 ${source}/verify-kext.py "$WC_PCI_KEXT_OUT/WCIVSHMEM.kext/Contents/MacOS/WCIVSHMEM" > artifact.json
    mkdir -p "$out/Library/Extensions"
    cp -r "$WC_PCI_KEXT_OUT/WCIVSHMEM.kext" "$out/Library/Extensions/"
    mkdir -p "$out/share/doc/wc-ivshmem"
    cp ${../docs/macos-vulkan-client.md} "$out/share/doc/wc-ivshmem/README.md"
  '';
  # Reviewable source distribution. This package performs no activation.
  source-package = t.pkgs.runCommand "wc-pci-driver-source" {} ''
    mkdir -p "$out/share/wc-pci-driver"
    cp -r ${source}/. "$out/share/wc-pci-driver/"
    cp ${../docs/macos-vulkan-client.md} "$out/share/wc-pci-driver/README.md"
  '';
}
