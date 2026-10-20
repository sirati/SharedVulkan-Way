"""Observe the exact provider process. Never signal it or unlink its backing."""
import json,os,pathlib,select,time
base=pathlib.Path('/mnt/datasets/builds/scratch/linux-bar-rdd/product-wayland')
evidence=pathlib.Path('/mnt/datasets/builds/evidence/linux-bar-rdd/product-wayland')
rows=json.loads((evidence/'live-host.json').read_text())
[provider]=[r for r in rows if r['exe']==str(base/'snapshot-v3/assets/sv-gpu-server')]
pid=provider['pid']
expected=provider['stat'].split(') ',1)[1].split()[19]
result={'pid':pid,'start_ticks':expected,'signals_sent':False,'backing_reused_or_deleted':False,'provider_exit_status':'requires execution-session receipt'}
try:
    fd=os.pidfd_open(pid)
except ProcessLookupError:
    result['process_exit_observed']=True
else:
    try:
        current=pathlib.Path(f'/proc/{pid}/stat').read_text().split(') ',1)[1].split()[19]
        assert current==expected,'recorded provider PID was reused'
        elapsed=time.clock_gettime(time.CLOCK_BOOTTIME)-int(expected)/os.sysconf('SC_CLK_TCK')
        result['process_exit_observed']=bool(select.select([fd],[],[],max(0,480-elapsed))[0])
    finally:
        os.close(fd)
result['retained_owner']=not result['process_exit_observed']
if result['retained_owner']:
    result['action']='STOP RETRIES. Keep provider scope and both original BAR backings. No termination, deletion or reuse.'
    result['backings']={}
    for name in ['gpu','arena']:
        path=pathlib.Path('/dev/shm/linux-bar-rdd-product-wayland-'+name)
        st=path.stat();result['backings'][name]={'path':str(path),'dev':st.st_dev,'inode':st.st_ino,'bytes':st.st_size}
    proc=pathlib.Path(f'/proc/{pid}')
    for name in ['status','maps','cgroup','stat']:
        try:(evidence/f'retained-provider-{name}.txt').write_text((proc/name).read_text())
        except FileNotFoundError:pass
else:
    result['action']='Process exit observed only. Verify exit status, reviewed safe binary and actual completion/browser/CPU receipts before any backing cleanup.'
(evidence/'provider-observation.json').write_text(json.dumps(result,indent=2)+'\n')
print(json.dumps(result,indent=2))
raise SystemExit(0 if result['process_exit_observed'] else 3)
