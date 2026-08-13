//! `wc-proxy` — a Wayland server to a local client and a client of the Windows
//! compositor. See `lib.rs` for what it is and why it exists.
//!
//! ```text
//!   weston-terminal          wc-proxy                wc-compositor.exe
//!   (stock libwayland)                               (Windows)
//!        │                       │                         │
//!        │  wl_shm.create_pool   │                         │
//!        │  + SCM_RIGHTS fd  ───▶│                         │
//!        │                       │  mmap the client's fd   │
//!        │                       │  publish a region       │
//!        │                       │  create_pool + name ───▶│ opens the name
//!        │  commit ─────────────▶│  copy damage ──────────▶│ blit into an HWND
//! ```
//!
//! # Why the body is behind a `cfg`
//!
//! This crate is the **host** half of a host/guest pair and is Linux-only by
//! design — see the note in `Cargo.toml`. It is nonetheless a workspace member,
//! because the project's gates are `--workspace` and a crate outside them is a
//! crate nobody lints. So on a non-Unix target it compiles to a program that
//! says what it is, rather than to a compile error or to an absence.

#[cfg(unix)]
mod run;

fn main() -> std::process::ExitCode {
    #[cfg(unix)]
    {
        run::main()
    }
    #[cfg(not(unix))]
    {
        eprintln!(
            "wc-proxy is the Linux half of the boundary and does not run here. \
             It receives the client's file descriptor, which is the one thing \
             the Windows side must never do."
        );
        std::process::ExitCode::from(2)
    }
}
