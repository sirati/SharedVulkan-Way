#!/bin/bash
set -euo pipefail
fixture=${HL_PROBE_ROOT:?}
export HOME="$fixture/home" TMPDIR=/tmp HL_TEST_DEFERRED_EL1=1
mkdir -p "$HOME" "$fixture/tmp"
old=/nix/store/17p5zj06sz3jv8ys0y77sddvffj1dsg9-hl-deferred-irq-old-test-only-arm64-apple-darwin-0.3.0-rc2/bin/hl
new=/nix/store/5hhjakqvzkqwp8l65y0fkk6i8ia9390a-hl-deferred-irq-new-test-only-arm64-apple-darwin-0.3.0-rc2/bin/hl
/usr/bin/codesign --verify --strict "$old"
/usr/bin/codesign --verify --strict "$new"
/usr/bin/python3 - "$fixture" "$old" "$new" <<'PY'
import subprocess,sys,os
root,old,new=sys.argv[1:]
args=['--audio-backend','null','--fs-mode=rooted','--isolated','--sysroot','/nix/store/imbmjaqwlgy9pj80xbsfn017bb5wvhfc-hyperlinux-probe-sysroot','--guest-home','/home/user','--guest-cwd','/home/user','--bind',root+'/home:/home/user','--bind',root+'/tmp:/tmp','--bind','/nix/store:/nix/store:ro','/nix/store/rssgp4dzy73kbzab2s6b9b7712xi1h87-hl-deferred-signal-client-aarch64-unknown-linux-gnu-0.1.0/bin/hl-deferred-signal-client']
for label,binary in [('old',old),('new',new)]:
 p=subprocess.Popen([binary]+args,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
 timed=False
 try:output,_=p.communicate(timeout=8)
 except subprocess.TimeoutExpired:
  timed=True;p.kill();output,_=p.communicate(timeout=5)
 print('BEGIN '+label,flush=True);print(output,end='',flush=True);print('END '+label+' status='+str(p.returncode)+' timeout='+str(timed),flush=True)
 assert 'actual canceled masked EL1' in output,output
 if label=='old':assert timed and 'deferred-signal-client:' not in output
 else:
  assert not timed and p.returncode==0,output
  assert output.count('actual IRQ HVC13')>=2,output
  assert 'deferred-signal-client:' in output,output
print('same real EL1 cancellation times out before correction and completes through two IRQ deliveries after correction')
PY
