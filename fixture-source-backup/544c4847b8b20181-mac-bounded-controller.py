import os, signal, subprocess, sys, tempfile, shutil
root = tempfile.mkdtemp(prefix="wc-hl-bounded.", dir="/tmp")
os.chmod(root, 0o700)
os.environ["HL_PROBE_ROOT"] = root
child = subprocess.Popen(sys.argv[1:])
try:
    status = child.wait(timeout=35)
except subprocess.TimeoutExpired:
    table = subprocess.check_output(['/bin/ps', '-axo', 'pid=,ppid='], text=True)
    parents = {int(pid): int(ppid) for pid, ppid in (line.split() for line in table.splitlines())}
    owned = [child.pid]
    for index in range(len(parents)):
        additions = [pid for pid, parent in parents.items() if parent in owned and pid not in owned]
        if not additions:
            break
        owned.extend(additions)
    for pid in reversed(owned):
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    child.wait(timeout=5)
    print('bounded fixture timed out; killed only its recorded descendant PIDs', owned, flush=True)
    shutil.rmtree(root)
    sys.exit(124)
shutil.rmtree(root)
sys.exit(status if status >= 0 else 128-status)
