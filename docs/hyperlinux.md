# Hyper Linux topology

Hyper Linux runs a Linux ARM64 ELF process inside one macOS `hl` process. It
implements Linux syscalls in that process. There is no Linux kernel process.
Linux fork starts another `hl` through `posix_spawn` and transfers descriptors
and an address-space snapshot. This integration targets upstream revision
`8048a96ba0abc7e2f548da684690bf415c99214e`.

The integration uses the existing native compositor as the other process.
Graphics buffers stay in host shared mappings. Unix sockets provide native
Wayland operations, descriptor transfer, bootstrap and doorbells. There is no
TCP graphics route, X server, XWayland fallback, guest proxy process or transport
bridge process. The runtime patch removes Hyper Linux's automatic XQuartz bind
and DISPLAY rewrite. The launcher selects Wayland backends and unsets DISPLAY.

| Responsibility | Location and owner |
| --- | --- |
| Application, libwayland, Vulkan loader, sv-icd, upstream Mesa Zink and VA frontend | Linux ARM64 ELF closure, executing inside `hl` |
| Retained host file mapping | Native Rust `wc-hyperlinux` static library linked into `hl` |
| Guest VA, Stage-1 pages, Stage-2 backing and Linux mmap lifecycle | Thin C adapter in `hl`, serialized by its existing mmap lock |
| Per-descriptor flags, dup aliases, Unix readiness and SCM_RIGHTS | Hyper Linux's existing descriptor table and host descriptors |
| Wayland socket and buffer importer | Existing native compositor |
| GPU cold handshake, arena import, control region and session dispatch | `sv-gpu-server hl-local`, embedded as a thread through `sv-guest wayland --with-gpu` |
| GPU wake bytes | Existing vdoorbell token protocol on local native Unix streams, with ControlSet wake dispatch |
| Native D-Bus, PulseAudio and implemented portal interfaces | Native per-user session services package, separate from the transport |

## Mappings and lifecycle

Upstream Hyper Linux copies ordinary file-backed mmap contents into its primary
buffer, including MAP_SHARED mappings. That path cannot carry wl_shm, shared ICD
arenas or PulseAudio SHM. Our patch intercepts file-backed MAP_SHARED before
that copy path. Rust duplicates the host descriptor, maps its backing with
MAP_SHARED and owns that view. It aligns the host mapping to the native page
size and reports the Linux 4 KiB file offset separately.

The C adapter reserves a free IPA interval from the existing reserved backing
band. Its allocator checks both SysV SHM and our mappings. It installs exact
4 KiB Stage-1 entries, so a Linux-page offset need not equal a macOS-page offset.
Fresh guest VAs use Hyper Linux's existing gap allocator and its full mmap
interval. Existing guest regions determine address occupancy. Shared mappings
therefore coexist with ordinary mappings and MAP_FIXED replacements. There is
no separate fixed allocation slot table. The host's sparse
mapping commits memory on demand.

The address resolver consults shared views before the original SysV resolver.
SysV and external mappings therefore coexist. Shared mappings have distinct
backing owners and view records. Partial munmap or mprotect splits a view and
retains its owner. Closing, duplicating or replacing a descriptor does not revoke
its live mapping. Final unmap removes guest access before removing Stage-2 and
releasing Rust's host view. A failed Stage-2 removal retains its backing and
IPA reservation until a successful VM destruction. It restores the primary
reservation without zeroing
external storage. Exec removes external views before the ordinary reset zeroes
private guest memory.

Fork IPC version 8 transfers each live view's numeric VA, length, protection and
file offset with an independently owned CLOEXEC descriptor. The child maps that
same kernel object through Rust and installs its own Stage-2 mapping. Export first retains a numeric-record/fd snapshot under mmap_lock, then
releases that lock before blocking IPC. No Rust pointer is serialized, and a primary-buffer snapshot does not replace external
shared storage. Descriptor aliases and guest close-on-exec remain in the existing
fd graph. Host descriptors use CLOEXEC independently of guest exec eligibility.
A mapped descriptor is duplicated under the existing fd lock, then the lock is
released before Rust or HVF runs. This also covers upstream legacy entries that
have not yet acquired a ref-counted descriptor object.

