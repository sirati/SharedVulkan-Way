# shared_vulkan — architecture and design

Written 2026-08-11, against `requirements-shared-memory.md`, `security-model.md`,
`hardware-constraints.md`, `measured-gpu-side.md`, `api-scale.md`,
`research-sharedgl.md` and `research-vulkan-forwarding.md`.
Revised 2026-08-11 against `measured-gpu-side.md` "Step 0", and again the same
day against its second round of measurements ("Large pages, AWE, and the
privilege that unlocks both", "Can we have both? `QueryWorkingSetEx`, measured")
and against the codegen stage as actually built — see §0.1 and §0.2.

This document **decides**. Where the research surveyed options, this picks one
and says why. Where a decision is genuinely close, it says what would change it.
Citations are to the research documents by section, e.g. (fwd §2.1) for
`research-vulkan-forwarding.md` §2.1 and (sgl §7.2) for `research-sharedgl.md`
§7.2.

Nothing here is implementation code. The layout tables are specifications.

**Superseded passages are kept, not deleted.** Where measurement has overtaken a
decision, the original text stays in a blockquote marked
**[SUPERSEDED — <date>, see …]** and the replacement follows immediately. Half
the value of this document is the record of what was believed and why; a design
that quietly rewrites its own history cannot be audited by the person who has to
trust it later.

---

## 0. Status: what step 0 settled, and what it did not

This document was written before its own gating experiment had been run. That
experiment has now been run, on the real machine, and the results are in
`measured-gpu-side.md` ("Step 0", 2026-08-11). This section is the summary; §5
and §14 carry the consequences.

**The gate passed.** A 64 MiB window of guest RAM was allocated and pinned
inside the Windows guest, located in QEMU's `memory-backend-memfd` by a Linux
process on the other side of the hypervisor, written into from that process, and
then imported by the Intel driver as `VkDeviceMemory`. All 16384 pages of the
driver's own mapping carried the host's bytes; bytes written back through the
`VkDeviceMemory` mapping appeared in the memfd at the offsets the translation
predicts and were read out of guest-physical memory a third way, by QEMU's `xp`.
`memoryTypeBits` was `0x6` and the chosen type index was 1 — the identical
answer ordinary process memory gets. **The driver does not distinguish shared
guest RAM from any other host pointer.** That is the friendliest available
result and it is the one thing the entire design was hanging on
(`measured-gpu-side.md`).

Three things it also settled, each of which changes text below:

- **The pages of a pinned 4 KiB Windows allocation are scattered.** A 64 MiB
  window was **1471** discontiguous runs in guest-physical space, mean ~44 KiB,
  so attaching one slab is roughly 1500 `mmap(MAP_FIXED)` calls rather than one.
  §5's slab attach was not written against that; §5.1, §5.2 and §5.7 are. The
  answer is large pages (§5.1) with the 4 KiB path retained as a fallback,
  because the fallback is affordable — `vm.max_map_count` is 1048576 on this
  host — and because large-page allocation can legitimately fail.
- **GPA → memfd offset is not a formula.** On this machine the below-4 GiB split
  is 3 GiB, not the 3.5 GiB an i440fx uses at smaller RAM sizes, and two ranges
  below 4 GiB are not RAM at their identity offsets. §5.1 and R21.
- **AWE was not tested**, because `SeLockMemoryPrivilege` is unassigned to any
  account. The fallback (`VirtualAlloc` + `VirtualLock`) carried the whole
  experiment instead, which is why the result above is stronger than planned:
  the host had to *find* the pages by content rather than being told where they
  were.

**What is still not proven**, and is not to be quietly assumed anywhere below:

1. **No GPU has touched these pages.** What passed is an *import*: the driver
   accepted the pointer, mapped it, and its mapping is demonstrably the same
   physical memory. Whether the GPU can DMA from a host-shared page — its own
   translation is entirely downstream of the import succeeding — is a trivial
   compute workload and it has not been run. That is R15 and §13 step 7, and it
   is now the largest unmeasured thing in the project.
2. **Coherency was measured at rest.** One writer at a time, with a file-based
   rendezvous between phases. Nothing was measured about concurrent access, and
   nothing about cache behaviour under load. §5.6's decision is still an
   argument — now with supporting evidence — and not a measurement (R10).

Both of those still stand unchanged, and item 1 is now the largest gap in the
project by a wide margin (§14). The other two items on this list did not survive
the day:

> **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "Large pages, AWE, and the
> privilege that unlocks both" and "Can we have both? `QueryWorkingSetEx`,
> measured"]**
>
> *3. Large pages are being measured now, and the design assumes them (§5.1).
> Whether the Intel driver accepts an import of large-page-backed memory is
> not to be assumed from the 4 KiB result: an import path that pins pages may
> well care about the PTE granularity underneath. Every number attached to the
> large-page path in this document is marked pending until
> `measured-gpu-side.md` carries it.*
>
> *4. AWE remains untested end to end, and so does R19. The privilege is being
> granted for the large-page work, which makes AWE measurable again rather than
> blocked; §5.1 says which way the decision goes on today's evidence.*
>
> Both were measured hours later, on the same machine, and both went the design's
> way. The pending figures are numbers now and nothing in this document should
> still be read as waiting on them. §0.1.

### 0.1 Round two: large pages, AWE, and where the frames are not

Measured the same day, once `SeLockMemoryPrivilege` was granted (R25):
`measured-gpu-side.md`, "Large pages, AWE, and the privilege that unlocks both"
and "Can we have both? `QueryWorkingSetEx`, measured". Six results, each of which
changes text below.

1. **Large pages win, and not narrowly.** Same 64 MiB window, four allocators,
   counted as contiguous runs in QEMU's memfd: `VirtualLock`ed 4 KiB pages
   **1471**, AWE **5863–16339**, `AllocateUserPhysicalPages2` with
   `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE` **3992**, and
   `VirtualAlloc(MEM_LARGE_PAGES)` **22**. The driver imported every one of them
   with `memoryTypeBits = 0x6`, so what separates them is fragmentation, not
   admissibility.
2. **AWE is the *worst* allocator for contiguity, not the best** — 5863 runs on
   one attempt and 16339 on another, essentially scattered and variable with the
   guest's own memory state. Frame numbers and contiguity are separate problems
   and AWE solves only the first. §5.7 argued this; it is now measured.
3. **`NONPAGED_LARGE` is not the fix this document named.** The call succeeds,
   but on Windows 11 build 26200 it satisfied only 3992 of 16384 requested frames
   and returned them at one page per run. The decision it was going to force —
   AWE2 as the primary path, deleting the locate step — does not arise.
4. **Contiguity improves with size, and slab size is our choice.** 64 MiB → 22
   runs, 1 GiB → **1 run**, 2 GiB → 2; and `vkAllocateMemory` accepted a 1 GiB
   and a 2 GiB window each as a single `VkDeviceMemory`. The driver imposes no
   slab limit at these sizes, so §5.2's 64 MiB is an allocator decision and
   nothing else — corrected there.
5. **Windows reports no frame numbers to user mode outside AWE.**
   `QueryWorkingSetEx` was the candidate for having contiguity *and* frames; over
   262144 entries its 64-bit word took exactly one distinct value, and
   `ReservedUlong` is zero for every page of every allocator. It is still worth
   calling for its `LargePage` bit, which discriminates the allocators cleanly so
   the GPU side can assert what it actually got — and on large pages it strides
   by 2 MiB, 156 µs per GiB instead of 48.8 ms.
6. **The content scan is the locator, and its cost does not scale with the
   arena.** 1.8 s at 64 MiB, 2.02 s at 1 GiB, 2.32 s at 2 GiB, because the sweep
   reads all 13312 MiB of guest RAM whatever the window is. §5.7 and §14 flagged
   the extrapolation from 64 MiB to 2 GiB as unproven; it is measured, and the
   worry was misplaced.

Two consequences beyond the numbers. **R19 and R20 are retired in full**
(`security-model.md`): the AWE window imported without complaint, and
`AllocateUserPhysicalPages` still returns raw guest-physical frame numbers on
Windows 11 build 26200 — all 16384 verified against the memfd through the R21
translation. That settles §14's items 2, 5, 6 and 7 and means **the
attestation-signed pinning driver §5.1 held in reserve is not needed**. And
**R26 is new**: a `VirtualLock`ed window re-located 3m14s later gave a
byte-identical offset table, which is evidence against casual relocation and
*not* evidence against relocation under memory pressure, which was deliberately
not provoked on somebody's daily machine. §5.1 carries the mitigations.

### 0.2 What the codegen stage settled

The generator of §7 is built and tested: **10,145 hand-written lines producing
126,205 generated lines for a 239-command Zink profile, 657 tests, Miri clean.**
Four of its findings are corrections to this document rather than notes about the
code, and they are made in place below.

- **§6.2's byte-for-byte differential against Venus is not achievable**, and the
  reason is two deviations this document had already taken. What was built
  instead compares member sequence, kind and width across **208 shared structs**,
  which is stronger than a byte comparison rather than weaker. Corrected in §6.2.
- **§6.1 mis-described the optional-pointer flag as "smaller".** It is the same
  eight bytes as gfxstream's token. The advantage is that it carries no address.
  Corrected in §6.1.
- **§7.4's exception budget was pessimistic in a specific way**: three of the six
  exceptions dissolve in Rust rather than being handled. ~11 entries remain
  against the ~20 that would have flipped §7.1's decision, and the declarator
  parser came in at **375 lines** against the 800 that would have. §7.1's exit
  condition is met with room.
- **Pass 4 (`arena_bind`) is not emitted at all**, because owned Rust values make
  it a no-op. §7.2.

Two further findings are about the registry and about R8, and are recorded in
§7.1, §7.3, §11 and §13:

- **A feature is not a leaf.** `vulkan-headers 1.4.350` splits each core version
  into `apitype="internal"` sub-features chained by `depends=`, so
  `<feature name="VK_VERSION_1_0">` contains no commands of its own. Without
  resolving the chain the Zink profile silently resolved to **55** commands
  rather than 239 — a small protocol, not an error.
- **A live R8 bug, caught only by an assertion against published values.**
  `vk-parse`'s `EnumSpec::Offset` carries `dir` as *positive* rather than as "is
  negative", so the obvious reading inverts the sign of every
  extension-introduced enum value. The round-trip tests agreed on the wrong
  number, exactly as R8 says they would; only checking generated constants
  against `vulkan_core.h` caught it.

### 0.3 What the ICD stage settled

Built 2026-08-12: pass 10 (`sv-icd-entry`, generated) and the hand-written app
side (`sv-icd`). **The real Vulkan loader enumerates us**: `vulkaninfo` with
`VK_DRIVER_FILES` pointing at the generated manifest reports
`shared_vulkan forwarded device`, the §9.1 memory table, and no loader
complaint at `VK_LOADER_DEBUG=all`; it does the same under
`VK_LAYER_KHRONOS_validation`, which is what exercises the dispatchable-handle
contract of §6.4. §13 step 5 is met except for the triangle, which needs a GPU
side. Six of the findings are corrections to this document.

1. **The ICD's C surface is not the wire profile, and §12 implies it is.** The
   Zink profile resolves to 239 forwarded commands; the surface the ICD must
   *export* is **250**, and the difference is precisely the commands §8 and §10
   say are answered app-side — proc-address lookup, the swapchain family, the
   surface queries, the debug-utils messenger — plus `VkAllocationCallbacks`,
   which §7.4 strips *from the wire* and which is still in every C signature.
   The generator therefore resolves a second, wider profile for the ABI. A
   generator driven by one profile silently omits eleven entry points, and the
   loader's symptom is a null function pointer with no diagnostic.
2. **`len=` is load-bearing again at this boundary, and §7.4 says the
   opposite.** That section's finding — every variable-length thing carries its
   own `u64` count, so `len=` is "a verifier rather than an input" — is true of
   the wire and **stops at the C ABI**. Here `len=` is the only thing that
   bounds a read of the *application's* memory, and a wrong one is an
   out-of-bounds read of somebody else's process rather than a failed check. The
   consequence is concrete: `(rasterizationSamples + 31) / 32` on
   `VkPipelineMultisampleStateCreateInfo::pSampleMask`, which §7.4 is content to
   record unparsed, has to be parsed here. It is the only such expression on the
   surface and the generator now refuses anything it cannot resolve.
3. **A count sibling is frequently a *pointer*, and the registry does not say
   so.** `vkGetPhysicalDeviceQueueFamilyProperties` has
   `len="pQueueFamilyPropertyCount"` and `uint32_t* pQueueFamilyPropertyCount`;
   C dereferences it in the caller's head and `vk.xml` records only the name.
   The first `vulkaninfo` run against this ICD died on exactly that — the
   address, taken as a length, asked for a `Vec` of 3.4×10¹⁵ elements. Nothing
   in the registry distinguishes the two cases; the generator has to look at the
   sibling's own C type.
4. **An array *parameter* is a pointer, not an array.**
   `vkCmdSetBlendConstants(VkCommandBuffer, const float blendConstants[4])` takes
   a `const float*`; C decays it and Rust does not. A `[f32; 4]` in an
   `extern "system"` signature is a different calling convention, and the only
   diagnostic is an FFI-safety warning. Array-to-pointer decay applies at the
   outermost level of a parameter and nowhere else — `char deviceName[256]`
   inside a struct is a genuine array.
5. **A command that returns a value which is neither `void` nor `VkResult` has
   no way to return it.** §7.2's reply rule, as implemented, is
   `generates_reply = returns_result || !outputs.is_empty()`, and three commands
   in the Zink profile satisfy neither: `vkGetBufferDeviceAddress`,
   `vkGetBufferOpaqueCaptureAddress` and `vkGetDeviceMemoryOpaqueCaptureAddress`
   all return a `VkDeviceAddress` with no output parameter, so the generated
   reply has nowhere to put it. It is **latent rather than live** — §9.2 does not
   advertise `bufferDeviceAddress`, so a conforming application never calls them
   — but it is a hole in the protocol and not in the ICD, and the fix belongs in
   `plan.rs`: a reply exists whenever the command returns anything at all.
6. **A dispatchable handle cannot be generated, and that bounds the hand-written
   half.** §7.3 says the ICD stubs are "one mechanical shape"; seventeen of the
   250 are not, and sixteen of those for one structural reason. What
   `vkCreateInstance` returns is a pointer to an allocation whose first word the
   loader owns, and no fold over a wire reply produces one — the reply carries a
   `u64` id. Every command that mints or destroys a dispatchable object is
   therefore hand-written, and the list is short and closed because Vulkan has
   five dispatchable types. Two further facts fell out: **a physical device and a
   queue must return the same pointer every time** (an application may compare
   them, and a fresh allocation per call also leaks), and §12's description of
   `sv-icd-entry` as "lib, **generated**" needs the qualifier that it has a small
   hand-written half.

Two smaller corrections. **§8's "Exports: exactly `vk_icdGetInstanceProcAddr`,
`vk_icdNegotiateLoaderICDInterfaceVersion` and `vk_icdGetPhysicalDeviceProcAddr`"
is right at loader interface 7 and wrong below it**, where the loader looks for
`vkGetInstanceProcAddr` by name; we export it as well, which costs one
forwarding function. And **§7.5's estimate for this pass was low by the same
factor as the rest**: 10,000 lines budgeted for "ICD entry stubs + proc tables",
**47,120** emitted — because the estimate did not include a `#[repr(C)]` type
layer (596 types) or the C-to-wire lowering, both of which turn out to be where
the work is.

What is **not** settled, and is the honest state of this stage: nothing reaches a
GPU. `sv-transport` and `sv-gpu-server` are not on master, so every forwarded
command is answered app-side, `vkQueueSubmit` accepts and drops, and the arena is
a `memfd` this process created rather than guest RAM `sv-hostd` located. The
arena mapping and `vkMapMemory` are real — the pointer is a real pointer into
real shared pages — and the half that has not happened is the GPU side importing
them.

### 0.4 What the Zink stage settled

Built 2026-08-12. **OpenGL runs.** `eglinfo` on the surfaceless platform reports

```
OpenGL core profile renderer: zink Vulkan 1.4(AMD Ryzen 9 9950X3D … (RADV RAPHAEL_MENDOCINO) (MESA_RADV))
OpenGL core profile version:  3.2 (Core Profile) Mesa 26.2.0
```

— GL 3.2 core, 3.2 compatibility and GLES 2.0, every context naming *the GPU
side's* device — and `scripts/sv-zink-gl.sh` then renders a frame and checks its
pixels: an indexed, depth-tested, textured, lit 4608-triangle mesh drawn into an
FBO and read back, agreeing with the **same program on llvmpipe at 64.1 dB
PSNR**. Two entirely different rasterisers producing the same image is the
strongest available statement that what crossed the wire was right. §13 step 6 is
met for GL 3.2; the deqp suite it also names has not been run.

The stage is **223 of the 239 commands forwarded by a generated pass**, against
about forty written by hand before it. Sixteen remain hand-written and each has a
reason in `sv_codegen::exceptions::GPU_HAND_WRITTEN`. Eleven of its findings are
corrections to this document.

**1. §7.2's taxonomy is missing a pass, and it is not a two-by-two.** The
previous stage recorded the gap — no wire→C "raise" for the GPU side — and it is
now generated (`emit::gpu_entry`, pass 12). What it also found is that the table
has a *third* axis. "C→wire, GPU side" looked reusable from the app side's
`lower_*`, and is not: a `returnedonly` struct's members are the application's
uninitialised memory on the app side and **the driver's answer** on the GPU side,
so the app side's lowering deliberately zeroes exactly the members the GPU side
exists to carry. The first generated run reported
`VkPhysicalDeviceProperties` with an empty `deviceName` and `apiVersion = 0`.
The direction does not determine the conversion; **the validity state does**, and
`lower_full_*` is emitted alongside `lower_*` for that reason.

**2. Pass 4 is not dead, it moved.** §7.2 strikes out `arena_bind` because owned
Rust values make it a no-op. That is true of *decoding* and false of *raising*,
which is the operation gfxstream's `deepcopy` actually serves: a C
`VkGraphicsPipelineCreateInfo` holds eight pointers to sub-structures that must
outlive the call. Pass 4 is `sv_gpu_entry::Raise`.

**3. Passes 5 and 6 have to partition the handle space, and §7.2 does not say
so.** Both claimed `VkDeviceMemory`. An app-side memory id is not a table entry
at all — it is `ARENA_MEMORY_ID`, a sentinel, or an offset into a slab — so the
handle map cannot resolve one and took the ring fatal on the first
`vkBindBufferMemory` an application made. Only the *pair* means anything, so
pass 6 owns it and pass 5 skips it.

**4. `objecttype=` is a fifth kind of handle occurrence, and nothing reads it.**
`VkDebugUtilsObjectNameInfoEXT::objectHandle` is a `uint64_t` whose type is
decided at run time by a *sibling member*. No type-directed walk can see it, so
the app side's id reached the driver unmapped and RADV dereferenced it. Two
commands on this surface; both are hand-written.

**5. The count-then-fill idiom was broken in the app side's writeback, silently,
for every enumeration in the API.** It derived `*pCount` from *the length of the
reply's array* rather than from the reply's count field. A GPU side answering the
count query honestly — a number and no array, since `pProperties` was NULL —
therefore wrote zero. It looked correct for exactly as long as the hand-written
GPU side returned the full array on every call. The symptom was `vulkaninfo`
printing an empty queue-family list and the validation layer indexing that empty
vector and dying inside `ValidateDeviceQueueCreateInfos`.

**6. `Vec<Box<T>>` as a pointer pool is unsound, and only Miri could say so.**
Moving a `Box` retags its pointee, invalidating every pointer derived from it —
and pushing it into a vector is a move, as is each reallocation afterwards. The
code was entirely safe Rust and worked perfectly. `Box::into_raw` is the fix.
This is R7's argument for Miri paying for itself in one afternoon.

**7. The queue-family table is ours, not the driver's, for exactly §9.1's
reason.** A GPU side has one queue on one family, chosen at startup. Passing
RADV's five families through had an application select family 3 and
`vkGetDeviceQueue` hand it family 0's queue in silence. One synthesised family,
with the real flags of the one that exists.

**8. §9.2 describes the device-extension list as something the app side
curates. It cannot.** The list is the profile's extensions intersected with what
the driver actually has, and only the side holding the driver knows the second
half. It is asked of the GPU side once at connect.

**9. Opening the driver before the application connects has a cost §3 does not
mention.** `vkCreateDevice` binds an id to a device that already exists, so the
application's requested feature set cannot be honoured. The mitigation is to
enable everything the physical device reports — and to raise the instance to 1.3,
because `scalarBlockLayout`, `timelineSemaphore` and `dynamicRendering` live in
the 1.2 and 1.3 feature blocks and a 1.1 instance cannot name those structures.

**10. `vkUpdateDescriptorSetWithTemplate`'s `pData` is the one parameter on this
surface whose size is a property of another object's contents.** No fold over
`vk.xml` can marshal it. It does not have to cross: the pointer is the
*application's* and the ICD runs in the application's process, so the app side
expands it into the `vkUpdateDescriptorSets` it means, using the entries it
remembered at template creation. Zink's default descriptor path uses templates
unconditionally, so this is on the critical path for OpenGL rather than a corner.

**11. Overriding a generated `Icd` create-command silently opts out of id
minting.** `emit_mint_outputs` lives in the trait *default*. Two descriptor
update templates both numbered 0 aliased in the app side's table, the second
overwrote the first, and a descriptor write went to a binding the other set's
layout does not have. The failure is silent because 0 is `VK_NULL_HANDLE` and
nothing rejects it.

**What is not settled**, and is the honest state of this stage:

- **GL 3.2, not 4.6.** llvmpipe gives 4.6 through the same program. Which
  capability Zink is missing has not been established, and finding out is the
  first job of the next stage — Zink computes its GL version from Gallium caps
  that are themselves computed from device features, so the answer is a specific
  feature or format that is not crossing correctly.
  **[RESOLVED — §0.5 items 1 and 2. It was 4.6 all along and three answers were
  wrong: a forwarded `apiVersion` above what we implement, and two extension
  names left out of the profile. The guess in the sentence above — "a specific
  feature or format that is not crossing correctly" — was wrong in an
  instructive way: every feature was crossing correctly and was never asked
  for.]**
- **`VUID-vkBindBufferMemory-memory-02726` fires twelve times per run.** The
  arena on the export path is allocated with `VkExportMemoryAllocateInfo`, so
  every buffer bound into it must be created with a matching
  `VkExternalMemoryBufferCreateInfo`. The app side cannot add that chain — it
  does not know the handle type — and the GPU side cannot add it in the generated
  path. RADV tolerates it; another driver need not. This is a consequence of the
  arena provider and belongs with §5.4.
  **[RESOLVED — §0.5 item 3 and §5.4. Both halves of "neither side can" were
  right; the answer was that the GPU side stops using the generated path for
  `vkCreateBuffer`. It is also not export-specific: the import providers carry
  the identical obligation under `-02727`. Validation is now clean.]**
- **No windowed GL.** `zink_kopper` wants a swapchain and §10 keeps presentation
  outside the protocol, so glxgears cannot run: Xvfb's GLX has no DRI3 and there
  is no other display. What was tested instead is the same workload without the
  window.
  **[RESOLVED — §0.6. `glxgears` runs, in an X window, and agrees with llvmpipe
  on every histogram statistic. The DRI3 half of this sentence was right about
  the fact and wrong about the layer: kopper needs no DRI3, but Mesa's *GLX*
  refuses the visual for lack of it before zink is reached, which
  `LIBGL_KOPPER_DRI2=1` skips.]**
- **`vkQueueSubmit` is forwarded rather than blob-executed.** §6.5's
  `svExecuteCommandBlob` is still not built; every `vkCmd*` crosses as its own
  ring packet. The generic forwarding is strictly better than the hand-written
  version it replaced — it carries the `pNext` chain — but it is not §6.5.

### 0.5 What the GL 4.6 and Direct3D stage settled

Built 2026-08-12. **OpenGL 4.6.** `eglinfo` on the surfaceless platform now
reports

```
OpenGL core profile renderer: zink Vulkan 1.3(AMD Ryzen 9 9950X3D … (RADV RAPHAEL_MENDOCINO) (MESA_RADV))
OpenGL core profile version:  4.6 (Core Profile) Mesa 26.2.0
```

— the same 4.6 core, 4.6 compatibility and GLES 3.2 that Zink reports when it is
run on that RADV *directly*, and with the GL extension string now differing from
that reference only in extensions no GL version requires. `scripts/sv-zink-gl.sh`
grew a third check, `gl4-probe`, which uses the three capabilities that were
missing and judges each on its pixels: a patch tessellated to level 16 and mapped
onto a disc (the disc fills 0.794 of its bounding box; four corners alone would
fill 0.500), four instances placed by a vertex attribute with
`glVertexAttribDivisor`, and `glMultiDrawArraysIndirectCount` given four commands
and a parameter buffer saying two. All three agree with llvmpipe at **69.8 dB
PSNR**, and `gl-probe`'s original frame is unchanged at 64.1 dB.

Six findings, and the first is the one §0.4 asked the next stage to find.

**1. The GL version was held at 3.2 by a *version number*, not by a feature.**
`vkGetPhysicalDeviceProperties` was forwarded whole, so `apiVersion` was the
driver's **1.4.348** while the wire profile is core 1.0–1.3, the manifest says
1.3 and `vkEnumerateInstanceVersion` says 1.3. A physical device was claiming a
version its own ICD does not implement. Zink takes `screen->vk_version` from
exactly that field, and its device-info pass stops chaining an extension's
feature struct into `vkGetPhysicalDeviceFeatures2` once the version says the
extension is core, reading the feature out of the core aggregate instead
(`zink_device_info.py`, `features_promoted`). At 1.4 it therefore read
`vertexAttributeInstanceRateDivisor` from `VkPhysicalDeviceVulkan14Features`,
which this profile has never heard of, got zero, and dropped
`GL_ARB_instanced_arrays` — the GL 3.3 gate — while the
`VkPhysicalDeviceVertexAttributeDivisorFeaturesEXT` we *do* answer, and which
says `true`, went unasked. **Claiming a version above what you implement does
not merely overstate the answer; it makes the honest answer unreachable.**
`apiVersion` is now clamped in `sv_icd::icd::clamp_api_version` and is the one
field of that struct which is ours rather than the driver's.

**2. The remaining two steps were extension *names*, both for capabilities we
already had.** `VK_KHR_maintenance2` gates the whole tessellation stage in Zink
(`zink_init_shader_caps` skips the tessellation-control and -evaluation stages
unless `tessellationShader && have_KHR_maintenance2`), and it reads
`have_KHR_maintenance2` from the extension list, promoted-to-core or not — so
`tessellationShader = true`, which we had been reporting all along, bought
nothing and GL stopped at 4.0. `VK_KHR_draw_indirect_count` likewise gates
`GL_ARB_indirect_parameters`, the last extension before 4.6, although its two
commands are core since 1.2 and were already forwarded. Both are in the profile
now. **Neither added a capability. Both were §9.2's "curated, not mirrored"
curating out something the curator had no reason to think mattered**, which is
the standing hazard of a curated list and is worth a rule: *an extension whose
every command and type is already in the profile costs nothing to advertise, and
consumers use extension names as capability flags whether or not the
specification promoted them.*

**3. §0.4's `VUID-vkBindBufferMemory-memory-02726` is not export-specific, and
neither side could fix it from where §0.4 left it.** The requirement is on
external memory of *any* provenance — export is `-02726`, import is the
identically-shaped `-02727` — so it applies to both of §5.4's providers and not
just to the one `testhost` happens to use. It is fixed on the **GPU side**, in
a now hand-written `vkCreateBuffer` that merges a
`VkExternalMemoryBufferCreateInfo` naming the arena's handle type into every
forwarded buffer. That side is the only one that holds the value: on the
app-allocated path the `Attach` that carries the arena to the ICD is sent by
`sv-hostd`, which does not know the handle type either, so an app-side fix would
need a new protocol message carrying a value the app side would only send
straight back. Three consequences worth stating:
  - **The GPU side is not a faithful relay any more, and this is the first place
    it is not.** §7.2's passes all *convert* what they were given; none of them
    invents. Adding a `pNext` link the application did not write is a new
    operation, and the mirror of §0.4 item 10 where the app side expands
    `vkUpdateDescriptorSetWithTemplate` into what it means. The rule that falls
    out: **each side may rewrite exactly what only it knows.**
  - **It cannot be conditional**, because `vkCreateBuffer` happens before anyone
    has asked for memory requirements, let alone chosen a type. Every forwarded
    buffer is therefore created external-capable, and the price is that a driver
    may narrow `memoryTypeBits` for such a buffer.
    `arena::check_external_buffers_keep_the_arena` asks that question once at
    startup and refuses to serve if the narrowing would exclude the arena's own
    memory type. On RADV the mask does not move.
  - **The same argument applies to images** (`VkExternalMemoryImageCreateInfo`,
    `VUID-vkBindImageMemory-memory-02728`) and does not fire only because §9.1
    puts optimal-tiling images in memory type 0, which is a GPU-side allocation.
    That is a load-bearing consequence of §9.1's ordering which §9.1 does not
    state, and it should be read as one: reorder that table and images land in
    the arena and need the same treatment.

  Validation is now **zero errors** over a full `gl4-probe` run, where it was
  fourteen.

**4. An output *blob* was crossing with its length thrown away.** The app side's
lowering sends an output array's capacity — that is Venus's `_partial` array —
but a sized `void*` fell through to "send nothing at all", so the GPU side, which
sizes the buffer it hands the driver from the length of the array it received,
passed a **null `pData` with a non-zero size**. `vkGetPipelineCacheData` returned
`VK_INCOMPLETE` on every run, which is how it was noticed;
`vkGetQueryPoolResults` on the same path returns `VK_SUCCESS` with the caller's
buffer untouched, which nothing reports. Zink does not take that second path —
it copies query results into a buffer with `vkCmdCopyQueryPoolResults` — so it
had not fired. This is §0.4 item 5's mistake in the other direction: there the
*count* was derived from the array's length on the way back, here the array's
length was dropped on the way out. **The count-then-fill idiom has now been got
wrong once in each direction, which suggests the shape rather than the code is
what is hard**, and that a generator which treats "output" as "nothing to send"
will keep finding new ways to say it.

**5. `DriverFns` was derived from the set of *generated* forwarders**, so a
hand-written command that still had to call the driver had nowhere to get its
entry point. It said the wrong thing for as long as every hand-written command
either answered locally or went through `ash`. `vkCreateBuffer` is the first that
must call the driver with the *registry's* signature — it raises the registry's
`VkBufferCreateInfo`, and reaching the driver through `ash` would hand one
binding's struct to a function typed by another. The table now covers every
forwarded command, hand-written or not.

**6. Direct3D: the blocker is §10, and it is not `bufferDeviceAddress`.** See
the section below; this is the correction to §9.2 and §11.

#### Where Direct3D actually stands

Measured, not reasoned: `scripts/sv-dxvk-d3d11.sh` runs `scripts/d3d11-probe.c`
— a windowless D3D11 program that renders to a texture and reads it back —
under Wine 11.0 with a real DXVK, against this ICD.

**DXVK 2.6.2 reaches `D3D_FEATURE_LEVEL_12_0` on our forwarded device.** It
enumerates the physical device, reads §9.1's memory table back verbatim, computes
a maximum Direct3D feature level of 12_0 and selects 11_1 — and then fails on
exactly one thing: **`VK_KHR_swapchain`, which DXVK marks `Required` even for a
device that will never present.** Its only other required device extension is
`VK_EXT_robustness2`, which we advertise. So:

- **`bufferDeviceAddress` is not what stops D3D11.** DXVK ≤ 2.6.2 requires it
  only when it can also see `VK_NVX_binary_import` and `VK_NVX_image_view_handle`
  and is turning on CUDA interop; on this device it never asks. §9.2's sentence
  "a device that does not advertise the feature causes DXVK 3.x and vkd3d-proton
  to fail device creation cleanly" is right about the direction and wrong about
  the version: **it is DXVK ≥ 2.7**, which rewrote its requirement list and now
  demands `bufferDeviceAddress`, `descriptorIndexing`, `VK_KHR_pipeline_library`
  and a good deal more (`dxvk_device_info.cpp`, `ENABLE_FEATURE(vk12,
  bufferDeviceAddress, true)`). Measured: 2.7.1 refuses this device, naming
  `VK_KHR_pipeline_library` first because that is where its list reaches first.
- **The decision not to advertise `bufferDeviceAddress` therefore costs less than
  §9.2 assumed.** It costs D3D12 through vkd3d-proton, which is structural —
  `ID3D12Resource::GetGPUVirtualAddress` is in the D3D12 API itself, so a raw
  device address is not an implementation choice vkd3d could make differently —
  and it costs DXVK ≥ 2.7. It does **not** cost D3D9/10/11 through DXVK ≤ 2.6,
  which is a supported, widely deployed configuration.
- **What D3D9/10/11 costs instead is §10 stage 2**, or a smaller piece of it: a
  `VK_KHR_swapchain` that exists. That is a real piece of work and it is the same
  piece of work windowed GL needs (`zink_kopper`, §0.4). Two of the three things
  this project cannot currently do turn out to be one thing.

Two findings about the plumbing rather than about Direct3D.

**`sv-gpu-server serve --clients N` accepts exactly `N` connections before it
serves any of them, and that is the wrong shape for a real application.** Wine
starts helper processes that each create a Vulkan instance, so the count is a
property of the workload rather than of the configuration: one too few and a
helper takes the application's slot — after which the ICD refuses to load and
the loader reports only `VK_ERROR_INCOMPATIBLE_DRIVER` — and one too many and
the server waits for a client that never arrives while the application's first
command times out. Neither symptom names the cause. §3 does not say the GPU side
must know its client count in advance and it should not: accepting on a thread
while serving would remove the parameter entirely. Recorded rather than fixed;
`scripts/sv-dxvk-d3d11.sh` works around it by warming the Wine prefix first so
that "1" is a fact.

And one about Wine, which cost most of an afternoon to see: **Wine puts instance
extensions on the host instance that the application never asked for**, and `vkCreateInstance` fails with
`VK_ERROR_EXTENSION_NOT_PRESENT` if the host ICD lacks them. It needs
`VK_KHR_external_memory_capabilities`, `VK_KHR_external_semaphore_capabilities`
and `VK_KHR_get_physical_device_properties2`, and it maps the Windows
`VK_KHR_win32_surface` onto the host's **`VK_EXT_headless_surface`** when there
is no display — which is precisely the surface extension §10 stage 1 already
chose, so Wine's headless path and this design agree by accident. The three
capability extensions are now advertised, and the three
`vkGetPhysicalDeviceExternal*Properties` queries behind them are answered
**app-side with "no handle type is supported"** rather than forwarded. That is a
strengthening of §9.2 rather than a departure from it: §9.2 says we do not
advertise external handle types we cannot honour, and before this it was true
only because nothing could ask — the queries are core 1.1, an application can
make them regardless, and RADV's honest answer is our dishonest one.

### 0.6 What the presentation stage settled

Built 2026-08-12. **Presentation works, and the two things §0.5 said it would
unblock are unblocked.**

```
scripts/sv-present.sh    3 frames through a headless VK_KHR_swapchain, byte-identical, validation clean
scripts/sv-zink-gl.sh    glxgears in an X window, agreeing with llvmpipe on every histogram statistic
scripts/sv-dxvk-d3d11.sh Direct3D 11 on DXVK 2.6.2: 31.1% coverage, 1082 colours — the same frame RADV draws unforwarded
```

The measurement §10 asks for and never had: at **1920×1080, `vkQueuePresentKHR`
costs 1.47 ms**, of which the whole is one `vkQueueSubmit` round trip carrying a
pre-recorded `vkCmdCopyImageToBuffer`, a fence wait, and the app side reading
arena memory it already has mapped — **5.4 GB/s of presented pixels**, against
§10's estimate that 60 Hz at 1080p needs 500 MB/s. At 256×256 it is 0.18 ms.
Presentation is not the bottleneck it was budgeted to be.

Seven findings. The first is a correction to this section's own plan, the second
and third are corrections to §8, and the fifth is a limit that would have stopped
any real application and had nothing to do with presentation.

**1. Presentation needs no protocol, and §10 stage 2's `svPresentBlit` should
not be built.** §10 says "the app side sends `svPresentBlit`; the GPU side runs
`vkCmdCopyImageToBuffer` … and signals a present timeline value" — a new
non-Vulkan command (§6.3), a GPU-side handler, and a timeline. None of it is
necessary. The app side can already issue every Vulkan command there is, because
that is what an ICD *is*, so it creates the swapchain's images, creates the arena
buffers, records the copy and submits it out of the existing wire.
**`sv-gpu-server` is unchanged by this entire stage** and is never told that any
of it is a swapchain. Because the copy command buffer is recorded once at
swapchain creation, a frame costs *one* round trip rather than one per command.

  The rule that falls out is the mirror of §0.5's "each side may rewrite exactly
  what only it knows": **a capability that is expressible in the forwarded API
  does not deserve a protocol extension.** The two places this project genuinely
  needed one — `vkUpdateDescriptorSetWithTemplate`'s `pData` and
  `vkCreateBuffer`'s external-memory chain — are both cases where one side held
  information the API gives the other side no way to name. Presentation is not
  one of those, and it is worth checking the next candidate against this test
  before adding an opcode.

**2. §8's "we opt *out* of interface-3 surface creation" no longer exists, and
its failure mode is silence.** That sentence — "by not exposing
`vkCreateXXXSurfaceKHR`, so the loader hands us a `VkIcdSurfaceXXX` from
`vk_icd.h` which we cast" — described Vulkan-Loader behaviour that has since
changed. In **1.4.350**, `allocate_icd_surface_struct` gives a per-ICD surface
slot to *every* driver that enables `VK_KHR_surface` at interface ≥ 3, and
`wsi_unwrap_icd_surface` then creates that driver's surface **lazily, by calling
the driver's own `vkCreateXxxSurfaceKHR`**. A driver that does not export one
gets `VK_ERROR_EXTENSION_NOT_PRESENT` from the lazy creation, which the
terminators quietly turn into `*pSupported = false` and
`VK_ERROR_SURFACE_LOST_KHR`. Nothing is logged, by the loader or by anyone.

  The symptom was Mesa printing `zink: could not create swapchain` three times
  while this ICD's own trace showed that **not one** of its surface entry points
  had been called, and the same shape from the Vulkan side as
  `surface support family 0: no`. Reading `loader/wsi.c` was the only way to find
  it. So the ICD now creates and owns its surfaces, `VkSurfaceKHR` is one of its
  own ids, and the `vk_icd.h` cast §8 describes is gone.

  The cost is exactly what §8 was trying to avoid: `vkCreateXcbSurfaceKHR` takes
  a `VkXcbSurfaceCreateInfoKHR` whose `connection` member is an
  `xcb_connection_t*`, which comes from a platform header this project does not
  include. Three commands are therefore hand-written *off* the generated C
  surface and injected into the proc tables by name
  (`sv_codegen::emit::icd_entry::EXTRA_LOCAL`), with the two create-info structs
  declared by hand beside them — nine lines and no build dependency. **Exporting
  none of the surface creators was not an option either**: exporting some makes
  `VkSurfaceKHR` "either one of my ids or a loader pointer", which cannot be told
  apart at run time. It is all of them or none, and none is no longer available.

**3. The `…PresentationSupportKHR` family is not optional, and the loader
`abort()`s without it.** `terminator_GetPhysicalDeviceXcbPresentationSupportKHR`
does not fall back to `VK_FALSE` for a driver that lacks the entry point — it
logs "ICD for selected physical device does not export …" and aborts the
process. So advertising `VK_KHR_xcb_surface` at all obliges an ICD to answer,
even though the answer here is a constant. That is a general property of the WSI
terminators worth remembering: **the surface family is the one place where "the
loader will handle it" is false in both directions.**

**4. An extension implemented entirely app-side must not be intersected with
what the GPU side has**, which is a qualification to §0.4 item 8. That item's
rule — the device-extension list is "the profile's extensions intersected with
what the driver actually has, and only the side holding the driver knows the
second half" — is right for every extension whose commands are *forwarded*.
`VK_KHR_swapchain`'s are not: the whole of it lives in `sv_icd::wsi`, and the
side that keeps §9.2's promise is the app side. It is added unconditionally.

  It is still in the *profile*, and for a reason that is not about the app side
  at all: the application transitions a swapchain image to
  `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR`, and that layout does not exist on a
  GPU-side device that never enabled the extension. So the profile carries the
  name so the GPU side enables it, and `excluded_commands` keeps every one of its
  commands off the wire. **A profile entry and a wire entry are different
  things**, which §11 and §9.2 both read as if they were the same.

**5. `Geometry::for_arena` set a 4 MiB slab, and a slab is the ceiling on every
host-visible allocation an application can make.** §5.3 makes a `VkDeviceMemory`
a sub-range of one slab, so `ArenaAllocator::alloc` refuses anything larger with
`LargerThanSlab`, which the ICD reports as `VK_ERROR_OUT_OF_DEVICE_MEMORY`. One
1080p BGRA frame is 8.29 MB, so `vkCreateSwapchainKHR` failed at 1920×1080 while
the identical call at 256×256 succeeded. §5.2 says 64 MiB and the live geometry
said 4; it is 64 now.

  **This was never a presentation bug.** Any application wanting a single
  host-visible allocation above 4 MiB — a staging buffer for one large texture, a
  vertex buffer for a real mesh — would have been refused, with a result code
  that says "out of device memory" for an arena that is 99% free. It went
  unnoticed because Zink stages in pieces and every probe so far has been
  256×256. **A limit expressed as a configuration constant, whose violation is
  reported as a resource shortage, is close to undiagnosable**, and the general
  lesson is that `max_allocation()` belongs in the numbers an application can
  read — `maxMemoryAllocationCount` has a sibling this table does not report.

**6. The arena-backed swapchain image is *admissible* and is still the wrong
trade.** Measured rather than assumed: RADV reports
`COLOR_ATTACHMENT | COLOR_ATTACHMENT_BLEND | BLIT_SRC | TRANSFER_SRC` in
`linearTilingFeatures` for `VK_FORMAT_B8G8R8A8_UNORM`, so a linear, arena-backed
colour attachment would work and presentation would need no copy at all. It is
not what is built, for three reasons in increasing order of weight: it moves
every frame's render target out of `DEVICE_LOCAL`, which is the one thing §9.1's
ordering exists to prevent; it drags `VkExternalMemoryImageCreateInfo` onto
*every* forwarded image, which §0.5 item 3 already identified as unconditional
and therefore global; and the copy is 1.47 ms at 1080p, which is not where the
time goes. The zero-copy variant is recorded as measured-possible rather than
assumed-impossible, and it becomes attractive the moment the boundary is a real
VM rather than a socket.

**7. `d3d11-probe.c`'s triangle was wound the wrong way and was being back-face
culled** — on every implementation, including RADV with no forwarding at all,
which is how it was finally established. D3D11's default rasterizer state is
`CULL_BACK` with `FrontCounterClockwise = FALSE` and its viewport transform puts
y down, so the vertices as listed were back-facing and every pixel was culled.
The frame came back as the clear colour and nothing anywhere reported an error.
It was invisible until this stage because DXVK never reached a draw: it refused
the device over `VK_KHR_swapchain` (§0.5). **A control run against the real
driver is worth more than any amount of reading**, and it took two minutes.

**What is not settled**, and is the honest state of this stage:

- **`vkQueuePresentKHR` waits.** It submits the copy, waits on a fence, and only
  then hands the pixels to the display target. §9.3 wants a timeline and §10
  wants triple buffering; what is built is a queue of length zero, which is why
  `IMMEDIATE`, `MAILBOX` and `FIFO` are all advertised and all mean the same
  thing. It is correct, it never tears, and it costs 1.47 ms of the application's
  frame at 1080p. The fix needs the timeline mirror wired to submissions, which
  is the same thing `vkQueueSubmit` has been waiting for since §0.3.
- **`vkGetImageMemoryRequirements` passes the driver's `memoryTypeBits`
  through**, while `vkGetBufferMemoryRequirements` is patched to §9.1's numbering
  (`icd::get_buffer_memory_requirements`). The two sides number memory types
  differently, so an application indexing our four-entry table with RADV's mask
  is reading somebody else's answer, and it works today only because RADV's type
  0 and ours are both the device-local one. `wsi` sidesteps it by choosing type 0
  outright rather than consulting the mask. **The image path should be patched
  the same way as the buffer path**; it is a latent version of §0.4 item 3.
- **Presentation to X costs an `XSync` per frame.** `XSetErrorHandler` is
  process-global rather than per-`Display`, so the ICD installs its own handler,
  syncs to be sure its own errors have been consumed, and restores the
  application's. Without a handler Xlib's default calls `exit(1)` — which it did,
  killing `glxinfo` with `BadDrawable` from `XCreateGC` on a window the
  application had already destroyed. A driver that aborts the application because
  a frame could not be shown is worse than one that drops the frame.
- **No Wayland, and no compositor.** §10 stage 3's damage rectangles and video
  encode are untouched, and the phase-2 compositor is untouched.
- **`glxgears` needs `LIBGL_KOPPER_DRI2=1` on Xvfb.** Not because kopper needs
  DRI3 — its whole point is that the swapchain is Vulkan's — but because Mesa's
  *GLX* refuses the visual for lack of DRI3 before zink is reached at all. §0.4
  read that wall from the other side and concluded glxgears could not run; the
  wall is one layer higher than it looked.

### 0.7 What enforcing the boundary rule settled

Built 2026-08-13. **The cold path passes no descriptor.** §1a is now an invariant
of the document rather than a property one axis happened to have, and the Linux
axis was brought into line with the Windows one rather than the other way round:
`sv-hostd` creates a **named** object under `/dev/shm` and sends its name;
`sv-gpu-server attach` and the ICD open it themselves; `sv_transport::cold` has
no `sendmsg`, no ancillary data and — for the first time — **no `unsafe` at
all**. `scripts/sv-memfd-path.sh` passes over it unchanged, which is the
statement that matters: the daemon logs the object it created, the GPU side logs
the name it opened, and nothing was handed over.

Six findings. The first three are the reason the rule was worth stating; the
fourth is the one that stops it being satisfiable everywhere, and it is a
measurement rather than an opinion.

**1. `SCM_RIGHTS` was not an implementation detail of the same-machine loop, it
was in the protocol.** `ColdMessage` was framed by a function that took a
`BorrowedFd`, so every message on the cold path was a message only a Unix socket
could deliver, on a path D10 says is virtio-serial first and vsock later. The
Windows half had already noticed — `sv_transport::stream` exists because "an fd
cannot cross this boundary" — and the conclusion drawn there was that the
*cross-VM* path avoids descriptors, not that descriptors should not exist.
Stating it as an invariant makes the difference visible: one file changed shape
and four call sites simplified.

**2. A name costs nothing at run time, and the acknowledgement it needs is worth
having anyway.** The handoff happens once, at connect, so there is nothing for it
to cost per call — but "should not move" is a prediction and predictions get
checked. `scripts/sv-bench.sh` was run four times against each tree, alternating,
so that a loaded machine hit both equally. Forwarded glxgears: **master 557, 613,
560, 553 FPS; this branch 583, 592, 524, 713 FPS.** The difference is not
measurable, and the reason it is not is visible in the *control* run of the same
four pairs — zink on RADV with no forwarding at all measured 692, 697, 795, 721
and 682, 665, 617, **357** FPS. A native control that varies by a factor of two
is the measurement floor of a desktop with a load average of twelve, and it is
wider than anything a once-per-connect handoff could do. **The absolute numbers
here are not comparable with §0.6's**, which were taken on an idle machine; what
is comparable is the pair, and the pair says nothing moved.

What the name *does* need is a reply — the daemon must
not unlink the object before a client has opened it — so the client now sends
`Accept` after it has mapped, which is §4.3 step 5 finally being used for
something. That reply also turns "the client could not open the name" into a
line in the daemon's log rather than `NotConnected` three layers up.

  **The reply is not optional and the cost of believing otherwise was an
  afternoon.** The first version unlinked as soon as the client loop finished,
  reasoning that unlinking releases the name and not the memory. It does — but a
  client that has been *sent* the name and has not yet *opened* it is between the
  two, and `sv-gpu-server serve` had no reply to wait for. The ICD reported
  `open(named object) failed: errno 2`, Mesa reported
  `ZINK: vkCreateInstance failed (VK_ERROR_INCOMPATIBLE_DRIVER)`, and
  `sv-zink-gl.sh` failed with "a gear is missing" — three layers of message, none
  of which mentions a name. **A named object needs an owner for its name in a way
  a memfd never did**, and the two obligations that follow are: unlink at the
  first moment every client has it open (not at exit — no `Drop` runs for
  `SIGKILL`, and every harness kills the GPU side, which leaked a 16 MiB
  `/dev/shm` object per run until this was found), and never before.

**3. `sv-hostd`'s two modes needed two different kinds of name, and only one of
them is free.** In `Mode::Local` the daemon creates the object, so it names it.
In `Mode::Qemu` the object is QEMU's memory backend and the daemon does not own
it: if the guest is on `memory-backend-file` the name is the `mem-path` and
everything is tidy, and if it is on `memory-backend-memfd` — which is what every
measurement in this project was made against — **the object has no name but
`/proc/<qemu-pid>/fd/<n>`**. That is legal under §1a, and it is worth being
explicit about what it costs rather than hiding behind the permission: it moves
the `/proc` open from the daemon to every ICD, which weakens §3's argument that
the daemon exists so that "an arbitrary application process" need not do it. It
never reaches the GPU side on any topology — the GPU side is in the VM and
locates its own memory by content (§5.1) — so the invariant holds, but a real
deployment should prefer `memory-backend-file` and get a name that is a name.

**4. The local-udmabuf answer for RADV works across a hypervisor and cannot work
on one machine, and the reason is that a named object cannot be sealed.** §1a
permits the GPU side to wrap memory it already maps in a dma-buf of its own —
nothing crosses — and `/dev/udmabuf` is how Linux does that. But
`UDMABUF_CREATE` requires `F_SEAL_SHRINK` on the object it is given, and a file
under `/dev/shm` is created with `F_SEAL_SEAL` already set: `F_GET_SEALS` returns
1 and `F_ADD_SEALS(F_SEAL_SHRINK)` returns `EPERM`. Only
`memfd_create(MFD_ALLOW_SEALING)` produces a sealable object, and a memfd has no
name. So the two properties the same-machine topology needs — *nameable* so that
§1a's handoff works, and *sealable* so that udmabuf will take it — are mutually
exclusive on Linux, and no amount of code changes that. Across a hypervisor the
conflict does not arise, because there the object is QEMU's own memfd, sealed by
QEMU, reached by path rather than by handoff. Measured 2026-08-13 and pinned by
`cold::tests::a_named_object_cannot_be_sealed_and_therefore_cannot_back_a_udmabuf`.

**5. `Provider::GpuExport` therefore survives, as a marked exception, and the
honest statement is that the rule is not satisfiable for RADV on one machine.**
The intention was to delete it: NVIDIA's refusal of host pointers was corrected
in §0.5's stage (it imports a `MAP_SHARED` mapping of shmem, and a `/dev/shm`
object is shmem), so the provider looked like a leftover. It is not, because on
this host the only real GPU is RADV, and all three routes are closed:

