# macOS Vulkan client and PCI access

The first guest-driver target is **Intel macOS in a VM with a host-provided
ivshmem PCI device**. The source is in `drivers/macos-ivshmem`. It is an IOKit
kernel extension and a root-only userclient, not a Wayland client. The kernel
extension is not installed or loaded by any build/check script.

This is preparatory client support. A successful offline cross-build is not
proof that a stock macOS guest can yet run a Vulkan application through it.
The runtime and broker integration gates below remain open.

## Why IOKit for this target

Public IOKit provides `IOPCIDevice::getDeviceMemoryWithRegister`,
`IOSubMemoryDescriptor::withSubRange`, and
`IOUserClient::clientMemoryForType`. These describe and map a physical PCI
aperture without copying its bytes into register RPC replies or guest heap
buffers. The host GPU can use the host mapping of the same backing pages.

DriverKit would avoid a kernel extension. Public PCIDriverKit documentation
exposes register accesses and BAR information, but no documented method to
return a BAR memory descriptor to a third-party driver. Apple's open-source
PCIDriverKit implementation calls `_CopyDeviceMemoryWithIndex` internally.
That method belongs to `IOPCIDevicePrivate`, not the supported public class.
The implementation also has architecture/platform mapping restrictions. The
installed macOS SDK's kernel proxy headers expose generated internal methods;
that does not make them a supported DriverKit API. This implementation does
not call them and does not ship an experimental DEXT fallback.

The local cross toolchain has macOS SDKs with public IOKit kernel headers and
kernel support archives. It does not contain the separate DriverKit SDK or a
Linux-native IIG compiler. A future DriverKit implementation needs a documented
mapping mechanism plus that toolchain, approved PCI entitlements, signing and
an activation application. It must not substitute a register-copy transport.

References:

