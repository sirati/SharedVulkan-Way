#!/usr/bin/env bash
# Source checks only. Run through the shared build queue; preserves ivshmem.sys.
set -euo pipefail
if [[ $# != 1 ]]; then
    echo "usage: $0 BUILD_DIRECTORY_FROM_wc-ivshmem-range-build.sh" >&2
    exit 2
fi
build=$(realpath "$1")
repo=$(cd "$(dirname "$0")/.." && pwd)
: "${CLANG_CL:=clang-cl}"
wdk=$build/microsoft.windows.wdk.x64/c
sdk=$build/microsoft.windows.sdk.cpp/c
out=$build/analysis
mkdir -p "$out"
for unit in Range.c Range.h RangeBounds.h; do
    cmp "$repo/drivers/windows-ivshmem/$unit" "$build/source/$unit"
done
"$CLANG_CL" --version > "$out/toolchain.txt"
common=(--target=x86_64-pc-windows-msvc /nologo /kernel /GS /O2 /Zl /W4
    /clang:-Wconversion /clang:-Wshadow /clang:-Wcast-align /clang:-Wformat=2 /clang:-Wundef
    /D_AMD64_ /DAMD64 /D_WIN64 /D_WIN32 /D_KERNEL_MODE /DWINVER=0x0A00
    /D_WIN32_WINNT=0x0A00 /DNTDDI_VERSION=0x0A000000
    /DKMDF_VERSION_MAJOR=1 /DKMDF_VERSION_MINOR=15
    /I"$wdk/Include/10.0.26100.0/km" /I"$wdk/Include/10.0.26100.0/km/crt"
    /I"$sdk/Include/10.0.26100.0/shared" /I"$sdk/Include/10.0.26100.0/ucrt"
    /I"$wdk/Include/wdf/kmdf/1.15" /I"$build/source")
sha256sum "$build/ivshmem.sys" > "$out/image-before.sha256"
for unit in Driver Device Queue Range; do
    "$CLANG_CL" "${common[@]}" /c /Fo"$out/$unit.obj" "$build/source/$unit.c" \
        > "$out/$unit-warnings.log" 2>&1
    "$CLANG_CL" "${common[@]}" /clang:--analyze \
        /clang:-Xclang /clang:-analyzer-output=plist \
        /clang:-Xclang /clang:-analyzer-display-progress \
        /clang:-Xclang /clang:-analyzer-stats \
        /clang:-o /clang:"$out/$unit.plist" "$build/source/$unit.c" \
        > "$out/$unit-analyze.log" 2>&1
done
sha256sum --check "$out/image-before.sha256"
echo "Source checks completed. Review warnings, analyzer reports and coverage limitations."
