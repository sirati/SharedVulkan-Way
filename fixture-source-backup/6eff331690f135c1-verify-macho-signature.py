from pathlib import Path
import hashlib,json,plistlib,struct,sys
results=[]
for arg in sys.argv[1:]:
 b=Path(arg).read_bytes(); h=struct.unpack_from('<8I',b); assert h[0]==0xfeedfacf and h[1]==0x100000c
 off=32; sig=None
 for _ in range(h[4]):
  cmd,n=struct.unpack_from('<2I',b,off)
  if cmd==0x1d: pos,length=struct.unpack_from('<2I',b,off+8);sig=b[pos:pos+length]
  off+=n
 assert sig is not None
 magic,length,n=struct.unpack_from('>3I',sig);assert magic==0xfade0cc0 and length<=len(sig)
 blobs={}
 for i in range(n):
  slot,pos=struct.unpack_from('>2I',sig,12+i*8); m,size=struct.unpack_from('>2I',sig,pos); blobs[slot]=sig[pos:pos+size]
 cd=blobs[0];_,_,version,flags,hashoff,_,special,pages,limit=struct.unpack_from('>9I',cd)
 hashsize,hashtype,_,pageshift=cd[36:40];assert hashtype==2 and hashsize==32
 if version>=0x20300 and limit==0xffffffff:limit=struct.unpack_from('>Q',cd,56)[0]
 for i in range(pages):
  actual=hashlib.sha256(b[i*(1<<pageshift):min(limit,(i+1)*(1<<pageshift))]).digest()
  assert actual==cd[hashoff+i*hashsize:hashoff+(i+1)*hashsize],f'code page {i}'
 for slot,blob in blobs.items():
  if 0<slot<=special:
   assert hashlib.sha256(blob).digest()==cd[hashoff-slot*hashsize:hashoff-(slot-1)*hashsize],f'special {slot}'
 ent=plistlib.loads(blobs[5][8:]) if 5 in blobs else {}
 if Path(arg).name=='hl':assert ent.get('com.apple.security.hypervisor') is True
 results.append(dict(path=arg,cpu='ARM64',code_pages_verified=pages,page_size=1<<pageshift,flags=hex(flags),entitlements=ent))
print(json.dumps(results,indent=2))
