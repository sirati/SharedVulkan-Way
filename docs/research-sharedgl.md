# SharedGL architecture study

Source: `https://github.com/dmaivel/sharedgl`, cloned to `/tmp/sharedgl-study/sharedgl`.
HEAD at time of study: `297d05ab596b0b3c169748a99529f5dde8f343a5` ("Fix script name", 2026-04-19),
version `0.10.0` per `CMakeLists.txt:2`. 51 commits total. MIT.

Everything below is read from the source at that commit. Where the README and the code
disagree, the code is described and the disagreement is called out explicitly.

Throughout, remember the topology inversion that motivates this study:

- **sharedgl:** Linux host owns the GPU and is the *server*; Windows guest is the *client*.
- **us:** Windows guest owns the (VFIO-passed) Intel iGPU and must be the *server*;
  the GPU-less NixOS host is the *client*.

Section 7 collects the direction analysis. Sections 1–6 answer the questions as asked.

---

## 0. Executive summary

sharedgl is a **synchronous, batched, blocking RPC** for OpenGL over a flat shared-memory
window. It is not a ring buffer, has no fences, no pipelining, no interrupts, and no zero-copy
path. The design is:

1. Client accumulates an int32-granular command stream into a private buffer.
2. On any call that needs a return value — and on every buffer swap — the client memcpy's the
   whole batch into a *single global* staging FIFO, sets one global `SUBMIT` flag, and spins.
3. Server (busy-polling) memcpy's the batch out into its own private buffer, walks it with a
   824-case `switch`, calls real GL, writes return values into the client's 4 KiB mailbox,
   clears `SUBMIT`.
4. Presentation is `glReadPixels` into a shared framebuffer region, which the client then blits
   with `XPutImage`/`SetDIBitsToDevice`.

Its virtues for us: the transport substrate (ivshmem + polled registers + spinlocks) is genuinely
direction-agnostic and cheap to reimplement. Its vices: essentially every design decision above
the transport is wrong for Vulkan, and several are wrong for OpenGL too (see §5).

---

## 1. Transport

### 1.1 The device: ivshmem-plain — confirmed

Confirmed, and specifically **`ivshmem-plain`**, not `ivshmem-doorbell`.

`src/server/main.c:40-56` generates the VM config the user is told to paste:

- libvirt: `<shmem name="sharedgl_shared_memory"><model type="ivshmem-plain"/>` (`main.c:43-46`)
- qemu: `-object memory-backend-file,size=%ldM,share,mem-path=/dev/shm/sharedgl_shared_memory,...`
  (`main.c:48-49`)

The shared memory name is `sharedgl_shared_memory` (`inc/sharedgl.h:73`).

There are two host-side modes (`src/server/main.c:163-179`):

- shared memory (default): `shm_open(SGL_SHARED_MEMORY_NAME, O_CREAT|O_RDWR, S_IRWXU)`,
  `ftruncate` to size, `mmap(..., MAP_SHARED, ...)`.
- networking (`-n`): `mmap(MAP_ANON|MAP_SHARED)` — the *same* memory layout is built in
  anonymous memory and ENet is used to shuttle the FIFO and register page around. This is a
  useful structural detail: the layout is transport-independent by construction.

Guest side, three access paths:

| Guest | Path | Code |
|---|---|---|
| Linux (as VM guest) | `open()` a char device exposing PCI BAR2, then `mmap` | `src/client/memory.c:15-41`, `kernel/linux/sharedgl.c:44-54` |
| Linux (on the host itself, no VM) | plain `shm_open(SGL_SHARED_MEMORY_NAME)` | `src/client/glimpl.c:604-605` |
| Windows | ivshmem driver IOCTLs | `src/client/pb.c:134-194` |

`init_shm` tries `shm_open` first and falls back to scanning `/dev/sharedgl` for a file whose
name contains `bar2` (`glimpl.c:604-611`, `memory.c:35-36`). That fallback ordering is why the
same binary works both in a Linux guest and directly on the host.

The Windows client opens the ivshmem device by interface GUID
`df576976-569d-4672-95a0-f57e4ea0b210` (`pb.c:22-23`), issues `IOCTL_IVSHMEM_REQUEST_SIZE`
(0x801) and `IOCTL_IVSHMEM_REQUEST_MMAP` (0x802), and maps with
**`IVSHMEM_CACHE_WRITECOMBINED`** (`pb.c:170-178`). That cache choice matters for us — see §7.

### 1.2 Region layout

Not a ring buffer. A fixed-offset arena. From `inc/sharedgl.h:31-58` and the server's
initialisation at `src/server/processor.c:318-344`:

```
offset 0x00000                 global register page (4 KiB, only ~18 ints used)
offset 0x01000                 mailbox array: 16 slots x 4 KiB = 64 KiB
offset 0x11000 (SGL_STAGE_OFFSET)  staging FIFO, size = mem_size - 0x11000 - fb_size
offset 0x11000 + fifo_size     framebuffer, size = max_w * max_h * 4
```

`SGL_STAGE_OFFSET = SGL_MAILBOXES_OFFSET + SGL_MAILBOXES_SIZE = 0x1000 + 16*0x1000 = 0x11000`
(`sharedgl.h:52-55`). FIFO size and framebuffer start are computed at
`processor.c:319-320` and published at `processor.c:331`.

**Global registers** (all int32, `sharedgl.h:31-48`):

| Offset | Name | Written by | Meaning |
|---|---|---|---|
| 0 | `SUBMIT` | client set / server clear | the one global doorbell-by-polling flag |
| 4 | `RETVAL` | server | scalar return value |
| 8 | `READY_HINT` / `SHM_SLOT_MASK` | both | **aliased** — see §5 |
| 12 | `LOCK` | both | global spinlock word |
| 16 | `CLAIM_ID` | server | next client id (network path only) |
| 20 | `CONNECT` | client | "I want to connect", carries client id |
| 24 | `FBSTART` | server | u64 offset of framebuffer |
| 32 | `MEMSIZE` | server | u64 total region size |
| 40 | `SWAP_BUFFERS_SYNC` | clients | cross-process spinlock guarding present |
| 44 | `GLMAJ` | server | reported GL major |
| 48 | `GLMIN` | server | reported GL minor |
| 52 | `RETVAL_V` | server | start of the bulk return area (4044 bytes to end of slot) |
| 56 | `STAGE_CLIENT_ID` | client | who owns the current submission |
| 60 | `STAGE_SIZE` | client | byte length of the current submission |
| 64 | `FIFO_SIZE` | server | usable FIFO bytes |
| 68 | `MAX_CLIENTS` | server | 16 |

**Mailboxes.** Each client `id` (1..16) owns a 4 KiB slot at
`SGL_MAILBOXES_OFFSET + (id-1)*0x1000` (`sharedgl.h:56-58`). The slot uses the *same* register
offsets as the global page, so `pb_read(SGL_OFFSET_REGISTER_RETVAL)` transparently resolves to
either the global page or the client's slot depending on `pb_is_global_offset`
(`pb.c:62-89`). Return values land in the mailbox, control flags in the global page.

