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

## What this does not tell us

That the extension is advertised is not that an import of *our particular
pages* will succeed. Importing memory that is guest RAM shared with the host,
rather than ordinary process memory, is the actual experiment, and it has not
been run. What is now established is that the mechanism exists, is
page-granular, and does not need a fallback invented before the first line of
the arena is written.
