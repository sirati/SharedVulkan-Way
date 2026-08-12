# Phase 2 — A Wayland compositor that runs on Windows

**Feasibility and architecture report.** Research only; no code was written.

Written to a temp path because `/mnt/datasets/builds` is unmounted. Relocate as needed.

---

## 0. Verdict up front

The goal as stated is **four separate projects wearing one coat**. They do not have the same
answer, so here they are separately:

| Stated goal | Verdict |
|---|---|
| A Wayland compositor running natively on Windows | **Feasible.** No precedent on Windows, but a direct precedent exists on macOS (Smithay-based). |
| Built on the same libraries COSMIC uses (smithay stack) | **Feasible only as a hard fork.** Nothing in the Wayland half of the stack compiles for Windows today. `calloop` is the one clean exception. |
| Running Linux Wayland apps compiled for mingw | **Feasible but by far the largest cost, and it is per-toolkit.** GTK4 hard-disables its Wayland backend on Windows; QtWayland is not built for Windows. This is not a small patch, it is a port of each toolkit's Wayland backend. |
| Native Windows decorations | **Feasible, and Windows is a good fit** — but you must implement **KDE's `org_kde_kwin_server_decoration`**, not just `xdg-decoration`, because **GTK4 does not implement `xdg-decoration` at all**. And every libadwaita/`GtkHeaderBar` app is CSD-locked at construction time and can *never* get a native title bar. See §4.4. |
| "All extensions we can implement" | **~55 clean, ~60 with caveats, ~22 impossible or pointless** out of 137 enumerated. Full matrix in §5. |
| Tray icon support | **Feasible, and the easiest major item.** Not a Wayland protocol at all — it is StatusNotifierItem over D-Bus, and D-Bus runs on Windows. The hard part is not D-Bus; it is that **Windows 11 hides new tray icons with no supported API to un-hide them**. See §6. |
| Accelerated video decode over forwarded Vulkan | **Yes — and it is the single best-suited workload for phase 1**, because decoded pixels never need to cross the arena. But note §7.6a: **VA-API now runs natively on Windows on D3D12**, which may be the better path and needs no proxy work. |
| Digital rights management | **Hardware DRM is implementable** — §8's "contradictory" verdict was wrong and is superseded by **§11**. The supported design passes a handle, never pixels; Microsoft's D3D12 spec names the compositor as the intended recipient and Chromium ships it from an unsigned, low-privilege sandboxed process. **The one real blocker is that a Linux client cannot hold an SL3000 licence** (§11.4), which makes this the strongest argument in the report for the mingw-native client architecture. |

**The single most important correction to the plan:** the question you flagged as decisive — fd
passing — is **not** the thing that kills this. It is real, it is confirmed, and it is a
*bounded* problem with a proven precedent (waypipe) and, in the Rust stack, a patch surface of
roughly one file. The thing that actually threatens the schedule is the **client side**: there
is today no such thing as "a Linux Wayland application compiled for mingw", because the major
toolkits do not build their Wayland backends on Windows at all.

**The second correction, and it concerns phase 1 directly:** phase 1 exists to give processes on
the *Linux* host a GPU that lives in the *Windows* guest. But a mingw-native Wayland client is
already a Windows process sitting next to the real driver — it does not need a forwarder at all.
**"Run mingw-compiled clients" and "use the shared Vulkan from phase 1" pull in opposite
directions.** The topology in which phase 1 is load-bearing is the one where clients stay on
Linux. This is developed in §9.1 and it drives the architecture recommendation.

**Scale.** Yes, this is tens of thousands of lines. My estimate, excluding phase 1:

| Component | Estimate |
|---|---|
| Forked `wayland-backend` transport + handle table | 3–5k |
| Forked `wayland-server` socket layer | 1–2k |
| Forked/patched `smithay` core (`utils`, `shm`, de-dmabuf) | 3–6k |
| New `backend_win32` (windowing, input, output, cursor) | 8–15k |
| Vulkan-on-phase-1 renderer + compositing | 5–10k |
| Protocol implementations (the ~115 viable ones) | 15–30k |
| SNI/D-Bus → `Shell_NotifyIcon` bridge | 3–5k |
| Client-side: libwayland-client Windows port | 3–5k |
| **Client-side: per-toolkit Wayland backend port (GTK4, Qt6, SDL3)** | **20–60k each, and this is the risk** |

Compositor-side total is roughly **40–75k lines**. With the toolkit ports it is plausibly
**100k+**. A minimal but genuinely useful subset — §9's stages 1–4 — is far smaller, on the
order of 15–20k lines.

---

## 1. Can smithay run on Windows?

**Short answer: not today, in any configuration, and the blockers are in the core rather than
the backends.** But the shape of the fork is clear and the design is more amenable than it first
looks.

### 1.1 Crate-by-crate

| Crate | `x86_64-pc-windows-gnu` | Blocker |
|---|---|---|
| `wayland-scanner` | **Builds** | none — `proc-macro2`/`quote`/`quick-xml` only |
| `calloop` | **Builds** | none (see §1.2) |
| `wayland-sys` | **Cannot** | `pkg-config` build script + `dlib` FFI to `libwayland-*.so` |
| `wayland-backend` (`rs`, default) | **Cannot** | `rs/socket.rs`: `std::os::unix::net::UnixStream`, `rustix::net::{sendmsg,recvmsg,SendAncillaryBuffer,…}`, `rustix::cmsg_space!(ScmRights(..))`. `rs/wire.rs`: `OwnedFd`. `protocol.rs`: unconditional `use std::os::unix::io::AsRawFd` |
| `wayland-backend` (`*_system`) | **Cannot** | pulls `wayland-sys` |
| `wayland-client` | **Cannot** | via `wayland-backend`; `UnixStream` in `Backend::connect` |
| `wayland-server` | **Cannot** | own `src/socket.rs`: `UnixListener`, `OpenOptionsExt`, `rustix::fs::flock`. `rs/server_impl/common_poll.rs` is epoll/kqueue with no third path |
| `wayland-protocols`, `-wlr`, `-misc` | **Cannot, transitively only** | no platform code of their own; gate is entirely `wayland-backend` |
| `smithay` (**any** feature set, incl. `--no-default-features`) | **Cannot** | five independent unconditional blockers — §1.3 |

### 1.2 `calloop` is the good surprise — but read the caveat

It is no longer an epoll wrapper. `src/sys.rs`:

```rust
#[cfg(unix)]
use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd as Borrowed, RawFd as Raw};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, AsSocket, BorrowedSocket as Borrowed, RawSocket as Raw};
use polling::{Event, Events, PollMode, Poller};
```

`PingSource` has three implementations — `sources/ping/{eventfd.rs, iocp.rs, pipe.rs}` — selected
by `cfg`. There is a real `ci-windows` job on `windows-latest`. `polling` 3.x is explicitly
"portable interface to epoll, kqueue, event ports, and IOCP".

**The caveat is architecturally significant and must be designed around from day one.** On
Windows, `Generic<F: AsFd>` resolves to `AsSocket`, and `polling`'s Windows backend is wepoll,
which **only supports sockets**:

> "Wepoll (and thus this binding) only support sockets, so you can't use arbitrary file
> descriptors."

So calloop on Windows **cannot poll the Win32 message queue, waitable handles, timers, or
pipes**. A Wayland compositor on Windows has two event sources that must both be serviced — the
Wayland sockets and the `HWND` message pump (`GetMessage`/`PeekMessage`) — and calloop can only
see one of them. This forces one of:

- run the Win32 message pump on its own thread and forward into calloop via a `PingSource`
  (whose Windows impl is IOCP-based and works); or
- drive the loop from `MsgWaitForMultipleObjects` and poll calloop non-blockingly; or
- drive from the message pump and use `WSAAsyncSelect`/`WSAEventSelect` to turn socket
  readiness into window messages — arguably the most Windows-native answer.

Two further notes: `calloop`'s `signals` feature is `cfg(target_os = "linux")`, and its CI is
MSVC, not `windows-gnu`. `windows-sys` ships import libs so gnu ought to work, but this is
**unverified**.

### 1.3 Why smithay itself fails, and why there is no `wayland_frontend`-only escape

`smithay/Cargo.toml` has **zero `[target.*]` sections**. Its `rustix` dependency requests
`["event", "fs", "mm", "net", "pipe", "process", "shm", "time", "param"]` unconditionally, and on
Windows rustix provides only `event`, `mm`, `shm`, `net` — `fs`, `pipe`, `process`, `time`,
`param` are all `#[cfg(not(windows))]`. Unlike calloop (which requests unavailable features but
never calls them, making them harmless no-ops), smithay **uses** the missing ones:

| Location | Blocker | Gated by |
|---|---|---|
| `src/utils/clock.rs` | `rustix::time::clock_gettime` | **nothing** — `pub mod utils` is unconditional |
| `src/utils/sealed_file.rs` | `rustix::fs::memfd_create`, `fcntl_add_seals`, `rustix::shm::open` | **nothing** |
| `src/utils/fd.rs` | `std::os::unix::io::*`; reads `/proc/self/fd/{fd}` | **nothing** |
| `src/backend/allocator/dmabuf.rs`, `udmabuf.rs` | `rustix::ioctl`, `DMA_BUF_SYNC`, `rustix::mm::mmap` | **nothing** — `pub mod allocator` and `pub mod dmabuf` are both unconditional |
| `src/wayland/shm/pool.rs` | `rustix::mm::{mmap,munmap}`, `libc::sigaction(SIGBUS, …)` | `feature = "wayland_frontend"` |

So `--no-default-features` does not help: `utils` and `backend::allocator::dmabuf` compile
unconditionally and each fails on its own.

The Linux-only *optional* deps — `drm`, `input`, `udev`, `gbm`, `libseat`, `x11rb` — **are**
cleanly feature-gated (`backend_drm`, `backend_libinput`, `backend_udev`, `backend_gbm`,
`backend_session_libseat`, `backend_x11`). That part of the design is fine. The problem is
everything in the supposedly portable core.

Two specific sub-answers to the "winit + Vulkan backend" question:

- **`backend_winit` is not a portable windowing backend.** winit supports Windows; smithay's
  backend does not. `src/backend/winit/mod.rs` opens with `use wayland_egl as wegl;` and matches
  the window handle with `_ => panic!("only running on Wayland or with Xlib is supported")`. It
  is a *nested Wayland/X11* backend and force-enables `wayland-client`, `wayland-egl`,
  `backend_egl`, `renderer_gl`.
- **`backend_vulkan` exists but is DRM-shaped.** `ash` is portable, but the module is built
  around `VK_EXT_physical_device_drm` (`PhysicalDeviceDrmPropertiesEXT`) for
  `primary_node()`/`render_node()`. It is a device-enumeration layer keyed on DRM nodes, not a
  standalone renderer.

**There is no viable "winit + Vulkan" smithay backend. It would have to be written from
scratch.**

### 1.4 One nuance that makes the fork cheaper than it looks

The `protocol.rs` Unix coupling is **superficial**. The generic design is already right:

```rust
pub enum Argument<Id, Fd> { Int(i32), Uint(u32), ..., Fd(Fd) }
pub struct Message<Id, Fd> { ... }
```

`AsRawFd` appears only as a trait bound on `PartialEq`/`Eq`/`Display` impls, e.g.
`impl<Id: PartialEq, Fd: AsRawFd> PartialEq for Argument<Id, Fd>`, used solely for
`a.as_raw_fd() == b.as_raw_fd()` and a `Display` line. Swapping that for a local
`trait AsRawHandle` is a handful of lines. **The protocol machinery is genuinely fd-type-agnostic
already**; the port is concentrated in the transport, not the parser.

### 1.5 Precedent

**No Windows precedent exists.** Searched: smithay issues (only window-management issues match
"windows"), wayland-rs issues, C libwayland ports, waypipe ports. What exists instead:

