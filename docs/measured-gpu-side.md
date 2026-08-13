# What the GPU side actually reports

Measured 2026-08-11 by running `vk-probe` inside the Windows guest on
testhost, against the passed-through Intel iGPU. Not documentation, not
inference — the driver's own answers.

## The result the design hung on

```
=== device 0: Intel(R) UHD Graphics
    type=INTEGRATED_GPU api=1.4.323 driver=0x195ba4
    VK_EXT_external_memory_host: YES
    minImportedHostPointerAlignment: 4096 bytes
```

**`VK_EXT_external_memory_host` is present.** The GPU side can take a plain
pointer to memory we allocated and import it as `VkDeviceMemory`, which is
precisely what the passthrough arena requires. The zero-copy design is viable
on the real target hardware rather than merely in principle.

`minImportedHostPointerAlignment` is **4096 bytes** — exactly a page. That is
the best case: the arena lives in page-granular shared memory anyway, so the
allocator's granularity is dictated by something we were going to align to
regardless. Nothing has to be over-allocated to satisfy the driver.

## The memory table, which is unusually friendly

```
heaps:
  [0]   6624 MiB  DEVICE_LOCAL
memory types:
  [0] heap=0  DEVICE_LOCAL
  [1] heap=0  DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT
  [2] heap=0  DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT | HOST_CACHED
```

One heap, and it is an integrated GPU, so memory is unified: every type except
[0] is simultaneously `DEVICE_LOCAL` and `HOST_VISIBLE`. There is no discrete
VRAM that an application can allocate but never map, and therefore no class of
allocation that is structurally unable to live in the arena. The awkward case
this design would otherwise have to handle — "device-local memory the app
wants mapped" — does not arise here.

`HOST_COHERENT` on types [1] and [2] also means the driver is not asking us to
manage flushes for those types on this hardware. That does not remove the need
to implement `vkFlushMappedMemoryRanges`/`vkInvalidateMappedMemoryRanges`
honestly, because a different GPU side (a discrete card, a different vendor)
may well advertise non-coherent types — but it does mean the first
implementation can be correct without being clever.

## Other extensions, asked in the same run

```
VK_KHR_external_memory_win32           yes
VK_KHR_external_memory_fd              no
VK_EXT_external_memory_dma_buf         no
VK_KHR_buffer_device_address           yes
VK_KHR_timeline_semaphore              yes
(148 device extensions total)
```

- `external_memory_win32` present, `fd`/`dma_buf` absent, exactly as expected
  for a Windows GPU side. The import path must therefore be pluggable rather
  than assuming any one handle type — which is independently required, since
  RADV refuses shared host mappings altogether.
- `timeline_semaphore` present, which the synchronisation design wants.
- `buffer_device_address` present. Worth care rather than celebration: the
  driver supporting it is not the problem. The problem the forwarding research
  identified is that DXVK ≥ 2.7 and vkd3d-proton emit shaders that dereference
  raw device addresses, which a proxy cannot intercept — a structural obstacle
  to D3D12 over a forwarded implementation, not a missing feature. This
  measurement confirms the driver would not be the limiting factor if that were
  ever solved.

## The import, actually performed

Advertising an extension is not the same as accepting an import, so the probe
was extended to try it. Same machine, same run:

```
=== host-pointer import test
    getMemoryHostPointerProperties: SUCCESS
    memoryTypeBits: 0x6
    importing into memory type 1
    vkAllocateMemory(import): OK
    mapped ok: first byte = 0xab (wrote 0xAB), same address = true
    wrote 0x5C via Vulkan mapping, read 0x5c via the original pointer
      -> SHARED, zero-copy confirmed
```

Read that last pair of lines carefully, because it is the whole design in four
values. A megabyte of ordinary page-aligned memory was allocated and filled
with `0xAB`. The driver imported that pointer as `VkDeviceMemory` and
`vkMapMemory` returned **the same virtual address** — not a copy, the same
pages. Writing `0x5C` through the Vulkan mapping was immediately visible
through the original pointer.

So on this hardware the GPU can be pointed at memory the application allocated,
with no marshalling and no copy. `memoryTypeBits == 0x6` says the driver
permits types 1 and 2, which are precisely the two host-visible types, so an
arena allocation has somewhere legal to go.

This retires R9 (the driver refusing host pointers) for this GPU side. It does
not retire R20.

## What this does not tell us

That the extension is advertised is not that an import of *our particular
pages* will succeed. Importing memory that is guest RAM shared with the host,
rather than ordinary process memory, is the actual experiment, and it has not
been run. What is now established is that the mechanism exists, is
page-granular, and does not need a fallback invented before the first line of
the arena is written.

---

# Step 0: the shared-page import, measured

Run 2026-08-11 on `testhost`, against the same Intel iGPU in the same Windows
guest, with the Linux host in the loop. The section above ends by saying the
actual experiment had not been run. It has now.

