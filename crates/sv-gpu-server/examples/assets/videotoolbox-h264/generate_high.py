#!/usr/bin/env python3
"""Generate original High-profile payloads, never patch existing NAL headers."""
from pathlib import Path
import argparse
import hashlib
import json
import subprocess
from generate_pcm import Bits, picture, raw_frame

ROOT = Path(__file__).resolve().parent


def parameters():
    s = Bits()
    s.fixed(100, 8); s.fixed(0, 8); s.fixed(10, 8); s.ue(0)
    s.ue(1); s.ue(0); s.ue(0); s.bit(0); s.bit(0)  # fresh High syntax
    s.ue(0); s.ue(0); s.ue(0); s.ue(1); s.bit(0)
    s.ue(3); s.ue(3); s.bit(1); s.bit(1); s.bit(0); s.bit(0)
    p = Bits()
    p.ue(0); p.ue(0); p.bit(0); p.bit(0); p.ue(0)
    p.ue(0); p.ue(0); p.bit(0); p.fixed(0, 2)
    p.se(0); p.se(0); p.se(0); p.bit(1); p.bit(0); p.bit(0)
    return s.nal(0x67) + p.nal(0x68)


def nals(data):
    starts = []
    at = 0
    while at + 3 < len(data):
        size = 4 if data[at:at+4] == b'\0\0\0\1' else 3 if data[at:at+3] == b'\0\0\1' else 0
        if size:
            starts.append((at, at + size)); at += size
        else:
            at += 1
    return [(start, payload, starts[i+1][0] if i+1 < len(starts) else len(data))
            for i, (start, payload) in enumerate(starts)]


def sha(data):
    return hashlib.sha256(data).hexdigest()


