# shared_vulkan — architecture and design

Written 2026-08-11, against `requirements-shared-memory.md`, `security-model.md`,
`hardware-constraints.md`, `measured-gpu-side.md`, `api-scale.md`,
`research-sharedgl.md` and `research-vulkan-forwarding.md`.

This document **decides**. Where the research surveyed options, this picks one
and says why. Where a decision is genuinely close, it says what would change it.
Citations are to the research documents by section, e.g. (fwd §2.1) for
`research-vulkan-forwarding.md` §2.1 and (sgl §7.2) for `research-sharedgl.md`
§7.2.

Nothing here is implementation code. The layout tables are specifications.

---

## 1. Vocabulary, and the invariant everything rests on

- **GPU side** — the process holding the real Vulkan driver. On `testhost`
  this is inside the Windows VM.
- **App side** — the process running the application, served by our ICD. On
  `testhost` this is the Linux host.

Either side may be Linux or Windows; either may be the VM. No decision below
assumes which.

**The invariant:** the shared regions are mapped at a different base address on
each side, but **every byte has the same offset from the region base on both
sides**. Offsets are the only currency that crosses. No pointer, no host
address, no length derived from a pointer difference ever appears on the wire.
This is what makes the design direction-agnostic and what makes the arena work,
and it is checked once at handshake (§4.3) rather than assumed per call.

The second invariant, from the requirement document: **the command stream
carries offsets, never payloads.** A design that copies application data into
the ring is rejected regardless of how fast the copy is.

---

## 2. Decisions at a glance

| # | Question | Decision |
|---|---|---|
| D1 | Where does the arena live? | Ordinary **guest RAM**, allocated by whichever side is in the VM. On `testhost`: allocated by the GPU side via Windows **AWE**, mapped by the app side through QEMU's existing `memory-backend-memfd`. No ivshmem, no new device, no unsigned driver. (fwd §2.2) |
| D2 | Fallback if AWE's PFNs are not usable | An **attestation-signed ~200-line pinning driver**. ivshmem is not a fallback we plan for; it is a last resort we expect never to reach. (fwd §2.2 verdict table) |
| D3 | How does the GPU side get `VkDeviceMemory`? | `VK_EXT_external_memory_host`, `HOST_ALLOCATION_BIT_EXT`, **measured present, alignment 4096** (`measured-gpu-side.md`). Behind a pluggable `SlabImport` trait so dma-buf can be substituted on a Linux/AMD GPU side. |
| D4 | Import granularity | Per **slab** (default 64 MiB), not per `VkDeviceMemory`. One driver import per slab; app-side allocations are sub-ranges. Requires the `(memory, offset)` rewrite pass. (fwd §2.1) |
| D5 | Wire format | **Venus `VK_EXT_command_serialization`** encoding rules, reimplemented in Rust, with three deliberate deviations (§6.2). |
| D6 | pNext encoding | **gfxstream's** length-prefixed links with a 0 terminator; lenient app side, fatal GPU side. (fwd §1.3, §4.3) |
| D7 | Handles | **Venus's app-side-assigned ids on the wire** + **gfxstream's rich per-object table on the GPU side**. Object creation costs no round trip. (fwd §4.3) |
| D8 | Rings | **SPSC, per queue**, plus one per device and one per instance, plus one reverse ring. Spin-then-park, notification only when the consumer has parked. (fwd §1.2, §1.3) |
| D9 | Doorbell | Polling first, behind a `Doorbell`/`Parker` trait. Interrupts are an optimisation to be justified by measurement, not an architecture. (sgl §7.5) |
| D10 | Cold path | A socket: **virtio-serial** at first because it is already attached, **vsock** once `viosock` is installed. Handshake, capability negotiation, wakeups, teardown. Never data. (fwd §6.2, §6.3) |
| D11 | Cache attributes | **Write-back on both sides, everywhere, in v1.** The mechanism to vary it exists in the slab descriptor and has exactly one legal value until measurement says otherwise. This retires R10 rather than managing it. (§5.6) |
| D12 | Entry point | A **Vulkan ICD**, not a layer. Loader interface **7**, degrade to 5. (fwd §3.1) |
| D13 | Codegen input | **Pure Rust**: `vk-parse` plus our own C-declarator parser and typed model, in `vk-registry`. Cross-checked once against Khronos's `vulkan_object.py` as a test fixture. (§7.1) |
| D14 | Command buffers | Recorded straight into an arena block; `vkQueueSubmit` ships an `(offset, len)`. Zero round trips and zero copies from record to execute. (fwd §1.3, §7.2) |
| D15 | Submit | **Asynchronous**, non-negotiable. |
| D16 | Sync | Everything is a 64-bit monotonic timeline; counters mirrored into the control region so an app-side wait is a local load. (fwd §2.9) |
| D17 | Presentation | **Outside the protocol.** Headless first; a swapchain that blits GPU-side into a linear arena buffer second. (fwd §2.8) |
| D18 | Scope | **Zink.** DXVK ≤ 2.6 as a stretch. DXVK ≥ 2.7 and vkd3d-proton are out of scope, and we will not advertise `bufferDeviceAddress` in v1 — a clean device-creation failure beats silent corruption. (fwd §5) |
| D19 | Reverse channel | Built from day one, not retrofitted. Validation-layer output on the app side is the primary debugging tool. (fwd R14) |
| D20 | Region ownership | The side **inside the VM** creates and publishes both regions; the other side attaches. sharedgl's "server initialises" rule inverted correctly, with the readiness flag sharedgl aliased away. (sgl §7.2b) |

---

## 3. Component topology

```
  APP SIDE (Linux host, no GPU)                    GPU SIDE (Windows VM, iGPU)
  ┌────────────────────────────────┐               ┌────────────────────────────────┐
  │ application                    │               │ sv-gpu-server                  │
  │   ↕ Vulkan loader              │               │  • AWE allocator (owns regions)│
  │ sv-icd (cdylib)                │               │  • cold-path socket server     │
  │  • generated encoder           │               │  • ring consumers (1 thread/   │
  │  • handle ids, arena allocator │               │    ring, spin-then-park)       │
  │  • WSI                         │               │  • generated decoder + subdec  │
  └──────┬─────────────────────────┘               │  • object table (SlotMap)      │
         │ UNIX socket                             │  • SlabImport (host ptr)       │
  ┌──────▼─────────────────────────┐               │   ↕ ash → Intel ICD → iGPU     │
  │ sv-hostd (app side, one per    │               └────────┬───────────────────────┘
  │ machine)                       │                        │
  │  • opens QEMU's memfd          │  ── cold path ─────────┤ virtio-serial → vsock
  │  • GPA → memfd offset          │                        │
  │  • builds the contiguous view  │                        │
  │  • hands fds to ICD processes  │                        │
  └──────┬─────────────────────────┘                        │
         │                                                  │
  ┌──────▼──────────────────────────────────────────────────▼───────────────────┐
  │  CONTROL REGION (16 MiB)      │        PASSTHROUGH ARENA (default 2 GiB)     │
  │  header, ring directory,      │  slabs → suballocations → VkDeviceMemory     │
  │  rings, reply rings, reverse  │  command blobs, present buffers, replies     │
  │  ring, timeline mirrors       │  that overflow a reply ring                  │
  └───────────────────────────────┴──────────────────────────────────────────────┘
              same physical pages; same offsets; different base addresses
```

**`sv-hostd` is a component the brief did not name, and it is forced by the
facts.** Mapping guest RAM means opening QEMU's memfd through
`/proc/<qemu-pid>/fd/<n>`, which an arbitrary application process should not be
able to do and often cannot (fwd §2.2, "GPA → memfd offset"). It also
centralises the GPA→offset translation, which is the fragile part (R21), and
lets several application processes share one attachment. In the same-machine
development loop it degenerates to "create a memfd, `ftruncate`, hand it out",
which is why steps 1–6 of §13 need no VM at all.

---

## 4. Transport: regions, layout, rings, handshake

### 4.1 Two regions, and why the split is by purpose

Per the requirement document: a small **control region** that only our code
touches, and a large **passthrough arena** from which `VkDeviceMemory` is
carved. They differ in lifetime, access pattern and who writes them, and
merging them would mean the arena's allocator could corrupt the rings.

They are two independent mappings with independent bases. Nothing in the
protocol relates an arena offset to a control offset.

### 4.2 Control region layout

Fixed layout, sized at creation, default 16 MiB. All multi-byte fields are
little-endian; the design refuses big-endian peers at handshake rather than
byte-swapping.

**Header — offset 0, one 4 KiB page.**

