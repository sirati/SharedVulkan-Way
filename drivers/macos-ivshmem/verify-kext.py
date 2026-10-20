#!/usr/bin/env python3
"""Check the cross-linker's artifact, without implying target-kernel admission."""
import json
import pathlib
import struct
import sys


def verify(path):
    data = pathlib.Path(path).read_bytes()
    if len(data) < 32:
        raise ValueError("short Mach-O header")
    magic, cpu, subtype, kind, count, command_bytes, flags, reserved = struct.unpack_from("<8I", data)
    if magic != 0xFEEDFACF or cpu != 0x01000007 or kind != 11:
        raise ValueError("expected a thin Intel MH_KEXT_BUNDLE, not an executable or dylib")
    end = 32 + command_bytes
    if end > len(data):
        raise ValueError("truncated load commands")
    cursor = 32
    symtab = None
    commands = []
    for _ in range(count):
        if cursor + 8 > end:
            raise ValueError("truncated command header")
        command, size = struct.unpack_from("<2I", data, cursor)
        if size < 8 or size % 8 or cursor + size > end:
            raise ValueError("invalid command length")
        if command in (0xE, 0x80000028):
            raise ValueError("userspace dyld/main load command in kernel bundle")
        if command == 2:
            if size < 24:
                raise ValueError("short symbol table command")
            symtab = struct.unpack_from("<4I", data, cursor + 8)
        commands.append(hex(command))
        cursor += size
    if cursor != end or symtab is None:
        raise ValueError("missing symbol table or inconsistent load-command size")
    offset, symbols, strings, string_bytes = symtab
    if offset + symbols * 16 > len(data) or strings + string_bytes > len(data):
        raise ValueError("truncated symbol table")
    definitions = set()
    imports = set()
    for index in range(symbols):
        strx, typ, section, desc, value = struct.unpack_from("<IBBHQ", data, offset + index * 16)
        if strx >= string_bytes:
            raise ValueError("symbol outside string table")
        finish = data.find(b"\0", strings + strx, strings + string_bytes)
        if finish < 0:
            raise ValueError("unterminated symbol")
        name = data[strings + strx:finish].decode("ascii")
        if typ & 0xE0:  # STAB/debug entries are not definitions or imports.
            continue
        if typ & 0x0E:
            definitions.add(name)
        elif typ & 1:
            imports.add(name)
    for expected in ("_kmod_info", "_WCIVSHMEM_start", "_WCIVSHMEM_stop"):
        if expected not in definitions:
            raise ValueError("missing module definition " + expected)
    if not any("WCIVSHMEM" in name and "metaClass" in name for name in definitions):
        raise ValueError("driver metaclass missing")
    if not imports:
        raise ValueError("kernel imports missing")
    print(json.dumps({"architecture": "x86_64", "filetype": "MH_KEXT_BUNDLE",
                      "load_commands": commands, "kernel_imports": sorted(imports),
                      "has_code_signature": "0x1d" in commands,
                      "trusted_signing_validated": False, "runtime_validated": False}, indent=2))


if __name__ == "__main__":
    try:
        verify(sys.argv[1])
    except (IndexError, ValueError, struct.error) as error:
        sys.exit(str(error))
