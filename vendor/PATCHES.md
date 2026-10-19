# What was changed in `vendor/`, and why

Three crates from the `wayland-rs` workspace, vendored and patched so that the
Wayland **server** stack builds and runs for `x86_64-pc-windows-gnu`.

| Crate | Upstream version | Lines changed | Files touched |
|---|---|---|---|
| `wayland-backend` | 0.3.17 | see below | 12 of 31 |
| `wayland-server` | 0.31.14 | see below | 5 of 6 |
| `wayland-scanner` | 0.31.11 | 5 emission sites + 2 lints | 4 of 10 |

Every changed hunk carries a `WAYCOMPOSE FORK` comment saying what it was and
why it is not that any more. `git diff` against a pristine extraction of the
same crate versions is the authoritative answer; this file is the map.

**Read `docs/design.md` §2 and §3 first** if you want the reasoning rather than
the inventory. This file is deliberately mechanical.

## Upstream refresh, 2026-10-19

Each crate was moved to its newest crates.io release by applying the upstream
release-to-release diff on top of the fork. No fork hunk conflicted, and none
was absorbed upstream, so every patch below is still carried.

| Crate | Was | Now | What upstream changed |
|---|---|---|---|
| `wayland-backend` | 0.3.16 | 0.3.17 | `ReadEventsGuard` of the `sys` client is `Send + Sync` |
| `wayland-server` | 0.31.13 | 0.31.14 | core protocol 1.26: `wl_shm` v3, `wl_shm_pool` errors, `wl_seat` v11, new formats |
| `wayland-scanner` | 0.31.11 | 0.31.11 | already the newest release |
| `xkbcommon` | 0.8.0 | 0.9.0 | `key_get_mods_for_level`; `new_from_string` goes through `from_buffer`; `new_from_fd` no longer drops the last byte |
| `smithay` | 0.7.0 | 0.7.0 | already the newest release; see below |

`smithay`'s manifest now asks for `xkbcommon` 0.9.0 instead of 0.8.0, which is
the one new change to it. smithay master already uses 0.9.0 and the API smithay
calls did not change.

**Why `smithay` stays on the 0.7.0 release and not on git master.** Master
(`023b78b1`, 2026-10-15) is 486 commits past `v0.7.0`, +24.7k/-9.8k lines in
`src/`. A trial three-way merge of this fork onto it conflicts in 16 files,
three of them modify/delete: `selection/data_device/dnd_grab.rs` and
`server_dnd_grab.rs` are gone (drag-and-drop was rewritten), and so is
`virtual_keyboard_handle.rs`. Those are the areas carrying our touch, drag and
keyboard patches. Their Windows behaviour is only proven by the VM gates, so
the port should be done together with those gates. Until then, the newest
release is the newest version this fork can vouch for.

---

## The shape of the change

The port is smaller than the research predicted, and it is worth saying why in
one paragraph because it determines how the rest of this reads.

The research (§1.4) observed that `wayland-backend`'s protocol layer "is
genuinely fd-type-agnostic already" — `Argument<Id, Fd>` and `Message<Id, Fd>`
are generic over the fd type, and the only Unix coupling is a set of `AsRawFd`
bounds used for nothing but `PartialEq` and `Display`. **That is correct, and
cashing it in is most of the port.** One new module, `src/os.rs`, decides what
an fd, a socket and a poller *are*; the other eleven files mostly stop naming
`std::os::unix` and name `crate::os` instead.

What genuinely had to be *written*, rather than redirected, is three things:
the Windows arm of the `h` argument in `rs/wire.rs`, the Windows transport in
`rs/socket.rs`, and a third `#[cfg]` path in the server's poll loop.

---

## `wayland-backend` 0.3.17

### Added

**`src/os.rs`** — new, ~290 lines. The whole portability decision in one place.

* `RawFd`, `OwnedFd`, `BorrowedFd<'a>` — on Unix, re-exports of `std`; on
  Windows, `wc_handle::Handle` and a lifetime-carrying wrapper around it. The
  raw/owned distinction collapses on Windows because a `Handle` is refcounted
  and already carries its own lifetime; upstream's `try_clone_to_owned()`
  becomes `.clone()`, which is the honest analogue of `dup()`.
* `ProtoFd`, `RawProtoFd`, `BorrowedProtoFd<'a>` — the same three types again,
  under the names the **protocol** uses. Unconditional aliases of the three
  above, and today that is all they are. They exist because on Unix the socket
  layer and an `h` argument were both `OwnedFd`, so the payload of an
  `Argument`/`Message` could not be changed without dragging `SCM_RIGHTS` with
  it; on Windows they were always distinct types and nothing had to say so.
  `docs/waycompose-design.md` §76 for the measurement, §72.3 for what the
  separation is for.