| Offset | Type | Field | Written by | Meaning |
|---|---|---|---|---|
| 0x00 | u64 | `magic` | owner | `0x5348_5652_4B31_0000`. **Written last, with a release store.** Zero means "not ready". |
| 0x08 | u32 | `layout_version` | owner | Version of *this header*. Bumped only when the table below changes. |
| 0x0C | u32 | `wire_version` | owner | Version of the generated protocol. |
| 0x10 | u64 | `protocol_hash` | owner | Hash of the generated protocol manifest (§6.2). |
| 0x18 | u32 | `generation` | owner | Incremented on every (re)initialisation. |
| 0x1C | u32 | `flags` | both | `READY`, `OWNER_FATAL`, `PEER_FATAL`, `SHUTDOWN`. |
| 0x20 | u64 | `control_size` | owner | |
| 0x28 | u64 | `arena_size` | owner | |
| 0x30 | u64 | `arena_slab_size` | owner | Default 64 MiB. |
| 0x38 | u32 | `ring_slot_count` | owner | Default 64. |
| 0x3C | u32 | `ring_data_bytes` | owner | Default 64 KiB. |
| 0x40 | u64 | `ring_dir_offset` | owner | |
| 0x48 | u64 | `slab_dir_offset` | owner | |
| 0x50 | u64 | `timeline_dir_offset` | owner | |
| 0x58 | u64 | `reverse_ring_offset` | owner | |
| 0x60 | u64 | `owner_heartbeat` | owner | Monotonic tick, updated at least every 100 ms. |
| 0x68 | u64 | `peer_heartbeat` | peer | Same, other direction. |
| 0x70 | u64 | `owner_epoch_ns`, `peer_epoch_ns` | both | Diagnostics only. |

The rest of the page is reserved and must be zero.

**Ring directory** — `ring_slot_count` × 64 B (one cache line each).

| Field | Type | Meaning |
|---|---|---|
| `state` | u32 | `FREE` / `CLAIMED` / `LIVE` / `DEAD` |
| `kind` | u32 | `INSTANCE` / `DEVICE` / `QUEUE` / `REVERSE` |
| `owner_id` | u64 | App-assigned device or queue id |
| `cmd_off`, `cmd_len` | u32,u32 | Command ring data buffer |
| `reply_off`, `reply_len` | u32,u32 | Reply ring data buffer |
| `ctrl_off` | u32 | Ring control block (below) |
| `depends_ring` | u32 | For `QUEUE` rings: the device ring it orders against (§4.4) |

**Ring control block** — one 4 KiB page per ring, deliberately over-sized so
producer and consumer never share a cache line, which is the mistake sharedgl's
single global `SUBMIT` int makes structural (sgl §1.3, §7.2f).

| Line | Contents |
|---|---|
| +0x000 | producer: `cmd_tail: u64`, `next_seqno: u64` |
| +0x040 | consumer: `cmd_head: u64`, `completed_seqno: u64` |
| +0x080 | status: `status: u32` (`IDLE`/`ALIVE`/`FATAL`), `idle_timeout_us: u32`, `notify_count: u64` |
| +0x0C0 | reply producer: `reply_tail: u64` |
| +0x100 | reply consumer: `reply_head: u64` |
| +0x140 | error detail: `fatal_code: u32`, `fatal_seqno: u64`, `fatal_msg: [u8; 160]` |

**Slab directory** — one entry per arena slab: `{state, arena_offset, size,
cache_class, gpu_memory_type, import_state, refcount}`. Written by the app side
when it requests a slab, completed by the GPU side when the import succeeds.

**Timeline mirror table** — 1024 × 64 B, each `{semaphore_id: u64, value: u64,
waiter_count: u32}`. The GPU side stores the value with a release store after
every signal; an app-side `vkGetSemaphoreCounterValue` or `vkWaitForFences` is
then a local acquire load, not a round trip (fwd §2.9).

**Reverse ring** — a single SPSC ring in the GPU→app direction, §4.6.

Sizing with the defaults: 4 KiB header + 4 KiB directory + 64 KiB timelines +
64 × 4 KiB control + 64 × (64 + 64) KiB data ≈ 8.6 MiB, which is why the region
default is 16 MiB.

### 4.3 Handshake

sharedgl has no readiness flag at all, because `READY_HINT` was aliased away by
`SHM_SLOT_MASK` at the same offset (sgl §5, §7.2b). Do not repeat that. The
sequence:

1. The **owner** (the side inside the VM; on `testhost` the GPU side)
   allocates both regions, zeroes them, writes every header field, then stores
   `magic` with `Release`. On re-initialisation it first zeroes `magic`,
   increments `generation`, and only then rewrites the rest.
2. The owner offers the regions over the cold-path socket:
   `Hello { layout_version, wire_version, protocol_hash, generation,
   control_pfns[], arena_pfns[], slab_size, sizes, endianness, pointer_width }`.
   The PFN arrays are run-length encoded, which is cheap because AWE frames sort
   into long runs in practice (fwd §2.2, "Contiguity is a non-problem").
3. `sv-hostd` translates GPA → memfd offset, reserves one `PROT_NONE` range per
   region and `mmap(MAP_FIXED)`s each coalesced run into it. The result is
   virtually contiguous with identical internal offsets — the §1 invariant.
4. The peer acquire-loads `magic`, then checks in this order and refuses with a
   named error on any mismatch: magic, `layout_version`, `wire_version`,
   `protocol_hash`, endianness, `control_size` against the mapping length.
5. The peer replies `Accept { generation }` and claims rings.

**`generation` is the reconnect mechanism.** Every ring operation carries the
generation it was created under; a mismatch means the peer restarted and every
handle, offset and timeline the other side holds is stale. The app side turns
that into `VK_ERROR_DEVICE_LOST` on every outstanding call, which is exactly
what the specification provides for. The guest can reboot underneath us at any
time, which is why this matters more here than it did for sharedgl, whose
server outlives its clients by construction.

**`protocol_hash` is the answer to R13.** Venus's wire format encodes command
length implicitly in the command type, so two builds from different registries
desynchronise silently rather than failing. The hash is computed by the
generator over the full command and struct table it emitted — not over `vk.xml`,
because gfxstream's own source warns that `optional=` semantics move between
header releases, so the generator version matters too (fwd §4.3).

### 4.4 Rings

**Design, taken from Venus's `vn_ring` and gfxstream's ASG, which converged
independently (fwd §1.2, §1.3):** single-producer/single-consumer circular
buffer; `head` and `tail` on separate cache lines with explicit acquire/release
ordering; the consumer spins while work arrives and only sets `IDLE` and parks
after `idle_timeout_us` with an empty ring; the producer checks `IDLE` before
paying for a notification.

**Which rings exist:**

| Ring | Count | Producer | Purpose |
|---|---|---|---|
| Instance | 1 | app | `vkCreateInstance`, physical-device enumeration, everything before a device exists |
| Device | 1 per `VkDevice` | app, under a mutex | Object creation and destruction, descriptor updates, everything that is not a queue operation |
| Queue | 1 per `VkQueue` | app | `vkQueueSubmit`, `vkQueueWaitIdle`, present |
| Reverse | 1 | GPU | Debug messages, device-lost, asynchronous errors |

This is the direct answer to sharedgl's single global `SUBMIT` slot, where
exactly one submission from one client can be in flight system-wide (sgl §1.3).
Per-queue rings mean two queues never serialise against each other, and the
rings stay small — 64 KiB — precisely because the arena carries the data.

**Why the device ring takes a mutex rather than being per-thread.** Vulkan
permits two application threads to create objects on one device concurrently.
gfxstream's answer is a ring per thread plus a global shared seqno word that
every command waits on, which restores a total order and therefore most of the
serialisation it was trying to avoid (fwd §1.3, "Sequence numbers"). A mutex
around a per-device ring producer costs one uncontended atomic in the common
case and does not impose ordering on the *queue* rings, which are the ones that
matter for throughput. If profiling shows object creation is a bottleneck — it
will not be, since Venus's client-assigned ids make creation a fire-and-forget
write — the fix is more device rings with the barrier below, not a global
seqno.

**Cross-ring ordering: the `RingBarrier`.** Because rings are independent, an
object created on the device ring may not yet exist on the GPU side when a queue
ring references it. The app's own happens-before does not survive the split.
Every ring publishes `completed_seqno` into its control block; a packet may
carry an optional `{ring_id, seqno}` barrier, and the consumer waits for that
ring's `completed_seqno` to pass before decoding further. The app-side encoder
emits one whenever a packet references an id created on another ring since the
last barrier — which is Venus's `vkWaitRingSeqnoMESA` applied to our ring
topology (fwd §1.2). In the common case the barrier is already satisfied and
costs one acquire load. Getting the emission rule wrong is R22.

**Replies.** Each ring has a **reply ring in the opposite direction**, not a
fixed mailbox. sharedgl's 4044-byte `RETVAL_V` mailbox is why reading back 4 MiB
costs ~1365 blocking round trips (sgl §3.4) — the single worst artefact in that
design. Our replies stream; a reply larger than the reply ring spills into an
arena scratch block and the reply carries `(offset, len)`. Most commands
generate no reply at all: following Venus, a reply is produced only when the
`GENERATE_REPLY` flag is set (fwd §1.2).

### 4.5 Doorbell and wait, behind a trait

Ship polling. sharedgl proves polling works and demonstrates nothing whatsoever
about interrupts, since it never uses `ivshmem-doorbell` and never issues the
doorbell IOCTLs its own vendored driver implements (sgl §1.5, §7.5). We are not
architecting around an interrupt path nobody has measured.

Two traits, one on each side of a park:

- `Parker::park(ring, deadline) -> Woken | TimedOut | Fatal`
- `Doorbell::ring(ring)`

Implementations, in the order they get written:

1. `SpinYield` — bounded spin with `pause`, then `yield`, then sleep with
   exponential backoff to a 200 µs ceiling. v1 on both sides.
2. `Futex` — Linux-only, works across processes on shared memory. Used by the
   same-machine development loop, where it is strictly better than spinning and
   costs nothing.
