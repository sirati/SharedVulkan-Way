//! The compositor's Wayland-side state, and the handlers that turn protocol
//! into window commands.
//!
//! Everything in this file runs on the Wayland thread and only there. Nothing
//! here touches Win32; the only way out is [`Ui::send`].

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use smithay::input::keyboard::{FilterResult, KeyboardHandle, XkbConfig};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle,
};
// ADDED FOR STAGE 20. smithay's touch handle is the same shape as the pointer
// one — it owns the per-contact focus map and the `frame` bookkeeping, so this
// file supplies events and never a `WlTouch`.
use smithay::input::touch::{
    DownEvent, MotionEvent as TouchMotionEvent, OrientationEvent, ShapeEvent, TouchHandle, UpEvent,
};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
// smithay's own input vocabulary rather than `wayland_server`'s protocol enums.
// They have the same names and are different types: the protocol ones are the
// wire encoding of one interface's argument, and these are what the input
// handles take, because a `KeyState` also has to describe a key from a
// libinput device that never touched the protocol. Naming the wrong one
// compiles nowhere, which is the good case — but the compiler's message says
// only "similar names", so the reason they are two types is worth recording.
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::reexports::wayland_server::protocol::{
    wl_buffer, wl_output, wl_seat, wl_surface::WlSurface,
};
use smithay::utils::{Logical, Point, Serial, SERIAL_COUNTER};
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, Damage,
    SurfaceAttributes,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
// ADDED FOR STAGE 17: the edge a client names when it asks to be resized.
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::ResizeEdge;
use smithay::wayland::shm::{ShmHandler, ShmState};
// ADDED FOR STAGE 9. Both of these are Firefox requirements measured rather
// than guessed at, and they are requirements for different reasons — see
// docs/waycompose-design.md §13.3.
//
// `Output` is the monitor. Without one a GTK client has an empty
// `GdkDisplay` monitor list, sizes its first window from a fallback and never
// learns a scale factor; Firefox came up 552x172 in a run where the compositor
// advertised no `wl_output` at all.
//
// `DataDeviceState` is the clipboard, and it is load-bearing for something that
// has nothing to do with clipboards: GTK 3 **defers creating a seat** until
// `wl_data_device_manager` appears, so a compositor with a perfectly good
// `wl_seat` and no data device gives a GTK client no keyboard and no pointer
// at all. §13.4 has the trace.
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::utils::Transform;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::SelectionHandler;
// ADDED FOR STAGE 9: **both** decoration protocols, advertised together, so
// that which one Firefox honours is a measurement and not a belief.
//
// The research said GTK does not implement `xdg-decoration` and uses KDE's
// instead. That is checkable two ways and both were done: `libgdk-3.so`
// contains the string `org_kde_kwin_server_decoration_manager` and does not
// contain `zxdg_decoration_manager_v1`, and a compositor that offers both is
// told which one is bound. docs/waycompose-design.md §13.7 has the answer and what it
// costs.
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server
    ::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols_misc::server_decoration::server
    ::org_kde_kwin_server_decoration::{Mode as KdeMode, OrgKdeKwinServerDecoration};
use smithay::reexports::wayland_protocols_misc::server_decoration::server
    ::org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode;
use smithay::reexports::wayland_server::WEnum;
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};

// ADDED FOR STAGE 12 (docs/waycompose-design.md §23), the cursor.
//
// `CursorShapeManagerState` is `wp_cursor_shape_v1`: a client names a shape
// instead of shipping pixels, which is what GTK4 binds and is the half of the
// problem that maps onto `LoadCursorW` with nothing in between.
//
// `TabletSeatHandler` is here for a reason that is not about tablets.
// `wp_cursor_shape_manager_v1` has a `get_tablet_tool_v2` request beside its
// `get_pointer`, so smithay's dispatch for the manager is bounded on both
// handlers and the protocol cannot be delegated without implementing it. The
// trait's one method has a default body and this compositor keeps it: there is
// no `zwp_tablet_manager_v2` global, so no client can ever obtain a tablet tool
// to set an image on.
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::seat::CURSOR_IMAGE_ROLE;
use smithay::wayland::tablet_manager::TabletSeatHandler;
// ADDED FOR STAGE 21: three protocols whose implementation is a Win32 call, and
// which are here because the Win32 call exists rather than because the protocol
// is short. docs/waycompose-design.md §25 has the argument for each and — for the two that
// were considered and left out — for their absence.
use smithay::wayland::idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState};
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::xdg_activation::{
    XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
};
use smithay::{
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_idle_inhibit,
    delegate_kde_decoration, delegate_keyboard_shortcuts_inhibit, delegate_output, delegate_seat,
    delegate_shm, delegate_xdg_activation, delegate_xdg_decoration, delegate_xdg_shell,
};

use crate::platform::{CursorSpec, Edge, FromUi, IconImage, ToUi, Ui, WindowId, WindowKind};
use crate::surface::{
    bars, decorator, Bars, PixelError, PixelFormat, PixelSource, Rect, Shadow, ShmSource,
};

/// Which decoration mode the compositor advertises, and answers with.
///
/// ADDED FOR STAGE 17, and it is the whole of docs/waycompose-design.md §22.2. Until this
/// stage the answer was hard-coded to `Client`, on the reasoning that
/// docs/goal-firefox.md wants Firefox painting its tabs into the title bar and
/// only a client that draws a title bar can do that. The reasoning was right
/// and the *default* was wrong: the compositor was promising a frame it had not
/// built, so every window came up with Windows' caption **and** the client's,
/// one directly above the other, which is the bug the user reported.
///
/// So the mode is a flag with `Server` as its default — Windows' caption is a
/// real server-side decoration and claiming it is the honest answer — and
/// `Client` is what the frame-extension work of §22.7 is developed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Decorations {
    /// Windows draws the caption; the client draws none. **The default.**
    #[default]
    Server,
    /// The client draws its own title bar, and Windows draws its caption above
    /// it. This is the two-title-bar arrangement every stage before 17 shipped,
    /// kept as a flag because it is the control leg every claim about the other
    /// two is measured against.
    Client,
    /// ADDED FOR TABS IN THE TITLE BAR. The client draws its own title bar **and the system
    /// caption is folded into the client area** (`FrameStyle::extend_top`), so
    /// the client's tab strip is inside the window's own frame rather than
    /// below it.
    ///
    /// On the wire this is `Client` — it has to be, because the tab strip only
    /// exists when the client is decorating itself, and the one lever that
    /// decides that is the manager's `default_mode` (§22.2). The difference is
    /// entirely on the Win32 side, and it is `WM_NCCALCSIZE`.
    Tabs,
}

impl Decorations {
    /// Parse the `--decorations` argument. `None` for anything else.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "server" => Some(Self::Server),
            "client" => Some(Self::Client),
            "tabs" => Some(Self::Tabs),
            _ => None,
        }
    }

    /// Whether the Win32 side should fold the caption into the client area.
    pub fn extends_frame(self) -> bool {
        matches!(self, Self::Tabs)
    }

    /// The KDE protocol's spelling, which is the one that decides GTK's
    /// behaviour: `gtk_window_should_use_csd()` consults nothing else on
    /// Wayland, and it reads the **manager's** default rather than the answer
    /// to any per-surface request.
    fn kde(self) -> KdeDefaultMode {
        match self {
            Self::Server => KdeDefaultMode::Server,
            // `Tabs` is `Client` here, and that is the whole reason it is not a
            // third value on the wire: the tab strip this arrangement puts in
            // the caption is drawn by the client, and it only exists at all
            // when the manager's default says `Client`.
            Self::Client | Self::Tabs => KdeDefaultMode::Client,
        }
    }

    /// The `xdg-decoration` spelling. Measured to be used by nothing this
    /// project runs (§15.7), and answered consistently anyway.
    fn xdg(self) -> DecorationMode {
        match self {
            Self::Server => DecorationMode::ServerSide,
            Self::Client | Self::Tabs => DecorationMode::ClientSide,
        }
    }
}

/// What kind of shell surface a window is.
///
/// ADDED FOR STAGE 9. Before this stage a window was always an `xdg_toplevel`
/// and the field could be one. Firefox is menus: the hamburger, every dropdown,
/// the URL bar's autocomplete and every tooltip is an `xdg_popup`, and stage 6
/// recorded the symptom of not having them — *"the click which would open a
/// menu now arrives and nothing happens"*.
enum Role {
    Toplevel(ToplevelSurface),
    /// A menu, dropdown or tooltip. Its geometry comes from an
    /// `xdg_positioner` rather than from a window manager, and it is placed
    /// relative to its parent's window-geometry rectangle.
    Popup(PopupSurface),
}

impl Role {
    fn surface(&self) -> &WlSurface {
        match self {
            Role::Toplevel(t) => t.wl_surface(),
            Role::Popup(p) => p.wl_surface(),
        }
    }

    fn toplevel(&self) -> Option<&ToplevelSurface> {
        match self {
            Role::Toplevel(t) => Some(t),
            Role::Popup(_) => None,
        }
    }
}

/// One toplevel or popup, from the Wayland side.
struct Window {
    id: WindowId,
    role: Role,
    /// Shared with the UI thread; see `platform`'s header for why pixels live
    /// here rather than in the command queue.
    shadow: Arc<Mutex<Shadow>>,
    mapped: bool,
    /// ADDED FOR STAGE 21, and only one thing reads it: an idle inhibitor on a
    /// minimised window must not keep the display awake, because a minimised
    /// window is one the protocol calls invisible. Tracked here rather than
    /// asked of Win32 because the answer has to be available on the Wayland
    /// thread, which may not call Win32 at all; the value is `WM_SIZE`'s
    /// `SIZE_MINIMIZED` arriving as [`FromUi::Minimized`].
    minimized: bool,
    title: String,
    app_id: String,
    /// Which connection this window's client arrived on.
    ///
    /// ADDED FOR STAGE 32, and it is the whole of the answer for a client that
    /// never calls `set_app_id`: there is no name to group such windows by, so
    /// they are grouped by the thing that is still true of them. Cached here
    /// rather than looked up per use, because a window outlives the moments at
    /// which its surface is convenient to ask.
    client: u32,
    /// Callbacks the client is waiting on before it draws its next frame.
    ///
    /// Held rather than answered immediately. Answering at once is legal and is
    /// what a naive compositor does; it also turns every client that draws in
    /// response to `frame` into a spin loop that renders as fast as the CPU
    /// allows, which is `weston-simple-shm` at several thousand frames a second
    /// and a machine that sounds broken.
    frame_callbacks: Vec<WlCallback>,
    presented: u64,
    /// The client has set an icon of its own through `xdg-toplevel-icon-v1`.
    ///
    /// Once it has, a publish on `wc_icon_manager_v1` for the same app id must
    /// not overwrite it: the client is the authority on its own icon, and the
    /// resolver is a guess made from a name. The reverse is not true — a
    /// client that sets an icon *after* the resolver's guess has been applied
    /// replaces it.
    icon_from_client: bool,
    /// The client called `xdg_toplevel_icon_manager_v1.set_icon` and the state
    /// it named becomes current at the next commit.
    ///
    /// A flag rather than reading the cached state on every commit: the icon is
    /// double-buffered like everything else, so "has it changed" is not a
    /// question the current value can answer, and an animating client commits
    /// sixty times a second with the same icon attached.
    icon_dirty: bool,
    /// The window-geometry rectangle's origin, in root-surface coordinates —
    /// the number that turns a pixel of the window into a point of the surface.
    ///
    /// ADDED FOR §37, and it is the whole of that section's bug. `compose_tree`
    /// blits the tree at `-(gx, gy)`, so the window's top-left pixel **is**
    /// surface point `(gx, gy)`; a pointer at window pixel `(x, y)` is
    /// therefore at surface point `(x + gx, y + gy)`. Every input event took
    /// the window pixel and called it a surface point, which for any client
    /// that declares an inset — every GTK client, every Weston toytoolkit
    /// client — put the click `(gx, gy)` up and to the left of the pixel the
    /// user aimed at.
    ///
    /// Written by `compose_tree`, from the same `(gx, gy)` the blit used,
    /// rather than read again from the cached state at input time. That is
    /// deliberate and it is the point of the field: **the pointer and the
    /// pixels must use the same number**, and two independent reads of a
    /// double-buffered value can disagree by one commit — which is a click that
    /// lands in the wrong place for exactly one frame, the least debuggable
    /// version of this bug.
    content_offset: (i32, i32),
}

pub struct State {
    compositor: CompositorState,
    shm: ShmState,
    xdg_shell: XdgShellState,
    /// ADDED FOR STAGE 9. Kept as a field rather than dropped after
    /// `create_global` because every surface that maps has to be sent
    /// `wl_surface.enter` for it: a client that is never told which output it
    /// is on does not know its scale, and GTK's is the code that asks.
    output: Output,
    /// ADDED FOR STAGE 9. Held only to keep the global alive; nothing reads it.
    /// See the note on the import for why a clipboard is a prerequisite for
    /// input.
    _data_device: DataDeviceState,
    /// ADDED FOR STAGE 9. Both decoration protocols; see the import.
    _xdg_decoration: XdgDecorationState,
    kde_decoration: KdeDecorationState,
    /// ADDED FOR STAGE 17. Which mode is advertised as the manager default and
    /// answered to anything that asks. See [`Decorations`].
    decorations: Decorations,
    /// Owns the seat: `SeatState` keeps every `Seat` it makes, so there is no
    /// separate field for the one seat and dropping the local from `new` does
    /// not destroy the global.
    seat_state: SeatState<Self>,
    /// `None` only if the keymap failed to compile, which is a hard startup
    /// error everywhere it can be one — but *not* here, because a compositor
    /// that refuses to run because a keymap is bad is worse than one that runs
    /// without a keyboard and says so.
    keyboard: Option<KeyboardHandle<Self>>,
    pointer: PointerHandle<Self>,
    /// ADDED FOR STAGE 20. `None` on a machine with no digitiser, and then
    /// `wl_seat` never advertises the touch capability — see `platform::
    /// has_touch`. The `Option` is the honesty: a capability is a promise that
    /// events of that kind can happen, and adding the handle unconditionally
    /// would make the promise on every laptop without a touchscreen.
    touch: Option<TouchHandle<Self>>,
    /// Which window the platform says has keyboard focus, and which one the
    /// pointer is over. Kept as ids rather than surfaces because that is what
    /// arrives from the UI thread, and resolved to a surface at the moment of
    /// use so a message about a window that has since been destroyed is
    /// ignorable rather than a dangling reference.
    keyboard_focus: Option<WindowId>,
    pointer_focus: Option<WindowId>,
    /// Where the pointer is, in surface coordinates of [`Self::pointer_focus`].
    /// `wl_pointer.button` and `.axis` carry no coordinates — the protocol says
    /// they happen wherever the last motion was — so the compositor has to
    /// remember.
    pointer_at: (f64, f64),
    /// Which surface each live touch contact landed on.
    ///
    /// ADDED FOR §40, to answer one question smithay will not: **when a
    /// surface is destroyed with a finger still on it, which contacts have to
    /// be ended?** `TouchHandle` keeps the per-slot focus privately and does
    /// not release it when the focused surface dies, so without this the
    /// client is left holding a contact that can never come up — there is no
    /// `wl_touch.leave` to rescue it, and the next `down` on the same slot
    /// arrives with the previous one still notionally held.
    ///
    /// Keyed by the *surface* rather than by the window, because the surface
    /// is what gets destroyed: a `wl_subsurface` can go away while its window
    /// lives on, and WLCS's `sends_touch_up_on_surface_destroy` runs that case
    /// against both subsurface builders.
    touch_contacts: Vec<(i32, WlSurface)>,
    /// `wp_cursor_shape_v1`, held only so the global outlives the display.
    ///
    /// ADDED FOR STAGE 12. Named with a leading underscore for the same reason
    /// as `_data_device`: nothing calls a method on it, and dropping it would
    /// destroy the global.
    _cursor_shape: CursorShapeManagerState,
    /// The pixels of the last cursor-role surface to commit, and which surface
    /// they came from.
    ///
    /// ADDED FOR STAGE 12. **This is a cache, and it exists because the two
    /// requests arrive in either order.** A client may `set_cursor(surface)`
    /// and *then* attach and commit the buffer (GTK does), or attach and
    /// commit and *then* `set_cursor` (Qt does). In the first order the commit
    /// is what has the pixels; in the second the `set_cursor` is, and by then
    /// smithay has already taken the buffer out of the surface's pending state
    /// and it cannot be read again. One slot covers both, because a client has
    /// one cursor at a time.
    cursor_pixels: Option<(WlSurface, IconImage)>,
    /// The surface the client last handed to `wl_pointer.set_cursor`, if it
    /// handed one over rather than naming a shape.
    ///
    /// ADDED FOR STAGE 13, and it exists for two jobs that are really one.
    ///
    /// A cursor surface arrives *empty*: `set_cursor` names it before the
    /// client has attached a buffer to it, so the pixels turn up on a later
    /// commit and there is no way to build an `HCURSOR` at the moment of the
    /// request. Remembering which surface it was is what lets that commit be
    /// recognised when it comes.
    ///
    /// And recognising it is also what makes the guarantee legible: a surface
    /// in here is a cursor, is **never** composited into any window, and
    /// it says so in the log the first time a client sets one. It was
    /// *nearly* true by accident already — a cursor surface is nobody's
    /// subsurface, so the tree walk in `compose_tree` never reached it — but
    /// its pixels were still blitted into `staging` and kept there forever,
    /// and "not reached by the current traversal" is not a property anybody
    /// can rely on while the scene graph is still growing.
    cursor_surface: Option<WlSurface>,
    /// The popup that called `xdg_popup.grab`, if one is up.
    ///
    /// ADDED FOR STAGE 13. See [`XdgShellHandler::grab`] for what it is and
    /// [`Self::dismiss_grab_if_outside`] for what it does.
    popup_grab: Option<WindowId>,
    /// Caps Lock and Num Lock as the system last reported them, carried in on
    /// [`FromUi::KeyboardFocus`]. See [`Self::sync_locked_modifiers`].
    locks: (bool, bool),

    /// The connection, for the two smithay entry points that take one and are
    /// called from outside a handler: `set_data_device_focus` when Windows
    /// moves the focus, and `set_data_device_selection` when the Win32
    /// clipboard changes. Both are events that originate on the UI thread, so
    /// there is no `&DisplayHandle` in scope to borrow.
    display: DisplayHandle,
    /// The one seat, for the same two calls. `SeatState` owns it and this is an
    /// `Arc` handle to it, so holding one here does not duplicate anything.
    seat: Seat<Self>,

    ui: Ui,
    events: Receiver<FromUi>,
    /// Tried in order until one recognises the buffer. One entry today; see
    /// `surface.rs` for why it is a list.
    sources: Vec<Box<dyn PixelSource>>,
    /// ADDED FOR STAGE 9: one shadow per **surface**, not per window.
    ///
    /// A commit copies the client's buffer in here; `compose_tree` copies these
    /// into the window's shadow in stacking order. The extra copy is what buys
    /// the right to release the client's buffer immediately (the argument in
    /// `surface.rs`) while still being able to redraw a subsurface that has not
    /// committed this frame.
    staging: HashMap<WlSurface, Shadow>,
    windows: HashMap<WlSurface, Window>,
    /// Every surface that has created a decoration object, on either protocol.
    ///
    /// ADDED FOR STAGE 33, and it is one bit per window that could not be
    /// recovered afterwards. The compositor's account of a run said which
    /// decoration mode it *offered* and nothing about who took it up, so a
    /// client that ignored both protocols and drew its own title bar was
    /// indistinguishable in the log from one that accepted the system's — and
    /// the difference is the whole of the bug the user reported. See
    /// [`crate::surface::decorator`].
    negotiated_decoration: std::collections::HashSet<WlSurface>,
    next_id: WindowId,
    started: Instant,
    last_frame: Instant,
    /// Synthesise an icon at map time, so the platform's icon path is executed
    /// by something rather than merely compiled.
    debug_icon: bool,
    /// Icons published by app id, by something that is not the window's client
    /// — in practice wc-proxy, which is the only thing in the topology that can
    /// read a Linux icon theme (`crate::icon`).
    icons: crate::icon::IconStore,
    /// Kept alive so the global stays advertised, and so `Display::global_id`
    /// is not the only handle to it.
    _icon_manager: smithay::reexports::wayland_server::backend::GlobalId,
    /// The same, for the standard protocol; it also owns the size list that
    /// every binding client is told about.
    _toplevel_icon: smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager,