**Sizes.** Default `-m 32` MiB and `-r 1920x1080` ⇒ framebuffer 8,294,400 B (≈7.91 MiB),
FIFO ≈ 24 MiB. If `fifo_size` computes negative the server refuses to start
(`processor.c:322-325`). The README's "Blank window in the VM ⇒ the ivshmem `<size>` doesn't
cover what the server allocates" follows from the client blindly reading `MEMSIZE` and
`mmap`-ing that much (`pb.c:112-116`) without checking it against the BAR size.

### 1.3 Signalling: polling and spinlocks. No interrupts anywhere.

There is **no interrupt, no doorbell, no eventfd, no futex** in the entire data path. Both sides
busy-wait.

Server (`processor.c:169-211`): `wait_shm` loops forever, checking `CONNECT` for new clients and
`SUBMIT == 1` for work, with `_mm_pause()` as the only concession (`processor.c:206`, and the
comment on 203-205 is literally `/* some sort of "sync" */`).

Client (`glimpl.c:415-452`):

```c
spin_lock(lockg);
while (pb_global_read(SGL_OFFSET_REGISTER_SUBMIT) == 1) {   /* someone else is in flight */
    spin_unlock(lockg);
    while (pb_global_read(SGL_OFFSET_REGISTER_SUBMIT) == 1);
    spin_lock(lockg);
}
pb_write(SGL_OFFSET_REGISTER_SUBMIT, 1);       /* my mailbox flag */
pb_copy_to_shared();
pb_global_write(SGL_OFFSET_REGISTER_STAGE_CLIENT_ID, client_id);
pb_global_write(SGL_OFFSET_REGISTER_STAGE_SIZE, (int)submit_size);
pb_global_write(SGL_OFFSET_REGISTER_SUBMIT, 1);  /* global flag */
spin_unlock(lockg);
while (pb_read(SGL_OFFSET_REGISTER_SUBMIT) == 1);   /* block until server clears my mailbox */
```

The spinlock itself is a plain CAS on a shared int: client `src/client/spinlock.c:127-143`
(`__sync_bool_compare_and_swap` / `InterlockedCompareExchange`), server
`processor.c:24-34`. Both spin with `_mm_pause` / ARM `yield`.

The critical structural consequence: **`SUBMIT` is a single global slot.** Exactly one
submission from one client can be in flight system-wide. The 16 mailboxes give per-client
*return values*, not per-client *queues*. Everything serialises.

Client slot release is done under the same global lock by the server
(`processor.c:136-154`), clearing the slot and the bit in `SHM_SLOT_MASK`.

### 1.4 Guest kernel driver — what it does, and what userspace couldn't

**Linux** (`kernel/linux/sharedgl.c`, 272 lines): a minimal PCI char driver. It binds
`1af4:1110` (`sharedgl.c:204-212`), `ioremap`s all memory BARs (`sharedgl.c:91-105`), creates
one char device per BAR named `sharedgl/<bus>:<slot>.<fn>/bar<N>` (`sharedgl.c:128-141`,
`221-231`), and forces mode 0666 via a `dev_uevent` hook (`sharedgl.c:62-66`).

Its only real operation is `dev_mmap`, which `io_remap_pfn_range`s **BAR2, hardcoded**,
regardless of which minor was opened (`sharedgl.c:44-54`, note `pchar->bar[2].phys` on line 49).

*What could userspace not do?* Almost nothing. `/sys/bus/pci/devices/*/resource2` is mmap-able
from userspace already; the module exists to (a) give a stable, discoverable path and (b) hand
out a world-writable node so unprivileged apps can map the BAR. There is no interrupt handling,
no ioctl, no ring management, no DMA. **For a Rust reimplementation on Linux you do not need a
kernel module at all** if you're willing to grant access to the sysfs resource file (or run the
mapper with `CAP_SYS_RAWIO`); this module is a permissions shim.

**Windows** (`kernel/windows/`, ~1078 lines): this is Geoffrey McRae's / Red Hat's LookingGlass
ivshmem KMDF driver, vendored — `kernel/windows/LICENSE:1-2` credits
"Copyright 2017-2022 Geoffrey McRae / Red Hat". sharedgl's fork ("ksgldrv") relaxes exactly one
thing: the single-mapping restriction. `kernel/windows/Queue.c:245-247` shows the
`STATUS_DEVICE_ALREADY_ATTACHED` check commented out, which is what allows multiple client
processes to map the region simultaneously. That is the same effect the README's `ntoseye`
runtime-patch route achieves against the stock driver.

Here Windows genuinely needs a driver: there is no user-mode path to a PCI BAR.

### 1.5 Doorbell support exists in the driver and is completely unused

The vendored Windows driver has full doorbell and MSI-X interrupt support:

- `kernel/windows/Public.h:64-70`: `IOCTL_IVSHMEM_REQUEST_PEERID`, `RING_DOORBELL` (0x804),
  `REGISTER_EVENT` (0x805).
- `kernel/windows/Queue.c:365-401`: `ioctl_ring_doorbell` →
  `WRITE_REGISTER_ULONG(&devRegisters->doorbell, vector | (peerID << 16))`.
- `kernel/windows/Queue.c:403+`: `ioctl_register_event`, max 32 events (`Public.h:49`).
- `kernel/windows/Device.c:119-222`: MSI-X resource enumeration and `WdfInterruptCreate`;
  ISR/DPC at `Device.c:327-357`.

sharedgl's userspace **never issues either IOCTL**. `src/client/pb.c:29-31` only defines
`REQUEST_SIZE`, `REQUEST_MMAP`, `RELEASE_MMAP`. Grep confirms no `RING_DOORBELL` or
`REGISTER_EVENT` call site outside the driver.

Combined with the fact that the *only* configuration the server emits is `ivshmem-plain`
(`main.c:43-49`) — which has no BAR0 register block and therefore no doorbell at all — the
honest statement is: **sharedgl demonstrates nothing about interrupts. It is a pure polling
design.** See §7.5 for what that means for us and what I could not determine.

---

## 2. Command protocol

### 2.1 Encoding

A flat, variable-length, **int32-granular** stream. One opcode int followed by inline arguments,
also int-sized. Terminator is `SGL_CMD_INVALID == 0`.

- Client pushes: `pb_push(int)` (`pb.c:227-234`), `pb_pushf(float)` — reinterprets the bits,
  no conversion (`pb.c:247-254`), `pb_push64(int64)` — `memcpy` then advance 2 ints
  (`pb.c:236-245`), `pb_memcpy` for bulk with 4-byte round-up (`pb.c:300-309`),
  `pb_memcpy_unaligned` + `pb_realign` for strided gathers (`pb.c:311-335`).
- Client terminates: `pb_push(0)` at the top of `glimpl_submit` (`glimpl.c:483`).
- Server walks: `while (*pb != SGL_CMD_INVALID) { switch (*pb++) { ... } }`
  (`processor.c:424-429`, closing at `processor.c:6231`).

The opcode space is a single positional enum, `SGL_CMD_INVALID` through `SGL_CMD_MAX`, spanning
`inc/sharedgl.h:103-1203` — roughly 1100 opcodes. `src/server/processor.c` implements 824 `case`
labels.

Doubles are pushed as floats in several places (`glimpl.c:2464-2469` `glMultMatrixd`) — see §5.

**Strings** use two incompatible conventions:

- Counted: `push_counted_string` (`glimpl.c:402-407`) writes `len` then the bytes;
  server reads via `read_counted_strings` (`processor.c:42-49`) or
  `read_counted_c_strings` which re-materialises NUL-terminated copies into a scratch buffer
  (`processor.c:51-73`).