3. `SocketDoorbell` — a one-byte write on the cold-path socket. Works across the
   VM boundary today with no new device. Latency is virtio-serial-class
   (millisecond, fwd §6.2) so it is a *wakeup from parked*, never a
   per-submission signal — which is exactly the role the spin-then-park design
   assigns it.
4. `IvshmemMsi` — only if measurement ever justifies the device.

**Timeouts are mandatory on every wait.** R4 is a denial of service in both
directions, and a polling transport makes deadlock easy by accident. A wait that
expires checks the peer heartbeat and the generation counter before deciding
between "slow" and "gone".

### 4.6 The reverse ring

GPU→app, carrying `svDebugMessage`, `svDeviceLost`, `svSlabImportFailed` and
asynchronous driver errors. Neither Venus nor gfxstream has a generated
reverse-call path, and Venus does not support debug-utils messengers in its
protocol at all (fwd §4.3, R14). For us that would mean developing an ICD
without validation-layer output, which is the primary debugging tool for this
class of code. It is roughly 200 lines and it goes in from day one.

The GPU side runs the validation layers *in its own process*, over the real
driver, where they can actually see the calls; their messages are forwarded to
the app side's registered `VkDebugUtilsMessengerEXT` callbacks. The app side
also runs validation locally over our ICD, which catches a different class of
error. Both are useful and they are not the same check.

---

## 5. The passthrough arena

### 5.1 Where it lives, and how the two sides agree on which pages

**Decision: ordinary guest RAM, allocated by the side inside the VM.**

