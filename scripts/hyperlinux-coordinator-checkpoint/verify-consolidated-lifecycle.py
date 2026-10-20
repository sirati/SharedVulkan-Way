#!/usr/bin/env python3
import json
import os
from pathlib import Path
import subprocess
import sys
revision = sys.argv[1]
assert len(revision) == 40 and all(c in "0123456789abcdef" for c in revision)
evidence = Path("/mnt/datasets/builds/evidence/hyperlinux")
expression = 'let f = builtins.getFlake "git+file:///mnt/datasets/builds/shared-vulkan-base/.claude/worktrees/hyperlinux-integration?rev='+revision+'"; stack = import (f.outPath + "/nix/hyperlinux/stack.nix") { inputs = f.inputs // { nixpkgs = f.inputs.hyperlinux-runtime-nixpkgs; nativeNixpkgs = f.inputs.nixpkgs; }; }; in { source = f.outPath; derivation = stack.hl.drvPath; }'
value = json.loads(subprocess.check_output(["nix","eval","--impure","--json","--expr",expression], text=True))
receipt = evidence/("consolidated-lifecycle-"+revision[:8]+".drvpath")
receipt.write_text(value["derivation"])
source = evidence/("consolidated-lifecycle-source-"+revision[:8])
subprocess.run([sys.executable,str(evidence/"check-realized-runtime-patches.py"),str(receipt),str(source)],check=True)
script = str(Path(value["source"])/"nix/hyperlinux/special-fd-lifecycle-check.sh")
env = os.environ.copy()
env.update({"PYTHON":sys.executable,"CC":"/nix/store/788mx070y81zjlg5ipcl0cra3afviw9k-gcc-wrapper-15.2.0/bin/cc","MODE":"portable"})
subprocess.run(["bash",script,str(source),str(evidence/("consolidated-lifecycle-portable-"+revision[:8]))],env=env,check=True)
env.update({"MODE":"darwin","WC_MACOS_CLANG":"/nix/store/c491n6fvrc1lnc0ji5jqbrxz05xfx3cw-clang-21.1.8/bin/clang","WC_MACOS_LLD_BIN":"/nix/store/jc6p3sxz2g0difj5rca9hp6wialwi28n-lld-21.1.8/bin"})
subprocess.run(["bash",script,str(source),str(evidence/("consolidated-lifecycle-darwin-"+revision[:8]))],env=env,check=True)
print("PASS actual consolidated production sources: portable lifecycle plus two negative controls and real-kqueue Darwin build",flush=True)