Shared executable mappings and Rosetta shared mappings currently return explicit
errors. They do not use the old payload-copy path. They require separate mapping
and W^X validation before this runtime supports applications that need them.
The adapter runs native msync on external shared backing. Copied private mappings retain upstream limitations and require application
and sandbox validation. Native host mappings are not a claim that every guest
syscall now enforces VMA access permissions as a Linux kernel would.

## Native protocol endpoint

`sv-gpu-server hl-local --runtime DIR --socket DIR/sv-gpu --bell DIR/sv-gpu.bell`
accepts the existing AttachRequest and AttachRequestFds messages. DIR must be a
private directory owned by the native uid. Both socket peers must have that uid.
Each session gets a shared backing file, Control header and ordinary Vulkan host
import. Attach uses the existing protocol IDs and generation. The file name is
removed only after the client acknowledges mapping it. Both endpoints keep
independent mappings afterward.

The endpoint broadcasts existing one-byte RESCAN tokens through nonblocking
native sends and dispatches incoming tokens to the existing ControlSet. A queued
rescan covers publications whose later notification coalesces into a full socket
buffer. Reconnect sends a rescan. Normal disconnect marks the ring fatal and
wakes both lanes before the backing is retired. Native reads
remain blocking. No intermediate hub process is started. GPU command payloads,
responses and buffers use the shared mappings.

Session cleanup waits for device idle and destroys known leftover objects before
freeing imported Vulkan memory. A live worker, failed drain or unknown destructor
retains that complete session's backing. It is not returned to another client.
A process crash still depends on the native driver's process cleanup guarantees;
this is distinct from the broker's cross-VM retained-memory problem.

## Identity, D-Bus and PulseAudio

The pinned runtime reports uid/gid 1000 regardless of its native owner. Our patch
reports native uid/gid through Linux identity queries, the synthetic account
files and proc status. Linux SO_PEERCRED uses Darwin's kernel peer uid/gid and
LOCAL_PEERPID. A Linux SCM_CREDENTIALS message is validated against the native
identity and guest process ID, then omitted because the native service obtains
its peer identity from the kernel. Linux cmsg type 2 must never be forwarded as
Darwin SCM_TIMESTAMP.

The factory binds the private native runtime directory as `/run/user/UID` and
also at its native path. The second bind preserves names supplied by the native
GPU endpoint. It selects the native bus, libpulse socket and cookie. It does not
disable PulseAudio SHM. A constrained `/dev/shm/pulse-shm-DIGITS` adapter uses
Darwin shm_open/shm_unlink on `/pulse-shm-DIGITS`, with native owner checks. These
are the same POSIX SHM objects used by upstream native PulseAudio. Other guest
SHM files use the bound per-user shm directory.

Native services and portal methods are implemented by the separate session
services package. Successful socket startup alone does not establish working
D-Bus EXTERNAL authentication, portal request methods, native consent or audio
SHM negotiation. Tests must establish those behaviors with unchanged Linux
clients. Linux SO_PASSCRED ancillary synthesis and all native credential variants
are not yet proven. Cookie authentication is not evidence of audio zero-copy.

## Nix selection

`nix/hyperlinux/runtime.nix` applies the source patch, copies the thin adapter
and C header and links `libwc_hyperlinux.a`. It requires a Linux build platform
and ARM64 Darwin host platform and preserves entitlement signing after final
installation. `backend.nix` builds the native static library. No Darwin library
is placed in the Linux loader's search path.