On `testhost` that means the GPU-side process allocates it with Windows AWE:
acquire `SeLockMemoryPrivilege`, `AllocateUserPhysicalPages`, then
`MapUserPhysicalPages` into one reserved contiguous VA range. The returned
`PageArray` holds real physical frame numbers — Microsoft's own released kernel
source shows `NtAllocateUserPhysicalPages` doing a verbatim `RtlCopyMemory` of
the MDL's PFN array into the user buffer, with three independent corroborations
in the same file (fwd §2.2, "Windows AWE returns real guest-physical frame
numbers"). Inside a VM, "physical" is guest-physical, so `PageArray[i] << 12` is
a QEMU GPA, and the whole of guest RAM is already one
`memory-backend-memfd,share=on` that the Linux side can map today (fwd §10a).
The pages are locked for the lifetime of the allocation; the privilege is a
local security-policy grant that does not touch code integrity, Secure Boot or
test-signing, so the hard constraint in `hardware-constraints.md` is untouched.

The user's stated preference and the research's ranking agree, and so do the two
independent lines of reasoning: the pages the two sides can most easily agree on
are also the only kind of pages the import mechanism actually likes (fwd §2.2
verdict). That convergence is the reason to be confident in a decision that
still has an unrun experiment behind it (§14).

**Fallback, if AWE's frame numbers turn out not to be usable** — either because
a modern Windows build transforms them, or because R19's "AWE address ranges
cannot be used to buffer data for graphics or video calls" turns out to be
enforced rather than vestigial — is an **attestation-signed pinning driver**:
`MmProbeAndLockPages` on an ordinary `VirtualAlloc` region, walk
`MmGetMdlPfnArray`, copy the frames out. Roughly 200 lines, and attestation
signing is a genuine Microsoft signature that loads on retail Windows 11 with
Secure Boot and HVCI on — it is not test-signing (fwd §2.2). The cost is an EV
certificate and paperwork, not a technical obstacle, and it dodges every AWE
restriction at once.

**ivshmem is not in the plan.** It has the best driver story and the worst
mechanism story: host-pointer import is implemented as kernel userptr, userptr
pins pages, and BAR mappings are `VM_IO|VM_PFNMAP` which the pinning path
rejects by construction. Looking Glass had to write `kvmfr` —
`devm_memremap_pages()` to manufacture `struct page`s, exported as dma-buf —
precisely because this does not work, and Looking Glass never asks a GPU to DMA
*out* of the BAR anyway, so its track record is not evidence for our case (fwd
§2.2). A BAR would need `HOST_MAPPED_FOREIGN_MEMORY_BIT_EXT`, which essentially
nobody implements. We keep it in the risk table and not in the design.

**Direction-agnosticism.** The rule is "the side inside the VM allocates,
because only it can produce pages the hypervisor can locate". Two traits:
`ArenaProvider` (allocate, pin, enumerate frames) and `ArenaAttacher` (translate
frames to a file offset, build the contiguous view). Today the GPU side provides
and the app side attaches; if the app side were the VM, they swap, and nothing
above §5 changes.

### 5.2 Slabs, cache class, and the allocator

The arena is divided into **slabs** of `arena_slab_size` (default 64 MiB). A
slab is the unit of GPU-side import and the unit of cache-attribute uniformity.
gfxstream uses 16 MiB with 1 MiB rounding for small allocations
(`kDefaultHostMemBlockSize`, fwd §2.1); 64 MiB is the same idea with fewer
driver imports, chosen because `maxMemoryAllocationCount` is a real limit and
each import is a pinning operation.

**Allocator: a segregated free-list over 4096-byte pages, per slab.**

- Granularity is **4096 bytes**, dictated by the measured
  `minImportedHostPointerAlignment` and by the page granularity of the shared
  mapping. Nothing is over-allocated to satisfy the driver
  (`measured-gpu-side.md`).
- Size classes are powers of two from 4 KiB to 4 MiB, each a free list of page
  runs; anything larger is served from a per-slab first-fit list of runs.
- `VUID-VkMemoryAllocateInfo-allocationSize-01745` requires the *size* to be a
  multiple of the alignment too, so allocation sizes round up to 4096 and the
  rounding is reported honestly in `VkMemoryRequirements`.
- Buddy allocation was the alternative and is rejected: it wastes up to 2× on
  the mid-size allocations that dominate (uniform buffers, staging rings), and
  the arena is a fixed pre-negotiated window (R11) where waste converts directly
  into `VK_ERROR_OUT_OF_DEVICE_MEMORY`.
- The allocator lives **entirely on the app side**. It is the side that receives
  the allocation requests, and keeping the free lists in private memory rather
  than in the shared region means the GPU side cannot be corrupted by an
  allocator bug and the allocator can be Miri-tested against a plain `Vec<u8>`
  (fwd §9).
- Slabs are created lazily. When the app side needs a slab it picks a free
  region of the arena, writes a slab-directory entry, and sends
  `svImportArenaSlab`. The GPU side imports it and replies. A failed import is a
  hard error surfaced as `VK_ERROR_OUT_OF_DEVICE_MEMORY` from the triggering
  `vkAllocateMemory`, plus a reverse-ring diagnostic.
- Slabs are **never unimported while the process lives.** Unmapping a slab the
  GPU might still be reading is the one lifetime race with no cheap answer, and
  slab churn buys nothing: the arena is a fixed window anyway.

**Cache class.** Each slab carries one, and in v1 there is exactly one legal
value: **write-back**. See §5.6.

### 5.3 What a `VkDeviceMemory` is, and what `vkMapMemory` returns

An app-side `VkDeviceMemory` backed by the arena is a triple
`(slab_id, offset_in_slab, size)` — a sub-range of a slab, 4096-aligned, whose
`offset_in_slab + size` never exceeds the slab. The GPU side holds one real
`VkDeviceMemory` per slab.

**Every `(memory, offset)` pair crossing the wire is rewritten** by the
generated `transform` pass: the app-side memory id becomes the slab's GPU-side
handle and the offset becomes `slab_offset + app_offset`. This is exactly
gfxstream's `deviceMemoryTransform()`, and it is not optional — it appears in
`vkBindBufferMemory`, `vkBindImageMemory`, the `Bind*MemoryInfo` structs,
`VkMappedMemoryRange`, sparse binding and several `pNext` extenders (fwd §2.1).
A generated pass that walks whole struct graphs is the only tractable way to
catch all of them; hand-listing the sites is how this goes wrong.

**`vkMapMemory` on the app side returns `arena_base + slab_base + alloc_offset +
offset`.** A real pointer into real shared pages. It involves **no round trip
and no copy**: the arena is mapped for the process's whole lifetime, so
`vkMapMemory` and `vkUnmapMemory` are pure app-side bookkeeping. That is the
requirement, stated as an implementation consequence.

**On the GPU side nothing calls `vkMapMemory` on arena-backed memory at all.**
The specification does not require the driver to return the pointer we imported
— ANV happens to, and the extension's own issue resolutions say implementations
are allowed to but not required to (fwd §2.1). We never need it: when the GPU
side wants CPU access to arena bytes (decoding a command blob, sourcing a
present blit) it reads through *its own* mapping of the arena, at the offset the
wire carried. This also sidesteps the caveat that imported memory is not
"host mapped device memory" unless explicitly mapped, so
`vkFlushMappedMemoryRanges` would not necessarily cover our own pointer.

Non-arena memory (heap 0, §9.1) is never host-visible, so `vkMapMemory` on it is
impossible by construction rather than by check.

### 5.4 Import, behind a trait

```
trait SlabImport {
    fn import(&self, device, arena_offset, size, cache_class) -> Result<VkDeviceMemory>;
    fn memory_type_for(&self, arena_offset, size) -> u32;
}
```

Implementations:

| Impl | Mechanism | When |
|---|---|---|
| `HostPointerImport` | `VkImportMemoryHostPointerInfoEXT` with `HOST_ALLOCATION_BIT_EXT` | Windows GPU side (measured available), Intel/NVIDIA Linux |
| `DmaBufImport` | `VK_KHR_external_memory_fd` + `VK_EXT_external_memory_dma_buf` | Linux GPU side, mandatory on AMD |
| `NoopImport` | lavapipe adopts the pointer | Development only |

**Pluggability is not speculative refinement.** RADV rejects *every* shared
mapping through this path — libdrm passes `AMDGPU_GEM_USERPTR_ANONONLY`
unconditionally and Linux gives `MAP_SHARED|MAP_ANONYMOUS` a backing file via
`shmem_zero_setup()` — so a shared arena is unimportable on AMD/Linux by
construction, surfacing as `VK_ERROR_INVALID_EXTERNAL_HANDLE` (fwd §2.1). The
brief requires either side to be either OS, so the trait exists from the first
commit even though `testhost` only ever exercises the first row.

**Copy gfxstream's defensive memory-type rewrite.** Call
`vkGetMemoryHostPointerPropertiesEXT` first; if the preferred type index is not
in the returned `memoryTypeBits`, pick the lowest set bit instead. A
`memoryTypeBits` mask that excludes your choice is a normal condition to handle,
not a failure (fwd §2.1).

**`NoopImport` is a great harness and a dangerous oracle.** lavapipe's
properties query returns `memoryTypeBits = 1` unconditionally and the allocation
simply adopts the pointer; it will cheerfully import a pointer every real driver
rejects. Use it to develop the plumbing. Never treat a lavapipe pass as evidence
the memory mechanism works (fwd §2.1).

### 5.5 Lifetime

This is a correctness question, not a security one (`security-model.md`, fwd
§2.5). Three mechanisms, all from the prior art:

1. **Deferred free.** `vkFreeMemory` does not return the range to the free list.
   It moves it to a retire list tagged with the current value of every timeline
   the allocation could be referenced by; the allocator reclaims it only once
   the GPU side's mirrored timeline values have passed. This is gfxstream's
   `removeDelayed` applied to memory rather than handles.
2. **Deferred handle destruction.** The GPU-side object table keeps a destroyed
   object alive until in-flight command buffers referencing it have retired.
   Not derivable from `vk.xml`; budgeted as hand-written (fwd §4.3).
3. **A read-pending word per command blob.** An arena block holding a recorded
   command stream carries an 8-byte handshake word the GPU side flips from
   `READ_PENDING` to `READ_COMPLETE`, so the app side never recycles a blob the
   decoder is still walking. gfxstream's `CommandBufferStagingStream` idiom,
   cheap and effective (fwd §2.5).

**Against a crashed peer.** The heartbeat words plus the generation counter.
GPU side sees the app-side heartbeat stall: tear down that context, destroy its
objects, release its slabs and arena regions. App side sees the GPU-side
heartbeat stall or the generation change: every outstanding call returns
`VK_ERROR_DEVICE_LOST`, which is the spec-sanctioned way to say the GPU went
away and which applications are required to handle.

**Bounds checks.** Every offset read from the wire is compared against the
region length before use — one comparison, and the realistic failure it catches
is a generator bug producing a wrong `len=` (R2, R8). The distinction that
matters, and which the decoder encodes as a convention rather than rediscovering
per call site: a value the GPU side reads *out* of the arena into a local
variable is snapshotted by the act of reading, so checking it is an ordinary
check and worth doing; a value the GPU reads *later, by DMA* cannot be
meaningfully checked at all, and pretending otherwise would be theatre (fwd
§2.5).

### 5.6 Coherency and cache attributes

**Decision: write-back on both sides, uniformly, in v1.**

With a genuine shared mapping of ordinary RAM on x86 there is nothing to do:
both sides map the same physical pages, hardware maintains coherency, and
ordinary acquire/release ordering suffices. This is why Venus's vtest
`bo_flush`/`bo_invalidate` are literal no-ops and why gfxstream has no
app-side `on_vkFlushMappedMemoryRanges` at all (fwd §2.3).

The interesting question was whether to follow gfxstream in deriving a
per-allocation cache attribute from the memory type — WC for upload, WB for
readback. **We do not, in v1, for a concrete reason:** on the AWE path the
GPU-side window is write-back and `MapUserPhysicalPages` takes no cache
attribute, while the app side's view is an ordinary `mmap` of a memfd, also
write-back. There is no supported way to make either side write-combining
without adding a driver, and mapping the same physical page with two different
attributes from two page tables is architecturally undefined on x86 (R10). So
the honest v1 is: one cache class, WB, identical on both sides, and R10 is
retired by construction rather than managed.

The `cache_class` field stays in the slab descriptor with one legal value. What
would change this: a measurement showing WB upload bandwidth is the bottleneck
*and* a transport that can give both sides a matching WC mapping. Note that this
is the exact inverse of sharedgl's mistake — it maps everything
`WRITECOMBINED`, which is right for a pure producer and wrong for a consumer,
and our inversion swaps which machine is which (sgl §7.2e).

`vkFlushMappedMemoryRanges` and `vkInvalidateMappedMemoryRanges` are implemented
as real forwarded commands with a real range rewrite, because a future
non-coherent GPU side needs them and because implementing them as no-ops now
would hide the fact that we never wrote them. On the arena's coherent types they
resolve to nothing on the GPU side, which is honest: the memory genuinely is
coherent. We are not lying to the application, which is the point of not
advertising a non-coherent-only table — and could not be, since the
specification requires at least one `HOST_VISIBLE | HOST_COHERENT` type to
exist, so the tempting "force the app to call flush so we can intercept" design
is not legal in the first place (fwd §2.0).

---

## 6. The protocol

### 6.1 What we take

**Venus's `VK_EXT_command_serialization` encoding rules** (fwd §1.2), because it
is a 134-line specification, direction-neutral, MIT, already proven by two
implementations, and missing only the things that genuinely cannot be
serialised. Specifically we take:

- Command stream: `VkCommandTypeEXT`, `VkCommandFlagsEXT`, then parameters.
  Reply generated only when `GENERATE_REPLY` is set.
- Integers little-endian padded to 32 bits; floats reinterpreted; enums as
  `i32`; `size_t` as `u64`.
- Handles as `u64` object ids; `VK_NULL_HANDLE` is 0.
- Non-void pointers as `u64` count followed by values, padding after the array
  rather than per element.
- Sized `void*` as a blob; `char*` as a blob including the NUL.
- Unions as a `u32` member index then the member.
- **Optional pointers as a one-word boolean**, not gfxstream's 8-byte
  big-endian pointer token. It is smaller and does not put addresses on the
  wire (fwd §4.3).
- **Venus's three-state output validity** (`VALID`/`INVALID`/`PARTIAL`) and its
  `_partial` encode/decode variants, which is the better model for the
  count-then-fill idiom than inferring "output" from non-const pointers.
- **Venus's clamping of array replies against the caller's buffer.** gfxstream
  does not clamp; it overwrites `*pCount` from the reply and then reads. Under
  our security model this is not about a hostile peer — it is that an unclamped
  write is a buffer overflow in our own code the moment either side has a bug,
  and the clamp is one comparison (fwd §4.3).
- The exclusion list, verbatim in spirit: function pointers, pointers to mapped
  memory regions, WSI handles and external handles are **not serialisable**.
  Mapped memory being explicitly out of scope of the wire format is the same
  conclusion the requirement document reaches from the other direction.

**gfxstream's pNext encoding**: each link length-prefixed with a `0` terminator,
so a reader can skip a link it does not know without consulting a type table.
Venus's alternative — a per-parent switch with no length prefix — is smaller but
turns any version skew into a desynchronised stream. Asymmetric handling, also
gfxstream's: the app side silently drops unknown links and continues down
`pNext`; the GPU side treats one as fatal. gfxstream's reason was
client-lenient/server-strict; ours is that a fatal abort turns a silent protocol
mismatch into a loud one (fwd §1.3, §2.5).

**gfxstream's boxed-handle table on the GPU side** (§6.4) and **its pass
taxonomy** (§7.2).

### 6.2 Where we deviate, and why

1. **A `u32` byte length in every command header.** Venus's command length is
   implicit in the type, and the spec says plainly that an unrecognised command
   cannot be skipped. That is R13 and it is the worst failure mode this project
   has: two builds do not fail to connect, they desynchronise and misinterpret
   everything downstream. Four bytes per command buys a decoder that can detect
   the desynchronisation at the very first mismatched command and abort with the
   offending seqno in the ring's `fatal_msg`. Since commands are batched into
   arena blobs and the ring carries descriptors, the overhead is not on any hot
   path. *What would change my mind:* a measurement showing the length field
   costs meaningfully in the sub-decoder over recorded `vkCmd*` streams — in
   which case the sub-decoder alone drops it, since a blob is verified as a unit.

2. **A `protocol_hash` in the handshake** (§4.3), because pinning `vk.xml` is
   necessary but not sufficient — the generator version matters too.

3. **A length-prefixed, self-describing reply stream** rather than a fixed
   mailbox, so large `vkEnumerate*` and `vkGet*` replies do not become sharedgl's
   3072-byte block loop (sgl §3.4).

Everything else follows Venus. Deviating further would mean losing the ability
to differentially test our encoder against Venus's MIT-licensed generated C on
the shared subset, which is the strongest correctness evidence available and far
better than the symmetric self-consistency R8 correctly says proves nothing
(fwd §9).

### 6.3 Our non-Vulkan commands

Venus needed 18 protocol-level commands beyond Vulkan; gfxstream invented 27.
Ours, kept deliberately short:

| Command | Purpose |
|---|---|
| `svCreateRing`, `svDestroyRing`, `svNotifyRing` | Ring lifecycle and wakeup |
| `svRingBarrier` | Cross-ring ordering (§4.4) |
| `svImportArenaSlab`, `svReleaseArenaSlab` | Slab import, with the memory-type reply |
| `svExecuteCommandBlob` | Hand a recorded `vkCmd*` blob at `(offset,len)` to the sub-decoder |
| `svQueueSubmitAsync` | Submit with no reply; timeline values are the completion signal |
| `svBindTimelineMirror` | Bind a semaphore to a slot in the timeline mirror table |
| `svPresentBlit` | GPU-side copy of a presentable image into a linear arena buffer |
| `svDebugMessage`, `svDeviceLost` | Reverse ring only |
| `svPing`, `svShutdown` | Liveness and orderly teardown |

Thirteen. They are hand-written, not generated, and they are the only place the
protocol is ours rather than Vulkan's.

### 6.4 Handles

**App-side-assigned object ids on the wire; a rich table on the GPU side.**

The app side allocates ids from a per-device 64-bit counter, so
`vkCreateBuffer` encodes the id it chose and returns immediately — **object
creation costs no round trip**, which is the single largest latency win
available and is Venus's design (fwd §4.3). The GPU side maintains a
`SlotMap<Key, HandleInfo>` where `HandleInfo` holds the driver handle, the type
tag, the dispatch pointer, ordering state and deferred-destroy state — which is
gfxstream's boxed-handle manager, and in Rust most of it comes free. The two
designs are complementary: the id is what crosses, the table is what holds the
state.

Handles crossing the boundary are therefore **always table indices, never
pointers** (R3). This is required for correctness anyway, since the two sides do
not share an address space, so it is free.

**Dispatchable handles on the app side** are a separate problem, and it is where
Rust's natural idioms are all wrong. Every dispatchable object we return must be
a pointer to a `#[repr(C)]` struct whose first pointer-sized field is
`ICD_LOADER_MAGIC` — and the loader *overwrites* that word with its own dispatch
table pointer, so those 8 bytes are permanently lost. Never `Arc<T>` (its
allocation starts with the refcounts), never `#[repr(Rust)]`, never
`Box<dyn Trait>`. Kazan's `struct DispatchableType<T> { loader_dispatch_ptr:
usize, value: T }` pattern, copied exactly. The loader's assertion is debug-only,
so getting it wrong is a segfault inside the loader rather than an error (fwd
§3.3, R17).

### 6.5 Command buffers

`vkCmd*` calls are **not** sent individually. Each `VkCommandBuffer` owns an
arena block; recording encodes straight into it; `vkEndCommandBuffer` publishes
`(offset, len)`; `vkQueueSubmit` sends `svExecuteCommandBlob` descriptors. The
GPU side decodes the blob with a **separate restricted sub-decoder** that
accepts only the `vkCmd*` opcode subset — gfxstream's `vk_sub_decoder.cpp` and
Venus's `vkExecuteCommandStreamsMESA` (fwd §1.3, §7.2).

With the arena this is genuinely zero-copy from record to execute: the bytes the
application's `vkCmdDraw` produced are the bytes the decoder reads. Blob
lifetime is the read-pending word of §5.5 plus retirement by timeline value.

`vkResetCommandPool` / `vkResetCommandBuffer` return blobs to the arena
allocator through the same deferred path.

### 6.6 Licences, stated once

| Source | Licence | What we take |
|---|---|---|
| Venus (`VK_EXT_command_serialization`, `vn_ring`) | MIT | Wire format *rules* and ring design, reimplemented in Rust. MIT would permit more; we still credit it in `THIRD-PARTY.md`. |
| gfxstream | Apache-2.0 (output), MIT (generators) | **Design only**: pass taxonomy, boxed handles, length-prefixed pNext, staging-stream idiom. No code. |
| Khronos `vk.xml`, `vk_icd.h` | Apache-2.0 OR MIT | Re-expressed in Rust freely. |
| rutabaga_gfx / kumquat | BSD-3-Clause | Read as an architectural template. Not vendored. |
| Looking Glass | **GPL-2.0** | Architecture studied from documentation. **No code, ever**, and the present path is written without reference to its source. |
| Kazan | **LGPL-2.1+** | The dispatchable-handle pattern is a three-line idiom dictated by `vk_icd.h`, not a copyrightable expression. Nothing else is taken. |

The workspace is `MIT OR Apache-2.0`. Vendoring Apache-2.0 code would collapse
that to Apache-2.0-only for the affected crate and lose GPLv2 compatibility, so
if gfxstream code is ever vendored it goes in its own crate with its own licence
declaration. The differential-test harness that compiles Venus's generated C
lives in a test-only crate, clearly marked.

---

## 7. Codegen

The generator is the first real component of the project, not a convenience
added later. 747 commands and 1701 structs (`api-scale.md`) is five figures of
mechanical Rust that would be wrong in a hundred places if typed by hand, and
stale the day the registry moves.

### 7.1 The model

**Decision: pure Rust.** `vk-registry` grows from its current wrapper into a
typed, fully-resolved model: `Command`, `Param`, `Struct`, `Member`, `Handle`,
`Enum`, `Bitmask`, `FuncPointer`, `Extension`, `Feature`, with reverse links,
resolved aliases, resolved `<require>` provenance, and — the part `vk-parse` does
not give us — a real C declarator parse of the `code` strings, yielding
indirection depth, constness per level, array extents, and the `len`/`altlen`
pairing as an explicit map.

`vk-parse` is the right parser (actively maintained, MIT/Apache, complete
attribute coverage, recoverable errors) and its one significant gap is exactly
this: `src/c.rs` is a char tokenizer, not a declaration parser, and every
downstream project re-solves it (fwd §4.2). We solve it once, in one place,
around 400–600 lines with `nom`, modelled on `vulkanite`'s two-layer
`Type`/`AdvancedType` split — `AdvancedType::HandleArray(ty, len_param)` is
precisely the distinction a remoting layer needs (fwd §4.2).

The rejected alternative was running Khronos's `vulkan_object.py` /
`base_generator.py` once and serialising to JSON, which the research leaned
towards as less work. It is less work, and it is rejected because it makes the
generated protocol a function of a checked-in artefact that can silently drift
from `$VK_XML`, which is the exact failure `api-scale.md` pins the registry to
prevent — and because a Python step in the middle of a build the dev shell
otherwise keeps to one toolchain is a maintenance cost forever, to save a week
once. *What would change my mind:* if the declarator parser exceeds ~800 lines,
or if the exception list for things the registry cannot express passes ~20
entries, the balance flips and we adopt the JSON model. Either way we run
`vulkan_object.py` **once, offline**, and check its output in as a test fixture
that our model is compared against — a genuinely independent check on the part
of this project most likely to be quietly wrong.

Registry sourcing is unchanged: `$VK_XML` from the pinned `vulkan-headers`, no
fallback to a system path, exactly as `vk-registry` already enforces.

### 7.2 The passes

Adapted from gfxstream's taxonomy, which is the most reusable artefact in the
research because each pass is a full walk over every struct and each exists for
a reason we would otherwise discover painfully (fwd §1.3).

| # | Pass | Side | What it emits |
|---|---|---|---|
| 1 | `size` | app | Exact serialised byte count, so a contiguous span can be reserved before writing |
| 2 | `encode` | app | Writes into a reserved span with a bump cursor — straight into the ring or the arena blob, no intermediate buffer |
| 3 | `decode` | GPU | Field-by-field reads through a bounds-checked cursor into owned storage |
| 4 | `arena_bind` | GPU | Ties decoded pointer-shaped fields to bump-allocated storage with the packet's lifetime (gfxstream's `deepcopy`, minus the C++ pool) |
| 5 | `handle_map` | GPU | Walks a struct graph replacing ids with driver handles |
| 6 | `transform` | GPU | Semantic, not syntactic: the `(memory, offset)` rewrite of §5.3, memory-type-index remapping, format/limit fixups |
| 7 | `pnext` | both | Chain walk with length-prefixed links |
| 8 | `dispatch` | GPU | The opcode table. `ash` supplies driver dispatch tables for free |
| 9 | `subdecode` | GPU | The restricted `vkCmd*` decoder of §6.5 |
| 10 | `icd_entry` | app | `extern "system"` stubs and the `vkGetInstanceProcAddr`/`vkGetDeviceProcAddr` tables |
| 11 | `roundtrip_tests` | both | Generated property tests: encode → decode → compare, including pNext chains |

Not emitted, deliberately: gfxstream's `decodersnapshot` (7,063 lines of VM
snapshot support we will never use) and `api_log_decoder`. `marshaling`,
gfxstream's older safe path kept in parallel with `reservedmarshaling`, is also
skipped — one path, and it is the fast one, because a second path is a second
place to be subtly wrong.

The `(size, encode)` split is not collapsible. Both mature projects have a
separate size pass and both need it for the same reason: you cannot reserve a
ring span before you know how big the message is (fwd §1.3, §4.4).

### 7.3 Safe by construction

The generated marshalling is the largest body of code in the project and the one
where a mistake is silent corruption rather than a crash. R7 is where the
security stance stays strict, and it is achievable here.

- **The generated protocol crate is `#![forbid(unsafe_code)]`.** All of it:
  encode, decode, pnext, transform, handle map, sub-decoder.
