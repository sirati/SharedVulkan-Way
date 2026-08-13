# One binary per side: the Wayland compositor inside the Vulkan program

**Status: design, plus the first two pieces built. Written 2026-08-13.**

The brief, in the user's words, twice:

> the windows wayland impl is supposed to be inside the shared vulkan program
> behind a compilation flag, and supposed to use all of its primitives. given
> that a lot of window painting happens directly via vulkan or opengl both shared
> vulkan and the wayland need to know what each other do and e.g. share handles.

> as i said the wayland and the shared vulkan are supposed to be one binary!

and the requirement that gives it its point:

> we want direct memory mapping but we do want to preserve that we allocate
> direct gpu memory if we want to, and that there is no copying between the two

This document is the design. §1 is what the split costs, measured rather than
asserted, because that is what decides the order of the work. §2 is the crate
layout and the feature gating. §3–§5 are the arena, the handle and the ownership
rule that replaces the copy. §6 is what one arena costs — there is a real loss
and it is named. §7 is the boundary rule, which gets *harder* under unification.
§8 is when the physical merge should happen and what has to be true first.

---

## 0. Status in one paragraph

Two things are built and are in this worktree: `sv_arena::handle`, which is
`firefox-accel.md` §7's shared handle as a type that cannot represent a
descriptor, and `sv_arena::shared`, which is one allocator serving several
consumers with disjoint reserved windows. Together they are the part of the
unification that makes shared handles *possible*, and they were chosen first
because they are purely additive — they relocate no file, so none of the branches
in flight (§8) becomes harder to merge. Two facts were measured that the split
had made nobody check: **the whole arena stack already cross-compiles clean for
`x86_64-pc-windows-gnu`**, and so does `sv-gpu-server`. There is no technical
obstacle to the unified guest binary; the obstacle was only ever that the source
lives in two repositories. Nothing here has been run on the guest, no frame has
yet travelled by handle, and §8 says plainly what remains.

---

## 1. What the split costs, measured

Five consequences, in descending order of how much they cost. The first is the
one the requirement is about and it is worse than the earlier documents say.

### 1.1 The frame crosses the VM boundary twice, in opposite directions

`firefox-accel.md` §7 counted two copies and called the second "the one that
should not exist". That count was made from shared_vulkan's side alone, and it is
an undercount, because the compositor's side has copies of its own. The full path
a Firefox frame takes today, on the testhost topology:

| # | step | where | crosses? |
|---|---|---|---|
| — | WebRender renders into a `VkImage` | **guest** GPU memory | — |
| 1 | `vkCmdCopyImageToBuffer` into a linear arena buffer (§0.6) | **guest** → shared_vulkan's arena, which is guest RAM | no |
| 2 | `sv-icd` reads the arena and writes Firefox's `wl_shm` pool | arena → **host** RAM | **out** |
| 3 | `wc-proxy` copies the client's pool into waycompose's arena (§9.5) | host RAM → **guest** RAM | **back in** |
| 4 | `wc-compositor` blits the arena slice into the Win32 window | **guest** | no |

**Four copies, and the pixels leave the guest and come back.** They are rendered
by the guest's GPU, into the guest's own RAM, to be displayed by a compositor
running in the guest — and on the way they make a round trip to a host that has
no GPU at all (`design.md` §0.10) and does nothing to them.

Steps 2 and 3 exist for exactly one reason: **shared_vulkan's arena and
waycompose's arena are two different regions, and neither program can name a
range of the other's.** The client's `wl_shm` pool in the middle is the only
place both could meet.

With one arena and one guest process the path is:

| # | step | where | crosses? |
|---|---|---|---|
| — | WebRender renders into a `VkImage` | **guest** GPU memory | — |
| 1 | `vkCmdCopyImageToBuffer` into a linear arena buffer | **guest** → the arena | no |
| 2 | the compositor blits that range into the window | **guest** | no |

