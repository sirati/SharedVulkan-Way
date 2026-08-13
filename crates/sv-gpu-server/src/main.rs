//! The Vulkan-only GPU server executable.
//!
//! A thin shim over [`sv_gpu_server::entry::run_main`]. Everything that was here
//! is in `entry.rs`, unchanged apart from the entry points taking an explicit
//! argument vector — see `docs/one-binary.md` §2.2.
//!
//! **This binary still exists, and that is deliberate.** `sv-guest` is the GPU
//! server *and* the compositor; this one is the GPU server alone, so every
//! harness script that predates the merge still works unchanged.

fn main() -> std::process::ExitCode {
    sv_gpu_server::entry::run_main(std::env::args().collect())
}
