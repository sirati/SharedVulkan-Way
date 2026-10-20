#!/usr/bin/env bash
# **Does hardware video decode work, and can this project forward it?**
#
#   scripts/sv-video-decode.sh [icd-manifest-basename]   default: radeon_icd.x86_64.json
#
# Seven checks. The first four are about the drivers. The last three are about
# the forwarder: the wire profile plans, the forwarded device answers the video
# capability queries with the driver's own numbers and decodes the fixture
# byte-identically to libavcodec (docs/video-decode.md §7), and ffmpeg's own
# Vulkan decoder runs through it for H.264, H.265, H.265 Main10, AV1 and VP9.
#
# The forwarded half needs the binaries built; the default `full` profile
# carries the video extensions, so no profile switch is needed any more:
#
#   cargo build -p sv-icd -p sv-gpu-server -p video-probe
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
# The driver the GPU side opens. The system Mesa in /run/opengl-driver is built
# against a newer glibc than the dev shell's binaries and no longer loads into
# them (docs/video-decode.md §7), so the default is the dev shell's own Mesa,
# found on XDG_DATA_DIRS; SV_VIDEO_ICDDIR overrides.
ICDDIR=${SV_VIDEO_ICDDIR:-}
if [ -z "$ICDDIR" ]; then
  IFS=: read -ra _dirs <<<"${XDG_DATA_DIRS:-}"
  for d in "${_dirs[@]}"; do
    [ -f "$d/vulkan/icd.d/$ICD" ] && { ICDDIR="$d/vulkan/icd.d"; break; }
  done
  ICDDIR=${ICDDIR:-/run/opengl-driver/share/vulkan/icd.d}
fi
RUNDIR="$(mktemp -d "${TMPDIR:-/tmp}/sv-video-decode.XXXXXX")"
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
# The profile is a build input: an ICD built for `zink` has no video opcodes and
# reports no video extensions, correctly. The default `full` profile has them.
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
    bad "the forwarded device reports no video extensions — rebuild sv-icd and sv-gpu-server"
  else
    ok "the forwarded device advertises the profile's video extensions"

    # The H.264 block of each report: four profiles, their coded extents, DPB
    # slot counts, bitstream alignments, decode flags, `maxLevelIdc` — which is
    # a `StdVideoH264LevelIdc`, the type that used to stop the planner — and
    # every `vkGetPhysicalDeviceVideoFormatPropertiesKHR` row.
    #
    # Every codec block from H.264 on — H.265 and AV1 too, since the default
    # `full` profile carries their extensions and the GPU side serves their
    # decode family (docs/video-decode.md §7). The ICD's own `[icd]` summary
    # line on stderr is the only line the direct run cannot have.
    sed -n '/^  h264/,$p' "$RUNDIR/direct.txt" | grep -v '^\[icd\]' > "$RUNDIR/direct.h264"
    sed -n '/^  h264/,$p' "$RUNDIR/fwd.txt"    | grep -v '^\[icd\]' > "$RUNDIR/fwd.h264"
    LINES=$(wc -l < "$RUNDIR/direct.h264")
    if [ "$LINES" -gt 20 ] && cmp -s "$RUNDIR/direct.h264" "$RUNDIR/fwd.h264"; then
      ok "$LINES lines of H.264, H.265 and AV1 capabilities crossed the boundary byte-identically"
      echo "     sha256 $(sha256sum < "$RUNDIR/direct.h264" | cut -c1-16)…"
    else
      diff "$RUNDIR/direct.h264" "$RUNDIR/fwd.h264" | head -20 | sed 's/^/  /'
      bad "the forwarded capabilities are not the driver's capabilities"
    fi

    # And the decode. It stopped here at "no queue family with VIDEO_DECODE"
    # until docs/video-decode.md §7, when the GPU side began serving its decode
    # family; the check is now the same one check 3 makes of the driver: the
    # frames are the frames.
    if forwarded "$RUNDIR/fwddec.txt" "$PROBE" decode "$RUNDIR/idr.264" "$RUNDIR/fwd.nv12"; then
      grep -E '^(device|memory|decoded|decode submit)' "$RUNDIR/fwddec.txt" | sed 's/^/  /'
      if cmp -s "$RUNDIR/fwd.nv12" "$RUNDIR/ref.nv12"; then
        ok "decoded through the forwarder, byte-identical to libavcodec ($(sha256sum < "$RUNDIR/fwd.nv12" | cut -c1-16)…)"
      else
        bad "the forwarded decode ran but its frames are not libavcodec's"
      fi
    else
      sed 's/^/  /' "$RUNDIR/fwddec.txt" | tail -15
      bad "the forwarded decode failed"
    fi
  fi
