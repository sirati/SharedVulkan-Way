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
mod drag;
#[cfg(windows)]
mod win32;
#[cfg(windows)]
pub use win32::{current_layout, has_touch, keyboard_repeat, start};

#[cfg(not(windows))]
mod headless;
#[cfg(not(windows))]
pub use headless::{current_layout, has_touch, keyboard_repeat, start, Click};

/// ADDED FOR STAGE 20. The ellipse a `wl_touch.shape`/`.orientation` pair
/// describes, derived from Win32's contact rectangle by
/// [`crate::input::touch::shape_from_contact`] — which is where the reasoning
/// and the tests are, because none of it needs a window.
pub use crate::input::touch::Shape as TouchShape;

/// Identifies one toplevel, and therefore one window.
///
/// A `u64` counter rather than a pointer or a `WlSurface`: it has to cross to a
/// thread that must not touch Wayland objects, and it has to stay unique after
/// the surface is gone so a message in flight for a destroyed window is
/// ignorable rather than a use-after-free.
pub type WindowId = u64;

/// How much of the window frame the system owns.
///
/// # A flag rather than a pixel count, and stage 3 had it the wrong way round
///
/// The goal (docs/goal-firefox.md) wants two things that are in tension: native
/// Windows decorations, *and* the client painting its own UI into the title
/// bar. Windows applications resolve that with `WM_NCCALCSIZE` and
/// `DwmExtendFrameIntoClientArea` — the system frame stays, and the client area
/// is extended upward over the caption so the application draws where the title
/// bar would be while Windows keeps the border, the shadow, the snap behaviour
/// and the resize edges.
///
/// Stage 3 anticipated that with an `extend_top: i32` and three conversions
/// built on it: the surface would sit at client `y = -extend_top`, the client
/// area would be *taller* than the surface, and a `WM_SIZE` would subtract the
/// strip again. **Every one of those is the wrong shape**, and the tabs-in-title-bar stage found
/// it out by writing the `WM_NCCALCSIZE` this field was reserved for. In the
/// arrangement Win32 actually has:
///
/// * The client area is not a fixed window with content shifted up out of it.
///   `WM_NCCALCSIZE` moves the *client rectangle's own top edge* up to the top
///   of the window, so the client area grows and its origin stays at `(0, 0)`.
///   There is no `-extend_top`; nothing can be painted above a client origin,
///   because `BeginPaint` clips to the client area.
/// * The surface therefore **fills** the client area, exactly as it does with
///   the system caption in place. `client_size_for` and `surface_size_for` were
///   the identity in both arrangements, not only in the unextended one.
/// * The pixel count is not knowable here at all. It is
///   `AdjustWindowRectEx`'s top inset, which depends on the theme, on the DPI
///   *of the monitor the window is on*, and on whether the window is maximised
///   — so it is measured per window, per message, on the Win32 side, and never
///   carried across this boundary. `win32::top_inset` is the one place it is
///   computed.
///
/// What is left to say across the thread boundary is the decision, which is a
/// `bool`. The conversions are gone rather than kept as identities: three
/// functions that return their arguments are harder to read than their absence,
/// and their presence was what made the wrong model look tested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FrameStyle {
    /// Fold the system's caption into the client area, so the client's own
    /// pixels reach the top of the window and its tab strip is *inside* the
    /// frame rather than below it.
    ///
    /// `false` is stage 17's arrangement and the default: Windows draws the
    /// caption, the client draws under it.
    pub extend_top: bool,
    /// Also call `DwmExtendFrameIntoClientArea`, so DWM keeps drawing its frame
    /// behind the strip the caption used to occupy.
    ///
    /// A separate switch from `extend_top`, and it is a switch rather than an
    /// implied consequence because the two are separable and only one of them
    /// is load-bearing. `WM_NCCALCSIZE` is what moves the client rectangle;
    /// the DWM call only decides whether there is glass *behind* pixels this
    /// compositor is about to overwrite. It also makes the extended strip
    /// alpha-sensitive, which is a way for a client's own translucency to
    /// become a hole in the title bar — so being able to run the same window
    /// both ways, minutes apart, is what turns that from a worry into a
    /// measurement (§30.6). Ignored entirely when `extend_top` is `false`.
    pub dwm_glass: bool,
    /// Leave the strip where DWM drew its minimise, maximise and close buttons
    /// transparent, and forward the frame's mouse messages to
    /// `DwmDefWindowProc`, so the window keeps **the system's own** caption
    /// buttons while the client paints the rest of the caption.
    ///
    /// This is what makes the arrangement the Windows one rather than a
    /// picture of it. Without it the client's pixels cover the whole strip and
    /// the window has no caption buttons at all — no minimise, no maximise, no
    /// snap layouts, and nothing but the client's own drawing in the corner.
    /// With it, the buttons are the ones Windows drew, so their hover states,
    /// their theme and their `HTMAXBUTTON` hit test are Windows' too, and
    /// Windows 11's snap-layouts flyout works for the same reason it works
    /// anywhere else (§30.5).
    ///
    /// Needs `dwm_glass`: the transparency is only transparency inside a region
    /// `DwmExtendFrameIntoClientArea` extended, and outside one the same
    /// `FillRect` is an opaque black rectangle. §30.6 is that measurement.
    pub caption_buttons: bool,
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