**Two copies, zero crossings.** Copy 1 stays and should: the frame is rendered in
tiled GPU memory and the compositor needs it linear, which is a property of the
hardware and not of our architecture. Copy 2 is a composite into a window and is
what a compositor *is*. What disappears is the pair that existed only because two
programs could not name the same memory.

This is the whole of "there is no copying between the two". It is not one memcpy
saved; it is two full-frame transits of a hypervisor boundary, at 4.6 MB a frame
for Firefox's content subsurface (waycompose §15.10).

### 1.2 The allocator was written three times

Not twice. Over what the topology wants to be one region there are three free
lists:

| where | allocates | note |
|---|---|---|
| `sv_arena::alloc::ArenaAllocator` | `VkDeviceMemory` sub-ranges | the original |
| `wc_proxy::pool::Alloc` | client pools copied into the arena | **mirrors `sv_arena::GRANULARITY = 4096` by hand rather than depending on it** |
| `wc_handle::arena`'s outbound window | compositor-minted objects | in the guest, where the other two are not |

The middle one is the tell. Its own comment records that it copies the constant
by hand, which means somebody stood at the boundary, saw the shared value, and
could not reach it. §3 replaces all three.

### 1.3 The locator was reached for instead of the allocator

waycompose consumes `sv_hostd::Hostd::qemu` and `sv_hostd::locate::attach` — root,
QMP, `/proc/<qemu-pid>/fd`, a 13 GiB content sweep at 2.37–2.42 s (§12.5) — and
of `sv-arena` it uses exactly one item, `extent::RunList`. It reached for the
*locator* because that is the part a separate program can use: a locate produces
a mapping, which is a thing you can hand across a process boundary. An
*allocation* is a position in a free list, which is not.

So the deployment acquired root, a second 13 GiB sweep and a second set of
stamps-that-pixels-later-overwrite, in order to get a region it then allocated
from with a different allocator. One arena means one locate.

### 1.4 `wc-arena-host` is a workaround for a split that should not exist

`crates/wc-arena-host` has `sv-hostd`, `sv-transport`, `sv-arena` and
`arena-record` as **absolute** path dependencies on a sibling repository, declares
its own `[workspace]`, carries its own `Cargo.lock`, and is in the parent
workspace's `exclude` list. waycompose §12.3 argues the exclusion correctly for
the world it was written in: a member is resolved by
`cargo clippy --workspace --target x86_64-pc-windows-gnu`, which is the gate the
project exists to keep green, and it would fail on any machine without the
sibling checked out.

**Two measurements say the argument no longer bites.**

- `sv-hostd` already `cfg(target_os = "linux")`-gates `locate`, `qemu` and
  `Hostd` itself, so on Windows it compiles to a shell. Its own comment says the
  split "is checked by `cargo check --target x86_64-pc-windows-gnu`, which is why
  it is a `cfg` and not a comment."
- The rest of the stack cross-compiles clean, measured today in `nix develop`:

  ```
  $ cargo check -p sv-arena -p arena-record -p sv-transport -p sv-wire \
        --target x86_64-pc-windows-gnu
  Finished `dev` profile
  ```

So the Windows gate was never the real constraint. **Source reachability was** —
i.e. the repository split itself. The exclusion is a workaround for the thing
this document removes, and it goes away with it.

There is a live hazard in it worth fixing regardless: the path points at
`/mnt/datasets/builds/shared-vulkan-base`, the *main* checkout, so anybody
working in an `sv-worktrees/<name>` worktree builds waycompose against master
silently. Today the four crates happen to be identical, so nothing has broken
yet.

### 1.5 The same latent bug was found twice, at full price

`sv-gpu-server --clients N` served one shared session; `wc-proxy` served one
connection. Both broke on Firefox, which opens several processes and several
connections. Two codebases, one blind spot, diagnosed and fixed twice —
`firefox-accel.md` §4 and waycompose §15.8 are the same paragraph written by two
people who could not see each other's tree.

