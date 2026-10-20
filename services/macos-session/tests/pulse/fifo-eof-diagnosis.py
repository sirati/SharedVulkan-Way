import array,ctypes as C,fcntl,json,os,select,signal,subprocess,sys,time
from pathlib import Path
signal.alarm(30)
root=Path(os.environ['FIXTURE_ROOT'])
if '--child' not in sys.argv:
 results=[]
 for provider in ('/nix/store/i4g0c9xybpvx72m3zymz3dxz230za24s-wc-pulseaudio-aarch64-darwin-17.0',os.environ['PULSE_PACKAGE']):
  p=subprocess.run([sys.executable,__file__,'--child'],env=dict(os.environ,PULSE_PACKAGE=provider),capture_output=True,text=True,timeout=12)
  assert p.returncode==0,p.stderr
  results.append(json.loads(p.stdout))
 result={'providers':results,'audio_started':False}
 (root/'fifo-eof-diagnosis.json').write_text(json.dumps(result,indent=2)+'\n');print(json.dumps(result));sys.exit()
class PollFD(C.Structure):
 _fields_=[('fd',C.c_int),('events',C.c_short),('revents',C.c_short)]
provider=Path(os.environ['PULSE_PACKAGE']);lib=C.CDLL(str(provider/'lib/pulseaudio/libpulsecommon-17.0.dylib'))
lib.pa_poll.argtypes=[C.POINTER(PollFD),C.c_ulong,C.c_int];lib.pa_poll.restype=C.c_int
failures=[]
for trial in range(300):
 path=root/f'probe-{os.getpid()}.fifo';os.mkfifo(path,0o600)
 r=os.open(path,os.O_RDONLY|os.O_NONBLOCK);w=os.open(path,os.O_WRONLY|os.O_NONBLOCK)
 try:
  assert os.write(w,bytes(512))==512
  os.close(w);w=-1
  assert os.read(r,512)==bytes(512)
  p=PollFD(r,select.POLLIN,0);assert lib.pa_poll(C.byref(p),1,0)>=0
  if not p.revents&select.POLLHUP:
   count=array.array('i',[0]);fcntl.ioctl(r,0x4004667f,count,True)
   kq=select.kqueue();events=kq.control([select.kevent(r,filter=select.KQ_FILTER_READ,flags=select.KQ_EV_ADD|select.KQ_EV_ONESHOT)],1,0);kq.close()
   failure={'trial':trial,'pa_poll':p.revents,'fionread_after_poll':count[0],'kqueue':[[e.flags,e.data] for e in events]}
   time.sleep(.001);again=PollFD(r,select.POLLIN,0);lib.pa_poll(C.byref(again),1,0);failure['after_1ms']=again.revents
   failures.append(failure)
 finally:
  if w>=0:os.close(w)
  os.close(r);path.unlink()
print(json.dumps({'provider':str(provider),'trials':300,'immediate_missing_hup':len(failures),'examples':failures[:10]}))
