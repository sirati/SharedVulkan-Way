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
2. **`SIO_AF_UNIX_GETPEERPID` is untested.** Wine implements no `IOC_VENDOR`
   ioctl in that range at all — a deliberately invented control code returns the
   identical `WSAEOPNOTSUPP` (10045) — so peer credentials on Windows are code
   that has compiled and never run. It is written to degrade to "pid 0" rather
   than fail, which is the right shape, but the value has never been observed.
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

**The remaining verification needs the guest.** It is one command
(`scripts/wc-handshake.sh` with the binaries copied over) and it is the first
thing to do when the machine is reachable again.

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
| Stage 4: `backend_win32`, one HWND per toplevel, shm blit, pointer + keyboard | 8,000–15,000 lines | **low** — this is the research's own estimate and nothing measured here bears on it |
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

1. **Nothing has still run on Windows.** Everything executable was executed
   under Wine 11.14 staging. §4.4 stands unchanged and is the most important
   section in this document. One narrowing: libxkbcommon is pure userspace
   computation over a string — no syscalls beyond `fopen`, no sockets, no timers
   — so §7.2 is the result here where Wine is closest to conclusive. "Closest to"
   is not "is".
2. **`SIO_AF_UNIX_GETPEERPID` is still untested.** Unchanged.
3. ~~No compositor exists.~~ **Superseded.** One exists, it reaches
   `xdg_toplevel`, and it draws nothing.
4. **Level-triggered wepoll is still asserted, not observed under load.**
   Unchanged, and stage 2 did not exercise it any harder: the poll loop is still
   the sleep-poll loop of stage 1.
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

### 7.8 What to re-check on the guest, in priority order

The list from §4.4 plus stage 2's additions. This is the afternoon's agenda when
the machine is reachable.

1. **`SIO_AF_UNIX_GETPEERPID`** returns a real pid. Wine implements no
   `IOC_VENDOR` ioctl at all, so this has compiled and never run.
2. **`afunix.sys` accept/peer-address semantics.** Stage 1 found Wine reports
   `sun_family = 0` where Windows documents `AF_UNIX`; the fix works on both, but
   the divergence class is proven to exist.
3. **wepoll under load, level-triggered.** A partial read that exercises
   re-readiness. Nothing has produced one yet.
4. **Named section create/open across a process boundary**, with the real
   `Local\` namespace and the real `BCryptGenRandom` — now on the critical path
   for keymaps as well as `wl_shm`.
5. **`libxkbcommon.a` linked into a real `.exe`.** Lowest risk of the list —
   §7.2's caveat — but it is 590 KB of somebody else's C and it costs one command.
6. **The 56 MB debug binary itself.** Nothing has checked that a binary this
   size loads sensibly on real Windows; Wine's loader is not `ntdll`'s.
7. **`std::time::Instant`'s resolution**, if §7.7 item 5 is fixed with QPC.
