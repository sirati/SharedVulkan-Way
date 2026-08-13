//! The Wayland-only compositor executable.
//!
//! A thin shim over [`wc_compositor::run`]. Everything that was here before the
//! unification is now in `lib.rs`, unchanged apart from `fn main` becoming
//! `pub fn run` — see `docs/one-binary.md` §2.2.
//!
//! **This binary still exists, and that is deliberate.** The unified guest
//! binary is `sv-guest`, which is the compositor *and* the GPU server; this one
//! is the compositor alone. Keeping it means every harness script that predates
//! the merge still works, and it means the Windows cross gate can build the
//! Wayland half on a machine with no Vulkan driver.

fn main() -> std::process::ExitCode {
    wc_compositor::run()
}
