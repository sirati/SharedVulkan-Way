# Firefox through the forwarder, measured

`goal-firefox.md`'s fourth clause — *"hardware acceleration is working for the
renderer"* — had never been tested with Firefox. The chain is Firefox →
WebRender → GL → Zink → shared_vulkan → the GPU, and every link but the first
was measured. This is the first link.

Measured 2026-08-13 on **devhost**, in `nix develop`, with the GPU side on
the system RADV (Mesa 26.1.1) and Firefox **151.0.2** from the system. Not on
testhost: the question here is whether Firefox will drive a forwarded ICD at
all, and that is answered on either machine — cheaply and without contending for
somebody else's guest. Every number below says which shell it came from.

**The short version: Firefox selects our ICD, GL through Zink through the
forwarder works in both the EGL and the GLX paths, and Firefox still cannot use
it — because `sv-gpu-server` serves exactly one session per process lifetime and
Firefox is a multi-process application.** That is a limitation of ours, not of
Firefox, and it is the whole of what is in the way.

---

## 1. The chain works, up to but not including Firefox

Both GL entry points, against the forwarded device, in `nix develop`:

```
$ EGL_PLATFORM=surfaceless eglinfo -B          # with VK_DRIVER_FILES=sv_icd.json
OpenGL core profile renderer: zink Vulkan 1.3(AMD Ryzen 9 9950X3D 16-Core
                              Processor (RADV RAPHAEL_MENDOCINO) (MESA_RADV))
OpenGL core profile version:  4.6 (Core Profile) Mesa 26.2.0

$ DISPLAY=:7x glxinfo -B                        # Xvfb, same env
OpenGL renderer string:       zink Vulkan 1.3(AMD Ryzen 9 9950X3D 16-Core
                              Processor (RADV RAPHAEL_MENDOCINO) (MESA_RADV))
OpenGL core profile version:  4.6 (Core Profile) Mesa 26.1.1
```

The second one matters more than the first and had not been isolated before.
`eglinfo` on the surfaceless platform is what §0.4's stage measured; **GLX under
X11 is the path Firefox actually takes**, it goes through `zink_kopper` and a
`VK_KHR_swapchain`, and it reports the same device and the same GL version.

Note the two Mesa versions in one shell: `eglinfo` resolves to nixpkgs-unstable's
26.2.0 and `glxinfo` to the system's 26.1.1. `docs/video-decode.md` §1 records
the same split for RADV. It changes nothing here — both loaded Zink on top of our
ICD — but a number quoted from one is not a number about the other.

## 2. Firefox selects the ICD, and that is not an inference

The ICD's own line appears in Firefox's stderr:

```
sv-icd: connected to the GPU side at /tmp/svff/zink/gpu.sock
```

and the GPU side records the attach and starts serving:

```
[gpu] client 0 attached (control region by name, arena as a dma-buf descriptor)
[gpu] serving rings [1, 2, 3] on AMD … (RADV RAPHAEL_MENDOCINO) (radv)
```

So the loader enumerated us, Zink chose us, and a Firefox process completed the
cold-path handshake. Nothing about the ICD, the manifest, the loader or Zink's
device selection is in the way.

## 3. The control: Firefox works in this harness

Run identically but with `LIBGL_ALWAYS_SOFTWARE=1`:

| | llvmpipe | forwarded (zink) |
|---|---|---|
| Firefox stayed up for the full 60 s | **yes** | no — exits within seconds |
| stderr | empty | `ManageChildProcess(glxtest): poll failed`, `Exiting due to channel error` ×2 |
| WebGL renderer, read from the page | `llvmpipe, or similar` | *never got a context* |
| WebGL version | `WebGL 2.0` | — |
| `clearColor(0.2,0.6,0.9,1)` read back | `51,153,230,255` — exact | — |

The readback is worth a sentence. The page reads
`WEBGL_debug_renderer_info`, clears to a known colour and `readPixels` one pixel
back to a listener the harness can read — no screenshot and no OCR, so the result
is a string in a log rather than something a person judged by eye. On llvmpipe it
is exactly the colour asked for, which is the check that the GL pipeline produced
pixels rather than merely returning a context.

**So the harness is sound and Firefox is happy in it.** The difference is ours.

## 4. What is actually in the way: one session per server process

`sv-gpu-server serve --clients N` reads as "serve N clients". It does not. It
accepts N attach handshakes into **one shared control region and one set of
rings**, and it does not begin serving *any* of them until all N have attached
(`crates/sv-gpu-server/src/main.rs`, the `for i in 0..clients` loop, followed by
a single `session::serve`). After that it serves that one session until 30 s of
idle and exits.

