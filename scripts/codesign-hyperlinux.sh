#!/usr/bin/env bash
# Linux-native signing for the pinned upstream Makefile's codesign invocation.
set -euo pipefail
: "${HL_RCODESIGN:?path to Linux-native rcodesign}"
entitlements= binary=
while (($#)); do
  case "$1" in
    --entitlements) entitlements=$2; shift 2 ;;
    -f) shift ;;
    -s) [[ "$2" == - ]] || exit 2; shift 2 ;;
    -*) echo 'Unsupported codesign argument' >&2; exit 2 ;;
    *) [[ -z "$binary" ]] || exit 2; binary=$1; shift ;;
  esac
done
[[ -n "$entitlements" && -n "$binary" ]] || exit 2
exec "$HL_RCODESIGN" sign --entitlements-xml-file "$entitlements" "$binary"