| route for RADV | measured, 2026-08-13 |
|---|---|
| host pointer into a named object | `vkAllocateMemory: ERROR_INVALID_EXTERNAL_HANDLE` — structural, `AMDGPU_GEM_USERPTR_ANONONLY` |
| a dma-buf the GPU side makes locally from that object | impossible: the object cannot be sealed (finding 4) |
| …from a sealable memfd instead | `/dev/udmabuf`: `Permission denied` — `root:kvm` 0660, this account is not in `kvm` |

> **[CORRECTED — 2026-08-13, §0.10.] That last row is a fact about the wrong
> host.** It was measured on **devhost**, where `sirati` is in
> `users wheel lp networkmanager scanner libvirtd` and the device is
> `crw-rw----+ root kvm`. On **testhost** — the machine the VM runs on, and the
> only machine where the cross-VM question arises — the same account is in
> `302(kvm)`, `/dev/udmabuf` opens, and a `UDMABUF_CREATE_LIST` over QEMU's
> memfd succeeds. Both were re-measured. The row stays as written because it is
> true of devhost and devhost is where the five local harnesses run;
> what was wrong was reading it as a property of the project.
>
> This is the same shape as `flake.nix`'s RTX 5090 note: a measurement taken on
> one machine, recorded without the machine in it, and then read as a limit.

