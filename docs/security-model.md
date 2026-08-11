# Security model: this is not a security boundary

## The stance

**shared_vulkan is not a security boundary, and must not be described as one.**

Both ends belong to the same person and are, in the intended deployment, two
halves of one laptop. The app side is trusted by the GPU side in the same way a
process trusts a library it loaded. If someone can run code on the app side,
they were already inside.

Rust is used here for a different reason: to make the code **as correct as it
can be made**, given that it consists largely of generated marshalling over raw
shared memory, where the failure mode of a mistake is silent corruption rather
than a clean crash. Rust's guarantees buy us confidence in *our own* code. They
buy us nothing about the boundary, because a shared arena that two processes
write to is outside what the language can reason about by construction.

So:

- Do **not** contort the design to make the boundary defensible. In particular,
  do not add copying, revalidation or shadow buffers whose only purpose is to
  protect one side from the other. That would defeat the entire point of the
  passthrough arena, which exists so the GPU can DMA from application memory
  without a copy.
- Do **take** the cheap safety measures — the ones that cost approximately
  nothing and mostly catch our own bugs rather than attacks. Bounds-checking an
  offset against the region length is a compare. A handle table is required for
  correctness anyway. Take those.
- Do **document every risk**, which is the actual requirement. The list below is
  to be maintained as the design develops, not written once.

## Why validation would be theatre here anyway

Worth stating plainly, because it is the strongest argument against pretending:
the arena is memory both sides can write concurrently. Anything the GPU side
validates can be changed by the app side immediately afterwards, before the GPU
reads it. Validation followed by use is a time-of-check/time-of-use race by
construction, and the only way to close it is to copy into memory the app side
cannot reach — which is exactly the design we have been told not to build.

A boundary that cannot be enforced should not be advertised. Hence this
document.

## Known risks

Maintained list. Each entry says what can go wrong and what, if anything, we
do about it.

### R1 — The app side can feed arbitrary data to a real GPU driver
The GPU side passes app-supplied structures to a production Vulkan driver.
Malformed input can crash that driver, hang the GPU, or trigger a GPU reset.
**Consequence on testhost is worse than usual:** that GPU drives the physical
laptop panel, so a hang is immediately visible to whoever is sitting in front
of it, and recovery may need the VM restarted.
*Mitigation:* none beyond validation layers during development. Accepted.

### R2 — Arena offsets can point anywhere in the arena
Commands carry offsets rather than payloads. A wrong offset lets the GPU side
read or write another allocation's memory, or — if unchecked — outside the
region entirely.
*Mitigation:* bounds-check every offset against the region length. Cheap, and
catches our own generator bugs, which is the realistic failure. Per-allocation
containment is **not** attempted: it cannot be enforced (see TOCTOU above) and
costs more than it buys.

### R3 — Handles are attacker-chosen integers
Vulkan handles are opaque 64-bit values. If the GPU side treats an app-supplied
handle as a pointer, the app side chooses what it dereferences.
*Mitigation:* handles crossing the boundary are always indices into a table
owned by the GPU side, never raw pointers. This is required for correctness
regardless — the two sides do not share an address space — so it is free.

### R4 — Denial of service, in both directions
Either side can spin, hold a lock, fill a ring, or exhaust the arena. A polling
transport makes this easy by accident.
*Mitigation:* timeouts on waits, and a generation counter so a restarted peer is
detected rather than deadlocked against. Not defended against a deliberate
attempt.

### R5 — The mapping itself is total access
If the arena lives in the VM's RAM (a `memory-backend-memfd` with `share=on`),
the side that mmaps it can read all of it, not merely the arena. This is already
true on testhost today for unrelated reasons, and is inherent to running the
VM at all — the hypervisor can always read guest memory.
*Mitigation:* none possible. Noted so nobody believes the arena is a limit on
what the other side can see.

### R6 — Unsigned kernel drivers
If a transport requires a driver Windows will not load without permanent
test-signing, adopting it lowers the security posture of the whole guest, not
just this project. On testhost the guest is a family laptop in production
use.
*Mitigation:* hard constraint — no permanent test-signing. This may eliminate a
transport option entirely, which is a design input rather than a risk to accept.

### R7 — `unsafe` in our own code
Mapping shared memory, casting generated structures, and pointer arithmetic in
the arena allocator are all `unsafe`, and no amount of stance-taking makes a
mis-transmuted struct benign.
*Mitigation:* this is the one place to be strict. Confine `unsafe` to a small
number of reviewed modules with stated invariants, keep the generated code
safe-by-construction where possible, and test the allocator and the ring under
Miri. This is what "use Rust to make it as safe as possible" actually means in
practice.