`mk-app.nix` replaces the example factory's X11/AppKit launch route. It returns
a standard Nix app entry and its launcher/closure. Its default Linux ICD comes
from `icd.nix`. Its default Mesa is an exact copy of the shared stock-Zink
package definition, upstream revision `42c325ea02518276f08c5b44b487e37affc9fe69`,
with no Mesa/Zink patch. `VK_DRIVER_FILES` and `VK_ICD_FILENAMES` select the Linux
ELF ICD. The launcher selects Zink for GL and Zink's upstream VA frontend for
VA-API. It supplies no Firefox preferences or sandbox bypasses.

The Linux ELF interpreter is resolved through a sysroot link to the ARM64 glibc
loader. The full Linux closure remains at its original store paths; `/nix/store`
is explicitly bound read-only so PT_INTERP, RUNPATH and absolute dlopen paths
remain valid. Our guest library search precedes the sysroot fallback. Native
Vulkan driver discovery is a separate argument to the compositor service and
must name a Darwin manifest, never the guest sv-icd JSON.

`compositor.nix` generates an idempotent per-user launchd ensure command for the
existing combined `sv-guest` binary. It starts the compositor on its main thread
and the local GPU endpoint on a thread. It never removes a live endpoint owned
by another build. Session services use their own `--ensure` command. App exit
does not stop another application's compositor, bus or PulseAudio daemon.

`stack.nix` assembles the runtime, native compositor, session services, stock
ARM64 Linux Firefox, Linux ICD and upstream Mesa into the public
`apps.aarch64-darwin.hyperlinux-firefox` output. It uses the existing native
KosmicKrisp provider and authorized MTLTexture import patch, with separate native
Vulkan-loader discovery. The native session service declares its Mach section
service in launchd and shares the existing broker allocation lifecycle.
`apps.aarch64-darwin.hyperlinux-shared-probe` is a separate headless acceptance
entry, with isolated HOME/XDG and null audio. All custom compilation and assembly
runs on Linux. Realize and copy the complete closure on vmhost before `nix run` on
the Mac. Output evaluation is not deployment or a Darwin runtime test.

## Validation

The first native test runs `wc-hl-shared-peer` against the Linux C client in
`nix/hyperlinux/shared-client.c`. Only wake bytes cross the socket. Three exchanges
publish and read shared atomics in both directions. The client closes and
aliases fds, changes protections, partially unmaps, forks a writer, executes a
fresh image and verifies FD_CLOEXEC. All HOME/XDG paths are isolated. Native Linux
success validates the test and kernel sharing; it does not validate HVF.

The ARM64 Linux client passed on the real Neo ARM64 Mac through the signed
linked hl binary. It exercised shared atomic exchange and wakes in both
directions, SCM_RIGHTS in both directions, a received descriptor mapped at a
4 KiB offset, alias close, protection restoration, partial unmap and private
MAP_FIXED replacement, msync, fork and guest exec/CLOEXEC. The host peer uses
the same native Rust backing code as the embedded library. This establishes
the shared-page sub-gate. An upstream Wayland client sharing its original
wl_shm buffer with the existing compositor is a separate gate. Then test Vulkan, Zink,
VA-API, D-Bus, portal requests, PulseAudio shared memory, disconnect and crash
cleanup. No native Mac GUI, services, signing settings or VM state have been
changed by offline validation.

## Embedded render node and immutable PRIME identity

The native library calls the same `sv-drm::drm::DrmNode` ioctl state machine as
the full Linux CUSE implementation. Its bounded metadata and relay ABI are now
portable modules. The hl device registry supplies `/dev/dri/renderD128`, Linux
character identity 226:128, per-open GEM handles and existing cap/version
responses. Dup aliases share the existing open description. A retained empty
pipe gives the node a quiet native readiness descriptor. No CUSE process or
Linux kernel driver is started. The sysroot includes ordinary `/dev/dri`
enumeration and a PCI-shaped `/sys` hierarchy for unchanged libdrm/Mesa discovery.
`SV_DRM_NODE` selects that same rdev for the Linux ICD's DRM properties.

