# Local Windows Wayland applications

A Windows compositor and applications on the same Windows machine do not need
a VM, ivshmem device, privileged broker or doorbell. Their protocol connection
uses Windows AF_UNIX, and their wl_shm pixels occupy the original pagefile-backed
named sections. The compositor opens those sections directly.

`wc-local-client` provides a fixed-width C ABI in
`integrations/windows-local/wc-local-client.h`. It wraps the existing
`wc_transport` local socket and `wc_handle` named-section implementation. It
adds no allocator or keeper process. Each endpoint lives in a protected full-SID directory. The adapter audits its
owner, DACL and ancestry, rejects reparse directories, and retains directory
and exact socket file handles that prevent replacement. The server seals the
AF_UNIX socket to the current SID and SYSTEM before publishing readiness.
Clients audit and pin that actual socket before connecting. Windows enforces
socket write access on connection. This uses filesystem access control, not a
PID lookup. Other users and unsupported ACLs fail closed. The local entry refuses the BAR endpoint and BAR selection env.

Section descriptors use the existing canonical variable-length codec. This
boundary accepts only bounded `Local\wc-` Section names, not raw HANDLEs,
shared-file paths or BAR references. Descriptor length is at most 4096 bytes;
section size is at most 1 GiB. There are at most 256 connections, capabilities
and mappings each. Process-local identities increase monotonically and are
never reused. Clones retain the original capability. Every exposed view
retains its capability and import mapping until explicit unmap, even after
all fd aliases close. Imports can be mapped read-only, never writable.
Exporting a descriptor is a pure operation. Its sender must retain its backing
until the receiver has opened it; an application's live pool mapping supplies
that retention. Windows retains an opened consumer view independently.

## Two separate application ABIs

`scripts/wc-mingw-build.sh --scratch PATH` builds native LLP64 libwayland-client
and unchanged upstream Weston weston-info and simple-shm. Its shipped OS shim
provides the missing client socket/readiness, anonymous-section, mapping and
fd APIs. Only libwayland's platform integration and descriptor codec change.
The native runtime has its own fd namespace, so applications and libraries
that operate on those fds must use the companion headers/runtime. It is not a
claim that arbitrary prebuilt Windows binaries or toolkits have a Wayland
backend. It does not load Cygwin. Compile-time assertions check 32-bit long and
64-bit pointers, and PE import checks reject Cygwin dependencies.

`scripts/wc-cygwin-local-build.sh --scratch PATH` builds a separate genuine
LP64 Cygwin library/application bundle. A real Cygwin socketpair preserves its
POSIX display fd and poll behavior while in-process pumps reach the native
local socket. A POSIX companion associates real Cygwin token fds with native
named sections. These files contain no pixel data. mmap returns the original
native view; close, dup, dup2, fcntl and munmap preserve its lifetime. This
adapter is separate from the active cross-VM BAR adapter and its fixed-size
lease/stream descriptor dialect. Neither existing BAR files nor their runtime
fixture are changed by these builds.

Both builds pin and verify upstream source hashes and record artifact hashes,
PE imports and adapter source hashes. The application source files are checked
again after compiling. Their sources remain unchanged; they link to the
shipped library and portability runtime. This does not establish compatibility
with arbitrary prebuilt Cygwin executables whose libc imports bypass that
runtime.

## Current boundaries and validation

This first local adapter supports wl_shm section resources, not arbitrary fd
kinds, clipboard pipes or native Vulkan WSI. A section is allocated at its
requested size. Enlargement/ftruncate and mremap of an existing allocation are
explicitly unsupported, rather than copied into new backing. An application
may allocate another section. Native poll currently accepts one display fd.
The Cygwin pump supports one display connection per process and refuses a
second connection instead of replacing its owner. Cygwin children refuse
copied native capabilities and do not join parent pump threads. General
fork/daemon and toolkit compatibility are not claimed.

A separate POSIX contract probe checks original shared bytes, descriptor
import, fd aliases, creator close, read-only consumer views and zero retained
native resources after explicit close/unmap. It is not the ordinary application
proof. Ordinary application acceptance uses actual Windows, separate MinGW and
Cygwin ABI stamps, upstream registry enumeration, and unchanged simple-shm
rendering through native HWNDs. Optional frame dumps observe the original
section for evidence; presentation still draws from that section directly.
The harness closes only its owned compositor HWND through WM_CLOSE, then
requires normal application and window teardown. It never injects global input
or changes services, drivers or raw-resource policy.

Source/build gates passed in the initial implementation. Actual Windows local
ordinary-application acceptance remains pending the coordinator's exclusive
disposable fixture assignment. Wine, an own protocol client and cross-VM runs
are not substitutes for that gate.