Both fixes converged on the same shape (`Arc` the shared thing, per-consumer the
rest), which is the strongest possible argument that it should have been one
piece of code. §3 is that piece.

---

## 2. The shape: two binaries, one workspace, features for composition

### 2.1 The two binaries

| binary | side | is | contains |
|---|---|---|---|
| **`sv-guest`** | inside the VM (Windows) | the GPU server **and** the compositor | ring consumers, object table, `SlabImport`, the arena provider — plus smithay, the Win32 window, the seat, the surfaces |
| **`sv-host`** | outside the VM (Linux) | the arena daemon **and** the Wayland proxy | QMP, the locate, the stitch, GPA translation — plus the AF_UNIX listener, the wire forwarder, the pool transport |

Plus `sv-icd.so`, which is unchanged and is not a third binary: it is a cdylib
the application loads, and it lives on the host side inside the application's own
process.

The names are deliberate. The old ones (`sv-gpu-server`, `wc-compositor`) each
name half of what the binary now does, and keeping either would make the other
half look bolted on.

### 2.2 Crate layout

Nothing is rewritten. The existing crates keep their names, their contents and
their tests; what changes is that two of them become libraries with a thin binary
on top, and two new binary crates compose them.

```
crates/
  # shared, and the reason there is a "shared"
  sv-wire  sv-transport  sv-arena  arena-record     ← all four cross-compile today
  sv-proto  sv-icd-entry  sv-gpu-entry  vk-registry  sv-codegen
  sv-testkit

  # the guest side's two halves, each a lib
  sv-gpu-server   lib + bin   ← bin becomes a thin `main` calling `run()`
  wc-compositor   lib + bin   ← same
  wc-server  wc-handle  wc-transport  wc-desk
  vendor/smithay  vendor/wayland-*  vendor/xkbcommon

  # the host side's two halves, each a lib
  sv-hostd        lib + bin   ← already a lib; the bin becomes thin
  wc-proxy        lib + bin   ← same

  # new: the composition points, and the only new code the merge needs
  sv-guest        bin         ← features: gpu, wayland
  sv-host         bin         ← features: gpu, wayland

  # deleted by the merge
  wc-arena-host               ← §1.4; its one test moves to sv-arena
```

`wc-arena-host` is the only crate the merge deletes. Its single test —
`the_two_projects_agree_on_the_stamp`, which compares `wc_handle::arena`'s
hand-copied twenty bytes against `arena_record`'s — becomes unnecessary the
moment `wc-handle` can depend on `arena-record`, which §1.4 measured that it can.
A test asserting that two definitions agree is worth exactly as much as the
second definition, and the second definition is what goes.

### 2.3 The features, and why they are features here and `cfg` everywhere else

The user asked for the Wayland half to be "behind a compilation flag" and that is
a requirement, not a suggestion. Neither workspace has a single `[features]`
table today, and both argue in prose for `cfg(target_os)` instead. That argument
is right and this does not contradict it, because the two mechanisms are
answering different questions:

- **`cfg(target_os)` is for platform.** Which OS is this, what can it call. It is
  not a choice; the target decides. It stays exactly as it is.
- **A cargo feature is for composition.** Which halves of the program am I
  building. It *is* a choice, it is the user's, and there is no `cfg` that
  expresses it.

Three features, on `sv-guest` and `sv-host` only:

| feature | pulls in | gives you |
|---|---|---|
| `gpu` | `sv-gpu-server` / `sv-hostd` | today's Vulkan-only binaries |
| `wayland` | `wc-compositor` / `wc-proxy` | today's Wayland-only binaries |
| `default = ["gpu", "wayland"]` | both | the unified binary, and the only one where a frame can travel by handle |

So `--no-default-features --features gpu` is exactly `sv-gpu-server` as it stands
today, and `--no-default-features --features wayland` is exactly
`wc-compositor.exe`. That matters for more than tidiness: it is what lets the
Windows cross gate keep testing the Wayland half on a machine with no Vulkan
driver, and it is what keeps every existing harness script working unchanged.