The tools are `crates/awe-probe` (guest side, cross-compiled to
`x86_64-pc-windows-gnu`), `crates/arena-host` (host side, run as root because
the memfd is only reachable through QEMU's `/proc` entry) and
`crates/arena-record`, which defines the page record **once** so that a match
cannot be an artefact of the two sides disagreeing about a field offset. They
rendezvous through files on the existing virtio-fs share.

## The verdict, first

**R20 is retired for the case that could be tested, and the arena mechanism
works end to end.** A window of guest RAM was written by the guest, found and
rewritten by a Linux process on the other side of the hypervisor, imported by
the Intel driver as `VkDeviceMemory`, and observed through the driver's own
mapping to contain the host's bytes — with the traffic then run in the reverse
direction and verified in QEMU's memory a third way.

**One leg of R20 could not be tested: AWE.** `SeLockMemoryPrivilege` is not
held by the account and is unassigned in the guest's local security policy, so
`AllocateUserPhysicalPages` fails outright. Granting it is a security-policy
change, which was out of scope for this run. See "What AWE would take" below.

## The machine, as configured

    -machine pc,accel=kvm,smm=off          i440fx, not q35
    -m 13312                               13312 MiB
    -object memory-backend-memfd,id=virtiofsmem,size=13312M,share=on
    -numa node,memdev=virtiofsmem

QEMU 10.2.2, pid 205866, and the backing object is **fd 34**, a
`/memfd:memory-backend-memfd (deleted)` of 13958643712 bytes.

## R21: the GPA → memfd offset mapping, read rather than assumed

From `info mtree -f` over QMP, the RAM aliases in the guest's flat view are:

```
0000000000000000-00000000000bffff  virtiofsmem @0
0000000000100000-0000000000102fff  virtiofsmem @0000000000100000
000000000010f000-00000000bfffffff  virtiofsmem @000000000010f000
0000000100000000-000000037fffffff  virtiofsmem @00000000c0000000
```

So the translation on this machine is

    gpa < 0xc000_0000            ->  offset = gpa
    gpa >= 0x1_0000_0000         ->  offset = gpa - 0x4000_0000
    0xc000_0000 .. 0xffff_ffff   ->  PCI hole, no RAM

**The below-4G split is 3 GiB (`0xc0000000`), not the 3.5 GiB (`0xe0000000`) an
i440fx machine uses at smaller RAM sizes** — QEMU drops the split to 3 GiB once
`ram_size` crosses 3.5 GiB on gigabyte-aligned machine types. Anyone hardcoding
either constant has a 512 MiB error waiting. Note also the two carve-outs below
4 GiB that are *not* RAM at their identity offsets: the `0xc0000-0xfffff` ROM
window and `0x103000-0x10efff`, which the twelve Hyper-V SynIC message pages
overlay. A translation that walks the flat view survives them; an arithmetic
one does not.

The formula was then checked against QEMU's own guest-physical reader rather
than against itself. With the window live, page 0 of the 64 MiB run sat at
memfd offset `0x276a6d000`, which the formula calls GPA `0x2b6a6d000`:

```
(qemu) xp /16xb 0x2b6a6d000
00000002b6a6d000: 0x53 0x56 0x41 0x52 0x45 0x4e 0x41 0x31   "SVARENA1"
00000002b6a6d008: 0xbd 0x5c 0x7f 0x07 0x69 0x1e 0x70 0x75   run 0x75701e69077f5cbd

(qemu) xp /16xb 0x276a6d000          # the identity-mapping mistake
0000000276a6d000: 0x00 0x00 0x00 0x00 0x00 0x00 0x00 0x00
```

That is R21 demonstrated in both directions: the right formula finds our page,
and the wrong one reads unrelated memory that happens to be zero — silence, not
an error, exactly as the risk predicts.

## AWE: refused, and not worked around

```
SeLockMemoryPrivilege: not-held
AllocateUserPhysicalPages failed: 1314 (ERROR_PRIVILEGE_NOT_HELD)
```

`whoami /priv` shows an elevated, high-integrity token with 24 privileges —
including `SeDebugPrivilege` and `SeLoadDriverPrivilege` — but not
`SeLockMemoryPrivilege`. `secedit /export /areas USER_RIGHTS` confirms why:
`[Privilege Rights]` has no `SeLockMemoryPrivilege` line at all, so the right is
assigned to nobody, which is the Windows default.

**What AWE would take**, established so the decision can be made deliberately:
add the account (or a group) to `SeLockMemoryPrivilege` in the local security
policy — `secedit /configure` with that one line, `LsaAddAccountRights`, or
gpedit's "Lock pages in memory" — and then obtain a *new* token, because
privileges are baked into a token at logon. For the ssh session that means
reconnecting; **the interactive user does not have to log off and the machine
does not have to reboot.** It touches no code-integrity setting: not Secure
Boot, not HVCI, not test-signing, so the hard constraint in
`hardware-constraints.md` is untouched. It is nonetheless a change to the
guest's security policy and was not made.

## The fallback path, which still answers most of the question

Without AWE the guest used `VirtualAlloc(MEM_COMMIT)` + `VirtualLock`, which
needs no privilege. That gives up (b) — frames we chose, whose numbers we know
— but keeps (a) and (c): the pages are guest RAM, they are pinned, a Linux
process maps them across the hypervisor, and they are what the driver is asked
to import. The host therefore had to *find* the pages instead of being told
where they were, which is a stronger test of the translation than trusting a
PFN array would have been: `arena-host scan` sweeps all 13312 MiB of the memfd
for the run's magic, and every page it reports is a page it located by content.

## What happened, in order

Three runs — 1 MiB (256 pages), 64 MiB (16384 pages, the design's slab size) and
a 256 KiB confirmation run against the final binary. All three behaved
identically; the 64 MiB numbers are quoted.

```
guest : VirtualAlloc+VirtualLock 64 MiB at 0x1a3e4630000, stamp 16384 pages
host  : scan 13312 MiB -> located 16384 pages, 1.8 s
host  : write1 -> 16384 pages via mmap(MAP_SHARED) + msync, 60 ms
guest : host->guest through the plain window: 16384/16384, 0 wrong indices
guest : getMemoryHostPointerProperties = SUCCESS, memoryTypeBits = 0x6
guest : vkAllocateMemory(import) = ok            <- THE GATE
guest : vkMapMemory ok, mapped_same_address = true
guest : guest records via the Vulkan mapping : 16384/16384
guest : HOST records via the Vulkan mapping  : 16384/16384   <- the answer
guest : wrote VKWROTE1 through the Vulkan mapping, 16384 pages
host  : verify -> VULKAN-written record present in 16384/16384 pages, 0 wrong
qemu  : xp at the page's GPA shows "HOSTWRT1" at +64 and "VKWROTE1" at +96
host  : write2 (after the import) -> 16384 pages
guest : second host write, after the import: 16384/16384 pages match
```

Read the two marked lines together. The driver accepted an import of memory
that a process on the other side of the hypervisor had written into moments
earlier, and every one of the 16384 pages the driver mapped carried the host's
bytes. Then the reverse: bytes written through the `VkDeviceMemory` mapping
appeared in QEMU's memfd, at the offsets the translation predicts, and QEMU's
own `xp` read them back out of guest-physical memory. Nothing in that loop can
be explained by a copy.

`memoryTypeBits = 0x6` and `memory_type_index = 1` — the same answer the
ordinary-memory probe got. The driver does not distinguish these pages from any
other host pointer, which is the friendliest possible result.

## Fragmentation, which the design should know about

The pages of a locked Windows allocation are scattered in guest-physical space:

| window | pages | contiguous runs in the memfd | mean run |
|--------|-------|------------------------------|----------|
| 1 MiB  |   256 |                           29 |  ~35 KiB |
| 64 MiB | 16384 |                         1471 |  ~44 KiB |

So an `ArenaAttacher` stitching a contiguous host-side view of a 64 MiB slab
needs roughly 1500 `mmap(MAP_FIXED)` calls, not one. That is affordable here —
`vm.max_map_count` is **1048576** on this host, so even a multi-gigabyte arena
stays two orders of magnitude inside the limit — but it is a per-slab cost at
attach time and a reason to keep slabs large. AWE would not fix it by itself:
`AllocateUserPhysicalPages` chooses frames but promises nothing about
contiguity. `AllocateUserPhysicalPages2` with `MEM_EXTENDED_PARAMETER_NONPAGED_LARGE`
would, at 2 MiB granularity — §14's item 8 is now worth more than the ten
minutes it was budgeted, and it needs the same privilege AWE does.

## What this does not prove

Being precise, because a false positive here is the worst outcome available:

1. **AWE was not tested.** R19 — whether WDDM's user-pointer import path accepts
   an AWE range — is untouched, and so is the question of whether
   `AllocateUserPhysicalPages` still returns raw guest PFNs on Windows 11 24H2.
   Both need the privilege.
2. **No GPU touched these pages.** What is proven is that the driver *accepted*
   the import and that its mapping is the same physical memory. Whether the GPU
   can DMA from a host-shared page — TLB, page tables, and the iGPU's own
   translation are all downstream of the import succeeding — is R15's trivial
   compute workload, and it has not been run.
3. **`mapped_same_address = true` means the Vulkan mapping is the original
   pointer**, so "read through the Vulkan mapping" and "read through our own
   pointer" are the same instruction. The claim rests on the import succeeding
   and on the host-side round trip, not on the read being through a distinct
   mapping.
4. **Coherency was measured at rest**, one writer at a time with a file-based
   rendezvous between phases. Nothing here says anything about concurrent
   access or about cache attributes under load (R10).
5. The guest has a `virtio-balloon`; `info balloon` reports `actual=13312`, so
   nothing is inflated today. A future inflation is a way for arena pages to be
   taken away that the design does not currently consider.

## Re-running it

Step 7 crosses this boundary for real, so the harness is worth keeping usable.
The two sides rendezvous through files on the existing virtio-fs share
(`/srv/winshare/probe` on the host, `Z:\probe` in the guest); the guest half
polls, so the host half can be driven by hand between phases.

```
# build (from the dev shell)
cargo build -p awe-probe  --release --target x86_64-pc-windows-gnu
cargo build -p arena-host --release
# publish the guest half where the guest can see it; copy the host half over
install -m0755 target/x86_64-pc-windows-gnu/release/awe-probe.exe /srv/winshare/probe/

# guest (ssh, console only, no window)
awe-probe.exe --workdir "Z:\probe" --pages 16384 --mode large
#   --mode large|awe|awe2|locked  which allocator (default: awe, then locked)
#   --stress N [--stress-only]    repeat the allocation N times and report

# host, as root; QEMU's memfd is the fd whose /proc link is /memfd:memory-backend-memfd
arena-host scan   --fd /proc/<qemu>/fd/<n> --run <run_id> --out map.txt
arena-host write1 --fd ... --map map.txt --stamp <hex>   ; echo stamp=<hex>  > .../host-wrote.txt
arena-host verify --fd ... --map map.txt --run <run_id> --stamp <hex> --vkstamp <hex>
arena-host write2 --fd ... --map map.txt --stamp <hex2>  ; echo stamp2=<hex2> > .../host-done.txt
```

`--regions` defaults to the mapping measured above and must be re-read from
`info mtree -f` if the guest's RAM size or machine type ever changes — that is
R21, and the default being right today is not a reason to trust it tomorrow.
`arena-host translate` takes the guest's PFN list instead of scanning, and is
the path that becomes live the day AWE is available.

The unit tests are where the translation arithmetic is pinned down:
`cargo test -p arena-host` covers the measured mapping, the PCI hole, and the
identity-mapping mistake; `cargo test -p arena-record` covers the record layout
the two sides share.

---

# Large pages, AWE, and the privilege that unlocks both

Measured 2026-08-11, immediately after the section above. Step 0's fragmentation
result — 1471 discontiguous runs for a 64 MiB window — prompted the obvious
question: if we know we need a large contiguous chunk, ask for one. Windows does
support that, and the mechanism is **large pages**: `VirtualAlloc` with
`MEM_LARGE_PAGES`, in multiples of `GetLargePageMinimum()` (2 MiB here). Each
large page is one physically contiguous 2 MiB block and is never paged.

Large pages and AWE need the same thing: `SeLockMemoryPrivilege`. So the
privilege question that step 0 stopped at had to be answered first.

## The privilege grant, and how to undo it

**What was changed.** One LSA account right was added, to one account:

    account : TESTGUEST\<guest-user>  (S-1-5-21-2794265523-3702185664-1359551570-1001)
    right   : SeLockMemoryPrivilege  ("Lock pages in memory")
    method  : LsaAddAccountRights, via scripts/win-account-right.ps1
    date    : 2026-08-11

`LsaAddAccountRights` was chosen over `secedit /configure` deliberately: it adds
one right to one SID and touches nothing else, whereas `secedit` applies a whole
template. Before the change the account held **no** explicitly assigned rights
at all (`LsaEnumerateAccountRights` → status 2, everything it had came from
group membership), so the account's entire explicit-rights set is now exactly
this one entry.

**To revert**, from the guest:

```
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\win-account-right.ps1 `
  -Sid S-1-5-21-2794265523-3702185664-1359551570-1001 `
  -Right SeLockMemoryPrivilege -Revoke
```

and reconnect (or log off) so the next token is issued without it. `-List` shows
the current state without changing anything. A full pre-change snapshot of the
machine's user-rights policy is also saved at
`C:\Windows\Temp\user-rights-BEFORE-svprobe.inf` (`secedit /export`), which can
be diffed or re-applied if anything else is ever suspected.

**What it cost, verified rather than assumed.** Each of these was checked before
and after:

| claim | before | after |
|-------|--------|-------|
| a reboot is needed | — | no; a fresh ssh connection issues a token that has it |
| the interactive user must log off | — | no; that session was never touched |
| `testsigning` | not set | not set |
| `nointegritychecks` | not set | not set |
| `hypervisorlaunchtype` | `Off` | `Off` |
| DeviceGuard `SecurityServicesRunning` | `0` | `0` |

So the grant is a local security-policy change and nothing more: no code
integrity setting moved, and the constraint that ruled out test-signing is
untouched. What it *widens* is recorded as R25 in `security-model.md`.

## Fragmentation, which was the point

Same 64 MiB window, same host-side run count, four allocators:

| allocator | runs in guest-physical space | mean run | frames known? |
|-----------|------------------------------|----------|---------------|
| `VirtualAlloc` + `VirtualLock` (4 KiB) | 1471 | ~44 KiB | no |
| `AllocateUserPhysicalPages` (AWE, 4 KiB) | 5863 – 16339 | ~11–44 KiB | **yes** |
| `AllocateUserPhysicalPages2`, `NONPAGED_LARGE` | 3992 of 3992 | **4 KiB** | yes |
| **`VirtualAlloc(MEM_LARGE_PAGES)`** | **22** | **~2.9 MiB** | no |

**Large pages are a 67× improvement** over the step-0 baseline and better than
the 32-run floor a naive reading predicts, because adjacent 2 MiB blocks merge:
22 runs for 32 large pages means ten pairs happened to be neighbours.

Two of those rows are worth reading carefully.

**AWE is the *worst* case for contiguity, not the best.** `AllocateUserPhysical`
`Pages` hands out whatever frames are free, one at a time; measured in-process
over the returned frame array it was 5863 runs (2.8 pages per run) on one
attempt and 16339 on another — essentially scattered, and variable with the
guest's memory state. Getting frame numbers and getting contiguity are separate
problems, and AWE only solves the first.

**`MEM_EXTENDED_PARAMETER_NONPAGED_LARGE` did not deliver large pages here.**
`AllocateUserPhysicalPages2` resolves from `kernelbase.dll` (not `kernel32.dll`
— asking only kernel32 produces a wrong negative) and the call *succeeds*, but:
it satisfied only 3992 of 16384 requested frames, took ~200 ms, and the frames
it returned were **1.0 pages per run** — completely scattered 4 KiB frames. On
Windows 11 build 26200 this parameter bought nothing. `design.md` §14 item 8
proposed it as the fix for fragmentation; measured, it is not.

## Does the driver still import them?

The import path pins pages, so a 2 MiB page is not obviously the same thing to
it as a 4 KiB one. It is:

```
large-pages window, 64 MiB:  getMemoryHostPointerProperties = SUCCESS
                             memoryTypeBits = 0x6, type 1, import = ok
                             mapped_same_address = true
                             host records via the Vulkan mapping: 16384/16384
awe window, 64 MiB:          identical, and 16384/16384
awe2 window, 16 MiB:         identical, and 3992/3992
```

Every allocator produced memory the driver accepted, with the same
`memoryTypeBits` as ordinary process memory, and in every case the full
host↔guest↔Vulkan loop of step 0 passed unchanged: the host wrote through the
memfd, the guest saw it through the `VkDeviceMemory` mapping, the guest wrote
back through that mapping, the host read it, and a second host write made after
the import was seen by the guest.

## R19 is retired, and so is the last of R20

With the privilege present, the AWE path ran for real:

- `AllocateUserPhysicalPages` returned 16384 frames, and **`arena-host translate`
  verified all 16384 against the memfd**: every frame number, put through the
  GPA→offset mapping, landed on exactly the page the guest said it was. That is
  `design.md` §5.1's mechanism working end to end, and it settles §14 item 2 —
  **`AllocateUserPhysicalPages` still returns raw guest-physical frame numbers**
  on Windows 11 build 26200. Not "no reason to think it changed": measured.
- The driver imported that AWE window without complaint, which retires **R19**.
  Microsoft's "AWE address ranges cannot be used to buffer data for graphics or
  video calls" does not bind WDDM's user-pointer import path on this driver.

## Is large-page allocation reliable?

A slab allocator that intermittently cannot get its memory is worse than one
that is merely slow, so this was measured rather than assumed:

```
64 MiB   x5 : 5/5 succeeded, slowest 26 ms
256 MiB  x5 : 5/5 succeeded, slowest 28 ms
1024 MiB x3 : 3/3 succeeded, slowest 90 ms
```

Context, because this is the favourable case and should not be read as a
guarantee: Windows 11 build 26200, 13248 MiB total with 8973 MiB free, 1.3 hours
of uptime. Large-page allocation needs *contiguous* physical memory, and its
failure mode (`ERROR_NO_SYSTEM_RESOURCES`, 1450) becomes more likely the longer
the guest has been running and the more fragmented its physical memory is. None
of that was exercised here, and deliberately so — the guest is somebody's daily
machine and manufacturing memory pressure on it is not acceptable.

**What the fallback should be.** Three things, in order:

1. **Allocate the whole arena once, at GPU-side start-up**, not per-slab on
   demand. Fragmentation grows with uptime, so the cheapest mitigation is to ask
   early and hold. This also matches R11: the arena is a fixed pre-negotiated
   window anyway.
2. **Fall back to 4 KiB locked pages per slab** if `MEM_LARGE_PAGES` fails. It
   is the step-0 configuration, it needs no contiguity, it never failed, and the
   only cost is the VMA count on the host side — which `vm.max_map_count` of
   1048576 makes affordable. A slab should record which kind it is.
3. **Degrade per slab, not per arena.** A 64 MiB slab that cannot get large
   pages should become a 4 KiB slab; the arena should not fail because one slab
   was unlucky.

The `ArenaProvider` trait in `design.md` §5.1 is the right place for this: it
already abstracts "allocate, pin, enumerate frames", and large-versus-small is
one more thing it decides rather than something its callers see.

## The recommendation

Use **`VirtualAlloc(MEM_LARGE_PAGES)`** for the arena, with 4 KiB locked pages
as the per-slab fallback. It gives 22 runs instead of 1471, needs no PFN
enumeration at all, and the driver imports it exactly like anything else.

Note what that costs: large pages give contiguity but **not** frame numbers, so
the host has to locate the pages some other way. Today that is the content scan
`arena-host` does, which is fine for an experiment and not fine for a product.
The clean combination is to allocate with `MEM_LARGE_PAGES` and obtain the
frames separately — `QueryWorkingSetEx` reports each page's physical address to
a caller holding `SeLockMemoryPrivilege`, which is now available and is worth
measuring before the arena is written. The AWE route remains the alternative
that reports frames directly, at the cost of the fragmentation measured above.

---

# Can we have both? `QueryWorkingSetEx`, measured

The previous section left the arena with a real tension. Large pages give
physical contiguity — 22 runs for 64 MiB — but no frame numbers. AWE gives frame
numbers but the worst contiguity measured. If some user-mode call reported
physical addresses for large-page memory, we would have both and the arena's
shape would be settled. I named `QueryWorkingSetEx` as the candidate. It is not.

## It reports no physical address at all

The structure it fills, `PSAPI_WORKING_SET_EX_BLOCK`, is one word per page, and
`SeLockMemoryPrivilege` does not add a field to it. Measured over a 1 GiB
large-page window, all 262144 entries:

```
wsex[4KiB-stride]: n=262144 in 48.8 ms (186 ns/entry)
  valid=262144 largePage=262144 locked=262144 shared=0 bad=0
  shareCount=0 node=0 prot=0x4
  raw[0]=0x0000000000c00041  raw[1]=0x0000000000c00041
  reserved bits 24..30 max=0, bits 32..63 max=0x0 over 1 distinct value(s)
```

Read the last line: across a quarter of a million pages the whole 64-bit word
took **exactly one distinct value**. A frame number would have to differ per
page; nothing here does. The documented `ReservedUlong` (bits 32..63) — the only
place a physical address could hide — is zero for every page of every allocator
tried. This is not "the field is zeroed on this build"; there is no field, and
the measurement confirms the structure carries nothing page-specific beyond the
virtual address you passed in.

So **`QueryWorkingSetEx` does not serve.** Windows does not report frame numbers
to user mode outside AWE, which is exactly why AWE exists.

## What it *is* good for: telling the allocations apart

The `LargePage` bit works, and the three allocators are distinguishable
programmatically. Same call, same fields, three windows:

| window | raw word | `Locked` | `LargePage` | reserved 24..30 |
|--------|----------|----------|-------------|-----------------|
| `VirtualLock`ed 4 KiB | `0x0000000005400041` | 1 | 0 | 5 |
| AWE | `0x0000000000400041` | 1 | 0 | 0 |
| `MEM_LARGE_PAGES` | `0x0000000000c00041` | 1 | **1** | 0 |

So the GPU side can assert what it actually got rather than what it asked for —
worth doing, since a slab that silently fell back to 4 KiB pages has different
properties (R26) and the host side should be told which kind it is attaching.
`Locked` is set on all three, so it confirms pinning but does not discriminate.

**Cost.** ~180 ns per entry at 4 KiB stride — 48.8 ms for 1 GiB, 93.9 ms for
2 GiB, so linear and fine at attach time, once. But the call answers per
*address*, not per page-table entry, so on large pages you can stride by the
large page: 512 entries instead of 262144, **156 µs instead of 48.8 ms, a 312×
saving** for the same coverage (345 µs for 2 GiB). Enumerating a large-page
arena is free; enumerating a 4 KiB one is a one-off ~50 ms per gigabyte.

## So how does the host find large-page memory? Ranked

Since no API hands over the frames, the options are:

1. **The content scan (`arena-host scan`) — recommended.** The guest stamps the
   window, the host sweeps QEMU's memfd for the stamp. Measured cost:

   | window | scan wall time | pages located |
   |--------|----------------|---------------|
   | 64 MiB | 1.8 s | 16384 |
   | 1 GiB | 2.02 s | 262144 |
   | 2 GiB | 2.32 s | 524288 |

   **The cost is flat in window size, because it is linear in *guest RAM* size,
   not arena size** — the sweep reads 13312 MiB whatever the window is, and only
   the match handling grows: 32× the arena cost 0.5 s more. `design.md` flagged
   the extrapolation from 64 MiB as unproven; measured at 1 GiB and 2 GiB, it
   does not extrapolate the way a reader would fear. It is ~2 s of one-off
   attach cost for any arena this machine can hold, and it would be ~2 s on a
   4 KiB arena too.

   It is also the only method that *verifies* rather than trusts: it finds pages
   by their contents, so a wrong answer is not possible in the way a wrong
   translation formula is (R21).

2. **AWE's frame list**, if the frames matter more than contiguity. It reports
   frames directly and `arena-host translate` verified 16384/16384 of them — but
   at 267× the run count of large pages, which is the trade the previous section
   measured. Reasonable only if a future GPU side turns out to need frame
   numbers for something other than locating the pages.

3. **A pinning driver**, which could report anything it likes. Now clearly not
   worth an EV certificate: R19 is retired and the scan is 2 s.

The scan's one weakness is that it locates pages *once*. If a frame could later
move, the table would go stale silently — that is R26, and it is another reason
the large-page path is preferable, since large pages are not candidates for
relocation by the ordinary paging machinery.

## Two things the 1 GiB run settled on the way

**Large-page contiguity gets better with size, not worse.**

| window | contiguous runs in guest-physical space |
|--------|------------------------------------------|
| 64 MiB | 22 |
| 1 GiB | **1** |
| 2 GiB | 2 |

The 1 GiB window came back as one single extent — 512 adjacent 2 MiB blocks, the
whole gigabyte — and 2 GiB as two. A host-side attacher maps those with one or
two `mmap` calls rather than the ~1471-per-64-MiB the 4 KiB path needs. (The
1 GiB window's base landed at memfd offset `0x200000000`, GPA `0x240000000`:
gigabyte-aligned.) Windows is evidently satisfying these from a contiguous pool,
so asking for the whole arena in one call is better than asking slab by slab.

**Imports of 1 GiB and 2 GiB work.** `vkAllocateMemory` with
`VkImportMemoryHostPointerInfoEXT` accepted each in a single `VkDeviceMemory` —
`memoryTypeBits = 0x6`, mapped at the same address, and for the 1 GiB case all
262144 pages verified against the host afterwards. Slab size is therefore a
choice about allocator behaviour and import count, not a limit the driver
imposes at these sizes.

**Frames did not move under `VirtualLock`, over the interval tested.** A 64 MiB
locked window was located in the memfd, held while the guest went about ordinary
business, and re-located 3m14s later: byte-identical offset table, all 16384
pages, 931 runs both times. That is evidence against casual relocation and *not*
evidence against relocation under memory pressure, which was not provoked. It is
recorded as R26 rather than treated as settled — and it is another reason to
prefer large pages, which are not candidates for the ordinary paging machinery
at all.

## A false negative worth recording

One run first reported `host_records_via_vk = 0/262144`, which looked like a
1 GiB import silently failing to share. It was not: the rendezvous flag file had
been written with a literal `n` instead of a newline (`stamp=abcd...2222n`), so
the guest's hex parse failed, fell back to zero, and dutifully searched for a
stamp of zero. Host-side verification of the same pages found all 262144 host
records and all 262144 Vulkan-written records present.

Recording it because the experiment is only worth what its failure modes are
worth: the run id and per-page index checks did their job — the "failure" was
localised to a parse in the harness in a couple of minutes — but a reader
skimming logs would have seen a zero and believed it. Counts that come out at
exactly zero deserve the same suspicion as counts that come out perfect.

---

# A GPU running work against arena memory — the instrument, not yet the reading

Built and validated 2026-08-11; **not yet run on the target hardware**, for the
reason given at the end. Read the title as written: everything below is a
working experiment and a set of results from *other* machines. No number here
comes from the Intel iGPU.

This is `design.md` §14 item 1 and `security-model.md` R15 — **has a GPU ever
executed a workload against arena memory?** Everything above measures that the
driver *accepts* our pages: the import succeeds, `vkMapMemory` returns the same
virtual address, and CPUs on both sides of the hypervisor see the same bytes.
An import is not a submission.
GPU-side address translation is entirely downstream of the import succeeding and
none of it had been exercised.

## What the experiment is

`crates/gpu-probe`, a sibling of `awe-probe` rather than a rewrite of it: the
window comes from `awe_probe::alloc`, the same four allocators the sections
above measured, so this is the same memory and not an approximation of it.
`crates/arena-host` gains three commands for the Linux side, and
`crates/arena-record` gains the definition of the arithmetic — *once*, because
three parties now have to agree about it rather than two.

The window is split in half. The first half is a `VkBuffer` bound to the
imported `VkDeviceMemory` at offset 0; the second is a second `VkBuffer` bound
to the same allocation at offset `half`, which also answers "can a buffer live
part-way into imported memory?", the question a suballocating arena actually
asks. A compute shader reads every input word and writes

    out[i] = transform(in[i], i)

where `transform` is a finalising hash of the value *and* the index. That choice
is the whole point. A GPU that wrote a constant, read the wrong page, read a
stale page, or got the index off by one produces a value that is unrelated to
the right one rather than nearly right, so the check cannot be passed by
accident. `arena_record::gpu::transform` and
`crates/gpu-probe/shaders/arena.comp` state the same function in Rust and GLSL,
and `build.rs` fails the build if the constants in the two ever drift.

Three parties then judge the same bytes: the shader produces them, the Windows
guest checks them through its mapping, and a Linux process checks them again by
reading QEMU's memfd. The guest's check is the weak one — its mapping and the
Vulkan mapping are the same virtual address, measured — so the host's
independent read through different page tables in a different process is what
the claim rests on.

### Two design details that are load-bearing

**The host predicts, it does not compare.** The guest and the host both check
the output against `transform(host_fill(stamp, i), i)`, derived from the stamp
the writer announced — not against a transform of whatever the input currently
holds. Comparing against the current input would let a GPU reading a stale copy
of a page and a CPU reading the same stale copy agree with each other, and the
check would pass on memory that is not shared at all.

**The first 256 bytes of every page are reserved and nothing writes them.** The
`SVARENA1` record therefore survives the workload, so after the GPU has written
the whole window the host can still ask "is this the page I mapped?" and get an
answer. R26 says the 4 KiB path has no cheap detector because step 0's stamps
are destroyed the moment real data lands in the pages; this is R26's proposed
canary, built and exercised, at a cost of 6.25% of the window.

## Two bugs the tests caught before any GPU ran

Recorded because a measurement is worth what its failure modes are worth, and
both of these would have produced a *false pass* rather than a visible error.

1. **`transform(0, 0) == 0`.** The original hash had zero as a fixed point, so
   word 0 of a page the GPU never touched would have been accepted as correct. A
   non-zero seed constant fixes it, and the test now asserts no fixed point over
   4096 indices.
2. **The stamp fold collapsed.** `host_fill` folded the two halves of the 64-bit
   stamp with a plain XOR, so every stamp of the form `0xAAAAAAAA_AAAAAAAA`
   became zero and two different stamps could produce byte-identical fills —
   which would have silently disarmed the concurrency check, whose entire
   content is telling two stamps apart. Each half is now multiplied by a
   different odd constant first.

Neither was found by inspection. Both were found by a unit test asking whether
an untouched page passes, which is the test worth writing.

## The harness, validated end to end before touching the laptop

`gpu-probe` is OS-agnostic in its Vulkan half and takes a pointer, so the whole
path — shader, binding, submission, fences, and the three-party protocol —
was run on the build machine first. That is not squeamishness: the target GPU
drives a physical laptop panel and somebody is using the machine (R1, R18), and
a first submission that hangs it would be visible to them.

With `--file`, the Linux build backs its window with a `MAP_SHARED` file that
`arena-host` plays the host half against. One machine, no hypervisor, so it
proves nothing about the arena — but it proves the harness, which is the part
that could otherwise produce "every word wrong" on the laptop for a reason that
has nothing to do with the GPU:

```
located 2048 pages, 1 contiguous run
gpufill: 983040 payload words across 1024 input pages in 2.65 ms
guest : checked 983040, correct 983040, wrong 0, untouched 0, headers 2048/2048
host  : gpuverify -> 983040/983040 as the shader required, headers 1024/1024
```

The concurrency phase was exercised the same way: 4004 dispatch-and-check passes
against 1229 host churn passes, and the guest saw **both** stamps (2.12 G words
of the old value, 1.82 G of the new), which is the check that the two sides
actually overlapped. A concurrency result where the GPU only ever saw one value
would mean nothing, and the probe says so rather than reporting a clean zero.

## A finding on the way: two drivers refuse a shared mapping

The same binary, the same 4 MiB window, on the build machine's three
implementations. The only variable is whether the pages are an anonymous private
mapping or a `MAP_SHARED` mapping of a file:

| driver | anonymous private | `MAP_SHARED` of a file |
|---|---|---|
| NVIDIA RTX 5090 (proprietary, Linux) | imported, `memoryTypeBits=0xc` | **`VK_ERROR_OUT_OF_DEVICE_MEMORY`** |
| RADV (AMD iGPU, Mesa) | imported, `memoryTypeBits=0x20` | **`VK_ERROR_INVALID_EXTERNAL_HANDLE`** |
| lavapipe | imported, `memoryTypeBits=0x1` | imported |

RADV's refusal is `design.md` §5.4's prediction measured rather than cited —
libdrm passes `AMDGPU_GEM_USERPTR_ANONONLY` unconditionally and Linux gives
`MAP_SHARED|MAP_ANONYMOUS` a backing file, so a shared arena is unimportable on
AMD/Linux by construction, and the error code is exactly the one the forwarding
research names.

**The NVIDIA result is new and corrects the design.** §5.4's table lists
"Intel/NVIDIA Linux" as a `HostPointerImport` row. That row is right for
ordinary process memory and wrong for the memory this design actually shares: an
arena mapping is necessarily shared, and the proprietary driver rejects it — with
`OUT_OF_DEVICE_MEMORY`, which reads like a resource problem and is not one. So a
Linux GPU side needs `DmaBufImport` on **both** AMD and NVIDIA, not only AMD, and
`HostPointerImport` on Linux is currently evidenced only by lavapipe, which
§5.4 already calls a dangerous oracle.

None of this touches the target hardware, whose GPU side is Windows and whose
answer is above: the Intel driver imports host-shared guest RAM with
`memoryTypeBits = 0x6`. It does say the pluggable `SlabImport` trait is carrying
more weight than the table suggests.

## Re-running it

The two sides rendezvous through the same virtio-fs share as step 0
(`/srv/winshare/probe` on the host, `Z:\probe` in the guest).

```
# build (from the dev shell — glslangValidator is now in it, for the shader)
cargo build -p gpu-probe --release --target x86_64-pc-windows-gnu
cargo build -p arena-host --release
install -m0755 target/x86_64-pc-windows-gnu/release/gpu-probe.exe /srv/winshare/probe/

# the whole run, both sides, one command
scripts/gpu-workload-run.sh --agent <ssh-agent-socket> --pages 16384 --mode large
scripts/gpu-workload-run.sh --agent ... --concurrent    # adds the churn phase

# or by hand — guest first, it waits for the host between phases
gpu-probe.exe --workdir "Z:\probe" --pages 16384 --mode large
#   --no-host      fill the input locally; no memfd, no host half
#   --no-perf      correctness only, disturb the machine as little as possible
#   --concurrent   the host-writes-while-the-GPU-reads phase
# host, as root, against QEMU's memfd
arena-host scan     --fd /proc/<qemu>/fd/<n> --run <run_id> --out map.txt
arena-host gpufill  --fd ... --map map.txt --stamp <hex>
echo stamp=<hex> > /srv/winshare/probe/host-gpu-fill.txt
arena-host gpuverify --fd ... --map map.txt --stamp <hex> --run <run_id>

# the harness, on any Linux machine with a GPU, sharing nothing
gpu-probe --file /tmp/window --pages 4096 --workdir /tmp/probe
```

Safety properties worth not removing: the fence has a 2 s timeout rather than an
unbounded wait, `VK_ERROR_DEVICE_LOST` exits rather than retries, the window is
released on every exit path including panics, and the shader has no unbounded
loop and writes nothing outside its own buffer.

The unit tests are where this is pinned down without a GPU: `cargo test -p
arena-record` covers the shared arithmetic and the fixed-point bug, `cargo test
-p gpu-probe` covers the verifier — including that a zeroed output does *not*
pass and that a right-value-wrong-index answer does not either — and `cargo test
-p arena-host` covers the host's page-index arithmetic and its behaviour when
the scan misses a page.

## What did run: two real GPUs, imported host memory, correct results

Not the target, and not the arena — ordinary anonymous process memory on the
build machine, with no hypervisor anywhere near it. But two real drivers from
two vendors, and this is the first time in the project that a GPU has executed a
workload against memory imported through `VK_EXT_external_memory_host` at all.
Identical 128 MiB window, 64 MiB per buffer, 15728640 payload words:

| | NVIDIA RTX 5090 (discrete) | RADV, Ryzen 9950X3D iGPU (unified) |
|---|---|---|
| import `memoryTypeBits` | `0xc`, used type 2 | `0x20`, used type 5 |
| buffer `memoryTypeBits` | `0x1f` — intersects | intersects |
| bind at 0 and at 64 MiB | OK | OK |
| correct / checked | **15728640 / 15728640** | **15728640 / 15728640** |
| untouched, wrong | 0, 0 | 0, 0 |
| page headers intact | 32768 / 32768 | 32768 / 32768 |
| submit+fence, empty dispatch | 0.051 ms | 0.651 ms |

Three things follow that do transfer to the design, even though the memory does
not.

**The bind gate is not obviously a problem.** On both drivers the buffer's
`memoryTypeBits` intersected the mask the pointer import permitted, and
`vkBindBufferMemory` accepted the allocation at offset 0 *and* at a 64 MiB
offset. The failure mode where imported memory is mappable but not bindable did
not occur on either. It remains unmeasured on the target.

**Submission overhead is real and differs by an order of magnitude between
implementations** — 0.051 ms against 0.651 ms for the same empty dispatch. That
is worth knowing before the forwarding design starts counting round trips: the
floor is a property of the implementation, not of the work.

**The performance question has a proxy answer, and it is the encouraging one.**
Bandwidth over the payload, read plus write, with the empty-submit floor
removed:

| | imported host pointer | driver alloc, same memory type | driver alloc, device-local |
|---|---|---|---|
| RTX 5090 (discrete) | 55.9 GB/s | 58.7 GB/s | 3213 GB/s |
| RADV iGPU (unified) | **33.6 GB/s** | 31.0 GB/s | **62.5 GB/s** |

Read the two rows against each other, because the contrast is the result. On the
discrete card imported host memory is crippled — it is across PCIe, and the
gap is the bus. **On the unified-memory iGPU the gap is 1.9×, not two orders of
magnitude**, which is the shape the arena needs: host memory is a fraction
slower than the GPU's preferred pool rather than a different class of thing.

Two cautions on that table, because it would be easy to over-read. The RTX
5090's device-local figure is **cache-resident** — the card has 128 MB of L2 and
the working set is 128 MiB — so it is "much faster", not a DRAM bandwidth
measurement; the honest discrete-card claim is only that imported host memory is
bus-limited. And on both machines **the import itself costs nothing**: imported
memory ran within a few percent of a driver allocation of the *same memory type*
(1.047× on NVIDIA, 0.935× on RADV — imported was marginally the faster of the
two there). What costs is which memory type the pages are, not whether the
driver got them from us.

None of this is the Intel iGPU, and an AMD APU is not an Intel one. It does
mean the design's viability question now has a measured analogue instead of an
assumption, and that the analogue says the arena is plausible for real work
rather than only for staging.

## What this does not establish — the target reading has not been taken

Stated plainly, because the section above is a set of results from the wrong
machines and would otherwise be easy to skim as though the question had been
answered.

**No GPU has yet executed this workload against arena memory.** The run needs
the Windows guest and the Linux host on `testhost` at the same time, and the
ssh agent that authenticates to both stopped signing part-way through this work
— `agent refused operation`, the 1Password agent having locked — so the live
half never happened. Nothing was worked around: no number attributed to the
Intel iGPU appears anywhere above.

So these remain open:

1. **R15 proper.** Whether an Intel Windows driver, headless,
   imported-memory-only and with no swapchain, will execute a submission at all.
   Two other drivers doing so on Linux is encouragement, not evidence about this
   one.
2. **Whether the iGPU's translation reaches host-shared guest RAM.** The AMD and
   NVIDIA runs imported *ordinary process memory*. The arena's memory is
   simultaneously guest RAM and mapped by a process across the hypervisor, and
   the whole content of §14 item 1 is that GPU-side translation is downstream of
   the import and has never been exercised on those pages. R9's new result
   sharpens this rather than softening it: two of three Linux drivers refuse a
   merely *shared* mapping outright, so "imports anonymous memory" and "imports
   arena memory" are demonstrably different claims.
3. **Large pages against 4 KiB.** The chosen path and the fallback have never
   been compared under GPU load. A difference would matter more than almost
   anything else here, and `--mode large` versus `--mode locked` is the whole
   experiment.
4. **R10 under concurrency.** The churn phase has run only against lavapipe,
   which is a CPU.

To take the reading, publish the binaries and run
`scripts/gpu-workload-run.sh --agent <socket> --pages 16384 --mode large`, then
the same with `--concurrent`, then with `--mode locked`.

---

# The reading was taken: the Intel iGPU computed against arena memory

Measured 2026-08-12 on `testhost`, 64 MiB large-page window, the run the
section above says is needed. **R15 is retired and §14 item 1 is closed.**

```
guest : device: Intel(R) UHD Graphics — queue family 0, VK_EXT_external_memory_host: yes
guest : imported 64 MiB as VkDeviceMemory: memoryTypeBits=0x6, using type 1
guest : bind: buffer memoryTypeBits=0x7 (import permitted 0x6), alignment 64
guest : bind: vkBindBufferMemory OK at offset 0 and at offset 33554432
guest : submit -> fence signalled in 3.265 ms
guest : payload words checked 7864320, correct 7864320, wrong 0, untouched 0
guest : page headers intact 16384/16384
host  : gpuverify -> 7864320/7864320 as the shader required, headers 8192/8192
        VERDICT: the Linux host can read what the GPU computed
```

Read the last two lines together. The input half was filled **by a Linux process
through QEMU's memfd**, the shader read it and wrote `transform(in[i], i)` into
the output half, the guest checked it, and then a process on the other side of
the hypervisor checked the same bytes through different page tables. The
transform depends on the value *and* the index, so a GPU that read a stale page,
the wrong page, or the right page at the wrong offset produces an unrelated
number rather than a nearly-right one.

**Nothing about the arena is unproven at the memory level any more.** Step 0
proved the driver accepts our pages; this proves the GPU executes against them.

## The performance answer, which is the encouraging one

Same run, `--iters 10`, over 7864320 payload words read+write:

| | median | with the 1.347 ms empty-submit floor removed |
|---|---|---|
| **imported host pointer (the arena)** | 3.262 ms | **34.08 GB/s** |
| driver allocation, same memory type (1) | 3.732 ms | 33.56 GB/s |
| driver allocation, device-local (type 0) | 3.646 ms | 29.02 GB/s |

**The import costs nothing: 0.991× against a driver allocation of the same
type.** That matches the AMD APU proxy (0.935×) and the NVIDIA one (1.047×)
almost exactly, so the "imported memory is a fraction slower" shape holds on the
target hardware and not only on the analogues.

The third row is the surprise and it is worth not skipping: on this unified iGPU
**device-local memory is *slower* than the arena**, 29.02 GB/s against 34.08.
There is one heap and every type is `DEVICE_LOCAL`; type 0 differs from type 1
only in not being host-visible, and being uncached for the CPU evidently costs
the GPU nothing while costing this benchmark's access pattern something. The
practical consequence is that on this machine there is no memory the arena is
giving up performance to, which removes the last reason to want a staging path
in v1.

`submit+fence with an empty dispatch: min 1.347 ms, median 1.445 ms` — an order
of magnitude worse than the RTX 5090's 0.051 ms and twice RADV's 0.651 ms. That
is the floor a forwarding design has to amortise, and it is a property of this
driver rather than of the forwarding.

## Four harness faults found on the way, all of which fake a negative result

Recorded because the section above was written against a harness that had never
run end to end, and every one of these presents as the arena failing.

1. **`gpu-workload-run.sh`'s root helper ran the command unprivileged.** It was
   `{ $1; } | ~/rootc`, which executes `$1` as the user and feeds *its output*
   to root. `/proc/<qemu>/fd/*` therefore came back empty and every redirect
   into `/srv/winshare` failed with `EACCES`, both silently.
2. **`rootc` truncates and kills anything slower than half a second.** It pipes
   the script into `socat`, which sees EOF and closes the connection after its
   default 0.5 s, SIGHUPping the root shell and its children. Measured:
   `date; sleep 3; date; echo survived` prints the first date and nothing else.
   **The ~2 s arena scan was killed part-way on every run**, leaving an empty
   `map.txt`, a `gpufill` that wrote nothing, and a guest reporting "0 of
   7864320 words". The working pattern is detach with `nohup setsid`, redirect
   to a file, poll for a completion marker, fetch afterwards — `pl.sh rootlong`.
3. **A glibc binary built on another NixOS machine does not run here.** Its
   `INTERP` names a store path that does not exist on the target and the error
   is `cannot execute: required file not found`, which reads like a missing
   binary. The fix is a **static musl build**; `flake.nix` now carries
   `x86_64-unknown-linux-musl` for exactly this, and it is what makes `sv-hostd`
   deployable to the target without putting a toolchain on somebody's laptop.
4. **Windows OpenSSH kills a `nohup`'d background process when the channel
   closes.** The command runs under a job object and the job is torn down; msys
   `nohup` sets a signal disposition, which a job-object kill does not respect.
   The symptom is `guest.log` created by the redirect and left at zero bytes.
   Hold the ssh channel open for the run instead.

Two of these produced *exactly* the "0 of N words" shape that the false negative
already recorded in this document produced, and for four different reasons. A
count that comes out at zero really does deserve the same suspicion as one that
comes out perfect.

---

# `sv-hostd`'s QEMU mode, measured against the live guest

Built and run 2026-08-12. This is design.md §13 step 7's host half: discover
QEMU, read `info mtree -f` over QMP into a region table, locate the guest's
extent by content, coalesce, and stitch it into one virtually contiguous view.

```
guest RAM in the flat view: 13311 MiB in 4 alias(es)
  gpa 0x000000000000..0x0000000bffff -> offset 0x000000000000
  gpa 0x000000100000..0x000000102fff -> offset 0x000000100000
  gpa 0x00000010f000..0x0000bfffffff -> offset 0x00000010f000
  gpa 0x000100000000..0x00037fffffff -> offset 0x0000c0000000
located 16384/16384 pages in 2.20s (swept 13312 MiB; 0 other-run, 0 outside RAM)
coalesced into 29 run(s) covering 64 MiB — 565.0 pages per run
  first run: offset 0x00013ca00000 (gpa 0x00017ca00000), 2048 KiB
stitched 64 MiB into one view with 29 mmap(MAP_FIXED) call(s) in 95.74µs
verify: 16384/16384 pages of the stitched view carry their own extent index
```

Four things this settles.

**The region table is read, and it is the measured one.** Four RAM aliases,
including the 3 GiB split and the 1 GiB offset above 4 GiB, discovered rather
than compiled in — the backend's *id* comes from `query-memdev`, its descriptor
from the `/proc/<pid>/fd` link, and QEMU's pid from a cmdline match that refuses
rather than guesses if two match.

**Guest RAM is 13311 MiB, not 13312, and the missing 311296 bytes are exactly
the ROM window plus the twelve SynIC pages.** 76 pages of the backing object are
not memory the guest can see. `arena-host`, which sweeps the whole file, would
accept a stamp found in them; `sv-hostd` checks every hit against the region
table and counts the rejects. On this run the count was 0, which is what it
should be — but it is now a number rather than an assumption.

**The scan cost is 2.20 s**, against the 1.8–2.32 s measured for `arena-host`.
Same order, same shape, and still flat in arena size because it is linear in
guest RAM.

**Stitching 64 MiB of large-page memory took 29 `mmap` calls and 95.74 µs.** The
run count is the fragmentation figure this document measured at **22** for the
same window; 29 on this occasion. That is the same allocator on a guest with
three more days of uptime, so treat 22 as one sample of a small number rather
than a constant. Either way it is two orders of magnitude below the 1471 of the
4 KiB path, and the attach is *microseconds* — the cost of the extent is
entirely in the locate.

**`verify` is the §1 invariant, asserted over real guest memory.** Every one of
the 16384 pages of the stitched view carries the page index the guest stamped
into it, so extent offset *n* holds the page the guest called *n* — which is the
property everything above the attacher is built on, and which a stitcher that
mapped the right pages in the wrong order would fail while still looking like a
working arena to any test that does not read the index.

---

# The reverse direction: two Linux drivers, and only one needs a privilege

Measured 2026-08-12 on the build machine with `crates/dmabuf-probe`. The question
is the *conventional* topology — GPU side on the host, app side inside the VM, a
guest borrowing the host's GPUs with no passthrough. The arena must still be
guest RAM, because an fd cannot cross into the guest, so the importing driver is
NVIDIA or RADV rather than Intel-on-Windows.

The premise everybody was working from is in this document already: "two drivers
refuse a shared mapping", RADV with `INVALID_EXTERNAL_HANDLE` and NVIDIA with
`OUT_OF_DEVICE_MEMORY`. From that, `sv_arena::import` concluded that a Linux GPU
side needs `/dev/udmabuf`, which is `root:kvm` 0660 and unreachable without a
group change.

**Half of that conclusion is wrong, and the variable is which kind of shared
memory it is.** Four rows per device, 4 MiB and 128 MiB, identical results:

| backing | NVIDIA RTX 5090 | RADV (9950X3D iGPU) | lavapipe |
|---|---|---|---|
| anonymous private | imported, `0xc` | imported, `0x20` | imported |
| **`MAP_SHARED` of a memfd** | **imported, `0xc`, coherent both ways** | refused, `INVALID_EXTERNAL_HANDLE` | imported |
| `MAP_SHARED` of a tmpfs file | **imported** | refused | imported |
| `MAP_SHARED` of an ext4 file | refused, `OUT_OF_DEVICE_MEMORY` | refused | imported |
| the memfd handed to `VkImportMemoryFdInfoKHR` | refused, no memory type | refused | imported |

The earlier measurement used `gpu-probe --file /tmp/window`, and `/tmp` here is
ext4. **The arena is a memfd**, which is shmem — so the recorded refusal was
measured against a backing the arena does not use.

Two consequences, and they differ per vendor:

- **NVIDIA can serve a guest with no privilege change at all.** It imports a
  shared mapping of shmem — memfd or tmpfs, which is exactly what QEMU's
  `memory-backend-memfd` is — as `VK_EXT_external_memory_host`, with bytes
  verified in both directions. No udmabuf, no `kvm` group, no dma-buf.
  `sv_arena::import::path_for_driver` currently sends `"nvidia"` to
  `BackingKind::DmaBuf` and that is now wrong.
- **RADV still cannot**, and the reason is structural rather than incidental:
  libdrm passes `AMDGPU_GEM_USERPTR_ANONONLY` unconditionally, so *any* shared
  mapping is refused whatever it is backed by. udmabuf remains the only route,
  and it remains blocked: `/dev/udmabuf` is `root:kvm` 0660 and this account is
  not in `kvm`, so `open` gives `EACCES` (13).

**What to run once `sirati` is in `kvm`**, unchanged, to close the RADV half:

```
cargo build -p dmabuf-probe --release
./target/release/dmabuf-probe --size-mib 128
```

The probe already creates the memfd with `MFD_ALLOW_SEALING`, seals it with
`F_SEAL_SHRINK`, and calls `UDMABUF_CREATE`; the only thing that fails today is
the `open`. It prints the `fd(udmabuf)` row per device and says whether the
question was answered or merely blocked, so a single run settles it.

Worth stating either way: the group change buys **RADV and nothing else**. If
the reverse direction only ever has to serve the discrete card, it is not
needed.

---

# The cross-VM topology, running — and three things the design had wrong

Built and run 2026-08-12 against a local NixOS guest (`nix/guest.nix`,
`nix build .#svGuest`): 2 GiB of `memory-backend-memfd` RAM, a QMP socket, and a
virtio-serial port, on a machine that can be rebooted — which `testhost`
cannot.

```
guest : llvmpipe (lavapipe), provider AppMemfd
guest : extent 48 MiB of guest RAM, run 0xfc8ccc910e1cbdf7, kernel page size 4 KiB,
        stamped 12288 pages
guest : arena imported as a host pointer, memory type 0
guest : publishing the extent on /dev/virtio-ports/org.sirati.svcold.0
host  : the guest offers run 0xfc8ccc910e1cbdf7, 4096 control + 8192 arena pages, locator Scan
host  : located 12288/12288 pages in 128.58ms over 2047 MiB of guest RAM; 1 run, 0 outside RAM
host  : stitched 48 MiB with 1 mmap call; all 12288 pages carry their own index
guest : the host located the extent and accepted generation 1
guest : control region published, generation 1
host  : ICD client 0 attached (1 run)
guest : serving rings [1, 2, 3] on llvmpipe (lavapipe)

$ VK_DRIVER_FILES=…/sv_icd.json SV_SOCKET=/tmp/sv-guest/icd.sock vulkaninfo --summary
        deviceName = llvmpipe (LLVM 21.1.8, 256 bits)
```

That last line is the whole topology in one string. `vulkaninfo` ran on the
**host**, with no driver of its own available, through the real Vulkan loader
and our ICD; the command crossed a ring in memory belonging to **another
kernel**, located by sweeping QEMU's memory backend for a magic number; a driver
inside the VM answered; the reply came back through the generated decoder. The
app side and the GPU side did not share a descriptor, a filesystem, or an
address space.

Three corrections came out of it, and each of them was invisible until this
configuration existed.

## 1. §4.3's publish order destroys the stamps §5.1 depends on

design.md §4.3 step 1 has the owner "allocate both regions, **zero them**, write
every header field, then store `magic`", and step 2 offer the extent over the
cold path. §5.1 has the guest stamp every page and the host find those pages by
content. On one machine the two are compatible, because the host is handed a
descriptor and never has to search.

**Across a hypervisor they contradict each other.** `Control::create` zeroes the
control region, which erases exactly the stamps the host is about to sweep for.
Measured, first run of this path:

```
sv-hostd: located 8192 of 12288 pages
```

— the 8192 arena pages present and all 4096 control pages missing. Nothing said
why; the count was simply short, and a short count reads like fragmentation or a
slow guest rather than like a design error.

**The fix is to publish before initialising, and it costs nothing**, because the
design already requires the peer to tolerate an unpublished region: `magic` is
written last precisely so an early attacher gets `NotReady` and retries (§4.3
step 4 — and sharedgl's aliased-away readiness flag is why that rule exists). So
the order is stamp → publish → *let the host locate* → then write the header over
stamps that have done their job. §4.3's step order needs the qualifier that step
1's zeroing must follow step 3's locate whenever the locator is `Scan`.

## 2. `run_count == 0` conflates two different claims

`Attachment::run_count` is documented as "zero means the extent is one
contiguous range of the fd", and the client takes that as licence to
`mmap(fd, control_offset)`. That is right for `Mode::Local`, where `sv-hostd`
*made* the file and offset 0 of the memfd is offset 0 of the extent.

It is wrong for `Mode::Qemu`, where the file is QEMU's memory backend and offset
0 of it is **guest-physical zero**. A one-run extent at file offset
`0x2a00_0000` would have been mapped at 0, and the client would have found
valid, unrelated guest memory and reported no error — R21's failure mode
arriving through the handoff rather than through the translation.

"Contiguous" and "at the attachment's offsets in the file" are different claims
and the protocol was using one number for both. `qemu-serve` now always sends
the real run count; a one-run extent costs the client exactly one `mmap` either
way, so the shortcut bought nothing.

## 3. A backend's aliases appear in more than one flat view

`parse_flat_view` originally took every matching line in `info mtree -f`, with
the reasoning that "aliases of one backend appear only in the system flat view
in practice". That is true of `testhost`, which runs `smm=off`. It is false of
a guest with SMM: **every alias appeared exactly twice**, once under
`AS "memory"` and once under the SMM view, and a 2 GiB guest reported
**4095 MiB** of RAM.

It was benign in that run only because the duplicates were identical, so the
lookup took the first and got the right answer. It would not have stayed benign
— SMRAM legitimately aliases the same backend at *different* guest addresses,
and a table holding both would answer `offset_to_gpa` with whichever sorted
first. The parser now tracks the `FlatView` blocks and takes the one whose
address-space list contains `"memory"`.

## What the daemon→ICD boundary needs that the guest→host one does not

design.md §5.7 is firm that "**the run list does not cross the wire, and it never
did have to**". That is about the **guest↔host** boundary, where the guest cannot
know the runs — the entire reason `ScanLocator` exists.

The **daemon↔ICD** boundary is the opposite, and §3's own topology box already
says so: `sv-hostd` "hands **fd + runs** to the ICDs". It has to, for a
structural reason rather than a convenience: **a scattered extent has no single
descriptor whose `mmap` reproduces it.** The daemon's contiguous view is many
`mmap(MAP_FIXED)` calls into one reservation, and a view is not a file. Either
the runs reach the client and it stitches its own — one `mmap` per run, the cost
§5.7 already budgets — or the daemon copies, which §1's second invariant
forbids.

`ColdMessage::Runs` is that message, and `SharedRegion::stitch` is the one
implementation both sides call. Two implementations of scatter stitching would
be two chances to get an offset subtly different, and the symptom of that is an
arena where one side's writes land where the other is not reading — for which
there is no error.

## The numbers, against the Windows guest for comparison

| | local Linux guest | `testhost`'s Windows guest |
|---|---|---|
| guest RAM swept | 2047 MiB | 13311 MiB |
| locate | **128 ms** | 2.20 s |
| runs per 48–64 MiB | **1** (4 KiB pages, freshly booted) | 29 (2 MiB large pages) |
| stitch | 1 `mmap` | 29 `mmap`, 95.74 µs |

The one-run result is not evidence that 4 KiB pages are fine — it is a guest that
booted seconds earlier with 2 GiB free, which is the most favourable memory state
that exists. The Windows figure of 1471 runs per 64 MiB on the same page size is
the one to plan against. What the local rig is good for is that the *locate* is
sixteen times faster, because the sweep is linear in guest RAM and this guest has
a sixth as much — so an iteration is seconds rather than a round trip over
somebody's WiFi.

---

# The Windows platform layer, running on the Intel driver

Run 2026-08-13 with `scripts/sv-windows-e2e.sh`: two processes **inside the
guest**, no VM boundary, the real Intel iGPU at the far end. This is the
analogue of `sv-end-to-end.sh`, and what differs is only the platform layer —
named section objects instead of a memfd, a named pipe instead of a Unix socket,
and **no handle passed at all**.

```
[gpu] Intel(R) UHD Graphics (intel), queue family 0, provider AppMemfd (named section)
[gpu] arena 64 MiB imported as a host pointer, memory type 1
[gpu] waiting for the app side on \\.\pipe\shared_vulkan.e2e-…
[gpu] app side attached (two named sections, no handle passed)
[gpu] serving rings [1, 2, 3] on Intel(R) UHD Graphics (intel)
app : sv-icd: connected to the GPU side at e2e-…
app : === device 0: Intel(R) UHD Graphics  type=INTEGRATED_GPU api=1.4.323  (148 extensions)
app : === device 1: Intel(R) UHD Graphics  type=INTEGRATED_GPU api=1.3.0    (25 extensions)
```

**The verdict is a count, not a name.** On Linux the app side has no GPU, so
"reports the GPU side's driver" is proof. Here both processes are on the machine
that owns the GPU, the loader enumerates the Intel ICD *and* ours, and both say
`Intel(R) UHD Graphics`. What proves it is that there are **two of them on a
machine with one GPU**: a second entry with the same driver version can only be
ours forwarding to the first. `api=1.3.0` against `1.4.323` and 25 extensions
against 148 are the corroboration — 1.3.0 is our manifest's cap and 25 is the
profile we advertise. Device 1's heap 1 is 56 MiB, host-visible: that is the
arena, seen through §9.1's memory table.

`CreateFileMapping` + `MapViewOfFile` + `VK_EXT_external_memory_host` therefore
work together on the real Intel driver, and `DuplicateHandle` is not needed for
any of it.

## Four things in the way, none of them in our code

**1. The Vulkan loader ignores `VK_DRIVER_FILES` in an elevated process.**

```
[Vulkan Loader] INFO: Loader is running with elevated permissions.
                      Environment variable VK_DRIVER_FILES will be ignored
```

Windows OpenSSH hands this account an elevated, high-integrity token, so every
command run over ssh is elevated. This is a deliberate loader security measure —
an elevated process must not be talked into loading an attacker-named DLL — and
it applies to `VK_ADD_DRIVER_FILES` and the layer variables too.

**The symptom is a perfect-looking success.** `vulkaninfo` runs, exits 0, and
reports `Intel(R) UHD Graphics` with `driverID =
DRIVER_ID_INTEL_PROPRIETARY_WINDOWS` — the real driver reached directly, which
is exactly what a working forwarder would also print. Only the loader log says
otherwise. `runas /trustlevel:0x20000` does not help; the loader still sees the
process as elevated.

The way through is the registry, which is the documented mechanism and what
`scripts/win-register-icd.ps1` already existed for. The harness registers,
runs, and **unregisters on every exit path** including interrupt; the exposure
is a few seconds in which another application would enumerate one extra device.

**2. A `.ps1` with no BOM is read in the machine's ANSI code page.** Windows
PowerShell 5.1 does this, and `win-register-icd.ps1`'s comments contained em
dashes and section signs. On the German guest they decoded to characters that
closed a string early, and PowerShell reported a syntax error *on a different
line* than the one it could not read. The file is ASCII-only now; a BOM would
also work, but ASCII asks nothing of whoever copies the file, and this file is
built on Linux, pushed over scp, crossed virtio-fs and read on German Windows.

**3. Backslashes do not survive the shell layers**, even with
`MSYS_NO_PATHCONV=1`. `C:\\sv\\win-register-icd.ps1` arrived as
`C:svwin-register-icd.ps1`. Forward slashes work everywhere Windows accepts a
path, which `gpu-workload-run.sh` had already learned for `Z:/probe`.

**4. `vulkaninfo` cannot be the app side on Windows.** There is always a Win32
surface, so it calls `vkGetPhysicalDeviceSurfaceSupportKHR` and dies with
`ERROR_EXTENSION_NOT_PRESENT` before printing a single device. On Linux it skips
surface queries because `DISPLAY` is unset, which is why this never came up.
`VK_KHR_win32_surface` is simply not implemented — presentation is app-side and
X11/Wayland today. That is a WSI gap, not a platform-layer one, and `vk-probe`
is the right app for this test because it needs no surface.

## Two smaller notes

**The GPU side's own loader enumerates our ICD once it is registered**, and our
ICD then refuses to load, six times, with

```
sv-icd: cannot create the arena (the app-side arena mapping is implemented for
Linux only …); the driver will not load
```

That is correct behaviour rather than a bug: `SV_SOCKET` is unset in the GPU
side's environment, so `sv-icd` takes its standalone path, that path needs an
arena it cannot make on Windows, and it **refuses rather than answering from a
table**. Only `Mapping::create` is Linux-only; `Mapping::attach`, which is what
the connected path uses, is portable, which is why the app side works. Worth
knowing that registering the ICD system-wide makes the GPU side load its own
client DLL, and worth keeping the refusal loud.

**The named pipe is single-instance and does not re-listen.** A second
application against a live GPU side gets `CreateFileW … errno 231`
(`ERROR_PIPE_BUSY`) after the connect timeout. One session, one client is the
current design; serving several would need the `ConnectNamedPipe` loop the Linux
`--clients N` path already has.

---

# Where the *driver's* memory lives, and a dma-buf made out of it

Measured 2026-08-13 on **testhost**, the machine the VM actually runs on.
Everything before this section asks one direction of one question: *can the
guest's driver import memory the arena chose?* This asks the other direction —
**can the Linux host reach memory the driver chose?** — because that is what
decides whether `sv_gpu_server::arena`'s `GpuExport` provider is available
across a hypervisor at all, and `Provider::for_driver_across_a_vm` currently
answers `None` for the one driver that cannot import a host pointer.

The instrument is `gpu-probe --vk-alloc`, which is deliberately not a workload:
it opens the device, allocates through `vkAllocateMemory`, maps, writes the same
`arena_record` pages every other phase writes, and waits. `arena-host scan` —
unchanged, the step-0 instrument — is the reader.

## What the guest driver offers

```
device: Intel(R) UHD Graphics
memory heaps:
  heap 0: 6624 MiB flags DEVICE_LOCAL
memory types:
  type 0: heap 0 flags DEVICE_LOCAL
  type 1: heap 0 flags DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT
  type 2: heap 0 flags DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT | HOST_CACHED
```

One heap, unified, no separate VRAM. Types 1 and 2 are `memoryTypeBits = 0x6`,
which is the same pair step 0's import was given — the two measurements agree
about the device from opposite directions.

## 1. The driver's own allocation is ordinary guest RAM

64 MiB allocated and stamped through each host-visible type, then swept for in
QEMU's `memory-backend-memfd` (13312 MiB, fd 35 of the `windows-vm` process):

| type | flags | pages located | contiguous runs in the memfd |
|---|---|---|---|
| 1 | `DEVICE_LOCAL HOST_VISIBLE HOST_COHERENT` | **16384 / 16384** | 26, then 31 on a second run |
| 2 | …`HOST_CACHED` | **16384 / 16384** | 31, then 55 on a second run |

**Every page of a `vkAllocateMemory` allocation was in the memfd.** It is not
Intel stolen memory, it is not behind a BAR, and it is not anywhere the host
cannot map. The `x-igd-opregion=on` passthrough does not put ordinary allocations
anywhere special.

The contiguity is the surprise. Against the four allocators of "Large pages,
AWE, and the privilege that unlocks both", for the same 64 MiB window:

| allocator | runs |
|---|---|
| `VirtualLock`ed 4 KiB pages | 1471 |
| AWE | 5863–16339 |
| `AllocateUserPhysicalPages2` + `NONPAGED_LARGE` | 3992 |
| `VirtualAlloc(MEM_LARGE_PAGES)` | 22 |
| **the driver's own `vkAllocateMemory`** | **26–55** |

Mean run ≈ 1.2–2.5 MiB, which is the shape of 2 MiB-backed memory. The driver
gets within about 1.5× of the best allocator this project found **and needs no
`SeLockMemoryPrivilege`** — the privilege §0.1 had to have granted before large
pages were measurable at all.

## 2. The host can write it, and the driver's own mapping sees the write

`arena-host write1` through `MAP_SHARED` of the memfd, into all 16384 located
pages of each type, while the guest still held the allocations; then the guest
read back **through the `vkMapMemory` pointer the driver gave it**:

```
type 1: of 16384 pages, 16384 still carry our run id, 16384 carry the host's
        write, 16384 of those at the page index the host believed
type 2: of 16384 pages, 16384 still carry our run id, 16384 carry the host's
        write, 16384 of those at the page index the host believed
```

Every page, both types, and the page index the host computed matches the index
the guest reads it at — so the mapping is not merely "some shared memory", it is
aligned the way an arena needs. A later run in which the host deliberately
**did not** call `write1` reported `0 carry the host's write` for both types,
which is the negative control for the same code path.

## 3. And it can be turned into a real Linux dma-buf

`/dev/udmabuf` on **testhost** is `crw-rw---- root kvm` and `sirati` is in
`302(kvm)`, so it opens. Over the located pages of a live driver allocation:

```
QEMU's memfd seals: 0x7 [SEAL SHRINK GROW]
located pages coalesce into 31 run(s); offering the first 10 (14 MiB) to udmabuf
/dev/udmabuf: opened
UDMABUF_CREATE_LIST: got dma-buf fd 5
mapped the dma-buf: 3760/3760 pages carry the guest's record at the index the
map says — the dma-buf and the memfd are the same memory
```

Three things in that, each of which was an assumption before:

- **QEMU seals its memfd, and with the seal udmabuf needs.** `F_GET_SEALS`
  returns `SEAL|SHRINK|GROW`; udmabuf requires `F_SEAL_SHRINK`. This is read
  from the running process rather than from QEMU's documentation.
- **The scatter is expressible.** 31 runs is far under
  `/sys/module/udmabuf/parameters/list_limit`, which is 1024 here.
- **The dma-buf's bytes are the same memory**, checked by reading the guest's own
  records back through it — the check `sv_gpu_server::arena` records lavapipe
  failing, applied here rather than assumed.

`size_limit_mb` is **64**, so a 64 MiB arena is exactly at the ceiling and the
1 GiB slab §0.1 item 4 recommends is not possible without changing the module
parameter. That is a number to know before designing around this.

## What this does *not* establish

Four things, and the first two are corrections to the premise this measurement
was asked under rather than footnotes to it.

1. **It does not remove the content sweep, and it does not remove root.** The
   pages were found by sweeping 13 GiB of guest RAM for the stamp, and the memfd
   was reached through `/proc/<qemu-pid>/fd/35`. Nothing about the driver
   allocating instead of the application changes either: §0.1 item 5 measured
   that Windows reports no frame numbers to user mode outside AWE, and this is
   the *driver's* memory, so AWE does not even apply to it. Whichever side
   allocates, the host still has to *find* the pages. What did improve is
   fragmentation — 26–55 runs instead of 1471, so attaching a slab is tens of
   `mmap(MAP_FIXED)` calls rather than about fifteen hundred.
2. **Nothing here is measured about relocation.** The allocations were held for
   about 17 seconds. R26 measured a `VirtualLock`ed window stable over 3m14s and
   explicitly not under memory pressure; this is weaker evidence than that, not
   stronger.
3. **`DEVICE_LOCAL`-only memory — type 0 — is unmeasured, and it is the type
   that matters most for video.** It cannot be mapped, so this probe cannot
   stamp it, and a decoded video frame lives in exactly that kind of memory: a
   `VkImage` with `VIDEO_DECODE_DST`/`DPB` usage and `OPTIMAL` tiling. On a
   unified iGPU with one heap it is *plausible* that type 0 is also guest RAM,
   and plausible is what this project does not accept. Measuring it needs a
   staging copy or a dispatch that writes the record pattern into device-local
   memory, which is a piece of work rather than a flag.
4. **No GPU has read the host's write.** The probe does no dispatch. The
   converse — a GPU reading imported host memory — is what "a GPU has now run"
   established; this direction has not been established for either side.

## The thing that decides what any of it is worth: the host has no GPU

```
$ lspci -nnk | grep -A3 VGA
00:02.0 VGA compatible controller: Intel Corporation Raptor Lake-P [UHD Graphics]
	Kernel driver in use: vfio-pci
	Kernel modules: i915, xe
$ ls /dev/dri/
by-path
```

`/dev/dri` on the testhost host is **empty**. The only GPU is bound to
`vfio-pci` and handed to the guest, and there is no second one. A dma-buf is a
handle to memory; importing one into a GL or Vulkan context needs a DRM render
node, and there is none to import it into.

So the dma-buf measured above is real, and on this machine its consumers can
only be CPU-side. That is not nothing — it is a `wl_shm`-shaped path with no
copy — but it is not "Firefox can hardware-decode into it", and the difference
matters. See design.md §0.10.

## Re-running it

```
# guest half, from the dev shell
cargo build --release -p gpu-probe --target x86_64-pc-windows-gnu
scripts/pl.sh push target/x86_64-pc-windows-gnu/release/gpu-probe.exe
scripts/pl.sh root 'install -d -m0775 -g users /srv/winshare/svvkalloc &&
                    install -m0755 /tmp/gpu-probe.exe /srv/winshare/svvkalloc/'
scripts/pl.sh guest 'cd /z/svvkalloc && ./gpu-probe.exe --vk-alloc \
                     --workdir "Z:\svvkalloc" --pages 16384'      # holds, waits

# host half, as root, while the guest holds the allocations
QP=$(pgrep -f 'qemu-system-x86_64 -name windows-vm'); FD=35
arena-host scan    --fd /proc/$QP/fd/$FD --run <id> --out map.txt
arena-host udmabuf --fd /proc/$QP/fd/$FD --map map.txt --limit-mb 16
arena-host write1  --fd /proc/$QP/fd/$FD --map map.txt --stamp <hex>
echo stamp=<hex> > /srv/winshare/svvkalloc/host-vkalloc-fill.txt
```

The guest half releases as soon as the fill file appears, so it holds somebody's
laptop memory for seconds rather than minutes.