* `Handle` — `wc_handle::Handle`, re-exported so that a consumer minting a named
  object (smithay's `SealedFile`, §80) can name the type without depending on
  `wc-handle` directly.
* `WireFd` — what `ProtoFd` **is**: an `h` argument that is **either** a name the
  peer can open (`Named`, a `wc_handle::Handle`) **or** a descriptor that is
  already open (`Open`, Unix only — `SCM_RIGHTS` has no Windows equivalent, so
  that arm would be uninhabited there). The unnamed form is deliberately *not* a
  sixth `wc_handle::Descriptor` variant: `Descriptor` is a wire format meaning "a
  name a peer can open", and an `SCM_RIGHTS` fd has no name.
  `docs/waycompose-design.md` §77.
* `RawWireFd`, `BorrowedWireFd<'a>` — the send-side and borrowed forms of the
  same disjunction, which `RawProtoFd` and `BorrowedProtoFd` now alias.
* `FdEncoding` — **which dialect a connection speaks**, the thing that used to be
  a `#[cfg]` in `rs/wire.rs`. Same name and same two variants as
  `wc_proxy::wire::FdEncoding`, deliberately. Its `Default` is `cfg`-ed rather
  than derived: an unannounced peer is out-of-band on Unix and in-band on
  Windows, where there is no ancillary channel to be out of band *in*.
  `docs/waycompose-design.md` §79.
* `WlStream` — the socket type, `wc_transport::Stream` on both platforms.
* `PollHandle` — an `OwnedFd` (epoll/kqueue) on Unix, a `polling::Poller`
  (wepoll) on Windows.
* `FdRepr` — the trait that replaces the `Fd: AsRawFd` bounds. It names what
  those impls actually need (compare two handles, print one) instead of naming
  a Unix type.
* `errors::{broken_pipe, protocol, too_big}` — the four `rustix::io::Errno`
  call sites. The Unix side keeps the real `Errno`, so `raw_os_error()` is
  unchanged for anything downstream matching on it.
* `c_abi` — `wl_argument`, `wl_interface`, `wl_message`. On Unix these come from
  `wayland-sys` as before. On Windows they are declared locally, because
  `wayland-sys` fails to compile for exactly one reason: `use
  std::os::unix::io::RawFd` at the top of its `common.rs`, for the `h` member of
  a union the rust backend never reads. (Its `pkg-config` build script does
  nothing at all with no features enabled — the research's account of *why* it
  fails is not quite right, though the conclusion that it cannot be used is.)

### Changed

| File | Change |
|---|---|
| `Cargo.toml` | `rustix` and `wayland-sys` moved behind `[target.'cfg(unix)']`. `wc-transport` and `wc-handle` added unconditionally (the latter moved out of `cfg(windows)` by `docs/waycompose-design.md` §77, because `os::WireFd::Named` names a `wc_handle::Handle` on Unix too), `polling` for Windows. `libwayland_*` features re-declared as inert. `rs_sys_impls` test target removed. |
| `src/lib.rs` | `mod sys` gated on `unix` **and** on the system features rather than on `test`; `mod test` likewise. `pub use rs::client/server` made unconditional on Windows. |
| `src/protocol.rs` | `Fd: AsRawFd` → `Fd: FdRepr` (5 bounds); the two bodies that called `as_raw_fd()`; `wayland_sys::common` → `crate::os::c_abi`. |
| `src/debug.rs` | same bound substitution. |
| `src/types/server.rs` | `Credentials` fields spelled `i32`/`u32`/`u32` instead of via `rustix::process::Raw*`. **Same types**, so not an API break. |
| `src/rs/wire.rs` | the `h` argument. See below — this is the interesting one. |
| `src/rs/socket.rs` | Windows `send_msg`/`rcv_msg` (plain send/recv, no ancillary data); non-blocking set at construction because Winsock has no per-call `MSG_DONTWAIT`; `AsSocket`/`AsRawSocket` impls; `peer_credentials()`. |
| `src/rs/server_impl/client.rs` | imports; `AsSocket`/`AsRawSocket` for `Client`; `get_credentials` routed through `wc-transport` and no longer `expect()`s. |
| `src/rs/server_impl/handle.rs` | `poll_fd: PollHandle`; Windows client registration (`add_with_mode(..., PollMode::Level)`); **deregistration in `State::cleanup`**. |
| `src/rs/server_impl/common_poll.rs` | Windows `Poller` construction; Windows `dispatch_all_clients`; `poll_handle()`; deregistration on the error path. |
| `src/rs/server_impl/mod.rs`, `src/client_api.rs`, `src/server_api.rs`, `src/rs/client_impl/mod.rs` | imports, and `poll_fd`/`connection_fd` given Windows counterparts returning `BorrowedSocket`/`BorrowedHandle`. |

### The `h` argument, in `rs/wire.rs`

This is the only place the two platforms genuinely diverge, and it is about
twenty lines.

* **Unix, unchanged.** The fd is `dup()`ed into a queue for the `SCM_RIGHTS`
  control message and occupies **zero** bytes of message body.
* **Windows.** The typed descriptor is written into the body encoded exactly
  like an `array` — a `u32` length, the bytes, padded to four. The header's size
  field accounts for it automatically and the parser reads it back positionally
  from the same signature, so nothing else in the protocol machinery notices.

Two consequences are load-bearing and are documented at the call site:

1. **A descriptor transfers nothing.** Unlike `SCM_RIGHTS`, the kernel takes no
   reference; the receiver holds a *name* until it opens the object. The creator
   must keep its `Handle` alive at least as long as the protocol object it backs.
   Every real use site does this anyway, because it needs the memory for the same
   period.
2. **The Windows byte stream is not libwayland's byte stream.** See
   `docs/design.md` §2.4 for why that is acceptable and what it forecloses
   (nothing, as it turns out).

### Deregistration — a bug this port had to introduce a fix for

Upstream never removes a client from its epoll set, and it does not need to:
closing an fd removes it from every epoll set it was in. wepoll gives no such
guarantee — `polling` states that a registered source "must last until it is
deleted". A client socket dropped while still registered leaves a dangling
registration, and Windows reuses socket handles aggressively, so a later client
could inherit it and be delivered another client's readiness.

`State::cleanup` therefore deletes dead clients from the poller before dropping
them, and the error path in `dispatch_events_for` does the same. This is the
obligation the `unsafe` block around `Poller::add_with_mode` relies on.

### Deleted

* `tests/rs_sys_impls.rs` — asserts that the rust and sys backends implement the
  same traits. There is no sys backend here.
* `src/test/` is no longer compiled (gated, not deleted). Every test in it is
  instantiated four times over the (client, server) pairings of the two
  backends. It also does not build under plain `cargo test` **upstream**,
  because the manifest asks for `wayland-sys` with no features while the module
  is gated on `cfg(test)`; upstream presumably only runs `--all-features`.

  Still running under `cargo test`: the wire round-trips in `rs/wire.rs`, the
  socket tests in `rs/socket.rs` — including `write_read_cycle_fd`, which is
  what proves the Unix `SCM_RIGHTS` path still works after the port — and
  `tests/null_object.rs`.

---

## `wayland-server` 0.31.14

| File | Change |
|---|---|
| `Cargo.toml` | `rustix` behind `cfg(unix)`; `wc-transport` added; `libwayland_1_2x` features kept but forwarding to the backend's inert ones. |
| `src/socket.rs` | the listening socket. See below. |
| `src/display.rs` | stream type; `AsFd` impl gated to Unix, with an inherent `poll_handle()` on Windows. |
| `src/lib.rs`, `src/dispatch.rs`, `src/global.rs` | fd types from `wayland_backend::os`; `backend::os` re-exported, because `ProtoFd` and `FdEncoding` appear in every generated request and event (§79). |
| `src/display.rs` | `DisplayHandle::set_client_fd_encoding`, the per-connection dialect (§79). |
| `src/lib.rs` | `Resource::fd_encoding()`, so a **sender** can ask which dialect the owning client speaks before building an `h` (§80). |

### The lockfile, where Windows is *better*

`bind_absolute` loops on Unix because `open()` and `flock()` are two steps: a
concurrent unlink between them lets two processes each hold an exclusive lock on
a different file with the same name, which the `dev`/`ino` comparison exists to
detect. libbsd's `flopen` has the same loop for the same reason.

Windows has no `flock`, but it has mandatory sharing rules applied atomically at
open time. Opening the lockfile with `share_mode(0)` makes exclusivity and
creation one operation, so the race cannot occur and neither the loop nor the
`dev`/`ino` check is needed. The lock is still released by the OS on abnormal
termination, which is the property that stops a crashed compositor wedging its
own socket name.

### `accept()`, and a measurement

Upstream calls `listener.accept()` and discards the peer address. On Windows
that is not free: `uds_windows::UnixListener::accept` parses the `sockaddr_un`
and **fails the entire call** if `sun_family != AF_UNIX`. A Wayland client never
`bind()`s, so it has no address to report.

Measured under Wine 11.14 staging: `accept()` returns `len = 110` with
`sun_family = 0`, so every connection was rejected with "file descriptor did not
correspond to a Unix socket" and the server accepted nobody. That specific
behaviour is a Wine artifact — real Windows is documented to report `AF_UNIX` —
but the fix is correct on both platforms rather than a workaround: pass null for
the address, since we do not use the value and should not require it to be
well-formed. `wc_transport::accept` does that.

### `XDG_RUNTIME_DIR`

Replaced by `wc_transport::socket_dir()`, which is byte-for-byte the same
behaviour on Unix and returns `%LOCALAPPDATA%\waycompose` on Windows. Windows
AF_UNIX binds to a **real filesystem path** — not an abstract namespace and not
a `\\.\pipe\` name — so the socket has to live in a real directory.

---

## `wayland-scanner` 0.31.11

The scanner is a proc-macro: it runs on the host and never cross-compiles, so
only its *output* matters. Five emission sites named `std::os::unix::io`:

* `src/client_gen.rs` — the `use OwnedFd` preamble, `write_request`'s signature,
  and the `Type::Fd` field type.
* `src/server_gen.rs` — the same three, for `write_event`.
* `src/common.rs` — the borrowed-fd field type.

All now emit `wayland_backend::os::{ProtoFd, BorrowedProtoFd}` — the protocol's
fd rather than the socket's, `docs/waycompose-design.md` §76. The scanner already
emitted `wayland_backend::protocol::*` paths, so requiring `wayland-backend` in
scope is a pre-existing constraint and not a new one.

`tests/scanner_assets/test-{client,server}-code.rs` are golden files updated to
match. The test still does its job — it catches accidental changes to code
generation — it is just comparing against the new correct output.

Two clippy lints (`parse.rs` collapsible match, `util.rs` redundant format
reference) were fixed so that `cargo clippy` is clean; neither changes
behaviour.

---

# Stage 2 additions

Two more crates, and one change to `wayland-backend`. These are a **different
kind of fork** from the three above and the distinction is made in
`Cargo.toml`'s `exclude` comment: `wayland-backend`, `wayland-server` and
`wayland-scanner` are ours and are held to this project's lints; `smithay` and
`xkbcommon` are near-upstream and are kept rebaseable, because the plan is to
send most of these patches back (`docs/design.md` §5.3).

| Crate | Upstream version | Diff | Files touched |
|---|---|---|---|
| `smithay` | 0.7.0 | 529 added, 146 removed | 27 changed + 2 new, of 246 |
| `xkbcommon` | 0.9.0 | 39 added, 11 removed | 3 of 6 |
| `wayland-backend` (stage 2 addition) | — | `os.rs`: `AsFd` trait, `Mapping` re-export | 2 |

Of smithay's 529 added lines, **293 are code and 236 are comments**; of those
293, **51 are the two new modules** (`utils/os.rs` and `utils/clock/timespec.rs`).
So roughly **240 changed lines across 27 existing files** — against
`docs/design.md` §5.2's estimate of "300–400 changed lines". The estimate was
high, and the reason is §5.4 item 1: the `SIGBUS` handler did not have to be
reimplemented.

---

## `smithay` 0.7.0

### The feature gate, which is half the work and mentions Windows nowhere

`backend_dmabuf` is a new feature. It gates `backend::allocator::dmabuf`, the
`linux-dmabuf` protocol implementation in `wayland::dmabuf`, and the
`ImportDma`/`ImportDmaWl` renderer traits. It is in `default` and implied by
`backend_drm`, `backend_egl` and `backend_vulkan`, so **no existing
configuration changes behaviour**.

Before it, `backend/mod.rs` declared `pub mod allocator;` and
`allocator/mod.rs` declared `pub mod dmabuf;` with no `cfg` at all, so
`--no-default-features` still compiled Linux dma-buf ioctl code and its
`DMA_BUF_IOCTL_SYNC` bindings. That is a defect on its own terms — a user who
asked for no backends should not get kernel ABI — and the argument for fixing it
needs no mention of Windows, which is exactly why it should upstream.

**Measured: it removes 19 of the 56 errors (34 %).** §5.3 predicted "four
one-line gates remove 2,537 lines and 19 of the 56 errors"; the count is exact
and the number of sites was larger than four, because gating the module exposed
call sites in `renderer/mod.rs` and `desktop/wayland/` that had been hidden
behind the earlier failure.

One addition rather than a gate: a second blanket `impl ImportAll for R` bounded
on `ImportMemWl` alone. Without it, `--no-default-features --features
wayland_frontend` leaves `ImportAll` with no implementors, and a software
renderer that handles `wl_shm` perfectly well cannot be used with the `desktop`
or damage-tracking machinery.

### The portable fallbacks — the `#1027` pattern

smithay merged PR #1027, "portability: Fallback for `SealedFile`", which added
`target_os` fallback arms to `utils/sealed_file.rs` and `utils/clock.rs` for
FreeBSD. These are third branches of the same argument, and belong upstream for
the same reason.

* **`utils/clock.rs` → `utils/clock/{mod.rs,timespec.rs}`.** `ClockId` and
  `Timespec` came straight from `rustix::time`, which does not exist for
  Windows. `timespec.rs` re-exports rustix's unchanged on Unix and builds the
  two from `std` on Windows. **`Realtime` is exact** (`SystemTime::UNIX_EPOCH`
  *is* `CLOCK_REALTIME`); **`Monotonic`'s epoch is process start rather than
  boot**, which is a real behavioural difference and is documented at the top of
  that file, along with what has to replace it (`QueryPerformanceCounter`) when
  `wp_presentation` needs a client-comparable clock.

* **`utils/sealed_file.rs`.** A third arm creating a named section via
  `wc_handle`. **"Sealed" is genuinely weaker here** — a sealed `memfd` cannot
  be written by anyone, whereas a client handed a section *name* could
  `OpenFileMappingW(FILE_MAP_WRITE, …)` on it. Recorded in the doc comment
  rather than papered over; the fix is a DACL on the section, which is a
  `wc_handle` change.

* **`wayland/commit_timing/mod.rs`** now takes `Timespec` from `crate::utils`
  instead of `rustix::fs`, which is the same type on Unix.

### Stage 6: the two changes that make a keyboard possible

Both are small, both are on the path upstream, and both are consequences of
things this file already records rather than new positions.

* **`input/keyboard/xkb_config.rs`: `XkbConfig` gains `keymap: Option<&'a str>`,
  checked before the RMLVO fields.** `compile_keymap` otherwise calls
  `xkb_keymap_new_from_names`, which resolves names against the
  xkeyboard-config data tree and **must** fail on Windows (`docs/design.md`
  §7.2 arranged for it to fail visibly). Since `Seat::add_keyboard` goes through
  `compile_keymap`, there was no way to construct a `KeyboardHandle` at all;
  `set_keymap_from_string` exists but only *after* one has been made. §7.2
  predicted the shape of this exactly — *"an `XkbConfig` variant carrying a
  keymap string, which is small and probably upstreamable"* — and it is nine
  lines. `Option` so `..XkbConfig::default()` callers are untouched and Unix
  behaviour is bit-identical.

* **`utils/sealed_file.rs`: the Windows arm calls `Handle::create_shared`
  instead of `Handle::create_section`.** One word, and it is the difference
  between a keymap a client can open and one it cannot. A section is
  `Local\wc-…`, a name only the NT object manager understands; the client that
  opens a keymap is on the far side of the boundary. `create_shared` asks
  `wc_handle::publish` where objects made for a peer go and falls back to a
  section when nobody said — which is right for two Windows processes and is
  what this file already documented. See `docs/design.md` §11.2.

Nothing else in smithay changed for stage 6. The seat, the capabilities, the
pointer and the keyboard are upstream code driven from `wc-compositor`.

### Stage 20: `wl_touch.cancel` was never sent

**`input/touch/mod.rs`: `TouchInternal::cancel` loses a guard, three lines.**
The only change touch needed, and it is a plain upstream bug rather than a
portability one — nothing in it mentions Windows.

`cancel` opened with the same test `frame` uses:

```rust
if state.current.map(|c| c >= state.pending).unwrap_or(false) {
    continue;
}
```

For `frame` that reads "this slot has already had a frame for everything
pending on it, so there is nothing to terminate", and is right. For `cancel` it
reads "this slot is in a settled state, so do not cancel it" — which is every
live contact in an ordinary gesture, because an ordinary gesture is `down`,
`frame`, `motion`, `frame`, and only then the compositor deciding the stream was
a system gesture. So `wl_touch.cancel` was emitted **only** for a contact with
an unframed event outstanding, and never in the case the event exists for.

Found by measurement, not by reading: `weston-eventdemo` against this
compositor received the `down` and the `frame` and then nothing, while the
compositor's own log said it had cancelled. See `docs/waycompose-design.md`
§24.4 — this file's other references still say `docs/design.md`, which is the
Vulkan side's document since the two repositories became one, and sweeping them
belongs to whoever owns that rename rather than to this entry.

The guard goes; `state.focus.take()` already skips contacts that `up` has
released, which is the condition that was wanted. `current` is still stamped so
a later `frame` stays a no-op, and `frame_pending` is now cleared because the
protocol says "no frame event is required after the cancel event".

The failing code, the fix, the measurement that found it and what is left for a
human are written out in full under **"Upstreamable: bugs found in `smithay`
that are not about Windows", item 1**, at the bottom of this file, and are not
repeated here — one description, in the place that tracks the submission.

What belongs in *this* inventory rather than there is what it means for the
fork. §5.3's position applies at its strongest — smithay has merged this patch
class three times — so the correct end state is that **this hunk disappears from
this file** when a release contains it, rather than being carried. And until
then it is the one hunk in `vendor/smithay` that a rebase must not drop
silently: a lost feature gate fails to compile and gets noticed in a minute, and
a lost `cancel` fix compiles and quietly stops sending an event.

### `cfg(unix)` where the API is Unix by subject matter

Not "does not port yet" — cannot exist elsewhere:

* `backend/renderer/sync/mod.rs`: `Fence::export()`/`is_exportable()` traffic in
  a Linux `sync_file` fd. `SyncPoint` itself stays portable.
* `utils/fd.rs` (`DeviceFd`): an open handle on `/dev/dri/cardN`, with a
  `dev_path()` that reads `/proc/self/fd`.
* `wayland/security_context`: `create_listener` hands the compositor a *bound,
  listening* AF_UNIX socket and a hangup fd. Neither is expressible as a Windows
  `h` argument, and there is no way to transfer a listening socket over a
  connection that carries no ancillary data. Sandboxing on Windows will need
  AppContainer, not this protocol wearing a shim.

### The fd abstraction — the part that stays in the fork

`utils/os.rs` (new, 72 lines, 12 of them code). On Unix it re-exports `std`; on
Windows it re-exports `wayland_backend::os`, where `OwnedFd` is a
`wc_handle::Handle` — a *named* kernel object. It also re-exports `ProtoFd` on
both platforms, for the three `ObjectData::request` signatures in
`wayland/selection/` whose `Message<ObjectId, _>` is the backend's to name
(`docs/waycompose-design.md` §76).

`docs/design.md` §5.3 predicted this would be the piece upstream will not take,
and that is still the assessment: it presupposes this project's transport
decision, and upstream `wayland-backend` has no `os` module to re-export from.

Its module docs make one distinction that the Unix type system erases and that
is the reason it exists: **a protocol `h` argument and a socket are not the same
kind of thing.** A `wl_shm_pool` handle cannot be polled and a socket cannot be
mapped; on Unix both are `i32`. Code meaning "socket" follows calloop's
spelling (`use std::os::windows::io::{AsSocket as AsFd, …}`, which calloop 0.14
already does in `sources/generic.rs`); code meaning "`h` argument" comes through
this module.

`wayland_frontend` now also enables `wayland-backend`, which is where that
module lives.

### `wayland/shm/pool.rs` — the file §5.4 called the hardest, measured

**It was the easiest, and the reason is in `docs/design.md` §2.6.** The
process-wide `SIGBUS` handler exists because a Linux client can `ftruncate` its
pool *smaller* and fault the compositor. **A Windows section's size is fixed at
`CreateFileMappingW` time and cannot be reduced**, so the event does not exist,
and no vectored exception handler is installed. §5.4 budgeted "120–180 lines,
and it is the one estimate I would not trust without a spike" for an
`EXCEPTION_IN_PAGE_ERROR` reimplementation. **Those lines are not needed.**

What the file did need: `MemMap` carries the `wc_handle::Mapping` on Windows, not
only a pointer — on Unix `mmap` is an operation on the address space and the
pointer stands alone, whereas `MapViewOfFile` returns a view whose lifetime is
the mapping's, so dropping it dangles the pointer. And the guard is factored
into `guard_enter`/`guard_leave`, which are no-ops on Windows.

**Stage 4 changed one more thing here, and it is a diagnostic rather than a
port.** The Windows `map()` reported its failure with `debug!`, and this binary
installs no `tracing` subscriber, so the line went nowhere — while the caller in
`shm/handlers.rs` disconnected the client with `Failed to mmap fd {fd:?}`,
having the handle and not the error. Stage 4 spent an hour guessing at a
`MapViewOfFile` failure whose cause (`File not found`, because the sending side
had unlinked the object; `docs/design.md` §9.7) was already known here. Both
arms are `eprintln!` now. A protocol-fatal failure is not a debug detail, and
this is the one place in the fork where that judgement differs from upstream's.

**`with_data_mut` returns `Err(())` on Windows** rather than pretending. The
mapping is `FILE_MAP_READ`, so a `*mut u8` into it would fault on the first
store. Its only caller is the public `with_buffer_contents_mut`, which nothing
in this project uses; implementing it needs a writable `wc_handle` mapping and a
decision about section ownership.

### `wayland/virtual_keyboard` — ported rather than gated

`zwp_virtual_keyboard_v1.keymap` hands over a keymap by fd, and
`Keymap::new_from_fd` is `cfg(unix)` in the crate fork. The Windows arm maps the
section, takes the bytes and calls `new_from_string` — the same compiler entered
one function later. Gating the protocol out would have been easier and would
have been wrong: nothing about a virtual keyboard is Unix-specific.

### Mechanical, and a note on one that is not quite

Sixteen files changed `use std::os::unix::io::…` to `use crate::utils::os::…`.
`wayland/shm/handlers.rs` is the exception worth naming: it used `AsRawFd` for
nothing but a diagnostic string, and now uses `{fd:?}`, which prints the number
on Unix and stays meaningful where a descriptor is not an integer. That one is
an improvement independent of the port.

Three pre-existing upstream warnings were fixed because they fire on Linux too:
two unused `tracing::error` imports and one `fn_ptr_addr_cast` in the `sigaction`
setup.

`examples/` and `benches/` are dropped from the vendored copy, with their
`dev-dependencies` (`criterion`, `image`, `clap`, `tracing-subscriber`). They
are not part of the library and their dependency weight is real.

---

## `xkbcommon` 0.9.0

Three sites, as `docs/design.md` §5.4 item 2 predicted, plus one manifest
consequence.

1. `src/xkb/mod.rs`: `use std::os::unix::io::OwnedFd` and the `memmap2` import
   become `cfg(all(feature = "wayland", unix))`.
2. `Keymap::new_from_fd` becomes `cfg(all(feature = "wayland", unix))`. The gate
   is `unix` rather than the feature because **nothing in libxkbcommon is
   Unix-only** — it has supported Windows since 1.0.0. The Unix-ness is in the
   *signature*: an `OwnedFd` and an mmap of it.
3. `src/xkb/compose.rs`: `OsStrExt::as_bytes` on a locale name. On Windows an
   `OsStr` is potentially-ill-formed UTF-16 with no lossless byte view, so the
   Windows arm uses `to_string_lossy` — a locale name that is not valid Unicode
   is not one libxkbcommon could have matched.

Manifest: `memmap2` becomes a `[target.'cfg(unix)'.dependencies]` entry, and
`src/lib.rs`'s `extern crate memmap2` is gated to agree. The `quick-evdev`
example and its `evdev` dev-dependency are dropped — `evdev` is a Linux uapi
binding that cannot build for Windows at all.

Four `elided_lifetimes` warnings from a newer rustc were fixed (`-> KeymapMods`
becomes `-> KeymapMods<'_>`). Nine further clippy warnings in untouched upstream
code are left alone; see `Cargo.toml`'s `exclude` note for why.

**This crate is `#[link(name = "xkbcommon")]`, so none of the above matters
without a library to link against.** That is `nix/libxkbcommon-mingw.nix`, and
one upstream libxkbcommon bug had to be patched to build it —
`nix/libxkbcommon-mingw-basetsd.patch`, which is written as a submittable commit.

---

## `wayland-backend`, stage 2 addition

Two additions to `src/os.rs`, both for smithay's benefit:

* **an `AsFd` trait for Windows.** Upstream never needed one because on Unix it
  comes from `std`. Deliberately no `AsRawFd` alongside it: on Windows the raw
  and owned forms are the same refcounted handle, so the two would collapse into
  one method and the ownership distinction would silently stop meaning anything.
* **`pub use wc_handle::Mapping`.** Whoever holds the pointer into a mapped
  section has to be able to name the thing keeping it alive, and making
  `wayland/shm/pool.rs` depend on `wc-handle` directly would have been worse.

`tests/null_object.rs` gained `#![cfg(unix)]`: it needs `UnixStream::pair()`, and
there is no `socketpair(2)` for Windows AF_UNIX. What it tests is portable; its
setup is not. This is what makes `cargo clippy --all-targets --target
x86_64-pc-windows-gnu` compile.

---

## What was NOT done, and would need doing next

* **Nothing in "Upstreamable" below has been sent upstream.** That section is
  the list and it carries its own status line; it is named here because this is
  where a reader looks for outstanding work, and because carrying a bug fix
  rather than submitting it is how a fix becomes a divergence.
* **`wayland-client` is not forked.** Nothing here needs it: `wayland-server`
  does not depend on it, and the stage-1 probe writes the wire format by hand.
  `wayland-backend`'s *client* half is ported (it is in the same crate and had
  to compile), so the remaining work is the `wayland-client` crate itself, which
  matters only for Architecture A.