    // --- stage 21 ----------------------------------------------------------
    /// Kept alive so `zwp_idle_inhibit_manager_v1` stays advertised.
    _idle_inhibit: IdleInhibitManagerState,
    /// Surfaces holding at least one `zwp_idle_inhibitor_v1`, and how many.
    ///
    /// **A count, not a set**, because one surface may hold several: the
    /// protocol puts no limit on it and a toolkit that inhibits once for the
    /// video and once for the fullscreen state is doing nothing wrong. A set
    /// would have the first destroy cancel the second inhibitor's effect.
    idle_inhibitors: HashMap<WlSurface, usize>,
    /// What was last sent to the platform, so an unchanged answer costs nothing.
    idle_inhibited: bool,
    /// `xdg_activation_v1`'s token pool. Held rather than dropped because it is
    /// mutable state — tokens are minted, spent and expired through it.
    xdg_activation: XdgActivationState,
    /// `zwp_keyboard_shortcuts_inhibit_manager_v1`'s inhibitor list.
    shortcuts_inhibit: KeyboardShortcutsInhibitState,
    /// Icons this compositor has actually put on a window, and how many were
    /// asked for and had nothing to answer with. Printed at shutdown, because
    /// "the icon is generic" is otherwise indistinguishable from "the icon
    /// never arrived".
    pub icons_applied: usize,
    pub icons_missing: usize,

    /// Kept alive so `wc_clipboard_manager_v1` stays advertised.
    _clipboard_manager: smithay::reexports::wayland_server::backend::GlobalId,
    /// Fetches in flight: the token the UI thread will answer with, and who to
    /// answer.
    ///
    /// Keyed by a token this compositor mints rather than by the one the client
    /// sent, because two proxies are two clients and their tokens are allowed
    /// to collide. The client's own number is carried alongside and echoed
    /// back.
    clip_fetches: HashMap<u64, (crate::clipboard::proto::wc_clipboard_manager_v1::WcClipboardManagerV1, u32)>,
    next_clip_token: u64,
    /// Who to tell when the system drag ends. One at a time, because there is
    /// one mouse.
    clip_drag_source: Option<crate::clipboard::proto::wc_clipboard_manager_v1::WcClipboardManagerV1>,
    pub clip_published: usize,
    pub clip_fetched: usize,
    pub clip_offered: usize,
    pub clip_drags: usize,

    pub clients_seen: usize,
    pub toplevels: usize,
    /// ADDED FOR STAGE 9, so the harness can assert that a menu opened rather
    /// than inferring it from a picture.
    pub popups: usize,
    pub presents: usize,
    pub stopped: bool,
}

/// Per-client data.
#[derive(Default)]
pub struct ClientState {
    compositor_state: CompositorClientState,
    /// Which connection this is, counted from 1 in accept order.
    ///
    /// ADDED FOR STAGE 26 (docs/waycompose-design.md §26). A conformance suite driving
    /// this compositor from outside knows a client-side `wl_surface*` and
    /// nothing about our window ids, so the only handle it can name a window
    /// by is *which connection* plus *which object id on it*. The connection
    /// has no such name in the protocol — `ClientId` is an opaque handle whose
    /// `Debug` spelling is an implementation detail — so it is counted here.
    /// Costs one `u32` per client and is otherwise inert.
    pub index: u32,
}

impl ClientState {
    pub fn new(index: u32) -> ClientState {
        ClientState { index, ..Default::default() }
    }
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        // The ordinal is printed as well as the backend's handle, and on both
        // platforms. It is what a peer outside this process counts (see
        // `index`), and a log that names only the opaque `ClientId` cannot be
        // lined up against anything.
        println!(
            "wc-compositor: client {} ({client_id:?}) initialized",
            self.index
        );
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        println!("wc-compositor: client {client_id:?} disconnected: {reason:?}");
    }
}

