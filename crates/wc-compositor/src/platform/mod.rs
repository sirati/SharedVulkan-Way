//! The boundary between the Wayland thread and whatever draws windows.
//!
//! # The two threads, and why the split is here
//!
//! Windows requires a window's messages to be pumped by the thread that
//! created it, and the pump is a blocking `GetMessage` loop. The Wayland side
//! is a `poll`-shaped dispatch loop that on Windows cannot be nested inside
//! `calloop`, because `Backend::poll_fd()` returns a wepoll handle and a wepoll
//! handle is not a socket (docs/design.md §3.3). Two blocking loops, neither of
//! which can host the other, is two threads; that was settled in stage 1 and
//! this stage is where it has to be built.
//!
//! **The main thread is the UI thread.** The Wayland dispatch loop is spawned.
//! Three reasons, in order of how much they cost to get wrong:
//!
//! 1. Every smithay state object the compositor owns is thread-affine in
//!    practice (`Rc`, `RefCell`, and `wayland_server`'s own non-`Send` handles).
//!    Creating all of it inside the spawned thread's closure means **nothing has
//!    to be `Send` except the plain-data messages in this file**, which the
//!    compiler then checks for us. The reverse split would need the smithay
//!    state moved onto a thread after construction, which does not compile, or
//!    constructed on the UI thread and used from the other, which does not work.
//! 2. On real Windows a process's first thread is the one the shell, the
//!    taskbar and DDE-era APIs assume is the GUI thread. Nothing here depends on
//!    that today; it costs nothing to not find out later.
//! 3. Shutdown is a message. `WM_QUIT` ends the pump, `main` joins the Wayland
//!    thread, and the exit code comes back through a channel. The other way
//!    round, the UI thread has to be told to stop by something that is not a
//!    window message, which is the classic way to end up with a process that
//!    will not exit.
//!
//! # The handoff, in both directions
//!
//! Wayland -> UI is an `mpsc` queue plus a wakeup. **The wakeup is a
//! `PostMessage` to a hidden window, never `PostThreadMessage`**, and that is
//! not a style preference: thread messages have no window to be dispatched to,
//! so they are silently discarded by the modal loops `DefWindowProc` runs
//! during a window drag or resize. A compositor whose repaints stop the moment
//! the user grabs the title bar is exactly the failure this avoids.
//!
//! UI -> Wayland is an `mpsc` queue drained on each turn of the dispatch loop.
//! No wakeup: that loop already polls. The cost is that a click-to-close takes
//! up to one poll interval to reach the client, and the poll interval is 5 ms.
//!
//! # What crosses, and what does not
//!
//! Pixels do not cross in a message. Each window gets an `Arc<Mutex<Shadow>>`
//! at creation and both threads hold it: the Wayland thread locks it to blit a
//! commit in, the UI thread locks it to paint. [`ToUi::Present`] carries only
//! the damage rectangles, so a client committing at 60 Hz does not allocate and
//! copy a framebuffer into a queue sixty times a second.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::surface::{Rect, Shadow};

#[cfg(windows)]
mod win32;
#[cfg(windows)]
pub use win32::{current_layout, start};

#[cfg(not(windows))]
mod headless;
#[cfg(not(windows))]
pub use headless::{current_layout, start, Click};

/// Identifies one toplevel, and therefore one window.
///
/// A `u64` counter rather than a pointer or a `WlSurface`: it has to cross to a
/// thread that must not touch Wayland objects, and it has to stay unique after
/// the surface is gone so a message in flight for a destroyed window is
/// ignorable rather than a use-after-free.
pub type WindowId = u64;