* **`wayland-protocols` needs no fork, and this is now measured.**
  `wayland-protocols` 0.32.13, `wayland-protocols-wlr` 0.3.12 and
  `wayland-protocols-misc` 0.3.12 all check clean for `x86_64-pc-windows-gnu`
  against these forks, with no changes at all — observed as a side effect of the
  smithay probe in `docs/design.md` §5.1. The research's claim that their gate is
  "entirely `wayland-backend`" is confirmed.
* **smithay compiles against these forks with zero errors in the forks.** All 56
  of its Windows errors are in smithay's own source. See `docs/design.md` §5.
* **No `h` argument has crossed on a real Windows machine.** It has crossed
  under Wine (`scripts/wc-handshake.sh wine`), which exercises our code but not
  `afunix.sys`. See `docs/design.md` §4.4 for the precise limits of that claim.

---

# Upstreamable: bugs found in `smithay` that are not about Windows

Everything above is a *fork* — a change that exists because the crate had to
build or run for `x86_64-pc-windows-gnu`, and that upstream has no reason to
want. This section is the opposite: defects in `smithay` that reproduce on any
platform, that were found because this project exercised a path nobody else
had, and that upstream would probably take.

They are listed as **tasks for a human**, because sending a patch to another
project is a judgement about that project's process, its maintainers and the
wording of a bug report — none of which an agent should decide unilaterally.
Each entry is written so that the work left is *submitting* it, not
rediscovering it.

