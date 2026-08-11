# Forwarding Vulkan across a VM boundary — design-grounding research

Research for `shared_vulkan`. Written 2026-08-11.

Companion documents in this directory: `hardware-constraints.md` (measured facts
about the target machine), `research-sharedgl.md` (separate stream).

**Epistemic status.** Everything below is either read from primary source
(Mesa/virglrenderer/venus-protocol source pulled from
`gitlab.freedesktop.org`, the Vulkan specification `.adoc` sources, `vk.xml`
parsed locally, QEMU documentation) or explicitly marked as unconfirmed.
Where I could not confirm something I say so rather than guessing. Counts
attributed to "local `vk.xml`" were produced by parsing
`/nix/store/…-vulkan-headers-1.4.341.0/share/vulkan/registry/vk.xml` with
ElementTree on this machine.

---

## 0. The one-paragraph answer

The protocol layer of this problem is solved and the solution is reusable:
Venus's `VK_EXT_command_serialization` is a complete, direction-neutral wire
format for Vulkan, its generator is 3,126 lines of Python, and it is MIT
licensed. The problem that is *not* solved for our topology is memory. Vulkan
mandates that every implementation expose a `HOST_VISIBLE | HOST_COHERENT`
memory type, every existing forwarder satisfies that by mapping the *real*
pages across the boundary, and every existing mechanism for doing so
(virtio-gpu host-visible blob window, `KVM_SET_USER_MEMORY_REGION`, dma-buf +
`SCM_RIGHTS`) moves host pages into a guest. We need the opposite. The only
transport on this machine that is symmetric enough to do that is ivshmem, and
ivshmem's viability rests entirely on whether a *signed* Windows driver exists.
That single question dominates the design.

---

## 1. Prior art

### 1.1 Venus — the closest existing thing

Venus is a virtio-GPU protocol for serialising Vulkan. It has three parts:

| Part | Where | Licence |
|---|---|---|
| Protocol definition + generator | `gitlab.freedesktop.org/virgl/venus-protocol` | MIT (generator), Apache-2.0 OR MIT (XML) |
| Guest driver (`vulkan_virtio`) | Mesa, `src/virtio/vulkan/` | MIT |
| Host renderer (`vkr`) | virglrenderer, `src/venus/` | MIT |

Licence headers verified directly: `vn_protocol.py` carries
`# Copyright 2020 Google LLC` / `# SPDX-License-Identifier: MIT`;
`xmls/VK_MESA_venus_protocol.xml` carries
`SPDX-License-Identifier: Apache-2.0 OR MIT`; Mesa's `vn_ring.h`,
`vn_renderer.h`, `vn_device_memory.h` all carry
`SPDX-License-Identifier: MIT`. **All permissive. All reusable in a Rust
project under MIT/Apache-2.0.**

Sources:
- <https://docs.mesa3d.org/drivers/venus.html>
- <https://gitlab.freedesktop.org/virgl/venus-protocol>
- <https://gitlab.freedesktop.org/mesa/mesa/-/tree/main/src/virtio/vulkan>
- <https://gitlab.freedesktop.org/virgl/virglrenderer/-/tree/main/src/venus>
- <https://www.collabora.com/news-and-blog/blog/2022/10/19/a-look-at-vulkan-extensions-in-venus/>

Note: `gitlab.freedesktop.org` sits behind an Anubis anti-bot wall that rejects
the WebFetch user agent but serves `curl` normally. Use `curl` on
`/-/raw/main/...` paths, or the GitLab REST API at
`/api/v4/projects/virgl%2Fvenus-protocol/repository/tree`.

#### 1.1.1 How the protocol is defined and generated

The protocol is three XML files plus two Python modules:

```
xmls/vk.xml                             (Khronos, verbatim copy)
xmls/VK_EXT_command_serialization.xml   (588 lines, 456 <enum> entries)
xmls/VK_MESA_venus_protocol.xml         (315 lines)
vkxml.py                                (1150 lines — registry parser)
vn_protocol.py                          (1976 lines — code generator)
```

That is **3,126 lines of Python generating the entire Vulkan marshalling layer
for both sides**. This is the single most useful scale data point in this
document: the generator is small because the wire format is regular. The
generated output is two families of headers, `vn_protocol_driver_*.h` for Mesa
and `vn_protocol_renderer_*.h` for virglrenderer, each containing
`vn_sizeof_*`, `vn_encode_*`, `vn_decode_*` and dispatch functions per command.

`VK_EXT_command_serialization.xml` contains 456 `<enum>` entries, essentially
one `VkCommandTypeEXT` per serialised command. Compare against the full API:
local `vk.xml` (1.4.341) has **709 non-alias commands**. So Venus serialises
roughly two thirds of the API surface, having deliberately skipped the parts
that are not serialisable.

Adding an extension is documented in the repo `README` as: add the string to
`VK_XML_EXTENSION_LIST`, update `VK_EXT_command_serialization.xml` using
`utils/print_vk_command_types.py`, regenerate. That is the whole workflow.

#### 1.1.2 The wire format

`docs/VK_EXT_command_serialization.txt` is a 134-line document that specifies
the entire format. It is short enough to reimplement from scratch and precise
enough to be worth quoting. Command stream:

| DW | Description |
|---|---|
| 0 | `VkCommandTypeEXT` |
| 1 | `VkCommandFlagsEXT` |
| 2-N | command parameters |

Reply stream:

| DW | Description |
|---|---|
| 0 | `VkCommandTypeEXT` |
| 1-N | command return and out parameters |

Rules, condensed from the spec text:

- Command length is **implicit** in the command type and is not encoded.
  "It is impossible to skip an unrecognized command type during
  deserialization." Both ends must agree on the protocol version exactly.
- A reply is generated only when `VK_COMMAND_GENERATE_REPLY_BIT_EXT` is set in
  the flags word — so the vast majority of calls are fire-and-forget.
- Integers little-endian, padded to 32 bits. Floats reinterpret-cast to the
  same-width unsigned integer. Enums as `int32_t`. `size_t` as `uint64_t`.