- In-place NUL-scan: the `ADVANCE_PAST_STRING()` macro (`processor.c:36-40`) scans forward
  int-by-int checking each of the 4 bytes for zero. Used by e.g.
  `SGL_CMD_BINDFRAGDATALOCATION` (`processor.c:527-533`) and
  `SGL_CMD_GETUNIFORMLOCATIONARB` (`processor.c:6135-6141`).

### 2.2 Return values: fully synchronous, no pipelining, no fences

There is no fence, no sequence number, no completion queue. Any GL entry point with a return
value does: push opcode → `glimpl_submit()` (which blocks until the server has drained the
*entire* batch) → read `RETVAL`. Examples:

- `glimpl_get_buffer_parameter` (`glimpl.c:1408-1416`)
- `glIsEnabled` (`glimpl.c:2755-2762`)
- `glimpl_unmap_buffer` (`glimpl.c:1468-1484`)
- `shm_create_context` reading packed max dimensions (`glimpl.c:671-675`)

Void-returning calls just accumulate; the batch is flushed by the next value-returning call or
by `SwapBuffers`. So the effective batching granularity is "whatever falls between two `glGet*`
calls", which for a badly-written app is one call.

Return channel is the client's mailbox: scalar in `RETVAL`, bulk in `RETVAL_V`. The server
copies the whole tail of its private register page into the client's slot after the batch
(`processor.c:6239-6249`):

```c
memcpy(client_slot + SGL_OFFSET_REGISTER_RETVAL,
       (char*)p + SGL_OFFSET_REGISTER_RETVAL,
       SGL_CLIENT_SLOT_SIZE - SGL_OFFSET_REGISTER_RETVAL);
*(int*)(client_slot + SGL_OFFSET_REGISTER_SUBMIT) = 0;   /* release the waiter */
```

`RETVAL_V` capacity is 4096 − 52 = 4044 bytes; `inc/sharedgl.h:60-62` documents this.

**Errors are not propagated at all.** The server drains `glGetError` after every single command
and only *logs* it (`processor.c:6226-6230`). The client's `glGetError()` unconditionally
returns `GL_NO_ERROR` (`glimpl.c:2745-2748`). Applications are structurally blind to GL errors.

There is no explicit GPU sync at end-of-batch — `glFinish()` is commented out at
`processor.c:6236`. The client is released as soon as the commands have been *issued*, not
completed. Fine for GL, meaningless for Vulkan.

### 2.3 Context and state

- Server creates **one SDL hidden window + one GL context per connected client**:
  `connection_add` (`processor.c:102-108`) calls `sgl_context_create`
  (`src/server/context.c:23-73`). The window is `SDL_WINDOW_HIDDEN` at max resolution
  (`context.c:45-50`) and exists purely to own a context.
- Before executing each batch the server makes that client's context current
  (`processor.c:415-418` → `sgl_set_current`, `context.c:87-98`).
- Context is therefore **per client process**, not per GL context. The client's
  `glXCreateContext` / `DrvCreateContext` are local bookkeeping; `src/client/platform/glx.c`
  keeps a struct with `major`/`minor`/`profile_mask`/etc. that is only ever queried back
  locally (`glx.c:1100-1151`).
- The client also maintains a shadow of a good deal of GL state so it can emulate
  client-side arrays and pixel-store: `glimpl_vaps[GLIMPL_MAX_OBJECTS]`, colour/tex/normal/edge
  pointers, `glimpl_unpack_store`/`glimpl_pack_store` (structs at `glimpl.c:80-130`,
  pixel-store handling at `glimpl.c:960-1020`).

### 2.4 Handshake and versioning

There is essentially none, and this is a real gap.

Shared-memory attach sequence:

1. Server, at startup, zeroes the mailboxes and FIFO and writes every register
   (`processor.c:328-344`), including `MEMSIZE`, `FIFO_SIZE`, `GLMAJ`, `GLMIN`, `FBSTART`,
   `MAX_CLIENTS = 16`.
2. Client maps 4 KiB, reads `MEMSIZE` and `FIFO_SIZE`, unmaps, remaps the full size
   (`pb.c:111-116`).
3. Client takes the global lock, scans `SHM_SLOT_MASK` for a free bit, sets it, and takes that
   slot id (`glimpl.c:619-655`).
4. Client writes its id into `CONNECT` and releases the lock (`glimpl.c:660-666`).
5. Server observes `CONNECT != 0` inside `wait_shm`, creates a context, zeroes `CONNECT`
   (`processor.c:178-198`).
6. Client submits `SGL_CMD_CREATE_CONTEXT` and reads back `PACK(width, height)`
   (`glimpl.c:671-675`, server side `processor.c:433-435`).

No magic number, no protocol version, no capability bits. The opcode enum is *positional*, so a
client and server built from different commits will silently misinterpret the stream — the
"Crashes like `IOT instruction`" entry in the README's troubleshooting is consistent with that
class of failure. `SGL_CMD_HELLO_WORLD` exists in the enum (`sharedgl.h:116`) and is never
emitted or handled.

Networking has a marginally better handshake — a `struct sgl_packet_connect` carrying
`client_id`, `framebuffer_size`, `fifo_size`, `gl_major`, `gl_minor`, `max_width`, `max_height`
(`inc/network/packet.h:17-25`, sent at `processor.c:213-234`). Still no version field.

---

## 3. Buffer and memory handling

This is the section that matters most for us, and the answer is blunt: **sharedgl is
copy-everything. There is no zero-copy path anywhere.**

### 3.1 The copy chain

A single texture upload traverses:

```
app's buffer
  → pb_memcpy into the client's PRIVATE anonymous buffer   (pb.c:300-309, buffer from pb.c:129)
  → pb_copy_to_shared: memcpy into the shared staging FIFO (pb.c:381-385)
  → server memcpy out of shared memory into its OWN private heap buffer (processor.c:406-409)
  → glTexImage2D(..., uploaded)                            (processor.c: e.g. 553-559)
  → GPU
```

Three CPU copies before the driver even sees it, one of which crosses the PCI BAR.

The server's defensive copy is deliberate — `shared_exec` is a `calloc`'d buffer of
`SGL_OFFSET_COMMAND_START + fifo_size` (`processor.c:350-355`) and the batch is copied into it
before execution (`processor.c:406-409`) so the client cannot mutate the stream mid-walk.

The client's copy is **avoidable and the code knows it**. `pb_set(fd, direct_access)` supports
writing straight into the shared FIFO (`pb.c:124-127`):

```c
if (direct_access) { in_base = base; in_cur = base; }
else               { in_base = mmap(NULL, in_capacity, ..., MAP_PRIVATE|MAP_ANONYMOUS, -1, 0); }
```

but `glimpl_init` calls `init_shm(false)` (`glimpl.c:768`), so direct access is **never
enabled**, and `inc/client/pb.h:26-28` carries `to-do: determine if direct_access is really
unneeded`. My read: with `WRITECOMBINED` mapping on Windows, sequential writes straight into the
BAR would actually be the fast path, and this is dead performance left on the table.

### 3.2 How pointer arguments are passed: `VP_UPLOAD`

The mechanism for every `const void*` argument in GL. `glimpl_upload_buffer`
(`glimpl.c:1022-1027`):

