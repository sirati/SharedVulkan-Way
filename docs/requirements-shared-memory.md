# Requirement: real shared memory, not a copying RPC

## Terminology, fixed here to stop the confusion

"Host" and "guest" are useless words for this project, because the machine
that owns the GPU is the VM. Everywhere in this repo:

- **GPU side** — the process with the real Vulkan driver and the physical GPU.
  On testhost this is *inside the Windows VM*. The user has also called this
  "the host", in the sense of the host of the graphics device.
- **App side** — the process running the application that wants to render, and
  which we lie to with a Vulkan ICD. On testhost this is the *Linux host*.
  The user has also called this "the guest".

Either side may be Linux or Windows, and either may be the VM. Nothing in the
design may assume otherwise.

## The requirement

The application's memory must be genuinely shared with the GPU, not copied
across a boundary. Concretely: when the app side calls `vkAllocateMemory` on a
host-visible heap and then `vkMapMemory`, the pointer it receives must point at
pages that the GPU on the other side can access **directly, by DMA, without an
intervening copy**.

This is a stronger requirement than "make it fast", and it is not what sharedgl
does. sharedgl copies everything — three CPU copies per upload before the
driver sees the data — and its one `direct_access` zero-copy path exists but is
never enabled. Its readback does a blocking round trip per 3072 bytes. That
design cannot satisfy this requirement, so it is prior art for the *transport
and command encoding* only, not for memory.

## Therefore: two regions, not one

The shared memory is split by purpose, because the two halves have completely
different lifetimes, access patterns and safety properties.

### 1. The control region

Ours. Command rings, submission queues, return slots, fences, doorbells,
handshake (magic, version, generation counter). Small, fixed layout, written
and read by our code only. Its size is known at startup.

This is the part sharedgl is genuinely instructive about — and the part where
its single global `SUBMIT` int is the mistake to avoid.

### 2. The passthrough arena

A large region from which `VkDeviceMemory` allocations are carved. Mapped into
the app-side process so `vkMapMemory` can hand out a real pointer into it, and
simultaneously reachable by the GPU on the GPU side so the driver can bind it
to buffers and images.

The app writes vertices, uniforms and texture data straight into it. Nothing
marshals that data. The command stream carries *offsets into the arena*, not
payloads.

## What has to be true for this to work

These are the questions the design must answer with evidence, not hope:

1. **How does the GPU side turn arena pages into `VkDeviceMemory`?** The
   obvious candidate is `VK_EXT_external_memory_host`, which imports a plain
   host pointer as device memory
   (`VK_EXTERNAL_MEMORY_HANDLE_TYPE_HOST_ALLOCATION_BIT_EXT`). If the GPU-side
   driver supports it, the arena can be imported directly and the GPU DMAs from
   it. Its `minImportedHostPointerAlignment` then dictates the arena's
   allocator granularity. Does Intel's Windows driver expose it? Does ANV on
   Linux? If not, what is the fallback — dma-buf import, virtio-gpu blob
   resources, or something else?

2. **Where does the arena physically live?** Two candidates, with very
   different consequences:
   - An **ivshmem BAR**. Simple and symmetric, but it is MMIO rather than
     ordinary RAM, which affects whether a GPU driver will accept it as
     importable memory, and it needs a driver on Windows — with the signing
     problem that entails on a production machine.
   - **The VM's own RAM**, which on this machine is already a
     `memory-backend-memfd` with `share=on` (virtio-fs forced that), so the
     Linux side can already mmap guest RAM. If the arena is ordinary guest RAM,
     the GPU-side driver treats it as ordinary host-visible memory. The
     difficulty moves to pinning and to translating guest-virtual to an offset
     in the memfd, which in Windows is a kernel-mode job — reintroducing the
     driver problem from a different direction.
   Decide with evidence which is achievable without unsigned drivers.

3. **Coherency.** Who flushes what, and when. `vkFlushMappedMemoryRanges` and
   `vkInvalidateMappedMemoryRanges` become real operations that must be honoured
   across the boundary rather than no-ops. Non-coherent memory types may be the
   honest thing to advertise. Related: sharedgl maps its region
   `WRITECOMBINED`, which is right for a pure producer and wrong for a consumer,
   and in our inversion the roles are swapped from its assumption.

4. **What we advertise.** `vkGetPhysicalDeviceMemoryProperties` is where we
   tell the application what memory exists. Arena-backed memory should appear as
   a host-visible heap; anything that cannot be shared (device-local VRAM the
   app never maps) can stay on the GPU side and be moved with ordinary Vulkan
   transfer commands. Getting this table right is what makes applications
   naturally use the fast path instead of the slow one.

5. **Lifetime.** The arena is memory one side can write while the other reads,
   and `vkFreeMemory` has to behave sanely while the GPU may still be reading.

   Note this is a *correctness* question, not a security one. **This is not a
   security boundary** — see security-model.md. Do not propose copying,
   revalidation or shadow buffers whose purpose is to protect one side from the
   other; that would defeat the arena, and it could not be enforced anyway,
   since anything validated can be rewritten before it is read. Cheap measures
   that mostly catch our own bugs (bounds-checking an offset against the region
   length, handle tables) are worth taking. Everything else gets documented as
   a risk rather than defended against.

## Consequence for the protocol

Because the arena carries the data, the command stream carries only commands
and offsets. That keeps the ring small and makes the single-submission-in-flight
mistake unnecessary: rings can be per-queue and modest, since they never have to
absorb a texture upload.
