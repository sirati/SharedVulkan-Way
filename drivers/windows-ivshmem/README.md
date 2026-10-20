# Protected ivshmem range mappings

This is source preparation for a replacement Windows ivshmem driver. It is
not installed or loaded. The current Looking Glass driver exposes a whole BAR
mapping and cannot enforce payload-only access. Until a protected driver and
its broker control path are validated, client-writable directory metadata is
cooperative bookkeeping and abandoned producer holds remain retained.

The overlay applies to virtio-win/kvm-guest-drivers-windows commit
`d92b9c073ec1108f51846d0bd109c5796b46fd36`, directory `ivshmem`.
Upstream source is at
https://github.com/virtio-win/kvm-guest-drivers-windows/tree/d92b9c073ec1108f51846d0bd109c5796b46fd36/ivshmem.
`LICENSE` preserves that directory's BSD license and copyright notices.
`Range.c`, `Range.h`, and `RangeBounds.h` are Waycompose additions under the same terms.
`protected-ranges.patch` contains the integration changes.

Prepare a fresh source tree outside this repository:

```
scripts/wc-ivshmem-range-source.sh /mnt/datasets/builds/scratch/ivshmem-protected
```

The script fetches the exact commit, checks the patch before applying it, and
copies the added source files. It does not build, sign, install, or load
anything. Upstream's `ivshmem/ivshmem.vcxproj` then includes the overlay. Building
requires the Windows 10 WDK with KMDF 1.15, its SDK, and MSBuild driver targets.
A normal MinGW sysroot lacks KMDF and is insufficient. No complete WDK was
found on this Linux build host, so kernel compilation and Driver Verifier
validation remain outstanding. Any build on this host must run through
`/mnt/datasets/builds/wcrun/cg`, including its outer `nix develop` invocation.
No driver installation or signing policy change is part of this work.

## API and authority

All raw-device IOCTLs require the actual caller process's primary token user to
be SYSTEM. The WDF caller-context callback performs this check before queueing
legacy requests. A client-supplied SID or claimed role is not consulted.

`RANGE_QUERY` returns a version, original BAR size, and hardware generation.
Each successful hardware prepare advances the generation. Generation exhaustion
leaves the protected interface unavailable. `RANGE_CONFIGURE` installs one
immutable policy per generation: one writable payload window and one disjoint
read-only window. Every other page, including boot, control, directory and ring
metadata, is outside client mappings. Both windows must come from trusted
original BAR geometry and retained grants. The driver does not infer them from
the shared directory. `RANGE_POLICY_QUERY` lets a restarted privileged service
read that actual immutable policy and compare it with rooted original geometry.
An unconfigured policy is explicit and still returns its actual generation;
a stale retained capability generation or an unsupported query refuses. It is not a
mapping census and does not prove any old grant or byte range is free.

`RANGE_MAP` accepts a process HANDLE from the SYSTEM broker's own handle table,
not a PID. `ObReferenceObjectByHandle` checks its access in UserMode and retains
the actual EPROCESS. Mapping requires `PROCESS_VM_OPERATION | SYNCHRONIZE`.
Only protected BAR mode captures the retained mapping-target process handle
with the extra VM operation right at its first pipe PID/SID check, rather than
reopening later by PID. Ordinary clients, readiness and holder authentication
keep their original duplication, synchronization and limited-query mask. This check
compares the candidate process token SID with the pipe token SID. It does not
independently exclude same-SID PID reuse between querying the pipe PID and
opening that process. Mapping and death watch use the same retained target
object, so its mapping lifetime remains bound to that object. A future session
join must separately establish the requesting process and service authority;
it must not claim the SID check alone proves the original pipe peer object. Requests check generation,
page alignment, physical-address bounds, integer overflow, policy containment,
and maximum write permission. Read-only mappings use `MdlMappingNoWrite` and
all mappings use `MdlMappingNoExecute`. The entire BAR alias family uses cached
memory; the patch refuses legacy mappings requesting a different cache type.
The reply contains a target address and mapping identity, never a raw device
handle or an alias of the control pages.

