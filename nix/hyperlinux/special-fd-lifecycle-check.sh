#!/usr/bin/env bash
# Invoke through wcrun/cg. Portable mode tests lifecycle exclusion with kqueue
# stubs; Darwin mode only builds the real-kqueue contract for the native lease.
# Usage: MODE=portable|darwin CC=... PYTHON=... bash SCRIPT PATCHED_SOURCE OUT
set -euo pipefail
src=$(realpath "$1")
out=$(realpath -m "$2")
here=$(cd "$(dirname "$0")" && pwd)
"${PYTHON:-python3}" "$here/special-fd-lifecycle-check.py" "$src" "$out"
case "${MODE:-portable}" in
portable)
 cc=${CC:-cc}
 "$cc" -std=c11 -E -I"$out/fake" -I"$src/src" -I"$out" "$out/lifecycle-unit.c" > "$out/lifecycle-portable.i"
 "$cc" -x cpp-output -std=c11 -O2 -ffunction-sections -fdata-sections -Wall -Wextra -Werror -Wno-unused-parameter "$out/lifecycle-portable.i" -Wl,--gc-sections -pthread -o "$out/lifecycle-portable"
 "$out/lifecycle-portable"
 "$cc" -std=c11 -E -I"$out/negative" -I"$out/fake" -I"$src/src" -I"$out" "$out/negative/lifecycle-unit.c" > "$out/lifecycle-negative.i"
 "$cc" -x cpp-output -std=c11 -O2 -ffunction-sections -fdata-sections -Wall -Wextra -Werror -Wno-unused-parameter "$out/lifecycle-negative.i" -Wl,--gc-sections -pthread -o "$out/lifecycle-negative"
 for gate in publication retirement; do
   set +e
   if [[ "$gate" == retirement ]]; then "$out/lifecycle-negative" --retirement-only > "$out/negative-$gate.log" 2>&1; else "$out/lifecycle-negative" > "$out/negative-$gate.log" 2>&1; fi
   status=$?
   set -e
   cat "$out/negative-$gate.log"
   [[ "$status" == 1 ]] || exit 1
   echo "NEGATIVE CONTROL $gate rejected as required"
 done
 ;;
darwin)
 : "${WC_MACOS_CLANG:?set the Darwin cross clang}"
 : "${WC_MACOS_LLD_BIN:?set the directory containing ld64.lld}"
 sdk=${WC_MACOS_SDK:-/mnt/datasets/builds/macos-sdk/MacOSX14.5.sdk}
 "$WC_MACOS_CLANG" --target=arm64-apple-macosx14.0 -isysroot "$sdk" -std=c11 -E -I"$src/src" -I"$out" "$out/lifecycle-unit.c" > "$out/lifecycle-darwin.i"
 "$WC_MACOS_CLANG" --target=arm64-apple-macosx14.0 -isysroot "$sdk" -x cpp-output -std=c11 -O2 -Wall -Wextra -Werror -Wno-unused-parameter -c "$out/lifecycle-darwin.i" -o "$out/lifecycle-darwin.o"
 "$WC_MACOS_LLD_BIN/ld64.lld" -arch arm64 -platform_version macos 14.0 14.5 -syslibroot "$sdk" -dead_strip -lSystem "$out/lifecycle-darwin.o" -o "$out/lifecycle-darwin"
 ;;
*) echo 'MODE must be portable or darwin' >&2; exit 2 ;;
esac
sha256sum "$out"/lifecycle-*.i "$out"/lifecycle-test-main.h "$out"/lifecycle-unit.c > "$out/SOURCE-SHA256SUMS"
