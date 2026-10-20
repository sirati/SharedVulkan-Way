import hashlib,json,os,subprocess,sys
config=json.loads('{"runtime": "/nix/store/qz9d6bsyyaiqg3b679880mx4f82zk6ay-hl-waycompose-arm64-apple-darwin-0.3.0-rc2/bin/hl", "runner": "/nix/store/h60pbyjqgmz63gx961w48nz2n48xi7x9-hyperlinux-probe-runner", "sha256": "173f00a24ef1485cb9fba2444b20a89f3f5212b486ce5689dd9b9665e2a19ad1", "primary": "/nix/store/h9y6q3lx0l1pp45za12p6j6ildb0wqrg-hl-primary-output-authority-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-primary-output-authority-client", "ptrace": "/nix/store/12mlakn4mjl3xnbdz2r7yrcg862d7wjq-hl-ptrace-pstate-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-ptrace-pstate-client", "cleanup": "/nix/store/17pnljin9pbzq0mq7q0i4nxyp93jghmx-hl-seqpacket-cleanup-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seqpacket-cleanup-client", "status": "/nix/store/030c4g4f12ibljhchpinq58f0baky0fn-hl-seccomp-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-seccomp-client", "async": "/nix/store/46b2dlp16iyqq8msxr3c7gdvsj99c3v4-hl-async-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-async-fault-client", "buswait": "/nix/store/5x0lm4winiqg7450na12yji6znmyiyyj-hl-fatal-bus-wait-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-fatal-bus-wait-client", "external": "/nix/store/hzdymqflnafy2535z2wngd6qdlsml7p1-hl-external-fault-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-external-fault-client"}')
assert hashlib.sha256(open(config["runtime"],"rb").read()).hexdigest()==config["sha256"]
subprocess.run(["/usr/bin/codesign","--verify","--strict",config["runtime"]],check=True)
root=os.environ["HL_PROBE_ROOT"]
os.environ["HOME"]=root+"/home";os.environ["HL_NATIVE_FIXTURE"]=root
os.makedirs(root+"/home",exist_ok=True);os.makedirs(root+"/tmp",exist_ok=True)
case=sys.argv[1]
def run(key,*args):
 print("BEGIN candidate",case,key,*args,flush=True)
 subprocess.run([config["runner"],config[key],*args],check=True)
 print("END candidate",case,key,*args,"PASS",flush=True)
if case=="primary":
 for mode in range(6): run("primary",str(mode))
elif case=="status":
 for mode in ["--basic","--backend-denial","--trap","--tsync","--stacked","--tsync-divergent","--kill-thread","--arguments","--wait-status"]: run("status",mode)
elif case=="buswait": run("buswait",config["external"])
else: run(case)