**Status: none of these has been reported upstream.** Until one is, the fork
carries a divergence that will conflict on every `smithay` bump, and the cost
of not sending it is paid again at each rebase.

---

## 1. `wl_touch.cancel` is a no-op for every settled contact

**Where.** `vendor/smithay/src/input/touch/mod.rs`, `TouchInternal::cancel`,
against smithay 0.7.0. The hunk carries a `WAYCOMPOSE FORK` comment with the
same reasoning as below.

**The bug.** `cancel` guarded its loop with the same test `frame` uses:

```rust
for state in self.focus.values_mut() {
    if state.current.map(|c| c >= state.pending).unwrap_or(false) {
        continue;                       // <-- wrong here
    }
    ...
}
```

`current >= pending` means *"this slot has already had a frame sent for
everything pending on it"*. For `frame` that is exactly right — a second frame
with nothing new to terminate is a no-op. For `cancel` it is exactly wrong,
because a settled slot is the normal state of a live contact: a real touch
stream is `down`, `frame`, `motion`, `frame`, … and at the moment a compositor
decides the gesture belongs to the system, **every live slot satisfies the
guard**. So `cancel` skips all of them and sends nothing.

**What the client sees.** Nothing. It keeps its contacts for ever, while the
compositor's own log says it cancelled them. There is no error, no warning, and
no protocol violation — the events simply never arrive, which is the hardest
class of bug to notice from either side.

