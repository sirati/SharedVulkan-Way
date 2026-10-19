#!/usr/bin/env bash
# §104 end to end, under Wine: a "dma-buf" window through wc-proxy
# --dmabuf-node into wc-compositor.exe, with nothing copied.
#
#   scripts/wc-dmabuf.sh            (from inside `nix develop`, after
#                                    cargo build -p wc-proxy and
#                                    cargo build -p wc-compositor --target x86_64-pc-windows-gnu)
#
# The client (scripts/wc-dmabuf-client.c, built here) binds the proxy's
# zwp_linux_dmabuf_v1, puts its pixels in /dev/shm/<name>, hands over an
# sv_drm token naming that file with create_immed, and moves a bar *in the
# file* every frame. The proxy tells the compositor wl_shm.create_pool
# (SharedFile{/dev/shm/<name>}) and the compositor maps the file itself (Wine
# resolves the unix path), so:
#
#   * the compositor's frame dump shows the four quadrants the client drew;
#   * the bar moves between dumps -- only possible if the compositor reads
#     the shared file, since the proxy never touched the pixels;
#   * the proxy's stats say 0 copies and dmabuf_commits > 0, and its frame
#     trace has one `px.commit ... copied=0 zero_copy=1` per commit;
#   * the compositor counts a shared-file map and no arena slice.
#
# Scratch under $WC_DMABUF_SCRATCH (default /mnt/datasets/builds/kwin-gpu-scratch/e2e).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
LINUX_BIN="$ROOT/target/debug"
WIN_BIN="$ROOT/target/x86_64-pc-windows-gnu/debug"
SCRATCH=${WC_DMABUF_SCRATCH:-/mnt/datasets/builds/kwin-gpu-scratch/e2e}
SECS=${WC_DMABUF_SECS:-6}
W=320 H=200

pass=0; fail=0
ok()  { echo "  PASS  $*"; pass=$((pass + 1)); }
bad() { echo "  FAIL  $*"; fail=$((fail + 1)); }
note(){ echo "  ....  $*"; }

for t in wine Xvfb magick wayland-scanner cc pkg-config; do
  command -v "$t" >/dev/null || { echo "need $t (run inside nix develop)"; exit 2; }
done
[ -x "$LINUX_BIN/wc-proxy" ] || { echo "no $LINUX_BIN/wc-proxy"; exit 2; }
[ -f "$WIN_BIN/wc-compositor.exe" ] || { echo "no $WIN_BIN/wc-compositor.exe"; exit 2; }

# Any character device will do for the feedback's main device; a render node
# if there is one, so the line reads like the real thing.
NODE=/dev/null
for n in /dev/dri/renderD128 /dev/dri/renderD129; do [ -c "$n" ] && { NODE=$n; break; }; done

mkdir -p "$SCRATCH"
RUN=$(mktemp -d "$SCRATCH/run-XXXXXX")
# Its own prefix, under the scratch directory: the dev shell points
# WINEPREFIX at /tmp, which other harnesses share.
# Short, because the socket lives deep inside it and a unix socket path has
# 107 bytes.
export WINEPREFIX="${WC_DMABUF_WINEPREFIX:-$SCRATCH/wp}"
export WINEDEBUG="${WINEDEBUG:--all}"
# A fresh prefix takes a minute to boot, which is not the compositor's time
# to make its socket.
if [ ! -f "$WINEPREFIX/system.reg" ] || [ ! -d "$WINEPREFIX/drive_c/windows" ]; then
  note "booting a Wine prefix in $WINEPREFIX"
  wineboot -i >/dev/null 2>&1; wineserver -w
