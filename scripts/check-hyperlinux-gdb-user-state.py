#!/usr/bin/env python3
"""Bounded native RSP check against an explicitly enabled EL0 app debugger.
Controller starts its exact runtime with --gdb PORT and checks normal guest
completion after this client detaches. No host process or runtime is launched.
"""
import socket
import sys

with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5) as channel:
    channel.settimeout(5)
    def byte():
        value = channel.recv(1)
        if not value:
            raise RuntimeError("Debugger disconnected")
        return value
    def packet(payload):
        wire = payload.encode()
        channel.sendall(b"$" + wire + b"#" + f"{sum(wire) & 255:02x}".encode())
        while byte() != b"$":
            pass
        reply = bytearray()
        while True:
            value = byte()
            if value == b"#":
                break
            reply += value
        checksum = byte() + byte()
        assert int(checksum, 16) == sum(reply) & 255
        try:
            channel.sendall(b"+")
        except BrokenPipeError:
            if payload != "D":
                raise
        return reply.decode()
    assert packet("?").startswith(("T", "S"))
    original = bytes.fromhex(packet("g"))
    assert len(original) == 788
    for bit in (5, 0x10, 0x80, 1 << 20, 1 << 21, 1 << 22):
        changed = bytearray(original)
        changed[0:8] = (0x12345678).to_bytes(8, "little")
        changed[264:268] = bit.to_bytes(4, "little")
        assert packet("G" + changed.hex()) == "E16"
        assert bytes.fromhex(packet("g")) == original
        assert packet("P21=" + bit.to_bytes(4, "little").hex()) == "E16"
        assert bytes.fromhex(packet("g")) == original
    pc = int.from_bytes(original[256:264], "little")
    assert packet("P20=" + (pc | 1).to_bytes(8, "little").hex()) == "E16"
    assert packet("c" + f"{pc | 1:x}") == "E16"
    assert packet("s" + f"{pc | 1:x}") == "E16"
    assert bytes.fromhex(packet("g")) == original
    valid = bytearray(original)
    flags = int.from_bytes(valid[264:268], "little")
    valid[264:268] = ((flags & 0x03001c00) | 0x90000000).to_bytes(4, "little")
    assert packet("G" + valid.hex()) == "OK"
    assert bytes.fromhex(packet("g")) == valid
    assert packet("D") == "OK"
print("GDB conditional EL0 app boundary: invalid G/P/c/s refused unchanged, valid NZCV preserved and detached")
