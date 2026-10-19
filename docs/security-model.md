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

*Revisited 2026-10-15: ivshmem, for waycompose's arena only, for a stated new
reason* (`waycompose-design.md` §83). **The reason is root in the product path.**
The compositor's arena was guest RAM, and on testhost the host can reach guest
RAM only through QEMU's monitor and descriptor table, both root's, so every
session the desktop shortcut started became root twice. An `ivshmem-plain`
device backed by a file at a path lets one group reach those pages instead.
**The constraint above is met, not waived.** The driver is the Looking Glass B7
build, signed by *Microsoft Windows Hardware Compatibility Publisher* under
*Microsoft Windows Third Party Component CA 2014*, so it is not test-signed and
needs nothing changed in code integrity. §83.8 records the hashes and what
`Get-AuthenticodeSignature` reported. **The old mechanism objection is not
contradicted.** It concerns importing a BAR into a GPU driver, and shared_vulkan's
arena, which is imported, stays guest RAM. The compositor's arena is read and
written by CPUs only. The new costs are R29.

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
> (docs/design.md §6.2, §7.3).*
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
converts directly into this error (docs/design.md §5.2).
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
and renders garbage. *Annotation added by docs/design.md §11:* the obstruction is
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
first bad command instead of propagating (docs/design.md §4.3, §6.2).

### R14 — No reverse channel means no validation-layer output on the app side
Debug callbacks must run in the app side's address space, and neither Venus nor
gfxstream implements a generated reverse-call path. Validation layers are the
primary debugging tool for an ICD, so this is a development-velocity risk rather
than a niche gap.
*Mitigation:* a dedicated GPU→app reverse ring in the control region, built from
day one rather than retrofitted (docs/design.md §4.6). Roughly 200 lines.

### R15 — The GPU-side process must survive without the driver's usual context
Nothing has been validated against a Windows Vulkan driver running headless in a
background process, imported-memory-only, with no swapchain. Intel does not test
its driver this way.
*Mitigation:* none available in advance. Run a trivial compute workload through
the GPU-side server before building on it (docs/design.md §13 step 7).
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
anyway (docs/design.md §8, §9.2).

### R17 — We would be the first production Rust Vulkan ICD
The only substantially complete prior attempt (Kazan) is dormant and LGPL. The
dispatch-magic contract in particular is a place where Rust's natural idioms
(`Arc`, trait objects, `#[repr(Rust)]`) are all wrong, and the loader only
asserts in debug builds — so a mistake is a segfault inside the loader.
*Mitigation:* copy the `#[repr(C)] { loader_dispatch_ptr: usize, value: T }`
pattern exactly, never `Arc` as a handle, and add compile-time signature
assertions against the `PFN_*` typedefs (docs/design.md §6.4, §8).

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
side is not UMA, rather than advertising it optimistically (docs/design.md §9.1).
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
*Update 2026-08-13: this risk now has a direct instrument rather than a proxy.
The guest can re-read the identity of exactly the frames it already holds in
**0–22 ms** (512 frames 0 ms, 16384 frames 1 ms, 262144 frames 22 ms) and see
whether each is still mapped at the virtual address it was — R28,
`measured-gpu-side.md`. That is cheap enough to run at every handshake and it
answers the question the canary below only approximates. A second data point was
taken with it: a 64 MiB locked window held 300 s on a machine in use came back
`frame list unchanged after 300s`, all 757 runs identical. Relocation under
memory pressure is still not provoked. The canary stays until the direct check is
wired into `sv-gpu-server`.*

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
docs/design.md §10 stage 2 makes the app side responsible for putting a finished
frame in a window, which means an ICD `dlopen`ing `libX11.so.6` and issuing X
requests from a process it does not own. Three distinct hazards, all realised
at least once during the stage that introduced this entry (docs/design.md §0.6):

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

### R28 — The frame-list locator rests on an undocumented Windows interface
Added 2026-08-13 (`measured-gpu-side.md`, "Asking Windows where the pages are";
docs/design.md §0.14, §5.1). The guest can be asked where its own pages are, rather
than the host searching 13 GiB for them, via
`NtQuerySystemInformation(SystemSuperfetchInformation)` with
`SuperfetchPfnQuery`. That is the interface RAMMap uses and it is not in the SDK.

**What it costs in rights: nothing new.** It needs
`SeProfileSingleProcessPrivilege` and `SeDebugPrivilege`. Both are default rights
of `BUILTIN\Administrators`, both were already enabled on the account this
project uses, and both were verified present *before* any change was made. No
`LsaAddAccountRights`, no new SID, no reboot, no code-integrity setting — this is
strictly less than R25, which had to be granted, and unlike R25 it is not
reversible-with-a-cost because there is nothing to revoke.