Firefox is a multi-process application and the processes do not start together:

1. **`glxtest`** — Firefox forks a short-lived child at startup, before the
   browser exists, purely to find out what GL can do. This child is the first
   thing to load the ICD. It took the session.
2. **The browser process** starts about a second later, connects to a socket
   that exists, and is never served: the previous `session::serve` still owns it.
3. Firefox gives up. `poll failed` on the glxtest pipe, `Exiting due to channel
   error` twice, exit 1.

Three harness attempts, each of which failed in a way that names the property:

- `--clients 4`: two processes attached, the server waited for four, nothing was
  ever served, and Zink reported `vkCreateInstance failed
  (VK_ERROR_INCOMPATIBLE_DRIVER)` — which is what "the ICD connected and got no
  reply" looks like three layers up.
- `--clients 1`: `glxtest` got the session, the browser got nothing.
- A respawn loop restarting `--clients 1` after each exit: **no faster**, because
  the server does not exit when its client disconnects — it waits out the 30 s
  idle timeout. Measured directly: `glxinfo` then `glxgears` twelve seconds
  later against one respawn loop gave **one** attach, not two, and `glxgears`
  produced nothing.

That last measurement is the one to keep. It is not about Firefox at all: **two
ordinary GL programs, run one after another against one GPU side, and the second
does not get served.**

## 5. What would have to change

Nothing in the ICD, nothing in Zink, nothing about the profile. The GPU side
would have to serve **independent sessions**, which means, concretely:

- one control region and one ring set *per client*, rather than one shared set
  sized by `--clients`;
- an accept loop that spawns a session rather than falling through to a single
  `session::serve`;
- an arena decision. Today each server instance allocates its own arena from the
  driver; several sessions could share one arena — which is the natural answer,
  since the whole design is one arena per device — but the slab allocator and the
  object table are currently per-session.

That is a stage, not an afternoon, and it is the next thing standing between this
project and any real application. It was not reached before now because every
harness in the tree — `sv-end-to-end.sh`, `sv-zink-gl.sh`, `sv-dxvk-d3d11.sh`,
`sv-present.sh`, `sv-bench.sh`, `sv-video-decode.sh` — runs exactly one client at
a time, and so does `vulkaninfo`, `eglinfo`, `glxgears` and `video-probe`.
**Every instrument this project has is single-process, so the one property a real
application needs is the one nothing measured.** That is the same shape as §0.9's
"implemented meant compiled".

## 6. What this did not answer

- **The cost against llvmpipe.** That was question 2 and it needs a Firefox that
  runs. llvmpipe's side of it is measured — Firefox is up, WebGL 2.0, correct
  pixels — and the forwarded side has no number.
- **Whether WebRender picks a hardware backend.** `about:support` is the arbiter
  and Firefox never got far enough to have one. What is known is only that the
  ICD was selected and the handshake completed.
- **Anything on testhost.** This is devhost, with a real local GPU. On the
  machine that matters the host has no GPU at all (`design.md` §0.10), so the
  comparison there is "forwarded versus llvmpipe" with no third option — which
  makes the cost question sharper, not softer.
- **Video decode in Firefox.** Out of scope here and unchanged:
  `docs/video-decode.md` §5 still recommends software decode, and §6 records that
  the forwarder carries the capability queries and not a decode.

## 7. Where the frame ends up, and the copy count

Question 3 was what a shared handle between the two projects would have to be.
This is design rather than measurement, and it is written as such.

**Today, with no shared handle**, a Firefox frame reaching the compositor's arena
buffer is:

1. WebRender renders into a GL framebuffer, which Zink backs with a `VkImage` in
   GPU-side memory. *No copy — this is the render.*
2. `zink_kopper` presents: `vkQueuePresentKHR` on our swapchain, which §0.6
   implements as a GPU-side `vkCmdCopyImageToBuffer` into a linear arena buffer.
   **Copy 1**, GPU-side, into memory both sides map.
3. The app side reads that arena buffer and writes it into the Wayland buffer the
   compositor is given. **Copy 2**, CPU-side — and it is the one that should not
   exist.

**With a shared handle it is one copy**, and the handle is not a file
descriptor and cannot be: §1a forbids one crossing, and on the testhost
topology the compositor is in the guest anyway. What it has to be is **an arena
slab id plus an offset and a layout** — the same `(slab, offset, len)` triple the
protocol already carries for every buffer — with the compositor's `wl_buffer`
bound directly to that range instead of to a copy of it. waycompose already maps
arena slices (`mapped: 0 section, 0 shared-file, 3 arena-slice`), so the
consumer side of that exists; what does not exist is the swapchain image and the
`wl_buffer` naming the *same* arena range, which needs the two projects to agree
on one allocator rather than each having their own.

