import hashlib,json,os,subprocess,sys
config=json.loads('{"primary": "/nix/store/h9y6q3lx0l1pp45za12p6j6ildb0wqrg-hl-primary-output-authority-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-primary-output-authority-client", "ptrace": "/nix/store/rxdhzvdmhn25d60sz0gw2cqd9i92aa65-hl-ptrace-pstate-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-ptrace-pstate-client", "dc": "/nix/store/vwbjjp0zva6pisrm6a1a8gxlafjg8g4g-hl-dc-zva-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-dc-zva-client", "seq": "/nix/store/fg9g96yh4gddad619fa060ihcckj66ql-hl-seqpacket-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seqpacket-client", "cleanup": "/nix/store/17pnljin9pbzq0mq7q0i4nxyp93jghmx-hl-seqpacket-cleanup-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seqpacket-cleanup-client", "alias": "/nix/store/yv84r7wbinkdgwvg478h5i4kapb5lmmz-hl-seqpacket-alias-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seqpacket-alias-client", "seccomp": "/nix/store/030c4g4f12ibljhchpinq58f0baky0fn-hl-seccomp-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seccomp-client", "async": "/nix/store/46b2dlp16iyqq8msxr3c7gdvsj99c3v4-hl-async-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-async-fault-client", "external": "/nix/store/hzdymqflnafy2535z2wngd6qdlsml7p1-hl-external-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-external-fault-client", "buswait": "/nix/store/5x0lm4winiqg7450na12yji6znmyiyyj-hl-fatal-bus-wait-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-fatal-bus-wait-client", "runner": "/nix/store/xzj4pa5pn11r54s6g1mn8rw8lmxsn8dh-hyperlinux-probe-runner", "runtime": "/nix/store/c2a4ikfm2976s5pjiifs605i2l7jfhmz-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl", "sha256": "ba03f48a9cd94f525dff9e211c93d6979dec5e6d8fd1860a078f155ab8737379"}')
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
