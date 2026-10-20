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
source = new_pool(1, 0, False)
target = new_pool(2, 0, True)
assert source and target
block = new_block(source, 512)
export = new_export(source, release, None)
kind, bid, sid, offset, size = C.c_int(), U(), U(), Z(), Z()
assert block and export
assert put(export, block, C.byref(kind), C.byref(bid), C.byref(sid), C.byref(offset), C.byref(size)) == 0
assert kind.value == 1
importer = new_import(target, release, None)
assert importer and attach(importer, sid, getfd(source), False) == 0
foreign = get(importer, kind, bid, sid, offset, size, False)
assert foreign and origin(foreign) and not ours(foreign)
assert revoke(importer, bid) == 0
assert origin(foreign) and ours(foreign), 'localization erased origin'

# Exercise rejection after localization, through the real pstream/mainloop.
for name, result, args in [('pa_mainloop_new', P, []), ('pa_mainloop_get_api', P, [P]), ('pa_mainloop_iterate', C.c_int, [P, C.c_int, C.POINTER(C.c_int)]), ('pa_mainloop_free', None, [P])]:
    fn = getattr(pulse, name)
    fn.restype, fn.argtypes = result, args
loop = pulse.pa_mainloop_new()
api = pulse.pa_mainloop_get_api(loop)
a, peer = socket.socketpair()
io = function('pa_iochannel_new', P, [P, C.c_int, C.c_int])(api, a.fileno(), a.fileno())
a.detach()
stream = function('pa_pstream_new', P, [P, P, P])(api, io, target)
function('pa_pstream_require_original_pool', None, [P])(stream)
deaths = []
notify_type = C.CFUNCTYPE(None, P, P)
notify = notify_type(lambda *_: deaths.append(True))
function('pa_pstream_set_die_callback', None, [P, notify_type, P])(stream, notify, None)
class Chunk(C.Structure):
    _fields_ = [('memblock', P), ('index', Z), ('length', Z)]
chunk = Chunk(foreign, 0, 512)
function('pa_pstream_send_memblock', None, [P, U, C.c_int64, C.c_int, C.POINTER(Chunk), Z])(stream, 0, 0, 0, C.byref(chunk), 0)
assert not function('pa_pstream_is_pending', B, [P])(stream), 'refused frame queued'
status = C.c_int()
assert pulse.pa_mainloop_iterate(loop, 0, C.byref(status)) >= 0
assert len(deaths) == 1, deaths
peer.settimeout(1)
assert peer.recv(1) == b'', 'refused stream sent bytes'
function('pa_pstream_unref', None, [P])(stream)
pulse.pa_mainloop_free(loop)
peer.close()
unref_block(foreign)
foreign_on_close = get(importer, kind, bid, sid, offset, size, False)
assert foreign_on_close and origin(foreign_on_close) and not ours(foreign_on_close)
free_import(importer)
assert origin(foreign_on_close) and ours(foreign_on_close), 'importer close erased origin'
unref_block(foreign_on_close)
free_export(export)
unref_block(block)
# Recycled constructor structs must not retain the foreign origin bit.
new_any = function('pa_memblock_new', P, [P, Z])
new_fixed = function('pa_memblock_new_fixed', P, [P, P, Z, B])
free_type = C.CFUNCTYPE(None, P)
noop_free = free_type(lambda *_: None)
new_user = function('pa_memblock_new_user', P, [P, P, Z, free_type, P, B])
buffer = C.create_string_buffer(512)
for _ in range(200):
    for native in (new_block(source, 512), new_any(source, 128 * 1024),
                   new_fixed(source, buffer, 512, False),
                   new_user(source, buffer, 512, noop_free, None, False)):
        assert native and not origin(native)
        unref_block(native)
unref_pool(target)
unref_pool(source)
result = dict(passed=True, localization_preserves_origin=True, importer_close_preserves_origin=True,
              real_failure_callback_count=len(deaths), queued=False,
              received_bytes=0, recycled_native_blocks=800)
(root / 'origin-regression.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result))
