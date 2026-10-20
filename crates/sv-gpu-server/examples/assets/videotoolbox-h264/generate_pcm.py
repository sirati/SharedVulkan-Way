#!/usr/bin/env python3
"""Emit original H.264 I_PCM/P-skip/B-skip fixtures and verify with FFmpeg.

This is a small fixture encoder, not a header patcher. Every NAL, macroblock,
and escaped RBSP is generated here. Expected output pixels are specified
independently of FFmpeg and compared byte for byte with its software output.
"""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess


class Bits:
    def __init__(self):
        self.bits = []

    def fixed(self, value, width):
        assert 0 <= value < 1 << width
        self.bits.extend((value >> bit) & 1 for bit in range(width - 1, -1, -1))

    def bit(self, value):
        self.bits.append(int(bool(value)))

    def ue(self, value):
        assert value >= 0
        width = (value + 1).bit_length()
        self.bits.extend([0] * (width - 1))
        self.fixed(value + 1, width)

    def se(self, value):
        self.ue(2 * abs(value) - (1 if value > 0 else 0))

    def align(self):
        while len(self.bits) % 8:
            self.bit(0)

    def raw(self, data):
        assert len(self.bits) % 8 == 0
        for value in data:
            self.fixed(value, 8)

    def nal(self, header):
        self.bit(1)
        self.align()
        output = bytearray([header])
        zeros = 0
        for at in range(0, len(self.bits), 8):
            value = sum(bit << (7 - index)
                        for index, bit in enumerate(self.bits[at:at + 8]))
            if zeros == 2 and value <= 3:
                output.append(3)
                zeros = 0
            output.append(value)
            zeros = zeros + 1 if value == 0 else 0
        return b"\x00\x00\x00\x01" + output


def parameters(poc, gaps=False, profile=66):
    sps = Bits()
    sps.fixed(profile, 8)
    sps.fixed(0, 8)
    sps.fixed(10, 8)
    sps.ue(0)  # SPS id
    sps.ue(0)  # four frame_num bits
    sps.ue(poc)
    if poc == 0:
        sps.ue(0)  # four POC LSB bits
    if poc == 1:
        sps.bit(1)  # delta_pic_order_always_zero_flag
        sps.se(0)  # offset_for_non_ref_pic
        sps.se(0)  # offset_for_top_to_bottom_field
        sps.ue(1)
        sps.se(2)  # one-frame POC cycle
    sps.ue(4)  # max_num_ref_frames
    sps.bit(gaps)
    sps.ue(3)  # 64 coded pixels wide
    sps.ue(3)  # 64 coded pixels high
    sps.bit(1)  # frame_mbs_only_flag
    sps.bit(1)  # direct_8x8_inference_flag
    sps.bit(0)  # no display crop
    sps.bit(0)  # no VUI
    pps = Bits()
    pps.ue(0)
    pps.ue(0)
    pps.bit(0)  # CAVLC
    pps.bit(0)
    pps.ue(0)  # no FMO
    pps.ue(0)  # one active L0 reference
    pps.ue(0)  # one active L1 reference
    pps.bit(0)
    pps.fixed(0, 2)
    pps.se(0)
    pps.se(0)
    pps.se(0)
    pps.bit(1)  # deblocking_filter_control_present_flag
    pps.bit(0)
    pps.bit(0)
    return sps.nal(0x67) + pps.nal(0x68)


def picture(poc_type, frame_num, y, *, kind=2, idr=False,
            long_term=None, mmco=(), poc_lsb=None):
    """kind 2 uses I_PCM; kind 0/1 uses a whole-frame P/B skip run."""
    bits = Bits()
    bits.ue(0)
    bits.ue(kind)
    bits.ue(0)
    bits.fixed(frame_num, 4)
    if idr:
        bits.ue(0)
    if poc_type == 0:
        bits.fixed(frame_num * 2 if poc_lsb is None else poc_lsb, 4)
    if kind == 1:
        bits.bit(1)  # spatial direct prediction
    if kind in (0, 1):
        bits.bit(0)  # no active-count override
        bits.bit(long_term is not None)
        if long_term is not None:
            bits.ue(2)
            bits.ue(long_term)
            bits.ue(3)
        if kind == 1:
            bits.bit(0)  # no L1 modification
    if idr:
        bits.bit(0)
        bits.bit(0)
    elif kind != 1:
        bits.bit(bool(mmco))
        if mmco:
            for operation, first, second in mmco:
                bits.ue(operation)
                if operation in (1, 2, 3, 4, 6):
                    bits.ue(first)
                if operation == 3:
                    bits.ue(second)
            bits.ue(0)
    bits.se(0)  # slice_qp_delta
    bits.ue(1)  # disable deblocking
    if kind in (0, 1):
        bits.ue(16)
    else:
        for _ in range(16):
            bits.ue(25)  # I_PCM
            bits.align()
            bits.raw(bytes([y]) * 256 + bytes([80]) * 64 + bytes([180]) * 64)
    return bits.nal(0x65 if idr else 0x01 if kind == 1 else 0x41)