**The features gate composition, never a primitive.** `sv-arena`, `sv-transport`,
`sv-wire` and `arena-record` have no features and must not acquire any: they are
the primitives both halves use, a feature on one of them would make "the arena"
mean two different things depending on who built it, and unifying the arena is
the entire point.

**The no-copy path requires `default`.** A frame travelling by handle needs the
renderer and the presenter in one process; with `--features gpu` alone there is
no presenter and the swapchain falls back to today's blit. That is a real
constraint and it is stated here rather than discovered: the feature flags buy
build configurations, not four working topologies.

---

## 3. One arena, one allocator — built

`sv_arena::shared`, in this worktree, 10 tests.

### 3.1 The rule

One region. One `ArenaAllocator` behind one `Mutex`, inside an `Arc`. Consumers
join by `SharedArena::share()`. **Consumers are separated by reservation, not by
lock scope.**

A `Window` is a page-granular range cut out of the arena and *removed from the
free list*, so the general allocator can never hand out a byte of it. Three
roles, named for who **writes** them, because that is the only thing that makes
a disjointness argument mean anything:

| role | written by | read by | what it carries |
|---|---|---|---|
| `Present` | the GPU side | the compositor | `svPresentBlit`'s destination — §4's whole subject |
| `Inbound` | the host side | the compositor | pixels copied in from an unmodified `wl_shm` client (§5.2) |
| `Outbound` | the compositor | the host side | keymaps and other server-minted objects (waycompose §14.8) |

### 3.2 Two properties the type enforces rather than documents

**A window cannot be cut once anybody is sharing.** `reserve` takes `&mut self`
and reaches the inner state through `Arc::get_mut`, which returns `None` the
moment a second reference exists. waycompose §17.2 found this shape for
`reserve_outbound` and stated the reason exactly: *"a window cannot be moved
under a session that already holds the arena, and that is checked at the only
moment it could go wrong rather than asserted in a doc comment."* This is the
same trick, generalised to more than one window and moved into the crate that
owns the region.

**A window's offset is discovered, not chosen.** `reserve` asks the allocator for
the bytes and reports where they turned out to be, rather than picking an offset
and assuming the free list agreed. The first version of this function did pick,
and it was wrong within a minute of being tested: the allocator is arena-wide
first-fit, so a window "at the top" was handed the bottom. A window the allocator
can still hand out is worse than no window, so the discovered form is the only
safe one. A failed reservation gives its pieces back rather than leaking them.

### 3.3 The lock is not on the frame path

`alloc` takes the mutex for the free-list edit and drops it before returning.
**Nothing holds it across a copy.** waycompose §17.2 measured a commit at 414 µs
of `memcpy` and said plainly that four connections holding one mutex across that
would serialise every client's frames against every other's. That cost is not
paid here, and the reason is structural: the mutex protects a free list, and a
free list edit is a `HashMap` lookup and a couple of splices.

### 3.4 The join

```rust
pub fn slice_of(&self, a: Allocation, arena: u32) -> handle::Slice
```

An `Allocation` is what the allocator produced; a `Slice` is what the far side is
told; **they are the same bytes, because one allocator produced both.** With two
allocators this function could not have existed, and copy 2 of §1.1 is what stood
in for it.

---

## 4. The handle — built

`sv_arena::handle`, in this worktree, 11 tests. This is `firefox-accel.md` §7's
answer as a type.

```rust
pub struct Slice  { arena: u32, slab: u32, offset_in_slab: u64, len: u64 }
pub struct FrameHandle {
    slice: Slice, width: u32, height: u32, stride: u32, format: Format, seq: u64,
}
```

Four decisions worth stating:

**The slab is part of the name.** Not folded into a flat offset, because a slab
is the import unit on the GPU side (D4): one `VkDeviceMemory` lives inside
exactly one slab, and a handle spanning two would name something the driver never
imported. `Slice::validate` refuses that case by name.

