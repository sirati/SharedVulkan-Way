//! §103.7. The AppKit platform: each toplevel an `NSWindow`, each popup a
//! borderless child window, AppKit's main thread as the UI pump.
//!
//! # The two threads, on macOS
//!
//! The split is the one `platform/mod.rs` describes for Win32 and it holds for
//! the same reasons: smithay's state lives on the spawned Wayland thread, and
//! AppKit insists on the process's first thread. The wakeup is
//! `dispatch_async_f` onto the main queue, which AppKit's run loop services in
//! its common modes, so it keeps firing during a live resize and while a menu
//! tracks: the macOS twin of `PostMessage` to a hidden window, and for the same
//! reason (a thread message would stall exactly when the user grabs the frame).
//!
//! # Re-entrancy, which is this file's one structural rule
//!
//! AppKit calls delegates synchronously from inside the calls that cause them:
//! `setContentSize:` runs `windowDidResize:` before it returns. So the window
//! map ([`STATE`]) is borrowed only by [`drain`], and no AppKit callback ever
//! touches it. Callbacks reach what they need through their own [`WinShared`],
//! which lives in the view's and the delegate's instance variables, and through
//! [`INPUT`], which [`drain`] borrows only around calls that cannot call back.
//!
//! # Coordinates
//!
//! The content view is flipped, so a point in it is a point in the surface's
//! own coordinates with the origin at the top left. One surface pixel is one
//! point: the compositor supports buffer scale 1 only (`state.rs`), so on a
//! Retina screen AppKit scales the layer up, which is correct and soft
//! (§103.7 records the scale work as later).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSApplicationDelegate, NSApplicationTerminateReply,
    NSBackingStoreType, NSCursor, NSEvent, NSEventModifierFlags, NSImage, NSMenu, NSMenuItem,
    NSPasteboard, NSResponder, NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
    NSWindowButton, NSWindowCollectionBehavior, NSWindowDelegate, NSWindowOrderingMode,
    NSWindowStyleMask, NSWindowTitleVisibility,
};
use objc2_foundation::{NSData, NSNotification, NSPoint, NSRect, NSSize, NSString, NSTimer};

use super::mac_keys::{self, CmdKey, KeyFold};
use super::{assemble, CursorSpec, Edge, FromUi, IconImage, PlatformConfig, PopupBounds, Started, ToUi, WindowId, WindowKind};
use crate::surface::{PixelFormat, Rect, Shadow};

// ------------------------------------------------------------------ FFI ---

#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}

#[link(name = "System")]
unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(queue: *const DispatchQueue, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

type CFTypeRef = *const c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGDataProviderCreateWithData(
        info: *mut c_void,
        data: *const c_void,
        size: usize,
        release: Option<extern "C" fn(*mut c_void, *const c_void, usize)>,
    ) -> *mut c_void;
    fn CGDataProviderRelease(provider: *mut c_void);
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        bitmap_info: u32,
        provider: *mut c_void,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> *mut c_void;
    fn CGImageRelease(image: *mut c_void);
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    fn CGWarpMouseCursorPosition(point: NSPoint) -> i32;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> CFTypeRef;
    fn TISGetInputSourceProperty(source: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    static kTISPropertyInputSourceID: CFTypeRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(kind: CFTypeRef, level: u32, name: CFTypeRef, id: *mut u32) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

/// A `CGImageRef` as an Objective-C argument: objc2's debug builds check
/// argument encodings, and `initWithCGImage:size:` declares `^{CGImage=}`.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct CGImageRef(*mut c_void);

// SAFETY: a pointer to the opaque CGImage struct, which is what the encoding says.
unsafe impl objc2::encode::Encode for CGImageRef {
    const ENCODING: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("CGImage", &[]));
}
// SAFETY: as above.
unsafe impl objc2::encode::RefEncode for CGImageRef {
    const ENCODING_REF: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&<Self as objc2::encode::Encode>::ENCODING);
}

/// `kCGBitmapByteOrder32Little`: the shadow is BGRA in memory, which is a
/// little-endian 32-bit ARGB word.
const BYTE_ORDER_32_LITTLE: u32 = 2 << 12;
const ALPHA_PREMULTIPLIED_FIRST: u32 = 2;
const ALPHA_NONE_SKIP_FIRST: u32 = 6;

// ------------------------------------------------------------- the Dock ---

/// One entry of the application's Dock menu (§103.8): the macOS form of a
/// jump-list task. `key` is what the launcher gets back when it is picked.
#[derive(Clone, Debug)]
pub struct DockItem {
    pub title: String,
    pub key: String,
}

type DockPick = Box<dyn Fn(&str) + Send + Sync>;

static DOCK: Mutex<Option<(Vec<DockItem>, DockPick)>> = Mutex::new(None);

/// Set the Dock menu and what a pick (or a click on the tile while no window
/// is open, with the key `""`) does. The launcher calls this; the compositor
/// itself has no idea what an application's actions are.
pub fn set_dock_menu(items: Vec<DockItem>, on_pick: DockPick) {
    *DOCK.lock().unwrap_or_else(|e| e.into_inner()) = Some((items, on_pick));
}

fn dock_pick(key: &str) {
    if let Some((_, pick)) = DOCK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        pick(key);
    }
}

// ------------------------------------------------------------ face mode ---

/// §103.13: this process is an application's face, not a whole compositor.
static FACE: AtomicBool = AtomicBool::new(false);
/// The window's IOSurface pair from the main process, the one to show, and
/// whether it carries alpha.
type FaceSurfaces = HashMap<WindowId, ([super::macmach::Surface; 2], u8, bool)>;
static FACE_SURFACES: Mutex<Option<FaceSurfaces>> = Mutex::new(None);
type BoundsSink = Box<dyn Fn(WindowId, Option<Rect>) + Send + Sync>;
static BOUNDS_SINK: Mutex<Option<BoundsSink>> = Mutex::new(None);
/// Set when the user quit the face: it leaves as soon as its windows are gone.
static FACE_QUITTING: AtomicBool = AtomicBool::new(false);
type FaceExit = Box<dyn Fn() + Send + Sync>;
static FACE_EXIT: Mutex<Option<FaceExit>> = Mutex::new(None);

fn face_surfaces<R>(f: impl FnOnce(&mut FaceSurfaces) -> R) -> R {
    let mut g = FACE_SURFACES.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(HashMap::new))
}