Concretely, the shared handle is three things and none of them is a descriptor:

- the **arena slab id**, which both sides already resolve to their own mapping;
- an **offset and length** within it, page-aligned;
- a **layout** — width, height, stride, format — which `wl_shm` already carries
  and which the swapchain already knows.

That is `svPresentBlit`'s destination and `wl_shm_pool`'s source being the same
bytes. It removes copy 2 and nothing else; copy 1 stays, because the frame is
rendered in tiled GPU memory and the compositor needs it linear.

## Re-running it

```
nix develop
cargo build -p sv-icd -p sv-gpu-server
bash /path/to/harness zink    # the forwarded run
bash /path/to/harness soft    # the llvmpipe control
```

The harness is not checked in, deliberately: it is thirty lines of Xvfb, a
respawn loop and a `nc` listener whose only purpose was to establish §4, and §4
says the respawn loop does not work. It is worth rewriting once the GPU side
serves more than one session, and not before.

---

# The fix, and Firefox rendering

Built and measured 2026-08-13, same day, same machine (**devhost**,
`nix develop`, GPU side on the system RADV, Firefox 151.0.2).

## What changed

`sv-gpu-server serve` now accepts **one session per client and serves each as it
arrives**. `--clients N` means "accept up to N sessions"; for `N = 1`, which is
what every existing harness passes, the observable behaviour is what it was.

The split is by whether two clients could tread on each other:

| | scope | why |
|---|---|---|
| `driver`, `fns` | **per device**, `Arc` | immutable once opened; sharing needs no lock |
| `arena` | **per session** | see below |
| `objects` (handle table, slabs) | **per session** | ids are assigned app-side from its own counter (§6.4), so two clients hand out the same ids |
| `queue_lock` | **per device**, `Arc<Mutex<()>>` | one `VkQueue` per device, and Vulkan requires external synchronisation |

Each session gets its own control region — created *after* the accept, so a
client that never comes costs nothing — its own `Control`, its own ring set, and
its own thread running `session::serve`.

### Why each session gets its own arena

The tidy answer is one arena per device, partitioned. It is not available for
free, and the reason is not a lock: **the arena's allocator lives on the app
side**, one per client (`sv-arena`), so two clients over one arena both allocate
from offset zero. Handing each a disjoint sub-range fixes that and needs no lock
at all — the `Attachment` already carries `arena_offset`, and disjointness would
be established once, single-threaded, in the accept loop.

It was still rejected, for a reason that is about isolation rather than races:
**the descriptor names the whole object.** A partitioned arena would hand every
client an fd it could map in full, so one client not reading another's buffers
would be a convention. A separate allocation per session makes it a fact —
`security-model.md`'s R3 argument one level up.

The cost is real and is not hidden: **`N` × `--arena` bytes of device memory**,
so `--clients 8 --arena 256M` is 2 GiB. `--arena` is the knob.

### The one thing that is shared and mutable

`Shared::queue_lock`, and its documentation is deliberately modest. Vulkan
requires the application to synchronise a `VkQueue` externally; the driver opens
exactly one, so every session shares it. **The generated dispatch does not take
the lock, and nothing does today**: the objects table is locked per lookup and
released before the driver call, so two ring threads *of one session* could
already reach `vkQueueSubmit` concurrently before any of this. Multi-session does
not create that race — it widens it from "two rings of one client" to "two
clients". The lock exists, next to the thing it protects, for whoever
hand-writes a queue call; closing it properly is a change to pass 12.

Saying that plainly is better than a `Mutex` that looks like it covers the
generated path and does not.

## The measurement that failed before

Three `glxinfo` runs, one after another, against one GPU side:

```
--- run 1 --- OpenGL renderer string: zink Vulkan 1.3(… RADV RAPHAEL_MENDOCINO …)
--- run 2 --- OpenGL renderer string: zink Vulkan 1.3(… RADV RAPHAEL_MENDOCINO …)
--- run 3 --- OpenGL renderer string: zink Vulkan 1.3(… RADV RAPHAEL_MENDOCINO …)
--- sessions attached: 3 ---
```

**Three, where §4 measured one.** That is the whole of the fix, checked by the
same measurement that diagnosed the problem.

## Firefox renders through the forwarder

