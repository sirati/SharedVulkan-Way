# Kernel source compatibility audit

The source at `f5a9c63b` compiles and links as an unsigned AMD64 native kernel
image with LLVM 21.1.8, Microsoft WDK/SDK NuGet packages 10.0.26100.1 and
KMDF 1.15. This establishes a real kernel compilation result. It establishes
neither target Windows support nor safe mapping and retirement at runtime.
Production activation remains off.

## Exit synchronization dependency

`Range.c` declares these imports explicitly:

```c
NTKERNELAPI NTSTATUS NTAPI PsAcquireProcessExitSynchronization(PEPROCESS Process);
NTKERNELAPI VOID NTAPI PsReleaseProcessExitSynchronization(PEPROCESS Process);
```

Both names resolve in the pinned Microsoft WDK `ntoskrnl.lib` and appear in the
linked image's import table. Neither declaration exists in the pinned WDK's
public include tree. No Microsoft Learn DDI contract for either routine was
located. Their signatures agree with System Informer's own
[ntfill.h declarations](https://github.com/winsiderss/systeminformer/blob/2e1349b065e5cf187db390697062fc95c846de33/KSystemInformer/include/ntfill.h#L593).
That project's [use of the pair](https://github.com/winsiderss/systeminformer/blob/2e1349b065e5cf187db390697062fc95c846de33/KSystemInformer/object.c)
also treats a failing acquire as refusal and releases only after success.
These are independent implementation references, not a Microsoft support promise.

The current source uses the pair at PASSIVE_LEVEL around the attach/map/detach
and attach/unmap/detach intervals. A failing acquire never permits attach or
unmap. A worker then waits for the retained process object's completion before
freeing its MDL. The API names and matching declarations alone do not validate
these lifetime assumptions on an actual Windows kernel.

| Target or component | Established result | Still required |
| --- | --- | --- |
| WDK 10.0.26100.1 x64 import library | Both names resolve; native image linked | None for this compile gate |
| Windows 10 releases | Project target and native subsystem are Windows 10; KMDF 1.15 originated in Windows 10 version 1507 | Exact supported release list, target export and behavioral validation |
| Disposable Windows 11 clone | Existing image is identified as Windows 11 | Exact build and servicing revision, actual kernel exports, later authorized runtime validation |
| Testhost daily Windows VM | No Windows build or kernel artifact collected in this work | Exact version and compatibility evidence before any separately authorized deployment |

An import library's version is not evidence that every Windows 10 or Windows 11
kernel exports an undocumented symbol. There is no claim that this driver loads
on all such versions. Checking a future target must include the exact kernel
image's exports and build identity; exports alone still do not prove behavior.
This audit did not start, mount, or modify either guest.

## Documented DDIs and constraints

The physical sub-MDL is built with
[MmAllocateMdlForIoSpace](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-mmallocatemdlforiospace),
available since Windows 8. Its contract permits device I/O space, requires page
alignment, limits the total extent to less than 4 GiB, and specifies `IoFreeMdl`
for release. The overlay uses translated original BAR geometry, checked page
bounds, and its own sub-MDL. This mechanism must not be reused for guest OS RAM.

[MmMapLockedPagesSpecifyCache](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-mmmaplockedpagesspecifycache)
requires an exception handler for UserMode mapping. The overlay has one.
UserMode mapping and unmapping are at PASSIVE_LEVEL, below the documented
APC_LEVEL ceiling. The documented read-only mapping flag is available since
Windows 8. The map is non-executable and uses the cached alias policy.
[MmUnmapLockedPages](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-mmunmaplockedpages)
requires the original process context, hence the retained exact process object
and attach interval. The
[KMDF version history](https://learn.microsoft.com/en-us/windows-hardware/drivers/wdf/kmdf-version-history)
places the selected framework version 1.15 in Windows 10 version 1507. These
public requirements do not resolve the separate undocumented exit-fence issue.

## Alternatives evaluated

- Retaining an `EPROCESS`, checking a process state, or attaching to its address
  space does not provide a documented atomic exit fence. A state check followed
  by an attach would leave a race. The
  [attach contract](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-kestackattachprocess)
  requires a matched detach and warns against complex work while attached; it
  provides no exit synchronization guarantee. No such substitution was made.
- The public rundown APIs synchronize a driver-owned object when its owner
  participates in that protocol. They cannot make Windows process teardown
  honor a new driver-owned lock. Accessing rundown state inside an opaque
  `EPROCESS` would introduce another undocumented layout dependency. See
  [run-down protection](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/run-down-protection)
  and [opaque kernel structures](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/eprocess).
- A documented process-exit callback could mark a per-process registry as
  exiting. It does not by itself fence a concurrent arbitrary-process attach.
  Making the callback wait for the mapping worker would need a separate proof
  and conflicts with Microsoft's guidance against blocking or synchronizing
  with other threads in such callbacks. See
  [process notification](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddk/nf-ntddk-pssetcreateprocessnotifyroutineex)
  and [callback best practices](https://learn.microsoft.com/en-us/windows-hardware/drivers/kernel/windows-kernel-mode-process-and-thread-manager).
- Mapping only from a target's own live requesting thread would change the
  broker-only IOCTL design and still require a safe revoke/exit design. A new
  client mapping endpoint must not expose the privileged original device or
  use cooperative client cleanup as retirement proof. It is an architectural
  candidate, not a drop-in fix.
- [ZwMapViewOfSection](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-zwmapviewofsection)
  and [ZwUnmapViewOfSection](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-zwunmapviewofsection)
  accept an explicit target process handle. They operate on section views, not
  these MDL mappings. A section-backed design would need new proofs for BAR
  backing, page and allocation granularity, immutable read-only protection,
  cache aliases and view identity. Microsoft's physical-memory section warning
  also applies. This is a redesign candidate; the current driver cannot simply
  replace the MDL unmap call with a section unmap call.

No documented drop-in replacement satisfying the present mapping and revoke
contract was established. The existing exit fence was preserved.

## Warning and analyzer checks

`scripts/wc-ivshmem-range-analyze.sh BUILD_DIRECTORY` runs a stricter compile and
Clang Static Analyzer over the exact scratch source copied by the build recipe.
It checks that the overlay matches this checkout and preserves the compiled
`.sys`. Run it through `/mnt/datasets/builds/wcrun/cg` on this build host.

The stricter compilation uses `/W4`, conversion, shadow, cast-alignment, format
and undefined-macro warnings. Existing warnings remain visible. The overlay's
container casts come from `CONTAINING_RECORD`; list entries originate in
properly aligned nonpaged `RANGE_MAP` objects. The flags complement converts an
integer constant to an unsigned mask. `GetExceptionCode()` returns an unsigned
exception status that is assigned to `NTSTATUS`. These warnings are recorded,
not treated as proof that the runtime is safe. The old pool allocator preserves
the Windows 10 baseline; replacing it with `ExAllocatePool2` would raise the
minimum Windows version. Upstream warnings include include-case differences,
ignored `#pragma align`, event pointer type and integer conversions. No warning
suppression or upstream behavioral change was added.

The analyzer runs the default checks and records plist diagnostics plus
function progress. On the exact `f5a9c63b` overlay it reported only a dead store
in the WDK's `ntddk.h`, repeated for each translation unit, and no source-level
analyzer diagnostic. This is limited analysis, not a clean kernel safety bill.
It does not model WDF callbacks, kernel allocation/reference counts, concurrent
workers, process exit or device removal. SEH exception paths are not established
as exhaustively analyzed: LLVM's
[expression engine](https://github.com/llvm/llvm-project/blob/llvmorg-21.1.8/clang/lib/StaticAnalyzer/Core/ExprEngine.cpp#L1746)
handles unsupported SEH statement cases as aborted paths. Function progress
alone cannot prove those paths or all inlined callees were explored.

Build logs, plist reports, source references and binary inspection remain
outside Git under
`/mnt/datasets/builds/evidence/windows-producer-retirement/kernel-recipe` and
`/mnt/datasets/builds/evidence/windows-producer-retirement/kernel-analysis`.
MSVC/WDK analysis, target export validation, Driver Verifier, actual protected
mapping tests and full authenticated retirement remain outstanding. None of
these checks signs, installs or loads a driver or changes security policy.
