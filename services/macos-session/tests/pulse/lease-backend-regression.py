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
target = new_pool(1, 0, False)
assert source and target
block = new_block(source, 512)
export = new_export(source, release, None)
kind, bid, sid, offset, size = C.c_int(), U(), U(), Z(), Z()
assert block and export
reserve = function('pa_memexport_enable_original_quarantine', C.c_int, [P])
assert reserve(export) == 0
assert put(export, block, C.byref(kind), C.byref(bid), C.byref(sid), C.byref(offset), C.byref(size)) == 0
assert kind.value == 1
importer = new_import(target, release, None)
assert importer
function('pa_memimport_enable_original_leases', None, [P])(importer)
assert attach(importer, sid, getfd(source), False) == 0
foreign = get(importer, kind, bid, sid, offset, size, False)
assert foreign and origin(foreign) and not ours(foreign)

assert revoke(importer, bid) == -2
assert origin(foreign) and not ours(foreign), 'revoke localized an original lease'
target_export = new_export(target, release, None)
assert reserve(target_export) == 0
assert put(target_export, foreign, C.byref(kind), C.byref(bid), C.byref(sid), C.byref(offset), C.byref(size)) == -2
free_export(target_export)
acquire = function('pa_memblock_acquire', P, [P])
release_block = function('pa_memblock_release', None, [P])
original = acquire(block)
pattern = bytes(range(256)) * 2
C.memmove(original, pattern, 512)
release_block(block)
function('pa_memimport_orphan', None, [P])(importer)
assert origin(foreign) and not ours(foreign), 'orphan localized original backing'
foreign_address = acquire(foreign)
assert C.string_at(foreign_address, 512) == pattern
# Every acquire needs a matching release; map addresses may differ.
release_block(foreign)
# Quarantine source export refs and drop the application ref. The source
# allocator must not reuse that slot or punch it during vacuum.
free_export(export)
unref_block(block)
for _ in range(1024):
    native = new_block(source, 512)
    assert native
    ptr = acquire(native)
    C.memset(ptr, 0xA5, 512)
    release_block(native)
    unref_block(native)
function('pa_mempool_vacuum', None, [P])(source)
ptr = acquire(foreign)
assert C.string_at(ptr, 512) == pattern, 'quarantined original slot reused or punched'
release_block(foreign)
unref_block(foreign)  # Last orphan use destroys importer, not producer quarantine.
# The final release callback may synchronously tear down its transport.
# Orphaning must not free the recursive mutex/importer before callback return.
reentry_events = []
orphan = function('pa_memimport_orphan', None, [P])
@release_type
def reentrant_release(import_pointer, _block_id, _userdata):
    reentry_events.append('callback_enter')
    orphan(import_pointer)
    reentry_events.append('orphan_returned')
reentry_block = new_block(source, 512)
reentry_export = new_export(source, release, None)
rkind, rbid, rsid, roffset, rsize = C.c_int(), U(), U(), Z(), Z()
assert put(reentry_export, reentry_block, C.byref(rkind), C.byref(rbid), C.byref(rsid), C.byref(roffset), C.byref(rsize)) == 0
reentry_import = new_import(target, reentrant_release, None)
function('pa_memimport_enable_original_leases', None, [P])(reentry_import)
assert attach(reentry_import, rsid, getfd(source), False) == 0
reentry_foreign = get(reentry_import, rkind, rbid, rsid, roffset, rsize, False)
assert reentry_foreign
unref_block(reentry_foreign)  # Callback orphans final block synchronously.
assert reentry_events == ['callback_enter', 'orphan_returned']
free_export(reentry_export)
unref_block(reentry_block)

# The first quarantined whole pool still consumes half the 128 MiB budget.
pool2, pool3 = new_pool(1, 0, False), new_pool(1, 0, False)
export2, export3 = new_export(pool2, release, None), new_export(pool3, release, None)
assert reserve(export2) == 0
assert reserve(export3) == -1, 'whole-pool quarantine budget was not enforced'
free_export(export2)
assert reserve(export3) == 0, 'empty exporter reservation was not released'
free_export(export3)
unref_pool(pool2)
unref_pool(pool3)
unref_pool(target)
unref_pool(source)  # Quarantined exporter deliberately keeps source pool alive.
result = dict(passed=True, revoke_refused=True, foreign_producer_export_refused=True,
              orphan_preserved_original=True, allocation_and_vacuum_preserved_pcm=True,
              wholepool_budget_enforced=True, reentrant_final_callback_orphan=True)
(root / 'lease-backend-regression.json').write_text(json.dumps(result, indent=2) + '\n')
print(json.dumps(result))
