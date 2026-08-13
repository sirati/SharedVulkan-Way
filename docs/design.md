# waycompose — architecture and design

Written 2026-08-13, against `shared-vulkan-base/docs/phase2/research-wayland-on-windows.md`
(2851 lines, 2026-08-12), hereafter cited as **(res §n)**.

This document **decides**. Where the research surveyed options, this picks one
and says why. Where a decision is genuinely close, it says what would change it.

**Superseded passages are kept, not deleted.** Where measurement has overtaken a
decision, the original text stays in a blockquote marked
**[SUPERSEDED — <date>, see …]** and the replacement follows immediately. Half
the value of this document is the record of what was believed and why; a design
that quietly rewrites its own history cannot be audited by the person who has to
trust it later.

**Every claim here has a number or a citation behind it.** Where something is an
estimate, it says so in those words.

---

> **Stage 2 is §7.** §0 through §6 are stage 1's document and are left as they
> were written, except where a passage has been explicitly superseded. Read §7
> for what has changed: `libxkbcommon` is measured, smithay compiles for Windows,
> and a mingw-built compositor reaches `xdg_toplevel`.

> **Where the project is, as of stage 4.** An **unmodified** third-party Wayland
> client — `weston-terminal`, and `weston-presentation-shm` at 778 frames —
> paints into a window created by `wc-compositor.exe`, with a Linux proxy
> (`wc-proxy`) between them and **no file descriptor or handle crossing the
> boundary in either direction**. That is §9, and it is what makes
> `docs/goal-firefox.md` reachable at all: until it existed, §2.4's in-band `h`
> argument meant no stock libwayland client could send a buffer to the Windows
> build, and Firefox is a stock libwayland client. Sections 0–8 below are the
> stages that got there and are unchanged except where a later measurement
> contradicted them, which is marked in place.

## 0. Status: what stage 1 settled, and what it did not

The research was explicit that one question dominated the schedule risk
(res §9.3, stages 1–2): *does the Rust Wayland server stack build and run as a
Windows binary at all?* It could not answer that, because the research was
written before anyone tried. It is answered now.

**The gate passed.** `wayland-backend`, `wayland-server` and `wayland-scanner`
build for `x86_64-pc-windows-gnu`. A mingw-built `wc-server.exe` binds an
AF_UNIX socket, accepts a client, and completes a `wl_display` handshake. An
`h` argument crosses a socket that cannot carry file descriptors, and the server
maps the client's shared memory and reads the bytes out of it.

The evidence, and its exact limits, is §4. In one line: it runs under Wine, and
a **native Linux `wayland-info` — libwayland's own C client, which has never
heard of this project — enumerates the globals of the Windows binary.**

**The second question the brief asks — whether to keep smithay — is also
answered, and the answer reverses the direction the research was leaning.**
smithay fails with **56 errors in 22 files** out of 96,004 lines, against
**44,728 lines that already compile for Windows** and are exactly the compositor
logic we would otherwise have to write. Keep it, port it. §5 carries the numbers,
the two things that could still go wrong, and three factual corrections to the
research.

### 0.1 The size of it, measured

The research estimated (res §0, scale table):

| Component | Research estimate | Measured |
|---|---|---|
| Forked `wayland-backend` transport + handle table | 3,000–5,000 lines | **649 added, 207 removed**, across 15 files, plus one new 322-line module |
| Forked `wayland-server` socket layer | 1,000–2,000 lines | **142 added, 92 removed**, across 5 files |
| *(not predicted)* forked `wayland-scanner` | — | **49 added, 50 removed**, 4 files, of which 2 are golden test files |

Plus new code outside the forks:

| Crate | Lines | What it is |
|---|---|---|
| `wc-handle` | 980 | the `h`-argument shim: typed descriptors, named sections, and their tests |
| `wc-transport` | 316 | AF_UNIX on both platforms, peer credentials, accept |
| `wc-server` | 353 | the stage-1 server |
| `wc-probe` | 417 | a client that writes the wire format by hand |

**The research over-estimated the fork by roughly three to four times**, and the
reason is a good one rather than luck: its own §1.4 observed that the protocol
layer "is genuinely fd-type-agnostic already", and that observation turned out to
be worth more than the section that contained it suggested. Cashing it in — one
module deciding what an fd, a socket and a poller *are*, and eleven files
renaming `std::os::unix` to `crate::os` — is most of the port. What had to be
*written* rather than redirected is three things: the Windows arm of the `h`
argument, the Windows transport, and a third `#[cfg]` branch in the poll loop.

*Caveat on that comparison, stated because it would otherwise flatter us:* the
research's estimate presumably included work this stage has not done —
`wayland-client`, `wayland-protocols`, and the `wl_shm` pool implementation with
its `SEC_RESERVE` growth path. §6 carries what is left.

### 0.2 What is not proven

Listed here rather than left implicit, because the temptation to read §0 as
"Windows works" is strong and it would be wrong.

1. **Nothing has run on Windows.** Everything executable was executed under
   Wine 11.14 staging. Wine is not `afunix.sys`, not wepoll, and not the Windows
   scheduler. §4.4 is the honest accounting of what that does and does not buy.
   **[SUPERSEDED — 2026-08-13, see §7.8.** It has all run on the guest now, 18
   checks and no failures. The accounting in §4.4 was right about the *kind* of
   thing Wine cannot tell you and wrong about which way one of them would go.**]**
2. **`SIO_AF_UNIX_GETPEERPID` is untested.** Wine implements no `IOC_VENDOR`
   ioctl in that range at all — a deliberately invented control code returns the
   identical `WSAEOPNOTSUPP` (10045) — so peer credentials on Windows are code
   that has compiled and never run. It is written to degrade to "pid 0" rather
   than fail, which is the right shape, but the value has never been observed.
   **[SUPERSEDED — 2026-08-13, see §7.8 item 1.** Observed: it returns the peer's
   real pid, cross-checked from both ends of a connection between two
   processes.**]**
3. **No compositor exists.** There are no surfaces, no `xdg-shell`, no HWNDs and
   no pixels. `wl_shm.create_pool` maps the client's memory and prints what is in
   it; it does not implement `wl_shm`.
   **[SUPERSEDED — 2026-08-13, see §7.5.** One exists. It has surfaces and
   `xdg-shell`, through smithay's own implementations, and a real C client
   reaches `xdg_toplevel` against the Windows binary. There are still no HWNDs
   and no pixels. The updated list is §7.7.**]**
4. **Level-triggered wepoll is asserted, not observed under load.** The
   registration asks for `PollMode::Level` and `polling`'s IOCP backend reports
   `supports_level() == true` unconditionally, but no test has yet produced a
   partial read that would exercise re-readiness.
   **[PARTLY SUPERSEDED — 2026-08-13, see §7.8 item 3.** A fragmented client
   stream on the guest — three bytes every 15 ms — produces the partial read, and
   the protocol exchange completes through it. Still one client, so still not
   *under load*.**]**

---

## 1. The transport

### 1.1 Decision: AF_UNIX, not a named pipe

**This contradicts the research**, which recommends a named pipe as decision 1
of its component architecture (res §9.2), and it is worth taking the
disagreement seriously because the research's argument is well made.

Its case for named pipes has two legs:

> "The peer-PID problem has a clean answer if the transport is a named pipe
> rather than AF_UNIX: `GetNamedPipeClientProcessId` gives the server the
> client's PID… Windows AF_UNIX has no `SO_PEERCRED` equivalent, so with an
> AF_UNIX transport the peer PID must be asserted by the client in a handshake,
> unauthenticated. **This is a real argument for choosing named pipes over
> AF_UNIX as the Wayland transport.**" (res §2.5a)

and, from §9.2, that "Windows AF_UNIX buys nothing here (we cannot pass fds over
it either)".

**Both legs fail.**

**The peer-PID leg is factually wrong.** `SIO_AF_UNIX_GETPEERPID` exists. It is a
`WSAIoctl` control code declared in Microsoft's `afunix.h`, and mingw-w64 carries
the same declaration — read directly from
`<mingw>/x86_64-w64-mingw32/sys-include/afunix.h:17`:

```c
#define SIO_AF_UNIX_GETPEERPID _WSAIOR(IOC_VENDOR, 256)
```

which expands to `IOC_OUT | IOC_VENDOR | 256` = `0x58000100`. So Windows AF_UNIX
*does* have a peer-identity mechanism and the client does not have to assert
anything. (It is a PID and not a credentials triple, and Project Zero has shown
the analogous named-pipe PID can be spoofed, so it is an identity *hint* and not
a security boundary — see §1.3. But that is equally true of
`GetNamedPipeClientProcessId`, so it does not separate the two options.)

**The "buys nothing" leg misses the decisive constraint, which the research
itself established two sections earlier.** From res §1.2, on `calloop` — the one
crate in the smithay stack that already builds for Windows:

> "`polling`'s Windows backend is wepoll, which **only supports sockets**…
> So calloop on Windows **cannot poll the Win32 message queue, waitable handles,
> timers, or pipes**."

A named pipe is not a socket. Choosing it would mean the Wayland transport could
not be watched by the only event loop in the stack that ports for free, and
every readiness path would have to be rebuilt on overlapped I/O and IOCP. An
AF_UNIX socket is exactly what wepoll wants.

**So: AF_UNIX, on both platforms.** It also keeps `wayland-server`'s
`ListeningSocket` and `wayland-backend`'s `Client::new(stream)` shaped as they
are, which is a large part of why the fork is as small as §0.1 measures.

What is genuinely lost by not using a named pipe: `\\.\pipe\` names are a
namespace with ACLs and no filesystem, whereas an AF_UNIX socket is a real file
that needs a real directory (§1.2). That is a mild inconvenience, not a
constraint.

### 1.2 Where the socket lives

Windows AF_UNIX binds to a **real filesystem path** — not an abstract namespace,
not a `\\.\pipe\` name. There is no `XDG_RUNTIME_DIR` and no `/run/user/1000`,
so `wc_transport::socket_dir()` returns `%LOCALAPPDATA%\waycompose`, creating it
if needed. Chosen because it is per-user, is not roamed onto a network share
(which would break a socket), and survives a crash leaving a stale node in it.

The Unix behaviour is byte-for-byte unchanged: `$XDG_RUNTIME_DIR`, must be
absolute, same error.

*Measured, and a trap for the unwary:* Wine constructs the Windows environment
block itself, so `LOCALAPPDATA=... wine foo.exe` does **not** override what the
process sees. The test harness discovers the node with `find -type s` instead of
predicting it — which is also a better test, since it checks the path the code
really chose.

### 1.3 Peer credentials, and what they are worth

`wc_transport::peer_credentials` returns `{pid, uid, gid}`.

* **Linux:** `SO_PEERCRED`. Authoritative — the kernel fills it in and the peer
  cannot influence it. (Implemented with a direct `getsockopt` because
  `UnixStream::peer_cred` is still unstable as of Rust 1.97,
  rust-lang/rust#42839. Not worth a nightly toolchain for three integers.)
* **Windows:** `SIO_AF_UNIX_GETPEERPID` for `pid`; `uid` and `gid` are **0**,
  because Windows has no numeric user or group id and inventing one from the
  token SID would be a lie in a `u32`-shaped hole.

Failure is not fatal. Upstream `wayland-backend` writes
`expect("getsockopt failed!?")`; the fork does not. Aborting the process because
it could not learn a number it does not use for anything would trade a working
system for a diagnostic.

> **Treat the Windows PID as an identity hint, never as a security boundary.**
> Nothing in this project currently makes a trust decision on it, and nothing
> should start doing so without saying so in this document first.

---

## 2. The `h` argument — the fd shim

The question the research flagged as decisive (res §2), and the one the brief
asks to be decided and implemented.

### 2.1 The constraint

Windows AF_UNIX carries no ancillary data; Microsoft say so directly, and the
research's citation is sound. Windows' substitute, `DuplicateHandle`, is
**push**-shaped: the sender writes into the receiver's handle table and therefore
needs a process handle for the receiver carrying `PROCESS_DUP_HANDLE`.

For a compositor that is a bad trade, and it is worth being precise about why
rather than treating it as merely inconvenient. `PROCESS_DUP_HANDLE` on a client
process is a strong capability — it permits duplicating handles *out* of that
process as well as into it. A display server that holds it over every client has
acquired the ability to steal any client's handles, in order to receive
buffers. That is a much larger privilege than the job requires.

### 2.2 Decision: named kernel objects, opened by name, described in-band

**Named objects, no `DuplicateHandle`, no rights on the peer, no side channel.**

The research offers three candidates (res §2.5) and recommends "(c) the phase-1
arena as the default path, (a) `DuplicateHandle` over a named pipe as the escape
hatch". This design takes **(b), named objects**, as the default, with (c)
reserved as a variant and (a) as the escape hatch that nothing currently uses.

The reason (b) is better than the research gave it credit for is a fact phase 1
established on the real hardware and which the research's §2.5 records almost in
passing: a *named* section object needs no `DuplicateHandle` relationship at all,
because the peer opens it by name. Everything on the critical path can be named —
`CreateFileMappingW`, `CreateNamedPipeW`, and even Vulkan external memory via
`VkExportMemoryWin32HandleInfoKHR::name`. There is nothing on the near horizon
that forces (a).

(c) — allocating out of the phase-1 arena — is not rejected; it is the
`Descriptor::ArenaSlice` variant, present in the type from the start so that
adding it is not a wire-format break. It is not the *default* because it is only
available when phase 1 is in the loop, which is Architecture B; a compositor
serving a mingw-native client (Architecture A) has no arena to slice.

### 2.3 The encoding, and where it differs from the research

The research proposes replacing `SCM_RIGHTS` with "any other ordered side
channel", reasoning from a genuinely useful observation (res §2.2):

> "An `h` (fd) argument occupies **zero bytes** in the message body. Fds travel
> entirely out-of-band… So the fd stream is already a separate, strictly-ordered
> side channel, matched positionally to `h` arguments."

**We do not use a side channel, and the reason is that the same observation
points somewhere simpler.** A descriptor is a variable-length byte string;
Wayland's wire format already has a variable-length argument encoding (`array`:
a `u32` length, the bytes, padded to 4); and an `h` currently occupies zero
bytes. So encoding `h` exactly like `array` costs nothing structurally and
removes an entire class of problem:

* no second connection to open, order against, or deadlock on;
* no handle table to keep synchronised between two processes;
* no `MAX_FDS_OUT = 28` cap, and none of libwayland's "send exactly one byte of
  real data when the control buffer is full" workaround;
* **the type is on the wire.** This is the cost waypipe could not avoid — it
  needed a full protocol parser because, as the research quotes, "one cannot
  determine which passed file descriptor corresponds to which message without
  parsing the message". We escape it for the reason the research identified
  (res §2.3): on Windows there are no fds to begin with, so the code that creates
  the object always knows what it created and emits a typed token from the start.

The descriptor is little-endian, unlike the surrounding native-endian wire
format. Deliberate: Wayland is native-endian because a Unix socket cannot cross
an endianness boundary, but a descriptor may one day cross a proxy
(Architecture B), and a format that is only correct when both ends agree by
accident costs nothing to prevent.

### 2.4 What the deviation costs

**The Windows byte stream is not libwayland's byte stream.** A stock
`libwayland-client` cannot talk to our Windows server once an `h` is involved.
The brief asks what a patched client costs; this is the answer, and it is
smaller than it sounds:

* **There is no stock libwayland-client on Windows to be incompatible with.**
  GTK4 hard-disables its Wayland backend on Windows and QtWayland is not built
  for Windows at all (res §3), so the population of unpatched Windows Wayland
  clients is exactly zero and stays that way until somebody ports a toolkit.
* **It does not foreclose Architecture B.** In the "Linux clients over a proxy"
  topology the proxy terminates the Wayland connection and re-materialises fds at
  the boundary anyway — it speaks ordinary Wayland to the Linux client and ours
  to the compositor. The deviation is confined to the hop that was always going
  to be special.
* **The deviation is Windows-only.** The Unix build still uses `SCM_RIGHTS` and
  is byte-compatible with libwayland — which is what lets the protocol layer be
  tested against a real third-party client at all (§4.2), and is worth more than
  the compatibility it gives up.
* **The patch surface, measured:** the entire Windows arm of the `h` argument is
  about twenty lines in one file, `wayland-backend/src/rs/wire.rs`. The research
  predicted "a rewrite of one small file plus a type-parameter substitution"
  (res §2.2) and that is exactly what it was.

### 2.5 The lifetime rule — the one thing a caller must know

This is the real cost of name-based passing and it is not in the research.

**Sending a descriptor transfers nothing.** Unlike `SCM_RIGHTS`, where the kernel
takes a reference the moment `sendmsg` returns, the receiver holds only a *name*
until it opens the object. A Windows named section with no open handles is
destroyed and its name becomes free.

> **The process that creates the object must keep its `Handle` alive for at least
> as long as the protocol object that handle backs** — a `wl_shm_pool`, a keymap,
> a clipboard pipe.

Every real use site does this anyway, because it needs the memory for the same
period. But a sender that drops its handle immediately after `send` — which is
legal on Unix — leaves the receiver with a name for an object that no longer
exists.

The failure mode is at least clean rather than silent: `map()` fails, and it
cannot accidentally open somebody else's object, because names carry 128 bits
from the system CSPRNG and `create_section` refuses to adopt an existing name
(`CreateFileMappingW` reports `ERROR_ALREADY_EXISTS` and we close and error
rather than sharing).

**Residual race, stated plainly:** between the sender's `write` and the
receiver's `map()`, the object's existence depends on the sender's discipline.
If this ever bites, the fix is for the receiver to open eagerly at parse time
rather than lazily at `map()` time, which shrinks the window to the socket
round-trip. Not done yet because nothing has needed it and it complicates
ownership on the receiving side.

### 2.6 Security, honestly

A named object can be opened by anyone who can guess or learn its name; a
`DuplicateHandle` target is exactly one process. **This is a real weakening
relative to the Unix model**, where an fd is a capability that cannot be guessed
at all.

Mitigations in place: 128 bits from `BCryptGenRandom` per name, and creation in
the per-logon-session `Local\` namespace rather than `Global\`, so another user
on the same machine cannot reach it even knowing the name (and no
`SeCreateGlobalPrivilege` is needed).

Not in place, and the next step if this ever hosts a mutually-distrusting client
set: `CreatePrivateNamespace` with a boundary descriptor. Deliberately deferred
because it requires the compositor to launch the client, which is a lifecycle
change and not a transport one.

One compensating upside, inherited from the platform: **a Windows section cannot
shrink**, so the entire `wl_shm` `SIGBUS` attack class disappears. libwayland
installs a `SIGBUS` handler precisely because a Linux client can `ftruncate` a
pool smaller and fault the compositor. Here it cannot. (Noted by the research at
§2.6 and confirmed by the `CreateFileMapping` semantics we now depend on.)

### 2.7 The kinds that exist, and the one that never will

`Descriptor` has four variants, mapping to the research's §2.4 table of what an
fd actually *is* per use site: `Section` (shm pools, keymaps, ICC profiles,
gamma ramps), `Pipe` (clipboard and DnD transfer), `NtHandle` (the
`DuplicateHandle` escape hatch, unused), and `ArenaSlice` (phase 1, reserved).

There is no `Dmabuf` variant and there will not be one. The research is right
that `linux-dmabuf` "is not portable — it is a Linux kernel ABI by name and by
content", and that the answer is a replacement protocol rather than a shimmed
fd. That is stage 6 work.

---

## 3. The fork

Full inventory in `vendor/PATCHES.md`. Three things belong here because they are
decisions rather than mechanics.

### 3.1 One module decides what a file descriptor is

`wayland-backend/src/os.rs`, 322 lines, new. `RawFd`, `OwnedFd`, `BorrowedFd<'a>`,
`WlStream`, `PollHandle`, the `FdRepr` trait that replaces upstream's
`Fd: AsRawFd` bounds, the four `errno` values, and the three libwayland C ABI
structs.

On Windows `RawFd` and `OwnedFd` are **the same type**, and that is not laziness.
The distinction exists on Unix because an fd is an integer with no ownership
semantics of its own, so a separate type has to carry them. A `wc_handle::Handle`
is refcounted and already carries its own lifetime, so the two would be the same
type with the same behaviour. Upstream's `try_clone_to_owned()` becomes
`.clone()`, which is the honest analogue of `dup()`.

`FdRepr` is worth one sentence of justification: upstream's `AsRawFd` bounds were
never about file descriptors. They exist so two messages can be compared and so
`WAYLAND_DEBUG=1` can print something. Naming that requirement directly is what
makes the type parameter genuinely portable instead of merely looking portable.

### 3.2 The third poll path

Upstream has exactly two, chosen by `cfg`: epoll and kqueue, with no third — the
research flags this (res §1.1). The fork adds `polling::Poller`.

`polling` rather than raw IOCP for a reason beyond convenience: `calloop` is
itself built on `polling`, so the backend's dispatcher and the compositor's
eventual event loop agree about what a readiness source is, instead of the
backend inventing a second answer that would have to be bridged later.

wepoll supports level-triggered mode, so `PollMode::Level` is a faithful
substitution for upstream's `EventFlags::IN`; re-arming semantics would have
changed the dispatch loop.

**One obligation had to be introduced.** Upstream never removes a client from its
epoll set, and does not need to — closing an fd removes it from every epoll set.
wepoll gives no such guarantee, and Windows reuses socket handles aggressively,
so a client dropped while still registered could have its registration inherited
by a later client. `State::cleanup` now deletes dead clients from the poller
before dropping them. This is what the `unsafe` around `Poller::add_with_mode`
relies on.

### 3.3 Where the port *loses* something: `poll_fd()`

On Unix, `Backend::poll_fd()` returns an epoll/kqueue descriptor which is itself
pollable, so a compositor nests the entire backend inside its own `calloop` loop
as one source. Every smithay compositor is structured this way.

**On Windows this does not work.** The Windows counterpart, `poll_handle()`,
returns the wepoll handle — and wepoll polls sockets, and a wepoll handle is not
a socket. It is exposed anyway because it is a real waitable object that
`MsgWaitForMultipleObjects` can wait on, which is what the two-thread design the
research argues for on independent grounds (res §4.6, §9.2 decision 4) will want,
since that thread is waiting on the Win32 message queue at the same time.

So a compositor on Windows has two supported shapes: call `dispatch_all_clients`
from a thread waiting on this handle, or register each client socket with its own
`calloop` loop and call `dispatch_client`.

**This is a consequence of the wepoll-only-polls-sockets constraint the research
identified, arriving in a place it did not predict.** It is deliberately an
inherent method named differently rather than an `AsHandle` impl, so that nobody
reaches for it by trait bound and discovers the difference at runtime.

The client side does *not* have this problem: `InnerBackend::poll_socket()` is
the connection socket itself, and composes with `calloop` exactly as on Unix.

---

## 4. What is proven, and what is not

The house rule is that a claim needs a number or a citation. This section is
where the claims are stated at the strength the evidence actually supports.

### 4.1 The gates

All run through `nix develop`, never the `cargo` on `PATH`.

| Gate | Result |
|---|---|
| `cargo check --workspace --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo clippy --workspace --all-targets` (Linux) | clean, no warnings |
| `cargo clippy --workspace --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo test --workspace` | 30 tests, 0 failures |

The Windows `check` is the gate that matters. Note that `cargo test` cannot run
for the Windows target — the tests run on Linux, and the Windows behaviour is
covered by §4.3 instead.

### 4.2 On Linux, against a client that has never heard of us

`scripts/wc-handshake.sh linux`:

* `wc-probe` completes the handshake and sees 2 globals.
* **`wayland-info` — libwayland's own C client — enumerates them:**
  `wl_compositor` v6 and `wl_shm` v1.

The second is the one that counts. `wc-probe` shares no code with the server
beyond the socket, but it is still ours; a symmetric bug in our own wire encoding
would make a matched pair agree with each other and pass. `wayland-info` is C,
written by other people, and cannot be wrong in the same direction we are.

### 4.3 Under Wine, as a Windows binary

`scripts/wc-handshake.sh wine`, all four checks pass:

1. **`wc-server.exe` binds an AF_UNIX socket.** Verified as a real host socket
   node with `find -type s`, at
   `drive_c/users/<user>/AppData/Local/waycompose/wayland-win`.
2. **`wc-probe.exe` completes the handshake against `wc-server.exe`** — both
   mingw-built binaries — receiving both globals and `wl_callback.done`.
3. **An `h` argument crosses.** The probe creates a 4096-byte section, writes
   `WAYCOMPOSE-SHM-OK` at offset 0 and `tail` at offset 4000, and sends *only the
   descriptor*. The server logs:

   ```
   wc-server:     descriptor: Section { name: "Local\\wc-e85ce588…", size: 4096 }
   wc-server:     SHM MAPPED len=4096 head="WAYCOMPOSE-SHM-OK" tail="tail"
   ```

   The two markers are at different offsets on purpose: a server reporting
   success from a zeroed or wrong-length mapping would still fail. Nothing but a
   name and a size crossed the socket, so bytes the server can print are bytes it
   fetched by opening the object.
4. **`wayland-info`, running as a native Linux binary, enumerates the globals of
   the Windows server.** This works because Wine backs a Windows AF_UNIX socket
   with a real host Unix socket node, so a Linux client can connect straight to
   it. A C client that knows nothing about us, talking to our `.exe`, over the
   real wire format.

### 4.4 What Wine does and does not prove

**Wine is not Windows.** Stating the limits precisely, because "it works under
Wine" is exactly the kind of claim that decays into "it works on Windows" if left
unqualified.

**What a pass here does establish:** that our code paths are self-consistent and
structurally possible — the wire format round-trips, the poll loop dispatches,
the descriptor encoding survives a real socket, named section objects are created
and opened by name across a process boundary, and none of it depends on anything
Unix-shaped that we failed to notice.

**What it does not establish:** anything about `afunix.sys`, about wepoll's real
behaviour under load, about `SIO_AF_UNIX_GETPEERPID`, or about how Windows
schedules any of this.

Two measured examples of the gap, both of which cost real time:

* **Wine 11.0 has no AF_UNIX at all.** `socket(AF_UNIX, SOCK_STREAM, 0)` returns
  `WSAEAFNOSUPPORT` (10047); `WSAEnumProtocols` lists 8 providers and none with
  `af=1`; `grep -a AF_UNIX` over its `ws2_32.dll` finds nothing. Only
  `wine-wow64-staging` has it — 11.14 staging lists a 9th provider,
  `af=1 type=1 proto=0 name=AF_UNIX`. The flake pins the staging build for this
  reason and the comment there says so.
* **Wine's `accept()` fills in the peer address differently from Windows.** It
  returns `len = 110` with `sun_family = 0`, where Windows is documented to
  report `AF_UNIX`. `uds_windows` validates that field and failed every
  connection. The fix — ask the OS not to report an address we do not use — is
  correct on both platforms, but the *bug* was only visible under Wine and would
  have been invisible on Windows.

The second is the useful lesson: Wine finds a different class of bug than
Windows would, and finding none of them is not the same as there being none.

**The remaining verification needs the guest.** It has now been done —
`scripts/wc-guest.sh` and **§7.8**, which answers every item above and finds a
third divergence the two examples here did not predict: `Sleep(1)` is 1 ms under
Wine and 15.8 ms on Windows. Note also that it was *not* one command with the
binaries copied over: a native Linux client cannot open an `afunix.sys` node, so
the third-party client has to reach the guest through a byte relay. Wine gave
that evidence away for free and Windows charges for it.

---

## 5. The position on smithay

The brief asks for a position: port the blocking files upstream-style, vendor a
fork, or drop smithay and build on `wayland-server` directly. The user asked for
"the same libraries the cosmic wayland server uses", so diverging needs a
reason.

**Decision: use smithay. Port it, upstream-style, with a thin fork for what
upstream will not take.**

That is the opposite of what I expected before measuring, and the research's
framing pointed the other way (res §9.4, Risk 2: "Consider whether smithay is
load-bearing at all… this is worth a serious look before committing"). The
numbers do not support it.

### 5.1 What was measured

smithay 0.7.0, checked for `x86_64-pc-windows-gnu` through this project's nix
shell, with `default-features = false, features = ["wayland_frontend",
"desktop"]` — the configuration a Windows compositor would actually use — and
with this project's three forks patched in. Verified twice, independently.

| | |
|---|---|
| Total source | **96,004 lines, 246 files** |
| Errors | **56** |
| Files with errors | **22** |
| Lines in those 22 files | 8,385 |
| Lines in them mentioning *any* platform-coupled symbol | **139 (1.7 %)** |
| Errors in dependencies | **0** |

The per-file distribution is the decision-relevant shape: `dmabuf.rs` 15,
`shm/pool.rs` 13, `clock.rs` 5, `commit_timing` 3, `dmabuf/mod.rs` 2,
`sealed_file.rs` 2, **and one error each in fourteen further files.** Twenty of
the fifty-six errors are a single `use std::os::unix::io::{…}` line.

**Our forks carry smithay on Windows already.** `wayland-backend`,
`wayland-server` and `wayland-scanner` compiled clean *as smithay's
dependencies*, for the Windows target, with no further changes. So did
`wayland-protocols`, `-wlr` and `-misc`, which settles an open item in §6.

### 5.2 The decision number

Splitting smithay's 96,004 lines by what a Windows compositor could use:

| | lines | share |
|---|---|---|
| Unusable on Windows regardless (DRM, EGL, GLES, udev, libinput, X11, XWayland, session, Vulkan allocator…) | 39,946 | 41.6 % |
| **Compiles for Windows today and is exactly what we would otherwise write** | **44,728** | **46.6 %** |
| Needs porting | 11,330 | 11.8 % |

Of that last 11,330, some 3,465 lines are dma-buf and DRM-sync code that should
simply be gated out rather than ported, leaving ~7,865 — of which an estimated
300–400 lines actually change.

The 44,728 is the number that decides it. It is ~21,000 lines of protocol
implementations (`xdg-shell` and `wlr-layer-shell` 5,344, `compositor` 2,601,
`seat` 1,348, `tablet` 1,170, `text-input`, `input-method`, `viewporter`,
`presentation`, `session-lock`, `pointer-constraints`, `fractional-scale`,
`cursor-shape`, …), ~7,900 lines of render-element and damage-tracking
machinery, 4,768 lines of `desktop/` space and window management, 3,390 lines of
pointer and touch grab handling, 3,101 lines of geometry and utility code, and a
1,354-line **pixman software renderer that would actually work on Windows**.

> **Rejecting smithay means throwing away ~44,700 lines of already-Windows-clean
> compositor logic in order to avoid touching ~139 platform-coupled lines in 22
> files.** That is not a close call.

The research's own instinct that "smithay's main value (DRM/libinput/udev
backends, the desktop-space helpers) is the part we cannot use anyway" is half
right and half wrong in a way that inverts the conclusion: the DRM/libinput/udev
half is indeed unusable, but the desktop-space helpers, the protocol
implementations and the damage tracking are the *larger* half and they are fine.

### 5.3 Why upstream-style rather than a hard fork

Two reasons, one of them measured.

**The patch shape is one upstream has accepted three times.** smithay has merged
"make this work on a non-Linux OS" changes in PRs #867 (FreeBSD build fixes,
merged 2023-02-24), #911 (`backend_vulkan` and `xwayland` on FreeBSD, merged
2023-04-27, merged by the maintainer) and #1027 ("portability: Fallback for
`SealedFile`", merged 2023-05-17) — all authored by a System76/Pop!_OS
developer, i.e. by COSMIC. **#1027 touched `utils/sealed_file.rs` and
`utils/clock.rs`, two of the three files that are genuinely hard here**, and
added exactly the kind of `target_os` fallback arm we need a third branch of.

**The gating half is defensible on its own merits, independent of Windows.**
`backend/mod.rs` declares `pub mod allocator;` with no `cfg`, and
`allocator/mod.rs` declares `pub mod dmabuf;` with no `cfg`, so today
`--no-default-features` still compiles Linux dma-buf ioctl code. Four one-line
gates remove 2,537 lines and **19 of the 56 errors (34 %)**, and the argument for
them — "do not compile Linux kernel ABI code when the user asked for no
backends" — needs no mention of Windows at all. The FreeBSD porters benefit too,
which is who got #867/#911/#1027 merged.

The honest counter-signal, also measured: a GitHub search of the smithay repo
for "windows" in issue and PR titles returns 17 results, **all** about
application windows or XWayland. There is no Windows-port issue, PR or
discussion. We would be first, and the fd-abstraction half is a bigger ask than
a `target_os` fallback — it needs an `os` shim module of the kind
`vendor/wayland-backend/src/os.rs` already is. **Expect the gating and the
`clock.rs`/`sealed_file.rs` work to upstream and the fd abstraction to live in a
fork.** That is the same split the research recommends (res §9.4) and it is the
right one; what has changed is that the fork side is now measured at hundreds of
lines rather than thousands.

### 5.4 The two things that could still go wrong

> **[SUPERSEDED — 2026-08-13, see §7. Both were measured in stage 2 and neither
> went wrong. The original text is kept because the *shape* of the estimates is
> the interesting part: one was wrong by being too pessimistic in a way that was
> flagged in the same paragraph, and the other was right about the mechanism and
> silent about the one thing that actually bit.]**

Stated separately because the estimate above does not cover them.

1. **`wayland/shm/pool.rs` is the one architecturally hard file** (370 lines, 13
   errors). It `mmap`s the client's pool *and installs a process-wide `SIGBUS`
   handler*, because on Linux a client can `ftruncate` its pool smaller and fault
   the compositor. The mapping half lands on `wc_handle::Handle::map()`, which
   already exists. The fault-guard half needs a vectored exception handler for
   `EXCEPTION_IN_PAGE_ERROR` instead of `sigaction`/`si_addr`.
   **Estimated 120–180 lines, and it is the one estimate I would not trust
   without a spike.** Note the compensating fact from §2.6: a Windows section
   cannot shrink, so the threat the handler defends against does not exist here
   — the guard may be able to be *omitted* on Windows rather than reimplemented,
   which would make this the easiest file rather than the hardest. That is worth
   half a day to establish before budgeting for the hard version.
2. **`libxkbcommon` for mingw is unquantified and sits outside every number
   above.** smithay depends on the `xkbcommon` crate **non-optionally**, and that
   crate does not compile for Windows without a three-site patch (it uses
   `std::os::unix` and `OsStr::as_bytes`); it also `#[link(name = "xkbcommon")]`,
   so a real *build* — as opposed to `cargo check`, which never links — needs
   libxkbcommon cross-built for mingw. The research is optimistic here
   (res §3: "libxkbcommon is the one bright spot — Windows-supported since
   1.0.0"), and it may well be right, but nothing in this measurement touched
   linking. **This is the next thing to measure**, and it is a prerequisite for
   the smithay decision actually being actionable.

**Outcome.** (1) was the *easiest* file, exactly as the hedge in its own last
three sentences suggested: the guard is omitted, not reimplemented, and the
120–180 lines were not spent. (2) is answered yes, with one upstream bug found
along the way that neither the research nor this section anticipated. §7.

### 5.5 Corrections to the research

- **"smithay cannot build for Windows in any configuration" is overstated.** It
  does not build, but the failure is 56 errors in 22 files, 20 of which are one
  import line each — not a categorical impossibility. The research asserted this
  without running a build, and says so honestly; the number is what was missing.
- **`src/backend/allocator/udmabuf.rs` does not exist** in smithay 0.7.0. The
  `allocator/` directory is exactly `dmabuf.rs, dumb.rs, format.rs, gbm.rs,
  mod.rs, swapchain.rs, vulkan/`.
- **The rustix feature list is wrong.** Actual:
  `["event", "fs", "mm", "net", "pipe", "process", "shm", "time"]`. The research
  lists `param`, which is not requested, and misses `event`, `mm`, `net` and
  `shm`. Of the eight, **six** provide nothing on Windows (not five of five):
  `event` and `net` are available there.
- **What the research got right, and it is the load-bearing part:** the blockers
  really are in code that is not feature-gated, so `--no-default-features` does
  not dodge `dmabuf.rs` today. It is one line each to make that possible.

---

## 6. The honest size of what remains

Estimates, and labelled as such. Stage numbers refer to the research's staging
table (res §9.3), which this project is following.

| Work | Estimate | Confidence |
|---|---|---|
| `wayland-client` fork (Architecture A only) | 100–300 lines of diff | **high** — the backend's client half is already ported and compiles; only the `wayland-client` crate itself remains, and it is the same shape as `wayland-server`, which cost 142 lines |
| `wayland-protocols`, `-wlr`, `-misc` | **0 lines — done** | **measured**: all three check clean for `x86_64-pc-windows-gnu` against these forks, as a side effect of the smithay probe in §5.1. The research's prediction that their gate is "entirely `wayland-backend`" is confirmed. |
| smithay port (§5) | ~~300–400 changed lines, plus the two unknowns in §5.4~~ **done: 240 changed lines across 27 files, plus 2 new modules** | **measured** — §7.3. The estimate was high because §5.4 item 1 turned out not to need writing at all. |
| `libxkbcommon` cross-built for mingw, and linking | ~~unknown~~ **done: one upstream patch, one nix derivation, three crate sites** | **measured** — §7.2. Runs under Wine; the guest still has to confirm. |
| `XkbConfig` variant taking a keymap string (so `KeyboardHandle::new` works on Windows) | 20–50 lines | **high** — `new_from_names` is one call site and `new_from_string` already works there (§7.2) |
| A DACL on `wc_handle` sections, so `SealedFile` is actually sealed | 50–150 lines | **medium** — the mechanism (`SECURITY_ATTRIBUTES` on `CreateFileMappingW`) is standard; getting the SID right is the work. §7.7 item 6 |
| `wl_shm` properly (pools, buffers, `SEC_RESERVE` growth) | 500–1,000 lines | **medium** — the `resize` problem is real (a Windows section's size is fixed at creation) and the two answers are both known |
| ~~Stage 4: `backend_win32`, one HWND per toplevel, shm blit, pointer + keyboard~~ | ~~8,000–15,000 lines~~ | superseded — see the two rows below |
| One HWND per toplevel, shm blit, damage, title, close, resize (stage 3, §8) | **done: ~2,400 lines of new code across 6 files, plus a 900-line test client and a 490-line harness** | **measured** — 5,290 lines of new files in total, of which roughly 40% is comment. The research's estimate covered this *and* input *and* a scene graph in one number |
| Pointer and keyboard input, subsurfaces, popups (what is left of stage 4) | 2,000–5,000 lines | **low–medium** — the `XkbConfig` gap (§7.7 item 8) is small and known; a scene graph is not, and nothing measured yet bears on it |
| Verification on the real guest | one afternoon | **high** |

**The single largest risk is unchanged by this stage and is not in the
compositor.** The research's Risk 1 stands: there is no such thing as a Linux
Wayland application compiled for mingw, because GTK4 and Qt6 both refuse to build
their Wayland backends on Windows. Nothing in stage 1 makes that better or worse.
It remains the argument for building Architecture B first, and §2.4 confirms that
the fd-shim deviation does not obstruct it.

**What this stage did change** is that the compositor half is no longer a risk.
The research called stages 1–4 "the real project risk"; stages 1 and 2 are done,
and they were three to four times smaller than estimated. What is left in that
band is stage 3 (a headless compositor) and stage 4 (the first real window),
neither of which contains an unknown of the kind that had to be settled here.

---

## 7. Stage 2: `libxkbcommon`, and smithay as a Windows binary

Written 2026-08-13. Stage 1 ended with two open items (§5.4) and one decision
that could not be acted on until the first of them was settled. Both are settled.

### 7.1 Status in one paragraph

**libxkbcommon cross-compiles for `x86_64-pc-windows-gnu`, the `xkbcommon` crate
binds to it, and a Windows binary compiles a real 2,081-line keymap and
translates keycodes with modifiers.** **smithay compiles for
`x86_64-pc-windows-gnu`** — 56 errors to zero — with `wayland_frontend` and
`desktop` enabled. **`wc-compositor.exe`, a mingw-built smithay compositor,
stands up a `wl_display`, advertises `xdg_wm_base`, and lets
`weston-multi-resource` — a C client from the Weston project — create a
`wl_surface`, an `xdg_toplevel`, receive a `configure` and commit.** That is the
stage-2 milestone, and it is observed under Wine against the `.exe`, not
inferred from a unit test.

### 7.2 `libxkbcommon`: the answer is yes, with one upstream bug

The question §5.4 item 2 asked was three questions, and they have different
answers.

**Does libxkbcommon cross-compile for mingw? Yes, after a one-line patch, and
the patch is a case-sensitivity bug that upstream's CI structurally cannot see.**

`src/utils.h` includes `<BaseTsd.h>` unconditionally under `_WIN32`, but takes
only `SSIZE_T` from it, two lines later, inside `#ifdef _MSC_VER`. mingw-w64
ships that header as `basetsd.h`, lower case, and only that spelling. Windows
filesystems are case-insensitive, so MSVC and clang-cl *on Windows* never notice.
Cross-compiling with `x86_64-w64-mingw32-gcc` from a case-sensitive filesystem,
every translation unit that reaches `utils.h` fails:

```
../src/utils.h:28:11: fatal error: BaseTsd.h: No such file or directory
```

**Measured: 250-odd of the 290 ninja targets.** libxkbcommon 1.13.2,
`x86_64-w64-mingw32-gcc` 15.3.0.

This is precisely the gap the research flagged and then discounted. res §5 says
mingw is "**claimed-fixed but not CI-covered**"; res §3 says "libxkbcommon is the
one bright spot — Windows-supported since 1.0.0, non-MSVC Windows compilers fixed
in 1.12.3". Both sentences are true and the first one is the load-bearing one:
the fix landed, the coverage did not, and it regressed. **Correction to the
research: "the one bright spot" is right about the destination and wrong about
the road being clear.**

The fix is `nix/libxkbcommon-mingw-basetsd.patch`, written as a submittable
commit with the measurement in the message.

*Also blocking, and packaging rather than upstream:* nixpkgs' `libxkbcommon`
hard-enables X11 — `libxcb` in `buildInputs`, `libx11.out` in `mesonFlags` — and
`libx11` genuinely does not cross-compile to mingw here (`xorgproto` for mingw
installs no `include/X11`). `nix/libxkbcommon-mingw.nix` switches off X11,
Wayland, docs, the registry and the tools, which is everything smithay does not
use.

**Does the Rust crate bind to it? Yes, three sites, as predicted.** §5.4 item 2
guessed "a three-site patch (it uses `std::os::unix` and `OsStr::as_bytes`)" and
that is exactly what it was. `vendor/PATCHES.md` has them.

**Can a Windows binary load a keymap and translate a keycode? Yes.**
`scripts/wc-xkb.sh`, all five checks, under Wine:

| | |
|---|---|
| `xkb_context_new` | ok |
| `xkb_keymap_new_from_string`, 2,081-line `us` keymap, 71,249 bytes | ok, keycodes 9..709 |
| keycode 38 → keysym `0x61` `a`, UTF-8 `"a"` | ok |
| keycode 38 + Shift → keysym `0x41` `A`, UTF-8 `"A"` | ok |
| `xkb_keymap_get_as_string` → 35,571 bytes, recompiles | ok |

The Shift case is there because the first three would pass against a keymap that
had compiled to nothing useful; a modifier requires the compiled tables to be
real. Proven twice, in C (`scripts/probe-xkb.c`, linked by hand) and in Rust
(`crates/wc-xkb-probe`, linked by cargo) — different questions, because the
second also tests the fork's `cfg` gating and cargo's linker invocation.

Static, not shared: `-Ddefault_library=static` gives `libxkbcommon.a`, so the
`.exe` carries the keymap engine and nothing is resolved from a search path we
arranged.

**And the thing that does not work, deliberately.**
`xkb_keymap_new_from_names` — the call smithay's `XkbConfig::compile_keymap`
makes, and the call every Linux compositor makes — **fails on Windows and must**:

```
xkbcommon: ERROR: [XKB-632] Failed to add any default include path
                  (system path: C:/ProgramData/waycompose/xkb)
xkbcommon: ERROR: [XKB-595] Cannot load XKB rules "evdev"
```

It resolves RMLVO names against the xkeyboard-config data tree, ~2,000 files of
X11 configuration that does not exist on Windows. The compiled-in config root
deliberately names a path nobody creates, so that a `new_from_names` which
silently started working off a Linux store path would be a visible bug rather
than an accident, and both probes assert on the failure.

> **The keymap design follows from this and is not optional.** RMLVO names are
> resolved on Linux at build time (`scripts/gen-keymap.sh`); the Windows side
> only ever compiles a *string*. This is what the research recommends (res §5)
> and what Weston's RDP backend does. The cost is that the set of layouts is
> fixed at build time, and a compositor answering `WM_INPUTLANGCHANGE` must map
> the new HKL onto one it shipped — FreeRDP's `xkb_layout_ids.c` is that table,
> and it is stage-4 work.

**Consequence for smithay, not yet paid:** `XkbConfig::compile_keymap` is
`new_from_names`, so `KeyboardHandle::new` cannot succeed on Windows as smithay
is written. It is not on the path to `xdg_toplevel` and nothing in this stage
calls it. Fixing it is an `XkbConfig` variant carrying a keymap string, which is
small and probably upstreamable, and is the first thing stage 4 needs.

### 7.3 smithay: 56 errors to zero

Measured with `default-features = false, features = ["wayland_frontend",
"desktop"]`, this project's three stage-1 forks patched in, plus the
`vendor/xkbcommon` fork. The 56 errors in 22 files reproduce exactly.

| Step | Errors removed | Running total |
|---|---|---|
| baseline | — | 56 |
| the `backend_dmabuf` feature gate | **19** | 37 |
| `utils/os.rs` + `utils/clock/timespec.rs` and their call sites | 21 | 16 |
| `sealed_file`, `security_context`, `commit_timing`, `socket`, `virtual_keyboard` | 16 | **0** |

The 19 is worth noting: §5.3 predicted "four one-line gates remove 2,537 lines
and **19 of the 56 errors (34 %)**". Exact.

**The diff, measured:** 529 lines added and 146 removed across 27 changed files
and 2 new ones, of 246. Of the 529 added, 293 are code and 236 are comments; of
the 293, 51 are the two new modules. So roughly **240 changed lines across 27
existing files**, against §5.2's estimate of 300–400. The estimate was high, and
§7.4 is why.

One small correction to §5.2's own arithmetic: the per-file distribution is
"one error each in **sixteen** further files", not fourteen. 15+13+5+3+2+2 = 40,
plus 16 = 56.

### 7.4 `shm/pool.rs` was the easiest file, not the hardest

§5.4 item 1 called it "the one architecturally hard file", budgeted 120–180
lines for a vectored exception handler, and said it was "the one estimate I would
not trust without a spike" — while noting, in the same paragraph, that §2.6 might
make the guard omittable entirely.

**It is omittable, and the guard is omitted.** The `SIGBUS` handler exists
because a Linux client can `ftruncate` its pool *smaller* and fault the
compositor mid-read. A Windows section's size is fixed at `CreateFileMappingW`
time and cannot be reduced. The event does not exist, so nothing catches it, and
the 120–180 lines were not spent. This is the one place so far where the Windows
model is *stronger* than the Unix one.

What the file did need was smaller and different in kind: `MemMap` has to carry
the `wc_handle::Mapping` and not only a pointer, because `MapViewOfFile` returns
a view whose lifetime *is* the mapping's — dropping it dangles the pointer, which
is a silent use-after-free rather than a compile error.

`with_data_mut` returns `Err(())` on Windows rather than pretending: the mapping
is `FILE_MAP_READ` and a `*mut u8` into it would fault on the first store.

### 7.5 The evidence, and what it is worth

`scripts/wc-xdg.sh all` — 10 passed, 0 failed.

**On Linux**, `weston-multi-resource` and `weston-flower` both reach
`xdg_toplevel`.

**Under Wine, as a Windows binary**, `wc-compositor.exe`:

1. binds an AF_UNIX socket — verified as a real host socket node with
   `find -type s`, at
   `drive_c/users/<user>/AppData/Local/waycompose/wayland-xdgwin`;
2. accepts a **native Linux** `weston-multi-resource`, which creates a
   `wl_surface`;
3. creates an **`xdg_toplevel`** — the milestone;
4. sends a `configure`, and the client commits.

Steps 2–4 all run through smithay's own `compositor` and `xdg::shell` modules.
The client is C, uses libwayland-client and the upstream `xdg-shell` XML, and has
never heard of this project — the same standard `wayland-info` set in §4.2, one
protocol layer deeper.

**A limit found by running, not predicted: `weston-flower` does *not* work
against the Windows build, and the reason is §2.4 arriving on schedule.**
Toytoolkit allocates its `wl_shm` pool during `display_create()`, before any
surface exists. `wl_shm.create_pool` carries an `h`; a stock libwayland client
sends the fd out-of-band by `SCM_RIGHTS`; Wine's AF_UNIX carries no ancillary
data; so the in-band descriptor our parser expects is simply absent and framing
desynchronises at that request. Measured with `WAYLAND_DEBUG=1`: the client
sends `create_surface`, `get_xdg_surface`, `get_toplevel` and `commit`, and the
server sees none of them.

`weston-multi-resource` is a simple-client rather than a toytoolkit one and
creates the surface and toplevel *before* touching shm, so the whole xdg-shell
exchange completes before the first `h`. Both are run on Linux, where
`SCM_RIGHTS` works, and `weston-flower`'s Linux pass is what establishes that
request order is the only difference.

> **This makes §2.4 concrete in a way it was not before.** "The Windows byte
> stream is not libwayland's byte stream" was written as a cost that nobody
> currently pays, because there are no Windows libwayland clients. It is also a
> cost paid by *our own testing*: any third-party client that sends an `h` can
> only be used against the Windows build up to that point. Every stage from here
> that involves buffers needs either a client speaking our dialect
> (`wc-probe.exe --shm` is the existing one) or the Architecture B proxy.

### 7.6 The gates

| Gate | Result |
|---|---|
| `cargo check --workspace --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo clippy --workspace --all-targets` (Linux) | clean, no warnings |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo test --workspace` | 30 tests, 0 failures |
| `scripts/wc-xkb.sh` | 5 passed, 0 failed |
| `scripts/wc-xdg.sh all` | 10 passed, 0 failed |

The Windows clippy gate is stronger than stage 1's, which did not pass
`--all-targets`; making it pass needed `#![cfg(unix)]` on
`wayland-backend/tests/null_object.rs`, which uses `UnixStream::pair()`, and
there is no `socketpair(2)` for Windows AF_UNIX.

`vendor/smithay` and `vendor/xkbcommon` are **excluded from the workspace**, so
clippy does not lint them. That is deliberate and the criterion is in
`Cargo.toml`: the three stage-1 forks are ours and are linted; these two are
near-upstream and are kept rebaseable, because most of these patches are meant to
go back. Their changes are reviewed by diff, and `vendor/PATCHES.md` is the
index. **Stated plainly because it is a real weakening of the gate**: nine clippy
warnings in untouched upstream `xkbcommon` code are not being fixed, and our
patches to those two crates are not clippy-checked.

### 7.7 What is not proven, updated

§0.2's list, with stage 2's changes:

1. ~~Nothing has still run on Windows.~~ **Superseded by §7.8.** Everything in
   stages 1 and 2 has now run on the guest, 18 checks with no failures. §4.4's
   framing stands — a Wine pass still proves only self-consistency — but the
   list of things it could not reach is no longer hypothetical, and one of the
   divergences it warned about turned out to be Wine's bug rather than
   Windows'.
2. ~~`SIO_AF_UNIX_GETPEERPID` is still untested.~~ **Superseded by §7.8 item 1:**
   it works, it returns the peer's real pid, and it reports
   `cbBytesReturned = 0` while doing so.
3. ~~No compositor exists.~~ **Superseded.** One exists, it reaches
   `xdg_toplevel`, and it draws nothing.
4. ~~Level-triggered wepoll is still asserted, not observed under load.~~
   **Partly superseded by §7.8 item 3:** a deliberately fragmented client stream
   on the guest forces partial reads and re-readiness, and the xdg-shell exchange
   completes through it. Still not observed under *load* — one client dribbling
   bytes is not many clients competing — and the poll loop is still stage 1's
   sleep-poll loop, which §7.8 now shows sleeps ~15.6 ms per iteration on real
   Windows rather than the 5 ms it asks for.
5. **New: the `Monotonic` clock's epoch is process start, not boot.** Correct
   for every current caller (ordering, differences, input timestamps) and wrong
   for a client comparing against its own clock, which is what `wp_presentation`
   expects. Must become `QueryPerformanceCounter` before presentation feedback.
6. **New: `SealedFile` is not sealed on Windows.** A client handed a section
   *name* could `OpenFileMappingW(FILE_MAP_WRITE, …)` and modify the keymap other
   clients read. A sealed `memfd` cannot be written by anyone. The fix is a DACL
   on the section, which is a `wc_handle` change. **This is a genuine regression
   against the Unix security model and the first one this project has taken.**
7. **New: `with_data_mut` is unimplemented on Windows**, so
   `wayland::shm::with_buffer_contents_mut` fails. No current caller.
8. **New: `KeyboardHandle::new` cannot succeed on Windows**, because
   `XkbConfig::compile_keymap` is `new_from_names`. §7.2.

### 7.8 What the guest answered

**Everything in stages 1 and 2 has now run on real Windows.** The standing
caveat of both stages — "nothing has ever run on Windows" — is closed. The
machine is the testhost guest, Windows 11 build 10.0.26200.9168; the harness is
`scripts/wc-guest.sh`, which cross-compiles the C probes, deploys everything,
runs it and cleans up after itself. The four `.exe`s it carries over are a
`cargo build --target x86_64-pc-windows-gnu` of committed `master`.
**18 checks, 0 failures.**

| Check | Wine 11.14 staging | Real Windows |
|---|---|---|
| `SIO_AF_UNIX_GETPEERPID` returns the peer's real pid | `WSAEOPNOTSUPP` (10045), always | **pass** — cross-checked in both directions |
| `accept()` reports a usable peer address | `sun_family = 0`; stock `uds_windows` rejects | **pass** — `sun_family = AF_UNIX`; stock `uds_windows` would accept |
| control: a bogus `IOC_VENDOR` ioctl still fails | fails | fails |
| `wc-probe.exe` ↔ `wc-server.exe` handshake | pass | **pass** |
| an `h` argument crosses, via a named section | pass | **pass** — real `Local\` namespace, real `BCryptGenRandom` name |
| `wayland-info` (libwayland C) enumerates the globals | pass, directly | **pass, through a relay** (see below) |
| `wc-compositor.exe` advertises smithay's five globals | pass | **pass** |
| smithay's `ShmState` accepts `create_pool` with a section | not run | **pass** |
| `weston-multi-resource` reaches `xdg_toplevel` + configure + commit | pass | **pass** |
| the same over a deliberately fragmented stream | not run | **pass** — 3-byte writes, 15 ms apart |
| `probe-xkb.exe` (C) — every check, incl. the deliberate failure | pass | **pass** |
| `wc-xkb-probe.exe` (the Rust crate) | pass | **pass** |
| `weston-flower` (known broken, §7.5) | fails at `create_pool` | **fails identically** |

The priority list this section used to be, with its answers:

1. **`SIO_AF_UNIX_GETPEERPID` works.** `scripts/probe-accept.c` runs a server
   and a client as separate processes; the client sends its own pid as text, the
   server asks the kernel, and the two agree — 7672 and 7904, each side seeing
   the other. Stage 1's transport decision (§1.1) rested partly on this existing,
   and it exists. Two details worth carrying: the ioctl fails with
   `WSAEOPNOTSUPP` on an *unconnected* socket, so the error means "no peer"
   rather than "not implemented"; and **`WSAIoctl` reports
   `cbBytesReturned = 0` even on success**, with the pid written anyway. Code
   that validates the returned length would reject a correct answer.
   `wc_transport::peer_credentials` checks only the return code, which is right.
2. **`accept()` fills the peer address in properly**, and this is where Wine was
   actively misleading. Windows returns `len = 110, sun_family = AF_UNIX` with an
   empty path for an unnamed peer, so the predicate that `uds_windows` 1.2.1
   applies (`src/stdnet/mod.rs:148`) passes and the stock crate would have
   accepted every connection. Wine returns `sun_family = 0` and the stock crate
   rejects every connection. Our `accept` passes a null address and never asks
   the question, which remains the right thing to do — but it is now known to be
   a workaround for a Wine bug rather than for a Windows one.
3. **wepoll survives a partial read.** The relay can fragment the client's
   stream, and at three bytes every 15 ms a Wayland message header cannot arrive
   in one read: the server must park a partial message and be woken again. All
   four xdg-shell milestones still land. That is the first time re-readiness has
   been exercised anywhere.
4. **Named sections cross a process boundary on the real namespace.** The server
   logs the descriptor it received — `Section { name:
   "Local\\wc-52bec8b6…", size: 4096 }` — and then the two markers it read out of
   the mapping. It can only have opened the object by name.
5. **`libxkbcommon.a` runs in a real `.exe`**, both the C probe and the Rust
   crate, keycode 38 → `a`, +Shift → `A`, `get_as_string` round-tripping, and
   `xkb_keymap_new_from_names` failing exactly as designed because there is no
   xkeyboard-config tree. §7.2 holds.
6. **The 56 MB debug binary loads like any other.** `wc-compositor.exe` (56.8 MB)
   and `wc-server.exe` (16.5 MB) both take ~62 ms beyond their one-second idle
   timeout, and are indistinguishable from each other. Size costs nothing here.
7. **QPC resolves to 100 ns** (10 MHz), and two back-to-back calls really do
   observe a 1-tick difference, so §7.7 item 5's fix has the resolution it needs.
   QPC is boot-based: it differs from `GetTickCount64` by 5.15 s over 4.3 hours
   of uptime (0.03%), which is this guest being hibernated and resumed rather
   than a different epoch.

#### Where Wine was a poor proxy, specifically

Two stages of decisions rest on Wine, so the divergences matter more than the
agreements. There are three, and they are not the same kind of wrong.

* **It reported a function as absent that is present.** Wine implements no
  `IOC_VENDOR` ioctl, so `SIO_AF_UNIX_GETPEERPID` failed there in a way
  indistinguishable from the OS not having it. Had that been read as the answer,
  §1.1's comparison against a named pipe would have lost one of its arguments to
  a Wine limitation. It did not, because the code kept the call and treated
  failure as non-fatal — but the general lesson is that **a negative result under
  Wine is not a result**.
* **It reported a struct as malformed that is well-formed** — §4.4's second
  example, now closed from the other side. Wine's `sun_family = 0` broke
  `uds_windows` entirely and cost real time; Windows never had the problem.
  Wine found a bug that does not exist.
* **It flattered a timing primitive by 15×, and this one is still live.**
  `Sleep(1)` takes **1.06 ms under Wine and 15.8 ms on real Windows** — the
  default platform timer tick, unchanged since NT. The poll loop in
  `wc-server` and `wc-compositor` sleeps 5 ms between iterations (§3.2's "honest
  about doing nothing clever"), which on Windows is really ~15.6 ms, capping
  dispatch at about 64 Hz. Nothing measured so far cared, because nothing has a
  frame deadline. **The two-thread design owes a timer decision** —
  `timeBeginPeriod`, a waitable timer, or the message pump's own wait — and it
  cannot be validated under Wine, because under Wine the problem is not there.

What Wine got right is the rest of the table, which is most of it: the wire
format, the in-band descriptor encoding, the section shim, smithay's protocol
paths and all of libxkbcommon. §4.4's framing survives intact — a pass under
Wine says the code is self-consistent and structurally possible, and says nothing
about the platform.

#### What the guest cost, and what it did not prove

There is no third-party Wayland client for Windows, and Wine's trick of backing a
Windows AF_UNIX socket with a host socket node — which is what let a native Linux
`wayland-info` talk to a mingw `.exe` for free — has no equivalent here.
`afunix.sys` creates a node on NTFS that no Linux process can open. Rather than
fall back to a matched pair of our own binaries, `scripts/probe-relay.c` forwards
a TCP connection on the guest's loopback into the compositor's AF_UNIX socket,
and `ssh -L` puts the Linux end of that pipe in an `XDG_RUNTIME_DIR`. A stock
libwayland client then opens it without knowing it is not a compositor. The
byte stream is unaltered; the compositor's socket, `afunix.sys`, the framing and
wepoll are all on the real side of the pipe.

Two things that topology does not carry, both stated so nobody reads more into
the passes than is there:

* **The peer of a relayed client is the relay.** `SIO_AF_UNIX_GETPEERPID` on
  those connections reports the relay's pid, truthfully. The ioctl is measured
  directly by `probe-accept.exe` instead, which is why that check is separate.
* **Nothing out of band crosses a TCP hop.** On Windows nothing is out of band —
  an `h` is a length-prefixed descriptor in the stream (§2.3) — so there is
  nothing to lose. On a Linux client an `SCM_RIGHTS` fd would simply vanish,
  which is the *same* failure `weston-flower` already has for a different reason
  and is why it stays a Linux-only control.

`weston-flower` was run against the real Windows compositor anyway, and fails
there exactly as it fails under Wine: no milestones, the framing desynchronised
by the `wl_shm.create_pool` that toytoolkit issues during `display_create()`.
That is worth having, because it confirms §7.5's diagnosis is about the encoding
and not about Wine.

Still not proven, and not a guest question: rendering, an HWND, presentation
timing, and §7.7 items 6 and 7 — the unsealed `SealedFile` in particular is a
design gap, not a platform one.

> **Superseded in part by §8.** The HWND and the blit now exist and are
> exercised under Wine; presentation timing and the two `SealedFile`/`with_data_mut`
> gaps are unchanged. §8.11 is the stage-3 guest list.

---

## 8. Stage 3: a surface becomes a window

Written 2026-08-13, on top of the guest verification in §7.8. Stages 1 and 2
built a protocol server; this is the stage where it becomes a compositor.

### 8.1 Status in one paragraph

**One `HWND` per `xdg_toplevel`, with Windows' own caption, border and buttons,
showing the client's `wl_shm` pixels blitted with GDI.** `set_title` reaches
`SetWindowTextW`, the close button becomes `xdg_toplevel.close`, a `WM_SIZE`
becomes a configure, and damage limits both the copy out of the client's buffer
and the region written to the screen. The milestone is a screenshot taken by
`xwd` from the X server — outside the process, by a tool that is not ours —
showing a titled, decorated window containing an animation; and it is measured
rather than admired, because the client paints known colours at known
coordinates and ImageMagick finds them there. `scripts/wc-window.sh` is 19
checks on Linux and 18 under Wine, 0 failures. The screenshot itself is
`docs/stage3-window.png`, kept because it is the deliverable of the stage and a
table of numbers is not the same as seeing the window.

### 8.2 The two threads, and where the state lives

The brief's first hard question. The two-thread design was settled in stage 1
(§3.3: no `poll_fd`, so the backend cannot nest in `calloop`); this stage had to
build it, and building it is mostly deciding what may touch what.

**The main thread pumps Win32 messages; the Wayland dispatch loop is spawned,**
and every smithay object is constructed *inside that thread's closure*. That
last clause is the whole trick. smithay's state is thread-affine in practice —
`Rc`, `RefCell`, `wayland_server`'s non-`Send` handles — and creating it inside
the spawned closure means **the compiler enforces the boundary**: the only things
that can cross are the plain-data messages in `platform/mod.rs`, because nothing
else is `Send`. The reverse split does not compile, which is a good property for
a rule to have.

Three concrete decisions came out of it, each the answer to a way of getting it
wrong:

* **The wakeup is a `PostMessage` to a hidden message-only window, never
  `PostThreadMessage`.** A thread message has no `hwnd`, so `DispatchMessage`
  has nowhere to put it, and the modal loops `DefWindowProc` runs during a
  window drag or resize discard it. The symptom would be a compositor that stops
  repainting for exactly as long as the user holds the mouse on the title bar —
  which reads as a rendering bug and is not one. Posting to a window works
  because those same modal loops dispatch window messages for every window on
  the thread.
* **Pixels do not travel in a message.** Each window gets an
  `Arc<Mutex<Shadow>>` at creation and both threads hold it; `ToUi::Present`
  carries only damage rectangles. A client committing at 60 Hz would otherwise
  allocate and copy a framebuffer into a queue sixty times a second.
* **No `&mut` to per-window state exists anywhere in `win32.rs`.** This is the
  one that would have bitten. Almost every interesting Win32 call sends a message
  to our own window procedure *before returning* — `SetWindowPos` sends
  `WM_SIZE`, `ShowWindow` sends `WM_SHOWWINDOW`, `SetWindowTextW` sends
  `WM_SETTEXT`, `DestroyWindow` sends `WM_DESTROY` — so the window procedure runs
  inside the command handler, on the same thread, with the same data reachable. A
  `RefCell` borrowed across a `SetWindowPos` is a panic; a `&mut` handed through
  `GWLP_USERDATA` is undefined behaviour. Both are *intermittent*, because they
  need the reentrant path to be taken, which is the worst failure mode available.
  The state is therefore an `Rc<WindowState>` whose mutable fields are `Cell`s,
  and both sides take shared references, so the overlap is sound **by
  construction** rather than by anyone being careful.

The cost, stated: UI events reach the client on the next turn of the poll loop,
which is a 5 ms sleep. §8.7 is why that number is not being changed.

### 8.3 Resize is a negotiation, and something has to be on screen meanwhile

The brief's second hard question, and the one with no free answer. Windows
resizes the window and wants it painted *now*; Wayland requires the compositor to
send a configure and wait, possibly for several frames, for the client to commit
at the new size.

**The window shows the client's last buffer at its true size, anchored at the
content origin, with the remainder filled flat.** Not stretched. Three reasons,
in the order they matter:

* Stretching is a lie about the content. `StretchBlt` of text is unreadable, and
  `HALFTONE` on a window-sized bitmap every mouse-move is real CPU.
* Growing shows a band that fills in a frame later, which is what X11
  compositors have always done and what users already read as "the application is
  catching up". Shrinking clips, which is free and correct.
* It makes the negotiation **visible**. If a client stops acking configures the
  band never fills, and that is a bug somebody can see — where a smooth stretch
  would hide a stuck client indefinitely.

Two supporting details, both arrived at by getting them wrong first. The window
class does **not** set `CS_HREDRAW | CS_VREDRAW`: those invalidate the whole
client area on every resize message, which during a drag is a full repaint per
mouse-move and a visible flash under content that did not change. And
`WM_ERASEBKGND` is handled rather than left to a class brush, excluding the
content rectangle first, so the only pixels written twice are the ones outside
the buffer.

On the Wayland side the whole negotiation is `send_pending_configure`, which
sends nothing when the size is unchanged — necessary because Windows emits a
`WM_SIZE` per pixel of a drag.

**Measured end to end with a third-party client:** the simulated resize makes the
compositor send a real `xdg_toplevel.configure`, and `weston-terminal` honours it
— its first buffer is 806x491 and its last is 914x571, so it reallocated and
recommitted at a new size, and the shadow reallocated and reported full damage.

### 8.4 Where the pixels are, kept behind a seam

Stage 3 has exactly one source of pixels, and a compositor that only ever wanted
that could call `smithay::wayland::shm::with_buffer_contents` from its `commit`
handler in four lines.

It is a trait anyway (`crates/wc-compositor/src/surface.rs`), because
`docs/goal-firefox.md` puts the client on the **other side of a hypervisor**. A
Windows section name is not something a Linux process can create or open, so
same-machine `wl_shm` is a stepping stone rather than the destination, and the
buffer eventually has to live in shared_vulkan's arena. The cost of the seam now
is one file; the cost of discovering later that the section-name assumption is
spread through the commit path, the damage tracker and the blitter is not
bounded. Everything downstream of `Shadow::blit` deals in bytes and rectangles
and has never heard of a section, and the compositor logs which source served
each window — a stage that added a second provider with no way to tell which one
ran would not really have added it.

**The shadow copy is the other load-bearing decision, and it is about
`wl_buffer.release`.** Wayland lets a client reuse a buffer the moment it is
released, and a single-buffered client stalls forever if the compositor holds
its buffer. But Win32 asks for pixels at times Wayland knows nothing about:
`WM_PAINT` fires when a window is uncovered, dragged back on-screen or restored,
with no commit in sight. A compositor blitting straight from client memory would
have to hold every buffer until the next `WM_PAINT` that might need it, which is
"until the window closes". One `memcpy` per damaged row buys the right to release
immediately, and it is what makes the pixels safe to hand to another thread.

Two smaller facts worth recording:

* **Wayland's `Xrgb8888` and a 32-bit `BI_RGB` DIB are the same bytes in the
  same order** — B, G, R, X — because both are a little-endian `u32` of
  `0x00RRGGBB`. That is why the blit is a per-row `memcpy` and not a swizzle, and
  it is asserted in a test rather than trusted in a comment.
* **`SetDIBitsToDevice` is passed the whole image with a zero source origin**,
  and the clip region `BeginPaint` installs decides which pixels reach the
  screen. `CreateDIBSection` + `BitBlt` would be faster, but
  `SetDIBitsToDevice`'s `ySrc`/`uStartScan` are documented in terms of a
  *bottom-up* DIB and their meaning for the top-down one we need (negative
  `biHeight`, matching Wayland's origin) is genuinely ambiguous. So damage limits
  two of the three things it should — how much is copied out of the client's
  buffer, and how much is written to the framebuffer — and not the size of the
  DIB handed to GDI. A known constant factor on an already-CPU path; the fix is
  the DIB section when a measurement says it matters.

### 8.5 The evidence, in two halves, and what each is worth

> **[CLOSED BY STAGE 4 — §9.8.]** The split below is real for stage 3 and the
> reasoning is unchanged, but the caveat it ends on no longer stands.
> `wc-proxy` terminates the Wayland connection on the client's side of the
> boundary, so `weston-terminal` and `weston-presentation-shm` — unmodified,
> stock libwayland, no knowledge of this project — now drive the whole buffer
> path **and** produce an `HWND`, in one experiment. What follows is why that
> was not possible when it was written.

This is the part to read sceptically, because the stage cannot make one claim
with one experiment.

§7.5's finding governs everything: **no third-party Wayland client can send a
buffer to the Windows build**, because an `h` is out-of-band on Unix and in-band
on Windows, and a stock libwayland client writes no descriptor bytes. So:

**Linux — the pixels and the protocol, against clients that cannot share our
bugs.** `weston-presentation-shm` and `weston-terminal`: C, libwayland, upstream
xdg-shell, no knowledge of this project. They drive the whole buffer path — pool,
buffer, attach, damage, commit, release, frame callback — and the harness reads
the result back as PPM and measures it with ImageMagick. Twelve presents with
**twelve distinct images** from the first (a client whose pixels never change
would pass a present count and fail this — `weston-smoke` is exactly that, 244
presents and one image, because it animates only under pointer input); a 914x571
frame with 234 distinct colours from the second, which is a legible terminal
window with text in it. What this half cannot do is produce an `HWND`.

**Wine — the window, driven by a client that is ours.** `wc-shmclient.exe`
speaks the dialect. **This is a real weakening and should be read as one:**
`weston-terminal` cannot share a bug with the compositor and this program can.
What it buys is the half nothing else can reach — a named section created by one
Windows process, opened by name by another, blitted by GDI into a window with
Windows' own caption.

The screenshot is what makes the second half worth anything, and it is measured.
The client paints a deliberately checkable picture: a flat field, a one-pixel
border, and 16x16 markers in pure red, green and blue at three corners and
**not** the fourth. On the captured X root:

| measured on the screenshot | result |
|---|---|
| the client's background field | `418x298+5+31` — a 420x300 buffer less its 1 px border, so `AdjustWindowRectEx` was exact |
| red marker | 256 px at `+5+31` → buffer `(1,1)` |
| green marker | 256 px at `+407+31` → buffer `(403,1)` |
| blue marker | 256 px at `+5+313` → buffer `(1,283)` |
| bottom-right corner | unmarked, as drawn |
| the moving bar | **exactly one**, `24x100+181+130` |
| two shots 1.5 s apart | differ by 2610 px |

Every marker is at the exact coordinate the client drew it, which rules out a row
offset, a stride error, a flip and a mirror in one measurement — the asymmetric
fourth corner exists precisely so a flip cannot pass. **Exactly one bar** is the
damage result: the client ran with `--partial-damage`, erasing the old bar and
damaging only the union, so a compositor that mishandled the rectangle would
leave a trail or a stale band. Two differing shots rule out a window showing one
frozen frame.

`wc-shmclient` is double-buffered on purpose. A single-buffered client cannot
tell a compositor that *holds* its buffer from one that copies and releases, so a
use-after-release bug would look fine to it; two buffers and a strict "never draw
into a buffer that has not been released" rule make the compositor's release
timing observable at all.

### 8.6 The brief asked about `weston-simple-shm`. The answer is no, and it is measured

The brief proposed `weston-simple-shm` as the stage-3 client, reasoning that it
is pure `wl_shm` with no toytoolkit so §2.4's framing problem should not apply —
and asked for that to be checked rather than assumed.

**It applies.** §2.4 is not about toytoolkit; it is about any `h` argument at
all. `weston-flower` fails at its *first* request because toytoolkit allocates a
pool during `display_create()`; a simple-client fails at its first *buffer*,
which is later and no better. The ordering trick that carried stage 2 — surface
and toplevel before any `h` — buys a window with nothing in it, which is exactly
not this stage's milestone.

`scripts/wc-window.sh`'s third block measures it rather than asserting it: a real
Weston client reaches `xdg_toplevel` against `wc-compositor.exe` and then
delivers **0 frames**. That is a negative result under Wine, which §7.8 warns
against trusting — but it is corroborated from two directions. The mechanism is
structural rather than platform behaviour (our parser requires bytes libwayland
never writes, and Windows AF_UNIX carries no ancillary data by Microsoft's own
documentation), and §7.8 already ran `weston-flower` against the **real Windows**
compositor and saw it fail identically at `create_pool`.

Incidentally, `weston-simple-shm` is not in nixpkgs' weston at all — it builds
none of the `simple-*` clients. `weston-presentation-shm` is the closest
available equivalent and is what the harness uses.

### 8.7 The timer decision: deliberately not taken

§7.8 measured `Sleep(1)` at 1.06 ms under Wine and **15.8 ms on real Windows**,
so the poll loop's 5 ms sleep is really ~15.6 ms and dispatch caps near 64 Hz,
and recorded that the two-thread design owed a decision.

**The decision is to change nothing, and that is a decision rather than an
omission.** 64 Hz is more than this compositor needs, nothing here has a frame
deadline that misses, and the obvious fix is the wrong one: `timeBeginPeriod`
raises the system-wide timer tick and costs battery on a laptop — a cost this
project has already been criticised for in the sibling tray application. Raising
the timer resolution to serve a compositor that is idle most of the time is a bad
trade.

If a cadence is ever needed it should come from the natural source rather than a
shorter sleep: the message pump's own wait, or presentation. A compositor that
wakes when a client commits, when input arrives, or when a window message appears
is both correct and the battery-friendly shape, and it is what the two-thread
design already points at — the UI thread already blocks in `GetMessage` and wakes
on a `PostMessage`. The Wayland thread's poll loop is the remaining spinner, and
the fix for it is the one §3.3 has always described, not a timer.

The frame-callback throttle is a separate mechanism and is unaffected: callbacks
are held and answered at most every 16 ms, which is what stops a client that
draws in response to `frame` from becoming a spin loop. Measured with
`weston-smoke`: 244 presents in 5 s (~49 Hz) with the throttle, against "as fast
as the CPU allows" without it.

### 8.8 What Wine could not tell us this time

Two new entries in the running account of Wine's limits, both found by running.

* **A headless Wine cannot create a window at all.** With no `DISPLAY`,
  `CreateWindowExW` fails for **every** visible top-level window while a
  message-only window succeeds, and `GetSystemMetrics(SM_CXSCREEN)` still
  cheerfully answers 1024. `scripts/probe-window.c` isolates it: seven parameter
  combinations, every visible one failing with error 1400 or 0, across two window
  classes and both `hInstance` conventions. This is a Wine/X fact and not a
  Windows one — but it is why `Xvfb` is now a dependency in `flake.nix` rather
  than a convenience, and it is the third time Wine has produced a failure that
  says nothing about Windows.
* **Wine draws the window frame only inside a virtual desktop.** With no window
  manager on a bare X server, an ordinary Wine window is left undecorated,
  because Wine ordinarily asks the WM to decorate. `wine explorer /desktop=`
  makes Wine draw the whole frame itself, which is what the screenshot shows. On
  real Windows a `WS_OVERLAPPEDWINDOW` always gets the system frame, so the
  virtual desktop is the closest available analogue — but **the caption in the
  screenshot is Wine's rendering of a Windows caption, not Windows'**, and that
  distinction belongs in §8.11 rather than in the milestone.

One operational consequence, recorded because it cost real time: `wine explorer
/desktop=` gives its child a console window *inside* the desktop, so a shell
redirect on the outside captures the loader's chatter and nothing else. The
harness asserted against an empty log while the compositor was working perfectly
and printing into a console visible only in a screenshot. `wc-compositor --log`
now points `STD_OUTPUT_HANDLE` at a file, which is needed on the guest too: a
Windows process launched without a console has no standard output at all.

### 8.9 The gates

| Gate | Result |
|---|---|
| `cargo check --workspace --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo clippy --workspace --all-targets` (Linux) | clean, no warnings |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo test --workspace` | **62 tests**, 0 failures (was 30) |
| `scripts/wc-handshake.sh` (stage 1) | 6 passed, 0 failed |
| `scripts/wc-xkb.sh` (stage 2) | 5 passed, 0 failed |
| `scripts/wc-xdg.sh all` (stage 2) | 10 passed, 0 failed |
| `scripts/wc-window.sh linux` (stage 3) | 19 passed, 0 failed |
| `scripts/wc-window.sh wine` (stage 3) | 18 passed, 0 failed |

One operational caveat, because it will be hit and it looks like a regression.
**The four Wine harnesses share one `WINEPREFIX` and collide if run back to
back.** Observed once: `wc-handshake.sh` reported 5/1 immediately after a
`wc-window.sh wine` run and 6/0 on its own, and 6/0 again with a two-second gap
between harnesses. The previous run's `wineserver` is still shutting down and
holding the prefix. Either leave a gap or give each harness its own prefix; the
failure is contention and not code.

The 32 new tests are mostly in two places that exist because they *can* be
tested: `surface.rs`'s blitter (damage clipping, stride padding, resize
reallocation, refusing a short buffer) and `wc-shmclient`'s `proto` module. The
second is deliberate structure rather than diligence — **`cargo test` cannot run
for the Windows target** (§4.1), so anything behind `cfg(windows)` is checked by
the compiler and by nothing else. `wc-shmclient` is therefore a library plus a
thin binary, with every byte-composing function built on both platforms and only
the socket loop behind the gate. Its tests walk each composed message stream the
way a server's parser does and assert it ends exactly on a boundary, for titles
and descriptors of every length mod 4 — precisely the class of bug that presents
as §2.4's silent desynchronisation.

One deliberate weakening of the Linux harness is worth naming, because it is the
kind of thing that quietly becomes a lie. The reverse direction — resize,
activation and close — has no real driver on Linux, since there is no window
manager; `--simulate-wm` plays one on a schedule. The events it generates are
real `FromUi` events taking the same path a `WM_SIZE` takes, and the *client's*
response to them is entirely the client's, so "`weston-terminal` honoured the
close and exited 0" is a genuine third-party result. What is synthetic is only
the button press.

That schedule was measured into its current shape twice. Frame-counted triggers
alone never fired for `weston-terminal`, which presents five frames and stops
because a terminal redraws only when its content changes; and once the close had
a deadline, the earlier close meant its resize was never reached either. Both
triggers are now "count **or** deadline, whichever first". **Static clients are
the normal case, not the corner case** — a real application spends almost all its
life not drawing — so a schedule that only advances on frames tests the reverse
direction against animations and nothing else.

### 8.10 What is not proven, updated

§7.7's list, with this stage's changes:

1. ~~Nothing has run on Windows.~~ **Closed by §7.8 — for stages 1 and 2.
   Stage 3 has not.** Every window, every blit and every screenshot in this
   section is Wine, and Wine's `user32` and `gdi32` are reimplementations. This
   is the largest single caveat here, and §8.8 is two fresh reasons to take it
   seriously.
2. **`SealedFile` is still not sealed on Windows** (§7.7 item 6). Unchanged, and
   still the only place this port is weaker than the Unix original.
3. **`with_data_mut` is still unimplemented** (§7.7 item 7). Still no caller —
   the compositor reads client buffers and never writes them.
4. **`KeyboardHandle::new` still cannot succeed on Windows** (§7.7 item 8).
   Untouched: this stage does no input and nothing in it calls `compile_keymap`.
   It remains the first thing stage 4 needs.
5. **New: alpha is discarded.** GDI treats a 32-bit `BI_RGB` DIB as opaque, so an
   `Argb8888` surface is drawn solid. The compositor says so per window in its
   log rather than hiding it. A translucent or shaped window will look wrong; the
   fix is a layered window or `AlphaBlend`.
6. **New: buffer scale and transform are honoured only in their identity form.**
   Anything else needs damage mapped between surface and buffer coordinates and a
   blit that rotates or subsamples, which is a renderer's job. The compositor
   logs a warning rather than silently drawing the wrong thing.
7. **New: the icon path is implemented and unexercised by any client.**
   `WM_SETICON` works — `--debug-icon` synthesises one so the code has actually
   executed — but nothing derives an icon from `set_app_id`, and per
   `docs/goal-firefox.md` nothing can until something resolves the app-id on the
   machine the *client* is on. The command carries **pixels and never a path**,
   which is the part that had to be right now rather than later.
8. **New: there is no scene graph.** Subsurfaces are accepted and ignored;
   popups get a configure and no window. `weston-subsurfaces` maps its main
   surface and presents one frame, which is correct for what exists. Stage 4.
9. **New: DPI awareness is requested and unverified.**
   `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` is resolved by
   `GetProcAddress` and reports `ok` under Wine, which says only that the call
   was made. On a scaled desktop an unaware process gets virtualised windows and
   a blurred buffer, which for this compositor is a correctness bug rather than
   polish.

### 8.11 What to re-check on the guest

§7.8 closed the stage 1 and 2 list. This is the stage 3 one, and it is short
because `scripts/wc-guest.sh` already does the deployment.

1. **A window appears at all**, and `CreateWindowExW` succeeds outside a virtual
   desktop. Everything in §8.5 rests on this and Wine is a poor witness for it.
2. **The caption is Windows'.** The screenshot shows Wine's rendering of one; the
   claim the brief asked for is that the title bar, borders and buttons are the
   system's own. On the guest that is a screenshot and a glance.
3. **`SetDIBitsToDevice` with a negative `biHeight`.** The top-down path is the
   one place the documentation is ambiguous (§8.4) and the most likely to differ
   between Wine's GDI and the real one. An upside-down window is the failure, and
   the asymmetric corner markers make it unmissable.
4. **The modal resize loop.** Dragging a border by hand is the thing the
   message-only-window decision in §8.2 exists for, and nothing automated
   exercises it. The failure is a window that stops repainting while the mouse is
   held down.
5. **`SetProcessDpiAwarenessContext` on a scaled display**, at 150% or 200%.
6. **`WM_SETICON` in the real taskbar**, which is where an icon is actually seen
   and where Wine's virtual desktop has nothing to show.
7. **Whether the system frame and a client's own decorations collide.**
   `weston-terminal`'s dumped frame contains a full toytoolkit title bar, so on
   Windows that window would have two. That is the tension `docs/goal-firefox.md`
   describes, arriving early and from an unexpected direction — a stage 4 design
   question rather than a bug, and the reason `FrameStyle::extend_top` exists
   already.

---

## 9. Stage 4: the host-side proxy, and an unmodified client in a Windows window

### 9.1 Status in one paragraph

**`weston-terminal` and `weston-presentation-shm`, unmodified, now paint into
windows created by `wc-compositor.exe`.** A new Linux binary, `wc-proxy`, is a
Wayland server to the local client — a real `AF_UNIX` socket, `SCM_RIGHTS` and
all — and a client of the Windows compositor in the dialect of §2.3. The
client's `wl_shm` pool descriptor is received here, mapped here, and goes no
further; what crosses is a **name the compositor opens itself** or an **offset
into a region it mapped before any protocol traffic existed**. §8.5's
split-evidence caveat is closed: one experiment now produces both halves.

### 9.2 Why this had to exist, and why §2.4 predicted its shape

§2.3 made `h` arguments in-band because Windows `AF_UNIX` carries no ancillary
data and `DuplicateHandle` would mean a compositor holding `PROCESS_DUP_HANDLE`
over every client. That decision stands. Its consequence was measured twice
independently — §7.8's `weston-flower` against the **real guest**, and §8.6's
Weston client under Wine reaching `xdg_toplevel` and then delivering **zero
frames** — and it is fatal to `docs/goal-firefox.md` on its own terms: Firefox
is a stock libwayland client, and no stock libwayland client can send a buffer
to the Windows build.

§2.4 already named the way out, and it is worth quoting because this stage is
that paragraph implemented rather than a new idea:

> "It does not foreclose Architecture B. In the 'Linux clients over a proxy'
> topology the proxy terminates the Wayland connection and re-materialises fds
> at the boundary anyway — it speaks ordinary Wayland to the Linux client and
> ours to the compositor. The deviation is confined to the hop that was always
> going to be special."

### 9.3 It is a wire forwarder, not a pair of protocol implementations

The obvious build is `wayland-server` on one side and `wayland-client` on the
other with a typed handler per interface. **`wc-proxy` is a byte forwarder
instead**, and the reason is `docs/goal-firefox.md` rather than economy: Firefox
binds `wl_seat`, `wl_output`, `wl_data_device_manager`, `xdg-decoration`,
`wp_viewporter`, `wp_fractional_scale` and whatever else it turns out to want,
and a typed proxy needs code for every one. A forwarder needs code only for the
messages that carry an `h`, and there are **four in the whole of `wayland.xml`
and `xdg-shell.xml`** — asserted as a set in a test, so that adding a protocol
with a fifth is a visible change rather than a silent one:

| message | direction | status |
|---|---|---|
| `wl_shm.create_pool` | request | the stage |
| `wl_keyboard.keymap` | event | implemented, unexercised (§9.10 item 5) |
| `wl_data_offer.receive` | request | named error, stage 5 |
| `wl_data_source.send` | event | named error, stage 5 |

**What it does need is a signature table**, and that is the one place this
project pays a cost §2.3 says it escapes. §2.3's claim is exact and still true:

> "the type is on the wire. This is the cost waypipe could not avoid — it needed
> a full protocol parser because one cannot determine which passed file
> descriptor corresponds to which message without parsing the message."

That escape is real **on the Windows side**, where a descriptor is
self-describing bytes in the body. It does not reach the host side, where the
client is a stock libwayland client whose `create_pool` carries zero bytes of
body and an fd in a control message. So the proxy pays waypipe's cost once, on
the side that has fds, and the compositor still does not. That asymmetry is the
design working rather than a hole in it.

The table is read from **the vendored XML** — `wayland.xml` copied byte for byte
from `vendor/wayland-server`, `xdg-shell.xml` from wayland-protocols 1.49 —
parsed with the same `quick-xml` the scanner uses. A hand-written table would
fit today and would be a second implementation of the protocol to keep in step
with the first; reading upstream's own description means a protocol is added by
copying a file, and means the proxy's idea of the wire format cannot disagree
with the compositor's.

Object ids get their interface from exactly three places and there is no fourth,
which is what makes complete knowledge of the object graph possible from
watching alone: object 1 is `wl_display`; a `new_id` argument whose XML names an
interface; and `wl_registry.bind`'s untyped `new_id`, the only place the wire
carries the type instead of the table. Ids are retired on
`wl_display.delete_id` and on nothing else — a client sending a destructor does
not free the id, the server does, and retiring early would mean disagreeing with
the compositor for exactly as long as a round trip takes, which is precisely
when a client is allowed to have messages in flight.

### 9.4 The rule, and the two things that may cross

**No descriptor and no handle crosses the boundary, ever, even here where both
sides are on one machine.** The boundary is always treated as a VM boundary. A
descriptor created locally out of memory that side already reaches is fine; a
descriptor *passed* is not. A name crossing, which the peer then opens itself,
is the sanctioned pattern.

`SCM_RIGHTS` on the client socket is not an exception to that: it is between the
client and the proxy, both on the host, and the fd it carries is created by the
client and consumed by the proxy. The upstream writer has **no fd path at all**,
so a descriptor cannot leak across by accident — there is no code that would
send one.

Two transports satisfy the rule, both now constructed, and `wc_handle` gained
one new descriptor kind for the first:

| transport | what crosses | what the peer does |
|---|---|---|
| `Descriptor::SharedFile { path, size }` | a path | `CreateFileW` + `CreateFileMappingW` |
| `Descriptor::ArenaSlice { arena, offset, len }` | three integers | arithmetic |

**`SharedFile` is new and `ArenaSlice` is not.** `ArenaSlice` has been in the
type since stage 1 precisely so that constructing it later would not be a
wire-format break (§2.2's option (c)); stage 4 constructs it. `SharedFile`
exists because `Local\wc-…` is a name only the NT object manager understands and
a Linux process can neither create one nor open one — a filesystem path is the
one name both namespaces share.

**Measured before anything was built on it** (`scripts/probe-filemap.c`, with
its Linux half `probe-filemap-peer.c`): a Wine process's `CreateFileMappingA`
view of a `Z:` path is coherent with a Linux `MAP_SHARED` mmap of the same file,
in **both directions**, with no flush and no reopen. The handshake is
deliberately two-way, because a one-way check passes if the reader merely
happens to fault in a page the writer already flushed, which is a much weaker
property than shared-page coherency and is not enough to carry a frame that is
being written while the compositor reads it. **This must be re-measured per
deployment**: virtio-fs without DAX caches file pages in the guest, and there
the two sides would each see their own copy — a correctness failure that looks
like a rendering bug. That is §9.11 item 1.

An arena is not a per-message fact and cannot be one. It is attached **out of
band, before any protocol traffic**, by whatever brought the two sides together:
shared_vulkan's cold-path handshake (its design.md §4.3) in the real deployment,
a `--arena id:size:path` flag on both processes here. `wc_handle::arena` is a
process-global registry for that reason. Threading it through `Handle::map()`
instead would mean a diff through smithay's `ShmState`, its `Client` data and
its `wl_shm` dispatch — exactly the "spread through the commit path" cost §8.4
says the seam exists to avoid. The fork's diff for arenas is **zero lines**.

**Which mechanism served a frame is counted rather than inferred.** All three
kinds arrive through the same smithay code, so the compositor cannot tell them
apart from its own side; `Handle::map()` can, because it is the function that
opens them, and it keeps a counter per kind. Every window logs
`pixels served by wl_shm (mapped: N section, N shared-file, N arena-slice)` and
the harness asserts on it. §8.4's rule — *"a stage that added a second provider
with no way to tell which one ran would not really have added it"* — applied to
a stage that added two.

### 9.5 The copy, and the question the brief asked

**The pool cannot be arena-backed directly, and the reason is structural rather
than an implementation gap.** `wl_shm.create_pool` is the client handing the
server memory it has *already allocated*; the protocol has no request in the
other direction, no way for a server to say "allocate your next pool here". By
the time the proxy learns a pool exists, libwayland has already called
`memfd_create`, sized it, mapped it and drawn into it. Nothing on Linux can
re-point an existing mapping at another object's pages. So the bytes must be
copied at least once on this side.

Stated honestly: **a copy per frame is the price of the client being
unmodified**, and it is paid on the host, which has spare CPU, rather than in
the guest, which has the GPU. A genuinely arena-backed pool is reachable only by
a client that *allocates* from the arena — which is what
`docs/goal-firefox.md`'s accelerated path is, where WebRender renders through
shared_vulkan into arena memory and no `wl_shm` pool exists at all. That is why
the transport seam matters more than the copy it currently performs.

So the measurable question is how much to copy. Three scopes, all correct,
differing in how much they copy and how much they must understand: `pool`
(everything, understands nothing), `buffer` (the attached buffer's extent),
`damage` (the rows the client damaged). Measured with `scripts/wc-proxy.sh cost`
— `weston-presentation-shm`, 250x250 `Xrgb8888`, a 15 MB pool, ~680 commits per
run:

| transport | scope | commits | MiB copied | µs/copy |
|---|---|---|---|---|
| shared-file | pool | 681 | 9727 | 561 |
| shared-file | buffer | 683 | 163 | 19 |
| shared-file | damage | 684 | 163 | 18 |
| arena | pool | 707 | 10099 | 1070 |
| arena | buffer | 690 | 164 | 26 |
| arena | damage | 688 | 164 | 25 |

Three things this says, and two it does not:

* **Pool scope costs 60x the bytes** — 14.3 MiB per commit against 244 KiB —
  because a client's pool is sized for its allocation habits and not for what it
  drew. It is the simplest correct proxy and it is not the one to ship.
* **`buffer` and `damage` are indistinguishable here**, which is right: this
  client damages its whole buffer every frame, so there is nothing for damage
  scoping to save. `weston-terminal` damages partially and there it wins.
* **A commit costs about 20 µs at 244 KiB**, or roughly 12 GB/s, which is memory
  bandwidth and not a bottleneck at any frame rate a window has.
* It does **not** distinguish the two transports at buffer or damage scope. The
  pool-scope figures differ between them by more than they differ between runs
  of the same configuration, so no claim is made there.
* It says **nothing about the cost across a hypervisor**, where `SharedFile`'s
  per-pool `CreateFileW` is a round trip through a filesystem daemon and
  `ArenaSlice`'s is arithmetic. That is the measurement that decides between them
  and it needs the guest (§9.11 item 2).

**One measured fix came out of this table.** The first run had damage scope at
41 µs against buffer's 20 for *identical bytes*, because a full-buffer damage
rectangle was being copied as 250 separate 1000-byte `copy_from_slice` calls
where buffer scope did one of 244 KiB. Damage-scoped copying is supposed to be
the cheap one; it only is if it does not shred a large contiguous run. A
full-width rectangle is now one copy, padding included, and the two scopes agree.

### 9.6 `wl_shm_pool.resize`, and the measurement that changed the design

A shared region cannot grow once the peer has mapped it: a Windows section's
size is fixed at `CreateFileMappingW` time (§2.5) and an arena slice's
neighbours are already allocated. The two apparent answers are to over-allocate
by some factor, or to re-key the pool onto a new object — and the second is not
available, because the pool's id belongs to the client and the protocol has no
way to say "that object now means this memory".

So: over-allocate by how much? **`weston-terminal` creates a 4096-byte pool and
then grows it nine times, by doubling, to 4190208 bytes — a factor of 1023.** No
headroom anyone would write down covers that, and the first version of this
proxy announced the pool immediately and then failed all nine resizes, logging
`resize to 12288 exceeds the 4096-byte region` nine times and presenting zero
frames.

The same `--trace` run showed the way out. All nine resizes arrive **back to
back, before any other request**, because toytoolkit sizes its pool during
`display_create()` and only afterwards makes a surface. So `create_pool` is now
**held** until the client does something that is not a resize on that pool, and
the pool is announced once, at the size the client settled on. Zero
over-allocation and zero failed resizes for the measured clients. It is a delay
and not a reordering: any other message flushes the pending pools first, so the
upstream order is the client's own apart from the absorbed resizes.
`--pool-headroom` survives for a client that resizes *later* — which neither of
these does, but a window dragged bigger will — and a resize past it is counted
and named rather than silently producing a short buffer.

### 9.7 The lifetime rule, arriving from the direction §2.5 warned about

§2.5 is the one thing that section says a caller must know:

> "Sending a descriptor transfers nothing… a sender that drops its handle
> immediately after `send` — which is legal on Unix — leaves the receiver with a
> name for an object that no longer exists."

**`weston-presentation-shm` does exactly that**, and not as an edge case: it
creates a pool, creates its buffers, and destroys the `wl_shm_pool` at once,
which is idiomatic because the `wl_shm` spec keeps the memory alive for the
buffers. All three requests reached the proxy before the compositor had read any
of them, so releasing on `destroy` unlinked the file the compositor was about to
open — `could not map SharedFile { … }: File not found`, delivered to the client
as a `wl_shm` protocol error, **zero frames presented**.

The fix is the receiver-side eagerness §2.5 asks for, implemented on the side
that can observe it. A pool's region is released only when all three hold:

1. the client has destroyed the `wl_shm_pool`;
2. **every buffer cut from it is gone** — the spec's own rule, and the one a
   first fix missed: releasing on the compositor's acknowledgement alone gave
   784 presented frames with **one** copy behind them, a window frozen on frame
   one while the compositor cheerfully redrew the same bytes 783 more times;
3. the compositor has sent `wl_display.delete_id` for the pool, which is a real
   acknowledgement that it processed the destructor and therefore opened the
   name.

All three are asserted in one test, each with the failure it prevents in its
comment.

**One diagnostic change came out of this and is worth keeping.** The failure was
invisible: `shm/handlers.rs` reports `Failed to mmap fd {fd:?}` because it holds
the handle and not the error, and the one line in `shm/pool.rs` that said *why*
was a `debug!` in a binary with no `tracing` subscriber. It is an `eprintln!`
now. A protocol-fatal failure is not a debug detail.

### 9.8 The evidence, in one piece

This is the part §8.5 could not write. There, the pixels and the protocol were
proved on Linux against clients that cannot share our bugs, and the window was
proved under Wine against a client that is ours — *"a real weakening and should
be read as one"*. `scripts/wc-proxy.sh window` is one experiment producing both,
and it runs each client over each transport so the result is a property of the
path rather than of one client's habits.

Everything below is measured from **outside every process involved**: `xwd`
reads the X root, ImageMagick measures it, and the compositor's own
`--dump-frames` output — new on Win32 for this stage, and the second thing §8.5
lacked — is read back as PPM by `identify`.

| measured | `weston-terminal` | `weston-presentation-shm` |
|---|---|---|
| reached `xdg_toplevel` through the proxy | yes | yes |
| a real `HWND` | yes | yes |
| frames of the client's pixels in it | 3 | 778 |
| what the compositor opened, shared-file run | 2 shared-file, 0 section, 0 arena | — |
| what the compositor opened, arena run | 2 arena-slice, 0 section, 0 shared-file | 1 arena-slice, 0 section, 0 shared-file |
| descriptors received / sent onward | 3 / **0** | 1 / **0** |
| title reaching the caption | `"Wayland Terminal"` | — |
| `set_app_id` | `org.freedesktop.weston.wayland-terminal` | — |
| window geometry | 806x491, as asked | — |
| dumped frame | 869x555, 234 distinct colours | 250x250 |
| two shots 1.5 s apart | identical (static client) | **24864 pixels differ** |
| the primaries on screen | — | red, green and blue all present |
| the picture's extent on screen | — | 219x213, inside a 250x250 buffer drawn 1:1 |
| orientation | 27 light rows top half, 6 bottom | — |

Five of those deserve a sentence each, because they are the ones that would pass
by accident otherwise.

**"Descriptors received / sent onward: 3 / 0"** is the rule, checked rather than
assumed. A run that got the pixels there by passing something would produce an
identical picture and fail this line.

**The transport counts** come from `wc_handle::Handle::map`, the function that
actually opened the object, and not from anything the compositor could infer.

**The orientation check took three attempts and each failure is instructive**,
because each looked like the right measurement. A band at the very top and the
very bottom of the buffer: both are toytoolkit's drop shadow, 0 against 0. A
band a tenth of the way in: lands just below the title bar, 0.20 against 0.01 —
right sign, unconvincing margin. The *brightest* row of each half: 0.95 against
0.76, because the frame's bottom border is a light line one or two rows tall and
a maximum cannot tell a title bar from a border. Counting light rows separates
them, because a title bar is tens of rows and a border is one or two. This is
§8.11 item 3 — `SetDIBitsToDevice` with a negative `biHeight`, the one place the
documentation is ambiguous — answered under Wine and still owed on the guest.

**`219x213 inside 250x250`** is the scale check. A DPI-virtualised window (§8.10
item 9) would be drawn larger and a stride error would smear the wedges past the
buffer's width; neither fails a "the colours are there" check.

**The buffer being 869x555 for an 806x491 window is correct**, not a bug:
toytoolkit draws a drop shadow around its frame and declares the visible part
with `xdg_surface.set_window_geometry`. Asserting equality there would have been
asserting that the client does not do something it is entitled to do.

**And the honest limits.** This is Wine, and Wine has now been wrong four times
on this project, so a *negative* here is not a result — `scripts/wc-guest.sh` is
the arbiter and §9.11 is the list. What a pass buys is that the whole chain runs
end to end: third-party client, host proxy, in-band descriptor, named shared
memory opened by the peer, GDI, an `HWND`, with nothing passed. One Wine
artefact was diagnosed rather than reported as a finding, and it is exactly the
kind that would have been written up as a limitation: three compositors run back
to back **in one virtual desktop name** collide, because the previous
`wineserver` is still winding down, and `CreateWindowExW` then fails for every
visible top-level window — §8.11 item 1's failure mode arriving as a test
artefact. A distinct desktop per run and a three-second gap fixed it, and §8.9's
warning about the shared `WINEPREFIX` is now a sharper rule than it was.

### 9.9 The gates

| Gate | Result |
|---|---|
| `cargo check --workspace --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo clippy --workspace --all-targets` (Linux) | clean, no warnings |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo test --workspace` | **116 tests**, 0 failures (was 62) |
| `scripts/wc-handshake.sh` (stage 1) | 6 passed, 0 failed |
| `scripts/wc-xkb.sh` (stage 2) | 5 passed, 0 failed |
| `scripts/wc-xdg.sh all` (stage 2) | 10 passed, 0 failed |
| `scripts/wc-window.sh linux` (stage 3) | 19 passed, 0 failed |
| `scripts/wc-window.sh wine` (stage 3) | 18 passed, 0 failed |
| `scripts/wc-proxy.sh window` (stage 4) | **33 passed, 0 failed** |
| `scripts/wc-proxy.sh cost` (stage 4) | 6 configurations measured |

The 54 new tests are concentrated where they can be run, which on this project
means "not behind `cfg(windows)`" (§4.1). `wc-proxy` is Linux-only by design —
it is the host half of a host/guest pair and its whole job is to receive a
descriptor, which is the one thing the Windows side must never do — so all of it
is testable, and the tests drive the entire `wl_shm` path with a scripted client
against a real `memfd`, reading the result back through `wc_handle::Handle::map`
exactly as the compositor does. The picture they paint is stage 3's: markers in
three corners and **not** the fourth, so a flip cannot pass.

Two are worth naming. `every_message_in_every_protocol_round_trips` encodes and
walks all 177 messages of both vendored protocols in both dialects, because the
proxy forwards messages it does not understand and a walk that is wrong about
one of *those* corrupts the stream for everything after it.
`the_upstream_stream_is_well_framed_in_our_dialect` re-walks the whole
concatenated output as a compositor would, which is what catches a size field
computed from the wrong place after a splice.

### 9.10 What is not proven

1. **Nothing here has run on the real guest.** Everything in §9.8 is Wine, and
   §8.10 item 1's warning is undiminished. §9.11 is the list.
2. **Filesystem coherency is measured for one filesystem.** `probe-filemap.c`
   says a `Z:` path under Wine is coherent both ways with a Linux `MAP_SHARED`
   mmap. It says nothing about virtio-fs, which without DAX caches file pages in
   the guest and would give each side its own copy. The `SharedFile` transport is
   unusable on the real boundary until that is measured; `ArenaSlice` does not
   depend on it, which is one more reason it is the real deployment's answer.
3. **The arena here is a file, not shared_vulkan's arena.** What is proven is the
   *shape*: one region attached out of band by both sides before any traffic,
   sliced per pool at 4096-byte granularity — `sv_arena::GRANULARITY`, which is
   the measured `minImportedHostPointerAlignment` on the target — with only
   `(arena, offset, len)` crossing and nothing opened per pool. What is not
   proven is that region being guest RAM located by walking QEMU's flat view.
   Substituting the real provider changes `Arena::open_or_create` and nothing
   above it.
4. **One client, and the proxy enforces it.** Multiple clients need a session per
   connection and an arena allocator that is not per-session. Out of scope by the
   brief and not started.
5. **No input is forwarded.** `wl_seat` is bound and its messages pass through,
   but the compositor advertises no capabilities, so no `wl_keyboard.keymap`
   event is ever sent — which means the reverse `h` path is **implemented and
   unexercised by a real client**. It is tested with a synthetic keymap event
   that a real descriptor round-trips through, so the code has executed; it has
   not met a compositor that sends one. §8.10 item 4 still stands underneath it.

   > **[CLOSED BY STAGE 6 — §11.2, and the wording above was too kind.]**
   > "Implemented and unexercised" reads as a coverage gap. It was not: the
   > compositor published the keymap as a `Local\wc-…` section, which no Linux
   > process can open, so the path **would have failed the first time anything
   > used it**. The first real client to bind a seat is what found that, and it
   > found it by dying at connect.
6. **Clipboard and drag-and-drop are a named error, not a gap.**
   `wl_data_offer.receive` and `wl_data_source.send` carry an `h` that is a pipe
   rather than memory, and `Descriptor::Pipe` has no transport here. The proxy
   refuses with the message's name in it rather than splicing a zero-length
   descriptor, which would be §2.4's silent desynchronisation in its purest form.
7. **A hostile client can still fault the proxy.** It can `ftruncate` its pool
   smaller between the proxy's size check and the copy, and the proxy would die
   where `libwayland-server` recovers, because libwayland installs a `SIGBUS`
   handler and this does not. Cooperative resizes are handled; this is the
   uncooperative one. Note the asymmetry with §2.6: the *compositor* is immune
   because a Windows section cannot shrink, and it is the host side that inherits
   the Linux hazard.
8. **`weston-terminal` presents three frames and stops**, which is correct — a
   terminal redraws when its content changes — but it means the static-client
   path carries no animation evidence. `weston-presentation-shm` is what carries
   it, and the two-shot check is meaningful only for that one. §8.9 recorded the
   same asymmetry for the Linux harness and it has not gone away.
9. **Two title bars, now visible.** The screenshot shows Windows' caption around
   toytoolkit's own, which is `goal-firefox.md`'s CSD-versus-native-frame tension
   arriving in a photograph rather than in prose. §8.11 item 7 predicted it;
   nothing has been done about it and `FrameStyle::extend_top` is still zero.

### 9.11 What to re-check on the guest

Ahead of §8.11's list, because the stage-4 path reaches the guest differently.

**All five are answered in §10.** Item 1 no, item 2 decisively for `ArenaSlice`,
items 3 and 5 yes, and item 4's premise turned out to be about the client's
machine rather than the client. The list is left as written because §10 is a
better section for having a prediction to be measured against.

1. **Whether a shared filesystem is coherent at all**, which decides whether the
   `SharedFile` transport exists on the real boundary. virtio-fs with DAX should
   be; without DAX it should not be. `scripts/probe-filemap.c` and
   `probe-filemap-peer.c` are exactly this measurement and run unchanged — the
   Windows half needs only the guest's path to the shared mount.
2. **What `SharedFile` costs per pool across the boundary**, against
   `ArenaSlice`'s arithmetic. On one machine they measure the same; across a
   hypervisor the peer's `CreateFileW` is a round trip through a filesystem
   daemon and the arena's is nothing. This is the measurement that turns a
   preference into a decision.
3. **`wc-proxy` cannot reach the guest's compositor directly.** Under Wine the
   compositor's `AF_UNIX` node is a real host socket, which is the only reason
   this stage runs on one machine at all (§4.2). On the guest `afunix.sys` makes
   an NTFS reparse point and no Linux process can connect to it by any path, so
   the proxy's upstream has to be `scripts/probe-relay.c` over an `ssh -L`
   forward — which the relay's own header says it was built for, and which now
   has a client that needs it for more than a handshake.
4. **`weston-terminal`'s nine-doubling pool growth**, which is the fact the
   deferral design rests on. It is a property of toytoolkit rather than of the
   platform and should not change, but the whole design assumes the resizes
   arrive before any other request and that is worth confirming once where it
   matters.
5. **Whether the run still reports the right transport** in the
   `mapped: N section, N shared-file, N arena-slice` line, which is the only
   thing standing between "the pixels arrived" and "the pixels arrived the way we
   think they did".

---

## 10. Stage 5: the same experiment, with a hypervisor in the middle

### 10.1 Status in one paragraph

**§9.11's five questions are answered, and the answer to the second one changes
the transport.** `weston-terminal` and `weston-presentation-shm`, unmodified,
now reach a real `HWND` on the real guest through `wc-proxy`, an `ssh -L`
forward and `scripts/probe-relay.c` — 23 checks, on Windows, with nothing in the
path reimplemented by Wine. A pool costs the guest's compositor **485–578 µs to
open as a `SharedFile` against 1.6–3.4 µs to resolve as an `ArenaSlice`**, a
factor of about 250 in the warm case and about 3000 in the cold one, and that
settles §9.11 item 2 in `ArenaSlice`'s favour by a margin no amount of tuning
closes. It also produced the finding this stage did not go looking for: the
shared filesystem is **not coherent while mapped**, so the compositor reads its
own cached copy of a region the proxy is still writing, and 462 of 645 frames
from a client that redraws every frame were the same picture. Every check above
that one passed while it happened. That is recorded as a `FAIL` in
`scripts/wc-guest.sh` and it is the honest result: the pixel path works, and the
region a file on virtio-fs provides is not one it can be built on.

### 10.2 Where the tests run from, and why it is three machines

Stages 1 to 4 needed one Linux box and one guest. This one needs two Linux
boxes, and the reason is worth stating because it looks like an accident of the
lab and is not.

Everything before this stage moved **protocol bytes**, and `probe-relay.c` moves
those over TCP: an `h` argument is in band on Windows (§2.3), so there is no
ancillary data for the hop to lose. Stage 4 moves **pixels**, and a pixel has to
land in memory both sides can name. The only such memory here is virtiofsd's
export — `/srv/winshare` on the VM host, `Z:` in the guest — and that export
exists between the guest and *its own host*, not between the guest and whichever
workstation is driving the tests.

So: blocks 1–6 of `scripts/wc-guest.sh` run from the workstation, which is where
the toolchain and the third-party clients are and which reaches the guest's sshd
directly. Blocks 7–9 push `wc-proxy`, a probe and a generated driver to
`WC_HOST` — the VM host — and run the proxy and the unmodified client there,
because that is the only Linux machine the shared region is reachable from. With
`WC_HOST` unset they skip and say so rather than testing something adjacent.

This does not weaken the evidence and it does not bend the rule. The proxy's job
is to be a Wayland server to a local client and a client of the Windows
compositor, and it does that on whichever box the region is on. The client's
file descriptor is consumed by the proxy on `WC_HOST`; what crosses to the guest
is a path or an `(arena, offset, len)` triple, checked from the compositor's own
`wc_handle::map_counts` and not from the fact that pixels appeared.

### 10.3 Item 1: the export is not coherent, and it is worth being precise

`probe-filemap.c` and `probe-filemap-peer.c` ran unchanged, as §9.11 said they
would. Three measurements, because "coherent" turned out to be two questions:

| | result |
|---|---|
| local NTFS, two Windows processes, one still mapped and unflushed | **MATCH**, 0 bytes wrong of 65536 |
| the export, Windows mapping against a Linux `MAP_SHARED` mmap, both live | **MISMATCH**, 65280 wrong of 65536, both directions |
| the export, host writes and closes, guest then opens for the first time | **MATCH**, 0 bytes wrong of 4096 |

The first row is the control and it is what makes the second mean anything: the
probe can see coherency when there is coherency, so the second row is the mount
and not the probe. The 256 bytes the second row scored as correct are the
pattern's own zero bytes — `(k * 7 + 3) & 0xff` is zero exactly once every 256 —
so nothing crossed at all, rather than something crossing partially.

The third row is the one that stops this being read as "the mount is broken".
The export moves bytes perfectly well; what it does not do is let two live
mappings of the same file see each other's stores. virtiofsd here runs
`--cache=auto` with no DAX, so the guest caches file pages, and each side ends
up with its own copy — exactly what §9.10 item 2 predicted and the reason it was
written down as a prediction rather than assumed away.

### 10.4 Item 2: the number that decides the transport

`crates/wc-handle/src/bin/wc-poolcost.rs` measures the shipping path —
`Handle::map` — rather than a C imitation of it, and separates three phases
because they hide in different places. Per pool of 1 MiB, on the guest:

| transport | reach it | touch 1 MiB | release |
|---|---|---|---|
| `Section` (named, no filesystem) | 5.7–6.7 µs | 0.32–0.78 ms | 55 µs |
| `SharedFile`, local NTFS | 47.8–61.3 µs | 0.73–0.80 ms | 104–113 µs |
| `SharedFile`, the export | **485–578 µs** | 2.3–3.0 ms | 103–114 µs |
| `ArenaSlice`, the export | **1.6–3.4 µs** | first touch only | 0.2 µs |

The NTFS row is the second control and it is doing real work. A slow
`CreateFileW` is only evidence about the mount if the identical call on a local
directory is fast; without that row the whole table would be equally consistent
with "Windows is slow", and the decision would rest on an attribution nobody had
checked. It is about a tenfold difference, and it is the hypervisor's.

**The arithmetic.** An arena is attached once — 0.66–8.0 ms for 64 MiB, for the
life of the process — and every pool after that is a registry lookup and checked
arithmetic against a region already mapped. A shared file is a `CreateFileW`,
a `CreateFileMappingW` and a `MapViewOfFile` **per pool, forever**, each one a
round trip through a filesystem daemon in another operating system. At the
observed rates a client that opens three pools pays 1.5 ms against 7 µs; the
nine-pool case §9.6 measured pays 4.5 ms against 20 µs. Neither is fatal on its
own. What decides it is that the arena's number does not depend on the mount at
all, and the shared file's is the mount, measured at 10× NTFS warm and — see
below — 150× when it is cold.

**Two honest caveats.** First, the spread. The first runs after the guest
rebooted measured 6.3–9.0 ms per open, an order of magnitude above the steady
state, and four consecutive later runs sat at 485–578 µs. Whether that is
WinFsp's cache warming, virtiofsd's, or the guest's metadata cache is **unknown**
and was not chased, because the decision does not turn on it: the cold number
makes the case more strongly than the warm one. Second, the size series (4 KiB
1.3 ms, 64 KiB 6.3 ms, 1 MiB 6.9 ms, 4 MiB 9.0 ms) was taken entirely within a
cold session, so it cannot be read as size dependence. What can be said is that
the open is dominated by mount state rather than by how much is being mapped.

The `touch` column is the term a "how long does the open take" measurement
misses, and it is the one place the two transports are equal: both pay ~2.5 ms
to fault a MiB in over the export, against 0.75 ms locally, because both are the
same mount. In the real deployment the arena is guest RAM and pays it once for
the region; a shared file pays it again for every pool.

### 10.5 Item 3: the proxy reaches the guest, exactly as §9.11 sketched it

`wc-proxy --upstream` takes a path, and an `ssh -L <socket>:127.0.0.1:<port>`
puts a Unix socket at that path whose far end is `probe-relay.exe` inside the
guest, which forwards byte for byte into the compositor's `afunix.sys` node. The
proxy cannot tell it is not talking to a local socket, and does not need to.

Three configurations ran: `weston-terminal` over the arena, over a shared file,
and `weston-presentation-shm` over the arena. All three reached `xdg_toplevel`,
all three got a real `HWND` — `0x40074` in one run, and `CreateWindowExW`
succeeding at all is §8.11 item 1 answered on the guest rather than under Wine —
all three blitted the client's pixels into it, and all three reported `3 fds in,
0 out`. The rule held with a hypervisor in the path.

One caveat, stated because the screenshot §8.11 asked for still does not exist:
the compositor runs inside an ssh session, so its window is created in a
non-interactive window station. `CreateWindowExW` succeeded there and the blits
happened — the frames are dumped and decode — but **nothing was on a visible
desktop and nobody looked at it**. §8.11 items 2, 4, 5 and 6 (the caption, the
modal resize loop, DPI scaling, the taskbar icon) all need an interactive
session and remain unrun.

### 10.6 Item 4: nine doublings is a property of the client's machine

§9.11 called `weston-terminal`'s nine-doubling pool growth "a property of
toytoolkit rather than of the platform" and expected it not to change. It
changed. On the guest run the proxy reported **3 pools, 3 resizes, 0 overflows**,
with the first pool settling at 38272 bytes; under Wine on the workstation the
same binary reports **3 pools, 9 resizes, 0 overflows** and 4190208 bytes.

The cause is not the guest at all. `weston-terminal` logs `could not load cursor
'dnd-move'` on the VM host and not on the workstation, because the host has only
`hicolor` installed and the workstation has half a dozen cursor themes. Fewer
cursor images, a smaller cursor pool, fewer doublings. The pool growth is a
property of *the machine the client runs on*, which is one step further from the
platform than §9.11 assumed.

What is invariant, and is what `Session::flush_deferred` actually rests on, is
the other half of that item: **every resize arrived before the pool was used, and
there were no overflows**, in every run on both machines. That is the property
worth asserting, and `wc-guest.sh` now asserts it instead of the count.

### 10.7 Item 5: the transport line is right, and it is the only thing that was

Every run reported the transport that actually served the pixels —
`mapped: 0 section, 0 shared-file, 2 arena-slice` for the arena runs,
`mapped: 0 section, 2 shared-file, 0 arena-slice` for the shared-file one. §9.11
item 5 is answered yes.

It is worth dwelling on how little that bought, because it is the stage's real
lesson. In the `weston-presentation-shm` run every check passed: `xdg_toplevel`,
a real `HWND`, **645 frames presented**, the right transport line, no descriptor
sent on, no pool overflow. And the window was frozen. Of those 645 dumped
frames only **18 were distinct images, and one of them was 462 of them** — from
a client that redraws every single frame. The same client under Wine, over a
coherent filesystem, produced 60 distinct images out of 780 with the commonest
appearing 13 times.

The mechanism is §10.3 arriving as pixels. The compositor maps the arena once at
startup; the guest caches those pages; the proxy's writes land in the host's
copy. Fresh content reaches the compositor at about 8 Hz — whenever the guest's
cache happens to revalidate — while the client presents at 32. A page the
compositor has never touched is fetched correctly, which is why the *first* frame
of every pool is right and why `weston-terminal`, which draws three frames and
stops, looks perfect.

**The same measurement over `SharedFile` gives the same numbers**: 642 frames,
18 distinct, commonest 459. Coherency is a property of the mount and not of the
transport, so this does not choose between them. §10.4 does, alone.

`wc-guest.sh` counts distinct dumped frames and fails on this. It is the only
check in the file that is failing, it is expected to fail on this deployment,
and it should stay failing until the region is a real one: virtio-fs with DAX,
or the arena this file is standing in for — guest RAM located through QEMU's
flat view, which is not a file (§9.10 item 3). Downgrading it to a note would
make a stale window indistinguishable from a live one, which is exactly the
distinction it exists to draw.

### 10.8 Wine was wrong a fifth time, and the clock is not the clock

> **[HALF WITHDRAWN BY STAGE 8 — §13.6.]** The DPI paragraph below is wrong, and
> the title of this section is wrong with it. The call was refused because it
> was made in an ssh session's window station, where the process is *already*
> per-monitor-aware before `main` runs; on the guest's real interactive desktop
> the identical call succeeds, which is what Wine said. The clock paragraph
> stands and is measured further in §13.8.

Two things the guest said that Wine did not.

**`SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)` is refused.** §8.10 item
9 recorded that Wine reports `ok` and that this "says only that the call was
made". On the guest the same call, resolved the same way by `GetProcAddress`,
returns failure — the process is already DPI-aware by manifest, or the context
is unavailable in the station it runs in, and the compositor cannot currently
tell which. So the item is not merely unverified; the mechanism it depends on
does not work as written, and a scaled display would virtualise the window.

**`Sleep(1)` takes 15.2 ms.** Measured at a mean of 15.164–15.260 ms over 20
tries, worst 16.176 ms, against roughly 1 ms under Wine. The `QueryPerformanceCounter`
frequency is 10 MHz and its resolution really is 100 ns, so the clock is fine;
it is the scheduler tick that is not. The poll loop's 5 ms sleep is a 15 ms sleep
on this machine, which puts a floor of about 65 Hz on anything driven by it and
would put it at 15 Hz if the sleep were ever raised. Nothing here needed
`timeBeginPeriod` yet. Something will.

### 10.9 Two things that cost an hour each, both worth writing down

**Windows' sshd ignores `~/.ssh/authorized_keys` for administrators.** The stock
`sshd_config` ends with `Match Group administrators` /
`AuthorizedKeysFile __PROGRAMDATA__/ssh/administrators_authorized_keys`, so a key
appended to the home-directory file is offered, refused, and looks exactly like a
key that was not installed. This is the same shape as the two lessons in
`wc-guest.sh`'s header — a correct-looking diagnosis of the wrong layer — and it
is now the third bullet there.

**A killed compositor leaves its socket node behind.** Killing the ssh session
that started a guest process kills the process, but the `afunix.sys` node it bound
survives it, and the next `bind` fails with `could not bind socket: Could not
write to XDG_RUNTIME_DIR`. That reads as a permissions problem on a directory
that is fine. The driver now names its socket per invocation and kills stale
compositors before it starts; the same leftover also makes `scp` fail with
`dest open ...: Failure`, because a running `.exe` holds its own file open.

### 10.10 A manifest that was never true

`cargo build -p wc-handle --target x86_64-pc-windows-gnu` did not compile:
`windows-sys`' `Win32_Storage_FileSystem` feature is what provides `CreateFileW`
and `DeleteFileW`, which arrived with `Descriptor::SharedFile` in stage 4, and it
was never added to the crate's own dependency list. The crate still built,
because `wc-compositor` enables the same feature and cargo unifies features
across a workspace build, and the workspace build is the only form anyone ran.

The fix is one line. The lesson is that `cargo check --workspace` is not a check
that a crate's manifest is complete, and this project has three crates whose
manifests nobody has tested alone.

### 10.11 The gates

| Gate | Result |
|---|---|
| `cargo test --workspace` | 116 tests, 0 failures — **unchanged** |
| `cargo clippy --workspace --all-targets` (Linux) | clean, no warnings |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo build -p wc-handle --target x86_64-pc-windows-gnu` | **now builds; did not before** |
| `scripts/wc-proxy.sh window` (stage 4, Wine) | 33 passed, 0 failed |
| `scripts/wc-guest.sh run` (blocks 1–6, the guest) | 18 passed, 0 failed |
| `scripts/wc-guest.sh stage4` (blocks 7–9, the guest) | **23 passed, 1 failed** |
| `scripts/wc-guest.sh all` (deploy, all nine blocks, clean) | 41 passed, 1 failed |
| the same with `WC_HOST` unset | 3 skipped, each with its reason |

The one failure is §10.7's and it is deliberate.

The test count did not move, and that is a real gap rather than an oversight.
Everything this stage added is either a measurement binary or a harness: the
whole of `wc-poolcost`'s body is behind `cfg(windows)`, so a unit test for it
would compile on the target that never runs `cargo test` and would not run on
the one that does — the same shape §4.1 describes and the same answer. What
carries stage 5 is `scripts/wc-guest.sh`, and its evidence is in the table above
rather than in `cargo test`.

### 10.12 What is not proven, updated

§9.10's list, with this stage's changes:

1. ~~Nothing here has run on the real guest.~~ **Closed.** The proxy, the relay,
   both transports, two unmodified third-party clients and a real `HWND` are
   guest measurements now. What is *not* closed is that no human has looked at
   one of these windows — see item 3.
2. ~~Filesystem coherency is measured for one filesystem.~~ **Measured, and the
   answer is no** (§10.3). `SharedFile` is not a usable transport for a live
   surface on this deployment, and neither is a file-backed arena. §9.10 item 2
   guessed right and for the right reason.
3. **New: the window has never been seen.** Everything on the guest runs in an
   ssh session's non-interactive window station. `CreateWindowExW` succeeds and
   the blits happen, but §8.11's items 2, 4, 5 and 6 need a logged-in desktop and
   none of them has run. This is now the largest single gap.
4. **The arena is still a file, and that is now a measured problem rather than a
   stylistic one.** §9.10 item 3 said substituting the real provider changes
   `Arena::open_or_create` and nothing above it. That is still true, and §10.7 is
   the reason it has to happen before anything animates: the shape is proven and
   the substrate is not.
5. **`SealedFile` is still not sealed on Windows**; `with_data_mut` is still
   unimplemented; `KeyboardHandle::new` still cannot succeed. Unchanged from
   §8.10 items 2–4, and untouched by this stage.

   > **[PARTLY CLOSED BY STAGE 6 — §11.2.]** `KeyboardHandle::new` succeeds now:
   > `XkbConfig` gained a keymap-string field, which is what §7.2 predicted.
   > `SealedFile` is *still* not sealed and `with_data_mut` is still
   > unimplemented — and stage 6 made the first of those matter more, because
   > the keymap is now a file in a directory rather than a session-local
   > section, which is more reachable rather than less.
6. **One client, no input, no clipboard, no scene graph.** §9.10 items 4, 5, 6
   and §8.10 item 8 are all unchanged. Nothing in stage 5 went near them.

   > **[INPUT CLOSED BY STAGE 6 — §11.]** One client, no clipboard and no scene
   > graph stand. Input does not: there is a `wl_seat` with a pointer and a
   > keyboard, driven by Win32 messages, and a third-party client has been typed
   > into. §9.10 item 5 above has its own marker.
7. **A hostile client can still fault the proxy** (§9.10 item 7), and now on a
   machine the compositor cannot see.

### 10.13 What to do next, in the order the evidence puts it

> **[ALL FOUR ITEMS NOW CLOSED.]** Item 1 by stage 7 (§12), items 2, 3 and 4 by
> stage 8 (§13). §13.12 is the current list.

1. **Give the arena a real region.** Guest RAM through QEMU's flat view, which is
   what `shared_vulkan` already does and what §9.10 item 3 always intended.
   §10.7 is the measurement that turns this from the tidy choice into the only
   one, and §10.4 says it costs nothing per pool once it exists.
2. **Log in to the guest and look at a window.** §8.11 items 2, 4, 5 and 6 have
   been deferred for two stages and every one of them is a glance.
3. **`SetProcessDpiAwarenessContext` needs to report *why* it failed** (§10.8),
   because "refused" is not enough to tell an already-aware process from an
   unavailable context, and one of those is fine and the other is a bug.
4. **Decide what to do about a 15 ms tick** (§10.8) before anything depends on
   the poll loop's cadence.

---

## 11. Stage 6: a seat, and a client you can type into

Written 2026-08-13. Stages 1-4 built something you could look at; this is the
stage where it becomes something you can use.

**Branched from stage 4 and merged after stage 5**, which is why nothing below
carries a guest number: stage 5 was taking the earlier stages to the real
hardware while this was being built, and the two did not meet until the merge.
§10's findings are folded into §11.10 and §11.11 rather than re-measured, and
where they change what this stage should expect — §10.3's coherency result most
of all — that is said at the point it matters.

### 11.1 Status in one paragraph

**`weston-eventdemo` and `weston-terminal`, unmodified, receive keystrokes that
began as `WM_KEYDOWN` in a Win32 message queue.** `wc-compositor.exe` advertises
a `wl_seat` with pointer and keyboard capabilities, compiles a keymap resolved
on Linux at build time, and publishes it as a file **both sides can name** —
which is `wl_keyboard.keymap`'s `h` argument travelling in the direction stage 4
implemented and §9.10 item 5 admitted had never been exercised. Win32 scan codes
become Linux evdev keycodes, `WM_SETFOCUS` becomes `wl_keyboard.enter`,
`TrackMouseEvent` makes `WM_MOUSELEAVE` exist so `wl_pointer.leave` can, and
`SetCapture` keeps a drag alive off the window. The milestone is
`docs/stage6-typing.png`: `weston-terminal` in a window with Windows' own
caption, with `echo waycompose types` in it, typed by `SendInput`. It is
measured rather than admired — the window changed by **0** pixels in two idle
seconds and by **337** after the keys, and the client committed **21** new
frames. `scripts/wc-input.sh` is 22 checks on Linux, 24 under Wine and 6 for the
terminal, 0 failures.

### 11.2 The keymap is an `h` in the other direction, and that is the whole
trap

§9.3's table has four messages carrying an `h`, and stage 4 moved one of them:
`wl_shm.create_pool`, which the **client** creates and the compositor opens. The
proxy therefore owned every transport decision, because the proxy was the
creator.

`wl_keyboard.keymap` runs the other way. The **compositor** creates the object
and the client opens it — and `Handle::create_section`, which is what
`SealedFile` called, mints `Local\wc-…`. That is a name only the NT object
manager understands. A Linux process cannot open one and cannot spell one.

So §9.10 item 5's "implemented, unexercised" was not a coverage gap. **It was a
path that would have failed the moment anything used it**, and the reason
nothing had used it was that the compositor advertised no keyboard.

The fix is the same shape as the arena's and the reasoning is §9.4's, restated:

> "An arena is not a per-message fact and cannot be one. It is attached out of
> band, before any protocol traffic… `wc_handle::arena` is a process-global
> registry for that reason. Threading it through `Handle::map()` instead would
> mean a diff through smithay's `ShmState`, its `Client` data and its `wl_shm`
> dispatch."

`wc_handle::publish` is that registry for the outbound direction: a directory
this process writes into and the prefix a peer prepends to reach the same bytes,
set once from `--shared-dir` / `--shared-peer-prefix`. `Handle::create_shared`
asks it, and falls back to a section when nobody said — which is correct for two
Windows processes and is what a same-machine deployment wants.

**smithay's diff for all of this is one word**: `create_section` becomes
`create_shared` in `utils/sealed_file.rs`. Threading a destination through
`SealedFile` → `KeymapFile` → `KeyboardHandle` → `Seat` → `SeatState` would have
been a parameter carried through five layers of upstream code so that one leaf
could spell a path, and it is not a patch anybody merges.

**The failure is now a diagnosis rather than a mystery, and that changed too.**
`wc-proxy` used to *note* a descriptor it could not materialise and forward the
message without it. For a keymap that is §2.4's silent desynchronisation in its
purest form: the client receives a well-formed `wl_keyboard.keymap`, calls
`mmap` on a descriptor it never received, and libwayland aborts it — with a
correct-looking keymap event as the last thing in its log. It is now fatal, with
the descriptor's kind and the flag to pass in the message.

**And stage 6 broke stage 4's harness, which is the best evidence that the two
directions are different problems.** `scripts/wc-proxy.sh` passed no
`--shared-dir`, because until now the compositor never created an object for a
client. The moment it had a keyboard, `weston-terminal` — which binds `wl_seat`
— started failing at connect, while `weston-presentation-shm` — which does not —
kept passing. One flag fixed it. Nothing about the pixel path changed.

One stage-4 assertion had to change with it, and it was measuring the wrong
thing. The harness demanded the proxy send **0** descriptors to the client, and
that passed because there were none to send. It now sends one per keymap. That
is not a violation: those fds are on the host side of the boundary, manufactured
here by opening a path the compositor named, and §9.4's rule is about the
boundary. What enforces the rule is that `wc-proxy`'s **upstream writer has no
fd path at all** — by construction, not by a counter — and the harness now
checks the thing that can actually go wrong: whether every name the compositor
sent was openable here.

### 11.3 Scan codes, not virtual keys, and why that is not a preference

`WM_KEYDOWN` carries two descriptions of one keypress: a virtual-key code in
`wParam` and a scan code in `lParam` bits 16-23 with an `E0` flag at bit 24.
Only one of them can be translated correctly.

**A virtual-key code has a layout baked into it.** On a German layout the key in
the position of the American `Z` reports `VK_Y`; every `VK_OEM_*` names a
different physical key on a different layout. **A scan code is positional** and
says nothing about what is printed on the key.

Wayland is positional too. `wl_keyboard.key` carries a keycode and the client
runs *our keymap* over it — which is the entire reason a keymap is sent at all.
So a compositor translating VK codes applies the layout twice: once in Windows
to obtain the VK, and once in the client's xkb, over a keycode that already had
one applied. **On `us` the two layouts agree and the bug is invisible. On `de`
the `Z` and `Y` keys swap and it is not.**

`WM_CHAR` is unused for the same reason one layer on: it is the result of
Windows' own composition, and a Wayland client does its own with its own dead
keys. `TranslateMessage` still runs — other parts of Win32 expect it to — and
its `WM_CHAR` is simply not handled.

**The table is mostly an identity, and that is history rather than luck.** Linux
numbered its evdev keycodes from the IBM PC/XT scan codes, so for the whole base
set — `0x01` Escape through `0x58` F12, 88 codes — **the scan code and the evdev
keycode are the same number**. `KEY_A` is 30 and `A` is `0x1E`; `KEY_SPACE` is
57 and space is `0x39`; `KEY_F11` is 87 and F11 is `0x57`. Asserted at eleven
points spread across the range rather than claimed once, because "obviously an
identity" is the kind of statement that is wrong at one end.

What is not an identity is the 40 `E0`-prefixed codes — the arrow cluster, the
right-hand modifiers, the keypad's Enter and slash, the Windows and media keys —
because those were added to both schemes later and in different orders. That
half is Linux's own `atkbd` table, copied. Seven unprefixed Japanese codes are
likewise a table.

**Three keys are genuinely ambiguous on Windows and are settled by the VK**,
which is unambiguous for them precisely because no layout relabels them:

| key | why | resolution |
|---|---|---|
| Pause | claims scan `0x45`, the `E1` prefix is swallowed before the message loop | `VK_PAUSE` → `KEY_PAUSE` |
| NumLock | also claims `0x45`, with the extended bit going either way per driver | `VK_NUMLOCK` → `KEY_NUMLOCK` |
| PrintScreen | sends `E0 2A E0 37` — a **phantom shift** first | `VK_SNAPSHOT` → `KEY_SYSRQ`, and `E0 2A`/`E0 36` are dropped |

The phantom shift has its own test, because forwarding it is silent: the
client believes Shift is held, every following key produces the shifted
character, and all the keycodes look right.

An untranslatable scan code produces **nothing**, and says so with the code in
the log. Inventing a keycode would make the client's xkb produce a keysym for a
key that does not exist.

### 11.4 Layouts: what works, and exactly what does not

The set is fixed at build time and cannot be otherwise. §7.2 established that
`xkb_keymap_new_from_names` resolves against the xkeyboard-config tree, that the
tree does not exist on Windows, and that `nix/libxkbcommon-mingw.nix` compiles
in a config root nobody creates so that a `new_from_names` which silently
started working would be a visible bug. Only `new_from_string` works there.

So `scripts/gen-keymap.sh --all` resolves **us, gb, de, fr** on Linux and the
compiled text is checked in at `crates/wc-compositor/keymaps/*.xkb` and
`include_str!`ed. Checked in rather than generated by a `build.rs`, for two
reasons: a build script calling `xkbcli` would make the build depend on
xkeyboard-config, defeating the point of resolving elsewhere; and a keymap
generated at build time is a keymap nobody has read, and these files are where a
user's characters come from.

Selection is `GetKeyboardLayout(0)`'s low word, a `LANGID`, matched whole and
then on its primary language — FreeRDP's `xkb_layout_ids.c` cut down to the
layouts that actually shipped, because a table naming a hundred layouts none of
which are present is a table that lies. `WM_INPUTLANGCHANGE` re-runs it and
`set_keymap_from_string` re-sends the keymap.

**Measured, and it was not the expected answer.** Under Wine,
`GetKeyboardLayout` returns `0x0809` — English (United Kingdom) — so the
compositor selects `gb` unless told otherwise. That is the mechanism working; it
is also why `scripts/wc-input.sh` passes `--layout us` rather than trusting the
machine.

**What this gets right for a non-US layout, stated precisely.** Because the
mapping is positional and the keymap is the user's, a German user pressing the
key labelled `Z` sends `KEY_Y` and the `de` keymap turns it into `z`. That is
correct, and it is correct *because* the VK was not used. The same holds for
every key on any of the four.

**What it gets wrong.** The first two are **known defects and not merely gaps**,
because in both cases the compositor keeps working and produces a wrong answer
with no diagnostic anywhere. That is the property that makes them worth a
heading rather than a bullet: a missing feature is discovered by the person who
needs it, and a silent wrong answer is discovered by nobody.

> **DEFECT 1: a layout outside the four gets American letters, silently.**
>
> `layout_for_langid` falls back to `us`. A Spanish, Polish, Czech or Nordic
> user's keyboard therefore produces **plausible wrong letters** — the positions
> are right, so the keyboard feels like it is working and the characters are not
> the ones on the keys. Nothing errors, nothing is logged at a level anyone
> reads, and the client cannot tell.
>
> It is a defect rather than a design limit because the fix is three lines — a
> name in `gen-keymap.sh`'s `ALL_LAYOUTS`, an entry in `input.rs`'s `LAYOUTS`,
> a row in `layout_for_langid` — and because the *fallback itself* is the wrong
> shape: falling back to a layout that produces confidently wrong text is worse
> than refusing the keyboard capability and saying which layout was asked for.
> Neither is done. The startup line does name the layout it chose, which is the
> only thread a user has to pull.

> **DEFECT 2: there is no IME, so CJK input is impossible.**
>
> Not "degraded" and not "untested" — impossible. Composition on Windows happens
> in `WM_IME_STARTCOMPOSITION` and friends and in `WM_CHAR`, and this compositor
> handles none of them (§11.3 says why `WM_CHAR` is deliberately unused, and
> that reasoning is right for a keymap and wrong for an IME). The Wayland answer
> is `text-input-v3` plus `input-method-v2`, and neither is implemented, so
> there is nowhere for a composed string to go even if one were collected.
>
> A Japanese, Chinese or Korean user gets the romaji keystrokes and no candidate
> window: they can type ASCII and nothing else. `docs/goal-firefox.md` does not
> ask for this, which is why it is not being built — but "the goal did not ask"
> is not the same as "it works", and this is the sentence that says so.

Two more, which are ordinary gaps:

3. **AltGr is forwarded as Ctrl+AltGr.** Windows synthesises a left-Ctrl press
   before the right-Alt of an AltGr, and both are forwarded, so a client sees
   both. On `us` there is no AltGr level and it never shows; on `de` the third
   level is exactly where the interesting characters live. Untested and expected
   to be wrong. Dead keys are related and weaker: the four layouts' dead keys
   work only insofar as the *client's* xkb composes them, which for a keyboard
   with real dead keys is not the same thing.
4. **Key repeat is Windows' rate only by coincidence.** `repeat_info` says 30/s
   after 500 ms, which are Windows' defaults written down rather than read with
   `SystemParametersInfo`. A user who changed theirs is not obeyed. Windows'
   own repeats — `WM_KEYDOWN` with `lParam` bit 30 set — are dropped, which is
   right: the protocol has the client run the timer, and forwarding both would
   give it two repeat streams at different rates.

### 11.5 The Win32 side, and the three omissions that are silent

Each of these produces a compositor that looks like it works.

* **`TrackMouseEvent`, or there is no `WM_MOUSELEAVE`.** Win32 sends
  `WM_MOUSEMOVE` for free and sends *nothing at all* when the pointer leaves
  unless the window has asked — once, per departure, because the request is
  consumed when it fires. Without it a client believes the pointer is inside
  forever: hover highlights that never turn off, a cursor that never changes
  back. It is armed on the first move after an entry rather than on every move,
  which would be a syscall per pixel of travel. `wl_pointer.leave` is checked on
  the wire by the harness for this reason and not inferred.

* **`SetCapture` on the first press, `ReleaseCapture` on the last release.**
  Win32 delivers mouse messages to the window under the pointer, so a drag that
  leaves the window loses its `WM_LBUTTONUP` — it goes to whatever was dragged
  onto, or nowhere — and the client is left holding a button forever. Every
  text selection and every scrollbar drag depends on this. Counted rather than a
  flag, because two buttons can be down and releasing on the first would
  reintroduce the bug for the second.

* **`WM_SYSKEYDOWN` as well as `WM_KEYDOWN`.** Windows routes Alt, F10 and every
  Alt-combination through the `SYS` pair. A compositor handling only
  `WM_KEYDOWN` delivers **no Alt at all** — so no Alt-shortcut in any client —
  while every other key works perfectly. The `SYS` cases return 0 rather than
  falling through to `DefWindowProc`, which would open the window menu on Alt
  and swallow the next key.

Three smaller decisions, each an answer to a way of being wrong:

* **`GetMessageTime`, not a clock read.** Clients use the differences between
  input timestamps for double-click detection and gesture timing. Reading a
  clock in the window procedure timestamps events with *when the compositor got
  round to them*, which on a 5 ms poll loop is not when they happened.
* **`lParam` halves are signed.** A pointer position during a drag off the left
  edge is negative, and `lparam & 0xffff` reads -1 as 65535 — a plausible number
  no clamp catches.
* **`WM_MOUSEWHEEL`'s sign is inverted and `WM_MOUSEHWHEEL`'s is not.** Windows
  counts a notch rolled away from the user as `+WHEEL_DELTA`; Wayland's vertical
  axis is positive *downwards* and rolling away scrolls the content up. Both
  are positive to the right horizontally. `WHEEL_DELTA` is 120 and
  `wl_pointer.axis_value120` counts in hundred-and-twentieths, which is not a
  coincidence — the protocol added v120 because Windows already spoke it — so
  the raw delta *is* the v120 value once the sign is fixed. The harness asserts
  the sign on both platforms, because a backwards mouse wheel is the most
  reported and least diagnosable input bug there is.

**Focus is `WM_SETFOCUS`, not `WM_ACTIVATE`.** They arrive together for a
top-level window and they are different facts: activation says which window is
frontmost and becomes `xdg_toplevel`'s `activated` state; focus says where a
keystroke goes and becomes `wl_keyboard.enter`. The brief's item 4 is this pair,
because **the user's window manager is Windows** and a compositor deciding its
own keyboard focus would be fighting it. A `WM_KILLFOCUS` is honoured only if
the window is still the focused one, because the two messages can arrive in
either order and clearing on a stale kill would take focus from a window that
had just been given it.

### 11.6 The evidence, in three legs, and what each is worth

§8.5 split the pixel evidence in two and had to apologise for it. The same split
exists here for the same structural reason, and it is worth being explicit about
which half each leg carries.

**Leg 1 — Linux, direct.** `weston-eventdemo`, a Weston demo client, prints
every input event it receives **including the Unicode codepoint its own
libxkbcommon produced** by running our keymap over our keycode. That is a
strictly stronger statement than "an event was sent": a wrong keycode, a wrong
keymap, a wrong evdev/xkb offset or a keymap that failed to cross all produce a
different letter, and none of them produce an error. `WAYLAND_DEBUG=1` runs
beside it so libwayland decodes the same bytes independently. What this leg
cannot say anything about is the translation, because there is no `WM_KEYDOWN`.

**Leg 2 — Wine, through the proxy.** `wc-compositor.exe` in a virtual desktop,
`wc-proxy` beside it, `weston-eventdemo` against the proxy, and
`probe-input.exe` injecting with `SendInput` — so a keystroke starts as a scan
code in the desktop's input queue and ends as a character in a Linux client.
This leg carries `crates/wc-compositor/src/input.rs`, the focus messages,
`TrackMouseEvent` and `SetCapture`, and it is held to **exactly** the assertions
leg 1 is: `check_client_events` is one function called by both, so a weaker
check cannot creep into the harder case.

**Leg 3 — the terminal.** `weston-terminal` through the proxy, typed at with
`SendInput`, photographed by `xwd` from outside every process involved. A
terminal is the demonstration because §8.7 measured that it redraws **only** when
its content changes, which is what makes "the picture changed" a statement about
the keys. The harness takes two screenshots while it is idle *before* typing —
without that, a blinking cursor would satisfy the check, and every terminal has
one.

| measurement | value |
|---|---|
| pixels changed in 2 idle seconds | **0** |
| pixels changed by `echo waycompose types` | **337** |
| frames the client committed after the keys | **21** (3 → 24) |
| keymap the client received, `us` | 35,572 bytes |
| keymap after the layout change, `de` | 37,532 bytes |
| descriptors, proxy | 3 in from the client, 1 out to it |

Three checks are worth naming because each catches something nothing else does.

**`y` then `z` from the same keycode.** The layout change sends a second
`wl_keyboard.keymap`, and evdev `KEY_Y` — the position of `Y` on an American
board and of `Z` on a German one — is pressed either side of it. A second keymap
*event* would not do: a compositor re-sending the same keymap produces one, and
so does a client that ignores it. Only the character changing says a different
keymap crossed the boundary, was opened from its name, and was compiled by the
client's own library.

**Shift + `KEY_Z` decoding as `Z`.** The one place the compositor's own xkb state
is load-bearing — it tracks which modifiers are down and tells the client with
`wl_keyboard.modifiers`. A compositor that forwarded keycodes and never sent a
`modifiers` event produces a lower-case `z`, and every other assertion in the
file still passes.

**The scroll sign.** Both legs produce the same physical gesture — one notch away
from the user — and both must yield `-10` and `axis_discrete -1`.

### 11.7 What Wine got wrong on the way, and it was three things

None of them were the compositor, and all three looked exactly like the
compositor.

1. **`FindWindowW` enumerates one desktop.** `wine explorer /desktop=wc-input`
   creates a second one; a probe launched on the default desktop finds no window
   of class `WaycomposeToplevel`, injects into the wrong input queue, and reports
   a compositor that ignores input. The probe now runs inside the same desktop.
2. **A console subsystem binary takes the foreground.** Launched inside the
   desktop, the probe got a `WineConsoleClass` window, and *that* became the
   foreground window — so `SetForegroundWindow(compositor)` was refused and every
   keystroke went to the console. The probe reported
   `foreground=… class 'WineConsoleClass'`, which is the only reason this was
   half an hour rather than a day. Building it `-mwindows` means no console
   exists to steal it, and is why `probe-input.c` writes to `--log`.
   `AttachThreadInput` is needed as well: `SetForegroundWindow` is refused for a
   process that is not already foreground, and `SetFocus`/`GetFocus` address the
   *calling thread's* input queue.
3. **A killed client does not flush stdio.** `weston-eventdemo`'s own output is
   fully buffered to a file, so a run stopped with a signal contained every
   `WAYLAND_DEBUG` line (stderr, unbuffered) and **none** of the decoded
   characters — every wire assertion passing and every "what did the client make
   of it" assertion failing, for no reason in the compositor. Fixed twice over:
   `stdbuf -oL`, and stopping the compositor first so the client leaves through
   `exit()`.

### 11.8 The bug a client found and a test could not

The first version handed smithay an evdev keycode. smithay's `Keycode` is an
**XKB** keycode, which carries X11's eight-key offset, and it subtracts the
eight on the way out — so every key reached the client eight too small.

What that looks like from outside is the point. Nothing errored. Every key
produced *a* character. `weston-eventdemo` printed `key 27, unicode 93` — a
right square bracket — for a keypress the compositor believed was `h`, and the
run typed `]9aaqt` for `hello`.

**A unit test over `crates/wc-compositor/src/input.rs` could not have caught
it**, because the table it checks was correct. The bug was one addition at the
boundary between our numbering and smithay's, and only a client running the
keymap could see it. That is the argument for §11.6's leg 1 in one paragraph,
and it is why the harness asserts on decoded characters rather than on keycodes.

### 11.9 The gates

| Gate | Result |
|---|---|
| `cargo check --workspace --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo clippy --workspace --all-targets` (Linux) | clean, no warnings |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean, no warnings |
| `cargo test --workspace` | **125 tests**, 0 failures (was 116) |
| `scripts/wc-xkb.sh` (stage 2) | 5 passed, 0 failed |
| `scripts/wc-xdg.sh all` (stage 2) | 10 passed, 0 failed |
| `scripts/wc-window.sh all` (stage 3) | 37 passed, 0 failed |
| `scripts/wc-proxy.sh window` (stage 4) | 36 passed, 0 failed |
| `scripts/wc-input.sh linux` | **22 passed, 0 failed** |
| `scripts/wc-input.sh wine` | **24 passed, 0 failed** |
| `scripts/wc-input.sh term` | **6 passed, 0 failed** |

The new tests are in `crates/wc-compositor/src/input.rs`, and the one worth
naming is `each_embedded_keymap_compiles_and_maps_the_key_it_should`: it
compiles all four checked-in keymaps with the same libxkbcommon the Windows
binary links and asserts what `KEY_A` and `KEY_Z` produce in each. A truncated
file, a keymap generated for the wrong layout, or one embedded under the wrong
name all produce a compositor that starts, sends a keymap, and gives the user
the wrong characters with no error anywhere. The `fr` row carries the test — if
`de` were silently a copy of `us`, only azerty would notice.

`crates/wc-compositor/src/input.rs` carries `#![allow(dead_code)]` and the
reason is stated there: the scan-code half is called only from `win32.rs`, so a
Linux build sees it as dead. It is not — it is what the tests exercise, and **a
table that only compiles on the platform that cannot run the tests is a table
nobody checks.**

### 11.10 What is not proven

1. **Nothing here has run on the real guest**, and for input that is a sharper
   caveat than for pixels. The *transport* half of §9.11 item 3 is no longer the
   obstacle — §10.5 assembled the proxy against the guest over an `ssh -L`
   forward and `probe-relay.exe`, so the road exists. What has not been done is
   driving input down it, and §10.12 item 3 says why that is harder than it
   sounds: everything on the guest so far has run in an ssh session's
   **non-interactive window station**, which has no foreground window to inject
   into. Leg 2 needs the logged-in desktop that §10.13 item 2 asks for, and
   which stage 8 is on right now.
2. **`SendInput` under Wine is not `SendInput` on Windows**, and it is a worse
   case than most of what Wine has been asked to stand in for. Real Windows
   applies UIPI and a set of `SetForegroundWindow` restrictions that Wine does
   not, and §11.7 shows the foreground rules already differ enough to change the
   harness. A guest run may need the probe elevated, or may need `PostMessage`;
   `probe-input.c` keeps `--post` for exactly that (`WC_PROBE_POST=1`), and the
   two modes agree on the keyboard — **16 checks against 24**, both green.

   **The fallback is keyboard-only, and that was measured rather than
   assumed.** A posted `WM_MOUSEMOVE` does not move the cursor, and Win32's own
   mouse tracking follows the real one: `TrackMouseEvent` fires `WM_MOUSELEAVE`
   immediately after every posted move, and `GetCursorPos` — the only source of
   coordinates for a posted button — reports somewhere else. On the wire that
   was a `leave`/`enter` pair per motion and a click at (636, 482) of a 500x400
   window, which the client correctly ignored. A keyboard message carries
   everything it needs in its own `wParam` and `lParam`, so the probe now
   refuses the mouse in that mode rather than injecting events the compositor
   would be right to mishandle. For the fallback's purpose — telling "our
   translation is wrong" from "the injection did not arrive" — the keyboard is
   the half that matters.
3. **`WM_INPUTLANGCHANGE` has never fired.** The layout-switch path is proven by
   the headless simulation calling the same code, and by `GetKeyboardLayout`
   selecting `gb` under Wine at startup — but no run has changed the layout of a
   live Windows keyboard and watched the keymap follow.
4. **Two known defects, both silent, both in §11.4.** Restated here rather than
   only cross-referenced, because this is the list people read: **a layout
   outside us/gb/de/fr produces American letters with no diagnostic**, and
   **there is no IME at all, so CJK input is impossible**. AltGr is a third,
   ordinary gap and is expected to be wrong.
5. **Client cursors are not drawn.** `wl_pointer.set_cursor` arrives — the log
   line proves it, `weston-terminal` sets one — and the system arrow is shown
   instead. The conversion is the same shape as `set_icon`, which exists; what is
   missing is the policy for hiding the system cursor while the client's is up.
   A text field will have the wrong pointer shape.
6. **One seat, one pointer, no touch and no tablet.** `wl_touch` is a capability
   the seat could carry and nothing produces the events;
   `WM_POINTER*`/`WM_TOUCH` are not handled at all.
7. **Popups still are not shown** (§9's `new_popup` is unchanged), so no menu
   can be clicked even though the click that would open it now arrives. This is
   the next thing a usable client needs and it is a scene-graph problem rather
   than an input one.
8. **No compositor-level key bindings, deliberately.** Every key goes to the
   client. That is right when the window manager is Windows — it has already
   taken Alt-Tab and the Windows key before the message loop sees them — but it
   means there is no way to reach the compositor itself, and nothing has decided
   whether there should be.
9. **Timestamps come from two clocks that do not agree.** Input carries
   `GetMessageTime`'s tick count and frame callbacks carry §7.7 item 5's
   process-start clock. Wayland requires only that each is comparable with
   itself, which holds; a client correlating an input event with a frame
   callback would be comparing epochs. Nothing does yet.
10. **A second client's keymap costs a second file.** Every `KeymapFile` creates
    its own object and there is one per seat, so this is bounded — but the
    published directory now accumulates files whose lifetime is the compositor's,
    and nothing sweeps it if the process is killed rather than exiting.

### 11.11 What to re-check on the guest

§9.11's list is answered by §10; this is what stage 6 adds to it, and input
reaches the guest differently again. Item 1 is the one that gates the rest, and
it is not a code problem: §10.12 item 3 records that everything on the guest so
far has run in an **ssh session's non-interactive window station**, which has no
foreground window and no desktop to inject into. Stage 8 is logging in.

1. **Whether injected input reaches the compositor's window at all**, with the
   real `SendInput` and the real foreground rules. This is the item most likely
   to differ, and §11.7 is why. `probe-input.exe --post` is the fallback and its
   *agreement* with `SendInput` is itself the measurement.
2. **What `GetKeyboardLayout` returns on the real machine**, and whether it is
   one of the four shipped. Wine says `0x0809`; the guest is a different
   installation with different regional settings, and a user whose layout is not
   in the list gets American letters silently.
3. **Whether the published directory carries the keymap across the boundary.**
   The first thing to check on the guest, because a compositor whose clients die
   at connect is worse than one that shows nothing.

   **§10.3 has already answered most of this, and the answer is encouraging for
   exactly the reason it was discouraging for pixels.** Its three rows are worth
   re-reading with a keymap in mind rather than a pool:

   | §10.3's measurement | what it means for a keymap |
   |---|---|
   | two live mappings of the export disagree, 65280 wrong of 65536, both ways | a pool cannot use `SharedFile` here |
   | host writes and **closes**, guest then opens for the first time — 0 wrong of 4096 | **this is the keymap's access pattern** |
   | local NTFS, still mapped and unflushed — 0 wrong | the control, so the mismatch is the mount |

   A pool is two simultaneously-mapped views of pages both sides keep writing
   and reading. A keymap is written once and read once, and `write_section`
   **maps a view, copies, and unmaps before returning** — so nothing is mapped
   on the writing side by the time the reader opens the path. That is §10.3's
   third row, which is the row that matched.

   Two caveats keep this a re-check rather than a conclusion. **The direction is
   the opposite of the one measured**: §10.3's third row is host-writes,
   guest-reads, and a keymap is guest-writes, host-reads. And §2.5's lifetime
   rule requires the compositor to hold the *section* — and therefore the file
   handle — open for as long as the keymap exists, so the write is closed and
   the file is not; whether that counts as closed is virtiofsd's business rather
   than ours. `--cache=auto` caches on the guest, which is the side doing the
   writing here.

   **`scripts/wc-guest.sh` is already wired to answer this**, and it had to be:
   its stage-4 blocks run `weston-terminal` through the proxy, `weston-terminal`
   binds a seat, and without `--shared-dir` the run now dies at connect —
   exactly what happened to `wc-proxy.sh` the moment stage 6 landed. The
   compositor is launched with a `keymaps/` directory on the same share, so
   whoever next runs the guest gets the measurement for free. A failure there is
   a **result** rather than a regression, and the proxy names the descriptor
   kind in the error so it can be told apart from anything else. Nobody has run
   it: stage 8 has the guest.

   If it does not hold, the fix is not a bigger hammer on `SharedFile`. It is
   the arena — §10.12 item 4's conclusion for pixels, reached for the keymap by
   the same route — and **stage 7 is giving the arena a real region right now**
   (§10.13 item 1). The keymap needs no code to follow it there:
   `Handle::create_shared` is the only place that decides where a
   compositor-created object lands, which is why it is one function and not a
   parameter threaded through smithay. Stage 7 should not need to know the
   keymap exists.
4. **Whether `WM_MOUSELEAVE` behaves.** `TrackMouseEvent` is a `comctl32`-era
   API and Wine's implementation is its own; a guest run that never produces
   `wl_pointer.leave` would mean the arm is not sticking.
5. **The three ambiguous keys.** Pause, NumLock and PrintScreen are resolved by
   VK precisely because their scan codes differ between drivers, and Wine is one
   driver. The resolution should be driver-independent; that is a claim, and it
   has been tested against one.

---

## 12. Stage 7: the arena stops being a file

### 12.1 Status in one paragraph

Written 2026-08-13, after stage 6. **The check stage 5 deliberately left failing
now passes, and the substrate is the reason.** `weston-presentation-shm`,
unmodified, on the real guest, over an arena that is **guest RAM located through
QEMU's flat view**, presented **626 frames of which 60 were distinct and the
commonest appeared 11 times** — against stage 5's 634 frames, 18 distinct, one
image 453 of them, and against the Wine control on a coherent filesystem's
780/60/13. The window is live by the same measurement that said it was frozen,
and the predicate did not move. `scripts/wc-guest.sh stage4` is **42 passed,
0 failed, 0 skipped** on real Windows. Getting there cost one new crate, one new
provider on each side, and no new implementation of anything `shared_vulkan`
already had: `crates/wc-arena-host` links `sv-hostd` and does the translation
exactly once across the two projects. A pool over the real region costs
**0.4–0.7 µs to reach and 5.4–7.1 µs to touch**, against 1.9–3.2 µs and
1.9–2.6 ms for the same code over a file on the export.

Five runs of the frame check, all after the substrate change: 555/60/10,
577/60/10, 592/60/10, 606/60/11, 626/60/11. The 60 is the client's own
repertoire of images and is the control's number exactly, in every run; the
frame total varies with how much of the 20-second window the run actually got.

### 12.2 What was built, in the order the bytes move

Five pieces, and the shape is the one §9.10 item 3 promised: everything above
`Arena::open_or_create` is untouched.

| piece | where | what it does |
|---|---|---|
| `arena::attach_guest_ram` | `wc-handle`, both platforms | commits, pins and **stamps** a region; registers it as arena *n* |
| `--arena-guest-ram id:size` | `wc-compositor` | calls it before the socket exists, prints the run id |
| `wc-arena-host` | Linux, root, **not a workspace member** | `sv-hostd` finds the pages and translates; writes a plan |
| `guestram::Plan` / `StitchedRegion` | `wc-proxy` | opens the named object, one `mmap(MAP_FIXED)` per run |
| `Arena::from_plan` | `wc-proxy` | the same allocator, over the stitched view |

The direction is the opposite of every other provider in `wc_handle::arena`, and
that inversion is the whole design rather than a detail. `attach` *opens*
something somebody else made. `attach_guest_ram` **makes** the region and waits
to be found, because that is the only direction that works across a hypervisor:
the host can read and write the guest's RAM through QEMU's memory backend, and
the guest cannot name a host object at all. `shared_vulkan` states it as a rule —
"the side inside the VM allocates, because only it can produce pages the
hypervisor can locate" — and this is that rule arriving in the compositor.

**No descriptor crosses, and the guest is now further from one than it was.**
Over `SharedFile` the guest was handed a path and opened it. Over a file arena it
was handed a path once. Over guest RAM it is handed **nothing**: it says a 64-bit
run id and a page count, and the host goes and reads its memory.

The counter that used to say this — `0 out` — no longer does, and that is stage
6's doing rather than this stage's. `wl_keyboard.keymap` runs server-to-client,
so the compositor now publishes a name and `wc-proxy` opens it locally and hands
the client a descriptor **it manufactured**; every run with a seat-binding client
reports `3 fds in, 1 out`. `scripts/wc-proxy.sh` was rewritten for that when
stage 6 landed and `scripts/wc-guest.sh` was not, because nobody ran the guest
blocks in between — so three of them failed on `0 out` the first time the two
were run together, and §12.11 is what came of chasing it. The rule is unchanged
and is enforced by construction: the proxy's *upstream* writer has no fd path at
all. On the `weston-presentation-shm` run, which binds no seat, it is still
`1 fds in, 0 out`.

> **[EXPANDED BY STAGE 10 — §14.7.]** This paragraph is the whole of the
> argument for why the number stopped being zero, and stage 10 found that it is
> not enough of one: a reader who meets `1 out` in a log cannot tell from here
> whether it is fine or a regression. §14.7 states it where the number appears —
> which two processes the descriptor exists between, why that pair is not the
> boundary, and what the count would have to be for the rule to be broken.

### 12.3 The integration decision, and why it is a dependency rather than a copy

The brief allowed three ways to reuse `sv-hostd`'s translation — depend on it,
invoke its binary, or vendor it with a note — and required that exactly one
implementation exist across the two projects. It is a **dependency**, in a crate
that is deliberately **excluded from the workspace**.

Invoking the binary was the first choice and does not work as it stands:
`sv-hostd qemu` prints a report for a human, not the backend's name and the full
run list, and adding an output mode to it would mean editing the other
repository. Vendoring is a copy by definition, and a copy of the one file whose
header says a silent wrong answer is "the worst thing this codebase can
produce" is the wrong file to copy.

So `crates/wc-arena-host` has `sv-hostd`, `sv-transport`, `sv-arena` and
`arena-record` as path dependencies and does none of the work itself. The reason
it is not a workspace member is mechanical and is the same reason `vendor/smithay`
is not one: **a member is resolved by every `--workspace` command, including
`cargo clippy --workspace --target x86_64-pc-windows-gnu`**, which is the gate
this whole project exists to keep green. A member with a path dependency on a
sibling repository would make that gate fail on any machine where the sibling is
not checked out. Excluded, the coupling costs nothing until somebody builds the
locator — which can only happen on the VM host anyway.

The path is absolute (`/mnt/datasets/builds/shared-vulkan-base/...`), which is
ugly and is the honest option: a relative path cannot be correct from both
`waycompose-base/` and `wc-worktrees/<name>/`, which sit at different depths, and
`scripts/wc-worktree.sh` already hard-codes that root.

**The one thing that is duplicated is twenty bytes**, and it is duplicated
under protest and with a check. `wc_handle::arena` spells out the stamp layout —
magic at 0, run id at 8, page index at 16 — rather than depending on
`arena-record`, because depending on it would put a sibling repository in the
*Windows* build's dependency graph. Two things make that safe. A disagreement is
loud: the host accepts a page only if the magic and the run id both match and
then requires the indices to be a permutation of `0..pages`, so a wrong offset
finds zero pages or duplicates, never a wrong arena. And the two definitions are
compared, in `wc-arena-host`'s only test — the one binary in either project that
links both.

### 12.4 The measurement §10.4's table was missing

Per pool of 1 MiB, on the guest. The first four rows are §10.4's, re-measured in
this session; the fifth is new.

| transport | reach it | touch 1 MiB | release |
|---|---|---|---|
| `Section` (named, no filesystem) | 6.1 µs | 0.74 ms | 58 µs |
| `SharedFile`, local NTFS | 45–47 µs | 0.70 ms | 100–110 µs |
| `SharedFile`, the export | 539–546 µs (6966 µs cold) | 2.1–2.2 ms | 108 µs |
| `ArenaSlice`, a file on the export | 1.9–2.0 µs | 1.9–2.33 ms | 0.2 µs |
| **`ArenaSlice`, guest RAM** | **0.4–0.7 µs** | **5.4–7.1 µs** | **0.0–0.1 µs** |

**The `reach it` column was never the interesting one and this is what shows
it.** A file-backed arena already cost 2 µs to slice, so the last row improves it
by a factor of three and that is nearly meaningless. The column that moves is
`touch`, by **a factor of 270–430** — 5.4–7.1 µs against 1.9–2.33 ms for the
identical loop over the identical amount of memory. That is the difference between reading
pages that are the guest's own pinned RAM and reading pages that virtio-fs has to
fetch, and §10.4 said the two transports were "equal" in that column precisely
because they were both the same mount. They are not the same mount any more.

Two caveats, because the number is flattering. The 6 µs or so is a *steady-state*
touch: the guest stamped every page of the region when it created it, so nothing
in the measurement is a first fault. And the attach is not free — `attach_guest_ram`
commits, stamps and pins 64 MiB, measured at 746 µs–6.7 ms depending on how warm
the guest is, once per process, and the *host's* half of the attach is the
locate, which is seconds (§12.5). Both are one-off and neither is on the frame
path, which is the whole property an arena has and a shared file does not.

### 12.5 What the locate actually costs, and the two numbers that surprised us

Measured on every run, nine runs:

| | |
|---|---|
| pages located | **16384 of 16384**, every time |
| sweep | 13312 MiB in **2.37–2.42 s** |
| stamps from other runs found | 0–4 |
| stamps outside guest RAM | **0**, every time |
| coalesced runs | **1497–1517** per 64 MiB |
| stitch | **4.16–4.24 ms** for ~1510 `mmap(MAP_FIXED)` calls |
| verify (every page at its own index) | 16384 of 16384, every time |

**The first surprise is that ~1500 runs is not a problem.** `shared_vulkan`
measured 1471 for a `VirtualLock`ed 64 MiB window and treated the fragmentation
as the reason to prefer large pages; we measured 1497–1517 for the same
allocator and the stitch costs 4.2 ms. It is not on the frame path, it is 2.8 µs
per `mmap`, and `vm.max_map_count` on this host is 1048576. Large pages would
reduce it to ~22 and need `SeLockMemoryPrivilege`; on this evidence there is no
reason to ask for the privilege yet. Reproducibility is worth noting too: the
run count moved between 1497 and 1517 across nine runs of the same size, so it
is a property of the guest's free-page state and not a constant.

**The second is `0 outside guest RAM`, which is the one that would have hurt.**
`sv-hostd`'s scan skips offsets in the backing object that the flat view does not
map, and its header warns that a stamp found there is a stale one from a previous
boot. Zero of ours landed there, on every run, which says the region table read
off `info mtree -f` and the sweep agree about what this machine's RAM is. The
flat view reported **13311 MiB in 4 aliases**, which is the below-4-GiB split, the
ROM window and the SynIC pages the module header describes — this is the machine
`sv-hostd`'s fixture was written against, and it behaves as the fixture says.

The `verify` line is the one that is not optional. It re-reads every page of the
stitched view and checks that the page at arena offset *n* is the page the guest
stamped *n*. A stitcher that mapped the right pages in the wrong order would pass
every other check in this document, produce a window full of plausible pixels,
and be wrong.

### 12.6 Coherency, measured live, in both directions

§10.3 measured a virtio-fs export with both sides mapped and got **MISMATCH on
65280 of 65536 bytes, both directions** — and block 7 reproduced exactly that,
unchanged, in the same session as everything below. The same shape of experiment
over guest RAM:

| | result |
|---|---|
| host writes 32 bytes; guest holds a live mapping and polls | **seen** |
| guest answers at another offset; host holds its stitched view and polls | **seen, after 20 ms** |

The guest's number is the honest one to quote: 20 ms is one poll interval, i.e.
the write was already there the first time anybody looked. The host→guest number
the harness prints (about 3.2 s) is **not a latency** and must not be read as
one — it is dominated by the host's own 2.4 s sweep before it could write at all,
because the guest starts watching before the locate begins. What the pair
establishes is the property, not the delay: two live mappings of this region see
each other's stores, which is exactly what two live mappings of a file on that
mount do not.

This block exists separately from the frame count on purpose. If the frames ever
go stale again, this says whether the region or the pixel path is the reason, and
without it the two are indistinguishable — which is the trap §10.7 describes.

### 12.7 The frame count, which is the point of the stage

`weston-presentation-shm` redraws every frame. The distinct-frame count is
therefore a direct measurement of whether the compositor is seeing the redraws,
and it is the only check in the file that ever caught the freeze.

| run | frames | distinct | commonest |
|---|---|---|---|
| §10.7, guest, file arena on the export | 645 | 18 | **462** |
| §10.7, guest, `SharedFile` on the export | 642 | 18 | 459 |
| §10.7, Wine on the workstation, coherent fs (the control) | 780 | 60 | 13 |
| **stage 7, guest, guest-RAM arena** (five runs) | **555–626** | **60** | **10–11** |

The guest run is now indistinguishable from the control, across five runs: the
same 60 distinct images every time — which is the client's own repertoire, not
ours — and a commonest of 10 or 11 against the control's 13. The check is
unchanged. It was not relaxed, re-scoped, or moved to a note; it asserts what it
asserted in stage 5, that no single image accounts for more than a quarter of
the frames, and 11 of 626 is two orders of magnitude inside it.

What is **not** re-run is `weston-presentation-shm` over the file arena. §10.3
established that the incoherence belongs to the mount rather than to the
transport, so running it again every time would be re-measuring virtio-fs at a
cost of a minute a run. Its numbers stay in §10.7 where they were made.
`weston-terminal` over the file arena and over `SharedFile` are both still run,
because they exercise the other two transports and their three-frame lifetime
never depended on coherency.

### 12.8 Three things that cost real time, all worth writing down

**`kill 0` is not a no-op.** The stage-4 driver ends with
`kill $PROXY $FWD $RELAY $COMP`, and `PROXY` is zero when the proxy was not
started in that shell — which used to be only the error path and is now the
normal path for this transport. `kill 0` signals the whole *process group*, so
the driver killed itself immediately after printing the client's exit code, and
the symptom was a run that produced the locate report, one line of client status,
and then silence. Nothing in it looks like a signal.

**`systemd-run` gives a unit systemd's `PATH`, which on NixOS has no `bash`.**
The host-side half runs in a transient unit (see §12.9), and a
`#!/usr/bin/env bash` script in it exits 127 before its first line. The only
trace is `env: 'bash': No such file or directory` in the journal, which nothing
was reading — the harness now prints `journalctl -u <unit>` when the unit does
not finish, which is how this was found on the second attempt rather than the
fifth.

**An image-name `taskkill` is not scoped to your experiment, and this is the
paragraph to find before reaching for one.** The stage-4 driver ran
`taskkill /F /IM wc-compositor.exe` before every run — correct for one
experiment on one guest, hostile the moment two are running, because it takes
down somebody else's compositor and the symptom lands in **their** log rather
than yours. Stage 7 and stage 8 were on this guest at the same time and that is
how it was noticed. Three things changed and they are the reason the per-experiment
environment variables exist at all:

* `WC_GUESTNAME` names the guest directory (`wctest` by default,
  `wctest7` here), so one deploy cannot overwrite another's binaries mid-run —
  which would present as the other run testing code nobody wrote.
* `WC_HOST_SHARE` / `WC_GUEST_SHARE` name the share (`/srv/winshare/wc7`), so
  frames, logs, keymaps and pool files do not collide.
* The kill is gone from the driver entirely, and the one in `deploy` fires only
  when `WC_GUESTNAME` is the default — i.e. only when this deploy owns the whole
  guest. `$$`-unique socket names and `--timeout` already did its job; the cost
  of dropping it is that a still-running `.exe` makes its own `scp` fail, which
  is a message rather than a mystery.

### 12.9 The proxy needs root on the VM host, and that is a deployment constraint

This is not a footnote. It is a property the Firefox topology
(`docs/goal-firefox.md`) inherits, and it is better discovered here than in the
middle of that.

**What needs it.** `wc-arena-host` connects to QEMU's QMP monitor
(`/run/windows-vm/qmp.sock`) and reads `/proc/<qemu-pid>/fd` to find the memory
backend. `wc-proxy` opens that backend at `/proc/<qemu-pid>/fd/<n>`. On
testhost QEMU is a **system** service running as root, so all three are
root-only: the socket is `srwxr-xr-x root:root`, and `/proc/<pid>/fd` of a root
process is unreadable to anyone else.

**It is the deployment, not the design.** `sv-hostd` hands the identical name to
unprivileged ICD processes and that works in its lab because QEMU runs as the
developer there. Nothing in the arena requires privilege; what requires it is
reaching a descriptor table that belongs to root.

**What it would take to avoid, in order of preference.**

1. **Run QEMU as the user, or give its QMP socket and `/proc` entry a group.**
   The cheapest fix by a wide margin and it is a NixOS change rather than a code
   change — `windows-vm.nix` would need a `User=`/`Group=` and a socket mode.
   The obstacle is that this VM does VFIO passthrough of the iGPU, which is why
   it is a root service in the first place, so this is a real question about
   that machine rather than a switch to flip.
2. **A privileged helper that returns a *name* rather than a descriptor** — the
   pattern the rest of this project uses. It does not exist for this case, and
   the reason is worth stating because it looks like it should: **a memfd has no
   name**. `/proc/<qemu-pid>/fd/<n>` *is* the name, and it is a name whose
   openability is exactly the privilege in question. There is no second name to
   hand over, so a helper of this shape cannot be written. (`memory-backend-file`
   would have one — a path on tmpfs — and swapping the backend is another VM
   change with its own consequences.)
3. **A privileged helper that passes the descriptor over `SCM_RIGHTS`.**
   Technically fine — both processes are on the host side, so no VM boundary is
   crossed — and deliberately not done. This project's rule is stated as "no
   file descriptor and no handle crosses the boundary, ever, **even when both
   sides happen to be on one machine**", and the value of a rule with no
   exceptions is that nobody has to adjudicate the next case. Buying a
   permission bit with the only invariant the design has is a bad trade.

So: recorded, not worked around. The mechanics in the harness are that it
locates through `~/rootc` and runs the proxy and its client in a
`systemd-run --collect` transient unit rather than a long-lived root shell,
because a root call that blocks for a minute frequently returns no output at
all. The unit's journal is the record and the harness prints it on failure.

### 12.10 The region can be located exactly once, and a second attempt fails loudly

The host writes pixels over the stamps as soon as the first pool is published —
`wc-proxy`'s allocator hands out offset 0 first, and a 15 MB pool takes out the
first 3662 pages of stamps with it. So the sweep works once per run, and the
question that matters is not whether a second locate is *possible* but what it
**reports**. `sv-hostd`'s own header names the failure mode this project keeps
meeting: reading a memfd offset as a GPA "lands on valid, zeroed, unrelated
memory and reports no error whatsoever — a silent wrong answer".

Measured, rather than argued. A 16 MiB arena, held live by the guest, located
twice with one page of stamps destroyed in between by an explicit 4096-byte
write at offset 0:

| | |
|---|---|
| locate 1 | 4096/4096 pages, verify 4096/4096, plan written (391 runs) |
| overwrite page 0's stamp | one page, exactly what a published pool does to it |
| locate 2 | **`located 4095 of 4096 pages after sweeping 13312 MiB (0 pages carried the magic with a different run id, 0 were outside guest RAM)`** — non-zero exit, **no plan written** |

**It fails loudly, and the reason is structural rather than lucky.** The locator
is a *content* search, so bytes that are no longer there are not found; it takes
the expected page count from the descriptor, so "found fewer" is an error with
both numbers in it; and `wc-proxy` reads a plan file, so a locate that produced
none cannot hand it a stale one. One page short is refused, therefore 3662 pages
short is refused a fortiori.

The one way to get a *wrong* answer instead of a refusal would be stale stamps
elsewhere in guest RAM carrying the **same** run id — a previous run's arena that
had not been reused. Two things stop it: the run id is 64 bits from the system
CSPRNG per attach, and `sv-hostd` refuses a duplicate page index rather than
choosing between two claimants. The sweeps here reported 0–4 pages carrying the
magic with a *different* run id, which is exactly that mechanism working:
previous runs' stamps are still lying in the guest's free memory, seen, and
correctly ignored.

What is genuinely lost is recovery: a compositor whose host restarted has no way
to be found again. The fix is a reserved, never-allocated stamp page that the
allocator refuses to hand out, which is about ten lines and which nothing yet
needs.

### 12.11 The keymap, and whether stage 6's hand-off exists yet

§11.11 item 3 left two questions for this stage. Both now have answers, and they
are different answers.

**Does the keymap survive the export? Yes — measured, on the guest, for the
first time.** §11.11 predicted it would, on the grounds that a keymap is §10.3's
*closed-write* row rather than its live-mapping row, with two caveats: the
direction measured was host-writes/guest-reads and a keymap is the reverse, and
`--cache=auto` caches on the writing side, which for a keymap is the guest. It
holds. Every seat-binding run reports `3 fds in, 1 out` with no `cannot open`,
`weston-terminal` gets past connect, reaches `xdg_toplevel` and draws — so the
compositor wrote a keymap into `Z:\wc7\keymaps\` and `wc-proxy` opened the same
bytes as `/srv/winshare/wc7/keymaps/…` and handed the client a descriptor made
from them. `wc-guest.sh` now asserts it in both halves — the descriptor counts
and the absence of `cannot open` — so it is a standing check rather than a
one-off observation.

**Is `Handle::create_shared` the hand-off §11.11 describes? Not yet, and it is
worth being exact about the gap.** §11.11 says "stage 7 should not need to know
the keymap exists", and that is true of everything built here — nothing in this
stage went near `publish`, `SealedFile` or smithay. But the sentence it appears
in also implies that if the export had failed, moving the keymap into the arena
would have been a matter of teaching one function. It would not have been. Three
things are missing, and the first two are real work:

1. **There is no allocator on the compositor's side of an arena.**
   `wc_handle::arena`'s header is explicit that this is deliberate — "Who owns
   which bytes of an arena is the sender's problem — `sv_arena::alloc` on the
   shared_vulkan side, `wc-proxy`'s pool allocator on this one." For a pool the
   proxy is the sender and owns the free list. For a keymap the **compositor**
   is the sender, and there is nothing on that side to ask for 71249 bytes.
   Cutting from the same arena would mean two allocators handing out the same
   offsets.
2. **A compositor-side arena is read-only by contract.** `Handle::map` gives a
   `Mapping`, never a `MappingMut`, and that is what stops a compositor writing
   into a client's pixels. `attach_guest_ram` does make the compositor the owner
   of those pages, and `arena::guest_write` can write them — but it is gated to
   provided regions and documented as the coherency probe's only path, precisely
   so that it does not quietly become a second way to write buffers.
3. **Which arena, and which sub-range, is the compositor's to give away is not
   expressed anywhere.** The wire is ready — `Descriptor::ArenaSlice` is
   symmetric and the client side of `wc-proxy` already resolves one — so this is
   a question of ownership, not of encoding, and it belongs in
   `shared_vulkan`'s cold-path handshake alongside the arena id.

   > **[CLOSED BY STAGE 10 — §14, and one clause of this was false.]** All three
   > items are closed: §14.3 is the allocator and the narrow write path, §14.5 is
   > the statement of ownership. But **"the client side of `wc-proxy` already
   > resolves one" was not true.** `materialise` had a `Descriptor::ArenaSlice`
   > arm, written in stage 4, and it could never have run: it resolved the slice
   > through `wc_handle::arena::window`, and `wc-proxy` does not register its
   > region in that registry and never has — the region belongs to `pool::Arena`,
   > which holds a `StitchedRegion` of ~1500 `mmap(MAP_FIXED)` calls. The first
   > thing to send an arena slice in that direction got `arena 0 is not attached`.
   > The arm also wrote its `memfd` with `write` rather than `pwrite`, leaving the
   > client's descriptor at EOF — invisible to libwayland's keymap path, which
   > `mmap`s, and fatal to the clipboard transfer that is next in this direction.
   > This is the second time a path this document called "implemented" turned out
   > to mean "compiled" (the first is §9.10 item 5, annotated above); §14.9 names
   > the pattern.

So the honest statement is: **the hand-off is aspirational, and it did not need
to be real.** The export carried the keymap, so nothing forced the question. If a
future deployment's mount does not, the work is an allocator and a narrow
writable path on the compositor side — one function's worth of *interface*, but
not one function's worth of code.

> **[CLOSED BY STAGE 10 — §14.]** A future deployment's mount did not have to
> fail for it to be worth doing: §14.8 is the argument, which is that removing
> the keymap is removing the shared filesystem's last protocol customer, and
> §14.4 measured the mint at 1.5–1.9 µs against 77.8–82.5 µs for a file in a
> *local* directory. The estimate above was right about the shape and wrong
> about the size — it was an allocator, a narrow write path, a statement of
> ownership, **and** a read path on the proxy side that this stage assumed
> existed.

### 12.12 The gates

| Gate | Result |
|---|---|
All of these are from **master's checkout after the merge**, not from the branch
before it. Every one was re-run there rather than carried over.

One thing about running them worth knowing, because it cost a confusing minute:
the workstation lost its LAN route to the guest mid-session — `ssh: connect to
host <guest-ip>: No route to host`, 18 checks failing at once, and green
again on retry with nothing changed. Blocks 7–10 were unaffected throughout,
because they reach the guest from the VM host over the host-only link at
<guest-nat-ip>. **All-of-block-1 failing is a network symptom, not a code one**; a
real regression does not take out `SIO_AF_UNIX_GETPEERPID` and the xkb probe
together.

| Gate | Result |
|---|---|
| `cargo test --workspace` | **133 tests**, 0 failures (124 on the branch, 116 in stage 5) |
| `cargo clippy --workspace --all-targets` (Linux) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `cargo test --manifest-path crates/wc-arena-host/Cargo.toml` | 1 test, 0 failures |
| `cargo clippy --manifest-path crates/wc-arena-host/Cargo.toml --all-targets` | clean |
| `scripts/wc-proxy.sh window` (stage 4, Wine) | 36 passed, 0 failed |
| `scripts/wc-input.sh` (stage 6, Wine) | 52 passed, 0 failed |
| `scripts/wc-guest.sh run` (blocks 1–6, the guest) | 18 passed, 0 failed, 4 skipped — unchanged |
| `scripts/wc-guest.sh stage4` (blocks 7–10, the guest) | **42 passed, 0 failed, 0 skipped** |

38 of those 42 were green before the merge and four are new: the keymap name
being openable, asserted once per client (§12.11). The three that broke on the
merge and are now green are §12.2's `0 out`.

The test count moved this time, and where it moved is the interesting part.
Four of the eight are in `wc-handle`'s `arena` module and run **on Linux**, over
the Unix twin of the guest-RAM provider — `mmap(MAP_ANONYMOUS)` for
`VirtualAlloc`, `mlock` for `VirtualLock`, the same stamping loop; the other four
are `wc-proxy`'s plan parser and stitcher, which need no VM at all. The twin is
not a mock and it is also not evidence about the thing that matters: it cannot
say anything about whether the pages are findable from outside a VM, which is the
entire claim. What it does cover is the stamp layout, the registry plumbing, the
lifetime and the refusals — where the ordinary bugs live. The claim itself is
`wc-guest.sh` block 10's and needs a hypervisor to make.

### 12.13 What is not proven, updated

§10.12's list, with this stage's changes:

1. ~~The arena is still a file.~~ **Closed.** It is guest RAM, located through
   QEMU's flat view, and the freeze it caused is gone by the measurement that
   found it.
2. **The window has still never been seen.** §10.12 item 3 is unchanged and is
   still the largest single gap. Everything here ran in an ssh session's
   non-interactive window station.
3. **New: the region can be located exactly once.** §12.10 measured what a
   second attempt does — it refuses, with both numbers, and writes no plan — so
   this is a lost-recovery limitation and not a silent-wrong-answer one. A
   compositor whose host restarted cannot be found again. Fixing it is a
   reserved, never-allocated stamp page, and nothing yet needs one.
4. **New: pinning is asserted, not proven over time.** `VirtualLock` succeeded on
   every run and `shared_vulkan`'s R26 re-located an identical table 3m14s later,
   but nothing here has provoked memory pressure and no run lasted more than a
   minute. If the guest's pages moved after the locate, the symptom would be
   exactly §10.7's — a window that goes stale — and the harness would catch it,
   which is the reason the frame check stays where it is.
5. **New: 64 MiB is a guess.** The arena is sized by a flag and the allocator
   refuses when it is full. One `weston-presentation-shm` used a single 15 MB
   pool; nothing has been measured about what a real application asks for, and
   `docs/goal-firefox.md`'s client will ask for more.
6. **New: one client at a time.** Every run has had exactly one client and one
   arena. Two proxies against one compositor would need two arena ids and there
   is no protocol for allocating them; `shared_vulkan`'s cold-path handshake is
   where that belongs.

   > **[CLOSED BY STAGE 12 — §17.2, for the case that matters, and the
   > reasoning here is where the confusion started.]** Several *connections* of
   > one proxy now share one arena and one allocator, which is what Firefox
   > needs and what §15.14's box was about. Two **proxies** would still need two
   > ids and that is still unbuilt — but it is a different statement, and this
   > item's wording let the two be read as one for two stages.
7. **New: the compositor cannot put anything *into* an arena.** §12.11 item 1
   and item 2. There is no allocator on the compositor's side and its view is
   read-only by contract, so a server-created object — a keymap, a clipboard
   offer, an icon — has no route into the region even though the wire has
   carried `ArenaSlice` in that direction since stage 1. Nothing needs it while
   the export carries the keymap; something will.

   > **[CLOSED BY STAGE 10 — §14.]** It can. `arena::reserve_outbound` declares
   > a sub-range of a provided arena to be the compositor's, `arena::give` cuts
   > `Gift`s out of it, and a `Gift` is the only value in `wc_handle` carrying a
   > write path into an arena — with no function that turns a `Descriptor` into
   > one, so item 2's read-only contract for a client's buffer is unchanged
   > rather than widened (§14.3). `wl_keyboard.keymap` travels that way now, and
   > the export carries no protocol object at all.
8. **§11.10's whole list is unchanged.** Stage 7 went nowhere near input, and
   in particular items 1 and 2 there — no input has run on the real guest, and
   `SendInput` under Wine is not `SendInput` on Windows — are exactly as they
   were. What stage 7 adds to them is only that the keymap's *transport* is now
   measured on the guest (§12.11), which was §11.11 item 3.
9. **A hostile client can still fault the proxy** (§9.10 item 7), on a machine
   the compositor cannot see, and now with root.

### 12.14 What to do next, in the order the evidence puts it

> **[ITEMS 1, 4 AND 5 CLOSED BY STAGE 8 — §13.]** Item 1's first half is done
> (§13.5); the typing half is not, and is carried forward as §13.11 item 8.
> Item 4 has an answer that is not the one it expects — the call was refused
> because the process was *already* aware, and on a real desktop it succeeds
> (§13.6). Item 5 is measured and decided (§13.8). Items 2 and 3 are untouched
> and stand. §13.12 is the current list.

1. **Log in to the guest and look at a window, and type into it.** §10.13 item 2
   and §11.10 item 1 are the same errand now, and stage 8 has the guest for it.
   §8.11 items 2, 4, 5 and 6.
2. **Settle the root question on testhost** (§12.9). It is one line of
   `windows-vm.nix` if VFIO permits a non-root QEMU and a genuine investigation
   if it does not, and the answer is a constraint on the Firefox topology either
   way. Better decided now than during it.
3. **Decide what a real application's arena costs.** Item 5 above. The number to
   measure is what Firefox's `wl_shm` pools actually total, and the answer
   changes the flag rather than the design.
4. **`SetProcessDpiAwarenessContext` needs to report *why* it failed** (§10.8).
   It reported `refused` again on every run of this stage.
5. **Decide what to do about a 15 ms tick** (§10.8). Unchanged and still
   unaddressed; `Sleep(1)` measured 15.464 ms on this stage's runs.

---

## 13. Stage 8: somebody looks at the window

Written 2026-08-13, against §10.12 item 3 and §10.13 items 2, 3 and 4 — which
are §12.14 items 1, 4 and 5 by the time this was merged. Stage 5 put the
compositor on real Windows; this is the stage where a human being sees what it
produces.

**Branched from stage 4 and merged after stage 7**, the same arrangement stage 6
had. Everything measured below was measured on the pre-merge tree unless it says
otherwise, which matters in exactly one place and is dealt with there: the
frame-rate number in §13.8 was taken over a **file-backed** arena, before stage 7
put the arena in guest RAM, and §13.8 is explicit about why that does not
confound it and what was re-run after the merge to check.

### 13.1 Status in one paragraph

**`weston-terminal`, running on a Linux machine on the other side of a
hypervisor, is a window on the guest's real desktop with the system's own
caption, its own title in that caption, and a button in the Windows taskbar.**
There is a photograph of it (`docs/stage8/stage8-window.png`), a photograph of
the taskbar with its button (`stage8-taskbar.png`), and a second client in a
second window (`stage8-window-flower.png`). Two of the four clauses in
`docs/goal-firefox.md` — title correct, taskbar entry correct — are now
answered by looking, and the third clause is answered *negatively* by the same
photograph: the client's own decorations are in there too, offset by 32 pixels
and clipped at the right and bottom, because the compositor ignores
`xdg_surface.set_window_geometry`. That bug has been in the tree since stage 3,
no measurement was ever going to find it, and it is item 1 of §13.12 because
Firefox is a GTK client with exactly that shape.

The two questions this stage was also given: §10.13 item 3 turns out to have
been a **misreading** rather than a bug — the DPI call was refused only in a
window station where the question is meaningless, and Wine was right (§13.6).
§10.13 item 4 has a number, a decision and a **retraction**: the poll loop no
longer calls `Sleep`, which is worth doing for latency and was justified here on
throughput grounds that turned out to be wrong (§13.8).

§8.11 item 5 is the one thing this stage could not settle, for a reason that is
about the guest's monitor rather than about the code.

### 13.2 An ssh session is not a desktop, and nothing says so

This is the fact the whole stage rests on, and it is worth stating as plainly as
possible because every earlier stage's evidence was collected inside it.

`wc-desk station` prints the same eight lines in both places. Side by side:

| | over ssh | scheduled task |
|---|---|---|
| session | 0 | 1 |
| window station | `Service-0x0-18f160f$` | `WinSta0` |
| desktop | `Default` | `Default` |
| input desktop | `<unreachable: ERROR_INVALID_FUNCTION>` | `Default` |
| `SM_CXSCREEN` | 1024x768 | 1280x800 |
| screen capture | **fails, `ERROR_ACCESS_DENIED`** | ok |

Every one of those left-hand answers is an answer rather than an error. The
window station has a `Default` desktop with the same name as the real one. It
reports a screen size, and 1024x768 is a plausible screen. `CreateWindowExW`
succeeds on it, `ShowWindow` succeeds, `SetDIBitsToDevice` succeeds, and
`GetLastError` is 0 throughout. **The only call in the set that fails is the one
that tries to read pixels back**, and nothing in stages 1–5 tried to.

So §10.12 item 3 was not "we did not get round to looking". It was that the
harness could not have looked, and could not have reported that it could not.

### 13.3 Getting onto `WinSta0`, and the three routes that were not available

A process cannot walk into another session's window station; something already
inside that session has to start it. Four ways were considered:

* **RDP.** The guest is Windows 11 **Home**, which has no RDP host. It also has
  no `qwinsta.exe` and no `tscon.exe` — both are simply absent from `System32`,
  which is a faster way to find out than reading the edition table.
* **A service, `WTSQueryUserToken`, `CreateProcessAsUser`.** The textbook route,
  and it works. It also means installing a service on a machine whose job is
  something else, to run a test binary.
* **`psexec -i 1`.** Not installed, and it is the same mechanism with somebody
  else's binary in front of it.
* **The task scheduler.** A task whose principal has
  `-LogonType Interactive` runs in the session its user is already logged on in,
  on that session's real window station, with no password and no service. This
  is what `scripts/wc-desktop.sh` uses, and it is eleven lines of PowerShell.

Two things about it cost a round each:

* **`$env:USERDOMAIN` is `WORKGROUP` under sshd**, not the machine name. So
  `-UserId "$env:USERDOMAIN\$env:USERNAME"` fails with *"No mapping between
  account names and security IDs"*, which reads as a broken account and is a
  wrong environment variable. Asking the token —
  `[Security.Principal.WindowsIdentity]::GetCurrent().Name` — is correct and is
  what the generated `runint.ps1` does.
* **A task's process does not die with the ssh session that started it.** That
  is the exact opposite of `wc-guest.sh`'s first operational lesson, and it is
  what makes the compositor startable at all here — but it also means a
  compositor left behind survives a torn-down connection, so `wc-desktop.sh`
  kills only its own, matched on the socket name in the command line. Another
  worktree is entitled to be driving the same guest at the same time, and
  `wc-guest.sh`'s blanket `taskkill /IM wc-compositor.exe` would reap it.

### 13.4 The desktop is locked, and it did not matter

The guest's session 1 is logged on: `explorer.exe` is running, `Shell_TrayWnd`
is visible at 0,752–1280,800. On top of all of it is a full-screen lock-screen
window, with Windows Hello politely reporting *"Ich versuche, Sie zu
erkennen…"* to an empty room. There is no password for this machine here, and
none was looked for; the registry has `AutoAdminLogon = 0` and no
`DefaultPassword`, which was checked once and then left alone.

#### `WTSQuerySessionInformation(WTSSessionInfoEx)` says the session is unlocked. It is not.

This one gets its own heading, because it is the API the next person will reach
for to decide whether a desktop is available, and it will tell them yes.

Measured on this guest: `WTSSessionInfoEx` returns `SessionFlags = 1`, which is
`WTS_SESSIONSTATE_UNLOCK`, for a session that is at that moment displaying a
Windows Hello credential prompt over a lock screen. Not a stale value and not a
race — it was read repeatedly, while a photograph of the same session showed the
lock screen with a clock on it.

**What to use instead:** the foreground window. `GetForegroundWindow` plus
`GetWindowTextW` returned `"Windows-Standardsperrbildschirm"` — Windows' own
name for the default lock screen — and after a synthetic keystroke it advanced
to the Hello prompt rather than to the desktop, which is what actually settled
it. `LogonUI.exe` being in the process list is a second, weaker signal.

None of that stops any of the work below. It changes only what a program should
believe when it asks the question, and this project has now been lied to about
DPI by Wine (§13.6), about screen size by a service window station (§13.2) and
about lock state by `wtsapi32` — all three in the shape of a plausible answer
rather than an error.

None of that turned out to matter, because a screen grab was the wrong
instrument anyway. `PrintWindow(hwnd, dc, PW_RENDERFULLCONTENT)` does not read
the framebuffer: it reads the window's DWM redirection surface, which exists and
is current for every composited window regardless of what is on top of it. So
the pictures below are of the real windows, drawn by the real theme at the real
DPI, on the real desktop — and one of them is of `Shell_TrayWnd` itself, which
is how the taskbar claim is made without anything being visible.

What that instrument cannot show is z-order, or whether the panel is lit. No
claim here rests on either. `docs/stage8/stage8-screen.png` is the whole screen,
kept precisely because it shows the lock screen: it is the control that says the
window captures are *not* screen grabs.

### 13.5 §8.11's list, answered

**Item 1 — a window appears outside a virtual desktop.** Yes, and now for the
right reason. §10.12 closed this on the guest, but in the blind window station;
it is now `WaycomposeToplevel` on `WinSta0\Default` alongside `explorer.exe`.

**Item 2 — the caption is Windows'.** Yes.
`docs/stage8/stage8-window.png` is a `WS_OVERLAPPEDWINDOW` with a Windows 11
caption bar: system font, the system's minimise / maximise / close glyphs at the
right, an 8-pixel border and a 39-pixel caption (`nonclient 16x39` in the
compositor's own log). Nothing in this project drew any of it, which is the
claim. It carries the client's title —
`"Wayland Terminal"` — and the caption *tracks* it: `weston-presentation-shm`
rewrites its own title on every frame with the delay it just measured, and the
window in the enumeration reads
`"presentation-shm: feedback [Delay 0 msecs]"`. So `set_title` →
`SetWindowTextW` is a live path and not a one-shot at map, which is what
`docs/goal-firefox.md` needs of it (Firefox retitles on every tab switch).

**Item 3 — `SetDIBitsToDevice` with a negative `biHeight`.** Not upside down,
and the evidence is a number rather than a glance. §8.4 says the project needs
both halves of the pixel path to see a blit bug at all: `--dump-frames` writes
the compositor's *last view of the client's pixels*, before GDI, and the
`PrintWindow` capture is what came out the other end. Crop the window's client
area out of the capture and compare it against the top-left crop of that dump,
and against the same crop of the **flipped** dump:

```
RMSE upright  778.8  (1.2%)
RMSE flipped 5852.7  (8.9%)
```

Upright by a factor of 7.5, and the residual 1.2% is a blinking cursor between
the two instants. `wc-desktop.sh` makes this a check rather than an anecdote,
including the negative case: if the flipped crop ever wins by the same margin it
prints "the window is UPSIDE DOWN", and if neither wins it says the client moved
and declines to decide.

The same comparison establishes something the orientation question does not,
and it is what §13.7 is about: the client area is the buffer's **top-left crop**.
Not a scaled version of it, not a sub-rectangle of it — the top-left corner.

**Item 4 — the modal resize loop.** It works, and this is the first thing that
has ever been inside it. `wc-desk drag` posts `WM_SYSCOMMAND`/`SC_SIZE` — the
same entry point the system menu's *Size* uses — and then arrow keys, all
through the window's own message queue, so this is the real modal loop entered
the real way rather than a simulation. From inside it:

* the window really resizes (266x289 → 530x498 for `weston-presentation-shm`,
  902x530 → 1166x794 for `weston-terminal`);
* the UI thread still answers a `SendMessageTimeout(WM_NULL, SMTO_ABORTIFHUNG)`
  while it is in there;
* the compositor sends a real `xdg_toplevel.configure` for the new size, and the
  client reallocates against it;
* **and it keeps presenting frames — 566 of them.** Sampled every three seconds
  across the hold, the `--dump-frames` count goes 809 → 910, 1003, 1091, 1271,
  1375 while the window is inside the loop. Those files are written from
  `present`, on the UI thread, so the count is literally the number of times that
  thread woke up and repainted.

That last one is §8.2's `PostMessage`-to-a-message-only-window decision being
right, measured rather than reasoned, three stages after it was made.

It took three clients to get there, and the reason is worth keeping. The first
attempt used `weston-terminal`, took **one** sample four seconds in, saw no new
frames and passed anyway on a before/after comparison — it could not distinguish
"the wake was discarded" from "an idle terminal had nothing to draw".
`weston-flower` was the obvious fix and is not one: it draws its flower once and
stops, which is why it produced four commits in two minutes. Only
`weston-presentation-shm` commits continuously. **A measurement that cannot tell
its failure from its null result is not a measurement**, and the harness now
takes a series across the hold rather than a pair around it.

**The honest caveat, and it is a real one.** The three runs above are what the
numbers come from, and each is attributable: the compositor's own log records
`configure 1150x755`, `464x464` and `514x459` for windows it had mapped at
886x491, 200x200 and 250x250, so it was *this* compositor's window that Windows
resized in each case. But `wc-desktop.sh` cannot reproduce the loop on demand.
Six later runs posted the same `WM_SYSCOMMAND`/`SC_SIZE` to the same window,
confirmed the thread was still answering, and came back with the window exactly
the size it started — and the harness now reports that as a **skip with its
reason** rather than a pass or a failure, because it is neither. What changed
between the working runs and the rest is not known; the leading suspicion is
that `DefWindowProc` declines to start a sizing loop for a window that is not
the active one, and on this desktop the active window is the lock screen.

So the modal loop is *observed to work* and *not reliably drivable from here*.
The three failed attempts to build a probe for it are in `cmd_drag`'s comment,
because each of them was a plausible idea that a subsequent run showed to be
measuring the wrong thing — and the last of them, "post `SC_SIZE`, tap one arrow
key, did the frame move", reported "the loop would not start" about loops that
had started, because Windows' keyboard sizing does not move the frame until
Return and spends the first key choosing an edge.

**Item 5 — a scaled display.** **Not answered, and it cannot be on this
machine.** The guest's desktop is 1280x800 at 96 dpi — a per-monitor-aware
process and a DPI-unaware one report the same numbers, which is the definition
of not being scaled. (The adapter's *current mode* is 1920x1200 at 60 Hz, so
something between Windows and the panel is scaling; whatever it is, it is not
Windows' DPI scale factor, and there is nothing for the compositor to be
virtualised by.) What did change is that the mechanism is now known to work —
see §13.6 — and that the compositor prints the DPI of every window it maps, so
the first scaled display it meets will say so in its own log rather than needing
this investigation again.

**Item 6 — `WM_SETICON` in the real taskbar.** The window gets a taskbar button;
the icon on it is Windows' generic application icon, because **nothing calls
`WM_SETICON`**. `platform/mod.rs` says as much about `IconImage` — "nothing
sends this yet" — and `docs/stage8/stage8-taskbar.png` is what that sentence
looks like on a taskbar. Pinned three buttons to the left of it, for whatever it
is worth, is Firefox.

**Item 7 — the system frame and the client's own decorations colliding.**
Confirmed, and worse than predicted. See §13.7.

### 13.6 `SetProcessDpiAwarenessContext`: a misreading, and Wine did not lie

§10.13 item 3 asked for the failure to report *why*, "because 'refused' is not
enough to tell an already-aware process from an unavailable context, and one of
those is fine and the other is a bug". `announce_dpi_awareness` now prints
`GetLastError` and the process awareness either side of the call. The answer is
the fine one, and §10.8's inference from it was wrong:

```
over ssh:  -> refused: awareness was already set (ERROR_ACCESS_DENIED)
           awareness per-monitor-aware -> per-monitor-aware
on WinSta0: -> ok (err 0)
           awareness unaware -> per-monitor-aware
```

A process in the ssh session is **already per-monitor-aware before `main` runs**
— session 0's window station has no display to virtualise against, so Windows
does not bother — and `SetProcessDpiAwarenessContext` documents
`ERROR_ACCESS_DENIED` for "the default API awareness mode for the process has
already been set". So the call was refused because the answer was already the
one being asked for. The same binary, on the real desktop, succeeds.

Two consequences, one of them uncomfortable.

**§10.8's conclusion has to be withdrawn.** It said "the mechanism it depends on
does not work as written, and a scaled display would virtualise the window".
Neither clause is true. The mechanism works; what did not work was the *place*
the measurement was taken, which is the same discovery as §13.2 arriving from a
different direction.

**Wine was not wrong the fifth time.** Wine reported `ok`; the real interactive
desktop also reports `ok`. The disagreement was between Wine and a window
station in which the question is meaningless. The count of things Wine has
actually lied about goes back to four, and the lesson is not "trust Wine less"
— it is that a negative result from the guest is only evidence if the guest was
asked in the place the code will really run.

`wc-desk` makes the same call as the compositor, deliberately, from a
differently-named executable. That is a control rather than duplication: if the
refusal had been an app-compat shim keyed on an image name or a manifest, the
two binaries would have disagreed. They do not.

### 13.7 What the picture actually shows, which is a bug

`docs/stage8/stage8-window.png` is the deliverable of this stage and the most
useful thing in it is wrong.

The compositor configured `weston-terminal` at 886x491. The client committed a
buffer of **946x555**, whose ink — its own client-side decorations — sits at
+32,+83 and stops 32 pixels short of the bottom. That is not the client being
disobedient: with client-side decorations, `xdg_toplevel.configure` names the
*window geometry*, and the buffer is larger by the shadow and frame margins the
client declares with `xdg_surface.set_window_geometry`. The geometry rectangle
here is 882x491 at +32,+32 — and 491 is exactly the height that was configured,
so the client obeyed precisely.

**The compositor does not implement `set_window_geometry` at all.** `grep` finds
no reference to it in `wc-compositor` or `wc-proxy`, and §13.5 item 3's
comparison shows the consequence directly: the client area is the buffer's
top-left crop. So it blits the buffer from (0,0) into a client area sized for the
geometry rectangle, and what lands on screen is:

* 32 pixels of the client's shadow margin along the top and left, as a border
  that is not a border;
* the client's own title bar, immediately under Windows' title bar — §8.11 item
  7's "two title bars", arriving exactly as predicted;
* and the right-hand and bottom 60x64 of the surface, including the other two
  sides of the client's frame, **clipped off**, which is why the outline in the
  screenshot has a left edge and no right one.

Nothing in stages 3, 4 or 5 could have caught this. Every check on the pixel
path compares the shadow against the client's buffer, and the shadow is a
faithful copy of the buffer — the copy is not where the mistake is. The mistake
is in which sub-rectangle of a correct buffer is the window, and the only
instrument that shows it is a picture of the window.

It is also the concrete form of the tension `docs/goal-firefox.md` describes.
The third clause of the goal — *Firefox painting its UI into the titlebar* —
needs the compositor to negotiate decorations with the client and then extend
the frame with `DwmExtendFrameIntoClientArea`. `FrameStyle::extend_top` exists
for that and is still 0. What this screenshot adds is that the *first* step is
not the DWM call: it is honouring the geometry rectangle, because until the
compositor knows where the client thinks its window is, it cannot decide what to
extend.

### 13.8 The 15 ms tick, and the answer that is not `timeBeginPeriod`

§10.13 item 4. Forty samples each, on the interactive desktop
(`docs/stage8/tick.txt`):

| wait | mean | median | max |
|---|---|---|---|
| `Sleep(0)` | 0.000 ms | 0.000 ms | 0.003 ms |
| `Sleep(1)` | 15.813 ms | 15.781 ms | 18.300 ms |
| `Sleep(5)` | 15.882 ms | 15.840 ms | 18.467 ms |
| `Sleep(16)` | 31.299 ms | 31.682 ms | 39.615 ms |
| high-resolution timer, 1 ms | 1.595 ms | 1.591 ms | 2.652 ms |
| high-resolution timer, 5 ms | 5.141 ms | 5.094 ms | 5.866 ms |

`Sleep(0)` is the control and it costs nothing, so the harness is measuring the
scheduler and not itself. `QueryPerformanceCounter` is 10 MHz, as §10.8 found.

The three rows that matter:

* **`Sleep(1)` and `Sleep(5)` are the same number.** It is a quantum, not an
  overhead: anything below one tick is rounded up to one tick, so the poll
  loop's 5 ms sleep has always been a 15.9 ms sleep and asking for 1 ms would
  not have been faster.
* **`Sleep(16)` is 31.3 ms.** One frame time at 60 Hz costs two ticks, so any
  cadence built on `Sleep` and aimed at a display refresh gets half of it.
* **A high-resolution waitable timer is accurate to within 0.6 ms at 1 ms and
  0.15 ms at 5 ms.** `CreateWaitableTimerExW` with
  `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` (Windows 10 1803) is the documented
  way to wait for less than a tick, and unlike `timeBeginPeriod` it is scoped to
  the wait rather than raising the *global* timer resolution — it costs this
  process, not every process on the machine and its idle power. The user's
  ruling against `timeBeginPeriod` therefore costs nothing.

**The decision, taken: the poll loop no longer calls `Sleep`.** `tick.rs`
creates one high-resolution waitable timer per thread and waits on it, falling
back to `Sleep` when the flag is refused — which is what a Windows older than
1803 does, and is the old behaviour rather than a failure. The compositor prints
which one it got at startup (`poll interval 5ms using high-resolution waitable
timer`), because a run that silently fell back and one that did not differ by
11 ms a turn and there is otherwise no way to tell them apart afterwards.

#### The claim this section made first, and why it was wrong

Kept, because the mistake is more useful than the correction.

The first version of this section said the poll interval was costing frame rate,
and had a number for it: counted from `--dump-frames`, the compositor presented
**465 frames in 12 seconds — 38.8 per second**, in three-second buckets of 93,
88, 180 and 104. The explanation was a beat. `State::tick_frames` answers frame
callbacks at most once per 16 ms; it is called once per turn; a turn was 15.9 ms;
so on most turns `elapsed` is *just under* the gate, the callbacks are not
answered, and the client waits another whole turn — 31.8 ms, or 31 frames a
second, with the occasional turn landing on time and producing the 180 bucket.
It fits the buckets almost too well.

**Then the timer landed, and the frame rate did not move.** Same client, same
transport, same 12-second window, with the compositor's own startup line
confirming the high-resolution timer was live: **34.6 frames a second**, against
38.8 with `Sleep`. Inside the run-to-run noise. If the turn length had been the
constraint, a turn going from 15.9 ms to 5.1 ms would have moved it and it did
not.

The beat was a plausible story with an arithmetic that worked out, and it was
not a finding.

**What was actually being measured was the instrument.** `--dump-frames` writes
a PPM per present, and in these runs that directory is on the **virtiofs
export**: 187 KiB per frame at 250x250, several megabytes a second through a
paravirtual filesystem, on the write path of the very thread whose cadence was in
question. The measurement had the observer inside the loop. That is the same
shape as §13.9's mistakes one layer down, and the tell was available all along —
`Sleep(16)` predicts a 32 Hz ceiling and the observed rate was 38.8, *above* it,
which should have been the end of the theory rather than a rounding error.

**The replacement is a control rather than a better number.**
`wc-compositor --poll-coarse` forces the `Sleep` path back on, so the two
cadences can be compared in one binary with one flag, on one tree, over one
transport, with nothing dumped: `scripts/wc-desktop.sh fps` runs the same client
for the same wall time twice and reads the compositor's own closing counter,
`(N frames presented)`, which costs an increment. That is the shape this
question needed from the start.

It has run, and it puts a number on the wait at last.
`weston-presentation-shm`, 45 seconds each, nothing dumped
(`docs/stage8/fps.txt`):

| poll wait | frames presented | per second | the proxy's view |
|---|---|---|---|
| high-resolution timer | 1464 | 32.5 | 1465 commits, 1464 copies |
| `--poll-coarse` (`Sleep`) | 1415 | 31.4 | 1416 commits, 1415 copies |

**1.03x.** The wait is worth three per cent, not two-fold, and the withdrawn
claim was not merely unproven but wrong by a factor of twenty.

The same table says why, and that is the more useful half: **the compositor
presented every commit it was given**, in both configurations — 1464 of 1465 and
1415 of 1416, one frame behind at the cut, nothing dropped or coalesced anywhere
in the loop. So the compositor is not the throttle at either cadence. The
*client* committed 1465 times, and a Wayland client waiting on a frame callback
commits exactly as often as one comes back. What paces this run is the round trip
that carries the callback out and the commit back — compositor → `afunix.sys` →
`probe-relay.exe` → TCP → two `ssh` forwards → proxy → client, and all the way
back. About 30 ms of it, against a 5 ms turn at one end, which is why moving that
end by 10 ms moves the total by 3%.

What can be said, and what cannot:

* **Certain:** a turn of the poll loop cost 15.9 ms and now costs 5.1 ms. That
  is `docs/stage8/tick.txt`, measured directly, and it is a *latency* fact.
  Every UI event crosses this loop — §8.2's stated cost, "UI events reach the
  client on the next turn" — so a close, a resize configure and a keystroke were
  all up to 15.9 ms late and are now up to 5.1 ms late. Nothing about that
  depends on the frame-rate question, and it is the whole justification the
  change needs.
* **Measured, and small:** the wait is worth 3% of throughput. It was claimed
  here as a halving; that claim is withdrawn and this is the number.
* **Ruled out:** the compositor. It presents every commit it receives, 1464 of
  1465, so nothing between `dispatch_clients` and `SetDIBitsToDevice` loses
  frames at either cadence.
* **Ruled out:** the copy. The proxy reports 126 µs per copy — three parts in a
  thousand of the wall clock at this rate.
* **Still open:** the **round trip**, which is what is left, and which is mostly
  a property of *this test rig* rather than of the design: the real topology has
  no `ssh -L` in it. Worth establishing before anything is optimised.
  §13.12 item 3.

### 13.9 Two worktrees, one guest, and a window that was not ours

An operational lesson that cost three runs and is going to recur, because the
project now has eight worktrees and one Windows machine.

Another worktree was driving the same guest at the same time. Its compositor
registers the same `WaycomposeToplevel` window class, runs the same
`weston-terminal`, and produces a window of very nearly the same size with
exactly the same caption. `wc-desk shot --class WaycomposeToplevel` therefore
photographed **its** window about as often as ours — and `wc-desk drag` put
*its* window into a modal resize loop.

The symptoms were not "wrong window". They were: an orientation check that
compared a capture against our own frame dumps and got a large error in both
directions, reported as "undecided"; and a resize check that intermittently did
nothing. Both read as flakiness, and both were chased as flakiness first.

Three rules came out of it, and they are in the code rather than here:

* **Address windows by process id, never by class.** `wc-desk` takes `--pid`,
  `wc-desktop.sh` learns it from the compositor's own command line, and `--class`
  is a filter within that rather than a selector.
* **Kill by socket name, not by image name.** `wc-desktop.sh` matches the socket
  in the command line, so it can only ever reap its own.
* **`wc-guest.sh`'s blanket kill is narrowed to an image *path*.** It used to do
  `taskkill /IM wc-compositor.exe`; stage 7 guarded that behind "only when
  `WC_GUESTNAME` is unset", which is the wrong way round — the default name is
  exactly what a run that has not thought about sharing the guest uses, so the
  blanket kill fired in precisely the case where two runs collide. It now kills
  only processes whose `Win32_Process.ExecutablePath` starts with this deploy's
  own directory, matched with a trailing separator so that `wctest` cannot match
  `wctest7`. An image name is not an identity on this guest; the path is,
  because every experiment deploys its own copy into its own directory.

There is a fourth thing that is not a rule but is worth saying: **a wipe and a
kill have an order.** `do_window` emptied the frame-dump directory and *then*
killed leftover compositors, so a compositor still alive from an interrupted run
kept dumping into the directory that had just been emptied, and the next run
counted its frames and read its log. The symptom was "the capture does not match
the dump", which is indistinguishable from a blit bug, and it was chased as one
twice.

### 13.10 The gates

Run from master's checkout after the merge. The two `window` rows are the
per-check results seen on the merged tree; their run was cut short at the
teardown by the unredirected `ssh -N -L` described below, after every check had
reported, and the summary line is arithmetic over those.

| Gate | Result |
|---|---|
| `cargo test --workspace` | 135 tests, 0 failures (133 on master, +2 for `tick.rs`) |
| `cargo clippy --workspace --all-targets` (Linux) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `cargo build -p wc-desk --target x86_64-pc-windows-gnu` | builds alone |
| `scripts/wc-desktop.sh probe` | 8 passed, 0 failed |
| `scripts/wc-desktop.sh window` (`weston-terminal`) | 8 passed, 0 failed, 2 skipped |
| `scripts/wc-desktop.sh window` (`weston-presentation-shm`) | 7 passed, 0 failed, 3 skipped |
| `scripts/wc-desktop.sh fps` | 1 passed, 0 failed — 1464 frames against 1415 |

The skips are the modal-loop block and, for the animating client, the
orientation check — `weston-presentation-shm` draws a uniformly black surface at
the moment of capture, so its flipped and unflipped crops are byte-identical and
the check correctly declines to decide rather than passing on a coincidence.
Both are the harness saying what it could not establish this run, which is the
behaviour §13.5 item 4 argues for.

**`fps` took three attempts and every failure printed as something else**, which
is the pattern this whole stage keeps running into.

* An `ssh -N -L` backgrounded on the far side with its stdout still attached to
  the outer `ssh` **holds that connection open forever**. The run hung, and it
  was still hanging when the guest hibernated underneath it, so it looked like
  the guest being the problem. The same line exists in `do_window`'s driver and
  has been fixed there too — it is why that block's teardown hung once as well.
* Two back-to-back runs on **one TCP port** left the second with a listener in
  `TIME_WAIT`, so its client never connected and it reported `0 commits` —
  which looked exactly like `--poll-coarse` breaking the compositor.
* Introducing a per-mode `$port` and reusing the name in a heredoc that
  `do_window` also expands made `$port` unbound *there*, and `set -u` killed the
  script one line after it had said which compositor it found.

None of the three was what it looked like, and all three are one line.

The test count moved by two, and only two, for the same reason as §10.11:
everything else added here is a Windows-only binary or a harness, and its
evidence is a photograph rather than an assertion. The two are `tick.rs`'s, and
they are the ones that can run on the machine that runs `cargo test` — that a
5 ms wait waits, and that it does not cost a scheduler tick. The photographs are
checked in under `docs/stage8/`, and `scripts/wc-desktop.sh` reproduces them.

Two things after the merge that were not true before it, both from
`wc-desktop.sh` being written against a stage-4 tree:

* **Stage 6's keymap.** The compositor advertises a `wl_seat` now, so
  `weston-terminal` binds one and is sent a `wl_keyboard.keymap`. Without
  `--shared-dir`/`--shared-peer-prefix` that keymap is a `Local\wc-…` section no
  Linux process can open and the proxy ends the connection — so exactly one of
  this script's two clients would have started failing and the other would not.
  Added. This is the same trap stage 7 hit in `wc-guest.sh` when stage 6 landed
  under it (§12.11), arriving here one merge later.
* **Stage 7's arena.** `wc-desktop.sh` still uses the *file-backed* arena, and
  that is now known to be the frozen one: a run of it reports **1749 frames
  presented, 18 distinct**, which is §12.7's problem intact. It does not affect
  any claim here — every window, caption, taskbar and DPI fact is about one
  frame, and the frame-count claims are explicitly about the poll loop's cadence
  rather than the pixels' freshness — and the harness now prints both numbers
  side by side so that the two can never again be confused for each other.

### 13.11 What is not proven, updated

§10.12's list, with this stage's changes:

1. ~~The window has never been seen.~~ **Closed.** §10.12 item 3 is what this
   stage was for.
2. **New, and the largest single thing: `xdg_surface.set_window_geometry` is
   not implemented** (§13.7). Every client with client-side decorations — which
   is every GTK client, which is Firefox — is being drawn offset by its shadow
   margin and clipped by it. It was invisible to every check that exists.
3. **The modal sizing loop cannot be entered on demand** (§13.5 item 4). It has
   been observed to work three times, each attributable to this compositor by its
   own log; the harness cannot make it happen when asked and now skips with its
   reason rather than guessing. The suspicion is that `DefWindowProc` will not
   start a sizing loop for a window that is not the active one, and on this
   desktop the active window is the lock screen — which would make it a property
   of the locked session rather than of the compositor, and would go away with a
   credential.
4. **A scaled display is still unmeasured** (§8.11 item 5). Not because it was
   skipped: this guest's desktop is 96 dpi and there is nothing to measure. The
   call now demonstrably works and the DPI of every mapped window is logged, so
   the next machine answers it for free.
5. **`WM_SETICON` is still never called**, so the taskbar icon is the system
   default. `docs/goal-firefox.md`'s first clause is half-done: the button is
   right, the picture on it is not, and the interesting half of that problem —
   resolving an app-id to icon pixels on the *Linux* side and shipping them —
   has not been started.
6. **What holds an animating client to ~32 frames a second is not known**
   (§13.8). Three things are now ruled out by measurement rather than by
   argument: the poll loop's wait (worth 3%, measured against its own control),
   the compositor (it presents 1464 of 1465 commits), and the copy (126 µs).
   What is left is the round trip out to the client and back, most of which is
   this test rig's `ssh -L` rather than the design's.
7. ~~The arena is still a file.~~ **Closed by stage 7** (§12), which landed
   while this stage was being built. `wc-desktop.sh` itself still uses the file
   arena and therefore still sees the freeze — 1749 presented, 18 distinct — but
   that is this harness being behind rather than the project being.
8. **`SealedFile`, `with_data_mut`, `KeyboardHandle::new`** — unchanged from
   §10.12 item 5.
9. **Input reaches the client, but not from this stage.** Stage 6 built the
   seat (§11); the keystrokes synthesised *here* went to Windows, to drive the
   modal loop, and none of them was forwarded to a Wayland client. Nothing has
   yet typed into a window on the interactive desktop, which is the half of
   §12.14 item 1 this stage did not do.
10. **Nothing has been watched by a human on the glass.** The captures are DWM
   redirection surfaces, which is the right instrument and not the same
   instrument. Whether the panel shows what DWM holds is untested, and the way
   to test it is to unlock the machine.

### 13.12 What to do next, in the order the evidence puts it

**1. ~~Implement `xdg_surface.set_window_geometry`. Nothing else on this list is
close.~~**

> **[CLOSED BY STAGE 9 — §15.6, and the prediction below was exact.]**
> It hit Firefox immediately and in the shape written here. The implementation
> landed one layer above where this item points: `FrameStyle::content_origin`
> is still zero and still purely the titlebar-extension parameter, because once
> the window shows a composited **tree** rather than one buffer (§15.5), the
> geometry origin has to be subtracted while the tree is assembled or every
> subsurface inherits the offset. The rest of this item is unchanged and is the
> reason the fix was not argued about.
>
> Two assertions in `scripts/wc-proxy.sh` had been encoding the bug — the window
> expected at 806x491, which is `weston-terminal`'s *buffer* — and are now
> 742x427, the geometry rectangle. A stage that finds them failing should
> suspect a regression here before it changes them.

It is not implemented anywhere — `grep -r set_window_geometry crates/` finds
nothing in `wc-compositor` or `wc-proxy` — and the consequence is that **every
client with client-side decorations is drawn wrong**. Not subtly: offset by its
whole shadow margin, and clipped by the same amount on the opposite two sides.

The numbers, from `weston-terminal` on the guest (§13.7):

| | |
|---|---|
| what the compositor configured | 886x491 |
| the buffer the client committed | **946x555** |
| the client's window-geometry rectangle | **882x491 at +32,+32** |
| what the compositor blitted | the buffer's top-left 886x491 |
| measured, capture against dump | RMSE **779** upright, **5853** flipped |

So the geometry *height* the client chose, 491, is exactly the height that was
configured — the client obeyed precisely and the compositor then read the wrong
rectangle out of the reply. What lands on screen is 32 px of the client's drop
shadow along the top and left, the client's own title bar immediately under
Windows', and the right-hand and bottom 60x64 of the surface missing.

**This will hit Firefox immediately.** Firefox is a GTK client, GTK uses
client-side decorations with a shadow margin of exactly this shape, and stage 9
is pointing it at this compositor now. The failure will look like a window
that is offset and clipped, and it will look like a blit bug, and it is not one:
the blit is correct and reads the wrong rectangle. The fix is in the commit
path, where `xdg_surface`'s pending geometry has to become both the size the
window is asked to be and the origin `Shadow::blit` copies from —
`FrameStyle::content_origin` already exists and is the place it belongs.

2. **Send a `WM_SETICON`** (§13.5 item 6), which means deciding where icon
   pixels come from: `xdg-toplevel-icon-v1` if Firefox implements it, otherwise
   an app-id lookup on the Linux side and the pixels shipped across.
   `docs/goal-firefox.md`'s first clause is otherwise done.
3. **Find out whether the frame rate is the rig's or the design's** (§13.8). The
   poll loop's wait, the compositor and the copy are all measured and all ruled
   out; what is left is a round trip that runs through `probe-relay.exe`, TCP and
   two `ssh` forwards, none of which exist in the topology
   `docs/goal-firefox.md` describes. Measure that before optimising anything, or
   the optimisation will be of the harness.
4. **Unlock the guest, or decide not to.** Three of this stage's remaining gaps
   — the modal loop that will not start, whether the panel shows what DWM holds,
   and a scaled display — are all downstream of a session sitting behind Windows
   Hello (§13.11 items 3, 4 and 9).

---

## 14. Stage 10: the compositor can put something *into* the arena

### 14.1 Status in one paragraph

Written 2026-08-13, branched from stage 7 and **merged after stage 8** — so
§13 is stage 8's and this is §14, and every gate below was re-run from master's
checkout after that merge rather than carried over from the branch.
**§12.13 item 7 is closed.** A
`wl_keyboard.keymap` now crosses as `Descriptor::ArenaSlice` — three integers,
naming nothing — and `weston-eventdemo`, which has never heard of this project,
decoded `helloZ` from it with its own libxkbcommon on a run where the compositor
was started with **no `--shared-dir` and no shared directory in existence**. The
whole of `scripts/wc-input.sh` is **82 passed, 0 failed** (52 before), 28 of
those from a new leg that is the old one with the transport swapped and nothing
else changed. Minting a 71249-byte object for a peer costs **1.5–1.9 µs** as an
arena slice against **77.8–82.5 µs** as a file in a *local* directory and
32.0–35.4 µs as a session-local section. The cost was two new concepts and one
new type: an **outbound window**, which is the statement §12.11 item 3 said did
not exist anywhere, and a **`Gift`**, which is the only value in `wc_handle` that
carries a write path into an arena and is not constructible from anything that
arrived on the wire.

What has **not** happened is a guest run. Everything here is Wine and unit
tests, and the arena under Wine is a *file* rather than guest RAM — which is the
boxed item at the top of §14.12 and is the only open question this stage has
that matters, because "it works over a file" is the claim stage 5 spent a whole
stage disproving for pixels.

### 14.2 What was built, in the order the bytes move

| piece | where | what it does |
|---|---|---|
| `arena::provide_file` | `wc-handle`, both platforms | opens a file arena **read-write** and marks it provided — the local twin of `attach_guest_ram` for a machine with no hypervisor |
| `arena::reserve_outbound(id, len)` | `wc-handle` | declares the last `len` bytes of a provided arena to be this process's to give away |
| `arena::Gift` / `arena::give` | `wc-handle` | first fit over that window, 4096-byte granularity, coalescing on drop |
| `Handle::create_shared` | `wc-handle` | **arena window, then published directory, then section** — most specific first |
| `--arena-provide`, `--arena-outbound` | `wc-compositor` | the two flags, applied before the socket exists |
| `PoolTransport::peer_slice` | `wc-proxy` | reads the compositor's window out of the region this side already had mapped |
| `Arena::reserve_outbound` | `wc-proxy` | shortens the pool allocator's free list by the same number |
| `outbound <off> <len>` | `wc-arena-host`'s plan | carries that number from where it was decided to where it is needed |

Everything above `Handle::create_shared` is untouched, which is the same claim
§12.2 made about `Arena::open_or_create` and is the reason both claims are worth
making. `vendor/smithay`'s `SealedFile::with_data` — the one call site, and the
thing that actually makes a keymap — is **byte for byte what stage 6 left**. It
calls `create_shared` and `write_at`, and both of those now mean something else.

### 14.3 The safety property, and why it is a type rather than a comment

§12.11 item 2 named the obstacle exactly: *"A compositor-side arena is read-only
by contract. `Handle::map` gives a `Mapping`, never a `MappingMut`, and that is
what stops a compositor writing into a client's pixels."* That is a property
worth more than the feature, so the feature was built without spending it.

**The rule that already existed does all the work.** A `Handle` decoded from the
wire owns nothing, and a `Handle` that owns nothing has no write path — that is
stage 1's rule and it is why a compositor cannot write a client's buffer today.
Stage 10 did not widen it. It split what "owns something" can *be*:

```rust
enum Owned {
    Object(platform::OwnedObject),   // a section, or a file
    Gift(arena::Gift),               // bytes out of the outbound window
}
```

A client's `wl_shm` pool arrives as a `Descriptor::ArenaSlice`, becomes a
`Handle::from_descriptor`, has `keepalive: None`, and is refused by `write_at`
and by `map_mut` exactly as before. A keymap is a `Gift`, and **there is no
function that turns a `Descriptor` into a `Gift`** — `give` is the only
constructor and it allocates only inside the declared window. So the question
"could a client's buffer write become a compositor write to arbitrary arena
memory?" is answered by the type checker rather than by review.

Three smaller decisions follow from the same argument and are worth naming
because each of them is a place the easy version would have been wrong:

* **`Gift::write_at`'s offset is relative to the gift**, and its bound is the
  gift's own length, which the allocator chose. There is no argument to it that
  reaches another gift, a client's pool, or the stamps. A wider
  `arena_write(arena, offset, bytes)` would have been four lines shorter and
  would have made the rule a matter of caller discipline.
* **A gift has no `map_mut`.** A `MappingMut` is a writable view that outlives
  the call and hands out `&mut [u8]`; over arena memory that is a lasting write
  capability into a region the peer is copying pixels through. A server-created
  object needs to be filled in once, and a bounded copy does that.
* **`guest_write` got narrower.** The coherency probe (§12.6) is still the only
  other write path into a provided arena, and it now refuses any range that
  overlaps the outbound window. Partly for the safety argument, and partly for
  ordinary correctness: the window holds live keymaps that clients have been
  told about, and a probe write into one would present as a client dying inside
  libxkbcommon, a long way from a coherency probe.

One thing that fell out and is better than it was designed to be:
`arena::guest_write` has exactly one caller in the whole tree and it is
`crates/wc-handle/src/bin/wc-poolcost.rs`. **The compositor binary contains no
path that writes an arbitrary arena offset at all.**

### 14.4 What it costs to mint an object for a client

`wc-poolcost` measured the *receiving* side per pool (§12.4). This is the
sending side, per object, through the same production path — `create_shared`,
`write_at`, drop — for a 71249-byte object, 64 iterations, three runs, under
Wine on this workstation:

| minting an object for a peer | create | fill | release | total |
|---|---|---|---|---|
| `Local\wc-…` section | 6.7–7.7 µs | 19.9–22.1 µs | 5.4–5.7 µs | 32.0–35.4 µs |
| a file in a published directory (**local** fs) | 28.0–30.2 µs | 31.7–33.3 µs | 17.9–19.0 µs | **77.8–82.5 µs** |
| **an arena slice** | **0.2–0.3 µs** | **1.1–1.4 µs** | **0.2 µs** | **1.5–1.9 µs** |

**The middle row is a floor and must be read as one.** That directory is on this
workstation's own filesystem, not on the guest's virtio-fs export, and §12.4
measured a *single open* of a file on that export at 539–546 µs — two orders of
magnitude above the 28 µs here. The real ratio on the deployment that matters is
therefore larger than the 45× this table shows, and by how much is not known
because it has not been measured there.

The honest counterweight: **a keymap is minted once per keyboard, not once per
frame.** 80 µs a client is not a number anybody would notice, and if this were
the whole argument it would not be worth the code. The argument is §14.7.

### 14.5 Which arena, and which bytes: the tail, and how the number travels

§12.11 item 3 asked for a statement of ownership rather than an encoding, and
the statement is `(arena, offset, len)` with two constraints on it.

**It is the tail.** Both allocators that can share an arena with this one —
`wc-proxy`'s pool allocator and `sv_arena::alloc` — are first fit from offset 0
upward, so the tail is what they reach last. The peer then has one number to
subtract from its own length rather than a hole to route a free list around, and
the free list stays a sorted run list with no special case in the one piece of
code that must not have one.

**The number has one origin.** It is decided inside the guest by
`reserve_outbound`, printed on the compositor's report line, and carried to the
proxy *in `wc-arena-host`'s plan* — not typed onto two command lines. Two
spellings of one number can differ; the symptom of their differing is two
allocators over the same bytes, which is a client's pixels written on top of a
live keymap, silently. `wc-proxy` refuses `--arena-outbound` together with
`--arena-plan` for that reason. The file-arena case has no plan, so there it
*is* a flag on both, and that is a real weakness of the file-arena case rather
than an inconsistency.

The unit test that matters here is not the allocator's. It is the one that fills
the arena: `a_keymap_cut_from_the_arena_reaches_the_client_with_no_shared_filesystem`
asks the pool allocator for everything it will give — 7936 pages of an 8192-page
arena — and asserts that not one of them is inside the window and that the
keymap still reads back afterwards.

### 14.6 The keymap, measured with an unmodified client

`scripts/wc-input.sh` grew a leg. It is the stage-6 Wine leg with two flags
changed and **nothing else** — the same `weston-eventdemo`, the same
`SendInput` of `helloZ`, the same `check_client_events` — because the claim is
that the transport changed and nothing else did, and a weaker check on the new
leg would prove nothing.

| | shared-file (stage 6) | arena (stage 10) |
|---|---|---|
| compositor flags | `--shared-dir`, `--shared-peer-prefix` | `--arena-provide`, `--arena-outbound` |
| proxy summary | `3 fds in, 1 out` | `3 fds in, 1 out (1 from the arena)` |
| keymap on the wire | `wl_keyboard@10.keymap(1, fd 8, 35572)` | the same |
| what the client typed | `helloZ` | `helloZ` |
| files in the keymap directory | 1 | **0** |
| checks | 26 passed, 0 failed | **28 passed, 0 failed** |

The `1 from the arena` is a new counter in the proxy's summary and it exists for
§8.4's reason: *a stage that added a second provider with no way to tell which
one ran would not really have added it.* The two runs produce byte-identical
characters in the client, so the count is the only thing that distinguishes
them.

One number in that table is worth an explicit note, because it looks like two
things disagreeing and is not. The compositor's own log says
`layout us (71249 bytes of keymap)` and the client is handed **35572**. Both are
right: smithay recompiles the embedded source through xkbcommon and serialises
the result, so the source length and the wire length were never going to agree,
and the window is sized against the second of them.

The fd count in the second row is the one that needs an argument rather than a
note, and it gets its own section next.

### 14.7 `3 fds in, 1 out`, and why that is not the invariant breaking

Stage 4 established `0 out` and stage 7 asserted it must stay true. The number
in the table above is `1`, so it is stated here, at the point where a reader
meets it, rather than left to a handover message. **A count that a reader cannot
judge from the document is not an assertion, it is a number.**

**Which two processes that descriptor exists between.** `wc-proxy` and its
Wayland client — `weston-eventdemo` here, Firefox in
`docs/goal-firefox.md` — over the AF_UNIX socket `wc-proxy` itself binds in
`--runtime-dir`. Both are ordinary Linux processes on the **host**. The
descriptor is an anonymous `memfd` the proxy created a moment earlier and copied
bytes into, and it travels by `SCM_RIGHTS` on a socket that has never left one
machine.

**Why that pair is not the boundary.** The boundary is the *other* socket: the
one from `wc-proxy` to `wc-compositor.exe`, through `ssh -L` and
`probe-relay.exe`, into a virtual machine. `crates/wc-proxy/src/proxy.rs` writes
that one with `write_some(upstream.as_raw_fd(), &to_server)` — a plain `write`,
no `sendmsg`, no control message, **no fd argument to pass one through**. The
count on the hop that crosses is therefore not merely zero, it is
unrepresentable; the module header states this as the reason the two sockets are
different kinds of thing, and it is the reason to trust the property rather than
the counter.

**Why the keymap gives the client no other choice.** `wl_keyboard.keymap` is
`(uint format, h fd, uint size)` and the client `mmap`s the `h`. There is no
by-name variant, no `keymap_v2`, nothing to negotiate. So the proxy must produce
a descriptor for its own client, and there are exactly two ways to get one:
receive it, or make it. It makes it — `materialise` creates a fresh `memfd` and
copies the bytes in, which is why the copy is deliberate and documented rather
than an inefficiency to be optimised away later. **A shared mapping would have
been cheaper and would have handed the client a window into the arena.**

**What the number would have to be for the rule to be violated.** Not this one.
The violated states are:

* **any** descriptor on the upstream socket, in either direction — which no code
  can produce, per above;
* a descriptor `wc-proxy` *received* being forwarded rather than consumed. The
  client's pool fd is `mmap`ed and dropped here; the count of descriptors the
  proxy passes on unchanged is zero and there is no code path that would make it
  anything else;
* the descriptor the proxy hands its client having **originated in the guest** —
  the same violation seen from the other end, and the one somebody would
  actually build if they wanted to save the copy. It cannot happen by accident:
  an `h` from the compositor arrives as `Descriptor` **bytes in the message
  body** (§2.3), which is a name, a path or three integers, and the one variant
  that would be a handle crossing — `Descriptor::NtHandle` — is asserted against
  by name in `nothing_but_a_name_or_an_offset_reaches_the_compositor`. Nothing
  constructs one, and §2.7 says nothing on the critical path ever needs to.

So `3 fds in, 1 out` reads: three descriptors arrived from a local client and
were consumed here; one was manufactured here and given to a local client; none
went near the hypervisor. The harnesses assert the shape of that line rather than
`0`, and this paragraph is what makes that assertion mean something.

**Why this is written out at length.** Because stage 6 already had to replace a
stage-4 assertion that was measuring the wrong thing, and this is the second
visit to the same trap. An invariant that is enforced by construction and
asserted by a counter erodes in exactly one way: somebody changes what the
counter counts, the assertion still passes, and the construction is never
re-checked. The construction is `proxy.rs`'s upstream writer having no fd
argument. If that ever changes, this section is wrong and the counter will not
notice.

### 14.8 Can `--shared-dir` go away? Yes, where there is an arena — and that is the point

Stated plainly, because it was asked plainly.

**`wc_handle::publish` has exactly one customer in the tree**, and it is
`SealedFile::with_data`, and that is the keymap. There is nothing else. So the
question "can the shared directory go away" is exactly the question "can the
keymap come out of the arena", and the answer is yes:

* **With an arena — `--transport arena` or `--transport guest-ram` — it can go
  entirely.** `scripts/wc-guest.sh`'s guest-RAM path now passes no
  `--shared-dir` at all and asserts that nothing lands in the export's `keymaps`
  directory. Every protocol object in both directions is then either a client's
  pool copied *into* the arena or a compositor's object cut *out* of it.
* **Without one it cannot, and should not.** `--transport shared-file` has no
  region to cut from, and two Windows processes on one machine want a
  `Local\wc-…` section rather than either. `create_shared`'s three-way choice is
  exactly this fact, and `--shared-dir` remains the right answer for the
  deployment that has no arena.

**What this is actually worth is not the 80 µs.** It is that stage 5 measured
the guest's virtio-fs export as *incoherent for live mappings* (§10.3:
MISMATCH on 65280 of 65536 bytes, both directions) and 330× slower to touch,
and stage 7 found that the keymap survived it anyway because a keymap happens to
be a closed write rather than a live mapping (§12.11). That was luck, and it was
luck the *next* server-created object — a clipboard offer being drained while the
client reads it, a cursor surface, an icon — would not have. Removing the last
protocol use of that filesystem means the next one inherits an arena instead of
inheriting the caveat.

What the guest still uses the share for is the harness's own artefacts: frames,
logs, pool files for the other two transports. Those are `--dump-frames` and
`--log`, they are ours, and none of them is a protocol object.

### 14.9 "Implemented" has meant "compiled" twice now, and that is a pattern

§12.11 said *"the wire is ready — the client side of `wc-proxy` already resolves
one"*, and the code was there: `materialise` had a `Descriptor::ArenaSlice` arm
written in stage 4. **It could never have run.** It resolved the slice through
`wc_handle::arena::window`, and `wc-proxy` does not register its region in that
registry and never has — the region belongs to `pool::Arena`, which holds a
`StitchedRegion` of ~1500 `mmap(MAP_FIXED)` calls into one reservation. The
first thing that ever sent an arena slice in that direction got
`arena 0 is not attached`.

The fix is small — the read moved onto the transport, `PoolTransport::peer_slice`,
because there must be one owner of that mapping and registering it twice would
be two owners who could disagree — and the fix is not the point. **The claim in
a merged design document was false**, and it was load-bearing: it is the sentence
that made this stage sound like an afternoon's work.

**This is the second instance.** §9.10 item 5 said `wl_keyboard.keymap` was
"implemented and unexercised by a real client", and §11.2's annotation on it is
blunt about what that turned out to mean: *"'Implemented and unexercised' reads
as a coverage gap. It was not: the compositor published the keymap as a
`Local\wc-…` section, which no Linux process can open, so the path **would have
failed the first time anything used it**."* Same shape, different stage, and
neither was found by review.

Two independent instances is enough to name the failure mode, so:

> **A path with no test that reaches it is not implemented, whatever it
> compiles to.** The two instances share a signature — a branch on a `Descriptor`
> variant that nothing constructed yet — and both were written *in anticipation*
> of a direction that did not exist. Anticipatory code is where this project
> keeps putting its false claims, because it is the code whose correctness
> nobody can check by running anything.

What follows from it for a reviewer, concretely: when a stage's write-up says a
path "already" works, the question to ask is not "is the code there" but
**"which test executes it, and what does that test assert about the result"**.
For this arm the answer was "a unit test constructs the descriptor and checks the
bytes come back", and writing that test is what found both this and the offset
bug below. The arm had existed for six stages.

**The same arm had a second defect underneath the first.** It wrote the bytes
into its fresh `memfd` with `write`, leaving the descriptor's file offset at EOF,
and handed *that* descriptor to the client. libwayland's keymap path `mmap`s and
does not care, which is why nothing would ever have caught it — but the next
customer of this direction is a clipboard transfer, which *reads*, and would have
got zero bytes and no error. It is `pwrite` now, so a fresh descriptor's offset
is where a fresh descriptor's offset should be. This was found by the same unit
test, doing `read_exact` on the fd because reading it back seemed like the
obvious thing to assert.

### 14.10 Two smaller surprises

**The startup log had to learn about the new case.** The compositor printed *"no
`--shared-dir`, so the keymap will be a Windows section"* on every arena run —
a true sentence about a code path that did not run, printed at exactly the moment
somebody is looking for the reason a client died. It now names whichever of the
three actually applies. This is the second time in this project that a
diagnostic surviving a stage unchanged was worse than no diagnostic.

**`weston-eventdemo` wants a 4 MiB pool.** It asks for **4190208 bytes** for its
cursor theme on top of its own 800 KiB surface, so a 4 MiB arena ran out, the
proxy reported `no run of 4190208 bytes`, and the client exited before a
keystroke was injected — 8 of the leg's 28 checks failing for a reason that was
nowhere near the transport. That is §12.13 item 5, "64 MiB is a guess", arriving
in the smallest client available, and the guess has to be generous.

### 14.11 The gates

| Gate | Result |
|---|---|
| `cargo test --workspace` | **138 tests**, 0 failures (135 on master, which is 133 plus stage 8's two) |
| `cargo clippy --workspace --all-targets` (Linux) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `cargo test --manifest-path crates/wc-arena-host/Cargo.toml` | 1 test, 0 failures |
| `cargo clippy --manifest-path crates/wc-arena-host/Cargo.toml --all-targets` | clean |
| `scripts/wc-proxy.sh window` (stage 4, Wine) | 36 passed, 0 failed — unchanged |
| `scripts/wc-input.sh all` (stage 6 + this, Wine) | **82 passed, 0 failed** (52 before) |
| `scripts/wc-desktop.sh` (stage 8, the interactive desktop) | **not run — needs the guest, same as below** |
| `scripts/wc-guest.sh` (the guest) | **not run — see the boxed item in §14.12** |

All of these are from **master's checkout after the merge with stage 8**, not
from the branch before it. Every one was re-run there rather than carried over,
which is the arrangement §12.12 used and the reason the test count in the first
row is not the one the branch reported.

The three new tests are one each side of the boundary plus the plan's parser,
and two of them are deliberately
*one* test rather than several: `wc_handle`'s outbound window and `publish`'s
directory are process-global by design, and `cargo test` runs threads at once, so
a second test that reserved a window would race the first and the loser would
report a perfectly true `AlreadyExists` about somebody else's arena.

### 14.12 What is not proven

**One item on this list is not like the others, so it is not on the list.**

> **This has never run over guest RAM. The arena under Wine is a file.**
>
> `provide_file` exists precisely because there is no hypervisor on this
> workstation and nothing can locate pages when there is no VM, so the substrate
> the Wine leg exercised is a file both processes open. Everything above
> `wc_handle::arena` is identical between the two — that is the claim §12.2 made
> about the pool direction and this stage inherits it — and **"identical above
> the provider" is exactly what stage 5 spent a whole stage disproving for
> pixels.** §10.7: every check above the pixels passed, over a transport that
> was not delivering them, for two stages.
>
> What is specifically unexercised is a `Gift` written into `VirtualAlloc`ed,
> pinned, stamped guest memory and read back by the host through the stitched
> view of QEMU's memory backend. §12.6 measured that region coherent in both
> directions for a live mapping, and §12.11 measured that a keymap is a closed
> write rather than a live one, so there are two independent reasons to expect
> it to work. Neither is a run.
>
> `scripts/wc-guest.sh` is wired for it: `--arena-outbound` on the compositor,
> `--outbound` on the locator, **no `--shared-dir` at all** on the guest-RAM
> path, and three assertions in the checker — that a descriptor came from the
> arena, that the export's `keymaps` directory stayed empty, and that the
> compositor reported the window. It needs a guest; stage 8 has finished with
> it and stage 9 has not. The harness being wired and the assertions being
> written is the right stopping point — taking the guest from a stage that is
> mid-run would not be, and §12.8 exists because somebody already did.
>
> > **[CLOSED BY STAGE 12 — §17, and the harness did not pass the first time.]**
> > A `Gift` has now been written into `VirtualAlloc`ed, pinned, stamped guest
> > memory and read back by the host through the stitched view: `wl_keyboard.keymap`
> > crossed as three integers on the real guest, twice in one run, with the
> > export's `keymaps` directory empty and `--shared-dir` not passed
> > (§17.5). Both "independent reasons to expect it to work" held. **What the
> > file substrate could not tell you is that it would not work at all**: the
> > first run of this harness reported `located 16374 of 16384 pages` and wrote
> > no plan, because the gift destroys the very stamps the host locates by and
> > §14.12 item 1's ordering argument is false (§17.4). A file arena has no
> > stamps, so no amount of the Wine leg could have found it — which is this
> > box's own claim, arriving in a form it did not predict.

§12.13's list, with the rest of this stage's changes:

1. **The gift overwrites stamps, and the ordering is argued rather than
   measured.** A gift written before the host's locate would take pages of
   stamps with it and the sweep would report `located 16366 of 16384`. The order
   holds in every topology that exists — the host cannot proxy a client it has
   not connected for, and the compositor cannot mint a keymap before a client
   binds a seat — but "holds by construction" is what §12.10 said about a second
   locate before measuring it, and it deserves the same treatment. It is part of
   the same guest run as the item above.

   > **[MEASURED BY STAGE 12 — §17.4 — AND THE ARGUMENT WAS WRONG.]** The
   > second clause is false. smithay seals the keymap in `Seat::add_keyboard`,
   > which this compositor calls **during start-up**, before the socket is
   > bound — so the first guest run of an outbound window reported
   > `located 16374 of 16384 pages` and wrote no plan, with no client anywhere
   > near it. Ten pages is a 37532-byte `de` keymap rounded up, and the
   > prediction of `16366` was right about everything except that it needs
   > nobody's help. The fix is `arena::defer_gifts`: gifts are held in the
   > compositor's heap until the first client connects, which is a moment that
   > provably follows the locate because the client arrives through a proxy that
   > was started from the plan. This paragraph's own caution — "holds by
   > construction is what §12.10 said before measuring it" — is the only reason
   > the run was made at all.
2. **Releasing a gift is only unit-tested.** A keymap is dropped when the layout
   changes, and the layout change is exercised on the Linux leg, where
   `create_shared` is not the arena path. So the free list's coalescing has never
   run in anger. It is thirty lines and it is tested; it has simply never been a
   real compositor's free list.
3. **One outbound window, one client, one object at a time.** The window is
   process-global, which is right — it is a property of the deployment, exactly
   as the arena registry and `publish` are — but nothing has yet asked for two
   objects to be live at once outside a unit test, and §12.13 item 6's "one
   client at a time" is unchanged and now has a second reason to matter.

   > **[CLOSED BY STAGE 12 — §17.2 and §17.5.]** Both halves. Firefox binds
   > `wl_seat` twice and gets a keymap for each, so two gifts were live in one
   > window at once on the guest; and "one client at a time" is gone — an arena
   > is a handle on one region and one free list, three connections shared it on
   > the guest and four on the Wine leg. The free list also ran in anger for the
   > first time on the *pool* side: offset 28672 was reused by a later pool
   > after an earlier one was released and coalesced, which is item 2's other
   > half.
4. ~~The compositor cannot put anything *into* an arena.~~ **Closed.** §12.13
   item 7, and §12.11 items 1, 2 and 3 with it.
5. **The window has still never been seen** (§12.13 item 2), pinning is still
   asserted rather than proven over time (item 4), 64 MiB is still a guess and
   this stage produced a client that would have exhausted 4 MiB (item 5), and
   §11.10's whole list is unchanged.
6. **The arena leg is a Wine pass and therefore a hypothesis.** What it can say
   is that the wire format, the allocator, the ownership split and the client's
   own libxkbcommon all agree; what it cannot say is anything about guest RAM,
   which is the boxed item.

   **On the count of times Wine has been wrong, this stage was handed three
   different numbers and is not going to add a fourth.** The number in the brief
   for this stage was eight; the number in the handover correcting it was seven;
   **§13.6, written this week and now merged, says four** — it withdrew the
   `SetProcessDpiAwarenessContext` entry on the grounds that a process in session
   0 is already per-monitor-aware because there is no display to virtualise, so
   Wine and the real desktop agreed all along and the disagreement was with a
   window station in which the question is meaningless.

   §13.6 is the authority here, because it is the only one of the three that
   comes with the measurement attached. The discrepancy is worth a paragraph
   rather than a silent correction, for the same reason §14.7 exists: **a number
   that is passed between people rather than derived from the document drifts,
   and it drifts upward**, because withdrawals are remembered by whoever made
   them and additions are remembered by everybody. The operational conclusion is
   the one §13.6 draws and it does not depend on the count at all — a negative
   result from Wine, or from the guest, is only evidence if the question was
   asked in the place the code will really run. For this stage that place has a
   hypervisor in it and Wine does not.

### 14.13 What to do next

1. **Run `scripts/wc-guest.sh stage4` on the guest, over `guest-ram`.** This is
   the boxed item in §14.12 and there is no second thing on this list until it is
   done: everything above is evidence about a file arena, and the deployment this
   project exists for does not have one. Stage 8 has finished with the guest;
   stage 9 has not, and `WC_GUESTNAME` and `WC_HOST_SHARE` keep this out of its
   way (§12.8). The run also settles §14.12 item 1.

   > **[CLOSED BY STAGE 12 — §17.]** Done, and with Firefox on the far end
   > rather than `weston-terminal`, which is why it closes §15.14's box in the
   > same run — §15.15 item 2 said one run would and it did.
   > `scripts/wc-guest.sh firefox` is 16 passed, 0 failed, the transport line is
   > `mapped: 0 section, 0 shared-file, 3 arena-slice`, and the compositor was
   > started with no `--shared-dir`. It settled §14.12 item 1 by refuting it
   > (§17.4).

   Stage 8 makes one part of it easier and one part harder. Easier: §13.2
   established that the interactive desktop is reachable and that a run in an
   ssh session's window station is measuring the wrong place — so if the arena
   leg is ever re-run for anything other than the transport, it should be run
   there. Harder: §13.6's lesson applies to this stage's own evidence with no
   translation. A file arena on a workstation is a window station in which the
   question is meaningless, in exactly the sense §13.6 means.
2. **§12.14's list is otherwise unchanged**, and its item 1 — log in and look at
   a window — is **closed by §13**, which is why it is no longer the largest gap
   in the project and the boxed item in §14.12 is.
3. **Give the file-arena case one origin for its number too.** The plan carries
   it for guest RAM and a flag carries it on both sides for a file, which is the
   disagreement this design otherwise avoids. The fix is probably a header in the
   arena file itself, and nothing yet needs it because the file arena is a test
   substrate.

---

## 15. Stage 9: Firefox, and the list it hands us

### 15.1 Status in one paragraph

**Firefox runs on this compositor, draws its whole interface, opens its menus,
and appears in a Windows window with Windows' own caption.** It got there by
closing five things that were missing, four of which were found by running it
rather than by reading anything: `wl_output`, `wl_data_device_manager`, a scene
graph, `xdg_popup`, and the window-geometry rectangle. The two questions the
brief called strategic are answered with evidence and neither answer is the
feared one — Firefox **falls back to `wl_shm`** when `zwp_linux_dmabuf_v1` is
absent, so no file descriptor has to cross anything; and it uses **KDE's**
`org_kde_kwin_server_decoration`, asking for *client-side*, which is what
`docs/goal-firefox.md`'s tabs-in-titlebar clause needs. What is still missing is
the titlebar clause itself, an icon, and everything about acceleration.

`docs/stage9/stage9-firefox-window.png` is the artefact: Firefox, unmodified, on the
Linux host, through `wc-proxy`, in an `HWND` made by `wc-compositor.exe`.

### 15.2 The requirement list, which is the point of the stage

`docs/goal-firefox.md` closes with *"Firefox needs a specific set. Enumerate
what it actually binds and implement that, rather than the 137 the research
classified."* This is that enumeration, measured with `WAYLAND_DEBUG=1` against
this compositor (`scripts/wc-firefox.sh enumerate`), and it is **nine globals**,
not 137.

Firefox opens **two Wayland connections that matter** and binds a different set
on each, which is worth stating first because it explains every version number
below: GTK's `GdkDisplay` is one client and Gecko's `nsWaylandDisplay` is
another, inside one process.

| global | bound by | version asked | what happens without it |
|---|---|---|---|
| `wl_compositor` | GTK, Gecko | 3, 4 | nothing runs |
| `wl_shm` | GTK, Gecko | 1, 1 | nothing draws |
| `wl_subcompositor` | GTK, Gecko | 1, 1 | **the window is empty** (§15.5) |
| `wl_seat` | GTK, Gecko | 5, 8 | no input |
| `wl_output` | GTK, Gecko | 2, 2 | **the window is 552x172** (§15.3) |
| `wl_data_device_manager` | GTK | 3 | **GTK never creates a seat** (§15.4) |
| `xdg_wm_base` | GTK, Gecko | 6, 6 | no window |
| `org_kde_kwin_server_decoration_manager` | GTK | 1 | CSD anyway, silently (§15.7) |
| `zxdg_decoration_manager_v1` | **nobody** | — | nothing (§15.7) |

And the requests it makes on them, counted over one 25-second run, because a
list of globals is not a list of what has to work:

| request | count | implemented |
|---|---|---|
| `wl_surface.commit` / `.attach` / `.damage_buffer` | 519 / 181 / 158 | yes |
| `wl_surface.frame` | 320 | yes, throttled to 16 ms |
| `wl_shm.create_pool` + `wl_shm_pool.create_buffer` | 161 / 160 | yes |
| `wl_compositor.create_region` + `wl_surface.set_opaque_region` | 77 / 47 | parsed, **ignored** |
| `wl_surface.set_input_region` | 24 | parsed, **ignored** |
| `xdg_toplevel.set_min_size` / `.set_max_size` | 25 / 25 | parsed, **ignored** |
| `wl_surface.set_buffer_scale` | 25 | honoured only at 1, and it says so |
| `xdg_surface.set_window_geometry` | 23 | **yes, new in this stage** |
| `wl_subsurface.set_position` / `.set_desync` | 3 / 1 | **yes, new in this stage** |
| `xdg_toplevel.set_title` / `.set_app_id` / `.set_parent` / `.set_maximized` | 1 each | title and app_id yes; parent ignored |
| `xdg_popup` + `xdg_positioner` | on demand | **yes, new in this stage** |

The three "parsed, ignored" rows are honest rather than alarming: an opaque
region is an optimisation, an input region matters only where the compositor
does hit-testing (Windows does it), and min/max size is a constraint on a
resize this compositor does not initiate. They are listed because a later stage
that starts doing its own hit-testing needs to know the data is already
arriving.

**What Firefox binds that we do not offer** is the other half of the list, and
it was measured twice. Against Weston 16 headless with the GL renderer — a
compositor that offers everything — Firefox additionally bound
`zwp_linux_dmabuf_v1` v4, `wl_drm` v1, `wp_presentation` v2,
`wp_viewporter` v1, `wp_commit_timing_manager_v1`, `wp_fifo_manager_v1`,
`wp_color_representation_manager_v1`, `zxdg_output_manager_v1` v2,
`zwp_relative_pointer_manager_v1` and `zwp_pointer_constraints_v1`. Several of
those are **Mesa's**, not Gecko's: `wl_drm`, `wp_fifo` and `wp_commit_timing`
come from the Wayland WSI inside the GL driver and appear only when Firefox
gets a GL context, which is the acceleration path and not this stage.

And the *upper bound* on the list, which needs no compositor at all: the
interface-name strings compiled into the binaries. `libxul.so` contains
`xdg_activation_v1`, `wp_fractional_scale_manager_v1`,
`zwp_pointer_gestures_v1`, `zwp_primary_selection_device_manager_v1`,
`gtk_primary_selection_device_manager`, `zwp_idle_inhibit_manager_v1` and
`wp_color_manager_v1`; `libgdk-3.so` adds `gtk_shell1`, `zxdg_shell_v6`,
`zwp_keyboard_shortcuts_inhibit_manager_v1`, `zwp_tablet_manager_v2` and
`wp_cursor_shape_manager_v1`. **Nothing outside those two lists can ever be
asked for**, which bounds every future stage's protocol work at roughly
twenty-five interfaces rather than 137.

### 15.3 `wl_output`: without one, the window is 552x172

The compositor advertised no output until this stage, and the symptom was not
an error. Firefox started, mapped, drew, and was **552x172 pixels** — a browser
window smaller than its own toolbar. With one output at 1600x1000 the same
build maps at 1280x900.

That is GTK sizing a window against an empty monitor list and falling back. It
is worth recording as the shape of the whole stage's findings: **the failure was
silent, plausible and wrong**, and nothing in the compositor's log said
anything.

The object is `smithay::output::Output` with a mode from `--output WxH`,
`create_global`, and `Output::enter` on each surface as it maps. The numbers are
a flag: on the guest they should come from `EnumDisplayMonitors` and do not, and
that is §15.14 item 3 rather than a comment nobody reads.

### 15.4 The clipboard gates the keyboard, and that is not a joke

The most surprising measurement of the stage. With `wl_seat` advertised at
version 9 and working, GTK's connection **did not bind it** and Firefox printed:

```
Gdk-CRITICAL **: gdk_seat_get_keyboard: assertion 'GDK_IS_SEAT (seat)' failed
```

Adding `DataDeviceState` — thirty lines, all of them smithay's defaults — made
GTK bind `wl_seat` v5 on the next run. So a compositor with a perfect seat and
no clipboard gives a GTK client **no keyboard and no pointer at all**.

**Why, and it is worth the paragraph, because "no input unless an unrelated
global is present" is exactly the coupling that will bite the next person
building a minimal compositor.** GDK does not handle `wl_seat` in its registry
callback at all. It registers a *deferred closure* against a named list of
globals it needs first, and runs it only when every one of them has been
announced. The mechanism is visible in the binary: `libgdk-3.so` carries the
symbols `required_device_manager_globals`, `seat_added_closure_run`,
`invoke_exported_closures` — and, tellingly, a second user of the same machinery,
`required_cursor_theme_globals` with `load_cursor_theme_closure_run`. It is a
general dependency-declaration mechanism with two clients, not a guard bolted
onto the seat.

The dependency is real rather than defensive: `GdkWaylandSeat` construction calls
`wl_data_device_manager_get_data_device()` unconditionally, because in GDK's
model a seat *owns* a data device. It will not build half of one, so it does not
build one at all, and the only thing the application sees is a `GDK_IS_SEAT`
assertion much later from whatever first asked the display for a keyboard.

Two things follow for anyone reading this as a specification. `wl_seat` alone
satisfied nothing; `wl_data_device_manager` alone satisfied the whole list on
3.24.51, with no primary-selection global present. And **the failure is silent
and late** — the registry trace shows the global announced and never bound,
which reads like a client that did not want it.

There is no clipboard *content* crossing yet. The two directions are
`SelectionHandler::send_selection` and its reverse, both of which hand the
compositor an fd to write into — on the guest, a `wc_handle::Handle` the
*compositor* creates. Until stage 10 that was the end of the sentence, because
§12.13 item 7 said a server-created object had no route across the hypervisor.
**§14 closed that**: there is an outbound arena window and a `Gift` now, and
§14.8 makes the point that where there is an arena the export directory can go
away entirely. So the clipboard is no longer blocked on a missing mechanism —
it is unbuilt, which is a different and much smaller statement than the one this
paragraph could make a stage ago.

### 15.5 Firefox's pixels are not on the surface you would expect

This is the finding that decides how much of a compositor this has to be.

Firefox's `xdg_toplevel` surface is **GTK's client-side-decoration surface**,
and it is transparent. Every pixel of the browser — chrome, tabs, URL bar,
content — is in a `wl_subsurface` of it, offset by the shadow margin
(`set_position(26, 23)` in every run). A compositor that blits the toplevel's
buffer and discards subsurfaces does not draw Firefox badly. It draws **nothing**:

```
present window=1 1332x952 argb hash=6a8459acffa14925 centre=00000000
```

and the dumped frame is 1,268,064 pixels of `#000000`, measured with
ImageMagick. All three hashes differed between frames, so the compositor was
certain it was presenting; the alpha channel was the only thing changing.

So this stage has a scene graph. It is the smallest one that is correct:

* Every surface gets a **staging shadow** of its own, filled at commit. The
  extra copy is what preserves stage 3's rule that the client's buffer is
  released immediately (`surface.rs`), while still allowing a subsurface that
  did not commit this frame to be redrawn.
* On any commit, the whole tree of the root surface is composited into the
  window's shadow with `with_surface_tree_upward`, parent first, in the order
  the protocol stacks it.
* The composite is a **copy, not a blend**. Firefox's tree is one transparent
  surface with one opaque rectangle over it, for which they are identical. The
  comment in `Shadow::compose` says what will break first.
* The **damage** reported to the platform is still only the committing
  surface's, translated — so a repaint of one subsurface does not invalidate the
  window.

The result is `docs/stage9/stage9-firefox-window.png`, and the check that says so
without a human looking is a colour count: 406 distinct colours where there
were 2.

### 15.6 The window is the geometry rectangle, and stage 8 called this exactly

**Credit where it is due: the diagnosis is stage 8's, not ours.** §13.12 item 1
is a page long, is the only item on that list with "nothing else is close"
attached to it, and predicted this stage's failure by name:

> **This will hit Firefox immediately.** Firefox is a GTK client, GTK uses
> client-side decorations with a shadow margin of exactly this shape, and stage
> 9 is pointing it at this compositor now. The failure will look like a window
> that is offset and clipped, and it will look like a blit bug, and it is not
> one: the blit is correct and reads the wrong rectangle.

It did hit Firefox immediately, and the shape was exactly as written. Stage 8
measured it on `weston-terminal` on the real guest — a 946x555 buffer against a
window-geometry rectangle of 882x491 at +32,+32, the compositor blitting the
buffer's top-left 886x491 — and left it as an item rather than a patch, which
was the right call for a stage whose errand was to look at the window.

**It is implemented here**, because the scene graph had to decide how big the
canvas was and picking the buffer would have been picking the wrong answer
knowingly. The canvas is `SurfaceCachedState::geometry` when the client sets one
and the root buffer's bounds when it does not; every surface is drawn at its
tree offset **minus the geometry origin**. Firefox's numbers: a 1332x952 buffer,
a 1280x900 window, 26 px of margin each side and 23 on top. `weston-terminal`'s
on this workstation: an 806x491 buffer and a 742x427 window.

The fix landed a layer above where §13.12 predicted. That section named
`FrameStyle::content_origin` and the blit, which is where it belongs if the
window shows one buffer; once the window shows a composited *tree* the origin
has to be subtracted while the tree is being assembled, or every subsurface is
offset by it too. `FrameStyle::content_origin` is untouched and still zero,
which is now purely the titlebar-extension parameter it was built to be.

> **For whoever merges next.** Two assertions in `scripts/wc-proxy.sh` had been
> asserting that this bug was *present*, and they are updated in place rather
> than relaxed: `weston-terminal`'s window was expected to be `806x491`, which
> is its buffer, and its dumped frame to be at least that, which was 869x555 of
> buffer. They are now `742x427` for both — the window-geometry rectangle, and a
> dump that is exactly the window because the shadow *is* the window now. A
> stage that finds those two numbers failing should check whether it has
> reintroduced §13.12 item 1 before it changes them.

### 15.7 Decorations: both offered, and the answer is KDE's

The research said GTK does not implement `xdg-decoration` and uses KDE's
instead, and the goal document said to measure rather than assume. Measured
three ways, all agreeing:

1. **Strings in the binaries.** `libgdk-3.so.0.2419.32` contains
   `org_kde_kwin_server_decoration_manager` and does **not** contain
   `zxdg_decoration_manager_v1`. `libxul.so` contains neither, so this is GTK's
   decision and Gecko has no say in it.
2. **The registry, with both advertised.** The compositor now offers both
   globals. Firefox binds `org_kde_kwin_server_decoration_manager` v1 and
   ignores `zxdg_decoration_manager_v1` — it is announced in the same
   `wl_registry.global` burst and never bound.
3. **What it then asks for**: `org_kde_kwin_server_decoration.request_mode` with
   `Mode::Client`. Firefox intends to draw its own titlebar.

That last line is the good news the goal document needed. Its hard clause —
*"firefox being able to paint its UI elements into the windows titlebar"* — is
only possible if Firefox is drawing a titlebar, and a client that had asked for
server-side decorations would have to be argued out of it. It has not; it is
already drawing one, and `docs/stage9/stage9-firefox-window.png` shows the consequence
plainly: **two titlebars**, Windows' caption above Firefox's tab strip. Closing
that gap is `DwmExtendFrameIntoClientArea` plus `WM_NCHITTEST`, moving
`FrameStyle::extend_top` off zero, and it is the single largest remaining item.

The compositor advertises `Client` as its default mode and answers
`xdg-decoration` with `ClientSide` for anything that does ask, because it has no
frame of its own to draw and Windows' caption is not something the Wayland side
can promise.

### 15.8 The proxy served one client. Firefox opens four

Everything above is the Linux compositor. Putting `wc-proxy` and
`wc-compositor.exe` in the path failed completely and instructively:

```
[GFX1-]: Wayland protocol error: Compositor () crashed, proxy: WP:E WP:CA WP:CR WP:CPCA WP:CPCF
ExceptionHandler::GenerateDump ... timeout: the monitored command dumped core
```

The proxy accepted exactly one connection and exited when it closed, which was
true of every client it had ever been given: `weston-terminal` and
`weston-presentation-shm` each open one and keep it. Firefox's own
`WaylandProxy` opens a **throwaway connection first** to check the compositor is
alive, closes it, and only then connects for real. Against a one-shot proxy the
throwaway is the only connection ever served, the real one finds a closed
socket, and Firefox reports the compositor as crashed and dumps core.

One run of Firefox makes **four** connections — the probe, GTK's, Gecko's, and one
more that binds a registry and goes away. So `wc-proxy` now accepts in a
loop, one thread and one **upstream connection per client**, which is what a
compositor expects — object ids are per-connection and multiplexing would need
the id-rewriting layer §9.3 exists to avoid. The pool transport is built per
session; `SharedFile`'s names carry 128 bits of randomness so two of them over
one directory cannot collide, and `Arena` **refuses the second client by name**,
which is §12.13 item 6 arriving as a real limitation instead of a note.

Two more things that cost time and are worth writing down:

* **A protocol the compositor advertises must be in the proxy's XML.** Adding
  the KDE decoration global made the proxy end the connection with
  `org_kde_kwin_server_decoration_manager has no message 0`. That is §9.3's
  design working — an interface it cannot walk is refused rather than
  desynchronised — and the fix was §9.3's promised cost exactly: copy two files
  into `crates/wc-proxy/protocols/`, add two lines. The message count in
  `wire.rs`'s round-trip test moved 354 → 376 and the interface count 28 → 32,
  which is the change being visible rather than silent.
* **§8.9 again, and it looks like a code bug.** Reusing a `wine explorer
  /desktop=NAME` name across back-to-back runs attaches to the previous,
  dying desktop, and then `CreateWindowExW` fails for every visible top-level
  window. It presents as `X Error: BadWindow ... X_CreateWindow` in the loader
  log and, in the compositor's own log, as `xdg_toplevel created` followed by no
  `HWND` — which is indistinguishable from the popup code being broken. The
  harness now generates a fresh desktop name per run.

With those closed, the Wine leg is green: 26 frames of Firefox's pixels into an
`HWND` sized 1280x900, `mapped: 0 section, 4 shared-file, 0 arena-slice`.

### 15.9 dmabuf, and the answer the rule wanted

The brief called this one strategically important, because a dmabuf is a Linux
kernel object *and* a file descriptor, and no descriptor may ever cross the VM
boundary. The measurement:

* **When `zwp_linux_dmabuf_v1` is advertised, Firefox uses it.** Against Weston
  16 headless with the GL renderer it bound it at v4, three times, alongside
  `wl_drm`.
* **When it is absent, Firefox falls back to `wl_shm` and renders correctly.**
  Not "fails", not "hangs" — the three answers the harness distinguishes. Every
  screenshot in this section is a `wl_shm` fallback.

So the rule is not in tension with the goal for *rendering*: the compositor
simply does not advertise dmabuf, and Firefox draws through shared memory.

What it costs is a copy per frame and the bandwidth in §15.10, and it costs the
`goal-firefox.md` acceleration clause **in this form** — a software-composited
`wl_shm` path is not "hardware acceleration is working for the renderer". The
resolution is not a dmabuf that crosses; it is that the swapchain is already
supposed to end up in an arena buffer on the GPU side (goal doc, "Accelerated
rendering"), so the thing Firefox hands the compositor stays memory both sides
mapped rather than becoming a descriptor. Nothing measured here contradicts
that, and nothing measured here supports it either.

### 15.10 What a Firefox arena costs

§12.13 item 5 said 64 MiB was a guess and that the number to measure was what
Firefox's `wl_shm` pools actually total. Measured, over runs of 25 to 150
seconds:

| what | bytes |
|---|---|
| GTK's pool, initially | 36,864 |
| GTK's pool after nine doublings | 18,837,504 |
| the CSD toplevel surface, per buffer | 5,072,256 (1332x952x4) |
| the content subsurface, per buffer | 4,608,000 (1280x900x4) |
| **settled, idle**: 4 pools | **14,325,120** |
| **a 40-second startup**: 88 pools | **401,397,120** |

Two things in that table matter more than the totals. The first is the doubling
run — nine of them, from 36 KB to 18 MB — which is §10.6's "nine doublings is a
property of the client's machine" appearing again in a completely different
client. The second is the last column of a row that is not there: **`0` pools
were destroyed.** In every run, of every length, `wl_shm_pool.destroy` was sent
zero times. Firefox creates a pool per buffer and keeps them.

The copy cost, from the Wine leg: 268 commits, 261 copies, 1,147.86 MiB in
33.0 ms — 4,503.5 KiB and **126 µs per copy**, at pool scope. That is the price
of the client being unmodified (§9.5), and on the host, which has spare CPU.

### 15.11 Nothing is ever freed, and in the real topology that is guest RAM

The zero in §15.10 deserves its own section, because it is not a curiosity about
Firefox. It is **an unbounded resource the compositor holds on behalf of a client
that never asks for it back**, and on the deployment this project is for, that
resource is the guest's pinned physical memory.

The measurement, restated so it cannot be read as a sampling artefact:
`wl_shm_pool.destroy` was sent **zero** times, in **every** run, at every length
from 25 seconds to 150. Not "rarely". Not "fewer than the creates". Never. The
same trace shows 39 `wl_region.destroy` and 2 `wl_surface.destroy`, so the client
is destroying objects it is finished with; pools are simply not among them.
Firefox allocates a pool per buffer and keeps every one for the life of the
process. 88 of them, 401 MB, in forty seconds of starting up.

**Why it is worse here than on Linux.** On an ordinary compositor a `wl_shm` pool
is a `memfd` the client owns; the compositor maps it, and the pages are charged
to the client's own address space and its own machine. Nothing about "the client
leaks pools" is the compositor's problem. Cross the hypervisor and every clause
of that changes: the pool's bytes are copied into an arena region that lives in
the **guest**, allocated by the guest's compositor, `VirtualLock`ed so the host
can find the pages (§12.2), and never returned because the only signal that would
return them is a `destroy` that never comes. A client on one machine can pin
memory on another machine without bound, and the machine it pins is the one with
the GPU and the smaller RAM.

**What the compositor should do about it, in the order the evidence supports.**
The instinct is to size the arena for the peak, and that is the one answer the
numbers rule out: 400 MB in forty seconds is a *rate*, not a ceiling, and nothing
has measured what a long session does.

1. **Release on `wl_buffer.destroy`, not on `wl_shm_pool.destroy`.** The protocol
   already allows this and it is the fix with the best evidence behind it: a pool
   is only alive because a buffer in it is, and a client is required to destroy
   its buffers. What is *not* yet measured is whether Firefox destroys buffers
   either — the traces here were taken to enumerate protocol, not to audit
   object lifetime, and the counts above are of `wl_shm_pool.destroy`
   specifically. That measurement is one grep of a trace and it should be the
   first thing the next stage does, because the whole design of a fix depends on
   the answer.
2. **Make the arena's region reusable rather than its size large.** §14 built a
   free list with coalescing for the outbound direction and §14.12 item 2 notes
   it has never run in anger. The inbound direction has the same shape and a much
   higher turnover; a pool that is dropped has to return its extent, or the
   allocator is an arena-shaped bump pointer.
3. **Refuse loudly, and early.** `wc-proxy` already reports overflows
   (`0 overflows` in every run here). An arena that fills should end the
   connection with the number, not fail the next commit — a compositor that stops
   painting because a client would not free a buffer is indistinguishable from a
   compositor that crashed, which is exactly the failure mode §15.8 spent two
   runs diagnosing from the other side.
4. **Do not copy pools that nothing reads.** §9.5's copy is per commit, but the
   *region* is per pool, and 86 of the 88 here are the same 4,608,000 bytes for
   the same surface. A transport that recognised a re-created pool of identical
   size for a surface it already has a region for would turn this from a leak
   into a cache. That is speculative and is written down as speculative.

None of this is built. What is built is the measurement, and the reason it is a
section rather than a line is that **the arena flag is not the fix and sizing it
larger would look like one**.

### 15.12 The evidence

| leg | what it is worth |
|---|---|
| `scripts/wc-firefox.sh enumerate` | the requirement list, from `WAYLAND_DEBUG=1`, against the Linux build. A stock libwayland client and a stock `wayland-server`; nothing Windows-specific can be hiding in it. |
| `scripts/wc-firefox.sh window` | the pixels, counted by ImageMagick from a file the compositor wrote. 406 colours where a missing scene graph gives 2. |
| `scripts/wc-firefox.sh menu` | a right-click at a named point, an `xdg_popup`, a second window, 293 colours in it. `docs/stage9/stage9-firefox-menu.png`. |
| `scripts/wc-firefox.sh wine` | the whole chain, and the only leg that produces an `HWND`. **Wine, so a hypothesis.** |
| `strings` over `libxul.so` and `libgdk-3.so` | the upper bound on the protocol list, and the decoration answer, from the binaries rather than from behaviour. |
| Weston 16 headless, GL renderer | what Firefox binds when a compositor offers everything — a third-party control for the enumeration. |

The `--click X,Y[,BUTTON][,MS]` flag exists because of this stage's shape:
`--simulate-wm` clicks the centre of the window, which in a browser opens
nothing. There is no window manager on the headless platform and no `SendInput`,
so without a way to say *where*, the popup path could only ever be exercised
under Wine — and Wine is a hypothesis. On Windows the flag is accepted and
ignored, with a line saying so, because there a click comes from
`scripts/probe-input.c` and the real message path is the better test.

### 15.13 The gates

| Gate | Result |
|---|---|
| `cargo test --workspace` | **138 tests**, 0 failures (master's 138; this stage adds none — see below) |
| `cargo clippy --workspace --all-targets` (Linux) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `cargo test --manifest-path crates/wc-arena-host/Cargo.toml` | 1 test, 0 failures |
| `cargo clippy --manifest-path crates/wc-arena-host/Cargo.toml --all-targets` | clean |
| `scripts/wc-firefox.sh enumerate` | 13 passed, 0 failed |
| `scripts/wc-firefox.sh window` | 7 passed, 0 failed |
| `scripts/wc-firefox.sh menu` | 6 passed, 0 failed |
| `scripts/wc-firefox.sh wine` | 7 passed, 0 failed |
| `scripts/wc-proxy.sh window` (stage 4, Wine) | 36 passed, 0 failed — unchanged |
| `scripts/wc-input.sh all` (stage 6 + stage 10, Wine) | 82 passed, 0 failed — unchanged |
| `scripts/wc-desktop.sh` (stage 8, the interactive desktop) | **not run — needs the guest** |
| `scripts/wc-guest.sh` (the guest) | **not run — §14.13 item 1 still owns it** |

All of these are from **master's checkout after the merge**, not from the branch
before it, which is §12.12's arrangement and §14.11's. Every one was re-run
there rather than carried over.

**Two assertions in `wc-proxy.sh` had to change, and they are the honest kind of
change.** That harness expected `weston-terminal`'s window to be 806x491, which
is its *buffer*; with the geometry rectangle honoured it is 742x427, and the
dumped frame is now exactly the window rather than 869x555 of buffer. Both old
numbers were assertions that the bug in §13.12 item 1 was still present. They
are updated in place with a comment saying so, rather than relaxed — see the
boxed note in §15.6.

**The test count did not move, and it is worth saying why rather than leaving it
to be read as a gap.** 138 is master's number — 133 through stage 7, plus stage
8's two and stage 10's three — and this stage adds none, which `git diff
master...HEAD | grep -c '^+.*#\[test\]'` reports as `0`. What it changes is two
*existing* assertions, both of which are exact figures precisely so that adding
a protocol is a visible change rather than a silent one (§9.3): 28 interfaces to
32, and 354 walked messages to 376.

Nothing else in this stage is unit-testable in the way the earlier ones were.
"GTK will not make a seat without a data device" is not a property of our code,
and a test asserting it would be asserting our belief about somebody else's;
"Firefox's pixels are in a subsurface" is a fact about a browser that a mock
would simply restate. The harness is where those live, and it runs the real
client — which is why `wc-firefox.sh` has 33 assertions in it and the unit
suite has none.

### 15.14 What is not proven

**One item is not like the others, so it is not on the list.** §14.12 set this
format for exactly this reason and the two boxes are the same shape, which is
not a coincidence — they are the same gap seen from the two ends of the arena.

> **Firefox has never touched the arena. Every run in this stage was
> `shared-file`.**
>
> Not because of a bug: `--transport arena` **refuses the second connection by
> name**, one region has one allocator, and there is no protocol for handing out
> a second arena id (§12.13 item 6). Firefox opens four connections (§15.8). So
> the two are simply incompatible today, and the incompatibility is loud rather
> than silent, which is the only good thing about it.
>
> That means every claim in this section about pixels reaching a Windows window
> is a claim about a **shared file that both sides open by path**. Everything
> above `wc_handle::Handle::map` is identical between the two transports — the
> proxy, the copy, the compositor, smithay's `wl_shm`, the blit — and
> **"identical above the provider" is exactly what stage 5 spent a whole stage
> disproving.** §10.7: every check above the pixels passed, over a transport
> that was not delivering them, for two stages. §14.12's boxed item makes this
> admission for the outbound direction and one stage later; this is the inbound
> one, with a real client on it.
>
> What is specifically unexercised is a Firefox `wl_shm` pool copied into an
> arena region and read by the compositor as three integers of arithmetic. The
> pool direction over an arena *is* measured — `wc-proxy.sh window`'s
> `term-arena` leg, and `wc-guest.sh` block 10 over real guest RAM — but with
> `weston-terminal`, which opens one connection and allocates 15 MB. The client
> that matters opens four and allocated 401 MB in forty seconds (§15.11).
>
> **Multi-connection arena support is the next stage**, and it is not a
> compositor change: it is one arena id per client, which is a `shared_vulkan`
> cold-path handshake question. Nothing here should be read as evidence that
> the deployment's transport works with the deployment's client, because
> nothing here tested that pair.
>
> > **[CLOSED BY STAGE 12 — §17 — and the last sentence of the guess was
> > wrong.]** That pair is tested: Firefox, on the real guest, over an arena
> > that is the guest's own pinned RAM, `mapped: 0 section, 0 shared-file, 3
> > arena-slice`, 46 frames of which 25 distinct, a last frame of 456 colours,
> > and nothing on any filesystem (§17.5). "Not a compositor change" was right.
> > **"One arena id per client" was not**: the id names a *region*, and one per
> > client would have meant a second `attach_guest_ram`, a second 13 GiB sweep
> > and a second plan per connection. What was actually needed is one allocator
> > shared by every session — 60 lines and an `Arc` — because the invariant is
> > one allocator per region, not one client per region, and the refusal this
> > box quotes had confused the two for two stages (§17.2).

§12.13's list, with the rest of this stage's changes:

1. ~~The window has still never been seen.~~ **Closed by §13**, and this stage
   inherits the closure rather than repeating it — but only for the window stage
   8 looked at. **Nothing in §15 has run anywhere but this workstation**: no
   Firefox window, no popup and no `WS_POPUP` has been in front of a human on the
   guest, and §13.6's lesson says that matters more than it sounds. The good news
   is that §15.6's fix is the one §13.7 said the guest's picture needed, so the
   next guest run has something specific to check.
2. **Everything in §15 is Wine or Linux.** In particular the `HWND` sizing, the
   `WS_POPUP` placement and `ClientToScreen` are Wine's implementations of
   them. Popups especially: `WS_EX_NOACTIVATE` and owner-window z-ordering are
   exactly the sort of thing Wine approximates.

   The count of things Wine has actually lied about is **four**, and this stage
   takes that from §13.6 rather than from a handover: that section withdrew the
   `SetProcessDpiAwarenessContext` entry with the measurement attached, and
   §14.12 item 6 explains why eight, seven and four were all in circulation at
   once. The operational rule does not depend on the number and this stage
   inherited a live example of it — §15.8's `X Error: BadWindow` was not Wine
   being wrong about `CreateWindowExW`, it was §8.9's reused desktop name, asked
   in a window station that was going away.
3. **New: nothing here has been typed into.** The seat exists and GTK now builds
   one, and the only input this stage delivered was a single synthetic click on
   the headless platform. §11.10's list is otherwise unchanged.
4. **New: a menu has never been dismissed.** The popup is created, placed, drawn
   and destroyed by the client. Nothing tests what happens when the user clicks
   elsewhere — `xdg_popup.popup_done` is wired to `WM_CLOSE`, which is not the
   message a click outside a menu produces.
5. **New: alpha is still thrown away** (§8.7), and it now matters more. The
   composite is a copy rather than a blend, so a client whose subsurfaces are
   genuinely translucent will show hard edges. Firefox's are not.
6. **New: two clients, one output, no idea what a second monitor does.**
7. **New: nothing frees a pool** (§15.11), which is a resource question rather
   than a correctness one until the transport is an arena, and then it is both.
8. **§12.13 items 3, 4 and 9 are unchanged**; item 5 is now measured (§15.10)
   rather than a guess, without being solved; and **item 7 is closed by §14**,
   which is what lets §15.4's clipboard paragraph end differently than it would
   have a stage ago.

### 15.15 What to do next, in the order the evidence puts it

1. **`DwmExtendFrameIntoClientArea`.** The goal document's hardest clause, and
   the prerequisite §13.7 named is now met: *"the first step is not the DWM call:
   it is honouring the geometry rectangle, because until the compositor knows
   where the client thinks its window is, it cannot decide what to extend."* It
   knows (§15.6). And §15.7 settles the other half — Firefox has already asked
   for client-side decorations through KDE's protocol, so there is nothing to
   negotiate, only room to make. `FrameStyle::extend_top` has been parameterised
   since stage 3 and is still zero; `docs/stage9/stage9-firefox-window.png` with its two
   title bars is the bug report, and it is §8.11 item 7 for the third time.
2. **Make the arena serve more than one connection**, and then run Firefox over
   it. This is the boxed item in §15.14 and it is the only thing on this list
   that changes what the evidence is worth rather than adding to it. One arena id
   per client is a `shared_vulkan` cold-path handshake question rather than a
   compositor one; the compositor's side of it is already a registry keyed by id
   (`wc_handle::arena`).

   It pairs naturally with §14.13 item 1 — that stage's boxed item needs a guest
   run over guest RAM, this one needs a client that opens four connections, and
   a single `wc-guest.sh` run with Firefox on the far end would close both. **The
   guest is not free**: §12.8 exists because an experiment took another agent's,
   and `WC_GUESTNAME` / `WC_HOST_SHARE` are how this is done without repeating
   it.

   > **[CLOSED BY STAGE 12 — §17.]** Both, in one run, exactly as predicted.
   > `scripts/wc-guest.sh firefox` is block 11; it ran as `wctest12` against
   > `/srv/winshare/wc12` while stage 11 had the interactive desktop, and killed
   > nothing of anybody's.
3. **Ask Windows how big the desktop is.** `--output` is a flag with a plausible
   default, which is exactly the shape of the bug §15.3 describes.
4. **Find out whether Firefox's pool cache ever releases.** §15.10. Zero
   `wl_shm_pool.destroy` in every run is either a cache that never shrinks or a
   cache we never put under pressure, and those need different fixes.
5. **`WM_SETICON`.** Stage 8 confirmed the taskbar button exists with the
   generic icon. `set_app_id` is `"firefox"`, arriving and logged; nothing
   resolves it to pixels. `xdg-toplevel-icon-v1` is **not** in either binary's
   strings, so the goal document's "check rather than assume" is answered: it
   will not be handed to us and the app-id route is the only one.

---

## 16. Stage 11: the taskbar shows the client's own icon

### 16.1 Status in one paragraph

**The Windows taskbar carries the client's real icon, and the pixels came from
an icon theme on the other side of a hypervisor.** There is a photograph
(`docs/stage11/stage11-taskbar-icons.png`), a control taken minutes later with
one flag changed (`stage11-taskbar-generic.png`, which is stage 8's generic
icon), and a measurement that does not need an eye: the host's own
`16x16/apps/qemu.png` is found inside the guest's window caption by
`compare -subimage-search` at +16,+7 with an RMSE of 595, against 26741 for the
same search in the control. `docs/goal-firefox.md`'s first clause — *taskbar
icon and title correct* — is closed.

Three things are worth saying before the detail. First, the interesting half of
this was never `WM_SETICON`: it was that `xdg_toplevel.set_app_id` names a
desktop entry in an icon theme that exists only on the *client's* machine, so
something on the Linux side has to resolve it and ship pixels. Second,
`xdg-toplevel-icon-v1` — the protocol that would do this properly — is
implemented here and **cannot** carry this case, for a reason that is a
measurement rather than an opinion (§16.2). Third, the taskbar showed the
generic icon for an entire run *after* both `HICON`s were set successfully, and
the reason was a console window (§16.6).

### 16.2 `xdg-toplevel-icon-v1`, measured in real clients, and why it cannot carry this anyway

`docs/goal-firefox.md` says to *"check rather than assume"* whether Firefox
implements it. Checked, on this machine's store, by looking for the interface
name in the binaries that would contain it:

| binary | `xdg_toplevel_icon_manager_v1` | control: `xdg_wm_base` |
|---|---|---|
| Firefox 151's `libxul.so` | **0** | 2 |
| GTK 3.24.51 and 3.24.52 (`libgdk-3.so`) | **0** | 3 |
| GTK 4.20.3 and 4.22.4 (`libgtk-4.so`) | **present**, both interfaces | 2 |
| weston 15 and 16 (`weston-terminal`) | **0** | 2 |

So the answer is no for everything this project has ever run, and yes for GTK 4
— which matters, because it means the protocol is not theoretical and a
compositor that ignored it would be wrong about a real toolkit. The XML is in
`wayland-protocols` 1.48 and 1.49 under `staging/`, and the vendored smithay
already has a `wayland::xdg_toplevel_icon` module, so implementing it cost about
forty lines. Stage 9 reached the same conclusion from the other direction
(§15.15 item 5) by grepping both of Firefox's binaries.

**But even a client that implements it would not have helped here, and this is
the part worth keeping.** The protocol attaches an icon to an `xdg_toplevel`
*object*. A Wayland object belongs to exactly one client, so only the
application can use it on its own behalf. The resolver in this topology is
wc-proxy, which is a *different* client of the compositor — it cannot touch the
application's objects, and the obvious workaround, injecting requests into the
application's own connection with ids the proxy mints, does not work:

```rust
// wayland-backend/src/rs/map.rs
match id.cmp(&store.len()) {
    Ordering::Greater => Err(()),      // "not the next free id"
```

Client ids must be *the next free one*. There is no high reserved band to take
from — an id above the end of the map is a protocol error — so the only ids a
proxy could inject are exactly the ones the client is about to allocate, and the
client's next `new_id` would then be a duplicate. The alternative is renumbering
every object id in both directions, which puts a rewrite in front of every
message in the protocol; §9.3's whole argument for this proxy being small enough
to trust is that it does *not* understand most of what it carries.

So the icon feed is keyed by **application id** rather than by object, on a
second connection, and `xdg-toplevel-icon-v1` is implemented alongside it for
the clients that do send it. The two are not alternatives: a client that sets
its own icon wins, always, because it is the application saying what it looks
like and the other is a guess made from a string.

### 16.3 What was built, in the order the bytes move

```
weston-terminal / firefox        wc-proxy                    wc-compositor.exe
   (unmodified, on Linux)      (has the icon theme)          (on the guest)
        │ set_app_id ─────────────▶ crates/wc-proxy/icons.rs
        │                            .desktop → Icon= → theme → PNG → BGRA
        │                                │
        │                                │ wc_icon_manager_v1, on a second
        │                                │ connection: chunk* image* publish
        │                                └──────────────────▶ icon::IconStore
        │ commit ──────────────────▶ arena ─────────────────▶ window maps
        │                                                     best_match(16,32)
        │                                                     WM_SETICON ×2
```

Five pieces, and each is where it is because of the boundary:

* **`crates/wc-proxy/src/icons.rs`** — app id to pixels. Desktop entry lookup,
  `Icon=`, an icon-theme search, PNG decode, premultiply. On the host, because
  that is the only machine with `$XDG_DATA_DIRS` on it.
* **`crates/wc-compositor/protocols/wc-icon-v1.xml`** — `wc_icon_manager_v1`:
  `chunk(array)`, `image(w,h)`, `publish(app_id)`, `forget(app_id)`.
* **`crates/wc-proxy/src/iconlink.rs`** — a hand-written Wayland client for
  exactly that interface, which reads its opcodes out of the compositor's own
  copy of the XML with `include_str!`, so the two ends cannot drift.
* **`crates/wc-compositor/src/icon.rs`** — the store, the staging arithmetic,
  buffer-to-image conversion, and `best_match`, which is the choice of which
  image fills which Win32 slot.
* **`platform/win32.rs`** — `set_icon` was written in stage 3 and had never
  run against anything but `--debug-icon`. It now takes a *set*.

### 16.4 The resolver, and the three things it does not do

The lookup is the XDG icon-theme specification's, implemented to the depth that
finds what real themes actually contain:

1. `$XDG_DATA_HOME`, then `$XDG_DATA_DIRS`, in order.
2. `<app_id>.desktop`, then the lower-cased name, then — and this is the one a
   panel actually relies on — any entry whose `StartupWMClass` is this app id.
3. `Icon=` from the `[Desktop Entry]` group **only**. Firefox's own desktop file
   has three further groups with their own `Icon=` keys, and a parser that took
   the first match in the file would give a window the "New Private Window"
   icon on some machines and the right one on others.
4. Inside a theme: the directory tree is walked and the size is taken from
   whichever path component looks like one, so `48x48/apps/firefox.png` and
   `apps/48/firefox.png` are both found. `index.theme` is read only for
   `Inherits`; `hicolor` is always last, as the specification requires.
5. `pixmaps/` as a final resort, treated as unsized.

Three sizes are shipped — 16, 32 and 48 — because Windows asks for two
(`SM_CXSMICON`, `SM_CXICON`) and a theme has both *drawn* rather than resampled.
Scaling one image at the far end would throw away information we were given for
free.

**Not implemented, and each is a real limit:**

* **SVG.** A scalable icon needs a renderer. A theme with only SVG resolves to
  nothing and degrades to the generic icon. Adwaita on this machine is largely
  SVG, which is exactly the sort of thing that makes a feature look broken on
  somebody else's machine, so it is recorded here rather than discovered there.
* **The icon *name* half of `xdg-toplevel-icon-v1`.** A client may send a name
  instead of buffers; the compositor has no theme to resolve it against. It is
  looked up in the published store instead (an application's icon name is
  usually its app id) and says so in the log when that misses.
* **A cache across runs.** Each proxy process resolves each app id once.

The cost, measured on the host: **3.4 to 6.3 ms** for an app id that resolves,
and **112 ms** for one that does not — the miss is expensive because it scans
every `.desktop` file in every data directory looking for a `StartupWMClass`.
Once per app id per connection, off the frame path, several round trips before
the window is ever shown.

### 16.5 What crosses, and why it is an array rather than a `wl_buffer`

Icon pixels are **data on the wire**: a `wl_array` argument, in chunks. Not a
`wl_shm` buffer, which is what `xdg-toplevel-icon-v1` uses and what the pool
machinery in this proxy already knows how to make.

The rule (§9.4) is that no descriptor crosses, and a `wl_shm` pool would not
have broken it — the proxy would have named a region the way it names every
other one. It would have put an `h`-shaped argument on a wire for a few
kilobytes sent once per application, to save a copy nobody can measure, and it
would have made the icon path depend on the pool allocator. An array is a
`memcpy` into a message.

The chunk size is **3072 bytes**, and that number is a measurement of the far
end rather than a preference: `wayland-backend`'s receive buffer is
`DEFAULT_MAX_BUFFER_SIZE = 4096` (`rs/mod.rs`), a message that does not fit in
it cannot be read at all, and a 48x48 BGRA image is 9216 bytes. So an icon is
`chunk chunk chunk image`, repeated per size, then `publish`.

Two conventions travel with the bytes and both are silent if wrong. The layout
is BGRA, which is what `wl_shm`'s ARGB8888, this compositor's shadow buffers and
a 32-bit `BI_RGB` DIB all already are (§8.4). The alpha is **premultiplied**,
which is what `wl_shm` specifies and what `AlphaBlend` — which is how the shell
composites a 32-bit icon — is defined on; PNG's is straight, so the resolver
multiplies through at decode. Getting that wrong is a pale halo round the icon
rather than an error, so it is asserted in a unit test rather than left to a
photograph.

### 16.6 Both `HICON`s were set, and the taskbar showed the generic icon anyway

This is the finding of the stage, it cost a run, and no log on either side could
have shown it.

The compositor's own account was unambiguous — `icon big <- 32x32`,
`icon small <- 16x16`, `icon set: 2 of 2 slots` — and the window's caption
carried the icon in the photograph. The taskbar button did not: it had Windows'
generic application icon on it, which is precisely the picture stage 8 took when
nothing called `WM_SETICON` at all.

The cause is that **the shell groups taskbar buttons by process, and a console
window is a window.** The compositor is started by a scheduled task (§13.3),
which gives it a console; the shell put the console window and the toplevel
under one button; and the icon on a *grouped* button comes from the executable
rather than from either window's `WM_SETICON`. `wc-compositor.exe` has no icon
resource, so the group got `IDI_APPLICATION`.

The fix is three lines and one guard: hide the console window, but only if
`GetConsoleProcessList` says this process is the only one attached to it. A
console shared with a `cmd.exe` or an ssh session belongs to that other process
and hiding it would take away somebody's terminal; a console with exactly one
process attached was created for this process and dies with it. It is not
tidiness — it is the difference between the taskbar showing the client's icon
and showing the generic one.

**Windows had not lied. Nothing had.** Every call succeeded and every log line
was true; the icon was on one of the process's two windows and the shell was
showing the other one's group. This is the same shape as §13.7 — the mistake was
not in the operation, it was in which of two things the answer was about — and
the only instrument that shows it is a picture of the taskbar.

### 16.7 What happens when the app id resolves to nothing

Deliberately the same length of code path as success, because a missing icon is
the *common* case and it must not degrade to a blank button, a crash or a
silence.

* A client with **no app id at all** — `weston-presentation-shm` sets none,
  measured — gets an empty set and a log line saying nothing can be resolved for
  it.
* An app id with **no desktop entry and no theme file** — `weston-terminal`'s
  own `org.freedesktop.weston.wayland-terminal` on a machine without a weston
  desktop entry, which is every machine, because weston installs none — resolves
  to zero images, and the proxy publishes *that*, which the compositor stores as
  a negative answer so the next window with the same app id costs nothing.
* An empty set reaching the platform destroys both `HICON`s and sets neither, so
  the window falls back to its **class icon**, which is `IDI_APPLICATION` — the
  generic one. That is what `stage11-taskbar-generic.png` is a photograph of.
* A compositor with no `wc_icon_manager_v1` global — an older build — makes the
  proxy's second connection bind nothing; it says so once and stops resolving.
  The `wl_display.sync` that follows `get_registry` is what makes "the manager
  has not arrived yet" distinguishable from "this compositor has no manager".
* A publisher that miscounts its bytes gets a protocol error on the *icon*
  connection, which is the one real advantage of the feed being a separate
  client: it costs an icon rather than a window.

`weston-terminal`'s app id is worth recording on its own, because the obvious
guess is wrong and the harness was written against the guess: it is
**`org.freedesktop.weston.wayland-terminal`**, not `weston-terminal`.

### 16.8 Four things that cost a run each, all of them about sharing a guest

§13.9 said an image name is not an identity on this guest. Stage 11 found two
more ways for that to be true and two ways to photograph the wrong instant.

> **The rule, because this is the third time and it will not be the last.**
>
> There is one Windows guest, one `/srv/winshare`, one taskbar, and as many
> experiments as there are worktrees. Everything below is a special case of two
> sentences, and the next person should meet them here rather than derive them
> again:
>
> 1. **An identity is a process id or a full path. Never an image name, never a
>    window class, never a glob.** Every experiment deploys its own copy into
>    its own directory, so the path is unique and the image name is not;
>    `WaycomposeToplevel` is registered by every compositor in the tree; and
>    `rm -f …/wg-*` matches somebody else's live sockets as happily as your dead
>    ones.
> 2. **A "polite" close is still a kill.** `taskkill /IM foo.exe` without `/F`
>    does not terminate anything — it sends `WM_CLOSE` to every window of every
>    matching process, which for a compositor means `xdg_toplevel.close`, a
>    client that exits correctly, and a window that is simply not there any
>    more. Nothing in either log says "somebody killed me"; the compositor's
>    side reads `close requested`, which is what a user clicking the X looks
>    like. Kills-by-name are not made safe by being gentle.
>
> The three instances so far: §12.8's image-name kill, which stage 8 narrowed to
> an image *path* (§13.9); this stage's `WM_CLOSE` (below); and stage 12's
> `clean` step running an unscoped `rm -f …/wg-*` over eight live sockets
> belonging to this stage's runs. Each was diagnosed as flakiness first, and
> each cost more than the fix.

* **`taskkill /IM wc-compositor.exe` without `/F` sends `WM_CLOSE`.** Another
  worktree running an older `wc-guest.sh` reaps nothing of ours — our process
  survives — but our *window* closes, the compositor correctly sends
  `xdg_toplevel.close`, the client exits politely, and the photograph is of a
  taskbar with no button on it. The compositor's own log says `window 1 close
  requested` with nothing on this side having asked. This stage now deploys the
  compositor under its own image name, `wc-s11-compositor.exe`, which no other
  harness kills. It happened again afterwards, so the rename is not a complete
  defence and the harness retries a pass whose window did not survive.
* **A scheduled task costs about a minute, and the first version of the harness
  did not know that.** It held the client for 60 seconds, photographed the
  window inside that, and then took the taskbar's picture two tasks later — by
  which time the window had been gone for a minute. The result was a taskbar
  with the right number of buttons and none of them ours, which reads exactly
  like "Windows refused the icon". The client is now held for 210 seconds and
  every picture is taken by one task.
* **A shot of a window that is not there leaves the previous shot's file.** The
  harness fetched it and reported a photograph it had not taken; the two passes'
  images were byte-identical and the comparison between them said the icon was
  in both. The output files are deleted on the guest before every attempt now.
* **Two runs on one TCP port**, which is §13.10's lesson arriving unchanged: the
  second run's relay listener sits in `TIME_WAIT`, the client never connects, and
  what the harness prints is the *client's* "Failed to process Wayland
  connection: Broken pipe" — which reads as the compositor having gone away. The
  port is derived from the run's pid now.

A fifth, smaller: registering two scheduled tasks with the same name in one run
leaves `Start-ScheduledTask` with nothing to start and no error, which printed
as "the compositor did not start" for the second pass only — with the
compositor's own log on the guest proving that it had.

**And the other half of the rule, which is about what a run leaves behind.**
While this stage was running, the guest was found to be holding **six abandoned
`wc-compositor.exe` processes started between 06:55 and 07:28**, still alive
seven hours later, each with a window on the desktop — after a stage had
reported that nothing of its own was running. That is the second time a cleanup
claim has turned out to be false rather than the first; §13.9's fourth lesson is
the same shape, where a compositor left over from an interrupted run kept
writing frames into a directory the next run had just emptied.

The mechanism is not mysterious: a scheduled task's process does not die with
the ssh session that started it (§13.3), so *every* path out of a harness that
is not an explicit kill leaves one behind — an interrupted run, a timeout, a
failed assertion that returns early, a `set -e` on a line before the teardown.

So the check has to be **"count the processes"** rather than **"the script
reached its cleanup block"**. The two are not the same statement, they differ in
exactly the cases anybody cares about, and only one of them is a measurement.
A harness that means to say the guest is clean should enumerate
`Win32_Process` and say how many it found — which is one line of the PowerShell
each of these scripts already ships — rather than infer it from having run some
code.

### 16.9 The evidence

`docs/stage11/README.md` is the exhibit list. The two that carry the claim:

| | |
|---|---|
| `stage11-taskbar-icons.png` | the guest's taskbar, our window's button, the host's QEMU icon on it |
| `stage11-taskbar-generic.png` | the same run with `--no-icons`: the same button, the generic icon |

The icon is QEMU's rather than Firefox's **on purpose**, and it is about the
measurement rather than about taste: the guest's taskbar has Firefox pinned to
it, so a search for the Firefox icon in a taskbar capture finds one whether or
not any of this works. The QEMU icon is in the host's theme, is not in the
guest's taskbar and is not in Windows at all, so finding it there is only
possible if it crossed.

The client is `weston-terminal`, unmodified, on the far side of a hypervisor —
and because weston ships no desktop entry for it, the harness writes a
three-line one naming an icon that the *host's own theme* holds. What is
synthetic is that file; the theme search, the size choice, the decode, the
premultiply, the chunking and `WM_SETICON` are all the real path.

**And then the goal's own client, with nothing synthetic in it at all.**
`stage11-taskbar-icons-firefox.png` is Firefox on the Linux host, through the
proxy, in a window made by `wc-compositor.exe`, with **Firefox's icon on its
taskbar button** — resolved from the `firefox.desktop` and the
`hicolor/32x32/apps/firefox.png` that the Firefox package on the host installs,
with no file written by this harness (`WC_ICON_NAME=` empty refuses to write
one). Two things about that run are worth keeping:

* It needs `--transport shared-file`, because Firefox opens two connections and
  the arena serves one (§12.13 item 6, §15.14's boxed item). The icon path does
  not care which transport carries the pixels — it does not use one — but the
  window will not appear without it.
* The window in the picture is Firefox's *"Profile Missing"* dialog, because the
  harness named a profile directory that did not exist. That is a fault in a
  command line and not in anything below it: the window, the caption, the button
  and the icon are all real, and the app id Firefox set on the way to
  complaining is the same `"firefox"` stage 9 measured.

### 16.10 The gates

| Gate | Result |
|---|---|
| `cargo test --workspace` | **154 tests**, 0 failures (138 on master, +16 here) |
| `cargo clippy --workspace --all-targets` (Linux) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `scripts/wc-icon.sh deploy` | builds and copies |
| `scripts/wc-icon.sh taskbar` (`weston-terminal`, the guest's desktop) | 12 passed, 0 failed |
| `scripts/wc-icon.sh taskbar` (Firefox, `--transport shared-file`) | 7 passed, 0 failed, 1 skipped |
| `scripts/wc-icon.sh` teardown check — `Win32_Process` count on the guest | 0 left behind |
| `cargo test --manifest-path crates/wc-arena-host/Cargo.toml` | 1 test, 0 failures |

The three `cargo` rows were **re-run from master's checkout after the merge**,
which is §12.12's arrangement and §14.11's and §15.13's; the guest rows are from
the branch, because the guest is one machine and re-running them would be the
same experiment against the same binaries. 154 is 138 plus this stage's 16 —
`git diff 1ab9f80..HEAD | grep -c '^+.*#\[test\]'` reports 16, and no existing
assertion changed, which is the difference between this merge and stage 9's.

The skip is the caption comparison in the Firefox run, which wants a 16x16
reference file to search for and is looking in the Firefox package's own theme;
it is a harness gap rather than a result, and the taskbar picture from that run
is the one that matters.

The sixteen tests are the parts that are arithmetic rather than Win32 or
filesystem: `best_match`'s preference for scaling down over scaling up, the
staging arithmetic that splits one run of chunks into several images, the
strided-buffer repack, the opaque-format alpha fix, the desktop-entry group
parser, the size-from-path rule, the theme chain, a path-traversal refusal, and
an end-to-end resolution against a theme built in a temporary directory — which
asserts the premultiply on a two-pixel PNG written by hand, because that is the
conversion whose failure is a halo rather than an error.

What is *not* a unit test is the whole of §16.6, and could not be: the icon was
set correctly on a window whose button the shell was not drawing.

### 16.11 What is not proven

1. **The taskbar claim rests on one photograph and its control.** The window
   caption's copy of the same icon is measured — `subimage-search`, RMSE 595
   against 26741 — and the taskbar's is not, because a taskbar button draws the
   icon scaled to 24 px and the search finds nothing at that size. A crop
   comparison against the button rectangle would close it and needs the button's
   position, which nothing here knows.
2. **`xdg-toplevel-icon-v1` is implemented and has never been exercised by a
   client.** No client this project can run implements it (§16.2), and the only
   thing that has driven the code path is the compositor's own `--debug-icon`.
   The right test is a GTK 4.20 application, which needs `wl_output`,
   `wl_subcompositor` and a data device — all of which stage 9 added — and one
   was not run here.
3. **Nothing has resolved an icon on the machine the goal describes.** The
   resolver reads the *host's* theme, which is right; but on this rig the host
   is the VM host rather than a general Linux desktop, and its theme has two
   application icons in it. A machine with Adwaita and SVG-only icons will
   resolve less than this one did, and §16.4 says so rather than finding out
   later.
4. **The icon is never updated after the window is mapped, except by a
   publish.** A client that changes its app id gets a fresh lookup; a client
   whose *theme* changes does not, and neither does a window whose DPI changes,
   which is where `SM_CXICON` would move.
5. **`forget` is implemented and unused.** Nothing in the proxy calls it.
6. **The second connection is a second client to the compositor**, so a
   compositor that counted clients or exited with `--once` when the last one
   left now sees two. `--once` is a harness flag and the change is benign, but
   it is a change.
7. **Any client can bind `wc_icon_manager_v1` and publish an icon for any app
   id.** There is no notion of who is entitled to say what an application looks
   like. That is the same trust model as the rest of this compositor (§2.6) and
   it is worth naming rather than assuming.

### 16.12 What to do next

1. **Give the compositor's window an `AppUserModelID`.** §16.6 is a symptom of
   the shell deciding what our window belongs to by looking at the process. A
   per-window `SetCurrentProcessExplicitAppUserModelID` — or the per-window
   property — is how a Windows application controls grouping, and it is what a
   compositor showing several clients' windows will need anyway: two toplevels
   from two different applications should not share a taskbar button, and today
   they would.
2. **Run a GTK 4 client** and close §16.11 item 2. The compositor advertises
   `xdg_toplevel_icon_manager_v1` with the sizes it wants; GTK 4.20 binds it; the
   whole path is written and untested against anything but ourselves.
3. **Resolve icon *names*, not just app ids.** A client that sends
   `xdg_toplevel_icon_v1.set_name` is asking for a theme lookup on the
   compositor's side, which is the one thing the compositor cannot do. The proxy
   could answer it — it is the same resolver, keyed by the same string — if the
   compositor could ask, which needs a request in the other direction.
4. **Cache resolutions across proxy runs**, or don't, but measure the 112 ms
   miss first: it is a full scan of every desktop file, and it happens once per
   app id that has no icon, which on a busy desktop is most of them.

---

## 17. Stage 12: Firefox, over the guest's own RAM

### 17.1 Status in one paragraph

Written 2026-08-13, branched from master after stage 9's merge and **merged after
stage 11** — so §16 is stage 11's and this is §17, and every gate below was
re-run from master's checkout after that merge rather than carried over from the
branch, which is the arrangement §12.12, §14.11 and §15.13 all used.
`docs/stage12/stage12-firefox-guest.png` is the stage: Firefox,
unmodified, drawing its tab strip and URL bar into an `HWND` made by
`wc-compositor.exe` **on the real guest**, from pixels that crossed as
`(arena, offset, len)` into the guest's own pinned RAM. The transport line is
`mapped: 0 section, 0 shared-file, 3 arena-slice`, the compositor was started
with **no `--shared-dir`**, and `scripts/wc-guest.sh firefox` is **16 passed, 0
failed** on real Windows. Both boxed items are closed: §14.12's, because the
compositor-side allocator has now run over guest RAM rather than over a file,
and §15.14's, because Firefox has now run over an arena rather than over a
shared file. They were the same gap and one run closed it, exactly as §15.15
item 2 predicted.

Getting there cost two things and only one of them was the advertised work.
The advertised one is 60 lines: an arena is now a **handle on one region and one
free list**, and every connection gets one. The other was a claim in this
document being wrong. §14.12 item 1 said the ordering between a gift and the
host's locate "holds by construction"; the first guest run of an outbound window
reported **`located 16374 of 16384 pages`** and wrote no plan, because smithay
seals the keymap inside `Seat::add_keyboard`, during start-up, a minute before
any client exists. Ten pages of stamps, which is exactly a 37532-byte `de`
keymap rounded up.

### 17.2 What the second connection actually needed, and why it is not a second arena

§15.14 named the next stage and guessed its shape: *"Multi-connection arena
support is the next stage, and it is not a compositor change: it is one arena id
per client, which is a `shared_vulkan` cold-path handshake question."* **The
first half is right and the second half is wrong**, and the difference is worth
stating because the wrong version is three orders of magnitude more work.

The refusal it replaces was this, in `run.rs`:

> the arena is already serving a client, and there is one arena (§12.13 item 6).
> Use `--transport shared-file` for a client that opens more than one connection.

with a comment saying a silent share "would give two clients overlapping offsets
into the same guest pages". **The premise is right and the conclusion does not
follow.** What must not be duplicated is the *allocator*. Two allocators over one
region hand out the same offsets; one allocator serving four connections is the
same operation as one allocator serving one connection's four pools, which it
has done since stage 4. The one-client rule was never a property of the region —
it was a property of `Box<dyn PoolTransport>` being moved into a session's
thread, and it had been read as a protocol limitation for two stages.

So an `Arena` is now `Arc<ArenaInner>`: the region, the origin string, the
outbound window, and one `Mutex<Alloc>` holding the free list, the live map and
the counters. `Arena::share()` clones the `Arc`; the accept loop calls it once
per client. Three consequences worth naming:

* **The arena id names a region, not a client.** One id per client would have
  meant one *region* per client: a second `attach_guest_ram` inside the guest, a
  second 13 GiB sweep on the host at 2.4 s each (§12.5), a second plan and a
  second stitch — four times over, for one Firefox, to avoid a mutex. And the
  four regions could not share a byte, so Firefox's 14 MB would have had to be
  four separate arenas sized for the peak of each.
* **The lock is not on the frame path.** `bytes_mut` takes it for a `HashMap`
  lookup and drops it before the copy. A commit is 414 µs of `memcpy` on the
  guest run below; four connections holding one mutex across that would
  serialise every client's frames against every other's, which would be a real
  cost of sharing and there is no reason to pay it.
* **The transport is built once, before the accept loop**, so a plan that cannot
  be read is now a startup failure with the message on stderr rather than the
  first client being told "no transport for it" twenty seconds later.

`reserve_outbound` gained a third refusal and the type does the work: it takes
`&mut self` and reaches the region through `Arc::get_mut`, so **the refcount is
"before the first client"**. A window cannot be moved under a session that
already holds the arena, and that is checked at the only moment it could go
wrong rather than asserted in a doc comment.

**Stage 11's second connection is not one of these, and the counter had to be
checked rather than assumed.** §16 added `wc_icon_manager_v1`, carried on a
connection the proxy opens **to the compositor**, one per served session. It
meets this stage head on and the two turn out to be orthogonal, for a reason
worth stating rather than a coincidence: an `IconLink` is a raw upstream socket
with no `PoolTransport` in it at all — icon pixels are `wl_array` chunks in the
message body, so there is nothing to allocate, nothing to publish and no
`Descriptor`. It never calls `make_transport` and never joins the region, which
is right: **a second region for a feed that has no pixels to place would be a
sweep and a plan for nothing.**

That does change what one number means, so it is said here rather than left to
be discovered. `served == joined` in the harness counts accepts on the proxy's
**listening** socket against `joins arena` lines, and every accept still gets
exactly one handle, so the assertion is unchanged and still means what it says.
What is no longer true is the sentence a reader might infer from it — that the
compositor sees as many clients as the proxy serves. It now sees twice that: one
upstream per session plus one icon feed per session, and the merged guest run
counted **3 accepts on the proxy, 3 `joins arena` lines, and 6 clients
initialised in the compositor**. Nothing depends on the smaller number, and
§17.4's release of held gifts fires on the first of *either* kind, which is
still after the locate because both are opened by a proxy that a plan started.

Two smaller things the merged run measured, both of them stage 11's rather than
this stage's, and neither a regression. `wc_icon_manager_v1` **was** bound over
the arena run — `a client bound wc_icon_manager_v1 (an icon resolver)` — and it
published `0 image(s)` for `"firefox"`, because the resolver runs where the proxy
runs and the VM host has no icon theme carrying Firefox's pixels; the window
kept the generic icon, which is §16.7's designed outcome and is why the feature
costs a picture rather than a window. And the first run of the merged tree on the
guest used a **stale `wc-compositor.exe`** — `cargo clippy --target
x86_64-pc-windows-gnu` builds metadata and not an executable, so a gate that was
green did not mean the deployed binary was the merged one. It presented as
`this compositor has no wc_icon_manager_v1` in the proxy's log, which is a true
sentence about the wrong binary. The numbers quoted here are from the run after
`cargo build --target x86_64-pc-windows-gnu`.

### 17.3 Two connections writing one region, and the check that replaces an argument

The region is a `MappingMut` or a `StitchedRegion`, and `wc_handle` says of the
first: *"a read-only view can be shared freely, a writable one is exclusive to
whoever holds the `&mut`"*. That rule is right for one owner and it is the wrong
shape for four connections, so it is **replaced by a narrower one in
`wc_proxy::pool` rather than widened in `wc_handle`**, where a `MappingMut` still
means what it always meant.

The narrower rule is that the only writable references handed out are extents
the allocator has handed out, and the allocator never overlaps two of them — so
the `&mut [u8]`s alive at any moment are disjoint sub-slices of one allocation,
which is what `split_at_mut` produces and is sound for the same reason. That is
one `unsafe` block with the allocator's invariant as its precondition, and
`four_connections_share_one_arena_without_overlapping` is the test: four threads,
32 slices, each filled with its own letter, asserted disjoint *and* asserted to
still hold one letter each afterwards. The second assertion is the one that
matters — **overlapping offsets that nobody writes look exactly like working
code**.

The read side got narrower rather than argued. `PoolTransport::peer_slice` used
to return any range of the region and a comment explained why that was all
right; it now **refuses anything outside the compositor's outbound window**. With
one session the difference was invisible. With four it is the difference between
reading a keymap and reading another client's buffer while it is being copied
into, so the disjointness of the reads and the writes is now a bounds check
against the range `reserve_outbound` removed from the free list, not a sentence.
Anything else a compositor describes is either a misconfiguration or one client's
pixels being handed to another, and both are refused with the numbers in the
message.

### 17.4 §14.12 item 1 was wrong, and the first guest run of it said so

This is the finding of the stage, and it is a finding about this document.

§14.12 item 1, on the ordering between a gift and the host's locate:

> The order holds in every topology that exists — the host cannot proxy a client
> it has not connected for, and the compositor cannot mint a keymap before a
> client binds a seat — but "holds by construction" is what §12.10 said about a
> second locate before measuring it, and it deserves the same treatment.

It deserved the same treatment and it got it, one stage later, and it failed:

```
S4-GUESTARENA: arena 0 provided from guest RAM: run 0x1b44…, 67108864 bytes, 16384 pages, locked=true
wc-arena-host: io: located 16374 of 16384 pages after sweeping 13312 MiB
S4-FAIL: wc-arena-host did not locate the arena
```

**The second clause is false.** smithay compiles and *seals* the keymap in
`Seat::add_keyboard`, which this compositor calls while it is starting up — the
log line `seat0 keyboard: layout de (73017 bytes of keymap)` is printed before
`listening on …`. So the keymap is minted before the socket is bound, let alone
bound to. Serialised it is 37532 bytes, which the gift allocator rounds to ten
pages, and ten is exactly what the sweep was short. §14.12 predicted the number
would look like `located 16366 of 16384`; it looked like `16374`, and everything
about the shape was right except that it happens with no client at all.

**Why deferring is the fix and not re-ordering something.** Every page of a
guest-RAM arena has to carry its stamp when the host sweeps, because the sweep is
a *content* search: a page whose stamp is gone has no known guest-physical
address, and the stitched view would have a `PROT_NONE` hole exactly where the
compositor's objects live. Nothing may write into the region until the sweep is
done, and no ordering of the compositor's own start-up achieves that — **the
compositor cannot know when a host it never talks to has finished.**

What it can know is that a client has connected, and that is sufficient and
provably so: the client arrived through a proxy, the proxy was started from a
plan, and the plan exists only if the locate succeeded. It is also the earliest
moment anything could want the bytes, because nothing inside the guest ever reads
a gift. So `arena::defer_gifts` holds every gift's bytes in memory, keyed by its
arena offset, and `arena::release_gifts` — called from the accept loop — writes
them in and says so:

```
wc-compositor: arena 0 objects made for clients are held in memory until the first
               client connects, so that the host can still find every page's stamp
wc-compositor: client 1 released 1 held object(s), 37532 bytes, into the arena
```

Three details that are not decoration. It is asked for rather than automatic,
because a file arena has no stamps and nothing looking for it, so
`--arena-provide` writes its keymap straight through and the Wine leg is
unchanged. `Gift::write_at`'s deferred path takes **only** the outbound lock:
`write_provided` takes the arena registry's read lock and `reserve_outbound`
takes them in the other order, and two locks in two orders is a deadlock that
would present as a compositor that accepted a client and then stopped. And a
`Gift` dropped while deferred takes its buffer with it, or the next gift at the
same offset would inherit somebody else's bytes.

The client that released the keymap on the guest run is **client 1 — Firefox's
throwaway probe connection**, the one §15.8 spent two runs diagnosing. It opens
a connection, binds nothing, and closes it. It is now the thing that tells the
compositor the host has finished looking.

### 17.5 What Firefox did on the guest

`scripts/wc-guest.sh firefox` is block 11 and it is `s4_run` with the client
swapped, plus four assertions that are only interesting because the client is
this one. 60 seconds, `--copy damage`, a 64 MiB arena with a 1 MiB outbound
window, `--output 1600x1000` so the window size is comparable with §15.6's.

Five runs, and the ranges are across all of them rather than one run's numbers
dressed as a constant — §12.5's discipline, and the run count of the stitch is
the same property of the guest's free-page state it was there.

| | |
|---|---|
| locate | **16384/16384 pages**, every run; 2.40 s sweep of 13312 MiB, 4 other-run stamps, **0 outside guest RAM** |
| stitch | 1107–1469 runs, 3.15–4.07 ms |
| connections | **3**, all three on one region and one allocator, every run |
| transport | **`mapped: 0 section, 0 shared-file, 3 arena-slice`**, every run |
| descriptors | `6–7 fds in, 2 out (2 from the arena)` |
| the export's `keymaps` directory | **empty**; `--shared-dir` was not passed |
| pools | 6–7 (2 released, 3 resizes, **0 overflows**), largest 5,072,256 |
| copies | 269 commits, 262 copies, **739–744 MiB in 102–108 ms** — 2889–2906 KiB and **388–414 µs** per copy |
| frames | **44–48 presented, all dumped, 23–25 distinct, the commonest 10–13** |
| the last frame | 1028x749, **456 distinct colours**, chrome `srgb(235,235,237)` against content `srgb(255,255,255)` |

`docs/stage12/` is the last of those runs, whole: the checker's output, the
compositor's own log from inside the VM, and the frame.

Four things in that table are worth more than the row they are on.

**The window is the geometry rectangle and then the guest argues with it.** The
compositor mapped `1280x900` — §15.6's number, on real Windows for the first
time — and then reported `window 1044x788 client 1028x749` and sent
`configure 1028x749`, because the guest's primary screen is 1024x768 and a
1280x900 client area does not fit on it. Firefox took the configure and redrew,
which is why the dumped frame is 1028x749 rather than 1280x900. That is the
resize round trip working on the deployment, and it is also §15.15 item 3
arriving as a fact: `--output` is a flag with a plausible default and the desktop
under it is a different size.

**Two keymaps were live in one window at once**, which is §14.12 item 3 — "one
outbound window, one client, one object at a time … nothing has yet asked for two
objects to be live at once outside a unit test". Firefox binds `wl_seat` twice,
GTK's at v5 and Gecko's at v8 (§15.2), and each bind gets a keymap. Measured, both
binds are on **one** connection rather than one each, which is not what §15.2's
"Firefox opens two Wayland connections that matter … GTK's `GdkDisplay` is one
client and Gecko's `nsWaylandDisplay` is another" reads like: the connection that
carries every pixel is a single one, `client 2`, with 269 commits and both seats
on it. The other two connections bind nothing and close.

**The free list ran in anger**, which is §14.12 item 2's other half. `pool 43`
was published at `arena 0 slice 28672+3460320` — offset 28672 is where `pool 37`
had been, and it was reused after that pool was released and its extent
coalesced. That path had been unit-tested and never run.

**Three connections, not four.** §15.8 counted four on this workstation and the
Wine leg below counted four again; this run counted three. Both numbers are
Firefox's, taken 25 minutes apart on two machines, and the stage does not claim to
know why they differ — what it claims is that the arena served every one of them,
which is the thing that could not happen before.

### 17.6 The Wine leg, which is the same run with two flags changed

`scripts/wc-firefox.sh wine-arena` is `wine` with `--transport arena` and
`--arena-provide` instead of `--shared-dir`, **and nothing else** — the same
Firefox, the same Xvfb, the same assertions — because the claim is that the
transport changed and nothing did, and §14.6 set that discipline for
`weston-eventdemo`.

| | shared-file (§15.8) | arena (this stage) |
|---|---|---|
| connections served | 4 | 4, all on one region |
| transport line | `0 section, 4 shared-file, 0 arena-slice` | **`0 section, 0 shared-file, 3–4 arena-slice`** |
| descriptors from the arena | 0 | **2** |
| files on any filesystem | 4 pools + 1 keymap | **0** |
| frames | 26 | 26–30 |
| copies | 4503.3 KiB / 131 µs | 4503.3 KiB / 131 µs |
| checks | 7 passed | **10 passed, 0 failed** |

It is a hypothesis and it is here for one reason: it found every bug in the
multi-connection code before any of them cost a guest run. **What it could not
find is §17.4**, because a file arena has no stamps, which is §14.12's boxed item
being exactly right about what a file substrate cannot tell you.

### 17.7 Four things worth writing down, three of which cost real time

**Firefox will not run as root while somebody else owns its directories.** The
host-side half runs in a root transient unit, because `/proc/<qemu-pid>/fd` is
root-only (§12.9), and Firefox refuses with
`Running Firefox as root in a regular user's session is not supported` — first
about `$HOME`, and then, once that was fixed, about `$XDG_RUNTIME_DIR`. It is a
real check against a real mistake and the answer is to give the run its own
directories rather than to argue with it: a root-owned home, a root-owned
`$RUN/xdg`, and the proxy's socket reached by **absolute path** in
`WAYLAND_DISPLAY`, which libwayland accepts and which is the whole reason the
runtime directory and the socket can be separated. Dropping to the invoking user
for the client alone was the alternative and it is worse: it would put the
socket and its client on two sides of a permission boundary for no gain.

**`located 16374 of 16384` is not a hardware problem and reads like one.** It is
§17.4, and the reason it took a run to find rather than a reading is that the
harness had been *written* for the outbound window since stage 10 and had never
been *run* — §14.12's box says so in as many words. The failure is loud, which is
§12.10's design working: no plan is written, so nothing downstream can use a
stale one.

**The frames go on the virtio-fs export.** A Firefox frame is a 2.3 MB PPM and a
long run makes hundreds of them, on the mount §10.3 measured at 330× slower to
touch. 48 of them is 110 MB and fine; a stage that raises the run length should
look at that number before it does.

**And one that cost nobody anything yet: `wc-guest.sh clean` ran an unscoped
`rm -f …/AppData/Local/waycompose/wg-*`.** Every experiment derived from this
script names its sockets `wg-…`, so a clean would have deleted another run's
**live** socket node, and the symptom would have landed in their log as a
compositor that cannot bind. It was found by reading the function before running
it, and the evidence that it was real is in the directory: at the moment of this
stage's clean, eight `wg-s11-*` nodes belonging to stage 11 were sitting in it.
It is now scoped exactly as stage 7 scoped the kill — the sockets are swept only
when `WC_GUESTNAME` is the default, i.e. when this deploy owns the whole guest —
and the `wctest12` directory, which *is* scoped, is killed by path and removed as
before.

**The rule this is an instance of lives in §16.8's box and is not restated
here.** Stage 11 wrote it down because this was the third time — "an identity is
a process id or a full path, never an image name, never a window class, never a
glob" — and counts this one among the three. The only thing worth adding from
this end is where it was hiding: stage 7 narrowed the *kill* five lines above and
nobody read the *clean*, so a fix that was already understood did not travel the
length of one function.

### 17.8 Nothing frees a pool, and it is guest RAM now — which is this stage's consequence

§15.11 is a section rather than a line because Firefox creates a `wl_shm` pool
per buffer and destroys none: **zero `wl_shm_pool.destroy` in every run, at every
length from 25 seconds to 150**. It ends by saying the resource in question would
be the guest's pinned physical memory *once the transport is an arena*, and by
listing four things to do about it "in the order the evidence supports", with
item 1 — release on `wl_buffer.destroy` instead — as the fix with the best
evidence behind it.

**This is the stage that made the transport an arena, so the conditional clause
is now the indicative one, and the same stage weakened the proposed fix.**
§15.15 item 4 said the buffer measurement was one grep of a trace and should be
the first thing the next stage did. It was, and the answer is:

| in one 25-second `about:blank` run, measured with `WAYLAND_DEBUG=1` | count |
|---|---|
| `wl_shm.create_pool` | 4 |
| `wl_shm_pool.destroy` | **0** |
| `wl_shm_pool.create_buffer` | 3 |
| `wl_buffer.destroy` | **1** |
| `wl_buffer.release` (compositor → client) | 16 |
| `wl_region.destroy` / `wl_surface.destroy` | 18 / 2 |

So Firefox is not a client that forgets to destroy pools while diligently
destroying buffers. **It destroys one buffer in three and no pools at all**, and
the two rows underneath show it is not a client that destroys nothing — regions
and surfaces go. §15.11 item 1's plan is therefore standing on a third of what it
looked like it was standing on: releasing a pool when its last buffer dies frees
one pool of the four here, and nothing at all of the 88 in §15.10's startup run
until the client happens to drop a buffer.

What this stage adds on the other side is that **the machinery below the policy
works**. The proxy's own release path ran in anger on the guest for the first
time: 6–7 pools, **2 released**, and `pool 43` published at `arena 0 slice
28672+3460320` — offset 28672 being where `pool 37` had been, returned and
coalesced. So the free list is not the problem and an arena is not a bump
pointer; the problem is that the signal to run it arrives twice in a session.

**What that costs, stated plainly.** A 60-second Firefox held inside a 64 MiB
arena with `0 overflows`, three times. §15.10 measured 401 MB of pools in a
40-second startup with resizes, which would not have fit — and the bytes it would
not have fit into are the guest's, `VirtualLock`ed, on the machine with the GPU
and the smaller RAM. The failure mode is now known and is not a crash: `create`
refuses with the numbers, `flush_deferred` leaves the pool unannounced and notes
it, and the client's window stops updating. That is a compositor that looks
frozen, which is the failure §15.11 item 3 asks to be made loud and which is
still only a log line.

It is not fixed here, and sizing the arena larger is not the fix — §15.11 says so
and this stage's numbers agree, because 401 MB in forty seconds is a rate. The
honest statement is that **the resource question moved from "hypothetical" to
"measured, unbounded, and in the guest", and the plan for it got weaker on the
same evidence.** It is §17.10 item 2.

### 17.9 The gates

| Gate | Result |
|---|---|
| `cargo test --workspace` | **157 tests**, 0 failures (154 on master after stage 11's merge; three new, all in `wc-proxy`) |
| `cargo clippy --workspace --all-targets` (Linux) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `cargo test --manifest-path crates/wc-arena-host/Cargo.toml` | 1 test, 0 failures |
| `cargo clippy --manifest-path crates/wc-arena-host/Cargo.toml --all-targets` | clean |
| `scripts/wc-proxy.sh window` (stage 4, Wine) | 36 passed, 0 failed — unchanged |
| `scripts/wc-input.sh all` (stage 6 + 10, Wine) | 82 passed, 0 failed — unchanged |
| `scripts/wc-firefox.sh enumerate` | 13 passed, 0 failed |
| `scripts/wc-firefox.sh wine-arena` (new) | **10 passed, 0 failed** |
| **`scripts/wc-guest.sh firefox` (new, the guest)** | **16 passed, 0 failed**, and re-run after the merge with stage 11's icon feed live |
| `scripts/wc-icon.sh` (stage 11, the interactive desktop) | **not run — it is stage 11's and it needs the desktop** |
| `scripts/wc-desktop.sh` (stage 8, the interactive desktop) | **not run — see §17.10 item 5, which is what it is for** |

Every one of these is from **master's checkout after the merge with stage 11**,
not from the branch before it, which is §12.12's arrangement and §14.11's and
§15.13's. The guest block in particular was re-run there rather than carried
over, because that is the run where this stage's arena and stage 11's second
connection are in the same process for the first time (§17.2).

The three new tests are all in `crates/wc-proxy/src/pool.rs`: four connections
over one arena from four threads, a `peer_slice` outside the outbound window
being refused, and a window that cannot be reserved once a session holds the
arena. The deferral of §17.4 is tested inside `wc_handle`'s existing
single outbound-window test, for the reason §14.11 gives — there is one window
per process and `cargo test` runs these threads at once, so a second test that
reserved one would race the first.

**On the count of times Wine has been wrong: four.** §13.6 is the authority, it
is the only number in circulation that comes with the measurement attached, and
§14.12 item 6 explains why three different counts were once in circulation at
all. This stage adds nothing to it: the Wine leg agreed with the guest about
everything it could see, and the one thing it could not see — a stamp — is not a
Wine question, it is a substrate question.

### 17.10 What is not proven

**Nothing on this list is boxed, and that is the first time.** §14.12's box and
§15.14's box are both closed, and the largest remaining gaps are ordinary items.

1. **Nothing frees a pool, and now it is guest RAM.** This one has a section of
   its own — **§17.8** — because it is this stage's consequence rather than an
   item on a list: the measurement is there, the fix §15.11 proposed is weaker
   than it looked on that measurement, and nothing has run long enough to find
   the ceiling. It is the largest open problem the stage leaves behind.
2. **Three connections is not a bound.** The allocator serves any number and the
   test uses four, but nothing has run two *different applications* against one
   arena, which is the case where one client's overflow becomes another's. The
   error names the connection count now, which is the least that can be done for
   it.
3. **The deferral is trusted to be brief.** Gifts are held in the compositor's
   own heap until the first client connects; a compositor that never gets one
   holds them forever, which is a keymap and harmless, and a compositor whose
   client connects *before* the locate would write them too early — the ordering
   is now enforced against the wrong end. That cannot happen in this topology,
   because the proxy is what connects and the plan is what starts it. It is the
   same shape of argument §14.12 item 1 made and lost, which is why it is on this
   list rather than in a comment.
4. **The window is 1028x749 because the guest's screen is 1024x768**, and nothing
   asks Windows how big the desktop is (§15.15 item 3). The resize round trip
   worked, so this is a default that is wrong rather than a path that is broken.
5. **Nobody has looked at *this* window, and that is now a narrow claim rather
   than a broad one.** It used to be that nothing in §15 had run anywhere but a
   workstation and no Firefox window had been in front of a human. **§16 closed
   the general form**: stage 11 ran Firefox on the guest's real interactive
   desktop through the scheduler's interactive-token route
   (`scripts/wc-desktop.sh`), photographed it, and it rendered correctly with its
   own taskbar icon — `docs/stage11/stage11-window-icons-firefox.png`. So the
   route exists and is proven, and what is left is the specific pair: **that run
   was over `shared-file`, and no guest-RAM arena has ever been on that
   desktop.** This stage's runs are in an ssh session's non-interactive window
   station, which is §13.6's lesson, and `docs/stage12/` is the compositor's own
   `--dump-frames` output — what it believes it composited, not what a display
   server holds. Joining the two is one run and it is §17.11 item 4.
6. **Nothing has been typed into Firefox** (§15.14 item 3), no menu has been
   dismissed (item 4), and alpha is still thrown away (item 5).
7. **§12.13 items 3, 4 and 9 are unchanged.** In particular pinning is still
   asserted rather than proven over time, and this run is 60 seconds.
8. **The `de` keymap is the guest's, and it is not the one anything else uses.**
   The workstation's runs are `us` at 35572 bytes on the wire; the guest's
   keyboard layout is `0x0407` and its keymap is 37532. Nothing depends on it and
   both are gifts, but a number that differs between two runs of "the same" test
   is worth writing down once.

### 17.11 What to do next

1. **`DwmExtendFrameIntoClientArea`.** §15.15 item 1 is untouched and is now the
   largest single item in the project: `docs/stage12/stage12-firefox-guest.png`
   is Firefox's own tab strip with Windows' caption above it, on the guest, which
   is the same bug report §15.7 filed with a stronger photograph.
2. **Decide what a pool that is never destroyed costs a long session** — §17.8,
   which is the largest open problem here and the one that is nobody else's
   because this stage is what put it in the guest's RAM. The measurement to make
   is a Firefox that browses rather than one that opens `about:blank`, and the
   number to watch is `overflows`. The fix to design is *not* item 1 of §15.11 on
   its own; the buffer counts say so.
3. **Ask Windows how big the desktop is** (§15.15 item 3), which this stage
   turned from a plausible-default bug into a measured one: 1600x1000 advertised,
   1024x768 underneath, and the client resized to fit.
4. **Run a guest-RAM arena on the interactive desktop.** §17.10 item 5. Stage 11
   built the route and proved it carries Firefox; every run of it so far is
   `shared-file`, and every run of the arena so far is a window station nobody
   can see. `scripts/wc-desktop.sh` plus this stage's `--arena-guest-ram` flags
   is one run, and it is the only thing left that would put a photograph behind
   the sentence "Firefox, over the guest's own RAM, in a window".

## 18. Stage 13: somebody *used* it, and four things were wrong at once

### 18.1 Status in one paragraph

**The window was unusable and every gate was green.** §17.10 item 5 said the
last missing pair was a guest-RAM arena on the interactive desktop; this stage
ran it, and on the way discovered that the reason nobody had noticed the defects
below is that nobody had *used* the thing — every previous stage measured it.
The client area tore into horizontal stripes, a second cursor appeared inside
the window, menus came up surrounded by opaque black, every menu the session had
ever opened was still on the screen, the scroll wheel did nothing, modifiers
stuck, and the taskbar icon was generic. Seven symptoms; **four causes**, and one
of them accounts for three of the symptoms. The tearing was the virtio-fs export
that §10.3 measured incoherent in stage 5 and that stage 7 replaced for
`wc-guest.sh` and nowhere else — `scripts/wc-desktop.sh`, the only harness that
puts a window in front of a person, was still passing `--arena 0:N:Z:\wc\arena`
five stages later. It does not any more, there is no fallback, and
`docs/stage13/stage13-clean-guest-ram.png` is Firefox on the guest's real
desktop over the guest's own RAM, rendering cleanly.

### 18.2 The tearing is the export, and the harness had never used the fix

The user's screenshot is `docs/stage13/stage13-torn-file-arena.png`: a block of
horizontal stripes where a menu should be, correct rows interleaved with stale
ones. The stripe height is the first measurement worth having — the popup is 460
pixels wide, which is 1840 bytes a row, so a 4096-byte page is 2.2 rows, and the
bands are two to three rows tall. That is page granularity, and page granularity
is the signature of a cache and not of a renderer.

**The compositor's own shadow is torn**, which is what decides where the bug is.
`docs/stage13/stage13-torn-popup-dump.png` is that popup's `--dump-frames` PPM —
what the compositor believed it had composited, written from `present` before
GDI touches anything — and it is the same stripes as the screen. So the blit is
innocent and the stride is innocent; the bytes were already wrong when the
compositor read them. §10.3's second row, arriving on somebody's screen four
months after it was measured:

| | result |
|---|---|
| the export, Windows mapping against a Linux `MAP_SHARED` mmap, both live | **MISMATCH**, 65280 wrong of 65536, both directions |

The frame counts say the same thing from the other side. The user's run over the
file arena dumped **583 frames of which 47 were distinct**. The guest-RAM run
below dumped **806 of which 462** — and 462 against 806 is the control's shape
(§12.7's 626/60 was an `about:blank` that had nothing to redraw; this one is a
browser being driven).

`scripts/wc-desktop.sh` now does what `wc-guest.sh` has done since stage 7:
`--arena-guest-ram 0:N` plus `--arena-outbound`, `wc-arena-host` run as root on
the VM host through `rootc`, and `wc-proxy --arena-plan` inside a
`systemd-run --collect` transient unit because `/proc/<qemu-pid>/fd` is
root-only (§12.9). One run of the locate, from this stage:

```
guest RAM in the flat view: 13311 MiB in 4 alias(es)
located 16384/16384 pages in 2.41s (swept 13312 MiB; 189384 other-run stamps, 0 outside guest RAM)
coalesced into 1462 run(s) covering 64 MiB — 11.2 pages per run
stitched 64 MiB into one view with 1462 mmap(MAP_FIXED) call(s) in 4.68ms
verify: 16384/16384 pages of the stitched view carry their own arena index
```

**There is deliberately no fallback.** If the compositor never prints its run id,
or `wc-arena-host` returns non-zero, the run fails and says so; it does not
truncate a file on the export and carry on. That is the whole lesson of this
stage stated as code: the previous arrangement degraded silently to the
incoherent substrate and *every check stayed green*, which is how it reached a
user. `purge_arena_file` also removes any `arena` left on the export by a
pre-stage-13 run, so a stale file cannot be picked up by something that was not
looking.

**Which paths can still reach the file provider, and why each is legitimate:**

* `--arena <id>:<size>:<path>` in `wc-compositor` and `--arena-path` in
  `wc-proxy` remain, and `arena::provide_file` is what implements them. This is
  the hypervisor-free twin stage 10 added: a Wine run on one Linux box has no
  QEMU, therefore no guest RAM to locate, and therefore no way to exercise the
  arena code path at all without it. `scripts/wc-handshake.sh` and
  `scripts/wc-xdg.sh` are those runs.
* `scripts/wc-guest.sh`'s `--transport arena` branch keeps it, and that branch
  is a *deliberate control*: it is how the incoherence is re-measured rather
  than assumed, which is what §12.6 is.
* Nothing that runs against the real guest with a window in front of a person
  can reach it. `wc-desktop.sh` has no code path that produces one.

### 18.3 There were two cursors because a cursor is not content

The user asked for one thing and it is not negotiable: **the cursor must be the
Windows cursor.** So `wl_pointer.set_cursor`'s surface must never be composited,
and this stage makes that a property rather than an accident.

It was *nearly* true already, and the "nearly" is the interesting part. A cursor
surface is nobody's subsurface, so `compose_tree`'s `with_surface_tree_upward`
never reached it, and the `is_root_of_a_window` early return caught it on the
way past — but only *after* its pixels had been blitted into `staging`, where
they stayed for the life of the process. "Not reached by the traversal we
currently have" is not something to rely on while the scene graph is still
growing, and it is not something a reader can check. `commit` now recognises the
seat's cursor surface by identity, **before** the window test, drops its staging
entry and returns.

`cursor_image` therefore records the surface and does nothing else. Turning it
into a real `HCURSOR` — `CreateIconIndirect` with `fIcon: 0` and the client's
hotspot, plus `WM_SETCURSOR` — is the right next thing and would make an I-beam
over a text field an I-beam; it belongs to whoever owns `wp_cursor_shape_v1`,
and this stage deliberately stops at the guarantee it owes: **exactly one
pointer, and it is Windows'.**

Honest limitation: this stage did not get a photograph of the second cursor
before it fixed it, so the causal claim is a reading of the code plus the user's
report, not a measurement. What *is* measured is that the compositor's log says
`a client set a cursor` on every Firefox run, so a cursor surface exists and is
committed to on this path.

### 18.4 One empty function body, three user-visible symptoms

This is the finding of the stage and it is worth stating as a shape rather than
as three bugs.

**`XdgShellHandler::popup_destroyed` has an empty default body in smithay, and
this compositor never overrode it.** From stage 9 to here, every `xdg_popup`
Firefox destroyed left its `HWND` on the screen. A live session accumulated
three stale "New Tab" tooltips at three different positions, a context menu, an
account panel and the hamburger menu, **all visible at once**, over the page the
user was reading. Nothing errored. Nothing was logged. The frame dumps were
healthy. The windows were real, mapped, correctly positioned and correctly
painted — they were simply never taken away, and there is no measurement in this
project that would have caught it, because every one of them counts things that
happened rather than things that stopped happening.

The two consequences were reported as separate bugs and are not:

* **"Clicking elsewhere does not dismiss a menu."** Half of that is the same
  bug and half of it is `xdg_popup.grab`, which was also an empty body. A menu
  is dismissed by the *compositor* — the client cannot see a click outside its
  own surfaces — so with no grab recorded, nothing ever sent `popup_done` and
  the client had no reason to destroy anything. Both are implemented; the grab
  is one level rather than a stack, and §18.10 says what that costs.
* **"The scroll wheel does nothing in Firefox."** `WM_MOUSEWHEEL` is delivered
  by Win32 to the window with **keyboard focus**, not to the window under the
  cursor. With half a dozen stale popups on screen the cursor was usually over
  one of them, so `pointer_focus` was a dead menu while the wheel arrived for
  the toplevel, and `if self.pointer_focus != Some(id) { return; }` ate it.
  **Confirmed by the user rather than argued**: with no popup on screen,
  scrolling always worked. The popup fix is the fix.

  The early return is now deliberate instead of silent — the axis is delivered
  to whatever the pointer is actually over, which is what `pointer.axis` would
  do anyway since `id` never routed anything, and a disagreement between the two
  is logged. A wheel with no pointer focus at all still has nowhere to go.

`xdg_popup.reposition` was the third empty body: `repositioned` was never sent,
so a submenu GTK moves to keep it on screen stayed where it was first put. It
now sends `repositioned`, then a `configure`, then a `ToUi::MovePopup` — in that
order, because a client that gets a configure with no token cannot match the
answer to its request and GTK ignores the move.

### 18.5 Popup alpha, and why a layered window rather than `AlphaBlend`

With the tearing gone, the menus turned out to render correctly and to be
**surrounded by an opaque black rectangle** where the client's drop shadow
should be, with the rounded corners cut out in the same black. That is not a
client bug: `map_window` has printed `argb: alpha ignored by GDI, drawn opaque`
since stage 3, and it means what it says. `SetDIBitsToDevice` on a 32-bit
`BI_RGB` DIB discards the fourth byte, so a GTK menu's transparent margin —
`A=0`, `RGB=0` — is drawn as black.

`AlphaBlend` is the obvious reach and it is the wrong one: it composites against
*what is already in the destination DC*, which is the window's own background,
and it cannot show the desktop behind the window because the window is opaque to
the system. Per-pixel transparency of a **window** on Win32 is a window-manager
property, and `WS_EX_LAYERED` + `UpdateLayeredWindow` with
`BLENDFUNCTION{AC_SRC_OVER, AC_SRC_ALPHA}` is the only mechanism for it.

**Popups only, and the reason is a cost rather than a preference.**
`UpdateLayeredWindow` replaces `WM_PAINT` entirely and takes the whole window
each time, so damage cannot be used — a layered window redraws everything on
every commit. A menu is small and changes rarely; a toplevel is neither. A
layered toplevel would also lose its native frame, because the bitmap becomes
the entire window and there is no non-client area left for Windows to draw a
caption in, which is the opposite of what `docs/goal-firefox.md` asks for. The
toplevel's own transparent margin is somebody else's fix and it is a better one:
server-side decorations remove the shadow inset rather than compositing it.

Two things fall out for free and are worth recording because they look like
work that still needs doing. `ULW_ALPHA` makes Windows hit-test a layered window
**by its alpha**, so clicks in the fully transparent shadow margin fall through
to whatever is behind it — which is what `wl_surface.set_input_region` would
have been asked to arrange, and it is arranged. And the map path now paints
before `ShowWindow`, because a layered window that has never been given a bitmap
is undefined rather than blank.

### 18.6 The icon: §17.2 recorded the right symptom and the wrong reason

§17.2 said `wc_icon_manager_v1` published `0 image(s)` for `"firefox"` *"because
the resolver runs where the proxy runs and the VM host has no icon theme
carrying Firefox's pixels"*. The first clause is right. **The conclusion drawn
from it — that the resolver is on the wrong machine — is wrong, and it matters,
because the fix it implies is to move the resolver across the VM boundary.**

`wc-proxy` is a Wayland *server*, reached by its client over an AF_UNIX socket.
An AF_UNIX socket has both ends in one kernel. **The proxy is therefore always
on the client's machine, and the client's machine is by definition the one the
application is installed on.** There is no wrong machine here and nothing needs
to cross anything.

What was actually missing was the *directory*, and it is measurable on
testhost in two commands:

```
$ ls /run/current-system/sw/share/icons/hicolor/*/apps/firefox.png
ls: cannot access ...: No such file or directory
$ ls /run/current-system/sw/share/applications/ | grep -i firefox
$ ls /nix/store/d8q8…-firefox-151.0.2/share/{applications,icons/hicolor/48x48/apps}/
applications: firefox.desktop
apps:         firefox.png
```

Firefox's desktop entry and its icons are in **Firefox's own package prefix**,
and on NixOS a package prefix is a store path that appears in `XDG_DATA_DIRS`
only if somebody put the package into a profile. The resolver's roots were
`XDG_DATA_HOME` and `XDG_DATA_DIRS`, and the proxy runs inside a `systemd-run`
transient unit whose environment is systemd's rather than a login shell's — so
on this rig the list frequently degrades to the hard-coded
`/usr/local/share:/usr/share`, which holds nothing at all on NixOS.

So the resolver now searches **the client's own installation prefix**, found
from the client rather than configured. `SO_PEERCRED` on the accepted socket
gives the client's pid; `/proc/<pid>/exe` is the kernel's answer to what it is
running, which survives a lying `argv[0]`, a wrapper script that `exec`s
something else, and a binary replaced on disk since; and every ancestor of that
path with a `share` directory, up to four levels, joins the search. That finds
`/usr/share` from `/usr/bin/foo`, `/usr/share` from `/usr/lib/firefox/firefox`,
and `/nix/store/…-firefox-151.0.2/share` from the store path three levels up —
which is exactly the file that exists. It is the FHS relationship between `bin/`
and `share/`, and it holds inside a Nix store path, inside a Flatpak, and inside
anything unpacked into a home directory, which is why it is preferred to a list
of special cases for package managers.

Order matters and it is: `--icon-data-dir` (new, repeatable, for callers with no
session environment to inherit — `scripts/wc-icon.sh` was already exporting
`XDG_DATA_DIRS` around the process for exactly this reason), then the XDG
directories, then the client's prefix. A theme the user installed is a
deliberate override of what a package shipped, so XDG wins; the client's prefix
is the guaranteed answer rather than the preferred one.

**No descriptor crosses anything.** `SO_PEERCRED` is a `getsockopt` on a socket
this process already owns, `/proc/<pid>/exe` is a `readlink`, both happen on the
proxy's own machine, and what leaves is a path. §14.7's count is unchanged: 3
fds in, 1 out.

### 18.7 Modifiers stick because `leave` tells the client and not us

Two causes, both real, neither large.

`set_keyboard_focus(id, false)` called `keyboard.set_focus(self, None, …)` and
nothing else. `wl_keyboard.leave` is correct as far as the *client* goes — the
client is required to treat every key as released — but the compositor's own xkb
state went on believing they were down, and it is the compositor's state that
decides what the next `modifiers` event says. Hold Ctrl, lose focus (which with
§18.4's stale popups was happening constantly), release Ctrl into whatever has
focus now, come back: smithay sends `enter` with a pressed-keys array and
`modifiers` with Ctrl depressed, and every keystroke is Ctrl-modified until the
user happens to press and release Ctrl over our window. Every pressed key is now
fed back as a release through `input_intercept` — which updates our state
without sending anything to the client, because `leave` has already said what
the client needs to hear — *before* `set_focus(None)`.

Caps Lock and Num Lock are the second cause and they are a different kind of
thing: they are **locked** modifiers, owned by the machine, and a user who
toggles Caps Lock in another application changes it for us too while we see no
key event at all. They are now read from the system on every focus gain —
`GetKeyState(vk) & 1`, the **low** bit, because the high bit is the physical
up/down of a key that is up almost all the time — and carried in on
`FromUi::KeyboardFocus` so the headless platform can state them too.

Deliberately unchanged, because it was checked and is correct: the auto-repeat
filter on bit 30 of `lParam`. The client repeats from `repeat_info`, and
forwarding Windows' repeat as well would give two repeat streams at different
rates.

### 18.8 Four things that cost real time

* **A `sed` range whose end pattern did not match deleted 1500 lines of
  `state.rs`** and the build stayed quiet about it for one command, because what
  was left still parsed. Recovered from `git` plus the edits re-applied. The
  lesson is not about `sed`: it is that a file this project keeps its reasoning
  in is not a file to edit with a range expression.
* **Backticks inside a double-quoted heredoc that bash expands.** The
  `wc-desktop.sh` host-side script is written through `ssh "…"`, so a comment
  containing `` `WAYLAND_DISPLAY` `` is a command substitution. It presented as
  `WAYLAND_DISPLAY: command not found` and a client that produced no log at all.
* **`WAYLAND_DISPLAY=wl-proxy` with a private `XDG_RUNTIME_DIR`.** Firefox needs
  a runtime directory it owns, and the proxy's socket is not in it. libwayland
  reads a `WAYLAND_DISPLAY` beginning with `/` as a path, which is the only
  thing that separates the two; §17's `wc-guest.sh` had learned this and
  `wc-desktop.sh` had to learn it again, which is an argument for the two
  harnesses sharing more than they do.
* **`wc-arena-host` is not a workspace member** (`Cargo.toml` excludes it: it
  links shared_vulkan's `sv-hostd` and has its own lockfile), so
  `cargo build --workspace` does not produce it and the deploy failed on a
  missing file. `wc-desktop.sh` now builds it by its own manifest, as
  `wc-guest.sh` does.

### 18.9 The gates

| gate | result |
|---|---|
| `cargo test --workspace` | **158 passed**, 0 failed (157 on master, +1: `the_client_prefix_is_found_from_bin_and_from_lib`) |
| `cargo clippy --workspace --all-targets` | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `cargo build --workspace --target x86_64-pc-windows-gnu` | an **executable**, not metadata — §17's trap, and this stage deployed from it four times |
| `wc-desktop.sh window`, guest-RAM arena, Firefox, interactive desktop | the arena located, the window captured, the caption and taskbar button asserted |
| the locate | 16384/16384 pages, 2.41 s, verify clean, 1462 runs |
| frames | 806 dumped, **462 distinct** (the file-arena run: 583 dumped, **47** distinct) |
| the picture | `docs/stage13/stage13-clean-guest-ram.png` |

### 18.10 What is not proven

1. **The popup grab is one level, not a stack.** Dismissing the innermost popup
   makes the client destroy it, which clears the record, so a second click
   dismisses the next one out. That is one click per level where a Linux
   compositor takes one click for all of them. A submenu is what would show it.
2. **`xdg_positioner`'s constraint adjustments are still ignored**, in
   `new_popup` and now in `reposition_request` too. This compositor does not
   know where the parent window is on the Windows desktop, so it cannot tell
   whether a placement would fall off the screen. Windows keeps an owned
   `WS_POPUP` on the monitor by itself, which is why it has not bitten — but
   that means Windows is silently moving popups and the client is not being told.
3. **No run in this stage exercised a popup end to end, and that is the largest
   gap it leaves.** The popup work — destroy, grab, reposition, and the layered
   window — is compiled, reviewed and reasoned from a live session's evidence,
   but `wc-desktop.sh window` drives a drag and not a menu, so its Firefox
   opened no popups and the final run's log contains no `xdg_popup` line at all.
   The torn run that *did* open four of them was the run whose defects this
   stage is about, and it predates every fix. **What is needed is a harness step
   that opens a menu**: right-click the client area through `wc-desk`, then
   assert on `xdg_popup created`, a `WaycomposeToplevel` in the window
   enumeration, a click elsewhere, `popup_done`, `xdg_popup destroyed`, and the
   window being *gone* from the enumeration. That last assertion is the one that
   would have caught this four stages ago, and nothing in this project asserts
   that a window stopped existing.
4. **The layered-window path redraws the whole popup per commit.** Fine for a
   menu; nobody has measured what it costs for a popup that animates. Nor has
   anything photographed a menu with its shadow, for the reason above.
5. **The second cursor was never photographed.** §18.3 is a code reading plus a
   user report, and the fix is a guarantee rather than a measured before/after.
6. **The icon fix has not been photographed on the guest either.** The resolver
   change is unit-tested against all three prefix layouts and the store path was
   verified by hand on testhost, but no run in this stage produced a Firefox
   taskbar button with Firefox's icon on it over guest RAM. That is one run and
   it is the first thing to do.
7. **`--icon-data-dir` has no harness using it yet.** It exists because the
   transient unit's environment is not a session's; nothing yet passes it.
8. **The frame counts are one run each.** 462/806 against 47/583 is a large
   enough difference not to need a repeat to believe, and it is still one run.
9. **The compositor's console window is on the desktop.** Both this stage's and
   another worktree's appear in `wc-desk windows` as visible
   `ConsoleWindowClass` entries titled with the binary's path — so stage 11's
   "hide the console when `GetConsoleProcessList == 1`" is not firing when the
   task scheduler launches the process. Not diagnosed; recorded because a
   screenshot tool that photographs a console window in front of the thing it is
   photographing is a poor instrument, and the same argument applies to
   `wc-desk` itself, which is still console-subsystem.

### 18.11 What to do next

1. **Photograph the icon over guest RAM** (§18.10 item 6). One run, and it
   closes §16's last gap in the topology `docs/goal-firefox.md` describes.
2. **Hide the console** — both the compositor's and `wc-desk`'s (§18.10 item 9).
   It is the difference between an instrument and an anecdote.
3. **A harness step that opens a menu** (§18.10 item 3), which is what would
   have caught this stage's largest bug four stages ago.
4. **A grab stack** (§18.10 item 1), which is what a submenu needs and what
   `xdg_popup`'s protocol actually describes.
5. **Ask Windows where the parent window is**, so `xdg_positioner`'s constraint
   adjustments can be honoured (§18.10 item 2). This is the same missing fact as
   §15.15 item 3 — nothing asks Windows how big anything is — and the two should
   be answered together.

## 19. Stage 14: one title bar, and it is Windows'

### 19.1 Status in one paragraph

Every window this project has ever made had **two** title bars — Windows'
caption, and directly beneath it Firefox's tab strip with its own minimise,
maximise and close buttons. It is in `docs/stage9/stage9-firefox-window.png`,
§15.7 named it as *"the single largest remaining item"*, and the user reported
it independently, on the crash reporter's window as well as the browser's, so it
was every window rather than a browser quirk. It is fixed, it took one line of
compositor and one environment variable, and the picture is
`docs/stage14/stage14-server.png`: Windows' caption with Firefox's icon and
title, and below it Firefox's tabs with no window buttons of their own. The run
behind that picture is the interactive desktop **and** Firefox **and** an arena
in the guest's own RAM, which is §17.11 item 4's arrangement; stage 13 got there
first, from the other direction, and §19.6 says what this one has that
`wc-desktop.sh` does not. What was *not* fixed is the other half of
the goal clause: Firefox painting its tabs *into* the caption rather than under
it. §19.7 wires the requests that half needs and §19.8 records the thing that
makes the rest of it impossible, which is that Wayland has no way for a client
to publish a draggable region.

### 19.2 One line decides it, and it is the manager's default rather than the surface's

The compositor now sends `default_mode(2)` — `Server` — when a client binds
`org_kde_kwin_server_decoration_manager`. Before this stage it sent
`default_mode(1)`.

The line it replaces had a paragraph of reasoning attached to it, and the
reasoning was right while the value was wrong:

> **`Client` is the default mode this compositor advertises, and that is the
> goal document's decision rather than a shrug.** docs/goal-firefox.md wants
> Firefox painting its tabs into the title bar, which is only possible if
> Firefox is drawing the title bar — a server-decorated window has a caption the
> client cannot touch.

Every clause of that is true. What it left out is that the compositor was
promising a frame it had not built: it said "you draw it" and then put the
surface inside a `WS_OVERLAPPEDWINDOW`, which draws a caption whether anybody
asked for one or not. Two title bars is exactly what that promise buys, and the
window would have had two of them however good the client-side story was, right
up until `FrameStyle::extend_top` moved off zero — which is stage 8's item, was
still untouched at stage 12, and is §19.7's.

So the mode is a flag, `--decorations server|client`, and `server` is the
default. Windows' caption **is** a server-side decoration; saying so is the
honest answer and it is the one that leaves a window with one title bar today.
`client` is what the frame-extension work is developed against, and it is what
`scripts/wc-firefox.sh`'s older blocks now pass explicitly rather than
inheriting — a block whose claims are about the shadow inset must name the mode
that produces one.

**It has to be the manager's default and not the answer to the surface's
request**, and this is the part that is not obvious. GTK 3 decides whether to
draw a title bar in `gtk_window_should_use_csd()`, which on Wayland consults
`gdk_wayland_display_prefers_ssd()` and nothing else — not `GTK_CSD`, which is
checked after an unconditional Wayland branch and is therefore dead here, and
not the per-surface mode. That function reads the value the *manager* sent at
bind. Answering `Server` to the surface's `request_mode` and leaving the
manager's default at `Client` changes nothing at all, except that GDK
re-requests the mode it wanted, which was captured as a loop against real KWin.

### 19.3 The second lever is in the client's environment, and no protocol carries it

`default_mode(2)` on its own is **not enough**, and the control leg in
`scripts/wc-firefox.sh titlebar` exists to keep saying so. With the compositor
advertising `Server` and nothing done to the client, Firefox 151.0.2 asks for
`Client` anyway and still reports the shadow inset:

| compositor | client environment | Firefox asks for | its window geometry |
|---|---|---|---|
| `default_mode(1)` | — | `request_mode(1)` | `(26, 23, 1280, 900)` |
| `default_mode(2)` | — | `request_mode(1)` | `(26, 23, 1280, 900)` |
| `default_mode(2)` | `MOZ_GTK_TITLEBAR_DECORATION=none` | `request_mode(2)` | `(0, 0, 1280, 900)` |

The reason is in Gecko rather than in GTK. `nsWindow::GetSystemGtkWindowDecoration()`
returns `GTK_DECORATION_CLIENT` for Wayland unconditionally, before it looks at
anything else, so `LookAndFeel`'s `GTKCSDAvailable` is true no matter what the
compositor said and Firefox draws a title bar of its own. The environment
variable is checked *before* that branch and is the only thing that turns it
off. `browser.tabs.inTitlebar=0` in the profile does the same job and is weaker:
the variable also disables the ARGB visual for the toplevel and makes
`SetCustomTitlebar()` inert.

> **The assumption this stage ships, stated plainly.** The variable is set in
> the *client's* environment, on the machine Firefox runs on, and there is no
> Wayland protocol that carries it — a compositor cannot ask a client to stop
> decorating itself beyond the mode it already sends. So the launcher sets it.
> Every harness here does (`scripts/wc-firefox.sh`'s `FX_ENV`,
> `scripts/wc-titlebar.sh`'s `fxenv`), and a deployment that starts Firefox some
> other way and does not set it gets two title bars back with no other symptom.
> It is a deployment requirement rather than a fix, and the honest place for it
> is a launcher script beside whatever starts the client.

The libadwaita theory, which was the obvious one, is wrong and was checked
rather than argued about: the only libadwaita knob in Firefox 151 is
`widget.gtk.libadwaita-colors.enabled`, a palette override, and the binary
contains no `adw_*` symbols at all — Firefox neither links nor `dlopen`s it.

### 19.4 A client that insists on `Client`, and why it is echoed

The KDE protocol's `request_mode` is a request and the compositor's `mode` is
the answer. This compositor echoes, even when the echo contradicts the default
it advertised, and that is a decision with three reasons in order of weight.

First, **the per-surface answer is not the lever** (§19.2). Refusing it changes
no GTK client's behaviour, because GTK read the manager's default once and is
not listening. Second, GDK's handler re-requests the mode it wants when it is
contradicted; against real KWin that was a loop, and KWin capitulates. A
compositor that does not capitulate is a compositor that spins. Third, a client
that has decided to draw its own title bar and is told it may not still draws
one — the only thing refusing buys is a window that has no bar and cannot be
moved, which is worse than the bug this stage is fixing.

The cost is honest and small: under `--decorations server` a client that insists
gets two title bars, exactly as every client did before this stage. Measured on
the control leg, Firefox sends **one** `request_mode` in the whole trace when it
is echoed, so nothing loops.

### 19.5 The geometry rectangle changes shape, and §15.6's code already handled it

§15.6 made the window the `xdg_surface.set_window_geometry` rectangle rather
than the buffer, because a GTK client's buffer is bigger than its window by an
invisible resize margin and drop shadow. Under server-side decorations there is
no shadow to subtract: Firefox reports `(0, 0, w, h)` and the window *is* the
buffer again.

Nothing had to change for that, and it is worth writing down that nothing did.
`compose_tree` takes the geometry origin out of every surface's tree offset and
sizes the canvas from the rectangle; with a zero origin the subtraction is a
no-op and the canvas is the buffer's bounds. The Win32 side sizes the `HWND`
from the same rectangle through `AdjustWindowRectEx`. Both are mode-agnostic by
construction rather than by a branch, which is what §15.6 built and is the
reason this stage is one line rather than a rework — on the guest, with
`--output 1100x700`, the window mapped `990x630` under both modes.

### 19.6 The picture, on the real desktop, over the guest's own RAM

`scripts/wc-titlebar.sh` is a script of its own, and after the merge with stage
13 that needs a better reason than it had when it was written.

When it was written the reason was that the claim is the *intersection* of two
harnesses and neither had the other half: `wc-guest.sh` ran Firefox over guest
RAM with the compositor in an ssh session, which is session 0 — every Win32 call
succeeds there and nothing is ever on a screen (§10.12 item 3) — and
`wc-desktop.sh` reached `WinSta0\Default` and photographed windows, over a
**file** arena on the virtio-fs export. Stage 13 closed the second half of that
gap independently and for a different reason: §18.2 found the export was what
had been tearing the user's window, and `wc-desktop.sh` uses guest RAM now.

What is left is the errand rather than the plumbing. `wc-desktop.sh` is stage
8's harness and photographs a window to answer questions about *the window* —
orientation, the modal sizing loop, the taskbar button, the frame rate — and it
runs one client under one arrangement. This script runs the **same client twice
under two decoration modes** and puts the two captures side by side, which is
the only form the claim can take: a window with one caption is also what a
Firefox that failed to start looks like, so a single picture proves nothing.
It also owns its own guest directory, socket, scheduled-task names and export
subdirectory (§16.8), which is what let it run against a guest three other
experiments were using while stage 13's own harness was being changed underneath
it.

What one leg does, in the order the pieces start: the compositor is registered
as a scheduled task with an `Interactive` principal so that it lands on the
logged-on session's window station and does not die with the ssh call that
started it; it commits and pins 64 MiB inside the VM and prints a run id;
`wc-arena-host` runs as root on the VM host, sweeps QEMU's memory backend for
that id and writes a plan; `probe-relay.exe` on the guest bridges a loopback TCP
port into the compositor's `afunix` node; an `ssh -L` puts the Linux end of that
in a directory `wc-proxy` can bind beside; and `wc-proxy` and Firefox run in a
root transient unit on the host, because `/proc/<qemu-pid>/fd` is root-only
there (§12.9). Then `wc-desk.exe shot --pid <the compositor> --class
WaycomposeToplevel` captures the window through `PrintWindow(PW_RENDERFULLCONTENT)`,
which reads the DWM redirection surface rather than the framebuffer and
therefore works while the guest's lock screen owns the foreground.

The `server` leg, `--output 1100x700`:

```
server: arena 0 provided from guest RAM: run 0x8f28ccafa4f065ff, 67108864 bytes, 16384 pages, locked=true
server: verify: 16384/16384 pages of the stitched view carry their own arena index
server: Firefox asked for Server, on real Windows
server: a real HWND, sized from the geometry rectangle: window 1 mapped 990x630
server: every pixel came out of guest RAM (mapped: 0 section, 0 shared-file, 3 arena-slice)
server: 120 frames of Firefox's pixels were blitted into it
shot: hwnd 0x400ea at 156,156 title "Mozilla Firefox" -> 960x543, per-monitor-aware
```

`docs/stage14/stage14-server.png` is that capture. One caption, Windows', with
the client's own icon — stage 11's feed, resolving on the far side — and the
client's own title in it, and Firefox's tabs directly below with nothing in the
corner but the tab-list chevron.

`docs/stage14/stage14-client.png` is the `client` leg, same size, same guest,
same arena, minutes apart, and it is what every stage before this one shipped:
Windows' caption with its three buttons, and in the tab strip below it Firefox's
own close button. The two title bars are one on top of the other and both have a
way to close the window.

The page it happens to be showing is `about:support`, which nothing here staged.
It is a useful accident: the build id, the binary path and the profile directory
in that table are this run's own provenance, printed by the client, inside the
guest, in the picture.

> **The guest run is flaky and the reason is not decorations.** Over the
> session the two legs were run nine times between them; four of those failed at
> the arena locate, with `wc-arena-host` reporting `located 16351 of 16384
> pages` and 189,364 pages carrying the arena magic under a *different* run id.
> Those are stale stamps from this session's own earlier runs and from the three
> other experiments sharing the guest. Both pictures above are from **one** run
> of `both` from this checkout, and it passed 14 of 14 first time; the failures
> are recorded because a harness that needs three attempts on a busy guest
> should say so rather than be discovered. It is a §12 problem arriving after a
> long day on one guest rather than anything this stage touched, and it is
> §19.12 item 4.

The corner is where the difference lives, and the three crops in
`docs/stage14/` are the same 420x46 rectangle of the same client's own pixels,
from the workstation, under three arrangements:

| file | arrangement | what is in the corner |
|---|---|---|
| `corner-client.png` | `default_mode(1)`, no variable | chevron, minimise, maximise, close |
| `corner-server.png` | `default_mode(2)` + the variable | chevron |
| `corner-menu-only.png` | `default_mode(1)`, `gtk-decoration-layout=menu:` | chevron, close — §19.9 |

### 19.7 The three requests a client-decorated window makes, wired to Win32

A window whose title bar is the client's cannot be moved by Windows, because
Windows does not know where its title bar is (§19.8). It moves by *asking*, and
until this stage all three of the requests it asks with had smithay's empty
default — so a client-decorated window could be closed, maximised and minimised
and could not be moved at all. They are wired now:

| request | Win32 |
|---|---|
| `xdg_toplevel.move` | `ReleaseCapture()`, then `WM_SYSCOMMAND`/`SC_MOVE` |
| `xdg_toplevel.resize` | `WM_SYSCOMMAND`/`SC_SIZE` with the matching `WMSZ_*` in the low nibble |
| `xdg_toplevel.show_window_menu` | `TrackPopupMenu(GetSystemMenu(hwnd, 0), TPM_RETURNCMD, …)`, and the chosen id posted back as a `WM_SYSCOMMAND` |

`set_maximized` and `set_minimized` were already wired, in stage 8, and are what
Firefox's double-click and middle-click title-bar actions route through
(`DoTitlebarAction()`), so those work as soon as the drag does.

Three things about this are worth keeping.

**`ReleaseCapture` is not optional.** The client's drag began with a
`WM_LBUTTONDOWN` that the window procedure answered with `SetCapture`, and
`DefWindowProc` refuses to start a moving or sizing loop for a window that holds
the capture. The symptom is a window that simply does not move, with nothing in
any log.

**All three post rather than send.** Each enters a modal loop that does not
return until the user lets go, and this code runs inside `drain`, which holds a
re-entrancy flag for its duration and re-posts its wake whenever another command
arrives while the flag is set. A `SendMessageW` here would hold that flag for
the whole gesture and spin the pump re-posting a wake it cannot service. Posting
lets the drain finish and the loop is entered from the pump, where §8.2's
finding applies — a posted wake still reaches the window from inside the loop,
which is why the window keeps repainting while it is being dragged.

**The window menu is the system's, not an imitation.** `GetSystemMenu` returns
the very menu the caption's right-click and Alt+Space produce, with Windows'
strings, accelerators and greying — a maximised window's *Maximize* is disabled
and a fixed-size window has no *Size* — and `TPM_RETURNCMD` turns the choice
back into the same `WM_SYSCOMMAND` the caption would have produced, so *Close*
on it is the same code path as the close button.

The seat and the serial are ignored in all three, deliberately. The protocol
wants them checked against a recent input event so a client cannot start a drag
out of nothing; here there is one seat, the events came from Win32 in the first
place, and Windows' own modal loop refuses to start unless a button is really
down — so the check the serial exists to make is made one layer down, against
the real hardware state.

### 19.8 What can never work through that path, and it is snap layouts

`WM_NCHITTEST` can never return `HTCAPTION` over Firefox's tab strip, and this
is a protocol fact rather than an unfinished piece of work. **Wayland has no
protocol by which a client publishes its draggable region.** Firefox's
`mDraggableRegion` is internal and is observable only *after* a click, as an
`xdg_toplevel.move` request. So the whole surface is `HTCLIENT`, every drag
arrives as a request from the client, and the compositor learns where the title
bar was by being told a drag started in it.

The consequence is concrete and should be known before anybody builds towards
it: **Windows 11's snap layouts cannot work through that path.** The flyout is
produced by hovering the maximise button, Windows finds that button by hit-test,
and there is no button to find. Neither is there an `HTMAXBUTTON` to return: the
compositor does not know where Firefox drew its maximise button either, for the
same reason.

The way out would be to draw *real* caption buttons in the extended frame, in
the corner Firefox has left empty, and hit-test them properly. That needs
Firefox to stop drawing its own, which is what §19.9 measured.

### 19.9 `gtk-decoration-layout`, measured: minimise and maximise go, close does not

Firefox reads GTK's `gtk-decoration-layout` and draws only the buttons it names
(`nsLookAndFeel.cpp`). The hypothesis this stage inherited was that
`gtk-decoration-layout=menu:` — nothing on the right-hand side — would leave
Firefox's tab strip with no window buttons at all, freeing that corner for real
Windows caption buttons with real `HTMINBUTTON`/`HTMAXBUTTON`/`HTCLOSE`
hit-testing, and therefore restoring snap layouts. Nobody had run it.

Run, on the workstation, against `--decorations client`, with the layout changed
in a copy of the real configuration directory pointed at by `XDG_CONFIG_HOME`
— the copy matters, and cost a round: setting `HOME` alone does not make GTK
read a `settings.ini` there, and a run that changes nothing looks exactly like a
setting that does nothing.

| `gtk-decoration-layout` | what Firefox drew |
|---|---|
| `icon:minimize,maximize,close` (this desktop's own) | minimise, maximise, close |
| `menu:` | **close** |
| `close:menu` | **close, still on the right** |

So the hypothesis is **half right and the important half is wrong**. Minimise
and maximise can be taken away. The close button cannot: it survives a layout
that names no right-hand buttons at all, and it does not move to the left when
the layout puts it there. Firefox draws a close button in the corner of a
tabs-in-title-bar window regardless of what GTK is told.

That is enough to stop the plan in §19.8's last paragraph as stated. Real
caption buttons drawn in the extended frame would sit next to a client-drawn
close button that cannot be removed, which is the two-title-bars bug again in
miniature. Anything further here is a Gecko question — `browser.tabs.inTitlebar`
is the switch that removes the whole bar, and it is the switch this stage
already uses — rather than a GTK one.

### 19.10 The gates

| Gate | Result |
|---|---|
| `cargo test --workspace` | **160 tests**, 0 failures — master's count after the merges with stages 13 and 21 |
| `cargo clippy --workspace --all-targets` (Linux) | clean |
| `cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu` | clean |
| `scripts/wc-firefox.sh titlebar` (new) | **9 passed, 0 failed** |
| `scripts/wc-firefox.sh enumerate` | 13 passed, 0 failed — unchanged |
| `scripts/wc-firefox.sh window` | 7 passed, 0 failed — unchanged |
| **`scripts/wc-titlebar.sh both` (new, the guest's interactive desktop)** | **14 passed, 0 failed** — both legs, from this checkout, after both merges |
| `cargo clippy --manifest-path crates/wc-arena-host/Cargo.toml --all-targets` | clean |
| `scripts/wc-proxy.sh`, `wc-input.sh`, `wc-guest.sh`, `wc-desktop.sh`, `wc-icon.sh` | **not re-run — the guest was contended and none of them touches a decoration** |

The five that were not re-run are named rather than omitted, and the reason they
are safe was **measured rather than assumed**, because the obvious way for this
stage to break something else is a client that starts behaving differently
because the default changed. `weston-terminal`, against the Linux compositor with
`--decorations server`, maps `742x427` — the same number `wc-proxy.sh` asserts
and §15.6 established, unchanged. It never binds the KDE decoration manager: the
interface appears in its trace once, in the registry burst, and is not taken up.
It draws its own frame either way, which is what a toolkit that predates the
protocol does. The three requests §19.7 wires had smithay's empty default before
and are only sent by a client that decorates itself, so they can only add
behaviour. `wc-firefox.sh`'s own older blocks *were* re-run, because they are the
ones that would have noticed (§19.2's last paragraph).

The stage adds no unit test, and that is a statement rather than an omission:
everything it changed is a value sent on a global's bind and three requests
handed to Win32, and neither is reachable from a test that does not run a client.
The `titlebar` block is the test, it runs the real Firefox against the real
compositor, and it carries a control leg (§19.3) so that it fails if the second
lever stops being needed as well as if it stops working.

### 19.11 What is not proven

1. **Nothing has been dragged.** §19.7's three requests compile, log and are
   posted, and no run has clicked Firefox's tab strip and watched the window
   follow the pointer. The Linux harness cannot: there is no window manager
   under the headless platform, so a `move_request` there has nowhere to go and
   the headless arm says so rather than pretending. Doing it needs
   `wc-desk.exe drag`'s trick pointed at a client-decorated window on the guest's
   desktop, which is one run and is item 1 of §19.12.
2. **`FrameStyle::extend_top` is still zero.** `WM_NCCALCSIZE` and
   `DwmExtendFrameIntoClientArea` are not written. The goal clause's second half
   — tabs *in* the caption rather than under it — is untouched, and §19.8 and
   §19.9 are the two things anybody starting it should read first.
3. **The environment variable is unenforced.** §19.3's box. Nothing checks that
   the client was started with it; a deployment that forgets gets two title bars
   and no diagnostic. The compositor *could* notice — a client that asks for
   `Client` when the default is `Server` is exactly that case, and it is already
   logged — and turning that log line into a warning is a cheap improvement
   nobody has made.
4. **One client, one version.** Firefox 151.0.2. GTK 4 does not implement the
   KDE protocol at all and would need `xdg-decoration`, which this compositor
   answers consistently (§19.2) and which no client here has ever bound
   (§15.7). §16.11 item 2 has been asking for a GTK 4 client since stage 11 and
   still is.

### 19.12 What to do next

1. **Drag a client-decorated window on the guest's desktop.** §19.11 item 1. It
   closes the only untested half of §19.7 and it is the run that says whether
   `ReleaseCapture` was the whole story.
2. **`WM_NCCALCSIZE` and `DwmExtendFrameIntoClientArea`**, which is §15.15 item
   1 and is now the only part of the goal clause left. Read §19.8 first: the
   frame can be extended and the client's pixels can be blitted over it, and the
   corner cannot be given real caption buttons while Firefox insists on drawing a
   close button there (§19.9). What that leaves is tabs in the caption with the
   window's own buttons in the *extended* strip beside Firefox's close button, or
   `--decorations server` and one native bar. The second is what ships today.
3. **Warn when the two levers disagree.** §19.11 item 3, and it is four lines.
4. **The stale-stamp problem.** §19.6's box is the first time the guest-RAM
   locate has failed repeatedly rather than once, and it failed *because the
   guest had been used*. `wc-arena-host` sweeps for a magic and a run id and
   reports how many pages carried the magic under a different id; that number is
   now six figures. Whether a long-lived guest eventually becomes unusable for
   this transport is a real question and it is not this stage's.

## 25. Stage 21: four protocols, one of which was a trap

### 25.1 Status in one paragraph

Written 2026-08-13, branched from master at `08f66b8` and **merged after stage
13** — so §18 is stage 13's and this is §25, and every gate below was re-run
from master's checkout after that merge rather than carried over from the
branch, which is the arrangement §12.12, §14.11, §15.13 and §17.1 all used. The
merge touched `state.rs` and `platform/mod.rs`, which stage 13 had edited
heavily for popup lifecycle and cursor-surface suppression, and neither
conflicted: this stage's `Window::minimized` and `ToUi::IdleInhibit` land beside
stage 13's `ToUi::MovePopup` rather than on top of it. Four items, grouped
because they are small and share files, and the first of them is not a feature
at all — it is a live foot-gun that has been in the tree since stage 11.
`xdg-toplevel-icon-v1` is genuinely complete in the compositor: `set_icon`, the
commit-time read of the double-buffered state, buffers through the
[`PixelSource`] list so an arena buffer works like any other, the `icon_name`
fallback, the size list announced at bind (§16.4). It was **not** in
`crates/wc-proxy/protocols/` and not in `Spec::builtin`. A guest client that
binds that global gets `Error::UnknownOpcode` on its first request and the proxy
ends the connection. The other three items are new globals:
`zwp_idle_inhibit_manager_v1`, `xdg_activation_v1` and
`zwp_keyboard_shortcuts_inhibit_v1`. All four now pass
`scripts/wc-inhibit.sh` in both modes — against the Linux compositor directly,
and against `wc-compositor.exe` through `wc-proxy`, which is the real topology.

### 25.2 The trap, and why nothing had tripped it

`Session::client_message` does this, in this order:

```rust
let interface = self.objects.interface(header.object).ok_or(...)?;
let msg = self.spec.request(&interface, header.opcode).ok_or_else(...)?;
let spans = wire::walk(body, &msg.args, FdEncoding::OutOfBand)...;
```

The opcode is resolved against the vendored XML **before** anything is walked.
So an interface the table does not know is fatal on the first message on that
object, whatever the message is. There is no fd involved and no walk attempted;
there is nothing to walk *with*.

The note the proxy wrote when it saw the bind said otherwise:

> client bound {iface}, which is not in the vendored protocols; messages on it
> cannot be walked and the connection will fail **if it ever carries an `h`**

That sentence is wrong and it is wrong in the most expensive direction: it tells
a reader that an fd-free protocol is safe to leave out, which is exactly the
conclusion that produced this bug. Four of the eight interfaces stage 21 adds
carry no fd anywhere and all four would have killed a connection. The note now
says "its first request cannot be walked and will end the connection".

Why nothing noticed for two stages is the part worth keeping. **A bind always
succeeds.** `wl_registry.bind` is a message on `wl_registry`, which is in the
table; the interface name is a string argument the proxy copies through, and the
new object is recorded under a name the proxy does not recognise without
anything going wrong yet. The failure is one message later, in the client. So
the shape of the bug is:

* the compositor advertises the global — correct, and it really is implemented;
* the client binds it and feature-detection *succeeds*;
* the client sends its first request and the connection dies.

That is strictly worse than not advertising it. §14.9 already names two
independent cases of "implemented" meaning "compiled"; this is a third kind —
"implemented" meaning "implemented in one of the two processes it takes". The
compositor's implementation was never the problem and reviewing the compositor
would never have found it.

The two halves cannot depend on each other: `wc-compositor` is a Windows binary
and `wc-proxy` is a Linux one, and they are only ever in one process during a
Wine smoke test. So there is no type that can enforce the agreement. What there
is now is `spec::tests::every_global_the_compositor_advertises_can_be_walked`,
which lists the compositor's globals by hand and asserts each has a request 0 in
the proxy's table. Duplication, deliberately, because the alternative is nothing.

### 25.3 What reproduced it

`scripts/wc-inhibit.sh wine`, off a stale `target/debug/wc-proxy` that predated
the fix — the binary had not been rebuilt after the XML was added. The run
printed, in the proxy's own words:

```
client bound xdg_toplevel_icon_manager_v1, which is not in the vendored
  protocols; messages on it cannot be walked and the connection will fail if
  it ever carries an h
```

four times, once per new interface, and every assertion in the gate failed. The
accident is a better demonstration than anything written on purpose: it is the
pre-fix behaviour, on the real topology, with the message that misdescribes it.
It also cost ten minutes to diagnose, which is the same ten minutes the
"build before you deploy" rule exists to prevent — `cargo clippy` had been green
against a `wc-proxy` that was never built.

### 25.4 `zwp_idle_inhibit`: the call is one line, the bookkeeping is not

The Windows side is `SetThreadExecutionState(ES_CONTINUOUS |
ES_DISPLAY_REQUIRED)` to inhibit and `SetThreadExecutionState(ES_CONTINUOUS)` to
release. Three things about it are decisions rather than transcription.

**It is per *thread*.** The state belongs to the calling thread and the system
takes the union across threads; it is released when that thread exits. So the
call is made from the UI thread, which is `main`'s and outlives everything, and
the Wayland side does the arithmetic and sends a `ToUi::IdleInhibit { on }` only
when the answer changes. Had it been called from the Wayland thread it would
work today and stop working silently the day that work moved.

**`ES_SYSTEM_REQUIRED` is deliberately not asked for.** The protocol asks the
compositor not to blank, lock or screensave. It says nothing about whether the
machine may suspend, and adding it would be this compositor setting a power
policy no client requested, on a guest whose host can suspend it regardless.

**`ES_DISPLAY_REQUIRED` does not stop the screen saver.** It resets the
*display* idle timer. The screen saver runs on the *input* idle timer, and a
client holding an idle inhibitor is usually playing a video — which is precisely
the case with no input. So `toplevel_proc` also refuses `WM_SYSCOMMAND` with
`SC_SCREENSAVE` or `SC_MONITORPOWER` while an inhibitor is held, which is the
documented way for a foreground application to veto it, and only while one is
held.

The bookkeeping is the part that is not a transcription of anything. smithay's
`IdleInhibitHandler::uninhibit` is called from exactly one place —
`zwp_idle_inhibitor_v1.destroy`, the client's explicit request. There is no
`destroyed` hook on that object. So a client that **crashes, is killed, or just
closes its connection** with an inhibitor alive leaves behind one that nothing
will ever revoke, and an idle inhibitor that cannot be revoked is a display that
never sleeps again for the life of the compositor.

That cannot be handled by being careful inside the handlers, because the event
never arrives. What does arrive is that the `WlSurface` stops being alive.
`State::tick_idle_inhibit` runs once per turn of the dispatch loop, sweeps
inhibitors whose surface is dead, and recomputes. It costs one
`HashMap::is_empty` in the ordinary case, because the ordinary case is that
nobody ever created one.

It also decides what "visible" means, which the protocol leaves to the
compositor: the window has committed a buffer and is not minimised. Minimising
the window playing the video is about as clear a statement as a user can make.
That is why `Window` gained a `minimized` field this stage — `FromUi::Minimized`
already arrived and nothing had kept it.

The gate creates an inhibitor, destroys it, then creates a second one and
**disconnects without destroying it**. Two on transitions and two off
transitions are asserted. One of each would pass a compositor that had the sweep
missing entirely.

### 25.5 `xdg_activation`: Windows decides, and the common case is the permitted one

`SetForegroundWindow` is not a command. Windows refuses it from a process that
is not entitled to steal the foreground; the entitlement is roughly that the
calling process already owns the foreground window, or received the last input
event, or there is no foreground window, or the foreground lock timeout has
expired.

**Every toplevel in this compositor is an `HWND` owned by one process**, however
many Wayland clients are behind them, and that turns out to matter here. "The
user clicks a link in one client's window and another client's window raises" is
a foreground request made by the process that already owns the foreground
window. Windows allows it. Two clients on two machines, one Win32 process, one
set of foreground rights — this is the topology paying a dividend rather than a
tax, which is rare enough to write down.

The case Windows refuses is a client asking unprompted while the user is in some
other application. It is right to refuse: that is the behaviour the rule exists
to stop, and a Linux compositor would apply a policy of its own for the same
reason. What the user gets instead is the flashing taskbar button —
`FlashWindowEx` with `FLASHW_TRAY | FLASHW_TIMERNOFG`, requested explicitly
rather than relying on the system's implicit flash, so the outcome does not
depend on an undocumented courtesy. `TIMERNOFG` means it stops when the window
reaches the foreground, so there is nothing to cancel.

`AllowSetForegroundWindow` is **not** called, and this is the one place the
brief and the code disagree. It grants foreground rights to a *different
process*, by pid. There is no different process. It becomes relevant the day the
compositor launches a Windows application on a client's behalf, and not before.

Token policy: single-use, and thirty seconds rather than the ten in smithay's
own example. The token is minted by the application the user clicked in and
spent by the application that is then *started*; a cold Firefox on this guest
takes longer than ten seconds to reach a surface. A limit that expires during
the ordinary case is not a security measure, it is a feature that works on a
warm cache. The pool is swept whenever a token is minted, so it is bounded by
the rate tokens are asked for — GTK requests one for every launch it
contemplates and spends only some.

**The client cannot be told.** `xdg_activation_v1` version 1 has no failure
event; `activate` is a request with no reply. So a client has no way to learn
that Windows said no, and the compositor's log is the only place the answer
exists. It says which of the two happened rather than that something was tried.

### 25.6 `zwp_keyboard_shortcuts_inhibit`: honestly complete, for nothing

Every inhibitor is granted the moment it is created. That reads like a cop-out
and is not one, and the distinction is the whole reason this item is in the
stage.

The inhibitor means "stop applying your own keyboard shortcuts and give the
client every key". `State::key` already hands every key to the focused client
with `FilterResult::Forward`, and the comment there predates this stage:

> No compositor-level bindings. Every key goes to the client, which is the right
> default for a compositor whose window manager is somebody else: Windows has
> already taken the combinations it reserves (Alt-Tab, the Windows key) before
> the message loop sees them, and intercepting more here would silently steal
> keys the application expects.

So there is no shortcut to suppress. Granting is not a stub standing in for
work; it is a true statement about what this compositor does with a key. This is
the rare case where advertising a protocol costs nothing and claims nothing
false, and it is worth naming as such because the same three lines in a
compositor that *did* have bindings would be a lie.

Two things would make it one, and both are things a later stage could do without
opening this file: adding a compositor-level key binding, and intercepting a key
in `win32::toplevel_proc` before it becomes a `FromUi::Key`. Either has to
consult `Seat::keyboard_shortcuts_inhibited` — smithay keeps the answer already;
this compositor simply has no question to ask it yet.

What it cannot deliver, and no compositor on Windows can, is the combinations
the *system* takes first: Ctrl-Alt-Del is the secure attention sequence and
Win-L is the shell's, and neither reaches a window procedure. A Linux compositor
grabbing the keyboard has the same hole for the same kind of key, so this is not
a Windows tax — but a client asking to inhibit shortcuts is often asking for
exactly those, so it is stated rather than left to be discovered.

### 25.7 The gate

`scripts/wc-inhibit.sh`, and it is a C client rather than a Rust test on
purpose. A unit test in `wc-proxy` proves the wire table; `wayland-info` proves
the globals are advertised. Neither proves that a client which binds one and
then *uses* it gets what the protocol promises — and "binds, then misbehaves" is
the failure this stage is about, because it is the one a client cannot
feature-detect its way around.

`scripts/probe-inhibit.c` is libwayland-client, built from the XML in
`crates/wc-proxy/protocols` — our vendored copies, so the probe's idea of an
opcode is the same file the compositor was generated from rather than whatever
`wayland-protocols` the machine has. It maps a real window, because an inhibitor
on an unmapped surface is one the compositor is supposed to ignore and a probe
that never mapped would pass against a compositor that ignored all of them.

What each protocol can prove differs, and the script says which is which:

| protocol | observable from the client? |
| --- | --- |
| `zwp_keyboard_shortcuts_inhibit` | **yes** — the grant is an `active` event |
| `xdg_toplevel_icon` | partly — `icon_size` events arrive; the pixels are the compositor's log |
| `xdg_activation` | partly — the token dance; the raise has no reply either way |
| `zwp_idle_inhibit` | **no** — the protocol has no events at all |

Two modes. `linux` runs the Linux build directly. `wine` runs
`wc-compositor.exe` with `wc-proxy` in between, and the proxy is not optional
there: the first attempt pointed the probe straight at the Wine socket the way
`scripts/wc-xdg.sh wine` does, and it stopped at `wl_surface commit`. That is
§2.4 arriving on schedule — the probe attaches a `wl_shm` buffer,
`wl_shm.create_pool` carries an `h`, a stock libwayland client sends it out of
band by `SCM_RIGHTS`, and Wine's AF_UNIX carries no ancillary data, so the
descriptor is absent and the parser desynchronises at that request. **A client
that maps a window cannot reach a Windows compositor without the proxy, by
construction.** Which makes the `wine` mode the better test anyway: it is the
only local run that proves §25.1's first item end to end.

Both modes pass: nine assertions in `linux` mode, ten in `wine` — the extra one
is that `wc-proxy` met no interface it could not walk, which is only checkable
where there is a proxy.

The whole table, run from master's checkout after the stage 13 merge rather than
from the branch, which is the arrangement §12.12, §14.11, §15.13 and §17.1 used:

| gate | result |
| --- | --- |
| `cargo test` | 160 passed, 0 failed (master was 158) |
| `cargo clippy`, both targets | clean |
| `scripts/wc-inhibit.sh` | PASS, 9 assertions |
| `scripts/wc-inhibit.sh wine` | PASS, 10 assertions |
| `scripts/wc-xdg.sh all` | 10 passed, 0 failed |
| `scripts/wc-proxy.sh` | 37 passed, 0 failed |

The last one is the regression that mattered, because this stage edited
`Spec::builtin` — the table every client's every message is walked against. It
covers both transports and three compositors with weston's own clients, and none
of them has heard of these four protocols, which is the point.

### 25.8 What Wine says, and what it cannot

The `wine` run reaches the Win32 calls and they return:

```
wc-compositor: window 1 activated (SetForegroundWindow)
wc-compositor: idle inhibit on  (SetThreadExecutionState 0x80000002, was 0x7)
wc-compositor: idle inhibit off (SetThreadExecutionState 0x80000000, was 0x80000002)
```

That is worth exactly what it is worth: a wrong argument width, a missing
import, a call made from the wrong thread would show up here. It says nothing
about whether a display stays on. Wine has no display to keep awake, and its
`SetForegroundWindow` answers about an X11 window on an Xvfb nobody is looking
at — it succeeded here, which on the real guest it may not. §13.6 counts four
occasions where Wine's answer and the guest's differed, and `SetThreadExecutionState`
and `SetForegroundWindow` are exactly the class of API it stubs.

Note the `was 0x7` in the first line: Wine reports a previous state of
`ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED | ES_USER_PRESENT`, which is its
stub's idea of a starting value and is not what a real thread returns. The code
only tests for the documented failure (a return of 0), so this is a curiosity
rather than a dependency — but it is a concrete example of Wine answering
plausibly and differently.

**What needs a real-guest re-check, in order of how much it would cost to be
wrong about:**

1. **`SetThreadExecutionState` actually keeps the display on.** Play a video in
   Firefox on the guest with the power plan's display timeout set low, and watch.
   The log line says what was asked for; only the guest says what happened.
2. **The `WM_SYSCOMMAND` veto reaches us.** `SC_SCREENSAVE` goes to the
   foreground window. If the guest's foreground window is not one of ours at the
   moment the timer fires, we never see it — the execution state still applies,
   the veto does not, and the difference only shows on a machine with a screen
   saver configured.
3. **`SetForegroundWindow` succeeds in the case that matters.** The claim in
   §25.5 is that clicking a link in one client's window raises another client's
   window because both are one process. That is an argument from the Windows
   rules, and it is the sort of argument that is right until the guest has a
   foreground lock timeout set differently. The test is two clients on the guest
   and a real click.
4. **`FlashWindowEx` when refused.** Only reachable by deliberately arranging for
   the compositor not to own the foreground — click into Notepad, then have a
   client activate. Worth one run, because the fallback is the entire user-visible
   behaviour in the refused case.

### 25.9 What was left out, and why

Nothing among the four. Each is either whole or not advertised. What was
considered during the stage and deliberately not built:

1. **`ext-idle-notify-v1` / `zwp_idle_notifier_v1`**, the reverse of
   idle-inhibit: the compositor tells clients when the *user* has gone idle.
   Not built, and not because it is large. Windows' answer is
   `GetLastInputInfo`, which is a poll — there is no notification — so a correct
   implementation is a timer on the UI thread whose period is the shortest
   timeout any client has requested. That is a real design with a real cost
   (a wakeup on an otherwise idle machine, on a guest, forever), and the
   protocol has clients that would then rely on it. It is a stage, not a
   footnote, and advertising a global that fired late or not at all would be
   §25.2's bug wearing a different hat.
2. **`ES_SYSTEM_REQUIRED`** — §25.4.
3. **`AllowSetForegroundWindow`** — §25.5.
4. **Validating an activation token's serial and seat.** `XdgActivationTokenData`
   carries the `(Serial, WlSeat)` the requesting client attached, and a stricter
   compositor refuses tokens without a recent input serial. Not done, because
   the serials that would be checked are ones this compositor minted from
   `SERIAL_COUNTER` in response to Win32 input, and it has no record of which of
   them was a click. Checking a serial for existence rather than for recency
   would be a validation that always passes, which is worse than none. The age
   check in §25.5 is the real limit today.
5. **Icon sizes from the guest rather than from a constant.** `[16, 32, 48]` is
   still hard-coded (§16.4); `GetSystemMetrics(SM_CXSMICON)` and `SM_CXICON` are
   the true answers and change with DPI. Unrelated to this stage, noticed while
   reading the code it touches, and left where it was.

### 25.10 What to do next

1. **Re-run `scripts/wc-inhibit.sh wine` after any protocol is added by anyone.**

   **The counts below are a moving target, and a mismatch is not a regression.**
   Three tests in `wc-proxy` assert exact figures and all three move on every
   protocol addition:

   | test | at this merge | what moves it |
   | --- | --- | --- |
   | `spec::the_builtin_protocols_parse` | 40 interfaces | any `<interface>` added |
   | `wire::every_message_in_every_protocol_round_trips` | 426 messages | any `<request>` or `<event>` added |
   | `spec::the_reverse_direction_has_fds_too` | 4 messages, unchanged | only a protocol carrying an `h` |

   The third one is the interesting one and it did **not** move this stage: an
   icon crosses as a `wl_buffer` object id, an activation token as a string, and
   an inhibitor carries nothing at all, so none of the four needs the fd shim
   and none of them can be broken by it. Having that asserted rather than
   believed is the point of the list being exact.

   The figures were 32 and 376 before this stage and four more protocols are
   being added by other branches as this is written — clipboard, cursor shapes,
   touch and the decorations work. So the next person to see 40 and 426 fail is
   almost certainly looking at somebody else's correct addition. Recount, do not
   revert: `grep -c '<interface name=' crates/wc-proxy/protocols/*.xml` answers
   the first, and the test prints its own actual for the second. The reason
   these are exact rather than a floor is §9.3's — the set of messages the proxy
   can walk is exactly the set of clients it can carry, so adding one should be
   a *visible* change to a line somebody has to edit, which is precisely the
   check that was missing when `xdg-toplevel-icon` went in without it (§25.2).
2. **The four guest re-checks in §25.8.** None needs a long session; all four
   are one interactive run with Firefox already open.
3. **Decide whether `ext-idle-notify` is a stage.** §25.9 item 1 has the design
   sketched; what it needs is somebody to say whether a periodic
   `GetLastInputInfo` on the guest is acceptable, which is a question about the
   deployment and not about the code.