/// The first thing a face does, before it has reached the main process: be
/// a regular application, so the activation LaunchServices sends with the
/// launch (a Dock click, `open`) finds one and the face comes to the front.
/// Measured: a face that waited for the main process first stayed behind
/// Finder, and macOS 14's cooperative activation then refused it later.
pub fn face_early() {
    if let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        app.finishLaunching();
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// Run this process as the face of one application (§103.13): the AppKit
/// platform, fed from the main process's socket rather than a Wayland thread
/// in this process. Returns when the main process says goodbye, the socket
/// ends, or the application has had no window for a while.
pub fn run_face(stream: std::os::unix::net::UnixStream, section: &str, ids: Vec<String>, dock: Vec<DockItem>) -> i32 {
    use super::faceproto::{read_frame, write_frame, FromFace, ToFace};
    let Some(mtm) = MainThreadMarker::new() else { return 2 };
    FACE.store(true, Ordering::Relaxed);
    let token: u64 = (std::process::id() as u64) << 32 ^ std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(7, |d| d.as_nanos() as u64);
    let Some(port) = super::macmach::receive_port() else { return 2 };
    match super::macmach::look_up(super::macos_router::SERVICE) {
        Ok(service) => {
            let kr = super::macmach::send_hello(service, port, token);
            super::macmach::release(service);
            if kr != 0 {
                eprintln!("wc-launch: face: the main process's mach service refused the hello ({kr:#x})");
                return 1;
            }
        }
        Err(kr) => {
            eprintln!("wc-launch: face: no main process service ({kr})");
            return 1;
        }
    }
    let Ok(writer) = stream.try_clone() else { return 1 };
    let (out_tx, out_rx) = std::sync::mpsc::channel::<FromFace>();
    let _ = out_tx.send(FromFace::Hello { section: section.to_string(), ids, token, pid: std::process::id() });
    std::thread::spawn(move || {
        let mut w = std::io::BufWriter::new(writer);
        while let Ok(m) = out_rx.recv() {
            let bye = matches!(m, FromFace::Bye);
            if let Some(b) = m.encode() {
                if write_frame(&mut w, &b).is_err() {
                    break;
                }
                // Flush everything queued now, then wait again: input stays
                // in order and is never held back behind a buffer.
                use std::io::Write;
                if w.flush().is_err() || bye {
                    break;
                }
            }
        }
    });
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<FromUi>();
    {
        let out = out_tx.clone();
        std::thread::spawn(move || {
            while let Ok(e) = ev_rx.recv() {
                if out.send(FromFace::Ui(e)).is_err() {
                    break;
                }
            }
        });
    }
    {
        let out = Mutex::new(out_tx.clone());
        *BOUNDS_SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move |id, rect| {
            let _ = out.lock().unwrap_or_else(|e| e.into_inner()).send(FromFace::Bounds { id, rect });
        }));
        let out = Mutex::new(out_tx.clone());
        set_dock_menu(
            dock,
            Box::new(move |key: &str| {
                let _ = out.lock().unwrap_or_else(|e| e.into_inner()).send(FromFace::Request { key: key.to_string() });
            }),
        );
        let out = Mutex::new(out_tx.clone());
        *FACE_EXIT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move || {
            let _ = out.lock().unwrap_or_else(|e| e.into_inner()).send(FromFace::Bye);
        }));
    }
    // Start the application now that the main process knows this face.
    let _ = out_tx.send(FromFace::Request { key: String::new() });

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<ToUi>();
    std::thread::spawn(move || {
        let mut r = std::io::BufReader::new(stream);
        // Surface pairs that arrived by mach before the message naming them.
        let mut early: HashMap<(WindowId, u32), [super::macmach::Surface; 2]> = HashMap::new();
        while let Ok(Some(body)) = read_frame(&mut r) {
            let Ok(m) = ToFace::decode(&body) else { break };
            let cmd = match m {
                ToFace::Bye => break,
                ToFace::Surface { id, generation, alpha, .. } => {
                    let mut pair = early.remove(&(id, generation));
                    let t0 = std::time::Instant::now();
                    while pair.is_none() && t0.elapsed() < std::time::Duration::from_secs(2) {
                        match super::macmach::recv_surfaces(port, 500) {
                            Some((i, g, p)) if (i, g) == (id, generation) => pair = Some(p),
                            Some((i, g, p)) => {
                                early.insert((i, g), p);
                            }
                            None => {}
                        }
                    }
                    match pair {
                        Some(p) => face_surfaces(|m| {
                            m.insert(id, (p, 0, alpha));
                        }),
                        None => eprintln!("wc-launch: face: window {id}'s surfaces never arrived"),
                    }
                    continue;
                }
                ToFace::Present { id, buffer } => {
                    face_surfaces(|m| {
                        if let Some(e) = m.get_mut(&id) {
                            e.1 = buffer.min(1);
                        }
                    });
                    ToUi::Present { id, damage: Vec::new() }
                }
                ToFace::Create { id, title, app_id, popup } => ToUi::Create {
                    id,
                    title,
                    app_id,
                    aumid: String::new(),
                    shadow: Arc::new(Mutex::new(Shadow::empty())),
                    kind: match popup {
                        Some((parent, x, y)) => WindowKind::Popup { parent, x, y },
                        None => WindowKind::Toplevel,
                    },
                    bounds: Arc::new(Mutex::new(None)),
                },
                ToFace::Title { id, title } => ToUi::Title { id, title },
                ToFace::AppId { id, app_id } => ToUi::AppId { id, app_id, aumid: String::new() },
                ToFace::Icon { id, icons } => ToUi::Icon { id, icons },
                ToFace::MovePopup { id, x, y, w, h } => ToUi::MovePopup { id, x, y, width: w, height: h },
                ToFace::ShiftOrigin { id, dx, dy } => ToUi::ShiftOrigin { id, dx, dy },
                ToFace::Map { id, w, h, fold } => ToUi::Map { id, width: w, height: h, fold_caption: fold },
                ToFace::Resize { id, w, h } => ToUi::Resize { id, width: w, height: h },
                ToFace::SizeHints { id, min, max } => ToUi::SizeHints { id, min, max },
                ToFace::SetWindowState { id, maximized, minimized } => ToUi::SetWindowState { id, maximized, minimized },
                ToFace::Fullscreen { id, on } => ToUi::Fullscreen { id, on },
                ToFace::Cursor { spec } => ToUi::Cursor { spec },
                ToFace::PointerHold { id, hold } => ToUi::PointerHold { id, hold },
                ToFace::BeginMove { id } => ToUi::BeginMove { id },
                ToFace::BeginResize { id, edge } => ToUi::BeginResize { id, edge },
                ToFace::WindowMenu { id, x, y } => ToUi::WindowMenu { id, x, y },
                ToFace::Destroy { id } => {
                    face_surfaces(|m| {
                        m.remove(&id);
                    });
                    ToUi::Destroy { id }
                }
                ToFace::Activate { id } => ToUi::Activate { id },
            };
            if cmd_tx.send(cmd).is_err() {
                return;
            }
            Wake.wake();
        }
        // The main process has gone or said goodbye: the face goes with it.
        let _ = cmd_tx.send(ToUi::Quit { clients: 0 });
        Wake.wake();
    });

    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    PumpInner::AppKit { commands: cmd_rx, events: ev_tx }.run();
    if let Some(f) = FACE_EXIT.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        f();
    }
    // Let the writer send the goodbye.
    std::thread::sleep(std::time::Duration::from_millis(100));
    0
}

/// The face's lifetime rule, checked on the timer: a face whose windows are
/// all gone leaves (at once after a Quit, after a linger otherwise), and so
/// does one whose application never showed a window.
fn face_tick() {
    use std::sync::atomic::AtomicU64;
    static STARTED: AtomicU64 = AtomicU64::new(0);
    static EMPTY_SINCE: AtomicU64 = AtomicU64::new(0);
    static HAD: AtomicBool = AtomicBool::new(false);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    if STARTED.load(Ordering::Relaxed) == 0 {
        STARTED.store(now, Ordering::Relaxed);
    }
    let n = STATE.with(|s| s.borrow().as_ref().map_or(0, |st| st.windows.len()));
    if n > 0 {
        HAD.store(true, Ordering::Relaxed);
        EMPTY_SINCE.store(0, Ordering::Relaxed);
        return;
    }
    if EMPTY_SINCE.load(Ordering::Relaxed) == 0 {
        EMPTY_SINCE.store(now, Ordering::Relaxed);
    }
    let empty = now - EMPTY_SINCE.load(Ordering::Relaxed);
    let leave = if HAD.load(Ordering::Relaxed) {
        FACE_QUITTING.load(Ordering::Relaxed) || empty > 10_000
    } else {
        now - STARTED.load(Ordering::Relaxed) > 120_000
    };
    if leave && let Some(mtm) = MainThreadMarker::new() {
        let app = NSApplication::sharedApplication(mtm);
        app.stop(None);
        post_wakeup_event(&app);
    }
}

// ------------------------------------------------------------- the wake ---

/// The Wayland thread's way to make [`drain`] run on the main thread.
///
/// Coalesced: while a drain is queued and not yet started, further wakes are
/// free. The flag is cleared *before* the drain reads the queue, so a command
/// sent during a drain always gets a drain of its own.
pub(crate) struct Wake;

/// One per process, like the main queue it guards: the trampoline clears it.
static PENDING: AtomicBool = AtomicBool::new(false);

impl Wake {
    pub(crate) fn wake(&self) {
        if !PENDING.swap(true, Ordering::AcqRel) {
            // SAFETY: the main queue is a process-lifetime object, the context
            // is unused, and the function is `extern "C"` with the right shape.
            unsafe { dispatch_async_f(&raw const _dispatch_main_q, std::ptr::null_mut(), drain_trampoline) };
        }
    }
}

extern "C" fn drain_trampoline(_: *mut c_void) {
    PENDING.store(false, Ordering::Release);
    drain();
}

// ------------------------------------------------------------ the state ---

/// What a window's view and delegate share, and the only window state an
/// AppKit callback may touch (see the module header).
struct WinShared {
    id: WindowId,
    events: Sender<FromUi>,
    /// The surface size as last configured, so a `windowDidResize:` caused by
    /// our own `setContentSize:` is not reported back as the user's.
    size: Cell<(i32, i32)>,
    /// Set around programmatic frame changes.
    quiet: Cell<bool>,
    /// The last `mouseDown:`, for `performWindowDragWithEvent:`, which needs
    /// the event that started the drag rather than whatever is current when
    /// the client's `xdg_toplevel.move` arrives.
    last_down: RefCell<Option<Retained<NSEvent>>>,
    /// A client-requested resize in progress: the edge and the frame and
    /// mouse position it started from.
    resizing: Cell<Option<(Edge, NSRect, NSPoint)>>,
    is_popup: bool,
    bounds: PopupBounds,
    /// `zwp_locked_pointer`/`confined_pointer` for this window.
    hold: Cell<Option<crate::hold::PointerHold>>,
    /// AppKit's zoom state as last reported, so a change is said once.
    zoomed: Cell<bool>,
}

impl WinShared {
    fn send(&self, e: FromUi) {
        if self.events.send(e).is_ok() {
            super::wake_wayland();
        }
    }
}

struct MacWin {
    window: Retained<NSWindow>,
    view: Retained<WcView>,
    shared: Rc<WinShared>,
    _delegate: Option<Retained<WcDelegate>>,
    shadow: Arc<Mutex<Shadow>>,
    kind: WindowKind,
    mapped: bool,
    icons: Vec<IconImage>,
    fold: bool,
    /// Maximise, minimise and fullscreen asked for before `Map`. AppKit
    /// ignores `zoom:` on a window that is not on screen, and Firefox asks to
    /// be maximised (its saved state) before its first commit.
    pending: (Option<bool>, Option<bool>, Option<bool>),
}

struct State {
    commands: Receiver<ToUi>,
    windows: HashMap<WindowId, MacWin>,
    made: u64,
    gone: u64,
    clients_at_exit: u32,
    /// `NSPasteboard.changeCount` we last reported or wrote ourselves.
    clip_seen: isize,
    idle_assertion: Option<u32>,
    cascade: NSPoint,
}

/// Seat-wide input state, shared by every view. See the module header for
/// when it may be borrowed.
struct Input {
    cmd: CmdKey,
    fold: KeyFold,
    cursor: Option<Retained<NSCursor>>,
    hidden: bool,
    focus: Option<WindowId>,
    layout: &'static str,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    static INPUT: RefCell<Option<Input>> = const { RefCell::new(None) };
    static EVENTS: RefCell<Option<Sender<FromUi>>> = const { RefCell::new(None) };
    static APP_DELEGATE: RefCell<Option<Retained<WcAppDelegate>>> = const { RefCell::new(None) };
    static TIMER: RefCell<Option<Retained<NSTimer>>> = const { RefCell::new(None) };
}

