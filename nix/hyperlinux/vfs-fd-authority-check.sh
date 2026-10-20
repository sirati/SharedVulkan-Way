#!/usr/bin/env bash
set -euo pipefail
src=$(realpath "$1")
out=$(realpath -m "$2")
here=$(cd "$(dirname "$0")" && pwd)
"${PYTHON:-python3}" "$here/vfs-fd-authority-check.py" "$src" "$out"
"${CC:-cc}" -D_GNU_SOURCE -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-unused-function -Wno-unused-variable -include "$out/portable.h" -I"$src/src" "$src/src/fd_object.c" "$out/vfs-check.c" -pthread -o "$out/vfs-check"
"$out/vfs-check"
sed 's/hl_fd_get_pinned(guest_fd/hl_fd_get(guest_fd/' "$out/vfs-check.c" > "$out/vfs-negative.c"
"${CC:-cc}" -D_GNU_SOURCE -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -Wno-unused-function -Wno-unused-variable -include "$out/portable.h" -I"$src/src" "$src/src/fd_object.c" "$out/vfs-negative.c" -pthread -o "$out/vfs-negative"
ulimit -c 0
if "$out/vfs-negative" > "$out/negative.log" 2>&1; then echo 'negative control unexpectedly passed'; exit 1; fi
echo 'PASS unpinned negative control rejected'
