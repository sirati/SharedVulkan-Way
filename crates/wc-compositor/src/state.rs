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
    AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle, RelativeMotionEvent,
};
// ADDED FOR §96: pointer locks, confinement and relative motion, which is what
// a game's mouse-look is made of. See `crate::hold` for the arithmetic and
// `State::update_pointer_hold` for the policy.
use smithay::wayland::pointer_constraints::{
    with_pointer_constraint, PointerConstraint, PointerConstraintsHandler,
    PointerConstraintsState,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::{delegate_pointer_constraints, delegate_relative_pointer};

use crate::hold::PointerHold;
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
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, SERIAL_COUNTER};
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
use smithay::reexports::wayland_server::protocol::wl_data_device_manager::DndAction;
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

use crate::platform::{
    CursorSpec, Edge, FromUi, IconImage, PopupBounds, ToUi, Ui, WindowId, WindowKind,
};
use crate::surface::{
    bars, decorator, fold_caption, view_of, Agreed, Bars, Decorator, PixelError, PixelFormat,
    PixelSource, Rect, Shadow, ShmSource, SinglePixelSource, View,
};
// §102: the three globals a nested compositor needs from its host.
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind as PresentedKind;
use smithay::wayland::presentation::{
    PresentationFeedbackCachedState, PresentationFeedbackCallback, PresentationState, Refresh,
};
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::{ensure_viewport_valid, ViewportCachedState, ViewporterState};
use smithay::{delegate_presentation, delegate_single_pixel_buffer, delegate_viewporter};

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
///
/// **§46 made `Auto` the default and `Server` a named leg.** `Server` was the
/// right default for as long as the compositor had only two arrangements and
/// had to promise one of them to every client; §31 built the third, and the
/// user's `weston-terminal` bug is a client for which `Server` is a promise
/// nobody can keep — it binds no decoration protocol, so it never hears the
/// promise, and it draws a title bar under ours. `Auto` advertises exactly what
/// `Server` advertises and differs only for a window that turns out to have
/// decorated itself anyway. See [`crate::surface::fold_caption`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Decorations {
    /// Advertise `Server`, and fold the caption for a window that decorated
    /// itself regardless. **The default since §46.**
    ///
    /// On the wire this is indistinguishable from [`Self::Server`]: same
    /// `default_mode`, same `xdg-decoration` answer. The difference is one
    /// `bool` per window, decided at map from the geometry inset, and it is
    /// what gives a non-negotiating self-decorating client one title bar
    /// instead of two.
    #[default]
    Auto,
    /// Windows draws the caption; the client draws none. **The default before
    /// §46**, and still the leg to name when a run wants the old behaviour
    /// — including the run that photographs the bug.
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
            "auto" => Some(Self::Auto),
            "server" => Some(Self::Server),
            "client" => Some(Self::Client),
            "tabs" => Some(Self::Tabs),
            _ => None,
        }
    }

    /// Whether **every** window in this run folds its caption, regardless of
    /// what the client turns out to be. `Tabs` only.
    ///
    /// §46 renamed nothing here and changed nothing about what it answers; what
    /// changed is that this is no longer the whole story, because `Auto` decides
    /// per window. See [`Self::auto`] and [`crate::surface::fold_caption`].
    pub fn extends_frame(self) -> bool {
        matches!(self, Self::Tabs)
    }

    /// Whether the fold is decided per window from what the client did.
    pub fn auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    /// The KDE protocol's spelling, which is the one that decides GTK's
    /// behaviour: `gtk_window_should_use_csd()` consults nothing else on
    /// Wayland, and it reads the **manager's** default rather than the answer
    /// to any per-surface request.
    fn kde(self) -> KdeDefaultMode {
        match self {
            // **`Auto` is `Server` on the wire, and that is the point.** A
            // client that binds this manager gets told the system decorates,
            // takes it, and draws no bar — which is already right and must not
            // change. `Auto` only differs for a client that never asked.
            Self::Auto | Self::Server => KdeDefaultMode::Server,
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
            Self::Auto | Self::Server => DecorationMode::ServerSide,
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
/// §102. A viewported surface's buffer and the view it was last drawn through;
/// see `State::viewed`.
struct Viewed {
    raw: Shadow,
    view: View,
}

/// §102. The globals `--nested-compositor-globals` adds. Held so they live as
/// long as the compositor; nothing reads them, because smithay's dispatch is
/// on the types.
struct NestedGlobals {
    _viewporter: ViewporterState,
    _single_pixel: SinglePixelBufferState,
    _presentation: PresentationState,
}

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
    /// Where this window's *own* popups may go, in this window's coordinates.
    ///
    /// ADDED FOR STAGE 38. Filled in by whatever can see the desktop — Windows
    /// through `platform/win32.rs`, the synthetic Win32 in `crate::control` on
    /// the conformance leg — and read here when a popup is parented to this
    /// window. See [`crate::platform::PopupBounds`] for why it is a cell and
    /// not a message.
    popup_bounds: PopupBounds,
    /// For a popup: the window it is parented to, and where it sits inside that
    /// parent's window geometry.
    ///
    /// ADDED FOR STAGE 38 for one reader, `crate::control`: the synthetic Win32
    /// has to know where a popup is in order to hit-test it, and a popup is the
    /// one window whose position the *compositor* chooses rather than the
    /// window manager. `None` for a toplevel, which Windows places.
    popup_at: Option<(WindowId, i32, i32)>,
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
    /// §91: the window size the platform last mapped or followed, so a commit
    /// that changes it is the client choosing a new size ([`ToUi::Resize`]).
    shown_size: (i32, i32),
    /// §91: the last `set_min_size` / `set_max_size` handed to the platform.
    size_hints: ((i32, i32), (i32, i32)),
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
    /// The popups that called `xdg_popup.grab`, outermost first.
    ///
    /// ADDED FOR STAGE 13 as one id; a stack since §85, because a submenu is a
    /// grab on top of its menu and the protocol dismisses them innermost first,
    /// all at once, for one click outside. See [`XdgShellHandler::grab`] and
    /// [`Self::dismiss_grabs_for_click`].
    popup_grabs: Vec<WindowId>,
    /// §93. A keyboard-focus loss and a deactivation not yet told to the
    /// client, in case the platform's focus only moved to one of the window's
    /// own popups. See [`HeldLoss`].
    focus_loss: HeldLoss,
    deactivation: HeldLoss,
    /// §85. The last `xdg_toplevel.move`/`resize` handed to the platform, for
    /// the one platform that has to run the gesture itself: the conformance
    /// stand-in (`crate::control`). Win32 runs it in `SC_MOVE`'s modal loop
    /// and never reads this. A `Cell` because the stand-in reads it through
    /// `&self`.
    gesture_request: std::cell::Cell<Option<(WindowId, Option<Edge>)>>,
    /// §85. The buttons smithay has been told are down, so a gesture can tell
    /// it they are up (smithay keeps its own list private).
    buttons_down: Vec<u32>,
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
    /// §102: the buffer behind every surface whose viewport makes its picture
    /// something other than the buffer itself, at the buffer's own size.
    ///
    /// For those surfaces `staging` holds the picture at the **surface's** size
    /// (the viewport's destination), because every reader of `staging` — the
    /// tree's bounds, the window's size, the hit test, `mapped_windows` —
    /// wants the surface's size and must not have to know about viewports. The
    /// buffer is kept beside it so that a commit which changes only the
    /// viewport (KWin's, when its window is resized: a new destination and no
    /// new buffer) can redraw the picture, and so that damage can stay
    /// incremental.
    viewed: HashMap<WlSurface, Viewed>,
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
    ///
    /// **§82: a map, and the value is the mode the two sides settled on.** The
    /// set could say *that* Firefox negotiated and not *what*, and what it
    /// negotiated is the difference between a tab strip in the caption and a
    /// tab strip under it.
    negotiated_decoration: HashMap<WlSurface, Agreed>,
    next_id: WindowId,
    started: Instant,
    /// When frame callbacks are answered (docs/waycompose-design.md §88).
    pub frame_clock: crate::tick::FrameClock,
    /// Callbacks of surfaces that are not windows — subsurfaces and cursors —
    /// waiting for the next tick, with the surface they were asked on.
    pending_callbacks: Vec<(WlCallback, u32)>,
    /// §102: `wp_presentation_feedback`s whose commit has been applied, with
    /// the surface they were asked on, waiting for the tick that answers them
    /// — the same tick that answers frame callbacks, which is the moment this
    /// compositor calls a frame shown (§88).
    feedback: Vec<(PresentationFeedbackCallback, WlSurface)>,
    /// §102: the frame clock's tick count, `presented`'s `seq`.
    present_seq: u64,
    /// §102: how many feedbacks were answered each way, for the log line at a
    /// window's end — the number a nested KWin's frame rate is.
    feedback_presented: u64,
    feedback_discarded: u64,
    feedback_superseded: u64,
    /// §102: `Some` when `--nested-compositor-globals` was given, and then the
    /// three globals exist. `None` is every session before §102, with exactly
    /// the globals it had.
    nested: Option<NestedGlobals>,
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
    /// §96. `zwp_pointer_constraints_v1` and `zwp_relative_pointer_manager_v1`,
    /// held so the globals live. smithay keeps the constraints themselves on
    /// the surfaces they were made for.
    _pointer_constraints: PointerConstraintsState,
    _relative_pointer: RelativePointerManagerState,
    /// §96. The constraint this compositor activated and is enforcing, and
    /// what the platform was last told about it. At most one: there is one
    /// pointer, and only the surface under it, in the window with the
    /// keyboard, can hold it.
    held: Option<Held>,
    /// §96. Where the *platform* last said the pointer was, which is not
    /// always where the client was told it is: a confined position has been
    /// clamped, and a locked one has not moved at all. The difference between
    /// two of these is the motion `zwp_relative_pointer_v1` reports.
    platform_at: Option<(WindowId, f64, f64)>,
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

    /// The Windows drag currently over one of our windows, if any: which
    /// window, and what it offers.
    ///
    /// ADDED FOR §42. `None` between drags, which is what makes a stray
    /// `DragMotion` — OLE is entitled to send one after a `DragLeave` if the
    /// user re-enters fast enough, and Wine does — a no-op rather than a
    /// motion delivered into a grab that is not running.
    dnd_over: Option<(WindowId, Vec<String>)>,
    /// The mime type the client under the drag last accepted, if any.
    ///
    /// `Some(None)` and `None` mean different things and the difference is the
    /// drop: `None` is "no client has said anything yet", `Some(None)` is "a
    /// client answered and refuses everything". Both forbid a drop; only the
    /// second is a decision.
    dnd_accepted: Option<Option<String>>,
    /// How many drags from Windows have entered a window of ours, and how many
    /// of those were dropped. The harness asserts on both: a drag that enters
    /// and never resolves is the failure this half exists not to build, and it
    /// is invisible in a count that only rises on success.
    pub dnd_entered: usize,
    pub dnd_dropped: usize,

    pub clients_seen: usize,
    pub toplevels: usize,
    /// ADDED FOR STAGE 9, so the harness can assert that a menu opened rather
    /// than inferring it from a picture.
    pub popups: usize,
    pub presents: usize,
    /// Commits since the dispatch loop last looked, for `WC_FRAME_TRACE`
    /// (docs/waycompose-design.md §88).
    pub trace_commits: u32,
    /// Input events handed to smithay since the loop last flushed, for
    /// `WC_FRAME_TRACE`'s `wc.flush` (docs/waycompose-design.md §89).
    pub trace_inputs: u32,
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
        nested_globals: bool,
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

        // §102. A nested compositor's host must offer these three — KWin's
        // Wayland backend exits at startup without any one of them — and no
        // session before §102 offered them. They stay behind a flag because a
        // global that is offered is used: GTK draws through `wp_viewporter`
        // whenever it exists, and Firefox on that path has not been measured.
        let nested = nested_globals.then(|| {
            println!(
                "wc-compositor: nested-compositor globals: wp_viewporter, \
                 wp_single_pixel_buffer_manager_v1, wp_presentation (clock {})",
                crate::tick::PRESENTATION_CLOCK_ID
            );
            NestedGlobals {
                _viewporter: ViewporterState::new::<State>(dh),
                _single_pixel: SinglePixelBufferState::new::<State>(dh),
                _presentation: PresentationState::new::<State>(dh, crate::tick::PRESENTATION_CLOCK_ID),
            }
        });

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
            popup_grabs: Vec::new(),
            focus_loss: HeldLoss::default(),
            deactivation: HeldLoss::default(),
            gesture_request: std::cell::Cell::new(None),
            buttons_down: Vec::new(),
            locks: (false, false),
            display: dh.clone(),
            seat,
            ui,
            events,
            // §102: a single-pixel buffer is "read" like any other, and only
            // exists when the nested-compositor globals do.
            sources: vec![Box::new(ShmSource), Box::new(SinglePixelSource)],
            staging: HashMap::new(),
            viewed: HashMap::new(),
            windows: HashMap::new(),
            negotiated_decoration: HashMap::new(),
            next_id: 1,
            started: Instant::now(),
            frame_clock: crate::tick::FrameClock::new(),
            pending_callbacks: Vec::new(),
            feedback: Vec::new(),
            present_seq: 0,
            feedback_presented: 0,
            feedback_discarded: 0,
            feedback_superseded: 0,
            nested,
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
            _pointer_constraints: PointerConstraintsState::new::<State>(dh),
            _relative_pointer: RelativePointerManagerState::new::<State>(dh),
            held: None,
            platform_at: None,
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
            dnd_over: None,
            dnd_accepted: None,
            dnd_entered: 0,
            dnd_dropped: 0,
            clients_seen: 0,
            toplevels: 0,
            popups: 0,
            presents: 0,
            trace_commits: 0,
            trace_inputs: 0,
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

    /// How long the loop may sleep before the next frame tick.
    pub fn until_frame(&self) -> std::time::Duration {
        self.frame_clock.until_next(Instant::now())
    }

    /// Answer every frame callback asked for since the last tick, on the tick.
    ///
    /// The tick is [`crate::tick::FrameClock`]'s grid, and the loop wakes on
    /// it ([`State::until_frame`]). Window and subsurface callbacks alike: a
    /// subsurface used to be answered at once, which made a client drawing
    /// into one — Firefox — run at whatever rate the round trip allowed
    /// rather than at the display's (docs/waycompose-design.md §88).
    pub fn tick_frames(&mut self) {
        if !self.frame_clock.due(Instant::now()) {
            return;
        }
        self.present_seq += 1;
        self.answer_feedback();
        let now = self.now_ms();
        for (root, window) in self.windows.iter_mut() {
            if sv_transport::frametrace::enabled() && !window.frame_callbacks.is_empty() {
                sv_transport::frametrace::emit(
                    "wc.done",
                    format_args!(
                        "win={} surf={} n={}",
                        window.id,
                        root.id().protocol_id(),
                        window.frame_callbacks.len()
                    ),
                );
            }
            for cb in window.frame_callbacks.drain(..) {
                cb.done(now);
            }
        }
        if !self.pending_callbacks.is_empty() {
            if sv_transport::frametrace::enabled() {
                let mut by: Vec<(u32, u32)> = Vec::new();
                for (_, s) in &self.pending_callbacks {
                    match by.iter_mut().find(|e| e.0 == *s) {
                        Some(e) => e.1 += 1,
                        None => by.push((*s, 1)),
                    }
                }
                for (s, n) in by {
                    sv_transport::frametrace::emit("wc.done_sub", format_args!("surf={s} n={n}"));
                }
            }
            for (cb, _) in self.pending_callbacks.drain(..) {
                cb.done(now);
            }
        }
    }

    /// Answer every presentation feedback waiting for this tick.
    ///
    /// ADDED FOR §102. `presented` for a commit whose surface is part of a
    /// window that is up — mapped and not minimised — and still has a picture;
    /// `discarded` for anything else (a cursor, a surface of a window that is
    /// not on screen, one destroyed since), which is the protocol's "the
    /// content update was never displayed".
    ///
    /// **Shown means "on this tick", the same moment frame callbacks are
    /// answered**, not "after the platform painted it". The paint is the UI
    /// thread's and happens on `WM_PAINT`, which this thread cannot see; the
    /// tick is the frame clock §88 put on DWM's vblank where there is one, and
    /// it is what a client paced by frame callbacks already treats as the
    /// display's rhythm. KWin's render loop advances on `presented` and with no
    /// answer at all stalls after its first frame, so a timely approximation is
    /// the thing that matters; across the proxy the timestamp is replaced by
    /// the Linux host's own clock anyway (`wc-proxy`, §102).
    ///
    /// No flags: this compositor cannot say the frame was tied to a vblank or
    /// timed by hardware, and zero is the protocol's way of not claiming it.
    fn answer_feedback(&mut self) {
        if self.feedback.is_empty() {
            return;
        }
        let time = crate::tick::presentation_now();
        let refresh = Refresh::fixed(self.frame_clock.period());
        for (fb, surface) in std::mem::take(&mut self.feedback) {
            let root = root_of(&surface);
            let shown = surface.is_alive()
                && self.staging.contains_key(&surface)
                && self.windows.get(&root).is_some_and(|w| w.mapped && !w.minimized);
            if shown {
                fb.presented(&self.output, time, refresh, self.present_seq, PresentedKind::empty());
                self.feedback_presented += 1;
            } else {
                fb.discarded();
                self.feedback_discarded += 1;
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
            // §89: the input event leaving the UI channel for smithay; the
            // turn's `wc.flush` is when its bytes leave the compositor.
            if sv_transport::frametrace::enabled()
                && crate::platform::trace_input("wc.in_tx", &event)
            {
                self.trace_inputs += 1;
            }
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
                FromUi::Activated { id, on } => self.on_activated(id, on),
                FromUi::KeyboardFocus { id, on, caps, num } => {
                    if on {
                        self.locks = (caps, num);
                    }
                    self.on_keyboard_focus(id, on)
                }
                FromUi::Key { id, keycode, pressed, time } => {
                    self.key(id, keycode, pressed, time)
                }
                FromUi::PointerEnter { id, x, y, time } => self.platform_motion(id, x, y, time),
                FromUi::PointerMotion { id, x, y, time } => self.platform_motion(id, x, y, time),
                FromUi::PointerLeave { id, time } => {
                    if self.platform_at.is_some_and(|(p, ..)| p == id) {
                        self.platform_at = None;
                    }
                    if self.pointer_focus == Some(id) {
                        self.pointer_motion(None, 0.0, 0.0, time);
                        self.update_pointer_hold();
                    }
                }
                FromUi::PointerRelative { id, dx, dy, time } => {
                    self.locked_motion(id, dx, dy, time)
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
                FromUi::DragEnter { id, x, y, mimes, actions, time } => {
                    self.dnd_enter(id, x, y, mimes, actions, time)
                }
                FromUi::DragMotion { id, x, y, time } => self.dnd_motion(id, x, y, time),
                FromUi::DragLeave { id, time } => self.dnd_leave(id, time),
                FromUi::DragDrop { id, x, y, time } => self.dnd_drop(id, x, y, time),
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
        // §93. A loss nothing took back within the grace is real.
        let now = Instant::now();
        if let Some(id) = self.focus_loss.due(now) {
            self.set_keyboard_focus(id, false);
        }
        if let Some(id) = self.deactivation.due(now) {
            self.deactivate(id);
        }
        // §85. After the events, because a move is one of them: on Win32 the
        // parent's `WM_WINDOWPOSCHANGED` has just rewritten its bounds.
        self.reconstrain_reactive_popups();
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

    // -----------------------------------------------------------------------
    // The drop target: a Windows drag, over one of our windows (§42)
    // -----------------------------------------------------------------------

    /// `IDropTarget::DragEnter`: something from a Windows application is over
    /// one of our windows.
    ///
    /// This is a **server-side** drag in Wayland's terms — the compositor is
    /// the source and no client owns a `wl_data_source` — which is exactly what
    /// `start_dnd` is for, and it is why `ServerDndGrabHandler` exists and has
    /// been empty since §29.
    ///
    /// # The grab needs a pointer that is already where the drag is
    ///
    /// `start_dnd` takes a `PointerGrabStartData` whose `focus` and `location`
    /// are where the gesture *began*, and smithay's DnD grab sends
    /// `wl_data_device.enter` to whatever is under that location. During an OLE
    /// drag there is no `WM_MOUSEMOVE` at all (§29.8), so the compositor's idea
    /// of where the pointer is can be arbitrarily old — it is wherever the
    /// mouse was when the user pressed the button in the *other* application.
    /// So the location is taken from `DragEnter`'s own coordinates, which
    /// Windows measured, and not from `self.pointer_at`.
    fn dnd_enter(&mut self, id: WindowId, x: f64, y: f64, mimes: Vec<String>, actions: u32, time: u32) {
        if self.dnd_over.is_some() {
            // OLE guarantees enter/leave are balanced per window, but a drag
            // that crosses from one of our windows straight into another
            // produces the second enter before the first leave under Wine.
            // Ending the old grab first keeps at most one running, which is
            // what "there is one mouse" means.
            self.dnd_leave_inner(time);
        }
        let Some(window) = self.by_id(id) else { return };
        let surface = window.role.surface().clone();
        self.dnd_entered += 1;
        self.dnd_over = Some((id, mimes.clone()));
        println!(
            "wc-compositor: a Windows drag entered window {id} at {x},{y} offering {} \
             mime type(s): {} (actions {})",
            mimes.len(),
            mimes.join(", "),
            crate::platform::dnd_action::describe(actions)
        );

        // The pointer has to be *at* the drag before the grab starts, or the
        // grab's first `enter` names whatever was under the stale position.
        self.pointer_motion(Some(id), x, y, time);

        let start_data = smithay::input::pointer::GrabStartData {
            focus: self
                .surface_under(id, x, y)
                .map(|(s, origin)| (s, origin.into()))
                .or(Some((surface, (0.0, 0.0).into()))),
            button: 0x110,
            location: (x, y).into(),
        };
        smithay::wayland::selection::data_device::start_dnd(
            &self.display.clone(),
            &self.seat.clone(),
            self,
            SERIAL_COUNTER.next_serial(),
            Some(start_data),
            None,
            smithay::wayland::selection::data_device::SourceMetadata {
                mime_types: mimes,
                dnd_action: dnd_actions_from_bits(actions),
            },
        );
    }

    /// `IDropTarget::DragOver`. The grab is running, so an ordinary pointer
    /// motion is what moves it: smithay's DnD grab intercepts `motion` and
    /// turns it into `wl_data_device.motion`, or into a `leave`/`enter` pair
    /// when it crosses a surface boundary.
    fn dnd_motion(&mut self, id: WindowId, x: f64, y: f64, time: u32) {
        if self.dnd_over.as_ref().map(|(w, _)| *w) != Some(id) {
            return;
        }
        self.pointer_motion(Some(id), x, y, time);
    }

    /// `IDropTarget::DragLeave`, and the cancellation path for everything else.
    fn dnd_leave(&mut self, id: WindowId, time: u32) {
        if self.dnd_over.as_ref().map(|(w, _)| *w) != Some(id) {
            return;
        }
        println!("wc-compositor: the Windows drag left window {id} without dropping");
        self.dnd_leave_inner(time);
    }

    /// End the grab without a drop, whatever the reason.
    ///
    /// **A button release is what ends a pointer grab in smithay**, not a
    /// method call: the grab holds the pointer until the button that started it
    /// goes up, and `ServerDnDGrab` treats that release as the drop. So a
    /// cancel is a release with the accepted-action bookkeeping already
    /// cleared, which makes smithay send `wl_data_device.leave` and finish the
    /// grab rather than `drop`.
    fn dnd_leave_inner(&mut self, time: u32) {
        self.dnd_over = None;
        // Nothing accepted, so the release cannot be read as a drop.
        self.dnd_accepted = None;
        self.dnd_release(time);
    }

    /// `IDropTarget::Drop`. The bytes are already staged on the platform side.
    fn dnd_drop(&mut self, id: WindowId, x: f64, y: f64, time: u32) {
        if self.dnd_over.as_ref().map(|(w, _)| *w) != Some(id) {
            return;
        }
        self.dnd_dropped += 1;
        println!("wc-compositor: the Windows drag was dropped on window {id} at {x},{y}");
        self.pointer_motion(Some(id), x, y, time);
        self.dnd_over = None;
        self.dnd_release(time);
    }

    /// The button release that ends the grab, one way or the other.
    fn dnd_release(&mut self, time: u32) {
        let pointer = self.pointer.clone();
        pointer.button(
            self,
            &smithay::input::pointer::ButtonEvent {
                button: 0x110,
                state: smithay::backend::input::ButtonState::Released,
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        pointer.frame(self);
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

    /// Whether every surface between `surface` and `root` still has a buffer.
    ///
    /// ADDED FOR §45.10 for the hit test, and made a function in §45.11 when it
    /// turned out the *bounding box* needed the same answer — which is the
    /// point §40 made about `placed_tree` and which this predicate had spent a
    /// section quietly contradicting.
    ///
    /// "A sub-surface is mapped when the parent is mapped **and** the
    /// sub-surface has a buffer." The staging entry is the buffer: it is
    /// created on commit and dropped on a null buffer or a destroy, so asking
    /// whether an ancestor has one is asking exactly the protocol's question.
    ///
    /// **The root is deliberately excluded.** "The root has no buffer" is the
    /// window-level question, answered by `Window::mapped` and by the platform
    /// ceasing to report the window; and a tree whose root has not drawn *yet*
    /// is the normal transient state §9 documented, where Firefox commits the
    /// subsurface holding the browser before the toplevel behind it.
    fn ancestors_mapped(&self, surface: &WlSurface, root: &WlSurface) -> bool {
        let mut ancestor = compositor::get_parent(surface);
        while let Some(p) = ancestor {
            if &p == root {
                return true;
            }
            if !self.staging.contains_key(&p) {
                return false;
            }
            ancestor = compositor::get_parent(&p);
        }
        true
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
            // **A subsurface is mapped only while its parent is.** The protocol
            // says so directly — "a sub-surface is mapped when the parent is
            // mapped and the sub-surface has a buffer" — and §40 implemented
            // only the second half: a surface that lost its own buffer stopped
            // taking input, and its children went on taking it as though
            // nothing had happened.
            //
            // WLCS's `unmapping_parent_stops_subsurface_getting_input` builds
            // exactly that: a subsurface, a subsurface of *it*, and a null
            // buffer committed to the middle one. The child is still there,
            // still has a buffer, and must not be clickable.
            //
            // Checked from the surface up to the root rather than by keeping a
            // mapped flag per surface, because the buffer *is* the flag — the
            // staging entry is created on commit and dropped on a null buffer
            // or a destroy (§40), so asking whether an ancestor has one is
            // asking the same question the child is asked.
            //
            // **The root is deliberately not part of this walk.** "The root has
            // no buffer" is the window-level question, and it is answered
            // already: `Window::mapped` gates it, and the platform stops
            // reporting the window at all. Including it here would also break
            // the case §9 documented — Firefox commits the subsurface holding
            // the browser *before* the toplevel behind it, so a tree whose root
            // has not drawn yet is a normal transient state rather than an
            // unmapped window.
            if !self.ancestors_mapped(&surface, &root) {
                return None;
            }
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

    /// §85. The toplevels that exist now, mapped or not: a toplevel that has
    /// been created and has not drawn yet is an application on its way.
    pub fn live_toplevels(&self) -> usize {
        self.windows.values().filter(|w| matches!(w.role, Role::Toplevel(_))).count()
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
                    popup_at: w.popup_at,
                    content: w.content_offset,
                })
            })
            .collect();
        out.sort_by_key(|r| r.id);
        out
    }

    /// ADDED FOR STAGE 38. Told by the synthetic Win32 where a window is on the
    /// one output, so that a popup of that window can be constrained to it.
    ///
    /// The real platform writes the same cell from the UI thread, from
    /// `MonitorFromWindow`; this is the same fact arriving from the stand-in.
    #[cfg(unix)]
    pub fn set_popup_bounds(&self, id: WindowId, bounds: Rect) {
        let Some(w) = self.windows.values().find(|w| w.id == id) else {
            return;
        };
        let mut cell = w.popup_bounds.lock().unwrap_or_else(|e| e.into_inner());
        *cell = Some(bounds);
    }

    /// Where a popup goes, in its parent's window-geometry coordinates.
    ///
    /// **REWRITTEN FOR STAGE 38, and the previous behaviour was a bug the user
    /// reported**: a menu opened near the right or bottom edge of the screen
    /// had the part that did not fit simply hanging off the display.
    ///
    /// `get_geometry` is the positioner's own arithmetic — anchor rectangle,
    /// anchor edge, gravity and offset — and nothing more. Up to this stage it
    /// was the whole answer, and the comment that said so argued that
    /// *"constraining a menu to the monitor is the compositor's job, and on
    /// Windows the compositor that owns the monitor is Windows"*. The first
    /// half is right and the second is false: `SetWindowPos` places a window
    /// exactly where it is told and constrains nothing. Windows keeps a menu on
    /// screen only for menus **it** owns, put up through `TrackPopupMenu`; an
    /// arbitrary `WS_POPUP` is placed literally, off the edge and all. Owning
    /// the monitor and constraining to it are different claims, and the code
    /// relied on the second while only the first was true.
    ///
    /// So the compositor does it, which is what `xdg_positioner`'s
    /// `set_constraint_adjustment` asks of it. The client declares which moves
    /// it will tolerate — `slide_x`, `flip_y`, `resize_x` and the rest — and
    /// `get_unconstrained_geometry` applies exactly those, in the precedence
    /// the XML fixes (flip, then slide, then resize, per axis, and a flip that
    /// fails to help is reverted). **That precedence is not reimplemented here
    /// on purpose**: a client that asked for `slide_x` alone must never be
    /// flipped, and the one implementation of that rule in the tree should be
    /// smithay's, which is generated from the same XML the client read.
    ///
    /// The naming is a trap worth restating, because it reads backwards:
    /// `get_geometry` is the *unconstrained* rectangle and
    /// `get_unconstrained_geometry` is the *constrained* one — "unconstrained"
    /// there means "adjusted until it no longer violates the constraint".
    ///
    /// `bounds` of `None` means nothing has told us where the monitor is, and
    /// then the answer is the raw arithmetic again — the pre-stage-38
    /// behaviour, which is wrong but is not a guess. A degenerate rectangle is
    /// treated the same way: fitting a menu into a zero-width monitor would
    /// produce a zero-width menu, and no answer is better than that one.
    fn constrain(positioner: PositionerState, bounds: Option<Rect>) -> Rectangle<i32, Logical> {
        match bounds {
            Some(b) if b.w > 0 && b.h > 0 => positioner.get_unconstrained_geometry(Rectangle::new(
                Point::from((b.x, b.y)),
                Size::from((b.w, b.h)),
            )),
            _ => positioner.get_geometry(),
        }
    }

    /// Move every reactive popup whose rectangle the parent's bounds now
    /// constrain differently. §85.
    ///
    /// `xdg_positioner.set_reactive`: "the popup will be reconstrained if the
    /// conditions used for constraining changed, e.g. the parent window moved".
    /// The answer goes out as a bare `configure`, with no `repositioned`
    /// (that one belongs to `xdg_popup.reposition` and its token), and the
    /// platform is told to move the window. A popup that is not reactive is
    /// left at its offset from the parent, which on Win32 is the platform's
    /// to keep (`follow_owner`) and in the stand-in is `control.rs`'s.
    ///
    /// Every turn rather than on an event, for the reason `publish_bounds`
    /// gives: the bounds are a cell the platform rewrites, and nothing tells
    /// this side when. A handful of rectangles per turn.
    fn reconstrain_reactive_popups(&mut self) {
        let mut moved = Vec::new();
        for w in self.windows.values() {
            let (Role::Popup(p), Some((parent, x, y))) = (&w.role, w.popup_at) else { continue };
            if !p.is_initial_configure_sent() {
                continue;
            }
            let positioner = p.with_pending_state(|s| s.positioner);
            if !positioner.reactive {
                continue;
            }
            let bounds = self.by_id(parent).and_then(popup_bounds_of);
            let rect = Self::constrain(positioner, bounds);
            let current = p.with_pending_state(|s| s.geometry);
            if rect != current || (rect.loc.x, rect.loc.y) != (x, y) {
                moved.push((w.id, p.clone(), rect));
            }
        }
        for (id, p, rect) in moved {
            println!(
                "wc-compositor: reactive xdg_popup window {id} reconstrained to {},{} {}x{}",
                rect.loc.x, rect.loc.y, rect.size.w, rect.size.h
            );
            p.with_pending_state(|s| s.geometry = rect);
            let _ = p.send_configure();
            if let Some(w) = self.windows.get_mut(p.wl_surface())
                && let Some(at) = w.popup_at.as_mut()
            {
                at.1 = rect.loc.x;
                at.2 = rect.loc.y;
            }
            self.ui.send(ToUi::MovePopup {
                id,
                x: rect.loc.x,
                y: rect.loc.y,
                width: rect.size.w,
                height: rect.size.h,
            });
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    /// §93. The toplevel a window belongs to: itself, or the window at the end
    /// of its popup chain.
    fn owning_toplevel(&self, id: WindowId) -> WindowId {
        self.popup_chain(id).last().copied().unwrap_or(id)
    }

    /// §93. The platform's `WM_SETFOCUS`/`WM_KILLFOCUS`, with a loss held
    /// back for [`FOCUS_MOVE_GRACE`] in case the focus only moved to a popup
    /// of the same window (or back from one). See [`HeldLoss`].
    fn on_keyboard_focus(&mut self, id: WindowId, on: bool) {
        if on {
            let gaining = self.owning_toplevel(id);
            let same = self.focus_loss.held().is_some_and(|h| self.owning_toplevel(h) == gaining);
            match self.focus_loss.regained(same) {
                Regained::Cancelled => {
                    println!(
                        "wc-compositor: window {id} has keyboard focus; it moved within window \
                         {gaining}, so the client is told nothing"
                    );
                    self.keyboard_focus = Some(id);
                    self.refocus_keyboard();
                    return;
                }
                Regained::Settle(held) => self.set_keyboard_focus(held, false),
                Regained::Nothing => {}
            }
            self.set_keyboard_focus(id, true);
        } else if self.keyboard_focus == Some(id) {
            self.focus_loss.hold(id, Instant::now());
        }
    }

    /// §93. `WM_ACTIVATE`, held back the same way as the keyboard focus.
    ///
    /// A popup's own activation is not a fact about anything the client can be
    /// told (`activated` is a toplevel state), so it only ever takes a held
    /// deactivation of its window back.
    fn on_activated(&mut self, id: WindowId, on: bool) {
        let window = self.owning_toplevel(id);
        if on {
            let same = self.deactivation.held() == Some(window);
            match self.deactivation.regained(same) {
                Regained::Cancelled => {
                    println!(
                        "wc-compositor: window {id} activated; the activation stayed within \
                         window {window}, so the client is told nothing"
                    );
                    return;
                }
                Regained::Settle(held) => self.deactivate(held),
                Regained::Nothing => {}
            }
            if window == id {
                self.set_state(id, |s| toggle(s, xdg_state::Activated, true));
            }
        } else if window == id {
            if let Some(held) = self.deactivation.take()
                && held != id
            {
                self.deactivate(held);
            }
            self.deactivation.hold(id, Instant::now());
        }
    }

    /// A toplevel lost activation, for real.
    fn deactivate(&mut self, id: WindowId) {
        // §85. A menu does not outlive its window losing the foreground: on
        // Windows that is a click in another application, which this
        // compositor never sees as a click, and a Firefox menu stayed open
        // over whatever the user had switched to.
        if self.popup_grabs.first().is_some_and(|&g| self.popup_chain(g).last() == Some(&id)) {
            self.dismiss_grabs_above(0, &format!("window {id} was deactivated"));
        }
        self.set_state(id, |s| toggle(s, xdg_state::Activated, false))
    }

    /// [`Self::set_keyboard_focus_now`], and then §96's question again: a
    /// pointer hold belongs to the window with the keyboard, so every change
    /// of keyboard focus can start or end one. One wrapper rather than a call
    /// after each of the five places that move the focus, so that a sixth
    /// cannot forget it.
    fn set_keyboard_focus(&mut self, id: WindowId, on: bool) {
        self.set_keyboard_focus_now(id, on);
        self.update_pointer_hold();
    }

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
    fn set_keyboard_focus_now(&mut self, id: WindowId, on: bool) {
        let Some(keyboard) = self.keyboard.clone() else { return };
        if on {
            // §93: a popup that took the platform's focus anyway hands it to its
            // window; in Wayland a popup without a grab never has the keyboard.
            let Some(surface) = self.surface_of(self.owning_toplevel(id)) else { return };
            self.keyboard_focus = Some(id);
            println!("wc-compositor: window {id} has keyboard focus");
            // §85: a grabbing popup of this window takes the keys instead.
            let surface = self.keyboard_target().unwrap_or(surface);
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
            if self.focus_loss.is(id) {
                self.focus_loss.take();
            }
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
        self.pointer_motion_by(id, x, y, time, None);
    }

    /// §96. The pointer moved because the *platform* said so — the user moved
    /// the mouse — as opposed to the scene changing under a pointer that stood
    /// still, which also goes through [`Self::pointer_motion`].
    ///
    /// Only this kind of motion is motion `zwp_relative_pointer_v1` reports,
    /// and only this kind is subject to a hold:
    ///
    /// * **locked**: the pointer does not move and the client hears only the
    ///   distance. On Win32 the platform reports nothing at all while a lock is
    ///   held — the system pointer is clipped and raw input carries the motion
    ///   as [`FromUi::PointerRelative`] — so this branch is the headless
    ///   platform's, where positions are all a test can produce.
    /// * **confined**: the position is clamped into the region before anybody
    ///   sees it. On Win32 `ClipCursor` has already done that, and the clamp
    ///   is then a no-op; here it is the whole enforcement.
    fn platform_motion(&mut self, id: WindowId, x: f64, y: f64, time: u32) {
        let delta = match self.platform_at.replace((id, x, y)) {
            Some((was, px, py)) if was == id => (x - px, y - py),
            _ => (0.0, 0.0),
        };
        let (mut x, mut y) = (x, y);
        if let Some(held) = self.held.as_ref().filter(|h| h.id == id) {
            match held.hold {
                PointerHold::Locked => {
                    self.locked_motion(id, delta.0, delta.1, time);
                    return;
                }
                PointerHold::Confined(r) => (x, y) = crate::hold::clamp_into(r, x, y),
            }
        }
        let moved = (delta != (0.0, 0.0)).then_some(delta);
        self.pointer_motion_by(Some(id), x, y, time, moved);
        // The pointer may just have entered a surface that holds a constraint
        // waiting for it, or left the one that was holding it.
        self.update_pointer_hold();
    }

    /// §96. Motion of a locked pointer: relative motion and a frame, and
    /// nothing else. The pointer does not move, so there is no `motion` event
    /// and no hit test — the surface that locked it keeps it.
    fn locked_motion(&mut self, id: WindowId, dx: f64, dy: f64, time: u32) {
        let Some(held) = self.held.as_ref().filter(|h| h.id == id && h.hold == PointerHold::Locked)
        else {
            // Raw motion for a window that no longer holds the pointer: the
            // platform has not caught up with a release yet. Nothing to say.
            return;
        };
        let surface = held.surface.clone();
        let origin = self.surface_under(id, self.pointer_at.0, self.pointer_at.1)
            .filter(|(s, _)| *s == surface)
            .map(|(_, o)| o)
            .unwrap_or((0.0, 0.0));
        let pointer = self.pointer.clone();
        pointer.relative_motion(
            self,
            Some((surface, origin.into())),
            &RelativeMotionEvent {
                delta: (dx, dy).into(),
                delta_unaccel: (dx, dy).into(),
                utime: u64::from(time) * 1000,
            },
        );
        pointer.frame(self);
    }

    /// [`Self::pointer_motion`], with the distance the mouse moved when the
    /// motion came from the mouse. The relative event goes in the same
    /// `wl_pointer.frame` as the absolute one, which is what the protocol means
    /// by them describing one motion.
    fn pointer_motion_by(
        &mut self,
        id: Option<WindowId>,
        x: f64,
        y: f64,
        time: u32,
        moved: Option<(f64, f64)>,
    ) {
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
            focus.clone().map(|(s, origin)| (s, origin.into())),
            &MotionEvent {
                location: (x, y).into(),
                serial: SERIAL_COUNTER.next_serial(),
                time,
            },
        );
        if let (Some((dx, dy)), Some((s, origin))) = (moved, focus) {
            pointer.relative_motion(
                self,
                Some((s, origin.into())),
                &RelativeMotionEvent {
                    delta: (dx, dy).into(),
                    delta_unaccel: (dx, dy).into(),
                    utime: u64::from(time) * 1000,
                },
            );
        }
        // Every logical group of pointer events ends in a `frame`. `wl_pointer`
        // version 5 made it required, clients batch on it, and omitting it
        // leaves a client holding a half-delivered event until the next one
        // arrives — which for a mouse that has stopped moving is forever.
        pointer.frame(self);
    }

    /// §85. A move or resize is starting, and the platform's modal loop is
    /// about to own the pointer: tell the client it left, and end the press
    /// that started the gesture without telling the client it was released.
    ///
    /// Windows' `SC_MOVE` loop eats the `WM_LBUTTONUP` that ends it, so this
    /// side never hears that release. Until §85 nothing ended the press here
    /// either, and smithay's implicit click grab — which holds the pointer to
    /// the surface a button went down on until every button is up — stayed
    /// installed after the move: motion over any other surface of ours was
    /// still delivered to the window that had been dragged, until the next
    /// click. A tab dragged by its strip left the rest of the session's hover
    /// pointed at it.
    ///
    /// `leave` is what the protocol owes the client here (WLCS
    /// `pointer_leaves_surface_during_interactive_move`/`_resize`), and it is
    /// also what a client takes as "every button is up". The pointer comes
    /// back with the next motion the platform reports after the loop.
    fn hand_pointer_to_platform(&mut self, id: WindowId) {
        let pointer = self.pointer.clone();
        let time = self.now_ms();
        if pointer.is_grabbed() {
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), time);
        }
        if self.pointer_focus == Some(id) {
            self.pointer_motion(None, 0.0, 0.0, time);
        }
        // Focus is gone, so these reach nobody; they only empty smithay's
        // pressed set, so the next press starts a fresh click grab.
        for button in std::mem::take(&mut self.buttons_down) {
            pointer.button(
                self,
                &ButtonEvent {
                    serial: SERIAL_COUNTER.next_serial(),
                    time,
                    button,
                    state: ButtonState::Released,
                },
            );
        }
    }

    /// See [`Self::gesture_request`]. Read only by the Unix-only stand-in.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn take_gesture_request(&self) -> Option<(WindowId, Option<Edge>)> {
        self.gesture_request.take()
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
        if self.pointer_focus != Some(id) {
            // A button from a window the pointer is not over. Reachable on
            // Win32 during a drag, where `SetCapture` keeps sending us the
            // release after the pointer has left; the release still has to be
            // delivered, so the pointer is put back over the window first.
            let (x, y) = self.pointer_at;
            self.pointer_motion(Some(id), x, y, time);
        }
        if pressed {
            if !self.buttons_down.contains(&button) {
                self.buttons_down.push(button);
            }
        } else if self.buttons_down.contains(&button) {
            self.buttons_down.retain(|b| *b != button);
        } else {
            // §85: a release for a press `hand_pointer_to_platform` already
            // ended. smithay has been told; the client was told `leave`.
            println!(
                "wc-compositor: window {id} pointer button {button:#x} release after a gesture \
                 ended its press; not delivered"
            );
            return;
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
        // **After the button, and that ordering is the protocol's.** §75.
        //
        // This used to run before `pointer.button`, so a click outside a
        // grabbing popup sent `xdg_popup.popup_done` and only then delivered the
        // press. WLCS names the requirement directly —
        // `does_not_get_popup_done_event_before_button_press` — and measured it:
        // "pointer button sent after popup done", expected false, actual true.
        //
        // The order matters to a client rather than being a formality. A menu
        // dismissed *before* the press has no way to tell a click that merely
        // closed it from a click that closed it and then activated whatever was
        // underneath; the press is the event that says which, and it has to
        // arrive while the popup still exists to attribute it.
        //
        // After `frame` rather than merely after `button`, because the press and
        // its frame are one logical group and `popup_done` is a different
        // interface's event about a different object.
        if !pressed {
            self.dismiss_grabs_for_click(id);
        }
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

    /// Dismiss the grabbing popups a click landed outside of.
    ///
    /// ADDED FOR STAGE 13. A menu is closed by the compositor and not by the
    /// client, because the client cannot see a click outside its own surfaces.
    /// Without this a menu stays up until the application decides to take it
    /// down, which for a click on the page behind it is never.
    ///
    /// §85: a click dismisses **every** grab above the one it landed in, and all
    /// of them when it landed in none, innermost first. Stage 13 dismissed only
    /// the innermost and relied on the client's destroy to expose the next, so
    /// a Firefox submenu took one click per level to close and the outer menu
    /// was never told at all by a client that did not destroy on `popup_done`
    /// (WLCS `grabbed_popups_get_done_events_in_correct_order`). A click in a
    /// non-grabbing popup counts as a click in the nearest grabbing popup it
    /// hangs off, so a tooltip over a menu does not close the menu.
    ///
    /// The popup itself is not destroyed here. `popup_done` is a *request* —
    /// the client destroys the object, and that arrives as
    /// [`XdgShellHandler::popup_destroyed`], which is the only place a window
    /// is removed. Tearing the `HWND` down from here as well would race the
    /// client and leave the protocol object pointing at nothing.
    fn dismiss_grabs_for_click(&mut self, clicked: WindowId) {
        if self.popup_grabs.is_empty() {
            return;
        }
        let keep = self
            .popup_chain(clicked)
            .into_iter()
            .find_map(|w| self.popup_grabs.iter().position(|&g| g == w))
            .map_or(0, |i| i + 1);
        self.dismiss_grabs_above(keep, &format!("click on window {clicked} outside the grab"));
    }

    /// `clicked`, then the window it is a popup of, and so on to the toplevel.
    fn popup_chain(&self, mut id: WindowId) -> Vec<WindowId> {
        let mut chain = vec![id];
        while let Some((parent, _, _)) = self.by_id(id).and_then(|w| w.popup_at) {
            if chain.contains(&parent) {
                break;
            }
            chain.push(parent);
            id = parent;
        }
        chain
    }

    /// Send `popup_done` to every grabbing popup from index `keep` up,
    /// innermost first, and forget them as grabs. The keyboard goes back to
    /// whatever is grabbing now, or to the toplevel.
    fn dismiss_grabs_above(&mut self, keep: usize, why: &str) {
        while self.popup_grabs.len() > keep {
            let Some(grabbing) = self.popup_grabs.pop() else { break };
            if let Some(Window { role: Role::Popup(p), .. }) = self.by_id(grabbing) {
                println!(
                    "wc-compositor: {why} -> window {grabbing} dismissed (xdg_popup.popup_done)"
                );
                p.send_popup_done();
            }
        }
        self.refocus_keyboard();
    }

    /// §85. Where keys go: the innermost grabbing popup, if it hangs off the
    /// window Windows has focused; otherwise that window.
    ///
    /// `keyboard_focus` stays the *platform's* answer — the `HWND` with
    /// `WM_SETFOCUS`, which should be a toplevel because a popup is
    /// `WS_EX_NOACTIVATE` and answers `MA_NOACTIVATE`, and is mapped to its
    /// toplevel when it is not (§93: Wine focused a clicked popup) — and this
    /// is the surface smithay is told. The two had been the same thing, which is why a grabbed menu could not be driven
    /// with the arrow keys (WLCS `grabbed_popup_gets_keyboard_focus`).
    fn keyboard_target(&self) -> Option<WlSurface> {
        let focused = self.owning_toplevel(self.keyboard_focus?);
        let grab = self
            .popup_grabs
            .iter()
            .rev()
            .find(|&&g| self.popup_chain(g).last() == Some(&focused));
        self.surface_of(*grab.unwrap_or(&focused))
    }

    /// Point smithay's keyboard at [`Self::keyboard_target`] if it is not
    /// there already. A no-op while nothing has keyboard focus.
    fn refocus_keyboard(&mut self) {
        let Some(keyboard) = self.keyboard.clone() else { return };
        let Some(target) = self.keyboard_target() else { return };
        if keyboard.current_focus().as_ref() == Some(&target) {
            return;
        }
        self.set_clipboard_focus(Some(&target));
        keyboard.set_focus(self, Some(target), SERIAL_COUNTER.next_serial());
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
    pub fn quit(&self, clients: u32) {
        self.ui.send(ToUi::Quit { clients });
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

    /// §102: the committed viewport of `surface` — source rectangle and
    /// destination size, either possibly unset — and its buffer scale.
    ///
    /// `None` without the nested-compositor globals, where no viewport can
    /// exist, so that the cache is not even asked: a session without the flag
    /// runs exactly the code it ran before §102.
    #[allow(clippy::type_complexity)]
    fn viewport_of(
        &self,
        surface: &WlSurface,
    ) -> Option<(Option<(f64, f64, f64, f64)>, Option<(i32, i32)>, i32)> {
        self.nested.as_ref()?;
        Some(compositor::with_states(surface, |states| {
            let vp = *states.cached_state.get::<ViewportCachedState>().current();
            let scale = states.cached_state.get::<SurfaceAttributes>().current().buffer_scale;
            (
                vp.src.map(|r| (r.loc.x, r.loc.y, r.size.w, r.size.h)),
                vp.dst.map(|d| (d.w, d.h)),
                scale,
            )
        }))
    }

    /// §102: a commit with no new buffer may still have changed the viewport,
    /// and then the surface's picture and size have changed.
    ///
    /// This is KWin's resize: the window is configured to a new size, KWin
    /// sets its main surface's viewport destination to it and commits — and
    /// the buffer, its one black pixel, was attached once at startup and is
    /// never attached again, so the buffer path never sees the new size.
    /// Measured under Wine: a 900x600 configure is answered with
    /// `set_destination(900, 600)` on the main surface and new 900x600 layer
    /// buffers, and the window's picture is 900x600 from the next frame.
    ///
    /// Redraws from the kept buffer (`State::viewed`), or from the surface's
    /// own pixels when it is gaining a view now, and recomposes the window.
    fn reapply_view(&mut self, surface: &WlSurface) {
        let Some((src, dst, scale)) = self.viewport_of(surface) else { return };
        let changed = if let Some(entry) = self.viewed.get(surface) {
            match view_of((entry.raw.width(), entry.raw.height()), scale, src, dst) {
                Some(view) if view == entry.view => false,
                Some(view) => {
                    let entry = self.viewed.get_mut(surface).expect("just read");
                    entry.view = view;
                    let staging = self.staging.entry(surface.clone()).or_insert_with(Shadow::empty);
                    staging.scale_from(&entry.raw, &view, None);
                    true
                }
                None => {
                    let entry = self.viewed.remove(surface).expect("just read");
                    self.staging.insert(surface.clone(), entry.raw);
                    true
                }
            }
        } else {
            let Some(shown) = self.staging.get(surface).filter(|s| !s.is_empty()) else { return };
            match view_of((shown.width(), shown.height()), scale, src, dst) {
                None => false,
                Some(view) => {
                    let raw = self.staging.remove(surface).expect("just read");
                    let mut staging = Shadow::empty();
                    staging.scale_from(&raw, &view, None);
                    self.staging.insert(surface.clone(), staging);
                    self.viewed.insert(surface.clone(), Viewed { raw, view });
                    true
                }
            }
        };
        if !changed {
            return;
        }
        let root = root_of(surface);
        if !self.windows.get(&root).is_some_and(|w| w.mapped) {
            return;
        }
        let (_, w, h) = self.compose_tree(&root, surface, &[]);
        if w <= 0 || h <= 0 {
            return;
        }
        let window = &self.windows[&root];
        let id = window.id;
        // The window follows a client that is still choosing its size, as
        // §91 and §96 have it on the other two paths; one that was configured
        // to a size is catching up with it and is not followed.
        let client_chooses = window.role.toplevel().is_some_and(|t| {
            t.with_pending_state(|s| {
                s.size.is_none()
                    && !s.states.contains(xdg_state::Maximized)
                    && !s.states.contains(xdg_state::Fullscreen)
            })
        });
        if client_chooses && window.shown_size != (w, h) {
            if let Some(window) = self.windows.get_mut(&root) {
                window.shown_size = (w, h);
            }
            println!("wc-compositor: window {id} client chose {w}x{h} by its viewport; the window follows");
            self.ui.send(ToUi::Resize { id, width: w, height: h });
        }
        self.ui.send(ToUi::Present { id, damage: vec![Rect::new(0, 0, w, h)] });
    }

    /// Copy a committed buffer into the shadow and tell the platform.
    fn present(
        &mut self,
        surface: &WlSurface,
        buffer: &wl_buffer::WlBuffer,
        damage: Vec<Rect>,
        surface_damage: bool,
    ) {
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

        let read_started = Instant::now();
        // §102: the viewport and buffer scale this commit applied, or `None`
        // when the nested-compositor globals are off and no viewport can exist.
        let viewport = self.viewport_of(surface);
        let staging = self.staging.entry(surface.clone()).or_insert_with(Shadow::empty);
        let viewed = &mut self.viewed;
        let mut result: Option<(Vec<Rect>, i32, i32)> = None;
        let mut served_by = "";
        // Ask each source in turn. Today there is one; the loop is what lets a
        // buffer that lives in an arena rather than a `wl_shm` pool arrive
        // without this function changing (docs/goal-firefox.md).
        for source in &self.sources {
            let read = source.with_pixels(buffer, &mut |bytes, spec| {
                let Some((src, dst, scale)) = viewport else {
                    result = Some((staging.blit(bytes, spec, &damage), spec.width, spec.height));
                    return;
                };
                // §102. `out_of_buffer` is checked against the buffer the
                // commit carried, which only now has a size; smithay posts the
                // error, and the client is gone once this dispatch ends.
                let logical = (spec.width / scale.max(1), spec.height / scale.max(1));
                if !compositor::with_states(surface, |st| ensure_viewport_valid(st, logical.into())) {
                    return;
                }
                match view_of((spec.width, spec.height), scale, src, dst) {
                    None => {
                        // Leaving a view: the shadow holds the old view's
                        // picture, so it is started again and the blit below
                        // copies the whole buffer rather than its damage.
                        if viewed.remove(surface).is_some() {
                            *staging = Shadow::empty();
                        }
                        result = Some((staging.blit(bytes, spec, &damage), spec.width, spec.height));
                    }
                    Some(view) => {
                        let entry = viewed
                            .entry(surface.clone())
                            .or_insert_with(|| Viewed { raw: Shadow::empty(), view });
                        // Surface damage is in surface coordinates, which a
                        // view makes different from the buffer's; rather than
                        // invert the view, the whole buffer is taken.
                        let whole = [Rect::new(0, 0, spec.width, spec.height)];
                        let written = entry.raw.blit(bytes, spec, if surface_damage { &whole } else { &damage });
                        let rects = (entry.view == view).then(|| view.map_damage(&written));
                        entry.view = view;
                        let drawn = staging.scale_from(&entry.raw, &view, rects.as_deref());
                        result = Some((drawn, view.dst.0, view.dst.1));
                    }
                }
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
        // §102. **A subsurface does not map a window whose root has never had
        // a buffer and which declared no geometry.** The protocol's own rule
        // ("a sub-surface is mapped when the parent is mapped"), applied where
        // it used to cost a visible jump: KWin's synchronised subsurfaces are
        // applied in the same transaction as its main surface and reach this
        // function first, so the window was mapped at the size of whatever
        // layer came first — a 32x32 cursor-sized one at (636, 396) — shifted
        // by that offset, and resized to the main surface's 1280x800 a moment
        // later. The root's own commit is the next call. A declared geometry
        // is left as it was: that is Firefox, whose subsurface-first map at
        // its geometry §9 measured and every later section relies on.
        if surface != &root
            && !self.staging.contains_key(&root)
            && !self.windows[&root].mapped
            && declared_geometry(&root).is_none()
        {
            return;
        }

        // Composite the tree, and find out what that changed. `Some(rects)` is
        // the damage in window coordinates; `None` means the canvas itself was
        // reshaped and everything is new.
        let composed_started = Instant::now();
        let (composited, cw, ch) = self.compose_tree(&root, surface, &written);
        if sv_transport::frametrace::enabled() {
            let ft_read = sv_transport::frametrace::us(composed_started - read_started);
            sv_transport::frametrace::emit_at(
                sv_transport::frametrace::at_us(read_started),
                "wc.present",
                format_args!(
                    "surf={} root={} read_us={ft_read} compose_us={} rects={} {cw}x{ch}",
                    surface.id().protocol_id(),
                    root.id().protocol_id(),
                    sv_transport::frametrace::us(composed_started.elapsed()),
                    composited.len(),
                ),
            );
        }
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
        // §91. **Is the client still the one choosing its size?** It is until
        // this compositor has configured it with one: the initial configure
        // carries none, and only a user's resize, a maximise or a fullscreen
        // puts one in the pending state. While it chooses, a commit at a new
        // window geometry *is* the new size, and the window follows it —
        // Firefox commits a first size before it has read its saved geometry,
        // and its real one a few frames later (§90.7). Once it has been told a
        // size, a commit at another one is the client catching up, and the
        // window keeps the size it was given.
        let client_chooses = window.role.toplevel().is_some_and(|t| {
            t.with_pending_state(|s| {
                s.size.is_none()
                    && !s.states.contains(xdg_state::Maximized)
                    && !s.states.contains(xdg_state::Fullscreen)
            })
        });
        let mut follow = toplevel && !first && client_chooses && (w, h) != window.shown_size;
        if first || follow {
            window.shown_size = (w, h);
        }
        let hints = if toplevel { size_hints_of(&root) } else { window.size_hints };
        let hints_changed = hints != window.size_hints;
        window.size_hints = hints;
        // §91.6. **A client that chose nothing gets a default.** Firefox sizes
        // a window with no saved geometry from `screen.availWidth`/`Height`
        // (90% of it, at most 1280x1040), and on the guest those read as
        // nothing when the window is made, so it commits exactly its own
        // `set_min_size`, 500x120, and stays there. A resizable toplevel with
        // no parent that is still choosing and has committed precisely its
        // minimum has not chosen a size; it is configured to Firefox's own rule
        // against the work area this window is on, and the window is resized to
        // match. A dialog (it has a parent), a fixed-size window (min = max), a
        // minimum that is a usable size, and any client that commits anything
        // but its minimum are left alone.
        let mut default_size = None;
        if toplevel
            && client_chooses
            // The hints can arrive a commit after the size does (they did on
            // the guest), so the commit that changes them is asked too.
            && (first || follow || hints_changed)
            && hints.0.0 > 0
            && hints.0.1 > 0
            && (w, h) == hints.0
            && hints.1 != hints.0
            // And degenerate: Firefox's crash reporter commits its minimum,
            // 600x400, and means it (measured on the guest, where the first
            // version of this rule enlarged it).
            && (w < 320 || h < 200)
            && window.role.toplevel().is_some_and(|t| t.parent().is_none())
            && let Some(b) = popup_bounds_of(window)
        {
            let dw = (b.w * 9 / 10).min(1280).max(hints.0.0);
            let dh = (b.h * 9 / 10).min(1040).max(hints.0.1);
            if (dw, dh) != (w, h) {
                default_size = Some((dw, dh));
                window.shown_size = (dw, dh);
                follow = false;
            }
        }
        self.presents += 1;
        // Before `Map`, so that the first placement already answers
        // `WM_GETMINMAXINFO` with them.
        if hints_changed {
            self.ui.send(ToUi::SizeHints { id, min: hints.0, max: hints.1 });
        }
        if follow {
            println!("wc-compositor: window {id} client chose {w}x{h}; the window follows");
            self.ui.send(ToUi::Resize { id, width: w, height: h });
        }
        if let Some((dw, dh)) = default_size {
            println!(
                "wc-compositor: window {id} committed only its minimum size {w}x{h}; \
                 configuring the default {dw}x{dh} (§91.6)"
            );
            if !first {
                self.ui.send(ToUi::Resize { id, width: dw, height: dh });
            }
            self.on_resized(id, dw, dh);
        }

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
            let fold_caption = self.report_decorator(&root, id, &app_id);
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
            // §91.6: a client that chose nothing is shown at its default.
            let (mw, mh) = default_size.unwrap_or((w, h));
            self.ui.send(ToUi::Map {
                id,
                width: mw,
                height: mh,
                fold_caption,
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
    /// Decide this window's frame, and say so. Returns whether its caption is
    /// folded into its client area, which is what `ToUi::Map` carries.
    ///
    /// §33 wrote this as a report and said in as many words that it reported
    /// and did not decide, because the arrangement to switch to did not exist
    /// on that branch and because the inset is not a fact a live window style
    /// should track. §31 built the arrangement and §46 takes the decision —
    /// **once, here, at map** — which is what makes the second objection not
    /// apply. See [`crate::surface::fold_caption`].
    fn report_decorator(&self, root: &WlSurface, id: WindowId, app_id: &str) -> bool {
        use smithay::wayland::shell::xdg::SurfaceCachedState;
        let inset = compositor::with_states(root, |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
                .map_or((0, 0), |r| (r.loc.x, r.loc.y))
        });
        let agreed = self
            .negotiated_decoration
            .get(root)
            .copied()
            .unwrap_or(Agreed::Nothing);
        let who = decorator(inset, agreed);
        // The verdict is the pair. See [`crate::surface::bars`]: §31's fold
        // reverses which of these two clients is the correct one, so a message
        // that named "two title bars" from `who` alone was right only while the
        // caption was never folded — which was true when §33 was written and
        // stopped being true when §31 merged.
        // §46: per window, not per run. `extends_frame()` is still the run-wide
        // `tabs` switch and is still honoured; `auto` adds the arm that asks
        // what this particular client turned out to be.
        let folded = fold_caption(
            self.decorations.extends_frame(),
            self.decorations.auto(),
            who,
        );
        let app = if app_id.is_empty() { "<no app id>" } else { app_id };
        match (who, bars(who, folded)) {
            // §82. The client said, on the protocol, that it draws its own bar.
            // Two lines rather than one arm per verdict because the reason is
            // different from the guessed case below and the log is where a
            // reader finds out which of the two this window was.
            (Decorator::AskedForClient, Bars::OneClient) => println!(
                "wc-compositor: window {id} ({app}) asked to decorate itself (Client) and was \
                 told yes, so this window's caption is folded into its client area ({:?}) and \
                 the client's own title bar -- Firefox's tab strip -- is the window's (§82)",
                self.decorations
            ),
            (Decorator::AskedForClient, _) => println!(
                "wc-compositor: window {id} ({app}) asked to decorate itself (Client) and this run \
                 does not fold its caption ({:?}), so THIS WINDOW HAS TWO TITLE BARS -- \
                 §22.4's cost of echoing; the default `auto` folds it (§82)",
                self.decorations
            ),
            (_, Bars::Agreed) => println!(
                "wc-compositor: window {id} ({app}) negotiated its decorations; \
                 geometry inset {},{}",
                inset.0, inset.1
            ),
            (_, Bars::OneSystem) => println!(
                "wc-compositor: window {id} ({app}) bound no decoration protocol and has no \
                 margin around its window; Windows' caption is the only title bar"
            ),
            (_, Bars::OneClient) => println!(
                "wc-compositor: window {id} ({app}) bound no decoration protocol and reports a \
                 {},{} margin around its window -- it is drawing its own title bar, so this \
                 window's caption is folded into its client area ({:?}) and that bar is the \
                 window's (§46)",
                inset.0, inset.1, self.decorations
            ),
            // The two that are defect reports. Named as such, because the
            // compositor is telling the truth about a window the user is about
            // to find wrong, and a line that read like the other two would be
            // read like the other two.
            (_, Bars::Two) => println!(
                "wc-compositor: window {id} ({app}) bound no decoration protocol and reports a \
                 {},{} margin around its window -- it is drawing its own title bar and did not \
                 read our default of {:?}, so THIS WINDOW HAS TWO TITLE BARS (§33)",
                inset.0, inset.1, self.decorations
            ),
            (_, Bars::None) => println!(
                "wc-compositor: window {id} ({app}) draws no title bar of its own and this run \
                 folds the caption away ({:?}), so THIS WINDOW HAS NO TITLE BAR -- \
                 `--decorations tabs` is right for a self-decorating client and wrong for this \
                 one, and it is a per-run flag (§33.7)",
                self.decorations
            ),
        }
        folded
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

        // Where each surface of the tree sits, relative to the root surface's
        // own origin. Collected first because composing needs `&mut` on the
        // window's shadow while `staging` is read, and because the traversal
        // closure cannot borrow `self`.
        let placed = placed_tree(root);

        // The window geometry rectangle, in root-surface coordinates. Absent
        // means "the whole buffer", which is what the protocol says and what
        // every non-GTK client relies on.
        let geometry = declared_geometry(root);
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
        // **An unmapped subtree is not part of the bounds, and §45.11 is where
        // that was learned rather than asserted.** §45.10 taught the hit test
        // that a subsurface is mapped only while its parent is; the bounding
        // box went on counting the children of an unmapped surface, so a window
        // whose overhanging subsurface had been unmapped stayed the larger
        // size. Nothing was drawn there — the unmapped surface has no staging
        // entry and neither, usually, do its children — but the *window* still
        // covered those pixels, and a window covers pixels whether or not it
        // paints them. WLCS's `input_falls_through_subsurface_when_parent_unmapped`
        // is precisely that: a lower window underneath the overhang, which
        // must receive the click and could not while a larger empty window sat
        // over it.
        //
        // This is §40's argument about `placed_tree` arriving where it was
        // always going: painting and hit-testing must not disagree about which
        // surfaces are in the tree, and for one section they did.
        let bounds = placed
            .iter()
            .filter(|(surface, _)| self.ancestors_mapped(surface, root))
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
        let was = window.content_offset;
        let mapped = window.mapped;
        let wid = window.id;
        window.content_offset = (gx, gy);
        // **And when it changes, the window has moved on screen.** §45.11.
        //
        // `content_offset` says where the window's top-left pixel sits inside
        // the root surface's buffer. If it changes while the window is up — a
        // subsurface appears off the left edge and the tree's bounds grow, or a
        // client re-declares its geometry — then the buffer has not moved and
        // the window's corner has, by exactly the difference. Nothing said so,
        // so the window stayed where it was and its contents slid inside it.
        //
        // Sent as a **delta**, not a position, and that is the architecture
        // rather than convenience: this compositor does not know where any of
        // its windows are. Windows owns that (`control.rs`'s header), and the
        // only thing this side can truthfully say is "whatever you have, it
        // moved by this much".
        //
        // **§82: only when the extent is the bounding box, never a declared
        // geometry.** A declared origin moving is a client changing its drop
        // shadow, not a window moving: GTK re-declares `(26, 23)` as `(0, 0)`
        // when Firefox is maximised — the shadow goes, the buffer is replaced
        // by one without it, and the window rectangle is the same rectangle.
        // Shifting the `HWND` by `(-26, -23)` there moved a *maximised* window
        // off its monitor by the shadow, and the first 23 rows of Firefox's tab
        // strip with it; measured under Wine (`origin shifted by -26,-23`, then
        // the tab strip above the screen in the capture), and the same message
        // would do the same on the guest. Every Wayland compositor keeps the
        // window geometry where it is and moves the surface under it, which is
        // what not shifting means here, and it is what §45.11's own unit test
        // already says for the synthetic Win32: "a declared inset leaves the
        // window where it was put". The bounding-box case — a subsurface
        // growing the window leftwards — is the one §45.11 was about and is
        // unchanged.
        let declared = matches!(geometry, Some(r) if r.size.w > 0 && r.size.h > 0);
        if mapped && was != (gx, gy) && !declared {
            self.ui.send(ToUi::ShiftOrigin {
                id: wid,
                dx: gx - was.0,
                dy: gy - was.1,
            });
        }
        let shadow = window.shadow.clone();
        let mut shadow = shadow.lock().unwrap_or_else(|e| e.into_inner());
        let reshaped = shadow.reset(gw, gh, format);
        // §95: every surface after the first is drawn *over* what is under it
        // (`Shadow::compose_over`), so what is under it must be this frame's.
        // The root's copy repaints the whole canvas whenever the canvas lies
        // inside the root's buffer, which a declared geometry always does; a
        // tree whose bounds reach past the root (a subsurface overhanging it)
        // starts from transparent instead of from the last frame's pixels.
        let root_covers = self.staging.get(root).is_some_and(|s| {
            gx >= 0 && gy >= 0 && gx + gw <= s.width() && gy + gh <= s.height()
        });
        if !root_covers {
            shadow.clear();
        }
        let mut first = true;
        for (surface, (x, y)) in &placed {
            // The same rule as the bounds above and as the hit test: a
            // subsurface of an unmapped surface is not mapped, so it is not
            // drawn either. In practice such a surface usually has no staging
            // entry of its own and this changes nothing — but "usually" is not
            // a reason for the paint path to use a different predicate from
            // the two that decide where the window is and what is under the
            // pointer.
            if !self.ancestors_mapped(surface, root) {
                continue;
            }
            if let Some(src) = self.staging.get(surface) {
                if first {
                    shadow.compose(src, (x - gx, y - gy));
                } else {
                    shadow.compose_over(src, (x - gx, y - gy));
                }
            }
            first = false;
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

/// What a window's popups have to fit inside, if anything has said.
///
/// ADDED FOR STAGE 38. A poisoned lock is treated as "nothing has said" rather
/// than unwrapped: the cell is written by the UI thread, and a panic there must
/// not take the Wayland thread with it — the worst it can cost is one menu
/// placed the way every menu was placed before this stage.
fn popup_bounds_of(window: &Window) -> Option<Rect> {
    *window.popup_bounds.lock().unwrap_or_else(|e| e.into_inner())
}

/// The window geometry rectangle a client has **declared**, if it has declared
/// one.
///
/// ADDED FOR §45, as one function because it now has two readers and they must
/// not drift: `compose_tree` turns it into the window's crop and size, and the
/// state-only commit path in `CompositorHandler::commit` compares it against
/// what the window already has to decide whether that work is needed at all.
///
/// `None` is a real answer and is not the same as `Some` of the buffer's
/// rectangle. It means the client has never called
/// `xdg_surface.set_window_geometry`, and the protocol then defines the
/// geometry as the bounds of the tree — which is a *derived* value that changes
/// whenever a subsurface moves, and therefore cannot be compared against a
/// stored one to detect a client's intent. Keeping the two cases distinct here
/// is what stops the caller confusing "the client changed its mind" with "a
/// subsurface moved".
/// §91: the committed `xdg_toplevel.set_min_size` and `set_max_size`, 0 where
/// unset (which is what the protocol's own 0 means too).
fn size_hints_of(root: &WlSurface) -> ((i32, i32), (i32, i32)) {
    use smithay::wayland::shell::xdg::SurfaceCachedState;
    compositor::with_states(root, |states| {
        let mut cache = states.cached_state.get::<SurfaceCachedState>();
        let c = cache.current();
        ((c.min_size.w, c.min_size.h), (c.max_size.w, c.max_size.h))
    })
}

fn declared_geometry(root: &WlSurface) -> Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> {
    use smithay::wayland::shell::xdg::SurfaceCachedState;
    compositor::with_states(root, |states| {
        states.cached_state.get::<SurfaceCachedState>().current().geometry
    })
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

/// §93. How long a lost focus or activation is held back to see whether it
/// only moved to a popup of the same window. Windows sends the loss and the
/// gain from inside one `SetFocus`/`SetActiveWindow`, microseconds apart, but
/// the Wayland thread can wake between the two messages; one loop turn is
/// 5 ms, so this is a few turns and well under anything a person sees.
const FOCUS_MOVE_GRACE: std::time::Duration = std::time::Duration::from_millis(30);

/// §93. A focus (or activation) loss that has not been told to the client
/// yet.
///
/// The client must not hear `wl_keyboard.leave` (or lose `activated`) when
/// the platform's focus only moved from a window to one of that window's own
/// popups: in Wayland a popup without a grab never has the keyboard, and
/// Firefox rolls every menu up the moment its window loses focus, so a click
/// on a menu item that moved the focus closed the menu instead of acting.
/// `WM_MOUSEACTIVATE` is the platform's half of the fix; this is the half
/// that holds whatever the platform does.
#[derive(Debug, Default)]
struct HeldLoss(Option<(WindowId, Instant)>);

/// What a gain does to a held loss.
#[derive(Debug, PartialEq, Eq)]
enum Regained {
    /// Nothing was held.
    Nothing,
    /// The focus came back to the same window (or a popup of it): the loss is
    /// dropped and the client is told nothing.
    Cancelled,
    /// The focus went to another window: the held loss is real and must be
    /// told first, so the old window's keys are released before the new one
    /// is entered.
    Settle(WindowId),
}

impl HeldLoss {
    fn hold(&mut self, id: WindowId, now: Instant) {
        self.0 = Some((id, now));
    }
    fn held(&self) -> Option<WindowId> {
        self.0.map(|(id, _)| id)
    }
    /// `same_window` says whether the held window and the gaining one are the
    /// same toplevel.
    fn regained(&mut self, same_window: bool) -> Regained {
        match self.0.take() {
            None => Regained::Nothing,
            Some(_) if same_window => Regained::Cancelled,
            Some((held, _)) => Regained::Settle(held),
        }
    }
    /// The held loss, if it has waited out the grace.
    fn due(&mut self, now: Instant) -> Option<WindowId> {
        match self.0 {
            Some((id, at)) if now.duration_since(at) >= FOCUS_MOVE_GRACE => {
                self.0 = None;
                Some(id)
            }
            _ => None,
        }
    }
    fn take(&mut self) -> Option<WindowId> {
        self.0.take().map(|(id, _)| id)
    }
    fn is(&self, id: WindowId) -> bool {
        self.0.is_some_and(|(held, _)| held == id)
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
        self.viewed.remove(surface);
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

        self.trace_commits += 1;
        if sv_transport::frametrace::enabled() {
            sv_transport::frametrace::emit(
                "wc.c",
                format_args!(
                    "surf={} win={} buf={} cbs={}",
                    surface.id().protocol_id(),
                    self.windows.contains_key(surface) as u8,
                    matches!(buffer, Some(BufferAssignment::NewBuffer(_))) as u8,
                    callbacks.len(),
                ),
            );
        }
        // §102. Presentation feedback rides the same commit, and is answered on
        // the same tick (`State::answer_feedback`). A feedback still waiting
        // from an earlier commit of this surface was superseded before any
        // tick showed it, which the protocol calls discarded.
        if self.nested.is_some() {
            let asked = compositor::with_states(surface, |states| {
                std::mem::take(
                    &mut states.cached_state.get::<PresentationFeedbackCachedState>().current().callbacks,
                )
            });
            if !asked.is_empty() {
                let (superseded, kept): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.feedback).into_iter().partition(|(_, s)| s == surface);
                self.feedback = kept;
                for (fb, _) in superseded {
                    fb.discarded();
                    self.feedback_discarded += 1;
                    self.feedback_superseded += 1;
                }
                self.feedback.extend(asked.into_iter().map(|fb| (fb, surface.clone())));
            }
        }
        if let Some(window) = self.windows.get_mut(surface) {
            window.frame_callbacks.extend(callbacks);
        } else {
            // A surface with no window of its own — a cursor, or a subsurface.
            // Its callbacks are answered on the next tick, like a window's: it
            // is on the same display (docs/waycompose-design.md §88).
            let id = surface.id().protocol_id();
            self.pending_callbacks.extend(callbacks.into_iter().map(|cb| (cb, id)));
        }

        // Buffer scale and transform are honoured only in their identity form.
        // Anything else would need the damage rectangles mapped between surface
        // and buffer coordinates and the blit to rotate or subsample, which is
        // a renderer's job and not this stage's. Saying so out loud beats
        // silently drawing the wrong thing.
        // §102: with a viewport the scale *is* honoured (`surface::view_of`).
        if (scale != 1 && !self.viewed.contains_key(surface)) || transform != wl_output::Transform::Normal {
            eprintln!(
                "wc-compositor: buffer scale {scale} / transform {transform:?} is not implemented; \
                 drawing the buffer unscaled"
            );
        }

        // §102. Whether any of it is in surface coordinates, which a viewport
        // makes different from buffer coordinates (see `present`).
        let surface_damage = damage.iter().any(|d| matches!(d, Damage::Surface(_)));
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
                self.present(surface, &buffer, rects, surface_damage)
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
                self.viewed.remove(surface);
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
            // without a buffer means nothing — but the *geometry* can mean a
            // great deal.
            //
            // **`xdg_surface.set_window_geometry` is double-buffered, and until
            // §45 it was only ever read on the buffer path.** `compose_tree` is
            // the one place that turns a geometry rectangle into
            // `Window::content_offset` and into the window's size, and
            // `present` is the only caller — so a client that set a geometry
            // and committed *without attaching a new buffer* had it applied to
            // nothing. The crop stayed where it was, the window kept its old
            // size, and every input event kept arriving at the old offset.
            //
            // That is not a corner case invented by the suite. It is what a
            // client does whenever its window outline changes but its pixels do
            // not: leaving a maximised state, a GTK client adjusting its shadow
            // margin, anything that acks a configure and re-declares its
            // geometry before it has drawn. WLCS asks it as
            // `XdgToplevelStableTest.pointer_respects_window_geom_offset` and
            // its touch twin, and both failed with the pointer arriving at the
            // raw window coordinate — the exact symptom §37 fixed for the
            // buffer path, surviving on the path §37 did not look at.
            //
            // **Only a *declared* geometry, and only when it has changed**, and
            // both halves were learned by breaking something.
            //
            // The first attempt recomposed on every state-only commit of a
            // window root. That is two tests worse, not better, and the reason
            // is instructive: when a client has declared *no* geometry the
            // protocol defines it as the bounds of the tree, so recomposing
            // re-derives `content_offset` from wherever the subsurfaces
            // currently are — and a parent's commit is exactly the moment
            // smithay is applying pending subsurface state, so the walk can
            // read positions that are about to change. The compositor then
            // caches a crop computed from a half-applied tree, and the pointer
            // arithmetic that §40 fixed is wrong again by that difference.
            // `place_above_simple` and `subsurface_moves_under_input_device`
            // are the two that noticed.
            //
            // So the trigger is a client's *stated* intent — a rectangle it
            // called `set_window_geometry` with — compared against the one the
            // window is already using. A derived geometry is left to the buffer
            // path, where the tree is settled, and to §40's hit test, which
            // reads subsurface positions live and never caches them.
            None => {
                self.reapply_view(surface);
                let declared = declared_geometry(surface)
                    .filter(|r| r.size.w > 0 && r.size.h > 0);
                if let (true, Some(r)) = (self.windows.contains_key(surface), declared) {
                    let before = self.windows[surface].content_offset;
                    let before_size = self.windows[surface]
                        .shadow
                        .lock()
                        .map(|s| (s.width(), s.height()))
                        .unwrap_or((0, 0));
                    // Not an early `return`: the pointer re-evaluation at the
                    // end of this function is §40's, it applies to every
                    // commit, and skipping it here would trade one bug for
                    // another.
                    let changed = before != (r.loc.x, r.loc.y)
                        || before_size != (r.size.w, r.size.h);
                    let (_, w, h) = if changed {
                        self.compose_tree(surface, surface, &[])
                    } else {
                        (Vec::new(), 0, 0)
                    };
                    let after = self.windows[surface].content_offset;
                    if w > 0 && h > 0 && (before != after || before_size != (w, h)) {
                        let id = self.windows[surface].id;
                        println!(
                            "wc-compositor: window {id} geometry committed without a buffer: \
                             crop {before:?} -> {after:?}, {}x{} -> {w}x{h}",
                            before_size.0, before_size.1
                        );
                        // The whole window, because the crop moved: every pixel
                        // of it now shows a different part of the buffer.
                        self.ui.send(ToUi::Present {
                            id,
                            damage: vec![Rect::new(0, 0, w, h)],
                        });
                        // §96, found by Minecraft: **a client still choosing
                        // its size has chosen this one**, and the window
                        // follows — the rule §91 made for the buffer path,
                        // which a geometry-only commit never reached. GLFW
                        // with libdecor maps its window *with* client-side
                        // decorations (854x480 inside 902x552 of frame and
                        // shadow), learns a moment later that the decorations
                        // are the server's, destroys the frame and declares
                        // 854x480 — with no new buffer, because its content
                        // did not change. The `HWND` stayed 902x552 and showed
                        // a band of background under the game for as long as
                        // it ran.
                        let window = &self.windows[surface];
                        let client_chooses = window.role.toplevel().is_some_and(|t| {
                            t.with_pending_state(|s| {
                                s.size.is_none()
                                    && !s.states.contains(xdg_state::Maximized)
                                    && !s.states.contains(xdg_state::Fullscreen)
                            })
                        });
                        if window.mapped && client_chooses && window.shown_size != (w, h) {
                            if let Some(window) = self.windows.get_mut(surface) {
                                window.shown_size = (w, h);
                            }
                            println!(
                                "wc-compositor: window {id} client chose {w}x{h} by its geometry \
                                 alone; the window follows"
                            );
                            self.ui.send(ToUi::Resize { id, width: w, height: h });
                        }
                    }
                }
            }
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
        // §96: a commit can move or unmap the held surface, or change the
        // constraint's region; and a held pointer's hold is re-checked.
        if self.held.is_some() {
            self.update_pointer_hold();
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
        let popup_bounds: PopupBounds = Arc::new(Mutex::new(None));
        // §85. A new toplevel ends every popup grab: the grab was a claim on
        // the pointer and keyboard for a menu, and a new window is about to
        // take both. WLCS `grabbed_popup_gets_done_event_when_new_toplevel_created`.
        self.dismiss_grabs_above(0, "a new toplevel was created");
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
            bounds: popup_bounds.clone(),
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
                shown_size: (0, 0),
                size_hints: ((0, 0), (0, 0)),
                icon_dirty: false,
                popup_bounds,
                popup_at: None,
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
            let id = window.id;
            println!("wc-compositor: window {id} asked to be moved");
            self.hand_pointer_to_platform(id);
            self.gesture_request.set(Some((id, None)));
            self.ui.send(ToUi::BeginMove { id });
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
            let id = window.id;
            println!("wc-compositor: window {id} asked to be resized from {edge:?}");
            self.hand_pointer_to_platform(id);
            self.gesture_request.set(Some((id, Some(edge))));
            self.ui.send(ToUi::BeginResize { id, edge });
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
        let id = self.windows.get(surface.wl_surface()).map(|w| w.id);
        if let Some(id) = id {
            self.ui.send(ToUi::SetWindowState {
                id,
                maximized: Some(true),
                minimized: None,
            });
        }
        // xdg-shell requires a configure in response to `set_maximized` even if
        // the compositor declines. The real state arrives a moment later from
        // `WM_SIZE`; this meets the protocol obligation on time rather than
        // making it conditional on a round trip through Win32.
        //
        // **§45: the configure has to carry the state, not merely exist.** It
        // used to be a bare `send_configure()`, on the reasoning that the state
        // was Windows' to report and would follow from `WM_SIZE`. Both halves
        // of that are true and the conclusion still did not hold: a client that
        // asks to be maximised and receives a configure *without* the maximized
        // state has been told, in the protocol's own terms, that its request
        // was declined. It then gets a second configure a moment later saying
        // the opposite. Nothing in xdg-shell licenses that sequence, and a
        // client is entitled to act on the first one — WLCS's
        // `window_can_maximize_itself` does exactly that, waiting for one
        // configure and reading the state off it.
        //
        // So the state is adopted here, at the moment the request is honoured
        // and forwarded, rather than borrowed from the echo. **If Windows
        // refuses, the echo corrects it** — `FromUi::Maximized { on: false }`
        // runs through the same `set_state` and sends another configure — so
        // the optimism is bounded by the same round trip that used to be waited
        // for, and the disagreement is now the exceptional case rather than the
        // ordinary one.
        //
        // **§91: and it carries a size.** A maximised configure is one the
        // client must obey, so it has to say what to obey; it used to inherit
        // the size of the configure every window got at map, and §91 removed
        // that one (it echoed a client's first commit back as an instruction).
        // The size sent is where the window's popups may go — on Windows the
        // monitor's work area, which a maximised window with its caption folded
        // fills exactly (§82), and the stand-in's output headless — and the
        // `WM_SIZE` that the maximise produces corrects it if Windows disagrees.
        match id {
            Some(id) => {
                let size = self.by_id(id).and_then(popup_bounds_of).map(|b| (b.w, b.h));
                self.set_state(id, |s| {
                    toggle(s, xdg_state::Maximized, true);
                    if let Some((w, h)) = size.filter(|&(w, h)| w > 0 && h > 0) {
                        s.size = Some((w, h).into());
                    }
                })
            }
            None => { surface.send_configure(); }
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        // The mirror of `maximize_request`, and it carries the state for the
        // same reason: a configure that does not clear `maximized` says the
        // request was declined.
        let id = self.windows.get(surface.wl_surface()).map(|w| w.id);
        if let Some(id) = id {
            self.ui.send(ToUi::SetWindowState {
                id,
                maximized: Some(false),
                minimized: None,
            });
        }
        match id {
            Some(id) => self.set_state(id, |s| toggle(s, xdg_state::Maximized, false)),
            None => { surface.send_configure(); }
        }
    }

    /// §96: `xdg_toplevel.set_fullscreen`, which is Minecraft's F11.
    ///
    /// Until §96 this was smithay's default — a bare configure — so the request
    /// was declined in the protocol's own terms and F11 did nothing. It is now
    /// honoured the way `maximize_request` honours a maximise: the platform is
    /// asked (borderless over the monitor, on Win32) and the state is adopted
    /// in the same configure, with the output's size; the `WM_SIZE` that
    /// follows corrects the size if the monitor is not the output.
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
        let Some(id) = self.windows.get(surface.wl_surface()).map(|w| w.id) else {
            surface.send_configure();
            return;
        };
        self.ui.send(ToUi::Fullscreen { id, on: true });
        let size = self.output.current_mode().map(|m| (m.size.w, m.size.h));
        println!("wc-compositor: window {id} asked for fullscreen ({size:?})");
        self.set_state(id, |s| {
            toggle(s, xdg_state::Fullscreen, true);
            if let Some((w, h)) = size.filter(|&(w, h)| w > 0 && h > 0) {
                s.size = Some((w, h).into());
            }
        })
    }

    /// §96: and back. The size is the platform's to say — the window goes back
    /// to the rectangle it had — so the configure carries the state alone and
    /// the `WM_SIZE` brings the size.
    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.windows.get(surface.wl_surface()).map(|w| w.id) else {
            surface.send_configure();
            return;
        };
        self.ui.send(ToUi::Fullscreen { id, on: false });
        println!("wc-compositor: window {id} left fullscreen");
        self.set_state(id, |s| toggle(s, xdg_state::Fullscreen, false))
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
            // §93: the platform's focus may sit on one of this window's
            // popups, and the client's is on this window either way.
            if let Some(f) = self.keyboard_focus
                && self.owning_toplevel(f) == id
            {
                self.set_keyboard_focus(f, false);
            }
            if self.deactivation.is(id) {
                self.deactivation.take();
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
            if self.nested.is_some() {
                println!(
                    "wc-compositor: presentation feedback so far: {} presented, {} discarded \
                     ({} superseded before a tick)",
                    self.feedback_presented, self.feedback_discarded, self.feedback_superseded
                );
            }
            self.ui.send(ToUi::Destroy { id: window.id });
            // §96: if it held the pointer, the platform is told to let go.
            self.update_pointer_hold();
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

        let parent = surface.get_parent_surface().map(|s| root_of(&s));
        let parent_window = parent.as_ref().and_then(|p| self.windows.get(p));
        let parent_id = parent_window.map(|w| w.id);
        let bounds = parent_window.and_then(popup_bounds_of);

        // Where the popup goes, in the parent's window-geometry coordinates.
        // See [`Self::constrain`]: the rectangle is the positioner's own
        // arithmetic, fitted to the monitor if anything has told us where the
        // monitor is.
        let rect = Self::constrain(positioner, bounds);

        // The declared adjustment is logged, and it is the first question to
        // ask of any report that a menu is in the wrong place: a client that
        // declared nothing is **entitled** to a popup that hangs off the
        // screen, and moving it would be the bug. `weston-terminal` declares
        // nothing; Firefox declares all six.
        println!(
            "wc-compositor: xdg_popup created (#{}) -> window {id} at {},{} {}x{} parent {:?} \
             adjust {:?} bounds {}",
            self.popups,
            rect.loc.x,
            rect.loc.y,
            rect.size.w,
            rect.size.h,
            parent_id,
            positioner.constraint_adjustment,
            match bounds {
                Some(b) => format!("{},{} {}x{}", b.x, b.y, b.w, b.h),
                None => "unknown".to_owned(),
            }
        );

        let Some(parent_id) = parent_id else {
            // A popup whose parent is not a window we know about. Legal to
            // refuse, and the client is told rather than left waiting.
            eprintln!("wc-compositor: xdg_popup has no mapped parent; dismissing it");
            surface.send_popup_done();
            return;
        };

        let shadow = Arc::new(Mutex::new(Shadow::empty()));
        let popup_bounds: PopupBounds = Arc::new(Mutex::new(None));
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
            // A popup is a window like any other and a submenu is parented to
            // it, so it gets a cell of its own; the platform fills it in from
            // the popup's own `HWND`.
            bounds: popup_bounds.clone(),
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
                shown_size: (0, 0),
                size_hints: ((0, 0), (0, 0)),
                icon_dirty: false,
                popup_bounds,
                popup_at: Some((parent_id, rect.loc.x, rect.loc.y)),
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
            // §93: a popup that held the platform's focus gives it back to
            // its window, which is where the client's focus has been all
            // along, and the loss is held for the grace: Windows hands the
            // focus to the owner when the popup goes, and if it does not,
            // the loss is told then.
            if self.keyboard_focus == Some(id) {
                let window = self.owning_toplevel(id);
                if window == id {
                    self.set_keyboard_focus(id, false);
                } else {
                    self.keyboard_focus = Some(window);
                    self.focus_loss.hold(window, Instant::now());
                }
            }
            if self.pointer_focus == Some(id) {
                self.pointer_motion(None, 0.0, 0.0, self.now_ms());
            }
            if let Some(i) = self.popup_grabs.iter().position(|&g| g == id) {
                // A grab destroyed from the middle takes the ones above it
                // with it: they hang off it and the protocol says so.
                self.dismiss_grabs_above(i + 1, "the popup under it was destroyed");
                self.popup_grabs.truncate(i);
                self.refocus_keyboard();
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
            println!(
                "wc-compositor: xdg_popup grab -> window {id} owns the pointer and the keyboard \
                 ({} grab(s) under it)",
                self.popup_grabs.len()
            );
            if !self.popup_grabs.contains(&id) {
                self.popup_grabs.push(id);
            }
            self.refocus_keyboard();
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
    /// The positioner goes through [`Self::constrain`], exactly as
    /// [`Self::new_popup`] does. **REWRITTEN FOR STAGE 38**, and this is the
    /// half that is easy to forget: a submenu that GTK opens near the bottom of
    /// the screen is *first* mapped wherever the positioner says and *then*
    /// repositioned with a second positioner carrying `slide_y`, so a
    /// compositor that constrains only at creation moves the menu on screen and
    /// then puts it back off again on the next keystroke.
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let Some(&Window { id, .. }) = self.windows.get(surface.wl_surface()) else {
            // A reposition for a popup we never mapped. Answering the token
            // anyway is better than silence: the client is waiting for it.
            surface.send_repositioned(token);
            return;
        };
        // The bounds belong to the **parent**, not to the popup being moved:
        // the positioner's rectangle is in the parent's coordinates, and so is
        // the answer. Resolved the same way `new_popup` resolves it, because a
        // popup can be reparented in neither protocol nor practice but can
        // certainly be repositioned after its parent has moved to another
        // monitor.
        let bounds = surface
            .get_parent_surface()
            .map(|s| root_of(&s))
            .and_then(|p| self.windows.get(&p))
            .and_then(popup_bounds_of);
        let rect = Self::constrain(positioner, bounds);
        println!(
            "wc-compositor: xdg_popup reposition -> window {id} to {},{} {}x{} (token {token})",
            rect.loc.x, rect.loc.y, rect.size.w, rect.size.h
        );
        surface.with_pending_state(|state| {
            state.geometry = rect;
        });
        surface.send_repositioned(token);
        let _ = surface.send_configure();
        // The synthetic Win32 hit-tests popups by this, so a popup that moved
        // and did not say so would keep taking clicks at its old position.
        if let Some(w) = self.windows.get_mut(surface.wl_surface())
            && let Some(at) = w.popup_at.as_mut()
        {
            at.1 = rect.loc.x;
            at.2 = rect.loc.y;
        }
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
                match data.0.lock().unwrap().chunk(&bytes) {
                    // §93: **not** a protocol error. It used to be, and the
                    // error ended the feed: after one copy too large (a long
                    // page's HTML), copy and paste did nothing for the rest
                    // of that client's session. The selection is refused and
                    // the feed stays.
                    Err(StageError::TooBig) => println!(
                        "wc-compositor: a selection passed {} MiB; it is refused and the \
                         system clipboard is left as it was",
                        crate::clipboard::MAX_SELECTION_BYTES >> 20
                    ),
                    Err(e) => {
                        let (code, why) = fail(e);
                        resource.post_error(code, why);
                    }
                    Ok(()) => {}
                }
            }
            Request::Data { mime_type } => {
                if let Err(e) = data.0.lock().unwrap().data(mime_type) {
                    let (code, why) = fail(e);
                    resource.post_error(code, why);
                }
            }
            Request::Publish => {
                let refused = data.0.lock().unwrap().overflowed();
                let payloads = data.0.lock().unwrap().take();
                if refused {
                    println!("wc-compositor: the refused selection's publish installs nothing");
                    return;
                }
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
        fd: smithay::utils::os::ProtoFd,
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

/// A drag started by a **client**, within Wayland.
///
/// Still at its defaults, and §29's reasoning for that is unchanged: a client
/// dragging out of its own window is intercepted by `wc-proxy`, which pulls the
/// payload and calls `wc_clipboard_v1.start_drag`, so the compositor is told
/// about it through that protocol rather than through this handler. The one
/// case this trait would serve — a drag from one Wayland window to another,
/// both on this compositor — smithay runs entirely by itself.
///
/// (The comment that used to be here said wc-proxy answers `start_drag` with an
/// immediate `cancelled`. That was true when it was written and stopped being
/// true in §29, which built the pull. Corrected rather than deleted, because a
/// stale comment about a *behaviour* is the §35 failure in miniature.)
impl ClientDndGrabHandler for State {}

/// A drag started by the **compositor**, which here means by Windows: something
/// is being dragged out of a Windows application and over one of our windows,
/// and `dnd_enter` has called `start_dnd`.
///
/// ADDED FOR §42. Every method here is a client's answer arriving
/// asynchronously, and the two that matter are `accept` and `action`, because
/// `IDropTarget::DragOver` has to answer OLE *synchronously* — see
/// [`crate::platform::ToUi::DragFeedback`] for that decision and its cost.
impl ServerDndGrabHandler for State {
    fn accept(&mut self, mime_type: Option<String>, _seat: Seat<Self>) {
        println!(
            "wc-compositor: the client under the drag accepts {}",
            match &mime_type {
                Some(m) => m.as_str(),
                None => "nothing",
            }
        );
        let accepted = mime_type.is_some();
        self.dnd_accepted = Some(mime_type);
        self.ui.send(ToUi::DragFeedback {
            accepted,
            // The action is reported separately by `action` below; until it
            // arrives, "accepted" alone means copy, which is what every
            // toolkit's default is and what `DROPEFFECT_COPY` shows.
            action: if accepted {
                crate::platform::dnd_action::COPY
            } else {
                crate::platform::dnd_action::NONE
            },
        });
    }

    fn action(&mut self, action: DndAction, _seat: Seat<Self>) {
        let bits = dnd_action_bits(action);
        println!(
            "wc-compositor: the client under the drag chose {}",
            crate::platform::dnd_action::describe(bits)
        );
        self.ui.send(ToUi::DragFeedback {
            accepted: self.dnd_accepted.as_ref().is_some_and(|m| m.is_some()),
            action: bits,
        });
    }

    /// The user let go. smithay has sent `wl_data_device.drop` to the client,
    /// which is now entitled to `receive` on the offer.
    fn dropped(&mut self, _seat: Seat<Self>) {
        println!("wc-compositor: the client under the drag was told about the drop");
    }

    fn cancelled(&mut self, _seat: Seat<Self>) {
        println!("wc-compositor: the drag over our window was cancelled");
        self.dnd_accepted = None;
    }

    /// `wl_data_offer.receive` on a **drag** offer, which carries an `h`.
    ///
    /// Closed rather than written, for exactly the reason
    /// [`SelectionHandler::send_selection`] closes its own — and it is the same
    /// sentence with one word changed, so it is worth being explicit that this
    /// is not an oversight in a new feature. The bytes reach the client through
    /// `wc_clipboard_v1.fetch(source=drag)`, which `wc-proxy` issues *instead*
    /// of forwarding this request; a `receive` that reaches here at all came
    /// from a client with no proxy in front of it (§29.6).
    fn send(&mut self, mime_type: String, fd: smithay::utils::os::ProtoFd, _seat: Seat<Self>) {
        drop(fd);
        println!(
            "wc-compositor: a client asked for the drag's bytes as {mime_type:?} through \
             wl_data_offer.receive, which carries a descriptor this compositor cannot \
             write into across the boundary; the bytes go through wc_clipboard_v1's \
             fetch(source=drag) instead. Closing the descriptor: an empty drop rather \
             than a dead client."
        );
    }

    fn finished(&mut self, _seat: Seat<Self>) {
        println!("wc-compositor: the client has finished with the drag");
        self.dnd_accepted = None;
    }
}

/// Wayland's `DndAction` bitflags from the plain bits that cross the platform
/// boundary, and back.
///
/// Two functions rather than a cast in four places, and they are here rather
/// than in `crate::platform` because `DndAction` is smithay's type and the
/// platform side must not link a protocol crate — the same rule that keeps
/// `Edge` and evdev keycodes in the boundary's own vocabulary.
fn dnd_actions_from_bits(bits: u32) -> DndAction {
    let mut out = DndAction::empty();
    if bits & crate::platform::dnd_action::COPY != 0 {
        out |= DndAction::Copy;
    }
    if bits & crate::platform::dnd_action::MOVE != 0 {
        out |= DndAction::Move;
    }
    if bits & crate::platform::dnd_action::ASK != 0 {
        out |= DndAction::Ask;
    }
    out
}

fn dnd_action_bits(action: DndAction) -> u32 {
    let mut out = 0;
    if action.contains(DndAction::Copy) {
        out |= crate::platform::dnd_action::COPY;
    }
    if action.contains(DndAction::Move) {
        out |= crate::platform::dnd_action::MOVE;
    }
    if action.contains(DndAction::Ask) {
        out |= crate::platform::dnd_action::ASK;
    }
    out
}

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
        self.negotiated_decoration
            .insert(toplevel.wl_surface().clone(), agreed_xdg(mode));
        println!("wc-compositor: xdg-decoration: client created a decoration object, telling it {mode:?}");
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_pending_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, requested: DecorationMode) {
        let mode = self.decorations.xdg();
        self.negotiated_decoration
            .insert(toplevel.wl_surface().clone(), agreed_xdg(mode));
        println!("wc-compositor: xdg-decoration: client asked for {requested:?}, answering {mode:?}");
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_pending_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        let mode = self.decorations.xdg();
        self.negotiated_decoration
            .insert(toplevel.wl_surface().clone(), agreed_xdg(mode));
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
        // §82: the object starts at the manager's default and only a
        // `request_mode` moves it, so a client that never asks has agreed to
        // the default.
        let agreed = match self.decorations.kde() {
            KdeDefaultMode::Client => Agreed::Client,
            _ => Agreed::Server,
        };
        self.negotiated_decoration.insert(surface.clone(), agreed);
        println!("wc-compositor: kde-decoration: client created a decoration object");
    }

    fn request_mode(
        &mut self,
        surface: &WlSurface,
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
            // §82: what was echoed is what was agreed, and it is what `auto`
            // decides this window's frame from at map. `None` asks for no
            // decoration from either side; it is not a client that is about to
            // draw a bar, so it is not folded.
            let agreed = match mode {
                KdeMode::Client => Agreed::Client,
                _ => Agreed::Server,
            };
            self.negotiated_decoration.insert(surface.clone(), agreed);
        }
    }
}

/// The `xdg-decoration` answer this compositor gave, as an [`Agreed`]. §82.
fn agreed_xdg(mode: DecorationMode) -> Agreed {
    match mode {
        DecorationMode::ClientSide => Agreed::Client,
        _ => Agreed::Server,
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

// ---------------------------------------------------------------------------
// §96: holding the pointer
// ---------------------------------------------------------------------------

/// The constraint this compositor activated, and what it told the platform.
#[derive(Debug)]
struct Held {
    /// The window the platform was told about, so the release names it.
    id: WindowId,
    surface: WlSurface,
    hold: PointerHold,
}

/// `zwp_pointer_constraints_v1`.
///
/// smithay keeps the constraints and sends `locked`/`confined` when told to
/// activate one; **when** is the compositor's policy, and this is it:
///
/// > A constraint is active while its surface is the one under the pointer,
/// > in the window that has the keyboard, and the pointer is inside its
/// > region.
///
/// The keyboard half is what makes this safe on Windows. A game that locks the
/// pointer has it only while it is the window the user is typing into; the
/// moment Windows gives the keyboard to anything else — Alt-Tab, a click on the
/// taskbar, the Start menu, a notification — the lock ends, and a `oneshot`
/// constraint is gone with it. It is also what WLCS asks: a constraint on a
/// surface that has not been selected stays inactive, and one on a surface
/// that loses the keyboard to a new window is released
/// (`PointerConstraints.when_surface_is_unselected_*`).
///
/// The pointer half is the protocol's: "the lock … will be activated once the
/// pointer enters the region". smithay ends a constraint itself when the
/// pointer leaves its surface.
impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        println!(
            "wc-compositor: a client asked to {} the pointer",
            match with_pointer_constraint(surface, &self.pointer, |c| {
                c.map(|c| matches!(*c, PointerConstraint::Locked(_)))
            }) {
                Some(true) => "lock",
                Some(false) => "confine",
                None => "constrain (and smithay refused)",
            }
        );
        self.update_pointer_hold();
    }

    /// Where the client is drawing a pointer while it is locked. Not used: when
    /// the lock ends the system pointer is still where it was locked, which is
    /// where the client last saw it, and moving it to the hint would be moving
    /// the user's pointer on a client's say-so.
    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
    }

    /// The fork's hook (vendor/smithay): the client destroyed the constraint,
    /// which is how GLFW ends a lock — Minecraft opening its pause menu or its
    /// inventory. Without this the platform would go on clipping a pointer the
    /// client has already let go of.
    fn constraint_removed(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        self.update_pointer_hold();
    }
}

delegate_pointer_constraints!(State);
// `zwp_relative_pointer_manager_v1` needs no handler: smithay sends
// `relative_motion` to every relative pointer of the focused client when
// `PointerHandle::relative_motion` is called, which `pointer_motion_by` and
// `locked_motion` do.
delegate_relative_pointer!(State);

impl State {
    /// §96. The window holding the pointer, and how: what the platform was last
    /// told. Read by the synthetic Win32 in `crate::control`, which has to clip
    /// its own pointer the way `ClipCursor` clips the real one.
    #[cfg(unix)]
    pub fn held_pointer(&self) -> Option<(WindowId, PointerHold)> {
        self.held.as_ref().map(|h| (h.id, h.hold))
    }

    /// Bring the active constraint, and the platform, into line with the
    /// policy on [`PointerConstraintsHandler for State`](State). Called after
    /// anything that can change the answer — the pointer moving, the keyboard
    /// moving, a constraint appearing or going away, a commit of the held
    /// surface — and cheap when nothing changed, because the platform is told
    /// only about a difference.
    fn update_pointer_hold(&mut self) {
        let pointer = self.pointer.clone();
        let keyboard_window = self.keyboard_focus.map(|k| self.owning_toplevel(k));
        // The surface that may hold the pointer: smithay's pointer focus, if
        // its window has the keyboard.
        let candidate = pointer.current_focus().filter(|s| s.is_alive()).and_then(|s| {
            let id = self.windows.get(&root_of(&s)).map(|w| w.id)?;
            (Some(self.owning_toplevel(id)) == keyboard_window).then_some((id, s))
        });

        // 1. Let go of the one that is held, if it should not be any more —
        //    or if it already isn't (smithay ends it on a pointer leave, and a
        //    client can destroy it).
        if let Some(held) = self.held.take() {
            let still = held.surface.is_alive()
                && with_pointer_constraint(&held.surface, &pointer, |c| {
                    c.is_some_and(|c| c.is_active())
                });
            let keep = still && candidate.as_ref().is_some_and(|(_, s)| *s == held.surface);
            if keep {
                self.held = Some(held);
            } else {
                if still {
                    with_pointer_constraint(&held.surface, &pointer, |c| {
                        if let Some(c) = c {
                            c.deactivate();
                        }
                    });
                }
                println!("wc-compositor: window {} let go of the pointer", held.id);
                self.ui.send(ToUi::PointerHold { id: held.id, hold: None });
            }
        }

        // 2. Take hold for the candidate, if it has a constraint waiting and
        //    the pointer is inside its region.
        if self.held.is_some() {
            return;
        }
        let Some((id, surface)) = candidate else { return };
        let (x, y) = self.pointer_at;
        let Some((under, origin)) = self.surface_under(id, x, y) else { return };
        if under != surface {
            return;
        }
        let Some(size) = self.staging.get(&surface).map(|s| (s.width(), s.height())) else {
            return;
        };
        let local = (x - origin.0, y - origin.1);
        let hold = with_pointer_constraint(&surface, &pointer, |c| {
            let c = c?;
            if c.is_active() {
                return None;
            }
            if !accepts_input_at(size, c.region(), local) {
                return None;
            }
            let hold = match &*c {
                PointerConstraint::Locked(_) => PointerHold::Locked,
                PointerConstraint::Confined(_) => {
                    let r = crate::hold::region_bounds(size, c.region())?;
                    // Into the window's coordinates, which are the
                    // platform's.
                    PointerHold::Confined(Rect::new(
                        r.x + origin.0.round() as i32,
                        r.y + origin.1.round() as i32,
                        r.w,
                        r.h,
                    ))
                }
            };
            c.activate();
            Some(hold)
        });
        let Some(hold) = hold else { return };
        println!("wc-compositor: window {id} holds the pointer: {hold:?}");
        self.ui.send(ToUi::PointerHold { id, hold: Some(hold) });
        self.held = Some(Held { id, surface, hold });
    }
}

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
// §102. Dispatch only; the globals exist with `--nested-compositor-globals`.
delegate_viewporter!(State);
delegate_single_pixel_buffer!(State);
delegate_presentation!(State);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// ADDED FOR STAGE 38, for [`State::constrain`] — the placement arithmetic, in
/// isolation.
///
/// A `PositionerState` is a plain struct of public fields, so the whole of
/// `xdg_positioner` can be exercised here without a client, a socket or a
/// window. What is being tested is **which** of smithay's two functions is
/// called and with what rectangle; the adjustment rules inside
/// `get_unconstrained_geometry` are smithay's and are tested there.
///
/// The negative cases are the point. A constraint that fires when it should not
/// is the same bug as one that never fires, wearing different clothes: it moves
/// a menu the client placed deliberately, and it does it in the common case
/// where nobody is looking for it.
#[cfg(test)]
mod placement_tests {
    use super::*;
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_positioner::{
        Anchor, ConstraintAdjustment, Gravity,
    };

    /// A menu of `w`x`h` hung off the bottom right corner of a 10x10 anchor at
    /// `(x, y)` — the shape of a context menu, and the one that runs off the
    /// right and the bottom of a screen.
    fn menu(x: i32, y: i32, w: i32, h: i32, adjust: ConstraintAdjustment) -> PositionerState {
        PositionerState {
            rect_size: Size::from((w, h)),
            anchor_rect: Rectangle::new(Point::from((x, y)), Size::from((10, 10))),
            anchor_edges: Anchor::BottomRight,
            gravity: Gravity::BottomRight,
            constraint_adjustment: adjust,
            ..Default::default()
        }
    }

    /// The 800x600 work area of a window whose own origin is the screen's.
    const SCREEN: Rect = Rect { x: 0, y: 0, w: 800, h: 600 };

    #[test]
    fn a_menu_that_fits_is_placed_exactly_where_the_positioner_asked() {
        let p = menu(100, 100, 60, 40, ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
        let got = State::constrain(p, Some(SCREEN));
        assert_eq!((got.loc.x, got.loc.y), (110, 110));
        assert_eq!((got.size.w, got.size.h), (60, 40));
        assert_eq!(got, p.get_geometry(), "an unconstrained menu was moved anyway");
    }

    #[test]
    fn a_menu_off_the_right_edge_slides_back_on() {
        // Anchored at 760: the raw arithmetic puts it at 770 and 30 pixels of
        // it are past 800.
        let p = menu(760, 100, 60, 40, ConstraintAdjustment::SlideX);
        assert_eq!(p.get_geometry().loc.x, 770);
        let got = State::constrain(p, Some(SCREEN));
        assert_eq!(got.loc.x, 740, "the menu did not slide back onto the screen");
        assert_eq!(got.loc.y, 110, "an x constraint moved it in y");
        assert_eq!((got.size.w, got.size.h), (60, 40), "a slide resized it");
    }

    #[test]
    fn a_menu_off_the_bottom_edge_slides_up() {
        let p = menu(100, 580, 60, 40, ConstraintAdjustment::SlideY);
        assert_eq!(p.get_geometry().loc.y, 590);
        let got = State::constrain(p, Some(SCREEN));
        assert_eq!((got.loc.x, got.loc.y), (110, 560));
    }

    /// The client's declaration is the whole of the protocol's contract here.
    /// A menu that asked to be slid and was flipped instead is on screen and
    /// in the wrong place — under the cursor that opened it, typically.
    #[test]
    fn a_client_that_asked_only_to_slide_is_not_flipped() {
        let slide = State::constrain(menu(760, 100, 60, 40, ConstraintAdjustment::SlideX), Some(SCREEN));
        let flip = State::constrain(menu(760, 100, 60, 40, ConstraintAdjustment::FlipX), Some(SCREEN));
        assert_eq!(slide.loc.x, 740);
        assert_eq!(flip.loc.x, 700, "a flip should mirror the anchor, not slide");
        assert_ne!(slide.loc.x, flip.loc.x);
    }

    /// `ConstraintAdjustment::None` is a client saying "leave it where I put
    /// it", and it is entitled to that even when the result is off screen.
    #[test]
    fn a_client_that_declared_nothing_is_left_alone() {
        let p = menu(760, 100, 60, 40, ConstraintAdjustment::None);
        assert_eq!(State::constrain(p, Some(SCREEN)), p.get_geometry());
    }

    #[test]
    fn without_bounds_the_answer_is_the_raw_arithmetic() {
        let p = menu(760, 100, 60, 40, ConstraintAdjustment::SlideX);
        assert_eq!(State::constrain(p, None), p.get_geometry());
    }

    /// A window whose monitor answered with nothing measurable. Fitting a menu
    /// into it would produce a menu of no size at all.
    #[test]
    fn a_degenerate_work_area_is_ignored_rather_than_fitted_into() {
        let p = menu(760, 100, 60, 40, ConstraintAdjustment::SlideX);
        let empty = Rect { x: 0, y: 0, w: 0, h: 0 };
        assert_eq!(State::constrain(p, Some(empty)), p.get_geometry());
    }

    /// The work area is in the **parent's** coordinates, so a window at (300,
    /// 200) on the desktop is told the screen starts at (-300, -200). Getting
    /// that sign wrong is the failure mode that looks right on a maximised
    /// window and is wrong everywhere else.
    #[test]
    fn the_work_area_is_relative_to_the_parent_window() {
        let bounds = Rect { x: -300, y: -200, w: 800, h: 600 };
        // 40 pixels from the right-hand edge of the screen, in the window's
        // coordinates: -300 + 800 = 500.
        let p = menu(450, 100, 60, 40, ConstraintAdjustment::SlideX);
        assert_eq!(p.get_geometry().loc.x, 460);
        let got = State::constrain(p, Some(bounds));
        assert_eq!(got.loc.x, 440, "the screen's right edge is at 500 in these coordinates");
    }
}

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

// ---------------------------------------------------------------------------
// §93: a focus that only moved to a popup of the same window is not a loss
// ---------------------------------------------------------------------------

#[cfg(test)]
mod held_loss {
    use super::{HeldLoss, Regained, FOCUS_MOVE_GRACE};
    use std::time::{Duration, Instant};

    /// Wine's order for a click on a menu: `WM_KILLFOCUS` to the window, then
    /// `WM_SETFOCUS` to its popup. The client must hear nothing.
    #[test]
    fn a_loss_taken_back_by_the_same_window_is_dropped() {
        let t = Instant::now();
        let mut h = HeldLoss::default();
        h.hold(1, t);
        assert_eq!(h.held(), Some(1));
        assert_eq!(h.regained(true), Regained::Cancelled);
        assert_eq!(h.held(), None);
        assert_eq!(h.due(t + FOCUS_MOVE_GRACE * 4), None, "nothing is left to settle");
    }

    /// Alt-Tab to another of our windows: the old one's loss is real and is
    /// told before the new one is entered.
    #[test]
    fn a_loss_followed_by_another_window_is_settled_first() {
        let t = Instant::now();
        let mut h = HeldLoss::default();
        h.hold(1, t);
        assert_eq!(h.regained(false), Regained::Settle(1));
        assert_eq!(h.held(), None);
    }

    #[test]
    fn a_gain_with_nothing_held_is_just_a_gain() {
        let mut h = HeldLoss::default();
        assert_eq!(h.regained(true), Regained::Nothing);
        assert_eq!(h.regained(false), Regained::Nothing);
    }

    /// A click in another application: nothing of ours gains, and the loss
    /// is told once the grace has passed — not before, and only once.
    #[test]
    fn a_loss_nobody_takes_back_is_due_after_the_grace() {
        let t = Instant::now();
        let mut h = HeldLoss::default();
        h.hold(7, t);
        assert_eq!(h.due(t), None);
        assert_eq!(h.due(t + FOCUS_MOVE_GRACE - Duration::from_millis(1)), None);
        assert!(h.is(7));
        assert_eq!(h.due(t + FOCUS_MOVE_GRACE), Some(7));
        assert_eq!(h.due(t + FOCUS_MOVE_GRACE * 2), None);
        assert!(!h.is(7));
    }

    /// The grace is short enough that a person never sees a stale
    /// `activated` state, and longer than the compositor's 5 ms turn.
    #[test]
    fn the_grace_is_a_few_turns() {
        assert!(FOCUS_MOVE_GRACE >= Duration::from_millis(10));
        assert!(FOCUS_MOVE_GRACE <= Duration::from_millis(100));
    }
}