Five harnesses — `sv-end-to-end.sh`, `sv-zink-gl.sh`, `sv-dxvk-d3d11.sh`,
`sv-present.sh`, `sv-bench.sh` — are the project's only evidence against a real
GPU, and every one of them runs the GPU side on RADV through this provider.
Deleting it would delete that evidence. So it stays, quarantined in
`sv_transport::localfd`, which is a module whose documentation is mostly about
why it should not exist; `Attachment::arena_object` is empty on exactly that path
and nowhere else, so "which handoff is this" is a field rather than a habit. Two
things narrowed while it stayed: the control region is a named object on *every*
path, so the exception is one object rather than two, and NVIDIA moved to the
named provider because it can.

**The single change that would remove the exception is a group membership**, not
a design change: `/dev/udmabuf` in the `kvm` group would let the GPU side wrap a
sealable memfd locally — which the app side would have to name some other way, so
it is not sufficient on its own — and, more usefully, would unblock the
*cross-VM* AMD topology that `Provider::for_driver_across_a_vm` currently answers
`None` for. That answer is a permission, not a limit, and §5.4 should be read
that way.

> **[CONFIRMED — 2026-08-13, §0.10.]** On testhost the membership is already
> there, and the whole chain runs: QEMU's memfd carries `F_SEAL_SHRINK`,
> `/dev/udmabuf` opens, `UDMABUF_CREATE_LIST` over the guest's pages returns a
> dma-buf, and reading the guest's own records back through it shows it is the
> same memory. So "a permission, not a limit" was right, and on the machine that
> matters the permission is held.

**6. An `Attachment` needed two names, and that is §4.1 becoming visible.** The
control region and the arena are "two independent mappings with independent
bases", and until now the wire hid whether they were one object or two behind
*how many descriptors arrived* — a discriminator that only exists on a Unix
socket. Two name fields say it outright, they are equal on every path where one
object holds both, and the Windows path fills them in with the section names both
sides already derive from the session string. A protocol that could not be
described without saying "count the file descriptors" was a protocol with a
same-machine assumption in it.

### 0.8 What the video decode stage settled

Measured 2026-08-13, `docs/video-decode.md` and `scripts/sv-video-decode.sh`.
**H.264 decodes, on all three GPUs this project can reach, byte-identically to
libavcodec — and none of it crosses the boundary.** The new crate is
`video-probe`; nothing else in the tree changed except a profile that is in the
tree precisely because it does not build.

```
RADV RAPHAEL_MENDOCINO   24 frames, median 0.806 ms, sha256 b3a1374e…
NVIDIA GeForce RTX 5090  24 frames, median 0.116 ms, sha256 b3a1374e…
Intel UHD Graphics       24 frames, median 0.664 ms, sha256 b3a1374e…   ← in the testhost guest
ffmpeg -pix_fmt nv12                                sha256 b3a1374e…
```

Seven findings, and the first is the one that decides whether this is worth
building.

**1. The wall is a *type*, not a queue and not a session.** Adding
`VK_KHR_video_queue`, `VK_KHR_video_decode_queue` and `VK_KHR_video_decode_h264`
to a profile resolves cleanly and then fails in `plan.rs` on
`VkVideoDecodeH264CapabilitiesKHR.maxLevelIdc`, because `StdVideoH264LevelIdc`
is declared in `vk.xml` only as `requires="vk_video/vulkan_video_codec_h264std.h"`
and §6.1 refuses an external type. Six such types are on the H.264 decode
surface. §7.1's "one XML file and nothing else" is the design that makes the
generator trustworthy and it is exactly what stops here; the profile is kept as
`vk_registry::profiles::video` so the wall is ten seconds away rather than an
afternoon. **Two of the six are used by value**, which also makes `abi.rs`'s
zero-sized `AbiType::Opaque` wrong for them — latently, since nothing reaches it
today.

> **[CORRECTED — 2026-08-13, §0.9.]** The wall came down the next day and
> §7.1's design survived intact: the generator still reads XML and nothing else,
> it now reads *two* files. `video.xml` ships beside `vk.xml` in the same
> `vulkan-headers` package and is the registry the `vk_video/*.h` headers are
> generated from. The by-value problem was **not** latent — it was live in the
> zink profile, on `xcb_visualid_t`. §0.9.

**2. §0.6's rule has its first real second candidate, and it points at a smaller
protocol rather than a larger one.** "A capability that is expressible in the
forwarded API does not deserve a protocol extension" — decode is not expressible,
because the generator cannot describe the types. But the fix is not to make the
whole of Vulkan Video cross. If the **GPU side owns the video session, the DPB
and the decode queue privately**, what crosses is a bitstream and an image id,
the second ring D8 would otherwise demand never exists, and the app side never
sees a queue family it cannot use. That is the shape the topology wants anyway,
and it is what §11's future work should say.

**3. Both video decode queue families in play cannot copy.** RADV's is
`VIDEO_DECODE` alone and the guest Intel's is `VIDEO_DECODE` alone; only NVIDIA's
also carries `TRANSFER`. So a decode and its readback are two queues on two
families even before any forwarding, which `video-probe` handles with
`VK_SHARING_MODE_CONCURRENT` and a semaphore. §0.4 item 7's "one synthesised
family" is a decision this stage does not break, but it is the first workload
that would.

**4. `DPB_AND_OUTPUT_COINCIDE` and `DPB_AND_OUTPUT_DISTINCT` are both real and
both had to be implemented.** NVIDIA reports only `COINCIDE` — the decoded
picture *is* the DPB slot; RADV and Intel report `DISTINCT`. An app side cannot
choose between them, so anything forwarding decode carries both.

**5. Session memory ranges from nothing to four allocations** — Intel 0 binds,
RADV 1 of 177,152 bytes, NVIDIA 4 totalling 294,912. Another count-then-fill on
the critical path, which §0.4 item 5 and §0.5 item 4 have now each been got
wrong once in opposite directions.

**6. The capability numbers of Vulkan Video are not yet believable, and the two
RADVs in our own dev shell disagree.** Mesa 26.1.1 reports H.264 `maxLevelIdc`
as **14** (the `StdVideoH264LevelIdc` enum value for level 5.1); Mesa 26.2.0, on
the same hardware on the same day, reports **52** (the bitstream `level_idc`
byte). Intel reports **1**, i.e. level 1.1, beside a 4096×4096 coded extent, and
reports H.265 `decodeFlags` with *neither* required bit set while
`vkGetPhysicalDeviceVideoFormatPropertiesKHR` returns no formats for a profile it
says is supported. The *profile* query — will you take this codec, chroma format
and bit depth — is answered honestly by every driver. The numbers attached to
the answer are not, and §9.2's "advertise what the driver reports, never
synthesise" needs the qualifier that some of what it reports is wrong.

**7. Firefox is no longer the obstacle the phase-2 research said it was, and the
new obstacle is §1a.** Firefox 153 (2026-07-21) ships a Vulkan Video decoder,
via ffmpeg's `AV_HWDEVICE_TYPE_VULKAN`, opt-in behind
`media.hardware-video-decoding-vulkan.enabled`. It would *select* a forwarded
device — it accepts `VK_PHYSICAL_DEVICE_TYPE_VIRTUAL_GPU`, and it requires only
`VK_KHR_video_queue` and `VK_KHR_video_decode_queue` to do so. It then exports
every decoded frame as a **Linux dma-buf**, with no host-memory fallback
anywhere in the path. A dma-buf is a host kernel object, the frame is in a
Windows guest, and a dma-buf is a file descriptor besides.

Read from source rather than from search results — `FFmpegVulkanVideoDecoder.cpp`
at `265f4b8f`, 2026-08-07 — because the claim decides whether hardware decode is
reachable at all. Four refinements, and the conclusion survived all of them:

  - **The "copy path" copies into another *exportable image*, not to the CPU.**
    `media.hardware-video-decoding-vulkan.direct-export.enabled` defaults off,
    so the copy path is the only one that runs; it is `vkCmdCopyImage` into a
    ring of 32 images created `VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT` with
    `VkExportMemoryAllocateInfo{DMA_BUF}`, and then `vkGetMemoryFdKHR`. The file
    contains no `vkCmdCopyImageToBuffer`, no `av_hwframe_transfer_data` and no
    `vkMapMemory`. The pref chooses between *zero copies and one copy*, not
    between GPU and host.
  - **Every frame carries a second descriptor**, an exported semaphore
    `sync_fd` handed to `DMABufSurface::SetSemaphoreFd`. §1a rules out both.
  - **§0.8 item 3's queue-family awkwardness is already solved upstream.**
    Firefox's `InitCtx` scans for a transfer-only family itself and falls back to
    ffmpeg's `queue_family_tx_index` and then to family 0, so neither RADV's nor
    the guest Intel's `VIDEO_DECODE`-only family is part of the gap.
  - **The failure would be fatal rather than a fallback.** Init-time failures
    fall cleanly through `InitHWDecoderIfAllowed` to VA-API and then software;
    the first-frame failures — `"Failed to create NV12 image"`, `"Failed to
    export NV12 FD"` — are `NS_ERROR_DOM_MEDIA_FATAL_ERR`, and
    `MediaFormatReader`'s hardware-to-software recovery is guarded by
    `!decoder.HasFatalError()`, which admits only `DECODE_ERR` and
    `NEED_NEW_DECODER`. Turning the pref on against a forwarded device does not
    degrade to software; it breaks playback.

The gap is one output path in one Firefox file — a *third* one, since neither
existing branch can be redirected: everything below `GetVideoFrameSurface` is
typed on `AVDRMFrameDescriptor`. `docs/video-decode.md` §5 has the code.

### 0.9 What the video wire stage settled

Built 2026-08-13, `docs/video-decode.md` §4a, §4b and §6. **§0.8's wall is
down: the generator carries the `StdVideo*` types, the video profile builds the
whole stack, and the H.264 capability path crosses the boundary byte-identically
to the driver's own answer.** The decode itself still does not run, and the
reason is now the one §0.8 item 2 already named.

**1. The answer to "how should the generator learn about `vk_video`" was to read
the other registry.** §0.8 item 1 framed the choice as parsing C headers or
hand-writing layouts, and said either would put a second source of truth about a
C layout into the project. Neither was needed. Every `vk_video/*.h` header opens
with *"This header is generated from the Khronos Vulkan XML API Registry"*, and
that registry is `share/vulkan/registry/video.xml` — same package as `vk.xml`,
same schema, same parser. `vk_registry::Model::merge_video` is the whole change;
`§7.1`'s "one XML file and nothing else" becomes "two, from one pinned store
path", which is the same property.

The relationship to the headers is now the one `sv-testkit::cheader` already had
to `vulkan_core.h`: **they are the check, not the input.** They are in
`header_paths()`, so every `STD_VIDEO_*` extent and every `StdVideo*` enum value
is compared alongside the rest of the API; `check_structs` compares each
`StdVideo*` struct's members and array extents; `check_flag_words` compares each
bitfield struct's field order, widths and offsets. Three negative controls plant
the faults that matter.

**2. The zero-sized `AbiType::Opaque` was a live bug, not a latent one.** §0.8
item 1 said "latently, since nothing reaches it today". Refusing an external
type used by value found `vkGetPhysicalDeviceXcbPresentationSupportKHR`'s
`xcb_visualid_t visual_id` — in the **zink** profile, on the shipping C surface,
declared as a zero-sized incomplete type in an `extern "system"` signature. It
has never had a symptom because that entry point exists only so the loader's
terminator does not `abort()` and it ignores the argument.
`exceptions::PLATFORM_SCALARS` is now a closed two-entry table with the header
line that gives each width. This is the third realised instance of R8's shape —
a wrong answer with no symptom — and the second one found by adding an assertion
rather than by hitting it.

It is also the project's other recurring pattern, and the pattern deserves the
name rather than a third separate note: **"implemented" here has more than once
meant "compiled".** The generated `extern "system"` signature existed, the ICD
exported it, the loader found it, and nothing anywhere read the parameter — so
every check the project had said yes. The same shape produced §0.2's 55-vs-239
command set and §0.3's null function pointers. What breaks it is always the same
move: assert against something outside the code that says what the answer
*should* be.

**3. Bitfields come back into scope, narrowly and by data.** §7.4 put them out
because `vk.xml` has one bitfield struct and it is ray tracing. `video.xml` has
**293 members of the form `uint32_t x : 1`**. `plan::FlagWordPlan` carries a
struct whose members are *all* `uint32_t` bitfields fitting one word as that
word — `#[repr(transparent)] struct StdVideoH264SpsFlags(pub u32)` — and refuses
anything that mixes bitfields with ordinary members, so
`VkAccelerationStructureInstanceKHR` stays excluded and its exclusion stays a
tripwire. **The wire does not depend on the bit order**, because the word is
copied and never taken apart; only the generated accessors do, and the layout
they assume is compared against the headers field by field.

**4. §0.8 item 1's "these have to be deep-copied by hand" was wrong.**
`StdVideoH264SequenceParameterSet`'s three pointers are described by
`video.xml`'s own `len=` and `optional=`, so the ordinary generator produces
`Vec<i32>` and `Option<Box<…>>` and walks them like any other pointer graph. The
thing that made them look unwalkable — no `sType`, no `pNext` — only defeats a
*generic* walker, and this generator is not one.

**5. Two-dimensional arrays existed and had never been reached.**
`StdVideoH264ScalingLists::ScalingList4x4` is `uint8_t[6][16]`. The GPU side's
`raise` asked the whole member for its element type at both levels and emitted a
loop pushing a `u8` into a `Vec<[u8; 16]>`; the header cross-check compared the
*product* of the extents, which calls `[6][16]` and `[96]` the same thing. Both
were found by a profile reaching them and both are fixed. Neither is
video-specific and neither was reachable from any earlier profile.

**6. Merging a second registry is inert for a profile that does not reach it.**
The zink `protocol_hash` is `0x44c19f913c79a5fa` before and after, and the ICD
and GPU-side command counts are unchanged. Stated because "we added a registry
and the wire did not move" is the claim that makes the change safe to land.

**7. The capability path crosses, and the numbers are the driver's.** With
`SV_PROTO_PROFILE=video`, the forwarded device advertises
`VK_KHR_video_queue`, `VK_KHR_video_decode_queue` and `VK_KHR_video_decode_h264`
and answers `vkGetPhysicalDeviceVideoCapabilitiesKHR` and
`vkGetPhysicalDeviceVideoFormatPropertiesKHR`. The 44-line H.264 block of
`video-probe caps` is **byte-identical** to the same probe run directly against
the same RADV, `sha256 9c1dca7512d844d6…` — including `maxLevelIdc`, the
`StdVideoH264LevelIdc` that stopped the planner, and a `pNext` chain carrying
two codec-specific capability structures. §1 says these numbers are not
believable, which is exactly why forwarding them had to be checked against the
driver rather than against a range.

**8. The remaining wall is the one queue family of §0.4 item 7, and it is not to
be removed by adding a second one.** `video-probe decode` through the forwarder
stops at "no queue family with `VIDEO_DECODE`". Advertising a decode family would
mean a second ring (D8), a family the app side can see, and building the design
§0.8 item 2 rejects — so this stage did not. What is measured is that everything
below that decision works, and that what remains is a stage: the GPU side owning
the session, the DPB and the decode queue privately, answering a protocol
command the generator cannot write. §11's future work.

One thing about the harness rather than the result. **A `pNext` that the
specification requires and the driver does not check is a segfault with no
message**: `vkGetPhysicalDeviceVideoCapabilitiesKHR` with a bare
`VkVideoCapabilitiesKHR` kills the process inside RADV before anything is
printed. Every other mistake in this stage produced a `VkResult`.

---

### 0.10 Where the driver's own memory lives, measured

Measured 2026-08-13 on **testhost**, `docs/measured-gpu-side.md`, "Where the
*driver's* memory lives, and a dma-buf made out of it". Asked because §0.7's
`Provider::for_driver_across_a_vm` answers `None` for the one driver that cannot
import a host pointer, and because the reason recorded for that was a permission
measured on a machine with no VM on it.

**Five results, and they do not all point the same way.**

**1. The guest driver's own `vkAllocateMemory` is ordinary guest RAM.** 64 MiB
through each of the Intel iGPU's two host-visible memory types; **16384 of 16384
pages of each were found in QEMU's `memory-backend-memfd`**. Not stolen memory,
not behind a BAR, nowhere the host cannot map. The `x-igd-opregion=on`
passthrough does not change that for ordinary allocations.

**2. It is nearly as contiguous as the best allocator this project found, and
needs no privilege.** 26–55 runs for 64 MiB, against `VirtualAlloc(MEM_LARGE_PAGES)`
at 22, `VirtualLock`ed 4 KiB pages at **1471** and AWE at 5863–16339 (§0.1). So
attaching a slab this way is tens of `mmap(MAP_FIXED)` calls rather than about
fifteen hundred, and `SeLockMemoryPrivilege` — which §0.1 had to have granted
before large pages were measurable at all — does not enter into it.

**3. The host can write it and the driver's own mapping sees the write.**
`arena-host write1` through `MAP_SHARED` of the memfd into all 16384 located
pages of each type, read back through the `vkMapMemory` pointer the driver
returned: **16384/16384 on both types, each at the page index the host
computed.** A run in which the host deliberately did not write reported 0, which
is the negative control for the same path.

**4. A real Linux dma-buf can be made out of it, on the Linux side, with nothing
crossing.** QEMU's memfd carries seals `SEAL|SHRINK|GROW` — read with
`F_GET_SEALS` from the running process, not from QEMU's documentation — and
`F_SEAL_SHRINK` is exactly what udmabuf requires. `/dev/udmabuf` opens on
testhost, `UDMABUF_CREATE_LIST` over the 31 runs returns a dma-buf, and the
guest's own records read back through it: **3760/3760 pages at the index the map
says.** §1a permits this and says why — *"that fd is made on one side and never
leaves it"* — and it is now a thing that has been done rather than a thing that
was argued.

`/sys/module/udmabuf/parameters/size_limit_mb` is **64**, so §0.1 item 4's 1 GiB
slab is not available this way without changing a module parameter.

**5. And the premise that this removes the sweep and the root requirement is
wrong.** The pages were found by sweeping 13 GiB of guest RAM for the stamp, and
the memfd was reached through `/proc/<qemu-pid>/fd/35`. Neither has anything to
do with *who allocated*: §0.1 item 5 measured that Windows reports no frame
numbers to user mode outside AWE, and this is the driver's memory, so AWE does
not apply to it at all. Whichever side allocates, the host still has to find the
pages, and finding them needs the sweep and the sweep needs root. What inverting
the direction buys is item 2 — fragmentation — and item 4, which is a capability
the other direction does not have. It does not buy an unprivileged design.

**What is not measured, and one of them is the important one.**

- **`DEVICE_LOCAL`-only memory is unmeasured**, because it cannot be mapped and
  so cannot be stamped. That is type 0 on this device and it is exactly where a
  decoded video frame lives — a `VkImage` with `VIDEO_DECODE_DST`/`DPB` usage and
  `OPTIMAL` tiling. On a unified iGPU with one heap it is plausible that type 0
  is also guest RAM; plausible is what this project does not accept. Measuring it
  needs a staging copy or a dispatch that writes the pattern into device-local
  memory.
- **Relocation.** The allocations were held about 17 seconds. R26's 3m14s is
  stronger evidence and was itself explicitly not taken under memory pressure.
- **No GPU read the host's write.** The probe does no dispatch.
- **This is a unified iGPU.** A discrete card passed through would very likely
  put `DEVICE_LOCAL` memory behind a BAR and the answer could be entirely
  different. Nothing here generalises to one.

**The finding that decides what the dma-buf is worth: testhost's Linux host has
no GPU.** `00:02.0` is bound to `vfio-pci` and handed to the guest, there is no
second GPU, and **`/dev/dri` is empty**. A dma-buf is a handle to memory;
importing one into a GL or Vulkan context needs a DRM render node and there is
none. So the dma-buf measured here is real and its consumers on this machine can
only be CPU-side — a `wl_shm`-shaped path with no copy, which is worth having and
is not the same claim as "Firefox can hardware-decode into it".

**Consequences for §0.8 item 2 and for `docs/video-decode.md` §6.** The earlier
conclusion — Firefox's Vulkan decode path ends in `vkGetMemoryFdKHR` on both
branches, so hardware decode needs a Firefox patch — rested on "no forwarded
Vulkan can mint a dma-buf". That premise is now false in general: on testhost a
dma-buf over guest RAM can be minted. It is still true *in the way that matters
for Firefox on this machine*, for a different and more concrete reason: the frame
would have to be device-local memory whose placement is unmeasured, wrapped in a
dma-buf carrying a DRM format modifier, imported by a GPU the host does not have.
Two of those three are open questions and the third is a fact. **The honest
statement is that the blocker moved from "impossible" to "three specific
unknowns, one of which is currently insurmountable on this hardware", and that
`video-decode.md` §5's recommendation — software decode on the host — is
unchanged for now.**

**No code was changed.** `sv_gpu_server::arena`'s provider table still answers
`None` for RADV across a VM and still prefers `AppNamedShm` everywhere else; the
comments now point at this measurement instead of at a permission on the wrong
host. Inverting the direction is a decision that wants the type-0 measurement
first, because an arena that can only be `HOST_VISIBLE` memory is a different
design from one that can be any of it.

---

### 0.11 Firefox, and the one property no instrument measured

Measured 2026-08-13 on **devhost** in `nix develop`, `docs/firefox-accel.md`.
`goal-firefox.md`'s fourth clause — accelerated rendering — had never been tested
with Firefox; the chain Firefox → WebRender → GL → Zink → shared_vulkan was
measured at every link but the first.

**1. Firefox selects the ICD, and GL through Zink through the forwarder works in
both entry points.** `eglinfo` on surfaceless and — new, and the one that matters
— **`glxinfo` under X11**, which is the path Firefox actually takes through
`zink_kopper` and a `VK_KHR_swapchain`, both report
`zink Vulkan 1.3(… RADV RAPHAEL_MENDOCINO …)` at GL 4.6. Firefox's own stderr
carries `sv-icd: connected to the GPU side` and the GPU side logs the attach and
`serving rings`. Nothing about the loader, the manifest, Zink's device selection
or the profile is in the way.

**2. Firefox still cannot use it, and the reason is ours.** With
`LIBGL_ALWAYS_SOFTWARE=1` in the identical harness Firefox runs for the full 60 s
with empty stderr, reports WebGL 2.0 on `llvmpipe, or similar`, and reads back
`clearColor(0.2,0.6,0.9,1)` as exactly `51,153,230,255`. Through the forwarder it
exits within seconds on `poll failed` and `Exiting due to channel error`. The
control establishes that the harness is sound.

**3. `--clients N` is one *shared* session, not N sessions, and the GPU side
serves exactly one per process lifetime.** The accept loop gathers N handshakes
into one control region and one ring set and only then runs a single
`session::serve`, which then holds the socket for 30 s of idle. Firefox forks
`glxtest` before the browser exists; that child takes the session, the browser
gets nothing, Firefox gives up. A respawn loop does not help, because the server
does not exit when its client disconnects.

**4. The measurement that makes it not about Firefox: two ordinary GL programs,
one after another, against one GPU side — the second is not served.** `glxinfo`
then `glxgears` twelve seconds later gave one attach, not two.

**5. And the reason it was never found: every instrument in this project is
single-process.** `sv-end-to-end.sh`, `sv-zink-gl.sh`, `sv-dxvk-d3d11.sh`,
`sv-present.sh`, `sv-bench.sh` and `sv-video-decode.sh` all run one client;
so do `vulkaninfo`, `eglinfo`, `glxgears` and `video-probe`. The one property a
real application needs is the one nothing exercised — §0.9's "implemented meant
compiled" in its third form.

**What it costs to fix** is a stage: one control region and one ring set per
client, an accept loop that spawns sessions rather than falling through to one,
and a decision about whether sessions share an arena (they should — one arena per
device is the design) given that the slab allocator and object table are
currently per-session. Nothing in the ICD, in Zink or in the protocol changes.

**Not answered**: the cost against llvmpipe, which needs a Firefox that runs;
whether WebRender picks a hardware backend, since `about:support` is the arbiter
and Firefox never reached one; and anything on testhost, where §0.10's finding
that the host has no GPU makes "forwarded versus llvmpipe" the only comparison
there is.

**And the copy count**, which is design rather than measurement
(`firefox-accel.md` §7): today a Firefox frame reaching the compositor's buffer
is two copies — a GPU-side `vkCmdCopyImageToBuffer` into a linear arena buffer
(§0.6), then a CPU-side copy from the arena into the compositor's `wl_buffer`.
The second should not exist, and removing it needs a shared handle that is **not
a descriptor**: an arena slab id, an offset and length, and a layout — the same
triple the protocol already carries — with the `wl_buffer` bound to that range
rather than to a copy of it. §1a permits that because nothing crosses; what is
missing is the two sides agreeing on one allocator.

### 0.12 One session per client, and Firefox rendering through it

Built and measured 2026-08-13 on **devhost** in `nix develop`,
`docs/firefox-accel.md`. §0.11's blocker is gone and the fourth clause of
`goal-firefox.md` has moved.

**1. `sv-gpu-server` serves one session per client, each as it arrives.**
`--clients N` now means "accept up to N sessions"; at `N = 1` — what every
existing harness passes — the observable behaviour is unchanged. Per session: a
control region created *after* the accept, a `Control`, a ring set, an `Objects`
table, an arena, and a thread. Per device, behind an `Arc`: the driver, its
entry points, and a queue lock.

**2. The measurement that diagnosed the problem now passes.** Three `glxinfo`
runs one after another against one GPU side gave **three** sessions, where §0.11
measured one. `scripts/sv-zink-gl.sh` still passes all four of its checks —
64.12 dB and 69.82 dB against llvmpipe, glxgears matching — so nothing regressed.

**3. Each session gets its own arena, and that is a departure from "one arena
per device" with a reason.** The tidy answer is one arena partitioned by offset,
and it needs no lock: the `Attachment` already carries `arena_offset` and
disjointness would be established once, single-threaded. It was still rejected,
because **the descriptor names the whole object** — a partitioned arena hands
every client an fd it can map in full, so isolation between clients would be a
convention. A separate allocation makes it a fact, which is R3's argument one
level up. The cost is `N` × `--arena` bytes of device memory and is stated rather
than hidden.

