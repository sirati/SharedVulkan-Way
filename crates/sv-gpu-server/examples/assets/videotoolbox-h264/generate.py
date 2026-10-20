#!/usr/bin/env python3
"""Recreate the synthetic H.264 fixture with an ffmpeg/libx264 executable."""
from pathlib import Path
import subprocess
import argparse
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument("ffmpeg", nargs="?", default="ffmpeg")
parser.add_argument("--full-range", action="store_true")
args = parser.parse_args()
ffmpeg = args.ffmpeg
output = Path(__file__).with_name("full-range-64x50.h264" if args.full_range else "progressive-64x50.h264")
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
        "-c:v", "libx264", "-profile:v", "baseline", "-preset", "veryfast",
        "-qp", "12", "-threads", "1", "-x264-params",
        "keyint=4:min-keyint=4:scenecut=0:bframes=0:ref=1:aud=1:repeat-headers=1",
        *(["-color_range", "pc"] if args.full_range else []),
        "-f", "h264", "-y", str(output),
    ], check=True)