**The fix**, three lines, no Windows content:

```rust
fn cancel(&mut self, data: &mut D, seat: &Seat<D>, seq: Serial) {
    for state in self.focus.values_mut() {
        state.current = Some(seq);
        state.frame_pending = None;
        if let Some((focus, _)) = state.focus.take() {
            focus.cancel(seat, data, seq);
        }
    }
}
```

Dropping the guard is safe: a slot that has already been released sends nothing
anyway, because `up` takes its focus and this arm only fires on a focus that is
still present — which is the condition the guard was reaching for. `current` is
still stamped so a `frame` arriving after a cancel stays a no-op, and
`frame_pending` is cleared because the protocol says outright that *"no frame
event is required after the cancel event"*, and a frame for a contact that has
just been told it did not happen is noise.

**How it was found.** `scripts/wc-input.sh` asserts `cancel()` **on the wire**
rather than asserting that the compositor called it. `weston-eventdemo` — an
unmodified third-party client using libwayland's own decoder — received the
`down` and the `frame` and then no cancel. See `docs/design.md` §24.4.

**Why nobody hit it before.** Cancel is rare. It fires when a compositor takes
a gesture away from a client, and most compositors on most hardware seldom do.
A test that calls `cancel()` and checks the compositor's state rather than the
client's would pass.

