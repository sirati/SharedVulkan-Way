#!/usr/bin/env python3
"""Extract the sole code section from hl's ARM64 Mach-O assembly objects.

GNU objcopy on Linux may lack Mach-O input support. This handles only the
upstream shim/vDSO objects, rejects relocations, and never changes machine code.
"""
import struct
import sys
from pathlib import Path

if len(sys.argv) not in (3, 5):
    raise SystemExit("usage: macho-text-blob.py OBJECT OUTPUT [HEADER SYMBOL]")
data = Path(sys.argv[1]).read_bytes()
magic, cpu, subtype, kind, ncmds, sizeofcmds, flags, reserved = struct.unpack_from("<8I", data)
if magic != 0xFEEDFACF or cpu != 0x100000C or kind != 1:
    raise SystemExit("expected a little-endian ARM64 Mach-O object")
at = 32
sections = []
for _ in range(ncmds):
    cmd, size = struct.unpack_from("<2I", data, at)
    if size < 8 or at + size > 32 + sizeofcmds:
        raise SystemExit("invalid load command")
    if cmd == 0x19:  # LC_SEGMENT_64
        nsects = struct.unpack_from("<I", data, at + 64)[0]
        for i in range(nsects):
            sec = at + 72 + 80 * i
            name, segment, address, length, offset, align, reloff, nreloc, *_ = struct.unpack_from("<16s16sQQ8I", data, sec)
            if name.rstrip(b"\0") == b"__text":
                if nreloc or offset + length > len(data):
                    raise SystemExit("code section has relocations or invalid bounds")
                sections.append(data[offset:offset + length])
    at += size
if len(sections) != 1 or not sections[0]:
    raise SystemExit("expected exactly one nonempty __text section")
Path(sys.argv[2]).write_bytes(sections[0])

if len(sys.argv) == 5:
    symbol = sys.argv[4]
    if not symbol.isascii() or not symbol.isidentifier():
        raise SystemExit("invalid C symbol")
    rows = [", ".join(f"0x{b:02x}" for b in sections[0][i:i + 16]) for i in range(0, len(sections[0]), 16)]
    Path(sys.argv[3]).write_text(
        f"static const unsigned char {symbol}[] = {{\n" + ",\n".join(rows) +
        f"\n}};\nstatic const unsigned int {symbol}_len = sizeof({symbol});\n"
    )