/// How much of the window frame the system owns.
///
/// # Why this is a struct with a zero field rather than a constant
///
/// The goal (docs/goal-firefox.md) wants two things that are in tension: native
/// Windows decorations, *and* the client painting its own UI into the title
/// bar. Windows applications resolve that with
/// `DwmExtendFrameIntoClientArea` — the system frame stays, and the client area
/// is extended upward into it so the application draws where the title bar
/// would be while Windows keeps the glass, the snap behaviour and the buttons.
///
/// Stage 3 does not do that. It builds a plain `WS_OVERLAPPEDWINDOW` and
/// `extend_top` is always 0. The field exists so that the size arithmetic —
/// which is the part that is fiddly and the part that would otherwise be
/// written as "client rect == surface size" in six places — is **already
/// parameterised**, and so that the conversion has one home. The unit tests
/// exercise a non-zero value, so this is a tested path rather than a comment
/// about a future one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FrameStyle {
    /// Pixels of system non-client area folded into the client area for the
    /// client to draw into.
    pub extend_top: i32,
}

impl FrameStyle {
    /// Where the client's buffer origin sits inside the window's client area.
    ///
    /// `(0, 0)` while `extend_top` is 0. Once the frame is extended, the
    /// surface starts at the top of the *extended* area, which is above the
    /// ordinary client origin — so this is the number that stops the content
    /// from being drawn under the title bar.
    pub fn content_origin(self) -> (i32, i32) {
        (0, -self.extend_top)
    }

    /// The client-area size that must be requested to give the client `w`x`h`
    /// of surface.
    ///
    /// With the frame extended, the client area has to be taller than the
    /// surface by the extended strip, because the strip is part of the client
    /// area from Win32's point of view and part of the surface from Wayland's.
    pub fn client_size_for(self, w: i32, h: i32) -> (i32, i32) {
        (w, h.saturating_add(self.extend_top))
    }

    /// The inverse: the surface size implied by a client area of `w`x`h`.
    ///
    /// This is what a `WM_SIZE` has to go through before it becomes an
    /// `xdg_toplevel.configure`, and it is the reason `WM_SIZE` does not
    /// simply forward its `lParam`.
    pub fn surface_size_for(self, w: i32, h: i32) -> (i32, i32) {
        (w.max(0), (h - self.extend_top).max(0))
    }
}

/// Which edge an interactive resize is being driven from.
///
/// ADDED FOR STAGE 17. Spelled out here rather than passing Wayland's
/// `xdg_toplevel::ResizeEdge` through, for the reason the whole file exists: the
/// Wayland thread must not have to know what an `HWND` is and the platform must
/// not have to link a protocol crate. The two encodings are the same idea with
/// different numbers — Wayland's is a bitmask, Win32's `WMSZ_*` is an ordinal —
/// and the translation happens once at each end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The client named no edge, which the protocol allows and which means the
    /// resize is not being driven by a pointer on a border.
    None,
    Top,
    Bottom,
    Left,
    TopLeft,
    BottomLeft,
    Right,
    TopRight,
    BottomRight,
}

/// An icon, as pixels.
///
/// **Deliberately not a path or a resource id.** In the topology the project is
/// aimed at, the client is on a different machine from the window: `set_app_id`
/// names a desktop entry that exists in a Linux icon theme, and the pixels have
/// to be resolved there and shipped across before `WM_SETICON` can be called
/// with them (docs/goal-firefox.md). A command that carried a filename would
/// work perfectly on this machine and be unimplementable on the one that
/// matters.
///
/// Stage 11 is where something started sending it: `xdg-toplevel-icon-v1` from
/// a client that implements it, or `wc_icon_manager_v1` from a resolver on the
/// Linux side for one that does not (docs/design.md §15).
#[derive(Clone, Debug)]
pub struct IconImage {
    pub width: i32,
    pub height: i32,
    /// BGRA, tightly packed, same layout as [`Shadow`].
    pub pixels: Vec<u8>,
}

/// What sort of window to make.
///
/// ADDED FOR STAGE 9. The distinction is not cosmetic: a popup must not take
/// activation (a menu that stole focus would close itself), must not appear in
/// the taskbar, must have no caption, and must be allowed to extend past its
/// parent's edges. On Win32 that is `WS_POPUP` with an owner, which is exactly
/// what the system's own menus are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowKind {
    Toplevel,
    /// `xdg_popup`. `x` and `y` are in the parent's window-geometry
    /// coordinates, which is where `xdg_positioner` puts them.
    Popup { parent: WindowId, x: i32, y: i32 },
}

