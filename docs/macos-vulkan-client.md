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

## Broker and ICD integration gates

The adapter selects/maps a subrange in the root broker and can request an owned
Mach memory-entry send right for exactly that mapping. Kernel support for
exporting this I/O mapping as a memory entry must be tested; an error is returned
without falling back to a copy. Its Rust `MemoryEntry` is not an fd.

The existing Unix broker protocol transfers fds with SCM_RIGHTS. It cannot
transfer a Mach send right. Complete client attachment still needs:

1. A shared-broker Mach-right handoff authenticated by the kernel audit token's
   uid and pid, with existing core region ownership/lease decisions. No
   parallel authorization daemon or caller-supplied uid is acceptable.
2. A receiver mapping API that owns the Mach right and deallocates its mapping
   only at epoch teardown. `SharedRegion::from_raw` alone does not transfer
   the mapping owner's lifetime; `map_fd`/`map_fixed` cannot consume a Mach port.
3. Host-side allocation/announcement of non-overlapping per-user BAR extents,
   the existing MAPPED/RETIRING/UNMAPPED protocol, and retained region leases.
4. A broker bell adapter that arms the kernel IRQ port, fans wake hints to
   authorized sessions, and rescans rings after attach/reconnect/rearm.
5. A macOS `RingTransport::connect` cold path using those mappings and bells.
   `sv-icd/src/transport.rs` currently returns the Linux/Windows-only error on
   macOS. `BEAT_INTERVAL` and `finish_attach` are also gated to those systems;
   expand those gates only with a working macOS attach path.

The ICD build script now emits `./libsv_icd.dylib` in the macOS manifest. The
explicit relative directory resolves beside the manifest, so a cross-built
package can be copied without relying on dyld search paths. This filename fix
alone does not enable client attachment.

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