fn send_global(e: FromUi) {
    EVENTS.with(|s| {
        if let Some(tx) = s.borrow().as_ref()
            && tx.send(e).is_ok()
        {
            super::wake_wayland();
        }
    });
}

fn with_input<R>(f: impl FnOnce(&mut Input) -> R) -> Option<R> {
    INPUT.with(|i| i.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn ms(event: &NSEvent) -> u32 {
    (event.timestamp() * 1000.0) as u64 as u32
}

// --------------------------------------------------------------- classes ---

struct ViewIvars {
    win: Rc<WinShared>,
}

define_class!(
    // SAFETY: NSView may be subclassed; no Drop impl, and every override below
    // has AppKit's signature.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WcSurfaceView"]
    #[ivars = ViewIvars]
    struct WcView;

    impl WcView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        /// The client draws its own title bar when the caption is folded, so
        /// a press there is the client's, not a window drag.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        /// Nothing: the contents are set on present, and AppKit must not
        /// replace them with a drawing of its own.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {}

        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            self.install_tracking();
            // SAFETY: calling the superclass's implementation of the method
            // being overridden.
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, e: &NSEvent) {
            let (x, y) = self.point(e);
            self.ivars().win.send(FromUi::PointerEnter { id: self.id(), x, y, time: ms(e) });
            apply_cursor();
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, e: &NSEvent) {
            self.ivars().win.send(FromUi::PointerLeave { id: self.id(), time: ms(e) });
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            self.motion(e);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, e: &NSEvent) {
            if self.resize_step(e) {
                return;
            }
            self.motion(e);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, e: &NSEvent) {
            self.motion(e);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, e: &NSEvent) {
            self.motion(e);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            *self.ivars().win.last_down.borrow_mut() = Some(e.retain());
            self.button(e, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, e: &NSEvent) {
            if self.ivars().win.resizing.take().is_some() {
                // The client asked for this resize with the button down; it
                // still hears the release, as it would after a native one.
            }
            self.button(e, false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, e: &NSEvent) {
            self.button(e, true);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, e: &NSEvent) {
            self.button(e, false);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, e: &NSEvent) {
            self.button(e, true);
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, e: &NSEvent) {
            self.button(e, false);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, e: &NSEvent) {
            // AppKit's own inertia, after the fingers lift. Linux toolkits run
            // their own kinetic scrolling from the finger events, so passing
            // these through would apply it twice (cocoa-way's finding).
            if e.momentumPhase().0 != 0 {
                return;
            }
            let (h, v, h120, v120) =
                mac_keys::scroll_axes(e.hasPreciseScrollingDeltas(), e.scrollingDeltaX(), e.scrollingDeltaY());
            if h == 0.0 && v == 0.0 {
                return;
            }
            self.ivars().win.send(FromUi::PointerAxis {
                id: self.id(),
                horizontal: h,
                vertical: v,
                v120_h: h120,
                v120_v: v120,
                time: ms(e),
            });
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            if e.isARepeat() {
                return; // Wayland clients repeat from repeat_info
            }
            key_event(self.focus_target(), e.keyCode(), true, ms(e));
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, e: &NSEvent) {
            key_event(self.focus_target(), e.keyCode(), false, ms(e));
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, e: &NSEvent) {
            let kvk = e.keyCode();
            if let Some(down) = mac_keys::modifier_down(kvk, e.modifierFlags().0 as u64) {
                key_event(self.focus_target(), kvk, down, ms(e));
            }
        }
    }
);

impl WcView {
    fn new(mtm: MainThreadMarker, frame: NSRect, win: Rc<WinShared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { win });
        // SAFETY: NSView's designated initialiser.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn id(&self) -> WindowId {
        self.ivars().win.id
    }

    /// Keys go to the window that has keyboard focus as far as AppKit is
    /// concerned, which for a popup's events is still its toplevel: a
    /// borderless child window never becomes key.
    fn focus_target(&self) -> WindowId {
        with_input(|i| i.focus).flatten().unwrap_or_else(|| self.id())
    }

    fn point(&self, e: &NSEvent) -> (f64, f64) {
        let p = self.convertPoint_fromView(e.locationInWindow(), None);
        (p.x, p.y)
    }

    fn install_tracking(&self) {
        let mtm = self.mtm();
        for area in self.trackingAreas().iter() {
            self.removeTrackingArea(&area);
        }
        let opts = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::InVisibleRect
            | NSTrackingAreaOptions::EnabledDuringMouseDrag;
        // SAFETY: the owner is this view, which outlives the area it holds.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                self.bounds(),
                opts,
                Some(self),
                None,
            )
        };
        let _ = mtm;
        self.addTrackingArea(&area);
    }

    fn motion(&self, e: &NSEvent) {
        let win = &self.ivars().win;
        let t = ms(e);
        match win.hold.get() {
            Some(crate::hold::PointerHold::Locked) => {
                win.send(FromUi::PointerRelative { id: self.id(), dx: e.deltaX(), dy: e.deltaY(), time: t });
                return;
            }
            Some(crate::hold::PointerHold::Confined(r)) => {
                win.send(FromUi::PointerRelative { id: self.id(), dx: e.deltaX(), dy: e.deltaY(), time: t });
                let (x, y) = self.point(e);
                let cx = x.clamp(f64::from(r.x), f64::from(r.x + r.w - 1));
                let cy = y.clamp(f64::from(r.y), f64::from(r.y + r.h - 1));
                if (cx, cy) != (x, y) {
                    self.warp_to(cx, cy);
                }
                win.send(FromUi::PointerMotion { id: self.id(), x: cx, y: cy, time: t });
                return;
            }
            None => {}
        }
        win.send(FromUi::PointerRelative { id: self.id(), dx: e.deltaX(), dy: e.deltaY(), time: t });
        let (x, y) = self.point(e);
        win.send(FromUi::PointerMotion { id: self.id(), x, y, time: t });
        apply_cursor();
    }

    /// Move the system pointer to a point in this view. `CGWarpMouseCursorPosition`
    /// takes global display coordinates, which have their origin at the top
    /// left of the primary screen.
    fn warp_to(&self, x: f64, y: f64) {
        let Some(window) = self.window() else { return };
        let in_window = self.convertPoint_toView(NSPoint::new(x, y), None);
        let on_screen = window.convertPointToScreen(in_window);
        let mtm = self.mtm();
        let primary_h = NSScreen::screens(mtm).firstObject().map_or(0.0, |s| s.frame().size.height);
        // SAFETY: a plain value call.
        unsafe {
            CGWarpMouseCursorPosition(NSPoint::new(on_screen.x, primary_h - on_screen.y));
            CGAssociateMouseAndMouseCursorPosition(1);
        }
    }

    fn button(&self, e: &NSEvent, pressed: bool) {
        let button = mac_keys::evdev_button(e.buttonNumber() as i64);
        // A press with nothing having announced the pointer (a click straight
        // after activation) still needs a position the client agrees with.
        let (x, y) = self.point(e);
        let win = &self.ivars().win;
        win.send(FromUi::PointerMotion { id: self.id(), x, y, time: ms(e) });
        win.send(FromUi::PointerButton { id: self.id(), button, pressed, time: ms(e) });
    }

    /// One step of a client-requested edge resize (`xdg_toplevel.resize`).
    /// AppKit has no call that starts a native resize from an edge, so the
    /// drag is followed here.
    fn resize_step(&self, e: &NSEvent) -> bool {
        let win = &self.ivars().win;
        let Some((edge, start, mouse0)) = win.resizing.get() else { return false };
        let Some(window) = self.window() else { return false };
        let now = NSEvent::mouseLocation();
        let (dx, dy) = (now.x - mouse0.x, now.y - mouse0.y);
        let mut f = start;
        let (left, right, top, bottom) = match edge {
            Edge::Left => (true, false, false, false),
            Edge::Right => (false, true, false, false),
            Edge::Top => (false, false, true, false),
            Edge::Bottom => (false, false, false, true),
            Edge::TopLeft => (true, false, true, false),
            Edge::TopRight => (false, true, true, false),
            Edge::BottomLeft => (true, false, false, true),
            Edge::BottomRight => (false, true, false, true),
            Edge::None => return false,
        };
        let min = window.contentMinSize();
        if right {
            f.size.width = (start.size.width + dx).max(min.width.max(50.0));
        }
        if left {
            f.size.width = (start.size.width - dx).max(min.width.max(50.0));
            f.origin.x = start.origin.x + start.size.width - f.size.width;
        }
        if top {
            // AppKit's y grows upward: dragging the top edge up grows the window.
            f.size.height = (start.size.height + dy).max(min.height.max(50.0));
        }
        if bottom {
            f.size.height = (start.size.height - dy).max(min.height.max(50.0));
            f.origin.y = start.origin.y + start.size.height - f.size.height;
        }
        window.setFrame_display(f, true);
        let _ = e;
        true
    }
}