- **Never transmute a struct out of shared memory.** The peer can mutate it, so
  a `&VkFoo` over shared bytes is an immediate aliasing-model violation, and
  padding makes it unsound regardless. Generate field-by-field reads from a
  cursor. Both Venus and gfxstream do this in C; it is the same shape in safe
  Rust (fwd §9).
- **A bounds-checked cursor**, `read_u32() -> Result<u32>`, so every overrun is
  a returned error rather than undefined behaviour. One comparison per field,
  which is noise next to the work being done.
- **Count-then-loop, never slice-from-peer-length.** Where the registry says
  `len="bindingCount"`, the generated code reads the count, checks it against
  the remaining buffer, and loops.
- **Clamp every array reply** against the caller's buffer (Venus's rule, §6.1).
- The unavoidable `unsafe` is the ICD boundary itself — `extern "system"`
  functions taking raw pointers from the application. Those live in a separate
  generated crate, `sv-icd-entry`, and every stub has the identical mechanical
  shape "null-check, dereference, call a safe inner function", so the unsafe
  block is one line and reviewable by pattern.

`unsafe` in hand-written code is confined to three modules with written
invariants: the mapping, the ring, and the arena allocator (fwd §9).

**Miri covers more than it looks like it should**: the allocator fully, backed
by a `Vec<u8>`; the ring as two threads over a `Vec<u8>`, with
`-Zmiri-many-seeds` for the interleavings; and every generated round-trip. What
it cannot model is the cross-process race, which is outside the language by
construction — we model the shared region as raw bytes with no Rust-level
aliasing claims and say so.

