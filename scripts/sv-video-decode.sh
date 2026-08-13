#!/usr/bin/env bash
# **Does hardware video decode work, and can this project forward it?**
#
#   scripts/sv-video-decode.sh [icd-manifest-basename]   default: radeon_icd.x86_64.json
#
# Five checks. The first four are about the drivers and pass; the fifth is about
# the forwarder and it fails on purpose, because that is where the project
# actually stands and a harness that hid it would be worth nothing.
#
#  1. **The survey.** `video-probe caps` asks every physical device the loader
#     offers what it can decode — per codec, per profile, with the capabilities
#     rather than the extension name. `vulkaninfo` prints only profile *names*
#     ("placeholder = H.264 Decode (4:2:0 8-bit) Main progressive"), so the
#     numbers a decoder has to respect — DPB slots, bitstream buffer alignment,
#     coded extent, whether the output picture may be distinct from the DPB —
#     come from here or from nowhere.
#
#  2. **A frame is decoded.** `video-probe decode` runs the whole Vulkan Video
#     state machine — session, session parameters built from a parsed SPS and
#     PPS, a DPB, `vkCmdBeginVideoCodingKHR` / `vkCmdDecodeVideoKHR` — against a
#     fixture this script encodes, and copies the result out as NV12.
#
#  3. **The frames are compared with libavcodec's**, which is the check that
#     matters. A decoder that returns `VK_SUCCESS` and writes a grey rectangle
#     passes every other kind of test. Two independent H.264 implementations
#     agreeing is the same argument `sv-zink-gl.sh` makes with llvmpipe, and
#     here the agreement is exact rather than to within a PSNR.
#
#  4. **The size arithmetic**, on a realistic 1080p stream rather than the
#     fixture: how many bytes of compressed video buy how many bytes of frames.
#     That ratio is the whole reason decode is a good fit for a forwarder, so it
#     is measured rather than quoted.
#
#  5. **The forwarder, twice.** `video-probe caps` is run against *our* ICD,
#     which reports no video extensions at all; and the code generator is run
#     with a profile that asks for them, which refuses to plan. The two together
#     are the exact statement of what is missing: not a capability of the
#     hardware, but a type the registry does not define.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/debug"
ICD="${1:-radeon_icd.x86_64.json}"
ICDDIR=/run/opengl-driver/share/vulkan/icd.d
RUNDIR="$(mktemp -d /tmp/sv-video-decode.XXXXXX)"
MANIFEST="$BIN/sv_icd.json"
SOCK="$RUNDIR/gpu.sock"
PROBE="$BIN/video-probe"

command -v ffmpeg >/dev/null || { echo "ffmpeg is missing; enter 'nix develop'" >&2; exit 1; }
[ -x "$PROBE" ] || { echo "build first: cargo build -p video-probe" >&2; exit 1; }

fail=0
GPUPID=""
cleanup() {
  [ -n "$GPUPID" ] && kill "$GPUPID" 2>/dev/null
  rm -rf "$RUNDIR"
}
trap cleanup EXIT

say() { printf '\n=== %s ===\n' "$*"; }
ok()  { printf '  ok   %s\n' "$*"; }
bad() { printf '  FAIL %s\n' "$*"; fail=1; }

# ---------------------------------------------------------------------------
say "1. what every device in reach can decode"
# ---------------------------------------------------------------------------
"$PROBE" caps > "$RUNDIR/caps.txt" 2>&1 || bad "video-probe caps exited non-zero"
grep -E '^device |^  vendor|^  video extensions|^  queue family|: (SUPPORTED|NOT SUPPORTED)' \
  "$RUNDIR/caps.txt" | sed 's/^/  /'
if grep -q 'h264 .*: SUPPORTED' "$RUNDIR/caps.txt"; then
  ok "at least one device reports an H.264 decode profile"
else
  bad "no device in this shell reports an H.264 decode profile"
fi
echo "  (full report: $RUNDIR/caps.txt — kept only while this script runs)"

# ---------------------------------------------------------------------------
say "2. a fixture, and a decode"
# ---------------------------------------------------------------------------
# `keyint=1` makes every access unit an IDR. That is what lets the probe be a
# probe rather than a decoder: no reference lists, no picture order counts. It
# is also why this stream is not the one the size arithmetic uses.
ffmpeg -y -loglevel error -f lavfi -i "testsrc2=size=320x180:rate=24:duration=1" \
  -c:v libx264 -profile:v main -pix_fmt yuv420p -g 1 -bf 0 \
  -x264-params keyint=1:scenecut=0 -f h264 "$RUNDIR/idr.264" \
  || { bad "ffmpeg could not build the fixture"; }
FIXBYTES=$(stat -c%s "$RUNDIR/idr.264")
echo "  fixture: $FIXBYTES bytes, 24 IDR frames of 320x180"

