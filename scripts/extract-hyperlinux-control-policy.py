#!/usr/bin/env python3
"""Emit the actual pure control policy for portable ELF loader tests.
Usage: extract-hyperlinux-control-policy.py /path/to/hyperlinux/src > policy.c
Compile test-hyperlinux-elf-control.c with src/elf.c and policy.c.
"""
import pathlib
import re
import sys
src = pathlib.Path(sys.argv[1])
header = (src / "guest.h").read_text()
source = (src / "guest.c").read_text()
print("#include <stdint.h>")
for name in ("PT_POOL_SIZE", "SHIM_DATA_BASE", "BLOCK_2MB"):
    match = re.search(r"^#define\s+" + name + r"\s+.+$", header, re.M)
    if not match:
        raise SystemExit("Missing policy constant: " + name)
    print(match.group())
start = source.index("static int range_overlaps(")
end = source.index("int guest_control_gpa_overlap(", start)
policy = source[start:end]
if "int guest_control_layout_overlap(" not in policy:
    raise SystemExit("Missing layout helper")
print(policy)