### R8 — Generated code trusting the registry
The wire format is generated from `vk.xml`. Wrong `len=`/`optional=` metadata,
or a generator bug reading it, produces marshalling that is wrong in the same
way on both sides and therefore *looks* consistent while corrupting memory.
*Mitigation:* pin `vk.xml` through nix so it changes only deliberately;
differential-test generated marshalling round-trips rather than trusting
symmetry. Stronger: compare byte-for-byte against Venus's MIT-licensed generated
C on the shared subset, which is independent evidence rather than self-agreement
(design.md §6.2, §7.3).

---

R9–R18 arose from `research-vulkan-forwarding.md` §8. R19–R24 arise from the
decisions taken in `design.md`. All are recorded here because this file, not the
design, is the maintained list.

### R9 — `VK_EXT_external_memory_host` may not accept our arena
The zero-copy design rests on the GPU-side driver importing arena pages as
`VkDeviceMemory`. `vkGetMemoryHostPointerPropertiesEXT` returning
`memoryTypeBits == 0` is a *legal* driver response, and there are field reports
of `VK_ERROR_OUT_OF_DEVICE_MEMORY` from this path on some drivers. The extension
being advertised — which `measured-gpu-side.md` establishes — is not the same
claim as our particular pages being importable.
*Mitigation:* `design.md` §13 step 0 tests it before anything else is built. If
it fails, every fallback is worse and the copying mode of
`research-vulkan-forwarding.md` §2.6 does not meet the requirement at all.

### R10 — Cache attribute mismatch corrupts silently
Mapping the same physical pages with different cache attributes from two page
tables is architecturally undefined on x86, and the failure mode is corrupted
data under load rather than a fault.
*Mitigation:* retired by construction rather than managed — `design.md` §5.6
fixes one cache class, write-back, identical on both sides, because neither the
AWE window nor a memfd mapping can be made write-combining without adding a
driver. The `cache_class` field exists with one legal value. Any future need for
`honor-guest-pat=on` is treated as evidence we got this wrong.

### R11 — The arena is a fixed-size, pre-negotiated window
Host-visible memory cannot be grown on demand. An application allocating more
than we reserved gets `VK_ERROR_OUT_OF_DEVICE_MEMORY` at an arbitrary point.
gfxstream hit this and answered with block granularity plus a suballocator; we
inherit the constraint.
*Mitigation:* size generously, report the *real* size in the heap so
applications budget against it, and grow by adding slabs rather than resizing.
A segregated free list rather than a buddy allocator, because buddy waste
converts directly into this error (design.md §5.2).

### R12 — `bufferDeviceAddress` puts DXVK ≥ 2.7 and vkd3d-proton out of reach
A property of those projects and of the D3D12 contract, not a bug to fix.
*Mitigation:* scope to Zink and DXVK ≤ 2.6, and **do not advertise the feature**
— a clean device-creation failure is far better than a device that advertises it
and renders garbage. *Annotation added by design.md §11:* the obstruction is
more precisely that raw VAs in application-written data are invisible to the
`transform` pass and would forbid suballocation, relocation and deferred
allocation — not that such an address is meaningless across the boundary. The
scope decision is unchanged; the reason is cost, not impossibility.

### R13 — Version skew between the two sides corrupts rather than fails
Venus's wire format encodes command length implicitly in the command type, so
two builds from different registry *or generator* versions desynchronise the
stream and misinterpret everything downstream rather than reporting a mismatch.
gfxstream's own source warns that `optional=` semantics are not stable across
header releases, so pinning `vk.xml` is necessary but not sufficient.
*Mitigation:* a `protocol_hash` over the generated command and struct table,
exchanged in the handshake with a hard refusal on mismatch, **plus** an explicit
`u32` length in every command header so a desynchronisation is detected at the
first bad command instead of propagating (design.md §4.3, §6.2).

### R14 — No reverse channel means no validation-layer output on the app side
Debug callbacks must run in the app side's address space, and neither Venus nor
gfxstream implements a generated reverse-call path. Validation layers are the
primary debugging tool for an ICD, so this is a development-velocity risk rather
than a niche gap.
*Mitigation:* a dedicated GPU→app reverse ring in the control region, built from
day one rather than retrofitted (design.md §4.6). Roughly 200 lines.

### R15 — The GPU-side process must survive without the driver's usual context
Nothing has been validated against a Windows Vulkan driver running headless in a
background process, imported-memory-only, with no swapchain. Intel does not test
its driver this way.
*Mitigation:* none available in advance. Run a trivial compute workload through
the GPU-side server before building on it (design.md §13 step 7).

### R16 — Loader unknown-extension trampoline ceiling
`MAX_NUM_UNKNOWN_EXTS` is 250 in the loader, and its known-extension list
depends on the loader's own header version. An older app-side loader could run
out of trampolines.
*Mitigation:* advertise a curated extension set rather than mirroring all 148
device extensions the GPU side reports — which is required for scope reasons
anyway (design.md §8, §9.2).