def sps_metadata(nal):
    """Independent syntax inventory and StdVideo-canonical diagnostic hash.

    This does not change either complete original decode input stream.
    """
    rbsp = bytearray()
    zeros = 0
    for value in nal[1:]:
        if zeros == 2 and value == 3:
            zeros = 0
            continue
        rbsp.append(value)
        zeros = zeros + 1 if value == 0 else 0
    source = [(value >> shift) & 1 for value in rbsp for shift in range(7, -1, -1)]
    at = 0
    fields = {}
    def read(n):
        nonlocal at
        value = 0
        for _ in range(n):
            value = value * 2 + source[at]
            at += 1
        return value
    def ue():
        n = 0
        while read(1) == 0:
            n += 1
        return (1 << n) - 1 + read(n)
    fields['profile_idc'] = read(8)
    fields['constraint_and_reserved_bits'] = read(8)
    fields['level_idc'] = read(8)
    fields['sps_id'] = ue()
    assert fields['profile_idc'] == 100
    fields['chroma_format_idc'] = ue()
    fields['bit_depth_luma_minus8'] = ue()
    fields['bit_depth_chroma_minus8'] = ue()
    fields['qpprime_y_zero_transform_bypass_flag'] = read(1)
    assert read(1) == 0  # no scaling matrices
    fields['log2_max_frame_num_minus4'] = ue()
    fields['pic_order_cnt_type'] = ue()
    if fields['pic_order_cnt_type'] == 0:
        fields['log2_max_pic_order_cnt_lsb_minus4'] = ue()
    else:
        assert fields['pic_order_cnt_type'] == 2
    fields['max_num_ref_frames'] = ue()
    fields['gaps_allowed'] = read(1)
    fields['pic_width_in_mbs_minus1'] = ue()
    fields['pic_height_in_map_units_minus1'] = ue()
    assert read(1) == 1  # progressive
    fields['direct_8x8_inference_flag'] = read(1)
    fields['frame_cropping_flag'] = read(1)
    if fields['frame_cropping_flag']:
        fields['crop'] = [ue() for _ in range(4)]
    fields['vui_present'] = read(1)
    bounds = None
    if fields['vui_present']:
        if read(1):
            fields['aspect_ratio_idc'] = read(8)
            if fields['aspect_ratio_idc'] == 255:
                fields['sar'] = [read(16), read(16)]
        if read(1):
            fields['overscan_appropriate_flag'] = read(1)
        if read(1):
            fields['video_format'] = read(3)
            fields['video_full_range_flag'] = read(1)
            if read(1):
                fields['colour'] = [read(8) for _ in range(3)]
        if read(1):
            fields['chroma_sample_loc'] = [ue(), ue()]
        if read(1):
            fields['timing'] = [read(32), read(32), read(1)]
        assert read(1) == 0 and read(1) == 0  # no HRD
        assert read(1) == 0  # no pic_struct
        fields['bitstream_restriction_flag'] = read(1)
        if fields['bitstream_restriction_flag']:
            begin = at
            fields['std_omitted_vui_bounds'] = [read(1), ue(), ue(), ue(), ue()]
            bounds = (begin, at)
            fields['max_num_reorder_frames'] = ue()
            fields['max_dec_frame_buffering'] = ue()
    assert source[at] == 1 and not any(source[at+1:])
    canonical = Bits()
    canonical.bits = source[:at]
    if bounds:
        replacement = Bits()
        replacement.bit(1)
        for value in (0, 0, 16, 16):
            replacement.ue(value)
        begin, end = bounds
        canonical.bits = source[:begin] + replacement.bits + source[end:at]
    reconstructed = canonical.nal(0x67)[4:]
    fields['std_canonical_sps_hex'] = reconstructed.hex()
    fields['std_canonical_sps_sha256'] = sha(reconstructed)
    fields['original_sps_sha256'] = sha(nal)
    fields['canonical_difference_scope'] = 'only StdVideo-omitted VUI restriction bounds' if reconstructed != nal else 'byte-exact'
    return fields


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--ffmpeg', required=True)
    args = parser.parse_args()
    original = raw_frame(40) + raw_frame(120) + raw_frame(120)
    flags = 'keyint=250:min-keyint=250:scenecut=0:bframes=0:ref=1:qp=12:threads=1:lookahead-threads=1:8x8dct=1:cabac=1:aud=1:repeat-headers=1:force-cfr=1:slices=1:weightp=0'
    command = [args.ffmpeg, '-hide_banner', '-loglevel', 'info', '-f', 'rawvideo', '-pixel_format', 'yuv420p',
               '-video_size', '64x64', '-framerate', '25', '-i', 'pipe:0', '-frames:v', '3', '-an',
               '-c:v', 'libx264', '-profile:v', 'high', '-level:v', '1.0', '-preset', 'medium',
               '-x264-params', flags, '-f', 'h264', 'pipe:1']
    encoded = subprocess.run(command, input=original, capture_output=True, check=True)
    (ROOT / 'high-x264-encoder.log').write_bytes(encoded.stderr)
    cases = [('cavlc-pcm', parameters() + picture(0, 0, 40, idr=True) + picture(0, 1, 120)),
             ('cabac', encoded.stdout)]
    result = {'generator': 'generate_high.py', 'input_sha256': sha(original), 'input_frame_y': [40, 120, 120],
              'input_cb': 80, 'input_cr': 180, 'encoder_command': command,
              'ffmpeg_version': subprocess.run([args.ffmpeg, '-version'], capture_output=True, check=True).stdout.decode(),
              'cases': []}
    for name, data in cases:
        filename = f'original-high-{name}-64x64.h264'
        decode_command = [args.ffmpeg, '-hide_banner', '-loglevel', 'error', '-err_detect', 'explode',
                          '-f', 'h264', '-i', 'pipe:0', '-pix_fmt', 'yuv420p', '-f', 'rawvideo', 'pipe:1']
        decoded = subprocess.run(decode_command, input=data, capture_output=True, check=True)
        assert not decoded.stderr, decoded.stderr
        count = 2 if name == 'cavlc-pcm' else 3
        assert len(decoded.stdout) == count * 6144
        if name == 'cavlc-pcm':
            assert decoded.stdout == original[:2*6144]
        (ROOT / filename).write_bytes(data)
        oracle = f'original-high-{name}-64x64.yuv'
        (ROOT / oracle).write_bytes(decoded.stdout)
        headers, aus = [], []
        for start, payload, end in nals(data):
            kind = data[payload] & 31
            entry = {'type': kind, 'range': [start, end], 'payload_range': [payload, end],
                     'sha256': sha(data[start:end]), 'payload_sha256': sha(data[payload:end])}
            if kind in (7, 8):
                entry['payload_hex'] = data[payload:end].hex()
                if kind == 7:
                    entry['syntax'] = sps_metadata(data[payload:end])
                headers.append(entry)
            elif kind in (1, 5):
                aus.append(entry)
        assert len(aus) == count
        planes = []
        for i in range(count):
            frame = decoded.stdout[i*6144:(i+1)*6144]
            values = [sorted(set(frame[:4096])), sorted(set(frame[4096:5120])), sorted(set(frame[5120:]))]
            planes.append({'sha256': sha(frame), 'unique_y_cb_cr': values})
        result['cases'].append({'name': name, 'file': filename, 'sha256': sha(data), 'headers': headers,
                                'access_units': aus, 'oracle_file': oracle, 'oracle_sha256': sha(decoded.stdout),
                                'frames': planes, 'decode_command': decode_command, 'decode_stderr': ''})
    (ROOT / 'high-provenance.json').write_text(json.dumps(result, indent=2) + '\n')
    (ROOT / 'high-provenance.txt').write_text('Original generated High100 progressive 8-bit 4:2:0 64x64 streams.\n'
        'CAVLC uses freshly encoded High SPS/PPS and two original I_PCM pictures.\n'
        'CABAC uses unmodified libx264 High output from independently specified Y40/Y120/Y120 Cb80 Cr180 inputs.\n'
        'First P may be intra-coded; third frame must be dependent Pskip as recorded in encoder log.\n'
        'FFmpeg explode-mode software decoded complete original streams before any native hardware run.\n'
        'Per-pixel YUV outputs, complete stream/header/AU hashes and exact commands are in high-provenance.json.\n'
        'StdVideo omits some VUI restriction fields; canonical SPS roundtrip may differ only there. Original payloads remain unchanged.\n')
    print(json.dumps(result['cases'], indent=2))


if __name__ == '__main__':
    main()
