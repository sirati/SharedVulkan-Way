#!/usr/bin/env python3
"""Prepare a bounded remote fixture matrix from realized build receipts."""
import hashlib
import json
from pathlib import Path
import sys

build = json.loads(Path(sys.argv[1]).read_text())
receipt = json.loads(Path(sys.argv[2]).read_text())[0]
assert receipt["excluded_test_injection_check_passed"]
outputs = [item["outputs"]["out"] for item in build]
def package(name):
    matches = [path for path in outputs if Path(path).name[33:].startswith(name)]
    assert len(matches) == 1, (name, matches)
    return matches[0]
names = {
    "primary": "hl-primary-output-authority-client",
    "ptrace": "hl-ptrace-pstate-client",
    "dc": "hl-dc-zva-client",
    "seq": "hl-seqpacket-client",
    "cleanup": "hl-seqpacket-cleanup-client",
    "alias": "hl-seqpacket-alias-client",
    "seccomp": "hl-seccomp-client",
    "async": "hl-async-fault-client",
    "external": "hl-external-fault-client",
    "buswait": "hl-fatal-bus-wait-client",
}
config = {key: package(name) + "/bin/" + name for key, name in names.items()}
config["runner"] = package("hyperlinux-probe-runner")
config["runtime"] = package("hl-waycompose") + "/bin/hl"
assert str(Path(config["runtime"]).parent.parent) == receipt["output"]
config["sha256"] = receipt["binary_sha256"]
assert hashlib.sha256(Path(config["runtime"]).read_bytes()).hexdigest() == config["sha256"]
script = r'''import hashlib,json,os,subprocess,sys
config=json.loads(CONFIG)
assert hashlib.sha256(open(config["runtime"],"rb").read()).hexdigest()==config["sha256"]
subprocess.run(["/usr/bin/codesign","--verify","--strict",config["runtime"]],check=True)
root=os.environ["HL_PROBE_ROOT"]
os.environ["HOME"]=root+"/home"
os.environ["HL_NATIVE_FIXTURE"]=root
os.makedirs(root+"/home",exist_ok=True)
os.makedirs(root+"/tmp",exist_ok=True)
case=sys.argv[1]
def run(key,*args):
    print("BEGIN consolidated",case,key,*args,flush=True)
    subprocess.run([config["runner"],config[key],*args],check=True,timeout=31)
    print("END consolidated",case,key,*args,"PASS",flush=True)
if case=="primary":
    for mode in range(6): run("primary",str(mode))
elif case=="seccomp":
    for mode in ["--basic","--backend-denial","--trap","--tsync","--stacked","--tsync-divergent","--kill-thread","--arguments","--wait-status"]: run("seccomp",mode)
elif case=="buswait": run("buswait",config["external"])
elif case.startswith("dc-"): run("dc",case[3:])
elif case.startswith("external-"): run("external",case[9:])
else: run(case)
'''
Path(sys.argv[3]).write_text(script.replace("CONFIG", repr(json.dumps(config))))
Path(sys.argv[3]+".config.json").write_text(json.dumps(config,indent=2)+"\n")
print(json.dumps({"script":sys.argv[3],"runtime":config["runtime"],"sha256":config["sha256"],"cases":["primary","ptrace","dc-valid","dc-validtag","dc-ro","dc-none","dc-0x100000","dc-bus","seq","cleanup","alias","seccomp","async","external-handled","external-pending","buswait"]},indent=2))
