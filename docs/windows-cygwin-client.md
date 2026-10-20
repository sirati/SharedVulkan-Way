# Cygwin and MinGW Wayland clients

The native MinGW `wc-shmclient` uses our Windows descriptor dialect. It is
not a libwayland compatibility test. A Cygwin executable also has a different
C ABI from MinGW: x86-64 Cygwin is LP64, while native Windows is LLP64.
Treating a Cygwin fd as a Winsock handle is incorrect on both the descriptor
and socket layers.

The official Cygwin package catalog sampled on 2026-10-20 contains no
libwayland or Weston package. `scripts/wc-cygwin-build.sh` cross-builds a
client-only upstream libwayland 1.24 and the unchanged upstream Weston 1.4
`weston-info` application using native clang/lld on Linux. The official
Cygwin runtime, CRT, FFI library and headers are pinned in
`integrations/cygwin/packages.json`; source hashes are separate in
`sources.json`. The extracted target GCC compiler is never executed.
Native lld links against its CRT objects because clang's Cygwin driver
otherwise delegates linking to a target GCC executable. Compile-time
assertions require the actual Cygwin LP64 ABI. The artifact manifest records
hashes and PE imports and verifies the application imports `cygwin1.dll`.

```sh
scripts/wc-cygwin-build.sh --scratch /mnt/datasets/builds/scratch/cygwin-client
```

This build alone proves neither Windows runtime compatibility nor rendered
Wayland applications. The library integration in `integrations/cygwin/wayland-os.c`
refuses outbound ancillary descriptors with ENOTSUP. Cygwin AF_LOCAL streams
silently drop ancillary data; the optional AF_UNIX backend's sendmsg/recvmsg
are unsupported in the audited source. Pretending SCM_RIGHTS arrived would
turn the first wl_shm pool into a misleading protocol failure.

The portable bundle includes a Cygwin ABI diagnostic and a descriptor probe.
The probe returns 0 only if a live descriptor really transfers, and 2 if
that facility is absent. A missing descriptor is recorded as a limitation,
not a passing buffer test. `weston-info` is a real, unmodified libwayland
application, but it only queries globals and does not map a window.

The separate `wc-cygwin-link` native DLL and in-process libwayland adapter support
one bounded connection across a VM boundary. The library
preserves Cygwin's local socketpair and POSIX readiness for the application,
with no fork or target compiler. With `WC_WAYLAND_RING=1`, libwayland selects
the ring adapter while the unchanged application still calls wl_display_connect. The adapter's
native DLL alone requests the Wayland BAR and doorbell from the privileged
Windows broker. Every control/ring view retains the actual driver mapping
owner. The Linux `wc-cygwin-host` connects the ring to a Linux Wayland socket,
uses an eventfd doorbell wake without a periodic idle poll, and refuses any
compositor SCM_RIGHTS event. It never creates, resizes or truncates the
QEMU BAR backing object. The registry-only layout requires at least 6 MiB; the rendered layout also reserves the header and pixel window.

Build the native bridge through the shared build wrapper, then include it
in the Cygwin bundle:

```sh
/mnt/datasets/builds/wcrun/cg nix develop -c cargo build \
  -p wc-cygwin-link --target x86_64-pc-windows-gnu
/mnt/datasets/builds/wcrun/cg nix develop -c cargo build \
  -p wc-cygwin-link --bin wc-cygwin-host
scripts/wc-cygwin-build.sh --scratch /mnt/datasets/builds/scratch/cygwin-client \
  --bridge-dir target/x86_64-pc-windows-gnu/debug
```

The runtime fixture must be coordinated with the Windows Vulkan client
owner. Use exactly one copied disposable Windows VM, a separate Wayland
BAR at slot 0x1c, and `org.sirati.wcbell.0`. Its Vulkan BAR at 0x1d belongs to
the other transport. Never detach the daily VM's iGPU or assume a ghost PnP
device is present. Keep profiles and HOME/XDG state under a throwaway run
root. A same-machine MinGW test or a registry roundtrip cannot establish a
rendered Cygwin window, a zero-copy wl_shm path, or hardware Vulkan results.