**No base address, on either side.** §1's invariant is that the two sides map at
different bases and offsets are the only currency. `arena_offset` takes the slab
size as an argument rather than storing it, because the slab size is the control
header's `arena_slab_size` and belongs to the session, not the frame.

**The format discriminants are `wl_shm`'s own.** `Argb8888 = 0`, `Xrgb8888 = 1`.
A compositor handed a handle does no translation at all — the number in the
handle is the number it passes to `wl_shm_pool.create_buffer`. A translation
table is a place for two sides to disagree, and having no second table to drift
is what one binary is for. Unknown formats are refused, not defaulted.

**The wire form is `[u32; 12]`.** Which is precisely the argument list of a
Wayland request built from `uint` arguments, and also what `sv_wire` carries, so
the ring path and the protocol path move identical bytes and a round-trip test
through one is a proof about the other. §7 is why the form matters as much as the
content.

### 4.1 How a Vulkan allocation becomes a `wl_buffer` without a copy

The mechanism, end to end, in the unified guest binary:

1. The swapchain's present target is allocated from the **one** arena, in the
   `Present` window: `shared.alloc(stride * height)`.
2. `vkQueuePresentKHR` records `vkCmdCopyImageToBuffer` into it. This is copy 1
   of §1.1 and it stays — tiled to linear is the hardware's requirement.
3. The ICD builds a `FrameHandle` from that allocation:
   `shared.slice_of(alloc, 0)` plus the layout it already knows, plus the next
   `seq`.
4. The handle travels as twelve `uint`s. In the unified topology it never
   crosses the VM boundary at all — both ends are in the guest — but it is
   *encoded* as though it did, and §7 says why that is not ceremony.
5. The compositor validates it (`FrameHandle::validate`, plus
   `check_in_window(Role::Present, …)`), then binds a `wl_buffer` to that arena
   range and composites it.

No `wl_shm_pool` is created, no `memfd` exists, and no descriptor is minted.
waycompose §9.5 predicted exactly this and could not build it: *"a genuinely
arena-backed pool is reachable only by a client that allocates from the arena —
which is what `goal-firefox.md`'s accelerated path is, where WebRender renders
through shared_vulkan into arena memory and no `wl_shm` pool exists at all."*
That sentence is a description of one program, written from inside the other one.

**The protocol object.** The compositor advertises a global — call it
`sv_arena_buffer_v1` — with one request whose signature is twelve `uint`s and
which returns a `wl_buffer`. It has no `h` argument. §7.

---

## 5. Ownership: what the copy was silently providing

This is the part of removing a copy that is easy to miss, so it is written before
anything is removed.

**A copy is a synchronisation primitive.** Today the compositor owns its own
bytes: once `wc-proxy` has copied a client's pool into the arena, the client may
scribble on its pool freely, because the compositor is looking at a different
copy. Delete the copy and that stops being true — the renderer and the presenter
are now looking at the same pages, and the renderer overwriting a frame that is
mid-composite produces tearing at best.

So the handle carries `seq`, and the rule is:

1. The renderer allocates **at least two** present buffers and cycles them. One
   is not enough for the same reason one is not enough for any swapchain.
2. A buffer named by `seq = n` is owned by the presenter from the moment the
   handle is sent until it releases `n`. The release is `wl_buffer.release`,
   which the protocol already has and which clients already honour — this is not
   a new mechanism, it is the existing one finally meaning something.
3. The renderer may not write a buffer it has not had released. In one process
   this is checkable directly, and it should be checked rather than assumed:
   `debug_assert` is not enough, because the failure is a visual artefact on
   somebody else's laptop and not a panic.
4. `seq` is monotonic per surface. A presenter that receives `n` while holding
   `n-1` releases `n-1` — an unreleased older frame is a dropped frame, which is
   correct, and not an error.

**What this does not need:** a fence, a timeline semaphore, or anything crossing
the boundary. Both parties are in the guest and in one address space. The
Vulkan-side wait that guarantees copy 1 has *completed* before the handle is sent
is an ordinary `vkQueueWaitIdle` or timeline wait on the present queue, which
§0.6's swapchain already does.