define_class!(
    // SAFETY: NSObject may be subclassed; the protocol methods below have
    // NSWindowDelegate's signatures.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WcWindowDelegate"]
    #[ivars = ViewIvars]
    struct WcDelegate;

    unsafe impl NSObjectProtocol for WcDelegate {}

    unsafe impl NSWindowDelegate for WcDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            // The client decides; the window goes when it destroys its toplevel.
            let w = &self.ivars().win;
            w.send(FromUi::CloseRequested { id: w.id });
            false
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, n: &NSNotification) {
            let w = &self.ivars().win;
            let Some(window) = notification_window(n) else { return };
            publish_bounds(&window, w);
            if w.quiet.get() {
                return;
            }
            // Win32 says SIZE_MAXIMIZED in WM_SIZE; AppKit's equivalent is the
            // zoom state, read here because zooming is a resize.
            let zoomed = window.isZoomed();
            if zoomed != w.zoomed.replace(zoomed) {
                w.send(FromUi::Maximized { id: w.id, on: zoomed });
            }
            let content = window.contentRectForFrameRect(window.frame());
            let size = (content.size.width.round() as i32, content.size.height.round() as i32);
            if size != w.size.get() && size.0 > 0 && size.1 > 0 {
                w.size.set(size);
                w.send(FromUi::Resized { id: w.id, width: size.0, height: size.1 });
            }
        }

        #[unsafe(method(windowDidMove:))]
        fn window_did_move(&self, n: &NSNotification) {
            if let Some(window) = notification_window(n) {
                publish_bounds(&window, &self.ivars().win);
            }
        }

        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            let w = &self.ivars().win;
            if w.is_popup {
                return;
            }
            let caps = NSEvent::modifierFlags_class().contains(NSEventModifierFlags::CapsLock);
            with_input(|i| i.focus = Some(w.id));
            w.send(FromUi::Activated { id: w.id, on: true });
            w.send(FromUi::KeyboardFocus { id: w.id, on: true, caps, num: false });
            check_layout();
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _n: &NSNotification) {
            let w = &self.ivars().win;
            if w.is_popup {
                return;
            }
            // Release exactly what is held, so a Cmd+Tab away does not leave
            // Control stuck down in the client.
            let held = with_input(|i| {
                if i.focus == Some(w.id) {
                    i.focus = None;
                }
                i.fold.release_all()
            })
            .unwrap_or_default();
            for keycode in held {
                w.send(FromUi::Key { id: w.id, keycode, pressed: false, time: 0 });
            }
            w.send(FromUi::KeyboardFocus { id: w.id, on: false, caps: false, num: false });
            w.send(FromUi::Activated { id: w.id, on: false });
        }

        #[unsafe(method(windowDidMiniaturize:))]
        fn window_did_miniaturize(&self, _n: &NSNotification) {
            let w = &self.ivars().win;
            w.send(FromUi::Minimized { id: w.id, on: true });
        }

        #[unsafe(method(windowDidDeminiaturize:))]
        fn window_did_deminiaturize(&self, _n: &NSNotification) {
            let w = &self.ivars().win;
            w.send(FromUi::Minimized { id: w.id, on: false });
        }

        #[unsafe(method(windowDidEnterFullScreen:))]
        fn window_did_enter_full_screen(&self, _n: &NSNotification) {
            let w = &self.ivars().win;
            w.send(FromUi::Maximized { id: w.id, on: true });
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn window_did_exit_full_screen(&self, _n: &NSNotification) {
            let w = &self.ivars().win;
            w.send(FromUi::Maximized { id: w.id, on: false });
        }
    }
);

impl WcDelegate {
    fn new(mtm: MainThreadMarker, win: Rc<WinShared>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars { win });
        // SAFETY: NSObject's initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// The Dock menu, outside the class macro, which cannot take an early return.
fn dock_menu(mtm: MainThreadMarker, target: &WcAppDelegate, items: &[DockItem]) -> Option<Retained<NSMenu>> {
    if items.is_empty() {
        return None;
    }
    let menu = NSMenu::new(mtm);
    for (tag, item) in items.iter().enumerate() {
        // SAFETY: a plain menu item whose action is the delegate's `dockPick:`.
        let mi = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(&item.title),
                Some(sel!(dockPick:)),
                &NSString::from_str(""),
            )
        };
        // SAFETY: the target is the app delegate, retained for the process's
        // life in APP_DELEGATE.
        unsafe { mi.setTarget(Some(target)) };
        mi.setTag(tag as isize);
        menu.addItem(&mi);
    }
    Some(menu)
}

fn notification_window(n: &NSNotification) -> Option<Retained<NSWindow>> {
    let obj = n.object()?;
    obj.downcast::<NSWindow>().ok()
}

/// §38's popup bounds: the work area, in the toplevel's own coordinates.
fn publish_bounds(window: &NSWindow, w: &WinShared) {
    if w.is_popup {
        return;
    }
    let Some(screen) = window.screen() else { return };
    let v = screen.visibleFrame();
    let c = window.contentRectForFrameRect(window.frame());
    let rect = mac_keys::bounds_in_parent(
        (v.origin.x, v.origin.y, v.size.width, v.size.height),
        (c.origin.x, c.origin.y, c.size.width, c.size.height),
    );
    let mut cell = w.bounds.lock().unwrap_or_else(|e| e.into_inner());
    if *cell != Some(rect) {
        *cell = Some(rect);
        if let Some(sink) = BOUNDS_SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            sink(w.id, Some(rect));
        }
    }
}

struct AppIvars {
    events: Sender<FromUi>,
}

define_class!(
    // SAFETY: as above; NSApplicationDelegate's signatures.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WcAppDelegate"]
    #[ivars = AppIvars]
    struct WcAppDelegate;

    unsafe impl NSObjectProtocol for WcAppDelegate {}

    unsafe impl NSApplicationDelegate for WcAppDelegate {
        /// Cmd+Q and the Dock's Quit ask every window to close, and the
        /// compositor stays. cocoa-way quits the whole compositor here, which
        /// takes every client with it.
        #[unsafe(method(applicationShouldTerminate:))]
        fn application_should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            FACE_QUITTING.store(true, Ordering::Relaxed);
            let ids: Vec<WindowId> = STATE.with(|s| {
                s.try_borrow()
                    .ok()
                    .and_then(|g| {
                        g.as_ref().map(|st| {
                            st.windows
                                .iter()
                                .filter(|(_, w)| w.kind == WindowKind::Toplevel)
                                .map(|(id, _)| *id)
                                .collect()
                        })
                    })
                    .unwrap_or_default()
            });
            for id in ids {
                let _ = self.ivars().events.send(FromUi::CloseRequested { id });
            }
            super::wake_wayland();
            NSApplicationTerminateReply::TerminateCancel
        }

        /// A click on the tile of a running application with no window open
        /// starts it again (`run`), which is what a Mac user expects of a
        /// Dock tile and what a Linux application without windows needs.
        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn application_should_handle_reopen(&self, _app: &NSApplication, visible: bool) -> bool {
            if !visible {
                dock_pick("");
            }
            true
        }

        #[unsafe(method_id(applicationDockMenu:))]
        fn application_dock_menu(&self, _app: &NSApplication) -> Option<Retained<NSMenu>> {
            let mtm = self.mtm();
            let items: Vec<DockItem> = DOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|(i, _)| i.clone())
                .unwrap_or_default();
            dock_menu(mtm, self, &items)
        }
    }


    impl WcAppDelegate {
        #[unsafe(method(dockPick:))]
        fn dock_pick_item(&self, sender: &NSMenuItem) {
            let tag = sender.tag();
            let key = DOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .and_then(|(items, _)| items.get(tag as usize).map(|i| i.key.clone()));
            if let Some(key) = key {
                dock_pick(&key);
            }
        }
    }

    impl WcAppDelegate {
        /// The clipboard and layout poll. macOS has no pasteboard change
        /// notification, so `changeCount` is compared on a timer.
        #[unsafe(method(tick:))]
        fn tick(&self, _t: &NSTimer) {
            if FACE.load(Ordering::Relaxed) {
                face_tick();
                return;
            }
            poll_clipboard();
            check_layout();
        }

        #[unsafe(method(closeKeyWindow:))]
        fn close_key_window(&self, _sender: Option<&AnyObject>) {}
    }
);

impl WcAppDelegate {
    fn new(mtm: MainThreadMarker, events: Sender<FromUi>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(AppIvars { events });
        // SAFETY: NSObject's initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

// ----------------------------------------------------------------- keys ---

fn trace_keys() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("WC_MAC_TRACE").is_some())
}

fn key_event(id: WindowId, kvk: u16, pressed: bool, time: u32) {
    if trace_keys() {
        println!("wc-compositor: macos key kVK {kvk:#04x} {} for window {id}", if pressed { "down" } else { "up" });
    }
    let send = with_input(|i| {
        let code = mac_keys::evdev_keycode(kvk, i.cmd)?;
        i.fold.apply(kvk, code, pressed).map(|p| (code, p))
    })
    .flatten();
    if let Some((keycode, pressed)) = send {
        send_global(FromUi::Key { id, keycode, pressed, time });
    }
}

/// AppKit does not send `keyUp:` for a key released while Cmd is held: the
/// release goes to the menu's key-equivalent machinery and stops there. With
/// Cmd as Control that would leave the client's C held after Cmd+C. A local
/// monitor sees every key-up first and delivers those itself.
fn install_cmd_keyup_monitor() {
    let block = block2::RcBlock::new(|e: std::ptr::NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: AppKit passes a live event for the duration of the call.
        let event = unsafe { e.as_ref() };
        if event.modifierFlags().contains(NSEventModifierFlags::Command)
            && let Some(id) = with_input(|i| i.focus).flatten()
        {
            key_event(id, event.keyCode(), false, ms(event));
            return std::ptr::null_mut();
        }
        e.as_ptr()
    });
    // SAFETY: the handler is retained by AppKit for the process's lifetime.
    let monitor = unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(objc2_app_kit::NSEventMask::KeyUp, &block) };
    std::mem::forget(monitor);
}