/// Wayland thread -> UI thread.
#[derive(Debug)]
pub enum ToUi {
    /// Make a window for this toplevel or popup. It starts hidden: Wayland
    /// says a surface is not mapped until it has committed a buffer, and
    /// showing an empty window first is a visible white flash.
    Create {
        id: WindowId,
        title: String,
        app_id: String,
        shadow: Arc<Mutex<Shadow>>,
        kind: WindowKind,
    },
    /// `xdg_toplevel.set_title`.
    Title { id: WindowId, title: String },
    /// `xdg_toplevel.set_app_id`. Not used for anything on the platform side;
    /// recorded so the window knows what it is, and so the Wayland side can
    /// match it against an icon somebody published for that app id
    /// (`crate::icon`).
    AppId { id: WindowId, app_id: String },
    /// Set the window and taskbar icon.
    ///
    /// **A set of sizes, not an image.** Windows asks for two — `ICON_SMALL`
    /// at `SM_CXSMICON` and `ICON_BIG` at `SM_CXICON` — and an icon theme has
    /// both drawn rather than resampled, so the choice belongs at the end that
    /// knows what Windows asked for. `crate::icon::best_match` is that choice;
    /// an empty list clears the icon back to the system's generic one.
    Icon {
        id: WindowId,
        icons: Vec<IconImage>,
    },
    /// `xdg_popup.reposition`: an already-mapped popup moves.
    ///
    /// ADDED FOR STAGE 13. Separate from [`ToUi::Map`] because a popup that is
    /// already on screen must not be re-shown — `ShowWindow` again would take
    /// it through a hide/show and flicker — and separate from a resize because
    /// the position is in the parent's coordinates and has to be converted
    /// against the owner window, which only the platform can do.
    MovePopup {
        id: WindowId,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    },
    /// The shadow has new pixels in these rectangles, in surface coordinates.
    Present { id: WindowId, damage: Vec<Rect> },
    /// The surface has committed its first buffer and may now be shown, at
    /// this surface size.
    Map { id: WindowId, width: i32, height: i32 },
    /// The client asked to be maximised, unmaximised or minimised.
    SetWindowState {
        id: WindowId,
        maximized: Option<bool>,
        minimized: Option<bool>,
    },
    /// `xdg_toplevel.move`: a drag has begun somewhere the client considers a
    /// title bar, and it wants the window manager to take over.
    ///
    /// ADDED FOR STAGE 17, and it is the **only** way a client-decorated window
    /// can be moved here. Wayland has no protocol by which a client publishes
    /// its draggable region: Firefox's `mDraggableRegion` is internal and is
    /// observable only *after* a click, as this request. So `WM_NCHITTEST` can
    /// never answer `HTCAPTION` over the tab strip, the whole surface stays
    /// `HTCLIENT`, and every drag arrives here instead. §22.8 records what that
    /// costs, which is Windows 11's snap-layout flyout.
    BeginMove { id: WindowId },
    /// `xdg_toplevel.resize`, with the edge the client named.
    BeginResize { id: WindowId, edge: Edge },
    /// `xdg_toplevel.show_window_menu`, at a point in **surface** coordinates.
    ///
    /// The menu belongs to the compositor, which here means it belongs to
    /// Windows: `GetSystemMenu` is the same menu the caption's right-click and
    /// Alt+Space produce, so a client that asks for one gets the system's
    /// rather than an imitation of it.
    WindowMenu { id: WindowId, x: i32, y: i32 },
    /// The toplevel is gone.
    Destroy { id: WindowId },

    // --- stage 21 ----------------------------------------------------------
    /// Keep the display awake, or stop keeping it awake.
    ///
    /// **Not per window, deliberately.** `zwp_idle_inhibit_manager_v1` hands
    /// out an inhibitor per surface and Win32's counterpart —
    /// `SetThreadExecutionState` — has no window argument at all: it is a
    /// property of the calling *thread*, and there is one UI thread. So the
    /// Wayland side does the "any surface still holds one?" arithmetic, where
    /// it can see which surfaces are alive and mapped, and what crosses is the
    /// answer. Sent only on a change, so a client that creates and destroys
    /// inhibitors around every video frame does not produce a Win32 call per
    /// frame.
    IdleInhibit { on: bool },
    /// Bring this window to the foreground: `xdg_activation_v1.activate`.
    ///
    /// A request rather than a command, and it is Windows that decides — see
    /// `win32::activate_window` for exactly when it is allowed to succeed and
    /// what happens when it is not.
    Activate { id: WindowId },