**What could go wrong and is not yet answered:** whether the compositor's
composite of an arena slice is fast enough to hold the release for less than a
frame, and what happens to the release path when the guest is under the memory
pressure waycompose §15.11 describes (nothing is ever freed; Firefox creates 88
pools in 40 seconds and destroys none). Neither is measured. §8 puts both in the
first stage after the merge.

---

## 6. What one arena costs, argued rather than assumed

`firefox-accel.md` rejected a shared arena for the GPU side and gave a reason
that was correct: *"the descriptor names the whole object"*, so a partitioned
arena would hand every client an fd it could map in full, and one client not
reading another's buffers would be a convention rather than a fact. It chose
`N × --arena` bytes instead — **2 GiB at `--clients 8 --arena 256M`**.

Under unification that trade changes, in both directions, and both are stated.

**What is gained.** The 2 GiB, on a machine where guest RAM is the scarce
resource and the arena is guest RAM (D1). And §1.1's two boundary crossings.

**What is lost, honestly.** On the **guest** side, nothing: one process owns the
region, the consumers are threads, and there is no descriptor to hand anybody, so
the old argument has no purchase. On the **host** side the loss is real. `sv-hostd`
publishes the *name* of an object and each ICD opens it for itself (§1a's
supersession note), and a name names the whole object — `/dev/shm` has no way to
name a sub-range. So with one arena, an ICD that wanted to read another client's
pixels could map the region and do it.

Three things make that acceptable, and the third is the only one that is a
mechanism rather than a mitigation:

1. `security-model.md` already puts both sides in one trust domain, and every
   application served by one `sv-hostd` is already running as one user on one
   machine.
2. It was already true for the control region, which every ICD maps in full and
   which contains every session's rings.
3. **The allocator makes accidents impossible even though it does not make malice
   impossible.** Disjointness of what any consumer is *handed* is enforced by one
   free list; `check_in_window` refuses a described range outside the window it
   claims; and waycompose §17.3's `peer_slice` narrowing is the same refusal from
   the consumer's end. The realistic failure — an off-by-one that hands one
   client's frame to another — is caught. The unrealistic one is not.

**This is a weakening and it is written down as one.** `security-model.md` R3's
argument should be revised when the merge lands rather than left to be read as
still holding. If the isolation is ever wanted back, the mechanism is a
per-client `memfd` sealed to a sub-range handed out by the daemon — which needs a
descriptor to travel host-to-host only, is legal under §1a, and is not built.

---

## 7. The boundary rule gets harder, not easier

`design.md` §1a: no file descriptor and no handle ever crosses. The one
quarantined exception is `sv-transport/src/localfd.rs`, documented as something
that should not exist.

**Unification makes violating this rule easier, and that has to be answered
structurally.** Today the compositor and the GPU server cannot pass each other a
pointer, because they are separate programs in separate machines; the transport
would have to grow a way to carry one and there is nowhere to put it. In one
process a `VkImage`'s device memory and the compositor's surface are reachable
from one address space, and passing a raw pointer would simply *work* — on the
development machine. It would be unrepresentable on the deployment, which §1a
calls "a same-machine path wearing the design's clothes", and it would be
discovered at the moment the design was finally run for real.

Three structural defences, in the order they bite:

**1. The type cannot hold one.** `FrameHandle` has no variant, no `Box<dyn …>`,
no pointer, no `RawFd`, no `HANDLE`. Every field is an integer. This is not
"does not"; it is "cannot", and the test
`the_wire_form_is_integers_and_therefore_cannot_carry_a_descriptor` asserts that
the whole value survives a round trip through `[u32; 12]` — so a future field
that is *not* an integer fails that test at the moment it is added.