def raw_frame(y):
    return bytes([y]) * 4096 + bytes([80]) * 1024 + bytes([180]) * 1024


def cases():
    output = []
    for poc in (0, 1, 2):
        groups = {
            "marking": [
                dict(frame_num=0, y=40, idr=True),
                dict(frame_num=1, y=80, mmco=((4, 2, 0), (3, 0, 1))),
                dict(frame_num=2, y=40, kind=0, long_term=1),
                dict(frame_num=3, y=120, mmco=((5, 0, 0),)),
                dict(frame_num=1, y=120, kind=0),
                dict(frame_num=2, y=160, mmco=((4, 1, 0), (6, 0, 0))),
                dict(frame_num=3, y=160, kind=0, long_term=0),
            ],
            "gap": [dict(frame_num=0, y=40, idr=True),
                    dict(frame_num=3, y=80), dict(frame_num=4, y=80, kind=0)],
        }
        for name, frames in groups.items():
            data = parameters(poc, gaps=name == "gap")
            data += b"".join(picture(poc, **frame) for frame in frames)
            output.append((f"original-pcm-poc{poc}-{name}-64x64.h264", data,
                           b"".join(raw_frame(frame["y"]) for frame in frames), None))
        source_frames = [dict(frame_num=0, y=40, idr=True),
                         dict(frame_num=1, y=40, kind=0),
                         dict(frame_num=2, y=40, kind=0),
                         dict(frame_num=3, y=120)]
        original_parameters = parameters(poc)
        source_nals = [picture(poc, **frame) for frame in source_frames]
        source = original_parameters + b"".join(source_nals)
        source_name = f"original-pcm-poc{poc}-bootstrap-source-64x64.h264"
        output.append((source_name, source,
                       b"".join(raw_frame(frame["y"]) for frame in source_frames), None))
        final_start = len(original_parameters) + sum(map(len, source_nals[:-1]))
        # Select the exact original SPS/PPS bytes and final complete original
        # VCL NAL. No slice/RBSP byte is rewritten for fresh decoder submission.
        bootstrap = source[:len(original_parameters)] + source[final_start:]
        output.append((f"original-pcm-poc{poc}-bootstrap-64x64.h264", bootstrap,
                       raw_frame(120), dict(file=source_name,
                       parameter_range=[0, len(original_parameters)],
                       original_vcl_range=[final_start, len(source)])))
    frames = [dict(kind=2, frame_num=0, poc_lsb=0, y=40, idr=True),
              dict(kind=2, frame_num=1, poc_lsb=4, y=120),
              dict(kind=1, frame_num=2, poc_lsb=2, y=80),
              dict(kind=0, frame_num=2, poc_lsb=6, y=120)]
    data = parameters(0, profile=77)
    data += b"".join(picture(0, **frame) for frame in frames)
    expected = b"".join(raw_frame(frame["y"])
                        for frame in sorted(frames, key=lambda frame: frame["poc_lsb"]))
    output.append(("original-pcm-b-skip-64x64.h264", data, expected, None))
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("ffmpeg", nargs="?", default="ffmpeg")
    parser.add_argument("--output-dir", type=Path, default=Path(__file__).parent)
    args = parser.parse_args()
    args.output_dir.mkdir(parents=True, exist_ok=True)
    version = subprocess.run([args.ffmpeg, "-version"], check=True,
                             capture_output=True, text=True).stdout.splitlines()[0]
    records = []
    for name, data, expected, original_source in cases():
        path = args.output_dir / name
        path.write_bytes(data)
        decoded = subprocess.run([
            args.ffmpeg, "-nostdin", "-hide_banner", "-loglevel", "error",
            "-err_detect", "explode", "-i", str(path), "-fps_mode", "passthrough",
            "-f", "rawvideo", "-pix_fmt", "yuv420p", "-",
        ], check=True, capture_output=True)
        if decoded.stdout != expected or decoded.stderr:
            raise RuntimeError(f"independent software pixel verification failed for {name}: "
                               f"{decoded.stderr.decode(errors='replace')}")
        record = dict(file=name, size=len(data), sha256=hashlib.sha256(data).hexdigest(),
                      frames=len(expected) // 6144, decoded_bytes=len(expected),
                      decoded_sha256=hashlib.sha256(expected).hexdigest())
        if original_source is not None:
            record["original_source"] = original_source
        records.append(record)
    evidence = dict(generator_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                    ffmpeg=version, verification="-err_detect explode; exact YUV420P bytes; empty error stderr",
                    fixtures=records)
    (args.output_dir / "pcm-provenance.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))


if __name__ == "__main__":
    main()
