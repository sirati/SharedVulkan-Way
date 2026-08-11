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
symmetry.
