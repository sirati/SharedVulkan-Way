//! The WLCS integration module: the Wayland Conformance Suite, driving
//! `wc-compositor` from outside the process.
//!
//! Built as a `cdylib`. `wlcs <this>.so` loads it, looks up
//! [`wlcs_server_integration`], and from then on the suite's 1,228 tests are
//! run against whatever this file connects them to. See docs/waycompose-design.md §26 for
//! the choice of suite and for what a pass here does and does not mean.
//!
//! # The one assumption WLCS makes that is false here
//!
//! WLCS was written for Mir and assumes the compositor is a **library it can
//! instantiate in its own process**: `create_server` returns a struct of
//! function pointers, `start` spins the compositor's mainloop on a thread, and
//! `create_client_socket` hands back one end of a socketpair that the server
//! object already holds the other end of. Every part of that is unavailable to
//! this project. `wc-compositor` is a program, not a library; in the
//! configuration it ships in it is a *Windows* program on the far side of a
//! hypervisor; and the one thing that must never cross the boundary is a file
//! descriptor (docs/waycompose-design.md §2.1), which is the currency the whole
//! integration API is denominated in.
//!
//! So the shim is a **process supervisor and a socket client**, and the fd it
//! returns from `create_client_socket` is one it opened itself, on this
//! machine, by `connect()`ing to a listening socket:
//!
//! ```text
//!   wlcs (this process)                    wc-compositor (another process)
//!   ───────────────────                    ───────────────────────────────
//!   create_server  ─ mkdtemp
//!   start          ─ spawn ──────────────► argv, XDG_RUNTIME_DIR=rundir
//!                    wait for two sockets    ├── rundir/wc-wlcs   (wayland)
//!                                            └── rundir/control   (§26)
//!   create_client_socket ─ connect ───────► the wayland socket, one fd back
//!   position_window_absolute ─ "position"─► the control socket
//!   create_pointer → move/button  ────────► the control socket
//!   stop           ─ SIGTERM, waitpid
//! ```
//!
//! No descriptor crosses anything. The fd handed to WLCS is the client end of
//! a connection this process made, which is exactly what a Wayland client on
//! this machine would have.
//!
//! # Which compositor is on the other end, and why that is a flag
//!
//! `WC_WLCS_COMPOSITOR` names the program to spawn and `WC_WLCS_ARGS` the
//! arguments to add. That indirection is the point of the design rather than a
//! convenience: the *same* shim runs the suite against the Linux build of
//! `wc-compositor` (fast, no Win32) and against `wc-proxy` in front of a real
//! `wc-compositor.exe` (slow, the real thing), because in both cases what this
//! file needs is a program that listens on a Unix socket and answers a control
//! socket. `scripts/wc-wlcs.sh` is the harness that sets both up, and
//! docs/waycompose-design.md §26.4 is the statement of what each leg covers.
//!
//! # Two libwayland entry points, resolved rather than linked
//!
//! `position_window_absolute` is handed a client-side `wl_display*` and
//! `wl_surface*` and has to turn them into something the compositor can
//! recognise. It does that with `wl_display_get_fd` (which fd is this
//! connection? — and this file handed out every fd, so it knows which
//! connection that is) and `wl_proxy_get_id` (which object id is this
//! surface?). Both come out of the process that loaded us with `dlsym`, not
//! out of a `#[link]`, so this crate has no link-time dependency on libwayland
//! and the Windows cross gate compiles it to an empty library rather than
//! failing at the linker.

#![cfg_attr(not(unix), allow(unused))]

#[cfg(unix)]
mod imp;

#[cfg(unix)]
pub use imp::*;
