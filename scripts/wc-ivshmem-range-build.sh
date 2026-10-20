#!/usr/bin/env bash
# Cross-compile only. Never sign, install, or load the resulting driver.
# Invoke through /mnt/datasets/builds/wcrun/cg on the shared build host.
set -euo pipefail
if [[ $# != 2 ]]; then
    echo "usage: $0 PREPARED_SOURCE BUILD_DIRECTORY" >&2
    exit 2
fi
src=$(realpath "$1")
mkdir -p "$2"
out=$(realpath "$2")
repo=$(cd "$(dirname "$0")/.." && pwd)
: "${CLANG_CL:=clang-cl}" "${LLD_LINK:=lld-link}" "${PYTHON:=python3}"
[[ $(git -C "$src" rev-parse HEAD) == d92b9c073ec1108f51846d0bd109c5796b46fd36 ]]
for unit in Range.c Range.h RangeBounds.h; do
    cmp "$repo/drivers/windows-ivshmem/$unit" "$src/ivshmem/$unit"
done
fetch() {
    local package=$1 digest=$2 archive="$out/$1.nupkg"
    if [[ ! -f $archive ]]; then
        curl -fL --retry 3 "https://api.nuget.org/v3-flatcontainer/$package/10.0.26100.1/$package.10.0.26100.1.nupkg" -o "$archive"
    fi
    echo "$digest  $archive" | sha256sum --check
}
fetch microsoft.windows.wdk.x64 247b2919ae451f65ba5f1cd51c7c39730fb0fc383d607f3e8ab317fddc8a8239
fetch microsoft.windows.sdk.cpp b4730a467a8f29145fc0136b2b3f626767e985f6aa6e32f1352953c38d6ff5d2
fetch microsoft.windows.sdk.cpp.x64 7670843ddee568ca1e89b0feb36046ede6a3df5dbf5fa0106ec2eee9e8224e38
"$PYTHON" - "$out" "$src" <<'PY'
from pathlib import Path
import re, shutil, sys, zipfile
out, src = map(Path, sys.argv[1:])
for package in ['microsoft.windows.wdk.x64', 'microsoft.windows.sdk.cpp', 'microsoft.windows.sdk.cpp.x64']:
    with zipfile.ZipFile(out / (package + '.nupkg')) as archive:
        archive.extractall(out / package)
# Windows include spelling is case insensitive. Keep original headers intact,
# adding aliases for include spellings on the Linux build filesystem.
files = [f for name in ['microsoft.windows.wdk.x64', 'microsoft.windows.sdk.cpp']
         for f in (out / name).rglob('*') if f.is_file() and not f.is_symlink()]
byname = {}
for f in files:
    byname.setdefault(f.name.lower(), []).append(f)
for f in files:
    if f.suffix.lower() not in ['.h', '.inc']:
        continue
    for name in re.findall(r'#\s*include\s*[<"]([^>"\r\n]+)', f.read_text(errors='replace')):
        if '/' in name or '\\' in name:
            continue
        for target in byname.get(name.lower(), []):
            alias = target.with_name(name)
            if not alias.exists():
                alias.symlink_to(target.name)
# Do not modify the prepared source checkout for case aliases.
copy = out / 'source'
copy.mkdir(exist_ok=True)
for f in (src / 'ivshmem').iterdir():
    if f.is_file() and not f.is_symlink():
        shutil.copy2(f, copy / f.name)
for f in list(copy.glob('*.h')):
    alias = f.with_name(f.name.lower())
    if not alias.exists():
        alias.symlink_to(f.name)
PY
wdk=$out/microsoft.windows.wdk.x64/c
sdk=$out/microsoft.windows.sdk.cpp/c
"$CLANG_CL" --version > "$out/toolchain.txt"
"$LLD_LINK" --version >> "$out/toolchain.txt"
for unit in Driver Device Queue Range; do
    "$CLANG_CL" --target=x86_64-pc-windows-msvc /nologo /c /kernel /GS /O2 /Zl /W3 \
        /D_AMD64_ /DAMD64 /D_WIN64 /D_WIN32 /D_KERNEL_MODE /DWINVER=0x0A00 \
        /D_WIN32_WINNT=0x0A00 /DNTDDI_VERSION=0x0A000000 \
        /DKMDF_VERSION_MAJOR=1 /DKMDF_VERSION_MINOR=15 \
        /I"$wdk/Include/10.0.26100.0/km" /I"$wdk/Include/10.0.26100.0/km/crt" \
        /I"$sdk/Include/10.0.26100.0/shared" /I"$sdk/Include/10.0.26100.0/ucrt" \
        /I"$wdk/Include/wdf/kmdf/1.15" /I"$out/source" \
        /Fo"$out/$unit.obj" "$out/source/$unit.c"
done
"$LLD_LINK" /nologo /driver /subsystem:native,10.00 /machine:x64 /entry:FxDriverEntry \
    /nodefaultlib /dynamicbase /nxcompat /integritycheck /release /timestamp:0 \
    /out:"$out/ivshmem.sys" /libpath:"$wdk/Lib/10.0.26100.0/km/x64" \
    /libpath:"$wdk/Lib/wdf/kmdf/x64/1.15" \
    "$out/Driver.obj" "$out/Device.obj" "$out/Queue.obj" "$out/Range.obj" \
    ntoskrnl.lib hal.lib wmilib.lib BufferOverflowK.lib wdfdriverentry.lib wdfldr.lib
sha256sum "$out/ivshmem.sys"