**What a human has to decide.** Whether to open an issue first or send the pull
request directly; smithay's maintainers may want a regression test in their own
harness rather than a description of ours, and if so, the shape of that test is
the only real work — the fix itself is settled and measured.

---

## 2. (none yet)

When the next one turns up it goes here, in the same shape: where, what, what
the client sees, the fix, how it was found, why nobody hit it, and what is left
for a human to decide.

---

## 3. `smithay`: protocol errors xdg-shell requires and upstream never raises

**Added for waycompose §45.10.** Unlike everything above, this is not a
portability change — it would apply unchanged on Linux, and it is the one entry
here that is a straightforward missing-feature report rather than a fork.

**Where.** `src/wayland/shell/xdg/handlers/wm_base.rs`, in
`xdg_wm_base::Request::GetXdgSurface`, and `src/wayland/compositor/handlers.rs`
in `wl_surface::Request::Attach`.

**What.** Upstream creates the `xdg_surface` unconditionally. xdg-shell requires
two refusals, and smithay makes neither:

* `xdg_wm_base.role` when the `wl_surface` already has a role. Upstream checks
  this on `get_toplevel` and `get_popup` but not on `get_xdg_surface`, so a
  subsurface can be handed to `get_xdg_surface` and the error arrives later, or
  not at all.
