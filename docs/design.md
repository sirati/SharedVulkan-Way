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

## 10. Stage 6: a seat, and a client you can type into

Written 2026-08-13, on top of stage 4's proxy. Stages 1-4 built something you
could look at. This is the stage where it becomes something you can use.

### 10.1 Status in one paragraph

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

### 10.2 The keymap is an `h` in the other direction, and that is the whole
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

### 10.3 Scan codes, not virtual keys, and why that is not a preference

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

### 10.4 Layouts: what works, and exactly what does not

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

**What it gets wrong**, in order of how likely it is to matter:

1. **A layout that is not one of the four gets `us` characters.** Positions stay
   right; letters do not. A Spanish or Polish user's keyboard produces American
   letters, with no error anywhere. Fixing it is a line in
   `gen-keymap.sh`'s list, a line in `LAYOUTS` and a row in `layout_for_langid`
   — which is cheap, and is why the limit is a list rather than a design.
2. **AltGr is forwarded as Ctrl+AltGr.** Windows synthesises a left-Ctrl press
   before the right-Alt of an AltGr, and both are forwarded, so a client sees
   both. On `us` there is no AltGr level and it never shows; on `de` the third
   level is exactly where the interesting characters live. Untested and expected
   to be wrong.
3. **Dead keys and IME do not exist.** Composition on Windows happens in
   `WM_IME_*` and in `WM_CHAR`, neither of which is used, and the Wayland answer
   is `text-input-v3`, which is not implemented. So the four layouts' dead keys
   work only insofar as the *client's* xkb composes them, which for a keyboard
   with real dead keys is not the same thing. **No CJK input is possible at
   all.**
4. **Key repeat is Windows' rate only by coincidence.** `repeat_info` says 30/s
   after 500 ms, which are Windows' defaults written down rather than read with
   `SystemParametersInfo`. A user who changed theirs is not obeyed. Windows'
   own repeats — `WM_KEYDOWN` with `lParam` bit 30 set — are dropped, which is
   right: the protocol has the client run the timer, and forwarding both would
   give it two repeat streams at different rates.

### 10.5 The Win32 side, and the three omissions that are silent

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

### 10.6 The evidence, in three legs, and what each is worth

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

### 10.7 What Wine got wrong on the way, and it was three things

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

### 10.8 The bug a client found and a test could not

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
keymap could see it. That is the argument for §10.6's leg 1 in one paragraph,
and it is why the harness asserts on decoded characters rather than on keycodes.

### 10.9 The gates

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

### 10.10 What is not proven

1. **Nothing here has run on the real guest**, and for input that is a sharper
   caveat than for pixels. §9.11 item 3 still stands: `wc-proxy` cannot reach a
   guest compositor's `AF_UNIX` node at all without `probe-relay.c` over an
   `ssh -L`, so leg 2 has never been assembled anywhere but Wine.
2. **`SendInput` under Wine is not `SendInput` on Windows**, and it is a worse
   case than most of what Wine has been asked to stand in for. Real Windows
   applies UIPI and a set of `SetForegroundWindow` restrictions that Wine does
   not, and §10.7 shows the foreground rules already differ enough to change the
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
4. **AltGr, dead keys and IME.** §10.4 items 2 and 3. AltGr is expected to be
   wrong; IME is absent, so CJK input is impossible and needs `text-input-v3`.
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

### 10.11 What to re-check on the guest

Ahead of §9.11, because input reaches the guest differently again.

1. **Whether injected input reaches the compositor's window at all**, with the
   real `SendInput` and the real foreground rules. This is the item most likely
   to differ, and §10.7 is why. `probe-input.exe --post` is the fallback and its
   *agreement* with `SendInput` is itself the measurement.
2. **What `GetKeyboardLayout` returns on the real machine**, and whether it is
   one of the four shipped. Wine says `0x0809`; the guest is a different
   installation with different regional settings, and a user whose layout is not
   in the list gets American letters silently.
3. **Whether the published directory is coherent across the boundary.** This is
   §9.11 item 1 again with a different consumer: the keymap is a `SharedFile`,
   and virtio-fs without DAX would give each side its own copy. A keymap is
   written once and read once, so it is the *easiest* case for a filesystem to
   get right — which makes it a good first measurement and a bad proof that
   pixels will work.
4. **Whether `WM_MOUSELEAVE` behaves.** `TrackMouseEvent` is a `comctl32`-era
   API and Wine's implementation is its own; a guest run that never produces
   `wl_pointer.leave` would mean the arm is not sticking.
5. **The three ambiguous keys.** Pause, NumLock and PrintScreen are resolved by
   VK precisely because their scan codes differ between drivers, and Wine is one
   driver. The resolution should be driver-independent; that is a claim, and it
   has been tested against one.