```c
pb_push(SGL_CMD_VP_UPLOAD);
pb_push(CEIL_DIV(size, 4));      /* length in ints */
pb_memcpy(data, size);           /* payload inline in the stream */
```

Server side (`processor.c:467-473`) does not copy — it records a pointer into its private buffer
and skips the payload:

```c
case SGL_CMD_VP_UPLOAD: {
    int vp_upload_count = *pb++;
    uploaded = pb;
    for (int i = 0; i < vp_upload_count; i++) pb++;
    break;
}
```

The *next* GL command consumes `uploaded` as its pointer argument, e.g.
`glBufferData(target, size, use_uploaded ? uploaded : NULL, usage)` (`processor.c:553-559`),
`glBindBuffersBase(..., uploaded)` (`processor.c:514-519`), `glBitmap(..., uploaded)`
(`processor.c:537-546`). `SGL_CMD_VP_NULL` sets it to `NULL` for the `pixels == NULL` case
(`glimpl.c:1052-1055`, `processor.c:484-487`).

Strided client arrays are gathered element-by-element into the stream with
`pb_memcpy_unaligned` in a loop then re-aligned (`glimpl.c:1062-1088`) — a per-vertex memcpy on
the client for every draw that uses client-side arrays.

### 3.3 The hard size cliff

The **entire batch** must fit in the FIFO. Both ends check, and both ends respond by
**silently dropping the whole batch**:

- Client: `pb_can_write` (`pb.c:91-95`) refuses the write and sets `write_overflowed`
  (`pb.c:97-103`); `glimpl_submit` then logs once and discards everything
  (`glimpl.c:485-493`). `pb_copy_to_shared` is a no-op when overflowed (`pb.c:383`).
- Client, second check: `submit_shm` rejects `submit_size > fifo_size` (`glimpl.c:420-427`).
- Server: rejects oversized submits and clears the client slot (`processor.c:398-404`).

With the default 32 MiB that means a ~24 MiB ceiling on *the sum of everything between two
flushes*. A single 4K RGBA8 texture is 33 MiB and simply cannot be uploaded. This is exactly why
the README says to try `-m 256`.

### 3.4 Readback: `VP_DOWNLOAD` — the worst cliff in the design

Getting data *back* is a round-trip per 3 KiB. `SGL_VP_DOWNLOAD_BLOCK_SIZE_IN_BYTES` is 3072
(`sharedgl.h:63-64`). `glimpl_download_buffer` (`glimpl.c:1029-1048`):

```c
for (int i = 0; i < blocks; i++) {
    pb_push(SGL_CMD_VP_DOWNLOAD);
    pb_push(true_count);
    glimpl_submit();                                   /* FULL blocking round-trip */
    memcpy((char*)dst + i*3072, retval_v, true_count);
}
```

Server side just advances a cursor over a stashed pointer (`processor.c:488-493`):

```c
case SGL_CMD_VP_DOWNLOAD: {
    int length = *pb++;
    memcpy(p + SGL_OFFSET_REGISTER_RETVAL_V, download_target + download_offset, length);
    download_offset += length;
}
```

`download_target` is set by whatever produced the data: `glMapBufferARB`
(`processor.c:6146-6152`), `glGetTexImage` into a scratch buffer (`processor.c:783-793`).

**A 4 MiB buffer map costs ~1365 full client↔server blocking round-trips.** Each round-trip is a
global-lock acquisition plus two spin-waits. This is catastrophic and it is the mechanism behind
"glMapBuffer is slow" in every report about this project.

### 3.5 `glMapBuffer` is emulated by download-then-full-reupload

`glimpl_map_buffer_range` (`glimpl.c:1418-1466`): if `length == 0` (i.e. `glMapBuffer`), it first
does a *separate* round-trip to fetch `GL_BUFFER_SIZE` (`glimpl.c:1429-1434`), allocates a local
scratch buffer (`glimpl.c:1438`, `src/client/scratch.c:200-225` — an `mremap`-growable arena),
submits the map command, then downloads the entire range.

`glimpl_unmap_buffer` (`glimpl.c:1468-1484`) re-uploads **the whole buffer**, unconditionally,
with no dirty tracking. `glimpl_flush_mapped_buffer_range` (`glimpl.c:1486-1494`) is the only
ranged variant.

There is exactly **one** map slot (`glimpl_map_buffer`, a single global struct) and it
mismatches loudly if you map two buffers at once (`glimpl.c:1470-1473`). The overlapping-map
guard is commented out at `glimpl.c:1421-1424`.

No persistent mapping, no coherent mapping, no `GL_MAP_UNSYNCHRONIZED_BIT` handling.

### 3.6 Presentation

Yes — the framebuffer comes back through shared memory, and it costs a lot.

Client (`glimpl.c:558-566`) pushes `SGL_CMD_SWAP_BUFFERS(w, h, vflip, format)` and submits.
Server (`processor.c:456-466`):

```c
void *framebuffer_target = (char*)shared + (*(uint64_t*)((char*)shared + SGL_OFFSET_REGISTER_FBSTART));
sgl_read_pixels(w, h, framebuffer_target, vflip, format, (size_t)pb - (size_t)cmd_base);
```

`sgl_read_pixels` (`src/server/context.c:100-131`) does:

1. `overlay_stage1` (FPS timing).
2. `glReadPixels(0, 0, mw, height, format, GL_UNSIGNED_BYTE, data)` — note **`mw`, the
   configured max width, not the actual width** (`context.c:106`). The framebuffer stride is
   permanently the max resolution.
3. A CPU vertical flip, pixel by pixel, if requested (`context.c:109-122`).
4. `overlay_stage2` — software glyph blit of the FPS counter directly into the pixel buffer
   (`src/server/overlay.c:8-29`, bitmap font in `inc/server/overlay_font.h`).

Client then blits from the shared region:

- Linux/GLX: an `XImage` is constructed *directly over* `glimpl_fb_address()` (`glx.c:1170-1181`)
  and `XPutImage`'d (`glx.c:1200`), followed by `XSync`.
- Windows ICD: `SetDIBitsToDevice(hdc, ..., framebuffer, &bmi, DIB_RGB_COLORS)`
  (`src/client/platform/windrv.c:774`), with `biHeight = -max_height` for top-down
  (`windrv.c:764`).

`glimpl_fb_address()` returns a pointer straight into the shared region (`glimpl.c:591-597`), so
the *final* blit is at least reading the shared memory directly rather than copying it again.

**Cost per frame:** a synchronous `glReadPixels` (which stalls the GPU pipeline), a full-frame
write across the BAR, optionally a full-frame CPU flip, then a full-frame read by the guest for
the blit. 1920×1080×4 = 8.3 MB. At 60 fps that is ~500 MB/s of BAR traffic for present alone,
plus the pipeline stall. There is no damage tracking, no double buffering (**one** framebuffer
region shared by all 16 clients), no format negotiation — `GL_BGRA` is hardcoded at both
`glx.c:1199` and `windrv.c:773`.

Because there is one framebuffer for everyone, present is serialised across processes by a
dedicated cross-process spinlock, `SGL_OFFSET_REGISTER_SWAP_BUFFERS_SYNC`
(`glx.c:1198-1201`, `windrv.c:771-777`).