// How fast a held key repeats, and how long before it starts.
//
// **The compositor does not repeat anything.** `wl_keyboard.repeat_info` is
// advertised and the *client* runs the timer, which is how the protocol is
// specified and is why a Wayland client's key repeat keeps working while the
// compositor is busy. Windows repeats too — a held key produces a stream of
// `WM_KEYDOWN` with the "previous state" bit set — and forwarding those as
// well would give a client two repeat streams at different rates. So they are
// dropped, in `win32.rs`, and this is the only rate anyone sees.
//
// > **[SUPERSEDED — 2026-08-13, see docs/waycompose-design.md §23.5]** The numbers are
// > Windows' own defaults expressed the way Wayland wants them:
// > `SPI_GETKEYBOARDDELAY` 1 is 500 ms, `SPI_GETKEYBOARDSPEED` is around 30/s
// > at its default. Reading the real values is a `SystemParametersInfo` call
// > and is noted in the design as not done, rather than guessed at silently.
// >
// >     const REPEAT_DELAY_MS: i32 = 500;
// >     const REPEAT_RATE_HZ: i32 = 30;
//
// The call is now made: `crate::platform::keyboard_repeat` reads both settings
// and converts them, and the two constants are gone. They were the same
// mistake as the cursor in miniature — a number this compositor invented and
// advertised as if it had been asked for — and a user who had moved the
// Control Panel slider got the default in every Wayland client and their own
// rate in every native one. The headless platform still returns 500 and 30,
// because there is no Windows there to ask and the harness must not depend on
// the build host's desktop settings.
impl State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        dh: &DisplayHandle,
        ui: Ui,
        events: Receiver<FromUi>,
        debug_icon: bool,
        layout: &str,
        output_size: (i32, i32),
        decorations: Decorations,
    ) -> Self {
        let mut seat_state = SeatState::new();
        // `SeatState` keeps every seat it makes and `Seat` is an `Arc` handle
        // rather than the seat itself, so this local used to be dropped here.
        // The clipboard work keeps it: `set_data_device_focus` and
        // `set_data_device_selection` both take one and are called from the UI
        // event pump, where there is no seat in scope.
        let mut seat: Seat<State> = seat_state.new_wl_seat(dh, "seat0");

        // **Capabilities are advertised by adding them, and only then.**
        // smithay recomputes `wl_seat.capabilities` from what the seat has and
        // sends it to every bound `wl_seat`, so the ordering requirement — a
        // capability must not be announced before the thing it names exists —
        // is met by construction rather than by getting a sequence right. A
        // client that binds later gets the current set in its `bind`.
        let pointer = seat.add_pointer();

        // STAGE 20, and the condition is the point. `add_touch` is what makes
        // smithay put `touch` in `wl_seat.capabilities`, so calling it is a
        // statement that this seat can produce `wl_touch` events — which on a
        // machine with no digitiser it cannot. `platform::has_touch` asks the
        // platform; on Windows that is `SM_MAXIMUMTOUCHES`, and on the headless
        // platform it is `--simulate-wm`'s scripted gesture, which is a real
        // source of the events even though it is not a finger.
        //
        // Until this stage the capability was correctly absent because
        // `add_touch` was never called and no touch event existed. It is
        // present now for the same reason: the events behind it are real.
        let touch = crate::platform::has_touch().then(|| seat.add_touch());

        // The keyboard is the one that can fail, because it compiles a keymap.
        // On Windows this is the *only* way it can succeed: `XkbConfig`'s RMLVO
        // fields go through `xkb_keymap_new_from_names`, which resolves against
        // an xkeyboard-config tree that does not exist there (design §7.2). The
        // `keymap` field is this project's patch to smithay and is what makes
        // the build-time-resolved string reachable from `add_keyboard`.
        let chosen = crate::input::layout(layout).or_else(|| {
            eprintln!(
                "wc-compositor: no keymap for layout {layout:?}; falling back to {}",
                crate::input::FALLBACK
            );
            crate::input::layout(crate::input::FALLBACK)
        });
        // ADDED FOR STAGE 12: asked for, not assumed. See the block comment
        // above, and `platform::keyboard_repeat`.
        let (repeat_delay_ms, repeat_rate_hz) = crate::platform::keyboard_repeat();
        let keyboard = chosen.and_then(|l| {
            match seat.add_keyboard(
                XkbConfig { keymap: Some(l.keymap), ..XkbConfig::default() },
                repeat_delay_ms,
                repeat_rate_hz,
            ) {
                Ok(k) => {
                    println!(
                        "wc-compositor: seat0 keyboard: layout {} ({} bytes of keymap), \
                         repeat {repeat_rate_hz}/s after {repeat_delay_ms} ms",
                        l.name,
                        l.keymap.len()
                    );
                    Some(k)
                }
                Err(e) => {
                    // Not fatal, and the line says which capability is missing
                    // rather than leaving a client to discover it.
                    eprintln!("wc-compositor: seat0 has no keyboard: {e}");
                    None
                }
            }
        });
        println!(
            "wc-compositor: seat0 capabilities: pointer{}{}",
            if keyboard.is_some() { ", keyboard" } else { "" },
            if touch.is_some() { ", touch" } else { "" }
        );

        // Said at startup, on the side that created the object, because the
        // failure it predicts happens later and somewhere else. A keyboard on
        // Windows with no published directory means the keymap will be a
        // `Local\wc-…` section; that is right for a Windows client and
        // unopenable by anything across the boundary, and the first a proxy
        // knows about it is when it has to end a connection. One line here
        // turns "the client died on connect" into "we were told this at
        // startup".
        //
        // STAGE 10 added a third possibility and the line has to know about it,
        // because saying "the keymap will be a Windows section" on a run where
        // it is an arena slice is worse than saying nothing: it is a true
        // sentence about a code path that did not run, printed at exactly the
        // moment somebody is looking for the reason a client died.
        if cfg!(windows) && keyboard.is_some() {
            match (wc_handle::arena::outbound(), wc_handle::publish::get()) {
                (Some(w), _) => println!(
                    "wc-compositor: the keymap will be a slice of arena {} \
                     (its outbound window is {}+{}), so nothing is opened and no filesystem \
                     is involved.",
                    w.arena, w.offset, w.len
                ),
                (None, Some(_)) => {}
                (None, None) => println!(
                    "wc-compositor: no --shared-dir and no --arena-outbound, so the keymap \
                     will be a Windows section. That is correct for a client on this machine \
                     and unopenable by one across the boundary."
                ),
            }
        }

        // Two icon globals, and they are not alternatives.
        //
        // `xdg-toplevel-icon-v1` is the protocol; a client that implements it
        // is the authority on its own icon and needs nothing else. Measured
        // against real clients (docs/waycompose-design.md §15.3) that is GTK 4.20 and not
        // Firefox 151, weston 15 or GTK 3 — so the second global exists for the
        // ones that do not, and is bound by the resolver rather than by them.
        let icon_manager = dh
            .create_global::<State, crate::icon::proto::wc_icon_manager_v1::WcIconManagerV1, _>(
                1,
                (),
            );
        // §29. The clipboard's side door, for the one thing
        // `wl_data_device` cannot express across this boundary: the bytes
        // themselves. See `protocols/wc-clipboard-v1.xml` and `crate::clipboard`.
        let clipboard_manager = dh
            .create_global::<State, crate::clipboard::proto::wc_clipboard_manager_v1::WcClipboardManagerV1, _>(
                1,
                (),
            );
        let mut toplevel_icon =
            smithay::wayland::xdg_toplevel_icon::XdgToplevelIconManager::new::<State>(dh);
        // The sizes Windows will ask for, told to the client at bind time:
        // `SM_CXSMICON` and `SM_CXICON` are 16 and 32 at 96 dpi, and 48 is what
        // a scaled display's `ICON_BIG` becomes. A client that honours these
        // sends exactly the images that will be used and none that will not.
        for size in [16, 32, 48] {
            toplevel_icon.add_icon_size(size);
        }

        // ADDED FOR STAGE 9: one output, describing the desktop the windows
        // will live on.
        //
        // **The numbers are a flag, and the flag is not the same claim as a
        // measurement.** In the deployment this is aimed at, the right values
        // come from `GetSystemMetrics` / `EnumDisplayMonitors` on the guest and
        // change when a monitor is plugged in; here they are `--output`,
        // defaulting to a size no real client will find surprising. What is
        // *not* a guess is that the object has to exist: see the import.
        //
        // The physical size is reported as 0x0, which is the protocol's own
        // spelling for "unknown" and is what every headless and virtual
        // compositor sends. Inventing millimetres would make a client compute a
        // DPI that is fiction.
        let (ow, oh) = output_size;
        let mode = Mode {
            size: (ow, oh).into(),
            refresh: 60_000,
        };
        let output = Output::new(
            "WC-1".to_owned(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "waycompose".to_owned(),
                model: "virtual".to_owned(),
            },
        );
        output.create_global::<State>(dh);
        output.change_current_state(Some(mode), Some(Transform::Normal), None, Some((0, 0).into()));
        output.set_preferred(mode);
        println!("wc-compositor: output WC-1 {ow}x{oh} at 60 Hz, scale 1");
        // Greppable, because "how many title bars does this window have" is
        // decided here and nowhere else, and a run that answers it wrongly
        // looks like a rendering bug rather than a negotiation one.
        println!("wc-compositor: decorations: manager default is {decorations:?}");

        State {
            compositor: CompositorState::new::<State>(dh),
            shm: ShmState::new::<State>(dh, Vec::new()),
            xdg_shell: XdgShellState::new::<State>(dh),
            output,
            _data_device: DataDeviceState::new::<State>(dh),
            _xdg_decoration: XdgDecorationState::new::<State>(dh),
            // **This one line decides how many title bars the window has**, and
            // it is `default_mode` on the *manager*, sent once at bind, rather
            // than the answer to any per-surface request: GTK 3 reads the
            // manager's default in `gdk_wayland_display_prefers_ssd()` and
            // ignores what it is told about its own surface. §22.2.
            kde_decoration: KdeDecorationState::new::<State>(dh, decorations.kde()),
            decorations,
            seat_state,
            keyboard,
            pointer,
            touch,
            keyboard_focus: None,
            pointer_focus: None,
            pointer_at: (0.0, 0.0),
            touch_contacts: Vec::new(),
            // ADDED FOR STAGE 12. Version 2 of the protocol, which is what
            // smithay's global advertises and what adds `dnd_ask` and
            // `all_resize` to the shape enum.
            _cursor_shape: CursorShapeManagerState::new::<State>(dh),
            cursor_pixels: None,
            cursor_surface: None,
            popup_grab: None,
            locks: (false, false),
            display: dh.clone(),
            seat,
            ui,
            events,
            sources: vec![Box::new(ShmSource)],
            staging: HashMap::new(),
            windows: HashMap::new(),
            negotiated_decoration: std::collections::HashSet::new(),
            next_id: 1,
            started: Instant::now(),
            last_frame: Instant::now(),
            debug_icon,
            icons: crate::icon::IconStore::default(),
            _icon_manager: icon_manager,
            _toplevel_icon: toplevel_icon,
            // STAGE 21. Three globals whose Windows implementation is one call
            // each; see the handlers at the bottom of this file, and §25 for
            // why these three and not the rest of the list.
            _idle_inhibit: IdleInhibitManagerState::new::<State>(dh),
            idle_inhibitors: HashMap::new(),
            idle_inhibited: false,
            xdg_activation: XdgActivationState::new::<State>(dh),
            shortcuts_inhibit: KeyboardShortcutsInhibitState::new::<State>(dh),
            icons_applied: 0,
            icons_missing: 0,
            _clipboard_manager: clipboard_manager,
            clip_fetches: HashMap::new(),
            next_clip_token: 1,
            clip_drag_source: None,
            clip_published: 0,
            clip_fetched: 0,
            clip_offered: 0,
            clip_drags: 0,
            clients_seen: 0,
            toplevels: 0,
            popups: 0,
            presents: 0,
            stopped: false,
        }
    }

    /// Milliseconds since the compositor started, which is what a frame
    /// callback carries.
    ///
    /// Note the known wrongness, recorded in docs/waycompose-design.md §7.7 item 5: the
    /// epoch is process start rather than boot, which is right for every
    /// current consumer (a client comparing successive callbacks) and wrong for
    /// `wp_presentation`. Not fixed here because nothing yet compares this
    /// against a clock it did not get from us.
    fn now_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    /// Answer frame callbacks, at most once per vsync-ish interval.
    ///
    /// 16 ms rather than a real vblank because there is no output object yet
    /// and nothing to synchronise to. It is a throttle, not a timing guarantee,
    /// and it is the difference between an animating client using a few percent
    /// of a core and using all of one.
    pub fn tick_frames(&mut self) {
        if self.last_frame.elapsed().as_millis() < 16 {
            return;
        }
        self.last_frame = Instant::now();
        let now = self.now_ms();
        for window in self.windows.values_mut() {
            for cb in window.frame_callbacks.drain(..) {
                cb.done(now);
            }
        }
    }

    /// Decide whether the display should be kept awake, and tell the platform
    /// when the answer changes.
    ///
    /// ADDED FOR STAGE 21. Called once per turn of the dispatch loop rather
    /// than only from the two protocol handlers, and that is the whole reason
    /// this is a function instead of two lines inside them.
    ///
    /// # smithay never tells us the inhibitor is gone
    ///
    /// `IdleInhibitHandler::uninhibit` is called from exactly one place —
    /// `zwp_idle_inhibitor_v1.destroy`, the client's explicit request. There is
    /// no `destroyed` hook on that object. So a client that crashes, is killed,
    /// or simply closes its connection with an inhibitor still alive leaves one
    /// behind that nothing will ever revoke, and an idle inhibitor that cannot
    /// be revoked is a display that never sleeps again for the life of the
    /// compositor. On a guest VM that is a real bill.
    ///
    /// Handling that is not optional and it cannot be done by being careful in
    /// the handlers, because the event never arrives. What does arrive is that
    /// the `WlSurface` stops being alive, and this is where that is noticed.
    ///
    /// # Invisible surfaces
    ///
    /// The protocol says the inhibitor applies while the surface is visible and
    /// leaves "visible" to the compositor. Two things are checked and both are
    /// the user's own doing: the window has committed a buffer (`mapped`) and
    /// it is not minimised. Minimising the window playing the video is about as
    /// clear a statement as a user can make that they are done watching it.
    ///
    /// # Cost
    ///
    /// One `HashMap::is_empty` per turn in the ordinary case, because the
    /// ordinary case is that nobody has ever created an inhibitor. When one
    /// exists, a scan of a map that holds one or two entries.
    pub fn tick_idle_inhibit(&mut self) {
        if self.idle_inhibitors.is_empty() && !self.idle_inhibited {
            return;
        }
        let before = self.idle_inhibitors.len();
        self.idle_inhibitors
            .retain(|surface, _| smithay::utils::IsAlive::alive(surface));
        if self.idle_inhibitors.len() != before {
            println!(
                "wc-compositor: {} idle inhibitor(s) went away with their surface",
                before - self.idle_inhibitors.len()
            );
        }

        // `root_of`, not the surface itself: `windows` is keyed by the surface
        // that has the shell role, and a client is entitled to inhibit on a
        // subsurface — the video, rather than the window containing it. Every
        // toolkit measured so far inhibits on the toplevel, so this line has
        // never yet made a difference, and the day it does the alternative is
        // an inhibitor that is silently ignored.
        let want = self.idle_inhibitors.keys().any(|surface| {
            self.windows
                .get(&root_of(surface))
                .is_some_and(|w| w.mapped && !w.minimized)
        });
        if want == self.idle_inhibited {
            return;
        }
        self.idle_inhibited = want;
        println!(
            "wc-compositor: idle inhibit -> {} ({} inhibitor(s) held)",
            if want { "on" } else { "off" },
            self.idle_inhibitors.len()
        );
        self.ui.send(ToUi::IdleInhibit { on: want });
    }

    /// Drain everything the UI thread has said since the last turn.
    pub fn pump_ui_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                FromUi::Stopped => self.stopped = true,
                FromUi::Resized { id, width, height } => self.on_resized(id, width, height),
                FromUi::CloseRequested { id } => {
                    if let Some(w) = self.by_id(id) {
                        match &w.role {
                            Role::Toplevel(t) => {
                                println!(
                                    "wc-compositor: window {id} close requested -> \
                                     xdg_toplevel.close"
                                );
                                t.send_close();
                            }
                            // A popup is dismissed rather than closed: the
                            // protocol's word for it is `popup_done`, and the
                            // client is required to destroy it.
                            Role::Popup(p) => {
                                println!(
                                    "wc-compositor: window {id} dismissed -> xdg_popup.popup_done"
                                );
                                p.send_popup_done();
                            }
                        }
                    }
                }
                FromUi::Maximized { id, on } => {
                    self.set_state(id, |s| toggle(s, xdg_state::Maximized, on))
                }
                FromUi::Minimized { id, on } => {
                    // STAGE 21: remembered, because an idle inhibitor held by a
                    // window the user has minimised must stop keeping the
                    // display awake. See [`State::tick_idle_inhibit`].
                    if let Some(w) = self.windows.values_mut().find(|w| w.id == id) {
                        w.minimized = on;
                    }
                    self.set_state(id, |s| {
                    // xdg-shell has **no minimised state**. `set_minimized` is a
                    // request a client makes; there is no matching entry in the
                    // `state` enum for the compositor to report back, which is a
                    // genuine asymmetry in the protocol rather than an oversight
                    // here.
                    //
                    // `suspended` (xdg_toplevel v6) is the nearest true thing:
                    // "the content is not visible, you may stop drawing".
                    // smithay filters it out for clients that predate v6, which
                    // then keep drawing into a window nobody can see —
                    // wasteful, and correct.
                        toggle(s, xdg_state::Suspended, on)
                    });
                }
                FromUi::Activated { id, on } => {
                    self.set_state(id, |s| toggle(s, xdg_state::Activated, on))
                }
                FromUi::KeyboardFocus { id, on, caps, num } => {
                    self.locks = (caps, num);
                    self.set_keyboard_focus(id, on)
                }
                FromUi::Key { id, keycode, pressed, time } => {
                    self.key(id, keycode, pressed, time)
                }
                FromUi::PointerEnter { id, x, y, time } => self.pointer_motion(Some(id), x, y, time),
                FromUi::PointerMotion { id, x, y, time } => self.pointer_motion(Some(id), x, y, time),
                FromUi::PointerLeave { id, time } => {
                    if self.pointer_focus == Some(id) {
                        self.pointer_motion(None, 0.0, 0.0, time);
                    }
                }
                FromUi::PointerButton { id, button, pressed, time } => {
                    self.pointer_button(id, button, pressed, time)
                }
                FromUi::PointerAxis { id, horizontal, vertical, v120_h, v120_v, time } => {
                    self.pointer_axis(id, horizontal, vertical, v120_h, v120_v, time)
                }
                FromUi::Layout { name } => self.set_layout(name),
                FromUi::ClipboardChanged { mimes } => self.on_clipboard_changed(mimes),
                FromUi::ClipboardData { token, data } => self.on_clipboard_data(token, data),
                FromUi::DragFinished { action } => {
                    println!("wc-compositor: the drag ended with action {action:#x}");
                    if let Some(source) = self.clip_drag_source.take() {
                        source.drag_finished(action);
                    }
                }
                FromUi::TouchDown { id, slot, x, y, shape, time } => {
                    self.touch_down(id, slot, x, y, shape, time)
                }
                FromUi::TouchMotion { id, slot, x, y, shape, time } => {
                    self.touch_motion(id, slot, x, y, shape, time)
                }
                FromUi::TouchUp { id, slot, time } => self.touch_up(id, slot, time),
                FromUi::TouchFrame => {
                    if let Some(touch) = self.touch.clone() {
                        touch.frame(self);
                    }
                }
                FromUi::TouchCancel => {
                    if let Some(touch) = self.touch.clone() {
                        // No `frame` after this: the protocol says outright
                        // that none is required, and smithay's `cancel` drops
                        // every focus, so a frame afterwards would be sent to
                        // nobody.
                        touch.cancel(self);
                    }
                }
            }
        }
    }

    /// The Win32 clipboard changed: offer it to Wayland.
    ///
    /// This is the *whole* of the announcement, and it goes out over ordinary
    /// `wl_data_device` rather than over `wc_clipboard_v1`. That is deliberate
    /// and it is what keeps the side interface small: smithay already knows how
    /// to mint a `wl_data_offer`, list its mime types on it, and send
    /// `selection` to whichever client has the focus — and wc-proxy forwards
    /// every one of those messages verbatim, because none of them carries a
    /// descriptor. The only message in the family that does is
    /// `wl_data_offer.receive`, and that one never reaches this compositor.
    fn on_clipboard_changed(&mut self, mimes: Vec<String>) {
        if mimes.is_empty() {
            println!("wc-compositor: the system clipboard is empty; clearing the selection");
            smithay::wayland::selection::data_device::clear_data_device_selection(
                &self.display,
                &self.seat.clone(),
            );
            return;
        }
        self.clip_offered += 1;
        println!(
            "wc-compositor: the system clipboard changed; offering {} mime type(s): {}",
            mimes.len(),
            mimes.join(", ")
        );
        smithay::wayland::selection::data_device::set_data_device_selection(
            &self.display,
            &self.seat.clone(),
            mimes,
            (),
        );
    }

    /// The UI thread has read the clipboard. Send it back to whoever asked.
    ///
    /// Chunked at [`crate::clipboard::CHUNK`] for the reason §16.5 gives about
    /// icons and which is not specific to icons: `wayland-backend`'s buffers
    /// are 4096 bytes, and a message that does not fit in one cannot be sent at
    /// all. A clipboard payload is routinely larger than an icon.
    fn on_clipboard_data(&mut self, token: u64, data: Option<Vec<u8>>) {
        let Some((manager, client_token)) = self.clip_fetches.remove(&token) else {
            // The publisher went away while the UI thread was reading. Not an
            // error: a proxy whose client exited mid-paste is the ordinary
            // case, and there is nobody left to tell.
            return;
        };
        match data {
            Some(bytes) => {
                self.clip_fetched += 1;
                println!(
                    "wc-compositor: clipboard fetch {client_token} answered with {} bytes",
                    bytes.len()
                );
                for part in bytes.chunks(crate::clipboard::CHUNK) {
                    manager.chunk(client_token, part.to_vec());
                }
                manager.done(client_token);
            }
            None => {
                println!("wc-compositor: clipboard fetch {client_token} has no such format");
                manager.fail(client_token, "the clipboard holds nothing of that type".into());
            }
        }
    }

    fn by_id(&self, id: WindowId) -> Option<&Window> {
        self.windows.values().find(|w| w.id == id)
    }

    fn surface_of(&self, id: WindowId) -> Option<WlSurface> {
        self.by_id(id).map(|w| w.role.surface().clone())
    }

    /// **Which surface of window `id` is under the point `(x, y)`**, and where
    /// that surface's origin sits — the pair every input event needs.
    ///
    /// # What this replaces, and why it is the compositor's job on both legs
    ///
    /// Until §40 this was `surface_and_origin`: the window's *root* surface and
    /// nothing else. §37 fixed the origin half — the second element stopped
    /// being a hard-coded `(0, 0)` and became the window-geometry inset — but
    /// the first half was still a placeholder, and it is the single largest
    /// source of WLCS failures in the suite. A subsurface never received a
    /// pointer event, and `wl_surface.set_input_region` was parsed by smithay
    /// and read by nobody.
    ///
    /// **This is not harness-only code, and the architecture note in
    /// `control.rs` is easy to misread as saying it is.** Windows routes a
    /// `WM_MOUSEMOVE` to an `HWND`, and there is one `HWND` per *window*. A
    /// `wl_subsurface` has no `HWND` — the whole surface tree is composed into
    /// one window's pixels by `compose_tree` — so the routing *within* a window
    /// is something Win32 has already declined to do and cannot do. Windows
    /// answers "which window"; only this function can answer "which surface".
    /// It runs on the real platform path exactly as it runs under `--control`.
    ///
    /// # The coordinate spaces, because there are three
    ///
    /// `(x, y)` arrives in **window-content pixels**: the platform has already
    /// subtracted the frame, so `(0, 0)` is the top-left of the client area.
    /// `placed_tree` reports each surface in **root-buffer** coordinates. The
    /// blit puts root-buffer point `(gx, gy)` at window pixel `(0, 0)`, where
    /// `(gx, gy)` is [`Window::content_offset`] — so the two spaces differ by
    /// exactly that offset, and a surface placed at `loc` has its origin at
    /// window pixel `loc - g`. That subtraction is done once, here, and the
    /// result is handed to smithay in the form it wants: smithay computes
    /// `location - origin`, so the client is sent `(x, y) - (loc - g)`, which
    /// is the same arithmetic `compose_tree` did to the pixels.
    ///
    /// For a window whose tree is one surface this returns exactly what
    /// `surface_and_origin` did — `loc` is `(0, 0)` and the origin is
    /// `(-gx, -gy)`, which is [`surface_origin`]'s value. The old function is
    /// the new one's single-surface case, so §37's fix is preserved rather than
    /// re-derived.
    ///
    /// # Topmost wins, and `None` is a real answer
    ///
    /// `placed_tree` is in paint order, so the reverse is top-to-bottom and the
    /// first surface that accepts the point is the one the client would see
    /// drawn there. If nothing accepts it — every surface's input region
    /// excludes it, or the window has no buffer yet — the answer is `None`, and
    /// the caller sends a `leave` rather than delivering to the root anyway.
    /// That is the case `set_input_region` exists to create and the one the
    /// placeholder got wrong in the direction that matters: silently claiming
    /// input a client asked not to receive.
    fn surface_under(&self, id: WindowId, x: f64, y: f64) -> Option<(WlSurface, (f64, f64))> {
        let window = self.by_id(id)?;
        // §37's sign, reused rather than re-derived: this is the root surface's
        // origin, and every other surface of the tree is that plus its own
        // placement. A second copy of the negation is a second chance to get it
        // backwards, and the whole of `surface_origin`'s doc comment is about
        // how easy that is.
        let (rx, ry) = surface_origin(window.content_offset);
        let root = window.role.surface().clone();
        placed_tree(&root).into_iter().rev().find_map(|(surface, loc)| {
            // The surface's origin in window-content pixels.
            let origin = (rx + f64::from(loc.0), ry + f64::from(loc.1));
            let local = (x - origin.0, y - origin.1);
            let size = self.staging.get(&surface).map(|s| (s.width(), s.height()))?;
            let accepted = compositor::with_states(&surface, |states| {
                let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                accepts_input_at(size, attrs.current().input_region.as_ref(), local)
            });
            accepted.then_some((surface, origin))
        })
    }

    /// Every mapped window, as `(id, client ordinal, surface object id, w, h)`,
    /// bottom of the stack first.
    ///
    /// ADDED FOR STAGE 26, and used only by `crate::control` — the socket a
    /// conformance suite drives this compositor through. Stacking order is
    /// window-id order, which is creation order: on Win32 a new window is
    /// created on top and a popup is created after the toplevel it belongs to,
    /// so the two agree for every case that has one.
    ///
    /// The size is read from the shadow rather than from a stored field
    /// because the shadow *is* the size — it is reset to the buffer's
    /// dimensions on every commit (`surface::Shadow::reset`), so it cannot
    /// disagree with what the client last drew.
    #[cfg(unix)]
    pub fn mapped_windows(&self) -> Vec<crate::control::Rect> {
        use smithay::reexports::wayland_server::Resource as _;
        let mut out: Vec<crate::control::Rect> = self
            .windows
            .values()
            .filter(|w| w.mapped)
            .filter_map(|w| {
                let surface = w.role.surface();
                let client = surface
                    .client()
                    .and_then(|c| c.get_data::<ClientState>().map(|d| d.index))?;
                let shadow = w.shadow.lock().ok()?;
                Some(crate::control::Rect {
                    id: w.id,
                    client,
                    surface: surface.id().protocol_id(),
                    w: shadow.width(),
                    h: shadow.height(),
                    toplevel: w.role.toplevel().is_some(),
                })
            })
            .collect();
        out.sort_by_key(|r| r.id);
        out
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// Keyboard focus follows the **foreground window**, because the user's
    /// window manager is Windows.
    ///
    /// A Wayland compositor normally decides this itself — click-to-focus,
    /// follow-mouse, a keyboard shortcut — and here it has no business
    /// deciding. There is one focus on the desktop, Windows owns it, and the
    /// only correct answer is to mirror it. `WM_SETFOCUS`/`WM_KILLFOCUS` is the
    /// message pair that carries it, and it is deliberately not `WM_ACTIVATE`:
    /// activation is about which top-level window is frontmost and focus is
    /// about where a keystroke goes, and `xdg_toplevel`'s `activated` state
    /// wants the first while `wl_keyboard.enter` wants the second.
    fn set_keyboard_focus(&mut self, id: WindowId, on: bool) {
        let Some(keyboard) = self.keyboard.clone() else { return };
        if on {
            let Some(surface) = self.surface_of(id) else { return };
            self.keyboard_focus = Some(id);
            println!("wc-compositor: window {id} has keyboard focus");
            // **The clipboard follows the keyboard, and until §29 nothing
            // said so.** `wl_data_device`'s selection is offered to the client
            // with *data device focus*, which smithay keeps separately and
            // which nothing here had ever set — so a compositor-provided
            // selection would have been announced to nobody, and a client's
            // `wl_data_device` would have sat silent forever. It is one call,
            // it is the same rule every compositor uses, and its absence is
            // invisible until something actually has a selection to offer.
            self.set_clipboard_focus(Some(&surface));
            keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
            // STAGE 13. The locks are Windows', and they are the half of the
            // modifier state that key events cannot carry.
            //
            // Caps Lock and Num Lock are *locked* modifiers: their state is a
            // property of the machine, it persists while our window is not
            // focused, and the user can change it while we are not looking —
            // pressing Caps Lock in another application changes it for
            // everything, us included, and we see no key event at all. Inferred
            // from the events we happen to have seen, our idea of the lock
            // drifts out of step with the keyboard light, and the symptom is
            // capital letters with the light off.
            //
            // So it is read from the system on every focus gain rather than
            // tracked. The platform supplies it because only the platform can
            // (`GetKeyState(VK_CAPITAL) & 1`), and it is carried on the focus
            // event rather than polled, so that the headless platform can state
            // it too.
            self.sync_locked_modifiers();
        } else {
            // Only if it is *still* ours. A focus change is two messages, and
            // they can arrive in either order: Windows sends `WM_KILLFOCUS` to
            // the old window and `WM_SETFOCUS` to the new one, and clearing on
            // a stale kill would take the focus away from a window that had
            // just been given it.
            if self.keyboard_focus != Some(id) {
                return;
            }
            self.keyboard_focus = None;
            println!("wc-compositor: window {id} lost keyboard focus");
            // STAGE 13, and it must happen **before** `set_focus(None)`.
            //
            // `wl_keyboard.leave` tells the *client* to treat every key as
            // released, and that part was always right. What was wrong is that
            // the compositor's own xkb state went on believing the keys were
            // down — and it is the compositor's state that decides what the
            // next `modifiers` event says.
            //
            // The sequence that bites: the user holds Ctrl, focus moves away
            // (alt-tab, or a popup taking the foreground), the user releases
            // Ctrl and the `WM_KEYUP` goes to whoever has focus now rather than
            // to us, and focus comes back. smithay then sends `enter` with a
            // pressed-keys array and a `modifiers` event that still has Ctrl
            // depressed, and every keystroke from then on is Ctrl-modified
            // until the user happens to press and release Ctrl over our window.
            //
            // `input_intercept` rather than `input`: this must not reach the
            // client as ordinary key-up events. `leave` has already said what
            // the client needs to hear, and a released key aimed at a surface
            // that is losing focus is a key event with nowhere legitimate to
            // go. The pressed set is read from smithay rather than tracked
            // here, because a second copy is a second thing that can disagree —
            // and disagreeing about which keys are down is the bug.
            for keycode in keyboard.pressed_keys() {
                keyboard.input_intercept::<(), _>(self, keycode, KeyState::Released, |_, _, _| ());
            }
            self.set_clipboard_focus(None);
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
    }

    /// Take Caps Lock and Num Lock from the system rather than from history.
    ///
    /// ADDED FOR STAGE 13. See the call site in [`Self::set_keyboard_focus`]
    /// for why this is read and not tracked. The depressed modifiers are left
    /// alone: those *are* ours, they follow from key events we received, and
    /// the focus-loss release above is what keeps them honest.
    fn sync_locked_modifiers(&mut self) {
        let Some(keyboard) = self.keyboard.clone() else { return };
        let (caps, num) = self.locks;
        let mut mods = keyboard.modifier_state();
        if mods.caps_lock == caps && mods.num_lock == num {
            return;
        }
        println!(
            "wc-compositor: locked modifiers from the system: caps {caps}, num {num} \
             (were caps {}, num {})",
            mods.caps_lock, mods.num_lock
        );
        mods.caps_lock = caps;
        mods.num_lock = num;
        keyboard.set_modifier_state(mods);
    }

    /// Who may see the selection. See the call site in [`Self::set_keyboard_focus`].
    fn set_clipboard_focus(&mut self, surface: Option<&WlSurface>) {
        let client = surface.and_then(|s| self.display.get_client(s.id()).ok());
        smithay::wayland::selection::data_device::set_data_device_focus(
            &self.display,
            &self.seat.clone(),
            client,
        );
    }

    /// One key, in evdev numbering, to whatever has focus.
    ///
    /// `id` is not used to route: `wl_keyboard` events go to the focused
    /// surface and to nothing else, and smithay's `KeyboardHandle` owns that
    /// focus. It is checked instead — a key from a window that does not have
    /// focus is a bug in the platform layer and is worth not delivering
    /// silently to whoever does.
    fn key(&mut self, id: WindowId, keycode: u16, pressed: bool, time: u32) {
        let Some(keyboard) = self.keyboard.clone() else { return };
        if self.keyboard_focus != Some(id) {
            return;
        }
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        keyboard.input::<(), _>(
            self,
            // **Plus eight, and this is the one number in the stage that was
            // wrong and was caught by a client rather than by a test.**
            //
            // smithay's `Keycode` is an *XKB* keycode, which carries X11's
            // eight-key offset; `wl_keyboard.key` carries an *evdev* keycode,
            // which does not. smithay subtracts the eight on the way out
            // (`wayland/seat/keyboard.rs`), so a caller that hands it an evdev
            // number is handing it one that is eight too small, and every key
            // reaches the client eight too small.
            //
            // What that looks like from outside is the reason it is worth
            // recording: nothing errors, every key produces *a* character, and
            // the characters are wrong. `weston-eventdemo` printed
            // `key 27, unicode 93` — a right square bracket — for a keypress
            // this compositor believed was `h`. A unit test over
            // `crate::input` could not have caught it, because the table it
            // checks was correct; only a client running the keymap could.
            (u32::from(keycode) + 8).into(),
            state,
            SERIAL_COUNTER.next_serial(),
            time,
            // No compositor-level bindings. Every key goes to the client, which
            // is the right default for a compositor whose window manager is
            // somebody else: Windows has already taken the combinations it
            // reserves (Alt-Tab, the Windows key) before the message loop sees
            // them, and intercepting more here would silently steal keys the
            // application expects.
            |_, _, _| FilterResult::Forward,
        );
    }

    /// Move the pointer, which is also how it enters and leaves.
    ///
    /// `PointerHandle::motion` with `Some(surface)` sends `enter` when the
    /// surface changes and `motion` when it does not; with `None` it sends
    /// `leave`. So there is one function here rather than three, and the
    /// enter/leave bookkeeping the protocol requires is smithay's rather than
    /// something this file could get subtly wrong.
    fn pointer_motion(&mut self, id: Option<WindowId>, x: f64, y: f64, time: u32) {
        let pointer = self.pointer.clone();
        // **The scene lookup, and the window bookkeeping, are deliberately
        // separate.** `pointer_focus` answers "which window is the pointer
        // over", which is Windows' answer and is true whether or not any
        // surface in that window accepts input there; `pointer_button` relies
        // on it to re-place the pointer under a `SetCapture` drag, and
        // `pointer_axis` on it to say where a wheel went. The hit test answers
        // "which surface", and its `None` — a point inside the window but
        // outside every input region — must not be allowed to read as "the
        // pointer left the window".
        match id {
            Some(id) => {
                self.pointer_focus = Some(id);
                self.pointer_at = (x, y);
            }
            None => self.pointer_focus = None,
        }
        let focus = id.and_then(|id| self.surface_under(id, x, y));
        pointer.motion(
            self,
            // The second element is the surface's origin in window-content
            // pixels, and smithay subtracts it from `location` to get the
            // surface-local point the client is sent. See `surface_under` for
            // the three coordinate spaces and which one this is.
            focus.map(|(s, origin)| (s, origin.into())),
            &MotionEvent {
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        // Every logical group of pointer events ends in a `frame`. `wl_pointer`
        // version 5 made it required, clients batch on it, and omitting it
        // leaves a client holding a half-delivered event until the next one
        // arrives — which for a mouse that has stopped moving is forever.
        pointer.frame(self);
    }

    fn pointer_button(&mut self, id: WindowId, button: u32, pressed: bool, time: u32) {
        let pointer = self.pointer.clone();
        // ADDED FOR STAGE 20, and it is about touch rather than about the
        // mouse. A window that does not consume `WM_POINTERDOWN` gets a
        // `WM_LBUTTONDOWN` synthesised from the primary contact as well, so a
        // tap would arrive at the client twice — once as `wl_touch` and once as
        // `wl_pointer` — and a toolkit that handles both, which Firefox does,
        // would act on it twice. There is no message that announces the
        // duplication; the only way to see it is a button appearing beside a
        // contact, and until this line nothing said a button had happened.
        println!(
            "wc-compositor: window {id} pointer button {button:#x} {}",
            if pressed { "pressed" } else { "released" }
        );
        if pressed {
            self.dismiss_grab_if_outside(id);
        }
        if self.pointer_focus != Some(id) {
            // A button from a window the pointer is not over. Reachable on
            // Win32 during a drag, where `SetCapture` keeps sending us the
            // release after the pointer has left; the release still has to be
            // delivered, so the pointer is put back over the window first.
            let (x, y) = self.pointer_at;
            self.pointer_motion(Some(id), x, y, time);
        }
        pointer.button(
            self,
            &ButtonEvent {
                serial: SERIAL_COUNTER.next_serial(),
                time,
                button,
                state: if pressed { ButtonState::Pressed } else { ButtonState::Released },
            },
        );
        pointer.frame(self);
    }

    fn pointer_axis(
        &mut self,
        id: WindowId,
        horizontal: f64,
        vertical: f64,
        v120_h: i32,
        v120_v: i32,
        time: u32,
    ) {
        let pointer = self.pointer.clone();
        // MADE DELIBERATE IN STAGE 13. This used to `return` silently, and that
        // silence is why "the scroll wheel does nothing in Firefox" took a user
        // report to find.
        //
        // The two systems disagree about who a wheel belongs to. **Win32 sends
        // `WM_MOUSEWHEEL` to the window with keyboard focus**, not to the
        // window under the cursor; Wayland delivers `wl_pointer.axis` to the
        // surface with pointer focus, which *is* the one under the cursor. So
        // `id` here is the focused window and need not be the window being
        // pointed at.
        //
        // The root cause of the reported bug was not this line — it was leaked
        // popup `HWND`s (see [`XdgShellHandler::popup_destroyed`]) sitting
        // under the cursor, so `pointer_focus` was a stale menu while the wheel
        // arrived for the toplevel. Fixing the popups fixed the scrolling, and
        // that was confirmed by the user before this was touched. But a wheel
        // arriving for a window that is not the pointer focus is a legitimate
        // situation on Windows, and eating it without a word is what turned a
        // one-line symptom into a hunt.
        //
        // So: deliver to whatever the pointer is actually over — which is what
        // `pointer.axis` would do anyway, since `id` was never used to route —
        // and say when the two disagree. A wheel with no pointer focus at all
        // has nowhere to go and is still dropped.
        let Some(over) = self.pointer_focus else {
            return;
        };
        if over != id {
            println!(
                "wc-compositor: wheel addressed to window {id} (Win32 focus), \
                 delivered to window {over} (pointer focus)"
            );
        }
        let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
        if v120_v != 0 || vertical != 0.0 {
            frame = frame.value(Axis::Vertical, vertical).v120(Axis::Vertical, v120_v);
        }
        if v120_h != 0 || horizontal != 0.0 {
            frame = frame.value(Axis::Horizontal, horizontal).v120(Axis::Horizontal, v120_h);
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    /// Dismiss the grabbing popup if the press landed anywhere else.
    ///
    /// ADDED FOR STAGE 13. A menu is closed by the compositor and not by the
    /// client, because the client cannot see a click outside its own surfaces.
    /// Without this a menu stays up until the application decides to take it
    /// down, which for a click on the page behind it is never.
    ///
    /// The popup itself is not destroyed here. `popup_done` is a *request* —
    /// the client destroys the object, and that arrives as
    /// [`XdgShellHandler::popup_destroyed`], which is the only place a window
    /// is removed. Tearing the `HWND` down from here as well would race the
    /// client and leave the protocol object pointing at nothing.
    fn dismiss_grab_if_outside(&mut self, clicked: WindowId) {
        let Some(grabbing) = self.popup_grab else { return };
        if grabbing == clicked {
            return;
        }
        if let Some(Window { role: Role::Popup(p), .. }) = self.by_id(grabbing) {
            println!(
                "wc-compositor: click on window {clicked} outside the grab -> \
                 window {grabbing} dismissed (xdg_popup.popup_done)"
            );
            p.send_popup_done();
        }
        self.popup_grab = None;
    }

    // --- touch, stage 20 ---------------------------------------------------
    //
    // Three routing facts that make this shorter than the pointer above rather
    // than longer, and all three are the protocol rather than a shortcut:
    //
    // * **There is no enter or leave.** `wl_touch.down` names the surface, and
    //   every later event for that contact belongs to it whatever it moves
    //   over. So there is no focus to track here, and no equivalent of
    //   `pointer_focus`; smithay's `TouchHandle` keeps the per-contact focus
    //   map that `down` establishes and `up` retires.
    // * **The contact id is already right.** It arrives from the platform in
    //   the protocol's numbering, small and dense, because Windows' pointer
    //   ids are not usable as contact ids and the mapping belongs where the
    //   pointer ids are (`crate::input::touch::Slots`).
    // * **`frame` is a separate event and it is required.** It is not emitted
    //   from these three functions, because one Win32 message can be a `down`
    //   and a `shape` and an `orientation`, and those are one logical update
    //   that has to close once. The platform decides where a frame ends; this
    //   file forwards that decision.

    /// A contact landed. This is the only event that carries a surface, and it
    /// is therefore the only one that can be dropped for want of a window.
    fn touch_down(
        &mut self,
        id: WindowId,
        slot: i32,
        x: f64,
        y: f64,
        shape: Option<crate::platform::TouchShape>,
        time: u32,
    ) {
        let Some(touch) = self.touch.clone() else { return };
        // §24 said touch reuses the pointer's conversion rather than doing its
        // own arithmetic, and §40 keeps that promise through the scene lookup
        // as well: a contact lands on whichever surface of the tree is under
        // it, honouring input regions, exactly as a click does. A contact on a
        // point no surface accepts is dropped rather than delivered to the
        // root — `wl_touch` has no "leave without down", so there is nothing to
        // send and the `down` simply does not happen.
        let Some((surface, origin)) = self.surface_under(id, x, y) else { return };
        // Logged, unlike pointer motion, because a contact is rare — a few per
        // gesture rather than one per pixel — and because "touch does nothing"
        // is otherwise indistinguishable from "no touch message ever arrived",
        // which on Win32 is the difference between this compositor being wrong
        // and the window never having opted out of synthesised mouse input.
        // The shape is in the line for the same reason it is conditional: a
        // client that gets no `shape` should be able to see that the digitiser
        // reported none.
        println!(
            "wc-compositor: window {id} touch down contact {slot} at {x:.0},{y:.0} ({})",
            match shape {
                Some(s) => format!(
                    "{:.0}x{:.0} at {:.0} deg",
                    s.major, s.minor, s.orientation
                ),
                None => "no contact geometry reported".to_owned(),
            }
        );
        self.touch_contacts.retain(|(s, _)| *s != slot);
        self.touch_contacts.push((slot, surface.clone()));
        touch.down(
            self,
            // The surface origin in compositor space, found the same way it is
            // in `pointer_motion` and for the same reason — §24 said touch
            // reuses the pointer's conversion rather than doing its own
            // arithmetic, and the way to keep that promise is to call the same
            // function rather than to write the same expression.
            Some((surface, origin.into())),
            &DownEvent {
                slot: Some(slot as u32).into(),
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        self.touch_shape(slot, shape);
    }

    fn touch_motion(
        &mut self,
        id: WindowId,
        slot: i32,
        x: f64,
        y: f64,
        shape: Option<crate::platform::TouchShape>,
        time: u32,
    ) {
        let Some(touch) = self.touch.clone() else { return };
        // Passed for smithay's benefit and **not** to change the focus:
        // `TouchHandle::motion` documents that the focus is only ever set by
        // `down`, and uses this to find drag-and-drop targets under a moving
        // contact. Which is why a motion for a window that has gone away is
        // still worth delivering — the contact's focus is the surface it landed
        // on, not this one.
        let focus = self.surface_under(id, x, y).map(|(s, origin)| (s, origin.into()));
        touch.motion(
            self,
            focus,
            &TouchMotionEvent { slot: Some(slot as u32).into(), location: (x, y).into(), time },
        );
        self.touch_shape(slot, shape);
    }

    fn touch_up(&mut self, id: WindowId, slot: i32, time: u32) {
        let Some(touch) = self.touch.clone() else { return };
        self.touch_contacts.retain(|(s, _)| *s != slot);
        println!("wc-compositor: window {id} touch up contact {slot}");
        touch.up(
            self,
            &UpEvent {
                slot: Some(slot as u32).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
    }

    /// The `shape`/`orientation` pair, or neither.
    ///
    /// `None` is the ordinary case and it is not a gap being papered over: the
    /// protocol says `shape` "is only sent by the compositor if the touch
    /// device supports shape reports" and that a client "has to make reasonable
    /// assumptions about the shape if it did not receive this event". Windows
    /// says whether it has one in `POINTER_TOUCH_INFO::touchMask`, that answer
    /// is carried here unchanged, and nothing is manufactured to fill it in.
    ///
    /// Both events go out **before** the `frame` the platform will send, which
    /// is what the protocol requires and is why this is called from inside the
    /// down and motion handlers rather than being an event of its own.
    fn touch_shape(&mut self, slot: i32, shape: Option<crate::platform::TouchShape>) {
        let Some(touch) = self.touch.clone() else { return };
        let Some(shape) = shape else { return };
        let slot = Some(slot as u32).into();
        touch.shape(self, &ShapeEvent { slot, major: shape.major, minor: shape.minor });
        touch.orientation(self, &OrientationEvent { slot, orientation: shape.orientation });
    }

    /// The user changed their Windows keyboard layout under a running client.
    ///
    /// `set_keymap_from_string` re-sends `wl_keyboard.keymap` to every bound
    /// keyboard, which is the protocol's own answer to this and is why the
    /// keymap is an event rather than a one-off — clients are required to
    /// handle it arriving again.
    fn set_layout(&mut self, name: &'static str) {
        let Some(keyboard) = self.keyboard.clone() else { return };
        let Some(l) = crate::input::layout(name) else { return };
        match keyboard.set_keymap_from_string(self, l.keymap.to_owned()) {
            Ok(()) => println!("wc-compositor: keyboard layout -> {}", l.name),
            Err(e) => eprintln!("wc-compositor: could not switch to layout {}: {e}", l.name),
        }
    }

    /// Tell the platform to stop pumping. Sending this rather than dropping the
    /// channel is what makes shutdown ordered: the UI thread destroys its own
    /// windows, from the thread that owns them, before it returns.
    pub fn quit(&self) {
        self.ui.send(ToUi::Quit);
    }

    fn on_resized(&mut self, id: WindowId, width: i32, height: i32) {
        let Some(window) = self.by_id(id) else { return };
        // **This is the whole of the resize negotiation on the Wayland side.**
        // Windows has already resized the frame and is painting something (see
        // `WindowState::paint` for what); all that can be done here is tell the
        // client the new size and wait for it to commit at that size.
        // `send_pending_configure` sends nothing when the size is unchanged,
        // which matters because Windows emits a `WM_SIZE` for every pixel of a
        // drag and a configure per pixel is legal but pointless.
        // Only a toplevel is resized by the system. A popup's size is the
        // client's, arrived at through the positioner, and a compositor that
        // configured one to a different size would be answering a question
        // nobody asked.
        let Some(toplevel) = window.role.toplevel() else { return };
        toplevel.with_pending_state(|state| {
            state.size = Some((width, height).into());
        });
        if let Some(serial) = toplevel.send_pending_configure() {
            println!(
                "wc-compositor: window {id} configure {width}x{height} (serial {})",
                u32::from(serial)
            );
        }
    }

    fn set_state<F: FnOnce(&mut ToplevelState)>(&mut self, id: WindowId, f: F) {
        let Some(window) = self.by_id(id) else { return };
        let Some(toplevel) = window.role.toplevel() else { return };
        toplevel.with_pending_state(f);
        toplevel.send_pending_configure();
    }

    /// Copy a committed buffer into the shadow and tell the platform.
    fn present(&mut self, surface: &WlSurface, buffer: &wl_buffer::WlBuffer, damage: Vec<Rect>) {
        // REWRITTEN FOR STAGE 9. Until this stage a window's shadow *was* one
        // client buffer, and every surface that was not a toplevel had its
        // pixels thrown away. That is exactly wrong for the client this project
        // exists to run: Firefox's toplevel `wl_surface` is GTK's
        // client-side-decoration surface, which is **transparent**, and every
        // pixel of the browser is in a `wl_subsurface` of it. A compositor with
        // no scene graph does not draw Firefox badly — it draws a black
        // rectangle. docs/waycompose-design.md §13.5 has the measurement.
        //
        // So a commit now lands in a per-surface staging shadow, and the
        // window's shadow is the composite of the tree.
        let root = root_of(surface);
        let is_root_of_a_window = self.windows.contains_key(&root);

        let staging = self.staging.entry(surface.clone()).or_insert_with(Shadow::empty);
        let mut result: Option<(Vec<Rect>, i32, i32)> = None;
        let mut served_by = "";
        // Ask each source in turn. Today there is one; the loop is what lets a
        // buffer that lives in an arena rather than a `wl_shm` pool arrive
        // without this function changing (docs/goal-firefox.md).
        for source in &self.sources {
            let read = source.with_pixels(buffer, &mut |bytes, spec| {
                result = Some((staging.blit(bytes, spec, &damage), spec.width, spec.height));
            });
            match read {
                Ok(()) => {
                    served_by = source.name();
                    break;
                }
                Err(PixelError::UnknownSource) => continue,
                Err(e) => {
                    eprintln!("wc-compositor: surface commit: {e}");
                    break;
                }
            }
        }

        // **Release before presenting, and unconditionally.** The client may
        // reuse the buffer the instant this returns, which is exactly what a
        // single-buffered client is waiting to do; the pixels it might overwrite
        // are already in the shadow. Releasing only on success would deadlock
        // any client whose buffer we failed to read.
        buffer.release();

        let Some((written, _, _)) = result else {
            eprintln!("wc-compositor: no pixel source could read the committed buffer");
            return;
        };
        if written.is_empty() {
            return;
        }
        // STAGE 13. A cursor surface leaves here and never reaches
        // `compose_tree`. Checked before the window test rather than after it,
        // and by identity rather than by "it has no window", because the
        // guarantee this stage owes the user is *this* one: a cursor is never
        // content. The staging entry is dropped as well, so there is nothing
        // left for a future traversal to find.
        if self.cursor_surface.as_ref() == Some(surface) {
            self.staging.remove(surface);
            return;
        }
        if !is_root_of_a_window {
            // A subsurface of a window that has not mapped yet, or a cursor
            // surface for a seat that has since been given a different one.
            // The pixels are kept; there is nowhere to put them.
            return;
        }

        // Composite the tree, and find out what that changed. `Some(rects)` is
        // the damage in window coordinates; `None` means the canvas itself was
        // reshaped and everything is new.
        let (composited, cw, ch) = self.compose_tree(&root, surface, &written);
        if cw <= 0 || ch <= 0 {
            // The tree has no canvas yet. This is not a corner case: Firefox
            // commits the **subsurface** holding the browser before it commits
            // the toplevel surface behind it, so the first commit of a window's
            // life arrives when the root has no buffer, no geometry and
            // therefore no size. Mapping here sent `ToUi::Map` with 0x0, which
            // the platform dutifully made a zero-sized window out of. The
            // pixels are already in staging and the root's own commit is a
            // millisecond behind.
            return;
        }

        let window = self.windows.get_mut(&root).expect("checked above");
        let id = window.id;
        let (w, h) = (cw, ch);
        window.presented += 1;
        let first = !window.mapped;
        window.mapped = true;
        let (title, app_id) = (window.title.clone(), window.app_id.clone());
        let client = window.client;
        // ADDED FOR STAGE 32. A popup has no taskbar button — it is
        // `WS_EX_TOOLWINDOW` with an owner — so there is nothing for an
        // identity to group, and stamping one on a menu would be a claim about
        // a button that does not exist. Measured, and not hypothetical: the
        // first run of the `apps` experiment put `Waycompose.unnamed.c1` on
        // three of `weston-terminal`'s context menus, harmlessly and wrongly.
        let toplevel = window.role.toplevel().is_some();
        self.presents += 1;

        if first {
            // Which source served this surface, said once per window. When
            // there is a second one — an arena for a client on the other side
            // of a hypervisor (docs/goal-firefox.md) — this line is how anyone
            // tells which path a window actually took, and a stage that added a
            // provider without that would not really have added it.
            // The counters come from `wc_handle::Handle::map`, which is the
            // function that actually opened the object, rather than from
            // anything this file could infer. Stage 4 gives a `wl_shm` pool
            // three possible homes — a named section, a path both sides can
            // reach, and a window into an arena — and all three arrive through
            // the same smithay code, so `served_by` alone stopped being enough
            // to answer "which mechanism carried these pixels".
            let (sections, files, slices) = wc_handle::map_counts();
            println!(
                "wc-compositor: window {id} pixels served by {served_by} \
                 (mapped: {sections} section, {files} shared-file, {slices} arena-slice)"
            );
            self.report_decorator(&root, id, &app_id);
            // ADDED FOR STAGE 9. `wl_surface.enter` is how a client learns
            // which output it is on, and therefore what scale to draw at and
            // what refresh rate to expect. It is sent at map because before a
            // surface has content it is not on any output — and because
            // smithay's `Output::enter` walks the surface's client list, which
            // needs the surface to be alive on the wire.
            self.output.enter(&root);
            // BEFORE `Map`, AND THAT IS THE WHOLE OF STAGE 32'S ORDERING.
            //
            // `Map` is what shows the window, and the shell makes the taskbar
            // button when the window is shown — it reads the identity property
            // then. Until stage 32 this went out *after* `Map`, which was
            // harmless when nothing read it and is not harmless now: the button
            // would be created under whatever identity the window was created
            // with, and a client that called `set_app_id` between creation and
            // its first buffer — which is every client, because `set_app_id`
            // costs no round trip and drawing does — would have its real name
            // arrive one message too late.
            //
            // Sent unconditionally, empty app id included: "this client never
            // named itself" is an identity too (`crate::taskbar`), and skipping
            // it would leave the nameless windows sharing whatever the shell
            // decided for the process.
            if toplevel {
                self.ui.send(ToUi::AppId {
                    id,
                    app_id: app_id.clone(),
                    aumid: crate::taskbar::identity(&app_id, client),
                });
            }
            self.ui.send(ToUi::Map {
                id,
                width: w,
                height: h,
            });
            if !title.is_empty() {
                self.ui.send(ToUi::Title { id, title });
            }
            // At map, and not before: a window that is not on screen has no
            // taskbar button to put an icon on, and `WM_SETICON` to a hidden
            // window is a message the shell never reads. The resolver may have
            // published long before this — it sees `set_app_id` on the wire,
            // which is several round trips ahead of the first buffer — which is
            // exactly why the store is keyed by app id and not by window.
            self.apply_stored_icon(id, &app_id);
        }
        self.ui.send(ToUi::Present {
            id,
            damage: composited,
        });
    }

    // -----------------------------------------------------------------------
    // Icons, stage 11
    // -----------------------------------------------------------------------

    /// Put whatever has been published for `app_id` on window `id`.
    ///
    /// **Always sends something.** An app id nobody has published for sends an
    /// *empty* set, which the platform turns into "leave the class icon
    /// alone" — the generic Windows application icon, which is what stage 8
    /// photographed. A missing icon has to degrade to that rather than to a
    /// blank button or a silence, and making the empty case an ordinary message
    /// rather than a `return` is what keeps the two paths the same length.
    fn apply_stored_icon(&mut self, id: WindowId, app_id: &str) {
        if self.debug_icon {
            self.ui.send(ToUi::Icon {
                id,
                icons: vec![debug_icon(id)],
            });
            return;
        }
        let icons = self.icons.get(app_id).map(<[IconImage]>::to_vec);
        match icons {
            Some(icons) => {
                self.icons_applied += 1;
                println!(
                    "wc-compositor: window {id} icon <- {} image(s) published for {app_id:?}",
                    icons.len()
                );
                self.ui.send(ToUi::Icon { id, icons });
            }
            None => {
                self.icons_missing += 1;
                // Both halves of the sentence matter: an empty app id is a
                // client that never called `set_app_id` and cannot be resolved
                // by anybody, and a non-empty one is a resolver that has not
                // answered — one is the client's doing and the other is ours.
                if app_id.is_empty() {
                    println!(
                        "wc-compositor: window {id} has no app id and no icon; \
                         nothing can be resolved for it"
                    );
                } else {
                    println!(
                        "wc-compositor: window {id} app id {app_id:?} has no published icon \
                         (the store holds {})",
                        self.icons.len()
                    );
                }
                self.ui.send(ToUi::Icon {
                    id,
                    icons: Vec::new(),
                });
            }
        }
    }

    /// Has anything at all been published this run? The difference between a
    /// resolver that answered "no icon" and a resolver that was never there.
    pub fn icons_is_empty(&self) -> bool {
        self.icons.is_empty()
    }

    /// A publisher has spoken: install the set and update every window that is
    /// already showing that app id.
    ///
    /// Windows whose client has set an icon of its own are skipped —
    /// `xdg-toplevel-icon-v1` beats a name lookup, always, because one is the
    /// application saying what it looks like and the other is a guess made from
    /// a string.
    fn publish_icons(&mut self, app_id: String, icons: Vec<IconImage>) {
        println!(
            "wc-compositor: icon published for {app_id:?}: {} image(s) [{}]",
            icons.len(),
            icons
                .iter()
                .map(|i| format!("{}x{}", i.width, i.height))
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.icons.publish(app_id.clone(), icons);
        let targets: Vec<WindowId> = self
            .windows
            .values()
            .filter(|w| w.mapped && !w.icon_from_client && w.app_id == app_id)
            .map(|w| w.id)
            .collect();
        for id in targets {
            self.apply_stored_icon(id, &app_id);
        }
    }

    /// The client set its own icon through `xdg-toplevel-icon-v1`, and this
    /// commit is where the double-buffered state became current.
    ///
    /// The buffers are read through the same [`PixelSource`] list the window's
    /// content goes through, which is not tidiness: an icon buffer from a
    /// client on the other side of a hypervisor arrives in the arena like any
    /// other, and a second reader that only understood `wl_shm` would work on
    /// this machine and not on the one that matters.
    fn take_client_icon(&mut self, surface: &WlSurface) {
        use smithay::wayland::xdg_toplevel_icon::ToplevelIconCachedState;

        let (name, buffers) = compositor::with_states(surface, |states| {
            let mut guard = states.cached_state.get::<ToplevelIconCachedState>();
            let current = guard.current();
            (
                current.icon_name().map(str::to_owned),
                current.buffers().to_vec(),
            )
        });
        let Some(window) = self.windows.get(surface) else {
            return;
        };
        let (id, app_id) = (window.id, window.app_id.clone());

        let mut icons = Vec::new();
        for (buffer, scale) in &buffers {
            let mut got = None;
            for source in &self.sources {
                let mut taken = None;
                match source.with_pixels(buffer, &mut |bytes, spec| {
                    taken = crate::icon::image_from_bytes(bytes, spec);
                }) {
                    Ok(()) => {
                        got = taken;
                        break;
                    }
                    Err(PixelError::UnknownSource) => continue,
                    Err(e) => {
                        eprintln!("wc-compositor: window {id}: icon buffer: {e}");
                        break;
                    }
                }
            }
            match got {
                Some(image) => icons.push(image),
                None => eprintln!(
                    "wc-compositor: window {id}: an xdg_toplevel_icon buffer (scale {scale}) \
                     could not be read; skipping it"
                ),
            }
        }

        if icons.is_empty() {
            // A name and no buffers is the other half of the protocol, and it
            // is the half this compositor cannot honour on its own: the icon
            // theme is on the client's machine. The name is looked up in the
            // store anyway, because the resolver publishes under names it took
            // from desktop entries and an application's icon name is usually
            // its app id — when it is not, this is a miss and says so.
            if let Some(name) = name {
                println!(
                    "wc-compositor: window {id} set an icon *name*, {name:?}; \
                     there is no icon theme on this machine, so it is looked up as an app id"
                );
                if let Some(window) = self.windows.get_mut(surface) {
                    window.icon_from_client = false;
                }
                self.apply_stored_icon(id, &name);
                return;
            }
            // `set_icon(null)` — the client took its icon back.
            println!("wc-compositor: window {id} cleared its icon; falling back to {app_id:?}");
            if let Some(window) = self.windows.get_mut(surface) {
                window.icon_from_client = false;
            }
            self.apply_stored_icon(id, &app_id);
            return;
        }

        if let Some(window) = self.windows.get_mut(surface) {
            window.icon_from_client = true;
        }
        self.icons_applied += 1;
        println!(
            "wc-compositor: window {id} icon <- {} buffer(s) from the client's own \
             xdg_toplevel_icon_v1",
            icons.len()
        );
        self.ui.send(ToUi::Icon { id, icons });
    }

    /// Is this surface a client's cursor image?
    ///
    /// ADDED FOR STAGE 12. `wl_pointer.set_cursor` gives the surface the
    /// `"cursor_image"` role and smithay refuses to give it any other, so the
    /// role is the authority on the question — better than comparing against
    /// [`Self::cursor_surface`], which is only set once the compositor has been
    /// *told* about the surface and is therefore empty for the first commit of
    /// a client that attaches before it calls `set_cursor`.
    fn is_cursor_surface(surface: &WlSurface) -> bool {
        compositor::get_role(surface) == Some(CURSOR_IMAGE_ROLE)
    }

    /// A cursor-role surface committed a buffer: keep its pixels, and if it is
    /// the cursor the pointer is wearing, show them.
    ///
    /// ADDED FOR STAGE 12. Called from `commit` **before** the ordinary path,
    /// and it does not divert it: whether a cursor surface is also composited
    /// as content is a scene-graph question and is answered in `present` /
    /// `compose_tree`, not here. This function only reads.
    ///
    /// The buffer is read through the same [`PixelSource`] list a window's
    /// buffer goes through, so a cursor whose pixels live in the arena rather
    /// than in a `wl_shm` pool costs nothing extra here.
    fn cursor_committed(&mut self, surface: &WlSurface, buffer: &wl_buffer::WlBuffer) {
        let mut got = None;
        for source in &self.sources {
            let mut taken = None;
            match source.with_pixels(buffer, &mut |bytes, spec| {
                taken = crate::icon::image_from_bytes(bytes, spec);
            }) {
                Ok(()) => {
                    got = taken;
                    break;
                }
                Err(PixelError::UnknownSource) => continue,
                Err(e) => {
                    eprintln!("wc-compositor: cursor buffer: {e}");
                    break;
                }
            }
        }
        let Some(image) = got else {
            eprintln!("wc-compositor: a cursor surface committed a buffer that could not be read");
            return;
        };

        self.cursor_pixels = Some((surface.clone(), image));
        if self.cursor_surface.as_ref() == Some(surface) {
            self.send_cursor_surface(&surface.clone());
        }
    }

    /// Send the cached pixels of `surface` to the platform, if they are the
    /// ones it holds.
    ///
    /// The hotspot is read here rather than cached with the pixels, because it
    /// is a property of the `set_cursor` request and not of the buffer: a
    /// client may move the hotspot without redrawing. smithay has already
    /// divided out the client's buffer scale by the time it is in the data map.
    fn send_cursor_surface(&mut self, surface: &WlSurface) {
        let Some((cached, image)) = self.cursor_pixels.as_ref() else {
            return;
        };
        if cached != surface {
            return;
        }
        let image = image.clone();

        let (hotspot_x, hotspot_y) = compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<smithay::input::pointer::CursorImageSurfaceData>()
                .map(|d| {
                    let a = d.lock().unwrap();
                    (a.hotspot.x, a.hotspot.y)
                })
                // A cursor surface that has never been through `set_cursor` has
                // no hotspot recorded. Zero is the protocol's own default and
                // is what `set_cursor` would have written.
                .unwrap_or((0, 0))
        });

        self.ui.send(ToUi::Cursor {
            spec: CursorSpec::Image { image, hotspot_x, hotspot_y },
        });
    }

    /// Say, once per window, who this compositor believes is drawing its title
    /// bar — and say it loudly when the answer is "both of us".
    ///
    /// ADDED FOR STAGE 33. The user's report was *"wayland terminal renders
    /// also a double titlebar"*, and the reason it took a photograph to find is
    /// that the compositor's own account of that run could not see it. Every
    /// line was right: the manager's default was `Server`, the window mapped at
    /// the geometry rectangle, the caption carried the client's title. The one
    /// thing nothing recorded is that the client never read any of it.
    ///
    /// So this is a diagnostic and it is deliberately not a decision. What to
    /// *do* about a client that decorates itself is §30's arrangement — fold
    /// the caption into the client area with `WM_NCCALCSIZE` and
    /// `DwmExtendFrameIntoClientArea` so that the bar the client is going to
    /// draw anyway becomes the only one — and that is a per-window window
    /// style, on the Win32 side, which this branch does not have. The one thing
    /// that has to exist first is the ability to *tell*, and that is here.
    fn report_decorator(&self, root: &WlSurface, id: WindowId, app_id: &str) {
        use smithay::wayland::shell::xdg::SurfaceCachedState;
        let inset = compositor::with_states(root, |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
                .map_or((0, 0), |r| (r.loc.x, r.loc.y))
        });
        let negotiated = self.negotiated_decoration.contains(root);
        let who = decorator(inset, negotiated);
        // The verdict is the pair. See [`crate::surface::bars`]: §31's fold
        // reverses which of these two clients is the correct one, so a message
        // that named "two title bars" from `who` alone was right only while the
        // caption was never folded — which was true when §33 was written and
        // stopped being true when §31 merged.
        let folded = self.decorations.extends_frame();
        let app = if app_id.is_empty() { "<no app id>" } else { app_id };
        match bars(who, folded) {
            Bars::Agreed => println!(
                "wc-compositor: window {id} ({app}) negotiated its decorations; \
                 geometry inset {},{}",
                inset.0, inset.1
            ),
            Bars::OneSystem => println!(
                "wc-compositor: window {id} ({app}) bound no decoration protocol and has no \
                 margin around its window; Windows' caption is the only title bar"
            ),
            Bars::OneClient => println!(
                "wc-compositor: window {id} ({app}) draws its own title bar and this run folds \
                 the caption into the client area ({:?}), so that bar is the window's -- \
                 geometry inset {},{} (§31, §33.7)",
                self.decorations, inset.0, inset.1
            ),
            // The two that are defect reports. Named as such, because the
            // compositor is telling the truth about a window the user is about
            // to find wrong, and a line that read like the other two would be
            // read like the other two.
            Bars::Two => println!(
                "wc-compositor: window {id} ({app}) bound no decoration protocol and reports a \
                 {},{} margin around its window -- it is drawing its own title bar and did not \
                 read our default of {:?}, so THIS WINDOW HAS TWO TITLE BARS (§33)",
                inset.0, inset.1, self.decorations
            ),
            Bars::None => println!(
                "wc-compositor: window {id} ({app}) draws no title bar of its own and this run \
                 folds the caption away ({:?}), so THIS WINDOW HAS NO TITLE BAR -- \
                 `--decorations tabs` is right for a self-decorating client and wrong for this \
                 one, and it is a per-run flag (§33.7)",
                self.decorations
            ),
        }
    }

    /// Draw every surface of `root`'s tree into `root`'s window shadow, and say
    /// what that changed and how big the result is.
    ///
    /// ADDED FOR STAGE 9. This is the whole scene graph, and it is deliberately
    /// the smallest one that is *correct* rather than the fastest one:
    ///
    /// * The canvas is the **window geometry rectangle**
    ///   (`xdg_surface.set_window_geometry`), not the root buffer. A GTK client
    ///   commits a buffer that is larger than its window on every side — 26 px
    ///   of invisible resize margin and drop shadow — and says so with a
    ///   geometry rectangle. Drawing the buffer instead of the rectangle offsets
    ///   the window, shows the client's own title bar under the system one and
    ///   clips the opposite edges away. Stage 8 measured that with
    ///   `weston-terminal`; Firefox has it for the same reason, since it is
    ///   GTK's code in both.
    /// * Surfaces are drawn parent first, in the order the protocol stacks
    ///   them, which is what `with_surface_tree_upward` promises.
    /// * The whole tree is redrawn on every commit. One `memcpy` per surface
    ///   per frame, on the machine with spare CPU; the alternative is tracking
    ///   which regions of the canvas each surface owns, which is a renderer.
    ///   The **damage** reported is still only what the committing surface
    ///   changed, so the Win32 side does not repaint the world.
    fn compose_tree(
        &mut self,
        root: &WlSurface,
        committed: &WlSurface,
        committed_damage: &[Rect],
    ) -> (Vec<Rect>, i32, i32) {
        use smithay::wayland::shell::xdg::SurfaceCachedState;

        // Where each surface of the tree sits, relative to the root surface's
        // own origin. Collected first because composing needs `&mut` on the
        // window's shadow while `staging` is read, and because the traversal
        // closure cannot borrow `self`.
        let placed = placed_tree(root);

        // The window geometry rectangle, in root-surface coordinates. Absent
        // means "the whole buffer", which is what the protocol says and what
        // every non-GTK client relies on.
        let geometry = compositor::with_states(root, |states| {
            states.cached_state.get::<SurfaceCachedState>().current().geometry
        });
        // **"The full bounds of the surface, *including any subsurfaces*."**
        //
        // That is `xdg_surface.set_window_geometry`'s wording for what the
        // geometry is when a client never sets one, and until §40 the last
        // three words were missing here: the fallback was the root buffer's
        // size, so a `wl_subsurface` hanging over its parent's edge was
        // clipped away — not drawn, and therefore not clickable either, since
        // the window is the canvas and `surface_under` hit-tests what the
        // window contains.
        //
        // WLCS builds exactly that shape on purpose (`SubsurfaceBuilder` in
        // `surface_builder.cpp` puts a 215x108 subsurface on an 80x50 parent),
        // and it is not a contrived one: a video player whose overlay controls
        // extend past the video, or any client that positions a subsurface at
        // a negative offset, has it.
        //
        // **This changes nothing for a client without subsurfaces**, which is
        // the case every earlier stage measured: the union of a one-surface
        // tree is that surface's own rectangle at `(0, 0)`, which is the
        // `root_size` this replaces. And it changes nothing for a client that
        // *does* set a geometry — Firefox, `weston-terminal`, every GTK
        // application — because that is still the branch above.
        let bounds = placed
            .iter()
            .filter_map(|(surface, (x, y))| {
                let s = self.staging.get(surface)?;
                (s.width() > 0 && s.height() > 0)
                    .then(|| (*x, *y, x + s.width(), y + s.height()))
            })
            .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)));
        let (gx, gy, gw, gh) = match geometry {
            Some(r) if r.size.w > 0 && r.size.h > 0 => (r.loc.x, r.loc.y, r.size.w, r.size.h),
            // The origin is part of the answer, not assumed to be `(0, 0)`: a
            // subsurface may sit at a negative offset, and then the tree's
            // top-left is above and to the left of the root's. That flows into
            // `content_offset` and out through `surface_origin`, whose sign
            // convention already handles a negative inset and is unit-tested
            // for it.
            _ => match bounds {
                Some((x0, y0, x1, y1)) => (x0, y0, x1 - x0, y1 - y0),
                None => (0, 0, 0, 0),
            },
        };
        let format = self
            .staging
            .get(root)
            .map(|s| s.format())
            .unwrap_or(PixelFormat::Xrgb8888);

        let window = self.windows.get_mut(root).expect("caller checked");
        // The one place this number is decided, for the pixels *and* for the
        // pointer. See `Window::content_offset`.
        window.content_offset = (gx, gy);
        let shadow = window.shadow.clone();
        let mut shadow = shadow.lock().unwrap_or_else(|e| e.into_inner());
        let reshaped = shadow.reset(gw, gh, format);
        for (surface, (x, y)) in &placed {
            if let Some(src) = self.staging.get(surface) {
                shadow.compose(src, (x - gx, y - gy));
            }
        }
        drop(shadow);

        if reshaped {
            return (vec![Rect::new(0, 0, gw, gh)], gw, gh);
        }
        // Translate the committing surface's damage into window coordinates.
        let (cx, cy) = placed
            .iter()
            .find(|(s, _)| s == committed)
            .map(|(_, l)| *l)
            .unwrap_or((0, 0));
        let damage: Vec<Rect> = committed_damage
            .iter()
            .filter_map(|r| {
                Rect::new(r.x + cx - gx, r.y + cy - gy, r.w, r.h).clip(gw, gh)
            })
            .collect();
        (damage, gw, gh)
    }
}

/// Every surface of `root`'s subsurface tree, with its position relative to
/// `root`'s own origin, **in paint order**: parent before child, and siblings
/// in the order `wl_subsurface.place_above`/`place_below` put them.
///
/// ADDED FOR §40, by lifting the walk out of `compose_tree` unchanged. It is a
/// free function with one caller-visible promise — the order — because it now
/// has two callers that must not disagree: the blit, which paints the list
/// front to back, and [`State::surface_under`], which hit-tests it back to
/// front. Hit-testing is the exact inverse of painting, and the only way to
/// keep that true as the tree grows a case is to derive both from one list.
///
/// `with_surface_tree_upward` is what promises the order, and the two closures
/// add the subsurface offset in the same way for the same reason: the pre-order
/// closure passes the accumulated position down to the children, and the
/// post-order one records it.
fn placed_tree(root: &WlSurface) -> Vec<(WlSurface, (i32, i32))> {
    use smithay::wayland::compositor::{
        SubsurfaceCachedState, TraversalAction, SUBSURFACE_ROLE,
    };
    let mut placed: Vec<(WlSurface, (i32, i32))> = Vec::new();
    compositor::with_surface_tree_upward(
        root,
        (0i32, 0i32),
        |_, states, loc| {
            let mut loc = *loc;
            if states.role == Some(SUBSURFACE_ROLE) {
                let p = states.cached_state.get::<SubsurfaceCachedState>().current().location;
                loc = (loc.0 + p.x, loc.1 + p.y);
            }
            TraversalAction::DoChildren(loc)
        },
        |surface, states, loc| {
            let mut loc = *loc;
            if states.role == Some(SUBSURFACE_ROLE) {
                let p = states.cached_state.get::<SubsurfaceCachedState>().current().location;
                loc = (loc.0 + p.x, loc.1 + p.y);
            }
            placed.push((surface.clone(), loc));
        },
        |_, _, _| true,
    );
    placed
}

/// Whether `surface` accepts input at `point`, in its own surface-local
/// coordinates.
///
/// ADDED FOR §40. Two rules, in this order, and the order is the protocol's:
///
/// 1. **The point must be on the surface.** `wl_surface.set_input_region` says
///    the region "is clipped to the surface size", so a region larger than the
///    buffer does not extend it — which is what WLCS's `ClippedLargerRegion`
///    parameterisation exists to check, and which is why the buffer bound is
///    tested even when a region was set.
/// 2. **An absent region means the whole surface**, not an empty one. That is
///    the protocol's initial value ("infinite"), and reading `None` as "no
///    input" would silently make every ordinary client unclickable.
///
/// 3. **A surface with no size takes no input**, whatever its region says.
///    That is the state a `wl_subsurface` sits in between `get_subsurface` and
///    its first commit, and smithay's own `contains_point` answers the same
///    way.
///
/// The size comes from the staging shadow rather than from a stored field for
/// the same reason `mapped_windows` reads it there: the shadow is reset to the
/// buffer's dimensions on every commit, so it cannot disagree with what the
/// client last drew.
///
/// **The region is an argument rather than something read from the surface,
/// and that is what makes the rules testable.** A `WlSurface` needs a live
/// display and a connected client to exist, so a function that took one could
/// only ever be exercised through the whole conformance suite — the same
/// hundred-second feedback loop that let the placeholder this replaces survive
/// as long as it did. The protocol lookup is one line in the caller.
fn accepts_input_at(
    size: (i32, i32),
    region: Option<&compositor::RegionAttributes>,
    point: (f64, f64),
) -> bool {
    if size.0 <= 0 || size.1 <= 0 {
        return false;
    }
    if point.0 < 0.0 || point.1 < 0.0 || point.0 >= f64::from(size.0) || point.1 >= f64::from(size.1)
    {
        return false;
    }
    match region {
        None => true,
        Some(region) => region.contains((point.0.round() as i32, point.1.round() as i32)),
    }
}

/// Where a surface's origin sits in its window's pixels, given the window
/// geometry offset the blit used.
///
/// ADDED FOR §37, as one function rather than one expression in three places.
///
/// # The sign, worked out rather than guessed
///
/// `compose_tree` blits every surface of the tree at `(x - gx, y - gy)`, so
/// window pixel `(0, 0)` shows the buffer's pixel `(gx, gy)`. Equivalently the
/// buffer's origin — surface point `(0, 0)` — is at window pixel `(-gx, -gy)`,
/// which is off the top-left of the window and is exactly right: an inset says
/// the client committed more buffer than window, and the extra is above and to
/// the left.
///
/// smithay takes the origin in that form and subtracts it
/// (`event.location - loc`), so a pointer at window pixel `(x, y)` reaches the
/// client as surface point `(x + gx, y + gy)` — the same arithmetic the blit
/// did, in the same direction.
///
/// **The numbers this is wrong by, when it is missing.** Weston's toytoolkit —
/// `weston-terminal`, `weston-eventdemo` — declares `(32, 32)`; Firefox under
/// client-side decorations declares `(26, 23)` and under server-side ones
/// `(0, 0)`, which is why the bug was invisible in half of the desktop.
pub(crate) fn surface_origin(content_offset: (i32, i32)) -> (f64, f64) {
    (-f64::from(content_offset.0), -f64::from(content_offset.1))
}

/// The root of `surface`'s subsurface tree — the surface that has the
/// `xdg_toplevel`, if any.
///
/// ADDED FOR STAGE 9.
fn root_of(surface: &WlSurface) -> WlSurface {
    let mut s = surface.clone();
    while let Some(parent) = compositor::get_parent(&s) {
        s = parent;
    }
    s
}

/// Shorthand so the state names below read as protocol rather than as paths.
mod xdg_state {
    pub use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::*;
}

type XdgState = smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;

fn toggle(states: &mut ToplevelState, which: XdgState, on: bool) {
    if on {
        states.states.set(which);
    } else {
        states.states.unset(which);
    }
}

/// A recognisable icon, so the platform's icon path runs under `--debug-icon`.
///
/// Four quadrants in flat colours, one of them white, chosen by the window id —
/// so a screenshot shows *which* window's icon it is rather than merely that an
/// icon exists.
fn debug_icon(id: WindowId) -> IconImage {
    const N: i32 = 32;
    let mut pixels = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let o = ((y * N + x) * 4) as usize;
            let quadrant = u8::from(x >= N / 2) | (u8::from(y >= N / 2) << 1);
            let (b, g, r) = match quadrant {
                0 => (0x40u8, 0x40u8, 0xE0u8),
                1 => (0x40, 0xE0, 0x40),
                2 => (0xE0, 0x40, 0x40),
                _ => (0x20, 0xC0, 0xC0),
            };
            let lit = (id % 4) == u64::from(quadrant);
            pixels[o] = if lit { 0xFF } else { b };
            pixels[o + 1] = if lit { 0xFF } else { g };
            pixels[o + 2] = if lit { 0xFF } else { r };
            pixels[o + 3] = 0xFF;
        }
    }
    IconImage {
        width: N,
        height: N,
        pixels,
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn new_surface(&mut self, _surface: &WlSurface) {
        println!("wc-compositor: wl_surface created");
    }

    /// A `wl_surface` went away.
    ///
    /// ADDED FOR §40, for one reason: **a contact still down on it has to come
    /// up.** `wl_touch` has no `leave` — `down` binds a contact to a surface
    /// and only `up` or `cancel` releases it — so a client whose surface is
    /// destroyed mid-gesture is otherwise left holding a finger it can never
    /// put down, and the slot cannot be reused. WLCS asks this as
    /// `TouchTest.sends_touch_up_on_surface_destroy`, against every surface
    /// type including both subsurface shapes.
    ///
    /// `up` rather than `cancel`, deliberately. `wl_touch.cancel` means "the
    /// compositor took the gesture away and you should undo what it did" — an
    /// edge swipe, a system gesture — and it ends *every* live contact on the
    /// seat at once. A surface being destroyed is neither: the other contacts,
    /// on other surfaces, are unaffected and must not be cancelled, and the
    /// client has no reason to roll anything back. The contact simply ended.
    ///
    /// The staging entry goes too. `commit` already drops it for a null
    /// buffer; this is the other way a surface's pixels stop existing, and
    /// leaving them behind would keep the surface's size answerable to
    /// `surface_under` after the surface itself was gone.
    fn destroyed(&mut self, surface: &WlSurface) {
        self.staging.remove(surface);
        let ended: Vec<i32> = self
            .touch_contacts
            .iter()
            .filter(|(_, s)| s == surface)
            .map(|(slot, _)| *slot)
            .collect();
        if ended.is_empty() {
            return;
        }
        let Some(touch) = self.touch.clone() else { return };
        self.touch_contacts.retain(|(_, s)| s != surface);
        let time = self.now_ms();
        for slot in ended {
            println!("wc-compositor: touch contact {slot} ended: its surface was destroyed");
            touch.up(
                self,
                &UpEvent {
                    slot: Some(slot as u32).into(),
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                },
            );
        }
        // One frame for the group, as for every other batch of touch events.
        touch.frame(self);
    }

    fn commit(&mut self, surface: &WlSurface) {
        println!("wc-compositor: wl_surface commit");

        // Take everything this commit carried, and **clear it**. smithay's
        // documentation on `SurfaceAttributes` is explicit that `buffer`,
        // `damage` and `frame_callbacks` accumulate from commit to commit until
        // the compositor empties them; leaving them would replay the first
        // frame's damage forever and answer each frame callback repeatedly.
        let (buffer, damage, callbacks, scale, transform) =
            compositor::with_states(surface, |states| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                let attrs = guard.current();
                (
                    attrs.buffer.take(),
                    std::mem::take(&mut attrs.damage),
                    std::mem::take(&mut attrs.frame_callbacks),
                    attrs.buffer_scale,
                    attrs.buffer_transform,
                )
            });

        if let Some(window) = self.windows.get_mut(surface) {
            window.frame_callbacks.extend(callbacks);
        } else {
            // A surface with no window of its own — a cursor, or a subsurface.
            // Its callbacks still have to be answered or its client stops
            // drawing, and answering at once is right because nothing is
            // throttling it to a display.
            let now = self.now_ms();
            for cb in callbacks {
                cb.done(now);
            }
        }

        // Buffer scale and transform are honoured only in their identity form.
        // Anything else would need the damage rectangles mapped between surface
        // and buffer coordinates and the blit to rotate or subsample, which is
        // a renderer's job and not this stage's. Saying so out loud beats
        // silently drawing the wrong thing.
        if scale != 1 || transform != wl_output::Transform::Normal {
            eprintln!(
                "wc-compositor: buffer scale {scale} / transform {transform:?} is not implemented; \
                 drawing the buffer unscaled"
            );
        }

        let rects: Vec<Rect> = damage
            .into_iter()
            .map(|d| match d {
                // With scale 1 and no transform these two coordinate spaces are
                // identical, which is why they can be merged here and could not
                // be in general.
                Damage::Buffer(r) => Rect::new(r.loc.x, r.loc.y, r.size.w, r.size.h),
                Damage::Surface(r) => Rect::new(r.loc.x, r.loc.y, r.size.w, r.size.h),
            })
            .collect();

        // After the buffer, deliberately. The first commit is what maps the
        // window, and a window that is not mapped has no taskbar button for an
        // icon to be on; doing this first would send a `WM_SETICON` for a
        // window the platform has not shown yet, which is legal, invisible and
        // impossible to tell from a broken icon.
        let icon_dirty = self
            .windows
            .get_mut(surface)
            .map(|w| std::mem::take(&mut w.icon_dirty))
            .unwrap_or(false);

        match buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                // ADDED FOR STAGE 12 (docs/waycompose-design.md §23). A cursor surface's
                // buffer is read for the `HCURSOR` and then handed on
                // unchanged — this reads and does not divert, so what the
                // scene graph does with a cursor surface stays the scene
                // graph's decision. `present` releases the buffer, which is
                // why this runs first.
                if Self::is_cursor_surface(surface) {
                    self.cursor_committed(surface, &buffer);
                }
                self.present(surface, &buffer, rects)
            }
            Some(BufferAssignment::Removed) => {
                // **The staging entry goes with the buffer, and until §40 it
                // did not.** A null buffer unmaps the surface — it has no
                // content and, the protocol is explicit, no input either. But
                // the pixels stayed in `staging`, so the surface kept its size
                // for anything that asked; `surface_under` takes its size from
                // there, so an unmapped subsurface went on swallowing every
                // click that landed on where it used to be, invisibly and
                // forever. That is WLCS's
                // `input_not_seen_in_region_after_null_buffer_committed`, and
                // it is the same leak `cursor_surface` above already guards
                // against by hand.
                self.staging.remove(surface);
                if let Some(window) = self.windows.get_mut(surface) {
                    // A null buffer unmaps the surface. The toplevel still
                    // exists, so the window is not destroyed — it has nothing
                    // to show, which on Win32 is a minimised window.
                    let id = window.id;
                    println!("wc-compositor: window {id} unmapped (null buffer)");
                    window.mapped = false;
                    self.ui.send(ToUi::SetWindowState {
                        id,
                        maximized: None,
                        minimized: Some(true),
                    });
                } else {
                    // A subsurface went away. The window it belongs to is still
                    // mapped and is still showing the pixels this surface put
                    // there, so the tree has to be recomposed — otherwise the
                    // input answer changes immediately (correct) and the
                    // picture does not (wrong), which is the more confusing
                    // half of the bug rather than the lesser one.
                    let root = root_of(surface);
                    if self.windows.contains_key(&root) {
                        let (damage, w, h) = self.compose_tree(&root, surface, &[]);
                        if w > 0 && h > 0 {
                            let id = self.windows[&root].id;
                            self.ui.send(ToUi::Present { id, damage });
                        }
                    }
                }
            }
            // A commit with no new buffer: state-only, or an ack. Damage
            // without a buffer means nothing.
            None => {}
        }

        if icon_dirty {
            self.take_client_icon(surface);
        }

        // **The scene can move under a stationary pointer, and until §40
        // nothing re-asked who was under it.**
        //
        // Every hit test before this ran on a pointer *event* — the pointer
        // moved, so the compositor worked out what it had moved onto. But the
        // other half of the same question is a commit: a `wl_subsurface` that
        // is repositioned, raised with `place_above`, given a new input region
        // or unmapped changes what is under a pointer that has not moved at
        // all. The client is entitled to a `leave` and an `enter` for that, and
        // it was getting neither — the last motion event stayed true forever.
        //
        // WLCS asks this directly and by name:
        // `subsurface_moves_under_input_device_once`, `..._twice`,
        // `subsurface_moves_out_from_under_input_device`,
        // `subsurface_extends_parent_input_region`, `place_above_simple`.
        //
        // Re-delivered at the *same* coordinates rather than as a synthesised
        // move, so this cannot invent motion the user did not make: the
        // position is `pointer_at`, which is where the platform last said the
        // pointer was. If the answer is unchanged, smithay coalesces it to a
        // `motion` at the point the client already knows about; if it changed,
        // the enter/leave pair it owes the client comes out of the same call
        // that would have sent it had the pointer moved instead.
        //
        // Gated on the committing tree being the one under the pointer, so an
        // application animating in the background does not re-run a hit test
        // for a window nobody is pointing at.
        if let Some(over) = self.pointer_focus
            && self.by_id(over).map(|w| w.role.surface().clone()) == Some(root_of(surface))
        {
            let (x, y) = self.pointer_at;
            self.pointer_motion(Some(over), x, y, self.now_ms());
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, focused: Option<&WlSurface>) {
        // smithay's own view of the focus, after the `enter`/`leave` pair has
        // been sent. Logged rather than acted on: the decision was made in
        // `set_keyboard_focus` from a Win32 message, and doing anything here
        // would be a second source of truth for the same fact.
        println!(
            "wc-compositor: keyboard focus -> {}",
            if focused.is_some() { "a surface" } else { "nothing" }
        );
    }

    /// What the pointer should look like.
    ///
    /// IMPLEMENTED IN STAGE 12 (docs/waycompose-design.md §23); before it, this logged
    /// once and the user saw the Win32 arrow over everything — no I-beam over
    /// text, no hand over a link.
    ///
    /// # One callback, three sources
    ///
    /// smithay funnels everything here, which is why this is one function and
    /// not three:
    ///
    /// * `wp_cursor_shape_device_v1.set_shape` arrives as `Named`. This is the
    ///   modern half and the one GTK4 binds.
    /// * `wl_pointer.set_cursor` with a surface arrives as `Surface`, and with
    ///   a null surface as `Hidden`. This is what GTK3 — and therefore Firefox
    ///   — still sends.
    /// * **A pointer leave arrives as `Named(Default)`, with no client having
    ///   asked for anything.** smithay sends it from `PointerHandle::leave`,
    ///   and it is what puts the arrow back when the pointer moves off a text
    ///   field. Without it a compositor would show the last client's I-beam
    ///   over its own desktop.
    ///
    /// # What this deliberately does not do
    ///
    /// It does not draw. The cursor surface's pixels become an `HCURSOR` and
    /// Windows composites it; the surface itself must never reach the scene
    /// graph, or the user sees two cursors — the client's, blitted into the
    /// window a frame late and clipped to it, under the system's own. That was
    /// a live bug when this was written.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        // The surface is *recorded*, not drawn — stage 13's half of this, and
        // it is unchanged. `commit` uses it to recognise a cursor's pixels and
        // drop them before they can reach a window.
        //
        // What stage 12 adds is the other half: the same pixels also become an
        // `HCURSOR`, so that dropping them costs nothing a user can see. The
        // two changes are the same sentence read in both directions — a cursor
        // surface is never composited *because* Windows is drawing it.
        self.cursor_surface = match &image {
            CursorImageStatus::Surface(surface) => Some(surface.clone()),
            CursorImageStatus::Hidden | CursorImageStatus::Named(_) => None,
        };

        match image {
            CursorImageStatus::Hidden => {
                self.ui.send(ToUi::Cursor { spec: CursorSpec::Hidden });
            }
            CursorImageStatus::Named(icon) => {
                self.ui.send(ToUi::Cursor { spec: CursorSpec::Named(icon) });
            }
            CursorImageStatus::Surface(surface) => {
                // If this surface has already committed its buffer — the
                // attach-then-set_cursor order — the pixels are in the cache
                // and the cursor can change now. If it has not, the commit
                // that follows will do it, and until then the previous cursor
                // stays up rather than blinking to an arrow and back.
                self.send_cursor_surface(&surface);
            }
        }
    }
}