The private connect ioctl validates the caller's actual same-uid Unix stream
socket and configured service name. It passes that descriptor to the native
GPU endpoint's `.relay` socket. The server serves the original socket directly;
no thread forwards GPU payload bytes between sockets. The existing shared arena
and rescan protocol remain the data and notification paths.

Darwin does not provide Linux kernel file seals. The guest syscall adapter
therefore protects reserved inode xattrs, retaining seal state across dup,
SCM_RIGHTS, fork and exec. Independently opened native flock handles outside the
rooted guest binds serialize seal changes and native writes/resize operations.
The adapter pins the descriptor before mutation. Guest memory resolution runs
outside this lock. Shared writable mapping reservations are recorded on the
same inode and survive sibling hl processes. New writable mappings and native
writes fail after WRITE/FUTURE_WRITE sealing, and shrink/grow restrictions are
checked separately. Previously authorized writable sync-page mappings remain
live. A crashed mapper can leave a conservative reservation. The adapter never
clears one based on a reused PID; the bounded per-file table can consequently
reject later forks or writable mappings after repeated crashes.

A PRIME header is accepted only as a canonical bounded Token encoding, with
shrink/grow/reseal protection and a write seal. An existing writable header view
makes publication fail with EBUSY. Only the token's sync page may retain a prior
writable view. The embedded runtime registers the original descriptor and at
most 448 immutable metadata bytes on the compositor's `.prime` endpoint.
Descriptor and mapping owners independently retain that registration connection.
A child reconstructs its local registration from protected inode metadata,
never a serialized Rust pointer. Native imports use the immutable retained
capability and existing broker allocation ownership. They do not interpret
synthetic F_GET_SEALS as a Darwin kernel guarantee. Changed header bytes are
rejected rather than used to change allocation identity. Frame pages are not
copied by metadata registration.

Linux checks cover the actual shared-inode seal adapter across duplicate fds
and fork, native write/resize rejection, header-writable mapping rejection and
native header mutation detection. A linked C ABI check drives the embedded
DRM core through version strings, invalid guest ranges, caps, repeated PRIME
imports, GEM close and private relay. Native Darwin checks compile the same
code. The shared-page HVF gate passed. Unchanged application sandboxes and
native presentation still need runtime evidence; these checks alone do not establish accelerated rendering or
VA decode.

The upstream fork implementation does not atomically stop sibling vCPUs while
it serializes memory and metadata. This integration therefore rejects process
fork with EAGAIN unless the caller is the only active guest thread and there
are no active shared-VM clones or live embedded render-node descriptions. That prevents a concurrent guest thread from
changing mappings or descriptor state during the snapshot. Applications that
fork after creating threads require a future stop-the-world implementation.
The headless acceptance client tests the supported quiescent case. Live Vulkan
contexts inherited across fork still need explicit driver validation.

## Public package outputs

The flake exposes `packages.aarch64-darwin.hyperlinux-firefox` and the matching
app, together with `hyperlinux-runtime`, `hyperlinux-native` and the
headless `hyperlinux-shared-probe` app. `nix/hyperlinux/stack.nix` assembles
the native runtime, compositor, native session services, stock ARM64 Linux
Firefox, Linux ELF ICD and stock upstream Linux Mesa. Its separate pinned
runtime nixpkgs supplies the cross toolchain. Cached application and native
Darwin dependencies use the main nixpkgs pin. The native KosmicKrisp provider
keeps the existing authorized import patch; this does not patch Linux Mesa.

The headless app runs both the shared-page fixture and an embedded render-node
ABI/seals fixture in private temporary HOME and XDG directories. It creates
no window and starts no session service. Package construction and these
acceptance fixtures do not establish default application acceleration, Zink
presentation or VA-API decode. Those require the complete unchanged Linux
client stack against the native compositor and its selected driver.
