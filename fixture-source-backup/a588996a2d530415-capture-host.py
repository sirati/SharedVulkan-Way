"""Create fresh owned originals, or capture the exact running fixture bytes."""
import hashlib,json,os,pathlib,sys
base=pathlib.Path('/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland')
evidence=pathlib.Path('/mnt/datasets/builds/evidence/linux-bar-rdd/product-wayland')
paths={'gpu':pathlib.Path('/dev/shm/linux-bar-rdd-product-wayland-gpu'),'arena':pathlib.Path('/dev/shm/linux-bar-rdd-product-wayland-arena')}
def identity(path):
    st=path.stat();return {'path':str(path),'dev':st.st_dev,'inode':st.st_ino,'bytes':st.st_size}
if sys.argv[1:]==['create']:
    assert not (evidence/'backing-identities.json').exists()
    assert not (base/'root.raw').exists() and not any(path.exists() for path in paths.values())
    saved={}
    for name,size in [('gpu',256<<20),('arena',128<<20)]:
        path=paths[name]
        fd=os.open(path,os.O_RDWR|os.O_CREAT|os.O_EXCL,0o600)
        try:os.ftruncate(fd,size)
        finally:os.close(fd)
        saved[name]=identity(path)
    (evidence/'backing-identities.json').write_text(json.dumps(saved,indent=2)+'\n')
    print('Created two new private original backings; no workload started')
else:
    receipt=json.loads((base/'snapshot-v3/receipt.json').read_text())
    rows=[]
    for proc in pathlib.Path('/proc').iterdir():
        if not proc.name.isdigit():continue
        try:
            exe=os.readlink(proc/'exe');argv=(proc/'cmdline').read_bytes().split(b'\0')
            name='qemu' if exe.endswith('/qemu-system-x86_64') and any(str(base/'root.raw').encode() in value for value in argv) else None
            for candidate in ['sv-gpu-server','wc-compositor']:
                if exe==str(base/'snapshot-v3/assets'/candidate):name=candidate
            if name is None:continue
            digest=hashlib.sha256((proc/'exe').read_bytes()).hexdigest()
            assert digest==receipt['artifacts'][name]['sha256'],name
            rows.append({'artifact':name,'pid':int(proc.name),'exe':exe,'sha256':digest,'argv':[value.decode(errors='replace') for value in argv],'stat':(proc/'stat').read_text(),'maps':(proc/'maps').read_text(),'status':(proc/'status').read_text(),'cgroup':(proc/'cgroup').read_text()})
        except (FileNotFoundError,PermissionError,ProcessLookupError):continue
    assert {row['artifact'] for row in rows}=={'qemu','sv-gpu-server','wc-compositor'} and len(rows)==3
    (evidence/'live-host.json').write_text(json.dumps(rows,indent=2)+'\n')
    saved=json.loads((evidence/'backing-identities.json').read_text())
    for name,path in paths.items():assert saved[name]==identity(path),name
    saved['root']=identity(base/'root.raw')
    (evidence/'backing-identities.json').write_text(json.dumps(saved,indent=2)+'\n')
    print('Live QEMU/GPU/compositor hashes match the frozen package; argv/maps/start identities saved')
