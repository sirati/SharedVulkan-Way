#!/usr/bin/env bash
# Usage: CC=clang PYTHON=python3 bash fd-authority-check.sh /patched/hl/source
set -euo pipefail
src=$(realpath "$1")
here=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
"${PYTHON:-python3}" - "$src" "$out" <<'PY'
from pathlib import Path
import sys
src, out = map(Path, sys.argv[1:])
s = (src / 'src/syscall.h').read_text()
a = s.index('#define FD_TABLE_SIZE')
b = s.index('/* ---------- API ---------- */', a)
# fd_object is portable; avoid importing unrelated Hypervisor definitions.
(out / 'portable.h').write_text('#define SYSCALL_H\n#include <stdint.h>\n' + s[a:b] + '''
#define LINUX_O_CLOEXEC 02000000
#define LINUX_EBADF 9
#define LINUX_EINVAL 22
#define LINUX_ENOMEM 12
#define LINUX_EMFILE 24
''')
PY
"${CC:-cc}" -D_GNU_SOURCE -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter \
  -include "$out/portable.h" -I"$src/src" "$src/src/fd_object.c" \
  "$here/fd-authority-check.c" -pthread -o "$out/check"
"$out/check"
