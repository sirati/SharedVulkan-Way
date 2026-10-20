#!/usr/bin/env bash
# Build only against an already prepared Linux 6.18.55 kernel build tree.
# Must be invoked through /mnt/datasets/builds/wcrun/cg on this host.
set -euo pipefail
[[ $# == 3 ]] || { echo "usage: $0 PREPARED_SOURCE KERNEL_BUILD OUTPUT_DIRECTORY" >&2; exit 2; }
src=$(realpath "$1")
kernel=$(realpath "$2")
mkdir -p "$3"
out=$(realpath "$3")
[[ $(cat "$src/UPSTREAM_COMMIT") == 725bd2f3c81d54edccb66fead01d4c0c222e2231 ]]
[[ $(cat "$kernel/include/config/kernel.release") == 6.18.55 ]]
cp "$src"/drivers/dma-buf/udmabuf* "$out/"
sha256sum "$out/udmabuf.c" "$out/udmabuf-protected.c" "$out/udmabuf-protected.h" "$out/udmabuf-protected-uapi.h" > "$out/source-hashes.sha256"
cat > "$out/Makefile" <<'MAKE'
obj-m := udmabuf_protected.o
udmabuf_protected-y := udmabuf.o
ccflags-y += -I$(src)
MAKE
if [[ ${UDMABUF_TEST_NODE:-0} == 1 ]]; then
    echo 'ccflags-y += -DUDMABUF_TEST_NODE' >> "$out/Makefile"
    echo '/dev/udmabuf-protected-test; fixture-only alternate node' > "$out/node-provenance.txt"
else
    echo '/dev/udmabuf; replacement node, not loaded' > "$out/node-provenance.txt"
fi
sha256sum "$kernel/.config" > "$out/kernel-config.sha256"
"${CC:-gcc}" --version > "$out/compiler.txt"
"${MAKE:-make}" -C "$kernel" M="$out" W=1 -j2 CC="${CC:-gcc}" HOSTCC="${HOSTCC:-gcc}" LD="${LD:-ld}" modules
sha256sum "$out/udmabuf_protected.ko"
echo 'Built only. No signing, installation or loading performed.'
