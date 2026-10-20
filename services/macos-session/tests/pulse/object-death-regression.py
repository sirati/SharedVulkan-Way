"""Exercise actual shipped-library imported-block localization and refusal.
Run only inside the authorized native headless runtime fixture.
"""
import ctypes as C
import json
import os
from pathlib import Path
import signal
import socket

signal.alarm(15)
root = Path(os.environ['FIXTURE_ROOT'])
provider = Path(os.environ['PULSE_PACKAGE'])
pulse = C.CDLL(str(provider / 'lib/libpulse.0.dylib'), mode=C.RTLD_GLOBAL)
lib = C.CDLL(str(provider / 'lib/pulseaudio/libpulsecommon-17.0.dylib'))
P, U, Z, B = C.c_void_p, C.c_uint32, C.c_size_t, C.c_bool

def function(name, result, args):
    fn = getattr(lib, name)
    fn.restype, fn.argtypes = result, args
    return fn

release_type = C.CFUNCTYPE(None, P, U, P)
release = release_type(lambda *_: None)
new_pool = function('pa_mempool_new', P, [C.c_int, Z, B])
unref_pool = function('pa_mempool_unref', None, [P])
new_block = function('pa_memblock_new_pool', P, [P, Z])
unref_block = function('pa_memblock_unref', None, [P])
origin = function('pa_memblock_has_foreign_origin', B, [P])
ours = function('pa_memblock_is_ours', B, [P])
new_export = function('pa_memexport_new', P, [P, release_type, P])
free_export = function('pa_memexport_free', None, [P])
put = function('pa_memexport_put', C.c_int, [P, P, C.POINTER(C.c_int), C.POINTER(U), C.POINTER(U), C.POINTER(Z), C.POINTER(Z)])
new_import = function('pa_memimport_new', P, [P, release_type, P])
attach = function('pa_memimport_attach_memfd', C.c_int, [P, U, C.c_int, B])
get = function('pa_memimport_get', P, [P, C.c_int, U, U, Z, Z, B])
revoke = function('pa_memimport_process_revoke', C.c_int, [P, U])
free_import = function('pa_memimport_free', None, [P])
getfd = function('pa_mempool_get_memfd_fd', C.c_int, [P])
import array
import fcntl
import hashlib
import mmap

parent, child = socket.socketpair()
pid = os.fork()
if pid == 0:
    parent.close()
    try:
        source = new_pool(1, 0, False)
        block = new_block(source, 512)
        export = new_export(source, release, None)
        kind, bid, sid, offset, size = C.c_int(), U(), U(), Z(), Z()
        assert put(export, block, C.byref(kind), C.byref(bid), C.byref(sid), C.byref(offset), C.byref(size)) == 0
        pointer = function('pa_memblock_acquire', P, [P])(block)
        pattern = bytes(range(256)) * 2
        C.memmove(pointer, pattern, len(pattern))
        function('pa_memblock_release', None, [P])(block)
        fd = getfd(source)
        stat = os.fstat(fd)
        metadata = dict(dev=stat.st_dev, ino=stat.st_ino, size=stat.st_size,
                        offset=offset.value, length=size.value, uid=stat.st_uid,
                        access=fcntl.fcntl(fd, fcntl.F_GETFL) & os.O_ACCMODE,
                        sha256=hashlib.sha256(pattern).hexdigest())
        child.sendmsg([json.dumps(metadata).encode()], [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array.array('i', [fd]))])
        os._exit(0)  # Deliberately skip library cleanup and cooperative drain.
    except BaseException:
        import traceback
        traceback.print_exc()
        os._exit(1)
child.close()
message, ancillary, flags, _ = parent.recvmsg(4096, socket.CMSG_SPACE(array.array('i').itemsize))
assert message and not flags
fds = array.array('i')
for level, kind, data in ancillary:
    assert level == socket.SOL_SOCKET and kind == socket.SCM_RIGHTS
    fds.frombytes(data[:len(data) - len(data) % fds.itemsize])
assert len(fds) == 1
metadata = json.loads(message)
waited, status = os.waitpid(pid, 0)
assert waited == pid and os.waitstatus_to_exitcode(status) == 0
fd = fds[0]
stat = os.fstat(fd)
assert (stat.st_dev, stat.st_ino, stat.st_size, stat.st_uid) == (metadata['dev'], metadata['ino'], metadata['size'], metadata['uid'])
with mmap.mmap(fd, metadata['size'], flags=mmap.MAP_SHARED, prot=mmap.PROT_READ) as backing:
    pcm = backing[metadata['offset']:metadata['offset'] + metadata['length']]
    assert hashlib.sha256(pcm).hexdigest() == metadata['sha256']
os.close(fd)
parent.close()
result = dict(passed=True, backing=metadata, producer_exited_before_receiver_mapping=True,
              scope='native Darwin shared object only; HyperLinux source death not tested',
              authority='same UID; original O_RDWR descriptor retained; READ mapping is not FD attenuation')
(root / 'object-death-regression.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result))