impl TabletSeatHandler for State {
    // The default `tablet_tool_image` is kept. See the import: this is a bound
    // on `wp_cursor_shape_v1`'s dispatch, not a feature — there is no
    // `zwp_tablet_manager_v2` global, so nothing can reach it.
}

/// Read the title and app id a client has set, if any.
fn metadata(toplevel: &ToplevelSurface) -> (String, String) {
    compositor::with_states(toplevel.wl_surface(), |states| {
        let Some(data) = states.data_map.get::<XdgToplevelSurfaceData>() else {
            return (String::new(), String::new());
        };
        let attrs = data.lock().unwrap();
        (
            attrs.title.clone().unwrap_or_default(),
            attrs.app_id.clone().unwrap_or_default(),
        )
    })
}

/// Which connection a surface belongs to, counted from 1 in accept order.
///
/// ADDED FOR STAGE 32. [`ClientState::index`] already exists and is already
/// counted for every client; this is the one place outside the control socket
/// that needs it, because a window whose client never named itself still has to
/// be told apart from a *different* client that never named itself
/// (`crate::taskbar`).
///
/// `0` for a surface whose client is gone, which cannot happen on the paths
/// that call this — the surface is live enough to have a role — and is a
/// deliberate non-panic if it ever does: a wrong taskbar group is a cosmetic
/// defect and killing the compositor over one is not.
fn client_index(surface: &WlSurface) -> u32 {
    use smithay::reexports::wayland_server::Resource as _;
    surface
        .client()
        .and_then(|c| c.get_data::<ClientState>().map(|d| d.index))
        .unwrap_or(0)
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_id;
        self.next_id += 1;
        self.toplevels += 1;

        let (title, app_id) = metadata(&surface);
        let client = client_index(surface.wl_surface());
        let shadow = Arc::new(Mutex::new(Shadow::empty()));
        println!(
            "wc-compositor: xdg_toplevel created (#{}) -> window {id}",
            self.toplevels
        );

        self.ui.send(ToUi::Create {
            id,
            title: title.clone(),
            app_id: app_id.clone(),
            // ADDED FOR STAGE 32. Stamped at creation, while the window is
            // still hidden, because that is the only moment that is certainly
            // before the shell has made a button for it. Most clients have not
            // called `set_app_id` yet at this point and get their connection's
            // identity here and their real one at map; a client that set it
            // before the first round trip — `weston-terminal` does — gets the
            // real one immediately.
            aumid: crate::taskbar::identity(&app_id, client),
            shadow: shadow.clone(),
            kind: WindowKind::Toplevel,
        });

        // The initial configure carries no size: the client picks, which is
        // right for a window that has not been placed yet and is what lets
        // `weston-simple-shm` choose its own 250x250. The size comes back at
        // the first commit, and the window frame is built from there.
        surface.send_configure();
        println!("wc-compositor: sent xdg_toplevel configure");

        self.windows.insert(
            surface.wl_surface().clone(),
            Window {
                id,
                role: Role::Toplevel(surface),
                shadow,
                mapped: false,
                minimized: false,
                title,
                app_id,
                client,
                frame_callbacks: Vec::new(),
                presented: 0,
                icon_from_client: false,
                icon_dirty: false,
                // Zero until the first commit, which is the only honest value:
                // a window with no buffer has no geometry rectangle either, and
                // no input can reach it before it has mapped.
                content_offset: (0, 0),
            },
        );
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let (title, _) = metadata(&surface);
        let Some(window) = self.windows.get_mut(surface.wl_surface()) else {
            return;
        };
        window.title = title.clone();
        let (id, mapped) = (window.id, window.mapped);
        if mapped {
            self.ui.send(ToUi::Title { id, title });
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let (_, app_id) = metadata(&surface);
        let Some(window) = self.windows.get_mut(surface.wl_surface()) else {
            return;
        };
        window.app_id = app_id.clone();
        let (id, mapped, own) = (window.id, window.mapped, window.icon_from_client);
        let client = window.client;
        if mapped {
            self.ui.send(ToUi::AppId {
                id,
                app_id: app_id.clone(),
                // ADDED FOR STAGE 32, and this is the *late* case: the window
                // is already on screen, so the shell has already made a button
                // for it under the old identity. What that costs is measured in
                // docs/waycompose-design.md §32.6; the platform is told, and
                // the platform decides what it can do about it.
                aumid: crate::taskbar::identity(&app_id, client),
            });
            // A new app id is a new answer to "what is this application", so
            // the icon is looked up again — unless the client has said what its
            // icon is itself, in which case the name is not evidence about it.
            if !own {
                self.apply_stored_icon(id, &app_id);
            }
        }
    }

    // ADDED FOR STAGE 17. The three requests a client-decorated toplevel makes
    // when the user grabs its own title bar, all of which had smithay's empty
    // default until now — so a window drawn by Firefox could be closed and
    // maximised but could not be *moved*, and there was no other way to move it
    // (see [`ToUi::BeginMove`]).
    //
    // The seat and the serial are ignored in all three, and that is a decision
    // rather than an omission. The protocol wants them checked against a recent
    // input event so that a client cannot start a drag out of nothing; on this
    // compositor there is one seat, the events came from Win32 in the first
    // place, and Windows' own modal loop refuses to start unless a button is
    // actually down — so the check the serial exists to make is made by the
    // platform, one layer down, against the real hardware state.

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            println!("wc-compositor: window {} asked to be moved", window.id);
            self.ui.send(ToUi::BeginMove { id: window.id });
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        edges: ResizeEdge,
    ) {
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            let edge = match edges {
                ResizeEdge::Top => Edge::Top,
                ResizeEdge::Bottom => Edge::Bottom,
                ResizeEdge::Left => Edge::Left,
                ResizeEdge::TopLeft => Edge::TopLeft,
                ResizeEdge::BottomLeft => Edge::BottomLeft,
                ResizeEdge::Right => Edge::Right,
                ResizeEdge::TopRight => Edge::TopRight,
                ResizeEdge::BottomRight => Edge::BottomRight,
                // `None`, and anything a newer protocol version adds. The enum
                // is `#[non_exhaustive]`, and treating an unknown edge as "no
                // edge" starts a keyboard-driven resize rather than refusing.
                _ => Edge::None,
            };
            println!("wc-compositor: window {} asked to be resized from {edge:?}", window.id);
            self.ui.send(ToUi::BeginResize {
                id: window.id,
                edge,
            });
        }
    }

    fn show_window_menu(
        &mut self,
        surface: ToplevelSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
        location: Point<i32, Logical>,
    ) {
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            println!(
                "wc-compositor: window {} asked for the window menu at {},{}",
                window.id, location.x, location.y
            );
            self.ui.send(ToUi::WindowMenu {
                id: window.id,
                x: location.x,
                y: location.y,
            });
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            self.ui.send(ToUi::SetWindowState {
                id: window.id,
                maximized: Some(true),
                minimized: None,
            });
        }
        // xdg-shell requires a configure in response to `set_maximized` even if
        // the compositor declines. The real state arrives a moment later from
        // `WM_SIZE`; this meets the protocol obligation on time rather than
        // making it conditional on a round trip through Win32.
        surface.send_configure();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            self.ui.send(ToUi::SetWindowState {
                id: window.id,
                maximized: Some(false),
                minimized: None,
            });
        }
        surface.send_configure();
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            self.ui.send(ToUi::SetWindowState {
                id: window.id,
                maximized: None,
                minimized: Some(true),
            });
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Drop the focus **before** the window goes, and unconditionally.
        // smithay holds the focused `WlSurface` in the keyboard handle, and a
        // handle still pointing at a destroyed surface sends `leave` to a dead
        // object on the next focus change. Doing it here rather than waiting
        // for a `WM_KILLFOCUS` matters because a client that destroys its
        // surface without the window being defocused first — which is what
        // happens when a client exits — never produces one.
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            let id = window.id;
            if self.keyboard_focus == Some(id) {
                self.set_keyboard_focus(id, false);
            }
            if self.pointer_focus == Some(id) {
                self.pointer_motion(None, 0.0, 0.0, self.now_ms());
            }
        }
        if let Some(window) = self.windows.remove(surface.wl_surface()) {
            println!(
                "wc-compositor: xdg_toplevel destroyed -> window {} ({} frames presented)",
                window.id, window.presented
            );
            self.ui.send(ToUi::Destroy { id: window.id });
        }
    }

    /// REWRITTEN FOR STAGE 9. Stage 4 sent a configure and drew nothing, and
    /// said so: *"a popup rendered as its own top-level `HWND` would be wrong
    /// in a way that is hard to undo."* It is its own `HWND`, and it is not a
    /// top-level one — `WS_POPUP`, owned by its parent, no caption, no taskbar
    /// button and no activation. That is the same shape Win32 menus have, and
    /// on this side of the boundary it is the only shape that can leave the
    /// parent window's bounds, which a menu near the bottom of a window must.
    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let id = self.next_id;
        self.next_id += 1;
        self.popups += 1;

        // Where the popup goes, in the parent's window-geometry coordinates.
        // `get_geometry` is the positioner's own arithmetic — anchor rectangle,
        // anchor edge, gravity and offset — which smithay implements from the
        // XML. What is deliberately **not** used here is
        // `get_unconstrained_geometry`: constraining a menu to the monitor is
        // the compositor's job, and on Windows the compositor that owns the
        // monitor is Windows. Letting `SetWindowPos` place it and reporting no
        // constraint back is honest; recomputing it here would be a second,
        // disagreeing, opinion. See docs/waycompose-design.md §13.6.
        let rect = positioner.get_geometry();
        let parent = surface.get_parent_surface().map(|s| root_of(&s));
        let parent_id = parent.as_ref().and_then(|p| self.windows.get(p)).map(|w| w.id);

        println!(
            "wc-compositor: xdg_popup created (#{}) -> window {id} at {},{} {}x{} parent {:?}",
            self.popups, rect.loc.x, rect.loc.y, rect.size.w, rect.size.h, parent_id
        );

        let Some(parent_id) = parent_id else {
            // A popup whose parent is not a window we know about. Legal to
            // refuse, and the client is told rather than left waiting.
            eprintln!("wc-compositor: xdg_popup has no mapped parent; dismissing it");
            surface.send_popup_done();
            return;
        };

        let shadow = Arc::new(Mutex::new(Shadow::empty()));
        self.ui.send(ToUi::Create {
            id,
            title: String::new(),
            app_id: String::new(),
            // A popup is `WS_EX_TOOLWINDOW` with an owner, so the shell gives
            // it no taskbar button at all and there is nothing for an identity
            // to group. Empty rather than invented: a string here would be a
            // claim about a button that does not exist.
            aumid: String::new(),
            shadow: shadow.clone(),
            kind: WindowKind::Popup {
                parent: parent_id,
                x: rect.loc.x,
                y: rect.loc.y,
            },
        });

        // The popup's configure carries the positioner's rectangle, which is
        // the protocol's way round: the client asked for a size and the
        // compositor either agrees or moves it. We agree.
        surface.with_pending_state(|state| {
            state.geometry = rect;
        });
        let _ = surface.send_configure();

        let client = client_index(surface.wl_surface());
        self.windows.insert(
            surface.wl_surface().clone(),
            Window {
                id,
                role: Role::Popup(surface),
                shadow,
                mapped: false,
                minimized: false,
                title: String::new(),
                app_id: String::new(),
                client,
                frame_callbacks: Vec::new(),
                presented: 0,
                // A popup has no taskbar button and no caption, so neither of
                // these can ever be true of one. They are here because `Window`
                // is one type for both roles, which is stage 9's decision.
                icon_from_client: false,
                icon_dirty: false,
                // A popup has an inset of its own — GTK gives its menus the
                // same shadow margin it gives its windows — and it is found the
                // same way, at its first commit. The *position* above is
                // already in the parent's window-geometry coordinates, which is
                // the parent's `content_offset` applied by the client rather
                // than by us; this is the popup's own.
                content_offset: (0, 0),
            },
        );
    }

    /// The client destroyed an `xdg_popup`.
    ///
    /// ADDED FOR STAGE 13, and its absence made the desktop unusable.
    /// **`XdgShellHandler::popup_destroyed` has an empty default body**, so
    /// from stage 9 to here every popup Firefox opened left its `HWND` on the
    /// screen forever. A live session accumulated three stale "New Tab"
    /// tooltips at three positions, a context menu, an account panel and the
    /// hamburger menu, all visible at once, over the page being read.
    ///
    /// Nothing errored and nothing was logged. The windows were real, mapped
    /// and correctly painted; they were simply never taken away, and the frame
    /// dumps looked healthy throughout. It is the clearest example this project
    /// has produced of a defect that only a person looking at a screen can
    /// find — and it caused two further symptoms that would have been chased
    /// separately (§18.4).
    ///
    /// The body is [`Self::toplevel_destroyed`]'s and that is the point: the
    /// two roles differ in how a window is *made* and not at all in how it is
    /// unmade. A popup can hold the pointer focus — it is the window under the
    /// cursor while a menu is open — and a pointer handle still pointing at a
    /// destroyed surface sends `leave` to a dead object.
    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(window) = self.windows.get(surface.wl_surface()) {
            let id = window.id;
            if self.keyboard_focus == Some(id) {
                self.set_keyboard_focus(id, false);
            }
            if self.pointer_focus == Some(id) {
                self.pointer_motion(None, 0.0, 0.0, self.now_ms());
            }
            if self.popup_grab == Some(id) {
                self.popup_grab = None;
            }
        }
        if let Some(window) = self.windows.remove(surface.wl_surface()) {
            self.staging.remove(surface.wl_surface());
            println!(
                "wc-compositor: xdg_popup destroyed -> window {} ({} frames presented)",
                window.id, window.presented
            );
            self.ui.send(ToUi::Destroy { id: window.id });
        }
    }

    /// `xdg_popup.grab`: the client wants this popup to own the pointer until
    /// something outside it is clicked.
    ///
    /// IMPLEMENTED IN STAGE 13, and it is the other half of the stale-menu bug.
    /// A menu is dismissed by *the compositor* — the client cannot see a click
    /// that lands outside its own surfaces, so if nobody sends `popup_done` the
    /// menu has no way to know it should go, and clicking elsewhere leaves it
    /// on screen.
    ///
    /// What is recorded is the innermost grabbing popup only, not a stack. A
    /// stack is what a submenu needs and it is more than this stage can prove:
    /// dismissing the innermost popup makes the client destroy it, which
    /// arrives at [`Self::popup_destroyed`] and clears the record, so a second
    /// click dismisses the next one out. That is one click per level rather
    /// than one click for all of them — a real difference from how a Linux
    /// compositor behaves, written down rather than hidden.
    fn grab(
        &mut self,
        surface: PopupSurface,
        _seat: wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
        if let Some(id) = self.windows.get(surface.wl_surface()).map(|w| w.id) {
            println!("wc-compositor: xdg_popup grab -> window {id} owns the pointer");
            self.popup_grab = Some(id);
        }
    }

    /// `xdg_popup.reposition` (xdg-shell v3): move a popup that is already up.
    ///
    /// IMPLEMENTED IN STAGE 13. The empty body meant `xdg_popup.repositioned`
    /// was never sent and the popup never moved, so a submenu that GTK
    /// repositions because it would fall off the screen stayed where it was
    /// first placed — off-screen, or on top of its parent.
    ///
    /// The protocol's order is fixed and all three parts are required:
    /// `repositioned` with the client's token, then a `configure` with the new
    /// rectangle, then the parent surface's `commit` applies it. A compositor
    /// that sends only the configure leaves the client unable to match the
    /// answer to its request, which for GTK means it ignores the move.
    ///
    /// The positioner's own arithmetic is used unchanged, exactly as
    /// [`Self::new_popup`] does — anchor rectangle, anchor edge, gravity and
    /// offset, which smithay implements from the XML. The constraint
    /// adjustments are still not applied here (§18.6): this compositor does not
    /// know where the parent window is on the Windows desktop, so it cannot
    /// tell whether the result would be off-screen. Windows keeps an owned
    /// `WS_POPUP` on the monitor by itself, which is why that has not bitten.
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let rect = positioner.get_geometry();
        let Some(&Window { id, .. }) = self.windows.get(surface.wl_surface()) else {
            // A reposition for a popup we never mapped. Answering the token
            // anyway is better than silence: the client is waiting for it.
            surface.send_repositioned(token);
            return;
        };
        println!(
            "wc-compositor: xdg_popup reposition -> window {id} to {},{} {}x{} (token {token})",
            rect.loc.x, rect.loc.y, rect.size.w, rect.size.h
        );
        surface.with_pending_state(|state| {
            state.geometry = rect;
        });
        surface.send_repositioned(token);
        let _ = surface.send_configure();
        self.ui.send(ToUi::MovePopup {
            id,
            x: rect.loc.x,
            y: rect.loc.y,
            width: rect.size.w,
            height: rect.size.h,
        });
    }
}