    /// End the pump.
    Quit,
}

/// UI thread -> Wayland thread.
#[derive(Debug)]
pub enum FromUi {
    /// The window's client area changed; the numbers are already converted to
    /// surface coordinates by [`FrameStyle::surface_size_for`].
    Resized {
        id: WindowId,
        width: i32,
        height: i32,
    },
    /// The user pressed the close button. This becomes `xdg_toplevel.close`,
    /// which is a **request**: the window does not go away until the client
    /// destroys the surface. That is Wayland's model and it is also why an
    /// unresponsive client's window cannot be closed by clicking the X — the
    /// same as on any Wayland compositor, and worth knowing before it is
    /// reported as a bug.
    CloseRequested { id: WindowId },
    /// The window was maximised or restored by the user or the system.
    Maximized { id: WindowId, on: bool },
    /// The window was minimised or restored.
    Minimized { id: WindowId, on: bool },
    /// The window gained or lost activation.
    Activated { id: WindowId, on: bool },

    // --- input, stage 6 ----------------------------------------------------
    //
    // Everything below is already in Wayland's units by the time it crosses:
    // **Linux evdev keycodes and button codes**, surface-local coordinates, and
    // a millisecond timestamp. The translation from Win32's scan codes and
    // client-area pixels happens on the platform side, in `crate::input` and
    // `FrameStyle`, for the reason the whole file exists — the Wayland thread
    // must not have to know what an `HWND` is, and the headless platform must
    // be able to produce the same events without inventing a Win32.
    /// Keyboard focus followed the foreground window.
    ///
    /// **Separate from [`FromUi::Activated`]**, which is `WM_ACTIVATE` and
    /// becomes `xdg_toplevel`'s `activated` state. This one is
    /// `WM_SETFOCUS`/`WM_KILLFOCUS` and becomes `wl_keyboard.enter`/`leave`.
    /// They usually move together and they are not the same thing: a window can
    /// be the active one while a child control holds the focus, and only the
    /// focus decides where a key goes.
    /// Keyboard focus followed the foreground window.
    ///
    /// `caps` and `num` are STAGE 13's and travel with the focus rather
    /// than being polled: they are *locked* modifiers, owned by the machine
    /// and changeable while this process is not looking, so the only correct
    /// time to read them is the moment focus is gained. Carrying them here
    /// rather than calling `GetKeyState` from the Wayland thread is what keeps
    /// the headless platform able to state them too.
    KeyboardFocus { id: WindowId, on: bool, caps: bool, num: bool },
    /// A key went down or came up, in evdev numbering.
    Key {
        id: WindowId,
        /// Linux evdev keycode — the number `wl_keyboard.key` carries, and the
        /// number `crate::input` produces. XKB's numbering is this plus eight
        /// and smithay's `Keycode` is the XKB one, so `state.rs` adds it back
        /// at the one place that talks to smithay. See the note there: the
        /// offset was got wrong first and a third-party client is what found
        /// it.
        keycode: u16,
        pressed: bool,
        /// Milliseconds, from `GetMessageTime` on Win32.
        time: u32,
    },
    /// The pointer entered the window, at surface coordinates.
    PointerEnter { id: WindowId, x: f64, y: f64, time: u32 },
    /// The pointer left the window. Needs `TrackMouseEvent` on Win32 — there is
    /// no `WM_MOUSELEAVE` without it, and its absence is silent.
    PointerLeave { id: WindowId, time: u32 },
    PointerMotion { id: WindowId, x: f64, y: f64, time: u32 },
    PointerButton {
        id: WindowId,
        /// Linux evdev button code: `BTN_LEFT` is 0x110.
        button: u32,
        pressed: bool,
        time: u32,
    },
    /// A scroll. Both axes in one message because one wheel event is one
    /// `wl_pointer.frame`, and splitting them would let a diagonal scroll
    /// arrive as two frames — which clients are entitled to treat as two
    /// separate gestures.
    PointerAxis {
        id: WindowId,
        /// Surface-local units, positive down / right, as `wl_pointer.axis`
        /// requires.
        horizontal: f64,
        vertical: f64,
        /// Hundred-and-twentieths of a wheel detent, which is exactly what
        /// Win32's `WHEEL_DELTA` counts in. `wl_pointer.axis_value120` takes
        /// these numbers unchanged, which is not a coincidence — the protocol
        /// added v120 because Windows and every high-resolution wheel already
        /// spoke it.
        v120_h: i32,
        v120_v: i32,
        time: u32,
    },
    /// The keyboard layout changed under us (`WM_INPUTLANGCHANGE`), naming one
    /// of `crate::input::LAYOUTS`.
    Layout { name: &'static str },

    /// The pump has stopped. Nothing more is coming and no window will paint
    /// again.
    Stopped,
}

/// The Wayland thread's handle to the UI thread.
///
/// `send` is infallible-looking on purpose: once the pump has stopped there is
/// nothing useful for a compositor to do about a failed window command, and
/// making every call site handle it would bury the ones that matter. The
/// [`FromUi::Stopped`] event is how the Wayland side learns to wind up.
pub struct Ui {
    tx: Sender<ToUi>,
    wake: Wake,
}

impl Ui {
    pub fn send(&self, cmd: ToUi) {
        if self.tx.send(cmd).is_ok() {
            self.wake.wake();
        }
    }
}

/// What `start` hands back to `main`: the thing that must be run on the calling
/// thread.
pub struct Pump {
    inner: PumpInner,
}

impl Pump {
    /// Run until [`ToUi::Quit`] or the platform decides to stop. Consumes the
    /// pump; the calling thread is inside the platform's event loop until it
    /// returns.
    pub fn run(self) {
        self.inner.run();
    }
}

#[cfg(windows)]
use win32::{PumpInner, Wake};

#[cfg(not(windows))]
use headless::{PumpInner, Wake};

/// What every platform's `start` builds, so the two implementations agree on
/// the plumbing and differ only in what a window is.
pub(crate) struct Started {
    pub ui: Ui,
    pub pump: Pump,
}

pub(crate) fn assemble(tx: Sender<ToUi>, wake: Wake, inner: PumpInner) -> Started {
    Started {
        ui: Ui { tx, wake },
        pump: Pump { inner },
    }
}

/// The arguments every platform's `start` takes.
pub struct PlatformConfig {
    /// Where UI events are delivered.
    pub events: Sender<FromUi>,
    /// How much frame the system owns.
    pub frame: FrameStyle,
    /// Write every presented frame here as a PPM, for the harness to read back.
    /// `None` in normal operation.
    pub dump_frames: Option<std::path::PathBuf>,
    /// Generate resize / activate / close events on a schedule.
    ///
    /// Only the headless platform honours this: on Windows those events come
    /// from `WM_SIZE`, `WM_ACTIVATE` and `WM_CLOSE` and a synthetic source
    /// would be testing the wrong thing. On Linux there is no window manager at
    /// all, so without it the entire platform-to-client direction — the resize
    /// negotiation and the close handshake — is never driven by anything.
    pub simulate_wm: bool,
    /// One scripted click, honoured by the headless platform only.
    ///
    /// ADDED FOR STAGE 9. On Win32 a click comes from `SendInput` in a separate
    /// process (`scripts/probe-input.c`), which is a better test because it is
    /// the real message path; there is no equivalent here and a menu cannot be
    /// opened without one. See [`headless::Click`].
    #[cfg(not(windows))]
    pub click: Option<headless::Click>,
}

/// Shared by both platforms: the receiving end of the command queue.
pub(crate) type Commands = Receiver<ToUi>;