fi
XPID=""
cleanup() {
  [ -n "${PROXY:-}" ] && kill "$PROXY" 2>/dev/null
  [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null
  [ -n "$XPID" ] && kill "$XPID" 2>/dev/null
  wineserver -k 2>/dev/null
  rm -f /dev/shm/sv-dmabuf-test-* 2>/dev/null
}
trap cleanup EXIT

# The client.
CLI=$RUN/cli
mkdir -p "$CLI"
for x in xdg-shell linux-dmabuf-v1; do
  wayland-scanner client-header "$ROOT/crates/wc-proxy/protocols/$x.xml" "$CLI/$x-client-protocol.h" || exit 1
  wayland-scanner private-code "$ROOT/crates/wc-proxy/protocols/$x.xml" "$CLI/$x-protocol.c" || exit 1
done
# shellcheck disable=SC2046
cc -O1 -Wall -o "$CLI/wc-dmabuf-client" "$ROOT/scripts/wc-dmabuf-client.c" \
  "$CLI/xdg-shell-protocol.c" "$CLI/linux-dmabuf-v1-protocol.c" -I"$CLI" \
  $(pkg-config --cflags --libs wayland-client) || exit 1

wait_for() {
  local f=$1 re=$2 secs=$3 i=0
  while [ "$i" -lt $((secs * 20)) ]; do
    [ -f "$f" ] && grep -qE "$re" "$f" 2>/dev/null && return 0
    sleep 0.05; i=$((i + 1))
  done
  note "gave up after ${secs}s waiting for /$re/ in $(basename "$f")"
  return 1
}

DISP=$((100 + RANDOM % 400))
Xvfb ":$DISP" -screen 0 1280x1024x24 -nolisten tcp >"$RUN/xvfb.log" 2>&1 &
XPID=$!
export DISPLAY=":$DISP"
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.1; done

mkdir -p "$RUN/frames" "$RUN/rt" "$RUN/pools"
chmod 700 "$RUN/rt"
SOCK="wcd$$"
wine explorer "/desktop=wc-dmabuf-$$,1280x1024" \
  "$WIN_BIN/wc-compositor.exe" --socket "$SOCK" --timeout $((SECS + 40)) \
  --dump-frames "$(winepath -w "$RUN/frames" 2>/dev/null)" \
  --log "$(winepath -w "$RUN/srv.log" 2>/dev/null)" >"$RUN/loader.log" 2>&1 &
SRV=$!
NODE_SOCK=""
for _ in $(seq 1 600); do
  NODE_SOCK=$(find "$WINEPREFIX/drive_c" -type s -name "$SOCK" 2>/dev/null | head -1)
  [ -n "$NODE_SOCK" ] && break
  sleep 0.2
done
[ -n "$NODE_SOCK" ] || { bad "the compositor never made its socket"; tail "$RUN/loader.log"; exit 1; }

WC_FRAME_TRACE="$RUN/proxy-trace.txt" XDG_RUNTIME_DIR="$RUN/rt" "$LINUX_BIN/wc-proxy" \
  --upstream "$NODE_SOCK" --socket wl-proxy --runtime-dir "$RUN/rt" \
  --transport shared-file --pool-dir "$RUN/pools" \
  --peer-prefix "$(winepath -w "$RUN/pools" 2>/dev/null)\\" \
  --dmabuf-node "$NODE" --accept-timeout 30 --timeout $((SECS + 30)) >"$RUN/proxy.log" 2>&1 &
PROXY=$!
wait_for "$RUN/proxy.log" '^\[proxy\] listening on' 20 || { bad "proxy did not listen"; cat "$RUN/proxy.log"; exit 1; }

XDG_RUNTIME_DIR="$RUN/rt" WAYLAND_DISPLAY=wl-proxy timeout $((SECS + 20)) \
  "$CLI/wc-dmabuf-client" "$SECS" "$W" "$H" >"$RUN/cli.log" 2>&1
RC=$?
sleep 1
kill "$PROXY" 2>/dev/null; wait "$PROXY" 2>/dev/null; PROXY=""
kill "$SRV" 2>/dev/null; wait "$SRV" 2>/dev/null; SRV=""

echo "== wc-dmabuf: $RUN"
[ "$RC" = 0 ] && ok "the client ran ($(grep -m1 '^frames' "$RUN/cli.log"))" \
  || { bad "the client exited $RC"; sed 's/^/        /' "$RUN/cli.log"; }

dev=$(stat -c '%Hr:%Lr' "$NODE")
grep -q "^feedback main_device=$dev table=2 formats=0x34325241,0x34325258" "$RUN/cli.log" \
  && ok "feedback: main device $dev ($NODE), ARGB8888 and XRGB8888" \
  || bad "feedback: $(grep -m1 '^feedback' "$RUN/cli.log")"
grep -q "^buffer created" "$RUN/cli.log" && ok "create_immed made a buffer" || bad "no buffer"
grep -q "offered zwp_linux_dmabuf_v1 v4, main device $dev" "$RUN/proxy.log" \
  && ok "proxy: offered, main device $dev" || bad "proxy did not say it offered the global"
grep -q "dma-buf wl_buffer@.*SharedFile.*zero copy" "$RUN/proxy.log" \
  && ok "proxy: the buffer is a SharedFile pool on the client's file" \
  || bad "proxy: no dma-buf buffer line"

stats=$(grep -m1 "commits, " "$RUN/proxy.log")
commits=$(sed -n 's/.*dmabuf_commits=\([0-9]*\).*/\1/p' <<<"$stats")
copies=$(sed -n 's/^.*[^0-9]\([0-9][0-9]*\) copies,.*/\1/p' <<<"$stats")
note "proxy: $stats"
[ "${commits:-0}" -ge 10 ] && ok "dmabuf_commits=$commits" || bad "dmabuf_commits=${commits:-none}"
[ "${copies:-x}" = 0 ] && ok "the proxy copied nothing (0 copies)" || bad "copies=${copies:-?}"
zc=$(grep -c "px.commit .*copied=0 zero_copy=1" "$RUN/proxy-trace.txt" 2>/dev/null)
other=$(grep "px.commit" "$RUN/proxy-trace.txt" 2>/dev/null | grep -vc "zero_copy=1")
[ "${zc:-0}" -ge 10 ] && [ "${other:-0}" = 0 ] \
  && ok "frame trace: $zc px.commit copied=0 zero_copy=1, no other px.commit" \
  || bad "frame trace: $zc zero-copy commits, $other others"

maps=$(grep -o "mapped: [0-9]* section, [0-9]* shared-file, [0-9]* arena-slice" "$RUN/srv.log" | tail -1)
case "$maps" in
  "mapped: 0 section, "[1-9]*" shared-file, 0 arena-slice") ok "compositor: $maps" ;;
  *) bad "compositor: ${maps:-no map counts}"; grep -i "error\|fail" "$RUN/srv.log" | head -5 | sed 's/^/        /' ;;
