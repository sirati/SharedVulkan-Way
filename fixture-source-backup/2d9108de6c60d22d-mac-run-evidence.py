import subprocess,shlex,sys
base='/mnt/datasets/builds/evidence/hyperlinux/'
controller=open(base+'mac-bounded-controller.py').read()
command=['ssh','-o','BatchMode=yes','-o','ConnectTimeout=10','-o','IdentityAgent=~/.1password/agent.sock','testhost.example','/usr/bin/python3','-c',shlex.quote(controller),'/bin/bash','-s']
with open(base+sys.argv[1]) as source,open(base+sys.argv[2],'w') as log:
 result=subprocess.run(command,stdin=source,stdout=log,stderr=subprocess.STDOUT)
raise SystemExit(result.returncode)