// ---------------------------------------------------------------------------
// Icons: the client's own, and somebody else's
// ---------------------------------------------------------------------------

impl smithay::wayland::xdg_toplevel_icon::XdgToplevelIconHandler for State {
    /// The icon is *pending* here, not current: `set_icon` is double-buffered
    /// and takes effect at the surface's next commit, which is where
    /// [`State::take_client_icon`] reads it. All this does is remember that
    /// there is something to read.
    fn set_icon(
        &mut self,
        _toplevel: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel,
        wl_surface: WlSurface,
    ) {
        if let Some(window) = self.windows.get_mut(&wl_surface) {
            println!(
                "wc-compositor: window {} set an icon through xdg_toplevel_icon_v1",
                window.id
            );
            window.icon_dirty = true;
        }
    }
}

smithay::delegate_xdg_toplevel_icon!(State);

use crate::icon::proto::wc_icon_manager_v1::{self, WcIconManagerV1};
use smithay::reexports::wayland_server::{DataInit, Dispatch, GlobalDispatch, New, Resource};

/// Per-publisher staging. One publisher builds one icon set at a time, and two
/// of them cannot get in each other's way because the state is on the resource
/// rather than on the compositor.
#[derive(Debug, Default)]
pub struct PublisherState(std::sync::Mutex<crate::icon::Staging>);