Same harness as §3, `--clients 16`:

| | llvmpipe | **forwarded (zink → RADV)** |
|---|---|---|
| Firefox survived the run | yes | **yes** (was: exited in seconds) |
| sessions attached | — | **2** |
| WebGL version | WebGL 2.0 | **WebGL 2.0** |
| `WEBGL_debug_renderer_info` | `llvmpipe, or similar` | **`Radeon HD 3200 Graphics, or similar`** |
| `clearColor(0.2,0.6,0.9,1)` read back | `51,153,230,255` | **`51,153,230,255`** |

The renderer string is Firefox's *sanitised* one — it buckets real devices to
generic names for fingerprinting reasons — and that is exactly why it is the
right arbiter here. Firefox classified the forwarded device as **an AMD hardware
GPU**, not as software. On the same machine, in the same harness, the software
run says `llvmpipe, or similar`. Firefox is not being told which it is by us; it
is deciding, and it decided hardware.

And the pixel is exact, so this is a frame and not a context.

## What it costs

A microbenchmark run inside the page: `gl.clear` + `gl.finish` on a 256×256
canvas, for two seconds, counted.

| | clears+finishes per second |
|---|---|
| llvmpipe | **12588** |
| forwarded (zink → shared_vulkan → RADV) | **7711** |

**0.61× of llvmpipe, and the number should not be read as a rendering
comparison.** `gl.finish()` is a full round trip, so this measures *latency*, at
a resolution where llvmpipe's rasterisation costs almost nothing — it is the
forwarder's worst case and llvmpipe's best. 7711/s is 130 µs per round trip,
which is well under RADV's 0.651 ms submit floor (`video-decode.md` §2), so
`mesa_glthread` and Zink's batching are already absorbing most of it.

The number that would matter is a fill-heavy or geometry-heavy frame, where
llvmpipe pays CPU per pixel and the forwarder does not. `sv-zink-gl.sh` has that
shape and reports 64.12 dB and 69.82 dB agreement with llvmpipe on the frames,
but it is not timed against it. **So the honest statement is: Firefox renders
accelerated through the forwarder, and how much that is worth is not yet
measured.** §0.10 makes testhost the venue where it matters, because there
llvmpipe is the only alternative.

## Still not done

- **Copy 2.** Deliberately not attempted in this round: two copies that render
  beat one copy that does not exist. The design answer is §7 and is unchanged.
- **`about:support`.** Firefox now stays up long enough to have one, and reading
  it still needs a route that is not a screenshot. WebGL's renderer string is a
  weaker arbiter than the graphics feature table and it is the one measured here.
- **testhost.** Everything above is devhost, which has a real local GPU.
- **The queue race**, above.

---

# The resolution sweep, and what it actually measures

Measured 2026-08-13 on **devhost**, `nix develop`, GPU side on the system
RADV. `glxgears` under Xvfb at four sizes, `vblank_mode=0`, twelve seconds each,
last FPS sample. Three paths: our forwarder, **Zink on the real RADV with no
forwarding at all**, and llvmpipe.

The third column is the one that decides how to read the other two, and the
first attempt at this sweep did not have it — the control failed to start and
the table went out with two columns and a wrong conclusion drafted from them.
The reason it failed is worth recording: setting `VK_DRIVER_FILES` to the RADV
manifest for the *control* breaks Zink, while leaving it unset (so the loader
sees every ICD and Zink picks the real device) works. `sv-bench.sh` had it right
and this sweep had it wrong.

| size | forwarded | zink on real RADV | llvmpipe | fwd / zink-RADV | zink-RADV / llvmpipe |
|---|---|---|---|---|---|
| 256×256 | 920.9 | 1140.3 | 6802.8 | 0.81 | 0.17 |
| 512×512 | 214.9 | 289.3 | 5431.4 | 0.74 | 0.05 |
| 1024×1024 | 69.4 | 73.0 | 2598.2 | **0.95** | 0.03 |
| 2048×2048 | 18.3 | 18.2 | 806.6 | **1.01** | 0.02 |

## Two findings, and they point in opposite directions

**1. The forwarder converges on native as the frame gets more expensive.**
0.81 → 0.74 → 0.95 → **1.01**. At 2048×2048 the forwarded path is
indistinguishable from the same GL on the same GPU with no forwarding at all,
within the noise of a twelve-second sample. That is the right shape: the
forwarder's cost is **per frame and fixed**, so it disappears as a proportion the
moment the frame costs more than the round trip. `sv-bench.sh` says the same
thing from the other end — `frame-200` is 73.7 µs native against 293.3 µs
forwarded, and glxgears at its default size is 726 FPS forwarded against 795
native, 0.91.

