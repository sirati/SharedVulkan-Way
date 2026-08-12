//! Backend API for wayland crates
//!
//! This crate provide low-level APIs for interacting with the Wayland protocol,
//! both client-side and server-side.
//!
//! Two possible backends are provided by this crate: the system backend ([`sys`] module)
//! which relies on the system-provided wayland libraries, and the rust backend ([`rs`] module)
//! which is an alternative rust implementation of the protocol. The rust backend is always
//! available, and the system backend is controlled by the `client_system` and `server_system`
//! cargo features. The `dlopen` cargo feature ensures that the system wayland
//! libraries are loaded dynamically at runtime, so that your executable does not link them and
//! can gracefully handle their absence (for example by falling back to X11).
//!
//! Additionally the default backends are reexported as toplevel `client` and `server` modules
//! in this crate. For both client and server, the default backend is the system one if the
//! associated cargo feature is enabled, and the rust one otherwise.
//!
//! Using these reexports is the recommended way to use the crate:
//! - If you don't need the `*_system` features, an other crate enabling them will not impact your code in
//!   any way as both backends have the same API (the system backend only has more methods)
//! - If your code needs to do FFI, you just need to directly depend on `wayland-backend` with the
//!   appropriate `*_system` feature enabled, and all the other crates in your dependency tree will
//!   automatically use the `sys` backend.
//!
//! Both the `wayland-client` and `wayland-server` crates follow this principle, so everything will "Just
//! Work" when using them.
//!
//! ## Logging
//!
//! This crate can generate some runtime error message (notably when a protocol error occurs). By default
//! those messages are printed to stderr. If you activate the `log` cargo feature, they will instead be
//! piped through the `log` crate.
//!
//! ## raw-window-handle integration
//!
//! The `rwh_06` feature activates the [`HasDisplayHandle`][raw_window_handle::HasDisplayHandle] implementation
//! for the client module [`Backend`][client::Backend].
//!
//! ### Deprecated raw-window-handle versions
//!
//! While raw-window-handle 0.5 is supported via the `raw-window-handle` feature, it is deprecated and will be removed in the future.
//!
//! Note that the `client_system` feature must also be enabled for the implementation to be activated.

#![forbid(improper_ctypes)]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs, missing_debug_implementations)]
// The api modules are imported two times each, this is not accidental
#![allow(clippy::duplicate_mod)]
#![cfg_attr(unstable_coverage, feature(coverage_attribute))]
// Doc feature labels can be tested locally by running RUSTDOCFLAGS="--cfg=docsrs" cargo +nightly doc -p <crate>
#![cfg_attr(docsrs, feature(doc_cfg))]

/// Reexport of the `smallvec` crate, which is part of `wayland-backend`'s public API.
pub extern crate smallvec;

/// Helper macro for quickly making a [`Message`][crate::protocol::Message]
#[macro_export]
macro_rules! message {
    ($sender_id: expr, $opcode: expr, [$($args: expr),* $(,)?] $(,)?) => {
        $crate::protocol::Message {
            sender_id: $sender_id,
            opcode: $opcode,
            args: $crate::smallvec::smallvec![$($args),*],
        }
    }
}

// internal imports for dispatching logging depending on the `log` feature
#[cfg(feature = "log")]
#[allow(unused_imports)]
use log::{debug as log_debug, error as log_error, info as log_info, warn as log_warn};
#[cfg(not(feature = "log"))]
#[allow(unused_imports)]
use std::{
    eprintln as log_error, eprintln as log_warn, eprintln as log_info, eprintln as log_debug,
};

// WAYCOMPOSE FORK: `unix` added, and `test` removed from the gate.
//
// The sys backend is `dlib` FFI to `libwayland-client.so`/`libwayland-server.so`,
// which do not exist on Windows and are not going to, hence `unix`.
//
// `test` is removed because upstream's gate compiles this module under
// `cargo test` while the manifest asks for `wayland-sys` with **no features**,
// so every `ffi_dispatch!` in it resolves to nothing and `cargo test` fails
// with 191 errors before running anything. Upstream presumably only ever runs
// `cargo test --all-features`. Since this fork has no use for the sys backend
// at all, gating it on the features that actually enable its dependency is both
// the smaller change and the correct one.
#[cfg(all(unix, any(feature = "client_system", feature = "server_system")))]
pub mod sys;

pub mod os;
pub mod rs;

// WAYCOMPOSE FORK: the `*_system` features can only ever select a backend that
// exists, so on Windows the rust backend is not merely the default, it is the
// only choice. Written as `any(windows, not(feature))` rather than by removing
// the features so that a Cargo feature unification from somewhere else in the
// dependency graph cannot silently break the Windows build.
#[cfg(any(windows, not(feature = "client_system")))]
pub use rs::client;
#[cfg(all(unix, feature = "client_system"))]
pub use sys::client;

#[cfg(any(windows, not(feature = "server_system")))]
pub use rs::server;
#[cfg(all(unix, feature = "server_system"))]
pub use sys::server;

// WAYCOMPOSE FORK: this suite exists to check that the rust and sys backends
// behave identically — every test is instantiated four times, over each
// (client, server) pairing of the two. With the sys backend gone it tests
// nothing this fork can be wrong about, so it is gated on the same features.
//
// What still runs under `cargo test`: the wire round-trip tests in
// `rs/wire.rs`, the socket tests in `rs/socket.rs` (including
// `write_read_cycle_fd`, which is the one that proves the Unix `SCM_RIGHTS`
// path still works after the port), and `tests/null_object.rs`.
#[cfg(all(test, any(feature = "client_system", feature = "server_system")))]
mod test;

mod core_interfaces;
mod debug;
pub mod protocol;
mod types;

/*
 * These trampoline functions need to always be here because the build script cannot
 * conditionally build their C counterparts on whether the crate is tested or not...
 * They'll be optimized out when unused.
 */

#[cfg(feature = "log")]
#[no_mangle]
extern "C" fn wl_log_rust_logger_client(msg: *const std::os::raw::c_char) {
    let cstr = unsafe { std::ffi::CStr::from_ptr(msg) };
    let text = cstr.to_string_lossy();
    log::error!("{text}");
}

#[cfg(feature = "log")]
#[no_mangle]
extern "C" fn wl_log_rust_logger_server(msg: *const std::os::raw::c_char) {
    let cstr = unsafe { std::ffi::CStr::from_ptr(msg) };
    let text = cstr.to_string_lossy();
    log::error!("{text}");
}
