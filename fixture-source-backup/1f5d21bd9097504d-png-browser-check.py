import struct,sys,zlib
blob=open(sys.argv[1],'rb').read()
assert blob[:8]==b'\x89PNG\r\n\x1a\n'
pos=8; compressed=bytearray()
while pos<len(blob):
 size=struct.unpack_from('>I',blob,pos)[0]; kind=blob[pos+4:pos+8]; data=blob[pos+8:pos+8+size];pos+=12+size
 if kind==b'IHDR': width,height,depth,color,compression,filtering,interlace=struct.unpack('>IIBBBBB',data)
 if kind==b'IDAT': compressed.extend(data)
assert (width,height)==(320,240)
assert depth==8 and color in (2,6) and interlace==0
channels=3 if color==2 else 4
stride=width*channels; raw=zlib.decompress(compressed); previous=bytearray(stride); offset=0
for y in range(height):
 mode=raw[offset]; offset+=1; row=bytearray(raw[offset:offset+stride]); offset+=stride
 for x in range(stride):
  left=row[x-channels] if x>=channels else 0
  up=previous[x]; corner=previous[x-channels] if x>=channels else 0
  if mode==0: predictor=0
  elif mode==1: predictor=left
  elif mode==2: predictor=up
  elif mode==3: predictor=(left+up)//2
  elif mode==4:
   p=left+up-corner; a,b,c=abs(p-left),abs(p-up),abs(p-corner)
   predictor=left if a<=b and a<=c else up if b<=c else corner
  else: raise AssertionError(mode)
  row[x]=(row[x]+predictor)&255
 if y==220: assert list(row[300*channels:300*channels+3])==[64,128,191]
 previous=row
print('stock ARM64 Firefox headless rendered the expected local-page color into a 320x240 PNG')