/// What the pointer should look like.
///
/// ADDED FOR STAGE 12 (docs/design.md §23). One message for both halves of the
/// protocol, because Win32 has one mechanism: `wl_pointer.set_cursor` sends a
/// surface and `wp_cursor_shape_v1.set_shape` sends a name, and both end as a
/// single `SetCursor` with a single `HCURSOR`.
///
/// **The cursor is a property of the seat, not of a window**, which is why
/// there is no [`WindowId`] here. Wayland scopes the cursor to the pointer's
/// current focus and the compositor is the one that decides what the pointer
/// looks like at any moment; Win32 agrees, in the sense that `SetCursor` sets
/// the cursor for the calling thread and every window it owns. Attaching this
/// to a window would need a policy for what happens when the pointer is over a
/// window whose client last spoke about a different one, and there is no such
/// moment: only one surface has pointer focus at a time.
#[derive(Clone, Debug)]
pub enum CursorSpec {
    /// `wl_pointer.set_cursor` with a null surface. The pointer is invisible
    /// over this client's windows and reappears everywhere else, which is what
    /// a text editor hiding the pointer while typing means.
    Hidden,
    /// A named shape. Resolved to a stock `IDC_*` by [`crate::cursor`]; the
    /// name is carried rather than the ordinal so the log says what the client
    /// asked for and not only what it got.
    Named(smithay::input::pointer::CursorIcon),
    /// The client's own pixels, from `wl_pointer.set_cursor` with a surface.
    ///
    /// The hotspot is in surface coordinates, which after
    /// `CursorImageAttributes` has divided out the client scale is the same as
    /// buffer coordinates for every buffer this compositor accepts (scale 1,
    /// no transform — see `state.rs`'s commit handler).
    Image {
        /// BGRA, tightly packed, alpha **premultiplied** — the same layout and
        /// the same convention as [`IconImage`], which is what a 32-bit
        /// `CreateIconIndirect` needs.
        image: IconImage,
        hotspot_x: i32,
        hotspot_y: i32,
    },
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

    /// The pointer should look like this from now on.
    ///
    /// ADDED FOR STAGE 12. Sent on every `wl_pointer.set_cursor`, every
    /// `wp_cursor_shape_device_v1.set_shape`, every commit of the surface a
    /// client is using as its cursor, and — from smithay, without any client
    /// asking — on every pointer leave, which is what puts the arrow back.
    Cursor { spec: CursorSpec },

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
    // --- the clipboard, §29 -------------------------------------------
    //
    // Bytes, in both directions, and **never a descriptor**. That is the whole
    // shape of the clipboard feature and it is visible here: `wl_data_offer.
    // receive` hands a compositor a pipe to write into, and there is no pipe
    // between a Wayland client on Linux and a compositor in a VM, so the
    // transfer is terminated in wc-proxy and only the payload crosses. By the
    // time it reaches this queue it is a `Vec<u8>` with a mime type on it.
    //
    /// Replace the system clipboard's contents.
    ///
    /// One payload per mime type, already in the client's own encoding. The
    /// conversion to `CF_UNICODETEXT` and friends is `crate::clipboard`'s, so
    /// that it is unit-tested on the machine that runs `cargo test`; what
    /// crosses here is what the Wayland client said.
    SetClipboard { payloads: Vec<(String, Vec<u8>)> },
    /// Read the system clipboard — or the drag currently over a window — and
    /// answer with [`FromUi::ClipboardData`].
    ///
    /// **Asynchronous, and it has to be.** The clipboard can only be read on
    /// the thread that owns a window, and the Wayland thread does not own one;
    /// a synchronous call would be the Wayland thread blocking on the UI
    /// thread, which is the deadlock this file's whole design avoids.
    FetchClipboard {
        token: u64,
        /// Read the drag in progress rather than the clipboard. Two sources,
        /// because Windows keeps them apart and reading the wrong one is how a
        /// paste picks up the last thing dragged.
        from_drag: bool,
        mime: String,
    },
    /// Begin a system drag carrying these payloads.
    ///
    /// The same bytes a [`ToUi::SetClipboard`] would carry, staged the same
    /// way, because a drag's data and a selection's data are the same thing
    /// with a different gesture attached. Answered exactly once with
    /// [`FromUi::DragFinished`].
    StartDrag {
        payloads: Vec<(String, Vec<u8>)>,
        /// `wl_data_device_manager.dnd_action` bits the source permits.
        actions: u32,
    },
    /// The Wayland source withdrew its drag.
    CancelDrag,

    /// End the pump.
    Quit,
}

