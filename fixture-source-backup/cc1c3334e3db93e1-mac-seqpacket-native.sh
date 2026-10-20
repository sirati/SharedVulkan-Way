#!/bin/bash
set -euo pipefail
/usr/bin/python3 - <<'PY'
import socket,select,json
for kind in (socket.SOCK_SEQPACKET,socket.SOCK_DGRAM):
 try:a,b=socket.socketpair(socket.AF_UNIX,kind)
 except OSError as e: print(json.dumps({'kind':kind,'pair_errno':e.errno}));continue
 try:
  a.setblocking(False);b.close()
  events=select.select([a],[a],[a],0)
  result={'kind':kind,'closed_read_ready':bool(events[0]),'closed_write_ready':bool(events[1]),'closed_exception':bool(events[2])}
  for operation in ('recv','send'):
   try: result[operation]=getattr(a,operation)(32 if operation=='recv' else b'abc').hex() if operation=='recv' else a.send(b'abc')
   except OSError as e: result[operation+'_errno']=e.errno
  print(json.dumps(result))
 finally:a.close();b.close()
PY
