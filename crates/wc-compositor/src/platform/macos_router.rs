//! §103.13. The macOS **main process**'s platform: no windows of its own, a
//! router to the faces that have them.
//!
//! One main process per user holds the compositor's whole Wayland state, the
//! arena, the session and the clipboard. Each Linux application has a *face*,
//! a small process started from the application's bundle, which exists only
//! because a Dock tile is a process: it owns the application's `NSApplication`
//! (tile, Dock menu, menu bar, activation) and its windows, and runs this
//! crate's AppKit platform (`macos.rs`) fed from a socket instead of from the
//! Wayland thread.
//!
//! This file is the other end of that socket. It keeps every window's state
//! (so a face that crashes can be given its windows again), decides which face
//! a window belongs to when it maps (by app id, `faceproto::app_id_matches`),
//! and turns each `Present` into a copy of the window's shadow into the back
//! surface of an IOSurface pair the face holds by mach port (`macmach.rs`).
//! The clipboard, the keyboard layout and idle inhibition are this process's,
//! because they are one per user, not one per application.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::faceproto::{self, FromFace, ToFace};
use super::macmach::{self, Surface};
use super::{dnd_action, FromUi, IconImage, PopupBounds, ToUi, WindowId, WindowKind};
use crate::surface::{PixelFormat, Shadow};

/// The main process's bootstrap service, in this user's GUI session.
pub const SERVICE: &str = "org.waycompose.main";

/// The main process's socket: `$TMPDIR/waycompose/main.sock`. `$TMPDIR` is
/// per user on macOS, the directory is made 0700 and the socket 0600, and
/// every connection's peer uid is checked as well.
pub fn socket_path() -> PathBuf {
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    tmp.join("waycompose").join("main.sock")
}

type Section = String;

/// What the launcher does with the faces' requests (§103.13).
pub struct FaceHooks {
    /// A face asked to run its application, an action of it or a URL.
    pub on_request: RequestHook,
    /// A face went away without saying goodbye; it should be started again.
    pub on_face_lost: LostHook,
}

/// `(section, key)`.
pub type RequestHook = Box<dyn Fn(&str, &str) + Send + Sync>;
/// `section`.
pub type LostHook = Box<dyn Fn(&str) + Send + Sync>;

static HOOKS: Mutex<Option<FaceHooks>> = Mutex::new(None);

pub fn set_face_hooks(h: FaceHooks) {
    *HOOKS.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
}

type FaceId = u64;

enum In {
    Ui(ToUi),
    Connected(FaceId, UnixStream),
    Face(FaceId, FromFace),
    Gone(FaceId),
    Port(u64, u32),
}

struct Face {
    stream: UnixStream,
    section: Section,
    ids: Vec<String>,
    token: u64,
    port: Option<u32>,
    hello: bool,
    bye: bool,
}

/// Damage rectangles as `(x, y, w, h)`; `None` is the whole window.
type Stale = Option<Vec<(i32, i32, i32, i32)>>;

struct Surf {
    s: [Surface; 2],
    back: u8,
    w: i32,
    h: i32,
    /// §103.15: what each surface of the pair has missed since it was last
    /// written: every present's damage goes to both, and a surface's list is
    /// copied and cleared when it becomes the back buffer. `None` is "all".
    stale: [Stale; 2],
}

/// §103.15: what the copies into the surfaces cost, for `WC_FACE_LATENCY`.
#[derive(Default)]
struct CopyStats {
    presents: u64,
    bytes: u64,
    full_bytes: u64,
    nanos: u64,
}

struct Win {
    kind: WindowKind,
    title: String,
    app_id: String,
    shadow: Arc<Mutex<Shadow>>,
    bounds: PopupBounds,
    icons: Vec<IconImage>,
    hints: Option<((i32, i32), (i32, i32))>,
    map: Option<(i32, i32, bool)>,
    state: (Option<bool>, Option<bool>),
    fullscreen: Option<bool>,
    face: Option<FaceId>,
    /// The section of the face that held it, while that face is gone.
    lost: Option<(Section, Instant)>,
    surf: Option<Surf>,
    next_gen: u32,
}