Network mode compresses the framebuffer with LZAV before sending (`processor.c:236-247`) and
decompresses client-side (`glimpl.c:568-581`). Shared-memory mode does not compress.

---

## 4. Hooking

### 4.1 Linux: a replacement `libGL.so.1`, not an ICD

`CMakeLists.txt:40-47` builds `sharedgl-core` as a shared library with
`OUTPUT_NAME "GL"` and `VERSION 1`, i.e. `libGL.so.1`, plus post-build symlinks
`libOpenGL.so.0` and `libGLX.so.0`. It links `X11`.

It is loaded by putting its directory on `LD_LIBRARY_PATH` (README), optionally `LD_PRELOAD` for
apps that resolve entry points oddly (the README's GLFW note). It is **not** a Mesa/GLVND ICD —
it does not implement `__GLXGLVNDVendor` or the `libglvnd` dispatch contract, it simply exports
`gl*` and `glX*` symbols by name and wins the link.

Init is via ELF constructor (`src/client/main.c:7-13`) calling `glimpl_init()` then
`glximpl_init()`; teardown via destructor calling `glimpl_goodbye()` (`main.c:15-18`).

`glXGetProcAddressARB` (`glx.c:1206-1223`) looks in a builtin table, then `dlsym` on itself,
then `RTLD_DEFAULT`.

### 4.2 Windows: a genuine Microsoft ICD

`scripts/wininstall.bat` copies `sharedgl64.dll` → `%windir%\System32\sharedgl.dll` and
`sharedgl32.dll` → `SysWOW64` (lines 20-22), then registers it under
`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\OpenGLDrivers\MSOGL` with
`DLL`, `DriverVersion`, `Flags`, `Version` (lines 12-13, 24-31), and the WOW6432Node mirror.

Stock `opengl32.dll` then calls the `Drv*` entry points, all in
`src/client/platform/windrv.c`: `DrvCreateContext` (641), `DrvCreateLayerContext` (650),
`DrvDeleteContext` (659), `DrvCopyContext` (664), `DrvSetContext` (672), `DrvReleaseContext`
(684), `DrvValidateVersion` (690), `DrvShareLists` (695), `DrvGetProcAddress` (702),
`DrvSetCallbackProcs` (717), `DrvDescribeLayerPlane` (725), palette entries (730/735/740),
`DrvSwapBuffers` (745), `DrvPresentBuffers` (782), `DrvSwapLayerBuffers` (787),
`DrvDescribePixelFormat` (795), `DrvSetPixelFormat` (812).

The GL 1.1 proc table required by `DrvSetContext` comes from `glimpl_GetProcTable()`
(`inc/client/glimpl.h:18-21`, table around `glimpl.c:11400+`), using Microsoft's
`PGLCLTPROCTABLE` layout vendored at `inc/client/platform/gldrv.h` (593 lines, Microsoft
copyright at lines 1-22).

`DllMain` (`src/client/winmain.c:27-57`) calls `glimpl_init()` on attach and `glimpl_goodbye()`
on detach, and applies the two env-var behaviours (`SGL_RUN_WITH_LOW_PRIORITY`,
`SGL_WINED3D_DONT_VFLIP`).

`src/client/platform/wgl.c` is a **dead** `opengl32.dll` replacement behind
`#ifdef OVERRIDE_OPENGL32`; its own header comment (`wgl.c:1-13`) says the ICD path made it
redundant, that MinHook is being removed, and that "even if the end-user defines
`OVERRIDE_OPENGL32`, this code will not compile."

`src/client/platform/egl.c` and `inc/client/platform/egl.h` are each a **single line**:
`// stub`. There is no EGL support.

### 4.3 Generated vs hand-written: 100% hand-written

**There is no code generation in this repository.** No script, no XML, no build step consuming
`gl.xml`. `inc/commongl.h` (6797 lines) is a hand-vendored flat GL header, not a generated one.

That means four parallel lists are maintained **by hand** and must stay in sync:

1. `SGL_CMD_*` positional enum — `inc/sharedgl.h:103-1203` (~1100 entries).
2. Client entry points — `src/client/glimpl.c`, ~1201 exported `gl*`/`glX*`/`wgl*` functions.
3. Server dispatch — `src/server/processor.c`, 824 `case` labels.
4. Opcode→string debug table — `src/server/sgldebug.c` (1106 lines).

This is the dominant cost of the project and the most obvious thing to replace with codegen. For
a Vulkan analogue the equivalent registry is `vk.xml`, which is far better structured than
`gl.xml` for this purpose (explicit `len=`, `optional=`, `externsync=`, structextends chains) —
a Rust build script over `vk.xml` (or the `vk-parse`/`ash` ecosystem) should generate all four
lists. **Do not hand-write the dispatch.**

---

## 5. What it does not do — specific citations

### Outright stubs

| Thing | Where | Note |
|---|---|---|
| `glReadPixels` | `glimpl.c:2750-2753` | body is `STUB();` which is `#define STUB()` → nothing (`glimpl.c:78`). The `SGL_CMD_READPIXELS` opcode exists and is never emitted. |
| `glGetError` | `glimpl.c:2745-2748` | always returns `GL_NO_ERROR` |
| `glGetSynciv` | `glimpl.c:8583` | commented "sync is stub" |
| `glGetBufferPointerv` | `glimpl.c:9946` | "stub" |
| `glGetBufferSubData` | `glimpl.c:9955` | "stub" |
| `glGetTexImage` family | `glimpl.c:10111`, `10119` | "stub, refer to ..." |
| misc | `glimpl.c:10255`, `10262` | "stub" |
| EGL | `src/client/platform/egl.c:1` | file is `// stub` |
| WGL/opengl32 override | `src/client/platform/wgl.c:1-13` | dead, does not compile |
| Server-side no-ops | `processor.c:6027-6029` (`GETNMINMAX`) and neighbours | empty `case` bodies |

### Precision and correctness

- Doubles discarded: `processor.c:946` `/* to-do: don't discard doubles */`;
  `glimpl.c:8832` "to-do: support doubles"; `glMultMatrixd` pushes floats
  (`glimpl.c:2464-2469`); `glDepthRange` pushes floats (`glimpl.c:2764-2769`).
- `glimpl.c:1405`: `pb_push(*(unsigned int*)data); // to-do: technically should look at type
  first, dont care` — clear-buffer-data ignores the declared type.
- `glimpl.c:2242`: "to-do: check if this is causing an issues (overreading)".
- `glimpl.c:7203`: "to-do: 2 functions below are probably wrong".
- `glimpl.c:7134`: "to-do: perhaps fix the three functions below".
- `glimpl.c:1864`: "to-do: check if mode and type is valid".
- `glimpl.c:8299`: "to-do: should glDelete* end w/ submit?".

### Hacks the author flags

- **WineD3D viewport guess**: `glScissor` calls `icd_resize` whenever the scissor is at the
  origin and bigger than 64×64, because WineD3D never calls `glViewport`
  (`glimpl.c:2635-2639`). Comment: `/* icd hack for WineD3D bc it doesn't appear to call
  glViewport */`.
- **Priority hack**: `SGL_RUN_WITH_LOW_PRIORITY` sets `IDLE_PRIORITY_CLASS` —
  `src/client/winmain.c:32-40`, comment: "some weird hack that can speed up processes... by
  setting the priority of the program to 'low', we give the kernel more CPU time". This is a
  symptom of the busy-wait design starving the vCPU.
- **Goodbye path**: `glimpl.c:409-413` `/* temporary solution to submit stalling out at recv
  when submitting the goodbye message */` — the `expecting_retval` global.
- **vflip on CPU** with a "to-do: fix overlay so vflip and -Height won't be needed"
  (`glx.c:1199`, `windrv.c:773`).

### Fixed limits

- `SGL_MAX_CLIENTS 16` (`sharedgl.h:51`), further capped at 32 by the slot bitmask width
  (`glimpl.c:624-625`).
- `GLIMPL_MAX_OBJECTS 256`, `GLIMPL_MAX_TEXTURES 8`, `GLIMPL_MAX_COUNT_FOR_MATRIX_OP 256`
  (`glimpl.c:28-30`).
- One `glMapBuffer` in flight per client (`glimpl.c:1468-1473`).
- One framebuffer for all clients (`processor.c:331`).
- One in-flight submission globally (`processor.c:200`, `glimpl.c:434-450`).

### Safety

- **`VP_DOWNLOAD` has no bounds check** (`processor.c:488-492`): `length` is read straight from
  the guest-supplied stream and used as a `memcpy` size into a 4044-byte window, from
  `download_target + download_offset` which is also guest-influenced. A malicious or buggy guest
  gets an arbitrary-length copy in the host server's address space. Same class of trust applies
  to the inline argument counts throughout (`processor.c:467-472`, `6178-6186`).
- The server installs a `SIGSEGV` handler whose job is to print which opcode was executing
  (`src/server/main.c:58-76`) — the author expects segfaults as a normal failure mode.
- The command-line parser installs a *different* `SIGSEGV` handler to catch missing arguments
  (`main.c:78-82, 96`) rather than checking `argc`.

### Dead / inconsistent

- `SGL_OFFSET_REGISTER_READY_HINT` is **aliased over** by `SGL_OFFSET_REGISTER_SHM_SLOT_MASK` —
  same offset (`sharedgl.h:33-34`). The server initialises it to 0 as `READY_HINT`
  (`processor.c:337`) and it then serves as the slot bitmask. There is *no* readiness flag.
- `is_value_likely_an_offset()` (`sharedgl.h:93-99`) — a heuristic that treats any pointer below
  1 MiB as an offset. Defined, never called anywhere in the tree.
- `glimpl_report()` is declared (`inc/client/glimpl.h:9`) and called only from dead code
  (`wgl.c:91`). **It has no definition anywhere.**
- `SGL_CMD_FLIP` — "USE SGL_CMD_SWAP_BUFFERS INSTEAD" (`sharedgl.h:105`).
- `SGL_CMD_REPORT_DIMS` — "NOT USED" (`sharedgl.h:112`), yet the server implements it
  (`processor.c:449-455`).
- `SGL_CMD_HELLO_WORLD`, `SGL_CMD_SEND_DATA`, `SGL_CMD_GET_CONTEXT`, `SGL_CMD_SET_CONTEXT`,
  `SGL_CMD_GET_SIZE`, `SGL_CMD_VP_UPLOAD_STAY` — enum entries that are unused or nearly so
  (`VP_UPLOAD_STAY` is implemented server-side at `processor.c:475-483` with the comment
  "unused currently so no free" — it leaks).
- **Network `retval_v` is 256 bytes** (`inc/network/packet.h:32`, `processor.c:6259`) while
  download blocks are **3072 bytes** (`sharedgl.h:63`). The network transport therefore cannot
  correctly service `glMapBuffer`/`glGetTexImage` readback. The comment at `sharedgl.h:60-62`
  ("max return in RETVAL_V is 4044") describes only the shared-memory path.
- `ENET_EVENT_TYPE_DISCONNECT_TIMEOUT` unhandled, `// to-do` (`processor.c:270-272`).
- Server-side "possibly optimize by just setting these to point into pb" appears three times
  (`processor.c:5514`, `5547`, `5923`).

---

## 6. Code size and shape

`find | xargs wc -l` over `.c`/`.h`/build files, 41,980 lines total. Vendored third-party
material is a large fraction.

### First-party

| Component | File(s) | LOC |
|---|---|---|
| Client GL implementation | `src/client/glimpl.c` | 11,495 |
| Server command dispatch | `src/server/processor.c` | 6,274 |
| Protocol / opcode enum | `inc/sharedgl.h` | 1,206 |
| GLX platform layer | `src/client/platform/glx.c` + `.h` | 1,370 |
| Opcode→string debug table | `src/server/sgldebug.c` | 1,106 |
| Windows ICD platform layer | `src/client/platform/windrv.c` | 825 |
| Linux kernel module | `kernel/linux/sharedgl.c` | 272 |
| Server main / arg parsing | `src/server/main.c` | 201 |
| Client transport (`pb`) | `src/client/pb.c` + `pb.h` | 453 |
| Server GL context (SDL) | `src/server/context.c` + `.h` | 149 |
| Dead WGL override | `src/client/platform/wgl.c` | 128 |
| FPS overlay | `src/server/overlay.c` | 95 |
| Scratch arena | `src/client/scratch.c` | 75 |
| Intrusive list | `src/server/dynarr.c` | 66 |
| Windows DllMain | `src/client/winmain.c` | 42 |
| BAR discovery | `src/client/memory.c` | 41 |
| Spinlock | `src/client/spinlock.c` | 30 |
| Linux constructor | `src/client/main.c` | 17 |
| Build + packaging scripts | `CMakeLists.txt`, `*.bat`, `*.sh` | ~290 |
| **First-party subtotal** | | **≈24,300** |

### Vendored

| Component | File | LOC | Origin |
|---|---|---|---|
| Flat GL header | `inc/commongl.h` | 6,797 | hand-assembled GL headers |
| ENet (single-header) | `inc/network/enet.h` | 6,275 | ENet |
| LZAV compressor | `inc/lzav.h` | 2,411 | LZAV |
| Windows ivshmem driver | `kernel/windows/*` | 1,078 | LookingGlass (`kernel/windows/LICENSE:1-2`) |
| Microsoft ICD header | `inc/client/platform/gldrv.h` | 593 | Microsoft |
| Bitmap font | `inc/server/overlay_font.h` | 266 | IBM VGA font |
| WDK CMake finder | `kernel/windows/findwdk/FindWdk.cmake` | 206 | findwdk |
| **Vendored subtotal** | | **≈17,600** | |

### Shape

```
inc/
  sharedgl.h            protocol: register offsets, opcode enum, packing macros
  commongl.h            vendored GL types/enums/prototypes
  client/{pb,glimpl,memory,scratch,spinlock}.h
  client/platform/{glx,wgl,egl,windrv,icd,gldrv}.h
  server/{processor,context,overlay,dynarr,overlay_font}.h
  network/{enet,packet}.h
src/
  client/{main,winmain,glimpl,pb,memory,scratch,spinlock}.c
  client/platform/{glx,wgl,egl,windrv}.c
  server/{main,processor,context,overlay,dynarr,sgldebug}.c
kernel/
  linux/sharedgl.c      BAR2 exposer
  windows/              LookingGlass ivshmem KMDF fork
scripts/                Windows install/uninstall/cert, ksgldrv.inf
```

Build targets (`CMakeLists.txt`): `sglrenderer` (Linux-only server, links `SDL2` + `epoxy`,
lines 33-36), `sharedgl-core` (client lib, both platforms, lines 39-62), `ksgldrv` (Windows
driver, lines 65-71).

### Rust effort estimate for a Vulkan analogue

The 17.6k vendored lines are irrelevant (Rust has `ash`, `enet`-alikes, `lz4_flex`). Of the
24.3k first-party lines:

- **~19k lines (glimpl.c + processor.c + the enum + sgldebug.c) should be generated**, not
  written. In Rust, a build script over `vk.xml` producing serialise/deserialise/dispatch for
  the Vulkan command set. Hand-written: the codegen itself (~2–4k lines including the
  hand-tuned exceptions), plus the exception list for commands whose pointer/length semantics
  the registry doesn't fully express (`pNext` chains, `VkWriteDescriptorSet` unions,
  `vkCmdPushConstants`, sparse binding).
