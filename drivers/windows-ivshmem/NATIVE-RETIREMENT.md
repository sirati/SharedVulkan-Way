# Windows native retirement design

This is a design for review, not an implemented interface. The unsigned Range
driver is not installed or loaded. Existing map IDs, process-exit waits and MDL
cleanup do not establish all-native retirement. Production release stays off.
The reviewed Linux overlay is unchanged.

## What the existing driver owns

`Range.c::map_range` retains the actual target EPROCESS resolved from the
SYSTEM caller's handle table with UserMode access checks. It allocates an
I/O-space sub-MDL for the original translated BAR extent, maps that MDL into
one target address space, retains its WDFDEVICE, and starts one kernel worker.
The MDL describes device I/O space. It is not a `MmProbeAndLockPages` pin of
ordinary pageable RAM. `IoFreeMdl` releases its descriptor; it is not a GPU
completion operation or release of every external driver's DMA mapping.

| Object or event | Existing accounting | What it cannot prove |
| --- | --- | --- |
| Range-created CPU map | Record, exact EPROCESS, MDL, address, map ID | Whether another kernel driver retained an address-derived native import |
| CPU write protection | `MdlMappingNoWrite`; all maps non-executable | GPU write protection or native-provider rights |
| Explicit UNMAP | Worker attaches under exit synchronization and calls `MmUnmapLockedPages`; caller joins worker | GPU queues idle, native registration destroyed or vendor DMA mappings gone |
| Target exit | Worker waits on retained process object and then frees its MDL | Independent native objects, outstanding hardware access or other processes' imports retired |
| Map thread termination | Table removal and final process/device references can be released | Last access to the physical BAR allocation |
| Legacy whole-BAR map | Still accepted for SYSTEM through upstream queue | Per-allocation coverage or a protected export census |
| Hardware release | Drains Range workers before upstream BAR cleanup | Safe GPU-native detach, DMA quiescence or backing release |
| Broker death | No inferred target completion | Durable reconstruction of grants or native ownership |

The undocumented process exit synchronization pair is a separate unresolved
compatibility dependency. Waiting for a signaled process is meaningful for that
process's CPU address space. It must never advance a native completion counter.
The present explicit unmap/drain path has no native guard, and is therefore
unsuitable for an allocation with a potentially surviving GPU host-pointer
import. This is a production gating limitation, not a newly tested crash.

No Range hook observes Vulkan `vkAllocateMemory` host-pointer import, CUDA
registration, D3D/vendor registration, GPU virtual address aliases, DMA adapter
mappings, other kernel MDLs or an import duplicated by a provider. A user virtual
address is not a transferable object with a kernel final-release notification.
An EPROCESS reference does not turn it into one. Do not add a RETIRED state
merely when `Maps` becomes empty.

## Required authority and immutable identity

A future export object must be kernel-owned, distinct from its monitor, and
bound at creation to the trusted original device/backing, not a pathname or
client-supplied address. Proposed immutable information is:

- A fresh driver/device-instance identity and nonreused export ID, separate
  from hardware generation and map ID. Counter exhaustion refuses creation.
- Original BAR identity and full physical capacity, hardware generation,
  immutable offset/length vector, total export bytes and maximum rights.
- Broker context, grant and allocation generation supplied only by the trusted
  owner and compared with its retained canonical records. These fields do not
  authenticate themselves merely by being echoed by a driver.
- Actual target object references for each CPU/native projection. No PID lookup
  or later reopen substitutes for the original target object.

The monitor handle must not retain a data mapping, export reference or native
hold. The owner holds the trusted monitor object before sharing data. Its query
returns immutable identity, monotonic state and failure information; a client
cannot write state or signal completion. Monitor handle provenance and original
export binding are required in addition to checking the RETIRED value.

This driver cannot currently provide original host QEMU memfd identity from
inside Windows. That cross-VM binding remains the canonical service owner's
separately authenticated retained fact. Physical BAR address or length alone
cannot establish the host allocation identity.

## Native-provider integration contract

An accounting API alone is insufficient if a client can bypass it and import
the same CPU pointer directly. The design must first choose an enforceable
architecture:

1. Keep native imports inside a trusted provider that owns all data mappings,
   queues and native objects, and does not expose an independently importable
   BAR pointer to an untrusted process. Application integration must preserve
   stock application behavior through our driver/library interface. Any route
   that still exposes the pointer needs a complete provider mediation proof.
2. Integrate with a documented native allocation/export mechanism whose kernel
   owner receives actual final alias/reference and hardware-completion events.
   Every participating import path must retain the same export object before
   asynchronous work or address publication, including provider-internal aliases.

Neither architecture is implemented or proven here. A generic privileged
`REGISTER_IMPORT` / `COMPLETE_IMPORT` IOCTL called by an untrusted importer is
not acceptable. It reports cooperation and cannot rule out an unregistered
import. Likewise, an allowlisted executable path or signed userspace helper
alone cannot prove complete interception of native registrations.

