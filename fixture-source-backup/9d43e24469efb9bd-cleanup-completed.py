"""Delete only this completed fixture's originals, after all retirement gates.
No process is signalled. Missing or uncertain evidence refuses all deletion.
"""
import json,os,pathlib,re,stat,subprocess,sys
base=pathlib.Path('/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland')
evidence=pathlib.Path('/mnt/datasets/builds/evidence/linux-bar-rdd/product-wayland')
receipt=json.loads((base/'snapshot-v3/receipt.json').read_text())
finished=json.loads((evidence/'execution-completion.json').read_text())
assert finished['gpu_exit_code']==0 and finished['vm_exit_code']==0 and finished['compositor_exit_observed'] is True
observation=json.loads((evidence/'provider-observation.json').read_text())
assert observation['process_exit_observed'] and not observation['retained_owner']
log=(evidence/'gpu.log').read_text(errors='replace')
assert 'all admitted owners retired; GPU provider exits cooperatively' in log
assert 'given back after checked GPU completion' in log
assert not re.search(r'GPU completion wait failed|retaining provider|leases are kept|keeping.*leases',log)
browser=(evidence/'firefox-run.log').read_text(errors='replace').replace('\r','')
assert re.search(r'^FIREFOX_PAYLOAD_STATUS=0$',browser,re.M)
runs=list((evidence/'firefox/product-rdd-fixture').glob('firefox-*'))
assert len(runs)==1
run=runs[0]
assert (run/'shutdown-observed.txt').is_file() and not (run/'gpu-completion-uncertain.txt').exists()
assert (run/'cleanup-survivors.txt').is_file() and not (run/'cleanup-survivors.txt').read_text().strip()
guest=(evidence/'stop-guest.log').read_text().replace('\r','')
assert len(re.findall(r'^MainPID=0$',guest,re.M))==5
assert len(re.findall(r'^ActiveState=inactive$',guest,re.M))==5
rows=json.loads((evidence/'live-host.json').read_text())
assert len(rows)==3
[provider]=[r for r in rows if r['exe']==str(base/'snapshot-v3/assets/sv-gpu-server')]
assert provider['sha256']==receipt['artifacts']['sv-gpu-server']['sha256']
for row in rows:
    proc=pathlib.Path(f"/proc/{row['pid']}/stat")
    if proc.exists():
        assert proc.read_text().split(') ',1)[1].split()[19]!=row['stat'].split(') ',1)[1].split()[19], 'recorded process remains'
units=[f'linux-bar-rdd-product-wayland-{kind}-20261020.scope' for kind in ['gpu','vm','compositor']]
scopes=subprocess.run(['systemctl','--user','show',*units,'-p','Id','-p','ActiveState','-p','SubState'],text=True,capture_output=True).stdout
assert scopes.count('ActiveState=inactive')==3 and scopes.count('SubState=dead')==3,scopes
identities=json.loads((evidence/'backing-identities.json').read_text())
paths={'gpu':pathlib.Path('/dev/shm/linux-bar-rdd-product-wayland-gpu'),'arena':pathlib.Path('/dev/shm/linux-bar-rdd-product-wayland-arena'),'root':base/'root.raw'}
for name,path in paths.items():
    saved=identities[name];current=path.lstat()
    assert stat.S_ISREG(current.st_mode) and current.st_uid==os.getuid()
    assert [current.st_dev,current.st_ino,current.st_size]==[saved['dev'],saved['inode'],saved['bytes']],name
if sys.argv[1:]!=['--delete']:
    print('PASS: completion gates allow deletion; no process signalled and no files removed')
else:
    for path in paths.values():path.unlink()
    (evidence/'clean-stop.json').write_text(json.dumps({'scopes':scopes,'signals_sent':False,'provider_completion':'reviewed safe provider exit0 and checked completion log','removed':[str(p) for p in paths.values()]},indent=2)+'\n')
    print('PASS: only completed owned original backings and disk removed')