### 7.4 The exceptions, budgeted rather than discovered

`vk.xml` cannot express everything, and every Vulkan serialiser eventually needs
an escape hatch. Budget for it now (fwd §1.3, §4.3):

- **Unions**: 16 exist, only 10 `selector=` annotations exist in the entire
  registry. Venus hardcodes three and refuses to serialise structs containing
  any other; gfxstream always marshals member `[0]`. We hardcode the
  discriminators we need and make an unhandled union a **generator-time error**,
  not a runtime one.
- **Non-`pNext` `void*`**: 103 of them. Those with a `len` are blobs. The rest
  (`window`, `handle`, `hostAddress`, `pUserData`, `pCheckpointMarker`) get a
  per-field policy entry. `pHostPointer` is the interesting one — it is
  `VK_EXT_external_memory_host`, and in our design it means an arena offset.
- **Conditional presence**: `pTessellationState` in
  `VkGraphicsPipelineCreateInfo` depends on `rasterizerDiscardEnable` in a
  sibling struct. gfxstream invented a small Lisp for this. We use a table of
  Rust closures keyed by `(struct, field)` — same expressiveness, no interpreter.
- **`len=` vs `altlen=`**: read `altlen`; there are only 5 latexmath cases in
  the registry and `altlen` exists to give the plain-C equivalent. Do not build
  a LaTeX parser. gfxstream hardcodes these by `(struct, field)`, which is worse.
- **`VkAllocationCallbacks`**: not serialisable. We strip `pAllocator` above the
  protocol, as Mesa's Venus driver does. Host allocators are not supported and
  that is legal.
- **`VkAccelerationStructureInstanceKHR`**: bitfields the registry cannot
  describe. Out of scope with ray tracing.

Every exception is a data entry in one file, and an unlisted case is a build
failure. That is the difference between a generator that is honest about its
limits and one that emits plausible-looking wrong code.

### 7.5 Size estimate

For the **Zink-scoped subset** (~450 commands, ~700 structs — roughly Venus's
curated coverage, fwd §4.4):

| Generated | Lines |
|---|---:|
| Types, enums, constants | 12,000 |
| `size` + `encode` (app) | 26,000 |
| `decode` + `arena_bind` (GPU) | 28,000 |
| `pnext` walkers | 8,000 |
| `handle_map` + `transform` | 6,000 |
| `dispatch` + `subdecode` | 8,000 |
| ICD entry stubs + proc tables | 10,000 |
| Generated round-trip tests | 6,000 |
| **Total generated** | **~104,000** |

Full-registry coverage would be roughly 2.5× that, consistent with Venus's
~130k for ~52% and gfxstream's ~233k for full coverage (fwd §4.1). We do not go
there; §11 explains why the subset is not a compromise but the scope.

| Hand-written | Lines |
|---|---:|
| `sv-codegen` (model consumers, 11 passes, exception table) | 4,500 |
| `vk-registry` typed model + C declarator parser | 1,600 |
| `sv-wire` (cursor, headers, primitives) | 600 |
| `sv-transport` (regions, rings, handshake, parkers, backends) | 1,800 |
| `sv-arena` (allocator, slabs, deferred free) | 1,000 |
| `sv-handles` (id allocation, object table) | 600 |
| `sv-icd` hand-written half (instance/device/queue objects, WSI, memory) | 1,800 |
| `sv-gpu-server` (ring consumers, semantic glue, import, submit path) | 4,500 |
| `sv-present` | 1,000 |
| `sv-hostd` (GPA translation, view construction, fd handoff) | 800 |
| Tools, probes, integration tests | 2,000 |
| **Total hand-written** | **~20,000** |

Call it **15–22k hand-written**. That is below gfxstream's ~30k of hand-written
semantic glue (fwd §1.3) because we skip snapshots, Android, GLES, the composer,
and the host-kernel fence machinery — and above sharedgl's per-component sizes
because we are doing something considerably harder. The number to watch is
`sv-gpu-server`: that is where "the real cost of the project" lives, and it is
the line item most likely to double.

---

## 8. The ICD

**An ICD, not a layer.** A layer is never the terminator of a call chain, zero
drivers makes `vkCreateInstance` fail outright with
`VK_ERROR_INCOMPATIBLE_DRIVER`, and physical-device enumeration is aggregated by
the loader from ICD-returned handles a layer cannot mint. On a machine with no
GPU there is nothing beneath a layer (fwd §3.1).

**Linux.** A `cdylib`, `libsv_icd.so`, plus a manifest:

```json
{ "file_format_version": "1.0.1",
  "ICD": { "library_path": "...", "api_version": "1.3.0",
           "library_arch": "64", "is_portability_driver": false } }
```

installed to `/usr/share/vulkan/icd.d/` by the nix package; the development loop
uses `VK_DRIVER_FILES` (current; `VK_ICD_FILENAMES` is deprecated and ignored
when both are set) pointing at a manifest the build script generates into
`OUT_DIR` with an absolute `library_path` — Kazan's clean pattern.

Two traps taken seriously:

- **The `api_version` demotion.** If the JSON says < 1.1, or
  `vkEnumerateInstanceVersion` is missing, or returns < 1.1, or returns anything
  but `VK_SUCCESS`, the loader demotes us to a 1.0 driver *and rewrites every
  `VkApplicationInfo::apiVersion` we receive to 1.0*. Ship a real `api_version`
  and implement `vkEnumerateInstanceVersion` (fwd §3.2).
- **`is_portability_driver: false`.** Setting it makes us invisible to every
  application that has not set
  `VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR`.

Exports: exactly `vk_icdGetInstanceProcAddr`,
`vk_icdNegotiateLoaderICDInterfaceVersion` and `vk_icdGetPhysicalDeviceProcAddr`,
as `#[no_mangle] pub unsafe extern "system"`, with hidden visibility, a version
script, and `-C link-arg=-Bsymbolic`. `vk_icdGetInstanceProcAddr` must handle a
NULL instance for global lookups. Compile-time assertions against the `PFN_*`
typedefs catch ABI drift, as Kazan does (fwd §3.2, §3.6).

**We do not export `vk_icdEnumerateAdapterPhysicalDevices`.** We have no PnP
adapter LUID, and its only benefit — the loader sorting physical devices for
drivers at interface ≥ 6 — is meaningless with one virtual device.

**Interface version: negotiate 7, degrade to 5.** 7 relaxes the export
requirements; 4 is where `vk_icdGetPhysicalDeviceProcAddr` becomes required for
unknown physical-device entry points; at ≤ 4 the *driver* must validate
`apiVersion` itself, which is the reason to be prepared for 5 rather than
assume 7.

**Windows.** `HKLM\SOFTWARE\Khronos\Vulkan\Drivers`, a `DWORD` named with the
full path to the manifest, value `0`. Plus the `WOW6432Node` mirror for 32-bit
applications. This is not a hack: the loader documentation names it "the primary
location for software based drivers", it needs admin rights but no signing, no
PnP device and no WHQL, and SwiftShader registers exactly this way (fwd §3.5).
Note this path only matters when the *app side* is Windows — not on
`testhost`, but the brief requires either side to be either OS.

