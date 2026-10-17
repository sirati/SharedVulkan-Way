#!/usr/bin/env bash
# §96.11: Prism Launcher on wc-proxy and wc-compositor.exe under Wine, past its
# setup page (Finish clicked by xdotool, never a key), into its main window.
# The main window sets an xdg_toplevel_icon_v1 and destroys the icon, then its
# buffers; before the smithay fix that was a `no_buffer` protocol error and the
# window died. PASS: the main window shows and the connection does not break.
set -u
# Run inside `nix develop` with prismlauncher, xdotool, Xvfb, xwd and magick on PATH.
W=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=$W/target/release; WINBIN=$W/target/x86_64-pc-windows-gnu/debug
RUN=$(mktemp -d ${TMPDIR:-/tmp}/wc-prism-XXXX); echo "run $RUN"
mkdir -p $RUN/xdg $RUN/keymaps; chmod 700 $RUN/xdg
export WINEPREFIX=${WINEPREFIX:-${SVMC_DATA:-/tmp/svmc}/wineprefix} WINEDEBUG=-all
KIDS=(); trap 'kill "${KIDS[@]}" 2>/dev/null; wineserver -k 2>/dev/null' EXIT
XN=173; Xvfb :$XN -screen 0 1600x1000x24 -nolisten tcp >$RUN/xvfb.log 2>&1 & KIDS+=($!); sleep 1
WCARENA=$((256<<20)); truncate -s $WCARENA $RUN/wc-arena; WSOCK=svp-$$
DISPLAY=:$XN SV_ALLOW_FILE_ARENA=1 wine explorer "/desktop=svp$$,1600x1000" "$WINBIN/wc-compositor.exe" --socket $WSOCK --timeout 200 --arena "0:$WCARENA:$(winepath -w $RUN/wc-arena)" --shared-dir "$(winepath -w $RUN/keymaps)" --shared-peer-prefix "$RUN/keymaps/" --log "$(winepath -w $RUN/compositor.log)" >$RUN/wine.log 2>&1 & KIDS+=($!)
NODE=""; for _ in $(seq 300); do NODE=$(find $WINEPREFIX/drive_c -type s -name $WSOCK 2>/dev/null | head -1); [ -n "$NODE" ] && break; sleep 0.2; done
SV_ALLOW_FILE_ARENA=1 $BIN/wc-proxy --upstream "$NODE" --socket svwl --runtime-dir $RUN/xdg --copy damage --transport arena --arena-path $RUN/wc-arena --arena-id 0 --arena-size $WCARENA --trace --accept-timeout 120 --timeout 200 >$RUN/proxy.log 2>&1 & KIDS+=($!)
for _ in $(seq 80); do [ -S $RUN/xdg/svwl ] && break; sleep 0.25; done
mkdir -p $RUN/nob; printf '#!/bin/sh\nexit 1\n' > $RUN/nob/xdg-open; chmod +x $RUN/nob/xdg-open
env -u DISPLAY XDG_RUNTIME_DIR=$RUN/xdg WAYLAND_DISPLAY=svwl QT_QPA_PLATFORM=wayland QT_WAYLAND_DISABLE_WINDOWDECORATION=1 WAYLAND_DEBUG=1 \
  PATH=$RUN/nob:$PATH DBUS_SESSION_BUS_ADDRESS=disabled: timeout 90 prismlauncher -d ${SVMC_DATA:-/tmp/svmc}/prism > $RUN/prism.log 2>&1 &
PP=$!; KIDS+=($PP)
# The setup page: click its Finish button by position (never a key: the
# focused widget is "Add Microsoft account").
for _ in $(seq 60); do w=$(DISPLAY=:$XN xdotool search --name "Quick Setup" 2>/dev/null | head -1); [ -n "$w" ] && break; sleep 0.5; done
echo "wizard window: ${w:-none}"
sleep 3
if [ -n "${w:-}" ]; then
  eval "$(DISPLAY=:$XN xdotool getwindowgeometry --shell "$w")"
  DISPLAY=:$XN xdotool mousemove $((X + WIDTH - 60)) $((Y + HEIGHT - 24)) click 1
fi
sleep 15; DISPLAY=:$XN xwd -root -silent | magick xwd:- $RUN/after.png
kill $PP 2>/dev/null; sleep 1
grep -a 'connection broke' $RUN/prism.log
echo "$RUN"
if grep -q 'connection broke' "$RUN/prism.log" || grep -q 'ProtocolError' "$RUN/compositor.log"; then
  echo "wc-prism-icon: FAIL"; exit 1
fi
grep -q "icon <- [0-9]* buffer(s) from the client's own xdg_toplevel_icon_v1" "$RUN/compositor.log" \
  && echo "wc-prism-icon: PASS" || { echo "wc-prism-icon: FAIL (no client icon)"; exit 1; }
