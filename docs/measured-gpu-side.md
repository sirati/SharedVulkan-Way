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
awe-probe.exe --workdir "Z:\probe" --pages 16384

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
