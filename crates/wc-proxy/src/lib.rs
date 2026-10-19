//! **wc-proxy** — a Wayland server to a local client and a client of the
//! Windows compositor, so that an unmodified Linux application can put pixels
//! in a window on Windows.
//!
//! # The problem this exists to solve, stated exactly
//!
//! docs/waycompose-design.md §2.3 made `h` arguments in-band on Windows, because Windows
//! AF_UNIX carries no ancillary data and `DuplicateHandle` would mean a
//! compositor holding `PROCESS_DUP_HANDLE` over every client. That decision is
//! right and it has one consequence, measured twice independently (§7.5, §7.8,
//! §8.6): **a stock libwayland client cannot send a buffer to the Windows
//! build.** A real Weston client reaches `xdg_toplevel` against
//! `wc-compositor.exe` and then delivers zero frames, and `weston-flower` fails
//! identically at `create_pool` on real Windows.
//!
//! docs/goal-firefox.md needs Firefox, which is a stock libwayland client. So
//! the goal was unreachable until something terminated the Wayland connection
//! on the client's side of the boundary and re-materialised the buffer on the
//! other. That is this program, and §2.4 predicted its shape:
//!
//! > "It does not foreclose Architecture B. In the 'Linux clients over a proxy'
//! > topology the proxy terminates the Wayland connection and re-materialises
//! > fds at the boundary anyway — it speaks ordinary Wayland to the Linux
//! > client and ours to the compositor. The deviation is confined to the hop
//! > that was always going to be special."
//!
//! # The rule, which constrains everything here
//!
//! **No file descriptor and no handle crosses the boundary, ever, even when
//! both sides happen to be on one machine.** The boundary is always treated as
//! a VM boundary. A descriptor created locally out of memory that side already
//! reaches is fine; a descriptor *passed* is not. A name crossing, which the
//! peer then opens itself, is the sanctioned pattern.
//!
//! The client's `wl_shm` fd is therefore received here with `SCM_RIGHTS`,
//! mapped here, and goes no further. What crosses is a
//! [`wc_handle::Descriptor`] naming memory the compositor can reach on its own:
//! a path it opens, or a window into an arena it mapped before any Wayland
//! traffic existed. See [`pool`] for the two and what each costs.
//!
//! # The modules, and why the split is where it is
//!
//! | module | what it is | testable without |
//! |---|---|---|
//! | [`icons`] | an app id, a desktop entry and an icon theme become pixels | anything |
//! | [`desktop`] | a desktop entry becomes a name, actions and the argv each runs (§92) | anything |
//! | [`iconlink`] | a second connection, on which those pixels are published | a compositor |
//! | [`spec`] | the protocol's argument signatures, read from the vendored XML | anything |
//! | [`wire`] | walking and rebuilding a message body in either dialect | anything |
//! | [`objects`] | which interface each object id has | anything |
//! | [`pool`] | where a pool's bytes go so only a name crosses | a compositor |
//! | [`session`] | the translation, as bytes-in/bytes-out | a socket |
//! | [`proxy`] | the sockets and the pump | nothing |
//!
//! Everything above `proxy` is free of I/O, which is deliberate and is the same
//! structure stage 3 gave `wc-shmclient` for the same reason (docs/waycompose-design.md
//! §8.9): the encoding is the part most likely to be quietly wrong, and a bug
//! in it presents as §2.4's silent desynchronisation rather than as an error.

//! # Linux only
//!
//! Every module below is `cfg(unix)`. This crate is the host half of a
//! host/guest pair: it exists *because* the client is on Linux and the
//! compositor is not, and its whole job — receiving a real file descriptor — is
//! the one thing the Windows side must never do. It stays a workspace member so
//! that the project's `--workspace` gates lint it, and compiles to an empty
//! library on the Windows target rather than to a second implementation of
//! something with no reason to exist there.

/// THE CLIPBOARD. The byte path's byte path: a third connection to the compositor,
/// because `wl_data_offer.receive` hands over a pipe and no descriptor may
/// cross the boundary. Unix-only for the same reason `iconlink` is — it is a
/// Wayland client of the far end and only the host side has one.
#[cfg(unix)]
pub mod clip;
/// GROWTH. Asking the compositor for another region, over the shared-memory
/// channel, and then going and finding it. `cfg(unix)` for the same reason
/// `guestram` is: it maps QEMU's backing object and forks the locator, neither
/// of which is a thing on the other side of the boundary.
/// §92. A desktop entry, its actions, and the argv each one runs: what the
/// guest's pinned button and jump list are built from.
#[cfg(unix)]
pub mod desktop;
#[cfg(unix)]
pub mod guestgrow;
#[cfg(unix)]
pub mod guestram;
#[cfg(unix)]
pub mod iconlink;
#[cfg(unix)]
pub mod icons;
#[cfg(unix)]
pub mod objects;
/// Frame callbacks answered here, a bounded number ahead of the compositor
/// (docs/waycompose-design.md §88).
#[cfg(unix)]
pub mod pace;
#[cfg(unix)]
pub mod pool;
#[cfg(unix)]
pub mod proxy;
/// The session's Wayland connections as rings in the ivshmem region, and
/// the doorbell that wakes a pump parked on one (docs/waycompose-design.md §89).
#[cfg(unix)]
pub mod rings;
/// §103.14: the macOS session's control and sound channels on the rings.
#[cfg(unix)]
pub mod macsvc;
/// §103.16: the session side of the multi-stream sound ring.
#[cfg(unix)]
pub mod sound;
#[cfg(unix)]
pub mod session;
#[cfg(unix)]
pub mod spec;
#[cfg(unix)]
pub mod wire;