**4. The one shared mutable thing is the queue, and the honest statement is that
the lock does not yet cover it.** Vulkan requires external synchronisation of a
`VkQueue`; the driver opens one, so every session shares it. `Shared::queue_lock`
exists for hand-written queue calls and **the generated dispatch does not take
it** — the objects table is locked per lookup and released before the driver
call, so two ring threads of one session could already race before any of this.
Multi-session widens the race from "two rings of one client" to "two clients"; it
does not create it. Closing it is a change to pass 12. A lock that looked like it
covered the generated path would be worse than one that says it does not.

**5. Firefox renders through the forwarder.** It survives the whole run, attaches
two sessions, gets a **WebGL 2.0** context, and reads `clearColor(0.2,0.6,0.9,1)`
back as exactly `51,153,230,255`. `WEBGL_debug_renderer_info` returns
`Radeon HD 3200 Graphics, or similar` — Firefox's *sanitised* name for a hardware
AMD device — where the software control on the same machine returns
`llvmpipe, or similar`. Firefox is deciding, not being told, and it decided
hardware.

**6. The cost is measured and must not be over-read.** A `gl.clear` +
`gl.finish` loop at 256×256: **7711/s forwarded against 12588/s on llvmpipe,
0.61×**. That is a *latency* benchmark — `finish` is a full round trip — at a
resolution where llvmpipe's rasterisation costs nothing, so it is the forwarder's
worst case and llvmpipe's best. 130 µs per round trip is already well under
RADV's 0.651 ms submit floor, so glthread and Zink's batching are absorbing most
of it. **How much acceleration is worth here is not yet measured**, and §0.10
makes testhost the venue where it matters, because there llvmpipe is the only
alternative.

Not attempted, deliberately: the second copy (§0.11's copy 2). Two copies that
render beat one copy that does not exist.

### 0.13 The resolution sweep, and the control that changed its meaning

Measured 2026-08-13 on **devhost** in `nix develop`,
`docs/firefox-accel.md`. §0.12's 0.61× was a `gl.finish()` microbenchmark at
256×256 and said so; this is the fill-heavy sweep that was supposed to replace
it. `glxgears` under Xvfb, `vblank_mode=0`, four sizes, three paths.

| size | forwarded | zink on real RADV | llvmpipe | fwd / zink-RADV |
|---|---|---|---|---|
| 256² | 920.9 | 1140.3 | 6802.8 | 0.81 |
| 512² | 214.9 | 289.3 | 5431.4 | 0.74 |
| 1024² | 69.4 | 73.0 | 2598.2 | **0.95** |
| 2048² | 18.3 | 18.2 | 806.6 | **1.01** |

**1. The forwarder converges on native as the frame gets more expensive.**
0.81 → 0.74 → 0.95 → **1.01**; at 2048² it is indistinguishable from the same GL
on the same GPU with no forwarding at all. The cost is per frame and fixed, so it
vanishes as a proportion once the frame costs more than the round trip.
`sv-bench.sh` agrees from the other end: `frame-200` 73.7 µs native against
293.3 µs forwarded, and glxgears at its default size 726 FPS against 795, 0.91.

**2. Both hardware paths lose to llvmpipe by 6–43×, and the forwarder is not
why.** Zink on the *real* RADV, no forwarding anywhere, is 0.17× llvmpipe at 256²
and 0.02× at 2048². Under Xvfb there is no DRI3 and no MIT-SHM, so `zink_kopper`
presents every frame through X11 core protocol — 16.8 MB per frame at 2048² —
while llvmpipe renders straight into the X server's memory. A `glxinfo` with no
environment set at all reports `llvmpipe` as the *native* GL under Xvfb: **there
is no hardware GLX path on this machine under Xvfb**, and the forwarder is the
only route to the GPU here.

**3. So the question is not settled, and the first version of this table would
have answered a different one.** The control column failed to start on the first
attempt — setting `VK_DRIVER_FILES` for the *control* breaks Zink, while leaving
it unset works, which `sv-bench.sh` had right — and without it the honest-looking
conclusion was "the forwarder is 43× slower than software rendering". It is not:
real hardware is, in this harness. A true number answering a different question is
§0.9's pattern again, and this time it was caught by adding the control rather
than by shipping it.

**4. What would settle it is a present path that is not X11 core protocol**, and
on testhost that is what exists: the consumer there is waycompose's compositor
reading an **arena slice**, not an X server reading a socket. That makes §0.11's
copy 2 and this measurement one piece of work rather than two — the shared handle
is not an optimisation, it is the thing that makes the comparison meaningful.

**testhost was not reached.** A forwarded number on the GPU-less host needs the
full cross-VM stack — `sv-hostd qemu-serve`, QMP, the memfd sweep, root, a
virtio-serial cold path — on a guest two other experiments are using. So **there
is no forwarded-versus-llvmpipe number on the machine where that comparison is
the only one there is**, and §0.12's "how much acceleration is worth here is not
yet measured" stands.

### 0.14 Where the pages are, asked instead of searched for

Measured 2026-08-13 on **testhost** against the live guest,
`docs/measured-gpu-side.md`, "Asking Windows where the pages are: the PFN
database, measured". Asked because §5.1's locator is a **content scan** — the
guest stamps every page, the host sweeps 13312 MiB for the stamp — which costs
~2 s per attach, needs root and the memfd, and cannot re-locate an extent once
the arena's own data has overwritten the stamps.

**The claim this section overturns is one sentence of §5.1**: *"Windows does not
report frame numbers to user mode outside AWE at all."* It was measured, and
what was measured was `QueryWorkingSetEx`, which is the documented API and does
not. It is not the only one.

**`NtQuerySystemInformation(SystemSuperfetchInformation)` with
`SuperfetchPfnQuery` reports the PFN database to user mode**, one
`MMPFN_IDENTITY` per physical frame carrying the frame number, the owning
process id and **the virtual address it is mapped at**. It is what RAMMap draws
its "Physical Pages" tab from. Six results:

1. **It works on build 26200, and it agrees with the scan page for page.** A
   64 MiB `VirtualLock`ed window located both ways at once gave **byte-identical
   offset tables, 16384/16384 pages, 1472 runs both times**. The host's own work
   drops from **2.308 s to 0.064 s**, because translating a frame list is
   arithmetic and sweeping a memfd is not.
2. **The guest-side dump costs ~330 ms and does not vary with the window** —
   3391496 frames examined whatever the arena is, exactly as the scan reads all
   13312 MiB whatever the arena is. So the honest comparison is 330 ms of *guest*
   CPU against 1.8–2.32 s of host CPU, with no root and no `/proc/<qemu>/fd/35`.
3. **Re-checking a frame list already held costs 0–22 ms**: 512 frames in 0 ms,
   16384 in 1 ms, 262144 in 22 ms, because that query names only the frames it
   cares about. R26's canary was invented to approximate this and can now be
   replaced by the thing itself.
4. **It re-locates a window whose contents are gone**, which is the property
   §5.1 gives up. Every byte of a 64 MiB large-page window was overwritten and
   both locators asked again: the scan **located 0 pages**; the query returned
   the **same 16384 frames, same 26 runs**. The host then wrote through the
   post-overwrite frame list and the guest found **16384/16384 pages carrying
   the host's record at the right page index**.
5. **It does not care who allocated, which is the constraint that had to
   survive.** The driver's own `vkAllocateMemory` memory — §0.10's direction,
   the one in which the arena *is* the GPU's allocation with no copy between
   them — located **16384/16384 on both host-visible types, identical to the
   scan**, and the host's writes arrived at every page index through the
   driver's own `vkMapMemory` pointer. The query asks by virtual address; a
   driver's mapping is a virtual address.
6. **It needs no privilege this guest does not already hold.**
   `SeProfileSingleProcessPrivilege` and `SeDebugPrivilege` are default rights of
   `BUILTIN\Administrators` and were already enabled on the account. No policy
   change, no new SID, no reboot, no code-integrity setting — strictly less than
   R25, which had to be granted. **The cost is that the interface is
   undocumented**, and that is the real argument against it (§5.1, R28).

**The route the requirement named is closed, and closed on four counts.**
virtio-fs DAX: QEMU 10.2.2's `vhost-user-fs-pci` has no `cache-size` property
and upstream never merged the fork's DAX patches; virtiofsd 1.13.3's
`setupmapping`/`removemapping` are `ENOSYS` stubs and its only DAX MR is open and
written against Cloud Hypervisor; and the Windows `viofs` driver mentions
`FUSE_SETUPMAPPING` only inside a copied Linux header — its VirtIO library
declares `capabilities[VIRTIO_PCI_CAP_PCI_CFG]`, five entries, so a shared-memory
capability of type 8 is structurally undiscoverable by it. The fourth count is
the one that would matter even if the other three were fixed: **the DAX window is
a PCI BAR aperture** (virtio spec §5.11.6.4, shared memory region 0), populated
sparsely and revocably by `FUSE_SETUPMAPPING`. That is the same object §5.1
rejects under the name ivshmem, for the same reason — a BAR is not ordinary RAM
and cannot be what `vkAllocateMemory` hands back. DAX would have solved locating
by making the memory unable to be the GPU's.

**Two other routes were priced and neither is taken.** A guest kernel driver
(`MmProbeAndLockPages` + `MmGetMdlPfnArray`) gives exact frames with no dump at
all, and Secure Boot is *not* enabled in this VM so test-signing would in fact
load — but R6 forbids permanent test-signing on this machine and attestation
signing costs an EV certificate, which is exactly why §5.1 dropped the pinning
driver. It now buys strictly less than it did then: the PFN query already
delivers the frames. And walking the guest's page tables from the host is
reachable — MemProcFS/LeechCore attaches to a live file-backed physical memory
source, and `RegionTable::gpa_to_offset` is already the remap it needs — but
QMP gives only the *current* vCPU's CR3, so choosing a process means Windows
introspection (kernel base, `_EPROCESS`, `_KPROCESS.DirectoryTableBase`) with
build-specific offsets. The PFN query's owner field is a **process id, not a page
directory base** (measured), so it does not shortcut that. It stays a recorded
alternative, not a plan.

**One number in §0.1 does not survive contact with a guest that has been up for
two days.** §0.1 item 4 measured a 1 GiB large-page window as **1 run**, on a
guest with 1.3 hours of uptime. The same allocation today is **399 runs**, and
64 MiB is 26 rather than 22. The conclusion — large pages beat the alternatives
by two orders of magnitude — is untouched; the claim that the whole arena
arrives as one or two `mmap` calls is not true on a guest in ordinary use, and
§5.7 is corrected.

**What changes in the design.** §5.1 gains a fourth locator and `Locator::Pfns`
stops being an AWE-only path; the wire type already exists
(`sv_transport::cold::Locator::Pfns`). The recommendation is stated there and is
deliberately not "delete the scan": the query is undocumented, so the scan runs
**once**, at attach, as the cross-check that the query is telling the truth, and
the query is what re-locates and re-validates afterwards — 2 s paid once,
1 ms per re-check thereafter, and a broken interface caught loudly on the one
occasion the stamps still exist.

### 0.15 The conformance suite, and what the control did to its number

Built and measured 2026-08-13 on **devhost** in `nix develop`,
`scripts/sv-cts.sh` against `vulkan-cts-1.4.6.1` on RADV RAPHAEL_MENDOCINO.
**`deqp-vk` runs against the forwarded ICD.** Every instrument in this project
until now judged one workload; this one judges the API, which is the only thing
that can answer a claim about *surface area*. `sv-zink-gl.sh` renders a correct
frame through 239 forwarded commands without saying a word about the other 501.

`dEQP-VK.api.smoke.*` — the traditional first target — is **6/6 through the
forwarder**, identical to the same six on the same RADV with no forwarding, and
the ICD's `sv-icd: connected to the GPU side` plus the GPU side's session log
are what say which implementation ran.

*(The numbers in the table are the 1-in-20000 sample, which completes. The
1-in-2000 run was attempted and **stopped deliberately**; what it measured
before it was stopped is item 10, and it is the most important number in this
section.)*

Two independent runs of the same 164 tests, because one run of a thing that
loses devices is a sample of one:

| | control (RADV direct) | forwarded, run A | forwarded, run B |
|---|---|---|---|
| Pass | **96** / 96 | **24** | **29** |
| NotSupported | 68 / 68 | 126 | 126 |
| Fail | 0 / 0 | 1 | 1 |
| DeviceLost | 0 / 0 | 12 | 8 |
| ResourceError | 0 / 0 | 1 | 0 |
| total | 164 | 164 | 164 |
| wall | 1 s / 5 s | 52 s | 91 s |

**The control is identical to the test between the two runs and the forwarded
column is not.** 96/68/0/0 both times, against 24 versus 29 passes and 12
versus 8 device losses — *the same tests, same binary, same driver, minutes
apart*. Whatever loses the device is timing-dependent, and any single forwarded
number here carries a variance of about five tests in 164.

**The join is the answer and the totals are not.** Run A: 72 tests pass on RADV
and not through the forwarder; **0** go the other way. Of those 72, **59 are
`NotSupported`** — the forwarded device declining something it does not
advertise, which is the profile and not a defect — **12 are `DeviceLost`** and
**1 is `Fail`**. Run B: 67, of which 58 `NotSupported`, 8 `DeviceLost`, 1
`Fail`. So the honest sentence is *nine to thirteen forwarding defects and
fifty-eight to fifty-nine gaps in the declared scope*, and no arrangement of the
totals says that. The skip half is stable to one test; the defect half is not.

Ten findings. The first four are about the instrument, and they are here
because each was a wrong number first; the tenth is the one that decides what
the next stage of this project is.

**1. The standard runner reported zero passes for a driver that passes.**
`deqp-runner` 0.18.0 is what Mesa CI uses and it does not know deqp's
`DeviceLost` status: `Failure getting run results: unknown dEQP status
'DeviceLost'` abandons the entire group, and every test in it is written out as
`Missing`. On this machine that happened to four groups out of four in a
**control** run against unforwarded RADV — 0 Pass, 0 Fail, 33 Missing, for a
configuration that had just passed 6/6. A tool that turns a driver's honest
answer into an absence of data is worse than no tool, and the chunking that
replaced it is thirty lines.

**2. The mustpass lists shipped beside the binary are not the binary's tests.**
362 of a 1451-test sample of `vk-default/*.txt` name tests the `deqp-vk` in the
same derivation does not have — all format combinations, e.g.
`…astc_10x10_srgb_block.b8g8r8_srgb.linear_optimal_nearest` where only the
`optimal_optimal_*` variants are generated. A quarter of the first scoreboard
was that. The caselist is now `--deqp-runmode=stdout-caselist` — 3,279,369
tests, the binary describing itself — and is cached under the store hash it came
from so a flake bump invalidates it by name.

**3. `DeviceLost` is contagious, and the first attempt to correct for it was a
heuristic that was wrong.** deqp holds **one** `VkDevice` for a whole
invocation, so the test that loses it takes every test after it in that process.
The first forwarded run reported twelve `DeviceLost` verdicts including
`dEQP-VK.ubo.3_level_array.std140.mat3x2.vertex_comp_access`, which passes on
its own every time. The fix attempted was "keep the first `DeviceLost` of a
round as the culprit, retry the rest", and the next run said plainly that it was
wrong: sixteen of them, the same innocent test among them. **The culprit does
not have to report `DeviceLost`; it can report `Pass` and leave the device
wedged for whoever comes next**, so no ordering heuristic can find it. What
works is exact and dull: every non-benign verdict is re-run **alone**, one
process and one GPU side per test, and the isolated verdict is the one that
counts. Eight verdicts of twenty changed under isolation.

**4. A GPU side that outlives its client wedges the machine, and the symptom
accuses the wrong thing.** An interrupted run left four `sv-gpu-server`
processes behind. `amdgpu: ring gfx_0.0.0 timeout, signaled seq=875426, emitted
seq=875428` then appeared on *every* subsequent invocation, and plain
unforwarded `dEQP-VK.api.smoke.*` went from 6/6 to 2/4 and aborted — which reads
as "CTS breaks RADV" and is not that. Killing the four restored 6/6 immediately.
The harness now records every server's pid and kills them on exit. Two things
follow: a leaked GPU side is not a leaked process, it is a leaked *driver
context with work queued on it*; and the control is what made the accusation
falsifiable in ten seconds.

**5. Presented as a deadline, a timeout is the wrong instrument for a hang.**
A chunk of 200 forwarded tests legitimately takes ten minutes and a hang is one
test that never returns, so a deadline generous enough for the first costs
fifteen minutes per instance of the second. Two tests hang the forwarder at this
sample — `dEQP-VK.memory.allocation.random.79` and a
`synchronization.timeline_semaphore.device_host…` case — and they turned a
25-minute run into an open-ended one. What distinguishes work from a hang is
*progress*, and deqp announces every test before running it, so the watchdog is
"no new output for `--stall` seconds". `stdbuf -oL` is what makes that
measurable at all: a redirected stdout is block-buffered, and without it a stall
is indistinguishable from ordinary buffering.

**6. The one synthesised queue family is the largest single non-extension gap
CTS finds.** §0.4 item 7 chose it deliberately — a GPU side has one queue on one
family, and passing RADV's five through had an application select family 3 and
get family 0's queue in silence. The cost is now a number: of the 59 tests that
skip forwarded and pass on RADV, **11 skip for the queue family alone** —
8 `No matching queue found: findQueueFamilyIndexWithCaps(requiredCaps=0x2;
excludedCaps=0x1)` and 3 `Exclusive compute queue not supported`. That is second
only to `VK_EXT_shader_object` (12) and ahead of
`VK_EXT_graphics_pipeline_library` (7) and the whole ray-tracing family (6).
§0.8 item 3 predicted a workload that would break the one-family decision;
this is it, and unlike video decode it is not one extension away.

**7. The gap is data now, on both sides of the boundary.** `sv-codegen gap`
reports it from the registry — **740 commands defined, 239 carried by the zink
profile, 501 not, 23 of those unreachable by any profile** — grouped by the
extension that introduces each, because nobody adds `vkCmdTraceRaysKHR`, they
add `VK_KHR_ray_tracing_pipeline`. The suite reports the other half, in deqp's
own words, restricted to tests the control passes. The intersection is the work
list and it is short: `VK_EXT_shader_object`,
`VK_EXT_graphics_pipeline_library`, `VK_KHR_maintenance6`,
`VK_KHR_push_descriptor`, `VK_EXT_sample_locations`,
`VK_EXT_depth_range_unrestricted` — and a second queue family, which is not an
extension at all.

**8. Two ring fatals, and both name the same suspicion.** The GPU side declined
to continue three times across the run, with two shapes:
`no VkDeviceMemory with id 129` and `no VkBuffer with id 140737343632080` —
both annotated by the code that raised them as a possible missing `RingBarrier`
(R22). The second id is `0x7FFF…`, a **host pointer**, not an app-side id, so at
least one of the two is a handle reaching the GPU side unmapped rather than an
ordering fault. Recorded rather than fixed; they are the first defects CTS has
found that no other harness in this tree could reach.

**9. The multi-session property holds — and the check that said otherwise was
measuring the check before it.** Concurrent clients on **one** GPU side — one
driver, one `VkDevice`, one `VkQueue`, three ring threads each — over
`dEQP-VK.api.smoke.*` and `compute.basic`: **48/48 Pass** at eight clients,
**36/36** at six, no ring fatal. The generated dispatch takes the device queue
lock as of this branch, and the command set it takes it for comes from the
registry's `externsync=` *and* its `<implicitexternsyncparams>` prose — the only
place `vkDeviceWaitIdle` is marked, and `vkDeviceWaitIdle` is precisely the one
that rides a different ring from `vkQueueSubmit`.

  In between those two results the same check reported **six `DeviceLost` at
  `vkWaitForFences` across six clients**, and it would have been recorded as a
  concurrency defect. It was not one. That run had the concurrency check *last*,
  after a sweep that had lost eight devices and put RADV through as many ring
  resets; repeated on a quiescent GPU the identical six clients gave 36/36. So
  the check now runs **before** the sweep, and the finding is a rule rather than
  a fix: **a check placed after a phase that resets the GPU is measuring that
  phase.** It is §0's recurring shape once more — a true observation attached to
  the wrong cause — and it survived one run in each direction, 48/48 and then
  12/18, which is exactly long enough to be believed.

**10. The forwarder hangs the GPU on about one in ten of the tests RADV
passes, and that is what stopped the wider run.** The 1-in-2000 sample — 1640
tests, ten times the table above — was run and abandoned. Its control finished
in **11 s: 904 Pass, 736 NotSupported, and not one device lost**. Its forwarded
pass reached **661 of 1640** before it was stopped, with **86 Pass, 503
NotSupported, 1 Fail and 71 DeviceLost** — and the reason for stopping was in
the kernel log rather than in the scoreboard:

```
amdgpu 0000:72:00.0: amdgpu: ring gfx_0.0.0 timeout, signaled seq=887853, emitted seq=887857
amdgpu 0000:72:00.0: amdgpu: Starting gfx_0.0.0 ring reset
amdgpu 0000:72:00.0: [drm] device wedged, but recovered through reset
```

— 313 `amdgpu` messages in ten minutes, on a shared machine at load average 86.
Every reset succeeded and `dEQP-VK.api.smoke.*` was back to 6/6 within seconds
of stopping, so nothing was damaged; but a harness that resets somebody else's
GPU every twenty seconds is not one to leave running, and the rate is the
finding rather than the inconvenience.

Two things follow, and they are not the same claim. **The upper bound is
71/661 ≈ 11%**, which is a *batch* figure and therefore includes collateral:
at the smaller sample, isolation reduced 19 batch `DeviceLost` to 12 real ones,
about 63%. Scaling that gives roughly **7% of forwarded tests hanging the GPU**,
which matches the isolated 12/164 directly. And **the control does not do it at
all** — zero device losses in 1640 tests, and zero in the 164 — so this is not
CTS being hard on RADV. It is ours.

That is the single largest defect this project has, it was invisible to all six
existing harnesses, and finding it is the whole return on building this one.
Nothing about it is diagnosed yet: the twelve isolated names are in
`/tmp/sv-cts-regressions.txt`, they span `ubo`, `ssbo`, `renderpasses`,
`subgroups`, `ycbcr`, `query_pool` and `spirv_assembly`, and no two of them
obviously share a mechanism.

**What is not settled**, and is the honest state of this stage:

- **This is a 1-in-20000 sample of 3.28 million tests, not a conformance run.**
  164 tests is enough to find defects and to size the gap; it is not enough to
  claim anything about the suite, and the word "conformant" does not apply to
  any number in this section. The instrument scales — `--fraction` is the only
  knob — and what it costs is wall time on a machine with a GPU other people are
  using.
- **The forwarded pass is 52× the control's wall time**, and that number is
  worth almost nothing as a performance figure: it includes a GPU-side restart
  per chunk, an isolation phase the control barely uses, and twelve device
  losses at ten seconds of kernel ring reset each. §0.13's sweep is the
  performance measurement; this is not one.
- **The device losses of item 10 are undiagnosed.** Each is reproducible by name
  and none has been reduced to a command. Two hangs are separate from them —
  `dEQP-VK.memory.allocation.random.79` and a
  `synchronization.timeline_semaphore.device_host…` case — and are what item 5's
  watchdog exists to survive rather than to explain.
- **The wider run has to be re-run on a machine nobody else is using**, and
  until then the 1-in-2000 column is 661 tests of 1640 and is quoted as such.
- **Nothing has run on testhost.** The Intel iGPU in the guest is the device
  the north-star goal needs and the cross-VM stack is what would have to be
  stood up to reach it; every number here is same-machine RADV over a Unix
  socket. §0.13 said the same thing about the resolution sweep and it is still
  true.

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

### 1a. The boundary invariant: no descriptor ever crosses

**No file descriptor and no handle ever crosses between the app side and the GPU
side. What crosses is a *name*, which the peer opens for itself.**

This ranks with the two invariants above and constrains more of the design than
either, so it is stated here rather than in §4.3 where it is implemented. It is
not a security property and it is not about trust — `security-model.md` puts the
two sides in one trust domain and nothing here changes that. It is about what a
**VM boundary can physically carry**, and it applies *even when both sides are on
one machine*, because the same-machine case is a development loop for a design
whose deployment is a hypervisor. A mechanism that works only because the peer
happens to share a kernel is a same-machine path wearing the design's clothes,
and it will be discovered at the moment the design is finally run for real.

The distinction, precisely, because it decides three separate questions below:

- **Illegal: passing a descriptor *across* the boundary.** `SCM_RIGHTS`,
  `DuplicateHandle`, an exported dma-buf fd handed to the peer. A virtio-serial
  port and a vsock carry bytes; there is nothing for any of these to travel in.
- **Legal: a descriptor created *locally*, by one side, out of memory that side
  can already reach.** The host wrapping its own view of guest RAM with
  `/dev/udmabuf` is fine — that fd is made on one side and never leaves it. So
  is `sv-hostd` opening QEMU's memory backend through `/proc/<qemu-pid>/fd/<n>`:
  the hypervisor's own child, reached by the machine that hosts it.
- **Legal: a *name* crossing, which the peer then opens itself.** This is the
  replacement mechanism, and the Windows↔Windows axis has done it correctly
  since it was written — two named sections, no handle passed, and therefore no
  `PROCESS_DUP_HANDLE` relationship between the GPU side and every application
  that attaches (§8, `sv_transport::winshm`). Generalising that is what this
  invariant asks for; `DuplicateHandle` is the *wrong* answer to `SCM_RIGHTS`
  and a named object is the right one, in both directions.

A name is text. That is the whole of why this works: the cold path (D10) is a
byte stream, so a handshake that carries names fits down a virtio-serial port
unchanged, and the same code serves the same-machine loop and the hypervisor.

**What it costs, and where it is not satisfied**, is §0.7, which is a finding
rather than a decision. One provider — the GPU side exporting a dma-buf — cannot
be brought into line, because a dma-buf has no name to give; it survives as a
marked exception on one host for one driver, and the measurements that pin it
there are in §0.7.

---

## 2. Decisions at a glance

| # | Question | Decision |
|---|---|---|
| D1 | Where does the arena live? | Ordinary **guest RAM**, allocated by whichever side is in the VM, mapped by the other through QEMU's existing `memory-backend-memfd`. **Measured working end to end** (`measured-gpu-side.md`). No ivshmem, no new device, no unsigned driver. (fwd §2.2) |
| D1a | How is it obtained inside the guest? | `VirtualAlloc(MEM_RESERVE\|MEM_COMMIT\|MEM_LARGE_PAGES)` — 2 MiB granularity, each large page physically contiguous and non-pageable. Needs `SeLockMemoryPrivilege` (R25). **Measured** (`measured-gpu-side.md`): 22 runs per 64 MiB, **1 run per GiB**, imported by the driver exactly like ordinary memory. **Fallback, measured:** 4 KiB `VirtualAlloc` + `VirtualLock`, no privilege, 1471 runs per 64 MiB. **AWE and `NONPAGED_LARGE` are measured and rejected** — §5.1. |
| D2 | Fallback if neither path yields usable pages | **None is planned any more.** The attestation-signed pinning driver is dropped: R19 and R20 are retired, an ordinary pointer imports, AWE returns real frames, and locating costs ~2 s — none of which an EV certificate would improve. It survives as a theoretical last resort, not as a plan, and so does ivshmem. (§5.1; fwd §2.2 verdict table) |
| D3 | How does the GPU side get `VkDeviceMemory`? | `VK_EXT_external_memory_host`, `HOST_ALLOCATION_BIT_EXT`, **measured present, alignment 4096** (`measured-gpu-side.md`). Behind a pluggable `SlabImport` trait so dma-buf can be substituted on a Linux/AMD GPU side. |
| D4 | Import granularity | Per **slab** (default 64 MiB), not per `VkDeviceMemory`. One driver import per slab; app-side allocations are sub-ranges. Requires the `(memory, offset)` rewrite pass. The driver accepted a single 2 GiB import, so the slab size is ours to pick and not a driver limit (§5.2). (fwd §2.1) |
| D5 | Wire format | **Venus `VK_EXT_command_serialization`** encoding rules, reimplemented in Rust, with three deliberate deviations (§6.2). |
| D6 | pNext encoding | **gfxstream's** length-prefixed links with a 0 terminator; lenient app side, fatal GPU side. (fwd §1.3, §4.3) |
| D7 | Handles | **Venus's app-side-assigned ids on the wire** + **gfxstream's rich per-object table on the GPU side**. Object creation costs no round trip. (fwd §4.3) |
| D8 | Rings | **SPSC, per queue**, plus one per device and one per instance, plus one reverse ring. Spin-then-park, notification only when the consumer has parked. (fwd §1.2, §1.3) |
| D9 | Doorbell | Polling first, behind a `Doorbell`/`Parker` trait. Interrupts are an optimisation to be justified by measurement, not an architecture. (sgl §7.5) |
| D10 | Cold path | A socket: **virtio-serial** at first because it is already attached, **vsock** once `viosock` is installed. Handshake, capability negotiation, wakeups, teardown. Never data — and, since §1a, **never a descriptor**: what crosses is the *name* of a shared object the peer opens itself, which is text and therefore fits a byte pipe. (fwd §6.2, §6.3) |
| D11 | Cache attributes | **Write-back on both sides, everywhere, in v1.** The mechanism to vary it exists in the slab descriptor and has exactly one legal value until measurement says otherwise. This retires R10 rather than managing it. (§5.6) |
| D12 | Entry point | A **Vulkan ICD**, not a layer. Loader interface **7**, degrade to 5. (fwd §3.1) |
| D13 | Codegen input | **Pure Rust**: `vk-parse` plus our own C-declarator parser and typed model, in `vk-registry`. Cross-checked once against Khronos's `vulkan_object.py` as a test fixture. (§7.1) |
| D14 | Command buffers | Recorded straight into an arena block; `vkQueueSubmit` ships an `(offset, len)`. Zero round trips and zero copies from record to execute. (fwd §1.3, §7.2) |
| D15 | Submit | **Asynchronous**, non-negotiable. |
| D16 | Sync | Everything is a 64-bit monotonic timeline; counters mirrored into the control region so an app-side wait is a local load. (fwd §2.9) |
| D17 | Presentation | **Outside the protocol**, and §0.6 makes that stronger than it was written: not merely unserialised but *needing no protocol at all*. The app side creates the swapchain's images and records the copy out of ordinary forwarded Vulkan, so `sv-gpu-server` has no presentation code. Headless first, then a real swapchain, then a window. (fwd §2.8, §10) |
| D18 | Scope | **Zink.** DXVK ≤ 2.6 as a stretch. DXVK ≥ 2.7 and vkd3d-proton are out of scope, and we will not advertise `bufferDeviceAddress` in v1 — a clean device-creation failure beats silent corruption. (fwd §5) |
| D19 | Reverse channel | Built from day one, not retrofitted. Validation-layer output on the app side is the primary debugging tool. (fwd R14) |
| D20 | Region ownership | The side **inside the VM** creates and publishes both regions; the other side attaches. sharedgl's "server initialises" rule inverted correctly, with the readiness flag sharedgl aliased away. (sgl §7.2b) |
| D21 | How does the attaching side find the pages? | By **locating**, not by being told, and the locator is the **content scan on every path**: 1.8 s at 64 MiB, 2.02 s at 1 GiB, 2.32 s at 2 GiB measured, because the cost is linear in *guest RAM* and not in arena size. It is also the only method that verifies rather than trusts. `PfnLocator` stays for the AWE path (16384/16384 frames verified); the 2 MiB strided probe is an optimisation v1 does not need. The **run list never crosses the wire** — it is derived on the attaching side. §5.1, §5.7. |
| D22 | Attach granularity | Per **extent**, not per slab. One `VirtualAlloc` covering control region and arena, one locate, two views. A slab is an import unit and a cache-uniformity unit, never an attach unit. §5.7. |

---

## 3. Component topology

```
  APP SIDE (Linux host, no GPU)                    GPU SIDE (Windows VM, iGPU)
  ┌────────────────────────────────┐               ┌────────────────────────────────┐
  │ application                    │               │ sv-gpu-server                  │
  │   ↕ Vulkan loader              │               │  • arena provider: large pages,│
  │                                │               │    stamps, owns both regions   │
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
  │  • locates the extent's pages  │                        │
  │  • GPA → offset via mtree      │                        │
  │  • coalesces runs, stitches    │                        │
  │  • hands NAME + runs to ICDs   │                        │
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
lets several application processes share one attachment.

> **[SUPERSEDED — 2026-08-13, §1a and §0.7]**
> *In the same-machine development loop it degenerates to "create a memfd,
> `ftruncate`, hand it out", which is why steps 1–6 of §13 need no VM at all.*
>
> "Hand it out" meant `SCM_RIGHTS`, and §1a forbids it. The daemon creates a
> **named** object under `/dev/shm` and hands out the *name*; the steps still
> need no VM. Two consequences §0.7 records: the object is no longer sealable, so
> it can never back a udmabuf, and on `Mode::Qemu` the name the ICDs are given is
> `/proc/<qemu-pid>/fd/<n>` unless the guest is on `memory-backend-file` — which
> moves the `/proc` open from the daemon to the ICD and so weakens the first
> sentence of this paragraph. It is legal (§1a) and it is not free.

Step 0 made it a larger component than the box suggests. `sv-hostd` does not
merely translate an address it was handed: on the paths that do not involve AWE
the guest **cannot enumerate its own frames at all**, so `sv-hostd` has to
*locate* the pages by content, coalesce what it finds into runs, and stitch the
view out of one `mmap(MAP_FIXED)` per run (§5.1, §5.7). The working prototype of
exactly this is `crates/arena-host`, written for step 0; its `scan`, `translate`
and region-table logic is the seed of `sv-hostd` rather than throwaway probe
code. Note that it had to run as root, because QEMU's memfd is only reachable
through `/proc/<qemu-pid>/fd/<n>` — which is the argument for a daemon, made
concrete.

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
   extent: ExtentDesc, slab_size, sizes, endianness, pointer_width }`.

   > **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "Fragmentation"]**
   > *`Hello { …, control_pfns[], arena_pfns[], … }`. The PFN arrays are
   > run-length encoded, which is cheap because AWE frames sort into long runs
   > in practice (fwd §2.2, "Contiguity is a non-problem").*
   >
   > Two things were wrong. Frame numbers are only available on the AWE path,
   > which is no longer the primary one; and contiguity is emphatically a
   > problem — a pinned 4 KiB 64 MiB window measured **1471** runs. What crosses
   > now is an `ExtentDesc` describing *how to find* the pages, not a list of
   > where they are. See §5.1.

   `ExtentDesc { run_id: u64, granularity: Small | Large2M, control_pages: u64,
   arena_pages: u64, locator: Scan | Probe | Pfns(runs[]) }`. It is small in
   every case: on the two locating paths it carries no addresses at all, and on
   the AWE path the frame list is run-length encoded — though AWE frames were
   measured to sort into 5863–16339 runs per 64 MiB, so "run-length encoded" is
   not the saving it sounds like, which is one more reason that path is not
   taken (§5.1).

   **`locator` was `Scan` on every path and is now chosen** (§0.14, 2026-08-13).
   `Pfns` is no longer AWE-only: the guest asks the PFN database where its own
   pages are (`awe_probe::superfetch::pfn_locator`) and publishes the run-length
   encoded answer, and the attaching side translates it
   (`sv_hostd::locate::PfnLocator`) instead of sweeping thirteen gigabytes. It
   is opt-in — `sv-hostd qemu --locator scan|pfns|both`, `arena-host locate
   --locator …`, defaulting to `scan` — because R28 makes the scan the
   cross-check that the undocumented interface is telling the truth, and `both`
   is that cross-check: the same extent located twice by two mechanisms with
   nothing in common, and a disagreement about any page fatal to the session.
   On the large-page path a 64 MiB window is 26 runs, so the descriptor is 416
   bytes rather than a page list.

   **`granularity` is asserted, never assumed.** The GPU side calls
   `QueryWorkingSetEx` on the extent and publishes the `LargePage` bit it
   actually observed, not the flag it passed to `VirtualAlloc` (§5.1). A slab
   that silently fell back to 4 KiB pages has different frame-stability
   properties (R26) and the attaching side has to know which kind it is
   attaching. The check costs 156 µs per GiB on a large-page extent.