/// The current keyboard layout's name among the compiled-in ones.
fn tis_layout() -> &'static str {
    // SAFETY: TIS returns a +1 input source (released below) and a borrowed
    // CFString property; both are checked for null.
    unsafe {
        let src = TISCopyCurrentKeyboardLayoutInputSource();
        if src.is_null() {
            return crate::input::FALLBACK;
        }
        let id = TISGetInputSourceProperty(src, kTISPropertyInputSourceID);
        let name = if id.is_null() {
            String::new()
        } else {
            let s: &NSString = &*(id as *const NSString);
            s.to_string()
        };
        CFRelease(src);
        mac_keys::layout_for_input_source(&name)
    }
}

fn check_layout() {
    if FACE.load(Ordering::Relaxed) {
        return; // the main process follows the layout (§103.13)
    }
    let now = tis_layout();
    let changed = with_input(|i| {
        let c = i.layout != now;
        i.layout = now;
        c
    });
    if changed == Some(true) {
        send_global(FromUi::Layout { name: now });
    }
}

// --------------------------------------------------------------- cursor ---

fn apply_cursor() {
    with_input(|i| {
        if let Some(c) = &i.cursor {
            c.set();
        }
    });
}

fn cursor_for(spec: &CursorSpec) -> Option<Retained<NSCursor>> {
    match spec {
        CursorSpec::Hidden => None,
        CursorSpec::Named(icon) => {
            let (name, private) = mac_keys::nscursor_selector(*icon);
            let sel = Sel::register(&std::ffi::CString::new(name).ok()?);
            let cls: &AnyClass = NSCursor::class();
            // SAFETY: `respondsToSelector:` is safe on any class object.
            let ok: bool = unsafe { msg_send![cls, respondsToSelector: sel] };
            if private && !ok {
                return Some(NSCursor::crosshairCursor());
            }
            if !ok {
                return Some(NSCursor::arrowCursor());
            }
            // SAFETY: every name in the table is an NSCursor class method that
            // takes no arguments and returns an NSCursor.
            let c: Option<Retained<NSCursor>> = unsafe { msg_send![cls, performSelector: sel] };
            c.or_else(|| Some(NSCursor::arrowCursor()))
        }
        CursorSpec::Image { image, hotspot_x, hotspot_y } => {
            let ns = nsimage(image)?;
            let hot = NSPoint::new(f64::from(*hotspot_x), f64::from(*hotspot_y));
            Some(NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &ns, hot))
        }
    }
}

// --------------------------------------------------------------- images ---

extern "C" fn free_pixels(info: *mut c_void, _data: *const c_void, _size: usize) {
    // SAFETY: `info` is the `Box<Vec<u8>>` leaked in `cgimage`, freed once.
    drop(unsafe { Box::from_raw(info.cast::<Vec<u8>>()) });
}

/// A CGImage over a copy of tightly packed BGRA pixels. Returns +1.
fn cgimage(width: i32, height: i32, pixels: Vec<u8>, alpha: bool) -> Option<*mut c_void> {
    if width <= 0 || height <= 0 || pixels.len() < (width as usize) * (height as usize) * 4 {
        return None;
    }
    let len = pixels.len();
    let data = pixels.as_ptr().cast::<c_void>();
    let info = Box::into_raw(Box::new(pixels)).cast::<c_void>();
    // SAFETY: the provider owns `info` and frees it through `free_pixels`; the
    // image retains the provider and the colour space.
    unsafe {
        let provider = CGDataProviderCreateWithData(info, data, len, Some(free_pixels));
        let space = CGColorSpaceCreateDeviceRGB();
        let info_bits = BYTE_ORDER_32_LITTLE | if alpha { ALPHA_PREMULTIPLIED_FIRST } else { ALPHA_NONE_SKIP_FIRST };
        let image = CGImageCreate(
            width as usize,
            height as usize,
            8,
            32,
            width as usize * 4,
            space,
            info_bits,
            provider,
            std::ptr::null(),
            false,
            0,
        );
        CGColorSpaceRelease(space);
        CGDataProviderRelease(provider);
        (!image.is_null()).then_some(image)
    }
}

fn nsimage(icon: &IconImage) -> Option<Retained<NSImage>> {
    let img = cgimage(icon.width, icon.height, icon.pixels.clone(), true)?;
    let size = NSSize::new(f64::from(icon.width), f64::from(icon.height));
    // SAFETY: `initWithCGImage:size:` retains the image, released after.
    unsafe {
        let ns: Retained<NSImage> = msg_send![NSImage::alloc(), initWithCGImage: CGImageRef(img), size: size];
        CGImageRelease(img);
        Some(ns)
    }
}

// ------------------------------------------------------------ clipboard ---

const UTI_TEXT: &str = "public.utf8-plain-text";
const UTI_HTML: &str = "public.html";
const UTI_PNG: &str = "public.png";
const UTI_FILE_URL: &str = "public.file-url";

const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

fn uti_for_mime(mime: &str) -> Option<&'static str> {
    if TEXT_MIMES.contains(&mime) {
        return Some(UTI_TEXT);
    }
    match mime {
        "text/html" => Some(UTI_HTML),
        "image/png" => Some(UTI_PNG),
        "text/uri-list" => Some(UTI_FILE_URL),
        _ => None,
    }
}

fn pasteboard(drag: bool) -> Retained<NSPasteboard> {
    if drag {
        // SAFETY: a valid pasteboard name constant.
        unsafe { NSPasteboard::pasteboardWithName(objc2_app_kit::NSPasteboardNameDrag) }
    } else {
        NSPasteboard::generalPasteboard()
    }
}

fn poll_clipboard() {
    if FACE.load(Ordering::Relaxed) {
        return; // the main process has the clipboard (§103.13)
    }
    let mut seen = STATE.with(|s| s.borrow().as_ref().map(|st| st.clip_seen)).unwrap_or(0);
    let fresh = clipboard_poll(&mut seen);
    STATE.with(|s| {
        if let Ok(mut g) = s.try_borrow_mut()
            && let Some(st) = g.as_mut()
        {
            st.clip_seen = seen;
        }
    });
    if let Some(mimes) = fresh {
        send_global(FromUi::ClipboardChanged { mimes });
    }
}

pub(super) fn pasteboard_count() -> isize {
    pasteboard(false).changeCount()
}

/// The pasteboard's offer as Wayland mime types, if it changed since `seen`.
pub(super) fn clipboard_poll(seen: &mut isize) -> Option<Vec<String>> {
    let pb = pasteboard(false);
    let count = pb.changeCount();
    if count == *seen {
        return None;
    }
    *seen = count;
    let mut mimes: Vec<String> = Vec::new();
    if let Some(types) = pb.types() {
        let has = |uti: &str| types.iter().any(|t| t.to_string() == uti);
        if has(UTI_TEXT) || has("NSStringPboardType") {
            mimes.extend(TEXT_MIMES.iter().map(|m| m.to_string()));
        }
        if has(UTI_HTML) {
            mimes.push("text/html".into());
        }
        if has(UTI_PNG) {
            mimes.push("image/png".into());
        }
        if has(UTI_FILE_URL) {
            mimes.push("text/uri-list".into());
        }
    }
    Some(mimes)
}

pub(super) fn read_pasteboard(drag: bool, mime: &str) -> Option<Vec<u8>> {
    let pb = pasteboard(drag);
    let uti = uti_for_mime(mime)?;
    if uti == UTI_TEXT {
        let s = pb.stringForType(&NSString::from_str(UTI_TEXT))?;
        return Some(s.to_string().into_bytes());
    }
    if uti == UTI_FILE_URL {
        let items = pb.pasteboardItems()?;
        let mut out = String::new();
        for item in items.iter() {
            if let Some(s) = item.stringForType(&NSString::from_str(UTI_FILE_URL)) {
                out.push_str(&s.to_string());
                out.push_str("\r\n");
            }
        }
        return (!out.is_empty()).then(|| out.into_bytes());
    }
    let data = pb.dataForType(&NSString::from_str(uti))?;
    Some(data.to_vec())
}

pub(super) fn write_pasteboard(payloads: &[(String, Vec<u8>)]) -> isize {
    let pb = pasteboard(false);
    pb.clearContents();
    let mut wrote_text = false;
    for (mime, bytes) in payloads {
        let Some(uti) = uti_for_mime(mime) else { continue };
        let ty = NSString::from_str(uti);
        if uti == UTI_TEXT || uti == UTI_FILE_URL {
            if uti == UTI_TEXT && wrote_text {
                continue;
            }
            let text = String::from_utf8_lossy(bytes);
            let text = if uti == UTI_FILE_URL { text.lines().next().unwrap_or("").to_string() } else { text.into_owned() };
            pb.setString_forType(&NSString::from_str(&text), &ty);
            wrote_text |= uti == UTI_TEXT;
        } else {
            pb.setData_forType(Some(&NSData::with_bytes(bytes)), &ty);
        }
    }
    pb.changeCount()
}

// ------------------------------------------------------------- the pump ---

/// The in-process AppKit pump, or (with `WC_MAC_ROLE=main`) the main
/// process's router to its faces (§103.13).
pub(crate) enum PumpInner {
    AppKit { commands: Receiver<ToUi>, events: Sender<FromUi> },
    Router(super::macos_router::Router),
}

