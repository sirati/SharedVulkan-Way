#!/bin/sh
# probe-build.sh <name>  -> builds scripts/probe-<name>.c to /tmp/probe-<name>.exe
# The bare gcc store path given in the task has no crt/libc paths baked in;
# the nixpkgs wrapper below wraps that same gcc 15.2.0 and does.
GCCDIR=/nix/store/8sv04sd04gzyy29ppznzl2xhw7l5n469-x86_64-w64-mingw32-gcc-wrapper-15.2.0/bin
PATH=$GCCDIR:$PATH
export PATH
SRCDIR=/mnt/datasets/builds/waycompose-base/scripts
n=$1
shift
MCF=/nix/store/1g04w6v3vpwmbinaagwwnd1n7v8dfv8j-mcfgthread-x86_64-w64-mingw32-2.4.2/lib
x86_64-w64-mingw32-gcc -O1 -g -L$MCF -o /tmp/probe-$n.exe $SRCDIR/probe-$n.c "$@"
echo "build rc=$?"