3. `sv-hostd` locates the extent's pages (§5.1), translates GPA → memfd offset by
   walking QEMU's flat view, coalesces into runs, reserves one `PROT_NONE` range
   per region and `mmap(MAP_FIXED)`s each run into it. The result is virtually
   contiguous with identical internal offsets — the §1 invariant — and nothing
   above the attacher ever learns that it was stitched. **The locator descriptor
   must ride the cold path and cannot live in the control region**, for the
   obvious reason that the control region is one of the two things being
   located.
4. The peer acquire-loads `magic`, then checks in this order and refuses with a
   named error on any mismatch: magic, `layout_version`, `wire_version`,
   `protocol_hash`, endianness, `control_size` against the mapping length.
5. The peer replies `Accept { generation }` and claims rings.

**Step 5 does a second job since §1a**, on the `Attach` handoff rather than on
the `Hello`: it is the client saying *I have opened the object by name and mapped
it*. The daemon needs that before it may unlink the name — unlinking releases the
name and not the memory, so a client that has already opened is unaffected, but
one that has not yet opened would find nothing. It is also where "I could not
open that name" is reported, in the daemon's log, instead of surfacing as a ring
that never answers.

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

**Decision: ordinary guest RAM, allocated by the side inside the VM.** This half
is no longer a bet. It was measured end to end on 2026-08-11: guest RAM written
by a Linux process through QEMU's memfd, imported by the Intel driver as
`VkDeviceMemory`, and read back through the driver's own mapping, with
`memoryTypeBits = 0x6` — the same answer ordinary process memory gets
(`measured-gpu-side.md`, "Step 0"). What changed is *how the pages are obtained
inside the guest*, and that is the rest of this section.

