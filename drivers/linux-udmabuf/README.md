# Protected udmabuf exports

This experimental source overlay extends Linux's existing udmabuf exporter.
It does not introduce an allocator or shared policy store. `wc-broker` must
retain the original source, reserve the entire physical grant durably before
export, check users and ranges, and permanently retain uncertain exclusions.
No production backend uses this ABI. No host module is installed or loaded.

The exact source is Linux stable 6.18.55, commit
`725bd2f3c81d54edccb66fead01d4c0c222e2231`, from
[Greg Kroah-Hartman's stable tree](https://github.com/gregkh/linux/blob/725bd2f3c81d54edccb66fead01d4c0c222e2231/drivers/dma-buf/udmabuf.c).
The source script verifies the pinned original file SHA-256 before applying the
patch. The upstream SPDX GPL-2.0 notice and module authorship remain intact;
the new kernel files use the same license. The ABI header preserves the Linux
syscall-note exception. Microsoft driver sources and licenses are separate.

```sh
scripts/wc-udmabuf-protected-source.sh /mnt/datasets/builds/scratch/udmabuf-source
/mnt/datasets/builds/wcrun/cg scripts/wc-udmabuf-protected-build.sh \
  /mnt/datasets/builds/scratch/udmabuf-source \
  /path/to/prepared/linux-6.18.55-build \
  /mnt/datasets/builds/scratch/udmabuf-module
```

The build is an external replacement udmabuf module, with a different module
name and the default `/dev/udmabuf` node. That default node cannot coexist with
stock udmabuf. For an isolated fixture only, set `UDMABUF_TEST_NODE=1` to compile
the same exporter with `/dev/udmabuf-protected-test`, preserving the stock node.
The build records this choice and the kernel config/source hashes. This is not
a second policy or allocator. The scripts do not install or load either module. The kernel
build tree must supply its exact matching configuration, generated headers and
symbol versions. Stock udmabuf is built-in and uses the unexported
`memfd_fcntl` function. The overlay reuses the pinned `mm/memfd.c` seal lookup
through the matching shmem/hugetlb inode headers, while retaining the original
inode-lock exclusion. This permits the isolated external-module build without
exporting new symbols or changing the host kernel. All builds on this shared host, including Nix dependency
builds, run through the exclusive `cg` queue with the shared 10 GiB, zero-swap
limit. Scratch and evidence stay under `/mnt/datasets`.

## Experimental interface

The private ioctl numbers in `udmabuf-protected-uapi.h` are not an upstream
allocation or a stable production ABI. `UDMABUF_PROTECTED_CREATE` requires
`CAP_SYS_ADMIN` in the initial user namespace. Existing raw-device ownership
and device ACL restrictions remain required. Desktop users receive only data
exports and never the original memfd, raw udmabuf device or monitor handle.

Creation captures one actual original memfd once, retaining that file object.
All copied offset/size ranges use that same object. The existing udmabuf seal,
page alignment, size limits and folio pinning machinery are reused. The overlay
also checks nonempty ranges, signed endpoints, original-file size and total-byte
overflow. The
kernel enforces the supplied extent; the canonical broker decides whether
that extent is authorized. No shared-directory word supplies kernel authority.

On success, the ioctl returns two CLOEXEC descriptors. `data_fd` is a standard
dma-buf. `monitor_fd` is a separate anonymous file exposing only INFO, RANGE and
poll. It cannot map or write payload bytes and never retains the data dma-buf.
INFO binds a kernel instance identifier and monotonic export ID to the actual
retained source device/inode and original size, immutable maximum access, total
bytes and exact
range count. RANGE returns each copied immutable source offset/size pair.
The monitor's source reference prevents inode identity reuse while it exists.
The broker must retain that exact monitor object with its exact grant/export
record; an integer FD, copied status struct or guessed export ID is no proof.

The monitor state is LIVE until the dma-buf final-release callback begins,
DRAINING while separately retained reservation fences are awaited, and RETIRED
only after exporter DMA mappings are cleaned up and all pinned folios are
released. Poll reports input readiness only for RETIRED; errors report FAILED.
Failure never reports retirement. Losing a monitor or receiving an unknown
state cannot authorize reuse. Kernel instance IDs distinguish module lifetimes;
IDs are identities rather than authentication credentials.

## Immutable maximum access

RW exports permit shared CPU read/write mappings and normal native imports.
RO exports reject writable mappings, clear `VM_MAYWRITE`, and retain that limit
across mmap aliases, fork and attempted mprotect upgrades. Both reject execute
mappings and clear `VM_MAYEXEC`. Duplicating or transferring an FD does not
change the export's immutable private permission state or its file mode.

RO exports deliberately reject native device attachment and kernel vmap.
A generic dma-buf importer has no immutable GPU read-only contract.
`DMA_TO_DEVICE` expresses an importer direction, not universal protection
against GPU shader writes without a validated hardware/importer enforcement
mechanism. RO Vulkan import remains unavailable. The broker must refuse that
projection, not substitute RW. Read-only CPU coherency operations are allowed;
CPU write/bidirectional access requests are rejected. Trusted kernel code and
the privileged original source owner remain outside the unprivileged-client
access boundary.

The CPU-access cache has one per-buffer mutex covering SG creation, direction,
begin/end sync and final cleanup. Each SG is published only after successful
creation and is unmapped exactly once under that mutex. RW CPU cache mappings
use BIDIRECTIONAL consistently across mixed read/write sync callers; RO uses
FROM_DEVICE for CPU reads. The stock unsynchronized cache initialization must
not be carried into a retirement-certified exporter.

## Lifetime and final retirement

The pinned source audit covers these mechanisms:

- Standard file references retain a dma-buf across dup, fork and SCM_RIGHTS,
  including queued transfers. Closing one FD or killing one process cannot
  force the last-reference callback.
- Direct dma-buf mmap keeps the dma-buf file as `vm_file`. The public
  `dma_buf_mmap` importer helper explicitly uses `vma_set_file` to retain that
  same file. The overlay also sets that exact `vm_file` itself before installing
  its VMA callbacks. VMA cloning and splitting retain file references and invoke the
  VMA open hook; VMA destruction invokes close before dropping its file ref.
  The overlay counts its initial VMA plus open/close descendants. These are
  extra fail-closed invariants, not user-supplied completion counts.
- Native attachment does not itself acquire a dma-buf reference. The pinned
  dma-buf contract requires importers to retain the `dma_buf_get` reference
  until all mapping and attachment cleanup ends. The overlay also counts
  attach/detach, device map/unmap and kernel vmap/vunmap callbacks. Core
  attachment lists and vmap state must agree at final release.
- Final exporter release snapshots every reservation fence through
  `DMA_RESV_USAGE_BOOKKEEP`, retaining independent fence references before the
  core destroys its reservation object. Cleanup runs on a private workqueue
  and waits without a timeout for every captured fence. Only then does it
  release DMA cache mappings and folio pins and publish RETIRED with release
  ordering. The monitor reads that state with acquire ordering.
- Any inconsistent native/VMA count, fence-snapshot allocation failure or wait
  failure publishes FAILED while retaining the pins and internal state.
  Unknown native completion therefore causes a retained exclusion, not reuse.
  Module exit drains the cleanup workqueue before destroying the exporter
  device or module text. An unsignaled fence can block cleanup/unload forever;
  there is no force-retirement timeout.

The release callback is the last exporter-file lifetime event, not a census
based on the service's own descriptor table. Its safety still assumes trusted
kernel importers honor their reference, map and fence contracts. A driver that
retains an untracked raw PFN after detach or fails to fence hardware writes is
a kernel correctness violation this exporter cannot detect. Kernel reference
tracking also does not establish cross-VM or producer GPU completion. The
canonical broker must compose all independent native holds and both sides'
trusted completion before reusing any physical extent. Retirement does not
scrub data; the existing owner must enforce zeroing and cross-user policy.

## Validation limits and required guest proof

Source preparation and an actual external-module compile/link against the
matching Linux 6.18.55 configuration are implemented. The fixture alternate-node
build passed with `W=1` using GCC 16.2.0. BTF generation was skipped because the
prepared dev output has no vmlinux image. This does not establish runtime
correctness. Isolated native alias and importer tests remain pending. No host kernel or module, family VM, Windows lease, security setting,
or application binary was changed for this preparation.

Required private Linux guest cases include initial RO mmap, failed writable
mmap/mprotect/execute upgrades, exact range bounds, multiple VMAs and split
munmap, surviving dup, fork and SCM_RIGHTS aliases, VMA survival after every
user FD closes, recipient exit with other holders alive, and eventual monitor
retirement only after the final alias disappears. A native importer fixture
must retain and release real dma-buf references, device attachments, mappings
and independently delayed fences. Actual GPU importer lifetime needs its own
proof in addition to a synthetic importer fixture. Negative cases must retain
pins and never produce RETIRED. Portable arithmetic tests or userspace counts
do not satisfy this kernel gate.

Source audit and future build/runtime evidence belong outside Git at
`/mnt/datasets/builds/evidence/linux-udmabuf-retirement`.