pub(crate) struct Router {
    pub(crate) commands: Receiver<ToUi>,
    pub(crate) events: Sender<FromUi>,
}

/// How long a crashed face's windows wait for it to come back.
const LOST_GRACE: Duration = Duration::from_secs(15);
/// How long the main process stays with no face at all.
const NO_FACE_EXIT: Duration = Duration::from_secs(60);

struct R {
    events: Sender<FromUi>,
    faces: HashMap<FaceId, Face>,
    ports: HashMap<u64, u32>,
    wins: HashMap<WindowId, Win>,
    pointer_face: Option<FaceId>,
    focus_face: Option<FaceId>,
    last_request: Option<(FaceId, Instant)>,
    clip_seen: isize,
    layout: &'static str,
    idle: Option<u32>,
    no_face_since: Option<Instant>,
    /// `WC_FACE_LATENCY=1`: AppKit's event time to this process, in µs.
    latency: Option<Vec<u64>>,
    copies: CopyStats,
}

/// WC_FACE_FULL_COPY, or a file `face-full-copy` in the launcher's support
/// directory (a face-started main process has launchd's environment): copy
/// whole windows, as before §103.15, for the comparison. Read once.
fn full_copy() -> bool {
    static FULL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FULL.get_or_init(|| {
        std::env::var_os("WC_FACE_FULL_COPY").is_some()
            || std::env::var_os("HOME").is_some_and(|h| {
                PathBuf::from(h).join("Library/Application Support/waycompose/face-full-copy").exists()
            })
    })
}

/// Milliseconds of `CLOCK_UPTIME_RAW` and its sub-millisecond rest in µs:
/// `NSEvent.timestamp`'s clock, so a face's input can be timed here.
fn uptime_us() -> u64 {
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock: u32) -> u64;
    }
    /// `CLOCK_UPTIME_RAW` from `<time.h>`.
    const CLOCK_UPTIME_RAW: u32 = 8;
    // SAFETY: plain call.
    unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) / 1000 }
}

impl R {
    fn send(&mut self, fid: FaceId, m: &ToFace) {
        let Some(f) = self.faces.get_mut(&fid) else { return };
        if faceproto::write_frame(&mut f.stream, &m.encode()).is_err() {
            // The reader thread reports the face gone; nothing to do here.
        }
    }

    fn event(&self, e: FromUi) {
        if self.events.send(e).is_ok() {
            super::wake_wayland();
        }
    }

    fn choose_face(&self, app_id: &str) -> Option<FaceId> {
        let live = |f: &&Face| f.hello && !f.bye;
        if let Some((fid, _)) = self.faces.iter().filter(|(_, f)| live(f)).find(|(_, f)| faceproto::app_id_matches(&f.ids, app_id)) {
            return Some(*fid);
        }
        // A window whose app id names no face: the application that asked to
        // run most recently (a launch whose window has not mapped yet), then
        // the focused one, then any.
        if let Some((fid, t)) = self.last_request
            && t.elapsed() < Duration::from_secs(60)
            && self.faces.get(&fid).is_some_and(|f| live(&f))
        {
            return Some(fid);
        }
        if let Some(fid) = self.focus_face.filter(|f| self.faces.get(f).is_some_and(|f| live(&f))) {
            return Some(fid);
        }
        self.faces.iter().find(|(_, f)| live(f)).map(|(k, _)| *k)
    }

    /// Give window `id` to face `fid`: everything it needs, in order.
    fn route(&mut self, id: WindowId, fid: FaceId) {
        let Some(w) = self.wins.get_mut(&id) else { return };
        w.face = Some(fid);
        w.lost = None;
        w.surf = None;
        let popup = match w.kind {
            WindowKind::Popup { parent, x, y } => Some((parent, x, y)),
            WindowKind::Toplevel => None,
        };
        let mut out = vec![ToFace::Create { id, title: w.title.clone(), app_id: w.app_id.clone(), popup }];
        if !w.icons.is_empty() {
            out.push(ToFace::Icon { id, icons: w.icons.clone() });
        }
        if let Some((min, max)) = w.hints {
            out.push(ToFace::SizeHints { id, min, max });
        }
        let map = w.map;
        let (st, fs) = (w.state, w.fullscreen);
        for m in out {
            self.send(fid, &m);
        }
        if let Some((mw, mh, fold)) = map {
            self.present(id);
            self.send(fid, &ToFace::Map { id, w: mw, h: mh, fold });
            self.present(id);
            if st.0.is_some() || st.1.is_some() {
                self.send(fid, &ToFace::SetWindowState { id, maximized: st.0, minimized: st.1 });
            }
            if let Some(on) = fs {
                self.send(fid, &ToFace::Fullscreen { id, on });
            }
        }
        println!("wc-compositor: window {id} -> face {fid} ({})", self.faces.get(&fid).map_or("?", |f| f.section.as_str()));
    }

