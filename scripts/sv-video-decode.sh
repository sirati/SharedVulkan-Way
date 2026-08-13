#!/usr/bin/env bash
# **Does hardware video decode work, and can this project forward it?**
#
#   scripts/sv-video-decode.sh [icd-manifest-basename]   default: radeon_icd.x86_64.json
#
# Six checks. The first four are about the drivers. The last two are about the
# forwarder, and they are the half that moved: the wire profile that used to
# refuse to plan now plans, and the forwarded device answers the video
# capability queries with the driver's own numbers — but the decode itself still
# does not run, and check 6 says so rather than skipping it.
#
# The forwarded half needs binaries built for the video profile:
#
#   SV_PROTO_PROFILE=video cargo build -p sv-icd -p sv-gpu-server -p video-probe
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
say "5. the wire profile plans"
# ---------------------------------------------------------------------------
# It did not, until the generator was given `video.xml` — Khronos's own
# machine-readable definition of the `StdVideo*` types, which ships beside
# `vk.xml` and is what the `vk_video/*.h` headers are generated from. See
# `vk_registry::video_registry_path`.
if (cd "$ROOT" && cargo run -q -p sv-codegen -- --profile video --stats) \
     > "$RUNDIR/plan.txt" 2>&1; then
  grep -E '^(profile|protocol_hash|commands|structs)' "$RUNDIR/plan.txt" | sed 's/^/  /'
  ok "the wire profile carries the video decode extensions"
else
  grep -A3 '^Error' "$RUNDIR/plan.txt" | sed 's/^/  /'
  bad "the video profile no longer plans"
fi

# ---------------------------------------------------------------------------
say "6. the same questions, asked of the forwarder"
# ---------------------------------------------------------------------------
# The binaries have to have been built with `SV_PROTO_PROFILE=video`, because the
# profile is a build input: an ICD built for `zink` has no video opcodes and
# reports no video extensions, correctly.
#
# What is compared is not "did it answer" but "did it answer the same thing".
# The capability numbers are the ones `docs/video-decode.md` §1 says no driver
# reports believably, so a forwarder that quietly rounded one would be
# indistinguishable from a driver that meant it — unless the two are put side by
# side, which is what this does.
# One GPU side per client, because `sv-gpu-server --clients N` waits for all N
# to attach before it serves any of them: a second run against a `--clients 1`
# server gets a closed socket and the ICD refuses to load, which looks exactly
# like the failure this script is about to claim and is not.
forwarded() {
  local out="$1"; shift
  rm -f "$SOCK"
  VK_DRIVER_FILES="$ICDDIR/$ICD" "$BIN/sv-gpu-server" serve \
    --socket "$SOCK" --arena $((256 << 20)) --clients 1 \
    > "$RUNDIR/gpu.log" 2>&1 &
  GPUPID=$!
  for _ in $(seq 1 400); do
    [ -S "$SOCK" ] && break
    kill -0 $GPUPID 2>/dev/null || break
    sleep 0.05
  done
  local rc=1
  if [ -S "$SOCK" ]; then
    env VK_DRIVER_FILES="$MANIFEST" SV_SOCKET="$SOCK" "$@" > "$out" 2>&1
    rc=$?
  else
    echo "the GPU side did not start" > "$out"
  fi
  kill "$GPUPID" 2>/dev/null; wait "$GPUPID" 2>/dev/null; GPUPID=""
  return $rc
}

if [ -f "$MANIFEST" ] && [ -x "$BIN/sv-gpu-server" ]; then
  # Direct, against the *same* ICD the GPU side will load, so the comparison is
  # transport against no transport rather than one driver against another. The
  # dev shell carries two RADVs and they answer differently (§1).
  VK_DRIVER_FILES="$ICDDIR/$ICD" "$PROBE" caps > "$RUNDIR/direct.txt" 2>&1

  forwarded "$RUNDIR/fwd.txt" "$PROBE" caps
  grep -E '^device |^  video extensions' "$RUNDIR/fwd.txt" | sed 's/^/  /'
  if ! grep -q '^  video extensions: VK_KHR_video' "$RUNDIR/fwd.txt"; then
    sed 's/^/  /' "$RUNDIR/fwd.txt" | head -8
    bad "the forwarded device reports no video extensions — rebuild with SV_PROTO_PROFILE=video"
  else
    ok "the forwarded device advertises the profile's video extensions"

    # The H.264 block of each report: four profiles, their coded extents, DPB
    # slot counts, bitstream alignments, decode flags, `maxLevelIdc` — which is
    # a `StdVideoH264LevelIdc`, the type that used to stop the planner — and
    # every `vkGetPhysicalDeviceVideoFormatPropertiesKHR` row.
    #
    # H.265 and AV1 are in the direct report and not in the forwarded one, on
    # purpose: the wire profile carries `VK_KHR_video_decode_h264` and neither of
    # the others, and §0.4 item 8 intersects the advertised list with the
    # driver's. So the comparison is over the H.264 block, which both have.
    sed -n '/^  h264/,/^  h265/p' "$RUNDIR/direct.txt" | grep -v '^  h265' > "$RUNDIR/direct.h264"
    sed -n '/^  h264/,/^  h265/p' "$RUNDIR/fwd.txt"    | grep -v '^  h265' > "$RUNDIR/fwd.h264"
    LINES=$(wc -l < "$RUNDIR/direct.h264")
    if [ "$LINES" -gt 20 ] && cmp -s "$RUNDIR/direct.h264" "$RUNDIR/fwd.h264"; then
      ok "$LINES lines of H.264 capabilities crossed the boundary byte-identically"
      echo "     sha256 $(sha256sum < "$RUNDIR/direct.h264" | cut -c1-16)…"
    else
      diff "$RUNDIR/direct.h264" "$RUNDIR/fwd.h264" | head -20 | sed 's/^/  /'
      bad "the forwarded capabilities are not the driver's capabilities"
    fi

    # And the decode, which does not run. The wall is no longer a type; it is
    # the one synthesised queue family of §0.4 item 7, and it is named here
    # rather than skipped, because "the decode has not been forwarded" is the
    # finding.
    if forwarded "$RUNDIR/fwddec.txt" "$PROBE" decode "$RUNDIR/idr.264" "$RUNDIR/fwd.nv12"; then
      echo "  the forwarded decode SUCCEEDED — docs/video-decode.md §6 needs rewriting"
      fail=1
    else
      sed 's/^/  /' "$RUNDIR/fwddec.txt"
      if grep -q 'no queue family with VIDEO_DECODE' "$RUNDIR/fwddec.txt"; then
        ok "the forwarded decode stops where the design says it does: no VIDEO_DECODE family"
      else
        bad "the forwarded decode failed somewhere other than the queue family"
      fi
    fi
  fi
else
  echo "  (sv-icd / sv-gpu-server not built; skipping the forwarded half)"
fi

echo
if [ "$fail" -eq 0 ]; then echo "sv-video-decode: PASS"; else echo "sv-video-decode: FAIL"; fi
exit "$fail"
