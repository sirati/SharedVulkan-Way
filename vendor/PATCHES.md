# What was changed in `vendor/`, and why

Three crates from the `wayland-rs` workspace, vendored and patched so that the
Wayland **server** stack builds and runs for `x86_64-pc-windows-gnu`.

| Crate | Upstream version | Lines changed | Files touched |
|---|---|---|---|
| `wayland-backend` | 0.3.16 | see below | 12 of 31 |
| `wayland-server` | 0.31.13 | see below | 5 of 6 |
| `wayland-scanner` | 0.31.11 | 5 emission sites + 2 lints | 4 of 10 |

Every changed hunk carries a `WAYCOMPOSE FORK` comment saying what it was and
why it is not that any more. `git diff` against a pristine extraction of the
same crate versions is the authoritative answer; this file is the map.

**Read `docs/design.md` §2 and §3 first** if you want the reasoning rather than
the inventory. This file is deliberately mechanical.

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

## `wayland-backend` 0.3.16

### Added

**`src/os.rs`** — new, ~290 lines. The whole portability decision in one place.

* `RawFd`, `OwnedFd`, `BorrowedFd<'a>` — on Unix, re-exports of `std`; on
  Windows, `wc_handle::Handle` and a lifetime-carrying wrapper around it. The
  raw/owned distinction collapses on Windows because a `Handle` is refcounted
  and already carries its own lifetime; upstream's `try_clone_to_owned()`
  becomes `.clone()`, which is the honest analogue of `dup()`.
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
| `Cargo.toml` | `rustix` and `wayland-sys` moved behind `[target.'cfg(unix)']`. `wc-transport` added unconditionally, `wc-handle` and `polling` for Windows. `libwayland_*` features re-declared as inert. `rs_sys_impls` test target removed. |
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

## `wayland-server` 0.31.13

| File | Change |
|---|---|
| `Cargo.toml` | `rustix` behind `cfg(unix)`; `wc-transport` added; `libwayland_1_2x` features kept but forwarding to the backend's inert ones. |
| `src/socket.rs` | the listening socket. See below. |
| `src/display.rs` | stream type; `AsFd` impl gated to Unix, with an inherent `poll_handle()` on Windows. |
| `src/lib.rs`, `src/dispatch.rs`, `src/global.rs` | fd types from `wayland_backend::os`. |

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

All now emit `wayland_backend::os::{OwnedFd, BorrowedFd}`. The scanner already
emitted `wayland_backend::protocol::*` paths, so requiring `wayland-backend` in
scope is a pre-existing constraint and not a new one.

`tests/scanner_assets/test-{client,server}-code.rs` are golden files updated to
match. The test still does its job — it catches accidental changes to code
generation — it is just comparing against the new correct output.

Two clippy lints (`parse.rs` collapsible match, `util.rs` redundant format
reference) were fixed so that `cargo clippy` is clean; neither changes
behaviour.

---

## What was NOT done, and would need doing next

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
