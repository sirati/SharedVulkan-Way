#!/bin/bash
set -euo pipefail
/usr/bin/python3 - <<'PY'
import socket,os,array,tempfile,json
paths=[];fds=[]
for n in range(3):
 fd,path=tempfile.mkstemp();os.unlink(path);os.write(fd,bytes([n]));fds.append(fd)
def send(s,n):return s.sendmsg([bytes([n])],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[fds[n]]))])
def receive(s,flags=0):
 d,c,f,a=s.recvmsg(1,4096,flags);nums=[]
 for level,kind,value in c:
  if level==socket.SOL_SOCKET and kind==socket.SCM_RIGHTS:
   x=array.array('i');x.frombytes(value[:len(value)//4*4]);nums+=list(x)
 assert len(d)==1 and len(nums)==1,(d,nums,f)
 try:
  actual=os.pread(nums[0],1,0)
  print(json.dumps({'record':d.hex(),'fd_payload':actual.hex(),'flags':f,'peek':bool(flags)}),flush=True)
  if not flags:assert actual==d,(d,actual,f)
 finally:
  for fd in nums:os.close(fd)
 return d[0]
a,b=socket.socketpair()
try:
 for n in range(3):assert send(a,n)==1
 assert [receive(b) for _ in range(3)]==list(range(3))
 assert send(a,2)==1
 assert [receive(b,socket.MSG_PEEK),receive(b,socket.MSG_PEEK),receive(b)]==[2]*3
 a.setblocking(False);b.setblocking(False);sent=0
 while sent<10000:
  try:assert send(a,sent%3)==1;sent+=1
  except OSError as e:
   print(json.dumps({'saturation_errno':e.errno,'sent':sent}));break
 drained=0
 while True:
  try:assert receive(b)==drained%3;drained+=1
  except BlockingIOError:break
 assert sent==drained,(sent,drained)
 print(json.dumps({'one_byte_rights_records':3,'peek_duplicates':2,'sent':sent,'drained':drained,'passed':True}))
finally:
 a.close();b.close()
 for fd in fds:os.close(fd)
PY