esac

# The pixels, as the compositor saw them: the quadrants, and the bar moving.
mapfile -t dumps < <(find "$RUN/frames" -name '*.ppm' | sort)
note "${#dumps[@]} frames dumped"
if [ "${#dumps[@]}" -ge 2 ]; then
  last=${dumps[-1]}
  geom=$(magick identify -format "%wx%h" "$last" 2>/dev/null)
  [ "$geom" = "${W}x${H}" ] && ok "the last dump is ${W}x${H}" || bad "the last dump is $geom"
  px() { magick "$1" -format "%[pixel:p{$2,$3}]" info: 2>/dev/null; }
  q="$(px "$last" 40 150)|$(px "$last" 280 50)|$(px "$last" 40 50)|$(px "$last" 280 150)"
  # the last frame has the bar at rows 0..7: sample below it.
  [ "$q" = "srgb(0,0,255)|srgb(0,255,0)|srgb(255,0,0)|srgb(255,255,255)" ] \
    && ok "quadrants: blue, green, red, white where the client drew them" \
    || bad "quadrants: $q"
  [ "$(px "$last" 10 3)" = "srgb(0,0,0)" ] && ok "the bar is where the last frame put it" \
    || bad "bar pixel: $(px "$last" 10 3)"
  distinct=$(for f in "${dumps[@]}"; do md5sum <"$f"; done | sort -u | wc -l)
  [ "$distinct" -ge 5 ] && ok "$distinct distinct frames: the bar moved in the shared file and the compositor saw it" \
    || bad "only $distinct distinct frames"
else
  bad "fewer than two frames dumped"
fi

echo "== $pass passed, $fail failed"
[ "$fail" = 0 ]
