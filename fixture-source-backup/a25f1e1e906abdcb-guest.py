import socket,sys,time,select,secrets
s=socket.socket(socket.AF_UNIX);s.settimeout(5)
for _ in range(50):
 try:s.connect('/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland/root.sock');break
 except (FileNotFoundError,ConnectionRefusedError):time.sleep(.2)
s.setblocking(False)
cmd=sys.stdin.read()
nonce=secrets.token_hex(8)
token=('__PRIVATE_DONE_'+nonce+'__').encode()
s.sendall(("set +e\n(\n"+cmd+"\n)\n"+"\nprintf '__PRIVATE_DONE_%s__\\n' "+nonce+"\n").encode())
end=time.monotonic()+180;buf=b''
while time.monotonic()<end:
 if not select.select([s],[],[],min(1,end-time.monotonic()))[0]:continue
 d=s.recv(65536)
 if not d:break
 buf+=d
 if token in buf:break
sys.stdout.buffer.write(buf)
if token not in buf:sys.exit('guest command timed out')
