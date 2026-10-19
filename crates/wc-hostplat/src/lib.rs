//! Host integration, the host's half that calls the platform
//! (docs/waycompose-design.md §106). `wc-hostint` decides; this crate asks
//! the host's audio system for its endpoints and streams, its shell for the
//! applications that open each type, opens what Linux asked for once
//! `wc_hostint::host::Service` has passed it, and registers Linux
//! applications with the host's shell.
//!
//! The modules outside `win` and `mac` are pure and tested where the crate
//! is built.

pub mod debounce;
pub mod handlers;
pub mod pcm;
pub mod share;
pub mod token;

#[cfg(windows)]
pub mod win;

// On every target under test, for its pure `mac::plan`; the FFI inside is
// macOS-only.
#[cfg(any(target_os = "macos", test))]
pub mod mac;
