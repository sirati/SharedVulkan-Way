#!/usr/bin/env bash
set -euo pipefail
src=$(realpath "$1")
out=$(realpath -m "$2")
here=$(cd "$(dirname "$0")" && pwd)
"${PYTHON:-python3}" "$here/vfs-fd-authority-check.py" "$src" "$out"
if [[ "${MODE:-portable}" == darwin ]]; then
 : "${WC_MACOS_CLANG:?set cross clang}"
 : "${WC_MACOS_LLD_BIN:?set ld64.lld directory}"
 sdk=${WC_MACOS_SDK:-/mnt/datasets/builds/macos-sdk/MacOSX14.5.sdk}
 for unit in "$src/src/fd_object.c" "$out/vfs-check.c"; do
   "$WC_MACOS_CLANG" --target=arm64-apple-macosx14.0 -isysroot "$sdk" -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-unused-function -Wno-unused-variable -include "$out/portable.h" -I"$src/src" -c "$unit" -o "$out/$(basename "$unit").o"
 done
 "$WC_MACOS_LLD_BIN/ld64.lld" -arch arm64 -platform_version macos 14.0 14.5 -syslibroot "$sdk" -lSystem "$out/fd_object.c.o" "$out/vfs-check.c.o" -o "$out/vfs-check-darwin"
 exit 0
fi
"${CC:-cc}" -D_GNU_SOURCE -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-unused-function -Wno-unused-variable -include "$out/portable.h" -I"$src/src" "$src/src/fd_object.c" "$out/vfs-check.c" -pthread -o "$out/vfs-check"
"$out/vfs-check"
sed 's/hl_fd_get_pinned(guest_fd/hl_fd_get(guest_fd/' "$out/vfs-check.c" > "$out/vfs-negative.c"
"${CC:-cc}" -D_GNU_SOURCE -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-unused-function -Wno-unused-variable -include "$out/portable.h" -I"$src/src" "$src/src/fd_object.c" "$out/vfs-negative.c" -pthread -o "$out/vfs-negative"
ulimit -c 0
if "$out/vfs-negative" > "$out/negative.log" 2>&1; then echo 'negative control unexpectedly passed'; exit 1; fi
echo 'PASS unpinned negative control rejected'