* `xdg_wm_base.invalid_surface_state` when the `wl_surface` already has a
  buffer. xdg-shell requires the first buffer to arrive after the role and
  after the initial configure is acked, so a surface that has already drawn
  cannot become an `xdg_surface`.

**The part that is not obvious**, and the reason there is a second file: "does
this surface have a buffer" cannot be answered from `SurfaceAttributes`. A
compositor consumes the buffer when it commits — waycompose does
`attrs.buffer.take()`, deliberately, so a frame's damage and callbacks are not
replayed — and after that the surface has drawn and the state says it never did.
Upstream has no record of the fact. So `BufferEverAttached` is a marker in the
surface's `data_map`, set by `wl_surface.attach` and never cleared, including
for a null buffer: the prohibition is about the surface's history rather than
its present contents.

**How it was found.** WLCS 1.8.1's `XdgSurfaceStableTest` — three tests, each
failing with "Expected protocol error not received". Checking only `current()`
and `pending()` fixed two of the three and left the committed-buffer case
failing, which is what named the consumed-buffer problem.

**Why nobody hit it before.** A compositor that never raises these errors is
indistinguishable from a correct one until a client does something illegal, and
well-behaved toolkits do not. The cost is that a buggy client gets undefined
behaviour instead of a diagnostic.