impl GlobalDispatch<WcIconManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WcIconManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        // Worth saying out loud once per publisher: this global is how a
        // machine that is not this one decides what a window looks like, and a
        // run where nobody binds it is a run where every icon will be generic.
        println!("wc-compositor: a client bound wc_icon_manager_v1 (an icon resolver)");
        data_init.init(resource, PublisherState::default());
    }
}

impl Dispatch<WcIconManagerV1, PublisherState> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &WcIconManagerV1,
        request: wc_icon_manager_v1::Request,
        data: &PublisherState,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use crate::icon::StageError;
        use wc_icon_manager_v1::{Error, Request};

        // Every arm that can fail ends the *publisher's* connection rather than
        // the window's client's, which is the one real advantage of the icon
        // feed being a separate client: a resolver that miscounts its bytes
        // costs an icon, not a window.
        let fail = |e: StageError| match e {
            StageError::TooBig => (Error::TooBig, "more staged bytes than the compositor holds"),
            StageError::Short => (Error::ShortImage, "image named more pixels than were staged"),
            StageError::BadSize => (Error::BadSize, "implausible icon size"),
        };

        match request {
            Request::Chunk { data: bytes } => {
                if let Err(e) = data.0.lock().unwrap().chunk(&bytes) {
                    let (code, why) = fail(e);
                    resource.post_error(code, why);
                }
            }
            Request::Image { width, height } => {
                if let Err(e) = data.0.lock().unwrap().image(width, height) {
                    let (code, why) = fail(e);
                    resource.post_error(code, why);
                }
            }
            Request::Publish { app_id } => {
                let images = data.0.lock().unwrap().take();
                state.publish_icons(app_id, images);
            }
            Request::Forget { app_id } => {
                println!("wc-compositor: icon for {app_id:?} forgotten");
                state.icons.forget(&app_id);
            }
            Request::Destroy => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The clipboard: the side door for the bytes
// ---------------------------------------------------------------------------

use crate::clipboard::proto::wc_clipboard_manager_v1::{self, WcClipboardManagerV1};

/// Per-publisher staging, on the resource rather than on the compositor, so two
/// proxies building selections at once cannot interleave into each other.
#[derive(Debug, Default)]
pub struct ClipboardState(std::sync::Mutex<crate::clipboard::Staging>);

impl GlobalDispatch<WcClipboardManagerV1, ()> for State {
    fn bind(
        _state: &mut State,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<WcClipboardManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, State>,
    ) {
        println!(
            "wc-compositor: a client bound wc_clipboard_manager_v1 \
             (the clipboard's byte path)"
        );
        data_init.init(resource, ClipboardState::default());
    }
}

impl Dispatch<WcClipboardManagerV1, ClipboardState> for State {
    fn request(
        state: &mut State,
        _client: &Client,
        resource: &WcClipboardManagerV1,
        request: wc_clipboard_manager_v1::Request,
        data: &ClipboardState,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        use crate::clipboard::StageError;
        use wc_clipboard_manager_v1::{Error, Request};

        // Every failure here ends the *publisher's* connection, which in this
        // topology is wc-proxy's third socket and not the application's. A
        // proxy that miscounts its bytes costs a paste, not a window — the same
        // property the icon feed has and for the same reason.
        let fail = |e: StageError| match e {
            StageError::TooBig => (Error::TooBig, "more staged bytes than the compositor holds"),
            StageError::TooMany => (Error::TooMany, "more mime types than a clipboard holds"),
        };

        match request {
            Request::Chunk { data: bytes } => {
                if let Err(e) = data.0.lock().unwrap().chunk(&bytes) {
                    let (code, why) = fail(e);
                    resource.post_error(code, why);
                }
            }
            Request::Data { mime_type } => {
                if let Err(e) = data.0.lock().unwrap().data(mime_type) {
                    let (code, why) = fail(e);
                    resource.post_error(code, why);
                }
            }
            Request::Publish => {
                let payloads = data.0.lock().unwrap().take();
                let system = crate::clipboard::to_system(&payloads);
                state.clip_published += 1;
                println!(
                    "wc-compositor: a client published a selection: {} mime type(s) -> {} \
                     system format(s) [{}]",
                    payloads.len(),
                    system.len(),
                    system
                        .iter()
                        .map(|(f, b)| format!("{} {} bytes", f.name(), b.len()))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                // The *payloads*, not the converted formats, cross to the UI
                // thread: the conversion belongs to `crate::clipboard` and the
                // headless platform has no `CF_UNICODETEXT` to convert to.
                // `win32.rs` calls `to_system` itself.
                state.ui.send(ToUi::SetClipboard { payloads });
            }
            Request::Fetch { token, source, mime_type } => {
                let internal = state.next_clip_token;
                state.next_clip_token += 1;
                state.clip_fetches.insert(internal, (resource.clone(), token));
                state.ui.send(ToUi::FetchClipboard {
                    token: internal,
                    // The `source` enum's `drag` entry. Compared as a number
                    // rather than through the generated type because the arg
                    // is a plain `uint` in the XML: an `enum=` attribute would
                    // make the generated signature a `WEnum`, which buys
                    // nothing for a two-value flag and costs a match arm for a
                    // value that cannot occur.
                    from_drag: source == 1,
                    mime: mime_type,
                });
            }
            // Drag and drop, from a Wayland client outward. §29 stages the
            // payload through exactly the same `chunk`/`data` path a publish
            // uses (see the XML) and hands it to the platform; a platform with
            // no OLE drag source answers `none` at once.
            //
            // **Answered unconditionally, on every path.** A `start_drag` that
            // produced no `drag_finished` would leave the client's
            // `wl_data_source` waiting forever, which is the half-implemented
            // drag this stage exists not to build.
            Request::StartDrag { actions } => {
                let payloads = data.0.lock().unwrap().take();
                state.clip_drags += 1;
                println!(
                    "wc-compositor: a client started a drag with {} mime type(s), \
                     actions {actions:#x}",
                    payloads.len()
                );
                state.clip_drag_source = Some(resource.clone());
                state.ui.send(ToUi::StartDrag { payloads, actions });
            }
            Request::CancelDrag => {
                state.ui.send(ToUi::CancelDrag);
            }
            Request::Destroy => {
                // Fetches this resource was waiting on are abandoned rather
                // than answered into a dead object, which the XML says.
                state.clip_fetches.retain(|_, (r, _)| r != resource);
            }
        }
    }
}

// ADDED FOR STAGE 9 -----------------------------------------------------------
//
// `OutputHandler` has no required methods: smithay's output global does all its
// own bookkeeping and the trait exists so a compositor can be told when a
// client binds one. Nothing here needs to know.
impl OutputHandler for State {}

/// The clipboard, from `wl_data_device`'s side.
///
/// # What §29 changed, and what it deliberately did not
///
/// This used to be smithay's defaults with a paragraph saying a Windows
/// clipboard could not be built on them, because `send_selection` hands the
/// compositor **a file descriptor to write into** and on the guest an `OwnedFd`
/// is a `wc_handle::Handle` — an object docs/waycompose-design.md §12.13 item 7 says has
/// no route to a client across the hypervisor. That paragraph was right about
/// the descriptor and wrong about the feature.
///
/// The way out is that the descriptor never has to exist here. `wc-proxy` runs
/// on the client's own machine, where the client's pipe is a real Linux pipe;
/// it terminates the transfer there and carries the **bytes** over
/// `wc_clipboard_v1`. So this compositor's part of the clipboard is: announce
/// what Windows holds (`on_clipboard_changed`, over ordinary `wl_data_device`),
/// and answer for the bytes (the `Dispatch` above). Neither needs an fd.
///
/// `send_selection` is therefore **unreachable in this topology** and is still
/// implemented, because "unreachable" is a claim about the proxy and this trait
/// is reachable by anything that connects directly. It refuses by closing the
/// descriptor, which the client reads as an empty selection — a failed paste,
/// which is recoverable, rather than a protocol error, which is not.
impl SelectionHandler for State {
    type SelectionUserData = ();

    fn send_selection(
        &mut self,
        _ty: smithay::wayland::selection::SelectionTarget,
        mime_type: String,
        fd: smithay::utils::os::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        // Dropped, not written. On Windows this "fd" is a `wc_handle::Handle`
        // the client named, and there is no writer for one here; on Linux it is
        // a pipe from a client that bypassed the proxy. Both get an empty
        // paste and a line saying why.
        drop(fd);
        println!(
            "wc-compositor: a client asked for the selection as {mime_type:?} through \
             wl_data_offer.receive. That request carries a file descriptor and this \
             compositor has no way to write into one across the boundary; the bytes go \
             through wc_clipboard_v1 instead, which wc-proxy binds. Closing the \
             descriptor: the paste comes back empty rather than killing the client."
        );
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self._data_device
    }
}

// Drag and drop, both directions, at their defaults: the events are accepted
// and nothing is done with them. A drag *within* one client works because
// smithay runs the grab; a drag onto the Windows desktop is `IDropTarget` and
// is not started here.
//
// §29 left this alone on purpose. Drag and drop is the same interface
// family as the clipboard and it is **not implemented**, which is a decision
// and not an oversight: a half-built DnD is a drag that starts, offers a
// format, and then hangs on the drop, and that failure — a client waiting
// forever on a transfer nobody will finish — is precisely what this stage
// exists to remove. wc-proxy answers `wl_data_device.start_drag` with an
// immediate `wl_data_source.cancelled`, so a client that tries gets a drag that
// does not begin instead of one that does not end. docs/waycompose-design.md §29.8.
impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}

/// `xdg-decoration`, which is the standard one and — measured — the one Firefox
/// does not use.
///
/// Implemented anyway, and answered with whatever [`Decorations`] this run was
/// started with, so that the two protocols cannot disagree. Note that
/// `xdg-decoration` gives the compositor the last word — the client *requests*
/// and the compositor *configures* — so this handler is entitled to overrule,
/// and does. GTK 3 has no code for this protocol at all (§15.7), so nothing
/// this project runs is affected either way.
impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        let mode = self.decorations.xdg();
        // STAGE 33. This client has been told; whatever it draws afterwards it
        // is not drawing in ignorance. See [`crate::surface::decorator`].
        self.negotiated_decoration.insert(toplevel.wl_surface().clone());
        println!("wc-compositor: xdg-decoration: client created a decoration object, telling it {mode:?}");
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_pending_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, requested: DecorationMode) {
        let mode = self.decorations.xdg();
        println!("wc-compositor: xdg-decoration: client asked for {requested:?}, answering {mode:?}");
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_pending_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        let mode = self.decorations.xdg();
        println!("wc-compositor: xdg-decoration: client unset its mode, answering {mode:?}");
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_pending_configure();
    }
}

