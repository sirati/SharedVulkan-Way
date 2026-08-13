extern crate libc;
// `memmap2` exists only for `xkb::Keymap::new_from_fd`, which this fork gates
// to `cfg(unix)` because its signature takes an `OwnedFd`. The manifest makes
// the dependency itself Unix-only, so the `extern crate` has to agree or a
// Windows build asks for a crate that was never resolved.
#[cfg(all(feature = "wayland", unix))]
extern crate memmap2;

pub mod xkb;
