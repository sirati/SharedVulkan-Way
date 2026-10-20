#!/bin/bash
set -euo pipefail
/usr/bin/python3 - <<'PY'
import socket,os,json,array,select,struct
out=[]
def record(tag,fn):
 try:out.append({'tag':tag,'value':fn()})
 except OSError as e:out.append({'tag':tag,'errno':e.errno})
def peek(a,flags=0,cap=8):
 try:
  d,c,f,addr=a.recvmsg(cap,256,flags)
  rights=[]
  for level,kind,value in c:
   if level==socket.SOL_SOCKET and kind==socket.SCM_RIGHTS:
    nums=array.array('i');nums.frombytes(value[:len(value)//4*4]);rights+=list(nums)
  for fd in rights:os.close(fd)
  return {'data':d.hex(),'flags':f,'rights':len(rights)}
 except OSError as e:return {'errno':e.errno}
for how in (socket.SHUT_WR,socket.SHUT_RD,socket.SHUT_RDWR):
 a,b=socket.socketpair(socket.AF_UNIX,socket.SOCK_DGRAM);a.setblocking(False);b.setblocking(False)
 try:
  a.send(b'queued');record('shutdown'+str(how),lambda:b.shutdown(how));out.append({'tag':'shutdownrecv'+str(how),'peer':peek(a),'self':peek(b)})
  record('shutdownsendpeer'+str(how),lambda:a.send(b'x'));record('shutdownsendself'+str(how),lambda:b.send(b'y'))
 finally:a.close();b.close()
a,b=socket.socketpair(socket.AF_UNIX,socket.SOCK_DGRAM);a.setblocking(False);b.setblocking(False)
try:
 b.send(b'queued');b.close();out.append({'tag':'closepending','first':peek(a),'second':peek(a)})
finally:a.close();b.close()
a,b=socket.socketpair(socket.AF_UNIX,socket.SOCK_DGRAM);a.setblocking(False);b.setblocking(False)
try:
 record('zerolen_send',lambda:b.send(b''));out.append({'tag':'zerolen_recv','value':peek(a)})
 for flags in (0,socket.MSG_TRUNC):
  b.send(b'abcdefghijkl');out.append({'tag':'trunc'+str(flags),'value':peek(a,flags,3)})
 r,w=os.pipe()
 try:
  b.sendmsg([b'right'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[r]))])
  out.append({'tag':'peekrights','peek':peek(a,socket.MSG_PEEK),'consume':peek(a)})
 finally:os.close(r);os.close(w)
 total=0
 try:
  while total<10000:b.send(b'x'*512);total+=1
 except OSError as e:out.append({'tag':'saturation','records':total,'errno':e.errno})
 drained=0
 while 'errno' not in peek(a):drained+=1
 out.append({'tag':'drained','records':drained})
finally:a.close();b.close()
for typ in (socket.SOCK_STREAM,socket.SOCK_DGRAM):
 a,b=socket.socketpair(socket.AF_UNIX,typ)
 try:
  for opt in (1,2,3,4,5):
   record('localopt_type'+str(typ)+'_opt'+str(opt),lambda opt=opt:a.getsockopt(0,opt,128).hex())
 finally:a.close();b.close()
print(json.dumps(out,indent=2))
PY
