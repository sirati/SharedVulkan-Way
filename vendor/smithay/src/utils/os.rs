//! ADDED BY THE WAYCOMPOSE FORK. Not upstream, and not expected to be.
//!
//! One place that decides what a file descriptor is, so that the rest of
//! smithay can say `crate::utils::os::OwnedFd` and otherwise be left alone.
//!
//! # Why this is fork-only
//!
//! `vendor/PATCHES.md` splits this port into what upstream would take and what
//! it would not. The feature gating (`backend_dmabuf`), the `clock.rs` and
//! `sealed_file.rs` fallbacks and the `cfg(unix)` narrowing of genuinely
//! fd-shaped APIs are all in the first category — smithay has merged that class
//! three times (PRs #867, #911, #1027). This module is in the second.
//!
//! It is in the second category because on Windows `OwnedFd` here is not a
//! Windows `HANDLE`, and it is not a socket either. It is
//! `wc_handle::Handle` — a *named* kernel object, reached through
//! `wayland_backend::os`, because that is what a Wayland `h` argument carries in
//! this project (docs/design.md §2.2). Upstream smithay cannot adopt that
//! without first adopting the transport decision behind it, and upstream
//! `wayland-backend` has no `os` module at all.
//!
//! # The two kinds of descriptor, which are not the same kind
//!
//! Code that says "fd" in smithay means one of two unrelated things, and the
//! Unix type system lets them share a spelling:
//!
//! * **A protocol `h` argument** — a `wl_shm_pool`'s memory, a keymap, a
//!   clipboard transfer pipe. These come off the wire and on Windows are named
//!   sections and named pipes. That is what this module's [`OwnedFd`],
//!   [`BorrowedFd`], [`AsFd`] and friends refer to.
//!
//! * **A socket** — the listening socket, a client connection. Windows has
//!   those, as real `SOCKET`s, and `calloop` already handles them by aliasing
//!   (`use std::os::windows::io::{AsSocket as AsFd, ...}` in its
//!   `sources/generic.rs`). Code here that means "socket" should follow
//!   calloop's spelling rather than come through this module.
//!
//! Conflating them is the specific mistake this module exists to prevent. A
//! `wl_shm_pool` handle cannot be polled and a socket cannot be mapped, and on
//! Unix both are `i32` so nothing complains.

#[cfg(unix)]
pub use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

#[cfg(all(windows, not(feature = "wayland_frontend")))]
compile_error!(
    "on Windows, smithay's descriptor types come from `wayland_backend::os`, which the \
     `wayland_frontend` feature brings in. Building for Windows without `wayland_frontend` \
     would leave `crate::utils::os` with nothing to re-export. Enable `wayland_frontend`."
);

// Note what is absent on Windows: `AsRawFd`, `FromRawFd` and `IntoRawFd`.
// There, `RawFd` and `OwnedFd` are the same refcounted `wc_handle::Handle`, so
// those three traits would be conversions between a type and itself and would
// stop carrying the ownership meaning they have on Unix. A use site that really
// wants the raw form has to write `cfg(unix)` and show up in the diff.
#[cfg(windows)]
pub use wayland_backend::os::{AsFd, BorrowedFd, Mapping, OwnedFd, RawFd};

/// The payload of a protocol `h` argument, as the backend names it.
///
/// The distinction drawn in this module's second heading has a spelling on the
/// backend side too, and this is that name imported rather than re-derived: it
/// is what `Message<ObjectId, _>` carries in an `ObjectData::request`
/// signature. On Unix it is the same type as [`OwnedFd`] today, which is
/// exactly why it needs its own name before
/// `docs/waycompose-design.md` §72.3 can change what an `h` is allowed to be.
/// See `docs/waycompose-design.md` §76.
#[cfg(feature = "wayland_frontend")]
pub use wayland_backend::os::{
    Handle,
    BorrowedProtoFd, BorrowedWireFd, FdEncoding, ProtoFd, RawProtoFd, WireFd,
};

/// See [`ProtoFd`]. A named `h` is mapped rather than `mmap`ed, and the view has
/// to outlive the pointer, so whoever holds the pointer must be able to name it.
#[cfg(unix)]
pub use wayland_backend::os::Mapping;

/// The transport socket a client connection arrives on.
///
/// This is the *other* kind of descriptor described above, and it deliberately
/// does not come from the `AsFd` family here: it is `std::os::unix::net::UnixStream`
/// on Unix and a Windows `AF_UNIX` `SOCKET` on Windows, and in both cases it is
/// pollable, which a `wl_shm_pool` handle is not.
///
/// Taken from `wayland_backend::os` rather than redefined so that smithay and
/// the backend cannot disagree about what a connection is — a mismatch there
/// would surface as `insert_client` refusing a socket the listener just
/// produced.
#[cfg(feature = "wayland_frontend")]
pub use wayland_backend::os::WlStream;