- **Transport core: ~1–2k lines.** Region layout, ring/mailbox, spinlocks, attach/handshake.
  This is the part worth designing carefully and it is small.
- **Platform: ~1–2k lines per side.** Windows: BAR mapping via the ivshmem driver +
  headless Vulkan device + present-to-shared-FB. Linux: `/dev/shm` or sysfs BAR mapping +
  a Wayland/X11 presenter.
- **A Vulkan ICD on the Linux side** is a much better-defined target than sharedgl's
  "replace libGL": you write a `VkNegotiateLayerInterfaceVersion`/`vk_icdGetInstanceProcAddr`
  entry point plus a JSON manifest in `/usr/share/vulkan/icd.d/`. Nix packaging of that is
  straightforward. Budget ~500 lines of glue.

So: roughly **6–10k hand-written Rust lines plus a generator**, versus sharedgl's 24k
hand-written C. The saving is entirely in refusing to hand-write dispatch.

---

## 7. Direction analysis: what mirrors and what must be redesigned

### 7.1 Direction-agnostic (mirror it)

**The shared region itself.** `ivshmem-plain` is a plain block of host memory: the host mmaps
`/dev/shm/<name>` (`main.c:164-175`), the guest maps BAR2 of the PCI device
(`kernel/linux/sharedgl.c:44-54` / `pb.c:170-178`). Both are ordinary read/write mappings of the
same physical pages. Nothing in the memory contents encodes which side is which. **Confirmed
symmetric in practice, for data.**

