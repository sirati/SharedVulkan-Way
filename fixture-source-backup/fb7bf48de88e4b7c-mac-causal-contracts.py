import hashlib,subprocess
vnode="/tmp/wc-hl-vnode-contract-2b7aced175e0"
assert hashlib.sha256(open(vnode,"rb").read()).hexdigest()=="2b7aced175e0ac5473b10c6aee793b24f88b81aa708ff8bf59c7463451253dac"
ro="/nix/store/6b4h8vi8zi7kviqx85ag7wjak36p5kzn-hl-readonly-current-max-check-arm64-apple-darwin-0.1.0/bin/hl-readonly-current-max-check"
for executable,limit in [(vnode,5),(ro,15)]:
 subprocess.run(["/usr/bin/codesign","--verify","--strict",executable],check=True)
 print("BEGIN native contract",executable,flush=True)
 subprocess.run([executable],timeout=limit,check=True)
 print("END native contract PASS",executable,flush=True)
