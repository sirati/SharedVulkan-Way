#!/bin/sh
# probe-wine.sh <a|b> <exe> [args...]
#  a = wine-wow-11.0, b = wine-wow64-staging-11.9
WINE_A=/nix/store/l83ndddax3qym8a0hnqj90ll36syk7gl-wine-wow-11.0/bin/wine
WINE_B=/nix/store/wwkgf576kjdl15n44hmb0m10c7x5ngjv-wine-wow64-staging-11.9/bin/wine
case "$1" in
  a) WINE=$WINE_A; WINEPREFIX=/tmp/wc-wine/pfx-a ;;
  b) WINE=$WINE_B; WINEPREFIX=/tmp/wc-wine/pfx-b ;;
  *) echo "usage: probe-wine.sh a|b exe [args]"; exit 2 ;;
esac
shift
export WINEPREFIX
export WINEDEBUG=${WINEDEBUG:--all}
export HOME=/tmp/wc-wine/home
mkdir -p "$HOME"
"$WINE" "$@"
echo "wine rc=$?"