**2. Both hardware paths lose to llvmpipe by 6–43×, and the forwarder is not
why.** Zink on the *real* RADV, no forwarding anywhere in the picture, is 0.17×
llvmpipe at 256² and 0.02× at 2048². Whatever is costing that is costing it to
real hardware too.

The cause is the harness rather than either driver: under **Xvfb** there is no
DRI3 and no MIT-SHM path for a windowed GL client, so `zink_kopper` presents
every frame through X11 core protocol over a socket — 16.8 MB per frame at
2048×2048. llvmpipe pays nothing for that, because it renders directly into the
X server's own memory. The `glxinfo` run in the middle of the first sweep is the
proof and it was an accident: with no environment set at all, the "native GL"
renderer string under Xvfb is `llvmpipe (LLVM 21.1.8, 256 bits)`. **There is no
hardware GLX path on this machine under Xvfb.** The forwarder is the only route
to the GPU here, and it is riding a present path that Xvfb makes expensive for
anything that is not already inside the X server.

## So the question the sweep was meant to settle is not settled

There is **no crossover in this harness** — llvmpipe wins at every size — and the
number is not about the forwarder, because it beats real hardware by the same
margin. Reporting "0.02× of software rendering" as a fact about this project
would have been wrong in the way §0.9 keeps naming: a true number that answers a
different question.

What would settle it is a present path that is not X11 core protocol. On
testhost that is exactly what exists: the consumer is waycompose's compositor
reading an **arena slice**, not an X server reading a socket. That is the same
path §7's shared handle is about, which makes the copy-2 work and this
measurement the same piece of work rather than two.

## testhost was not reached

Stated plainly rather than left to be inferred. The honest venue is the machine
with no GPU (§0.10), and getting a forwarded number there needs the full cross-VM
stack — `sv-hostd qemu-serve`, QMP, the memfd sweep, root, and a virtio-serial
cold path (`scripts/sv-guest-e2e.sh`) — on a guest two other experiments are
using. It was not attempted, so **there is no forwarded-versus-llvmpipe number on
the machine where that comparison is the only one there is.** The half of it that
is cheap — llvmpipe on the testhost host — was not taken either, because half a
comparison is not one.

---

# testhost, reached — and where Firefox stops

Measured 2026-08-13 on **testhost**, the machine the section above says was
not reached. Host: `/dev/dri` empty, no `/run/opengl-driver` at all, the iGPU
bound to `vfio-pci` and passed to the guest. Guest: Windows, Intel UHD Graphics,
Intel's proprietary driver. Harnesses: `scripts/sv-guest-host.sh` and
`scripts/sv-firefox-guest.sh`, both run *on* testhost.

**The short version: GL 4.3 on the guest's real GPU works from a host that has
none, including on the very Wayland socket Firefox uses — and Firefox still
renders in software, because its hardware WebRender wants an EGL config this
Wayland has no way to offer.** The forwarder is not what stops it, and the
evidence for that sentence is below rather than asserted.

## 1. What had to be built, because it did not exist

The Windows GPU side had **no cross-VM mode**. `serve` publishes two named
sections and a named pipe — objects with no meaning outside that kernel — so no
application on the Linux host could reach it however it was started. Only the
Linux guest path had `guest`, and this guest is Windows. The commit that adds
`sv-gpu-server guest --port` (now `sv-guest gpu guest --port`) is the missing
half; §1's table there says what differs from the Linux one and why the cold
path is a loopback TCP listener rather than a virtio-serial port.

`sv-hostd qemu-serve` also learned to take `--cold` more than once, because a
cross-VM session is one pinned window, one extent and one control region, and
Firefox needs several — §0.12's split, arriving on this path.

## 2. The chain, on the machine with no GPU

```
sv-hostd: the guest offers run 0x3dc2aaac7d4d5fe5, 4096 control pages
          + 16384 arena pages, locator Scan
sv-hostd: located 20480/20480 pages in 2.63s over 13311 MiB of guest RAM;
          25 run(s), 0 outside RAM
sv-hostd: stitched 80 MiB with 25 mmap call(s); all 20480 pages carry
          their own index
[gpu] Intel(R) UHD Graphics (intel), provider GuestRam (VirtualAlloc, pinned)
[gpu] serving rings [1, 2, 3] on Intel(R) UHD Graphics (intel)

OpenGL core profile renderer: zink Vulkan 1.3(Intel(R) UHD Graphics
                              (INTEL_PROPRIETARY_WINDOWS))
OpenGL core profile version:  4.3 (Core Profile) Mesa 26.2.0
```