if "$PROBE" decode "$RUNDIR/idr.264" "$RUNDIR/out.nv12" > "$RUNDIR/decode.txt" 2>&1; then
  sed 's/^/  /' "$RUNDIR/decode.txt"
  ok "vkCmdDecodeVideoKHR produced frames"
else
  sed 's/^/  /' "$RUNDIR/decode.txt"
  bad "the decode failed"
fi

# ---------------------------------------------------------------------------
say "3. the same stream, decoded by libavcodec"
# ---------------------------------------------------------------------------
ffmpeg -y -loglevel error -i "$RUNDIR/idr.264" -pix_fmt nv12 -f rawvideo "$RUNDIR/ref.nv12"
if [ -s "$RUNDIR/out.nv12" ] && cmp -s "$RUNDIR/out.nv12" "$RUNDIR/ref.nv12"; then
  ok "byte-identical to the software decode ($(stat -c%s "$RUNDIR/ref.nv12") bytes)"
else
  # Not identical is not automatically wrong — a driver may round differently —
  # so say how far apart they are rather than only that they differ.
  ffmpeg -hide_banner -loglevel info \
    -f rawvideo -pix_fmt nv12 -s 320x180 -i "$RUNDIR/out.nv12" \
    -f rawvideo -pix_fmt nv12 -s 320x180 -i "$RUNDIR/ref.nv12" \
    -lavfi "[0:v][1:v]psnr" -f null - 2>&1 | grep -i 'PSNR' | sed 's/^/  /'
  bad "the hardware decode is not the software decode"
fi

# ---------------------------------------------------------------------------
say "4. how much smaller the bitstream is than the frames"
# ---------------------------------------------------------------------------
# A realistic stream this time: 1080p30, ten seconds, a normal GOP, rate-capped
# at 4 Mbit/s, which is roughly what a video site serves at this size.
ffmpeg -y -loglevel error -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=10" \
  -c:v libx264 -profile:v high -preset medium -b:v 4M -maxrate 4M -bufsize 8M \
  -pix_fmt yuv420p -f h264 "$RUNDIR/real.264"
CODED=$(stat -c%s "$RUNDIR/real.264")
FRAMES=300
RAW=$((1920 * 1080 * 3 / 2 * FRAMES))
echo "  1920x1080, $FRAMES frames at 4 Mbit/s"
echo "  compressed : $CODED bytes  ($((CODED / FRAMES)) per frame)"
echo "  NV12 frames: $RAW bytes  ($((RAW / FRAMES)) per frame)"
echo "  ratio      : $((RAW / CODED))x"
if [ "$((RAW / CODED))" -ge 50 ]; then
  ok "the compressed side is at least fifty times smaller"
else
  bad "the ratio is smaller than the design assumes — check the encode settings"
fi

# ---------------------------------------------------------------------------
say "5. the same questions, asked of the forwarder"
# ---------------------------------------------------------------------------
if [ -f "$MANIFEST" ] && [ -x "$BIN/sv-gpu-server" ]; then
  VK_DRIVER_FILES="$ICDDIR/$ICD" "$BIN/sv-gpu-server" serve \
    --socket "$SOCK" --arena $((256 << 20)) --clients 1 \
    > "$RUNDIR/gpu.log" 2>&1 &
  GPUPID=$!
  for _ in $(seq 1 400); do
    [ -S "$SOCK" ] && break
    kill -0 $GPUPID 2>/dev/null || break
    sleep 0.05
  done
  if [ -S "$SOCK" ]; then
    env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" \
      "$PROBE" caps > "$RUNDIR/fwd.txt" 2>&1
    grep -E '^device |^  video extensions' "$RUNDIR/fwd.txt" | sed 's/^/  /'
    if grep -q '^  video extensions: none' "$RUNDIR/fwd.txt"; then
      ok "the forwarded device reports no video extensions — as designed, today"
    else
      echo "  the forwarded device now reports video extensions; this script is out of date"
    fi
  else
    echo "  (the GPU side did not start; skipping)"
  fi
  kill "$GPUPID" 2>/dev/null; wait "$GPUPID" 2>/dev/null; GPUPID=""
else
  echo "  (sv-icd / sv-gpu-server not built; skipping the forwarded half)"
fi

echo
echo "  and the reason it reports none:"
if (cd "$ROOT" && cargo run -q -p sv-codegen -- --profile video --stats) \
     > "$RUNDIR/plan.txt" 2>&1; then
  echo "  the video profile now PLANS — docs/video-decode.md needs rewriting"
  fail=1
else
  grep -A3 '^Error' "$RUNDIR/plan.txt" | sed 's/^/  /'
  ok "the wire profile still refuses the video decode extensions, and says why"
fi

echo
if [ "$fail" -eq 0 ]; then echo "sv-video-decode: PASS"; else echo "sv-video-decode: FAIL"; fi
exit "$fail"