- Object handles, dispatchable or not, serialise as a `uint64_t` **object id**.
  `VK_NULL_HANDLE` is id 0.
- Non-`void` pointers serialise as a `uint64_t` count followed by the values.
  Padding to 32 bits happens after all values, not after each.
- A `pNext` `void*` is "serialized as a pointer to a struct whose type is
  determined by `VkStructureType`". A `void*` with a known size is a blob and
  serialises as `uint8_t*`. `char*` serialises as a blob including the NUL.
- Unions serialise a `uint32_t` member index first, then the member.
- Fixed-size arrays serialise as pointers.
- For non-const (output) pointers, the pointee is assumed uninitialised
  **except** `sType`, `pNext`, object handles, and `uint32_t` size parameters —
  so only those parts are sent on the way out. This is the two-call
  count-then-fill idiom handled correctly.

And the load-bearing exclusion list, verbatim:

> When a command parameter is not of a type with a serialization specified, it
> is not serializable. This includes, but is not limited to,
>
>  - function pointers
>  - pointers to mapped memory object regions
>  - WSI handles
>  - external handles

**"Pointers to mapped memory object regions" are explicitly out of scope.**
Venus does not serialise mapped memory. It cannot. It relies on the pages being
genuinely shared. Hold that thought for §2.

Two honest caveats the document makes about itself: "A special rule must be
followed to serialize only the initialized parts. (More specific? There may
also be bugs (grep WA) in the codegen.)" and, on null pointers, "It is
impossible to distinguish `NULL` and a non-`NULL` pointer with zero value after
serialization. (Is this going to be an issue?)". So the spec is a working
document, not a ratified standard.

#### 1.1.3 The ring

The ring is defined in the protocol XML as `VkRingCreateInfoMESA` and
implemented identically on both sides. The XML fields:

```
resourceId, offset, size, idleTimeout,
headOffset, tailOffset, statusOffset,
bufferOffset, bufferSize, extraOffset, extraSize
```

Mesa's `struct vn_ring_layout` in `src/virtio/vulkan/vn_ring.h` mirrors these
exactly. The header's own description:

> A ring is a single-producer and single-consumer circular buffer. The data in
> the buffer are produced and consumed in order. An externally-defined
> mechanism is required for ring setup and notifications in both directions.
> Notifications for new data from the producer are needed only when the
> consumer is not actively polling, which is indicated by the ring status.

The status word is a bitmask, `VkRingStatusFlagBitsMESA`:
`VK_RING_STATUS_IDLE_BIT_MESA` (bit 0), `VK_RING_STATUS_FATAL_BIT_MESA`
(bit 1), `VK_RING_STATUS_ALIVE_BIT_MESA` (bit 2).

The consumer side in `virglrenderer/src/venus/vkr_ring.c` runs a dedicated
thread per ring that **spins** while work is arriving and only sleeps after
`idleTimeout` elapses with an empty ring, at which point it sets
`VK_RING_STATUS_IDLE_BIT_MESA` and blocks on a condition variable until the
producer sends `vkNotifyRingMESA`. Memory ordering is explicit C11 atomics:

```c
/* the renderer is expected to load the head with memory_order_acquire, ... */
atomic_store_explicit(ring->control.head, ring_head, memory_order_release);
/* the driver is expected to store the tail with memory_order_release, ... */
return atomic_load_explicit(ring->control.tail, memory_order_acquire);
```

This hybrid spin-then-sleep design is exactly what you want over a shared
memory region with an out-of-band doorbell, and it maps 1:1 onto
`ivshmem-doorbell`. **This design is directly reusable and I recommend copying
it rather than inventing one.**

Beyond the ring, `VK_MESA_venus_protocol.xml` defines 18 protocol-level
commands:

```
vkCreateRingMESA           vkDestroyRingMESA         vkNotifyRingMESA
vkWriteRingExtraMESA       vkWaitRingSeqnoMESA       vkExecuteCommandStreamsMESA
vkSetReplyCommandStreamMESA vkSeekReplyCommandStreamMESA
vkSubmitVirtqueueSeqnoMESA vkWaitVirtqueueSeqnoMESA
vkGetMemoryResourcePropertiesMESA
vkImportSemaphoreResourceMESA vkWaitSemaphoreResourceMESA
vkResetFenceResourceMESA
vkCopyImageToMemoryMESA    vkCopyMemoryToImageMESA
vkWriteResourceDescriptorMESA vkWriteSamplerDescriptorMESA
```

Note the shape of this list. It is the set of things a forwarder needs *beyond*
Vulkan: ring lifecycle, seqno waits, reply-stream redirection, memory/semaphore
import by resource id, descriptor writes by resource, and — significantly —
explicit image copy commands (`vkCopyImageToMemoryMESA`,
`vkCopyMemoryToImageMESA`) for when zero-copy is not available. We will need an
analogous set, and this is a good checklist.

#### 1.1.4 The transport abstraction — and its limits

This is the most important structural finding about Venus. `vn_renderer.h`
defines a clean vtable:

```c
struct vn_renderer {
   struct vn_renderer_info info;
   struct vn_renderer_ops       ops;        /* submit, wait */
   struct vn_renderer_shmem_ops shmem_ops;  /* create, destroy */
   struct vn_renderer_bo_ops    bo_ops;     /* create_from_device_memory,
                                               create_from_dma_buf, map,
                                               flush, invalidate,
                                               export_dma_buf, export_sync_file */
   struct vn_renderer_sync_ops  sync_ops;   /* create, read, write, reset, … */
};
```

There are exactly two implementations:

- **`vn_renderer_virtgpu.c`** (1262 lines) — `DRM_IOCTL_VIRTGPU_*` against
  `/dev/dri/renderD*`. Requires a virtio-gpu device and libdrm. Guest-only.
- **`vn_renderer_vtest.c`** (1080 lines) — **a plain `AF_UNIX` `SOCK_STREAM`
  socket** to virglrenderer's `virgl_test_server`.