Four sessions were located and served independently in one run. The guest got
**large pages** — 80 MiB in 25 physical runs rather than the 1471 that
`measured-gpu-side.md` records for 4 KiB locked pages — so the run list is small
and the stitch is 25 `mmap` calls.

## 3. The two facts about software fallback that this machine makes visible

Both are properties of the venue rather than of the work, and both were measured
here rather than reasoned about:

- **The default is llvmpipe, silently.** With Mesa present and no override,
  `eglinfo` on this host reports `llvmpipe (LLVM 21.1.8)` and GL 4.6. On a host
  with no GPU, a CPU implementation is what you get by *not asking*.
- **Forcing Zink fails loudly.** With `MESA_LOADER_DRIVER_OVERRIDE=zink
  GALLIUM_DRIVER=zink` and no ICD reachable at all, EGL returns
  `eglInitialize failed` and does **not** fall back. So a renderer string from
  that configuration is a device, and an absent one is not a quiet substitution.

## 4. `LIBGL_KOPPER_DISABLE=1`, and why it is needed

`sv-icd` advertises `VK_KHR_surface`, `VK_EXT_headless_surface`,
`VK_KHR_xlib_surface` and `VK_KHR_xcb_surface` — and **no
`VK_KHR_wayland_surface`**. So on the Wayland platform `zink_kopper` cannot make
a `VkSurfaceKHR`, and EGL initialisation hangs.

With kopper disabled, Mesa loads the **swrast** DRI driver for the Wayland
platform and puts Zink behind it as the *gallium* driver. `eglinfo` on the
Wayland platform, connected to waycompose's proxy and thence to the compositor
inside the VM:

```
Wayland platform:
EGL driver name: swrast
OpenGL core profile renderer: zink Vulkan 1.3(Intel(R) UHD Graphics
                              (INTEL_PROPRIETARY_WINDOWS))
OpenGL core profile version:  4.3 (Core Profile) Mesa 26.2.0
```

"swrast" names the *presentation* path — `wl_shm`, a CPU copy into the
compositor's buffer, which is §7's copy 2 — while every draw call still executes
on the Intel iGPU inside the VM. That is the arrangement this project wants and
it is running.

## 5. Where Firefox stops, exactly

Firefox loads the ICD — `sv-icd: connected to the GPU side at
/tmp/svff-int/icd.sock` is in its own stderr — and then:

```
MESA-EGL: warning: egl: failed to create dri2 screen
[GFX1-]: Failed to create EGLConfig for WebRender!
[GFX1-]: Failed GL context creation for hardware WebRender: true
[GFX1-]: Fallback WR to SW-WR
```

The window maps (`wc-compositor: window 1 mapped 1280x900`), frames arrive, and
`about:support` is a real browser on the guest's desktop — rendered by
**Software WebRender**.

Four things were varied and none of them moved it:

| varied | result |
|---|---|
| `LIBGL_KOPPER_DISABLE=1` | same failure — and `eglinfo` succeeds with it, so the GL stack is not what fails |
| dropping `MESA_LOADER_DRIVER_OVERRIDE=zink`, keeping `GALLIUM_DRIVER=zink` | same failure |
| the compositor's arena at 768 MiB instead of 64 MiB | same failure; 6 pools, 43 copies, 152 MiB, **0 overflows** — no exhaustion |
| `LIBGL_ALWAYS_SOFTWARE=1` (the control) | Firefox comes up and paints, via Software WebRender, with no EGL error at all |

**The EGL configs exist.** The Wayland platform's table has window-capable
`ARGB8888` (`0x34325241`) configs with depth 24 and stencil 8. So "no config"
is Firefox's own matching rule rejecting them, not Mesa having none.

The reading that fits every row: **Firefox's hardware WebRender on Wayland wants
a dmabuf-capable EGL display, and this topology cannot have one** — the host has
no DRM device to make dmabufs from, and `wc-proxy` offers `wl_shm` and nothing
else because no descriptor may cross the VM boundary. It is the same conclusion
the live session reaches on its own: `about:support` there reads
`WebRender (Software)` with `FEATURE_FAILURE_WEBGL_EXHAUSTED_DRIVERS`.