else
  echo "  (sv-icd / sv-gpu-server not built; skipping the forwarded half)"
fi

# ---------------------------------------------------------------------------
say "7. ffmpeg's own Vulkan decoder, through the forwarder, every codec"
# ---------------------------------------------------------------------------
# `video-probe` is IDR-only H.264 by design. ffmpeg's `-hwaccel vulkan` is a
# real decoder — reference lists, B-frames, H.265, AV1, VP9 — and it is what
# Firefox 153+ decodes with underneath. Each stream is decoded by it through the
# forwarder, every frame downloaded as NV12 (P010 for Main10), and compared with
# libavcodec's software decode of the same stream.
if [ -f "$MANIFEST" ] && [ -x "$BIN/sv-gpu-server" ]; then
  S2="testsrc2=size=1280x720:rate=30:duration=2"
  ffmpeg -y -loglevel error -f lavfi -i "testsrc2=size=1920x1080:rate=30:duration=4" \
    -c:v libx264 -profile:v high -preset medium -b:v 4M -pix_fmt yuv420p -f h264 "$RUNDIR/s.264"
  ffmpeg -y -loglevel error -f lavfi -i "$S2" -c:v libx265 -preset fast -b:v 2M \
    -pix_fmt yuv420p -x265-params log-level=error -f hevc "$RUNDIR/s.265"
  ffmpeg -y -loglevel error -f lavfi -i "$S2" -c:v libx265 -preset fast -b:v 2M \
    -pix_fmt yuv420p10le -x265-params log-level=error -f hevc "$RUNDIR/s10.265"
  ffmpeg -y -loglevel error -f lavfi -i "$S2" -c:v libsvtav1 -preset 10 -b:v 2M \
    -pix_fmt yuv420p -svtav1-params enable-stat-report=0 "$RUNDIR/s.ivf" 2>/dev/null
  ffmpeg -y -loglevel error -f lavfi -i "$S2" -c:v libvpx-vp9 -deadline realtime \
    -cpu-used 8 -b:v 2M -pix_fmt yuv420p "$RUNDIR/s.webm"
  hwdec() {
    ffmpeg -hide_banner -loglevel error -init_hw_device vulkan=vk:0 -filter_hw_device vk \
      -hwaccel vulkan -hwaccel_device vk -hwaccel_output_format vulkan -i "$1" \
      -vf "hwdownload,format=$2" -f rawvideo - 2>"$RUNDIR/ffhw.err" | sha256sum | cut -c1-16 > "$RUNDIR/ffhw.sha"
  }
  for spec in s.264:nv12:H.264 s.265:nv12:H.265 s10.265:p010le:H.265-Main10 s.ivf:nv12:AV1 s.webm:nv12:VP9; do
    IFS=: read -r file fmt name <<<"$spec"
    sw=$(ffmpeg -hide_banner -loglevel error -i "$RUNDIR/$file" -pix_fmt "$fmt" -f rawvideo - | sha256sum | cut -c1-16)
    rm -f "$RUNDIR/ffhw.sha"; forwarded "$RUNDIR/ffhw.txt" bash -c "$(declare -f hwdec); RUNDIR=$RUNDIR hwdec $RUNDIR/$file $fmt" || true
    hw=$(cat "$RUNDIR/ffhw.sha" 2>/dev/null)
    if [ "$hw" = "$sw" ]; then
      ok "$name through the forwarder: $hw… = libavcodec"
    else
      head -3 "$RUNDIR/ffhw.err" | sed 's/^/  /'
      bad "$name through the forwarder: '$hw' against libavcodec's $sw"
    fi
  done
fi

echo
if [ "$fail" -eq 0 ]; then echo "sv-video-decode: PASS"; else echo "sv-video-decode: FAIL"; fi
exit "$fail"