**What it reads.** The whole machine's PFN database: for every physical frame,
what the frame is used for, the process id that owns it, and the virtual address
it is mapped at. That is a *system-wide* read — the same information RAMMap
displays — and it is available to any process on the machine holding those two
privileges, which on a single-administrator laptop is any elevated process. It
tells an attacker who already has administrator rights nothing they could not get
from `SeDebugPrivilege` alone, which the account also holds. The exposure is
therefore not new; the dependency is.

**The actual risk is availability, not confidentiality.** The interface is
undocumented, its version constant (`SUPERFETCH_INFORMATION_VERSION = 45`) is
reverse-engineered, and `MMPFN_IDENTITY`'s union layout is `phnt`'s reading of a
kernel structure. Build 26200 already returns an undocumented `UseDescription`
of 13 for large pages that no published list names. A Windows update may change
any of it, and the failure mode of a *silently shifted field* is exactly R21's:
a confident, wrong frame list that translates to valid, unrelated memory.

*Mitigation, and it is the reason §5.1 does not delete the scan:* the frame list
is cross-checked against the content scan **once, at attach**, on the single
occasion when the stamps still exist and the two locators can be compared page
for page. A mismatch is fatal to the session and loud. After that the query is
trusted, because after that there is nothing left to check it against — and the
1 ms re-check it makes possible (R26) is a stronger continuous guarantee than
the design previously had at any price.

*What this does not mitigate:* a Windows update that breaks the interface
outright makes the query fail rather than lie, which degrades the design to
`ScanLocator` — measured working, and the reason the scan stays in the codebase
rather than being replaced.

### R29 — The ivshmem arena's file is a lever on the VM
Added 2026-10-15 with `waycompose-design.md` §83. The launcher's arena is BAR2 of
an `ivshmem-plain` device whose memory backend is
`/run/windows-vm/wc-arena/arena`, mode 0660 `root:wc-arena`, in a directory mode
0750. Membership of `wc-arena` is the whole of the access control, and a member
can do three things with the file.

**Stop the VM.** A member can truncate or `fallocate --punch-hole` the file.
QEMU maps it as guest-physical memory, and the next guest access to a page that
is no longer backed is a `SIGBUS` in QEMU. The VM dies without shutting down, and
the person using it loses their session. This is the new risk, and it is a
denial of service rather than a disclosure. It has not been triggered on
purpose, for the obvious reason, so the `SIGBUS` is the documented behaviour of
a shared file mapping and not a measurement.

**Read and write every frame the guest compositor shows.** The arena holds the
pixels of every Wayland client and the compositor's keymaps and clipboard
offers. This is R5 narrowed rather than widened: the guest-RAM route gave root
the whole of guest RAM, and this gives the group 128 MiB of a dedicated region
that holds nothing else.

**Feed the guest compositor arbitrary bytes.** This is R2's case, unchanged,
since a member can already be the proxy. The compositor bounds-checks every
slice against the region.

*Mitigation:* the group has one member, `sirati`, who is in `wheel` and can
already stop the VM by other means, so for the current membership nothing is
new. The file is recreated by the VM's own unit on every start and removed when
it stops. *What this does not mitigate:* adding anyone else to `wc-arena` gives
them the ability to stop a VM a person is using. Treat membership like `wheel`.

*The guest side has the matching property.* The ivshmem driver gives the BAR to
the first handle that asks and keeps it until that handle closes, so any guest
process that can open the device can hold the mapping and keep the compositor
from starting, or read the arena while it has it. On testhost the guest has one
interactive account. §83.8 records which accounts the device actually opens for.

*Since `waycompose-design.md` §87 the group reaches two more things.* **The GPU
sessions' arena**, `/run/windows-vm/wc-arena/gpu`, 256 MiB: each session's
control region and its arena, so a member can read every host-visible buffer a
forwarded application has (command blobs, staging uploads, read-back frames) and
**write a session's rings**, which is to say send Vulkan commands to the guest's
GPU driver. That is new in kind: §86.5 recorded a fault in that driver stopping
the VM, so a member can try to make the driver fault. It is the same reach the
user running the forwarded application already has, and `sirati` is that user.
**The doorbell's socket**, `svbell.sock`: a member can send spurious wakes,
which cost a syscall each, or hold QEMU's one connection and leave every session
without a doorbell, which makes it slow and not wrong. Both are recreated by the
VM's unit and given the group by it. *Mitigation, unchanged:* one member, in
`wheel`.

### R30 — The GPU broker hands its group the guest's memory

**Resolved by `waycompose-design.md` §87, by removal.** The GPU sessions' arena
moved onto a second ivshmem device whose file the user opens with R29's group
bit; `sv-hostd ivshmem-serve` runs as the user, refuses to run as root, and
locates nothing, so nothing on the launch path holds QEMU's memory backend and
nobody is handed `/proc/<pid>/fd/<n>` for all of guest RAM. The broker was never
started on testhost. Its code stays for the developer harnesses, which keep
the guest-RAM arena and their root, and the entry below stays as the record of
what it would cost to start it.