A control on **devhost** — real RADV, a `weston --renderer=pixman`
compositor offering `wl_shm` and no dmabuf, same Mesa, same Firefox — does *not*
hit the EGLConfig failure. The difference is that RADV has
`VK_KHR_wayland_surface`, so kopper works and Mesa loads the **zink** DRI driver
rather than swrast. That is the one variable between a Firefox that gets a
hardware GL context on a shm-only Wayland and one that does not.

## 6. So the two ways forward, named

1. **Give `sv-icd` a Wayland WSI** — `VK_KHR_wayland_surface` plus a present
   that attaches a `wl_shm` buffer to the client's surface. Then Mesa takes the
   kopper path it takes on devhost, which is measured to satisfy Firefox.
   This is the larger piece and it is the one that makes the result general.
2. **Put Firefox on X11 under an Xwayland whose backend is `wc-proxy`.** The ICD
   already has `VK_KHR_xlib_surface`, and §0.11 measured GLX through
   `zink_kopper` working. Cheaper, and it inherits §0.12's warning that X11 core
   protocol presentation is expensive.

Neither is a change to the cross-VM transport, which is done and measured.

## 7. What is *not* the problem, said plainly

- Not the arena: 0 overflows at 768 MiB, and the 64 MiB exhaustion that kills
  Firefox on a video (`arena 0 has no run of 2342912`) did not fire here.
- Not the forwarder: GL 4.3 on the real GPU, on the same Wayland connection,
  in the same run.
- Not multi-session: four sessions, four ICD clients, each on its own control
  region and its own arena.
- Not the deployment: the unified `sv-guest` binary runs both halves in the
  guest and both were exercised in one run.

---

# The Wayland WSI, and the gate that is not ours

Built and measured 2026-08-13 on **devhost** (`nix develop`, GPU side on the
system NVIDIA driver, Firefox 151.0.1, Mesa 26.1.1) and on **testhost** against
the live guest. The section above named two ways forward and said the Wayland one
"is the larger piece and it is the one that makes the result general". Both were
tried. **The finding is that neither of them turns on the variable that section
identified**, and the evidence for that sentence is a Mesa source line rather
than an inference.

## 1. `VK_KHR_wayland_surface` is built, and it is not enough

`sv-icd` now advertises `VK_KHR_wayland_surface` and implements it:
`vkCreateWaylandSurfaceKHR`, `vkGetPhysicalDeviceWaylandPresentationSupportKHR`,
and a present that attaches a `wl_shm` buffer to the application's own
`wl_surface` on the application's own `wl_display`, from an event queue of ours
(`sv_icd::present::wayland`). No descriptor crosses the VM boundary: the `memfd`
is created on the host, sent to `wc-proxy` on the same host, and copied by it
into the arena, which is the route `wc_proxy::pool` already documents.

`vulkaninfo` against the ICD reports `VK_KHR_wayland_surface : extension
revision 6`. **And Mesa still loads swrast.**

## 2. Why, exactly — and it is four lines of Mesa

`src/egl/drivers/dri2/platform_wayland.c`:

```c
dri2_initialize_wayland(_EGLDisplay *disp)
{
   if (disp->Options.ForceSoftware)
      return dri2_initialize_wayland_swrast(disp);
   else
      return dri2_initialize_wayland_drm(disp);
}
```

Zink on the Wayland platform is reachable through exactly two doors and both are
shut here:

- **`dri2_initialize_wayland_drm`** needs a DRM render node *from the
  compositor* — `zwp_linux_dmabuf_v1` feedback or `wl_drm` —
  and gives up when it has neither (`dri2_initialize_wayland_drm_extensions`:
  "wl_drm not advertised by compositor, so can't continue"). `wc-proxy` offers
  neither and cannot: the host has no DRM device, and §1a forbids the
  descriptors a dmabuf path is made of. `eglInitialize` then fails, and
  `eglapi.c` retries with **`Options.Zink = EGL_FALSE`** — which is where the
  `swrast` in `EGL driver name: swrast` comes from.
- **`dri2_initialize_wayland_swrast`** does honour `Options.Zink`
  (`driver_name = strdup(disp->Options.Zink ? "zink" : "swrast")`) and would take
  the kopper path — but it only runs under `LIBGL_ALWAYS_SOFTWARE`, and that same
  variable is read by Zink's `choose_pdev`, which then accepts **only** a
  `VK_PHYSICAL_DEVICE_TYPE_CPU` device. Measured: `MESA: error: ZINK: CPU device
  requested but none found!`. The only way through that door is for this driver
  to call itself a CPU, which is a lie that would make Firefox right to reject
  it.

So on a `wl_shm`-only Wayland with no DRM device anywhere, **Mesa will not use a
Vulkan driver at all, whatever that driver advertises.** The gate is above us.