**WSI.** `VK_KHR_surface` is loader-owned; `VK_KHR_swapchain` is always
driver-owned and we must implement it, along with
`vkGetPhysicalDeviceSurface{Support,Capabilities,Formats,PresentModes}KHR` and
the `…PresentationSupportKHR` family. We opt *out* of interface-3 surface
creation by not exposing `vkCreateXXXSurfaceKHR`, so the loader hands us a
`VkIcdSurfaceXXX` from `vk_icd.h` which we cast. **`VK_EXT_headless_surface` is
the first target** (fwd §3.4, §10). A silent-failure trap to remember:
`terminator_CreateSwapchainKHR` with a NULL driver pointer returns `VK_SUCCESS`
with an uninitialised swapchain.

**Extension filtering, not mirroring.** The loader has
`MAX_NUM_UNKNOWN_EXTS = 250` hand-generated trampolines, and its known-extension
list depends on the loader's own header version. We advertise the curated set of
§11 rather than reflecting all 148 device extensions the GPU side reports (R16).

---

## 9. What we advertise

### 9.1 Memory

Two heaps, four types, in this order — the order matters because applications
iterate and take the first match, and the specification only constrains ordering
where one type's flags are a strict subset of another's (fwd §2.4):

| Idx | Heap | Flags | Backing | Intended use |
|---|---|---|---|---|
| 0 | 0 | `DEVICE_LOCAL` | GPU-side allocation, never mapped | Render targets, optimal-tiling images, anything the app does not map |
| 1 | 1 | `HOST_VISIBLE \| HOST_COHERENT` | Arena | Staging and upload |
| 2 | 1 | `HOST_VISIBLE \| HOST_COHERENT \| HOST_CACHED` | Arena | Readback |
| 3 | 1 | `DEVICE_LOCAL \| HOST_VISIBLE \| HOST_COHERENT` | Arena | The "ReBAR" type modern engines look for |

The subset ordering rule is satisfied: 1 ⊂ 2 and 1 ⊂ 3 and 0 ⊂ 3, and each
precedes its superset. 0 and 1 are unordered relative to each other, which is
the freedom we use to put plain `DEVICE_LOCAL` first so that render targets do
not land in the arena.

- **Heap 1's size is the real arena size.** Applications budget against it and
  use it for `VK_EXT_memory_budget`; lying produces allocation failures at bad
  moments (R11).
- **Heap 0's size is the GPU side's real heap size minus the arena**, since on
  this hardware they are physically the same 6624 MiB.
- Type 3 is honest on this machine and not a marketing claim: the measured
  device has one heap and every type but one is simultaneously `DEVICE_LOCAL`
  and `HOST_VISIBLE` (`measured-gpu-side.md`). On a discrete GPU side we would
  drop type 3 and let type 1 do the work.
- The awkward case a forwarder normally has to handle — device-local memory the
  application wants mapped — **does not arise on this hardware**, which is the
  single biggest simplification the measurement bought.

### 9.2 Features and extensions

Curated, not mirrored. The v1 set is Zink's requirement set (§11), which is
`VK_KHR_maintenance1`, `maintenance5`, `create_renderpass2`, `dynamic_rendering`,
`descriptor_update_template`, `timeline_semaphore`, plus the
`VP_ZINK_gl21_baseline` profile's additions and the ladder to 4.6.

**We do not advertise `bufferDeviceAddress` in v1.** Not because the driver
lacks it — the measurement confirms it is present — but because an application
that writes a raw 64-bit address into arena memory for a shader to dereference
has done something our transform pass cannot see. A device that does not
advertise the feature causes DXVK 3.x and vkd3d-proton to fail device creation
cleanly and loudly. That is a much better outcome than a device that advertises
it and then renders garbage. See §11 for the nuance.

We also do not advertise external memory/semaphore/fence handle types we cannot
honour, which is legal and avoids gfxstream's rather comical OPAQUE_FD cookie
workaround (fwd §2.9).

### 9.3 Synchronisation

**Everything is a monotonic 64-bit timeline.** Binary semaphores and fences are
expressible on top of timelines; the reverse is not (fwd §2.9).
`VK_KHR_timeline_semaphore` is measured present on the GPU side.

- **Semaphores used only between GPU-side queues never cross the boundary.**
  Encode the submit and let the real driver handle it. That is the overwhelming
  majority of synchronisation in a real application, and it costs us nothing.
- **Fences and host waits do cross**, and are answered from the timeline mirror
  table in the control region: `vkGetFenceStatus`,
  `vkGetSemaphoreCounterValue` and the fast path of `vkWaitForFences` are local
  acquire loads. A blocking wait parks with a timeout and rechecks. A polling
  `vkGetSemaphoreCounterValue` loop — the pathological case, and common — never
  touches the transport.
- **`vkQueueSubmit` is asynchronous.** Encode and return. If submit is
  synchronous the design is dead on latency grounds regardless of transport.
  `vkQueueWaitIdle` and `vkDeviceWaitIdle` become timeline waits.

---

## 10. Presentation

**Outside the protocol**, as gfxstream does it — it never forwards
`VK_KHR_swapchain` at all, and presentation is deliberately not serialised
(fwd §2.8).

This is the one place our topology is strictly worse than the conventional one:
the GPU is in the VM and the screen is outside, so a finished frame must cross
every frame, forever. 1080p BGRA is 8.3 MB/frame, ~500 MB/s at 60 Hz. And the
app side has **no GPU**, so whatever it does with those pixels runs on llvmpipe.

**The decision is staged, because the honest answer may be that this problem
should not be solved at all:**

1. **v1: `VK_EXT_headless_surface` and no swapchain.** Zink under EGL/surfaceless
   and the entire deqp suite need no presentation. This unblocks everything in
   §13 steps 1–6 and costs nothing.
2. **v2: a real swapchain.** App-side `VK_KHR_swapchain` whose images are
   GPU-side `VkImage`s. On `vkQueuePresentKHR` the app side sends `svPresentBlit`;
   the GPU side runs `vkCmdCopyImageToBuffer` into a **triple-buffered linear
   arena buffer** and signals a present timeline value. The app-side WSI then
   blits from the arena to the window with `wl_shm` or `XShm` — a straight
   memcpy from shared memory into a window buffer, with no GPU involvement and
   no second copy of our own. Format is negotiated at swapchain creation, not
   hardcoded to BGRA the way sharedgl hardcodes `GL_BGRA` in two places
   (sgl §3.6).
3. **Only then, if measurement justifies it:** damage rectangles, and hardware
   video encode via QuickSync for a genuinely remote app side.

Three improvements over sharedgl's present path, all of which its own study
identifies (sgl §3.6, §7.4): triple buffering rather than one global frame slot
guarded by a cross-process spinlock; the vertical flip done on the GPU rather
than in a CPU pixel loop; and the framebuffer stride being the actual width
rather than the configured maximum.

**And the thing to measure before building any of it:** on this machine the
physical panel is driven by the passed-through iGPU. It may well be that the
right answer is to leave it there and treat the Linux side as a headless
compute/render node, at which point most of stage 2 evaporates (fwd §6.6). That
measurement is cheap and it is scheduled before the work, not after.

---

## 11. Scope, plainly

**In scope: Zink.** Mesa's `docs/features.txt` has zink at GL 4.6 / GLES 3.2,
it is a Khronos-conformant OpenGL 4.6 implementation, and — the single best
piece of evidence in the research — **Mesa's own CI runs zink on top of Venus on
lavapipe** (`zink-venus-lavapipe`), which is GL-on-Vulkan over a forwarded
Vulkan across a virtio-gpu boundary on a CPU backend, gated upstream (fwd §5.1).
Our configuration has precedent and a reference to compare against.

Zink's hard requirements are five extensions plus timeline semaphores, and the
authoritative full set is the machine-readable `VP_ZINK_requirements.json`
profile — which contradicts zink's own prose and should be believed over it.
Crucially for us: `bufferDeviceAddress`, `descriptor_buffer`,
`descriptor_indexing` and dma-buf/external memory are all **optional** to zink.
The one thing it requires that would kill a copying design — persistently mapped
`HOST_VISIBLE|HOST_COHERENT` memory for `GL_ARB_buffer_storage`, mandatory from
GL 4.4 — the arena handles natively. That is the requirement justifying itself.

**Stretch: DXVK ≤ 2.6.x**, which needed only two required device extensions and
enabled `bufferDeviceAddress` opportunistically for NVX CUDA interop only.

**Out of scope: DXVK ≥ 2.7 / 3.x and vkd3d-proton.** DXVK 3.x's `dxbc-spirv`
backend emits `CapabilityPhysicalStorageBufferAddresses` and
`AddressingModelPhysicalStorageBuffer64` unconditionally for every shader, and
binds UBOs/SSBOs as raw 64-bit addresses through push constants. vkd3d-proton is
worse: `ID3D12Resource::GetGPUVirtualAddress()` is part of the D3D12 API surface,
every buffer gets `SHADER_DEVICE_ADDRESS_BIT` unconditionally, and there is an
entire VA reverse-lookup subsystem plus a shader-read aux buffer of raw VAs. It
also demands a 1.4-class feature set and a million update-after-bind descriptors
per stage (fwd §5.2, §5.3).

