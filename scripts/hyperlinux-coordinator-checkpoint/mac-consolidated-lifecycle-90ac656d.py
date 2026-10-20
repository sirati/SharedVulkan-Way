import hashlib,subprocess
p='/tmp/wc-hl-lifecycle-90ac656d-cba4026cbac6'
assert hashlib.sha256(open(p,"rb").read()).hexdigest()=='cba4026cbac63a1e818fbe1a0c5e3bad7e319258a060135eaafa385ebbabc1ee'
subprocess.run(["/usr/bin/codesign","--verify","--strict",p],check=True)
subprocess.run([p],check=True,timeout=25)