**The register-page + spinlock + polling protocol.** The client's `spin_lock`
(`spinlock.c:127-143`) and the server's `sgl_spin_lock` (`processor.c:24-34`) are the same CAS on
the same shared word, compiled twice. Fully symmetric.

**The command-stream encoding.** Opcode + inline int args + zero terminator. Nothing
direction-specific.

**The staging FIFO and mailbox array.** Nothing about their layout privileges either side.

**The Windows ivshmem driver.** `ioctl_request_mmap` (`Queue.c:236-339`) just maps the BAR; it
neither knows nor cares whether the mapping process is producer or consumer. In our topology
the Windows side becomes the *server* and uses the identical driver.

### 7.2 Baked in "guest is client, host is server" (redesign)

**(a) Discovery is asymmetric and hardcoded — but shallowly.**
The server *only* knows how to `shm_open` a host `/dev/shm` name (`main.c:164`). The client
*only* knows how to find a BAR (`memory.c:15-41`, `pb.c:134-194`) with `shm_open` as a fallback
(`glimpl.c:604-606`). For us these must swap: the **server** finds the BAR (inside Windows), the
**client** opens `/dev/shm` (on the host). This is ~80 lines of code moving between two files.
Not architectural. Note the client's fallback ordering already proves both halves exist in one
binary.

**(b) Region initialisation belongs to the GPU owner, and there is no readiness handshake.**
`processor.c:328-344` has the server memset the mailboxes and FIFO and write every register
before any client attaches. The client is a pure reader of those registers
(`pb.c:112-113`, `glimpl.c:772-773`). In our inversion the **guest must start first** and
populate the region, and the host client must wait for it.

sharedgl has no way to express "not ready yet": `SGL_OFFSET_REGISTER_READY_HINT` was evidently
intended for exactly this and has been **aliased away** by `SGL_OFFSET_REGISTER_SHM_SLOT_MASK`
(`sharedgl.h:33-34`). A host client attaching to an uninitialised (all-zero, freshly
`ftruncate`d) region would read `MEMSIZE == 0` and `mmap(0)`.

**You must add a real readiness word: a magic value + protocol version + generation counter,
written last with a release fence.** This matters more for us than for sharedgl because the
guest can reboot underneath the host client at any time, whereas sharedgl's server outlives its
clients by construction. Add a generation counter so the host client can detect
"guest rebooted, everything I know is stale".

**(c) The framebuffer direction happens to be the one we want.**
`sgl_read_pixels` writes into shared memory on the GPU-owning side (`context.c:100-131`); the
GPU-less side reads and presents (`glimpl.c:591-597`, `glx.c:1200`). That is exactly our
requirement: guest renders, host displays. **This half mirrors cleanly.**

But note the *presenter* is currently client-side platform code. In our case the client is
Linux, so we keep a presenter like `glx.c:1153-1204` (or better, a Wayland/dmabuf one) and
delete the Windows presenter entirely. Conversely `windrv.c:745-780` — the Windows
`SetDIBitsToDevice` path — has no analogue on our side and goes away.

**(d) Server-only dependencies are Linux-only and must be rebuilt for Windows.**
The server hard-requires SDL2 + libepoxy and Linux (`CMakeLists.txt:33-36`; the README states
the server "can only be built and run on a Linux host"). `processor.c` `#include`s
`server/context.h` which `#include`s `<SDL2/SDL.h>` (`inc/server/context.h:5`) — there is **no
abstraction seam** between dispatch and windowing.

For us the server must run *inside Windows*. Everything in `context.c` and `overlay.c` is
rewritten. The good news: sharedgl's SDL hidden-window trick (`context.c:45-50`, a
`SDL_WINDOW_HIDDEN` window existing only to own a GL context) has **no Vulkan analogue needed** —
Vulkan is headless by default. Create a `VkInstance`/`VkDevice` with no surface, render to
`VkImage`s, and copy the final image to the shared region with `vkCmdCopyImageToBuffer` into a
host-visible buffer, then one `memcpy` to the BAR. That is *simpler* than what sharedgl does.

**(e) Cache-mode policy inverts.**
The Windows mapping is `IVSHMEM_CACHE_WRITECOMBINED` (`pb.c:173`). Write-combining is right for
a pure *producer* — which the Windows side is today (it writes commands). In our inversion the
Windows side becomes the *consumer* of commands and the *producer* of framebuffers. WC reads are
uncached and slow; reading a 24 MiB command stream over WC would be painful.

The cache mode is a per-mapping IOCTL parameter (`Public.h:17-21`, `Queue.c:262-271`), but the
driver maps **the whole region with one policy** and tracks a single `shmemMap`
(`Device.h:43`, `Queue.c:296-303`). Options: (i) accept `IVSHMEM_CACHE_CACHED` for everything and
use explicit fences, (ii) take two mappings of the same MDL with different cache types — note
`ioctl_release_mmap` (`Queue.c:341-365`) assumes one mapping and would need work, (iii) patch
the driver further. **This is a real, measurable decision and should be benchmarked early.**
sharedgl gives no evidence either way because it never reads bulk data through the BAR.