For a trusted native provider, a proposed in-kernel interface has these
semantics. Exact ABI, versioning and documented vendor support need review:

- `AcquireNative(export, exact_provider_instance, rights, target)` atomically
  refuses after retirement starts, validates bounds/rights, and returns an
  opaque kernel token retaining the original backing and provider instance.
  It runs before native address registration/import becomes visible.
- Provider-internal duplicate/alias creation acquires another reference before
  publishing it. Imported resources, queue submissions and asynchronous cleanup
  cannot outlive this retained graph.
- `BeginNativeDrain(token)` closes the token to new submissions/aliases and
  captures all queues/engines that could access it. A single queue fence is
  insufficient when other queues or aliases can still submit work.
- `CompleteNativeDrain(token, provider_completion)` is callable only through
  the trusted provider contract after actual successful queue completion,
  removal of every registration/GPU address/DMA mapping and final native object
  release. The completion belongs to this export and provider generation.
  A user event HANDLE or claimed fence value is not provider completion.
- Errors, device loss, provider death, surprise removal, timeout and unknown
  submission outcome mark failure and preserve the ownership record. They do
  not produce successful drain or force release of possibly referenced backing.

The provider must define callback execution level, reference/rundown rules,
lock order and unload protection. Never wait for provider completion while
holding the Range state mutex or from a process-exit notification callback.
Use independently referenced passive workers; retain module/device/provider
lifetimes until each callback and worker actually terminates. No callback may
resurrect an export after finalization starts.

A Vulkan trusted provider must stop further submissions, wait for actual
completion across every relevant queue, and destroy all bound resources and
imported memory while the original mapping is still valid. Only then may it
release its native token. Host memory must remain valid for the imported
allocation's lifetime. This userspace discipline is useful only if authority
and complete mediation have already been established. It does not create a
kernel final-release hook for arbitrary external clients or drivers.

## Proposed state and teardown order

`LIVE` accepts authorized new holds. `DRAINING` is entered atomically after the
owner closes new grants/exports and requests retirement. Existing CPU and native
holds remain authoritative. An error can enter `FAILED_RETAINED` from either
state; no automatic restart or timeout changes failure into retirement.

Retirement requires all of the following, for the same immutable export:

- No pending transferable data objects or ability to create new aliases.
- Every native provider has positively completed its drain and released every
  native token, including native references created through CPU pointers.
- Every CPU mapping and independently tracked export/kernel hold is removed,
  with actual worker termination and applicable translation invalidation.
- Original backing and hardware lifetime remain valid throughout that work.

For a CPU mapping used as a native import source, native drain must finish
before explicit CPU unmap. Native imports cannot be made safe by unmapping
first. Target process exit automatically removes its CPU view; a permitted
native mechanism must independently guarantee that its retained backing remains
valid through asynchronous cleanup. If it cannot, that mechanism is unsupported.

Only the trusted kernel export object's successful terminal transition can
support an owner-minted non-Clone `RetirementProof`. The owner factory compares
context, grant, allocation generation, capacity, target, rights and exact export
identity. Neither raw wire state nor a public Rust constructor can mint proof.
Consumer completion and producer death remain separate holds.

PnP stop/removal needs a real documented provider quiesce protocol before BAR
release. Device references alone do not prevent physical surprise removal.
Surprise removal may make the allocation unusable; it never authorizes reuse
of a canonical allocation as if a successful GPU drain had occurred. Retain the
failed generation and disable reuse. How original backing survives or becomes
permanently unavailable on removal must be specified per provider/device.

## Required validation before activation

First obtain documented native-provider support and review complete mediation,
including all pointer/import escape paths. Then implement independent identity,
hold and monitor objects with race review. Validate mapping and native semantics
on a separately authorized disposable Windows installation with a supported,
properly signed driver, without weakening system policy.

Required cases include mapping/import creation racing drain, alias duplication,
multiple queues, explicit unmap during a native hold, process exit with delayed
provider cleanup, provider failure, failed completion, broker restart, hardware
generation change, monitor-only survival and nonreused IDs. Use controllable
synthetic provider references/fences first. Real GPU testing must use valid
capability-gated calls, known completion and preserved uncertain owners. No
crash probe, reset, forced termination or fabricated completion is acceptable.

## Primary contracts for implementation review

- [MmAllocateMdlForIoSpace](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-mmallocatemdlforiospace)
- [MmMapLockedPagesSpecifyCache](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-mmmaplockedpagesspecifycache)
- [MmUnmapLockedPages](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-mmunmaplockedpages)
- [IoFreeMdl](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-iofreemdl)
- [Vulkan host pointer import](https://docs.vulkan.org/refpages/latest/refpages/source/VkImportMemoryHostPointerInfoEXT.html)
- [Vulkan memory release](https://docs.vulkan.org/refpages/latest/refpages/source/vkFreeMemory.html)

These contracts explain API requirements. They do not establish undocumented
vendor import lifetime behavior or a complete Windows native-provider API.