    /// Copy the shadow into the back surface and show it: all of it.
    fn present(&mut self, id: WindowId) {
        self.present_damage(id, None);
    }

    /// Copy what the back surface has missed (§103.15): the damage of every
    /// present since it was last the back buffer, which with a pair is this
    /// present's and the one before. `None` is the whole window.
    fn present_damage(&mut self, id: WindowId, damage: Option<&[crate::surface::Rect]>) {
        let Some(w) = self.wins.get_mut(&id) else { return };
        let Some(fid) = w.face else { return };
        let Some(port) = self.faces.get(&fid).and_then(|f| f.port) else { return };
        let shadow = w.shadow.clone();
        let s = shadow.lock().unwrap_or_else(|e| e.into_inner());
        let (sw, sh) = (s.width(), s.height());
        if sw <= 0 || sh <= 0 {
            return;
        }
        let alpha = s.format() == PixelFormat::Argb8888;
        let mut fresh = None;
        if w.surf.as_ref().is_none_or(|x| (x.w, x.h) != (sw, sh)) {
            let (Some(a), Some(b)) = (Surface::new(sw, sh), Surface::new(sw, sh)) else { return };
            w.next_gen += 1;
            let generation = w.next_gen;
            // The ports go first, so the face finds them queued when it reads
            // the message that names them (faceproto's header).
            if macmach::send_surfaces(port, id, generation, [&a, &b]) != 0 {
                return;
            }
            w.surf = Some(Surf { s: [a, b], back: 0, w: sw, h: sh, stale: [None, None] });
            fresh = Some(ToFace::Surface { id, generation, w: sw, h: sh, alpha });
        }
        let surf = w.surf.as_mut().expect("surface pair");
        let back = surf.back;
        for st in &mut surf.stale {
            match (st.as_mut(), damage) {
                (Some(list), Some(d)) => list.extend(d.iter().map(|r| (r.x, r.y, r.w, r.h))),
                _ => *st = None,
            }
        }
        let t0 = Instant::now();
        let full = (sw as u64) * (sh as u64) * 4;
        let copied = match surf.stale[back as usize].take() {
            // More than half the window: one pass over all of it costs less
            // than the rectangles' row-by-row walk, and is never wrong.
            Some(list) if list.iter().map(|r| (r.2.max(0) as u64) * (r.3.max(0) as u64) * 4).sum::<u64>() * 2 < full => {
                surf.s[back as usize].copy_rects(s.pixels(), sw, sh, &list) as u64
            }
            _ => {
                surf.s[back as usize].copy_from(s.pixels(), sw, sh);
                full
            }
        };
        surf.stale[back as usize] = Some(Vec::new());
        surf.back ^= 1;
        drop(s);
        let c = &mut self.copies;
        c.presents += 1;
        c.bytes += copied;
        c.full_bytes += full;
        c.nanos += t0.elapsed().as_nanos() as u64;
        if self.latency.is_some() && c.presents.is_multiple_of(100) {
            println!(
                "wc-compositor: surface copies, last {} presents: {:.1} MB copied of {:.1} MB in full ({:.1}%), {:.3} ms per present",
                c.presents,
                c.bytes as f64 / 1e6,
                c.full_bytes as f64 / 1e6,
                100.0 * c.bytes as f64 / c.full_bytes.max(1) as f64,
                c.nanos as f64 / 1e6 / c.presents as f64
            );
            *c = CopyStats::default();
        }
        if let Some(m) = fresh {
            self.send(fid, &m);
        }
        self.send(fid, &ToFace::Present { id, buffer: back });
    }

