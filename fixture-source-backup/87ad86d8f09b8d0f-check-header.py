import json,mmap,pathlib,struct
path=pathlib.Path('/dev/shm/linux-bar-rdd-product-wayland-arena')
e=pathlib.Path('/mnt/datasets/builds/evidence/linux-bar-rdd/product-wayland')
with path.open('rb') as f:
    st=path.stat()
    with mmap.mmap(f.fileno(),4096,access=mmap.ACCESS_READ) as view:
        first=view[:80]
        second=view[:80]
assert first==second,'header changed during receipt'
assert first[:8]==b'WCIVSHM1','unpublished header'
u32=lambda o:struct.unpack_from('<I',first,o)[0]
u64=lambda o:struct.unpack_from('<Q',first,o)[0]
r=dict(version=u32(8),header_bytes=u32(12),run=u64(16),generation=u64(24),region_bytes=u64(32),arena_id=u32(40),outbound_offset=u64(48),outbound_bytes=u64(56),rings_offset=u64(64),rings_bytes=u64(72),backing=str(path),backing_dev=st.st_dev,backing_inode=st.st_ino,raw_header_hex=first.hex())
assert r['version'] in [2,3] and r['header_bytes']==4096 and r['run'] and r['generation']
assert r['region_bytes']==st.st_size==128*1024*1024 and r['arena_id']==0
assert r['rings_bytes']==6*1024*1024 and r['rings_offset']+r['rings_bytes']==st.st_size
r['arena_bytes']=r['rings_offset']-4096
assert r['outbound_bytes']==8*1024*1024 and r['outbound_offset']+r['outbound_bytes']<=r['arena_bytes']
(e/'header-check.json').write_text(json.dumps(r,indent=2)+'\n')
pathlib.Path('/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland/host-run.txt').write_text(str(r['run'])+'\n')
print(json.dumps(r,indent=2))