**(f) One-in-flight-globally is scale-baked, and is fatal for Vulkan.**
`SUBMIT` is a single global int (`processor.c:200`, `glimpl.c:434-450`). One submission
system-wide, everyone else spins. Return values go into per-client mailboxes but there is no
per-client *queue*.

You cannot mirror this. Vulkan's model is many queues × many command buffers × explicit
`VkFence`/`VkSemaphore`/timeline semaphores. The minimum viable replacement:

- **Per-client (ideally per-`VkQueue`) SPSC ring** with separate cacheline-aligned head/tail
  words, so producer and consumer never share a dirty line.
- **A submission id / timeline word per ring**, so the client can map `vkQueueSubmit` →
  submission id and `vkWaitForFences` → "wait until completed_id ≥ N". This is the natural
  representation and sharedgl has nothing like it.
- **Detach return values from batch completion.** sharedgl blocks the caller for every
  `glGet*`; Vulkan's design lets you avoid that almost entirely (object handles are created by
  the *client* if you assign them, queries are explicit). Do not inherit the blocking RPC.

**(g) The three-copy chain is not required by the topology.**
The server's defensive copy (`processor.c:406-409`) is a security measure that becomes *more*
important in our inversion, not less — but note the direction of distrust flips too. Today the
host distrusts the guest. In our case the host is the client and the guest is the server; the
guest is the more-trusted-by-us component but is also a Windows VM. Decide explicitly whose
memory safety you are protecting. The `VP_DOWNLOAD` unbounded `memcpy` (`processor.c:488-492`)
is the archetype of what not to reproduce.

The client-side copy (`pb.c:381-385`, avoidable via the never-enabled `direct_access` at
`pb.c:124-127`) should simply not exist in a ring-buffer design — you write into the ring.

**(h) Multi-client design is thin and will need rework.**
16 slots, a 32-bit mask, one framebuffer, one map slot, one context per *process*. For Vulkan
you need per-`VkDevice` and per-`VkQueue` structure, and multiple presentable images
(a real swapchain, not one shared frame guarded by `SWAP_BUFFERS_SYNC`
at `glx.c:1198-1201`).

### 7.3 What sharedgl gets right that we should keep

- Transport-independence by construction: the *same* memory layout is used for shared memory and
  for networking (`main.c:177-179` mmaps anonymous memory in network mode, and the whole
  register/FIFO protocol runs over it). Worth preserving — a TCP fallback is excellent for
  bring-up and debugging before ivshmem works.
- The `pb_net_hooks` indirection (`inc/client/pb.h:33-41`) is a clean, tiny transport trait.
  In Rust this is one trait with ~6 methods.
- The mailbox-per-client return area is a sound idea; keep it, just add per-client rings too.
- Copying the batch out of shared memory before executing (`processor.c:406-409`) is correct
  discipline. Keep it, or use a ring with proper acquire/release ordering and validated bounds.

### 7.4 Cost model for our direction

Because the GPU is in the guest and the display is on the host, the per-frame framebuffer
transfer is **unavoidable and is the dominant cost**, exactly as in sharedgl but with the
directions swapped:

- guest GPU → guest host-visible buffer (`vkCmdCopyImageToBuffer`)
- guest CPU → shared BAR (memcpy)
- host reads BAR → presents

sharedgl's numbers apply directly: 1080p BGRA is 8.3 MB/frame, ~500 MB/s at 60 Hz.
Improvements sharedgl does not attempt and we should: double/triple buffering in the shared
region (sharedgl has exactly one frame slot, `processor.c:331`), damage rectangles, format
negotiation instead of hardcoded `GL_BGRA` (`glx.c:1199`), and doing the vflip on the GPU rather
than the CPU loop at `context.c:109-122`.

### 7.5 Interrupts and doorbells from the host side — what I could and could not determine

**What the code shows:** nothing. There is not a single interrupt or doorbell in sharedgl's data
path, in either direction. Both sides busy-poll (`processor.c:200-207`, `glimpl.c:436, 450`).
The only configuration the server ever emits is `ivshmem-plain` (`main.c:43-49`), which has no
BAR0 register block and therefore no doorbell mechanism at all.

**What is available but unused:** the vendored Windows driver has complete MSI-X + doorbell
support (`Device.c:119-222, 327-357`; `Queue.c:365-401`; `Public.h:64-70`). So if we switch to
`ivshmem-doorbell`, the **guest** can block on an event object instead of spinning, for free,
using code that is already in this repository. That matters for us specifically, because in our
inversion the guest is the busy side (it would be running the polling server), and burning a
vCPU on a passthrough VM is worse than burning a host core.

**What I could not determine from this repository, and you should prototype before committing:**
the host-side interrupt story. sharedgl never uses `ivshmem-doorbell`, so it demonstrates
nothing about it. The relevant facts are outside this codebase: `ivshmem-doorbell` requires a
separate `ivshmem-server` process and a UNIX socket, and the "host peer" is a *client of that
socket*, not simply a process that mmaps `/dev/shm`. Whether the host can conveniently (a)
receive an interrupt raised by the guest and (b) raise one into the guest, with acceptable
latency, is the single biggest open question for the design, and I could not answer it from
sharedgl. Note also that `kernel/linux/sharedgl.c` binds `1af4:1110` without a revision filter
(`sharedgl.c:204-212`) and `ioremap`s all BARs (`sharedgl.c:91-105`) — but `dev_mmap` hardcodes
BAR2 (`sharedgl.c:48-51`) and there is no `request_irq`, so even the doorbell variant would be
unusable through this module as written.

Recommendation: **build the polling version first** (it is what sharedgl proves works), keep the
wait primitive behind a trait with one implementation being a spin-then-yield backoff, and treat
doorbell interrupts as a later optimisation validated by measurement. Do not architect around
interrupts you have not yet demonstrated.

---

## 8. Honest gaps in this study

- I did not build or run anything. All conclusions are static reading of the source at
  `297d05a`.
- I read `src/client/glimpl.c` and `src/server/processor.c` selectively — the transport,
  submit/init, upload/download/map, presentation, and the head and tail of the dispatch switch.
  I did not read all 824 dispatch cases; per-command argument-marshalling bugs beyond those the
  author flagged in comments would not have been found.
- The claim "no code generation anywhere" is based on the absence of any generator script or
  build step in the tree and the hand-edited appearance of the four parallel lists. It is
  possible the author generates these out-of-tree and commits the results; nothing in the repo
  indicates either way.
- Performance numbers in §3.6 and §7.4 are arithmetic from resolution × bytes × frame rate, not
  measurements.
- The recent commit `deee1d3` ("Overhaul client runtime, transport handling, and platform
  backends") reworked much of the client. Older third-party descriptions of sharedgl's client
  will not match what is described here; the per-client mailbox array and `SHM_SLOT_MASK`
  appear to be part of that overhaul.
- The README describes ivshmem setup and driver options accurately as far as the code goes. The
  one place I'd call it misleading: it presents networking as a peer of shared memory ("slower,
  but works"), whereas the code shows readback over the network is structurally broken
  (256-byte `retval_v` vs 3072-byte download blocks, §5), so `glMapBuffer`/`glGetTexImage`
  cannot work correctly in network mode.