pub fn current_layout() -> &'static str {
    tis_layout()
}

/// No digitiser on a Mac: trackpads are pointers.
pub fn has_touch() -> bool {
    false
}

/// `(delay ms, rate per second)` from the user's Keyboard settings.
pub fn keyboard_repeat() -> (i32, i32) {
    let delay = NSEvent::keyRepeatDelay();
    let interval = NSEvent::keyRepeatInterval();
    let delay_ms = (delay * 1000.0).round().clamp(100.0, 2000.0) as i32;
    let rate = if interval > 0.0 { (1.0 / interval).round().clamp(1.0, 100.0) as i32 } else { 30 };
    (delay_ms, rate)
}

pub fn start(config: PlatformConfig) -> std::io::Result<Started> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| std::io::Error::other("the macOS platform must start on the main thread"))?;
    let (tx, rx) = std::sync::mpsc::channel();
    let main_role = std::env::var("WC_MAC_ROLE").as_deref() == Ok("main");
    // NSApplication exists before anything asks about screens.
    let app = NSApplication::sharedApplication(mtm);
    let policy = match std::env::var("WC_MAC_ACTIVATION").as_deref() {
        // The main process has no Dock tile and no menu bar: its faces do.
        _ if main_role => NSApplicationActivationPolicy::Prohibited,
        Ok("accessory") => NSApplicationActivationPolicy::Accessory,
        _ => NSApplicationActivationPolicy::Regular,
    };
    app.setActivationPolicy(policy);
    let work_area = NSScreen::mainScreen(mtm).map(|s| {
        let v = s.visibleFrame();
        (v.size.width as i32, v.size.height as i32)
    });
    Ok(assemble(
        tx,
        Wake,
        if main_role {
            PumpInner::Router(super::macos_router::Router { commands: rx, events: config.events })
        } else {
            PumpInner::AppKit { commands: rx, events: config.events }
        },
        work_area,
    ))
}

impl PumpInner {
    pub(crate) fn run(self) {
        let (commands, events) = match self {
            PumpInner::Router(r) => return r.run(),
            PumpInner::AppKit { commands, events } => (commands, events),
        };
        let mtm = MainThreadMarker::new().expect("the pump runs on the main thread");
        let cmd = std::env::var("WC_MAC_CMD_KEY").ok().and_then(|s| CmdKey::parse(&s)).unwrap_or_default();
        println!("wc-compositor: macOS platform, Cmd sends {cmd:?}");
        EVENTS.with(|e| *e.borrow_mut() = Some(events.clone()));
        INPUT.with(|i| {
            *i.borrow_mut() = Some(Input {
                cmd,
                fold: KeyFold::default(),
                cursor: Some(NSCursor::arrowCursor()),
                hidden: false,
                focus: None,
                layout: tis_layout(),
            })
        });
        STATE.with(|s| {
            *s.borrow_mut() = Some(State {
                commands,
                windows: HashMap::new(),
                made: 0,
                gone: 0,
                clients_at_exit: 0,
                clip_seen: NSPasteboard::generalPasteboard().changeCount(),
                idle_assertion: None,
                cascade: NSPoint::new(0.0, 0.0),
            })
        });
        let app = NSApplication::sharedApplication(mtm);
        let delegate = WcAppDelegate::new(mtm, events.clone());
        app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        install_menu(mtm, &app);
        install_cmd_keyup_monitor();
        // SAFETY: the target is retained in APP_DELEGATE for the process's life.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.25,
                &delegate,
                sel!(tick:),
                None,
                true,
            )
        };
        TIMER.with(|t| *t.borrow_mut() = Some(timer));
        APP_DELEGATE.with(|d| *d.borrow_mut() = Some(delegate));
        // Commands queued before the run loop started.
        drain();
        app.run();

        let (made, gone, live, clients) = STATE.with(|s| {
            let g = s.borrow();
            let st = g.as_ref().expect("state");
            (st.made, st.gone, st.windows.len(), st.clients_at_exit)
        });
        println!(
            "wc-compositor: windows at exit: {made} created, {gone} destroyed, \
             {live} still held, {live} still alive, {clients} client(s) still connected"
        );
        let _ = events.send(FromUi::Stopped);
    }
}

fn install_menu(mtm: MainThreadMarker, app: &NSApplication) {
    let bar = NSMenu::new(mtm);
    let app_item = NSMenuItem::new(mtm);
    bar.addItem(&app_item);
    let app_menu = NSMenu::new(mtm);
    let item = |title: &str, action: Option<Sel>, key: &str| {
        // SAFETY: a plain menu item; the actions are NSApplication's own.
        unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(mtm),
                &NSString::from_str(title),
                action,
                &NSString::from_str(key),
            )
        }
    };
    app_menu.addItem(&item("Hide", Some(sel!(hide:)), "h"));
    app_menu.addItem(&NSMenuItem::separatorItem(mtm));
    app_menu.addItem(&item("Quit (close all windows)", Some(sel!(terminate:)), "q"));
    app_item.setSubmenu(Some(&app_menu));
    let win_item = NSMenuItem::new(mtm);
    bar.addItem(&win_item);
    let win_menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str("Window"));
    win_menu.addItem(&item("Minimize", Some(sel!(performMiniaturize:)), "m"));
    win_menu.addItem(&item("Zoom", Some(sel!(performZoom:)), ""));
    win_item.setSubmenu(Some(&win_menu));
    app.setMainMenu(Some(&bar));
    app.setWindowsMenu(Some(&win_menu));
}

/// Run every queued command, on the main thread.
fn drain() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    loop {
        let next = STATE.with(|s| s.borrow().as_ref().and_then(|st| st.commands.try_recv().ok()));
        let Some(cmd) = next else { break };
        if let ToUi::Quit { clients } = cmd {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.clients_at_exit = clients;
                }
            });
            let app = NSApplication::sharedApplication(mtm);
            app.stop(None);
            post_wakeup_event(&app);
            return;
        }
        handle(mtm, cmd);
    }
}

/// `stop:` takes effect after the next event, so give it one.
fn post_wakeup_event(app: &NSApplication) {
    // SAFETY: an application-defined event with no window, which AppKit drops.
    unsafe {
        let e: Option<Retained<NSEvent>> = msg_send![
            NSEvent::class(),
            otherEventWithType: objc2_app_kit::NSEventType::ApplicationDefined,
            location: NSPoint::new(0.0, 0.0),
            modifierFlags: NSEventModifierFlags(0),
            timestamp: 0.0f64,
            windowNumber: 0isize,
            context: std::ptr::null::<AnyObject>(),
            subtype: 0i16,
            data1: 0isize,
            data2: 0isize
        ];
        if let Some(e) = e {
            app.postEvent_atStart(&e, true);
        }
    }
}

fn with_win<R>(id: WindowId, f: impl FnOnce(&mut MacWin) -> R) -> Option<R> {
    STATE.with(|s| s.borrow_mut().as_mut().and_then(|st| st.windows.get_mut(&id).map(f)))
}

/// Take a window's handles out of the map, so AppKit can be called with no
/// borrow held (see the module header).
fn handles(id: WindowId) -> Option<(Retained<NSWindow>, Retained<WcView>, Rc<WinShared>)> {
    with_win(id, |w| (w.window.clone(), w.view.clone(), w.shared.clone()))
}