    fn forward(&mut self, id: WindowId, m: ToFace) {
        if let Some(fid) = self.wins.get(&id).and_then(|w| w.face) {
            self.send(fid, &m);
        }
    }

    fn handle(&mut self, cmd: ToUi) -> bool {
        match cmd {
            ToUi::Create { id, title, app_id, aumid: _, shadow, kind, bounds } => {
                self.wins.insert(
                    id,
                    Win {
                        kind,
                        title,
                        app_id,
                        shadow,
                        bounds,
                        icons: Vec::new(),
                        hints: None,
                        map: None,
                        state: (None, None),
                        fullscreen: None,
                        face: None,
                        lost: None,
                        surf: None,
                        next_gen: 0,
                    },
                );
            }
            ToUi::Title { id, title } => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.title = title.clone();
                }
                self.forward(id, ToFace::Title { id, title });
            }
            ToUi::AppId { id, app_id, .. } => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.app_id = app_id.clone();
                }
                self.forward(id, ToFace::AppId { id, app_id });
            }
            ToUi::Icon { id, icons } => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.icons = icons.clone();
                }
                self.forward(id, ToFace::Icon { id, icons });
            }
            ToUi::MovePopup { id, x, y, width, height } => {
                if let Some(w) = self.wins.get_mut(&id)
                    && let WindowKind::Popup { parent, .. } = w.kind
                {
                    w.kind = WindowKind::Popup { parent, x, y };
                }
                self.forward(id, ToFace::MovePopup { id, x, y, w: width, h: height });
            }
            // WC_FACE_FULL_COPY=1: every present copies the whole window, as
            // before §103.15, for the comparison.
            ToUi::Present { id, damage } => {
                let full = full_copy();
                if std::env::var_os("WC_FACE_DAMAGE_DEBUG").is_some() {
                    println!("wc-compositor: present {id}: damage {:?}", damage.iter().map(|r| (r.x, r.y, r.w, r.h)).collect::<Vec<_>>());
                }
                self.present_damage(id, (!full).then_some(&damage[..]))
            }
            ToUi::ShiftOrigin { id, dx, dy } => self.forward(id, ToFace::ShiftOrigin { id, dx, dy }),
            ToUi::Map { id, width, height, fold_caption } => {
                let Some(w) = self.wins.get_mut(&id) else { return true };
                w.map = Some((width, height, fold_caption));
                if w.face.is_some() {
                    self.present(id);
                    self.forward(id, ToFace::Map { id, w: width, h: height, fold: fold_caption });
                    return true;
                }
                let target = match w.kind {
                    WindowKind::Popup { parent, .. } => self.wins.get(&parent).and_then(|p| p.face),
                    WindowKind::Toplevel => {
                        let app_id = w.app_id.clone();
                        self.choose_face(&app_id)
                    }
                };
                match target {
                    Some(fid) => self.route(id, fid),
                    None => println!("wc-compositor: window {id} mapped with no face to show it"),
                }
            }
            ToUi::Resize { id, width, height } => {
                if let Some(w) = self.wins.get_mut(&id)
                    && let Some(m) = w.map.as_mut()
                {
                    (m.0, m.1) = (width, height);
                }
                self.forward(id, ToFace::Resize { id, w: width, h: height });
            }
            ToUi::SizeHints { id, min, max } => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.hints = Some((min, max));
                }
                self.forward(id, ToFace::SizeHints { id, min, max });
            }
            ToUi::SetWindowState { id, maximized, minimized } => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.state = (maximized.or(w.state.0), minimized.or(w.state.1));
                }
                self.forward(id, ToFace::SetWindowState { id, maximized, minimized });
            }
            ToUi::Fullscreen { id, on } => {
                if let Some(w) = self.wins.get_mut(&id) {
                    w.fullscreen = Some(on);
                }
                self.forward(id, ToFace::Fullscreen { id, on });
            }
            ToUi::Cursor { spec } => {
                let to: Vec<FaceId> = match self.pointer_face.or(self.focus_face) {
                    Some(f) => vec![f],
                    None => self.faces.keys().copied().collect(),
                };
                for f in to {
                    self.send(f, &ToFace::Cursor { spec: spec.clone() });
                }
            }
            ToUi::PointerHold { id, hold } => self.forward(id, ToFace::PointerHold { id, hold }),
            ToUi::BeginMove { id } => self.forward(id, ToFace::BeginMove { id }),
            ToUi::BeginResize { id, edge } => self.forward(id, ToFace::BeginResize { id, edge }),
            ToUi::WindowMenu { id, x, y } => self.forward(id, ToFace::WindowMenu { id, x, y }),
            ToUi::Destroy { id } => {
                self.forward(id, ToFace::Destroy { id });
                self.wins.remove(&id);
            }
            ToUi::IdleInhibit { on } => super::macos::idle_assert(on, &mut self.idle),
            ToUi::Activate { id } => self.forward(id, ToFace::Activate { id }),
            ToUi::SetClipboard { payloads } => self.clip_seen = super::macos::write_pasteboard(&payloads),
            ToUi::FetchClipboard { token, from_drag, mime } => {
                let data = super::macos::read_pasteboard(from_drag, &mime);
                self.event(FromUi::ClipboardData { token, data });
            }
            ToUi::StartDrag { .. } => self.event(FromUi::DragFinished { action: dnd_action::NONE }),
            ToUi::CancelDrag | ToUi::DragFeedback { .. } => {}
            ToUi::Quit { clients } => {
                let all: Vec<FaceId> = self.faces.keys().copied().collect();
                for f in all {
                    self.send(f, &ToFace::Bye);
                }
                println!("wc-compositor: main process ends with {} face(s), {clients} client(s)", self.faces.len());
                return false;
            }
        }
        true
    }

    fn on_face(&mut self, fid: FaceId, m: FromFace) {
        match m {
            FromFace::Hello { section, ids, token, pid } => {
                let port = self.ports.remove(&token);
                if let Some(f) = self.faces.get_mut(&fid) {
                    (f.section, f.ids, f.token, f.hello) = (section.clone(), ids, token, true);
                    if port.is_some() {
                        f.port = port;
                    }
                }
                println!("wc-compositor: face {fid} is {section} (pid {pid}), surfaces by mach port: {}", port.is_some());
                // A face that comes back for windows a crash orphaned.
                let mut lost: Vec<(WindowId, bool)> = self
                    .wins
                    .iter()
                    .filter(|(_, w)| w.lost.as_ref().is_some_and(|(s, _)| *s == section))
                    .map(|(id, w)| (*id, matches!(w.kind, WindowKind::Popup { .. })))
                    .collect();
                lost.sort_by_key(|(id, popup)| (*popup, *id));
                for (id, _) in lost {
                    self.route(id, fid);
                }
            }
            FromFace::Ui(e) => {
                if let Some(lat) = self.latency.as_mut() {
                    let t = match &e {
                        FromUi::Key { time, .. } | FromUi::PointerButton { time, .. } | FromUi::PointerMotion { time, .. } => Some(*time),
                        _ => None,
                    };
                    if let Some(t) = t {
                        // The event's time is milliseconds, so this is the
                        // delay to within a millisecond, bounded below by 0.
                        let now = uptime_us();
                        lat.push(now.saturating_sub(u64::from(t) * 1000).min(10_000_000));
                        if lat.len() >= 100 {
                            lat.sort_unstable();
                            println!(
                                "wc-compositor: face input to main process, last {} events: p50 {} µs, p90 {} µs, max {} µs (AppKit's event time is in ms)",
                                lat.len(),
                                lat[lat.len() / 2],
                                lat[lat.len() * 9 / 10],
                                lat[lat.len() - 1]
                            );
                            lat.clear();
                        }
                    }
                }
                match &e {
                    FromUi::PointerEnter { .. } | FromUi::PointerMotion { .. } | FromUi::PointerButton { .. } => {
                        self.pointer_face = Some(fid);
                    }
                    FromUi::Activated { on: true, .. } => self.focus_face = Some(fid),
                    _ => {}
                }
                self.event(e);
            }
            FromFace::Bounds { id, rect } => {
                if let Some(w) = self.wins.get(&id) {
                    *w.bounds.lock().unwrap_or_else(|e| e.into_inner()) = rect;
                }
            }
            FromFace::Request { key } => {
                self.last_request = Some((fid, Instant::now()));
                // A face started again after a crash asks to run its
                // application like any face; it already has its windows back
                // (the replay on Hello, just before), so that would only open
                // another one.
                if key.is_empty() && self.wins.values().any(|w| w.face == Some(fid)) {
                    println!("wc-compositor: face {fid} asked to run its application and already shows its windows; not run again");
                    return;
                }
                let section = self.faces.get(&fid).map(|f| f.section.clone()).unwrap_or_default();
                if let Some(h) = HOOKS.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    (h.on_request)(&section, &key);
                }
            }
            // §106: what macOS asked the face's application to open. Each
            // becomes the request a Dock-menu page is (`open:<url>`); the
            // launcher maps a file to its Linux path before it is sent.
            FromFace::Open { urls } => {
                self.last_request = Some((fid, Instant::now()));
                let section = self.faces.get(&fid).map(|f| f.section.clone()).unwrap_or_default();
                if let Some(h) = HOOKS.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                    for u in urls.iter().take(super::faceproto::MAX_OPEN) {
                        (h.on_request)(&section, &format!("open:{u}"));
                    }
                }
            }
            FromFace::Bye => {
                if let Some(f) = self.faces.get_mut(&fid) {
                    f.bye = true;
                }
            }
        }
    }

    fn gone(&mut self, fid: FaceId) {
        let Some(f) = self.faces.remove(&fid) else { return };
        if let Some(p) = f.port {
            macmach::release(p);
        }
        for o in [&mut self.pointer_face, &mut self.focus_face] {
            if *o == Some(fid) {
                *o = None;
            }
        }
        let mine: Vec<WindowId> = self.wins.iter().filter(|(_, w)| w.face == Some(fid)).map(|(id, _)| *id).collect();
        println!(
            "wc-compositor: face {fid} ({}) {}, holding {} window(s)",
            f.section,
            if f.bye { "said goodbye" } else { "went away without a goodbye" },
            mine.len()
        );
        for id in &mine {
            if let Some(w) = self.wins.get_mut(id) {
                w.face = None;
                w.surf = None;
                w.lost = Some((f.section.clone(), Instant::now()));
            }
        }
        if !f.bye && !mine.is_empty()
            && let Some(h) = HOOKS.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        {
            (h.on_face_lost)(&f.section);
        }
        if f.bye {
            self.close_lost(&f.section, true);
        }
    }

    /// Ask the clients of windows nobody can show to close them.
    fn close_lost(&mut self, section: &str, now: bool) {
        let ids: Vec<WindowId> = self
            .wins
            .iter()
            .filter(|(_, w)| {
                w.kind == WindowKind::Toplevel
                    && w.lost.as_ref().is_some_and(|(s, t)| s == section && (now || t.elapsed() > LOST_GRACE))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(w) = self.wins.get_mut(&id) {
                w.lost = None;
            }
            println!("wc-compositor: window {id} has no face to come back to; asking its client to close it");
            self.event(FromUi::CloseRequested { id });
        }
    }

    fn tick(&mut self) -> bool {
        if let Some(mimes) = super::macos::clipboard_poll(&mut self.clip_seen) {
            self.event(FromUi::ClipboardChanged { mimes });
        }
        let now = super::macos::current_layout();
        if now != self.layout {
            self.layout = now;
            self.event(FromUi::Layout { name: now });
        }
        let sections: Vec<Section> = self.wins.values().filter_map(|w| w.lost.as_ref().map(|(s, _)| s.clone())).collect();
        for s in sections {
            self.close_lost(&s, false);
        }
        if self.faces.is_empty() {
            let t = *self.no_face_since.get_or_insert_with(Instant::now);
            if t.elapsed() > NO_FACE_EXIT && self.wins.values().all(|w| w.face.is_none()) {
                println!("wc-compositor: no face for {}s; the main process ends", NO_FACE_EXIT.as_secs());
                return false;
            }
        } else {
            self.no_face_since = None;
        }
        true
    }
}