> **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "AWE: refused, and not
> worked around" and "Fragmentation"]**
>
> *On `testhost` that means the GPU-side process allocates it with Windows
> AWE: acquire `SeLockMemoryPrivilege`, `AllocateUserPhysicalPages`, then
> `MapUserPhysicalPages` into one reserved contiguous VA range. The returned
> `PageArray` holds real physical frame numbers — Microsoft's own released
> kernel source shows `NtAllocateUserPhysicalPages` doing a verbatim
> `RtlCopyMemory` of the MDL's PFN array into the user buffer, with three
> independent corroborations in the same file (fwd §2.2, "Windows AWE returns
> real guest-physical frame numbers"). Inside a VM, "physical" is
> guest-physical, so `PageArray[i] << 12` is a QEMU GPA …*
>
> *The user's stated preference and the research's ranking agree, and so do the
> two independent lines of reasoning: the pages the two sides can most easily
> agree on are also the only kind of pages the import mechanism actually likes
> (fwd §2.2 verdict). That convergence is the reason to be confident in a
> decision that still has an unrun experiment behind it (§14).*
>
> The reasoning about what AWE *returns* still stands and has not been
> contradicted. What made it the wrong primary is that it was chosen for a
> problem it does not solve. AWE gives frame numbers; it promises **nothing**
> about contiguity, and contiguity turned out to be the expensive thing. It also
> could not be run at all: `SeLockMemoryPrivilege` is unassigned to any account
> in the guest's local security policy, which is the Windows default, so
> `AllocateUserPhysicalPages` fails with `ERROR_PRIVILEGE_NOT_HELD`. The whole
> of step 0 was carried by what this document had filed as a fallback.

**Decision, revised: large pages are the intended path.** The GPU-side process
allocates each extent with

    VirtualAlloc(NULL, len, MEM_RESERVE | MEM_COMMIT | MEM_LARGE_PAGES,
                 PAGE_READWRITE)

in multiples of `GetLargePageMinimum()`, which is 2 MiB on x64. Each large page
is a single physically contiguous, 2 MiB-aligned, non-pageable frame.

**Measured, and better than the arithmetic predicted** (`measured-gpu-side.md`,
"Large pages, AWE, and the privilege that unlocks both"): a 64 MiB window is
**22** runs rather than the 32 a naive reading gives, because adjacent 2 MiB
blocks merge; a **1 GiB window is a single run**; a 2 GiB window is two.
Contiguity therefore gets *better* with size, which inverts the usual intuition
and is the reason §5.7 asks for the whole arena in one call. That is the whole
reason to prefer this path: it does not make the memory better, it makes it
*findable and attachable* cheaply (§5.7).

Three consequences that are not just "fewer mmaps":

- **A large page is 2 MiB-aligned in guest-physical space**, and every RAM alias
  in QEMU's flat view on this machine is offset by 0 or `0x4000_0000` — both
  multiples of 2 MiB. So every large page begins at a 2 MiB-aligned *memfd
  offset*, and the extent can be found by probing one cache line every 2 MiB
  rather than sweeping 13 GiB (§5.7). This is arithmetic, not a measurement.

  > **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "So how does the host find
  > large-page memory? Ranked"]**
  > *…and it is the single biggest reason large pages are worth a security-policy
  > change.*
  >
  > It is not. The sweep it avoids was measured at 1.8–2.32 s for any window size
  > this machine can hold, paid once at attach; a 512× saving on a one-off two
  > seconds is not what buys the privilege. What buys it is contiguity —
  > 1471 runs becoming 22, and 1 at a gigabyte — and frame stability by
  > construction (R26). The strided probe is a real optimisation and it is not
  > load-bearing, so v1 does not implement it.
- **Non-pageable by construction**, so the frame a page sits on cannot be
  reclaimed and re-faulted somewhere else. On the 4 KiB path this rests on
  `VirtualLock`, which promises *residency* and does not, in its documented
  contract, promise frame stability — R26, and the frame-stability paragraph at
  the end of this section.
- **It is the same ordinary user pointer** the driver already accepted. Nothing
  about the import call changes; `minImportedHostPointerAlignment` is 4096 and a
  2 MiB-aligned pointer satisfies it trivially.

> **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "Does the driver still
> import them?"]**
>
> ***This is not yet measured and must not be treated as though it were.**
> Whether the Intel driver accepts an import backed by large pages is being
> measured now. A pinning import path can legitimately care about PTE
> granularity, and a refusal there would be a real finding rather than a
> nuisance. Every figure above marked "on the order of" stays pending until
> `measured-gpu-side.md` carries a number. **The project is not gated on it**: if
> large pages are refused, we fall back to the 4 KiB path, which is measured
> working, and we pay §5.7's attach cost.*

**It is measured, and the driver does not care.** A 64 MiB large-page window gave
`getMemoryHostPointerProperties = SUCCESS`, `memoryTypeBits = 0x6`, type 1,
`vkAllocateMemory(import) = ok`, `mapped_same_address = true`, and all 16384
pages carried the host's bytes through the `VkDeviceMemory` mapping — identical
in every field to the 4 KiB result. So did the AWE window, and so did the
`NONPAGED_LARGE` window at 16 MiB. **Single imports at 1 GiB and 2 GiB also
succeeded**, with all 262144 pages of the 1 GiB case verified against the host
afterwards. The import path does not distinguish PTE granularity, allocator, or
size at these scales (`measured-gpu-side.md`).

**When large-page allocation fails, degrade — once, per extent, loudly.**
`VirtualAlloc(MEM_LARGE_PAGES)` needs physically contiguous free memory and
fails with `ERROR_NO_SYSTEM_RESOURCES` on a guest whose physical memory has
fragmented, which on a machine that has been up for weeks is a normal Tuesday
rather than an exceptional condition. The policy:

1. Try large pages for the whole extent.
2. On failure, fall back to 4 KiB `VirtualAlloc(MEM_COMMIT)` + `VirtualLock` for
   the whole extent, and record the granularity in the `ExtentDesc` so the
   attaching side picks the right locator. Report it in the GPU side's own log
   and not on the reverse ring: the first extent contains the control region,
   so at the moment this decision is taken there is no ring to report it on.
   Attach becomes slower and nothing else changes.
3. Do **not** retry, do **not** loop, and do **not** mix granularities within one
   extent. Windows offers no way to defragment on request, so a retry is a
   spin; and mixing makes the extent's granularity — which the attaching side
   is told and which the canary rule of §5.2 keys off — a lie about half of it.
   Different extents may legitimately differ, which is why granularity is
   per-extent and not global. What was actually allocated is confirmed with
   `QueryWorkingSetEx` rather than inferred from the call that succeeded.

Allocating early helps and is free: the arena is committed when `sv-gpu-server`
starts, which is the point at which the guest's memory is least fragmented.
That, and not fear of the allocator, is the argument for eager commit (§5.7).

**How often it actually fails is not known, and what was measured is the
favourable case.** Five 64 MiB allocations, five 256 MiB and three 1 GiB all
succeeded, the slowest in 90 ms, on a guest with 8973 MiB free and 1.3 hours of
uptime (`measured-gpu-side.md`, "Is large-page allocation reliable?"). That says
the fast path is fast and says nothing about a guest that has been up for weeks,
because manufacturing memory pressure on a machine somebody else is using was
out of scope. Read it as evidence that the fallback is rarely taken here, not as
evidence that it is rarely needed anywhere — which is exactly why the fallback
exists and why degradation is per extent and loud.

**The privilege, stated as the cost it is.** `SeLockMemoryPrivilege` is required
for `MEM_LARGE_PAGES` exactly as it was for AWE. It is unassigned by default;
granting it means adding an account or group to `[Privilege Rights]` in the
local security policy and obtaining a new token, which for an ssh session means
reconnecting — no reboot, no interactive logoff, and no code-integrity setting
touched: not Secure Boot, not HVCI, not test-signing, so the hard constraint in
`hardware-constraints.md` is untouched (`measured-gpu-side.md`, "What AWE would
take"). It is nonetheless a real change to the security policy of somebody's
daily laptop, and it is worth being precise about what it buys and what it
costs:

- **It buys**, measured rather than estimated: a **67× reduction in mappings**
  at 64 MiB (1471 runs → 22) and a 1471× one at a gigabyte (1 run), frame
  stability by construction (R26), and the ability to allocate the whole arena as
  one extent that attaches with one or two `mmap` calls. It does *not*
  meaningfully buy a cheaper locate step: the sweep it would avoid is ~2 s once,
  whatever the arena size.
- **It costs** the ability, for the account holding it, to lock arbitrary
  amounts of memory non-pageably — a local denial-of-service surface on a
  machine a family member uses. Grant it to the account that runs
  `sv-gpu-server`, not to the interactive user, wherever the deployment permits
  the two to differ.
- **It is reversible**, and the 4 KiB fallback is what makes that true rather
  than nominal: remove the right, obtain a new token, and the arena still works,
  more slowly. A design that stops functioning when the privilege is withdrawn
  would have made the grant permanent in practice. This one does not.

> **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "Fragmentation, which was
> the point" and "R19 is retired, and so is the last of R20"]**
>
> ***AWE is a live option again, and it is not the plan.** With the privilege
> granted, `AllocateUserPhysicalPages` and `AllocateUserPhysicalPages2` with
> `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE` are both measurable, and both are being
> measured. **On current evidence the decision goes to large pages via
> `VirtualAlloc`**, for three reasons: the pointer it produces is the exact shape
> the driver has already been measured to accept; it needs no window remapping and
> no `MapUserPhysicalPages` bookkeeping in the hot path; and plain AWE solves the
> problem we do not have (frame numbers) while leaving the one we do
> (contiguity) untouched. What would change my mind, in order of likelihood:*
>
> *1. The driver refuses a large-page-backed import but accepts an AWE range …*
> *2. `AllocateUserPhysicalPages2` with `NONPAGED_LARGE` works. Then it is
> strictly better than both: 2 MiB contiguity and frame numbers, which together
> eliminate the locate step entirely … This is the outcome to hope for and it is
> one call away from being known.*
> *3. The locate step proves too slow at arena scale even with the probe stride.*

**All three were run, and the decision stands with none of its conditions met.**
`VirtualAlloc(MEM_LARGE_PAGES)` is the arena allocator; the 4 KiB
`VirtualLock`ed path is the fallback; **AWE is not used at all**. Taking the
three in the order they were hoped for:

1. **The driver accepts everything.** Large pages, AWE and `NONPAGED_LARGE` all
   imported with `memoryTypeBits = 0x6` and passed the full host↔guest↔Vulkan
   loop. So this branch never opens, and the reason to prefer large pages is
   entirely fragmentation.
2. **`NONPAGED_LARGE` does not work.** `AllocateUserPhysicalPages2` resolves from
   `kernelbase.dll` rather than `kernel32.dll` — asking only `kernel32` produces
   a wrong negative — and the call *succeeds*, which is the trap. It then
   satisfied 3992 of 16384 requested frames in ~200 ms and returned them at
   **1.0 pages per run**: completely scattered 4 KiB frames, no large pages, and
   a short allocation into the bargain. The hoped-for outcome is measured absent
   on build 26200.
3. **The locate step is not slow.** ~2 s regardless of arena size (§5.7), so the
   pressure that would have forced frame numbers does not exist.

Plain AWE remains the only way to obtain frame numbers, and its frames are real —
all 16384 resolved to the right memfd offsets. That is worth knowing and is not
worth using: 5863–16339 runs against 22 is a 267× penalty in mappings to replace
a two-second scan that also *verifies* what it finds. `PfnLocator` therefore
stays in the codebase, tested, and unused on `testhost`.

The shape of the code is unchanged by any of this, which was the point of
splitting it: an `ArenaProvider` that allocates and describes, an `ArenaLocator`
that produces a page map, and an `ArenaAttacher` that stitches runs. The
measurements picked implementations, not architecture.

> **[SUPERSEDED — 2026-08-11, R19 retired (`security-model.md`,
> `measured-gpu-side.md`)]**
>
> ***The pinning driver stays as the last fallback, with a narrowed role.** If
> neither AWE nor large pages yields what we need, an **attestation-signed ~200
> line driver** — `MmProbeAndLockPages` on an ordinary `VirtualAlloc` region, walk
> `MmGetMdlPfnArray`, copy the frames out — dodges every AWE restriction at once,
> and attestation signing is a genuine Microsoft signature that loads on retail
> Windows 11 with Secure Boot and HVCI on, not test-signing (fwd §2.2). Its role
> has narrowed sharply since this was written: the import itself is measured
> working from an ordinary pointer, so the driver would now buy only frame numbers
> and an escape from R19 — not viability. The cost is an EV certificate and
> paperwork. We are further from needing it than the original text assumed.*

**The pinning driver is dropped.** Everything it was held in reserve for has been
measured away: R19 is retired (the driver imported an AWE range), R20 is retired
in full, `AllocateUserPhysicalPages` still returns raw guest-physical frames on
build 26200, and the frames it would have reported are worth less than the scan
that finds pages by content. It would cost an EV certificate and paperwork to buy
a locate step 2 s faster, once, on a path we have decided not to take. If it is
ever revived it will be for a reason not yet on this page — and the reasoning
above is kept so that reason has to be stated rather than assumed.

**ivshmem is not in the plan.** It has the best driver story and the worst
mechanism story: host-pointer import is implemented as kernel userptr, userptr
pins pages, and BAR mappings are `VM_IO|VM_PFNMAP` which the pinning path
rejects by construction. Looking Glass had to write `kvmfr` —
`devm_memremap_pages()` to manufacture `struct page`s, exported as dma-buf —
precisely because this does not work, and Looking Glass never asks a GPU to DMA
*out* of the BAR anyway, so its track record is not evidence for our case (fwd
§2.2). A BAR would need `HOST_MAPPED_FOREIGN_MEMORY_BIT_EXT`, which essentially
nobody implements. We keep it in the risk table and not in the design.

**How the two sides agree on which pages: locate, do not compute.**

> **[SUPERSEDED in part — 2026-08-13, `measured-gpu-side.md` "Asking Windows
> where the pages are: the PFN database, measured", §0.14]**
>
> *The original design had one answer — the guest reports frame numbers, the host
> translates them — and that answer is available on the AWE path and nowhere
> else. It is not a matter of privilege: **Windows does not report frame numbers
> to user mode outside AWE at all.***
>
> The second sentence is false as stated and the first is true only of
> *documented* interfaces. What was measured was `QueryWorkingSetEx`, and that
> measurement stands: it carries no physical address in any field.
> `NtQuerySystemInformation(SystemSuperfetchInformation)` with
> `SuperfetchPfnQuery` does — one `MMPFN_IDENTITY` per physical frame, carrying
> the frame number, the owning process id and the virtual address it is mapped
> at, for **any** allocator that produces ordinary mapped pages. It is measured
> working on build 26200, agreeing with the scan on 16384 of 16384 pages, and it
> needs no privilege this guest does not already hold. `SuperfetchPfnLocator`
> below.
>
> `\Device\PhysicalMemory` needs a privilege of its own and is still not a
> supported route to one's own frames.

The original design had one answer — the guest reports frame numbers, the host
translates them — and until 2026-08-13 that answer was available on the AWE path
and nowhere else. Step 0 therefore
had to solve the problem the other way round, and did: **the app side finds the
pages by content.** The guest stamps every page of the extent with
`{magic, run_id, region_tag, page_index}` before publishing it; `sv-hostd`
sweeps the memfd for the magic and builds a page map from what it finds. All
16384 pages of the 64 MiB window were located this way, with zero wrong indices
(`measured-gpu-side.md`).

Four locators, one trait:

| Locator | Mechanism | Cost | Status |
|---|---|---|---|
| `ScanLocator` | Sweep the whole memfd for the magic. Works for any granularity | **1.8 s at 64 MiB, 2.02 s at 1 GiB, 2.32 s at 2 GiB, measured** | **v1, and the cross-check on every path** |
| `SuperfetchPfnLocator` | The guest asks the PFN database where its own pages are and reports the frames; the host translates them | **~330 ms in the guest for any window; 0.064 s on the host; 0–22 ms to re-check a list already held. Measured** | **Measured working, agrees with the scan 16384/16384.** Wired: `awe_probe::superfetch::pfn_locator` → `Locator::Pfns` → `sv_hostd::locate::PfnLocator`, selected by `--locator scan\|pfns\|both`. Not yet published by `sv-gpu-server`, which has no Windows provider |
| `PfnLocator` | Translate a reported frame list through the region table. **No longer AWE-only** — the mechanism above feeds it | O(pages) arithmetic, no I/O | Implemented in `sv_hostd::locate` and unit-tested; the `arena-host` seed remains as `arena-host locate`. **16384/16384 frames verified** against a live guest |
| `ProbeLocator` | Read one record every 2 MiB. Large pages only, because only they are guaranteed 2 MiB-aligned in GPA | ~6656 probes for 13 GiB, ~1/512 of a scan | Not implemented, and now pointless |

**Decision, revised 2026-08-13: the scan locates an extent once, and the query
re-locates it thereafter.** Neither alone is right, and the reason is that they
fail in opposite directions.

- **The scan verifies rather than trusts.** It finds pages by their contents, so
  it cannot produce R21's silent wrong answer. What it cannot do is run twice:
  the stamps are arena data by the time anyone would want to ask again, and a
  scan of an overwritten window **locates 0 pages**, measured.
- **The query trusts rather than verifies, and it does not care what is in the
  memory.** It answers from the kernel's own PFN database, so an overwritten
  window locates exactly as well as a fresh one — **same 16384 frames, same 26
  runs, before and after every byte was overwritten**, measured. What it cannot
  do is prove itself: it is an undocumented interface, and an undocumented
  interface that quietly starts returning a shifted field is precisely the
  failure this project refuses to accept.

Running the scan **once**, at attach, against the frame list the query produced
for the same extent, is the arrangement in which each covers the other's
weakness: the one occasion on which the stamps exist is the one occasion on
which the query can be checked, and after that the query is the only locator
that still works. The cost is the scan's 2 s, paid once, exactly as before —
plus ~330 ms of guest CPU — and it buys re-location, a 1 ms re-validation that
replaces R26's canary, and a loud failure if the interface ever moves.

`ProbeLocator` is now pointless and should be deleted from this table when
somebody is next in here: it was a cheaper scan, and the query is cheaper still
and does not need a second stamping convention.

`ProbeLocator` is `ScanLocator` with the search space divided by 512, and it is
sound for exactly one reason: on the large-page path the *only* place a stamped
page can be is a 2 MiB-aligned offset, so every offset that is not 2 MiB-aligned
can be skipped without being read. It is recorded rather than built, because it
comes with a coupling that has to be got right in two places at once: the guest
would have to stamp only the first 4 KiB page of each large page, and the record
would carry a large-page index rather than a page index. Get that pairing wrong
in either direction — stamp every page but probe with a stride, or stamp with a
stride but fall back to 4 KiB allocation — and the locator finds nothing, which
is at least a loud failure rather than a quiet one. **In v1 the guest stamps
every 4 KiB page on every path**, which is the one rule that is correct whatever
the allocator did.

**`QueryWorkingSetEx` cannot substitute for any of this, and is worth calling
anyway.** It was the candidate for having contiguity and frame numbers at once,
and it does not report a physical address at all: over 262144 entries of a 1 GiB
large-page window the 64-bit block took exactly one distinct value, and the
documented `ReservedUlong` — the only place a frame number could hide — is zero
for every page of every allocator tried. There is no field, on any build; Windows
does not report frames to user mode outside AWE (`measured-gpu-side.md`, "Can we
have both?"). What it *does* report is the `LargePage` bit, which separates the
three allocators cleanly (`0x…c00041` large, `0x…400041` AWE, `0x…5400041`
locked 4 KiB), so the GPU side asserts **what it actually got rather than what it
asked for** before publishing the `ExtentDesc` — a slab that silently fell back
to 4 KiB pages has different properties (R26) and the attaching side must be told
which kind it is attaching. The call answers per address rather than per page
table entry, so on large pages it strides by 2 MiB: 512 entries instead of
262144, **156 µs per GiB instead of 48.8 ms**. Asserting a large-page extent is
free; asserting a 4 KiB one costs ~50 ms per gigabyte, once, and is still worth
it.

**Frame stability: mitigated by construction, and with a canary where it is not
(R26).** The locate step runs once and its output is a table of memfd offsets, so
if a frame moved afterwards the app side would keep reading the old offset — no
fault, no error, just an arena where one side's writes stop reaching the other.
`VirtualLock` promises *residency*, not frame stability, and does not forbid
this. What was measured: a 64 MiB locked window, located, held while the guest
went about ordinary business, and re-located 3m14s later gave a **byte-identical
offset table** — all 16384 pages, 931 runs both times. That is evidence against
casual relocation and **not** evidence against relocation under memory pressure,
which was deliberately not provoked on a machine somebody else uses. Two
mitigations, and the first is why the second is rarely reached:

1. **Large pages remove the question**, because a large page is not a candidate
   for relocation by the ordinary paging machinery. This is the second
   independent argument for the large-page path, alongside contiguity.
2. **On the 4 KiB fallback the allocator reserves a canary per block** — bytes at
   a fixed position in each slab-sized block that it never hands out — which the
   attaching side re-checks periodically and at every handshake (§5.2). It costs
   nothing measurable and it converts a silent divergence into a loud one. A
   canary mismatch is fatal to the session, not something to repair: by the time
   it is noticed the two sides have already disagreed about memory.

The stamp is overwritten by arena data the moment the extent is used, and that
is deliberate: it is a locating device, not a header. That is also why the canary
above has to exist separately — by the time a frame could move, there is nothing
left to compare against. Two consequences worth stating because both are easy to
trip over later.

- **An extent must be located before it is used, and cannot be re-located
  afterwards *by the scan*.** If `sv-hostd` restarts, it does not re-scan and
  re-attach — it cannot, the stamps are gone. It bumps the generation, which
  every ring operation already carries, and every app-side client takes
  `VK_ERROR_DEVICE_LOST` (§4.3). That is the same path a guest reboot takes and
  it needs no new machinery.

  **This is now a property of `ScanLocator` rather than of the arena** (§0.14).
  The guest can be asked again for the frame list at any moment, at ~330 ms, and
  the answer for an overwritten window is the same one — measured, against a
  window every byte of which had been overwritten, cross-checked by the host
  writing into the re-obtained offsets and the guest finding all 16384 of them
  at the right page indices. So a `sv-hostd` restart *could* re-attach without a
  generation bump. It is not proposed for v1, because the generation bump is
  already built and correct and a restart is not a hot path; it is recorded so
  that "the arena cannot be re-located" stops being repeated as a fact about the
  design.
- **A permanent per-page stamp is rejected.** Reserving sixteen bytes at the
  head of every arena page would make the map re-derivable at any time, and it
  would put a hole in every page and destroy the flat offset invariant of §1 —
  which is the property the entire design is built on. Not worth it at any
  price. The R26 canary is not this: it is one allocation per block that the
  allocator declines to hand out, so the offset space stays flat and every byte
  an application ever sees is at the offset it was given.

**GPA → memfd offset is read, never computed.** On this machine the mapping is

    gpa <  0xc000_0000        ->  offset = gpa
    gpa >= 0x1_0000_0000      ->  offset = gpa - 0x4000_0000
    0xc000_0000..0xffff_ffff  ->  PCI hole, no RAM

but the constants are properties of *this* guest at *this* RAM size, not of
i440fx. The below-4 GiB split is **3 GiB, not the 3.5 GiB an i440fx uses at
smaller RAM sizes**, because QEMU drops the split once `ram_size` crosses
3.5 GiB on gigabyte-aligned machine types — anyone who hardcodes either constant
has a 512 MiB error waiting. Two ranges below 4 GiB are also not RAM at their
identity offsets: the `0xc0000-0xfffff` ROM window, and `0x103000-0x10efff`,
which twelve Hyper-V SynIC message pages overlay (`measured-gpu-side.md`, R21).

So: **`sv-hostd` parses QEMU's flat view (`info mtree -f`) into a region table
and looks addresses up in it. It does not evaluate a formula, and there is no
compiled-in default that is trusted without being re-read.** The reason to be
this firm is the measured failure mode: reading the memfd offset as if it were
the GPA lands on valid, zeroed, unrelated memory and reports no error whatsoever
— a silent wrong answer, which is the worst thing this codebase can produce. The
region table is re-read whenever the guest's RAM size, machine type or QEMU
version changes, and the handshake validates it before any Vulkan call by
checking a known pattern at known offsets — which on the locating paths is
exactly the stamp check, already performed.

**Direction-agnosticism.** The rule is "the side inside the VM allocates,
because only it can produce pages the hypervisor can locate". Three traits now,
because step 0 split the second one in half:

```
trait ArenaProvider {          // the side inside the VM
    fn commit(&self, len, granularity) -> Result<Extent>;   // large pages, then 4 KiB
    fn stamp(&self, extent: &Extent, run_id: u64);          // one record per page, or per
                                                            // large page; skip if frames known
    fn frames(&self, extent: &Extent) -> Option<RunList>;   // Some only on the AWE path
}
trait ArenaLocator {           // the attaching side
    fn locate(&self, fd: BorrowedFd, desc: &ExtentDesc) -> Result<PageMap>;
}
trait ArenaAttacher {          // the attaching side
    fn attach(&self, fd: BorrowedFd, map: &PageMap) -> Result<View>;  // reserve + MAP_FIXED per run
    fn detach(&self, view: View);                                     // one munmap
}
```

`frames()` returning `Option` is the honest encoding of what step 0 found: page
enumeration is a *capability of one allocation path*, not a property of the
guest. Today the GPU side provides and the app side locates and attaches; if the
app side were the VM they swap, and nothing above §5 changes.

### 5.2 Slabs, cache class, and the allocator

The arena is divided into **slabs** of `arena_slab_size` (default 64 MiB). A
slab is the unit of GPU-side import and the unit of cache-attribute uniformity.
gfxstream uses 16 MiB with 1 MiB rounding for small allocations
(`kDefaultHostMemBlockSize`, fwd §2.1); 64 MiB is the same idea with fewer
driver imports, chosen because `maxMemoryAllocationCount` is a real limit and
each import is a pinning operation.

**A slab is not the attach unit.** That is the one structural change
fragmentation forces, and it is a clarification rather than a redesign: pages
are committed, located and stitched per *extent* (§5.7), and a slab is a
sub-range of an extent that the GPU side imports. The distinction did not matter
when a slab was expected to be one contiguous run; it matters a great deal when
attaching a 64 MiB window costs 22 mappings on the intended path and 1471 on the
fallback — and when a whole 2 GiB large-page arena costs two (§5.7).

> **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "Two things the 1 GiB run
> settled on the way"]**
>
> ***64 MiB stays, and the 2 MiB granularity is why it is now comfortable rather
> than arbitrary.** A slab is exactly 32 large pages, so `GetLargePageMinimum()`
> rounding wastes nothing and every slab boundary is 2 MiB-aligned for free.
> Shrinking the slab towards 2 MiB would multiply driver imports by up to 32
> against `maxMemoryAllocationCount` and buy nothing, because the attach cost it
> would be trying to reduce is not paid per slab. Growing it buys nothing either,
> for the same reason. The one scenario that would force a much larger slab —
> locating each slab independently, at 1.8 s a sweep — is precisely the design
> §5.7 rejects.*
>
> The conclusion survives; that justification does not. "Exactly 32 large pages"
> was a tidy coincidence and it stopped being load-bearing the moment a 1 GiB
> window came back as **one** run and the driver accepted a **2 GiB** single
> import (`measured-gpu-side.md`). Alignment is free at any multiple of 2 MiB, so
> it does not pick 64 MiB out of the field.

**64 MiB stays, on narrower and more honest grounds: it is a lazy-import unit and
a failure unit, and nothing else.** The measurements removed every other
constraint that might have set it. The driver does not limit import size at these
scales; contiguity gets *better* with size rather than worse, so a big slab is not
harder to obtain than a small one; attach is per extent, so slab count does not
affect it; and locating is per extent too, so §5.7's 1.8-second argument no longer
applies per slab either. What is left:

- **Smaller is worse.** Driver imports are counted against
  `maxMemoryAllocationCount` and each one pins; 2 MiB slabs would multiply them
  by 32 to buy nothing.
- **Larger is not better here.** A slab is imported when the app side first needs
  it, so slab size is the granularity at which the driver's pinning tracks
  demand, and 64 MiB already amortises the import cost to nothing on a 2 GiB
  arena (32 imports, worst case, for the whole session).
- **It is a configuration value, not a constant.** `arena_slab_size` is in the
  header (§4.2) and both sides read it. *What would change it:* a measurement
  showing import time or `maxMemoryAllocationCount` binds — in which case raise
  it, because a single import of the entire arena is measured legal.
- **It is also the largest allocation an application can make**, and that is the
  reason it must never be quietly lowered. §5.3 makes a `VkDeviceMemory` a
  sub-range of *one* slab, so `alloc` refuses anything bigger with
  `LargerThanSlab`, and the ICD has no result code for that but
  `VK_ERROR_OUT_OF_DEVICE_MEMORY` — which an application reads as "the arena is
  full" for an arena that is 99% free. `Geometry::for_arena` shipped 4 MiB
  against this section's 64, and the first thing to notice was a 1080p swapchain
  (8.29 MB a frame) failing while a 256×256 one worked (§0.6 item 5). Any
  application wanting one large staging buffer would have hit it first.

Sub-allocation granularity inside a slab is unaffected and stays at 4096 bytes:
it is an offset into an already-imported `VkDeviceMemory`, and the large page
underneath imposes nothing on it.

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
- Slabs are **imported** lazily. When the app side needs a slab it picks a free
  region of the arena, writes a slab-directory entry, and sends
  `svImportArenaSlab`. The GPU side imports it and replies. A failed import is a
  hard error surfaced as `VK_ERROR_OUT_OF_DEVICE_MEMORY` from the triggering
  `vkAllocateMemory`, plus a reverse-ring diagnostic. *Note the narrowing:* the
  original wording was "created lazily", which implied the pages themselves
  arrived on demand. They do not — the extent is committed, locked and attached
  up front (§5.7), so lazy import bounds the number of driver imports and the
  driver's own pinning, and does nothing for the guest's resident set. R24's
  mitigation "allocate slabs lazily so the resident set tracks actual use" is
  superseded for that reason; what actually limits guest cost is the configured
  arena size and releasing the extent on last disconnect.
- Slabs are **never unimported while the process lives.** Unmapping a slab the
  GPU might still be reading is the one lifetime race with no cheap answer, and
  slab churn buys nothing: the arena is a fixed window anyway.
- **On a 4 KiB-granularity extent the allocator reserves a canary per slab** and
  never hands those bytes to an application: the **first 64 bytes of each
  slab-sized block**, containing `{magic, run_id, slab_index}`, written by the
  GPU side after the extent is stamped and located. The attaching side re-checks them at every
  handshake and on a timer, and a mismatch is fatal to the session. This is R26's
  detector — `VirtualLock` promises residency, not frame stability (§5.1) — and
  it exists only on the 4 KiB path, because large pages remove the failure by
  construction. Cost: 64 bytes per 64 MiB, which is 1 part in a million, against
  a failure mode whose alternative is silence.

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

That the app-side view is stitched from tens or thousands of separate mappings
(§5.7) is invisible here and must stay invisible. The reserved range is
virtually contiguous, every internal offset matches the GPU side's, and no
structure above `ArenaAttacher` — not the allocator, not the encoder, not the
`transform` pass — is permitted to know how many `mmap` calls produced it. The
run boundaries are not aligned to anything meaningful and an allocation
straddles them freely; that is fine, because they are boundaries in the *file*,
not in the address space.

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

**Where the dma-buf comes from is now a §1a question, and the answer differs by
topology.** A dma-buf the GPU side receives is a descriptor that crossed the
boundary and is forbidden; a dma-buf the GPU side **makes**, locally, out of
memory it already maps, is not. Across a hypervisor the second is available and
is the AMD answer: the GPU side wraps QEMU's memory backend with `/dev/udmabuf`
and imports the result, and nothing crosses but the extent descriptor. On one
machine it is *not* available, and §0.7 finding 4 has the measurement — the
handoff object must be nameable and a udmabuf's must be sealable, and on Linux
no object is both. `Provider::for_driver_across_a_vm` answering `None` for AMD is
therefore a statement about a group membership (`/dev/udmabuf` is `root:kvm`
0660) and not about the design.

**Pluggability is not speculative refinement.** RADV rejects *every* shared
mapping through this path — libdrm passes `AMDGPU_GEM_USERPTR_ANONONLY`
unconditionally and Linux gives `MAP_SHARED|MAP_ANONYMOUS` a backing file via
`shmem_zero_setup()` — so a shared arena is unimportable on AMD/Linux by
construction, surfacing as `VK_ERROR_INVALID_EXTERNAL_HANDLE` (fwd §2.1). The
brief requires either side to be either OS, so the trait exists from the first
commit even though `testhost` only ever exercises the first row.

**Every provider makes the arena external memory, and that reaches every buffer
bound into it.** A `VkDeviceMemory` that was exported or imported may only have
buffers bound to it that were *created* naming at least one of the same external
handle types (`VUID-vkBindBufferMemory-memory-02726` for the export row of the
table above, `-02727` for the import rows). The obligation is on `vkCreateBuffer`
— before the buffer exists, and long before anyone has chosen memory for it — so
it cannot be applied selectively to the buffers that will land in the arena, and
it cannot be applied by the app side, which does not know which row of this table
is in force. It is therefore applied by the GPU side, to every forwarded buffer,
from `Arena::external_handle_type`; §0.5 has the consequences and
`GpuServer::create_buffer` has the code. The trait keeps its signature: the
handle type is a property of the `Arena` the provider returned, not an argument
to `import`.

**`SlabImport` is unchanged by the fragmentation finding, and that is worth
saying rather than leaving implicit.** The GPU side imports a pointer into its
own *virtual* address space, which `VirtualAlloc` made contiguous regardless of
how the frames underneath are laid out. Scattering is entirely an attaching-side
problem; the importing side never learns of it, the trait needs no run list, and
`import(device, arena_offset, size, cache_class)` keeps its signature.

> **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "Does the driver still
> import them?"]**
> *The only thing the large-page decision adds is a question for the
> implementation to answer empirically — whether this call still succeeds when
> the pages behind the pointer are 2 MiB PTEs (§0, §14).*

It succeeds, with the same `memoryTypeBits = 0x6` and the same
`mapped_same_address = true` as every other allocator, and it succeeds at 64 MiB,
1 GiB and 2 GiB. The trait adds nothing at all: **`import` is granularity-blind,
allocator-blind and — up to 2 GiB — size-blind.**

**Copy gfxstream's defensive memory-type rewrite.** Call
`vkGetMemoryHostPointerPropertiesEXT` first; if the preferred type index is not
in the returned `memoryTypeBits`, pick the lowest set bit instead. A
`memoryTypeBits` mask that excludes your choice is a normal condition to handle,
not a failure (fwd §2.1). Measured on the real target, for both ordinary process
memory and host-shared guest RAM, the mask is `0x6` and the choice is index 1 —
so on this hardware the rewrite never fires. Keep it anyway: it costs one branch
and the whole point is that the mask is the driver's business, not ours
(`measured-gpu-side.md`).

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

**Step 0 supports this and does not settle it.** Bytes written by the host
appeared through the guest's plain mapping *and* through the driver's
`VkDeviceMemory` mapping; bytes written through the `VkDeviceMemory` mapping
appeared in the memfd and in QEMU's guest-physical reader — 16384 pages, in both
directions, with no flush of any kind performed on either side
(`measured-gpu-side.md`). But it was measured **at rest**: one writer at a time,
with a file rendezvous between phases. It says nothing about concurrent access
and nothing about cache behaviour under load. Treat §5.6 as an argument with
corroboration, not as a measurement, and treat R10 as retired *by construction*
— which is what the text below actually claims — rather than retired by
experiment.

**The second round added allocators, not writers.** Large-page, AWE and
`NONPAGED_LARGE` windows each passed the same loop — host writes, guest reads
through the `VkDeviceMemory` mapping, guest writes back, host reads — so the
conclusion now holds across every allocator and at 1 GiB as well as 64 MiB. It is
still one writer at a time with a file rendezvous between phases. **Concurrent
access has never been measured**, and that gap has not narrowed (§14).

The interesting question was whether to follow gfxstream in deriving a
per-allocation cache attribute from the memory type — WC for upload, WB for
readback. **We do not, in v1, for a concrete reason:** the GPU-side window is
write-back on every path §5.1 contemplates — `VirtualAlloc` with or without
`MEM_LARGE_PAGES`, and `MapUserPhysicalPages`, which takes no cache attribute at
all — while the app side's view is an ordinary `mmap` of a memfd, also
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

### 5.7 Fragmentation, and what attaching actually costs

Step 0 measured the thing this section did not previously contemplate: **the
pages of a pinned Windows allocation are scattered in guest-physical space.**

| Window | Allocation units | Runs in the memfd | Mean run |
|---|---:|---:|---|
| 1 MiB, 4 KiB pages | 256 pages | 29 | ~35 KiB |
| 64 MiB, 4 KiB pages | 16384 pages | **1471** | ~44 KiB |
| 64 MiB, AWE 4 KiB frames | 16384 frames | **5863 – 16339** | ~11–44 KiB |
| 16 MiB, `NONPAGED_LARGE` | 3992 of 16384 asked | **3992** | 4 KiB |
| 64 MiB, 2 MiB pages | 32 large pages | **22** | ~2.9 MiB |
| 1 GiB, 2 MiB pages | 512 large pages | **1** | 1 GiB |
| 2 GiB, 2 MiB pages | 1024 large pages | **2** | 1 GiB |

**Every row is now measured** (`measured-gpu-side.md`, "Fragmentation, which was
the point" and "Two things the 1 GiB run settled on the way"). Three things in
that table were not what this section originally predicted. The large-page count
beat the arithmetic — 22 rather than ≤ 32, because ten pairs of adjacent 2 MiB
blocks merged — and it keeps improving with size, a gigabyte arriving as a single
extent. AWE is the *worst* row, not a better one. And `NONPAGED_LARGE`, which
this document twice named as the fix, returned scattered 4 KiB frames and short
by 12392 of them.

**Every row of that table is a *freshly booted guest*, and that is a property of
the measurement rather than of the allocator** (§0.14, 2026-08-13). Repeated on
the same guest after ~2 days of ordinary use: 64 MiB of large pages is **26**
runs rather than 22, and **1 GiB is 399 runs rather than 1**. The ranking is
unchanged and the conclusion — large pages beat every alternative by two orders
of magnitude — is untouched. What does not survive is the sentence below about
"one or two `mmap` calls for the whole arena": on a guest that has been up for
days it is a few hundred, which the `vm.max_map_count` budget absorbs without
noticing but which is not the same claim. Read the table as the best case and
the paragraph after it as the one that binds.

Roughly 23 runs per MiB on the 4 KiB path is not a rounding error, it is a
factor of 1471, and it lands entirely on the attaching side. Working through it
honestly:

**Attach is one `mmap(MAP_FIXED)` per run, into a reserved `PROT_NONE` range.**
That mechanism was already in §4.3 and it is still correct; only the count was
wrong. The kernel cannot merge the resulting VMAs — adjacent runs are by
definition non-adjacent in the file — so **VMA count equals run count exactly**.
A 2 GiB arena is ~47,000 VMAs on the 4 KiB path and, measured rather than
estimated, **2** on the large-page path. The earlier figure of ~1,024 assumed
large pages would not coalesce; they do.

**It is affordable, and it is now a documented host dependency.**
`vm.max_map_count` is **1048576** on this host (`hardware-constraints.md`), so
even the 4 KiB path at 2 GiB sits at 4.5% of the per-process limit. That is a
property of this host and not of Linux: **at the older default of 65530 the 4 KiB
path would have capped the arena at roughly 2.8 GiB, and at a size varying with
the *guest's* fragmentation rather than with anything we control** — the worst
kind of limit, since the same binary would fail on a different day. The
large-page path does not care either way, which is a third argument for it. The
limit is per process and every
ICD client builds its own view from the fd and run list that `sv-hostd` hands
it, so this is a cost each application pays, not a system total. `sv-hostd`
reads `vm.max_map_count` at startup, compares it against the run count it just
built, and **refuses loudly with the actual numbers** rather than letting a
client discover it at the 1048577th mapping. The alternative failure is an
`mmap` returning `ENOMEM` halfway through building an arena, which is
recoverable in principle and horrible in practice.

**Latency: bounded by measurement, not yet measured directly.** The step-0 host
wrote all 16384 pages of a 64 MiB window through per-page `mmap(MAP_SHARED)`
plus `msync` in **60 ms**. Run-granular attach does 11× fewer calls and no
writes and no `msync`, so 60 ms is a generous upper bound for a 64 MiB attach on
the 4 KiB path, and a 2 GiB arena is 32 slabs' worth of the same work. Call the
budget **under two seconds, once, at startup**, and measure it before believing
it. On the large-page path it is one or two `mmap` calls for the whole arena and
disappears entirely. Detach is not a problem in either case: a single `munmap`
over the whole reservation frees every VMA inside it regardless of how many there
are. **Attach time at full arena size is still not measured directly** — §14.

**Locating dominates, which is why the extent is the attach unit.** Locating
sweeps the entire memfd — **1.8 s at 64 MiB, 2.02 s at 1 GiB, 2.32 s at 2 GiB**,
about 7.2 GiB/s — and that cost is a function of *guest RAM*, not of arena size:
the sweep reads all 13312 MiB whatever the window is, and only match handling
grows, so 32× the arena costs 0.5 s more.

> **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "So how does the host find
> large-page memory? Ranked"]**
> *The 4 KiB path locates by sweeping the entire memfd — 1.8 s for 13312 MiB …
> and that cost is a function of guest RAM, not of arena size.*
>
> Two corrections. It is not the 4 KiB path's problem — v1 scans on **every**
> path, including large pages (§5.1). And the linearity claim was a hypothesis
> extrapolated from a single 64 MiB run, flagged as unproven here and in §14; it
> has now been measured at 1 GiB and 2 GiB and holds, so this argument stands on
> data rather than on arithmetic.

Paying it per slab would put two seconds inside a `vkAllocateMemory` that
happened to need a new slab, which is not a latency, it is a fault. Hence D22:

- **One extent covers both regions.** A single `VirtualAlloc` holding the 16 MiB
  control region and the whole arena, stamped with one `run_id` and a per-page
  `region_tag`, located by one sweep, then stitched into **two** reserved ranges
  with independent bases. §4.1's "two independent mappings" is a statement about
  the protocol and survives intact; there is simply no reason to sweep 13 GiB
  twice.
- **The arena is committed at `sv-gpu-server` startup and does not grow, on
  either path.** R11 already says the arena is a fixed pre-negotiated window;
  this makes it true for a second, independent reason. Size it from
  configuration, default modestly on a machine whose RAM belongs to somebody
  else (R24), and release the extent when the last client disconnects.

  > **[SUPERSEDED — 2026-08-11, `measured-gpu-side.md` "So how does the host find
  > large-page memory? Ranked"]**
  > ***On the large-page path growth becomes cheap and is allowed.**
  > `ProbeLocator` reads one record every 2 MiB — 6656 probes for 13 GiB — so a
  > second extent costs milliseconds rather than seconds, and lazy growth is back
  > on the table as an option we do not have to take in v1.*
  >
  > It was conditional on `ProbeLocator`, which v1 does not implement because the
  > scan turned out to cost ~2 s on any path (§5.1). With the scan, a second
  > extent costs a second sweep, so growth is uniformly expensive and uniformly
  > refused. R11's amendment says the same thing from the allocator's end:
  > contiguous physical memory decays with guest uptime, so a slab added an hour
  > into a session is the one most likely to fail. Growth stays out of v1, and
  > the probe is what would bring it back.

**Bookkeeping per slab: none.** That is the point of moving attach to the
extent. The run list is per-extent state held by `sv-hostd` and by each client's
attacher: ~24 bytes per run, so ~1.1 MiB for a 2 GiB arena on the 4 KiB path and
**48 bytes** on the large-page path, where the measured run count for 2 GiB is
two. The transient page map during locate is 8 bytes
per page and is dropped once coalesced. A slab-directory entry gains nothing and
carries no runs.

**The run list does not cross the wire, and it never did have to.** On the
locating paths the guest does not know it — that is the entire reason
`ScanLocator` exists — and on the AWE path what crosses is the frame list, from
which the runs are derived on the far side. So in no configuration is the run
list itself protocol; it is a derived, side-local artefact, rebuilt per
attachment rather than carried. `sv-hostd` computes it once per generation and
hands it to each client with the fd, which is a local optimisation and not a
protocol feature.

**`SlabImport` is untouched; `ArenaAttacher` changed shape.** §5.4 explains the
first. The second is the trait split in §5.1: locating was folded into
"translate frames to a file offset" when frames were assumed available, and it
is now a separate stage with three implementations, because on the path we
actually measured there are no frames to translate.

**AWE would not have fixed this, and measurement made the point more sharply
than the argument did.** `AllocateUserPhysicalPages` chooses frames and promises
nothing whatsoever about their contiguity, so an AWE-backed window was expected
to be *as* scattered as the `VirtualLock`ed one. It is **worse**: 5863 runs on
one attempt and 16339 — one frame per run, complete scatter — on another, against
1471 for `VirtualLock` and 22 for large pages. It is also unstable between
attempts, because it depends on the guest's free-page state at the moment of the
call, which means no capacity planning is possible against it. The single thing
it buys is that the host is told the offsets instead of scanning for them, and
the scan costs ~2 s (§5.1).

The other mechanism this section named as a fix, `AllocateUserPhysicalPages2`
with `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`, **does not work on build 26200**:
3992 frames of 16384 requested, one page per run. So of the four allocators
tried, exactly one fixes fragmentation — `MEM_LARGE_PAGES` (§5.1). Fragmentation
and page enumeration remain orthogonal problems that happened to share a
prerequisite, and no available mechanism solves both.

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
- **Optional pointers as a one-word presence flag**, not gfxstream's 8-byte
  big-endian pointer token, because it does not put an address on the wire
  (fwd §4.3).

  > **[SUPERSEDED — 2026-08-11, the codegen stage as built; Venus's own source in
  > `reference/venus/NOTES.md`]**
  > ***Optional pointers as a one-word boolean**, not gfxstream's 8-byte
  > big-endian pointer token. It is smaller and does not put addresses on the
  > wire.*
  >
  > It is not smaller. `vn_encode_simple_pointer` is `vn_encode_array_size(enc,
  > val ? 1 : 0)` — a `uint64_t` 0 or 1, **eight bytes**, exactly the width of
  > gfxstream's token. The size claim is dropped; the reason to prefer it is
  > that it carries no address, which was always the argument that mattered.
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

> **[SUPERSEDED — 2026-08-11, the codegen stage as built; see
> `crates/sv-testkit/src/venus.rs` and `tests/venus_differential.rs`]**
>
> *Everything else follows Venus. Deviating further would mean losing the ability
> to differentially test our encoder against Venus's MIT-licensed generated C on
> the shared subset, which is the strongest correctness evidence available and far
> better than the symmetric self-consistency R8 correctly says proves nothing
> (fwd §9).*
>
> **The byte-for-byte comparison was already impossible when this was written**,
> and by our own hand. D6 replaces Venus's per-parent `pNext` switch with
> length-prefixed links, and deviation 1 above adds a `u32` length to every
> command header; nearly every Vulkan struct carries a `pNext`, so between them
> those two decisions change the bytes of almost everything. This paragraph
> treated a constraint it had already broken as a reason not to break others.

**Everything else follows Venus, and the differential test compares structure
rather than bytes.** What was built parses Venus's generated C and compares, for
each struct's `_self` body, the **ordered sequence of members and for each one its
kind and width** — scalar of a given width, array, pointer-to-one, blob, string.
That is exactly the information the generator derives from `len=`, `optional=`
and the C declarator, which are the three things R8 is about, and it catches the
whole class: a member one generator emits and the other does not, a member
ordering difference, an array read as a single value or the reverse, a scalar at
the wrong width. **208 structs are shared with the Zink profile and all 208
agree.**

It is stronger than a byte comparison in the respect that matters and weaker in
one that does not. Stronger: a byte comparison over one sample value can pass
while `len=` handling is wrong for every *other* value, whereas the structural
comparison holds for all values. Weaker: it does not check the primitive
encodings themselves, which are pinned separately by unit tests in `sv-wire`
against the quoted C in `reference/venus/NOTES.md`. Deviating further is still
undesirable — it would shrink the shared subset — but the constraint is "keep the
member sequences comparable", not "keep the bytes identical", and it is a weaker
constraint than this section claimed.

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

**Measured against that exit condition, 2026-08-11: it is met with room.** The
declarator parser is **375 lines** with no special cases against the ~800 that
would have flipped the decision, and the exception table holds **~11 entries**
against ~20 (§7.4). Pure Rust stands, and the JSON model is not revisited.

Registry sourcing is unchanged: `$VK_XML` from the pinned `vulkan-headers`, no
fallback to a system path, exactly as `vk-registry` already enforces.

**[AMENDED — 2026-08-13, §0.9 item 1.] There are two registries, from one
package.** `vk.xml` does not define the `StdVideo*` types; it names them with
`requires="vk_video/vulkan_video_codec_h264std.h"` and stops, which is what made
`docs/video-decode.md` §4 call video decode unforwardable. The definitions have a
registry of their own — `share/vulkan/registry/video.xml`, beside `vk.xml` in the
same store path, and the file the `vk_video/*.h` headers are generated *from*. It
is the same schema, read by the same parser, and merged in
`vk_registry::Model::merge_video`. Its path is **derived** from `$VK_XML` and a
missing file is an error, so the drift objection above does not reappear: the two
are the same release by construction. Nothing else about this section changes —
in particular the generator still reads no C, and the headers are read only by
`sv-testkit` as the independent check.

**A `<feature>` is not a leaf, and assuming it is produces a small protocol
rather than an error.** As of `vulkan-headers 1.4.350` each core version is split
into `apitype="internal"` sub-features chained by `depends=`:

```xml
<feature apitype="internal" name="VK_BASE_VERSION_1_0"     number="1.0">…73 commands…
<feature apitype="internal" name="VK_COMPUTE_VERSION_1_0"  depends="VK_BASE_VERSION_1_0">…36…
<feature apitype="internal" name="VK_GRAPHICS_VERSION_1_0" depends="VK_COMPUTE_VERSION_1_0">…28…
<feature                    name="VK_VERSION_1_0"          depends="VK_GRAPHICS_VERSION_1_0">…0…
```

A resolver that reads `<feature name="VK_VERSION_1_0">` and stops therefore gets
**no** commands from Vulkan 1.0 and does not notice. Ours did exactly that: the
Zink profile resolved to **55 commands instead of 239**, and it was caught only
because the profile test asserts a lower bound on the command count. The model
resolves the `depends=` expression language (`,` disjunction, `+` conjunction,
`+` binding tighter, parentheses) rather than reading feature names, and **every
profile carries an asserted command count**, because "the generator quietly
emitted a smaller protocol" is the same silent-wrongness R8 is about and there is
no other check that would have caught it. §11 and §13 step 3 both assumed the
leaf shape and are corrected accordingly.

### 7.2 The passes

Adapted from gfxstream's taxonomy, which is the most reusable artefact in the
research because each pass is a full walk over every struct and each exists for
a reason we would otherwise discover painfully (fwd §1.3).

| # | Pass | Side | What it emits |
|---|---|---|---|
| 1 | `size` | app | Exact serialised byte count, so a contiguous span can be reserved before writing |
| 2 | `encode` | app | Writes into a reserved span with a bump cursor — straight into the ring or the arena blob, no intermediate buffer |
| 3 | `decode` | GPU | Field-by-field reads through a bounds-checked cursor into owned storage |
| 4 | ~~`arena_bind`~~ | GPU | **Not emitted.** Ties decoded pointer-shaped fields to bump-allocated storage with the packet's lifetime (gfxstream's `deepcopy`, minus the C++ pool) — see below |
| 5 | `handle_map` | GPU | Walks a struct graph replacing ids with driver handles |
| 6 | `transform` | GPU | Semantic, not syntactic: the `(memory, offset)` rewrite of §5.3, memory-type-index remapping, format/limit fixups |
| 7 | `pnext` | both | Chain walk with length-prefixed links |
| 8 | `dispatch` | GPU | The opcode table. `ash` supplies driver dispatch tables for free |
| 9 | `subdecode` | GPU | The restricted `vkCmd*` decoder of §6.5 |
| 10 | `icd_entry` | app | `extern "system"` stubs and the `vkGetInstanceProcAddr`/`vkGetDeviceProcAddr` tables |
| 11 | `roundtrip_tests` | both | Generated property tests: encode → decode → compare, including pNext chains |
| 12 | `gpu_entry` | GPU | **wire → C**, the driver's function table, and the forwarding `Server` impl (added 2026-08-12) |

**This table is a two-by-two with a corner missing, and the corner is not
optional** (2026-08-12, the Zink stage). "Which side" × "which direction" has
four cells; passes 10 and 11 cover three of them and nothing covered *wire → C on
the GPU side*, which is what every `vkCreate*` needs to call a driver at all. The
previous stage wrote it by hand for about forty commands and recorded that it
would not scale; pass 12 generates it for 223 and the hand-written GPU side is
sixteen commands, each listed with its reason in
`sv_codegen::exceptions::GPU_HAND_WRITTEN`.

**And it is really a two-by-two-by-two.** The remaining cell — C → wire on the
GPU side — looked like a free reuse of pass 10's `lower_*` and is not, because
what differs between the two sides is not the direction but the **validity
state**: a `returnedonly` struct's members are the *caller's uninitialised
memory* on the app side (§6.1's three-state validity says do not read them) and
*the driver's answer* on the GPU side. Reusing the app side's lowering produced a
`VkPhysicalDeviceProperties` with an empty `deviceName`. Pass 12 emits
`lower_full_*` from the same emitter with that one flag flipped.

**Pass 4 is not dead; it moved into pass 12.** It is struck through below because
*decoding* has nothing to bind — but *raising* does, and it is exactly
gfxstream's `deepcopy`: a C `VkGraphicsPipelineCreateInfo` holds eight pointers
to sub-structures that must outlive the call. Its pool is `sv_gpu_entry::Raise`.

**Passes 5 and 6 partition the handle space.** `VkDeviceMemory` belongs to pass
6 and pass 5 must skip it: an app-side memory id is not a table entry — it is a
sentinel or a slab offset — so only the `(memory, offset)` *pair* can be
resolved. Both passes claiming it took the ring fatal on the first
`vkBindBufferMemory` an application made (§0.4).

Not emitted, deliberately: gfxstream's `decodersnapshot` (7,063 lines of VM
snapshot support we will never use) and `api_log_decoder`. `marshaling`,
gfxstream's older safe path kept in parallel with `reservedmarshaling`, is also
skipped — one path, and it is the fast one, because a second path is a second
place to be subtly wrong.

**Pass 4 turned out to have nothing to do, and is not emitted** (2026-08-11, the
codegen stage as built). gfxstream needs it because its decoder produces C
structs full of raw pointers that must be made to point at pool-allocated storage
with the packet's lifetime. Our decoder produces owned Rust values — `Vec<T>`,
`Option<Box<T>>` — whose storage *is* their lifetime, so there is no binding step
to generate. It is struck through above rather than deleted because a reader
comparing the emitted crate against this table deserves to know why a file is
missing; the pass numbering is otherwise unchanged. Ten passes were emitted at that point; pass 12 is the eleventh (§0.4).

The `(size, encode)` split is not collapsible. Both mature projects have a
separate size pass and both need it for the same reason: you cannot reserve a
ring span before you know how big the message is (fwd §1.3, §4.4). The two are
generated from one shared fold over the struct shape so they cannot drift, and
`Encoder::finish` checks at run time that they agreed — which turns "the two
passes disagree" from a silent desynchronisation into an error at the first
packet.

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

**A live R8 bug, worth recording in this document's own words**, because it is
the exact failure the risk describes and it was found in the first week. `vk-parse`
exposes an extension enum's sign as `EnumSpec::Offset { dir, .. }`, and `dir` is
named after the registry attribute `dir="-"` but holds the *opposite* sense: it is
`false` when the XML says `dir="-"` and `true` otherwise. Read as "is negative" —
the obvious reading, and the one written first — it **inverts the sign of every
extension-introduced enum value in the registry**, which for us would mean the
wrong `sType` on every extension struct. The round-trip tests passed, because both
sides agreed on the wrong number; symmetric self-consistency proves nothing, which
is precisely R8's claim, demonstrated rather than asserted. It was caught by a test
asserting generated constants against published `vulkan_core.h` values.

The rule it produces: **every derived numeric value gets at least one assertion
against an independently published number.** Round-trip tests check that we are
self-consistent; only an external fixture checks that we are right. This is the
same argument §7.1 makes for keeping `vulkan_object.py`'s output as a fixture, and
it is why that fixture is not optional.

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

  **[CORRECTED — 2026-08-13, §0.9 item 3.]** The registry *can* describe them —
  `vk.xml` writes `<member><type>uint32_t</type><name>mask</name> : 8</member>`
  and `vk-parse` carries the width — and `video.xml` has 293 of them, so the
  video profile does not exist without them. `plan::FlagWordPlan` carries a
  struct whose members are **all** `uint32_t` bitfields fitting one word, as
  that word. `VkAccelerationStructureInstanceKHR` mixes bitfields with ordinary
  members and is still refused, so this entry stands as written for the type it
  names; what changed is that it is no longer a statement about bitfields in
  general.

Every exception is a data entry in one file, and an unlisted case is a build
failure. That is the difference between a generator that is honest about its
limits and one that emits plausible-looking wrong code.

**Measured against that budget: three of the six dissolved, and ~11 entries
remain.** They dissolved because they are artefacts of generating C, and this
document budgeted them by reading two C generators (2026-08-11, the codegen stage
as built):

- **Unions.** A Rust `enum` *is* the tag, so an encoded union always knows which
  member it holds and no default is ever guessed. Venus carries a
  `UNION_DEFAULT_TAGS` table for exactly this problem and gfxstream always
  marshals member `[0]`; we hardcode nothing. `selector=` is still recorded in
  the model for validity checks, and a union member the generator cannot type is
  still a build failure.
- **Conditional presence.** `pTessellationState` is `Option<Box<…>>` and its
  presence is a fact of the value, not something to recompute — no closure table
  is needed for marshalling. The condition does matter when *lowering from the
  application's C structs*, which is `sv-icd-entry`'s problem, and the table
  belongs there when that crate is written.
- **`len=` vs `altlen=`.** Cheaper than budgeted, and the reason is an unplanned
  R8 mitigation: **every variable-length thing on our wire carries its own
  explicit `u64` count**, so `len=` is a *cross-check* rather than a load-bearing
  input. A wrong `len=` in the registry cannot desynchronise the stream — it can
  only make the generated consistency check fire. The registry metadata this
  project most feared being wrong about was demoted from input to verifier by a
  decision taken for a different reason.

The three that survive — the `void*` policy table, `VkAllocationCallbacks`, and
the bitfield structs out of scope with ray tracing — are ~11 entries in total,
against the ~20 that would have flipped §7.1's decision to the JSON model.

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
| `sv-codegen` (model consumers, ten emitted passes, exception table) | 4,500 |
| `vk-registry` typed model + C declarator parser | 1,600 |
| `sv-wire` (cursor, headers, primitives) | 600 |
| `sv-transport` (regions, rings, handshake, parkers, backends) | 1,800 |
| `sv-arena` (allocator, slabs, deferred free, provider/locator/attacher traits) | 1,200 |
| `sv-handles` (id allocation, object table) | 600 |
| `sv-icd` hand-written half (instance/device/queue objects, WSI, memory) | 1,800 |
| `sv-gpu-server` (ring consumers, semantic glue, import, submit path) | 4,500 |
| `sv-present` | 1,000 |
| `sv-hostd` (mtree region table, three locators, coalescing, stitching, fd handoff) | 1,200 |
| Tools, probes, integration tests | 2,000 |
| **Total hand-written** | **~20,000** |

The two revised lines are step 0's doing: `sv-hostd` inherited a region table, a
coalescer and three locators it was not budgeted for, and `sv-arena` inherited
the provider/locator/attacher split (§5.1). Neither moves the total materially.

**Measured for the part that exists, 2026-08-11.** The generator, the registry
model, the wire primitives and the test kit together are **10,145 hand-written
lines**, and they emit **126,205 generated lines** for a **239-command Zink
profile**, with 657 tests and a clean Miri run. Read against the tables above:
the generated estimate was low per command — 126k for 239 commands where 104k was
budgeted for ~450 — because the estimate was extrapolated from Venus's and
gfxstream's line counts for a different pass split and a different language, and
because generated Rust with per-field bounds checks is more verbose than the C it
was calibrated against. The hand-written estimate is roughly holding: the 10,145
spent covers `sv-codegen`, `vk-registry` and `sv-wire` — budgeted at 6,700
together — plus `sv-testkit`, which falls under the 2,000-line tools-and-tests
row. Call it 1.2–1.5× over on the parts that exist, and carry that factor rather
than the totals when reading the rest of the table. The line item to keep watching
is still `sv-gpu-server`.

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
the `…PresentationSupportKHR` family. **`VK_EXT_headless_surface` is
the first target** (fwd §3.4, §10). A silent-failure trap to remember:
`terminator_CreateSwapchainKHR` with a NULL driver pointer returns `VK_SUCCESS`
with an uninitialised swapchain.

> **[CORRECTED — 2026-08-12, §0.6 items 2 and 3; read against
> `Vulkan-Loader v1.4.350` `loader/wsi.c`]**
>
> *We opt out of interface-3 surface creation by not exposing
> `vkCreateXXXSurfaceKHR`, so the loader hands us a `VkIcdSurfaceXXX` from
> `vk_icd.h` which we cast.*
>
> **That opt-out no longer exists.** `wsi_unwrap_icd_surface` creates the
> driver's own surface lazily by calling the driver's `vkCreateXxxSurfaceKHR`,
> for every ICD that enables `VK_KHR_surface` at interface ≥ 3, and turns a
> missing one into `*pSupported = false` and `VK_ERROR_SURFACE_LOST_KHR` with no
> diagnostic anywhere. An ICD must export **all** of the surface creators for the
> platforms it advertises, or none of them — and "none" is not available once it
> advertises any surface extension. The ICD therefore owns its surfaces and a
> `VkSurfaceKHR` is one of its own ids.
>
> Worse in the same family: `terminator_GetPhysicalDeviceXcbPresentationSupportKHR`
> and its siblings call **`abort()`** rather than answering `VK_FALSE` when the
> selected ICD does not export them.
>
> The three commands whose C signature names a platform type — the two X11
> surface creators and `vkCreateHeadlessSurfaceEXT`, which is in the list only
> so that all-or-none holds — are hand-written off the generated surface and
> injected into the proc tables by name (`emit::icd_entry::EXTRA_LOCAL`).

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
advertise the feature fails device creation cleanly and loudly in the consumers
that need it, which is a much better outcome than a device that advertises it and
then renders garbage. See §11 for the nuance.

> **[CORRECTED — 2026-08-12, §0.5 "Where Direct3D actually stands"; measured
> with `scripts/sv-dxvk-d3d11.sh`]** The consumers are **vkd3d-proton and DXVK
> ≥ 2.7**, not "DXVK 3.x". DXVK ≤ 2.6.2 does not require the feature — it asks
> for it only alongside `VK_NVX_binary_import` for CUDA interop — and reaches
> `D3D_FEATURE_LEVEL_12_0` on this device. What stops D3D11 there is
> `VK_KHR_swapchain` and therefore §10, not this decision.
>
> **[SUPERSEDED — 2026-08-12, §0.6]** Nothing stops it now: §10 stage 2 is built,
> `VK_KHR_swapchain` is advertised, and `scripts/sv-dxvk-d3d11.sh` renders and
> reads back a frame on DXVK 2.6.2. The cost of declining `bufferDeviceAddress`
> is what the correction above says it is and no more: D3D12 through
> vkd3d-proton, and DXVK ≥ 2.7.

**An extension in the profile is not an extension on the wire.** §11 and the
paragraph above both read as if advertising an extension meant forwarding its
commands, and `VK_KHR_swapchain` is the counterexample: it is in the profile so
that its *types* exist on the C surface and its *name* reaches the GPU side's
`vkCreateDevice` — which is what makes `VK_IMAGE_LAYOUT_PRESENT_SRC_KHR` a legal
layout over there — while every one of its commands is in `excluded_commands` and
is answered by `sv_icd::wsi`. The device-extension list the app side reports is
therefore *not* purely the intersection §0.4 item 8 describes: an extension
implemented entirely app-side is added unconditionally, because the side keeping
the promise is this one (§0.6 item 4).

We also do not advertise external memory/semaphore/fence handle types we cannot
honour, which is legal and avoids gfxstream's rather comical OPAQUE_FD cookie
workaround (fwd §2.9). **The three `vkGetPhysicalDeviceExternal*Properties`
queries are answered app-side with an all-zero reply** rather than forwarded,
because they are core 1.1 and an application can make them whether or not the
extensions are advertised — and the GPU-side driver's honest answer about its own
export capabilities is a false answer about ours (§0.5).

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

> **[BUILT — 2026-08-12, §0.6. Stages 1 and 2 are done and stage 2 is not what
> this section describes.]** Read this section for its decisions, which held, and
> §0.6 for what the implementation corrected:
>
> - **there is no `svPresentBlit` and there should not be one.** Everything below
>   the words "the app side sends" is done out of ordinary forwarded Vulkan, by
>   the app side, with a command buffer recorded once at swapchain creation.
>   `sv-gpu-server` has no presentation code at all.
> - **`vkQueuePresentKHR` costs 1.47 ms at 1080p**, or 5.4 GB/s of presented
>   pixels, against the 500 MB/s this section budgets for 60 Hz. The frame is
>   *not* the expensive part.
> - **triple buffering is not built.** Present waits on its own fence, so the
>   queue is of length zero and `IMMEDIATE`/`MAILBOX`/`FIFO` are the same thing.
>   The vertical flip is not needed — `vkCmdCopyImageToBuffer` has no flip — and
>   the stride is the actual width, so two of the three improvements over sharedgl
>   below are had for free and the third is outstanding.
> - **an arena-backed swapchain image is admissible on RADV** (measured:
>   `linearTilingFeatures` for `B8G8R8A8_UNORM` carries `COLOR_ATTACHMENT`) and is
>   deliberately not used. §0.6 item 6 has the three reasons.

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
   §13 steps 1–6 and costs nothing. **[DONE]**
2. **v2: a real swapchain.** App-side `VK_KHR_swapchain` whose images are
   GPU-side `VkImage`s. On `vkQueuePresentKHR` the app side sends `svPresentBlit`;
   the GPU side runs `vkCmdCopyImageToBuffer` into a **triple-buffered linear
   arena buffer** and signals a present timeline value. The app-side WSI then
   blits from the arena to the window with `wl_shm` or `XShm` — a straight
   memcpy from shared memory into a window buffer, with no GPU involvement and
   no second copy of our own. Format is negotiated at swapchain creation, not
   hardcoded to BGRA the way sharedgl hardcodes `GL_BGRA` in two places
   (sgl §3.6). **[DONE, minus the triple buffering and without `svPresentBlit`:
   the copy is a `vkQueueSubmit` of a command buffer the app side recorded, so
   the GPU side needed no code. §0.6 item 1.]**
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

**The Zink profile resolves to 239 commands**, and that number is asserted in the
generator's tests rather than trusted. It has to be: `vulkan-headers 1.4.350`
splits each core version into `apitype="internal"` sub-features chained by
`depends=`, so `<feature name="VK_VERSION_1_0">` contains no commands of its own,
and a resolver that treats a feature as a leaf produces **55** commands here
without any error at all (§7.1). "Zink's requirements" is a graph to be walked,
not a list to be read.

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

> **[CONFIRMED — 2026-08-12, §0.5; `scripts/sv-dxvk-d3d11.sh` against DXVK
> 2.6.2 under Wine 11.0]** Both halves of that sentence measured out exactly.
> The two required device extensions are `VK_EXT_robustness2`, which we
> advertise, and `VK_KHR_swapchain`, which we do not — and the second is the
> whole of what remains. DXVK enumerated the device, read §9.1's memory table
> back, and computed a maximum Direct3D feature level of **12_0** before
> refusing for want of a swapchain. §9.2's looser "DXVK 3.x" was the sentence
> that needed correcting, not this one.
>
> **[MET — 2026-08-12, §0.6]** `VK_KHR_swapchain` is advertised and implemented,
> so both required extensions are present and DXVK 2.6.2 creates the device,
> selects 11_1 and draws: 31.1% coverage and 1082 distinct colours, the same
> frame the identical program gets from RADV with no forwarding at all. The
> stretch goal is reached.

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

**Video decode is in scope, and the shape it takes is a protocol extension.**
The types cross today and the capability queries cross byte-identically to the
driver's own answers (§0.9); what does not is a decode, because the ICD
advertises one universal queue family and a decode queue is a second one. The
answer is **not** to advertise a second family. §0.6's rule — a capability
expressible in the forwarded API does not deserve a protocol extension — has its
mirror here: decode's *state machine* is expressible, but the queue it runs on
cannot be shown to the app side without a second ring (D8) and a family the
application would have to be told not to use. So the GPU side owns the video
session, the DPB and the decode queue **privately**, and what crosses is a
compressed bitstream and an image id. That is 188× less data at 1080p30
(`docs/video-decode.md` §2), it is the only candidate since
`vkUpdateDescriptorSetWithTemplate` that passes §0.6's test, and it is a stage of
its own: the GPU side has to allocate the DPB, keep a slot table per session and
answer a command the generator cannot write. **H.264 only** until something is
measured — AV1's per-frame structure has eight pointers where H.264's has none,
so the marshalling argument does not transfer.

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
| `vk-registry` | lib | **Exists.** Typed model, the 375-line C declarator parser, `depends=` resolution and profiles (§7.1). The only thing that reads `vk.xml` — and, since §0.9, the `video.xml` beside it. |
| `sv-codegen` | bin + lib | **Exists.** The generator: ten emitted passes (`arena_bind` has nothing to do, §7.2), exception table, `quote`/`prettyplease` emission, protocol-hash computation |
| `sv-proto` | lib, **generated**, `forbid(unsafe_code)` | **Exists.** Types, encode, decode, pnext, handle_map, transform, subdecode — 126,205 lines for the 239-command Zink profile, 140,238 for the 252-command video profile (`SV_PROTO_PROFILE=video`, §0.9) |
| `sv-icd-entry` | lib, **generated** | `extern "system"` stubs and proc-address tables. The only generated crate that permits `unsafe`, in one mechanical shape |
| `sv-wire` | lib | **Exists.** Bounds-checked cursor, packet headers, primitive encodings. Hand-written, Miri-tested |
| `sv-transport` | lib | Region mapping, handshake, rings, reply rings, reverse ring, `Parker`/`Doorbell` traits and backends, cold-path socket |
| `sv-arena` | lib | Slabs, the 4096-granularity allocator, deferred free, `ArenaProvider`/`ArenaAttacher` |
| `sv-handles` | lib | App-side id allocation; GPU-side `SlotMap` object table with deferred destroy |
| `sv-icd` | cdylib | The app side: instance/device/queue objects, memory, WSI, the hand-written half of the ICD |
| `sv-gpu-server` | bin | The GPU side: ring consumers, `SlabImport`, semantic glue, submit path, validation forwarding |
| ~~`sv-present`~~ | — | **Not built, and should not be.** It was to be "swapchain plumbing shared by both sides", and there is nothing to share: §0.6 found that presentation is expressible entirely in the forwarded API, so the GPU side has no half. What exists is `sv_icd::wsi` (the swapchain, out of ordinary Vulkan) and `sv_icd::present` (the blit, and the only module that knows what a window is), both inside `sv-icd`. |
| `sv-hostd` | bin | App-side daemon: opens QEMU's memfd, GPA→offset, builds the contiguous view, publishes the object's **name** to ICD processes (§1a — it hands out no descriptor) |
| `vk-probe` | bin | **Exists.** The GPU-side capability probe of `measured-gpu-side.md` |
| `awe-probe` | bin, Windows | **Exists.** The guest half of step 0: commit, lock, stamp, import, verify. Cross-compiled to `x86_64-pc-windows-gnu` |
| `arena-host` | bin | **Exists.** The host half of step 0: mtree region table, scan/translate, run coalescing, mmap stitching. **This is the seed of `sv-hostd`**, not a throwaway probe |
| `arena-record` | lib | **Exists.** The stamped page record, defined **once** so a match cannot be an artefact of the two sides disagreeing about a field offset. Shared by the guest and host halves |
| `sv-testkit` | lib, test-only | **Exists.** Round-trip harness, the Venus differential comparison (structural, 208 shared structs — §6.2), fake transports, fixtures |

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

**0. Retire the gating risk. — DONE, 2026-08-11. It passed.**

> **[SUPERSEDED — the step as planned]**
> *Extend `vk-probe` to allocate an AWE region in the guest, call
> `vkGetMemoryHostPointerPropertiesEXT` on it and on an ordinary `VirtualAlloc`
> pointer, and attempt `vkAllocateMemory` with `VkImportMemoryHostPointerInfoEXT`
> on both. Separately, dump the PFN array and check it against `info mtree -f`
> from the host. Test: the import succeeds and the frames resolve to plausible
> memfd offsets. Nothing below is worth building until this passes. It is an
> afternoon.*

What was actually run is in `measured-gpu-side.md`, "Step 0", and it was a
better experiment than the one planned, for an unwelcome reason: AWE was
unavailable, so the host had to **find** the pages by content instead of being
told where they were, which tests the translation far harder than trusting a PFN
array would have. Three tools — `awe-probe` (guest), `arena-host` (host, as
root) and `arena-record` (the shared page layout) — rendezvous over the existing
virtio-fs share. The result: 64 MiB located, written from the host, imported by
the Intel driver, all 16384 pages carrying the host's bytes through the driver's
own mapping, and the reverse direction confirmed in QEMU's guest-physical
memory. `memoryTypeBits = 0x6`, the same as ordinary memory.

Three things it did **not** do, each of which moved elsewhere in this plan: AWE
was never exercised (privilege — done in step 0a), large pages had not been
thought of yet (§5.1, measured in step 0a), and **no GPU touched the pages**,
which is still true and is step 7's first job. The harness is kept runnable,
because step 7 crosses this same boundary for real.

**0a. The privilege, and the granularity measurements. — DONE, 2026-08-11.**
`SeLockMemoryPrivilege` was granted to one account with `LsaAddAccountRights`
(R25; reversible in one command, no reboot, no code-integrity setting touched),
and all three questions were answered: `VirtualAlloc(MEM_LARGE_PAGES)` gives 22
runs per 64 MiB and 1 per GiB; the driver imports large-page, AWE and
`NONPAGED_LARGE` windows identically, at up to 2 GiB in one `VkDeviceMemory`; and
`AllocateUserPhysicalPages2` with `NONPAGED_LARGE` does not deliver large pages
on build 26200. `QueryWorkingSetEx` was measured on the way and reports no frame
numbers at all. §5.1's open decision is settled for large pages, §5.7's pending
numbers are numbers, and R19 and R20 are retired
(`measured-gpu-side.md`, §0.1).

**1. `sv-wire` + `sv-transport` over a memfd, two Linux processes, no Vulkan.**
Handshake, magic-written-last, generation counter, ring ping-pong, reply ring,
reverse ring, heartbeat timeout, peer-restart detection. *Test: a
throughput/latency benchmark and a chaos test that kills each side at random
points; Miri over the ring with many seeds.*

**2. `sv-arena`.** Allocator, slabs, deferred free. *Test: Miri over a
`Vec<u8>`-backed arena; a property test that allocations never overlap and that
a freed range is not reissued before its timeline value passes.*

**3. The type model and the generator. — DONE, 2026-08-11**, and it went past
where it was aimed.

> **[SUPERSEDED — the step as planned]**
> ***3. The type model and the generator skeleton**, emitting a deliberately tiny
> command set: `vkCreateInstance`, `vkEnumeratePhysicalDevices`,
> `vkGetPhysicalDeviceProperties`, `vkCreateDevice`, `vkAllocateMemory`,
> `vkMapMemory`, `vkCreateBuffer`, `vkBindBufferMemory`.*

The tiny command set was skipped: the profile machinery that selects commands is
the same code whether it selects eight or 239, so the generator emits the whole
**239-command Zink profile** — 126,205 lines from 10,145 hand-written, 657 tests,
Miri clean. Ten passes are emitted (`arena_bind` has nothing to do — until pass 12, where it does, §0.4). Along
the way it corrected §6.1, §6.2, §7.2 and §7.4, found the `depends=` chain that
had been silently cutting the profile to 55 commands (§7.1, §11), and caught a
sign-inversion bug that every symmetric test agreed with (§7.3).

*Test, as run:* generated round-trip property tests, the structural differential
against Venus's generated C over 208 shared structs, and assertions of generated
enum values against published `vulkan_core.h` numbers. **Outstanding:** the model
has *not* yet been compared against the `vulkan_object.py` fixture. §7.1 asks for
that as an independent check on the part of the project most likely to be quietly
wrong, and §7.3's sign bug is the argument for doing it rather than against.

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

**6. Widen to Zink's requirement set. — DONE for GL 3.2, 2026-08-12.**

> **[SUPERSEDED — the step as planned]**
> ***6. Widen to Zink's requirement set**, and run the `zink-lavapipe` deqp suite
> through our ICD. *Test: deqp pass rate, compared against `zink-venus-lavapipe`
> as the reference configuration.* This is the largest single step and it is
> mostly generator coverage plus GPU-side semantic glue.*

It was mostly generator coverage, and the estimate of *which* generator work was
wrong in an informative way: the widening was not "add commands", it was **add
the pass §7.2 does not have**. 223 of 239 commands are forwarded by pass 12 and
sixteen are hand-written, against about forty hand-written before it. §0.4 has
the eleven corrections that came out of it.

*Test, as run:* `scripts/sv-zink-gl.sh` — Zink starting on the surfaceless EGL
platform and reporting a renderer string that names the GPU side's device, then
`scripts/gl-probe.c` rendering a depth-tested, textured, lit mesh into an FBO and
reading it back, compared against the identical program on llvmpipe at 64.1 dB
PSNR. **Outstanding:** deqp has not been run, and the context is GL **3.2** where
llvmpipe gives 4.6 — the missing capability is not yet identified and is the
first thing to find. `scripts/sv-end-to-end.sh` and `scripts/sv-memfd-path.sh`
still pass, the second on lavapipe with no GPU at all.

**7. Cross the boundary.** Windows build of `sv-gpu-server`, the large-page
arena with its 4 KiB fallback, `sv-hostd` (locate, coalesce, stitch), cold path
over virtio-serial. *Test: step 4's byte-pattern test, across the VM boundary,
then step 6's deqp suite across it.* Step 0 has already demonstrated the memory
half of this in isolation, so what step 7 adds is the protocol on top of it.

**Run the compute workload first, before anything else in this step.** R15 is
now the largest unmeasured thing in the project (§0): nobody has validated
Intel's Windows driver running headless in a background process with imported
memory only, and — more sharply — **no GPU has yet touched an arena page at
all**. Step 0 proved the driver accepts and maps our memory; it proved nothing
about the GPU reading it. A trivial compute shader over an arena buffer,
producing a result the host can check in the memfd, is an hour and it gates the
rest of the step.

**8. Presentation. — DONE, 2026-08-12**, and it did not wait for step 7.

> **[SUPERSEDED — the step as planned]**
> ***8. Presentation**, after the §10 measurement decides whether it is needed.*

The measurement this step was waiting on — "on this machine the physical panel
is driven by the passed-through iGPU … it may well be that the right answer is
to leave it there" (§10) — was overtaken. Presentation stopped being a question
about the panel the moment §0.5 established that **DXVK requires
`VK_KHR_swapchain` for a device that will never present**, and that
`zink_kopper` needs one for any windowed GL at all. Two of the three things the
project could not do were one piece of work, and it was this one.

*Test, as run:* `scripts/sv-present.sh` — a headless `VK_KHR_swapchain`, three
frames of a known pattern presented and compared **byte for byte**, clean under
`VK_LAYER_KHRONOS_validation`; `scripts/sv-zink-gl.sh`'s fourth check —
`glxgears` in an X window under Xvfb, agreeing with llvmpipe on every histogram
statistic; and 1.47 ms per 1080p present, measured. §0.6 has the seven findings,
of which the first is that this step needed no protocol work at all and the fifth
is a 4 MiB ceiling on host-visible allocations that had nothing to do with
presentation. **Outstanding:** the present is synchronous and there is no triple
buffering (§9.3's timeline mirror is the blocker, as it is for `vkQueueSubmit`),
and there is no Wayland.

**9. DXVK ≤ 2.6, as a stretch. — DONE, 2026-08-12**, and it did not need step 7
either: `scripts/sv-dxvk-d3d11.sh` runs a windowless Direct3D 11 program under
Wine 11.0 on DXVK 2.6.2 against this ICD, and it renders and reads back a frame
that matches what the same program gets from RADV unforwarded. The step said
"only once 6 and 7 hold"; 6 holds and 7 does not, and the dependency on 7 turns
out to have been about the *target machine* rather than about DXVK.

Steps 1, 2 and 3 are independent of each other and can proceed in parallel in
separate worktrees; 4 needs all three.

---

## 14. What remains unmeasured

Listed plainly, largest first. Rewritten 2026-08-11 after step 0 and again after
step 0a and the codegen stage; settled items are kept, marked, and moved to the
bottom, because what a list like this stops saying is as informative as what it
says. **Ten entries remain; five were measured away in the second round** and are
kept at the bottom. The list is shorter and sharper for it — but the first entry
has not moved at all, and it is the one that matters.

1. **Whether a GPU can DMA from these pages.** Unchanged, unmeasured, and now by
   some distance the largest gap in the project. Everything measured so far is
   CPU-side: the driver *accepts* an import of host-shared guest RAM, at four
   allocators and up to 2 GiB, and its mapping is demonstrably the same physical
   memory. **No GPU has executed a workload against arena memory.** An import is
   not a submission: the iGPU's own translation, its TLB and the WDDM residency
   path are all downstream of `vkAllocateMemory` returning, and none of them has
   been exercised. A trivial compute shader over an arena buffer, producing a
   result the host can check in the memfd, settles it (§13 step 7, R15). Until it
   runs, everything below the arena is built on a mechanism proven only as far as
   the driver's front door.
2. **Whether the Intel Windows driver tolerates headless service operation**
   with imported memory only and no swapchain. Not a configuration its vendor
   tests. R15, and it travels with item 1.
3. **Coherency under concurrent access.** Measured at rest only — one writer at a
   time, with a file rendezvous between phases — now across four allocators and
   at 1 GiB as well as 64 MiB, which broadens the evidence without changing its
   kind (§5.6). Nothing has been measured about two writers, about cache
   behaviour under load, or about a GPU and a CPU touching the same page. R10 is
   retired *by construction*, not by experiment, and that distinction should
   survive the next reader.
4. **The remaining numbers on the attach path.** Runs per window are measured at
   every size that matters; **attach time at full arena size is not**, on either
   path, and neither is probe-locate time, since `ProbeLocator` is not
   implemented (§5.1). The 4 KiB attach budget in §5.7 is still an extrapolation
   from a 60 ms, 64 MiB write pass — a hypothesis about linearity, of the same
   kind the locate-time extrapolation turned out to survive.
5. **Whether large-page allocation succeeds on a guest that has been up for
   weeks.** *Partly answered 2026-08-13 and not in the direction hoped: at ~2
   days of uptime allocation still **succeeds** at 64 MiB and at 1 GiB, but the
   contiguity it returns has collapsed — 1 GiB is 399 runs against the 1 run
   measured at 1.3 hours (§0.14, §5.7). So the risk is not that the allocator
   fails; it is that the number §5.7's best case rests on is a cold-boot number.*
   13 attempts at 64 MiB, 256 MiB and 1 GiB all succeeded, slowest
   90 ms, on a guest with 8973 MiB free and 1.3 hours of uptime — the favourable
   case, deliberately, because manufacturing memory pressure on somebody's daily
   machine was out of scope. The failure mode (`ERROR_NO_SYSTEM_RESOURCES`)
   becomes likelier with uptime and fragmentation, and it is why the 4 KiB
   fallback exists and why degradation is per extent (§5.1).
6. **Whether a locked 4 KiB page can be relocated under memory pressure.** A
   64 MiB window re-located 3m14s later gave a byte-identical offset table, which
   is evidence against *casual* relocation and no evidence at all about pressure,
   which was not provoked. R26, mitigated rather than open: large pages remove
   the question by construction, and the 4 KiB fallback carries a canary that
   turns a silent divergence into a loud one (§5.1, §5.2). The residual is that
   the canary has not been seen to fire, because nothing has yet made it.
   *2026-08-13: the question is unchanged and the instrument is not. Re-reading
   the identity of frames already held costs 1 ms for 16384 of them and 22 ms for
   262144 (§0.14), so this can be asked continuously rather than twice — and a
   second data point taken that way, a 64 MiB locked window held 300 s on a
   machine in use, again came back byte-identical. Pressure is still not
   provoked.*
7. **Whether balloon inflation can take arena pages.** `info balloon` reports
   `actual=13312`, so nothing is inflated today, and a locked user allocation
   should not be reachable by the balloon driver's non-paged pool — but that is
   reasoning, not a test, and R24 already says the arena's RAM belongs to
   somebody else (`measured-gpu-side.md`).
8. **Ring throughput and latency in this topology.** Every number in the
   research is either arithmetic or from another machine, and step 1 has not
   been run.
9. **Whether the app side should present at all**, or whether the physical panel
   should stay on the passed-through iGPU (§10, fwd §6.6).
10. **Whether our model agrees with Khronos's `vulkan_object.py`.** §7.1 asks for
    that fixture as an independent check on the type model and it has not been
    built. The differential against Venus covers the wire format; nothing yet
    covers the model itself, and §7.3's sign inversion is what an unchecked model
    looks like from the inside.

**Settled by step 0**, kept for the record:

> **[MEASURED — `measured-gpu-side.md` "Step 0"]**
> *1. Whether importing guest RAM shared with the host works. This is the big one
> and it is the whole design. … It does not establish that an import of our
> particular pages will succeed. Ordinary process memory and AWE-backed guest RAM
> that a host process also has mapped are not obviously the same thing to a
> driver that pins pages.*
>
> They are the same thing to this driver: `memoryTypeBits = 0x6` for both,
> `vkAllocateMemory` succeeds at 1 MiB and 64 MiB, and the bytes are shared in
> both directions. R20 is retired for everything except AWE specifically.

> **[MEASURED — `measured-gpu-side.md` "R21"]**
> *4. The GPA→memfd offset formula on this machine. i440fx computes the below-4G
> split with a machine-version compat knob; it is not a constant to hardcode, and
> `info mtree -f` must be parsed. R21.*
>
> Read off the running machine: the split is 3 GiB, not 3.5 GiB, and two ranges
> below 4 GiB are not RAM at their identity offsets. The instruction stands and
> is now stronger — §5.1 requires the translation to *walk* the flat view rather
> than evaluate anything, because the wrong answer is silent. What remains
> unmeasured is only whether we re-read it when the machine changes, which is a
> discipline question, not an experiment.

**Settled by step 0a**, kept for the record. All five are in
`measured-gpu-side.md`, "Large pages, AWE, and the privilege that unlocks both"
and "Can we have both? `QueryWorkingSetEx`, measured":

> **[MEASURED — the driver accepts large pages, and everything else]**
> *2. Whether the Intel Windows driver accepts a large-page-backed import. §5.1
> makes `MEM_LARGE_PAGES` the intended path on the strength of arithmetic and a
> privilege grant, not a measurement. A pinning import path may care about the
> PTE granularity underneath the pointer.*
>
> It does not care. Large-page, AWE and `NONPAGED_LARGE` windows all import with
> `memoryTypeBits = 0x6` and `mapped_same_address = true`, at 64 MiB, 1 GiB and
> 2 GiB. This also completes R20's last leg: **R20 is retired in full**, not
> "except AWE specifically".

> **[MEASURED — `NONPAGED_LARGE` does not deliver]**
> *5. Whether `AllocateUserPhysicalPages2` accepts
> `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`. Promoted from "ten minutes, worth
> spending" to a decision point: it would give **both** frame numbers and 2 MiB
> contiguity, which together delete the locate step entirely and make AWE the
> primary path (§5.1).*
>
> The call succeeds and buys nothing: 3992 frames of 16384 requested, ~200 ms,
> and **one page per run**. No large pages on build 26200. The decision point it
> was going to create does not exist, and the design's repeated nomination of it
> as "the fix for fragmentation" is corrected in §5.1 and §5.7. Note also the
> resolution trap: it lives in `kernelbase.dll`, and asking only `kernel32.dll`
> produces a wrong negative.

> **[MEASURED — AWE returns real guest-physical frames]**
> *6. Whether `AllocateUserPhysicalPages` still returns raw guest PFNs on
> Windows 11 24H2. The Windows Research Kernel source settles it for the NT 5.2
> era and there is no reason to think it changed, but "no reason to think" is not
> a measurement.*
>
> Measured on build 26200: all **16384** frames put through the R21 translation
> landed on exactly the page the guest said they were. `PfnLocator` works; it is
> simply not the path we take, because AWE's contiguity is the worst of the four
> allocators.

> **[MEASURED — R19 is retired]**
> *7. Whether AWE's "cannot be used to buffer data for graphics or video calls"
> is enforced or vestigial. R19, untouched by step 0 because AWE was never
> reached.*
>
> Vestigial, at least for this path: the driver imported an AWE window without
> complaint. Microsoft's sentence does not bind WDDM's user-pointer import path
> on this driver. **The attestation-signed pinning driver is dropped** (§5.1, D2).

> **[MEASURED — the locate cost does not scale with the arena]**
> *…the only measured attach figures are the 4 KiB ones, and even those are
> extrapolated from 64 MiB to 2 GiB by multiplication, which is a hypothesis
> about linearity.*
>
> For locating, the hypothesis was wrong in the *favourable* direction: 1.8 s at
> 64 MiB, 2.02 s at 1 GiB, 2.32 s at 2 GiB, because the sweep is linear in guest
> RAM rather than in arena size. A 32× larger arena cost 0.5 s more. The
> stitching side of the extrapolation is still open — item 4 above.

---

## 15. Risks

R1–R8 are in `security-model.md`. R9–R26 have been added to that file as part of
this design: the arena import risk, cache aliasing, the fixed arena window,
`bufferDeviceAddress` scope, version skew, the reverse channel, headless driver
operation, the loader trampoline ceiling, being the first Rust ICD, GPU hangs
being visible to a person, and the new ones this design introduces — AWE's
graphics restriction, the unmeasured guest-RAM import, GPA translation fragility,
cross-ring ordering, memory-table honesty, the guest RAM the arena takes away
from a production Windows session, the privilege grant, and frame stability.

That file is the maintained list. This one is the design.

> **[SUPERSEDED — 2026-08-11, the amendments have been made]**
>
> ***Three entries there now need amending, and this document is not the place to
> do it.** Recorded here so the discrepancy is deliberate rather than rot:*
>
> *- **R11 and R24** — "grow by adding slabs" and "allocate slabs lazily so the
> resident set tracks actual use" are superseded on the 4 KiB path …*
> *- **R19/R20** — both are now decisions between available paths rather than
> gates on the project, and the reason `SeLockMemoryPrivilege` is being granted
> has changed from "to test AWE" to "to get large pages" …*
> *- **A new risk is missing:** `VirtualLock` promises residency, not frame
> stability (§14 item 8). It is the same silent-wrong-answer shape as R21 and it
> has no cheap detector.*

**All three have been made, and two of them went further than this section
expected.** As `security-model.md` now stands:

- **R11 and R24 are amended.** "Grow by adding slabs" survives with a warning
  that growing late is worse than it looks; "allocate slabs lazily so the
  resident set tracks actual use" is superseded outright, on *both* paths, since
  neither commits memory lazily. What limits guest cost is the configured arena
  size and releasing the extent on last disconnect. §5.7 carries the same
  conclusion for growth.
- **R19 and R20 are retired, not amended.** Both were measured, not decided
  between: the driver imports an AWE range, and `AllocateUserPhysicalPages`
  returns real guest-physical frames. The privilege's justification did change as
  predicted — from "to test AWE" to "to get large pages" — and it is recorded as
  **R25**, which is a real widening of the guest's security policy and is
  reversible in one command.
- **The missing risk is now R26**, and it has mitigations rather than a shrug:
  large pages by construction, a canary per slab on the 4 KiB fallback, and a
  weak measurement (byte-identical offsets after 3m14s) that is explicitly not
  evidence about memory pressure.

**Two entries there still need work, and again this is not the place to do it.**

- **R8's mitigation is now inaccurate.** It reads "Stronger: compare
  byte-for-byte against Venus's MIT-licensed generated C on the shared subset",
  which §6.2 has just retracted: the comparison is structural — member sequence,
  kind and width over 208 shared structs — because our own D6 and the `u32`
  command header changed the bytes. The mitigation is *stronger* than the risk
  entry claims, not weaker, and the entry should say what was built. R8 has also
  been *demonstrated* since it was written (§7.3's sign inversion, which every
  symmetric test agreed with) and partly *mitigated* by an unplanned decision
  (every variable-length thing carries its own count, so `len=` is a verifier
  rather than an input — §7.4). All three belong in that entry.
- **R9 and R15 have not moved, deliberately.** R9 still points at §13 step 0 as
  the thing that would settle it, and R15's mitigation — "run a trivial compute
  workload through the GPU-side server" — is still unrun. That is §14's item 1,
  it is the largest gap in the project, and it should stay conspicuous in both
  documents until a GPU has actually read an arena page.