The existence of the vtest backend is genuinely significant: it proves the
Venus protocol runs with no VM, no virtio-gpu, and no kernel driver, over a
socket. That is 90% of what we want.

The remaining 10% is the part that kills it. Look at how vtest gets memory
across:

```c
/* vn_renderer_vtest.c, vtest_bo_map() */
void *ptr =
   mmap(placed_addr, bo->base.mmap_size, PROT_READ | PROT_WRITE,
        MAP_SHARED | (placed_addr ? MAP_FIXED : 0), bo->res_fd, 0);
```

`bo->res_fd` arrives over the socket via `SCM_RIGHTS` (the file confirms
`cmsg->cmsg_type != SCM_RIGHTS` handling at line 144). **vtest's "shared
memory" is file-descriptor passing.** That works between two processes on one
kernel and is meaningless across a VM boundary. And `vtest_bo_flush` and
`vtest_bo_invalidate` are both literally `/* nop */`, because the mapping is
genuinely the same physical memory.

The vtest map path also carries this comment, which is worth reading twice:

```c
/* We wrongly assume that mmap(dma_buf) and vkMapMemory(VkDeviceMemory)
 * are equivalent when the blob type is VCMD_BLOB_TYPE_HOST3D.  While we
 * check for VCMD_PARAM_HOST_COHERENT_DMABUF_BLOB, we know vtest can lie.
 */
```

#### 1.1.5 Is Venus fundamentally tied to guest-borrows-host-GPU?

Split the question in two, because the answer differs.

**The protocol: no.** `VK_EXT_command_serialization` is a byte stream in a
shared buffer, plus a reply stream, plus object ids. Nothing in the 134-line
spec assumes a direction. The ring is a symmetric SPSC circular buffer. The
`VkCommandTypeEXT` dispatch is direction-neutral. You could run it over a
carrier pigeon.

**The implementation: yes, thoroughly.** Every direction-specific assumption
lives below `vn_renderer`, and all of them point host→guest:

1. `vn_renderer_virtgpu` issues guest-kernel DRM ioctls to a virtio-gpu device.
   There is no host-side equivalent; the host is the thing being called.
2. Ring and reply shmem are *renderer-allocated resources injected into the
   guest*. `vn_MapMemory2` in `vn_device_memory.c` says so plainly:
   "`vn_renderer_bo_map` will block until the renderer creates the resource and
   injects the pages into the guest."
3. Host-visible `VkDeviceMemory` is exported by the host as a dma-buf and
   mapped into guest physical address space. Mesa's own documentation
   (docs.mesa3d.org/drivers/venus.html) states the renderer "exports device
   memory as mmapable dma-bufs and passes mappings to KVM for guest access
   setup" — i.e. `KVM_SET_USER_MEMORY_REGION`. Only the hypervisor can do that,
   and only in the host→guest direction.
4. Sync uses `virtgpu` `drm_syncobj` and virtio-gpu fence contexts —
   guest-kernel facilities.

The same documentation is refreshingly candid that even in the supported
direction this is on thin ice:

> The renderer makes assumptions about `VkDeviceMemory` that has
> `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT` that are illegal and rely on current
> behaviors of host drivers.

and Mesa's own `vn_MapMemory2` agrees:

```c
/* XXX We also assume that a vn_renderer_bo can be created as long as the
 * renderer VkDeviceMemory has a mappable memory type.  That is plain
 * wrong.  It is impossible to fix though until some new extension is
 * created and supported by the driver … */
```

**Verdict on Venus: reuse the protocol and the ring design; do not attempt to
reuse `vn_renderer` or `vkr`'s transport.** Concretely, we can lift
`VK_EXT_command_serialization.xml`, `VK_MESA_venus_protocol.xml`,
`vn_protocol.py` and `vkxml.py` (all MIT), keep the wire format, and write our
own transport layer where `vn_renderer` sits. That is a real head start on the
hardest-to-get-right part (the encoder/decoder for 456 commands) and leaves us
owning only the part that is genuinely novel.

#### 1.1.6 Venus on Windows — an important existence proof

virglrenderer's venus renderer is not Linux-only. Two pieces of evidence:

- `virglrenderer/src/venus/` contains `vkr_metal_helpers.m` — an Objective-C
  file, i.e. a macOS host backend already exists in tree.
- **WINQ-EMU** (<https://github.com/cmspam/winq-emu>) is a QEMU distribution
  for Windows hosts that, per its README, "ported virglrenderer's Venus Vulkan
  forwarding to Windows", including "Windows dma-buf shims enabling Vulkan
  surface imports". Stated licences: QEMU GPL-2.0, virglrenderer MIT,
  launcher MIT.

WINQ-EMU's *direction* is conventional (Windows host owns the GPU, Linux guest
borrows it), so it is not our topology. But it establishes that **a Venus-style
renderer/server can run on Windows**, which is what our server side needs. That
removes a risk I would otherwise have rated highly.

I could not confirm whether WINQ-EMU's Windows virglrenderer patches are
upstreamed, nor how complete they are. Their README says the patches are
"minimal, upstreamable" and that "anyone is welcome to … merge these changes
into upstream projects", but I have not verified the diff.

### 1.2 VAR — Vulkan API Remoting (academic)

A Penn State MSc thesis, "VAR: Vulkan API Remoting for GPU-accelerated
Rendering and Computation in Virtual Machines".
Catalog: <https://etda.libraries.psu.edu/catalog/18051yxl715>
PDF: <https://etda.libraries.psu.edu/files/final_submissions/22320>

This is directly on point: it implements an **ICD** that forwards to a VAR
server, and it names exactly our four hard problems — "communication overhead,
memory mapping, remote presentation, and multi-threading". Reported result:
"satisfactory frame rates in a variety of workloads, with particularly low
overheads when the computation load per frame is high", i.e. it does well when
the boundary crossing is amortised over heavy GPU work and badly otherwise.