A map owns its target process reference and its own physical sub-MDL. Its
kernel thread waits for either privileged unmap or the actual process object.
On process death, the OS has removed that process's user mappings before the
thread releases its MDL. On explicit unmap, process exit synchronization
protects the address space while the thread attaches and removes the mapping.
If exit has already begun, it waits for actual process completion instead.
The map table retains a kernel thread HANDLE. Reaping, unmap and hardware
drain wait for thread termination before freeing the record and final device
reference. Completed records are reaped on the next protected request or on
hardware removal. Hardware release drains all maps before freeing the original
BAR MDL. Broker pipe close or broker death does not infer target completion.

## Broker integration status

`wc-broker::sys::windows::protected_range` consumes the service's original
device object. A stock driver's unsupported capability query fails closed.
It has no raw-handle fallback. Its mapping entry point uses the authenticated
Writer's retained process object and requires a trusted allocation extent.
The source-only `--protected-bar` service option is off by default. Selecting
it requires SYSTEM and raw-device ACL protection, reserves every BAR and path
alias against legacy lending, and requires protected capabilities on each
validated original device. A failed capability query aborts startup. This
option does not enable the unfinished allocation/client protocol and is not
used by the installed service. Protected device objects remain service-owned.

`wc-broker::bar_control` keeps original extents, producer registrations,
recipient-bound pending transfers and independent consumer references in broker
memory. It can suballocate only an existing retained session grant. That
constructor requires proof of the delegating service and exact original BAR,
user, session and grant identity. It does not scan or import directory slots.
Producer death preserves transfers to other live recipients and their consumer
references. It cannot reuse an extent until every independent hold is gone. In-process
allocator teardown retains the canonical grant guard if any hold remains.
This is not durable state across OS process termination. A restarted service
must refuse unreconciled grants and the already configured driver policy; it
may not construct a fresh allocator from old directory words. Durable
registration/reconciliation remains part of production integration. Lifetime
accounting does not scrub bytes. Reassigning a drained grant to another SID or
UID requires privileged initialization of its entire capacity, including
padding, before mapping or export, or a permanent same-user partition. No
automatic cross-user reassignment or guest-memory scrubbing is implemented.

`wc-broker::service_control` defines a bounded canonical cross-VM frame under
`wc-broker/service-control/v1`, separate from public boot signing. It binds the
installation, both privileged service epochs, original source identities,
separate hardware and canonical pool generations, BAR geometry, session grant, SID to UID mapping, process registrations, exact
allocation/transfer/consumer phase, and strict sequence. Parsing does not grant
authority. Only service-only authentication and retained resource records can
authorize a state transition. Endpoint authentication also polls the
retained exact kernel process and permanently refuses further seal/verify calls
after death or a liveness-query error. Reconnect cannot reset replay state.
The Windows own-service adapter checks the current primary SYSTEM SID and
duplicates the actual current process object, without reopening a PID.

`wc-broker::service_bootstrap` defines bounded Hello, Challenge and Confirm
frames without requiring or granting a region. It carries independent
32-byte service challenges and durable epochs. The lifecycle must generate
challenges with the OS CSPRNG and compare the entire already-enrolled expected
source binding. A Hello cannot supply missing expected source fields or
authorize mapping. The codec does not establish peer authority.

These APIs are not yet wired into the production client protocol. In the
Windows-client-to-Linux-compositor topology, the next integration must retain
an authoritative existing session grant without creating a second whole-device
allocator. The Linux privileged relay must authenticate the actual compositor
with SO_PEERCRED and bind its completion to the protected service channel for
that original BAR and session. Shared ring owner words, session indexes and
consumer counters alone cannot authorize completion. The Windows service must
own the raw device and protected control pages; native clients receive only
payload mappings. Old directory allocations remain in a separate namespace and
cannot be adopted by reading client-writable tuples.

## Validation

The Rust broker cross-checks for Windows. Seven local state-machine tests cover
producer death with pending and live consumers, forged consumer completion,
and retirement of a kernel-confirmed dead recipient. A bounded native Windows
fixture confirms that the retained producer process HANDLE fires retirement
only after its child process actually terminates. Evidence is outside Git at
`/mnt/datasets/builds/evidence/windows-producer-retirement` and
`/mnt/datasets/builds/evidence/windows-bar-retirement/real-windows-OJRK27`.
These checks do not validate the kernel overlay or establish production crash
recovery. Portable C tests execute the same range helpers used by the driver
against control-page overlap, out-of-bounds extents, unaligned ranges and
physical-address overflow. They do not emulate kernel mapping semantics. Kernel build, code analysis, Driver Verifier, protected mapping tests,
and complete authenticated control-path integration remain required.
