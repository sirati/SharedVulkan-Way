#!/usr/bin/env bash
# Run inside /mnt/datasets/builds/wcrun/cg nix develop -c ... on vmhost.
# Input is a local clone; only the pinned commit's tracked files are used.
set -euo pipefail
[[ $# == 2 ]] || { echo 'usage: build-hyperlinux.sh SOURCE_CLONE OUTPUT_DIRECTORY' >&2; exit 2; }
project=$(cd "$(dirname "$0")/.." && pwd)
source_clone=$(realpath "$1")
out=$(realpath -m "$2")
rev=8048a96ba0abc7e2f548da684690bf415c99214e
: "${WC_MACOS_CLANG:?nix develop must supply a Linux clang capable of Darwin targets}"
: "${WC_MACOS_LLD_BIN:?nix develop must supply the Linux ld64.lld directory}"
: "${HL_RUST_TOOLCHAIN:?path to the Linux Rust toolchain with aarch64-apple-darwin installed}"
: "${HL_CODESIGN:?Linux-native codesign-compatible command preserving Hypervisor entitlements}"
sdk=${WC_MACOS_SDK:-/mnt/datasets/builds/macos-sdk/MacOSX14.5.sdk}
[[ -d "$sdk" && -x "$HL_CODESIGN" ]] || exit 2
mkdir -p "$out"
[[ ! -e "$out/source" ]] || { echo 'Output source directory already exists' >&2; exit 2; }
mkdir "$out/source"
git -C "$source_clone" archive "$rev" | tar -x -C "$out/source"
patch -d "$out/source" -p1 < "$project/nix/hyperlinux/host-backed-mappings.patch"
patch -d "$out/source" -p1 < "$project/nix/hyperlinux/initial-scripts.patch"
patch -d "$out/source" -p1 < "$project/nix/hyperlinux/seccomp.patch"
patch -d "$out/source" -p1 < "$project/nix/hyperlinux/external-guest-fault.patch"
patch -d "$out/source" -p1 < "$project/nix/hyperlinux/peek-rights-guard.patch"
patch -d "$out/source" -p1 < "$project/nix/hyperlinux/guest-status.patch"
patch -d "$out/source" -p1 < "$project/nix/hyperlinux/seqpacket.patch"
cp "$project"/nix/hyperlinux/hl_{shared,seals,drm,socket,copy,atomic,seccomp,seccomp_bpf,status,seqpacket}.{c,h} "$out/source/src/"
cp "$project/nix/hyperlinux/hl_script.h" "$out/source/src/"
cp "$project/include/wc_hyperlinux.h" "$out/source/src/"
export PATH="$HL_RUST_TOOLCHAIN/bin:$WC_MACOS_LLD_BIN:$PATH"
export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$project/scripts/aarch64-macos-linker.sh"
cargo build --manifest-path "$project/Cargo.toml" --target aarch64-apple-darwin -p wc-hyperlinux --lib --bin wc-hl-shared-peer
# Preserve upstream assembly/blob generation and final entitlement signing.
# clang's integrated assembler replaces Darwin `as`; wrappers are build tools,
# never processes in the resulting application topology.
mkdir -p "$out/tools"
cat > "$out/tools/clang" <<WRAPPER
#!/usr/bin/env bash
exec "$WC_MACOS_CLANG" --target=arm64-apple-macosx14.0 -isysroot "$sdk" -fuse-ld=lld "\$@"
WRAPPER
cat > "$out/tools/as" <<WRAPPER
#!/usr/bin/env bash
exec "$WC_MACOS_CLANG" --target=arm64-apple-macosx14.0 -isysroot "$sdk" -c "\$@"
WRAPPER
ln -s "$HL_CODESIGN" "$out/tools/codesign"
chmod +x "$out/tools/clang" "$out/tools/as"
export PATH="$out/tools:$PATH"
# Assemble the two position-independent text blobs with the cross compiler.
mkdir -p "$out/source/_build"
for unit in shim vdso_guest; do
  "$out/tools/clang" -c "$out/source/src/$unit.S" -o "$out/source/_build/$unit.o"
  symbol=shim_bin
  header=shim_blob.h
  if [[ "$unit" == vdso_guest ]]; then symbol=vdso_guest_bin; header=vdso_guest_bin.h; fi
  "${HL_PYTHON:-python3}" "$project/scripts/macho-text-blob.py" \
    "$out/source/_build/$unit.o" "$out/source/_build/$unit.bin" \
    "$out/source/_build/$header" "$symbol"
done
make -C "$out/source" hl VERSION=0.3.0-rc2-waycompose \
  WC_HYPERLINUX_LIB="$project/target/aarch64-apple-darwin/debug/libwc_hyperlinux.a"
mkdir -p "$out/bin"
cp "$out/source/_build/hl" "$out/bin/hl"
cp "$project/target/aarch64-apple-darwin/debug/wc-hl-shared-peer" "$out/bin/"
"$HL_CODESIGN" --entitlements "$out/source/entitlements.plist" -f -s - "$out/bin/hl"