**I could not retrieve the full text.** The PDF endpoint returns 403 to
WebFetch and resets the connection to `curl` from this host. The summary above
is from search-result snippets and the catalogue abstract, not from reading the
thesis. It is worth someone retrieving it manually — of all the prior art, this
is the one whose problem statement matches ours most exactly, and its memory
mapping and presentation chapters would be the most directly useful thing we
could read.

### 1.3 Other socket/shared-memory Vulkan forwarders

- **proxy-vulkan** (<https://github.com/feserr/proxy-vulkan>) — "send local
  Vulkan calls to the cloud", Cap'n Proto over TCP. Small and, from the
  description, a proof of concept rather than a usable implementation. Useful
  only as a sanity check that the ICD-shim approach is buildable. I did not
  assess its licence or completeness in detail.
- **Vulkan-Tools `icd/`** (<https://github.com/KhronosGroup/Vulkan-Tools/blob/main/icd/README.md>)
  — Khronos's own mock ICD. Not a forwarder, but it is a reference for the
  minimum an ICD must implement, and it is Apache-2.0.
- **VulkanSC-Emulation `icd/`**
  (<https://github.com/KhronosGroup/VulkanSC-Emulation/blob/main/icd/README.md>)
  — a real, shipping ICD that is a thin layer over another Vulkan
  implementation. Structurally the closest Khronos-maintained example of "an
  ICD that is not a driver".

---

## 2. Memory — the part that decides whether this works

### 2.1 The constraint that cannot be negotiated away

From the Vulkan specification source
(`Vulkan-Docs/chapters/memory.adoc`, lines 675-681,
<https://raw.githubusercontent.com/KhronosGroup/Vulkan-Docs/main/chapters/memory.adoc>):

```
There must: be at least one memory type with both the
ename:VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT and
ename:VK_MEMORY_PROPERTY_HOST_COHERENT_BIT bits set in its
pname:propertyFlags.
There must: be at least one memory type with the
ename:VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT bit set in its
pname:propertyFlags.
```

So the obvious escape hatch — advertise only non-coherent `HOST_VISIBLE`
memory, forcing applications to call `vkFlushMappedMemoryRanges` and
`vkInvalidateMappedMemoryRanges` where we can intercept them — **is not legal**.
Every conformant implementation must expose coherent host-visible memory, which
means an application may legally do:

```c
vkMapMemory(dev, mem, 0, VK_WHOLE_SIZE, 0, &p);   // once, at startup
/* … write to p forever, never calling another memory function … */
```

and expect the device to see those writes. There is no API call to hook.

This is the single hardest requirement in the whole project, and it is the
reason every existing forwarder maps real pages instead of copying.

### 2.2 How Venus solves it, and why we cannot copy the solution

Venus's `vkMapMemory` is not a copy and not an emulation. Reading
`vn_MapMemory2` in `src/virtio/vulkan/vn_device_memory.c`, the entire body is:
lazily create a renderer bo for the memory, then

```c
ptr = vn_renderer_bo_map(dev->renderer, mem->base_bo, placed_addr);
…
*ppData = (char *)ptr + offset;
```

The returned pointer *is* host GPU memory, made visible in the guest. In the
virtgpu backend that is `DRM_IOCTL_VIRTGPU_MAP` followed by `mmap` of the
resulting offset — the pages live in the virtio-gpu host-visible window, a PCI
BAR into which QEMU has mapped host memory via `KVM_SET_USER_MEMORY_REGION`.
In the vtest backend it is `mmap` of a dma-buf fd received over `SCM_RIGHTS`.

Correspondingly `vn_FlushMappedMemoryRanges` just forwards to
`vn_renderer_bo_flush`, which in the vtest backend is `/* nop */`. There is no
data movement anywhere in the mapped-memory path.

Every mechanism in that list moves **host** pages into a **guest**, or shares
pages between two processes on one kernel. We need to expose **guest** pages to
a **host** process (and eventually to a *different machine*). There is no
virtio-gpu equivalent running backwards.

### 2.3 What our topology gives us instead

Two facts from `hardware-constraints.md` change the shape of the problem in our
favour:

1. The guest already runs on `memory-backend-memfd,share=on` (virtio-fs
   requires it). **All guest RAM is already an mmap-able object in the host.**
2. Under VFIO every guest page is pinned anyway, so there is no
   ballooning/migration hazard from treating guest RAM as stable.

So in principle the host can already read and write any guest page. The
obstacle is purely one of *addressing*: a Windows user-mode process cannot tell
the host which guest-physical pages its buffer occupies, and its virtual
allocation is not physically contiguous. Obtaining a GPA from user mode needs a
kernel driver (`MmGetPhysicalAddress` on non-paged pool), which brings us back
to the signing problem.

ivshmem sidesteps the addressing problem entirely: QEMU maps one host object
(`memory-backend-file` / `-shm` / `-memfd`) into guest physical space as BAR2,
so both sides have a flat, agreed, contiguous window and offsets within it are
a common language. From the QEMU spec
(<https://raw.githubusercontent.com/qemu/qemu/master/docs/specs/ivshmem-spec.rst>):

> The Inter-VM shared memory device (ivshmem) is designed to share a memory
> region between multiple QEMU processes running different guests **and the
> host**. […] BAR2 maps the shared memory object.

and

> If you only need the shared memory part, BAR2 suffices. […] If you
> additionally need the capability for peers to interrupt each other, you need
> BAR0 and BAR1. **You will most likely want to write a kernel driver to handle
> interrupts.**

Status check: only the *legacy* `ivshmem` device was removed (QEMU 4.0, per
`docs/about/removed-features.rst`); `ivshmem-plain` and `ivshmem-doorbell` are
current and appear in neither `deprecated.rst` nor `removed-features.rst`.

Symmetry is real here: the region is one host object, mapped by the host
process directly and by the guest through a BAR. There is no privileged
direction. This is the property the design needs and it is why ivshmem is worth
fighting for.

### 2.4 The zero-copy lever: `VK_EXT_external_memory_host`

There is a mechanism that would let the **server** (inside the guest) make the
shared region directly GPU-accessible, closing the loop with no copy at all.

`VK_EXT_external_memory_host` (confirmed present in local `vk.xml`, extension
number 179, `supported="vulkan,vulkansc"`, ratified) defines
`VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT`,
`VkImportMemoryHostPointerInfoEXT` and `vkGetMemoryHostPointerPropertiesEXT`.
It lets an application import **an ordinary host pointer** as `VkDeviceMemory`.

On an Intel iGPU this is exactly the right primitive, because the iGPU has no
dedicated VRAM — "device memory" *is* system RAM. If the Windows Intel driver
supports the extension, the server can:

1. `mmap`/`MapViewOfFile` a slab of the ivshmem region,
2. import it as `VkDeviceMemory` via `VkImportMemoryHostPointerInfoEXT`,
3. hand the resulting memory to the GPU directly.

The client writes into the shared region; the GPU reads the same physical
pages. **Zero copy across the VM boundary, in both directions, for exactly the
memory type we are required to expose.** That would turn the hardest problem in
this project into a non-problem.

Constraints on it: the pointer must be aligned to
`VkPhysicalDeviceExternalMemoryHostPropertiesEXT::minImportedHostPointerAlignment`
(confirmed present in `vk.xml` with `limittype="min,pot"`), and
`vkGetMemoryHostPointerPropertiesEXT` must return a non-zero `memoryTypeBits` —
drivers are free to return zero, and there are reports of
`VK_ERROR_OUT_OF_DEVICE_MEMORY` from this path on some drivers.

**Unconfirmed and important.** Search results indicate Intel's Windows driver
has listed `VK_EXT_external_memory_host` since roughly version 31.0.101.3790
(<https://www.geeks3d.com/20221031/intel-graphics-driver-31-0-101-3790-released-vulkan-1-3-228/>),
but I could not verify this from a primary source. `vulkan.gpuinfo.org`'s
report pages are JavaScript-driven and its `listdevicescoverage.php` and
`/api/v3/` endpoints returned no usable data to `curl`. **This must be checked
on the actual machine**, and it is a five-minute check: run `vulkaninfo` in the
Windows guest and grep for `VK_EXT_external_memory_host`, then confirm
`minImportedHostPointerAlignment` and that
`vkGetMemoryHostPointerPropertiesEXT` on an ivshmem-backed pointer returns
non-zero `memoryTypeBits`. Whether that last call succeeds on a *PCI BAR*
mapping rather than ordinary RAM is a further question I cannot answer from
documentation — BAR memory may well be rejected or be uncached, which would
make it correct but slow. Test it before designing around it.

### 2.5 If we cannot share pages: emulating coherent memory

If the transport is a socket (no shared pages), coherent mapped memory has to
be emulated. It is not hopeless, because the Vulkan memory model provides two
sync points we can legally hang the data movement on.

**Host writes → device.** From `Vulkan-Docs/chapters/synchronization.adoc`
(lines 8398-8408,
<https://raw.githubusercontent.com/KhronosGroup/Vulkan-Docs/main/chapters/synchronization.adoc>):

> The first synchronization scope includes execution of flink:vkQueueSubmit on
> the host and anything that happened-before it, as defined by the host memory
> model.

So writes an application makes to coherent mapped memory before calling
`vkQueueSubmit` are required to be visible to the device only *because of* the
submit. **Shipping dirty pages at submit time is therefore correct**, not a
hack. The same section warns that non-temporal stores are outside the host
memory model and are the application's problem — which conveniently means we
inherit that caveat rather than owning it.

**Device writes → host.** Symmetrically, the host is only entitled to see
device writes after a fence/semaphore wait completes. So pulling memory back
when a fence signals is the corresponding correct point.

That gives a workable emulation:

- Allocate a shadow buffer on the client and return it from `vkMapMemory`.
- Track dirty pages: `mprotect(PROT_READ)` + `SIGSEGV` handler on Linux,
  `PAGE_GUARD` + vectored exception handler on Windows. Re-arm after each
  fault. (Windows also has `GetWriteWatch` with `MEM_WRITE_WATCH`, which is
  purpose-built for this and much cheaper than exception handling — worth
  preferring on the client-Windows case. I have not verified its interaction
  with memory that must also be readable by an application at full speed.)
- Flush dirty ranges on `vkQueueSubmit`, `vkFlushMappedMemoryRanges`,
  `vkQueueBindSparse`, and any host-visible-memory-touching command.
- Invalidate (pull back) on fence/semaphore wait completion and on
  `vkInvalidateMappedMemoryRanges`.

Costs and holes, honestly:

- Page-fault-per-first-write-per-page. For a streaming uniform buffer rewritten
  every frame this is a fault per page per frame — thousands of faults per
  second. Measurable but survivable; `GetWriteWatch` avoids the faults entirely
  on Windows.
- Bandwidth: you ship the dirty set every submit. A large persistently-mapped
  staging buffer degenerates to shipping the whole thing.
- **`VK_KHR_buffer_device_address` defeats it completely.** Once an application
  has a raw 64-bit device address it can write that address into a buffer and
  have a shader dereference it. There is no host-side call to intercept, and the
  address space is the server's, not ours. Any client-side shadow copy is
  invisible to such a dereference unless the shadow is at the same address on
  the server — which is possible in principle
  (`VkBufferOpaqueCaptureAddressCreateInfo` / `VK_MEMORY_MAP_PLACED_BIT_EXT`)
  but is a research project in itself. See §5 for who requires BDA.
- `VK_EXT_descriptor_buffer` has the same shape of problem: descriptors are
  written by the application straight into mapped memory.

So: emulation is a legitimate fallback for a *subset* of workloads, and it is
almost certainly enough for Zink. It is very unlikely to be enough for
vkd3d-proton. Plan for shared pages; keep emulation as the degraded mode.

### 2.6 Large resource transfer

Two regimes:

**With a shared region.** Client stages directly into the shared region and the
server imports it (§2.4) or does a device-side copy from an imported staging
buffer. Zero or one copy. This is the Venus/gfxstream model.

**Without.** The command stream carries the bytes. Venus provides the pattern:
`vkCopyMemoryToImageMESA` / `vkCopyImageToMemoryMESA` with an inline
`pData`/`dataSize` blob (confirmed in `VK_MESA_venus_protocol.xml`:
`<member len="dataSize">const <type>void</type>* <name>pData</name></member>`).
This is the explicit "we could not share, so here are the bytes" path, and the
fact that Venus needed it even *with* virtio-gpu available tells you it is not
an edge case.

Zero-copy across a VM boundary is achievable if and only if the two sides map
the same physical pages. That is ivshmem, or nothing. There is no clever
protocol trick that recovers it.

### 2.7 Presentation

The rendered image is on a GPU inside the VM; the display is on the client.
This is the one place where our inverted topology is **strictly worse** than
Venus. In conventional virtio-gpu the guest renders and the *host* displays, so
the finished frame is already on the side that owns the screen. For us it must
cross the boundary every frame, forever.

The good news is that this exact problem is solved in production by **Looking
Glass** (<https://looking-glass.io/>), which has our topology precisely: GPU in
the guest, display on the host. Its approach, in its own words:

> Looking Glass does not use any form of compression or color space conversion,
> all frames are transferred to the viewer (client application) in 32-bit RGBA
> without any transformations or modifications. This is possible through the
> use of a shared memory segment which enables extremely high throughput low
> latency guest to host communication.

Licence: **GPL-2.0** (Geoffrey McRae / HostFission). That is viral, so we can
study the architecture but cannot lift code into an MIT/Apache project.

Realistic options, with costs:

| Option | Mechanism | Bandwidth at 1080p60 / 4K60 | Latency | Notes |
|---|---|---|---|---|
| Uncompressed via shared memory | Server blits swapchain image to linear buffer in shared region; client presents | 8.3 MB/frame → **~500 MB/s** / 2 GB/s | Sub-frame | Looking Glass's approach. Needs ivshmem-class transport. |
| Uncompressed via socket | Same, but copied over TCP/virtio-serial | Same bandwidth, now on the wire | +copy | ~4 Gbit/s at 1080p60 just for pixels. Plausible on virtio-net, hopeless on virtio-serial. |
| Hardware video encode | Server encodes with Intel QuickSync, client decodes | 10-50 Mbit/s | +1 frame or more | The Sunshine/Moonlight model. Lossy. The right answer for a *remote* client, wrong for a local one. |
| Damage-tracked / tiled | Ship only changed tiles | Workload-dependent | Variable | Big win for desktop/GUI, no win for a game. |

Note a wrinkle specific to us: the client here is a machine with **no GPU**, so
whatever the client does to get pixels onto a screen runs on llvmpipe. A
full-screen RGBA blit per frame in software is fine, but it is not free, and it
rules out doing anything clever client-side.

I would implement the swapchain as: client-side `VK_KHR_swapchain` that
allocates its images as server-side `VkImage`s, and on `vkQueuePresentKHR`
issues a server-side copy into a linear buffer in the shared region, then
signals the client. Fall back to encode-and-stream only for genuinely remote
clients.

### 2.8 Synchronisation

Venus's model, from `vn_renderer.h`:

```c
/*
 * A sync consists of a uint64_t counter.  The counter can be updated by CPU
 * or by GPU.  It can also be waited on by CPU or by GPU until it reaches
 * certain values.
 *
 * This models after timeline VkSemaphore rather than timeline drm_syncobj.
 */
```

That is the right primitive and it is direction-neutral: **model everything as
a monotonic 64-bit timeline**, which is exactly `VkSemaphore` with
`VK_SEMAPHORE_TYPE_TIMELINE` (core in Vulkan 1.2). Binary semaphores and fences
are expressible on top of timelines; the reverse is not true.

Concretely:

- **Semaphores used only between server-side queues** never cross the boundary
  at all. Encode the submit, let the server's real driver handle it. This is
  the overwhelming majority of synchronisation in a real application and it
  costs us nothing.
- **Fences and host waits** do cross. Venus handles this with ring seqnos:
  `vkWaitRingSeqnoMESA`, `vn_ring_wait_seqno`, and a per-queue timeline bound
  to a `ring_idx` (`VkDeviceQueueTimelineInfoMESA`). The client blocks on a
  counter in the shared region that the server advances. With a shared region
  this is a load-acquire in a spin/sleep loop — sub-microsecond. Over a socket
  it is a round trip, so every `vkWaitForFences` costs at least one RTT.
- **Timeline semaphore host operations** (`vkWaitSemaphores`,
  `vkSignalSemaphore`, `vkGetSemaphoreCounterValue`) map onto the same
  mechanism. `vkGetSemaphoreCounterValue` in a polling loop is the pathological
  case; mirror the counter into the shared region so polling is local.
- **External semaphores/fences** (`VK_KHR_external_semaphore_fd`, `_win32`) are
  in the "not serializable" bucket per the wire spec. Venus punts with
  `vkImportSemaphoreResourceMESA` — import by resource id rather than by
  handle. We would do the same, and simply not advertise external
  semaphore/fence handle types we cannot honour, which is legal.

The cheap and important optimisation: `vkQueueSubmit` must be **asynchronous**
— encode and return. Venus does this (only commands with
`VK_COMMAND_GENERATE_REPLY_BIT_EXT` block). If submit is synchronous the design
is dead on latency grounds regardless of transport.

---

## 5. What Zink, DXVK and vkd3d-proton actually demand

This section decides project scope, and the answer is not the one the plan
assumes. **The hourglass does not have a single waist.** Zink is comfortably
achievable; modern DXVK and vkd3d-proton are not, for one specific reason.

### 5.1 Zink — achievable, and there is direct precedent

Status confirmed: Mesa's `docs/features.txt` lists zink as DONE for
**GL 4.6 / GLSL 4.60** and **GLES 3.2 / GLSL ES 3.2**, and zink appears on the
Khronos conformant-products list for OpenGL 4.6 (Collabora, on PowerVR).
- <https://gitlab.freedesktop.org/mesa/mesa/-/raw/main/docs/features.txt>
- <https://www.khronos.org/conformance/adopters/conformant-products/opengl>
- <https://docs.mesa3d.org/drivers/zink.html>

Hard requirements — only five extensions carry `required=True` in
`src/gallium/drivers/zink/zink_device_info.py`, whose header comment explains
the semantics ("returns NULL if the extension is unavailable"):
`VK_KHR_maintenance1`, `VK_KHR_maintenance5`, `VK_KHR_create_renderpass2`,
`VK_KHR_dynamic_rendering`, `VK_KHR_descriptor_update_template`.

Plus one hard runtime bail-out in `zink_screen.c`:

```c
if (!screen->info.have_KHR_timeline_semaphore && !screen->info.feats12.timelineSemaphore) {
   mesa_loge("zink: KHR_timeline_semaphore is required");
   goto fail;
}
```

The authoritative full requirement set is the machine-readable profile
`src/gallium/drivers/zink/VP_ZINK_requirements.json`, which is what
`zink_check_requirements` validates against. It declares
`VP_ZINK_gl21_baseline` at `api-version: 1.2.0` and the GL4.3+ profiles at
`1.3.0` — note this contradicts the prose in `zink.rst`, which says "Vulkan
1.0". **Treat the JSON as authoritative.** The GL2.1 baseline additionally
wants `VK_KHR_imageless_framebuffer`, `VK_EXT_custom_border_color` (with
`customBorderColorWithoutFormat`), `VK_EXT_provoking_vertex`,
`VK_EXT_line_rasterization`, `VK_KHR_swapchain_mutable_format`,
`VK_EXT_border_color_swizzle`, `VK_KHR_incremental_present`, and
`scalarBlockLayout`. Climbing to GL 4.6 adds `VK_EXT_transform_feedback` (at
GL 3.0), `VK_EXT_conditional_rendering`, `VK_EXT_depth_clip_enable`,
`VK_EXT_vertex_attribute_divisor` (GL 3.3), `VK_KHR_maintenance2`,
`VK_EXT_image_2d_view_of_3d`, `VK_KHR_draw_indirect_count`, and a pile of
ordinary core features and format/limit requirements.

The crucial negatives for us:

- **`VK_KHR_buffer_device_address` is optional.** In `zink_device_info.py` it is
  a plain entry with no `required=True`.
- **`VK_EXT_descriptor_buffer` is optional** (`gl46_optional`, opt-in via
  `ZINK_DESCRIPTORS=db`; the default `auto` uses classic descriptor sets).
- **dma-buf / external memory are optional** — needed only for the DRI3/GBM
  path and GL-CL interop. The Kopper WSI path needs only `VK_KHR_swapchain`.
- `VK_EXT_extended_dynamic_state*` and `VK_EXT_graphics_pipeline_library` are
  in the `gl46_optimal` *performance* tier, not the correctness tier.

And the single best piece of evidence in this entire document: **Mesa's own CI
runs zink on top of Venus.** From
`src/gallium/drivers/zink/ci/gitlab-ci.yml`:

```yaml
zink-lavapipe:            DEQP_SUITE: zink-lavapipe   VK_DRIVER: lvp
zink-venus-lavapipe:      DEQP_SUITE: zink-venus-lavapipe
zink-venus-lavapipe-full: ...
```

That is GL-on-Vulkan running on a *forwarded* Vulkan across a virtio-gpu
boundary, on a CPU backend, gated in upstream CI. Our GL story has precedent
and a reference configuration to compare against.

The one unavoidable cost: zink does use persistently mapped
`HOST_VISIBLE|HOST_COHERENT` memory for `GL_ARB_buffer_storage`
(`PIPE_MAP_PERSISTENT` handling in `zink_resource.c`), which is mandatory from
GL 4.4. So §2's coherent-memory problem is not avoidable even for the Zink-only
scope — it is just smaller.

### 5.2 DXVK — bimodal, and the modern branch is hostile

The task brief's "DXVK 2.x" framing is out of date: the current series is
**3.0.x**, and the decisive change landed in **2.7**.

Minimum API version, `src/dxvk/dxvk_instance.h`:

```cpp
constexpr uint32_t DxvkVulkanApiVersion = VK_API_VERSION_1_3;
```

In practice you need a Vulkan **1.4**-class feature set, because `maintenance5`
and `maintenance6` are hard-required and both are 1.4 core. The project wiki
says so outright: "For DXVK 3.x … A compatible driver should support Vulkan
1.4" (<https://github.com/doitsujin/dxvk/wiki/Driver-support>).

Required device extensions in 3.x (`getFeatureList()` in
`src/dxvk/dxvk_device_info.cpp`, `require=true` entries):
`VK_EXT_depth_clip_enable`, `VK_EXT_robustness2` (`robustBufferAccess2` **and**
`nullDescriptor`), `VK_KHR_load_store_op_none`, `VK_KHR_maintenance5`,
`VK_KHR_maintenance6`, `VK_KHR_swapchain`. Required Vulkan 1.2 features include
`descriptorIndexing`, `runtimeDescriptorArray`,
`descriptorBindingPartiallyBound`, `timelineSemaphore`, `scalarBlockLayout`,
`vulkanMemoryModel`, `hostQueryReset`, `samplerMirrorClampToEdge`, `shaderInt8`
— and, fatally, `bufferDeviceAddress`.

Also a limit you must not under-advertise: `maxPushConstantsSize >= 256`
(`MaxTotalPushDataSize` in `src/dxvk/dxvk_limits.h`), unless you expose
`VK_EXT_descriptor_heap`.

**The killer.** DXVK 3.x compiles shaders with the `dxbc-spirv` backend, which
emits, unconditionally, for every shader
(`dxbc-spirv/spirv/spirv_builder.cpp`):

```cpp
enableCapability(spv::CapabilityPhysicalStorageBufferAddresses);
enableCapability(spv::CapabilityVulkanMemoryModel);
pushOp(m_memoryModel, spv::OpMemoryModel,
  spv::AddressingModelPhysicalStorageBuffer64,
  spv::MemoryModelVulkan);
```

and binds UBOs/SSBOs as raw 64-bit addresses delivered through push constants
(`dxvk_pipelayout.h` returns `sizeof(VkDeviceAddress)` for uniform/storage
buffer block entries; `dxvk_context.cpp` fills them with
`slice.getSliceInfo().gpuAddress`). This is the pipeline layout model, not an
optional fast path.

By contrast **DXVK ≤ 2.6.x had only two required device extensions**
(`VK_EXT_robustness2` and `VK_KHR_swapchain`) and enabled
`bufferDeviceAddress` only opportunistically for NVX CUDA interop, with the
source comment "VK_KHR_buffer_device_address is expensive on some drivers".

So: **DXVK ≤ 2.6.x is a realistic target for a forwarder; DXVK ≥ 2.7 is not**,
unless we can make server VAs valid in the client's address space.

Secondary note: `VK_EXT_transform_feedback` is *optional* to DXVK, but without
it `CreateGeometryShaderWithStreamOutput` returns `DXGI_ERROR_INVALID_CALL`
while the device still advertises FL 11_0+. That is a hard-fail for any title
using stream output, not graceful degradation.

### 5.3 vkd3d-proton — out of scope, structurally

`include/vkd3d.h`:

```c
#define VKD3D_MIN_API_VERSION VK_API_VERSION_1_3
```

Hard-enforced requirements in `libs/vkd3d/device.c` (each aborts device
creation): `samplerMirrorClampToEdge`, `robustBufferAccess2` +
`robustImageAccess2` + `nullDescriptor`, `shaderDrawParameters`,
`VK_KHR_push_descriptor`, and `maintenance5` + `maintenance6` — the last two
being 1.4 core and *not* documented in the README. Plus a descriptor-indexing
floor of **1,000,000** update-after-bind sampled images / storage images /
storage buffers per stage and 2,048 samplers
(`VKD3D_MIN_VIEW_DESCRIPTOR_COUNT` in `vkd3d_private.h`).

Encouragingly, a lot is genuinely optional: `VK_EXT_descriptor_buffer`,
`VK_EXT_mutable_descriptor_type`, all ray tracing (`NO_DXR` disables it
cleanly), and sparse/tiled resources (costing only `TiledResourcesTier`).

But buffer device address is not optional and cannot be made so, because
**the D3D12 API itself exposes `ID3D12Resource::GetGPUVirtualAddress()` to the
application**. Every buffer gets the usage bit unconditionally in
`libs/vkd3d/resource.c`:

```c
buffer_info.usage |= VK_BUFFER_USAGE_SHADER_DEVICE_ADDRESS_BIT;
```

There is an entire VA reverse-lookup subsystem (`libs/vkd3d/va_map.c`) to map
an application-supplied VA back to a resource, and vkd3d writes VAs into a
host-visible GPU buffer (`raw_va_aux_buffer`) that shaders read and then
dereference — writes a proxy never observes. The dxil-spirv backend turns on
`AddressingModelPhysicalStorageBuffer64` whenever root descriptors exist, i.e.
essentially always, and unconditionally for ray tracing.

**Verdict: vkd3d-proton cannot be supported by a forwarder that does not make
server GPU virtual addresses directly meaningful on the client.** This is not a
matter of effort; it is a property of the D3D12 contract.

### 5.4 Consolidated hostility table

| Proxy-hostile feature | Zink | DXVK ≤2.6 | DXVK ≥2.7/3.x | vkd3d-proton |
|---|---|---|---|---|
| `bufferDeviceAddress` / raw VAs in app data | No (optional) | No (NVX interop only) | **Required** | **Structural** |
| Shaders using `PhysicalStorageBuffer64` | No | No | **Unconditional** | **Effectively always** |
| Persistently mapped `HOST_VISIBLE\|HOST_COHERENT` | **Yes** (GL 4.4) | Yes | Yes | Yes |
| `VK_EXT_descriptor_buffer` / `_heap` | Optional | n/a | Optional | Optional |
| External memory / dma-buf | Optional | Optional | Optional | Optional |
| Timeline semaphores + host waits | **Required** | Required | Required | Required |
| Sparse / tiled resources | Optional | Optional (costs FL) | Optional (costs FL) | Optional (costs tier) |
| Ray tracing | n/a | n/a | n/a | Optional |
| Minimum Vulkan level | 1.2 / 1.3 | 1.3 | 1.3 code, **1.4 in practice** | 1.3 code, **1.4 in practice** |

### 5.5 What this does to the plan

The stated goal — "Vulkan is the single waist of the hourglass, and we get GL
via Zink and D3D via DXVK/vkd3d for free" — holds for **GL via Zink** and for
**D3D9/10/11 via DXVK ≤ 2.6**. It does **not** hold for D3D12 via
vkd3d-proton, and it does not hold for current DXVK, unless the design commits
up-front to identity-mapped GPU virtual addresses.

That is a scoping decision to take now, not later. Three coherent positions:

1. **Zink-only first.** Target Zink, ship GL, treat DXVK ≤ 2.6 as a stretch.
   Lowest risk, real utility, and Mesa CI already proves the configuration.
2. **Commit to VA identity.** Design so that server VAs are reproduced on the
   client. Vulkan does provide the machinery in principle —
   `VkBufferOpaqueCaptureAddressCreateInfo`,
   `VkMemoryOpaqueCaptureAddressAllocateInfo` (both core 1.2, used by capture
   tools like RenderDoc for exactly this replay problem) and
   `VK_MEMORY_MAP_PLACED_BIT_EXT` for placing host mappings. This is a
   significant research effort with real chance of failure, and it does not
   help unless the *client-side* mapped memory is also at the right address.
3. **Accept D3D12 is out of scope** and route it through something else
   entirely (run the D3D12 application inside the guest, next to the GPU).

I recommend (1) with (3) as the honest answer for D3D12, and revisiting (2)
only if Zink works well and there is appetite.

---
