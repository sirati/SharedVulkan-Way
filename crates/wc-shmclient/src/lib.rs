//! A Wayland client that maps a window and animates it, writing the wire
//! format by hand.
//!
//! # Why this has to exist, and why that is a cost rather than a choice
//!
//! Stage 2 established the most expensive fact in this project
//! (docs/waycompose-design.md §7.5): **no third-party Wayland client can send a buffer to
//! the Windows build.** An `h` argument travels out-of-band by `SCM_RIGHTS` on
//! Unix and in-band as a length-prefixed descriptor on Windows (§2.3); Wine's
//! AF_UNIX carries no ancillary data; so the first `wl_shm.create_pool` from a
//! stock libwayland client leaves the descriptor simply absent from the byte
//! stream, and the server's parser desynchronises at exactly that request.
//! Every Wayland client in existence is a stock libwayland client.
//!
//! `scripts/wc-window.sh` **measures** that rather than asserting it: it runs a
//! real Weston client against `wc-compositor.exe` and shows the toplevel being
//! created and then zero frames arriving.
//!
//! So the Windows half of stage 3 needs a client that speaks the dialect, and
//! this is it. **That is a genuine weakening of the evidence and should be read
//! as one.** `weston-terminal` on Linux cannot share a bug with the compositor;
//! this program can, because it is ours. What it proves is narrower and still
//! worth having, because nothing else can prove it at all: that a `wl_shm`
//! buffer created as a named section by one Windows process is opened by name
//! and mapped by another, blitted into a real `HWND` by GDI, and shown. The two
//! halves of the stage's evidence are reported separately for that reason.
//!
//! Its other job is to be the client the Architecture B proxy is checked
//! against — and its last, because once that proxy exists a Linux Firefox can
//! reach the Windows compositor directly and this program is redundant.
//!
//! # What it does
//!
//! Binds `wl_compositor`, `wl_shm` and `xdg_wm_base`; creates a surface, an
//! `xdg_surface` and an `xdg_toplevel`; sets a title and an app id; waits for
//! the configure and acks it; allocates one section holding **two** buffers;
//! then loops: draw, attach, damage, frame, commit — honouring
//! `wl_buffer.release` and `wl_callback.done` as a real client must.
//!
//! Double buffering is not decoration. A single-buffered client cannot tell a
//! compositor that *holds* its buffer from one that copies and releases, so a
//! compositor with a use-after-release bug would look fine to it. Two buffers
//! plus a strict "never draw into a buffer that has not been released" rule is
//! what makes the compositor's release timing observable at all.
//!
//! `--partial-damage` makes the client damage only the rectangle it changed.
//! That is the one thing no available Weston client does — every one of them
//! sends a single full-surface damage rectangle — so without it the damage path
//! is exercised only by unit tests. With it, a compositor that ignored damage
//! would still produce a correct window, and one that mis-clipped would produce
//! a visibly wrong one.
//!
//! # Why this is a library and not only a binary
//!
//! Because `cargo test` cannot run for the Windows target (docs/waycompose-design.md
//! §4.1). Everything about *what bytes* go on the wire lives in [`proto`],
//! which builds on both platforms and is unit-tested on the one where tests
//! run; only the socket and the loop are behind `cfg(windows)`. Without that
//! split, the most error-prone part of this client — a size field counted from
//! the wrong place, a byte of padding missing — would be checked by the
//! compiler and by nothing else.

pub mod proto;

#[cfg(windows)]
mod win;

use std::process::ExitCode;

pub struct Config {
    pub width: i32,
    pub height: i32,
    pub frames: u32,
    pub partial: bool,
    pub title: String,
    pub app_id: String,
    /// A second `xdg_toplevel.set_app_id`, sent after the window is on screen.
    ///
    /// ADDED FOR STAGE 32. The protocol allows `set_app_id` at any time and the
    /// taskbar identity has to follow it, but no toolkit does it late — they
    /// all name themselves before they draw. So the late case cannot be
    /// measured with a client anybody ships, and this is the flag that makes it
    /// measurable with one we wrote. See [`proto::set_app_id`].
    pub late_app_id: Option<String>,
    /// How many frames to draw before sending it.
    pub late_after: u32,
    /// How long the loop may run before it gives up, in seconds.
    ///
    /// There has always been a deadline here — a client that hangs waiting for
    /// a compositor that will never answer has to end by itself. Sixty seconds
    /// was the constant; it is a flag as of STAGE 32 because a photograph of
    /// the guest's desktop is taken by a scheduled task and registering,
    /// starting, waiting for and unregistering one costs the better part of a
    /// minute (§16.6). A window that is gone before the camera arrives makes a
    /// picture of the wrong thing, which that stage paid for once.
    pub seconds: u64,
    pub path_arg: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 400,
            height: 300,
            frames: 60,
            partial: false,
            title: "waycompose shm client".to_string(),
            app_id: "org.waycompose.shmclient".to_string(),
            late_app_id: None,
            late_after: 30,
            seconds: 60,
            path_arg: None,
        }
    }
}

#[cfg(windows)]
pub fn run(config: Config) -> ExitCode {
    win::run(config)
}

/// On Unix there is nothing here to test.
///
/// The Windows arm exists because a stock libwayland client cannot send an `h`
/// to the Windows build. On Unix a stock libwayland client works perfectly and
/// a dozen of them are installed — `weston-terminal` and
/// `weston-presentation-shm` are what `scripts/wc-window.sh` uses — so
/// hand-rolling `SCM_RIGHTS` here would mean reimplementing libwayland in order
/// to test it against itself.
#[cfg(unix)]
pub fn run(_config: Config) -> ExitCode {
    println!(
        "wc-shmclient: SKIP — this client exists to send an `h` in the Windows \
         in-band encoding. On Unix an `h` travels by SCM_RIGHTS and real \
         libwayland clients work; use one of those."
    );
    ExitCode::SUCCESS
}