- [Public PCIDriverKit class](https://developer.apple.com/documentation/pcidriverkit/iopcidevice)
- [Public IOKit PCI device](https://developer.apple.com/documentation/kernel/iopcidevice)
- [Apple PCIDriverKit interface](https://github.com/apple-oss-distributions/IOPCIFamily/blob/main/PCIDriverKit/IOPCIDevice.iig)
- [Apple kernel descriptor implementation](https://github.com/apple-oss-distributions/IOPCIFamily/blob/main/IOPCIDevice.cpp)
- [Apple userclient privilege check](https://github.com/apple-oss-distributions/xnu/blob/main/iokit/Kernel/IOUserClient.cpp)

## Driver contract

Only PCI vendor/device `1af4:1110` matches. `WC_PCI_OPEN_TYPE` is the only
accepted connection type. Both `newUserClient` and `initWithTask` call the
kernel's `clientHasPrivilege(task, kIOClientPrivilegeAdministrator)`. On macOS
that check requires the task security token's uid to be zero. It does not
accept a supplied uid, a group membership, or a protocol secret.

The shared `wc-broker` platform backend opens the driver. Desktop processes
must receive only their authorized region capabilities and bell leases from
that broker, never its raw IOKit connection. The adapter is
`wc_broker::sys::macos_pci`; it refuses a non-root process before opening IOKit.

The scalar ABI lives in `abi.h`:

| Operation | Input | Output |
| --- | --- | --- |
| INFO | none | ABI version, BAR2 length, kernel page size, MSI-X vector count, peer id |
| SELECT | BAR2 offset, length | none; selects one immutable page-aligned range |
| RING | destination peer id, vector | none; writes only the ivshmem doorbell register |
| ARM, asynchronous | last observed IRQ count | callback receives count and timestamp |
| map memory type 1 | selected connection | exactly its selected BAR2 subdescriptor |

BAR2 is selected by its PCI configuration register, not the compact device
memory-array index. A 64-bit BAR consumes two PCI BAR slots. Bounds checks
avoid offset-plus-length overflow. Each connection returns the same memory
descriptor for memory type 1 throughout its lifetime. SELECT cannot replace a
live mapping. The driver does not expose arbitrary PCI configuration accesses,
physical addresses or BAR0. It maps exactly the selected range; the broker
must select only the region its existing core has authorized.

For doorbell devices the driver uses MSI-X interrupt source zero. It publishes
shared-memory writes before writing BAR0's doorbell. RING accepts a 16-bit peer
and a vector within the device's MSI-X table. Exactly one broker connection
owns the receive bell; other connections can hold region mappings. ARM uses a
monotonic interrupt count to handle an interrupt that arrives before rearming.
The broker must scan rings on connect and after every wake, then arm and scan
again before blocking. Wakes are hints; product bytes remain in shared memory.
Provider stop first takes the device lock, marks it stopped and detaches its
resources and bell owner. INFO, SELECT, RING, bell claim and user-map descriptor
acquisition use that same lock and refuse new calls after the stop checkpoint.
Stop drains/removes the IRQ source and releases detached resources after
unlocking. It calls the detached bell owner and balances its retain outside
the device lock, so the client-lock-to-device-lock order cannot deadlock with
provider stop. An already acquired subdescriptor retains its parent. These
source/lifecycle rules still need runtime hot-unplug and in-flight-call tests.

Plain ivshmem has no interrupt backend. It can map memory but ARM/RING fail;
plain-device mapping alone does not satisfy the latency/wakeup requirement.

## Ownership and dynamic memory

This path maps **host-provided PCI BAR pages** into a macOS guest. It is separate
from the existing macOS-owned POSIX-shm/guest-RAM backend. It does not wire
reclaimable anonymous guest pages, scan PFNs, allocate a RAM disk, or copy bytes
through a Unix socket, SSH, TCP, or vsock.

BAR2's aperture size is fixed by the virtual PCI device while the VM runs. A
host may reserve a large lazily backed aperture and allocate variable-sized
regions within it. It may discard/reuse backing only after the shared broker
and region protocol have drained GPU leases and observed the peer's UNMAPPED
acknowledgement. A finite BAR is not unbounded growth. Growing beyond its
aperture needs an additional supported device/aperture protocol; resizing it
under live mappings is not implemented.

Each region maps once and keeps its address throughout its epoch. Additional
regions receive separate mappings, so growth must not relocate previous GPU
addresses. The driver does not choose allocation policy or free extents when
a client closes. BAR backing belongs to the host VM/device lifetime. A broker
crash therefore does not return these host-provided pages to the guest page
allocator. The host must still retain live backing until acknowledged release.
This does not solve crash safety for the separate wired guest-RAM backend.

## Broker and ICD attachment

The multi-user capability is held. The preparatory VM-wide single-uid gate
below is mandatory and does not complete per-range ownership.

The root broker can publish an explicitly configured BAR2 slice with
`serve --pci-slice UID:POOL:REGISTRY:OFFSET:LEN:EPOCH`. Integers accept decimal
or `0x` hexadecimal. Registry IDs identify `WCIVSHMEM` devices, not arbitrary
paths. Slices must contain whole driver pages, stay within BAR2, and not overlap
another configured slice. The uid is root configuration, never client input.
The same functionality is available to an embedding broker through
`macos_pci::publish`.

Publication probes public `mach_make_memory_entry_64` export before making the
region visible. I/O-memory export can refuse on a target kernel. That refusal
propagates to the caller, with no copied allocation or file substitute.

A client first obtains an ordinary `find` lease on the existing Unix broker
connection. The reply marks a PCI piece `mach`, with no SCM_RIGHTS fd. A
LaunchDaemon MachServices endpoint, `org.waycompose.broker.memory`, redeems
that piece. The kernel audit trailer supplies the sender's effective uid and
pid. The shared Core must find a connected, positive lease with that exact
identity and epoch, including for root callers. The existing broker mutex
remains held through memory-right transfer or a doorbell write. There is no
second authorization manager, secret, or pending-token table.

The Mach endpoint checks in through launchd and never falls back to registering
an unprivileged service. Clients require a root audit identity on each reply,
which rejects service substitution in a user's bootstrap namespace. Requests
and replies use fixed sizes and validate descriptors, operation, nonce, epoch,
and piece geometry. Rejected received messages destroy their attached rights.
Successful receivers retain the Mach send right, a shared `mach_vm_map` view
with `copy=false` and `VM_INHERIT_NONE`, and the ordinary broker connection.
Every `SharedRegion` slice retains this owner. File-only APIs reject `mach`
pieces instead of indexing an empty descriptor list.

Set `SV_SOCKET=broker-pci:POOL` and `SV_BELL=pci:PEER:VECTOR` for the Intel macOS
ICD path. It uses the existing pool mailbox and `poolattach::check` offer
validation, including protocol IDs, pool nonce, geometry, bounds and stamps.
The common attach function claims rings and starts heartbeats. Arena use must
remain leased and entirely inside the selected aperture; host-region growth
outside it is refused.

Doorbell writes stay in the root broker and require the same lease. Application
producer calls enqueue a coalesced hint without blocking on Mach IPC. One
root IOKit async IRQ owner per PCI registry ID shares counted hints among its
published slices. A bounded Mach wait retains the mapping while sleeping
outside the Core lock, then rechecks the lease before replying. Eight wait
workers have a separate bounded queue from two immediate-operation workers,
so blocked IRQ waits cannot occupy redemption or outgoing-bell capacity. The client
rescans its process-owned control set and cold mailbox on each changed IRQ
count. Wait expiry does not release a mapping or a region lease. This entire
Mach/IRQ path is offline-compiled only and requires the runtime proof below.

The macOS manifest uses `./libsv_icd.dylib`, so the library resolves beside the
manifest without relying on dyld search paths.

## Offline builds and runtime gates

Use `nix/macos-ivshmem.nix` with the result of the existing
`darwin/cross-toolchain.nix` as `toolchain`. It provides native boundary checks,
an aarch64 macOS ABI-check executable, an **unsigned Intel kext**, and a source
package. The aarch64 check tests arithmetic only; the driver is Intel-only.
Use `--max-jobs 2 --cores 8` for Nix builds.

`build-kext.sh` accepts `MACOS_SDK`, Linux-native `DARWIN_CLANGXX` and classic `DARWIN_LD`, and `WC_PCI_KEXT_OUT`. It cross-compiles public IOKit source and
links a Mach-O kext with the SDK's `libkmodc++`/`libkmod`.
`nix/cctools-kext.nix` provides a pinned Linux-native classic ld64 because
LLVM ld64.lld does not implement kext output. It disables TAPI, XAR and LTO;
the driver uses compiled object/archive inputs and unresolved kernel KPIs. Kernel imports remain
undefined for the target kernel to resolve. Offline linking cannot establish
that every imported KPI exists on a particular running macOS kernel.

All builds/checks must run inside the shared serialized 10 GiB/no-swap build
wrapper and an inner `vm-tests.slice` scope with the same memory limits.
Scratch and results belong outside the repository on the datasets filesystem.
`check.sh` requires an explicit scratch directory and never touches a guest.

Distribution/load requires an Apple-authorized kext signing identity, valid
signing/notarization for the supported macOS release, and the target's normal
user/MDM approval. Kext approval on modern Intel macOS can require a restart.
Ad-hoc signing is not evidence of loadability. There is no install/load script,
no attempt to disable SIP or change boot security, and no authorization here to
restart a machine. This path does not support Apple Silicon guests.

Required runtime proof is on a separately admitted disposable Intel macOS VM:
root-only raw opens, non-root rejection, two uid lease isolation through the
broker, exact subrange mappings, bidirectional same-page mutation, address
stability over growth, GPU lease retirement, broker death/reconnect, MSI wake
counts and idle round-trip latency. A real Mac or daily-use VM must not be used
to bypass these gates. Stock applications, their sandbox, and stock Zink Mesa
remain unchanged.

## Preparatory ownership restriction: multi-user lending held

Mach rights and receiver mappings can outlive the Unix lease and the broker.
A guest reboot also does not drain persistent host backing or host GPU writers.
The current ivshmem ABI has no trusted host-backing identity that survives
registry/service aliases, PCI address relocation and guest boots. Registry IDs,
BAR addresses, pool headers writable by clients, and boot IDs cannot establish
that identity or prove safe reassignment.

Consequently this candidate does **not** complete multi-user per-range lending.
Publication and the Mach service fail closed unless the existing read-only
`/Library/Application Support/waycompose/pci-vm-owner` anchor restricts every
PCI backing in the VM permanently to one uid. This is deliberately broader
than a device or slice. A different uid is refused even for disjoint ranges,
new registry IDs, other devices, aliases, relocated BARs or another boot.
The shared broker crate's immutable raw-device snapshot policy enforces it;
there is no second daemon or watcher.

The staging snapshot version is `wc-pci-vm-owner-preparatory-v1`, with exactly
one tab-separated row binding `all-pci-backings` to a canonical `uid:N`.
All ancestor directories must be root-owned and not writable by group/others;
the anchor must be a root-owned regular 0400 file with one link. Nonempty macOS extended ACLs are also refused, including on ancestors. Symlinks,
missing, malformed, oversized, ambiguous or untrusted state grant nothing.
The broker never writes, provisions, clears, resets or reassigns this state.
Initial provisioning or any change requires a separately admitted host drain,
backing reset/zero proof and ownership migration; none is authorized or
performed by this candidate. Deleting state disables lending, not enrollment.

Completing the intended multi-user capability needs a trusted host-supplied
canonical backing identity and relative physical range, outside client-writable
memory, with identical identity for every alias and persistence across guest
boots/PCI relocation. That identity must feed the shared broker's durable
per-range uid policy before publication/export. Reassignment additionally needs
a proven host GPU drain and backing reset/zero boundary. Until then the
single-uid gate is preparatory and the multi-user capability remains held.