- **[cocoa-way](https://github.com/J-x-Z/cocoa-way) — the real precedent, and it is close.** A
  native macOS Wayland compositor built on Smithay, rendering with **Metal**, with a "Rootless"
  mode that maps individual `xdg-shell` toplevels to **native macOS windows** — precisely the
  model requested here. It **vendors a patched Smithay** rather than using upstream, and
  maintains a separate `waypipe-darwin`. Linux clients reach it via waypipe over SSH or local
  Unix sockets, or via Apple Container / Docker / OrbStack.
  **Read the caveat, though:** macOS is Unix. It has AF_UNIX *with* SCM_RIGHTS, so cocoa-way
  never had to replace the socket layer — it only fixed Linux-vs-Unix issues (memfd, clocks,
  dmabuf). Windows is strictly harder. It also does not run "macOS-compiled Linux apps"; the
  clients stay Linux, in a VM or container. Reported to have significant latency.
- **[waylovely](https://github.com/EchidnaHQ/waylovely)** — Smithay/Anvil fork for Android. Still
  a Linux kernel. Halted.
- Smithay client-toolkit [#41](https://github.com/Smithay/client-toolkit/issues/41) and
  [#292](https://github.com/Smithay/client-toolkit/issues/292) — the same `sealed_file.rs`
  memfd blocker, **unresolved even for FreeBSD**. If BSD is still blocked, Windows is a long way
  out.
- **[KDAB's "Wayland on Windows"](https://www.kdab.com/wayland-on-windows/) is not a native
  port** — it is WSL2 running Weston through VcxSrv with llvmpipe. Same category as WSLg (a real
  Weston with an RDP backend, inside the WSL2 Linux VM).
- [microsoft/wslg#1204](https://github.com/microsoft/wslg/issues/1204) is a *feature request* for
  exactly this, with no implementation.

---

## 2. File descriptor passing — the question you flagged as decisive

**Your premise is correct, and it does not kill the project.**

### 2.1 Confirmed: Windows AF_UNIX has no SCM_RIGHTS

Microsoft's own announcement states it directly:

> "Linux's unix socket implementation supports passing ancillary data such as passing file
> descriptors (SCM_RIGHTS) or credentials (SCM_CREDENTIALS) over the socket. **There is no
> support for ancillary data in the Windows unix socket implementation.**"

— [AF_UNIX comes to Windows](https://devblogs.microsoft.com/commandline/af_unix-comes-to-windows/)

The same post lists the other gaps: only **SOCK_STREAM** (no `SOCK_DGRAM`, no `SOCK_SEQPACKET`),
no abstract-namespace autobind, and **no `socketpair()`**. The WSL-interop follow-up repeats it:

> "As Windows Unix socket implementation does not currently support passing ancillary data such
> as `SCM_RIGHTS` etc., the ancillary data will also not be supported for Win32<->WSL interop
> over Unix sockets."

— [Windows/WSL Interop with AF_UNIX](https://devblogs.microsoft.com/commandline/windowswsl-interop-with-af_unix/)

I searched for any change through 2026 and found none — no Microsoft statement adding ancillary
data to `afunix.sys`, no third-party report of it working. **Treat this as settled.**

It is structural, not a quirk:

> "On Unix, FD *objects* can be sent via sockets in messages. On Windows, handle *objects* cannot
> be sent in messages; only handle *numbers* can."

— [Passing FDs/handles between processes on Unix and Windows](http://lackingrhoticity.blogspot.com/2015/05/passing-fds-handles-between-processes.html)

Windows' substitute, `DuplicateHandle()`, writes into another process's handle table and so
**requires a process handle for the destination** (`PROCESS_DUP_HANDLE`) — push-based, and
requiring rights on the peer. Cygwin, which emulates AF_UNIX on Windows, **also does not
implement fd passing**, for exactly this reason: a Cygwin fd is backed by several Windows
handles, and "there's no secure way to obtain those process handles without help from a mediator
process which runs at high enough privilege levels."

### 2.2 The wire format is much friendlier than the headline suggests

Read directly from libwayland 1.24.0 `src/connection.c`. Two facts change the cost estimate.

**An `h` (fd) argument occupies zero bytes in the message body.** Fds travel entirely
out-of-band. `build_cmsg()` drains a FIFO ring buffer into one `SCM_RIGHTS` control message;
`decode_cmsg()` pushes into `fds_in`. Demarshalling pops one fd per `h` in the signature:

```c
case WL_ARG_FD:
        if (connection->fds_in.tail == connection->fds_in.head) { /* error */ }
        ring_buffer_copy(&connection->fds_in, &fd, sizeof fd);
        connection->fds_in.tail += sizeof fd;
        closure->args[i].h = fd;
        break;
```

and the body bounds check is explicitly skipped for fds:
`if (arg.type != WL_ARG_FD && p >= end)`.

**So the fd stream is already a separate, strictly-ordered side channel, matched positionally to
`h` arguments.** Replacing SCM_RIGHTS with any other ordered side channel is a local change: it
does not perturb object ids, opcodes, or argument layout. The awkward "send exactly one byte of
real data when the control buffer is full" workaround and the `MAX_FDS_OUT = 28` cap both simply
disappear.

**Sizes, for scoping.** libwayland's entire `src/` is **14,633 lines**. The client-relevant part
is `connection.c` (1,623) + `wayland-client.c` (2,748) + `wayland-os.c` (262) ≈ **4.6k lines**.
`event-loop.c` (1,122) is server-side only and pure epoll/signalfd/timerfd — irrelevant if the
server is the Rust stack.

**In the Rust stack it is smaller still.** In `wayland-backend` 0.3.16 the entire fd transport is
one file, `src/rs/socket.rs` (~330 lines), the only place `ScmRights` appears:

```rust
cmsg_buffer.push(SendAncillaryMessage::ScmRights(fds));
...
RecvAncillaryMessage::ScmRights(fds) => Some(fds),
```

Combined with §1.4 (the protocol layer is already generic over `Fd`), **the fd shim is a rewrite
of one small file plus a type-parameter substitution — not a fork of the protocol machinery.**

### 2.3 Precedent: waypipe already solved exactly this

Waypipe forwards Wayland over SSH — a transport that **cannot pass fds either**:

> "File descriptors provided by either the application or the compositor are replaced by messages
> that tell the remote Waypipe instance to create a file descriptor with matching properties, and
> send it on to the compositor or the application."

— [Network transparency with Wayland, M. Stoeckl](https://mstoeckl.com/notes/gsoc/blog.html)

Waypipe's hard-won lesson is a warning: it needed a **full Wayland protocol parser**, because
"one cannot determine which passed file descriptor corresponds to which message without parsing
the message." An fd on the wire is untyped.

**We escape that cost, for a reason worth stating plainly: on Windows there are no fds to begin
with.** The client code that creates the object always knows what it is creating — the `wl_shm`
path creates a section, the clipboard path creates a pipe, the Vulkan path creates an
external-memory handle. So the client shim can emit a **typed** token from the start. We need a
typed handle table, not waypipe's parser. This is a genuine architectural advantage of doing the
port properly rather than proxying.

Waypipe also shows the failure mode when an fd kind cannot be handled: its man page notes it
filters "protocols that require file descriptors which Waypipe cannot yet handle", and
`--no-gpu` blocks `linux-dmabuf` and `wayland-drm` outright.

### 2.4 What an fd actually *is*, per use site

| Wayland use | fd kind on Linux | Windows equivalent |
|---|---|---|
| `wl_shm.create_pool(fd, size)` | memfd / shm file | file-mapping section, or an offset into the phase-1 arena |
| `wl_keyboard.keymap(fd, size)` | read-only memfd, client `MAP_PRIVATE`s it | read-only section, `FILE_MAP_COPY` |
| `wl_data_offer.receive` / `wl_data_source.send` | pipe | anonymous or named pipe `HANDLE` |
| `zwp_primary_selection_*`, `ext-data-control` | pipe | same |
| `zwp_linux_dmabuf.params.add(fd, …)` | dma-buf | **no equivalent** — see below |
| explicit sync / `linux-drm-syncobj` | sync_file / drm_syncobj | timeline semaphore NT handle / D3D12 fence |
| `wp_security_context` | listening socket fd | a named-pipe listener, passed by name |
| `wlr-gamma-control` | ramp table over fd | `SetDeviceGammaRamp` (256×3 WORDs) |
| colour-management ICC profile | profile over fd | section, or a file path |

**The dmabuf row is the one that matters. `linux-dmabuf` is not portable — it is a Linux kernel
ABI by name and by content** (DRM fourcc + DRM format modifiers + per-plane dma-buf fds + a
`dev_t` main device). The honest answer is not "shim the fd", it is **"define a replacement
protocol"**: a `wp_win32_buffer` carrying a Vulkan external-memory NT handle plus a
`VkExternalMemoryHandleTypeFlagBits`, and a sync protocol over `VK_KHR_external_semaphore_win32`
(a D3D12 fence *is* a timeline semaphore). Every client must be pointed at these instead of
dmabuf.

### 2.5 The three candidate mechanisms, and which to pick

**(a) `DuplicateHandle` push.** Sender duplicates into the receiver, sends the numeric handle
in-band. Chromium's Mojo does exactly this — `TransferHandle()` calls
`::DuplicateHandle(from_process, handle, to_process, &out_handle, …)` with
`DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE`. Needs a handle to the peer process; Chromium's
sandbox therefore also has a `BrokerDuplicateHandle()` path.
- **The peer-PID problem has a clean answer if the transport is a named pipe rather than
  AF_UNIX**: [`GetNamedPipeClientProcessId`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getnamedpipeclientprocessid)
  gives the server the client's PID (and `GetNamedPipeServerProcessId` the converse). Note
  Project Zero has shown this PID can be
  [spoofed](https://googleprojectzero.blogspot.com/2019/09/windows-exploitation-tricks-spoofing-named-pipe-client-pid.html)
  — an identity hint, not a security boundary.
- Windows AF_UNIX has no `SO_PEERCRED` equivalent, so with an AF_UNIX transport the peer PID must
  be asserted by the client in a handshake, unauthenticated. **This is a real argument for
  choosing named pipes over AF_UNIX as the Wayland transport.**

**(b) Named kernel objects.** Name the section, send the name in-band. No process rights needed.
Risks: namespace squatting, guessability; mitigate with `CreatePrivateNamespace`, random names,
restrictive `SECURITY_ATTRIBUTES`. Simplest, weakest isolation.

**(c) Allocate everything out of the phase-1 shared arena.** `wl_shm` pools, keymaps and
clipboard staging become `(arena_id, offset, length)` triples. **No new kernel object at all**, no
per-buffer handle churn, and it is the option that actually exploits phase 1.

**Recommendation: (c) as the default path, (a) over a named pipe as the escape hatch** for things
that must be real kernel objects — pipes, Vulkan external memory, semaphores.

### 2.6 One gotcha to design for up front

`wl_shm_pool.resize` grows a pool in place (`ftruncate` + `mremap` on Linux). **On Windows a
section object's size is fixed at creation** — `CreateFileMapping` takes a *maximum* size and
there is no API to grow an existing section
([CreateFileMappingW](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-createfilemappingw)),
and there is no `mremap`. Two workable answers:
- create with `SEC_RESERVE` and a large maximum, then `VirtualAlloc`-commit as the pool grows
  (the clean one); or
- treat `resize` as "new section, re-key the pool", costing a copy.

The compensating upside: because a Windows section **cannot shrink**, the whole `wl_shm` SIGBUS
attack class disappears. libwayland installs a `SIGBUS` handler precisely because a client can
`ftruncate` a pool smaller and fault the compositor (`wayland-shm.c`:
`sigaction(SIGBUS, &new_action, &wl_shm_old_sigbus_action)`). On Windows the analogue is
structured exception handling around view access — MS advises "Use structured exception handling
to protect any code that writes to or reads from a file view" — but the threat it defends against
is much weaker.

### 2.7 The cost of a patched client — and the much bigger problem behind it

Requiring a patched client library is legitimate and the patch is *small* (one file in
`wayland-backend`, or `connection.c` + `wayland-os.c` in libwayland-client).

**But the premise underneath it — "Linux Wayland applications compiled for mingw" — does not
currently exist, and that is a far larger cost than the fd shim.**

- **GTK4 hard-disables its Wayland backend on Windows.** From GTK's top-level `meson.build`:
  ```
  if os_win32
    wayland_enabled = false
    x11_enabled = false
  else
    win32_enabled = false
  endif
  ```
  A mingw GTK4 build produces a **Win32** GDK backend, not a Wayland one
  ([GTK: Using GTK on Windows](https://docs.gtk.org/gtk4/windows.html)).
- **QtWayland is not built for Windows**; it is a Linux/BSD platform plugin and its dependencies
  (`wayland`, `xkbcommon`) are Unix-oriented.
- The one dependency that *is* already portable is **libxkbcommon**, which has supported Windows
  since 1.0.0 ("Support building on Windows using the meson MSVC backend") and fixed non-MSVC
  Windows compilers in 1.12.3
  ([release notes](https://xkbcommon.org/doc/current/release-notes.html)). That matters a great
  deal, because the keymap is unavoidable.

So "run Linux Wayland apps compiled for mingw" means, **per toolkit**: port libwayland-client (or
substitute the Rust one), re-enable and port that toolkit's Wayland backend on Windows, and port
whatever it uses for buffer allocation (memfd, dmabuf, EGL). GDK's Wayland backend and
QtWayland's client are each tens of thousands of lines that assume Linux.

**This is the most under-estimated item in the plan, and it recurs per toolkit.** It is worth
asking whether the goal is better served by the cocoa-way model — clients stay Linux in a VM or
container, and reach the Windows compositor over a proxy — which sidesteps the toolkit port
entirely at the cost of latency and a second OS in the loop.

---

## 3. The client side — the item most likely to sink the schedule

This was not one of the eight questions, but it deserves its own section because the research
kept returning to it. **The stated goal contains a premise that is not true today**, and it is
the largest single cost in the project.

"Linux Wayland applications compiled for mingw" does not currently exist as a category:

- **GTK4 hard-disables its Wayland backend on Windows** (§2.7). A mingw GTK4 build gives you the
  Win32 GDK backend. To get a Wayland one you must re-enable it *and* port `gdk/wayland/`, which
  assumes memfd, epoll, EGL and dma-buf throughout.
- **QtWayland is not built for Windows.** It is a Linux/BSD platform plugin.
- **libxkbcommon is the one bright spot** — Windows-supported since 1.0.0, non-MSVC Windows
  compilers fixed in 1.12.3. Since the keymap is unavoidable, this genuinely helps.
- `x86_64-pc-windows-gnu` is a **Rust Tier 1 target with host tools**, so the Rust half of the
  toolchain story is fine.

The compositor is the tractable half of this project. The client half is a per-toolkit port of
code whose upstreams have deliberately turned it off for Windows. Nothing in the compositor
design changes this, which is why §9.1 recommends deciding the client topology *before* writing
anything.

---

## 4. Native Windows decorations — mapping xdg-shell onto HWNDs

### 4.1 Where it maps cleanly

| Wayland | Win32 | Notes |
|---|---|---|
| `xdg_toplevel` | `HWND` with `WS_OVERLAPPEDWINDOW` | Real Win11 title bar, min/max/close, snap layouts, DWM shadow, Alt+Space, Alt+Tab, taskbar entry — free. |
| `set_title` (UTF-8) | `SetWindowTextW` after UTF-8→UTF-16 | Clean. |
| `set_min_size`/`set_max_size` | `WM_GETMINMAXINFO` → `ptMinTrackSize`/`ptMaxTrackSize` | Convert geometry→window rect via `AdjustWindowRectExForDpi`. **A window must have `WS_THICKFRAME` or `WS_CAPTION` to receive `WM_GETMINMAXINFO` at all.** Wayland's 0 means "unset"; use a large sentinel. |
| `set_maximized` / `unset_maximized` | `ShowWindow(SW_MAXIMIZE)` / `SW_RESTORE` | **There is no non-activating maximize** (`SW_MAXIMIZE == SW_SHOWMAXIMIZED`); use `SetWindowPlacement` if you need one. |
| `set_minimized` | `ShowWindow(SW_MINIMIZE)` | One-way in Wayland too ("no way to know if the surface is currently minimized"), so the mismatch is harmless. |
| `close` event | `WM_CLOSE` → send `xdg_toplevel.close`, do **not** `DestroyWindow` | Correct on both sides: "The client may choose to ignore this request." |
| `xdg_wm_base.ping`/`pong` | `SendMessageTimeout(WM_NULL)` / `IsHungAppWindow` | Also call `DisableProcessWindowsGhosting` so Windows does not paint a ghost frame over a slow client. |
| `show_window_menu` | `GetSystemMenu` + `TrackPopupMenu`, result fed back as `WM_SYSCOMMAND` | Genuinely the native window menu — exactly what the protocol asks for. |
| `xdg-foreign-v2` | `GWLP_HWNDPARENT` | Trivial here: all clients live in one compositor process, so cross-client parenting is a plain HWND owner relation. Emit `zxdg_imported_v2.destroyed` when the exporting HWND dies. |
| `cursor-shape-v1` | `LoadCursor(NULL, IDC_*)` | Implement this so clients ask for a *named* shape and get real themed, DPI-aware Windows cursors instead of handing you a cursor `wl_surface`. |

**`wm_capabilities` (v5): advertise `maximize`, `fullscreen`, `minimize`, `window_menu`.** Windows
always has all four, and this is also how you tell a CSD client not to draw a button you cannot
honour.

### 4.2 `set_app_id` → AppUserModelID — officially blessed for exactly this case

This is the happiest finding in the section. `set_app_id` "identifies the general class of
applications… The compositor shell will try to group application surfaces together by their app
ID." Microsoft's [Application User Model IDs](https://learn.microsoft.com/en-us/windows/win32/shell/appids)
page opens by describing a compositor —

> "an application that owns multiple processes or **an application that is running in a host
> process** might need to explicitly identify itself so that it can group its otherwise disparate
> windows under a single taskbar button"

— and gives the mechanism:

> "**If different windows owned by one process require different AppUserModelIDs to control
> taskbar grouping, use `SHGetPropertyStoreForWindow` to retrieve the window's property store and
> set the AppUserModelID as a window property.**"
> "**A window-level AppUserModelID overrides a process-level AppUserModelID.**"

So one compositor process with N HWNDs and N per-window AppIDs gives N separate, independently
pinnable taskbar groups. Microsoft names RAIL, the Java runtime and DLLHost as precedents and
documents an `HKCR\Applications\<exe>\IsHostApp` marker for host processes.

Practical notes: AppID format is `Company.Product.SubProduct.Version`, **≤128 chars, no spaces** —
reverse-DNS Wayland app_ids fit, but sanitise. Per-window
`System.AppUserModel.RelaunchCommand`/`RelaunchDisplayNameResource`/`RelaunchIconResource` give
each app its own icon and pin target (`PreventPinning` **must** be set before `PKEY_AppUserModel_ID`).
**"A window's properties must be removed before the window is closed"** — set to `VT_EMPTY` on
destroy or you leak. With window-level AppIDs the common file dialog cannot infer the AppID, so set
`FOS_DONTADDTORECENT`. **No `WS_SYSMENU` → no taskbar icon.** And applications cannot pin
themselves: "That functionality is reserved strictly for the user."

Window icons: [`xdg-toplevel-icon-v1`](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/raw/main/staging/xdg-toplevel-icon/xdg-toplevel-icon-v1.xml)
gives a name or `add_buffer(wl_buffer, scale)` → `CreateIconIndirect` → `WM_SETICON`. **GTK4 already
implements it.** Advertise `icon_size` events matching `GetSystemMetricsForDpi(SM_CXSMICON/SM_CXICON, dpi)`.
`xdg-toplevel-tag-v1` exists "to make some window properties … persistent … even after the
application has been restarted" — that is the key for persisting `RelaunchCommand`.

### 4.3 `set_parent` → Win32 *owner*, not parent

`xdg_toplevel.set_parent`: "This surface should be stacked above the parent surface and all other
ancestor surfaces… so that the parent is raised when the dialog is raised."

Win32 owner semantics: "An owned window is always above its owner in the z-order" — an exact match.
But Win32 adds two semantics Wayland lacks: "The system automatically destroys an owned window when
its owner is destroyed" and "An owned window is hidden when its owner is minimized." Both are
arguably what the user wants; in the destroy case you must synthesise the teardown rather than
assume the client asked.

Mechanics: pass the owner as `CreateWindowEx`'s `hWndParent`; re-parent dynamically with
`SetWindowLongPtr(GWLP_HWNDPARENT)` followed by `SetWindowPos(..., SWP_FRAMECHANGED)` because
"certain window data is cached". (Microsoft contradicts itself here — *Window Features* says
ownership "cannot be transferred"; `GWLP_HWNDPARENT` is the mechanism that actually works.)
Gotchas: "When a non-topmost window is made topmost, **its owned windows are also made topmost**";
and owned windows get **no taskbar button** by default, so add `WS_EX_APPWINDOW` if a client sets a
parent on something that should still appear in the taskbar.

Pair with `xdg-dialog-v1`'s `set_modal` — but note the spec says "**Clients must implement the logic
to filter events in the parent toplevel on their own**", so do **not** `EnableWindow(parent, FALSE)`;
that would break a client expecting to keep receiving events. Flash the parent at most.

### 4.4 Decorations — the headline finding: **GTK4 does not speak `xdg-decoration` at all**

This is the single most actionable correction in the report, and it changes what you must build.

**GTK4 does not implement `xdg-decoration-unstable-v1`.** The protocol list in
`gdk/wayland/meson.build` contains `server-decoration` (KDE's pre-standard protocol) and no
`xdg-decoration`; grepping `gdk/wayland` for `zxdg_decoration`/`xdg_toplevel_decoration` returns
zero hits, in GTK4 *and* GTK3. What it uses instead, from `gdk/wayland/gdkdisplay-wayland.c`:

```c
gboolean gdk_wayland_display_prefers_ssd (GdkDisplay *display)
{
  if (display_wayland->server_decoration_manager)
    return display_wayland->server_decoration_mode ==
           ORG_KDE_KWIN_SERVER_DECORATION_MANAGER_MODE_SERVER;
  return FALSE;
}
```

**So you must implement `org_kde_kwin_server_decoration_manager` and advertise
`default_mode = Server`, or every GTK client gets CSD regardless of what you do with
`xdg-decoration`.** Two `xdg-decoration` MRs have sat open and unmerged for years
([gtk!2191](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/2191) from 2020,
[!6398](https://gitlab.gnome.org/GNOME/gtk/-/merge_requests/6398) from 2023); the maintainer
position is that an explicit "request SSD" API would be "either broken … or a lie". Note also that
`GTK_CSD=0` is a **no-op on Wayland** in GTK4.

**And even with the KDE protocol, most modern GTK apps still cannot take SSD.**
`gtk_window_set_titlebar()` unconditionally calls `gtk_window_enable_csd()`, setting
`client_decorated = TRUE` **at widget-construction time, before any compositor is consulted**. And
libadwaita hardcodes it: every `AdwWindow`/`AdwApplicationWindow` calls `gtk_window_set_titlebar()`
in `_init()` and `g_error()`s if the app tries to call it again.

> **Every libadwaita app and every app with a `GtkHeaderBar` — essentially every modern GNOME
> application — is CSD-locked by construction and can never be given a native Windows title bar.**
> Only headerbar-less GTK apps (GIMP 2.x, Inkscape, traditional apps) will take SSD.

**Qt6 and SDL are fine.** Qt 5.12+/Qt6 bind `zxdg_decoration_manager_v1` and honour SERVER_SIDE;
Qt sends `unset_mode` ("no preference, compositor decides") and suppresses CSD both when the mode is
server-side and while the decoration is unconfigured, so there is no CSD flash at startup. SDL2/SDL3
bind it and **explicitly request SERVER_SIDE**, falling back to libdecor only if the compositor
insists on CSD.

**winit** also requests server-side by default (via smithay-client-toolkit): "We prefer server side
decorations, however to not have decorations we ask for client side decorations instead."

**Therefore: implement both decoration protocols.** `zxdg_decoration_manager_v1` for
Qt/SDL/winit/KDE — always `configure` `server_side`, ignoring the client's stated preference, which
the protocol expressly permits — *and* `org_kde_kwin_server_decoration_manager` with
`default_mode = Server` for GTK3/GTK4. Between them that is essentially the whole ecosystem. The
xdg-decoration `configure` event is normatively binding ("**The specified mode must be obeyed by the
client**"), but only against clients that bound the protocol in the first place. Note the protocol's
name is misleading: `org_kde_kwin_server_decoration` is a **GTK** requirement, not a KDE one — KDE
apps are Qt apps and go through `zxdg_decoration_manager_v1`.

**Ordering trap:** `gdk_wayland_display_prefers_ssd()` reads a value set by the manager's
`default_mode` event, which arrives during the registry roundtrip. **Send `default_mode`
immediately on bind, before the client's initial roundtrip completes**, or GTK decides CSD for the
first window and never revisits it.

**The CSD fallback, and why the choice matters.** When a client insists on CSD, do **not** use
`WS_POPUP`. Keep `WS_OVERLAPPEDWINDOW` and handle `WM_NCCALCSIZE` — Microsoft:

> "When wParam is TRUE, simply returning 0 without processing the `NCCALCSIZE_PARAMS` rectangles
> will cause the client area to resize to the size of the window, including the window frame. This
> will remove the window frame and caption items from your window, leaving only the client area
> displayed."

then `SetWindowPos(..., SWP_FRAMECHANGED)`. This keeps the taskbar button, Alt+Tab, Aero Snap, the
resize borders (via your own `WM_NCHITTEST`) and snap layouts (via `HTMAXBUTTON`) — all of which
`WS_POPUP` throws away. Add `DwmExtendFrameIntoClientArea` with negative margins to restore the drop
shadow. This is what Chromium and Electron do.

Three known gotchas on that path:
- **Maximized overhang.** With `WM_NCCALCSIZE`-returns-0 a maximized window extends past the work
  area by the frame thickness; subtract
  `GetSystemMetricsForDpi(SM_CXSIZEFRAME) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER)` on all sides
  when handling `SIZE_MAXIMIZED`.
- **Hit-testing.** You must synthesise `WM_NCHITTEST` results from the client's CSD layout — but
  Wayland hands you the answer: a GTK header-bar drag issues `xdg_toplevel.move`, which you turn
  into `ReleaseCapture()` + `SendMessage(WM_SYSCOMMAND, SC_MOVE|HTCAPTION)` and let Windows run its
  own drag and snap, correctly.
- **CSD shadows break snap maths, and this is unfixable in general.** The buffer is larger than
  `set_window_geometry`, the HWND rect must equal the buffer (Win32 cannot draw outside it), so
  every snap/maximize computation is off by the shadow margins and Aero Snap snaps the *shadow*
  rect. You can compensate by offsetting every `SetWindowPos`/`GetWindowRect` by the geometry
  inset, but it stays approximate. **This is the strongest single argument for forcing SSD wherever
  the client will accept it.**

Worth pairing with KDE's `server-decoration-palette` → `DWMWA_CAPTION_COLOR`/`TEXT_COLOR`/
`BORDER_COLOR` so native title bars match app theming.

### 4.5 Geometry, DPI and the coordinate chain

`xdg_surface.set_window_geometry` is "the visible bounds from the user's perspective. Client-side
decorations often have invisible portions like drop-shadows which should be ignored." GTK4 makes it
concrete (`gdk/wayland/gdksurface-wayland.c`): geometry = surface size minus
`shadow_left/right/top/bottom`.

So, for a **CSD** client:
```
buffer_size = geometry + shadows      (client-chosen)
window_rect = geometry                (frameless; you present or crop the shadow)
```
and for an **SSD** client:
```
buffer_size == geometry == client_rect
window_rect  = AdjustWindowRectExForDpi(client_rect, style, FALSE, exstyle, GetDpiForWindow(hwnd))
```
(SSD clients draw no shadow — the spec requires maximized/tiled states to "draw without shadow or
other decoration outside of the window geometry".)

**Use `AdjustWindowRectExForDpi`, never `AdjustWindowRectEx`** — the latter "is not DPI aware, and
should not be used if the calling thread is per-monitor DPI aware." Also beware that `GetWindowRect`
"may include **invisible resize borders**" (~7–8 px per side on Win11) while
`DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` gives the visible bounds but is **not** adjusted
for DPI. That asymmetry will bite when translating pointer coordinates.

**DPI.** Set `<dpiAwareness>PerMonitorV2</dpiAwareness>` **in the manifest** — Microsoft: "We
recommended that you specify the default process DPI awareness via a manifest setting. While
specifying the default via API is supported, it is not recommended." PMv2 also auto-scales the
non-client area, so the Windows title bar scales itself.

- `GetDpiForWindow` → `wl_surface.preferred_buffer_scale` = `round(dpi/96)`, and
  `wp_fractional_scale_v1.preferred_scale` = **`dpi * 120 / 96` exactly** (150 % → 180). No rounding.
- `WM_DPICHANGED`: `lParam` is a suggested `RECT` and Microsoft is emphatic — "It is critical that
  your application use this rectangle… **Prevent the application window from getting into a
  recursive dpi-change cycle.**" Use it verbatim with `SWP_NOZORDER|SWP_NOACTIVATE`, then send a new
  `preferred_scale` + configure.
- `WM_GETDPISCALEDSIZE` is the right hook for clients that only accept quantised sizes: "If the
  application responds to this message, **the resulting size will be the candidate rectangle sent to
  `WM_DPICHANGED`**."
- **Do not create HWNDs with mixed DPI awareness** — "the DPI awareness of the **entire process** can
  be reset."

Fractional scaling requires `wp_viewporter` (the spec says so), which you need anyway — see §4.7.

### 4.6 Where it fights: resize ownership and the modal loop

**Nobody owns the size, and that is the problem.** In xdg-shell, `configure(width, height, states)`
is explicitly "a hint"; zero means the client decides; only `maximized` is binding ("The window
geometry specified in the configure event **must be obeyed** by the client, or the
`invalid_surface_state` error is raised"), and `fullscreen`/`resizing` specify a *maximum*. The
client acks and then commits a buffer of the size **it** chose.

Win32 is the inverse: the window manager sets the rect and `WM_SIZE`/`WM_WINDOWPOSCHANGED` are
notifications after the fact — "Setting the members of the `WINDOWPOS` structure that is passed with
`WM_WINDOWPOSCHANGED` has no effect." The only veto points are *before* the change:
`WM_GETMINMAXINFO`, `WM_SIZING`, `WM_WINDOWPOSCHANGING`, `WM_NCCALCSIZE`.

**Concrete consequence: your HWND will be a size the client has not drawn yet, on every frame of a
resize drag.** You must choose what to show in the gap — stretch the last buffer (blurry), letterbox
(flashy), or block. Linux compositors hide this because they own the frame pixels *and* the shadow;
you do not, because Windows draws the frame. The one clean push-back is `WM_SIZING`, whose `lParam`
is a `RECT*` you may modify — useful for cell-quantised clients like terminals, round-tripped through
`AdjustWindowRectExForDpi`. Prefer `WM_WINDOWPOSCHANGED` over `WM_SIZE`: Microsoft says it is more
efficient, and `WM_SIZE`'s `lParam` carries only the low 16 bits of each dimension.

**The modal loop is the sharpest break.** `WM_ENTERSIZEMOVE`: "Sent one time to a window **after it
enters the moving or sizing modal loop**… **The operation is complete when `DefWindowProc`
returns.**" The system's nested loop does dispatch *posted* messages, but as Raymond Chen puts it,
"**your message loop is no longer in control, and therefore any customizations you've made to your
message loop will not be in effect.**" Your loop body is what reads the Wayland socket — so client
`ack_configure`/`commit` traffic sits unread for the whole drag and your configures never flush. Note
that returning `HTCAPTION` from `WM_NCHITTEST` does **not** avoid this; it only selects which loop
`DefWindowProc` enters.

Mitigations, best first:
1. **Run Wayland dispatch on its own thread**, with the WndProc as a message-passing endpoint. The
   honest answer, and it is required anyway because calloop on Windows can only poll sockets (§1.2).
   Cost: everything thread-affine (`SetCapture`, `SetFocus`, `TrackPopupMenu`, OLE STA) must be
   marshalled.
2. **Timer pump** — `SetTimer` on `WM_ENTERSIZEMOVE`, `KillTimer` on `WM_EXITSIZEMOVE`, dispatch from
   `WM_TIMER`. Works, but is folklore: **not documented by Microsoft anywhere.**
3. **Pump from `WM_SIZING`/`WM_MOVING`**, which fire per mouse-move and give natural pacing. Also
   undocumented as a technique.
4. Intercept `WM_NCLBUTTONDOWN`/`WM_SYSCOMMAND` and implement dragging yourself — you lose Aero Snap,
   snap layouts and the Alt+Space menu, i.e. exactly the native feel you are building this for.

**`xdg_toplevel.move`/`.resize` map cleanly**, via `DefWindowProc(WM_SYSCOMMAND, SC_MOVE|SC_SIZE)`
(mask `wParam` with `0xFFF0` — the low four bits are system-reserved). **Better, and this is a real
find: Windows 11 has [`EnterMoveSizeLoop`](https://learn.microsoft.com/en-us/windows/win32/winmsg/winuser/nf-winuser-entermovesizeloop)**,
whose `MSO_SIZE_LEFT`…`MSO_SIZE_BOTTOMRIGHT`/`MSO_MOVE` codes map 1:1 onto
`xdg_toplevel.resize_edge`. Caveats: **no header or lib** (`LoadLibrary`/`GetProcAddress` on
`User32.dll`), no keyboard move/size, and it still blocks until the pointer is released. Also
remember the protocol says the surface loses pointer focus when a move starts — send
`wl_pointer.leave` at loop entry.

**State round-trip.** Wayland states are double-buffered and only apply on `ack_configure`+`commit`;
Win32 changes are immediate. Drive both paths through the same funnel: client `set_maximized` →
`ShowWindow` → `WM_WINDOWPOSCHANGED` → read the new client rect → `configure(w, h, [maximized,
activated])`. **Do not special-case `SC_MAXIMIZE`** — derive state from the resulting geometry so the
user-clicked and client-requested paths converge.

**Aero Snap → `tiled_*`/`constrained_*`.** A snapped Win11 window is *not* `WS_MAXIMIZE`; it arrives
as a plain `WM_WINDOWPOSCHANGED`. There is **no documented snap notification** and nothing
snap-related in `DWMWINDOWATTRIBUTE`. Infer it by comparing the window rect against `rcWork`
halves/quadrants and synthesise `tiled_left`/`tiled_right` (v2) plus v7's `constrained_*` — which is
genuinely the right semantic. To get the snap-layouts flyout at all the window needs a working
maximize button, and custom-drawn frames must return `HTMAXBUTTON` from `WM_NCHITTEST`.

### 4.7 Popups, subsurfaces, focus, input and DnD

**Popups: separate `WS_POPUP` HWNDs, not child windows** — popup geometry is explicitly allowed to
extend outside the parent, and child windows are clipped to the parent client area. Use
`WS_POPUP | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`, `SW_SHOWNOACTIVATE`, and return `MA_NOACTIVATE`
from `WM_MOUSEACTIVATE`.

**You can honour `xdg_positioner` fully.** The spec gives explicit precedence — "**1) Flip, 2) Slide,
3) Resize**" — and precise slide semantics, and the work area comes from `MonitorFromWindow` +
`GetMonitorInfo().rcWork` (use `rcMonitor` for fullscreen). GTK4 uses the *full* positioner including
`set_reactive`, `set_parent_size` and `set_parent_configure`, so you do not get to skip the hard
parts. `reposition` → recompute, then emit `repositioned(token)`, `xdg_popup.configure`,
`xdg_surface.configure` in that order.

**Grabs are the real mismatch.** `xdg_popup.grab` requires that "the top most grabbing popup will
always have **keyboard focus**" while the client keeps receiving pointer events for all its surfaces
(an X11 "owner-events" grab), and nested popups must be destroyed in reverse order. `SetCapture`
covers the pointer half — "**Only the foreground window can capture the mouse**", capture is
thread-affine, and "menu hotkeys and other keyboard accelerators do not work" while captured — but
not the keyboard half, because `WS_EX_NOACTIVATE` popups never take focus. **Keep Win32 focus on the
parent toplevel and route keyboard events to the topmost grabbing popup at the Wayland layer.** That
is a fiction you maintain, but it matches the protocol. Dismissal triggers → `popup_done` top-down:
`WM_CAPTURECHANGED`, `WM_ACTIVATEAPP(FALSE)`, `WM_KILLFOCUS`, outside click, `WM_DISPLAYCHANGE`,
session lock. **Do not use `TrackPopupMenu` for client popups** — it insists on owning both the modal
loop and the rendering, and the client owns the pixels.

**Subsurfaces must be composited by you, and `wp_viewporter` is not optional.** The spec says a
subsurface is "not automatically clipped" to its parent, can be placed *below* the parent, "never has
the keyboard focus", and — decisively, in v7 — "**the entire graph must be applied atomically**".
Child HWNDs cannot deliver atomic multi-surface content updates: `DeferWindowPos` batches geometry
but not content, so you get tearing between a video subsurface and the UI around it on every resize,
plus clipping, `WS_CLIPCHILDREN` repaint holes, and per-HWND `WM_PAINT` scheduling you do not control.
Composite the tree yourself — which is also the argument for DirectComposition visuals under one HWND
(§9.2), the only Windows mechanism whose commit model is genuinely atomic.
**GTK4 uses subsurfaces for graphics offload and always pairs them with `wp_viewporter`**, bailing out
entirely with "Can't use subsurfaces without subcompositor and viewporter" if either global is
missing. So `set_source` (crop) and `set_destination` (scale) are mandatory if GTK4 video is to work.

**Focus: Win32 is authoritative, Wayland mirrors it.** `WM_SETFOCUS` → `wl_keyboard.leave(old)`,
`enter(new, keys_currently_down)` (reconstruct from `GetKeyboardState`, converting VKs via
`MapVirtualKey(vk, MAPVK_VK_TO_VSC_EX)`), then `modifiers`, plus `configure` with/without
`activated`. `WM_ACTIVATEAPP(FALSE)` → leave everything and drop all popups.
Two things that will bite:
- **Stuck modifiers.** On `WM_KILLFOCUS` you must send `leave` *and* a `modifiers` event clearing
  everything, because Wayland clients latch pressed keys. Alt+Tab is the classic repro: Alt is down
  when focus leaves and you never see the release.
- **Serials.** `start_drag`, `popup.grab`, `set_selection` and `toplevel.move`/`resize` are all
  serial-validated. Mint a monotonic serial per delivered input event and keep a small ring so
  slightly-late requests still validate.
Because the compositor is one process owning all HWNDs, intra-compositor focus moves generally satisfy
`SetForegroundWindow`'s "calling process is the foreground process" condition; stealing only fails
when the user is in another application, which is correct behaviour. **`xdg-activation-v1` maps
beautifully onto `AllowSetForegroundWindow`** — both are single-shot token-based delegation of the
right to steal focus. On denial, `FlashWindowEx`, which is what Windows does natively anyway. Note
that "no menus are active" is a precondition, so a Win32 app in menu mode silently blocks activation.

**Keyboard: there is a published scancode table, and prior art that does exactly this job.**
`wl_keyboard.key` carries **Linux evdev keycodes** (xkb keycode = evdev + 8), and `wl_pointer.button`
carries `BTN_LEFT` = 0x110 etc.
- **Chromium's [`ui/events/keycodes/dom/dom_code_data.inc`](https://raw.githubusercontent.com/chromium/chromium/main/ui/events/keycodes/dom/dom_code_data.inc)**
  is the best single source: 320 rows with USB / **evdev** / XKB / **Windows scancode** / Mac columns
  side by side, `0xe0` high byte for extended keys, and documented collisions.
- **FreeRDP/WinPR** is the most directly reusable (`KEYCODE_TO_VKCODE_EVDEV[256]`, plus
  `WINPR_KEYCODE_TYPE_EVDEV` so you can skip the ±8).
- **Weston's RDP backend does precisely your job** — `GetVirtualKeyCodeFromVirtualScanCode` →
  `GetKeycodeFromVirtualKeyCode(..., XKB)` → `notify_key(scan_code - 8)` — and carries an
  `rdp_keyboards[]` table mapping Windows KLIDs to xkb layout/variant/options. **That table is your
  layout-selection answer**, and the RDP backend generally is the closest reference implementation in
  existence, because RDP also carries PC scancodes and Windows layout IDs into a Wayland compositor.
- **The structural simplification worth knowing:** for the non-extended AT set-1 block (roughly
  `0x01`–`0x58`) **the Windows scancode and the Linux evdev keycode are numerically identical** —
  Linux inherited set-1 numbering — and XKB = evdev + 8. Divergence starts only at the
  `E0`-prefixed extended keys. So the mapping is *identity* for the main block, plus a ~40-entry
  table for `0xE0xx` (arrows, nav cluster, KP Enter, right Ctrl/Alt, Win keys, media) and the
  `0xE1` Pause special case. This is a much smaller job than it first looks.
- Prefer **raw input** (`WM_INPUT`/`RAWKEYBOARD`) as the source: it gives `MakeCode` plus
  `RI_KEY_BREAK`/`E0`/`E1` without `lParam` bit-24 ambiguity, and per-device identity. Keep
  `WM_KEYDOWN` as the fallback for synthetic/IME input, and always let `WM_SYSKEYDOWN` reach
  `DefWindowProc` or Alt+Tab breaks. SDL's `SDL_windowsevents.c` documents the quirks worth stealing
  (OSK sets the break bit; `scanCode == 0` for multimedia keys; Pause arrives as `0xE11D`+`0x45`; the
  fake `E0 2A` shift must be dropped).

**The HKL → XKB layout mapping.** One agent concluded no standard table exists and you would have
to build one; the other found that **FreeRDP's `xkb_layout_ids.c`, surfaced in Weston's RDP backend
as `rdp_keyboards[]`, is exactly that table** — Windows KLIDs mapped to xkb layout/variant/options.
The second is correct and it saves the work. Wine's `dlls/winex11.drv` layout tables are a
secondary source. Resend `wl_keyboard.keymap` on `WM_INPUTLANGCHANGE`.

**Generating the keymap — take the low-risk route.** `xkb_keymap_new_from_names()` +
`xkb_keymap_get_as_string()` is what Weston does, and libxkbcommon has Windows CI (MSVC and clang-cl;
**mingw is claimed-fixed but not CI-covered**). The real problem is that `new_from_names` needs the
xkeyboard-config data tree on disk at runtime via `XKB_CONFIG_ROOT`, which is meaningless on Windows.
**Generate the keymap strings at build time on Linux for the bounded set of layouts you care about and
embed them as string literals.** Handle `WM_INPUTLANGCHANGE` by sending a new `wl_keyboard.keymap`.
And note the keymap goes over an fd — back to §2.

**Clipboard — the two research passes disagreed, and the resolution matters.** Wayland's selection
model (advertise MIME types, transfer lazily over a pipe on `receive`) maps *structurally* onto
Win32 **delayed rendering**: `SetClipboardData(fmt, NULL)` on `set_selection`, then answer
`WM_RENDERFORMAT` when someone actually pastes. That is the elegant mapping. But
`WM_RENDERFORMAT` is synchronous — "The clipboard owner must render data … **must not open the
clipboard**" — and Microsoft warns the app "may become visibly unresponsive (hung) while rendering
occurs, **as no other window messages can be processed**". If the thread servicing `WM_RENDERFORMAT`
is also the thread dispatching Wayland, a blocking pipe read is a **guaranteed deadlock**: the
client cannot write because you are not dispatching.

**Resolution: because §4.6 already forces a two-thread design, delayed rendering becomes safe** —
service `WM_RENDERFORMAT` by blocking on a worker while the Wayland thread keeps dispatching, with a
timeout. Use eager-pull (immediately `receive()` every advertised type on `selection` and store real
handles) as the fallback for single-threaded builds and for small formats where the round-trip is
cheaper than the machinery. Do not attempt delayed rendering on a single thread.
`WM_RENDERALLFORMATS` (fires when you are about to lose ownership or exit) is the one place you
*must* `OpenClipboard`/`SetClipboardData`/`CloseClipboard` yourself. Prefer the **OLE clipboard**
(`OleSetClipboard(IDataObject*)`) so one `IDataObject` serves both clipboard and DnD and you get
`TYMED_ISTREAM`. Incoming: `AddClipboardFormatListener` + `WM_CLIPBOARDUPDATE` → new
`wl_data_offer`. Two more traps: `OpenClipboard` fails while another process holds it (retry with
backoff — a frequent real failure), and **line endings differ** (`CF_UNICODETEXT` is CRLF/UTF-16LE,
`text/plain;charset=utf-8` is LF/UTF-8), so convert both ways or you get doubled newlines.
MIME mapping has no Microsoft convention, but four implementations converged on
`RegisterClipboardFormatW(mime_string)` verbatim; GTK additionally escapes native formats as
**`application/x.windows.<NAME>`** and Qt as `application/x-qt-windows-mime;value="..."`. Offer the
verbatim MIME string *and* the native equivalent for well-known types
(`text/plain;charset=utf-8`→`CF_UNICODETEXT`, `text/uri-list`→`CF_HDROP`, `text/html`→`"HTML Format"`,
`image/png`→`"PNG"`+`CF_DIB`).

**Drag and drop: modality in both directions.** `DoDragDrop` "enters a loop" and does not return
until the drag ends; `RegisterDragDrop` requires an STA thread that "**must be pumping messages** …
If this requirement isn't met, then any application that drags an object over the window … **will
hang until the target application closes**." Use
[`IDataObjectAsyncCapability`](https://learn.microsoft.com/en-us/windows/win32/api/shldisp/nn-shldisp-idataobjectasynccapability)
in both directions — as source, `SetAsyncMode(TRUE)` so cooperating targets (Explorer, Office) extract
off-thread; as target, `QueryInterface` the incoming object, `StartOperation`, marshal to a worker,
return from `Drop` immediately, then `EndOperation`. Where the source does not support async you must
render synchronously inside `Drop`, with a timeout, or a hung Wayland client hangs the source
application. `xdg-toplevel-drag-v1` has **no Win32 equivalent at all** (Chrome and Firefox implement
tab-tearing with their own capture logic) — fake it by moving the HWND from `GiveFeedback`.
`zwp_primary_selection_v1` has no Windows analogue; implement it purely between Wayland clients or
not at all.

---

## 5. "All extensions we can implement" — the full matrix

**137 protocol entries enumerated** from directory listings actually fetched (the GitLab
`/-/tree/` HTML pages are behind Anubis; the REST API was used instead):

- `wayland/protocol` → `wayland.xml` (23 interfaces)
- `wayland-protocols` `stable` (5) / `staging` (31) / `unstable` (18)
- `wlr-protocols/unstable` (10) plus 4 wlroots-only protocols
- `pop-os/cosmic-protocols/unstable` (10)
- `plasma-wayland-protocols/src/protocols` (29)
- `weston/protocol` (11)

**Totals: ~55 clean IMPLEMENTABLE, ~60 IMPLEMENTABLE WITH CAVEATS, ~22 IMPOSSIBLE OR POINTLESS.**

**Two corrections to widely-repeated assumptions**, from the real listings: `single-pixel-buffer`
and `xwayland-shell` are **staging, not stable** — current stable is exactly 5 protocols. And
`cosmic-screencopy` and `cosmic-atspi` **no longer exist** in cosmic-protocols (screencopy
migrated to `ext-image-copy-capture`); the tree now has `cosmic-corner-radius` and
`cosmic-keyboard-layout` instead.

### 5.1 Core — `wayland.xml` (23 interfaces)

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `wl_display` (v1) | IMPLEMENTABLE | Pure object bookkeeping. The *transport* is the work (§2). |
| `wl_registry` (v1) | IMPLEMENTABLE | No OS dependency. |
| `wl_callback` (v1) | IMPLEMENTABLE | `frame` off `DwmFlush`, a DXGI waitable swapchain, or `DCompositionWaitForCompositorClock`. |
| `wl_compositor` (v7) | IMPLEMENTABLE | Factory only. |
| `wl_shm` (v3) | CAVEATS | Needs an mmap-able fd → §2 shim. ARGB8888/XRGB8888 is already BGRA-in-memory = Win32 DIB order, so **zero conversion**. |
| `wl_shm_pool` (v3) | CAVEATS | Same shim; `resize` cannot grow a view in place (§2.6), and old views must stay alive for in-flight buffers. |
| `wl_buffer` (v1) | IMPLEMENTABLE | Abstract handle; `release` is compositor policy. |
| `wl_surface` (v7) | IMPLEMENTABLE | Double-buffered state/damage is pure protocol. `set_buffer_transform` (90/180/270/flipped) has no HWND equivalent — do it as a blit. |
| `wl_region` (v7) | IMPLEMENTABLE | Opaque region → compositing; input region → `HTTRANSPARENT` from `WM_NCHITTEST`, or `SetWindowRgn`. |
| `wl_subcompositor` (v1) | IMPLEMENTABLE | Factory. |
| `wl_subsurface` (v1) | CAVEATS | Composite into the *parent's* HWND, not as child HWNDs — child HWNDs break atomic commit and the sync/desync tree semantics. |
| `wl_seat` (v11) | IMPLEMENTABLE | Single seat; capabilities from `GetSystemMetrics(SM_DIGITIZER/SM_MOUSEPRESENT)` + `GetRawInputDeviceList`. |
| `wl_pointer` (v11) | IMPLEMENTABLE | `axis_value120` is *literally* `WHEEL_DELTA` = 120 — exact unit match. enter/leave via `TrackMouseEvent`+`WM_MOUSELEAVE`. |
| `wl_keyboard` (v11) | CAVEATS | Mandates an **XKB keymap over an fd** and evdev keycodes: synthesize a keymap, map AT set-1 scancodes → evdev (fixed table), plus the fd shim. `repeat_info` from `SPI_GETKEYBOARDDELAY/SPEED`. IME text does **not** flow here — see `text-input`. |
| `wl_touch` (v11) | IMPLEMENTABLE | `WM_POINTER*` gives id/x/y/down/up/motion/frame directly; `cancel` ← `POINTER_FLAG_CANCELED`. |
| `wl_output` (v4) | IMPLEMENTABLE | `EnumDisplayMonitors`/`QueryDisplayConfig`; physical size + make/model from EDID; `scale` from `GetDpiForMonitor` (integer only). Report `subpixel = unknown`. |
| `wl_data_device_manager` (v4) | CAVEATS | Factory. |
| `wl_data_device` (v4) | CAVEATS | Transfer is a **pipe fd** → shim, or slurp into a buffer and never expose an fd. Maps to OLE: clipboard = `IDataObject`, DnD = `IDropSource`/`IDropTarget`/`DoDragDrop`. **`DoDragDrop` is a blocking modal loop** — hostile to the event loop. |
| `wl_data_source` (v4) | CAVEATS | `dnd_actions` ↔ `DROPEFFECT_COPY/MOVE/LINK`; delayed rendering ↔ on-demand `GetData`. |
| `wl_data_offer` (v4) | CAVEATS | Needs a MIME↔clipboard-format table: `text/plain;charset=utf-8`↔`CF_UNICODETEXT`, `text/uri-list`↔`CF_HDROP`, `image/png`↔registered `"PNG"`. |
| `wl_shell` / `wl_shell_surface` (v1) | POINTLESS | Deprecated since 2016. Trivial to implement; essentially no modern client binds it. |
| `wl_fixes` (v2) | IMPLEMENTABLE | `destroy_registry` + `ack_global_remove`. Pure lifecycle correctness. Implement it — it fixes real leak/race bugs. |

### 5.2 `wayland-protocols/stable` (5 — complete)

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `xdg-shell` | IMPLEMENTABLE (**the crux**) | See §4. Hardest interop point is resize ownership. |
| `presentation-time` | CAVEATS | `IDXGISwapChain::GetFrameStatistics` / `DwmGetCompositionTimingInfo`. **Real hazard:** the `clk_id` epoch must match what the mingw client's `clock_gettime(CLOCK_MONOTONIC)` returns — align both on QPC or the numbers are garbage. `hw_clock`/`hw_completion`/`zero_copy` generally not claimable under DWM. |
| `viewporter` | IMPLEMENTABLE | Pure src-crop + dst-scale in our render pass. |
| `linux-dmabuf` | **IMPOSSIBLE as specified / REPLACE** | PRIME fds, DRM modifiers, `dev_t`. Replace with `wp_win32_buffer` over `VK_KHR_external_memory_win32` (§2.4). |
| `tablet` (v2) | CAVEATS | `tablet_tool` maps well to `POINTER_PEN_INFO` (pressure, tilt X/Y, twist, barrel, eraser, hover). **`tablet_pad` has no Win32 API at all** — only legacy vendor WinTab. Implement tool, stub pad. |

### 5.3 `wayland-protocols/staging` (31 — complete)

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `alpha-modifier` | IMPLEMENTABLE | `SetLayeredWindowAttributes(LWA_ALPHA)` gives it free in DWM, else fold into the shader. |
| `color-management` | CAVEATS | Win11 Advanced Color is a real target: `IDXGIOutput6::GetDesc1`, `IDXGISwapChain4::SetHDRMetaData`, `DXGI_COLOR_SPACE_*`, ICC via WCS. Caveats: ICC arrives **as an fd**; and Windows' pipeline is far less programmable than a KMS degamma/CTM/gamma chain, so arbitrary per-surface transfer functions land in our shader, not hardware. |
| `color-representation` | IMPLEMENTABLE | Pure metadata (H.273 matrix/range/siting). `ID3D11VideoProcessor::SetStreamColorSpace` takes exactly these. |
| `commit-timing` | CAVEATS | Queue the commit, release on `CreateWaitableTimerEx(…, HIGH_RESOLUTION)` against the QPC clock. Best-effort only — DWM schedules the present and exposes no target-time API. |
| `content-type` | IMPLEMENTABLE | Advisory hint; trivially conformant. Genuinely useful — drives tearing/independent-flip and video-path decisions. |
| `cursor-shape` | IMPLEMENTABLE (**big win**) | 1:1 to `LoadCursor(IDC_ARROW/IDC_IBEAM/IDC_SIZENS/…)`. Correct, DPI-aware, user-themed native cursors, and lets us avoid client cursor surfaces entirely. **Prioritise.** |
| `drm-lease` | **IMPOSSIBLE** | Hands a DRM fd + CRTC/connector objects for direct KMS scanout (VR direct mode). The analogue is OpenXR/vendor direct-display takeover, not expressible as a leased fd. |
| `ext-background-effect` | CAVEATS | `DwmSetWindowAttribute(DWMWA_SYSTEMBACKDROP_TYPE, Mica/Acrylic)`. Caveat: the protocol blurs an arbitrary **region**; DWM backdrop is whole-window. |
| `ext-data-control` | CAVEATS | Needs the fd shim, and is architecturally redundant — Windows' clipboard is already OS-global (`AddClipboardFormatListener`). Advertise it, but the native clipboard should be the source of truth. |
| `ext-foreign-toplevel-list` | IMPLEMENTABLE | Trivial for our own toplevels. Optionally extend to native windows via `EnumWindows` — arguably more useful here. |
| `ext-idle-notify` | IMPLEMENTABLE | `GetLastInputInfo()` polled on a timer. Caveat: session-wide, not per-seat. |
| `ext-image-capture-source` | IMPLEMENTABLE | Source-handle factory; output → `HMONITOR`, toplevel → `HWND`, both directly consumable by `GraphicsCaptureItem`. |
| `ext-image-copy-capture` | CAVEATS | Windows.Graphics.Capture does precisely this; Win11 can drop the capture border (`IsBorderRequired=false`) and cursor. Caveats: frames arrive as D3D11 textures (buffer shim), and **WGC reports no damage rectangles**, so always report full-surface damage. |
| `ext-session-lock` | CAVEATS (functionally fake) | We can hide all client windows behind a topmost fullscreen HWND and honour the crash-safety contract internally — but the **security** contract belongs to the OS (Win+L, Ctrl+Alt+Del/SAS, the secure desktop). Better: advertise it and back it with `LockWorkStation()`. |
| `ext-transient-seat` | **POINTLESS** | A Windows session has exactly one input queue; multi-seat is done by separate sessions, above the compositor. |
| `ext-workspace` | CAVEATS | Only `IVirtualDesktopManager` (`IsWindowOnCurrentVirtualDesktop`, `MoveWindowToDesktop`) is **public**; enumerate/create/rename/switch is `IVirtualDesktopManagerInternal`, whose IID and vtable change nearly every build. **Recommend compositor-internal workspaces.** |
| `fifo` | IMPLEMENTABLE | Classic vsync-throttled present. DXGI waitable swapchain / `DwmFlush` give exactly this signal. |
| `fractional-scale` | IMPLEMENTABLE (**important**) | The protocol's unit is 1/120 and `USER_DEFAULT_SCREEN_DPI` is 96 → `scale_120 = dpi * 120 / 96` **exactly, no rounding**. Per-Monitor-v2 + `WM_DPICHANGED` drives `preferred_scale`. Essential for non-blurry clients at 125%/150%. |
| `linux-drm-syncobj` | IMPOSSIBLE as specified / REPLACE | Needs DRM syncobj fds — but the *concept* is native: `VK_KHR_timeline_semaphore` + `VK_KHR_external_semaphore_win32`. Through the Vulkan proxy this is the natural and probably mandatory sync mechanism. |
| `pointer-warp` | IMPLEMENTABLE | `SetCursorPos()`. Win32 imposes no focus restriction, so *we* enforce the focused-surface rule. |
| `security-context` | CAVEATS | Requires a **listening socket fd** → re-spec as "compositor mints a named pipe and returns its name". The tagging half works; its security value presumes a Flatpak-like sandbox, and Windows' AppContainer produces none of these tokens. |
| `single-pixel-buffer` | IMPLEMENTABLE | A 1×1 solid-colour buffer with **no fd at all** — the one buffer protocol that is free on Windows. |
| `tearing-control` | CAVEATS | `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` + `DXGI_PRESENT_ALLOW_TEARING` on a flip-model swapchain. Caveat: tearing is honoured only in **independent flip**; a DWM-composited window silently never tears, so `async` degrades to a no-op. Report the `presentation_hint` actually achieved. |
| `xdg-activation` | CAVEATS | Token handoff ↔ `AllowSetForegroundWindow(pid)` + `SetForegroundWindow`. Win32's foreground rules are OS-enforced: if the requester is not foreground, this degrades to taskbar flashing — which is a faithful implementation of activation's intent. Lean into `FlashWindowEx`. |
| `xdg-dialog` | IMPLEMENTABLE | `set_modal` → `EnableWindow(parent, FALSE)` + owned window + `DS_MODALFRAME`. This **is** the Win32 modal convention. |
| `xdg-session-management` | IMPLEMENTABLE | Persist session + per-toplevel restore data in AppData/registry. Zero OS dependency. Bonus: hook `RegisterApplicationRestart`. |
| `xdg-system-bell` | IMPLEMENTABLE | `MessageBeep(MB_OK)`, honouring `SPI_GETSOUNDSENTRY`. Per-surface variant → flash the window. |
| `xdg-toplevel-drag` | CAVEATS | Tear-off tabs. Under OLE, `DoDragDrop` is modal with mouse capture; reposition the HWND from `IDropSource::GiveFeedback`. Fiddly. Cleaner: do Wayland↔Wayland DnD internally, bridge to OLE only at the Windows boundary. |
| `xdg-toplevel-icon` | CAVEATS | Buffer path: 32bpp DIB → `CreateIconIndirect` → `WM_SETICON`, which also feeds the taskbar. Name path requires shipping an **XDG icon-theme lookup** — no Windows equivalent of icon-by-name. |
| `xdg-toplevel-tag` | IMPLEMENTABLE (good map) | Use the tag as the window's AppUserModelID (`IPropertyStore` `PKEY_AppUserModel_ID`) so taskbar grouping, pinning and jump lists work per logical window. |
| `xwayland-shell` | POINTLESS | Exists only for Xwayland surface association. Out of scope. |

### 5.4 `wayland-protocols/unstable` (18 — complete)

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `fullscreen-shell` | **POINTLESS** | A client owning an entire output. A user-mode compositor cannot take exclusive ownership of a Windows display — DWM owns it. `set_fullscreen` + borderless topmost delivers everything achievable. Near-zero uptake. |
| `idle-inhibit` | IMPLEMENTABLE (exact match) | `PowerCreateRequest`/`PowerSetRequest(PowerRequestDisplayRequired)` — per-reason and visible in `powercfg /requests`; better than `SetThreadExecutionState`. Apply only while the surface is visible, per spec. |
| `input-method` (v1/v2) | CAVEATS (architecturally wrong) | This is the *IME-side* protocol. Implementable, but Windows already has TSF and the user's installed IMEs; exposing this lets a Wayland IME compete for the same keystrokes. **Prefer being a TSF consumer and not advertising this.** |
| `input-timestamps` | CAVEATS | Pen/touch are fine — `POINTER_INFO.PerformanceCount` is genuine QPC. Mouse/keyboard carry only `GetMessageTime()` milliseconds, so precision is lost. Must share the clock domain with `presentation-time`. |
| `keyboard-shortcuts-inhibit` | CAVEATS | *Our* shortcuts: trivial. *Windows'*: Ctrl+Alt+Del and Win+L **cannot** be inhibited by any user-mode process. Win+D/Win+Tab can be swallowed with `WH_KEYBOARD_LL`, with UIPI/elevation and anti-cheat friction. Report partial success honestly. |
| `linux-dmabuf` (unstable copy) | IMPOSSIBLE as specified | Deprecated duplicate; same verdict and replacement. |
| `linux-explicit-synchronization` | IMPOSSIBLE as specified | `sync_file`/dma-fence fds; already deprecated upstream. Replacement: `VK_KHR_external_semaphore_win32`. |
| `pointer-constraints` | CAVEATS | `lock_pointer` → hide cursor + `ClipCursor` to 1px + raw-input deltas (standard FPS pattern); `confine_pointer` → `ClipCursor` to the window rect. Caveats: `ClipCursor` is silently dropped on focus loss, UAC, Ctrl+Alt+Del, and by any other process calling `ClipCursor(NULL)` — re-arm on `WM_ACTIVATE`. |
| `pointer-gestures` | CAVEATS | Precision-touchpad multi-finger contacts are **not** delivered to ordinary apps by default. Options: `WM_GESTURE`, Direct Manipulation, or raw HID digitizer reports with own finger tracking (what Chromium does). 3-/4-finger swipes are usually consumed by the shell first. Pinch/2-finger swipe/hold achievable; system-reserved gestures not. |
| `primary-selection` | POINTLESS | Middle-click X11 primary selection. No Windows concept, no native app participates, and it needs the fd shim. Implementable purely Wayland-to-Wayland; will never interoperate outward. |
| `relative-pointer` | IMPLEMENTABLE | `WM_INPUT`/`RAWINPUT` with `MOUSE_MOVE_RELATIVE`. The `dx` vs `dx_unaccel` split maps naturally to accumulated `WM_MOUSEMOVE` vs raw deltas. |
| `tablet` (v1/v2 unstable) | CAVEATS | Superseded by stable; identical verdict (tool yes, pad no). Some older clients still bind it. |
| `text-input` (v1/v3) | CAVEATS (**essential**) | The only way CJK/IME input works. Maps to TSF (`ITfContextOwner`, `ITfContextOwnerCompositionSink`) or legacy IMM32: `set_surrounding_text`↔`GetText`, `set_cursor_rectangle`↔`ImmSetCandidateWindow`, `preedit_string`↔composition string, `commit_string`↔`GCS_RESULTSTR`. Painful on both ends (TSF is notoriously awkward; v3 is an async serial state machine) but unavoidable. |
| `xdg-decoration` | IMPLEMENTABLE — **and it is what you want** | See §4.4. |
| `xdg-foreign` | IMPLEMENTABLE | Opaque export handles are compositor-internal; the parenting is `GWLP_HWNDPARENT`. Trivial here. |
| `xdg-output` | IMPLEMENTABLE but obsolete | Superseded by `wl_output` v4. Implement only for old clients. |
| `xdg-shell` (`zxdg_shell_v6`) | POINTLESS/legacy | Deprecated duplicate. A handful of very old clients bind it. |
| `xwayland-keyboard-grab` | POINTLESS | No Xwayland here; the need is served by `keyboard-shortcuts-inhibit`. |

### 5.5 `wlr-protocols/unstable` (10) plus 4 wlroots-only

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `wlr-layer-shell` | CAVEATS | Genuine partial equivalent: `SHAppBarMessage(ABM_NEW/ABM_SETPOS)` reserves screen-edge space that maximized windows and the real taskbar respect — **exactly `exclusive_zone`**. Layers → z-order (`background` via `WorkerW`/`Progman`; `top`/`overlay` via `HWND_TOPMOST`). Caveats: one appbar per edge; nothing draws above Start/notification flyouts; `keyboard_interactivity=exclusive` fights Win32 focus rules. |
| `wlr-data-control` | CAVEATS | Superseded by `ext-data-control`; same shim, same redundancy. |
| `wlr-export-dmabuf` | IMPOSSIBLE as specified | Framebuffers as dma-buf fds with modifiers. Also deprecated upstream in favour of `ext-image-copy-capture`. |
| `wlr-foreign-toplevel-management` | IMPLEMENTABLE | `activate`/`close`/`set_*` → `SetForegroundWindow`/`WM_CLOSE`/`ShowWindow`; `set_rectangle` → `ITaskbarList3`/`DwmSetIconicThumbnail`. Ironically **easier than on Wayland**, since Win32 needs no privilege to enumerate and control windows. |
| `wlr-gamma-control` | CAVEATS (flaky) | Shape matches `SetDeviceGammaRamp` (256×3 WORDs) but arrives as an fd. Windows clamps the ramp unless `GdiIcmGammaRange` is set in the registry, ignores it entirely in HDR/Advanced Color mode and under exclusive flip, and some drivers drop it. This is how f.lux/Redshift work, so viable but unreliable. |
| `wlr-input-inhibitor` | POINTLESS/deprecated | No user-mode process can exclusively grab all input; `BlockInput()` needs elevation and is defeated by Ctrl+Alt+Del by design. |
| `wlr-output-management` | CAVEATS | `SetDisplayConfig`/`QueryDisplayConfig` covers enable/disable/mode/position; rotation via `DEVMODE.dmDisplayOrientation`. The protocol's atomic **test-then-apply maps beautifully onto `SDC_VALIDATE` then `SDC_APPLY`**. Caveats: per-monitor fractional scale is only settable via undocumented `DisplayConfigSetDeviceInfo`; `adaptive_sync` has **no public API**. |
| `wlr-output-power-management` | CAVEATS | `SC_MONITORPOWER` blanks displays but **system-wide, not per-output**, and Windows re-wakes on any input. Honour only at whole-system granularity. |
| `wlr-screencopy` | CAVEATS | Superseded by `ext-image-copy-capture`. WGC or `PrintWindow(PW_RENDERFULLCONTENT)`; `overlay_cursor` ↔ `IsCursorCaptureEnabled`. No damage from WGC. |
| `wlr-virtual-pointer` | IMPLEMENTABLE | `SendInput(INPUT_MOUSE)`, or better, inject into our own input path — avoids UIPI restrictions and keeps synthetic events scoped to Wayland clients. |
| `virtual-keyboard-unstable-v1` *(wlroots)* | IMPLEMENTABLE | Keymap fd (shim), then synthetic key injection into our seat. |
| `input-method-unstable-v2` *(wlroots)* | CAVEATS | Same TSF conflict as above. |
| `server-decoration` *(KDE copy, wlroots)* | IMPLEMENTABLE | Worth advertising alongside `xdg-decoration` — see §4.4. |
| `drm.xml` / `wl_drm` *(wlroots)* | **IMPOSSIBLE/legacy** | DRM auth + GEM names/PRIME fds. Superseded by linux-dmabuf even on Linux. |

### 5.6 COSMIC — `pop-os/cosmic-protocols/unstable` (10 — complete)

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `cosmic-a11y-unstable-v1` | CAVEATS | Windows has magnifier, screen reader, contrast themes and colour filters, but only some are programmatically toggleable — colour filters and Magnifier state are registry + broadcast hacks with no supported API. |
| `cosmic-corner-radius-unstable-v1` | IMPLEMENTABLE | `DwmSetWindowAttribute(DWMWA_WINDOW_CORNER_PREFERENCE)` is coarse (round/small-round/none, not an arbitrary radius). If we composite ourselves, exact radius is trivial. |
| `cosmic-image-capture-source-unstable-v1` | CAVEATS | Toplevel source = `GraphicsCaptureItem` from HWND (clean); workspace source depends on our workspace implementation. |
| `cosmic-keyboard-layout-unstable-v1` | CAVEATS | `GetKeyboardLayout`/`ActivateKeyboardLayout` + `WM_INPUTLANGCHANGE`. Windows layouts are HKLs, not XKB groups (mapping table needed), and layout state is per-thread/per-window, not one global group. |
| `cosmic-output-management-unstable-v1` | CAVEATS | Same `SetDisplayConfig` story; mirroring → clone mode; adaptive sync has no public API; xwayland-primary is moot. |
| `cosmic-overlap-notify-unstable-v1` | IMPLEMENTABLE | Pure geometry over our window list; to include native Win32 windows use `SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE)` rather than polling. |
| `cosmic-toplevel-info-unstable-v1` | IMPLEMENTABLE | `ext-foreign-toplevel-list` plus state/geometry/workspace. |
| `cosmic-toplevel-management-unstable-v1` | IMPLEMENTABLE | Move-to-workspace maps to the **public** `IVirtualDesktopManager::MoveWindowToDesktop` if workspaces are backed by real virtual desktops. |
| `cosmic-workspace-unstable-v1` / `-v2` | CAVEATS | Same undocumented-COM churn as `ext-workspace`. Compositor-internal workspaces are the safe implementation. |

### 5.7 KDE — `plasma-wayland-protocols` (29 — complete)

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `appmenu` | POINTLESS | Exports a DBusMenu path for a global menu bar. No global menu bar on Windows. |
| `blur` | CAVEATS | DWM Acrylic/Mica is whole-window only; arbitrary regions mean doing it ourselves. |
| `contrast` | CAVEATS | No DWM equivalent beyond the fixed Acrylic tint. |
| `dpms` | CAVEATS | `SC_MONITORPOWER` is system-wide, not per-output. |
| `fake-input` | IMPLEMENTABLE | `SendInput`, or internal injection. |
| `fullscreen-shell` | POINTLESS | KDE's copy; same verdict. |
| `idle` | IMPLEMENTABLE | Predecessor of `ext-idle-notify`; `GetLastInputInfo`. |
| `kde-external-brightness-v1` | IMPLEMENTABLE (good match) | Monitor Configuration API (`GetPhysicalMonitorsFromHMONITOR` + `Get/SetMonitorBrightness`, i.e. DDC/CI) for external panels, `WmiMonitorBrightnessMethods` for internal. DDC/CI is slow and often badly implemented. |
| `kde-lockscreen-overlay-v1` | **POINTLESS** | The Windows lock screen is a separate **secure desktop**; nothing we own can draw on it. |
| `kde-output-device-v2`, `kde-output-management-v2`, `output-management` (v1), `outputdevice` (v1) | CAVEATS | All the same `QueryDisplayConfig`/`SetDisplayConfig` story. |
| `kde-output-order-v1` | IMPLEMENTABLE | Derive from `QueryDisplayConfig` path order. |
| `kde-primary-output-v1` | IMPLEMENTABLE | Windows has a first-class primary monitor (`MONITORINFOF_PRIMARY`). |
| `kde-screen-edge-v1` | IMPLEMENTABLE | We own hit-testing; native equivalent is `SHAppBarMessage` with `ABS_AUTOHIDE`. Good match. |
| `keystate` | IMPLEMENTABLE | `GetKeyState(VK_CAPITAL/VK_NUMLOCK/VK_SCROLL)`. |
| `org-kde-plasma-virtual-desktop` | CAVEATS | Same undocumented-COM virtual desktop story. |
| `plasma-shell` | CAVEATS | KDE's layer-shell equivalent; same appbar + z-order mapping and same ceilings. |
| `plasma-window-management` | IMPLEMENTABLE | Taskbar protocol; all maps to `EnumWindows`/`ShowWindow`/`WM_SETICON`, though it duplicates the native taskbar. |
| `remote-access` | IMPOSSIBLE as specified | Hands out GBM/dmabuf fds; also deprecated in KDE. |
| `server-decoration-palette` | IMPLEMENTABLE (**excellent match**) | Client suggests a decoration palette → `DwmSetWindowAttribute(DWMWA_CAPTION_COLOR / DWMWA_TEXT_COLOR / DWMWA_BORDER_COLOR)`. Near-exact semantics; makes native title bars match app theming. |
| `server-decoration` | IMPLEMENTABLE | KDE's pre-`xdg-decoration` SSD negotiation. **Advertise both** — some Qt/KF5-era clients bind only this one. |
| `shadow` | POINTLESS | DWM already draws the shadow on decorated windows; accept and ignore. |
| `slide` | POINTLESS | Panel slide-in animation hint. |
| `surface-extension` | POINTLESS/legacy | Old `qt_surface_extension`. |
| `text-input` (v0), `text-input-unstable-v2` | CAVEATS | Same TSF/IMM32 mapping. **v2 is what KDE-era Qt binds** — needed alongside v3 if the client set is Qt-heavy. |
| `wayland-eglstream-controller` | **IMPOSSIBLE/POINTLESS** | NVIDIA EGLStream sharing; meaningless with a forwarded Vulkan. |
| `zkde-screencast-unstable-v1` | IMPOSSIBLE as specified | Returns a **PipeWire node id**. Capture is fine (WGC), but the node id *is* the payload and PipeWire does not exist on Windows. |

### 5.8 Weston — `weston/protocol` (11 — complete)

| Protocol | Verdict | Reason / Windows mapping |
|---|---|---|
| `ivi-application`, `ivi-hmi-controller` | POINTLESS | In-vehicle infotainment shell. No analogue, no clients outside automotive. |
| `text-cursor-position` | POINTLESS/legacy | Superseded by text-input. |
| `weston-content-protection` | CAVEATS (mostly no) | Requests HDCP type 0/1. Windows' analogue is OPM, usable only from a real protected media pipeline — we cannot ask DWM for HDCP on an arbitrary window. The practical partial answer is `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE/WDA_MONITOR)`, which blocks capture but **is not HDCP**. See §8. |
| `weston-debug` | IMPLEMENTABLE | Debug stream over an fd (shim), or re-spec as a log file/named pipe. |
| `weston-desktop-shell` | CAVEATS | Weston's private shell protocol; same appbar/z-order story, and only Weston's own shell client uses it. Low value. |
| `weston-direct-display` | **IMPOSSIBLE** | Asks for direct scanout on a hardware plane. DXGI's nearest analogue (independent flip / MPO) is chosen by DWM and the driver and **cannot be requested by an app**. |
| `weston-output-capture` | CAVEATS | WGC or `BitBlt` plus the buffer shim. |
| `weston-restricted-buffer` | IMPOSSIBLE as specified | Flags a **dmabuf** as residing in hardware-protected memory. Depends on dmabuf + a DRM protected-content path. See §8. |
| `weston-test` | IMPLEMENTABLE | Test harness (move pointer, send key, activate surface). **Genuinely useful for validating the Win32 backend** — worth implementing early. |
| `weston-touch-calibration` | CAVEATS | Windows owns digitizer calibration, so applying our own matrix double-calibrates. Refuse or pass through. |

### 5.9 The truly impossible set

Every one of these fails for the same reason: **it hands out a DRM/KMS or dma-fence file
descriptor, which has no Windows referent at all.**

1. `drm-lease` — leases CRTCs/connectors for direct scanout
2. `linux-dmabuf` (stable + unstable) — PRIME fds, DRM modifiers, `dev_t`
3. `linux-drm-syncobj` — DRM syncobj timelines
4. `linux-explicit-synchronization` — `sync_file`/dma-fence fds
5. `wlr-export-dmabuf` — framebuffer as dmabuf
6. `weston-direct-display` — request a KMS plane
7. `weston-restricted-buffer` — protected dmabuf memory
8. `wl_drm` — GEM names / DRM auth
9. `wayland-eglstream-controller` — NVIDIA EGLStream
10. `zkde-screencast` / `remote-access` — PipeWire node ids / GBM fds

**Items 2, 3 and 4 are impossible only *as specified*, and they are also non-negotiable for this
architecture.** They must be replaced by a `wp_win32_buffer` over `VK_KHR_external_memory_win32`
and a timeline-sync protocol over `VK_KHR_external_semaphore_win32`. Every client must be pointed
at these instead of dmabuf — and that porting cost is larger than the compositor itself.

**Pointless rather than impossible** (no Windows meaning, or the OS already owns it):
`wl_shell`/`wl_shell_surface`, `fullscreen-shell` (×2), `ext-transient-seat`, `xwayland-shell`,
`xwayland-keyboard-grab`, `primary-selection`, `wlr-input-inhibitor`, `kde-lockscreen-overlay`,
`appmenu`, `shadow`, `slide`, `surface-extension`, `text-cursor-position`, `ivi-*`.

### 5.10 Where Windows maps *better* than Linux — prioritise these

`cursor-shape` (→ `IDC_*`, free themed DPI-aware cursors), `fractional-scale` (the 1/120 unit is
exactly `dpi*120/96`), `idle-inhibit` (→ `PowerSetRequest`), `xdg-dialog` (→ `EnableWindow`, the
native modal convention), `xdg-toplevel-tag` (→ AppUserModelID for taskbar grouping),
`wlr-output-management`'s test-then-apply (→ `SDC_VALIDATE`/`SDC_APPLY`),
`kde-external-brightness` (→ DDC/CI), `server-decoration-palette` (→ `DWMWA_CAPTION_COLOR`), and
`single-pixel-buffer` (the only buffer protocol needing no fd at all).

---

---

## 6. The tray icon

### 6.1 What is actually being asked for

**There is no Wayland tray protocol, and there is deliberately never going to be one.** The full
protocol index at [wayland.app/protocols](https://wayland.app/protocols/) — core, stable,
staging, experimental, unstable, plus the wlr / KDE / Hyprland / COSMIC / Weston / River /
Treeland vendor sets — contains nothing for tray, status icons, or a notification area. The
nearest neighbours are `ext-foreign-toplevel-list-v1` (window lists) and
[`xdg-toplevel-icon-v1`](https://wayland.app/protocols/xdg-toplevel-icon-v1) (a *window* icon,
not a tray icon).

The position is that status icons are a desktop-shell/IPC concern, not a display-server one,
which is consistent with wayland-protocols
[governance](https://chromium.googlesource.com/external/anongit.freedesktop.org/git/wayland/wayland-protocols/+/HEAD/GOVERNANCE.md)
requiring a proposed protocol to genuinely need compositor mediation. The live standardisation
work is happening in **xdg-desktop-portal** instead — see the still-open
[StatusNotifier portal issue](https://github.com/flatpak/xdg-desktop-portal/issues/266) — not in
wayland-protocols.

**One proposal does exist, and its fate is instructive:
[MR !355, `ext-tray-v1`](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/merge_requests/355)**,
by the author of the Jay compositor, opened 2024-10-17. State: **open, never merged, never closed** —
and never even triaged (no labels). Its own checklist shows the blocker: the two required
implementations exist (Jay and `wl-tray-bridge`), but of the **two required member-project ACKs, zero
arrived.** Nobody NACKed it; it was simply ignored. The design is deliberately *not* SNI — it is
surface-embedding (`get_tray_item(new_id, wl_surface)`), so the client renders the icon and popup
itself. The objections in the thread come from the GNOME/portal side, roughly "nobody wants
window-system-specific status notifiers; fix SNI or the portal instead". A companion
[issue #94, "Tray icon protocol"](https://gitlab.freedesktop.org/wayland/wayland-protocols/-/issues/94)
(2022) is also still open, and its motivation is directly relevant to you: *"The StatusNotifierIcon
standard works for clients running on the same kernel, but as it is based on D-Bus, it doesn't work
for clients running in guests."*

The D-Bus-side modernisation attempts are equally dead: [xdg-specs#84](https://gitlab.freedesktop.org/xdg/xdg-specs/-/issues/84)
stalled in January 2022 and the portal issue has no implementation. **So every tray icon on Wayland
today goes over 2009-era KDE D-Bus interfaces, and that is not going to change on your timescale.**

**So "the tray icon protocol" means StatusNotifierItem (SNI) over D-Bus**, plus
`com.canonical.dbusmenu` for the menu. Neither involves a Wayland surface at all. The good news
is that this makes the tray almost entirely independent of the rest of the compositor — it can be
built and tested on its own.

### 6.2 The interfaces

**`org.kde.StatusNotifierItem`** (verified from KDE's shipping introspection XML,
[kstatusnotifieritem](https://raw.githubusercontent.com/KDE/kstatusnotifieritem/master/src/org.kde.StatusNotifierItem.xml)):

- Read-only properties: `Category` (ApplicationStatus/Communications/SystemServices/Hardware),
  `Id`, `Title`, `Status` (Passive/Active/NeedsAttention), `WindowId`, **`IconThemePath`**,
  `Menu` (object path), `ItemIsMenu`, `IconName`, `IconPixmap` (`a(iiay)`), `OverlayIconName`,
  `OverlayIconPixmap`, `AttentionIconName`, `AttentionIconPixmap`, `AttentionMovieName`,
  `ToolTip` (`(sa(iiay)ss)` = icon-name, pixmaps, title, description).
- Methods: `ContextMenu(x,y)`, `Activate(x,y)`, `SecondaryActivate(x,y)`,
  `Scroll(delta, orientation)`, and the newer `ProvideXdgActivationToken(token)`.
- Signals: `NewTitle`, `NewIcon`, `NewAttentionIcon`, `NewOverlayIcon`, `NewMenu`, `NewToolTip`,
  `NewStatus(status)`.

Two properties matter more than their obscurity suggests: **`IconThemePath`**, which is the
escape hatch for the icon-name problem (§6.4), and **`ItemIsMenu`** — if true, the host must show
the menu on *primary* click and never call `Activate`.

**`org.kde.StatusNotifierWatcher`** at `/StatusNotifierWatcher`:
`RegisterStatusNotifierItem(serviceOrPath)`, `RegisterStatusNotifierHost(service)`; properties
`RegisteredStatusNotifierItems`, `IsStatusNotifierHostRegistered`, `ProtocolVersion`; signals
`StatusNotifierItemRegistered/Unregistered`, `StatusNotifierHostRegistered/Unregistered`.
Note the registration argument accepts either a bus name or a path — the watcher must compose the
id from the **message sender** plus the path (defaulting to `/StatusNotifierItem`), not trust the
argument.

**`org.kde.StatusNotifierHost`** is a *role*, not really an interface: take the bus name
`org.kde.StatusNotifierHost-<pid>`, call `RegisterStatusNotifierHost`, then watch the item
signals. **This is load-bearing:** clients gate on `IsStatusNotifierHostRegistered`, and if no
host is registered a well-behaved app concludes there is no tray and hides its icon. Forgetting
this produces a phantom "the tray does nothing" bug.

**Bus naming:** the spec prose says `org.freedesktop.StatusNotifier*`, but the shipping world is
universally `org.kde.*` (KDE's implementation predated adoption; the spec was never ratified).
Some clients probe the freedesktop name first. **Own both names** — cheap insurance.

**`com.canonical.dbusmenu`** (verified from the introspection XML shipped by
[Waybar](https://raw.githubusercontent.com/Alexays/Waybar/master/protocol/dbus-menu.xml) and
mirrored in plasma-workspace):
`GetLayout(parentId, recursionDepth, propertyNames) → (revision, (ia{sv}av))` where the item
struct is recursive and `recursionDepth = -1` means the whole tree; `GetGroupProperties`,
`GetProperty`, `Event(id, eventId, data, timestamp)` with `eventId` ∈
`clicked`/`hovered`/`opened`/`closed`; `EventGroup`; `AboutToShow(id) → needUpdate`;
`AboutToShowGroup`. Signals `ItemsPropertiesUpdated`, `LayoutUpdated`,
`ItemActivationRequested`. Item properties: `type` (`standard`/`separator`), `label` (`_` marks
the mnemonic), `enabled`, `visible`, `icon-name`, **`icon-data` (raw PNG bytes — note this
differs from SNI's raw ARGB)**, `shortcut`, `toggle-type` (`""`/`checkmark`/`radio`),
`toggle-state` (0/1/−1), `children-display` (`""`/`submenu`), `disposition`.

**Sequencing trap:** call `AboutToShow` *before* rendering the menu and re-`GetLayout` if it
returns true. Many apps populate their menus lazily and will otherwise hand you an empty or stale
tree.

### 6.3 What real clients use — SNI is not one option among several, it is the only one

- **Qt:** `QSystemTrayIcon` uses SNI natively on Linux. Qt's
  [documentation](https://doc.qt.io/qt-6/qsystemtrayicon.html) states it supports "all system
  tray implementations that respect the freedesktop.org [StatusNotifierItem] specification". The
  implementation is `QDBusTrayIcon` + `QDBusMenuConnection` in the QPA layer; it registers an SNI
  and exports a `com.canonical.dbusmenu` for the `QMenu`. Under Wayland XEmbed is impossible, so
  Qt is SNI-only.
- **GTK:** `GtkStatusIcon` was deprecated in GTK 3.14 and **removed entirely in GTK4**, with no
  replacement ([GTK4 migration guide](https://docs.gtk.org/gtk4/migrating-3to4.html)). GTK4 apps
  therefore use [libayatana-appindicator](https://github.com/AyatanaIndicators/libayatana-appindicator)
  or speak SNI directly — and AppIndicator *is* an SNI + DBusMenu client.

**Consequence: implementing SNI + DBusMenu gets you Qt, GTK4, Electron and everything
ayatana-based, with zero Wayland involvement.** Inventing a Wayland tray protocol would get you
nothing, because no client would use it.

### 6.4 The Windows side

**D-Bus on Windows works, and it is actively maintained.** Transports: `nonce-tcp:` (localhost TCP
plus a `noncefile=` whose random bytes the client sends before SASL auth) has always been the
Windows path — and, importantly, **`unix:` sockets now work on Windows too, since dbus 1.15.0**:
"On Windows, dbus can now use AF_UNIX sockets, not just TCP. This requires Windows 10 build 17063 or
later" (dbus `NEWS`, shipped stable in 1.16.0). Abstract sockets never.

`autolaunch:` is the Windows discovery mechanism, and the spec notes it is unusual there: "On Unix,
autolaunch addresses are connectable, but not listenable. **On Windows, autolaunch addresses are
both connectable and listenable.**" The spec's own Windows section ends with a disclaimer —
"**The recent implementation is in an early state and may not work in all cirumstances and/or may
have security issues. Because of this the implementation is not documentated yet.**" The real
mechanism is in `dbus-sysdeps-win.c`: named objects `DBusAutolaunchMutex`, `UniqueDBusInitMutex`,
`DBusDaemonMutex` and a shared-memory section `DBusDaemonAddressInfo` holding the bus address; the
daemon holds the mutex for its lifetime. The Windows-only `scope=` key (`*install-path`, `*user`, or
a literal) suffixes those names, and because they go unprefixed to `CreateMutexA`/
`CreateFileMappingA` they land in the **per-logon-session** kernel namespace.

Maintenance status is good: latest stable **1.16.2 (2025-02-27)**, with `windows amd64`,
`windows msys64 ucrt64 cmake`, `windows vs15-64` and mingw-cross CI jobs, and recent commits doing
Windows CI maintenance. Known gaps: **no system bus** on Windows, and no named-pipe transport.
Binaries via MSYS2, vcpkg, or KDE Craft (whose blueprint configures `autolaunch:scope=*install-path`).

**One interop landmine worth knowing.** GDBus (GLib) implements the *same* four shared-memory/mutex
names and, failing to find a daemon, spawns `gdbus.exe _win32_run_session_bus` running GLib's own
in-process bus. So libdbus and GDBus interoperate — whichever starts first wins. **But GLib has no
notion of the `scope=` key and only ever uses the unscoped names**, so a Craft-style
`autolaunch:scope=*install-path` bus is invisible to GDBus and vice versa. This is a strong argument
for the recommendation in §6.5: set `DBUS_SESSION_BUS_ADDRESS` explicitly and never rely on
autolaunch at all.

**Win32 tray API.** [`Shell_NotifyIcon`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shell_notifyiconw)
with `NIM_ADD`/`MODIFY`/`DELETE`/`SETFOCUS`/`SETVERSION` and
[`NOTIFYICONDATA`](https://learn.microsoft.com/en-us/windows/win32/api/shellapi/ns-shellapi-notifyicondataw)
flags `NIF_ICON`/`NIF_TIP`/`NIF_MESSAGE`/`NIF_INFO`/`NIF_STATE`/`NIF_GUID`/`NIF_SHOWTIP`. Set
`uVersion = NOTIFYICON_VERSION_4` and call `NIM_SETVERSION` **after every `NIM_ADD`** — the docs
note the version is not persisted across logoff. v4 gives `WM_CONTEXTMENU`, `NIN_KEYSELECT`,
`NIN_SELECT`, `NIN_POPUPOPEN`/`CLOSE`; these arrive in the **`lParam`** of your callback message
with the icon id in its high word and screen coordinates in `wParam` — a layout change from pre-v4
that catches people.

The mapping is clean: `NIN_SELECT`/`NIN_KEYSELECT` → `Activate`; `WM_CONTEXTMENU` →
`ContextMenu` or show the DBusMenu; middle click → `SecondaryActivate`; wheel → `Scroll`. **Also
handle the registered `"TaskbarCreated"` message and re-add every icon** — Explorer restarts are
routine and otherwise all icons vanish permanently.

**The Windows 11 problem, and it is the design-defining constraint.** New tray icons go to the
**overflow flyout** by default, and the user must drag them out or enable them in Settings.
**There is no supported API for an application to promote itself into the always-visible area.**
`NOTIFYICONDATA` has no such flag; `NIS_HIDDEN` can only hide, never force-show. The legacy routes
are dead: `EnableAutoTray` affects only old auto-hide behaviour, and the per-icon state under
`…\TrayNotify` (`IconStreams`/`PastIconsStream`) is an undocumented obfuscated blob that Explorer
caches in memory and rewrites at shutdown — and it is keyed by *executable path*, which for us
would be the compositor, not the client app. `IUserNotification` concerns balloon notifications,
not icon placement. The shell also demotes idle icons back to overflow over time.
*Note the epistemic status:* no Microsoft page says "you cannot" in those words; the evidence is
the absence of any such flag in a fully documented structure, plus consistent community
corroboration. **Treat "the icon may be invisible until the user drags it out" as a permanent
platform property and handle it in first-run UX.**

**Icon conversion has a real trap.** SNI `IconPixmap` is `a(iiay)` — an array of
(width, height, bytes), so **pick by DPI rather than taking element 0**. The pixel format is
ARGB32 in **network byte order**, i.e. bytes on the wire are `A R G B`; Windows 32bpp DIBs are
`BGRA` in memory. The conversion is therefore a per-pixel **byte reversal**, not a memcpy. Build a
top-down 32bpp `BITMAPV5HEADER` DIB section (negative `biHeight`), add a zeroed 1bpp mask, and
call [`CreateIconIndirect`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-createiconindirect)
with `ICONINFO{fIcon=TRUE, hbmColor, hbmMask}`, deleting both bitmaps afterwards.
On **premultiplication**: SNI pixmaps are conventionally *straight* (non-premultiplied) ARGB32 and
`CreateIconIndirect`'s colour bitmap wants straight alpha too, so the common case needs no alpha
maths — but Qt's internal `Format_ARGB32_Premultiplied` regularly leaks premultiplied data onto
the wire, and the `AlphaBlend`/`DrawIconEx` path *does* assume premultiplied. This could not be
settled from a normative source: **verify empirically with a 50%-alpha test icon** — a dark halo
means double premultiplication, a bright halo means none.

**The icon-by-name gap.** `IconName` presumes XDG icon-theme lookup, which does not exist on
Windows. In order of quality: (1) prefer `IconPixmap` when present — Qt and ayatana usually send
one, which alone covers most apps; (2) honour **`IconThemePath`**, the property designed exactly
for this, with a minimal XDG lookup rooted there; (3) bundle a fallback theme (Adwaita or
breeze-icons, both freely licensed) plus a small `index.theme` parser and set `XDG_DATA_DIRS` for
clients; (4) a generic icon keyed off `Category`. Never render nothing.

**DBusMenu → `HMENU`.** Walk `GetLayout(0, -1, [])` into `CreatePopupMenu`/`InsertMenuItem`:
`separator` → `MFT_SEPARATOR`, `enabled=false` → `MFS_DISABLED`, `visible=false` → skip,
`checkmark`+state 1 → `MFS_CHECKED`, `radio` → `MFT_RADIOCHECK`, `children-display=submenu` →
recurse into `hSubMenu`, `icon-data` (PNG) → decode with WIC → `hbmpItem`. Keep a map from Win32
command id back to DBusMenu item id and send `Event(id, "clicked", …)` on selection.
[`TrackPopupMenu`](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-trackpopupmenu)
needs a foreground owner window or it will not dismiss on outside-click; the canonical workaround
(Microsoft KB Q135788, reproduced in essentially every tray implementation) is
`SetForegroundWindow(hwnd)` before and `PostMessage(hwnd, WM_NULL, 0, 0)` after. Use
`TPM_RETURNCMD` for a synchronous result, and a real top-level hidden window (not `HWND_MESSAGE`,
which cannot take foreground).

### 6.5 Recommendation

**Host the bus, the watcher and the host inside the compositor, and translate to
`Shell_NotifyIcon`.**

1. Spawn a private `dbus-daemon.exe` on `nonce-tcp:` bound to 127.0.0.1 and inject
   `DBUS_SESSION_BUS_ADDRESS` explicitly into every client. **Explicit beats autolaunch** — it
   gives isolation from any other D-Bus on the machine, deterministic lifetime tied to the
   compositor, and no shared-memory scope guessing.
2. Own `org.kde.StatusNotifierWatcher` **and** `org.freedesktop.StatusNotifierWatcher` at
   `/StatusNotifierWatcher`, and register `org.kde.StatusNotifierHost-<pid>` so
   `IsStatusNotifierHostRegistered` is true.
3. One tray icon per registered SNI on a single hidden owner window; watch `NameOwnerChanged` per
   item to drive `NIM_DELETE` on client exit; re-add everything on `TaskbarCreated`.
4. Build menus from DBusMenu on demand, not eagerly, with `AboutToShow` first.

Alternatives considered: implementing a minimal D-Bus broker inside the compositor (attractive —
roughly 2k lines, removes a native dependency and a process — but you then own SASL `EXTERNAL`
/`DBUS_COOKIE_SHA1` and every client's quirks; take it only if shipping `dbus-daemon.exe` is
unacceptable); inventing a Wayland tray protocol (pointless, zero clients); a separate helper
process (adds IPC and lifetime coupling for no benefit).

**The biggest gotcha is not D-Bus — it is Windows 11 hiding the icons with no API to escape.**
Everything on the D-Bus side is tractable engineering; that one is a platform wall. Plan first-run
guidance for it, and consider offering a compositor-owned panel surface as a supplement so the
feature is not hostage to Explorer's promotion policy.

**One topology note:** if clients are Linux binaries under WSL2 or in the NixOS host
(Architecture B in §9.1), autolaunch and localhost do not transparently span the boundary. The
bus must listen somewhere reachable from the client side, or a socket must be relayed. Native
Windows clients are the easy case; plan the bus address deliberately either way.

---

## 7. Accelerated video decode

**Verdict: yes, this works, and it is the single best-suited workload for the phase-1
architecture.** Do not build a DXVA bridge.

### 7.1 The extensions and their status

All ratified (`ratified="vulkan"` in `vk.xml`); versions from Vulkan-Headers 1.4.350.

| Extension | rev | last modified |
|---|---|---|
| `VK_KHR_video_queue` | 8 | 2021-04-13 |
| `VK_KHR_video_decode_queue` | 8 | 2021-04-13 |
| `VK_KHR_video_decode_h264` | 9 | 2022-12-19 |
| `VK_KHR_video_decode_h265` | 8 | 2022-12-19 |
| `VK_KHR_video_decode_av1` | 1 | 2024-02-01 |
| **`VK_KHR_video_decode_vp9`** | 1 | **2025-06-06** |
| `VK_KHR_video_encode_queue` | 12 | 2021-04-13 |
| `VK_KHR_video_encode_h264` / `h265` | 14 / 14 | 2023-12-19 |
| `VK_KHR_video_encode_av1` | 1 | 2024-05-09 |
| `VK_KHR_video_maintenance1` | 1 | 2023-12-19 |
| `VK_KHR_video_maintenance2` | 1 | 2025-01-24 |

VP9 decode was ratified June 2025 (Vulkan 1.4.317 / SDK 1.4.321); AV1 decode landed in 1.3.277.
So the codec gap that used to argue for DXVA has largely closed.
([VP9 announcement](https://www.khronos.org/blog/khronos-announces-vulkan-video-decode-vp9-extension),
[AV1 announcement](https://www.khronos.org/blog/khronos-releases-vulkan-video-av1-decode-extension-vulkan-sdk-now-supports-h.264-h.265-encode))

### 7.2 What Windows drivers actually expose

From [vulkan.gpuinfo.org](https://vulkan.gpuinfo.org/listextensions.php?platform=windows)
(denominator is all Windows reports including very old GPUs, so these are **floors**):

| Extension | Windows coverage |
|---|---|
| `VK_KHR_video_queue` | **54.17 %** |
| `VK_KHR_video_decode_queue` | **54.17 %** |
| `VK_KHR_video_decode_h264` | **54.17 %** |
| `VK_KHR_video_decode_h265` | 50.28 % |
| `VK_KHR_video_decode_av1` | 34.14 % |
| `VK_KHR_video_decode_vp9` | 31.17 % |
| `VK_KHR_video_maintenance1` | 46.57 % |
| `VK_KHR_video_maintenance2` | 30.43 % |
| `VK_EXT_external_memory_host` *(phase 1's premise)* | 87.01 % |

The structurally interesting number: **h264 decode coverage equals `video_queue` coverage
exactly**. Every Windows implementation that exposes Vulkan Video at all exposes H.264 decode.

AMD's Windows driver page explicitly lists `video_queue`, `decode_queue`, `decode_h264`,
`decode_h265`, `decode_av1`, **`decode_vp9`**, `encode_*` and `maintenance1`
([AMD release notes](https://www.amd.com/en/resources/support-articles/release-notes/RN-RAD-WIN-VULKAN.html);
AV1 decode arrived in Adrenalin 25.10.2).

A second pass reconstructed gpuinfo's server-side API and got the per-vendor breakdown over 1175
Windows devices, independently corroborated by
[Igalia's Vulkan Video status page](https://blogs.igalia.com/vjaquez/vulkan-video-status/) — the two
agree to the exact driver version:

| Extension | NVIDIA | AMD | Intel |
|---|---|---|---|
| `decode_h264` / `h265` | 204 / 178 | 109 | 42 |
| `decode_av1` | 89 | 72 | 34 |
| `decode_vp9` | 134 | 36 | **0** |
| `video_maintenance2` | **169 (NVIDIA only)** | 0 | 0 |

Earliest supporting driver: **NVIDIA** 527.86 (H.264/H.265), 538.35 (AV1), 573.38 (VP9); **AMD**
23.7.1, 24.8.1, 25.10.03.01; **Intel** 101.4311, 101.6629, **no VP9**.

**So all three IHVs ship Vulkan Video decode on Windows for H.264/H.265/AV1.** The low percentages
reflect a corpus containing a decade of pre-Vulkan-Video hardware; any GPU you would actually pass
through has it. Do not make `video_maintenance2` a hard dependency — it is NVIDIA-only so far.
**Still worth doing first: run `vulkaninfo` in the guest against the actual passed-through GPU.**

### 7.3 What the native Windows path still offers

- **Legacy codecs**: MPEG-2, VC-1, VP8. Vulkan Video has none and no proposals. With VP9 and AV1
  ratified, this is the whole remaining gap.
- **Protected/secure decode**: the substantive difference — see §8. Unreachable here regardless.
- **Maturity**: Vulkan Video pushes *all* bitstream parsing to the application — you supply
  `StdVideoH264SequenceParameterSet`/`PictureParameterSet` and explicit slice offsets. DXVA2/MF
  do much more for you. In practice this means "the client must be FFmpeg or equivalent", which
  is fine.

Decisive point: **any native path requires a second protocol and a second frame-handoff mechanism
into the compositor.** Vulkan Video reuses transport phase 1 already built.

### 7.4 Can the proxy carry it? Yes — the API shape is unusually friendly

- **Session memory: trivially marshalable.** `vkGetVideoSessionMemoryRequirementsKHR` returns flat
  PODs in the count-then-fill idiom; `vkBindVideoSessionMemoryKHR` takes handles and scalars. The
  bound memory is opaque driver scratch the app never maps — allocate it device-local GPU-side and
  forget it. Watch `VUID-vkBindVideoSessionMemoryKHR-pBindSessionMemoryInfos-07200`: `memorySize`
  must equal the reported size, so no loose sub-allocation.
- **DPB images and `VK_EXT_external_memory_host`: host-imported memory can never back a DPB, and
  that costs you nothing.** There is no VU in the video chapter constraining a DPB image's
  `memoryTypeBits`, so this is not a spec question — but two facts settle it in practice. From the
  Vulkan memory chapter, on `VkMemoryHostPointerPropertiesEXT::memoryTypeBits`: it "**should** only
  include bits that identify memory types which are **host visible**", and "**Implementations
  running on general-purpose operating systems should not support importing host pointers for
  memory types which are not host-visible.**" So on Windows, host-imported ⊆ host-visible. And on
  NVIDIA Windows the device-local and host-visible type sets are *disjoint*, with the driver
  further restricting `memoryTypeBits` per resource class. FFmpeg confirms what real drivers want:
  `hwcontext_vulkan.c` allocates all video frames, DPB included, `DEVICE_LOCAL`.
  **It is moot anyway** — DPB surfaces are `VK_IMAGE_TILING_OPTIMAL` and read-modify-written by
  fixed-function hardware at huge bandwidth; decoding into shared host memory would be a
  performance disaster even where legal. The proxy has a free choice here: only memory the client
  actually maps needs to be in the arena, and no real client maps a DPB or video-session memory, so
  the GPU side just does a real `vkAllocateMemory` and returns an opaque handle.
  *Honest caveat:* the argument is strong but inferential — no published measurement of what
  NVIDIA/AMD Windows report for a `VIDEO_DECODE_DPB` image was found. It is ~50 lines of test code
  to settle. Note also that `video_maintenance1`'s `VK_IMAGE_CREATE_VIDEO_PROFILE_INDEPENDENT_BIT_KHR`
  covers decode *output* and encode input pictures — **explicitly not the DPB**.
- **Bitstream buffers: yes, and this is the only hot path that matters.** The compressed input is
  a `VkBuffer` with `VK_BUFFER_USAGE_VIDEO_DECODE_SRC_BIT_KHR`. It must normally carry a
  `VkVideoProfileListInfoKHR`, but `VK_KHR_video_maintenance1` adds
  `VK_BUFFER_CREATE_VIDEO_PROFILE_INDEPENDENT_BIT_KHR`, which decouples it from the profile and is
  far friendlier to a generic proxy buffer pool. Respect
  `minBitstreamBufferOffsetAlignment`/`minBitstreamBufferSizeAlignment` *and*
  `minImportedHostPointerAlignment` simultaneously.
- **StdVideo structs: POD where it counts.**
  - *Per-stream* (created once, deep but cheap): `StdVideoH264SequenceParameterSet` nests to depth
    3 (`pOffsetForRefFrame`, `pScalingLists`, `pSequenceParameterSetVui` → `pHrdParameters`);
    `StdVideoH265SequenceParameterSet` has **seven** pointers and nests to **depth 4** — the worst
    case in the whole API.
  - *Per-frame (the hot path)*: **`StdVideoDecodeH264PictureInfo`, `StdVideoDecodeH264ReferenceInfo`
    and the H.265 equivalents contain no pointers at all — flat memcpy.** AV1 has 8 pointers per
    frame plus variable-length arrays in `StdVideoAV1TileInfo`; VP9 has 3. Even AV1 is a few
    hundred bytes of fixed-shape deep copy per frame, which is noise beside one 4K frame.
- **`vkCmdDecodeVideoKHR` and reference slots: fully marshalable.** `VkVideoDecodeInfoKHR`,
  `VkVideoReferenceSlotInfoKHR` and `VkVideoPictureResourceInfoKHR` are bounded arrays of scalars
  and handles — structurally identical to `vkCmdBindDescriptorSets`, which the proxy already
  carries. The codec `pNext` adds `uint32_t` offset/size arrays with explicit counts.
- **Proxy-specific win:** `VK_KHR_video_maintenance2` adds
  `VK_VIDEO_SESSION_CREATE_INLINE_SESSION_PARAMETERS_BIT_KHR`, letting SPS/PPS ride inline in the
  decode command instead of through a `VkVideoSessionParametersKHR` object — this removes a whole
  class of object-lifetime and update-aliasing bookkeeping. 30 % Windows coverage; use when
  present, keep the object path as fallback.

### 7.5 The topology argument — and it is decisive

The insight holds, and it **inverts the usual weakness of a forwarding Vulkan.**

Nothing in Vulkan requires an image's memory to be host-visible. The `VkImage`, `VkImageView`,
`VkDeviceMemory` and `VkVideoSessionKHR` for the DPB and decode output live in the GPU-side
Windows process, in real VRAM. The app-side handle is what it already is for every other image: an
opaque 64-bit handle. Phase 1's `VK_EXT_external_memory_host` machinery simply does not
participate.

So the flow is: compressed bitstream (small) crosses the arena → `vkCmdDecodeVideoKHR` is a
marshalled command record carrying no pixel data → the decoded frame is created, referenced and
consumed entirely GPU-side. **At 4K NV12 that is a few hundred KB of coded data per frame instead
of 12.4 MB of pixels.** The one thing a forwarding Vulkan is bad at — bulk pixel movement — is
precisely what decode does not need. And because the compositor is *also* on the Windows side, the
decoded frame never has to come back at all.

Running a shader on the decoded frame does not break this: the dispatch is forwarded too and
executes GPU-side against the same `VkImage`. You need `VkSamplerYcbcrConversion` with an
immutable sampler for `VK_FORMAT_G8_B8R8_2PLANE_420_UNORM`, and the usual
`VK_IMAGE_LAYOUT_VIDEO_DECODE_DPB_KHR`/`_DST_KHR` transitions — all ordinary forwarded calls.

**What does break:**
1. **CPU readback of decoded pixels** — `vkMapMemory` on decode output, or
   `vkCmdCopyImageToBuffer` into host-visible memory, costs the full ~12 MB/frame across the arena.
   That is FFmpeg *without* `-hwaccel_output_format vulkan`. Make this an explicit slow path.
2. **The compositor buffer handoff — this is where the real risk lives, and it is not a Vulkan
   Video problem.** On Linux, FFmpeg/mpv export Vulkan video frames via
   `VK_EXT_external_memory_dma_buf`/`VK_KHR_external_memory_fd` and hand them to the compositor as
   `zwp_linux_dmabuf_v1` buffers. **In this topology there are no dmabufs.** A client that insists
   on the dmabuf protocol for decoded frames cannot be served; it must present through the proxy's
   own Vulkan/WSI handle path — i.e. the `wp_win32_buffer` replacement from §2.4 and §5.9. Solve
   this first.

**The precedent, and its absence.** Worth knowing before committing: **Mesa's Venus — the most
mature Vulkan-forwarding implementation in existence — exposes no `VK_KHR_video_*` extension at
all.** Neither does gfxstream, and vkd3d-proton does not implement D3D12 Video either. Read that as
"nobody has forwarded Vulkan Video, so there is no reference to crib from and no shaken-out driver
bugs" — not as "it cannot be done". The API shape genuinely is forwardable.

Also note the `StdVideo*` deep copy is a **codegen job, not a hand-written one**: the nested pointers
are not in `pNext` chains and carry no `sType`, so a generic pNext walker will silently corrupt them.
Every array length is a `uint8_t` sibling field, so all copies are bounded by construction.

### 7.6 What clients actually use

- **FFmpeg** is the only mature consumer: `-hwaccel vulkan` arrived in **6.1 (Nov 2023)** with
  H.264/HEVC/AV1; VP9 decode in 8.0; 9.0 ships a broad Vulkan stack. *(The 6.1 milestone is
  solid; the 8.0/9.0 specifics come from secondary sources.)*
- **mpv** gets it through FFmpeg (`--hwdec=vulkan`).
- **Firefox: not shipped** — [bug 1753129](https://bugzilla.mozilla.org/show_bug.cgi?id=1753129)
  open since 2022; Firefox uses VAAPI on Linux.
- **Chromium**: VAAPI by default on Linux; no Vulkan Video decode path.
- **VAAPI-on-Vulkan does not exist**, verified against Mesa `main`: `grep -rl
  KHR_video_decode_queue src/` returns exactly two files, both Vulkan drivers (`radv_`, `anv_`), and
  `src/gallium/frontends/va/` contains zero occurrences of "vulkan". Airlie & Blumenkrantz's
  [MR !31517](https://gitlab.freedesktop.org/mesa/mesa/-/merge_requests/31517) (VA-API over Vulkan
  Video in Zink, Oct 2024) has sat unmerged for 22 months. **Do not plan on a shim appearing.**
- Client support is thinner than the extension maturity suggests: **mpv** requires an explicit
  `--hwdec=vulkan` (it is deliberately off the auto-probe whitelist); **Firefox** shipped a Vulkan
  video decoder in 153 but **Linux/GTK only, with both prefs defaulting to `false`**, and uses
  WMF/D3D11VA on Windows; **Chromium has none at all** (no `media/gpu/vulkan/`; issue open since
  Feb 2024).

### 7.6a The door that may outrank all of this: **VA-API runs natively on Windows**

This is the finding most likely to change the plan, and it was not on the original question list.

- `libva` has had a **Win32 backend since 2.17** — `va/win32/va_win32.h` exposes
  `vaGetDisplayWin32(LUID*)`.
- Mesa builds **`vaon12_drv_video.dll`** from `src/gallium/drivers/d3d12/` — a full VA-API driver on
  **D3D12 Video** (H.264 baseline→high10, HEVC Main/Main10, AV1 Main, VP9 profile 0/2, plus encode).
  Microsoft announced it on the
  [DirectX Developer Blog](https://devblogs.microsoft.com/directx/video-acceleration-api-va-api-now-available-on-windows/)
  and ships it as the `Microsoft.Direct3D.VideoAccelerationCompatibilityPack` NuGet package.
- **FFmpeg already supports it**: `hwcontext_vaapi.c` has a `HAVE_VAAPI_WIN32` path calling
  `vaGetDisplayWin32()` with DXGI adapter-LUID enumeration.

So a mingw-built ffmpeg/mpv on Windows can hardware-decode through
**libva-win32 → vaon12 → D3D12 Video → the real driver, completely bypassing the Vulkan forwarding
layer**, with *broader* codec coverage than Vulkan Video (it gets VP8, MPEG-2, VC-1) and no proxy in
the hot path. The cost is that the decoded frame lands in an `ID3D12Resource`, so compositing it
needs a Wayland buffer type carrying a **DXGI/D3D12 shared NT handle** rather than a shared-memory
pointer — which is the `wp_win32_buffer` you need anyway (§2.4), and, per §8, the only decode path
with any protected-content future at all.

### 7.7 Recommended design

0. **Decide between two viable paths first, and note they converge.** Forwarding Vulkan Video is
   sound and reuses phase 1. But **VA-API-on-D3D12 (§7.6a) is shipping today, needs no proxy work
   at all, has broader codec coverage, and is already wired up in FFmpeg** — and both paths end up
   needing the same thing: a Wayland buffer protocol carrying a Win32 external-memory handle. Build
   that protocol first; it is the common prerequisite and the real design risk. My recommendation:
   **implement the buffer protocol, wire up VA-API-on-D3D12 as the fast path to a working demo, and
   forward Vulkan Video as the path that keeps everything inside one API.**
1. **Do not build a DXVA2/D3D11VA bridge.** It buys only legacy codecs and an unreachable
   protected-decode path. If you want a native path, `vaon12` already is one and is maintained by
   Mesa and Microsoft.
2. **Advertise exactly what the guest driver reports.** Never synthesize support.
3. **Keep every video resource GPU-side and device-local** — session memory, DPB, decode output.
   Handles only across the boundary. Never attempt host-import for a DPB image.
4. **Host-import only the bitstream buffer, as an optimisation.** Probe the `memoryTypeBits`
   intersection at init; if empty, fall back to arena → staging → device copy. The payload is
   small enough that this is not a real loss.
5. **Deep-copy StdVideo params at the marshalling layer**; prefer maintenance2's inline session
   parameters where exposed.
6. **Target FFmpeg `-hwaccel vulkan -hwaccel_output_format vulkan`** as the reference client.
7. **Solve the compositor buffer handoff first** — that, not Vulkan Video, is the design risk.

---

## 8. Digital rights management

> ## ⚠ THE CONCLUSION OF THIS SECTION IS SUPERSEDED BY §11
>
> **The verdict below is wrong.** It rests on an unchecked premise: that protected pixels would have
> to traverse the shared-memory arena. They do not, and need not — the supported design passes a
> *handle and a geometry* while the pixels stay in memory we are never given an address for.
> Microsoft's own D3D12 specification describes sharing a protected resource session "to other
> processes, **such as the compositor**", and Chromium ships exactly this architecture from a
> low-privilege LPAC sandboxed process holding no signing certificate.
>
> **The evidence gathered in this section is sound and still worth reading** — the hardware
> guarantees, the tier definitions, the least-capable-monitor rule, the VM and blocklist findings
> all stand. It is the conclusion drawn from them that was a scope error. **Read §11 for the
> corrected analysis and an implementation path.**

**Verdict up front (SUPERSEDED — see §11): hardware DRM through this compositor is not hard, it is
logically contradictory.** Software DRM works and always would have. There is a middle tier that
works only by bypassing the compositor entirely. This section gives the reasons rather than a plan,
because the plan does not exist.

### 8.1 What the tiers actually require

**PlayReady** ([Microsoft Learn](https://learn.microsoft.com/en-us/playready/overview/security-level)),
quoted:

| Level | Client implementation |
|---|---|
| SL150 | "For clients under development or under test. Not suitable for commercial content… Assets, Client Secrets, or Content Secrets are not protected at all against unauthorized use." |
| SL2000 | "For devices and applications. Assets, Client Secrets, or Content Secrets are protected through **software or hardware** means." |
| SL3000 | "**For devices only.** Assets, Client Secrets, and Content Secrets are protected through **hardware** means, using a **Trusted Execution Environment (TEE) of the processor**." |

And, critically: "Key material … and decrypted compressed and **uncompressed** video samples are
protected by leveraging hardware security." *The uncompressed frame is exactly the thing phase 1
puts in a shared arena.* Enforcement is two-sided — the licence carries `MinimumSecurityLevel` and
the client refuses to bind if its own certificate is lower.

**Widevine on Windows desktop is a red herring, and this is now settled by primary source.**
Chromium's `media/base/media_switches.cc` carries the comment:

> `// Enables hardware secure decryption if supported by hardware and CDM.`
> `// NOTE: For Windows platform, hardware secure decryption is available via PlayReady SL3000.`

and `AddHardwareSecureWidevine()` in `cdm_registration.cc` has branches only for Android and
ChromeOS — **no Windows branch**. A Media Foundation Widevine hardware CDM did ship on Windows and
was **disabled by default in February 2026**: "Since we've switched to the direct PlayReady SL3000
playback… We want to disable Widevine Hardware Secure Decryption on Windows." Widevine L1/L2/L3 are
Android OEMCrypto device levels; on desktop EME the vocabulary is robustness strings
(`SW_SECURE_CRYPTO` … `HW_SECURE_ALL`). **So on Windows: hardware DRM means PlayReady SL3000.**

**Where the Windows TEE physically lives, and why it matters for a VM.** From
[Microsoft's hardware-DRM doc](https://learn.microsoft.com/en-us/windows/uwp/audio-video-camera/hardware-drm):
"Windows implements the OEM proxy layer and transfers the serialized PRITEE function calls to a user
mode driver in the Windows Media Foundation subsystem. This will eventually get routed to either the
Windows TrEE driver or **the OEM's graphics driver**." On a discrete-GPU PC the PlayReady TEE is
**behind the graphics driver, inside the GPU** — not the CPU, not the TPM, not the hypervisor. That
is the single fact that makes the passthrough case less hopeless than it first appears.

The structural point for both: **hardware DRM means the plaintext frame is never addressable by
anything the OS or user controls.** That single sentence decides everything below.

### 8.2 The Windows enforcement machinery

[Protected Media Path](https://learn.microsoft.com/en-us/windows/win32/medfound/protected-media-path):
"The PMP runs in a protected process isolated from the media application. **The application only
has the ability to exchange command and control messages with the PMP, but does not have access to
content after it is passed to the PMP.**" and "A trusted component is one that has been
**appropriately signed by Microsoft**." Untrusted kernel modules are caught by a periodic
cryptographic handshake (`peauth.sys`) and playback stops. There is no documented third-party path
into the protected environment.

[OPM](https://learn.microsoft.com/en-us/windows/win32/medfound/output-protection-manager) "enables
an application to protect video content **as it travels over a physical connector to a display
device**", and
[`OPMGetVideoOutputsFromHMONITOR`](https://learn.microsoft.com/en-us/windows/win32/api/opmapi/nf-opmapi-opmgetvideooutputsfromhmonitor)
creates an object "**for each physical monitor** … **Each physical monitor has its own
connector**." The trust anchor is the Microsoft-signed WDDM display miniport.

*Precision about what is documented vs inferred.* The OPM API text is per-physical-monitor and the
connector-type enum has DVI/HDMI/DisplayPort/LVDS/Miracast and **no `VIRTUAL` value**. But virtual
monitors are **not architecturally excluded**: IddCx defines full OPM plumbing (`IDDCX_OPMCTX`,
`EvtIddCxMonitorOPMCreateProtectedOutput`, `…OPMGetCertificate`, `IddCxMonitorSetSrmList`). What
actually gates it is the driver holding an **X.509 OPM certificate root-signed by Microsoft**, for
which there is no public application path — see the unanswered
[Microsoft Q&A on IDD OPM certificates](https://learn.microsoft.com/en-us/answers/questions/891828/indirect-display-driver-how-to-apply-or-get-the-op).
Empirically, virtual display drivers do not do HDCP and protected content renders black.

**And Microsoft does document a VM-specific failure, in exactly one place.** The
`MF_MEDIA_ENGINE_OPM_STATUS` enum:

```
MF_MEDIA_ENGINE_OPM_ESTABLISHED            = 1   OPM successfully established.
MF_MEDIA_ENGINE_OPM_FAILED_VM              = 2   OPM failed because running in a virtual machine.
MF_MEDIA_ENGINE_OPM_FAILED_BDA             = 3   ...no graphics driver, Basic Display Adapter
MF_MEDIA_ENGINE_OPM_FAILED_UNSIGNED_DRIVER = 4   ...driver not PE signed, falling back to WARP
```

Note carefully what this is and is not: it is a *Media Engine status*, not an OPM API return code.
The OPM API itself has no VM error, and **what `OPMGetVideoOutputsFromHMONITOR` returns for a
virtual, indirect or remote monitor is not documented anywhere.** OPM behaviour under RDP is never
stated either. But the existence of `FAILED_VM` is the closest thing to a first-party answer that
exists, and it says Microsoft's media stack does have a notion of "this is a VM, no OPM".

**Note an important non-problem:** if the Windows guest owns the passed-through GPU and drives a
real HDCP monitor from it, OPM in the guest is working with a real signed driver and a real
connector. **The VM boundary is not what breaks OPM.** What breaks it is the frame never reaching
that connector as a protected surface.

### 8.3 Can a protected surface ever be read? No — and it is stated as a guarantee

[`D3D11_CONTENT_PROTECTION_CAPS`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ne-d3d11-d3d11_content_protection_caps),
`..._HARDWARE_PROTECT_UNCOMPRESSED`, verbatim:

> "The hardware supports the protection of specific resources. This means that:
> - **The contents of a protected allocation can never be read by the CPU.**
> - **The hardware can ensure a protected resource cannot be copied to an unprotected resource.**"

[`D3D11_RESOURCE_MISC_HW_PROTECTED`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ne-d3d11-d3d11_resource_misc_flag),
verbatim: "**Resource creation will fail if hardware content protection is not supported.** …
cannot be used with `D3D11_USAGE_DYNAMIC` … `D3D11_USAGE_STAGING` … **No CPU access flags can be
specified.**"

**D3D12** states it even more bluntly, from Microsoft's own engineering spec
([DirectX-Specs, ProtectedResources.md](https://raw.githubusercontent.com/microsoft/DirectX-Specs/master/d3d/ProtectedResources.md)):

> "**Hardware must protect these resources from being read by cpu code either through protected
> memory or encrypted memory. An operation may read from a combination of protected and unprotected
> resources, but when it reads from any protected resource the output must be a protected
> resource.**"

and the old escape hatch is explicitly closed: "The D3D11 design originally allowed the
`D3D11_RESOURCE_MISC_HW_PROTECTED` flag to be ignored if a DRM system wasn't initialized… **This is
no longer valid for D3D11 or D3D12 starting with WDDM 2.4.**" At the driver level the
[Content Protection DDI](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/content-protection-ddi)
requires that "the driver should explicitly **fail** the calls to its **Lock** function to lock these
surfaces while content protection is enabled."

The only sanctioned readback is `..._ENCRYPTED_READ_BACK` via `ID3D11VideoContext::EncryptionBlt` —
optional, destination must be a staging resource, and **the data arrives still encrypted**, needing
a separate key from `GetEncryptionBltKey`. That is a certified CDM's tool, not a compositor's.

So the guarantee holds at **three independent layers — creation flags, capability contract, and
driver DDI.** There is no clever route.

**So the answer to the key question is no, by hardware design rather than policy. A
hardware-protected surface arriving in a shared-memory arena is a contradiction in terms: if you
can read it, it was never protected.**

### 8.4 Vulkan protected memory — measured, and it is a dead end

**(a) Real Windows support.** From
[vulkan.gpuinfo.org core-1.1 features, Windows](https://vulkan.gpuinfo.org/listfeaturescore11.php?platform=windows):

> **`protectedMemory` — 4.1 % supported / 95.9 % unsupported.**

Every neighbouring Vulkan 1.1 feature on the same page sits at 93–99.8 %. But the aggregate
understates it. **Pulling the actual device list — all 39 of them — gives: 31 Qualcomm Adreno /
Snapdragon X, 5 Samsung Xclipse, 1 ImgTec, 1 Samsung-Xclipse-under-RADV, and 1 Intel entry with an
anomalous driver version that does not match Intel's Windows versioning.**

> **Zero NVIDIA. Zero AMD desktop. Zero real Intel desktop.** Every single entry is a
> Windows-on-Arm SoC or an Android-wrapper submission.

For contrast, on Linux it is 23 devices, 21 of them Intel — and still no NVIDIA anywhere, on any OS.
Beware one false positive: `VK_KHR_surface_protected_capabilities` shows 51.66 % on Windows, but it
is an *instance* extension that merely lets you query `supportsProtected`, which reports `VK_FALSE`
without `protectedMemory`. Its presence means nothing.

**(b) Bridge to PlayReady/Widevine: none exists, and there is no mechanism to build one.** A grep of
the entire Vulkan registry finds **zero occurrences of `playready`, `widevine`, `hdcp`, `opm`, or
"trusted execution".** Vulkan's protected memory is an abstract memory-domain concept with no key
provisioning, no licence negotiation and no output-protection binding. On Android it is backed by
the platform TEE via `AHardwareBuffer`/`EGL_EXT_protected_content`; **on Windows there is no
equivalent binding, documented or otherwise.** Windows hardware DRM is reachable only through
D3D11/D3D12 + Media Foundation + PMP.

**(c) Vulkan Video "secure decode" is a capability bit gated on the feature that does not exist.**
`VK_VIDEO_CAPABILITY_PROTECTED_CONTENT_BIT_KHR` and
`VK_VIDEO_SESSION_CREATE_PROTECTED_CONTENT_BIT_KHR` let a decoder *write into* protected memory —
but `VUID-VkVideoSessionCreateInfoKHR-protectedMemory-07189` requires the `protectedMemory` feature
*and* the capability bit. On NVIDIA/AMD Windows `protectedMemory` is absent, so the flag is
unusable. And it decrypts nothing regardless: the bitstream must already be plaintext, so the CDM
problem is entirely unsolved even where the flag works.

**(d) The no-host-mapping rule**, quoted from the
[Vulkan spec](https://registry.khronos.org/vulkan/specs/latest/html/vkspec.html#memory-protected-access-rules):

> "`VK_MEMORY_PROPERTY_PROTECTED_BIT` … specifies that the memory type **only allows device access
> to the memory** … Memory types **must not** have `VK_MEMORY_PROPERTY_PROTECTED_BIT` set and any
> of `VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT` set, or `VK_MEMORY_PROPERTY_HOST_COHERENT_BIT` set, or
> `VK_MEMORY_PROPERTY_HOST_CACHED_BIT` set."

and

> "**Protected device memory must never be visible to the host.** Values written to unprotected
> device memory must not be a function of values from protected memory."

A protected `VkImage` therefore cannot be exported to a host-mappable handle, cannot be
`vkMapMemory`'d, cannot be blitted into the arena, and cannot even be *queried*.

### 8.5 Where the chain breaks, hop by hop

The intact Windows chain is: licence server → CDM in a TEE/PE → `ID3D11CryptoSession` → hardware
decode into a `HW_PROTECTED` surface → DWM composites it *without ever reading it* → WDDM miniport
scans out → OPM-negotiated HDCP on a physical connector. Every link is Microsoft-signed or
silicon.

1. **App → forwarding Vulkan ICD.** The ICD is an unsigned third-party DLL. Protected resource
   creation would simply fail (no crypto session, no DRM init). Note also that a real CDM-based
   player never goes through a Vulkan ICD at all — it uses D3D11/12 + MF. **Broken here already.**
2. **ICD → shared arena.** The categorical break. The arena is host-visible by construction;
   protected memory "must never be visible to the host". A frame that entered the arena either was
   de-protected or was never protected. **No configuration of a shared-memory arena satisfies the
   protected-memory rules.**
3. **Arena → compositor.** A second unsigned, uncertified user-mode process reading plaintext
   frames — precisely the threat the protected process exists to prevent. Admission to the PE
   requires Microsoft code-signing under the protected-media programme plus the PEAuth kernel
   handshake.
4. **Compositor → output.** We are not DWM and not an OTA. The policy engine only negotiates with
   objects implementing `IMFTrustedOutput`; an unrecognised sink means the content is blocked.
5. **VM boundary.** Less fatal than one might assume (see below), but the guest cannot attest
   anything about the NixOS host, and if the composited desktop is ultimately displayed on the
   *host's* screen, the final hop is outside any Windows protection entirely.

**Does hardware DRM work in a VM at all?** Honestly: **this could not be established cleanly, and
it is not worth faking.** What can be said:
- With **full GPU passthrough**, the guest runs the real vendor WDDM driver on real silicon with a
  real TEE and can drive a real HDCP connector. There is no *architectural* reason SL3000 must
  fail, and this is the configuration people occasionally report working.
- With **no passthrough** (Hyper-V synthetic video, virtio-GPU, VMware SVGA, RDP/vGPU) there is no
  OPM-capable miniport and no GPU secure environment; hardware DRM cannot initialise. This is the
  common case and the source of most "Netflix black screen in a VM" reports.
- **Vendor/platform blocklists dominate in practice.** Best-sourced data point: certain AMD GPUs
  were blocklisted in Chromium (inherited by Edge), blocking the PlayReady hardware DRM module
  despite full hardware support and HDCP 2.3 compliance, until Edge 137.0.3296.93 + KB5060999
  ([AMD community thread](https://community.amd.com/t5/graphics-cards/support-for-playready-3-0-on-rx-vega-discrete-graphics-2022-12/m-p/569155)).
  Netflix 4K on PC additionally requires Edge or the native app, specific GPU generations, and
  **all** active monitors HDCP 2.2-capable or it downgrades to 1080p.
- **No hypervisor/CPUID check was found anywhere in the PlayReady or Chromium DRM stacks.** What
  Chromium *does* check is the **display adapter**: commit `785eca0` disables hardware secure
  decryption for vendor IDs `0x0000`, `0xffff`, `0x15ad` (VMware) and `0x1414`+`0x008c` (WARP) — a
  passed-through NVIDIA/AMD/Intel ID is **not** on that list. It also requires DirectComposition and
  GPU compositing. So the gate is "is your adapter a software/emulated one", not "are you in a VM".
- Empirically there is **one** direct positive report — a
  [Proxmox forum thread](https://forum.proxmox.com/threads/can-proxmox-run-netflix-4k-hdr-on-windows-10-environment.99476/)
  describing Netflix 4K HDR working in a Windows guest with a passed-through RTX 2070 SUPER and a
  physically connected HDR monitor (the actual blocker turned out to be a missing HEVC codec, not
  DRM). No PlayReady level was verified there; it is resolution-based inference.
- **HDCP by configuration:** virtual monitor / IddCx — **no** (no obtainable OPM certificate;
  empirically black). HDMI dummy plug — **no**; it is a pull-up plus an EDID EEPROM with no HDCP
  receiver silicon. Physical monitor on the passed-through GPU — **yes, genuinely**. Looking Glass
  or any remote framebuffer — **no, twice over**: no HDCP sink, and Desktop Duplication blanks
  protected content anyway (the API documents a masked-out/blanked flag for exactly this reason).
- **One trap worth surfacing loudly.** From Microsoft's hardware-DRM doc: "Unlike software DRM,
  output protections are enforced on all monitors based on the **least capable monitor**… playback
  will fail if the license requires HDCP **even if the content is only being rendered on the monitor
  that supports HDCP**." So a Looking Glass indirect display or a dummy plug active *alongside* your
  good monitor kills hardware DRM on both.

### 8.6 The one thing that *is* offerable: `SetWindowDisplayAffinity`

From [Microsoft Learn](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity):
`WDA_MONITOR` — "The window content is displayed only on a monitor. Everywhere else, the window
appears with no content." `WDA_EXCLUDEFROMCAPTURE` — "the window does not appear at all."

And the disclaimer, verbatim:

> "This feature enables applications to protect their own onscreen window content from being
> captured or copied through **a specific set of public operating system features and APIs**.
> However, **it works only when the Desktop Window Manager (DWM) is composing the desktop.**"
> "It is important to note that **unlike a security feature or an implementation of Digital Rights
> Management (DRM), there is no guarantee** that using SetWindowDisplayAffinity … will strictly
> protect windowed content."

It is a cooperative hint enforced by DWM against well-behaved capture APIs — not HDCP, not
hardware DRM, not memory protection, and Microsoft says so on the reference page. Two extra
problems here: it is enforced *by DWM*, and for surfaces we composite ourselves **we are the
capture surface**; and it protects an `HWND`, so it only helps where a Wayland surface maps 1:1 to
a real top-level window (which, per §4, it does).

### 8.7 Tiered verdict

**(a) Achievable — ordinary software DRM.** Widevine L3, PlayReady SL150/SL2000. Decryption in
user space, decoded frames in normal memory, frames handed over like any other client buffer.
Nothing in this architecture obstructs it. Licence servers cap it — typically 720p/1080p, SD for
some catalogues, never 4K, and no HDR/Dolby Vision on most services.
*Caveat worth stating:* even SL2000 carries contractual robustness rules. A compositor that
publishes plaintext frames into an arena readable by other processes violates those in spirit even
where technically permitted. For a shipping product rather than a personal setup, that is a legal
question, not a technical one.

**(b) Achievable only by bypassing the compositor's pixel path — but there is a real design here.**
PlayReady SL3000 *if* the player renders on the passed-through GPU with a real HDCP 2.2 monitor
physically attached to it and **no virtual display active anywhere** (the least-capable-monitor
rule). The architecture that makes this work is the one Chrome and Edge already use: keep the frame
inside PMP, obtain an **opaque DirectComposition surface handle** via
`IMFMediaEngineEx::GetVideoSwapchainHandle()`, pass the *handle* — never pixels — across processes,
and let the compositor insert it into a DComp visual tree so **DWM** composites it.

This dovetails with the DirectComposition recommendation in §9.2: if you ever want tier (b), your
compositor gains a path that delegates a subsurface to a DComp visual **it never samples** — a hole
punch, not a texture. That is a genuine design rather than a fantasy, but it is substantial work,
and those pixels are then outside your control entirely: no effects, no transforms, no screenshots,
no `viewporter` scaling. Whether it then actually engages still depends on GPU generation and
browser blocklists, and cannot be promised from here.

**(c) Categorically impossible: hardware DRM *through* this compositor.** Not "hard", not
"unsupported today" — contradictory. Hardware protection guarantees that protected allocations
"can **never** be read by the CPU" and cannot be copied to unprotected resources (D3D), and that
protected memory "**must never** be visible to the host" (Vulkan). This architecture's central
mechanism is a host-visible shared arena. Both cannot hold. And even setting the arena aside,
entering the Protected Environment requires Microsoft-signed components plus a PEAuth kernel
handshake with no third-party path in, and **there is no Vulkan↔PlayReady/Widevine bridge on
Windows at all** — so a Vulkan-based compositor has no route to hardware DRM even in principle,
quite apart from `protectedMemory` sitting at 4.1 %.

**User-visible consequence.** Netflix/Disney+/Prime in a browser in this setup will play at
Widevine L3 / SL2000 quality — typically 720p, sometimes 540p, occasionally 1080p in Edge with
software PlayReady. No 4K, no HDR. Some services will refuse outright rather than downgrade;
Netflix in Edge may show an error rather than degrade, because its 4K path expects hardware DRM.
**Nothing implemented in the compositor changes that number** — the cap is set by the licence
server from a client certificate that cannot be obtained.

### 8.8 Recommendation

Implement the capture-exclusion flag and **call it exactly what it is — "screen-capture
exclusion", not "DRM"**: a Wayland protocol carrying a do-not-capture flag that the compositor
honours in its own screencopy/screenshot paths, plus `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`
on the outer Win32 window so Windows-side capture APIs skip the compositor. Document it as policy,
not as a security boundary.

**Do not build any protected-memory or crypto-session plumbing.** It cannot be completed, and a
half-built version implies a guarantee that cannot be honoured.

**Two cheap experiments worth more than anything published**, both runnable in the guest in ten
minutes: (1) `navigator.requestMediaKeySystemAccess('com.microsoft.playready.recommendation.3000', …)`
in the guest browser, then check `chrome://media-internals/#cdms` — its status enum
(`kDisabledBySoftwareEmulatedGpu`, `kGpuCompositionDisabled`, `kDisabledOnError`) names precisely
which gate is closing. (2) A ~50-line Vulkan program checking whether a `VIDEO_DECODE_DPB` image's
`memoryTypeBits` excludes host-visible types on your actual driver (§7.4). Both settle questions
this report could only answer inferentially.

---

## 9. Recommended architecture, staging, and the three biggest risks

### 9.1 The decision that shapes everything else

Before any code: **choose where the clients live.** There are two coherent architectures and
they differ by an order of magnitude in cost. Everything else in this report is downstream of
this choice.

**Architecture A — "native clients" (what was asked for).** Wayland clients are mingw-compiled
Win32 processes speaking a patched Wayland to a Win32-native compositor over a named pipe.
- Pro: no second OS, lowest latency, real native integration, direct use of phase 1.
- Con: requires porting each toolkit's Wayland backend to Windows (§2.7). GTK4 and Qt6 both
  refuse to build theirs on Windows today. This is the dominant cost and it is per-toolkit.

**Architecture B — "cocoa-way model".** Clients stay Linux (in the NixOS host, a container, or
WSL2) and speak unmodified Wayland; a proxy re-materialises fds at the boundary; the compositor
is Win32-native and renders with phase-1 Vulkan.
- Pro: **zero client porting.** Every existing Linux app works unmodified, today. The fd problem
  is solved at one place by one component. This is the architecture with a working precedent.
- Con: a second OS in the loop, added latency, buffer transport crosses a VM boundary.

**My recommendation: build B first, keep A as the optimisation.** They share the compositor —
the Win32 backend, all the protocol implementations, decorations, the tray bridge, the renderer.
They differ only in the transport module. B gets you a working, testable, useful system in
stage 3 instead of stage 9, and it de-risks the one item most likely to sink the project. If the
toolkit ports later succeed, the same compositor serves native clients with no rework.

> **Caveat added by §11, and it is the one thing that could overturn this recommendation.**
> Hardware DRM (PlayReady SL3000) requires the client to be a **Windows process able to call Media
> Foundation**, because the licence is bound to local hardware attestation that cannot be forwarded
> (§11.4). **Architecture B can therefore never play protected 4K content, no matter what the
> compositor does.** If protected playback is a real requirement rather than a nice-to-have, that is
> a decisive argument for Architecture A and this staging order should be revisited. Note the
> tension: Architecture A is also the topology in which the Vulkan forwarder has the *least* value
> (see the "logical tension" subsection below). These two goals genuinely pull apart, and the choice
> should be made deliberately rather than discovered in month four.

### There is a logical tension in the stated goal that is worth surfacing now

Phase 1 exists because the **NixOS host has no GPU** — the GPU was passed into the Windows guest.
Its whole purpose is to give processes on the *Linux* side a working Vulkan ICD backed by the
driver on the *Windows* side. "App side" = Linux host; "GPU side" = Windows guest.

Now apply that to the two architectures:

- **Architecture A (mingw-native clients).** The client is a Windows process, running on the same
  machine as the real driver. It does not need a forwarder — it can call the native Vulkan driver
  directly. **Phase 1 is largely redundant in Architecture A.** The stated goal asks to "use the
  same shared vulkan we just impl before", but in the native-client topology there is nothing to
  share across.
- **Architecture B (Linux clients).** The client is on the Linux host with no GPU. It uses the
  phase-1 ICD, renders into the shared arena, and the Windows-side compositor picks the buffer up
  from that same arena — **zero copy, because both sides already map it.** Phase 1 is not a
  compromise here, it is the load-bearing component.

So the two headline requirements — "run mingw-compiled clients" and "use the shared Vulkan from
phase 1" — **pull in opposite directions.** They are not contradictory (you could support both
transports), but pursuing A first spends the largest cost in the project to obtain a topology in
which phase 1 has the least value.

The topology also favours B on its own merits: the GPU lives in the Windows guest, the NixOS host
is right there, and "Linux clients, Windows compositor" is the natural shape of this machine.

### 9.2 Component architecture

```
                Windows 11 guest (owns the GPU)
  ┌──────────────────────────────────────────────────────────┐
  │  Compositor process                                      │
  │  ┌────────────────┐  ┌─────────────────────────────────┐ │
  │  │ Win32 backend  │  │ Wayland frontend (forked        │ │
  │  │  HWND per      │  │ smithay + wayland-server)       │ │
  │  │  toplevel      │◄─┤  xdg-shell, seat, shm, output…  │ │
  │  │  message pump  │  └─────────────────────────────────┘ │
  │  │  (own thread)  │            ▲                         │
  │  └───────┬────────┘            │ calloop (sockets only)  │
  │          │ PingSource          │                         │
  │          ▼                     │                         │
  │  ┌─────────────────────────────┴───────────────────────┐ │
  │  │ Transport: named pipe + typed handle table          │ │
  │  │   Section / Pipe / VkMemory / VkSemaphore tokens    │ │
  │  └─────────────────────────────────────────────────────┘ │
  │  ┌─────────────────────────────────────────────────────┐ │
  │  │ Renderer: Vulkan (phase 1 GPU side = real driver)   │ │
  │  └─────────────────────────────────────────────────────┘ │
  │  ┌─────────────────────────────────────────────────────┐ │
  │  │ SNI bridge: D-Bus session bus → Shell_NotifyIcon    │ │
  │  └─────────────────────────────────────────────────────┘ │
  └──────────────────────────────────────────────────────────┘
```

Key structural decisions:

1. **Named pipe, not AF_UNIX**, as the Wayland transport. Windows AF_UNIX buys nothing here (we
   cannot pass fds over it either) and named pipes give `GetNamedPipeClientProcessId` for the
   `DuplicateHandle` path. AF_UNIX's only advantage is that unmodified client code paths expect
   a socket — but we are patching the client library regardless.
2. **The fd type is carried, never inferred.** A typed handle table (§2.3) with an explicit tag
   per token. Do not repeat waypipe's protocol-parser cost.
3. **`wl_shm` pools live in the phase-1 arena** by default. `SEC_RESERVE` + incremental commit
   for growable pools.
4. **The Win32 message pump runs on its own thread** and feeds calloop through a `PingSource`,
   because calloop on Windows can only poll sockets (§1.2). Corollary: the modal resize loop
   (`WM_ENTERSIZEMOVE`) blocks only that thread, not the Wayland loop — which is a strong
   argument for this split beyond the polling constraint alone.
5. **One `HWND` per `xdg_toplevel`; present through DirectComposition, and make subsurfaces
   DComp visuals.** This is the strongest single mapping I found in the whole report, and it is
   worth stating explicitly because the obvious alternatives are both bad (child `HWND`s break
   Wayland's atomicity and clipping; manual compositing into the parent's buffer throws away the
   hardware).
   - `IDCompositionDesktopDevice::CreateTargetForHwnd` binds a visual tree to the toplevel's HWND.
   - Each `wl_surface` in the subsurface tree becomes an `IDCompositionVisual` whose content is a
     DXGI swapchain or DComp surface (`IDCompositionVisual::SetContent`), with the offset and
     clip set from `wl_subsurface.set_position` and the viewport.
   - **DComp's commit model is atomic, which is exactly Wayland's commit model.** From Microsoft:
     > "Calls to DirectComposition methods are always batched and executed atomically as a single
     > transaction. Calls take effect only when `IDCompositionDevice::Commit` is called, at which
     > time all pending method calls for a device are executed at once."

     So `wl_surface.commit` on a synchronised subsurface tree maps onto one `Commit()`. The
     hardest part of subsurface semantics — applying a whole tree's state atomically — is handled
     by the OS rather than by us.
   - Caveat 1: multiple DComp devices commit independently ("the batch of commands might not take
     effect at the same time"), so use **one device for the whole compositor**, not one per
     window.
   - **Caveat 2, and it is a real cost: Vulkan cannot present into a DComp visual directly.**
     `VK_KHR_win32_surface` + `VK_KHR_swapchain` bind a swapchain to an `HWND`, and there is no
     Vulkan extension for composition swapchains. DComp content must come from a DXGI swapchain
     created with `IDXGIFactory2::CreateSwapChainForComposition` (flip model only) or
     `IDXGIFactoryMedia::CreateSwapChainForCompositionSurfaceHandle`. So the renderer needs a
     **Vulkan↔D3D interop layer**: render in Vulkan into a shared texture via
     `VK_KHR_external_memory_win32`, then alias or copy it into the DXGI composition swapchain.
     This is well-trodden — DXVK does exactly this in production — but it is a component to
     budget for, and it means the presentation path is D3D even though the rendering path is
     Vulkan. If that interop proves troublesome, the fallback is one plain `HWND` swapchain per
     toplevel with subsurfaces composited manually, losing the atomicity win above.
6. **`linux-dmabuf` is not implemented.** `wp_win32_buffer` + a Win32 timeline-sync protocol
   replace it. Advertise neither dmabuf nor drm-syncobj.

### 9.3 Staged implementation order

Each stage is independently testable and produces something that demonstrably works.

| Stage | Deliverable | Test that proves it |
|---|---|---|
| **1. Transport spike** | Named pipe + typed handle table + patched `wayland-backend` socket layer. No compositor. | A trivial client sends a `wl_shm` pool through the shim; the server reads the pixels back. This is the go/no-go on §2 and it is small — days, not months. |
| **2. Build the fork** | `wayland-backend`, `wayland-server`, `smithay` core compiling for `x86_64-pc-windows-gnu`. Stub out `dmabuf`, port `utils/clock.rs`, `sealed_file.rs`, `shm/pool.rs`. | `cargo build --target x86_64-pc-windows-gnu` succeeds. Verify windows-gnu specifically, since calloop's CI is MSVC-only. |
| **3. Headless compositor** | `wl_display`/`registry`/`compositor`/`shm`/`seat`/`output`/`xdg-shell`, rendering to an offscreen buffer. No windows yet. | `weston-test` protocol + a headless client committing a buffer; assert pixels. |
| **4. First real window** | `backend_win32`: one `HWND` per toplevel, `WS_OVERLAPPEDWINDOW`, blit shm to the client area. Pointer + keyboard. `cursor-shape`. | `weston-terminal` or a GTK app (via architecture B) appears in a real Windows window with a real title bar, and accepts input. **This is the milestone that proves the whole thesis.** |
| **5. Correctness pass** | `xdg-decoration` (+ KDE `server-decoration`), `fractional-scale`, per-monitor DPI, `wl_output` from `EnumDisplayMonitors`, popups + `xdg-positioner`, subsurface compositing, resize negotiation. | A GTK app and a Qt app both behave correctly at 150% DPI on a two-monitor setup with menus. |
| **6. Vulkan renderer + the Win32 buffer protocol** | Replace the shm blit with phase-1 Vulkan compositing; define and implement `wp_win32_buffer` (external-memory NT handle) and Win32 timeline sync. | A Vulkan client renders zero-copy; measure against the shm path. **This protocol is the common prerequisite for accelerated video (§7) and for any tier-(b) DRM path (§8) — do not defer it.** |
| **6b. Accelerated video** | Either forward Vulkan Video, or wire VA-API-on-D3D12 (§7.6a), both landing frames via stage 6's buffer protocol. | `ffmpeg -hwaccel vulkan` or `-hwaccel vaapi` decodes and the frame composites without a CPU round-trip. |
| **7. Integration protocols** | Clipboard + DnD (OLE bridge), `text-input`/TSF, `idle-inhibit`, `xdg-activation`, `xdg-dialog`, `xdg-toplevel-tag`, foreign-toplevel. | Copy/paste and drag/drop both directions between a Wayland client and Notepad/Explorer. CJK input works. |
| **8. Tray** | D-Bus session bus + SNI watcher/host + `Shell_NotifyIcon` bridge + DBusMenu → `HMENU`. | An unmodified Qt app's `QSystemTrayIcon` appears in the Windows tray with a working context menu. |
| **9. The long tail** | The remaining ~100 viable protocols from §5, in value order (§5.10 first). | Per-protocol conformance tests. |
| **10. Native clients (optional)** | Architecture A: port a toolkit's Wayland backend to Windows. | The same app, mingw-native, with no Linux in the loop. |

Stages 1–4 are the real project risk. Stages 5–9 are large but linear and each is independently
shippable.

### 9.4 The three biggest risks

**Risk 1 — the client side, not the compositor side.** (Probability: high. Impact: schedule-fatal
for Architecture A.) There is no "Linux Wayland app compiled for mingw" today, and making one
means porting GTK's or Qt's Wayland backend to Windows — tens of thousands of lines each,
recurring per toolkit, and against upstreams that have explicitly disabled these builds. As a
calibration point: smithay-client-toolkit
[#292](https://github.com/Smithay/client-toolkit/issues/292), "Support memfd on FreeBSD", is
**still open** — and FreeBSD is a vastly smaller gap than Windows.
*Mitigation:* build Architecture B first. It removes this risk from the critical path entirely.

**Risk 2 — the smithay fork becomes unmaintainable.** (Probability: high. Impact: compounding.)
The blockers are in smithay's *core*, not its backends, so the fork touches files that change
constantly upstream. cocoa-way vendors a patched Smithay rather than tracking upstream, which is
the honest signal about how this goes. Every upstream release costs a merge.
*Mitigation:* push the genuinely portable fixes upstream (making `xkbcommon` and `reis` optional,
feature-gating `backend::allocator::dmabuf`, replacing `rustix::time` with `std::time` in
`utils/clock.rs`). These are small, defensible, and benefit the BSD porters too — and there is
real evidence upstream accepts exactly this class of patch: smithay
[#867](https://github.com/Smithay/smithay/pull/867) "Fixes for building on FreeBSD",
[#911](https://github.com/Smithay/smithay/pull/911) "Fix build of `backend_vulkan` and `xwayland`
on FreeBSD", and [#1027](https://github.com/Smithay/smithay/pull/1027) "portability: Fallback for
`SealedFile`" were all merged, all authored by a System76/COSMIC developer. Keep only the Windows-specific transport in the fork. Consider
whether smithay is load-bearing at all: `wayland-server` + `wayland-protocols` plus own state
management is a smaller fork than smithay's core, and smithay's main value (DRM/libinput/udev
backends, the desktop-space helpers) is the part we cannot use anyway. **This is worth a serious
look before committing** — the user asked for "the same libraries COSMIC uses", but the specific
library that carries the most value on Linux carries the least here.

**Risk 3 — decoration and resize semantics never fully reconcile.** (Probability: certain for a
subset. Impact: permanent quality ceiling.) Two independent problems that do not have clean
fixes: (a) Wayland's configure/ack_configure negotiation versus Win32's synchronous
`WM_ENTERSIZEMOVE` push loop, which will produce visible lag or letterboxing during interactive
resize; (b) GTK's client-side-decoration stance, which means a class of apps will never get
native title bars no matter what we advertise.
*Mitigation:* accept it and make the fallback good — per-app detection with a `WM_NCCALCSIZE`
borderless window (which preserves snap and resize, unlike `WS_POPUP`) for CSD clients. Do not
promise uniform native decoration; promise it where the client cooperates.

**Honourable mention — clock domains.** `presentation-time`, `input-timestamps` and the client's
`clock_gettime(CLOCK_MONOTONIC)` must all share an epoch or the numbers are silently garbage and
every frame-pacing bug becomes unfalsifiable. Decide on QPC on day one and thread it everywhere.
Cheap to get right up front, expensive to retrofit.

---

## 10. What I could not establish

Collected here because you asked to be told plainly rather than have gaps papered over.

*Items 1–10 predate §11 and are unchanged. Note that item 3 has been promoted: it is now the
go/no-go experiment for a tier §11 concludes is implementable, not a footnote to an impossibility.*

**Unverified, but cheap for you to settle on your own hardware:**
1. **Which Vulkan Video extensions your actual passed-through GPU exposes.** Run `vulkaninfo` in the
   guest. One command, worth more than every aggregate in §7.2.
2. **Whether a `VIDEO_DECODE_DPB` image's `memoryTypeBits` excludes host-visible types on your
   driver.** The spec does not mandate it; the argument in §7.4 is strong but inferential. ~50 lines
   of test code.
3. **Whether PlayReady SL3000 engages in your guest at all.** `chrome://media-internals/#cdms` after
   a `requestMediaKeySystemAccess` call names the exact gate that closes (§8.8).
4. **Whether the smithay stack builds for `x86_64-pc-windows-gnu` specifically.** calloop's CI is
   MSVC-only; `windows-sys` ships import libs so gnu *should* work, but nobody has demonstrated it.

**Established only by inference or field reports, not by a first-party statement:**
5. **That OPM fails on indirect/virtual display drivers.** No Microsoft statement says so. IddCx
   actually defines full OPM plumbing; the real gate is an unobtainable Microsoft-signed certificate.
   The one first-party data point is the `MF_MEDIA_ENGINE_OPM_FAILED_VM` enum value (§8.2).
6. **That `WM_TIMER` fires inside the Win32 size/move modal loop.** This is the standard workaround
   in SDL/GLFW-class code and it works, but Microsoft documents it nowhere. Treat it as folklore, and
   prefer the two-thread design (§4.6).
7. **Premultiplication convention for `CreateIconIndirect`.** Microsoft documents premultiplied alpha
   for `BLENDFUNCTION` and says nothing about `ICONINFO`. Verify empirically with a 50%-alpha test
   icon (§6.4).
8. **The exact ordering constraint on GTK's `default_mode` event** (§4.4) is read off the code path,
   not tested.

**Blocked by tooling, and worth a human with a browser:**
9. `gitlab.freedesktop.org` and `freedesktop.org/wiki` serve anti-bot pages to automated fetching.
   The wayland-protocols MR/issue history and the canonical StatusNotifierItem wiki pages were
   reached via API endpoints and Wayback snapshots rather than directly. The `ext-tray-v1` MR !355
   finding (§6.1) came through the GitLab REST API and should be re-read in a browser before you
   rely on its current state.
10. The full D-Bus specification page truncated on fetch, so the `nonce-tcp:`/`autolaunch:` details
    in §6.4 are paraphrase rather than quotation. Read the `#transports-nonce-tcp-sockets` and
    `#meta-transports-autolaunch` anchors before implementing.


**Added by §11 — open questions on the protected-content path:**
11. **Whether MF's VM detection (`MF_MEDIA_ENGINE_OPM_FAILED_VM`) fires under full GPU passthrough.**
    The single unknown that could invalidate the whole hardware-DRM tier, and step P1 of §11.8's
    staging table for that reason.
12. **Whether a DComp surface handle can be passed to a *lower*-integrity process than the producer.**
    Chromium only ever goes LPAC → browser → GPU, i.e. upward in privilege. Our client→compositor
    direction is probably peer-to-peer, but this is untested.
13. **Whether DWM imposes extra restrictions on a non-DWM-known window hosting a protected visual.**
    Chromium's case suggests none beyond OPM policy on the output, but no documentation states it.
14. **Whether `IDCompositionEffect`/blur can be applied to a protected visual.** Undocumented in both
    directions; mechanically it would require DWM to allocate a protected intermediate. Chromium
    never attempts it. Assume no until measured.
15. **Whether the `DISPLAY_ONLY` "another process created the HWND" clause applies to windowless
    composition surfaces.** Written for windowed swapchains; our design has no HWND swapchain.
    Setting `SetWindowDisplayAffinity` anyway is cheap insurance (§11.10.4).
16. **Whether `VK_KHR_external_memory_win32` may import a `HW_PROTECTED` D3D11 resource.**
    Unspecified rather than forbidden — no valid-usage statement authorises it and no structure
    carries protection semantics. Treated as unusable (§11.9.1).
17. **Whether the screen-space rect also drives multi-adapter GPU selection.** Recorded earlier from
    a Chromium comment I could not re-confirm on re-verification (§11.10.3). Plausible, unverified.

**One conflict between research passes, resolved:** whether a standard Windows-HKL → XKB-layout
mapping table exists. One pass concluded no and that you would have to build one; the other found
FreeRDP's `xkb_layout_ids.c`, surfaced in Weston's RDP backend as `rdp_keyboards[]`. **The second is
correct** — the table exists and saves the work (§4.7).

---

## 11. DRM reconsidered — the earlier conclusion was wrong

*This section supersedes the headline verdict of §8. The factual findings in §8 stand; the
conclusion drawn from them did not. §11.1 explains what changed and why.*

### 11.1 What I got wrong, and what I got right

**Superseded:** §8's headline, "hardware DRM through this compositor is not hard, it is logically
contradictory."

**The error was a scope error**, and it is worth naming precisely so a reader can tell which parts
of §8 still stand. My argument ran:

> D3D guarantees protected allocations are never CPU-readable; Vulkan requires protected memory
> never be host-visible; our central mechanism is a host-visible shared arena; therefore
> contradiction.

**The third premise does not hold.** Nothing requires protected pixels to use the arena. The arena
is the transport for the *unprotected* data path. Under the model put to me — the GPU allocates the
protected buffer, the CPU is never told where it is, and the CPU defines only the surface on which
that content is presented — what crosses the boundary is a **handle and a geometry**. The pixels
never move.

I should have caught this, because **§8.7(b) of my own report already described the escape** — the
`IMFMediaEngineEx::GetVideoSwapchainHandle()` DirectComposition hole-punch — and I nonetheless
filed it under "achievable only by bypassing the compositor" rather than recognising it as *the*
supported way a compositor handles protected content. That framing buried the answer underneath the
impossibility claim.

**What still stands from §8**, unchanged and re-verified:
- Protected surfaces cannot be sampled, copied to unprotected resources, mapped or read back. The
  three-layer guarantee (creation flags, capability contract, driver DDI `Lock` failure) is real.
- Vulkan has no route to Windows DRM (§11.3, §11.9).
- The least-capable-monitor rule and its consequences for virtual displays.
- Widevine hardware DRM on Windows desktop is not a thing; Windows hardware DRM means PlayReady
  SL3000.

**What changes:** the conclusion, the recommended architecture, and which of the two client
topologies from §9.1 you should choose if protected content matters.

### 11.2 The model is correct, and it has a name

The model — "the GPU allocates it, doesn't tell the CPU where it is, but lets the CPU define a
surface on which the content is presented; the panel queries as black while the display shows the
content" — is the standard **protected/secure video path**, and its compositor-facing expression is
a **hole punch**:

1. The **decoder** writes into protected memory the CPU cannot address.
2. The **compositor** never receives pixels. It receives an opaque handle plus a rectangle, and
   renders a *transparent hole* in its own content where the video belongs.
3. A **trusted composer** (DWM; the display controller's overlay planes at the hardware level) puts
   the protected surface behind that hole at scan-out.
4. **Output protection** (HDCP via OPM) is established on the physical connector — see the
   correction below, because this step does *not* work the way one would assume.
5. **Capture** paths are blanked, because capture reads a separate composition, not the overlay
   plane.

#### One correction to the model, and it creates a real obligation

The natural assumption — mine, in the first draft of this section — is that DWM negotiates HDCP/OPM
on our behalf, below the compositor and invisibly. **That is wrong.**

OPM is driven by the *application's* media pipeline and is **keyed to an HWND**. From
[Output Protection Manager](https://learn.microsoft.com/en-us/windows/win32/medfound/output-protection-manager):
"OPM enables **an application** to protect video content as it travels over a physical connector…
**Before an application plays protected content, it must set the appropriate protection mechanisms
on every video output** associated with the graphics card that will display the video." Where the
protected media path is used, `mfpmp.exe` makes those calls — still on the application's behalf,
not DWM's. The binding is explicit:
[`IMFMediaEngineProtectedContent::SetOPMWindow`](https://learn.microsoft.com/en-us/windows/win32/api/mfmediaengine/nf-mfmediaengine-imfmediaengineprotectedcontent-setopmwindow)
— "Specifies the window that should receive output link protections… The Media Engine uses this
window to set link protections, using the Output Protection Manager (OPM)."

**Design consequence:** a protected surface needs a real, stable HWND positioned at the right screen
rectangle, so Media Foundation can resolve it to a monitor and drive HDCP on that connector. §11.10.4
shows that the *client* can own this window rather than us — but somebody must, and it must be
correctly positioned, which drives the protocol requirement in §11.10.3.

#### Do not conflate the two "black" mechanisms

The `SetWindowDisplayAffinity(WDA_MONITOR)` analogy is a good intuition pump for the *observable
behaviour*, but Windows has **two independent mechanisms** that both produce "visible on the panel,
black to capture", and they must not be confused:

| | `DXGI_SWAP_CHAIN_FLAG_HW_PROTECTED` | `DISPLAY_ONLY` / `SetWindowDisplayAffinity` |
|---|---|---|
| What it protects | **memory** — hardware-enforced | **capture** — DWM software policy |
| Enforced by | GPU / driver | DWM's capture composition |
| Is it DRM? | yes | **explicitly not** |

Microsoft is blunt about the second: "**unlike a security feature or an implementation of Digital
Rights Management (DRM), there is no guarantee**", and "**it works only when the Desktop Window
Manager (DWM) is composing the desktop**"
([SetWindowDisplayAffinity](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity)).
Chromium sets both flags separately, precisely because neither implies the other.

### 11.3 Reconsidering the 4.1 % figure — it measured the wrong API

§8.4 measured Vulkan's `protectedMemory` on Windows: 4.1 %, every supporting device an Arm SoC.
That number is **accurate and irrelevant to whether Windows supports hardware DRM.**

Windows' protected media path is **D3D11/D3D12 + Media Foundation**. Vulkan is not a participant —
there is no Vulkan↔PlayReady binding on Windows (§8.4(b) stands, and §11.9.1 re-confirms it from
the registry). So the correct statement is not "Windows barely supports protected memory" but:

> **Vulkan has no route to Windows hardware DRM at all — while the D3D+MF route is ubiquitous.**

Ubiquity is established by a conformance requirement rather than a survey. From DirectX-Specs:

> "**Starting with WDDM 2.4 a driver must report protected resource support in both D3D11 and D3D12
> and support cross API sharing, or it must support protected resources in neither API.**"

WDDM 2.4 is Windows 10 1803, so on any system in scope this is all-or-nothing at the driver level —
and since PlayReady SL3000 / Netflix 4K demonstrably works on ordinary consumer NVIDIA, AMD and
Intel hardware through Edge, the "all" branch is what ships. Android's `protectedMemory` at
**64.11 %** versus Windows' 4.10 % (§11.9.2) confirms the underlying capability is mass-market; it
is simply not plumbed through Vulkan on Windows desktop.

**The consequence for phase 1 is the important part: protected content does not travel through the
Vulkan forwarder at all.** It is a second, parallel path — MF/D3D produces a handle, DComp carries
it, DWM composites it. Phase 1 is not involved, which is why the arena's host-visibility was never
the obstacle.

### 11.4 Where the CDM has to live — this *is* the real constraint

The constraint is not signing. It is **which operating system the client process runs on.**

The application does **not** need to be Microsoft-signed — §11.10.1 proves this conclusively from
Chromium. The application is a *client* of the protected path, never a member of it. But it must be
**a Windows process able to call Media Foundation.** Apply that to §9.1's two architectures and they
diverge sharply:

| | Architecture A — mingw-native Windows clients | Architecture B — Linux clients |
|---|---|---|
| Client can call MF / instantiate a PlayReady CDM | **Yes** — it is a Win32 process | **No** — Linux has no PMP, no MF |
| Best achievable DRM | **PlayReady SL3000**, subject to §11.8's preconditions | **Widevine L3 / software only**, permanently |
| Protected surface handoff | DComp surface handle over a Wayland buffer type | nothing to hand off |

> **Hardware DRM is the one capability that requires the mingw-native client architecture.** It is
> the single strongest argument in this report *for* Architecture A — and it cuts directly against
> §9.1's recommendation to build Architecture B first.

#### The precise reason a Linux client cannot be rescued — and it is not the memory

It would be easy to assume the blocker is that a Linux process cannot allocate Windows protected
memory. True, but incidental. **The blocker is key binding.**

In hardware DRM the content key is delivered by the licence server *encrypted to the client's
hardware certificate* and unwrapped **inside the TEE**. It never exists in addressable memory on
either side. On Windows that TEE is reached only through Media Foundation and the PlayReady `PRITEE`
path, which routes to the graphics driver (§8.1). There is no API surface, forwarded or otherwise,
by which a Linux process participates in that exchange.

The tempting bridge — "let the Linux app acquire the licence and hand the content key to the GPU
side to load into a crypto session" — **defeats the entire mechanism**, because the key would be in
the clear on the Linux side. That is precisely the attack SL3000 exists to prevent, and no licence
server will issue an SL3000 licence to a client that cannot attest to a hardware certificate.

**This is the API that cannot be bridged, named precisely:** the CDM's licence-acquisition and
key-loading path (`IMFContentDecryptionModule` / PlayReady `PRITEE`). It cannot be forwarded because
it requires *local hardware attestation* — the client must prove to a remote server that it runs on
specific silicon with a provisioned certificate. **Forwarding is exactly what attestation exists to
detect.** Everything else in the protected path — allocation, decode, composition, output protection
— bridges or delegates cleanly. This one does not, and no engineering changes it.

Note this is a *good* negative result: we are not being asked to be trusted and failing. We are being
told the client itself must be on the trusted platform, and our forwarder is irrelevant to the
question.

### 11.5 The design principle: we are a bridge, never an enforcer

> **We do not protect anything. We forward faithfully so the hardware's own enforcement stays intact
> end to end. Our incapacity is the guarantee.**

This removes the requirement that anyone trust us. We are not a trusted component, we implement no
protection mechanism, and we need not be correct-for-security — only correct-for-function. Protected
allocations are made by the GPU-side driver, live in memory we are never handed an address for, and
cross our boundary only as opaque handles. There is no code path by which protected pixels reach a
buffer we can read, so there is nothing for us to leak, by bug or by malice.

#### 11.5.1 Microsoft's own spec names the compositor as the recipient

This is the sentence that most directly refutes my earlier conclusion. From
[DirectX-Specs, `ProtectedResources.md`](https://raw.githubusercontent.com/microsoft/DirectX-Specs/master/d3d/ProtectedResources.md),
on `ID3D12ProtectedResourceSession`:

> **"This session may be used with `ID3D12Device::CreateSharedHandle` when we need to flow
> information to other processes, such as the compositor."**

Microsoft's D3D12 specification anticipates exactly the architecture proposed here: a protected
resource session shared, by handle, into a **separate compositor process**. Cross-process handoff of
protected content to a compositor is not a loophole to be argued for — it is the documented,
intended design, and the spec calls out the compositor by name as the motivating case.

My §8 treated "a third-party compositor in the path" as a trust violation. It is not. The compositor
receives a handle and a geometry, which is precisely what the spec describes. **The hardware's
enforcement keeps the content safe regardless of who holds the handle.**

#### 11.5.2 Fail-closed — verified at four layers, and Microsoft closed the one hole that existed

The approach rests on one property: **a bridging error must cause playback to fail, never to
silently produce unprotected output.** I checked this rather than assuming it.

| Layer | Behaviour on error | Source |
|---|---|---|
| Protected resource creation | "Resource creation will fail if hardware content protection is not supported." | [`D3D11_RESOURCE_MISC_FLAG`](https://learn.microsoft.com/en-us/windows/win32/api/d3d11/ne-d3d11-d3d11_resource_misc_flag) |
| Protected → unprotected flow | **"An operation may read from a combination of protected and unprotected resources, but when it reads from any protected resource the output must be a protected resource."** | DirectX-Specs |
| CPU access | driver "should explicitly **fail** the calls to its **Lock** function" while protection is enabled | [Content Protection DDI](https://learn.microsoft.com/en-us/windows-hardware/drivers/display/content-protection-ddi) |
| Output protection | licence policy refuses, or Media Engine reports `MF_MEDIA_ENGINE_OPM_FAILED_*` and playback stops | `MF_MEDIA_ENGINE_OPM_STATUS` |

and underpinning all of it, from DirectX-Specs:

> "Hardware must protect these resources from being read by cpu code either through protected memory
> or encrypted memory."

**The decisive detail: the fail-open path used to exist and was deliberately removed.** The spec
records the old behaviour — "Creating a texture using this flag does not automatically guarantee that
hardware protection will be enabled for the underlying allocation. Some implementations require that
the DRM components are first initialized prior to any guarantees of protection." — and then closes
it: **"This is no longer valid for D3D11 or D3D12 starting with WDDM 2.4."**

That is exactly the failure mode this principle worries about — an API that silently degrades to
unprotected output when the caller gets something wrong — and Microsoft eliminated it platform-wide.
On any WDDM 2.4+ system the protected flag is honoured or creation fails.

**Conclusion: fail-closed holds at four independent layers, by explicit design rather than by
accident.** A mis-bridged call produces a black window or a playback error, not a leak.

### 11.6 Which APIs must be bridged

The honest summary is that **the path that works needs almost no bridging, while the path that would
need bridging is blocked by something other than memory.**

#### Path 1 — D3D + Media Foundation, Windows-native client (the one that works)

**Nothing crosses the VM boundary.** The client is a Win32 process in the same guest as the GPU, the
driver, the CDM and DWM. shared_vulkan is not in this path at all. The only bridging is the Wayland
hop, and it carries a handle:

| What crosses | Direction | Our obligation |
|---|---|---|
| DComp surface handle for the protected swapchain | client → compositor | bind verbatim into our visual tree; never `OpenSharedResource`, never sample |
| Surface geometry, z-order, clip | client → compositor | ordinary compositor state |
| **Screen-space rectangle** | **compositor → client** | **required — see §11.10.3** |
| Output/protection status | compositor → client (optional) | report what Windows reports; never synthesise |

#### Path 2 — Vulkan protected memory through the forwarder (well-formed, unavailable)

Recorded because it is the design this project's architecture would naturally reach for, and the
bridge would be clean:

| Call | Bridging |
|---|---|
| `vkCreateDevice` with `VK_DEVICE_QUEUE_CREATE_PROTECTED_BIT` | forward; GPU side creates a real protected queue |
| `vkCreateImage`/`vkCreateBuffer` with `VK_IMAGE_CREATE_PROTECTED_BIT` | forward; allocation GPU-side, never arena-backed |
| `vkAllocateMemory` from a protected type | forward; **never** host-import or arena-place |
| `vkQueueSubmit2` with `VkProtectedSubmitInfo` | forward verbatim |
| `vkMapMemory` on protected memory | **must fail** — protected types are never host-visible |
| `vkCreateSwapchainKHR` with `VK_SWAPCHAIN_CREATE_PROTECTED_BIT_KHR` | forward |

Fail-closed by construction: protected memory types cannot carry `HOST_VISIBLE`, so there is no
arena placement to get wrong, and the forwarder's zero-copy mechanism
(`VK_EXT_external_memory_host`) simply cannot apply — the GPU side does a real `vkAllocateMemory`,
exactly as §7.4 concluded for video DPB memory. **Unavailable on Windows for two independent
reasons** (§11.9): desktop drivers do not expose `protectedMemory`, and Vulkan has no CDM binding.

### 11.7 What the compositor must actually do

#### 11.7.1 The representation

A protected surface reaches us as a **DirectComposition surface handle**, not a buffer. The client
drives Media Foundation and obtains an opaque handle; it is duplicated across the process boundary
and we bind it with `IDCompositionDevice::CreateSurfaceFromHandle`, attaching it to a visual via
`SetContent`.

**This slots into the DirectComposition design already recommended in §9.2 with no new machinery.**
We were going to build a DComp visual tree per toplevel anyway, with subsurfaces as child visuals. A
protected video surface is simply a child visual we never sample — a pleasing outcome, since the
architecture chosen for unrelated reasons (atomic commit semantics) is the one that makes protected
content possible.

#### 11.7.2 What we may and may not do

| Operation | Allowed | Why |
|---|---|---|
| Position, z-order, clip the visual | **Yes** | DWM applies it, not us |
| Scale / transform via `SetTransform` | **Yes** | DWM applies it — keeps the surface a real window, not a fixed rectangle |
| Composite unprotected content above/below | **Yes** | ordinary visual-tree layering |
| Sample it in a shader | **No** | hardware refuses; output would have to be protected |
| Copy to a normal texture | **No** | protected → unprotected blocked in hardware |
| Map / read back | **No** | driver fails `Lock` |
| Include in screencopy, screenshot, thumbnail | **No** | must be excluded or blanked by us |
| Our own blur / effects / rounded corners | **No** | requires sampling |

The transform row matters: because *DWM* performs it, a protected surface is **not** limited to an
unscaled axis-aligned rectangle. We can place, scale and clip it like any other window. We simply
cannot *read* it.

#### 11.7.3 Mixing protected and unprotected content in one frame

1. Render our own content with a **fully transparent rectangle** where the protected video belongs.
2. Place the protected visual **behind** that content in the visual tree.
3. `Commit()` the tree atomically (§9.2). DWM composites both at scan-out.

Consequences to design for, because they are user-visible:
- The video is always **behind** our content in that region — no translucent overlay *blended* with
  it (an opaque overlay is fine, it just occludes).
- Live thumbnails, our screencopy path, `ext-image-copy-capture`, workspace previews **must skip or
  blank protected surfaces.** Not us enforcing DRM — us not producing a broken black rectangle and
  not asking for pixels the hardware will refuse.
- `wp_viewporter` crop/scale must become DComp transforms rather than shader work, for protected
  surfaces specifically.

#### 11.7.4 Protocol implications

A new Wayland buffer type is required. It cannot be `wl_shm`, and it cannot be the `wp_win32_buffer`
of §2.4 (designed to be sampled). Protected content needs a distinct type whose contract is
explicitly *"the compositor will never read this"*:

- a `wp_win32_protected_surface` carrying a DComp surface handle;
- a surface attribute marking it protected, so screencopy/thumbnail/effect paths exclude it **by
  construction** rather than by remembering to check;
- a compositor→client **screen-space rectangle** event (§11.10.3);
- **no `wl_shm` fallback** — if the handle cannot be bound, the surface shows nothing. Fail-closed at
  the protocol layer too.

#### 11.7.5 Two constraints that will silently degrade quality

**Overlay eligibility is geometry-dependent.** There is no API to request a hardware plane —
`IDXGIOutput3::CheckOverlaySupport` is a *query*; promotion is driver and DWM policy. What an app
controls is eligibility: transform should be a positive scale-or-translation, and **downscaling is
poison**. Chromium's comment names our exact case: downscaling forces "DWM to perform an extra BLT
to avoid HW downscaling", which "prevents the use of hardware overlays especially for protected
video". Keep protected visuals axis-aligned, avoid downscaling, be sparing with opacity and rounded
corners. What DWM does with hardware-protected content it *cannot* promote is undocumented; Chromium
treats it as a failure state.

**Protected sessions can be invalidated at runtime.** `ID3D12ProtectedResourceSession` carries a
status fence incremented whenever protected content becomes invalid — typically across sleep/resume,
or by driver/hardware tamper protection. Our compositor must survive a protected surface going
invalid: drop the visual, have the client re-establish. In a VM, suspend/resume and GPU reset are
more frequent than on bare metal, so this is not an edge case.

#### 11.7.6 Capture paths

Desktop Duplication documents it explicitly:
[`DXGI_OUTDUPL_FRAME_INFO.ProtectedContentMaskedOut`](https://learn.microsoft.com/en-us/windows/win32/api/dxgi1_2/ns-dxgi1_2-dxgi_outdupl_frame_info)
— "Specifies whether the desktop image might contain protected content that **was already blacked
out in the desktop image**." Note *already*: blanking happens upstream in DWM's capture composition.
Windows.Graphics.Capture honours `SetWindowDisplayAffinity` but has **no equivalent status flag**, so
a WGC consumer cannot tell content was blanked. Our `ext-image-copy-capture` implementation should
therefore mark frames containing protected surfaces itself — Windows will not tell our clients.

### 11.8 Revised verdict

**Superseding §8.7.** Tier (b) was mis-scoped: what I described as "bypassing the compositor" is in
fact *the* supported way a compositor handles protected content.

| Tier | Status | Notes |
|---|---|---|
| **Software DRM** — Widevine L3, PlayReady SL150/SL2000 | **Works, both architectures** | Unchanged from §8. Licence-capped at 720p/1080p. |
| **Hardware DRM — PlayReady SL3000, Windows-native client** | **Architecturally supported. Implementable.** | Requires Architecture A, the DComp handle path, an HWND for OPM, and the preconditions below. Not "contradictory" — that was my error. |
| **Hardware DRM — Linux client** | **Impossible, for a precise reason** | Not the memory model: licence acquisition requires local hardware attestation that cannot be forwarded (§11.4). |
| **Compositor-applied effects on protected surfaces** | **Impossible** | Requires sampling. Transforms via DWM remain available. |

#### Environment preconditions — none of it is our code

1. **A physical HDCP 2.2+ monitor on the passed-through GPU.** Not Looking Glass, not a relayed
   framebuffer — no HDCP sink.
2. **No virtual/indirect display active anywhere**, because of the least-capable-monitor rule (§8.5):
   one dummy plug or IddCx display kills hardware DRM on *all* outputs.
3. **A real vendor WDDM driver in the guest**, which full passthrough provides.
4. **The adapter must not look emulated.** Chromium blocklists `0x0000`, `0xffff`, `0x15ad` (VMware)
   and WARP; a passed-through NVIDIA/AMD/Intel ID is not on that list.
5. **DirectComposition and GPU compositing enabled**; **Windows 10 20H1+**; **x86_64, not emulated
   ARM64** (§11.10.5).
6. **Whether MF's VM detection fires.** `MF_MEDIA_ENGINE_OPM_FAILED_VM` exists (§8.2) and I could not
   establish whether full GPU passthrough avoids it. **This is the one remaining unknown that could
   invalidate the whole tier, and it is empirically testable in an afternoon.**

#### The honest summary

**The memory-model objection is withdrawn.** Protected pixels never needed to cross the arena; the
architecture that avoids it is documented, shipping in Chromium, and named in Microsoft's D3D12
spec. Our compositor is a bridge that forwards handles and geometry, safe not because we are careful
but because **the hardware never gives us an address we could leak.**

**What remains blocked is narrower and better-defined than "contradictory":** a Linux client cannot
hold an SL3000 licence, and no compositor-side engineering changes that. Everything else is
implementation work with known APIs plus environment preconditions that are properties of the
machine.

**The plan-level consequence:** if protected 4K playback matters, it is a vote for Architecture A
(mingw-native clients) against §9.1's recommendation to build B first — and note the irony that
Architecture A is also where phase 1's Vulkan forwarder has the *least* value (§9.1's "logical
tension"). These two goals genuinely pull apart. Decide deliberately rather than discovering it in
month four.

#### Staging, if pursued

Slot after §9.3's stage 6 (the Win32 buffer protocol), because it shares the DComp infrastructure:

| Step | Deliverable | Test |
|---|---|---|
| **P1** | Environment probe | `chrome://media-internals/#cdms` in the guest after a `requestMediaKeySystemAccess('com.microsoft.playready.recommendation.3000', …)`. Its status enum names the exact gate that closes. **Do this before writing any code** — a one-afternoon go/no-go on the entire tier. |
| **P2** | `wp_win32_protected_surface` + typed handle transport | Client sends a DComp surface handle; compositor binds it and shows an *unprotected* test swapchain through the same path. |
| **P3** | Protected flags + HWND/OPM wiring | `HW_PROTECTED \| DISPLAY_ONLY`, `SetWindowDisplayAffinity`, OPM window positioned via the screen-rect event. Verify `Present` succeeds. |
| **P4** | Exclusion plumbing | Protected surfaces skipped by screencopy, thumbnails, effects; capture yields black/omitted, not a crash. |
| **P5** | Real content | Protected playback at 4K in a mingw client, with resume-from-sleep session invalidation handled. |

**If P1 fails, stop.** Everything after it is unreachable on this machine, and the software-DRM tier
is what you get.

### 11.9 The Vulkan and Linux picture

#### 11.9.1 The API is shaped correctly; the platform binding is missing

Vulkan's protected memory is a strict two-world partition — exactly the shape the model describes.
The spec's guarantees, verbatim
([Protected Memory Access Rules](https://docs.vulkan.org/spec/latest/chapters/memory.html)):

> - **Protected device memory must never be visible to the host.**
> - **Values written to unprotected device memory must not be a function of values from protected
>   memory.**

A protected image can be created, bound, sampled and composited by a protected-capable queue into a
protected swapchain (`VK_SWAPCHAIN_CREATE_PROTECTED_BIT_KHR`), and nothing can legally get back out.
**This is a handles-only compositor design expressed natively in Vulkan.**

Two constraints for any future protected pass: protected submissions **forbid queries and indirect
draws/dispatches**, and implementations may support **far fewer simultaneous protected allocations**
than unprotected ones. A GPU-driven, indirect-command compositor would need a simpler dedicated path.

**What does not exist outside Android is any binding to a DRM system.** The registry contains exactly
two protection-related extensions — `VK_EXT_pipeline_protected_access` and
`VK_KHR_surface_protected_capabilities`. There is no `VK_EXT_protected_content` and no Vulkan↔CDM
interop on any desktop platform. Importing a `HW_PROTECTED` D3D11 texture through
`VK_KHR_external_memory_win32` is **unspecified** — no valid-usage statement authorises it and no
`VkExternalMemory*` structure carries protection semantics; since D3D12 makes protectedness a
property of a *session* with no Vulkan analogue, the handle alone cannot convey it. Treat as
unusable rather than forbidden.

#### 11.9.2 Android proves the model

| Platform | `protectedMemory` |
|---|---|
| **Android** | **64.11 %** |
| Windows | 4.10 % |
| Linux | 2.38 % |

Android settles what the 4.1 % figure could not: **the feature is real, shipping and mass-market —
it is simply not plumbed into Windows desktop.** Android also has the complete end-to-end binding
§8 said did not exist anywhere: `AHARDWAREBUFFER_USAGE_PROTECTED_CONTENT` maps to
`VK_IMAGE_CREATE_PROTECTED_BIT`, so MediaDrm secure decode allocates from a secure heap and Vulkan
imports it as a protected image. Exactly this model, in production, on a billion devices.

One trap: `VK_KHR_surface_protected_capabilities` shows ~51 % on Windows, but it is an *instance*
extension that merely lets you ask the question and receive "no". Neither it nor
`VK_EXT_pipeline_protected_access` indicates protected-memory support.

#### 11.9.3 The surprise: Linux desktop is overtaking Windows desktop here

The 2.38 % Linux figure is a **lagging indicator**:

- **RADV now exposes `protectedMemory` backed by AMD TMZ** (Trusted Memory Zone), landing for Mesa
  26.2 on supported Vega and Navi parts.
- **ANV exposes it via Intel PXP** (Protected Xe Path), with explicit uAPI in the Xe driver.
- **NVIDIA: nothing anywhere** (NVK has zero occurrences; the proprietary driver could not be
  established).

> **Linux desktop is acquiring Vulkan protected memory while Windows desktop still has none.**

It does not rescue the Linux-client case, for the same reason as §11.4: the secure heap now exists
with **nothing authorised to fill it.** Widevine L1 on Linux desktop remains effectively
nonexistent — a protected buffer with no CDM permitted to put licensed content in it. The mirror
image of Windows, which has the CDM and no Vulkan path.

#### 11.9.4 The existing Wayland protocol is not a model for this

`weston-content-protection` is the only Wayland protected-content protocol, was never promoted to
wayland-protocols, and **its own description disclaims our use case**:

> "This protocol is not intended for implementing Digital Rights Management on general (e.g.
> Desktop) systems, and would only be useful for closed systems. **As the server is the one
> responsible for implementing the content-protection, the client can only trust the
> content-protection as much they can trust the server.**"

That sentence is the precise articulation of why it is the wrong model. It makes the **compositor
the enforcer** — asking the server to censor surfaces and manage link protection on the client's
behalf — which is the design that requires trusting us. Our protocol should be the opposite shape:
the client obtains protection from the platform, and hands us a handle we could not misuse if we
tried.

For contrast, Linux's KMS makes the same division structurally: the `Content Protection` connector
property has an `ENABLED` value **only the driver may set** — userspace writing it gets `-EINVAL`.
Userspace may request, never assert. That is the correct division of labour, and the one to mirror.

### 11.10 The Chromium reference implementation

Chromium is the existence proof: its GPU process is not DWM, not Microsoft-signed, and not in the
Protected Environment, yet it composites PlayReady SL3000 content. Tracing its code settled three
things, one of which is a protocol requirement I had missed entirely.

#### 11.10.1 Signing: definitively not required — the proof is stronger than expected

Chrome's `MediaFoundationCdm` runs in a utility process sandboxed as an **LPAC AppContainer** —
*less* privileged than an ordinary desktop application — and it plays hardware DRM. From
[`sandbox/policy/mojom/sandbox.mojom`](https://raw.githubusercontent.com/chromium/chromium/main/sandbox/policy/mojom/sandbox.mojom):

```
// Allows LPAC capabilities for the Windws Media Foundation CDM, including
// internet and private network access, COM, Identity & others.
[EnableIf=is_win] kMediaFoundationCdm,
```

*(the `Windws` typo is upstream's, quoted as-is)*

Protection is requested by a single attribute, with no signing ceremony anywhere
([`media/renderers/win/media_foundation_renderer.cc`](https://raw.githubusercontent.com/chromium/chromium/main/media/renderers/win/media_foundation_renderer.cc)):

```cpp
RETURN_IF_FAILED(
    creation_attributes->SetUINT32(MF_MEDIA_ENGINE_CONTENT_PROTECTION_FLAGS,
                                   MF_MEDIA_ENGINE_ENABLE_PROTECTED_CONTENT));
```

**Nothing in Chromium requests, checks, or possesses a Protected Environment signing certificate.**
A low-privilege sandboxed process obtains hardware DRM by asking Media Foundation politely. This is
the strongest possible confirmation of §11.4: the application is a *client* of the protected path,
never a member of it — and neither our clients nor our compositor need be trusted.

#### 11.10.2 The protected surface need not live in the process owning the HWND

This was my main open question. Chromium answers it with three processes, none of them DWM, none
signed:

- **MediaFoundationService (LPAC utility)** — hosts the CDM and `IMFMediaEngine`, *produces* the
  surface.
- **GPU process** — owns the `IDCompositionDevice`, the DComp target bound to the browser HWND, and
  the visual tree. *Binds* the handle, never reads it.
- **Renderer** — holds a placeholder video frame with no pixels at all.

They are joined solely by a duplicated `HANDLE` carried over IPC and bound with
`CreateSurfaceFromHandle`. The producer side is:

```cpp
RETURN_IF_FAILED(media_engine_ex->EnableWindowlessSwapchainMode(true));
...
RETURN_IF_FAILED(media_engine_ex->GetVideoSwapchainHandle(surface_handle));
```

**Our compositor takes the GPU-process role and the client takes the producer role, with the handle
crossing over Wayland** via the §2 typed-handle transport, with a new `Protected` tag. The GPU-side
backing is scanout-only and write-only by construction, and Chromium's own comment notes the texture
"shouldn't be accessed via GL at all" — the only representation it produces is an overlay. **Our
incapacity is enforced by the object model, not by our discipline.**

#### 11.10.3 The requirement I missed: the compositor must feed screen-space geometry *back* to the client

This inverts the usual Wayland direction and must be in the protocol from the start.

Media Foundation renders into the windowless swapchain **at a size and position it is told**. It has
no idea where the compositor placed the visual. Chromium therefore computes the screen-space rect
and pushes it back to the producer
([`gpu/ipc/service/dcomp_texture_win.cc`](https://raw.githubusercontent.com/chromium/chromium/main/gpu/ipc/service/dcomp_texture_win.cc)):

```cpp
output_rect = window_relative_rect_;
output_rect.set_x(window_relative_rect_.x() + parent_window_rect_.x());
output_rect.set_y(window_relative_rect_.y() + parent_window_rect_.y());
...
client_->OnOutputRectChange(output_rect);
```

which the media process applies via:

```cpp
RECT dest_rect = {0, 0, rect_size.width(), rect_size.height()};
RETURN_IF_FAILED(mf_media_engine_ex->UpdateVideoStream(
    /*pSrc=*/nullptr, &dest_rect, /*pBorderClr=*/nullptr));
```

The rect is not merely cosmetic. Chromium's comment records that it must be set with "an non-empty
destination rectangle. Otherwise, the next `EnableWindowlessSwapchainMode()` call to MFMediaEngine
will skip the creation of the DCOMP surface handle." **A zero or stale rect means no surface at
all.**

**Protocol consequence:** `wp_win32_protected_surface` needs a compositor→client event carrying the
surface's **screen-space rectangle** — not surface-local coordinates, not a scale factor, but where
it landed on the desktop. Wayland deliberately never tells clients their global position; this is a
documented exception we must carve out, and it is required for correct resolution, for surface
creation at all, and for positioning the OPM window (§11.10.4).

Expect the resize to lag: the round trip is asynchronous and cross-process, so protected video will
visibly trail the window during an interactive resize. Chromium accepts this; so must we.

*One caveat on this subsection:* on re-verification I confirmed the rect computation, the client
notification and the non-empty-destination-rectangle comment directly from source. A further
rationale I had recorded earlier — that the rect also drives GPU adapter selection on multi-adapter
systems — I could **not** re-confirm in the same file. It is plausible and consistent with the
virtual-window mechanism below, but treat it as unverified.

#### 11.10.4 The OPM window — the client can own it, which lightens our obligation

§11.2 concluded a protected surface needs a real HWND for OPM. Chromium shows the lighter form: it
creates a **virtual, never-visible window** purely for that purpose —

```cpp
// To enable OPM
// (https://docs.microsoft.com/en-us/windows/win32/medfound/output-protection-manager)
// protection for video playback, We call CreateWindowEx() to get a window
// and pass it to MFMediaEngine as an attribute.

if (virtual_video_window_) {
    RETURN_IF_FAILED(creation_attributes->SetUINT64(
        MF_MEDIA_ENGINE_OPM_HWND,
        reinterpret_cast<uint64_t>(virtual_video_window_)));
}
```

— and positions it at the target screen rect. So **the client can own its own hidden OPM window**,
provided we tell it the screen rect (§11.10.3). We need not hand out our HWNDs.

This also narrows an obligation I flagged earlier. The `DXGI_SWAP_CHAIN_FLAG` docs say that for
`DISPLAY_ONLY` "windowed swap chains where another process created the HWND, the owner of the HWND
must use the `SetWindowDisplayAffinity` function appropriately in order to allow calls to
Present/Present1 to succeed." In the composition-surface (windowless) design there is no HWND
swapchain, so that clause likely does not apply — but **I could not establish this definitively**,
and calling `SetWindowDisplayAffinity` on our toplevel is harmless and cheap. Do it.

#### 11.10.5 Additional preconditions, and a clean statement of fail-closed

Chromium's Windows feature checks add constraints §8 did not list: **Windows 10 20H1 or later**,
**x86_64 only**, and **not emulated on ARM64**. The full gate list is GPU compositing enabled,
DirectComposition present, adapter not software-emulated, not driver-blocklisted, accelerated video
decode enabled — each with its own named status enum, which is why the
`chrome://media-internals/#cdms` probe in §11.8's P1 is so informative.

And the fail-closed property, stated by the reference implementation's behaviour: **if Media
Foundation decides the output is not protectable — HDCP down, capture detected, OPM refused — it
blanks the surface and the compositor simply shows nothing.** We are not consulted, we cannot
override it, and there is no path by which the content instead arrives in a buffer we can read. That
is the property the whole approach rests on, and it is enforced by the producer and the platform,
not by us.