/// KDE's `org_kde_kwin_server_decoration`, which is the one GTK actually binds.
///
/// smithay's default `request_mode` echoes the client's request straight back,
/// which is the right behaviour and is also silent. This one says what
/// happened, because "which protocol did Firefox use and what did it ask for"
/// is the question the stage exists to answer.
impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration
    }

    fn new_decoration(
        &mut self,
        surface: &WlSurface,
        _decoration: &OrgKdeKwinServerDecoration,
    ) {
        // STAGE 33, and this is the protocol GTK uses, so it is the one that
        // fires for Firefox. `weston-terminal` never reaches this function at
        // all, which is the measurement §33 is about.
        self.negotiated_decoration.insert(surface.clone());
        println!("wc-compositor: kde-decoration: client created a decoration object");
    }

    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &OrgKdeKwinServerDecoration,
        mode: WEnum<KdeMode>,
    ) {
        println!(
            "wc-compositor: kde-decoration: client asked for {mode:?} (default is {:?})",
            self.decorations
        );
        // **Echoed, deliberately, even when it contradicts the default**, and
        // that is this stage's answer to "what happens for a client that
        // insists on Client".
        //
        // Three reasons, in order of weight. First, the per-surface answer is
        // not the lever: GTK 3 decides whether to draw a title bar from the
        // *manager's* `default_mode` and re-reads nothing afterwards, so
        // refusing here changes no client's behaviour. Second, GDK's handler
        // re-requests the mode it wants when it is contradicted, which was
        // captured as a loop against real KWin — KWin eventually capitulates,
        // and a compositor that does not is a compositor that spins. Third, a
        // client that has decided to draw its own title bar and is told it may
        // not still draws one; the only thing refusing buys is a client that
        // draws none and cannot be moved.
        //
        // The cost is honest and small: under `--decorations server` a client
        // that insists on `Client` gets two title bars, exactly as every client
        // did before this stage. Its shadow inset is still reported as window
        // geometry and `compose_tree` still honours it, so the *window* is
        // right even when the picture inside it has a bar too many.
        if let WEnum::Value(mode) = mode {
            decoration.mode(mode);
        }
    }
}