/// The peer of a Unix socket is this user, or the connection is refused.
fn same_user(s: &UnixStream) -> bool {
    use std::os::fd::AsRawFd;
    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: a connected socket and two out-parameters.
    let ok = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0;
    // SAFETY: plain call.
    ok && uid == unsafe { libc::geteuid() }
}

impl Router {
    pub(crate) fn run(self) {
        let (tx, rx) = std::sync::mpsc::channel::<In>();
        // The mach service first: holding it is also what makes this the one
        // main process of the session.
        let service = match macmach::check_in(SERVICE) {
            Ok(p) => p,
            Err(kr) => {
                eprintln!("wc-compositor: {SERVICE} is held by another main process (bootstrap {kr}); this one ends");
                let _ = self.events.send(FromUi::Stopped);
                return;
            }
        };
        let path = socket_path();
        let dir = path.parent().expect("socket dir").to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("wc-compositor: {}: {e}", path.display());
                let _ = self.events.send(FromUi::Stopped);
                return;
            }
        };
        let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        println!("wc-compositor: main process: faces on {} and mach service {SERVICE}", path.display());

        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                while let Ok(cmd) = self.commands.recv() {
                    if tx.send(In::Ui(cmd)).is_err() {
                        break;
                    }
                }
            });
        }
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                loop {
                    if let Some((token, port)) = macmach::recv_hello(service, 1000)
                        && tx.send(In::Port(token, port)).is_err()
                    {
                        break;
                    }
                }
            });
        }
        {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut next: FaceId = 1;
                for conn in listener.incoming() {
                    let Ok(s) = conn else { continue };
                    if !same_user(&s) {
                        eprintln!("wc-compositor: refused a face connection from another user");
                        continue;
                    }
                    let fid = next;
                    next += 1;
                    let Ok(reader) = s.try_clone() else { continue };
                    if tx.send(In::Connected(fid, s)).is_err() {
                        break;
                    }
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let mut r = std::io::BufReader::new(reader);
                        while let Ok(Some(body)) = faceproto::read_frame(&mut r) {
                            match FromFace::decode(&body) {
                                Ok(m) => {
                                    if tx.send(In::Face(fid, m)).is_err() {
                                        return;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("wc-compositor: face {fid}: {e}");
                                    break;
                                }
                            }
                        }
                        let _ = tx.send(In::Gone(fid));
                    });
                }
            });
        }

        let mut r = R {
            events: self.events.clone(),
            faces: HashMap::new(),
            ports: HashMap::new(),
            wins: HashMap::new(),
            pointer_face: None,
            focus_face: None,
            last_request: None,
            clip_seen: super::macos::pasteboard_count(),
            layout: super::macos::current_layout(),
            idle: None,
            no_face_since: None,
            latency: std::env::var_os("WC_FACE_LATENCY").map(|_| Vec::new()),
            copies: CopyStats::default(),
        };
        let mut last_tick = Instant::now();
        loop {
            let msg = rx.recv_timeout(Duration::from_millis(250));
            match msg {
                Ok(In::Ui(cmd)) => {
                    if !r.handle(cmd) {
                        break;
                    }
                }
                Ok(In::Connected(fid, s)) => {
                    r.faces.insert(
                        fid,
                        Face { stream: s, section: String::new(), ids: Vec::new(), token: 0, port: None, hello: false, bye: false },
                    );
                }
                Ok(In::Face(fid, m)) => r.on_face(fid, m),
                Ok(In::Gone(fid)) => r.gone(fid),
                Ok(In::Port(token, port)) => {
                    if let Some(f) = r.faces.values_mut().find(|f| f.hello && f.token == token && f.port.is_none()) {
                        f.port = Some(port);
                    } else {
                        r.ports.insert(token, port);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if last_tick.elapsed() >= Duration::from_millis(250) {
                last_tick = Instant::now();
                if !r.tick() {
                    let all: Vec<FaceId> = r.faces.keys().copied().collect();
                    for f in all {
                        r.send(f, &ToFace::Bye);
                    }
                    break;
                }
            }
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::io::stdout().flush();
        let _ = self.events.send(FromUi::Stopped);
        super::wake_wayland();
    }
}