Added 2026-10-15 with `waycompose-design.md` §86. `sv-hostd broker` is a root
daemon that the desktop launcher's `wc-session gpu` asks for GPU sessions. It
locates the guest's GPU arenas through QMP and `/proc/<qemu-pid>/fd` as root and
then runs `qemu-serve --drop-to <peer>`, so that Firefox opens its sessions as
the user rather than as root. That is the point of it, and it is also the risk:
the dropped `qemu-serve` holds QEMU's memory backend, the whole of guest RAM,
and the peer can open it through `/proc/<pid>/fd/<n>` for as long as the
process lives. **Whoever may connect to the broker's socket may read and write
the VM's memory.** That is R5's "root sees all of guest RAM" extended to the
socket's group, and it is wider than R29, whose file is 128 MiB of a dedicated
region.

*What the broker checks:* the peer's uid from `SO_PEERCRED`, never from the
request; the session directory is exactly `<runroot>/<id>` with a plain id,
a directory and not a link, owned by the peer and not writable by others; each
`cold<i>.sock` is a socket owned by the peer; at most 8 sessions; one request at
a time, 180 s to listen. *What it does not check:* that the sockets are not
swapped for something else between the check and the connect (the directory is
the peer's, so they can be), and what the guest side behind a cold socket
offers — the peer controls that too, and gets all of guest RAM anyway.

*Mitigation:* the socket is 0660 `root:<gid>` for the gid given and 0600 root's
without one; it is not running on testhost and has never run as root. *Before
it does:* the group must be one whose members could already stop or read the VM
(today, `wc-arena`'s one member, `sirati`, who is in `wheel`). A narrower design
would hand the ICD only the located runs, which needs a mapping of part of a
memfd that another process can open, and none exists without CAP_SYS_ADMIN.

### R31 — The Wayland rings' doorbell is a second port into the host
Added 2026-10-16 with `waycompose-design.md` §89. The launcher's Wayland
connections no longer cross the ssh tunnel. They are rings in the last 6 MiB of
the ivshmem arena device (R29's region, one file, one group), and a second
virtio-serial port, `org.sirati.wcbell.0`, carries one byte per wake in each
direction. Its host end is QEMU's chardev socket
`/run/windows-vm/wc-arena/wcbell.sock`, `0660 root:wc-arena` with a named ACL
for each member, made by the VM's unit exactly as §87's `svbell.sock` is.

**What the guest can do with it that it could not before: nothing.** The
argument, piece by piece, because "a new channel from the guest to the host"
is the kind of change that has to be argued rather than asserted:

* *No new listener.* QEMU's chardev socket is the listener, and only members of
  `wc-arena` can connect to it (the filesystem is its authentication, as for
  R29's files). wc-proxy, running as `sirati`, connects. Nothing listens on a
  network, a vsock port or a loopback port. The guest cannot reach any process
  on the host through the port except the one that connected to it.
* *No new data reach.* The ring bytes are written into the ivshmem region the
  guest could already write in full (R29: every frame, every keymap, and the
  compositor's half of the arena). The rings are its last 6 MiB.
* *No new parser.* What wc-proxy reads from a ring is the Wayland stream it
  already parsed when that stream came through the tunnel: the same
  `Session::server_message`, the same bounds checks. What it reads from the port
  is one byte at a time, each decoded as a slot and a lane; a byte that names
  nothing it waits on is counted as stray and ignored (`rings::BellHub`). The
  worst a hostile guest does with the port is wake a pump spuriously, which
  costs a syscall, or stay silent, which costs the pump its 20 ms timeout.
* *No new authority on the host.* ssh remains the guest's only way to cause
  anything to happen on the host: the forced command, its verbs and its
  forwards are unchanged (R33 for what the verbs admit since §94), and it is still what starts the proxy and the
  application. The rings are found through the header the compositor wrote,
  and the proxy attaches them only after the header's run id matches the one
  `up` was given over ssh, then checks the control region's protocol ids
  (R13) and every directory offset against its own layout (R2).
* *No descriptor.* The ring's write takes `&[u8]` and the doorbell's poke a
  `u8`. Neither has a parameter through which a descriptor could be named
  (§14.7, §36.5).

**What a member of `wc-arena` gains:** the ability to send the guest's
compositor spurious wakes, or to hold QEMU's one connection to the port and
leave every session's rings without a doorbell. QEMU serves one client at a
time and leaves the next in the kernel's backlog, so the proxy's connect
succeeds and is never served: its pumps fall back to their 20 ms timeout and
the compositor's to its 5 ms turn, which is slow and not wrong, and the
proxy's closing line says `0 wake(s) delivered`. Both are R29's lever over
again, smaller.

**What a guest process other than the compositor can do:** the port has one
opener. A process that opens it first keeps the compositor from having rings
(it falls back, loudly) and receives the proxy's wakes, which carry no data.
It could also write wake bytes. It can do nothing with the rings themselves
that it could not do to R29's arena already. On testhost the guest has one
interactive account.

*Mitigation, unchanged:* `wc-arena` has one member, `sirati`, who is in
`wheel`. The socket is recreated with the group by the VM's unit on every start
(once the NixOS change of §89.8 is switched); until then it exists only as
hot-added, with the same mode and ACL set by hand.

### R32 — The two doorbell ports admit the interactive user at medium integrity
Added 2026-10-16 with `waycompose-design.md` §91. R31 and §87 put two
virtio-serial ports on the guest, `org.sirati.wcbell.0` and `org.sirati.svbell.0`.
vioser makes every port a raw PDO with the descriptor
`D:P(A;;FA;;;SY)(A;;FA;;;BA)S:AI(ML;;NW;;;LW)`, which admits SYSTEM and the
Administrators group and nobody else. A shortcut started by Explorer runs with
the user's filtered token at medium integrity, so it could not open either port,
and every real double-click got the ssh fallback and no GPU doorbell (§90.3).

**The change.** `scripts/wc-port-access.ps1` gives exactly those two device
objects the descriptor

    D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;<the user's SID>)S:(ML;;NWNR;;;ME)

It keeps the two original entries, adds read and write (`0x12019f`, which has no
`WRITE_DAC`, `WRITE_OWNER` or `DELETE`) for one named account, and adds a Medium
label that refuses read and write to anything below medium integrity. The tool touches no
other port, the QEMU guest agent's included. The tool writes the descriptor in two places. The device's PnP
`Security` property is applied by the PnP manager whenever the device starts,
which covers a reboot and a vioser update. The live device object is set through
a handle, so the change takes effect without restarting a device. A SYSTEM task,
`waycompose port access`, re-applies both at every startup and finds the ports by
name, so a renumbered port still gets it. The task runs a copy of the script
under `C:\Program Files\waycompose`, which only Administrators and SYSTEM may
write; a SYSTEM task that ran a file the user can edit would give the user
SYSTEM.

**Who it admits, measured on testhost's guest (§91.2).** A process in session 1
started by the scheduler at the `Limited` run level (`S-1-16-8192`, not elevated,
the token Explorer gives a double-click) opens both ports read and write. A
Low-integrity copy of that token is refused with error 5, and so is a Low copy of
the administrator's token, so the label holds. Before the change the same medium
process was refused.

**What any medium-integrity process of that user can now do with the ports.**
Every program <guest-user> runs (a browser, a game, a downloaded tool) runs with this
token, so the argument has to cover all of them.

* *Write wake bytes to the host.* The host reads each byte as a lane number and
  treats it as a hint. R31 already argues this: a wrong byte wakes a pump that
  finds nothing to do (one syscall) or is counted as stray. The GPU doorbell is
  the same mechanism on the GPU rings (§87). Neither byte carries data, and
  neither can name a descriptor, a file or a command.
* *Read the host's wake bytes.* They carry no data. Their timing tells a reader
  roughly when the Wayland session or a GPU session is busy. The same process
  can already learn far more, see below.
* *Hold a port first.* A port has one opener (vioser makes it exclusive). A
  process that opens a port before the launcher does makes the next session fall
  back to ssh or to slice-length GPU waits, loudly and correctly. That is a
  denial of service against the user's own session, by the user's own program.
* *Nothing on the host.* The host ends are QEMU chardev sockets in
  `/run/windows-vm/wc-arena`, mode 0660 for `wc-arena` (R29, R31). The guest
  reaches only the process that connected to them, and only with bytes it
  treats as hints.

**Why this adds nothing a medium process could not already do.** The ivshmem
devices that carry the arena and the GPU sessions (R29) have the Looking Glass
driver's descriptor, measured on the same guest:

    D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1201bf;;;WD)(A;;0x1200a9;;;RC)S:AI(ML;;NW;;;LW)

Everyone (`WD`) may open them read and write, and their label is Low. Any
process of any user, Low-integrity ones included, can map the arena that holds
every frame, keymap and clipboard offer of the session, and write a GPU
session's rings. The port grant is narrower in every respect: one named
account, medium integrity and up, and a channel that carries no data. *Not
changed here, and worth a decision by the owner:* the ivshmem devices'
descriptor could get the same treatment (`wc-port-access.ps1` would need only
the device paths), which would close the Low-integrity reach to the arena.

**Alternatives that were weighed (§91.2).** A doorbell on the ivshmem device
itself (`ivshmem-doorbell`, which the Looking Glass driver supports through
`IOCTL_IVSHMEM_RING_DOORBELL` and event registration) needs the VM reconfigured
and restarted and an `ivshmem-server` on the host, a new resident component on
each side. A guest service that owns the ports and relays to user processes
over an ACL'd pipe is a new resident component, an extra hop per wake, and the
same grant expressed as a pipe ACL. Running the launcher elevated puts a UAC
prompt on every click and gives Firefox's session an administrator token.

**The way back.** `wc-port-access.ps1 -Action revert` restores the original
descriptor above on both ports and removes the PnP property;
`-Action uninstall` also removes the task and the Program Files copy. A port
some process holds open cannot be opened to change it (it is exclusive), so run
either with no session up.

### R33 — Jump lists put a command on the guest that reaches the host
Added 2026-10-16 with `waycompose-design.md` §92. **Resolved 2026-10-16 by §94**
for the gap it recorded (`run` took a command line); the argument for the whole
forced command, as it now stands, follows.

A pinned Linux application's jump list has tasks ("Open new private window")
and pages (the profile's most visited sites). Each entry is a Windows shortcut
that runs `wc-launch.exe firefox --action new-private-window` or
`wc-launch.exe firefox --url https://…`, and every entry on that list is written
by the guest. So anyone who can write the guest user's jump-list files, who can
run `wc-launch.exe` with arguments of their choice, or who can read the
launcher's key (`id_host`, readable by every process of the guest user) can ask
the VM host to do something. The claim is that what they can ask for is: open a
session, start an application **the host defines**, run one of its desktop
entry's actions, open an http(s) or `file:///` URL in it, and read that
application's name, icon and jump-list pages. Not a program, an argument, a
path, a preference or a file of their choosing.

**Until §94 that claim was false.** `wc-session run ID CMD …` carried a base64'd
command line from the guest since §28: its first word was resolved and run, the
rest were its arguments. Any holder of the key could run any program on the VM
host as `sirati`. The same verb took the profile's whole `user.js` (any
preference, the remote debugger's included) and both style sheets as guest
text; `action`, `open`, `app` and `icon` took the executable; `frequent` took a
profile path. §94.4 found one more of the same family: the guest-RAM `up`
checked a run id only when it began with `0x`, and printed it with `%s` into the
script it pipes to `$ROOTC`, so `up ID 1;cmd PAGES OUTBOUND` was a root command
injection in design. It was not exploitable on testhost only because `$ROOTC`
(`~/rootc`) no longer exists there.

**What crosses now (§94.3).** The forced command refuses the request whole
unless it is at most 16 KiB and 12 words of letters, digits, space and
`+ / = . _ -`, so no shell metacharacter, quote, glob character, newline or
non-ASCII byte reaches a verb; it splits it without pathname expansion. Each
verb takes an exact word count, and each word has a type: a session id or an
application id (a letter or digit first, so never an option; no `/`, no `..`;
at most 64), an action id, a base64'd URL, a run id (`0x` and hex), or a decimal
number in a range (DPI 48..960, caption height 0..512, GPU sessions 1..8). The
harness `crates/wc-launch/tests/wc_session.rs` runs the real script with 133
hostile requests and asserts each is refused, starts nothing, reaches no root
step and creates no file; against master's script it fails 11 of its 15 tests.

**The argv is the host's.** An application is a section of `apps.ini` beside
`wc-session`, written by the deploy step and by nothing the guest reaches: an
absolute `exec`, fixed `args`, a `profile` under `~/.wc-launch`, `prefs`,
`titlebar`, `desktop`, `gpu` and `env`. `run` executes exactly that, as an
array. `action` and `open` add what `wc-proxy desktop` builds from the
application's desktop entry on the host: it accepts an action id only if the
entry lists it (or it is the one built-in, Firefox's `new-tab`), refuses an
action whose program differs from the entry's own, and refuses any argument
that is not one word (`desktop::tests::only_the_entry_s_own_action_ids_run`).
The URL must be `http://`, `https://` or `file:///`, so it can never begin with
`-`; it must have no whitespace, control character, quote, backslash or
non-ASCII byte and be at most 4096 bytes (`desktop::tests::urls_are_checked_strictly`); it becomes one argument, in the slot the entry's own `Exec=` marks
with `%u`. `wc-proxy desktop` reads its options only before its first word, so
an id or a URL cannot be taken as `--dirs` (`desktop::tests::options_end_at_the_first_word`). The profile's `user.js`, `userChrome.css` and
GTK sheet are rendered on the host from the definition, by the launcher's own
renderer (`plan::host_files`) built for Linux; the guest's only input to them is
the two numbers.

**Root is out of reach.** The product's arena is ivshmem (R29) and needs no
root. The guest-RAM path, the only one that pipes to `$ROOTC`, is refused unless
the environment the script runs in sets `WC_LAUNCH_GUEST_RAM=1`. The guest
cannot set it: testhost's sshd has no `AcceptEnv` and no
`PermitUserEnvironment`, and the forced command sets nothing. The run id is
checked and quoted on that path anyway.

**What this does not close.**

* *The application is as powerful as its own interface.* The key now decides
  which programs start, with which arguments. It does not decide what a started
  Firefox does next, and the guest drives that Firefox: its window is on the
  guest's desktop, and any process of the guest user can send it input. Through
  Firefox's own UI that is everything Firefox can do as `sirati` on the host:
  Save As to any path the user can write (`~/.bashrc`, the installation's own
  `apps.ini`), `file:///` URLs that show any readable file, and with
  `devtools.chrome.enabled` the Browser Console, which runs privileged script
  that can start processes. So a hostile process on the guest that can also
  drive the guest's desktop can still reach code execution on the host, one
  step further than before and visibly, in a window. Closing that needs the
  application to run as a user or in a sandbox that is not `sirati`, which is a
  change to the product's shape and is recorded here, not made.
* *The installations on one guest share the run root.* Every launcher key on
  the guest can `down`, `logs` or `up` (which clears the directory) a session
  of another installation if it knows the id (`<name>-<pid>`). All of them are
  the same Windows user's.
* *`file:///` pages.* A frequent page may be a local file, as in Windows'
  Firefox, so `open` takes `file:///` URLs: the same reach as typing one into
  the address bar, which the bullet above already grants.

**The jump list's pages are history.** `wc-session frequent` reads the launcher
profile's `places.sqlite` and `favicons.sqlite`, from copies in a 0700 directory
that are removed before it exits. It sends URLs, titles and favicons to the
guest, as Windows' Firefox shows its own history in its own jump list. On the
guest they are written to `%LOCALAPPDATA%\waycompose\jumplist` (favicons, and
the URLs the user removed from the list) and to Windows' own jump-list file for
the identity. Nothing on either side logs a URL or a title. Firefox's
`browser.taskbar.lists.enabled` and `…frequent.enabled` turn the pages off.
Private windows record no history, so nothing from one can appear.

### R34 — The GPU sessions share one pool
Added 2026-10-16 with `waycompose-design.md` §97. The GPU ivshmem device was cut
into fixed slots, one per session (§87); it is now one pool, and every session
leases ranges of it as it needs them. Two applications' memory can now sit side
by side in the device, and a range one session gave back can be another's on
the next call.

**On the host nothing is wider than R29.** Every ICD maps the device's whole
file, by name, because a lease can be anywhere in it. Under §87 an ICD mapped
only its slot, but the file was already readable and writable by every process
of the group, which is to say by the user every forwarded application runs as;
a process that wanted another session's slot could open the file. A buggy ICD
can now scribble on another session's leases where it used to be confined to
its own slot by its mapping, and the cost of that is a corrupt frame or buffer
in the other application, not a reach it did not have.

**On the GPU side a session reaches only its own leases.** Every
`(memory, offset)` an application names is resolved in its session's own lease
table (`Objects::arena_slab`), and a command blob is read only when it lies in a
lease of that session; anything else is a protocol error on that session's
ring. Each lease is its own import into the session's own `VkDevice`, and a
release frees the import before the range returns to the pool, so a range is
never in two devices at once. The driver never sees another session's memory
through this session's device. *What this does not cover:* a forged offset the
lease table would accept, one inside the session's own leases, is the
session's own memory, R2's case, unchanged.

**One application can no longer take the whole device from the others, and
cannot take more than its share of it.** A session may lease three quarters of
the pool, its control region excepted, and the last `control + blob reserve`
bytes are kept for a session that has not started yet. A full pool refuses a
lease with `VK_ERROR_OUT_OF_DEVICE_MEMORY`, never by losing the device. *What
this does not cover:* two applications that each stay under the cap can still
fill the pool between them; the second to ask is refused. The pool's log line
names every session's holding at each refusal and each 32 MiB high-water step,
so who held the memory is on record.

**Size is a VM setting, not a run-time one.** The device does not grow while
the VM runs: vfio pins every byte of an ivshmem BAR (§97.3), so a device added
at run time would be host RAM taken from the host's applications, and adding
one takes QEMU's monitor, which is root's. Nothing on the launch path asks for
either. The device's size is set in the VM's configuration, beside the guest's
RAM, by whoever can change the VM.

### R35 — The shared doorbell hub
Added 2026-10-16 with `waycompose-design.md` §99. One process per user on the
VM host, `sv-hostd bell-hub` in the user unit `wc-bell-hub.service`, holds
QEMU's two doorbell sockets (`svbell.sock` and `wcbell.sock`, R29's group or
ACL) and serves them on `/run/user/<uid>/wc-bell/{sv,wl}.sock`: each socket
0600 in a directory the hub makes 0700, so only this user's processes reach
it. It runs as the user and refuses to run as root; it carries bytes and
nothing else, so no descriptor and no memory crosses through it.

**What a client can do.** Send bytes toward the guest, which wake rings in
the guest's GPU process or compositor: spurious wakes, at one syscall each,
which R31 already prices. Read every byte from the guest, which says which
ring lanes were written and when, the same timing information R31 records
for the port itself. Fail to read: its own queue fills and its own bytes are
dropped, and no other client is affected.

**What it changes.** Before §99 the first session to start held the port and
any later session was silently without bells; a crashed session's leftover
`sv-hostd` could hang the next session's application (§97.10). Now no session
can hold the port. *What this does not cover:* a process of this user that
connects to QEMU's socket directly, before the hub does, holds the port as
before; nothing but the hub should, and every installation's `wc-session`
since §99 does not.

### R36 — The guest can start a login screen and a desktop session on the VM host
Added 2026-10-17 with `waycompose-design.md` §102. The installation `wclinux` (VM host `~/wc-launch-linux`)
defines one application, `linux-desktop`, whose `exec` is nix-config's
`atp-nested` with fixed arguments (`login 1280x800 guest`). What reaches the
VM host from the guest is still only §94's: an application id, a session id,
two numbers. What the definition does with them is new, because it is the
first application whose start goes through root.

**The path to root, and its width.** `atp-nested` (as `sirati`, the session's
user) runs `sudo -n atp-nested-seat STYLE SOCKET W H PULSE guest`, the one
command the module's sudo rule allows `sirati` without a password. Nothing the
guest sends is in that line: the style and size come from `apps.ini`, the two
socket paths from the session's directory (`/tmp/wc-launch/<id>/wl-proxy` and
`.../xdg/pulse/native`), whose id `wc-session` has already reduced to
`[A-Za-z0-9_-]`. `atp-nested-seat` checks every argument itself, as if it came
from anywhere:

* the style and mode against fixed words, the size against digits and a range;
* each socket path (`safe_socket`): absolute; only `[A-Za-z0-9/._-]`, no `//`,
  `.` or `..` component, at most 200 bytes; equal to its own `realpath -e`, so
  no link anywhere along it; a socket owned by `$SUDO_UID`; and every directory
  from its parent up to `/` owned by root or `$SUDO_UID` and writable by
  nobody else, except root's sticky directories (`/tmp`). So nobody but the
  caller or root can make the name point at something else between the check
  and its use, and the name is safe to write into greetd's config and the
  session files (no quote, space or newline can be in it).

Until §102 the rule was "a socket in `/run/user/$SUDO_UID`". The new rule
admits the waycompose session's directory and no less safe place; it was
tested with a socket of the caller inside another user's world-writable
directory and with a `;` in the name (both refused, and nothing loaded).

**What root then does.** Loads `vkms`, starts greetd on the nested seat with
the patched greeter as `cosmic-greeter`, and on a login the chosen session as
`<desktop-user>`; root never opens the sockets. In `guest` mode the "Start Windows"
button is gone and the seat ends with the session. A one-shot autologin exists
for tests only, and only through a file in `/run/atp-nested-autologin` that root
must create (sudokey); it adds no way in.

**What a guest that holds the launcher's key can do with it**, then:
* show <desktop-user>'s login screen in a window on the guest, and start it again
  after it ends. It cannot log in without <desktop-user>'s password (face login is
  off: the host has no camera; both USB controllers are the guest's).
  Repeated wrong passwords are PAM's to throttle, as at any login screen.
* use memory and CPU on the VM host: the nested desktop is 1–2 GB and runs on
  the host's two CPUs (14–15), which `user.slice` is confined to so it cannot
  take the VM's. `nestedTest.memoryMax = "2G"` caps the <desktop-user> and greeter
  slices while a nested session runs, so the host's last memory stays the
  host's. One nested session at a time; a second start is refused.

**What the nested session's users can reach.** The launcher gives `<desktop-user>`
and `cosmic-greeter` ACL entries for exactly the session's Wayland socket, its
PulseAudio socket, and search (`x`) on the directories on the way that are
`sirati`'s and closed to others (`/tmp/wc-launch/<id>`, its `xdg`, `xdg/pulse`);
they are taken back when the seat ends (and the directory goes with the session).
Through that socket the nested KWin is a client of `wc-proxy` like any
application: it can draw windows and read the input given to them. The proxy
names the two windows from `wc-foreign-app-ids`, a file `sirati` writes in the
session directory and the proxy reads only if it is its own and writable by
nobody else (§102.4); a forged one could only choose a window's taskbar
button and icon, which a client can do by itself with `set_app_id`.

**The reverse direction.** `sirati` is the compositor of <desktop-user>'s session
here: it sees what <desktop-user> types into that window and could inject input,
exactly as the owner's GNOME does on laptop, where this module came from.
That is the design of a nested session and is not new with §102; it is why the
sudo rule names only the owner.

**The GPU environment** (`nestedTest.gpuForwarding`, off on testhost): if
on, the seat takes `NAME=value` words after the mode, only from a fixed list,
each checked for its kind (store paths for libraries and manifests,
`safe_socket` for `SV_SOCKET` and `SV_BELL`, fixed values for the rest), and
writes them as plasmashell's environment; anything else is refused.

### R37 — No ssh: the shared-memory device is the boundary, and a key signs the boot
Added 2026-10-19 with `waycompose-design.md` §105. Until §105 the guest's
only authority on the VM host was an ssh key in the installation's directory,
restricted by `authorized_keys` to `wc-session` as a forced command (R33).
That key started every session, carried every request and held the tunnel
the sound, the feeds and the GPU's cold path went through. §105 removes ssh
from the session entirely. What replaced each part of it:

* **Who may start a session.** The compositor writes a boot record into the
  ivshmem header page (`wc_handle::macboot`, at byte 1024) and an
  authenticator beside it (byte 1792): the installation's name, a sequence
  number and HMAC-SHA-256 over every field of the record under the
  installation's key. The host's session service (`wc-arena-host sessiond
  --ivshmem`, user unit `wc-sessiond.service`, as the user, refusing root)
  acts on a record only if the installation is registered
  (`~/.config/waycompose/installs/<name>` → its directory), the MAC verifies
  under `<dir>/session.key` (this user's, mode 0600, or refused), and the
  sequence number is above every one accepted for that installation before
  (`~/.local/state/waycompose/sessiond/<name>.seq`). The key is 32 random
  bytes made on the host at the first deploy and kept; it exists in two places,
  that file and `session.key` beside the launcher in the guest user's profile,
  whose ACL deploy reduces to that one user reading it.
* **What a session may ask.** Unchanged in substance. Requests are lines on
  the session's control ring (`OWNER_CONTROL`); the proxy runs each through
  that installation's `wc-session` with the words as `SSH_ORIGINAL_COMMAND`,
  so §94's validation is the same code (R33): application ids, an action id or
  a URL, two numbers. The proxy refuses any word outside `[A-Za-z0-9 +/=._-]`
  before `wc-session` sees it and puts the session's own id after the verb,
  so a guest cannot name another session. `app`, `frequent` and `icon` take
  no session, as on ssh.
* **How the guest reaches the host at all.** Only through the ivshmem device
  and the two virtio-serial ports R29, R31 and R35 describe. Nothing listens
  on a network. The guest has no ssh key for the host (deploy deletes
  `id_host`), and the host has no `authorized_keys` line for the installation
  (deploy removes it). The guest's loopback TCP ports (the relay's, the
  sound's, the GPU process's) are gone.

**What the guest can do that it could not before.** Start a session without
an ssh key, if it has the installation's key. That is the same population:
the ssh key sat in the same directory with the same reader. A process of the
guest user can read either and is the user. A process of another guest user
can read neither, and the device alone does not let it in: an unsigned or
mis-signed record is refused with a reason written back into the page.

**What a process that can map the device can do.** The Windows driver gives
one mapping per device, and while a session runs the compositor holds it, so
no other guest process can touch that session's rings or header. With no
session up, such a process can write a boot record, which is refused without
the key, or replay one it read earlier, which is refused by the sequence
number; a record whose fields were changed fails the MAC. It cannot forge a
session, and a refused record costs the host one page read and a log line.

**What the host's service trusts.** Every field of the record is untrusted
input: the session id is `wc-session`'s alphabet, the installation name
`[A-Za-z0-9_-]` up to 32, looked up only as a direct entry of the registry,
and the record must match the header the device holds (magic and run id) and
read the same twice. It answers only in the page it read, after checking that
page still holds the same record.

**What this does not protect.** A guest process running as the user can read
`session.key` and start sessions, as it could use the ssh key before. The key
is not rotated; redeploying keeps it, and deleting `<dir>/session.key` on the
host makes the next deploy issue a new one. A member of `wc-arena` on the host
can write the device file, so it can write a boot record too, but it can only
ask this user's service to start this user's installations, with nothing it
could not already run itself. Deployment itself still uses ssh from the
developer's workstation to both machines (an administrator's channel, not the
session's): that is how the key crosses, once.

**The debug transport.** `debug-ssh-transport = on` in `wc-launch.ini`,
deployed with `WC_LAUNCH_DEBUG_SSH_TRANSPORT=1`, brings back the pre-§105
path, key and forced command included. It is off by default, says
`DEBUG SSH TRANSPORT` three times in the session's log, and the harness fails
a run whose log says it.