This corrects the section above. Its control — real RADV on a
`weston --renderer=pixman` — differed from the testhost topology in a second
way that was not held fixed: devhost *has* `/dev/dri`, so that weston could
advertise a device and Mesa could take the DRM door. "RADV has
`VK_KHR_wayland_surface`" was true and was not the variable.

## 3. The X11 path does work, and it needed one environment variable

`platform_x11.c` has an override the Wayland platform has no equivalent of:

```c
   const bool force_zink =
      disp->Options.Zink && debug_get_bool_option("LIBGL_KOPPER_DRI2", false);
```

With it, Mesa skips the DRI3-multibuffer and render-device checks and goes
straight to `dri2_initialize_x11_kopper`. Without it, an Xwayland that has no DRM
device is refused outright — which is what the first three attempts here
measured, as `DRI3 not available` and `couldn't find RGB GLX visual or fbconfig`.

Measured on **devhost**, rootful Xwayland on the shm-only weston,
`scripts/sv-firefox-wayland.sh x11`:

| | before | with `LIBGL_KOPPER_DRI2=1` |
|---|---|---|
| `EGL driver name` (X11 platform) | *no configs at all* | **`zink`** |
| `eglgears_x11` | no fbconfig | **renders, on the real GPU, through the forwarder** — screenshot |
| Firefox: `Failed to create EGLConfig for WebRender!` | yes | **no** |
| Firefox: `Fallback WR to SW-WR` | yes | **no** |
| Firefox: `vkCreateSwapchainKHR` on our ICD | never | **yes**, `512x416 format 44 images 2 mode 2` |

The gears picture is the load-bearing one: OpenGL executing on a real GPU,
presented by this ICD's `XPutImage` into an Xwayland window that has no DRM
device, composited by a compositor that speaks only `wl_shm`. That is the whole
chain the topology needs, with the VM taken out.

## 4. On testhost, the same result and then a stall

Same configuration on the machine with no GPU, against the live guest
(`scripts/sv-firefox-guest.sh 8 150 x11`, `LIBGL_KOPPER_DRI2=1`):

```
sv-icd: connected to the GPU side at /tmp/svff-wsi/icd.sock
sv-icd: transport: vkGetPhysicalDeviceSurfacePresentModesKHR: X11 { window: 6291472 }
sv-icd: transport: vkCreateSwapchainKHR: X11 { window: 6291472 } 512x416 format 44 usage 0x17 images 2 mode 2
[GFX1-]: Calling WaitFlushedEvent::Run: is delayed: 14608
Exiting due to channel error.
```

**The EGLConfig failure is gone**: Firefox's hardware WebRender accepted the
display and built a swapchain on the guest's Intel iGPU from a host that has
none. Then its compositor thread is late by ~15 s and Firefox's GPU process
gives up. Reproduced twice, at 4 and at 8 sessions.

That is not `about:support` and it is not a browser anybody can use, so it is
reported as what it is: **the config rejection that this whole line of work was
about is fixed, and the first frame through the new path is too slow to keep
Firefox alive.** What is not yet separated is how much of the 15 s is the
cross-VM round trip per present, how much is the `XSync` this ICD's X11 target
does per frame (`sv_icd::present::x11::quietly`, a round trip to Xwayland on top
of the round trip to the GPU), and how much is a guest that a dozen other
experiments were using at the time.

## 5. What was added to tell "presented" from "visible"

`sv_icd::wsi::frame_note` samples one pixel per 4 KiB of every sixtieth
presented frame and says how many are not black, under `SV_DEBUG`. It exists
because this round confused the two claims twice: a Firefox whose swapchain,
fence, blit and `XPutImage` all succeed still showed a black window, and only

```
vkQueuePresentKHR: swapchain 0x55 frame 0 512x416 — 208/208 sampled pixels are not black
```

distinguished "the application rendered nothing" from "the display target is
broken". It said the frame had content — **and that there was exactly one of
them**, which is the fact that turns a rendering question into a lifecycle one.

## 6. Still not proven

- **`about:support` reading `WebRender`.** The arbiter named in the brief. On
  testhost Firefox does not survive to have one; on devhost it survives,
  logs no failure, and paints one frame.
- **A frame rate against the software baseline.** There is no steady state to
  measure yet.
- **The Wayland WSI end to end.** It is built, it is tested, and no Mesa on this
  topology will call it. It is the right answer for a compositor that can
  advertise a device, and a `wc-proxy` that could is a different piece of work.
