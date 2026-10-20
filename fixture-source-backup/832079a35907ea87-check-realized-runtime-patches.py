#!/usr/bin/env python3
"""Apply actual derivation patch list and copied inputs, fail on first error."""
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

derivation = Path(sys.argv[1]).read_text().strip()
raw = json.loads(subprocess.check_output(["nix", "derivation", "show", derivation], text=True))
item = next(iter(raw.get("derivations", raw).values()))
env = item["env"]
patches = env.get("patches", "").split()
copied = re.findall(r"\bcp (/nix/store/[^\s]+) src/([^\s]+)", env.get("postPatch", ""))
with tempfile.TemporaryDirectory(prefix="wc-hl-source-preflight.") as temporary:
    source = Path(temporary)/"source"
    shutil.copytree(env["src"], source)
    for path in [source, *source.rglob("*")]:
        if not path.is_symlink():
            path.chmod(path.stat().st_mode | 0o200)
    for patch in patches:
        print("APPLY", patch, flush=True)
        subprocess.run(["patch", "--batch", "-p1", "-i", patch], cwd=source, check=True)
    for path, target in copied:
        assert Path(target).name == target
        shutil.copyfile(path, source/"src"/target)
    if len(sys.argv) > 2:
        shutil.copytree(source, Path(sys.argv[2]))
    print(json.dumps({"derivation":derivation,"patch_count":len(patches),"copied_source_count":len(copied),"source_apply_pass":True,"compile_or_runtime_claim":False,"resulting_c_sha256":{name:hashlib.sha256((source/"src"/name).read_bytes()).hexdigest() for name in ["syscall.c","syscall_fs.c","fd_object.c"]}},indent=2),flush=True)