// ---------------------------------------------------------------------------
// STAGE 21: three protocols that are a Win32 call each
// ---------------------------------------------------------------------------

/// `zwp_idle_inhibit_manager_v1` — the screen must not blank during a video.
///
/// Both methods do bookkeeping only. The decision, and the one message that
/// crosses to the UI thread, are in [`State::tick_idle_inhibit`], for the reason
/// documented there: the interesting case is the one where `uninhibit` is never
/// called at all.
impl IdleInhibitHandler for State {
    fn inhibit(&mut self, surface: WlSurface) {
        let n = self.idle_inhibitors.entry(surface).or_insert(0);
        *n += 1;
        println!(
            "wc-compositor: a client created an idle inhibitor ({} on this surface)",
            *n
        );
        self.tick_idle_inhibit();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some(n) = self.idle_inhibitors.get_mut(&surface) {
            *n -= 1;
            if *n == 0 {
                self.idle_inhibitors.remove(&surface);
            }
        }
        println!("wc-compositor: a client destroyed an idle inhibitor");
        self.tick_idle_inhibit();
    }
}

delegate_idle_inhibit!(State);

/// How stale an activation token may be before it is refused.
///
/// **Thirty seconds, not the ten in smithay's own example.** The token is minted
/// by the application the user clicked in and spent by the application that is
/// then *started*, and a cold Firefox on this guest takes longer than ten
/// seconds to reach the point where it has a surface to activate. A limit that
/// expires during the ordinary case is not a security measure, it is a feature
/// that works only on a warm cache.
const ACTIVATION_TOKEN_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30);

/// `xdg_activation_v1` — clicking a link in one application raises another.
impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation
    }

    /// Minting is where the pool is swept, so it is bounded by the rate at
    /// which tokens are asked for rather than by anything running on a timer.
    /// GTK requests a token for every launch it contemplates and spends only
    /// some of them; without this, a long session accumulates every one it did
    /// not use.
    fn token_created(&mut self, _token: XdgActivationToken, _data: XdgActivationTokenData) -> bool {
        self.xdg_activation
            .retain_tokens(|_, d| d.timestamp.elapsed() < ACTIVATION_TOKEN_MAX_AGE);
        true
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        let age = data.timestamp.elapsed();
        // Spent, whatever happens next: `xdg_activation_v1` says a token is
        // good for one activation, and leaving a used one in the pool would let
        // a client raise itself repeatedly off a single click the user made
        // somewhere else.
        self.xdg_activation.remove_token(&token);

        if age >= ACTIVATION_TOKEN_MAX_AGE {
            println!(
                "wc-compositor: xdg_activation: ignoring a token {} s old",
                age.as_secs()
            );
            return;
        }
        let Some(window) = self.windows.get(&root_of(&surface)) else {
            // A surface with no window: either it has not mapped yet, or it is
            // not a shell surface at all. Nothing to raise either way.
            println!("wc-compositor: xdg_activation: no window for the surface named; ignoring");
            return;
        };
        println!(
            "wc-compositor: xdg_activation: window {} asked to be raised (token from {:?}, \
             {} ms old)",
            window.id,
            data.app_id.as_deref().unwrap_or("an unnamed app"),
            age.as_millis()
        );
        // Whether it *is* raised is Windows' decision and is not this side's
        // business; `platform::win32::activate_window` documents when the
        // answer is no, and the protocol gives us no way to tell the client
        // either way.
        self.ui.send(ToUi::Activate { id: window.id });
    }
}

delegate_xdg_activation!(State);

/// `zwp_keyboard_shortcuts_inhibit_v1` — and this one is complete because there
/// is nothing to inhibit.
///
/// **Granting every request is the accurate description of this compositor, not
/// a shortcut taken.** The inhibitor's meaning is "stop applying your own
/// keyboard shortcuts and give the client every key". [`State::key`] already
/// forwards every key to the focused client with `FilterResult::Forward` and no
/// compositor-level binding exists to suppress; the comment there explains why,
/// and it is the same reason: the window manager is Windows, which has taken
/// the combinations it reserves before the message loop ever sees them.
///
/// So the inhibitor is activated the moment it is created, and the client that
/// binds this protocol gets exactly what it asked for. The two ways this could
/// become a lie are worth naming, because both are things a later stage might
/// do without thinking of this file: adding a compositor-level key binding, and
/// intercepting a key in `win32::toplevel_proc` before it becomes a
/// `FromUi::Key`. Either one has to consult
/// `Seat::keyboard_shortcuts_inhibited` — smithay keeps the answer, this
/// compositor simply has no question to ask it yet.
///
/// What this cannot do, and what no compositor on Windows can, is hand over the
/// combinations the *system* takes first: Ctrl-Alt-Del is the secure attention
/// sequence and Win-L is handled by the shell, neither of which reaches a
/// window procedure at all. A Linux compositor grabbing the keyboard has the
/// same hole for the same kind of key, so this is not a Windows tax; it is
/// worth writing down because a client asking to inhibit shortcuts is often
/// asking for exactly those.
impl KeyboardShortcutsInhibitHandler for State {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.shortcuts_inhibit
    }

    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        inhibitor.activate();
        println!(
            "wc-compositor: keyboard shortcuts inhibited for a surface, and granted immediately \
             — this compositor forwards every key to the focused client already"
        );
    }
}

delegate_keyboard_shortcuts_inhibit!(State);

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_data_device!(State);
delegate_xdg_decoration!(State);
delegate_kde_decoration!(State);
// ADDED FOR STAGE 12. `wp_cursor_shape_manager_v1` and its per-pointer device.
delegate_cursor_shape!(State);

// ---------------------------------------------------------------------------
// §37: the window-geometry inset, which is the only thing between a window
// pixel and a surface point
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::surface_origin;

    /// What smithay does with the origin, written out so the test asserts on
    /// the number the *client* sees rather than on the one this file returns.
    ///
    /// `PointerHandle::motion` and `TouchHandle::down` both compute
    /// `event.location - loc` (vendor/smithay/src/input/pointer/mod.rs:795,
    /// input/touch/mod.rs:566), so this is that subtraction and nothing else.
    fn delivered(content_offset: (i32, i32), window_pixel: (f64, f64)) -> (f64, f64) {
        let origin = surface_origin(content_offset);
        (window_pixel.0 - origin.0, window_pixel.1 - origin.1)
    }

    /// The case that hid the bug for twenty-six stages: a client with no inset.
    ///
    /// Every check written before §37 used one of these — `wc-shmclient`,
    /// `weston-simple-shm`, the WLCS suite, Firefox under **server-side**
    /// decorations — and for all of them the conversion is the identity, so a
    /// missing term is invisible.
    #[test]
    fn a_client_with_no_inset_is_the_identity() {
        assert_eq!(delivered((0, 0), (200.0, 150.0)), (200.0, 150.0));
    }

    /// **The measurement, from `scripts/wc-input.sh pointer`.**
    ///
    /// `weston-eventdemo` declares `set_window_geometry(32, 32, 436, 336)` and
    /// was clicked at window pixel `(200, 150)`. Before §37 it printed
    /// `x: 200, y: 150`; the pixel under the cursor is the buffer's
    /// `(232, 182)`, because that is where `compose_tree` blitted it from.
    #[test]
    fn the_weston_toytoolkit_inset_is_thirty_two_each_way() {
        assert_eq!(delivered((32, 32), (200.0, 150.0)), (232.0, 182.0));
    }

    /// Firefox under client-side decorations, whose inset is not square — so a
    /// fix that carried one number for both axes would pass the test above and
    /// fail here.
    #[test]
    fn firefox_under_csd_is_offset_by_twenty_six_and_twenty_three() {
        assert_eq!(delivered((26, 23), (0.0, 0.0)), (26.0, 23.0));
    }

    /// The sign, stated as the thing a user can see.
    ///
    /// `weston-terminal`'s title bar is drawn in the first ~30 px of the
    /// *window*, which is surface rows 32..62. A click 8 px **below** the bar,
    /// at window pixel `(200, 40)`, must not be inside it — before §37 it
    /// arrived as surface `(200, 40)`, which is inside, and the terminal
    /// answered with `xdg_toplevel.move` and warped the pointer to the bar.
    #[test]
    fn a_click_below_the_title_bar_is_below_the_title_bar() {
        let (_, y) = delivered((32, 32), (200.0, 40.0));
        assert_eq!(y, 72.0);
        assert!(y >= 62.0, "a click at window y=40 landed in the title bar at surface y={y}");
    }

    /// An inset can be negative — `set_window_geometry` takes a signed origin,
    /// and a client whose window is *larger* than its buffer is legal. The
    /// conversion is arithmetic rather than a clamp, so it is symmetric.
    #[test]
    fn a_negative_inset_shifts_the_other_way() {
        assert_eq!(delivered((-5, -7), (100.0, 100.0)), (95.0, 93.0));
    }

    // -----------------------------------------------------------------------
    // §40: the input region, which was parsed and ignored
    // -----------------------------------------------------------------------

    mod input_region {
        use crate::state::accepts_input_at;
        use smithay::utils::Rectangle;
        use smithay::wayland::compositor::{RectangleKind, RegionAttributes};

        fn region(rects: &[(RectangleKind, i32, i32, i32, i32)]) -> RegionAttributes {
            RegionAttributes {
                rects: rects
                    .iter()
                    .map(|&(kind, x, y, w, h)| {
                        (kind, Rectangle::new((x, y).into(), (w, h).into()))
                    })
                    .collect(),
            }
        }

        /// The default, and the one that matters most: no region means the
        /// whole surface. `None` is the protocol's initial value and it means
        /// *infinite* — reading it as an empty region would make every
        /// ordinary client, which never calls `set_input_region` at all,
        /// silently unclickable.
        #[test]
        fn an_absent_region_accepts_the_whole_surface() {
            assert!(accepts_input_at((100, 100), None, (0.0, 0.0)));
            assert!(accepts_input_at((100, 100), None, (99.0, 99.0)));
        }

        /// The buffer is a bound even with no region: past the last pixel is
        /// off the surface. Half-open, so `(100, 100)` on a 100x100 surface is
        /// outside — the same convention `Rect::clip` and the blit use.
        #[test]
        fn the_buffer_bounds_the_surface_from_below_and_above() {
            assert!(!accepts_input_at((100, 100), None, (100.0, 50.0)));
            assert!(!accepts_input_at((100, 100), None, (50.0, 100.0)));
            assert!(!accepts_input_at((100, 100), None, (-1.0, 50.0)));
            assert!(!accepts_input_at((100, 100), None, (50.0, -1.0)));
        }

        /// A surface with no buffer takes no input. This is the state a
        /// `wl_subsurface` is in between `get_subsurface` and its first
        /// commit, and hit-testing it would put the pointer on a surface with
        /// nothing drawn on it.
        #[test]
        fn a_surface_with_no_buffer_takes_no_input() {
            assert!(!accepts_input_at((0, 0), None, (0.0, 0.0)));
            let r = region(&[(RectangleKind::Add, 0, 0, 100, 100)]);
            assert!(!accepts_input_at((0, 0), Some(&r), (0.0, 0.0)));
        }

        /// A region smaller than the surface: inside accepts, the gap between
        /// the region and the buffer edge does not. WLCS's `SmallerRegion`.
        #[test]
        fn a_smaller_region_leaves_the_rest_of_the_surface_inert() {
            let r = region(&[(RectangleKind::Add, 20, 20, 60, 60)]);
            assert!(accepts_input_at((100, 100), Some(&r), (50.0, 50.0)));
            assert!(accepts_input_at((100, 100), Some(&r), (20.0, 20.0)));
            assert!(!accepts_input_at((100, 100), Some(&r), (19.0, 50.0)));
            assert!(!accepts_input_at((100, 100), Some(&r), (80.0, 50.0)));
            assert!(!accepts_input_at((100, 100), Some(&r), (5.0, 5.0)));
        }

        /// **A region larger than the surface does not extend it.** The
        /// protocol says the input region "is clipped to the surface size", so
        /// the buffer bound is checked whether or not a region was set — which
        /// is the whole point of WLCS's `ClippedLargerRegion` parameterisation,
        /// and the case an implementation that checked the region *instead of*
        /// the bounds would get wrong.
        #[test]
        fn a_larger_region_is_clipped_to_the_surface() {
            let r = region(&[(RectangleKind::Add, -50, -50, 200, 200)]);
            assert!(accepts_input_at((100, 100), Some(&r), (50.0, 50.0)));
            assert!(!accepts_input_at((100, 100), Some(&r), (-10.0, 50.0)));
            assert!(!accepts_input_at((100, 100), Some(&r), (120.0, 50.0)));
        }

        /// An empty region is the opposite of an absent one: it accepts
        /// nothing. A client that calls `set_input_region` with a fresh
        /// `wl_region` is asking to be transparent to input, and that is a
        /// thing clients deliberately do.
        #[test]
        fn an_empty_region_accepts_nothing() {
            let r = region(&[]);
            assert!(!accepts_input_at((100, 100), Some(&r), (50.0, 50.0)));
        }

        /// `wl_region.subtract` is not a suggestion. A region built as one
        /// rectangle minus a hole has a hole, and the rectangles are applied
        /// in order — which is why `contains` scans the whole list rather than
        /// stopping at the first hit.
        #[test]
        fn a_subtracted_rectangle_is_a_hole() {
            let r = region(&[
                (RectangleKind::Add, 0, 0, 100, 100),
                (RectangleKind::Subtract, 40, 40, 20, 20),
            ]);
            assert!(accepts_input_at((100, 100), Some(&r), (10.0, 10.0)));
            assert!(!accepts_input_at((100, 100), Some(&r), (50.0, 50.0)));
            assert!(accepts_input_at((100, 100), Some(&r), (65.0, 50.0)));
        }

        /// Several disjoint rectangles, which is WLCS's `MultiRectCorners`:
        /// the corners accept and the middle does not.
        #[test]
        fn disjoint_rectangles_each_accept_their_own_area() {
            let r = region(&[
                (RectangleKind::Add, 0, 0, 20, 20),
                (RectangleKind::Add, 80, 80, 20, 20),
            ]);
            assert!(accepts_input_at((100, 100), Some(&r), (5.0, 5.0)));
            assert!(accepts_input_at((100, 100), Some(&r), (85.0, 85.0)));
            assert!(!accepts_input_at((100, 100), Some(&r), (50.0, 50.0)));
        }
    }
}
