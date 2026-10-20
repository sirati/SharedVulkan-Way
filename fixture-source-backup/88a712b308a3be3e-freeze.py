"""Copy explicitly selected fixture files and record their reviewed bindings.
This script never launches a workload. Hashes do not establish build provenance.
"""
import hashlib,json,pathlib,shutil,subprocess,sys
script=pathlib.Path(__file__).resolve()
base=script.parents[2] if script.name=='freeze-script' else script.parent
selected=sys.argv[2] if len(sys.argv)>2 else 'snapshot'
assert selected in ['snapshot','snapshot-v2','snapshot-v3']
sha=lambda p:hashlib.sha256(pathlib.Path(p).read_bytes()).hexdigest()
if len(sys.argv)>1 and sys.argv[1]=='verify':
    receipt=json.loads((base/selected/'receipt.json').read_text())
    for name,item in receipt['artifacts'].items():
        assert sha(item['snapshot_path'])==item['sha256'],name
    print('PASS: all frozen bytes match receipt; source/build provenance requires review')
else:
    config=json.loads((base/'selected-artifacts.json').read_text())
    for source in config['sources'].values():
        path=source['path']
        commit=subprocess.check_output(['git','-C',path,'rev-parse','HEAD'],text=True).strip()
        assert commit==source['commit'],path
        assert not subprocess.check_output(['git','-C',path,'status','--porcelain'],text=True).strip(),path
    destination=base/selected
    destination.mkdir(mode=0o700)
    assets=destination/'assets';assets.mkdir()
    receipt={**config,'artifacts':{},'build_provenance':'review declared source bindings and build logs; byte hashes alone do not prove source provenance'}
    for name,item in config['artifacts'].items():
        original=pathlib.Path(item['path']).resolve(strict=True)
        assert original.is_file()
        target=assets/name
        shutil.copyfile(original,target)
        target.chmod(0o555)
        digest=sha(original)
        assert sha(target)==digest,name
        receipt['artifacts'][name]={**item,'resolved_path':str(original),'snapshot_path':str(target),'sha256':digest,'bytes':target.stat().st_size}
    (destination/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
    (destination/'receipt.json').chmod(0o444)
    print(destination/'receipt.json',sha(destination/'receipt.json'))