fn handle(mtm: MainThreadMarker, cmd: ToUi) {
    match cmd {
        ToUi::Create { id, title, app_id, aumid, shadow, kind, bounds } => {
            create(mtm, id, title, app_id, aumid, shadow, kind, bounds);
        }
        ToUi::Title { id, title } => {
            if let Some((window, _, _)) = handles(id) {
                window.setTitle(&NSString::from_str(&title));
            }
        }
        ToUi::AppId { id, app_id, aumid } => {
            println!("wc-compositor: window {id} app id {app_id:?}, tile identity {aumid:?}");
        }
        ToUi::Icon { id, icons } => {
            with_win(id, |w| w.icons = icons.clone());
            set_app_icon(mtm, &icons);
        }
        ToUi::MovePopup { id, x, y, width, height } => {
            let parent = with_win(id, |w| match w.kind {
                WindowKind::Popup { parent, .. } => {
                    w.kind = WindowKind::Popup { parent, x, y };
                    Some(parent)
                }
                WindowKind::Toplevel => None,
            })
            .flatten();
            if let Some(parent) = parent {
                place_popup(id, parent, x, y, width, height);
            }
        }
        ToUi::Present { id, damage } => present(id, &damage),
        // Never sent off Windows; see its definition.
        ToUi::Direct { .. } => {}
        ToUi::ShiftOrigin { id, dx, dy } => {
            if let Some((window, _, _)) = handles(id) {
                let mut f = window.frame();
                f.origin.x += f64::from(dx);
                f.origin.y -= f64::from(dy);
                window.setFrameOrigin(f.origin);
            }
        }
        ToUi::Map { id, width, height, fold_caption } => map(mtm, id, width, height, fold_caption),
        ToUi::Resize { id, width, height } => {
            if let Some((window, view, shared)) = handles(id) {
                shared.size.set((width, height));
                set_content(&window, &view, &shared, width, height);
            }
        }
        ToUi::SizeHints { id, min, max } => {
            if let Some((window, _, _)) = handles(id) {
                window.setContentMinSize(NSSize::new(f64::from(min.0.max(1)), f64::from(min.1.max(1))));
                let mx = |v: i32| if v <= 0 { f64::MAX } else { f64::from(v) };
                window.setContentMaxSize(NSSize::new(mx(max.0), mx(max.1)));
            }
        }
        ToUi::SetWindowState { id, maximized, minimized } => {
            let early = with_win(id, |w| {
                if !w.mapped {
                    w.pending.0 = maximized.or(w.pending.0);
                    w.pending.1 = minimized.or(w.pending.1);
                }
                !w.mapped
            });
            if early == Some(true) {
                return;
            }
            if let Some((window, _, _)) = handles(id) {
                if let Some(m) = maximized
                    && window.isZoomed() != m
                {
                    window.zoom(None);
                }
                match minimized {
                    Some(true) if !window.isMiniaturized() => window.miniaturize(None),
                    Some(false) if window.isMiniaturized() => window.deminiaturize(None),
                    _ => {}
                }
            }
        }
        ToUi::Fullscreen { id, on } => {
            let early = with_win(id, |w| {
                if !w.mapped {
                    w.pending.2 = Some(on);
                }
                !w.mapped
            });
            if early == Some(true) {
                return;
            }
            if let Some((window, _, _)) = handles(id) {
                let is = window.styleMask().contains(NSWindowStyleMask::FullScreen);
                if is != on {
                    window.toggleFullScreen(None);
                }
            }
        }
        ToUi::Cursor { spec } => {
            let hidden = matches!(spec, CursorSpec::Hidden);
            let cursor = cursor_for(&spec);
            let was_hidden = with_input(|i| {
                let was = i.hidden;
                i.hidden = hidden;
                if let Some(c) = cursor {
                    i.cursor = Some(c);
                }
                was
            })
            .unwrap_or(false);
            // `hide`/`unhide` are counted; keep them balanced.
            if hidden && !was_hidden {
                NSCursor::hide();
            } else if !hidden && was_hidden {
                NSCursor::unhide();
            }
            apply_cursor();
        }
        ToUi::PointerHold { id, hold } => {
            if let Some((_, _, shared)) = handles(id) {
                let before = shared.hold.replace(hold);
                let locked = |h: Option<crate::hold::PointerHold>| matches!(h, Some(crate::hold::PointerHold::Locked));
                // SAFETY: plain value calls.
                unsafe {
                    if locked(hold) && !locked(before) {
                        CGAssociateMouseAndMouseCursorPosition(0);
                    } else if !locked(hold) && locked(before) {
                        CGAssociateMouseAndMouseCursorPosition(1);
                    }
                }
            }
        }
        ToUi::BeginMove { id } => {
            if let Some((window, _, shared)) = handles(id) {
                let ev = shared.last_down.borrow().clone();
                if let Some(ev) = ev {
                    window.performWindowDragWithEvent(&ev);
                }
            }
        }
        ToUi::BeginResize { id, edge } => {
            if let Some((window, _, shared)) = handles(id)
                && edge != Edge::None
            {
                shared.resizing.set(Some((edge, window.frame(), NSEvent::mouseLocation())));
            }
        }
        ToUi::WindowMenu { id, x, y } => {
            if let Some((_, view, _)) = handles(id) {
                let menu = NSMenu::new(mtm);
                let mk = |t: &str, a: Sel| {
                    // SAFETY: standard NSWindow actions, sent to the key window.
                    unsafe {
                        NSMenuItem::initWithTitle_action_keyEquivalent(
                            NSMenuItem::alloc(mtm),
                            &NSString::from_str(t),
                            Some(a),
                            &NSString::from_str(""),
                        )
                    }
                };
                menu.addItem(&mk("Minimize", sel!(performMiniaturize:)));
                menu.addItem(&mk("Zoom", sel!(performZoom:)));
                menu.addItem(&mk("Close", sel!(performClose:)));
                menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(f64::from(x), f64::from(y)), Some(&view));
            }
        }
        ToUi::Destroy { id } => destroy(id),
        ToUi::IdleInhibit { on } => idle_inhibit(on),
        ToUi::Activate { id } => {
            if let Some((window, _, _)) = handles(id) {
                window.makeKeyAndOrderFront(None);
                NSApplication::sharedApplication(mtm).activate();
            }
        }
        ToUi::SetClipboard { payloads } => {
            let count = write_pasteboard(&payloads);
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.clip_seen = count;
                }
            });
        }
        ToUi::FetchClipboard { token, from_drag, mime } => {
            let data = read_pasteboard(from_drag, &mime);
            send_global(FromUi::ClipboardData { token, data });
        }
        ToUi::StartDrag { .. } => {
            // §103.7: drags out of a Linux window are not wired yet; the
            // compositor hears that the drag ended with nothing taken.
            send_global(FromUi::DragFinished { action: super::dnd_action::NONE });
        }
        ToUi::CancelDrag | ToUi::DragFeedback { .. } => {}
        ToUi::Quit { .. } => unreachable!("handled in drain"),
    }
}

#[allow(clippy::too_many_arguments)]
fn create(
    mtm: MainThreadMarker,
    id: WindowId,
    title: String,
    app_id: String,
    aumid: String,
    shadow: Arc<Mutex<Shadow>>,
    kind: WindowKind,
    bounds: PopupBounds,
) {
    let events = EVENTS.with(|e| e.borrow().clone()).expect("events");
    let is_popup = matches!(kind, WindowKind::Popup { .. });
    // The work area before the window has a place on screen: the compositor
    // sizes a client that committed only its minimum (Firefox's first commit)
    // from this cell's width and height (state.rs §91.6), and it asks at the
    // first commit, before `Map`. The origin is corrected once the window is
    // placed (`publish_bounds`).
    if !is_popup && let Some(screen) = NSScreen::mainScreen(mtm) {
        let v = screen.visibleFrame();
        let mut cell = bounds.lock().unwrap_or_else(|e| e.into_inner());
        if cell.is_none() {
            *cell = Some(Rect::new(0, 0, v.size.width as i32, v.size.height as i32));
        }
    }
    let shared = Rc::new(WinShared {
        id,
        events,
        size: Cell::new((0, 0)),
        // Quiet until mapped: AppKit resizes a window while it is being set
        // up, and a client told 320x240 before its first configure is acked
        // draws at 320x240 (measured on the first run).
        quiet: Cell::new(true),
        last_down: RefCell::new(None),
        resizing: Cell::new(None),
        is_popup,
        bounds,
        hold: Cell::new(None),
        zoomed: Cell::new(false),
    });
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(320.0, 240.0));
    let style = if is_popup {
        NSWindowStyleMask::Borderless
    } else {
        NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable
    };
    // SAFETY: the designated initialiser; `releasedWhenClosed` is turned off
    // below because the map owns the window.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: a property setter; the Retained in the map keeps it alive.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str(&title));
    window.setAcceptsMouseMovedEvents(true);
    let view = WcView::new(mtm, frame, shared.clone());
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
    view.setWantsLayer(true);
    // NSViewLayerContentsRedrawNever: the layer's contents are ours.
    let _: () = unsafe { msg_send![&*view, setLayerContentsRedrawPolicy: 0isize] };
    if let Some(layer) = view_layer(&view) {
        // SAFETY: CALayer property setters with valid values.
        unsafe {
            // The backing layer of a flipped view is itself flipped, so the
            // visual top left is the layer's "bottomLeft" (measured: "topLeft"
            // put a smaller buffer at the window's bottom left).
            let gravity = NSString::from_str("bottomLeft");
            let _: () = msg_send![&*layer, setContentsGravity: &*gravity];
            let _: () = msg_send![&*layer, setOpaque: !is_popup];
        }
    }
    window.setContentView(Some(&view));
    let delegate = if is_popup {
        window.setHasShadow(true);
        window.setOpaque(false);
        window.setBackgroundColor(Some(&objc2_app_kit::NSColor::clearColor()));
        None
    } else {
        let d = WcDelegate::new(mtm, shared.clone());
        window.setDelegate(Some(ProtocolObject::from_ref(&*d)));
        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);
        // Tabbing would group Linux windows into one frame, which no Linux
        // window manager does.
        let _: () = unsafe { msg_send![&*window, setTabbingMode: 2isize] }; // NSWindowTabbingModeDisallowed
        Some(d)
    };
    match kind {
        WindowKind::Toplevel => println!("wc-compositor: window {id} created (macos) title={title:?} app_id={app_id:?}"),
        WindowKind::Popup { parent, x, y } => {
            println!("wc-compositor: window {id} created (macos popup) parent={parent} at {x},{y}")
        }
    }
    println!("wc-compositor: window {id} taskbar identity -> {aumid:?}");
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.made += 1;
            st.windows.insert(
                id,
                MacWin {
                    window,
                    view,
                    shared,
                    _delegate: delegate,
                    shadow,
                    kind,
                    mapped: false,
                    icons: Vec::new(),
                    fold: false,
                    pending: (None, None, None),
                },
            );
        }
    });
}

/// Set the content size and make the view fill it. The view follows the
/// window through its autoresizing mask as well; saying so explicitly costs
/// nothing and survives a style-mask change, which re-lays the frame out.
fn set_content(window: &NSWindow, view: &NSView, shared: &WinShared, width: i32, height: i32) {
    let was = shared.quiet.replace(true);
    window.setContentSize(NSSize::new(f64::from(width), f64::from(height)));
    if let Some(cv) = window.contentView() {
        view.setFrame(cv.bounds());
    }
    shared.quiet.set(was);
}