**One honest caveat, recorded rather than argued.** The obstruction is not
quite "a raw VA is meaningless across the boundary" — if we return the GPU
side's true device address from `vkGetBufferDeviceAddress` and never relocate,
an address the application writes into arena memory *is* dereferenceable by a
GPU-side shader. What actually breaks is everything downstream of that promise:
we could no longer suballocate freely, could never relocate or lazily create a
resource, would owe permanent residency for every allocation, and would have
data-borne references that the `transform` pass of §7.2 cannot see because they
never cross the wire. Combined with vkd3d's independent 1.4-class demands, the
scope decision is unchanged and firm — but the reason is "the design cost is
disqualifying and the benefit is a project we are not doing", not "it is
impossible". R12 in `security-model.md` is annotated accordingly. *What would
change my mind:* Zink shipping and conformant, plus a decision to expose a
second, non-suballocated, permanently-resident memory class for BDA-using
applications. That is a different project and it comes after this one.

**What this means for the DirectX goal, stated without hedging.** D3D9, D3D10
and D3D11 through DXVK ≤ 2.6 are a realistic target once Zink works. **D3D12 is
not, and will not be.** The honest answer for D3D12 on `testhost` is to run
D3D12 applications *inside the guest*, next to the GPU, where they already work.
The "Vulkan is the single waist of the hourglass, so GL and D3D come free"
framing holds for GL and for older D3D, and does not hold for D3D12.

---

## 12. Crates

| Crate | Kind | Contents |
|---|---|---|
| `vk-registry` | lib | **Exists.** Grows the typed model and the C declarator parser (§7.1). The only thing that reads `vk.xml`. |
| `sv-codegen` | bin + lib | The generator: 11 passes, exception table, `quote`/`prettyplease` emission, protocol-hash computation |
| `sv-proto` | lib, **generated**, `forbid(unsafe_code)` | Types, encode, decode, pnext, handle_map, transform, subdecode |
| `sv-icd-entry` | lib, **generated** | `extern "system"` stubs and proc-address tables. The only generated crate that permits `unsafe`, in one mechanical shape |
| `sv-wire` | lib | Bounds-checked cursor, packet headers, primitive encodings. Hand-written, Miri-tested |
| `sv-transport` | lib | Region mapping, handshake, rings, reply rings, reverse ring, `Parker`/`Doorbell` traits and backends, cold-path socket |
| `sv-arena` | lib | Slabs, the 4096-granularity allocator, deferred free, `ArenaProvider`/`ArenaAttacher` |
| `sv-handles` | lib | App-side id allocation; GPU-side `SlotMap` object table with deferred destroy |
| `sv-icd` | cdylib | The app side: instance/device/queue objects, memory, WSI, the hand-written half of the ICD |
| `sv-gpu-server` | bin | The GPU side: ring consumers, `SlabImport`, semantic glue, submit path, validation forwarding |
| `sv-present` | lib | Swapchain plumbing shared by both sides; `wl_shm`/`XShm` blit on the app side |
| `sv-hostd` | bin | App-side daemon: opens QEMU's memfd, GPA→offset, builds the contiguous view, hands fds to ICD processes |
| `vk-probe` | bin | **Exists.** Grows the arena-import probe of §13 step 0 |
| `sv-testkit` | lib, test-only | Round-trip harness, the Venus differential comparison, fake transports, fixtures |

`sv-proto` is the enormous one and it is generated, which is why the worktree
script's warning about shared `CARGO_TARGET_DIR` matters here more than on most
projects: a stale-rmeta miscompile in generated marshalling would be close to
undebuggable.

---

## 13. Implementation order

Each step is independently testable and each produces something that either
works or fails visibly. **Steps 1–6 need no VM, no Windows, and no GPU**,
because lavapipe lets the GPU side run on an ordinary Linux box and the
transport's shared-memory backend is a plain memfd. That is also the mitigation
for R18: the iGPU that would hang drives a family member's laptop panel, and we
should not be pointing malformed command streams at it while learning.

**0. Retire the gating risk.** Extend `vk-probe` to allocate an AWE region in
the guest, call `vkGetMemoryHostPointerPropertiesEXT` on it and on an ordinary
`VirtualAlloc` pointer, and attempt `vkAllocateMemory` with
`VkImportMemoryHostPointerInfoEXT` on both. Separately, dump the PFN array and
check it against `info mtree -f` from the host. *Test: the import succeeds and
the frames resolve to plausible memfd offsets.* Nothing below is worth building
until this passes. It is an afternoon.

**1. `sv-wire` + `sv-transport` over a memfd, two Linux processes, no Vulkan.**
Handshake, magic-written-last, generation counter, ring ping-pong, reply ring,
reverse ring, heartbeat timeout, peer-restart detection. *Test: a
throughput/latency benchmark and a chaos test that kills each side at random
points; Miri over the ring with many seeds.*

**2. `sv-arena`.** Allocator, slabs, deferred free. *Test: Miri over a
`Vec<u8>`-backed arena; a property test that allocations never overlap and that
a freed range is not reissued before its timeline value passes.*

**3. The type model and the generator skeleton**, emitting a deliberately tiny
command set: `vkCreateInstance`, `vkEnumeratePhysicalDevices`,
`vkGetPhysicalDeviceProperties`, `vkCreateDevice`, `vkAllocateMemory`,
`vkMapMemory`, `vkCreateBuffer`, `vkBindBufferMemory`. *Test: generated
round-trip property tests, plus the model compared against the
`vulkan_object.py` fixture.*

**4. End-to-end round trip on one Linux box.** `sv-gpu-server` on lavapipe,
`sv-icd` driven by a test harness (not yet the loader). Allocate arena memory,
map it, write a pattern app-side, read it GPU-side through the imported
`VkDeviceMemory`. *Test: the bytes match, and the GPU side never copied them.*
This is the milestone that proves the thesis.

**5. The loader.** Manifest, `VK_DRIVER_FILES`, dispatchable-handle magic,
interface negotiation, `vulkaninfo` producing a sane report, then a triangle
through `vkcube` on headless. *Test: `vulkaninfo` under the validation layers
with no errors — the dispatch-magic contract is the classic silent-segfault
trap and this is where it surfaces or does not.*

**6. Widen to Zink's requirement set**, and run the `zink-lavapipe` deqp suite
through our ICD. *Test: deqp pass rate, compared against `zink-venus-lavapipe`
as the reference configuration.* This is the largest single step and it is
mostly generator coverage plus GPU-side semantic glue.

**7. Cross the boundary.** Windows build of `sv-gpu-server`, AWE arena,
`sv-hostd`, cold path over virtio-serial. *Test: step 4's byte-pattern test,
across the VM boundary, then step 6's deqp suite across it.* Note R15: nobody
has validated Intel's Windows driver running headless in a background process
with imported memory only, so run a trivial compute workload through it before
building on it.

**8. Presentation**, after the §10 measurement decides whether it is needed.

**9. DXVK ≤ 2.6**, as a stretch, only once 6 and 7 hold.

Steps 1, 2 and 3 are independent of each other and can proceed in parallel in
separate worktrees; 4 needs all three.

---

## 14. What remains unmeasured

Listed plainly, largest first.

1. **Whether importing *guest RAM shared with the host* works.** This is the
   big one and it is the whole design. `measured-gpu-side.md` establishes that
   `VK_EXT_external_memory_host` is present and page-granular on the real GPU
   side — that the *mechanism* exists. It does not establish that an import of
   *our particular pages* will succeed. Ordinary process memory and AWE-backed
   guest RAM that a host process also has mapped are not obviously the same
   thing to a driver that pins pages. Step 0 exists for this and nothing should
   be built on top of it first.
2. **Whether `AllocateUserPhysicalPages` still returns raw guest PFNs** on
   Windows 11 24H2. The Windows Research Kernel source settles it for the NT 5.2
   era and there is no reason to think it changed, but "no reason to think"
   is not a measurement.
3. **Whether AWE's "cannot be used to buffer data for graphics or video calls"
   is enforced or vestigial.** The phrasing smells of GDI/DirectDraw and
   `MmProbeAndLockPages` has a dedicated AWE path with `MDL_DESCRIBES_AWE`, so
   kernel-level DMA from AWE is supported — but whether WDDM's user-pointer
   import path accepts it is empirical. This is R19 and the reason D2 exists.
4. **The GPA→memfd offset formula on this machine.** i440fx computes the
   below-4G split with a machine-version compat knob; it is not a constant to
   hardcode, and `info mtree -f` must be parsed. R21.
5. **Whether the Intel Windows driver tolerates headless service operation**
   with imported memory only and no swapchain. Not a configuration its vendor
   tests. R15.
6. **Ring throughput and latency in this topology.** Every number in the
   research is either arithmetic or from another machine.
7. **Whether the app side should present at all**, or whether the physical panel
   should stay on the passed-through iGPU (§10, fwd §6.6).
8. **Whether `AllocateUserPhysicalPages2` accepts
   `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`**, which would give both PFNs and
   2 MiB contiguity and cut the app-side VMA count 512×. Ten minutes, worth
   spending during step 0.

---

## 15. Risks

R1–R8 are in `security-model.md`. R9–R24 have been added to that file as part of
this design: the arena import risk, cache aliasing, the fixed arena window,
`bufferDeviceAddress` scope, version skew, the reverse channel, headless driver
operation, the loader trampoline ceiling, being the first Rust ICD, GPU hangs
being visible to a person, and the new ones this design introduces — AWE's
graphics restriction, the unmeasured guest-RAM import, GPA translation fragility,
cross-ring ordering, memory-table honesty, and the guest RAM the arena takes away
from a production Windows session.

That file is the maintained list. This one is the design.