The rendered path below supplies a BAR-backed application mmap/fd adapter
and descriptor translation at the Linux compositor boundary. GPU forwarding
remains upstream Mesa/Zink and is owned by the Windows Vulkan client work.


Real Windows checks on 2026-10-20 used the shared disposable unaccelerated
clone with 3 GiB and four vCPUs. The cross-built Cygwin LP64 executable ran
successfully. Its real socketpair probe sent and received the byte but
received zero descriptors, confirming that SCM_RIGHTS is unavailable.
The separate native MinGW client/server check committed and presented all
30 frames using two named-section buffers and released its HWND/client
cleanly. These were runtime checks on Windows, not Wine. No screenshot or
hardware-rendering claim is made by that same-kernel GDI check.

The reverse registry runtime also passed on real Windows with the same clone.
Unchanged Cygwin `weston-info` enumerated the Linux compositor globals through
broker-lent BAR rings and the doorbell. The host recorded 136 request bytes,
1,008 reply bytes and zero descriptors. The Linux compositor was headless;
this proves registry/roundtrip compatibility only. The clone stopped normally
after the host writer exited. That registry run did not test rendered buffers.

## BAR-backed rendered client path

The rendered integration adds a shipped Cygwin POSIX companion DLL. The upstream
Weston 10 `simple-shm.c` source is pinned and unchanged. It is rebuilt against
that companion and the integrated libwayland; the build checks its PE imports
so `mmap` and `os_create_anonymous_file` resolve to the companion. This does not
claim compatibility with arbitrary prebuilt executables importing only the
original Cygwin runtime.

The companion creates an unlinked local file as an fd identity token, retaining
a duplicate so a closed/reused descriptor number cannot acquire another pool's
identity. It never maps that file. Its shared `mmap` returns the native DLL's
pointer into a newly reserved BAR extent. Application stores therefore land in
the same pages Linux reads. Unrelated mappings call the real `cygwin1.dll`
implementation; unsupported mapping shapes fail explicitly.

In the rendered layout, the existing WCIVSHMEM header occupies the first page,
the pixel arena follows it, and the 6 MiB control/ring region occupies the tail.
Both ends check the pinned nonzero run ID, arena ID, device size and geometry.
The integrated libwayland serializer encodes each supported pool fd as the
existing in-band `ArenaSlice` descriptor. The Linux compositor uses its existing
`WireFd::Named` path. It maps only the read-only pixel window; neither header
nor ring bytes can be addressed by a pool descriptor.

`wc-cygwin-host --qemu-bar QMP DEVICE SLOT ARENA RUN WAYLAND_SOCKET WCBELL_SOCKET`
checks the kernel-authenticated QMP peer, the live PCI slot/function/vendor/
device/BAR2, its linked shared memory backend, and the exact backing inode held
open by QEMU. It retains that descriptor. The compositor's matching
`--arena-qemu-bar id:run:slot:device:qmp` performs the same identity checks.
This is the host side of actual QEMU BAR RAM, without the file-arena fixture
exception or a filesystem crossing the VM boundary.

Allocation is bounded to 64 pools and the BAR pixel window. Extents are not
recycled during the process, so destroying a local pool fd cannot invalidate
pixels still retained by compositor buffers. Disconnect joins both Cygwin pump
threads and marks the ring dead before session teardown. Token holds close at
process teardown; the operating system releases the retained device mapping.
This bounded implementation does not yet support pool growth, arbitrary file
mapping semantics, received keyboard-map descriptors, or clipboard pipes.
Received descriptors of those unsupported kinds are rejected before dispatch.

The bounded host path requires a fresh BAR backend for each session and holds
an exclusive initializer lock. It refuses every previously initialized header,
including an invalidated one. It therefore cannot reassign an old process's
pixel extents after a host restart. A long-running service with independently
restarted clients needs a broker-owned allocation and retirement protocol;
this proof does not substitute a reused offset for that protocol.


