#!/usr/bin/env bash
# Offline Intel kernel extension cross-build. No signing, installation or load.
set -euo pipefail
root=$(cd -- "$(dirname -- "$0")" && pwd)
: "${MACOS_SDK:?provide the macOS SDK from the existing Darwin cross toolchain}"
: "${DARWIN_CLANGXX:?provide the Linux-native clang++ used by the Darwin toolchain}"
: "${DARWIN_LD:?provide Linux-native classic ld64 with MH_KEXT_BUNDLE support}"
: "${WC_PCI_KEXT_OUT:?set an output directory under the datasets filesystem}"
headers="$MACOS_SDK/System/Library/Frameworks/Kernel.framework/Headers"
test -f "$headers/IOKit/pci/IOPCIDevice.h"
test -f "$MACOS_SDK/usr/lib/libkmodc++.a"
mkdir -p "$WC_PCI_KEXT_OUT/objects" "$WC_PCI_KEXT_OUT/WCIVSHMEM.kext/Contents/MacOS"
for name in WCIVSHMEM module; do
    "$DARWIN_CLANGXX" --target=x86_64-apple-macosx10.15 -isysroot "$MACOS_SDK" \
        -isystem "$headers" -DKERNEL -DAPPLE_KEXT_OVERRIDE=override \
        -std=c++17 -fno-exceptions -fno-rtti -fno-common -fapple-kext \
        -mkernel -mno-red-zone -mno-mmx -mno-sse -mno-sse2 -mno-avx -fno-stack-protector -Wall -Wextra \
        -Werror -Wno-unused-parameter -Wno-deprecated-declarations \
        -c "$root/$name.cpp" -o "$WC_PCI_KEXT_OUT/objects/$name.o"
done
"$DARWIN_LD" -arch x86_64 -kext -undefined dynamic_lookup \
    -macosx_version_min 10.15 -sdk_version "${MACOS_SDK_VERSION:-14.0}" \
    -o "$WC_PCI_KEXT_OUT/WCIVSHMEM.kext/Contents/MacOS/WCIVSHMEM" \
    "$WC_PCI_KEXT_OUT/objects/WCIVSHMEM.o" "$WC_PCI_KEXT_OUT/objects/module.o" \
    "$MACOS_SDK/usr/lib/libkmodc++.a" "$MACOS_SDK/usr/lib/libkmod.a"
cp "$root/Info.plist" "$WC_PCI_KEXT_OUT/WCIVSHMEM.kext/Contents/Info.plist"
echo 'Built unsigned Intel kext. Kernel symbol validation, trusted signing and runtime validation remain required.'