**2. The protocol message has nowhere to put one.** `sv_arena_buffer_v1`'s
request is declared with twelve `uint` arguments and no `h`. wayland-scanner
generates from that signature, so the generated function has no descriptor
parameter. This is exactly waycompose §14.7's argument — its upstream writer uses
`write_some(...)`, a plain `write` with no fd argument, so the count on the hop
that crosses "is not merely zero, it is unrepresentable" — moved from the writer
to the message.

**3. The handle is mandatory even where a pointer would be legal.** In the
unified guest binary both ends are in the guest and a pointer would be correct.
It is still forbidden, and this is the defence that costs something and is
therefore the one most likely to be argued away later. The reason to keep it:
the in-guest path and the cross-boundary path must be the *same* code, or the
cross-boundary path is only exercised by whichever harness still runs it, and
waycompose §14.9 has already recorded two occasions where "implemented" meant
"compiled" because the only path that ran was the other one.

**Where the counting standard applies.** waycompose §14.7 argues its `3 fds in,
1 out` at the point the number appears, and that is the standard. The merged
tree's harnesses must keep reporting that line, and the number must be argued
where it is printed. Under unification one input to it changes and should be
predicted here rather than explained afterwards: `wc-proxy`'s keymap `memfd` — the
one manufactured descriptor — is still manufactured, still host-to-host, and
still not the boundary. What *should* fall to zero is the count of descriptors on
the accelerated frame path, which is already zero and must be shown to stay zero
when the path stops going through a `wl_shm` pool.

---

## 8. When the physical merge should happen

### 8.1 The constraint

Ten agents are working right now, most on `waycompose` branches: the
interactive-desktop defects, a launcher, the arena allocator, decorations and
fullscreen, clipboard, cursor, touch, a small-protocols bundle, a
direct-page-location study, and a Firefox-on-passthrough integration. A tree
relocation underneath them makes every one of those branches unmergeable, and
"we lost a day of nine agents' work to a repo move" is not an acceptable
outcome.

At the time of writing, `waycompose` master has moved from `08f66b8` to `e7ed512`
— stage 13 has landed — so the queue is draining. `shared-vulkan` master is
`9d23dc1` with four worktrees open against it (`dax-locator`,
`firefox-passthrough`, `video-wire`, `video-decode`).

### 8.2 The order, and why it is this order

**Stage A — the primitives, additive. Done, in this worktree.** `sv_arena::handle`
and `sv_arena::shared`. Two new files and three lines in `lib.rs`; no file moves,
no crate is renamed, no existing signature changes. A branch that touches
`sv-arena` conflicts on three lines of a module list, which is a merge a human
does not notice. This is why it went first: it delivers the part of the
requirement that is about capability rather than layout, and it is the only part
that is safe to do while ten branches are open.

**Stage B — adoption, still additive, still two repositories.** waycompose's
`wc_proxy::pool::Alloc` and `wc_handle::arena`'s outbound allocator are replaced
by `sv_arena::shared`, and `wc_handle::arena`'s hand-copied stamp constants by
`arena-record`. This can be done today through the existing `wc-arena-host` path
dependency — §1.4 measured that the crates cross-compile, so the Windows gate
survives it — and it is worth doing before the move rather than after, because it
converts the merge from "relocate two trees and reconcile three allocators" into
"relocate two trees". **It requires a commit in the waycompose repository, which
this worktree cannot make.** See §8.4.

**Stage C — the physical merge.** `waycompose`'s `crates/`, `vendor/`, `docs/`
and `scripts/` move into this tree; the two workspaces become one; `sv-guest` and
`sv-host` appear; `wc-arena-host` is deleted; the feature tables of §2.3 are
added. Mechanically this is `git subtree`-shaped and is perhaps a day, most of it
in the flake and the harness scripts rather than in Rust.

**Stage D — the frame travels by handle.** `sv_arena_buffer_v1`, the swapchain
allocating from the `Present` window, the release protocol of §5, and the
measurement that decides whether any of it was worth it.

### 8.3 When stage C should happen