**What a human has to decide.** Whether `BufferEverAttached` is the shape
upstream wants — a `data_map` marker is cheap and public, but upstream may
prefer a private field on `PrivateSurfaceData`, or may prefer that the
compositor not consume the buffer at all. The two `post_error` calls are
uncontroversial on their own and could go upstream first, with the
committed-buffer case following once the record's shape is agreed.

* **`utils/sealed_file.rs`: the non-Linux Unix arm sizes the shm object with
  `ftruncate` and fills it through `mmap`, and keeps a copy of the bytes for
  `named()`** (waycompose-design.md §103.11). macOS POSIX shm objects support
  neither `write(2)` nor `read(2)`: `write_all` failed, so no keymap object
  existed, an in-band peer was sent no `wl_keyboard.keymap`, and every key a
  macOS user typed reached a client that could not decode it. The BSDs accept
  the same sequence, so the arm is not split further.

* **§107: two process-wide hooks for a compositor shared by several sessions**
  (waycompose-design.md §107). `wayland::shm::set_pool_check` installs a check
  every `wl_shm.create_pool` passes before its memory is mapped; the
  compositor refuses a pool whose arena slice is not the client's own
  session's, with `wl_shm.error.invalid_fd`. `wayland::set_nested_filter`
  gives `wp_viewporter`, `wp_single_pixel_buffer_manager_v1` and
  `wp_presentation` a `can_view`, so those globals are offered only to the
  sessions that asked for them. Both are no-ops until set, so every other
  user of the fork sees upstream's behaviour. Upstream would want a filter
  argument on each `State::new` instead of a global; the global keeps this a
  handful of lines.