/// `wl_data_device_manager.dnd_action`, which both sides of the boundary speak.
///
/// Wayland's numbering rather than `DROPEFFECT`'s, because the Wayland client
/// is the one that has to understand it and the platform is the one that has to
/// convert — the same rule that puts evdev keycodes rather than virtual-key
/// codes in [`FromUi::Key`].
pub mod dnd_action {
    pub const NONE: u32 = 0;
    pub const COPY: u32 = 1;
    pub const MOVE: u32 = 2;
    pub const ASK: u32 = 4;

    /// The bits by name, for a log.
    ///
    /// Worth a function rather than `{actions:#x}`: the two vocabularies either
    /// side of this boundary use the *same numbers for different things* —
    /// `ask` is 4 here and `DROPEFFECT_LINK` is 4 on Win32 — so a trace that
    /// prints only the number is a trace in which the one mistake that matters
    /// is invisible. See `drag::to_dropeffect`.
    pub fn describe(actions: u32) -> String {
        if actions == NONE {
            return "none".to_owned();
        }
        let mut out = Vec::new();
        for (bit, name) in [(COPY, "copy"), (MOVE, "move"), (ASK, "ask")] {
            if actions & bit != 0 {
                out.push(name);
            }
        }
        if out.is_empty() {
            return format!("unknown ({actions:#x})");
        }
        out.join("|")
    }
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

    // --- the clipboard, §29 -------------------------------------------
    /// The system clipboard changed, and now holds these mime types.
    ///
    /// On Win32 this is `WM_CLIPBOARDUPDATE`, which arrives for **every**
    /// change including this compositor's own — see the note in
    /// `win32.rs::on_clipboard_update` for why that echo is kept rather than
    /// filtered, and what breaks if it is not.
    ClipboardChanged { mimes: Vec<String> },
    /// The answer to a [`ToUi::FetchClipboard`]. `None` is "the clipboard has
    /// nothing convertible to that mime type", which is an ordinary outcome
    /// rather than an error: the user copied a bitmap and the client wanted
    /// text.
    ClipboardData { token: u64, data: Option<Vec<u8>> },
    /// A drag started with [`ToUi::StartDrag`] has ended, with the action the
    /// drop target actually took. Exactly one per `StartDrag`, on every path
    /// including "the drag could not be started" — a client waiting forever on
    /// a drag that never ends is the failure this whole feature exists not to
    /// build.
    DragFinished { action: u32 },
    // --- touch, stage 20 ---------------------------------------------------
    //
    // Already in Wayland's units, like everything above: surface-local
    // coordinates, a millisecond timestamp, and a **contact id that is not
    // Windows' `pointerId`** — the mapping is `crate::input::touch::Slots` and
    // lives on the platform side, seat-wide, for the reasons written there.
    //
    // There is no `TouchEnter`/`TouchLeave` pair and that is the protocol
    // rather than an omission: `wl_touch` has no notion of a contact hovering
    // over a surface, `down` names the surface it landed on, and every
    // subsequent event for that contact belongs to that surface until it comes
    // up. Win32 agrees by accident — a touch contact is implicitly captured to
    // the window that received its `WM_POINTERDOWN` — so no `SetCapture` is
    // needed here, unlike the mouse.
    /// A contact landed on a window.
    TouchDown {
        id: WindowId,
        /// Small and dense, 0 upwards, in the protocol's numbering.
        slot: i32,
        x: f64,
        y: f64,
        /// The contact patch, when the digitiser reports one. **`None` when it
        /// does not, and then no `shape` or `orientation` event is sent at
        /// all** — the protocol makes both conditional on the device
        /// supporting shape reports, and a fabricated ellipse would be a
        /// statement this compositor cannot make.
        shape: Option<TouchShape>,
        time: u32,
    },
    /// A contact moved. Carries the shape again because Windows re-reports it
    /// on every update and it genuinely changes as a finger presses harder.
    TouchMotion {
        id: WindowId,
        slot: i32,
        x: f64,
        y: f64,
        shape: Option<TouchShape>,
        time: u32,
    },
    /// A contact lifted. No coordinates: `wl_touch.up` carries none, and the
    /// last position is whatever the last motion said.
    TouchUp { id: WindowId, slot: i32, time: u32 },
    /// End of a logically grouped set of the three above.
    ///
    /// **Not optional.** A client is entitled to accumulate everything in a
    /// frame and act on none of it until the frame closes, so a compositor
    /// that omits this leaves touches buffered indefinitely — the same class of
    /// bug as a missing `wl_pointer.frame`, and with the same symptom: input
    /// that appears to work in a debug log and does nothing on screen. Carries
    /// no window id because the frame belongs to the seat, not to a window.
    TouchFrame,
    /// The system took the gesture away — an edge swipe, the touch keyboard,
    /// a window losing its pointer capture. Every live contact is finished at
    /// once and its id may be reused, which is exactly `wl_touch.cancel`.
    TouchCancel,

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