**Not now, and the trigger is a state rather than a date: when no `waycompose`
branch is open that touches `crates/` or `vendor/`.** Concretely, when the
worktrees at `08f66b8` — stage14-launcher, stage15-input, stage16-arena-alloc,
stage17-titlebar, proto-18 through proto-21, stage12-arena-firefox — have merged
to master or been abandoned. That is nine branches; one (stage13) has already
landed, which is a rate worth extrapolating from rather than a promise.

Two of them deserve a specific note:

- **`stage16-arena-alloc`** is the arena allocator, and it is the one branch that
  will conflict with stage B rather than merely with the move. It should land
  *before* stage B is attempted, and stage B should then be written against what
  it produced. Doing stage B first would mean asking that agent to rebase onto a
  moved allocator, which is exactly the cost this sequencing exists to avoid.
- **`firefox-passthrough`**, on the shared_vulkan side, is the integration that
  will first want §4's no-copy path. It is the natural consumer of stage D and
  its findings should shape stage D rather than the reverse.

**The window to aim for is after stage16 and the proto-* bundle land.** If the
current rate holds that is hours to a day, not weeks. If a branch stalls, the
merge should not wait on it indefinitely: a branch that has not moved is better
rebased once, deliberately, than used as a reason to keep two repositories.

### 8.4 What is needed to schedule it

Three things, none of which this worktree can do for itself:

1. **Permission to commit in `waycompose`**, for stage B. This worktree is
   `shared-vulkan` only, so the adoption half of the unification cannot be
   written here. It is roughly a day's work and it is the piece that makes stage
   C cheap.
2. **A decision on the merge direction.** This document assumes waycompose moves
   into shared_vulkan, because the arena, the locator and the boundary rule live
   here and because `sv-icd` is the artefact with the most machinery behind it.
   The opposite direction would work; what must not happen is a third repository.
3. **A quiet moment on the waycompose branches**, per §8.3 — announced, so that
   the agents holding them can push rather than be surprised.

### 8.5 What this document does not claim

Nothing here has run on the guest. No frame has travelled by handle. The two
built modules have 21 tests between them and every one is a unit test against a
free list and an integer round trip — they prove the primitives are consistent,
not that a window shows a picture. `firefox-accel.md`'s standing warning applies
and is repeated deliberately: **"implemented" has meant "compiled" twice now**,
and §4's path is implemented in exactly that sense until a frame goes through it.

### 8.6 Two things found while running the gates, neither caused by this work

Recorded because both will bite the merge if they are not known.

**`sv-hostd`'s tests name `/dev/shm` objects by pid, and pids recycle.**
`the_owner_publishes_and_a_second_view_attaches` failed once during this session
with `open(shm, O_CREAT|O_EXCL)` → `EEXIST`. The cause was three stale
`/dev/shm/sv.control.<pid>` objects left by dead processes from other worktrees
on this host (pids 1212403, 1222020, 419128 — all confirmed dead), and a fresh
test process drawing a pid that matched one of them. It passes again once they
are removed. This is latent today and becomes routine after the merge, because a
merged tree has more harnesses on one machine, not fewer: the object should carry
a random component or be `shm_unlink`ed on drop even on the failure path.

**Two clippy warnings pre-date this branch** and are in files this work did not
touch, so "clippy clean" is not literally true of master today:
`sv-hostd/src/qemu.rs:666` (`identity_op`, on a deliberate
`(0xcb000 - 0xcb000) + …` that spells out two memory holes) on Linux, and a
`redundant redefinition of a binding` in `sv-transport` on the Windows target.
Both are one-line fixes and neither is this branch's to make — `qemu.rs` is
`dax-locator`'s territory.

---

## Re-running what is built

```
nix develop
cargo test -p sv-arena                                    # 64, of which 21 are new
cargo test --workspace                                    # 956
cargo clippy --workspace --all-targets
cargo clippy -p sv-arena -p sv-transport -p sv-wire -p arena-record \
      --all-targets --target x86_64-pc-windows-gnu        # the measurement of §1.4
```
