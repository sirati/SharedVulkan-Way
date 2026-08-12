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
*Settled 2026-08-11 — the constraint cost nothing, because no path in the plan
needs a driver at all.* Two candidates were live when this was written. The
**attestation-signed pinning driver** (R19's fallback) is **dropped**, not merely
unused: everything it was held in reserve for has been measured away — an
ordinary pointer imports, AWE imports and still returns real guest-physical
frames, and the content scan locates pages in ~2 s, so an EV certificate would
have bought a one-off locate step 2 s faster on a path we have decided not to
take (`design.md` §5.1, D2). **ivshmem** is out for a mechanism reason rather
than a signing one — host-pointer import is kernel userptr, userptr pins pages,
and a BAR is `VM_IO|VM_PFNMAP`, which the pinning path rejects by construction
(`design.md` §5.1). The arena is ordinary guest RAM through the memfd the guest
already runs on. The constraint itself is not retired — it is a property of the
machine, not of the design — but nothing in the plan is now testing it. If a
driver is ever proposed again, this entry is why it has to be argued from a
stated new reason rather than revived from the old one.

### R7 — `unsafe` in our own code
Mapping shared memory, casting generated structures, and pointer arithmetic in
the arena allocator are all `unsafe`, and no amount of stance-taking makes a
mis-transmuted struct benign.
*Mitigation:* this is the one place to be strict. Confine `unsafe` to a small
number of reviewed modules with stated invariants, keep the generated code
safe-by-construction where possible, and test the allocator and the ring under
Miri. This is what "use Rust to make it as safe as possible" actually means in
practice.
*As built, 2026-08-11 (`design.md` §0.2, §7.3).* The generated protocol crate is
`#![forbid(unsafe_code)]` in its entirety — 126,205 generated lines, 657 tests,
Miri clean — so the largest body of code in the project contributes no `unsafe`
at all. Nothing is transmuted out of shared memory; every field is read through a
bounds-checked cursor. Hand-written `unsafe` remains confined to the three stated
modules (mapping, ring, allocator), and the ICD entry stubs, which are unavoidably
`extern "system"` over raw application pointers, are generated into a separate
crate with one mechanical shape so they are reviewable by pattern rather than
individually. What Miri cannot model is the cross-process race, which is outside
the language by construction — see the stance at the top of this file.

### R8 — Generated code trusting the registry — **realised, twice**
The wire format is generated from `vk.xml`. Wrong `len=`/`optional=` metadata,
or a generator bug reading it, produces marshalling that is wrong in the same
way on both sides and therefore *looks* consistent while corrupting memory.

**This is the entry that fired.** Twice, during the codegen stage, in precisely
the shape written above: a wrong answer that every symmetric test agreed with.
Both instances are recorded below, because a risk that has actually happened is
worth more than a risk that has only been imagined, and because the two of them
produce a rule.

*Mitigation:* pin `vk.xml` through nix so it changes only deliberately;
differential-test generated marshalling against an independent implementation
rather than trusting symmetry; and — the load-bearing one, learned rather than
planned — assert every derived numeric value against an independently published
number.

> **[SUPERSEDED — 2026-08-11, `design.md` §0.2 and §6.2; `crates/sv-testkit`]**
>
> *Stronger: compare byte-for-byte against Venus's MIT-licensed generated C on
> the shared subset, which is independent evidence rather than self-agreement
> (design.md §6.2, §7.3).*
>
> **The byte-for-byte comparison was never available, and by our own hand.** D6
> replaces Venus's per-parent `pNext` switch with length-prefixed links, and R13's
> mitigation adds a `u32` length to every command header. Nearly every Vulkan
> struct carries a `pNext`, so between them those two decisions change the bytes
> of almost every struct and every command. This promise was retracted, not kept.

**What was built instead, and why it is the stronger check.** The differential
parses Venus's generated C and compares, for each struct body, the **ordered
sequence of members and each member's kind and width** — scalar of a given width,
array, pointer-to-one, blob, string. That is exactly the information the
generator derives from `len=`, `optional=` and the C declarator, which are the
three things this risk is about, and it catches the whole class: a member one
generator emits and the other does not, a difference in member order, an array
read as a single value or the reverse, a scalar at the wrong width. **208 structs
are shared with the Zink profile and all 208 agree.**

It is stronger than a byte comparison in the respect that matters here: a byte
comparison passes or fails on one sample value, and can pass while `len=`
handling is wrong for every *other* value, whereas the structural comparison
holds for all values. It is weaker in one respect that does not: it does not
check the primitive encodings themselves, which are pinned separately by
`sv-wire` unit tests against the quoted C in `reference/venus/NOTES.md`.

The differential carries a **negative control**, which is what makes a green run
evidence rather than a formality: a test deliberately corrupts one shared struct
by reading an array member as a single value — the exact mistake a wrong `len=`
causes — and *requires* the comparison to fail. A differential can be green
because the two implementations agree or because the comparison would accept
anything, and without the control there is no way to tell which.

**Realised instance 1 — a sign inversion that every symmetric test agreed with.**
`vk-parse` exposes an extension enum's sign as `EnumSpec::Offset { dir, .. }`.
`dir` is named after the registry attribute `dir="-"` but holds the *opposite*
sense: it is `false` when the XML says `dir="-"` and `true` otherwise. Read as
"is negative" — the obvious reading, and the one written first — it **inverts the
sign of every extension-introduced enum value in the registry**, which for us
means the wrong `sType` on every extension struct.

Note carefully what did *not* catch it. The round-trip tests passed, because both
sides computed the same wrong number — symmetric self-consistency proving
nothing, which is this entry's central claim, now demonstrated rather than
asserted. The structural differential did not catch it either: it compares member
sequence, kind and width, and constants are none of those. What caught it was a
test asserting generated constants against published `vulkan_core.h` values
(`design.md` §7.3).

*The rule it produces:* **every derived numeric value gets at least one assertion
against an independently published number.** Round-trip tests establish that we
are self-consistent; only an external fixture establishes that we are right. That
is the same argument for keeping `vulkan_object.py`'s output as a fixture, and it
is why that fixture is not optional — it is **still outstanding** (`design.md`
§13 step 3), which this instance is the argument for doing rather than against.

**Realised instance 2 — the registry moved shape underneath the generator.**
`vulkan-headers 1.4.350` splits each core version into `apitype="internal"`
sub-features chained by `depends=`, so `<feature name="VK_VERSION_1_0">` contains
**no commands of its own**. A resolver that reads that feature and stops gets
nothing at all from Vulkan 1.0 and reports no error: the Zink profile silently
resolved to **55 commands instead of 239** — a small protocol, not a failure.
Only a lower-bound assertion on the command count noticed. The model now resolves
the `depends=` expression language rather than reading feature names, and **every
profile carries an asserted command count** (`design.md` §7.1, §11).

Same failure class, and worth stating because it widens the entry: pinning
`vk.xml` does not help here. The pinned file is the one with the new shape, and
the trust being violated is in the registry's *structure*, not its metadata. "The
generator quietly emitted a smaller protocol" has no other check that would have
caught it.

**An unplanned mitigation, and a real strengthening.** Every variable-length
thing on our wire carries its own explicit `u64` count — a decision taken for the
decoder's benefit, not this risk's. The consequence is that **a wrong `len=`
cannot desynchronise the stream**: the count on the wire governs, and a
disagreeing `len=` can only trip the generated cross-check. The registry metadata
this project most feared being wrong about was demoted from a load-bearing input
to a verifier (`design.md` §7.4). It does not cover `optional=`, the declarator,
or derived constants, which is why the structural differential and the
published-value assertions remain the primary defences — instance 1 was in
exactly the part this does not reach.

---

R9–R18 arose from `research-vulkan-forwarding.md` §8. R19–R24 arise from the
decisions taken in `design.md`. R25 arises from a change actually made to the
guest while measuring step 0, and R26 from something that measurement could not
settle. All are recorded here because this file, not the design, is the
maintained list.

### R9 — `VK_EXT_external_memory_host` may not accept our arena — **retired, measured**
The zero-copy design rests on the GPU-side driver importing arena pages as
`VkDeviceMemory`. `vkGetMemoryHostPointerPropertiesEXT` returning
`memoryTypeBits == 0` is a *legal* driver response, and there are field reports
of `VK_ERROR_OUT_OF_DEVICE_MEMORY` from this path on some drivers. The extension
being advertised — which `measured-gpu-side.md` establishes — is not the same
claim as our particular pages being importable.
*Mitigation:* `design.md` §13 step 0 tests it before anything else is built. If
it fails, every fallback is worse and the copying mode of
`research-vulkan-forwarding.md` §2.6 does not meet the requirement at all.
*Measured 2026-08-11 (`measured-gpu-side.md`).* The Intel driver advertises
`VK_EXT_external_memory_host` with `minImportedHostPointerAlignment = 4096` — a
page, so nothing has to be over-allocated to satisfy it — and the import was
then performed rather than inferred. `vkGetMemoryHostPointerPropertiesEXT`
returned `SUCCESS` with `memoryTypeBits = 0x6`, which is types 1 and 2, the two
host-visible ones, so an arena allocation has somewhere legal to go;
`vkAllocateMemory` accepted the import and `vkMapMemory` returned **the same
virtual address**, with a byte written through the Vulkan mapping immediately
visible through the original pointer. The feared answers — `memoryTypeBits == 0`
or `VK_ERROR_OUT_OF_DEVICE_MEMORY` — did not occur on this driver. Retired **for
this GPU side**; it says nothing about RADV, which refuses shared host mappings
altogether, which is why the import stays behind a trait (`design.md` §5.4).
Retiring R9 was never the same claim as R20, and R20 was measured separately.

*Widened 2026-08-11, on other hardware* (`measured-gpu-side.md`, "two drivers
refuse a shared mapping"). The same probe was run against three Linux
implementations with a `MAP_SHARED` window rather than an anonymous one, and
**two of the three refused it**: RADV with `VK_ERROR_INVALID_EXTERNAL_HANDLE`,
as predicted, and **NVIDIA's proprietary driver with
`VK_ERROR_OUT_OF_DEVICE_MEMORY`** — the exact field report quoted above,
reproduced. Both imported an ordinary anonymous mapping without complaint, so
the distinguishing property is that the memory is *shared*, which is not
optional for an arena. R9 is retired for the Intel/Windows GPU side and is
**live for every Linux GPU side except lavapipe**, whose acceptance
`design.md` §5.4 already warns is not evidence of anything.

### R10 — Cache attribute mismatch corrupts silently
Mapping the same physical pages with different cache attributes from two page
tables is architecturally undefined on x86, and the failure mode is corrupted
data under load rather than a fault.
*Mitigation:* retired by construction rather than managed — `design.md` §5.6
fixes one cache class, write-back, identical on both sides, because neither the
AWE window nor a memfd mapping can be made write-combining without adding a
driver. The `cache_class` field exists with one legal value. Any future need for
`honor-guest-pat=on` is treated as evidence we got this wrong.
*Note on the evidence, so this is not over-read.* Step 0 carried bytes through
the shared pages in both directions across the hypervisor without any explicit
flush, on every allocator tried, which is supporting evidence — but it was
measured **at rest**, one writer at a time, with a file-based rendezvous between
phases. Nothing has been measured about concurrent access or about cache
behaviour under load. §5.6 remains an argument with evidence behind it, not a
measurement (`design.md` §0).

*An instrument now exists, 2026-08-11* (`measured-gpu-side.md`, "A GPU running
work against arena memory"). `gpu-probe --concurrent` and `arena-host gpuchurn`
put a writer and the GPU on the same pages at the same time: the host alternates
the input region between two stamps while the GPU repeatedly copies it, and
every output word must then be one of the two values the writer ever wrote. A
word that is **neither** is the only thing an incoherent or torn read could
produce, so this can return positive evidence rather than only an absence.
Two things bound what a clean result would mean, and they are built into the
report rather than left to the reader: the check is at **4-byte granularity**,
so it says nothing about the atomicity of anything wider, and it observes
**whatever interleaving actually happened** — which is why the probe also counts
how many words held the old value and how many the new, and says outright that
the run measures nothing if the GPU only ever saw one of them.
**R10 is not retired by this.** An instrument that has not yet been pointed at
the target hardware changes nothing about the risk; what it changes is that the
gap is now closable by an experiment rather than only by an argument.

### R11 — The arena is a fixed-size, pre-negotiated window
Host-visible memory cannot be grown on demand. An application allocating more
than we reserved gets `VK_ERROR_OUT_OF_DEVICE_MEMORY` at an arbitrary point.
gfxstream hit this and answered with block granularity plus a suballocator; we
inherit the constraint.
*Mitigation:* size generously, report the *real* size in the heap so
applications budget against it, and grow by adding slabs rather than resizing.
A segregated free list rather than a buddy allocator, because buddy waste
converts directly into this error (design.md §5.2).
*Amended 2026-08-11.* "Grow by adding slabs" survives, but **growing late is
worse than it looks and lazy slabs are not the hedge they appear to be.** Both
paths commit their memory up front: the 4 KiB path must be committed and
located before the app side can map it at all, and the large-page path needs
contiguous physical memory, which is most available early and decays with guest
uptime (`measured-gpu-side.md`). A slab added an hour into a session is the
slab most likely to fail or to be badly fragmented. Prefer allocating the whole
arena at GPU-side start-up and treating late growth as a fallback that may fail,
not as the normal path.

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
*Sharpened 2026-08-11, and now the largest unmeasured thing in the project.*
Step 0 retired R9, R19 and R20, so what it proved is that the driver **accepts
and maps** our pages and that its mapping is demonstrably the same physical
memory. **No GPU has touched an arena page.** GPU-side address translation is
entirely downstream of the import succeeding, and none of it has been exercised.
`design.md` §13 makes the compute workload the first thing in step 7 rather than
a late check, for this reason.

*The workload exists, 2026-08-11* (`measured-gpu-side.md`, "A GPU running work
against arena memory"). `crates/gpu-probe` binds the imported allocation to two
`VkBuffer`s — at offset 0 and half-way in, so a suballocating arena's real
question is asked too — dispatches a compute shader that transforms every word
by a hash of its value *and* its index, waits on a fence, and has the result
judged independently by the guest and by a Linux process reading QEMU's memfd.
It reuses `awe_probe::alloc`, so it runs against the same memory the earlier
measurements did rather than an approximation of it. The whole path was
validated end to end on other hardware first, including the three-party
protocol; a first submission that hangs the target GPU would be visible to
whoever is at the laptop (R1, R18), so the fence has a 2 s timeout, a device
loss exits instead of retrying, and the window is released on every path.
It has run on **two real GPUs, and both passed** — NVIDIA's proprietary driver
and RADV, over a 128 MiB window of ordinary anonymous memory, 15728640 payload
words each, zero wrong and zero untouched, with buffers bound at offset 0 *and*
at 64 MiB. So a compute shader executing correctly against host-imported memory
is no longer hypothetical, and one specific way this could have failed early did
not: on both drivers the buffer's `memoryTypeBits` intersected the mask the
pointer import permitted, so imported memory was bindable and not merely
mappable. A mask that excluded it would have meant imported memory is usable for
CPU sharing and useless for GPU work, which would change the arena's premise.

**R15 is not retired, and the two passes are encouragement rather than
evidence.** They were the wrong driver, the wrong OS, and — the part that
matters most — the wrong *memory*: ordinary process memory, not guest RAM mapped
by a process across the hypervisor. R9's new result is the reason to keep those
apart rather than treat one as standing in for the other: two of three Linux
drivers accept an anonymous mapping and refuse a merely shared one, so "imports
anonymous memory" and "imports arena memory" are demonstrably different claims,
and "dispatches against anonymous memory" is a third. What remains unmeasured is
the whole of it on the target: an Intel Windows driver, headless,
imported-memory-only, no swapchain, over pages that are simultaneously guest RAM
and host-mapped. Building an instrument is not taking a reading.

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

### R19 — AWE's "not for graphics or video calls" — **retired, measured**
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
*Measured 2026-08-11, once `SeLockMemoryPrivilege` was granted (R25).* A 64 MiB
AWE window — `AllocateUserPhysicalPages` + `MapUserPhysicalPages` — was imported
by the Intel driver with `memoryTypeBits = 0x6`, mapped at the same address, and
carried the host's bytes through the `VkDeviceMemory` mapping in all 16384
pages. The documented restriction does not bind WDDM's user-pointer import path
on this driver. **The pinning driver is therefore not needed, and has been
dropped rather than held in reserve** (R6, `design.md` §5.1, D2). Note what
retired this risk and what did not: AWE is *admissible* — it imports, and its
frame numbers are real — and it still lost the arena to large pages on
contiguity (5863–16339 runs per 64 MiB against 22). Retirement here means the
documented prohibition is not real, not that AWE is the path we take.
Separately settled in the
same run: `AllocateUserPhysicalPages` still returns raw guest-physical frame
numbers on Windows 11 build 26200 — all 16384 were verified against the memfd
through the R21 translation (`design.md` §14 item 2, measured).

### R20 — Importing guest RAM shared with the host — **retired, measured**
The single largest gap in the whole design was whether a driver that pins pages
would accept memory that is simultaneously guest RAM, an AWE allocation, and
mapped by a process on the other side of the hypervisor. There was no prior art
for the combination — gfxstream's equivalent path runs in the conventional
direction.

**Measured 2026-08-11 on the real target** (`measured-gpu-side.md`, "Step 0").
A 64 MiB window of guest RAM was allocated in the Windows guest, located in
QEMU's `memory-backend-memfd` by a Linux process, written into from the host,
and then imported by the Intel driver:

- `vkGetMemoryHostPointerPropertiesEXT` → `SUCCESS`, `memoryTypeBits = 0x6` —
  the same answer ordinary process memory gets. The driver does not distinguish
  these pages.
- `vkAllocateMemory` with `VkImportMemoryHostPointerInfoEXT` → **OK**, at
  1 MiB and at 64 MiB.
- All 16384 pages of the driver's mapping carried the bytes the *host* had
  written across the hypervisor; bytes written back through the
  `VkDeviceMemory` mapping appeared in the memfd at the predicted offsets and
  were read out of guest-physical memory by QEMU's own `xp`; a second host
  write, made after the import, was seen by the guest.

So (a) guest RAM and (c) host-mapped across the hypervisor are settled: they
import, and the memory is genuinely shared in both directions.

**Leg (b), AWE, was measured later the same day**, once `SeLockMemoryPrivilege`
was granted (R25). The AWE window imported exactly like the others, and its
frame numbers resolved to the right memfd offsets for all 16384 pages — so the
design's §5.1 mechanism, "the side inside the VM allocates and reports frames,
the other side translates them", is measured end to end rather than assumed. All
three legs of R20 now hold.

Four allocators were run through the identical loop — `VirtualLock`ed 4 KiB
pages, AWE, `AllocateUserPhysicalPages2` with `NONPAGED_LARGE`, and
`VirtualAlloc(MEM_LARGE_PAGES)` — and the driver accepted every one of them with
the same `memoryTypeBits`. The import path does not care how the pages were
obtained. What differs between them is fragmentation, not admissibility
(`measured-gpu-side.md`, "Large pages, AWE, and the privilege that unlocks
both").
*Residual:* nothing here proves the *GPU* can DMA from these pages, only that
the driver accepts and maps them — that is R15's compute workload, for which an
instrument now exists (`crates/gpu-probe`) but which has not been run on the
target. Coherency was also measured at rest, one writer at a time (R10).

*Scope narrowed 2026-08-11.* R20 is retired **for the Intel/Windows GPU side**
and that boundary is now measured rather than assumed: on Linux, RADV and
NVIDIA's proprietary driver both refuse to import a shared mapping while
accepting an anonymous one (R9). "Guest RAM shared with the host is importable"
is a property of this driver, not of `VK_EXT_external_memory_host`.

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
*Measured 2026-08-11.* On this machine (`-machine pc`, 13312 MiB) the split is
**3 GiB**, not the 3.5 GiB an i440fx uses at smaller RAM sizes: below 4 GiB
`offset = gpa`, above it `offset = gpa - 0x4000_0000`. The failure mode was
confirmed as predicted rather than assumed — reading the memfd offset as if it
were the GPA lands on unrelated, zeroed memory and reports no error at all
(`measured-gpu-side.md`). Two carve-outs below 4 GiB are also not RAM at their
identity offsets: the `0xc0000-0xfffff` ROM window, and `0x103000-0x10efff`,
which twelve Hyper-V SynIC message pages overlay — so the translation must walk
the flat view, not compute a formula.

*Two consequences for the mitigation above.* "Cross-checked against the closed
form" is demoted: the closed form is a sanity check on the bulk of the range, not
a source of truth, because the carve-outs are exactly where it is wrong and
exactly where it would be wrong silently. And on the path actually chosen the
risk is much reduced, because the arena is **located by content** rather than
translated at all — `arena-host scan` sweeps the memfd for the run's stamp, so a
page it reports is a page it found, and a wrong answer is not available in the
way a wrong formula's is. Translation survives as the AWE alternative and as the
verifier for it (16384/16384 frames checked that way), not as the primary
mechanism. The handshake pattern check stays regardless — it is what catches R26
as well.

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
*Measured 2026-08-11 — the awkward case does not arise on this hardware, which
is luck and should be treated as such.* The GPU side reports **one** heap,
6624 MiB `DEVICE_LOCAL`, and of its three memory types every one except [0] is
simultaneously `DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT`. There is no
discrete VRAM an application can allocate but never map, so there is no class of
allocation structurally unable to live in the arena, and the coherent types mean
the first implementation can be correct without managing flushes. None of that
is a property of the design; it is a property of this iGPU. The conditional in
the mitigation stays, because the moment the GPU side is not UMA the table
becomes the lie this entry describes.

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
*Amended 2026-08-11 — "allocate slabs lazily" is superseded.* It does not do
what it promises on either measured path. The 4 KiB path commits and locks the
extent up front, because the app side cannot map pages that do not yet exist and
the offset table is built at attach time; the large-page path wants to allocate
early while contiguous physical memory is still available. Laziness therefore
buys no guest RAM back and costs reliability. The honest levers on guest cost
are the two that remain: **size the arena from configuration**, and **release it
promptly when the last client disconnects** — which is now the load-bearing one,
since the memory is held in full for as long as the GPU-side server runs. That
strengthens the closing advice: do not run the server at boot.

### R25 — The guest account now holds `SeLockMemoryPrivilege`
Granted 2026-08-11 to `TESTGUEST\<guest-user>` so the arena can use large pages and
AWE, both of which require it (`measured-gpu-side.md` records the exact change
and its revert). This is a real widening and belongs in this list rather than in
a paragraph of a measurement note.

**What it permits.** A process running as that account can lock pages into
physical memory that the kernel may not page out or trim: `VirtualLock` beyond
the working-set quota, `VirtualAlloc(MEM_LARGE_PAGES)`, and AWE. The concrete
abuse is a denial of service — a buggy or hostile process can pin memory the
system cannot reclaim, and unlike ordinary allocation the kernel has no recourse
but to fail other allocations. It also makes memory contents durably resident,
which slightly weakens the assumption that secrets eventually leave RAM.

**What it does not permit.** It is not a code-integrity setting. Secure Boot,
HVCI/VBS and driver signing are untouched — verified before and after: no
`testsigning`, no `nointegritychecks`, `hypervisorlaunchtype` still `Off`,
DeviceGuard `SecurityServicesRunning` still `0`. It grants no kernel-mode
execution, which is precisely why it is acceptable where R6's test-signing ban
is not. The account in question is already a local administrator and already
holds `SeDebugPrivilege` and `SeLoadDriverPrivilege`, both strictly more
powerful; this adds nothing that account could not already reach by other means.

**Scope.** One right, one SID, added with `LsaAddAccountRights` rather than a
`secedit` template so nothing else in the policy could move. The account held no
explicitly assigned rights before, so its explicit set is now exactly this one.
*Mitigation:* it is reversible in one command with no reboot and no interactive
logoff (`scripts/win-account-right.ps1 -Revoke`), and a pre-change policy export
is kept on the guest (`C:\Windows\Temp\user-rights-BEFORE-svprobe.inf`).

*Status after the measurements — this is no longer provisional.* When it was
granted it was an experiment's cost, to be revoked if the large-page route was
replaced. The route was not replaced; it was chosen. `VirtualAlloc(MEM_LARGE_PAGES)`
is the arena's primary allocator (22 runs per 64 MiB against 1471, and 1 run for
a whole gigabyte), and it is also R26's mitigation-by-construction, so the
privilege is now load-bearing on the main path and stays granted for as long as
that design stands. Two facts keep the exposure honest rather than open-ended:

- **The 4 KiB fallback needs no privilege at all** — `VirtualAlloc(MEM_COMMIT)`
  + `VirtualLock` carried the whole of step 0 before the grant existed. So a
  revocation degrades the arena to the fallback path; it does not stop the
  project. That is the escape hatch, and it is measured rather than assumed.
- **AWE no longer motivates it.** AWE was the first thing this privilege was
  wanted for — step 0 was blocked on it (`ERROR_PRIVILEGE_NOT_HELD`) — and once
  granted, AWE lost the arena on contiguity anyway (5863–16339 runs per 64 MiB).
  It survives as an alternative, not a plan, so if large pages were ever
  abandoned the privilege would have to be re-justified from AWE's own merits
  rather than inherited.

Treat "the privilege is still granted" as something to re-justify whenever the
memory plan changes, and revoke it if the plan ever stops needing large pages.

### R26 — `VirtualLock` promises residency, not frame stability
The 4 KiB path pins arena pages with `VirtualLock`, and the app side then holds
a table of memfd offsets derived from where those pages were when it attached.
`VirtualLock`'s contract is that the pages stay *resident* — it says nothing
about the physical frame behind a page staying the same one. Nothing in the
documented behaviour forbids the memory manager from relocating a locked page,
and a hypervisor-side actor could in principle move guest-physical pages too.
If a frame moved, the app side would keep reading the old offset: no fault, no
error, just an arena where one side's writes stop reaching the other. The two
sides would disagree about memory contents, which is R21's failure mode arriving
by a different route.

**There is no cheap detector on the 4 KiB path as designed.** The per-page
stamps that make step 0's verification work are overwritten the moment arena
data lands in those pages, so "is this still my page?" has nothing to compare
against. Re-scanning guest RAM costs a full sweep of the memfd (~2 s, measured)
and only tells you where the pages are *now*.

*Measured, weakly:* a 64 MiB `VirtualLock`ed window was located in the memfd,
held while the guest went about ordinary business, and re-located **3m14s**
later; the offset table was byte-identical, all 16384 pages, 931 runs both
times. That is evidence against casual movement and is **not** evidence against
movement under memory pressure, which was not provoked — deliberately, since the
guest is somebody's daily machine.

*Mitigation, in order of preference:*
1. **Use large pages** (`measured-gpu-side.md`) — **and this is the path that
   was chosen**, so the risk is removed by construction on the main path rather
   than managed. A large page is not a candidate for relocation by the ordinary
   paging machinery. This was a second independent argument for large pages; the
   first was fragmentation, and they agree.
2. If the 4 KiB fallback is used, **reserve a canary** — the arena allocator
   never hands out the first 64 bytes of each slab-sized block, and the host
   re-checks those canaries periodically and at every handshake. That restores a
   cheap detector at a cost of 64 bytes per block, and turns a silent
   divergence into a loud one.
   *Built and exercised 2026-08-11* (`measured-gpu-side.md`, "A GPU running work
   against arena memory"). `gpu-probe` reserves the first 256 bytes of every
   page: neither the host's fill nor the compute shader writes them, so the
   `SVARENA1` record survives the workload and both sides re-check page identity
   *after* the GPU has written the whole window rather than only before. On the
   harness run all 2048 page headers were intact on the guest side and all 1024
   output-page headers on the host side. The cost was measured rather than
   estimated — at page granularity it is 6.25% of the window, which is
   substantially more than the 64-bytes-per-slab this entry proposed, and the
   difference is the point: per-page canaries are what make a *wrong page*
   detectable, per-slab canaries only detect a wrong slab. Which granularity the
   arena wants is a design decision this measurement can now inform.
3. Treat a canary mismatch as fatal to the session, not as something to repair:
   by the time it is noticed the two sides have already disagreed.

### R27 — The ICD talks to the window system from inside the application
design.md §10 stage 2 makes the app side responsible for putting a finished
frame in a window, which means an ICD `dlopen`ing `libX11.so.6` and issuing X
requests from a process it does not own. Three distinct hazards, all realised
at least once during the stage that introduced this entry (design.md §0.6):

- **Xlib's default error handler calls `exit(1)`.** Every request names a window
  the *application* owns and may already have destroyed, and that race is the
  ordinary shape of a GL program rather than a corner: `glxinfo` creates and
  destroys a window per config it probes, and the first version of this code
  killed it with `BadDrawable` from `XCreateGC`. **A driver must never abort the
  application because a frame could not be shown.**
- **`XSetErrorHandler` is process-global, not per-`Display`.** Installing one
  changes the application's error handling too, for as long as it is installed.
- **Using the application's own `Display*`** — which the loader hands us in the
  surface — would put the driver inside somebody else's locking discipline, and
  Xlib is only thread-safe if the *application* called `XInitThreads`. glxgears
  does not.

*Mitigation, and all three are in `sv_icd::present`:* open a **separate**
connection (X window ids are global to the display, so a second connection can
draw into a window the first created, and `$DISPLAY` names the same server);
install the swallowing error handler only around our own requests and end that
window with an `XSync`, so every error we caused has been consumed before the
application's handler goes back; and never dereference the `Display*` or
`xcb_connection_t*` the surface carries — only the window id is read.

*What this does not mitigate:* the extra connection is a second client the X
server accounts to the same user, and a frame is `XPutImage`d unencrypted over
whatever transport `$DISPLAY` names. Both are properties of X rather than of
this design, and both are reasons the phase-2 answer is a compositor rather than
more of this.