The rendered path passed on real Windows on 2026-10-20. The unchanged rebuilt
Weston simple-shm client mapped one 250,000-byte BAR pool directly. The Linux
headless compositor presented 12 frames with 12 distinct pixel hashes. A final
BAR snapshot matched a hash recorded by an actual presentation. The control
rings carried 1,216 request bytes and 1,376 reply bytes, with zero kernel fds.
The client exited normally, joined both pumps, closed its retained token fd,
and marked the ring dead. Wrong PCI slot, Linux run identity, Windows run
identity and backend reuse checks all refused the invalid operation. A real
Cygwin unrelated anonymous mmap/munmap fallback also passed.

Evidence is under
`/mnt/datasets/builds/evidence/windows-cygwin-client/render-real-windows-2`.
The PNG is a forensic capture after both applications exited. It is outside
the live transport. No screenshot inspection or hardware GPU rendering is
claimed. This run also repeated the native MinGW same-kernel GDI test with
30 committed/presented frames and normal teardown. That native result does
not establish a native MinGW cross-VM pixel path. The disposable clone stopped
normally after both tests.


The POSIX boundary refuses any fallback mapping operation overlapping the
retained device mapping, including interior/partial unmaps, inactive pool
extents and fixed mappings for unrelated fds. Overflowing ranges also fail.
The native mapping owner remains valid independently of local token state.

The QMP socket and device configuration are trusted operator inputs. The
executable basename check detects unsupported launchers; it does not authenticate
arbitrary executable paths. Attach repeats the device's memory-backend link,
sharing, size, path and retained inode checks before returning. Device hotplug
and backend reassignment must remain disabled for the attached session.

## Allocation leases under development

The next API uses a generation-checked `BarLease` descriptor. An allocation
reserves address-stable capacity inside the original fixed BAR. Only its
logical length is initialized or mapped; growing it initializes newly exposed
bytes in place. It cannot extend the physical BAR or move existing pixels.
The default reservation is four times the next power of two of the initial
size, with a 1 MiB minimum. `WC_BAR_POOL_CAPACITY` selects an explicit capacity.
Exhaustion fails rather than switching backing or copying pixels.

The directory reserves the first 128 KiB of the arena and supports 1,024 live
allocations with generation-checked slot reuse. The host output partition is
separate from guest pixels. Linux maps guest pixels read-only, and maps only
atomic metadata and its own output partition writable. A pending wire transfer
retains the allocation before the descriptor is emitted. Receiving it converts
that reservation to a consumer lease. The last consumer Handle or Mapping must
drop before reuse, independently of producer close. A stuck metadata lock
returns an error without stealing the lock or releasing any reference.

This shared directory is cooperative coordination state. Client-writable
metadata does not authenticate a producer or authorize privileged retirement.
The broker integration requires separate trusted allocation records and an
actual retained process HANDLE proven exited. The cooperative directory has
no production retirement API. Kernel-death recovery must use independent
protected allocation and consumer records. DLL teardown retains outstanding local
capabilities and mappings; it does not pretend they completed or that the host
drained. Protected broker/driver integration is separate work.

The POSIX companion now tracks fd duplication, explicit close and each mapping.
It supports incoming read-only keymap tokens and logical `ftruncate` growth.
The library decoder materializes typed incoming descriptors before event
dispatch. These new paths require their own runtime acceptance. The earlier
12-frame evidence above remains the validation boundary for the previous
bounded implementation. Clipboard stream descriptors are not implemented by
this API stage.

Allocation directories require header format 3 and control wire version 2.
The old registry-only wire version remains 1, and ordinary ivshmem publishers
retain format 2. A retained format-2 client must reject format 3 before it can
claim rings or write pool offset zero. New lease clients require format 3
explicitly. This prevents an older allocator from overwriting directory bytes.
