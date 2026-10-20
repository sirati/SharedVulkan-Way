#!/usr/bin/env python3
"""Recreate the synthetic H.264 fixture with an ffmpeg/libx264 executable."""
from pathlib import Path
import subprocess
import argparse
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument("ffmpeg", nargs="?", default="ffmpeg")
parser.add_argument("--full-range", action="store_true")
parser.add_argument("--intra-bootstrap", action="store_true")
args = parser.parse_args()
ffmpeg = args.ffmpeg
if args.intra_bootstrap and args.full_range:
    parser.error("intra bootstrap fixture is video-range only")
output = Path(__file__).with_name("full-range-64x50.h264" if args.full_range else "progressive-64x50.h264")
if args.intra_bootstrap:
    output = output.with_name("non-idr-intra-64x50.h264")
raw = bytearray()
for frame in range(8):
    raw.extend(40 + frame * 12 + (x // 16) * 8 + (y // 10) * 6
               for y in range(50) for x in range(64))
    raw.extend([64 + frame * 5] * (32 * 25))
    raw.extend([192 - frame * 5] * (32 * 25))
with tempfile.TemporaryDirectory(prefix="sv-vt-fixture-") as directory:
    source = Path(directory) / "pattern.yuv"
    source.write_bytes(raw)
    subprocess.run([
        ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "info",
        "-f", "rawvideo", "-pixel_format", "yuv420p", "-video_size", "64x50",
        *(["-color_range", "pc"] if args.full_range else []),
        "-framerate", "25", "-i", str(source), "-frames:v", "8", "-an",
        "-c:v", "libx264", "-profile:v", "main" if args.intra_bootstrap else "baseline", "-preset", "veryfast",
        "-qp", "12", "-threads", "1", "-x264-params",
        "keyint=4:min-keyint=4:scenecut=0:bframes=2:open-gop=1:ref=1:aud=1:repeat-headers=1" if args.intra_bootstrap else "keyint=4:min-keyint=4:scenecut=0:bframes=0:ref=1:aud=1:repeat-headers=1",
        *(["-color_range", "pc"] if args.full_range else []),
        "-f", "h264", "-y", str(output),
    ], check=True)

if args.intra_bootstrap:
    # Keep the original SPS/PPS and original second I picture, which x264's
    # open GOP encodes as NAL type1. Do not rewrite its compressed slice bytes.
    import re
    stream = output.read_bytes()
    starts = list(re.finditer(b"\x00\x00(?:\x00)?\x01", stream))
    nals = [stream[m.end(): starts[i+1].start() if i+1<len(starts) else len(stream)]
            for i, m in enumerate(starts)]
    def ue(bits):
        zeros = 0
        while next(bits) == 0:
            zeros += 1
        return (1 << zeros)-1 + sum(next(bits) << (zeros-i-1) for i in range(zeros))
    sps = next(n for n in nals if n[0] & 31 == 7)
    pps = next(n for n in nals if n[0] & 31 == 8)
    intra = []
    for nal in nals:
        if nal[0] & 31 == 1:
            rbsp = nal[1:].replace(b"\x00\x00\x03", b"\x00\x00")
            bits = iter((byte >> bit) & 1 for byte in rbsp for bit in range(7, -1, -1))
            ue(bits)
            if ue(bits) % 5 == 2:
                intra.append(nal)
    assert len(intra) == 1
    output.write_bytes(b"".join(b"\x00\x00\x00\x01" + n for n in (sps, pps, intra[0])))