fn view_layer(view: &NSView) -> Option<Retained<AnyObject>> {
    // SAFETY: `layer` is an NSView property returning a CALayer or nil.
    unsafe { msg_send![view, layer] }
}

fn map(mtm: MainThreadMarker, id: WindowId, width: i32, height: i32, fold: bool) {
    let Some((window, view, shared)) = handles(id) else { return };
    let (kind, already) = with_win(id, |w| {
        let a = w.mapped;
        w.mapped = true;
        w.fold = fold;
        (w.kind, a)
    })
    .unwrap_or((WindowKind::Toplevel, true));
    shared.size.set((width, height));
    match kind {
        WindowKind::Popup { parent, x, y } => {
            place_popup(id, parent, x, y, width, height);
            if let Some((pwin, _, _)) = handles(parent) {
                // SAFETY: both windows are alive and owned by the map.
                unsafe { pwin.addChildWindow_ordered(&window, NSWindowOrderingMode::Above) };
            }
            window.orderFront(None);
        }
        WindowKind::Toplevel => {
            if fold {
                let mask = window.styleMask() | NSWindowStyleMask::FullSizeContentView;
                window.setStyleMask(mask);
                window.setTitlebarAppearsTransparent(true);
                window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
                // The traffic lights stay, drawn by AppKit over the client's
                // own title bar, as Windows keeps its caption buttons (§82):
                // the host's `mac-tabs` arrangement leaves their corner empty.
                // WC_MAC_FOLD_BUTTONS=off hides them for a client that draws
                // buttons of its own and was not told to leave room.
                if std::env::var("WC_MAC_FOLD_BUTTONS").as_deref() == Ok("off") {
                    for b in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
                        if let Some(button) = window.standardWindowButton(b) {
                            button.setHidden(true);
                        }
                    }
                }
            }
            set_content(&window, &view, &shared, width, height);
            // From here a size change is news to the client, including the
            // one fit_on_screen may make.
            shared.quiet.set(false);
            if !already {
                // Cascade from the previous window, inside the screen.
                let next = STATE.with(|s| s.borrow().as_ref().map(|st| st.cascade)).unwrap_or(NSPoint::new(0.0, 0.0));
                let next = if next.x == 0.0 && next.y == 0.0 {
                    window.center();
                    let f = window.frame();
                    NSPoint::new(f.origin.x, f.origin.y + f.size.height)
                } else {
                    window.cascadeTopLeftFromPoint(next)
                };
                STATE.with(|s| {
                    if let Some(st) = s.borrow_mut().as_mut() {
                        st.cascade = next;
                    }
                });
                fit_on_screen(&window, &shared);
            }
            window.makeKeyAndOrderFront(None);
            window.makeFirstResponder(Some(&view));
            let app = NSApplication::sharedApplication(mtm);
            app.activate();
            if FACE.load(Ordering::Relaxed) && !already {
                // The application's first window, seconds after its launch
                // (§103.13): the launch's own activation is long spent.
                #[allow(deprecated)]
                app.activateIgnoringOtherApps(true);
            }
            publish_bounds(&window, &shared);
        }
    }
    println!("wc-compositor: window {id} mapped {width}x{height} fold_caption={fold}");
    present(id, &[]);
    // What was asked for before the window existed on screen, now that it does.
    let (max, min, full) = with_win(id, |w| std::mem::take(&mut w.pending)).unwrap_or_default();
    if max.is_some() || min.is_some() {
        println!("wc-compositor: window {id} applies its pre-map state: maximized={max:?} minimized={min:?}");
        handle(mtm, ToUi::SetWindowState { id, maximized: max, minimized: min });
    }
    if let Some(on) = full {
        handle(mtm, ToUi::Fullscreen { id, on });
    }
}

/// Stage 38's rule on macOS: a window opens where it can be grabbed.
fn fit_on_screen(window: &NSWindow, shared: &WinShared) {
    let Some(screen) = window.screen() else { return };
    let v = screen.visibleFrame();
    let f = window.frame();
    // AppKit's y is bottom up; fit_to_work_area thinks top down, so flip.
    let top = |y: f64, h: f64| v.origin.y + v.size.height - (y + h);
    let (x, y, w, h) = super::fit_to_work_area(
        (f.origin.x as i32, top(f.origin.y, f.size.height) as i32, f.size.width as i32, f.size.height as i32),
        (v.origin.x as i32, 0, (v.origin.x + v.size.width) as i32, v.size.height as i32),
    );
    if (w, h) != (f.size.width as i32, f.size.height as i32) || x != f.origin.x as i32 {
        let ny = v.origin.y + v.size.height - f64::from(y) - f64::from(h);
        let _ = shared;
        window.setFrame_display(NSRect::new(NSPoint::new(f64::from(x), ny), NSSize::new(f64::from(w), f64::from(h))), true);
    }
}

fn place_popup(id: WindowId, parent: WindowId, x: i32, y: i32, width: i32, height: i32) {
    let Some((pwin, _, _)) = handles(parent) else { return };
    let Some((window, _, _)) = handles(id) else { return };
    let c = if with_win(parent, |w| w.kind).is_some_and(|k| matches!(k, WindowKind::Popup { .. })) {
        pwin.frame()
    } else {
        pwin.contentRectForFrameRect(pwin.frame())
    };
    let (ox, oy) = mac_keys::popup_origin((c.origin.x, c.origin.y, c.size.width, c.size.height), x, y, height);
    window.setFrame_display(
        NSRect::new(NSPoint::new(ox, oy), NSSize::new(f64::from(width.max(1)), f64::from(height.max(1)))),
        true,
    );
}

fn present(id: WindowId, _damage: &[Rect]) {
    let Some((shadow, view)) = with_win(id, |w| (w.shadow.clone(), w.view.clone())) else { return };
    // §103.13: a face shows the main process's IOSurface itself. Core
    // Animation reads the surface directly, so nothing is copied here.
    let surf = face_surfaces(|m| m.get(&id).map(|(p, front, alpha)| (p[*front as usize].as_ptr(), *alpha)));
    if FACE.load(Ordering::Relaxed) {
        if let (Some((ptr, alpha)), Some(layer)) = (surf, view_layer(&view)) {
            // SAFETY: an IOSurfaceRef is a valid `contents`; the layer retains
            // it, and the map keeps our reference until the window goes.
            unsafe {
                let _: () = msg_send![&*layer, setOpaque: !alpha];
                let _: () = msg_send![&*layer, setContents: ptr.cast::<AnyObject>()];
            }
        }
        return;
    }
    let (w, h, alpha, pixels) = {
        let s = shadow.lock().unwrap_or_else(|e| e.into_inner());
        (s.width(), s.height(), s.format() == PixelFormat::Argb8888, s.pixels().to_vec())
    };
    let Some(img) = cgimage(w, h, pixels, alpha) else { return };
    if let Some(layer) = view_layer(&view) {
        // SAFETY: a CGImage is a valid `contents`; the layer retains it.
        unsafe {
            let _: () = msg_send![&*layer, setContents: img.cast::<AnyObject>()];
        }
    }
    // SAFETY: balances the +1 from `cgimage`.
    unsafe { CGImageRelease(img) };
}

fn destroy(id: WindowId) {
    let Some(w) = STATE.with(|s| {
        let mut g = s.borrow_mut();
        let st = g.as_mut()?;
        let w = st.windows.remove(&id)?;
        st.gone += 1;
        Some(w)
    }) else {
        return;
    };
    if let WindowKind::Popup { parent, .. } = w.kind
        && let Some((pwin, _, _)) = handles(parent)
    {
        pwin.removeChildWindow(&w.window);
    }
    if w.shared.hold.get() == Some(crate::hold::PointerHold::Locked) {
        // SAFETY: a plain value call.
        unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
    }
    w.window.orderOut(None);
    w.window.close();
    with_input(|i| {
        if i.focus == Some(id) {
            i.focus = None;
        }
    });
    println!("wc-compositor: window {id} destroyed");
}

fn set_app_icon(mtm: MainThreadMarker, icons: &[IconImage]) {
    // Phase 1 (§103.8): one process, one Dock tile, so the tile wears the
    // icon of the application that last sent one. The tile processes of
    // phase 2 each wear their own.
    let Some(best) = icons.iter().max_by_key(|i| i.width * i.height) else { return };
    if let Some(img) = nsimage(best) {
        // SAFETY: a property setter on the shared application.
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&img)) };
    }
}

fn idle_inhibit(on: bool) {
    let mut current = STATE.with(|s| s.borrow().as_ref().and_then(|st| st.idle_assertion));
    idle_assert(on, &mut current);
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            st.idle_assertion = current;
        }
    });
}

/// Hold or release the display-sleep assertion recorded in `current`.
pub(super) fn idle_assert(on: bool, current: &mut Option<u32>) {
    match (on, *current) {
        (true, None) => {
            let kind = NSString::from_str("PreventUserIdleDisplaySleep");
            let name = NSString::from_str("A Linux application asked the display to stay on");
            let mut id = 0u32;
            // SAFETY: CFStrings by toll-free bridging; `id` is an out-parameter.
            let ok = unsafe {
                IOPMAssertionCreateWithName(Retained::as_ptr(&kind).cast(), 255, Retained::as_ptr(&name).cast(), &mut id)
            } == 0;
            if ok {
                *current = Some(id);
            }
        }
        (false, Some(id)) => {
            // SAFETY: an assertion id this process created.
            unsafe { IOPMAssertionRelease(id) };
            *current = None;
        }
        _ => {}
    }
}