### R17 — We would be the first production Rust Vulkan ICD
The only substantially complete prior attempt (Kazan) is dormant and LGPL. The
dispatch-magic contract in particular is a place where Rust's natural idioms
(`Arc`, trait objects, `#[repr(Rust)]`) are all wrong, and the loader only
asserts in debug builds — so a mistake is a segfault inside the loader.
*Mitigation:* copy the `#[repr(C)] { loader_dispatch_ptr: usize, value: T }`
pattern exactly, never `Arc` as a handle, and add compile-time signature
assertions against the `PFN_*` typedefs (design.md §6.4, §8).

### R18 — GPU hangs are visible to a person sitting at the laptop
The iGPU drives the physical panel of a family laptop in production use. A hang
or TDR from a malformed command stream is a visible fault for a non-technical
user, and recovery may need the VM restarted. This sharpens R1 rather than
adding to it.
*Mitigation:* develop against lavapipe on the GPU side. `design.md` §13 is
ordered so that steps 1–6 need no VM, no Windows and no GPU at all, which is the
main reason for that ordering.

### R19 — AWE's "not for graphics or video calls"
Microsoft's Address Windowing Extensions documentation states that "AWE address
ranges cannot be used to buffer data for graphics or video calls". The text is
old and the phrasing suggests GDI/DirectDraw, and `MmProbeAndLockPages` has a
dedicated AWE path (`MDL_DESCRIBES_AWE`) so kernel-level DMA from AWE ranges is
supported — but whether WDDM's user-pointer import path accepts one is
empirical, and the whole primary memory plan depends on it.
*Mitigation:* tested in `design.md` §13 step 0. If it fails, the fallback is an
attestation-signed ~200-line pinning driver over ordinary `VirtualAlloc` memory,
which dodges every AWE restriction — attestation signing is a genuine Microsoft
signature, not test-signing, so R6 is not violated.

### R20 — Importing guest RAM shared with the host is unmeasured
The single largest gap in the whole design. What is measured is that the
mechanism exists and is page-granular. What is *not* measured is whether a
driver that pins pages will accept memory that is simultaneously guest RAM, an
AWE allocation, and mapped by a process on the other side of the hypervisor.
There is no prior art for this exact combination — gfxstream's equivalent path
runs in the conventional direction.
*Mitigation:* none. It is a gate, not a risk to manage: `design.md` §13 step 0
answers it in an afternoon and nothing else is worth building first.

### R21 — GPA → memfd offset translation is configuration-dependent
The arena's location in the memfd is derived from guest physical frame numbers
plus QEMU's RAM aliasing. On i440fx the below-4G split depends on RAM size and
machine version and is **not** a constant; `info mtree -f` must be parsed, and
`info ramblock`'s `Offset` column is a red herring. A wrong translation does not
fail — it silently maps the wrong pages, and the two sides then disagree about
what the arena contains.
*Mitigation:* the translation lives in exactly one place (`sv-hostd`), is
cross-checked against the closed form, and is validated at handshake by writing
a known pattern at known offsets from the GPU side and reading it back on the
app side before any Vulkan call is made.

### R22 — Cross-ring ordering depends on a barrier we must emit correctly
Per-queue rings mean the application's own happens-before does not survive the
split: an object created on the device ring may not yet exist on the GPU side
when a queue ring references it. The `RingBarrier` mechanism closes this, but
the *emission rule* is app-side encoder logic that is easy to get subtly wrong,
and the failure is an intermittent "unknown handle" under load.
*Mitigation:* the barrier is emitted conservatively (any packet referencing an
id created on another ring since the last barrier) rather than cleverly; an
unresolvable id on the GPU side is fatal and loud rather than skipped; and the
chaos test in `design.md` §13 step 1 exercises multi-ring interleaving.

### R23 — The advertised memory table is a claim about another machine
`vkGetPhysicalDeviceMemoryProperties` is the steering wheel: applications choose
their whole data flow from it. Ours describes an arena on one machine and a heap
on another, and the two are only physically the same RAM by accident of this
hardware being a UMA iGPU. On a discrete GPU side, or if the arena is smaller
than we told the application, the table becomes a lie that surfaces as
allocation failure at an arbitrary moment.
*Mitigation:* report the *real* arena size in heap 1 and the real remaining heap
in heap 0; drop the `DEVICE_LOCAL | HOST_VISIBLE` type entirely when the GPU
side is not UMA, rather than advertising it optimistically (design.md §9.1).

### R24 — The arena takes guest RAM away from a production Windows session
The arena is locked, non-pageable guest RAM, carved out of the 13 GiB a family
member's Windows session is using. A generously sized arena (R11 argues for
generous) directly reduces what that session has. Under VFIO the pages are
pinned anyway, so this is not additional host cost — it is guest cost, and it is
paid whether or not anyone is rendering.
*Mitigation:* size the arena from configuration rather than a compiled-in
default; allocate slabs lazily so the resident set tracks actual use; release
the whole arena when the last app-side client disconnects. Do not run the
GPU-side server at boot until this has been exercised.
