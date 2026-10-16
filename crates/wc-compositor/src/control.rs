//! A socket that stands where Windows stands, so a conformance suite can drive
//! this compositor from outside the process.
//!
//! # Why this exists, and what it is *not*
//!
//! WLCS (docs/waycompose-design.md §26) is a shared library of Wayland conformance tests
//! that drives a compositor through three hooks: give me a client socket, move
//! that window to this absolute position, and move/click a pointer at these
//! compositor coordinates. The first hook is a `connect()`. The other two have
//! no answer in this project's architecture, because **this compositor does not
//! place windows and does not hit-test a pointer *between* windows.** Windows
//! does. A window is an `HWND`, `SetWindowPos` moves it, and the `WM_MOUSEMOVE`
//! that arrives has already been routed to the window under the cursor by the
//! system; `win32.rs` converts the coordinates and forwards them.
//!
//! **"Between windows" is load-bearing, and this comment claimed something
//! stronger until §40.** Routing *within* a window is not Windows' to do and
//! cannot be: a `wl_subsurface` has no `HWND` of its own, the whole surface
//! tree is composed into one window's pixels by `State::compose_tree`, and
//! `wl_surface.set_input_region` is a Wayland concept Win32 has never heard of.
//! So `State::surface_under` hit-tests the surface tree on *both* legs — it is
//! shipping code, not a harness stand-in — and what this file replaces is only
//! the outer question of which window the pointer is over. The stronger claim
//! is how the largest cluster of WLCS failures in the suite came to be read as
//! "not our layer" for as long as it was.
//!
//! So the two hooks have to be answered by something that stands in for
//! Windows. That is this file: **a synthetic Win32, at the same seam the real
//! one occupies.** It owns window positions, it does the between-windows hit
//! test, and it emits [`FromUi`] events — the *same* enum, into the *same*
//! channel, that `platform/win32.rs` fills from real window messages. Nothing
//! below that seam is replaced and nothing above it is bypassed.
//!
//! **State the consequence rather than bury it.** A WLCS pointer test that
//! passes here proves that everything from `FromUi` upwards is right: focus,
//! enter/leave bookkeeping, serials, `wl_pointer.frame`, surface-local
//! coordinates, popup grabs. It proves nothing about `WM_MOUSEMOVE`,
//! `TrackMouseEvent`, `SetCapture`, or the pixel-to-surface conversion in
//! `FrameStyle` — those are `win32.rs`, they are not on this path, and
//! docs/waycompose-design.md §13.6 is the record of what it costs to forget which half of
//! a claim a harness is making.
//!
//! # Unix only, and a flag rather than a build feature
//!
//! `--control` is accepted only where WLCS can run, which is Linux. It is a
//! runtime flag rather than a `cfg` so that the code is compiled by every
//! Linux build and linted by the ordinary gate; a test-only `cfg` is how a
//! harness path rots without anybody noticing (§14.9).
//!
//! # The protocol
//!
//! Newline-delimited requests on `AF_UNIX`/`SOCK_STREAM`, each answered by one
//! or more lines ending in `ok` or `err <reason>`. Text because the peer is a
//! C ABI shim in another process and the whole exchange is a few hundred
//! messages per test run; there is nothing here worth a binary encoding.
//!
//! ```text
//!   ping                                 -> ok
//!   windows                              -> window <id> <client> <surface> <x> <y> <w> <h>
//!                                           ... one per mapped window ...
//!                                           ok
//!   position <client> <surface> <x> <y>  -> ok | err unknown-surface
//!   pointer-move <x> <y>                 -> ok          (absolute, f64)
//!   pointer-button <evdev-code> <0|1>    -> ok
//!   pointer-leave                        -> ok
//!   touch-down <x> <y>                   -> ok | ok no-window
//!   touch-move <x> <y>                   -> ok | ok no-contact
//!   touch-up                             -> ok | ok no-contact
//! ```
//!
//! `<client>` is the connection's ordinal, counted from 1 in accept order, and
//! `<surface>` is the `wl_surface`'s protocol object id. Together they are the
//! only handle the suite has: WLCS knows a client-side `wl_surface*` and
//! nothing about our window ids, and the ordinal is what lets the shim say
//! *which* client's object id it means. Accept order is well defined — the
//! shim `connect()`s inside `create_client_socket` and the listen backlog is a
//! queue — so the two sides agree without a handshake.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};

use crate::platform::{Edge, FromUi, WindowId};

/// One window, as the synthetic Win32 sees it: a rectangle with an identity.
#[derive(Clone, Copy, Debug)]
pub struct Rect {
    pub id: WindowId,
    pub client: u32,
    pub surface: u32,
    pub w: i32,
    pub h: i32,
    /// An `xdg_toplevel` rather than an `xdg_popup`.
    ///
    /// Load-bearing for exactly one thing: a toplevel takes activation when it
    /// appears and a popup must not (docs/waycompose-design.md §15, `WindowKind::Popup` —
    /// a menu that stole the focus would dismiss itself). See
    /// [`Control::follow_focus`].
    pub toplevel: bool,
    /// For an `xdg_popup`: the window it is parented to, and where the
    /// compositor put it inside that parent's window geometry.
    ///
    /// ADDED FOR STAGE 38. **A popup is the one window this stand-in cannot be
    /// told the position of.** WLCS positions the windows it creates, and it
    /// never positions a popup, because in Wayland a popup's position is not
    /// the suite's to choose — the compositor computes it from the
    /// `xdg_positioner` and reports it in the configure. Until this field
    /// existed, every popup sat at (0, 0) in the hit test below, so
    /// `surface_actually_at_location` — which is how the whole
    /// `XdgPopupPositionerTest` family checks placement — asked whether the
    /// menu was under the cursor and was told about whatever was at the top
    /// left of the screen.
    pub popup_at: Option<(WindowId, i32, i32)>,
    /// [`crate::state::Window::content_offset`]: where the window's top-left
    /// pixel sits in the root surface's buffer.
    ///
    /// ADDED FOR §45.11, to fix a defect §40 introduced. It is `(0, 0)` for
    /// almost every window; it is the declared window-geometry inset for a
    /// client that set one; and it goes **negative** when a `wl_subsurface`
    /// hangs off the top or left of its parent, because §40 taught
    /// `compose_tree` that an undeclared geometry is "the full bounds of the
    /// surface, including any subsurfaces".
    ///
    /// That last case is why this field has to exist here. The window grows to
    /// contain the overhang — correctly — but the surface underneath it has not
    /// moved, so the window's top-left corner is now further left than the
    /// point the suite positioned. Without this, the hit test looks for the
    /// window at the old corner and every click inside the overhang misses it.
    pub content: (i32, i32),
}

/// What the Wayland side must be able to answer for the hit test to work.
///
/// A trait rather than a direct call into `State` so that the geometry
/// bookkeeping here is unit-testable without a display, a socket or a client.
pub trait Windows {
    /// Every **mapped** window, in stacking order, bottom first.
    fn mapped(&self) -> Vec<Rect>;
    /// Record the rectangle this window's popups must be constrained to, in
    /// that window's own window-geometry coordinates.
    ///
    /// ADDED FOR STAGE 38, and it is the stand-in's half of
    /// [`crate::platform::PopupBounds`]: on Windows the UI thread answers this
    /// from `MonitorFromWindow` and `GetMonitorInfo`, and here it is the one
    /// output minus wherever the suite put the window.
    fn set_popup_bounds(&self, id: WindowId, bounds: crate::surface::Rect);
    /// §85. A move (`None`) or resize (`Some(edge)`) the compositor has handed
    /// to the platform since the last turn. Win32 runs these in `SC_MOVE`'s
    /// modal loop; the stand-in has to run them itself. See [`Gesture`].
    fn take_gesture(&self) -> Option<(WindowId, Option<Edge>)> {
        None
    }
}

impl Windows for crate::state::State {
    fn mapped(&self) -> Vec<Rect> {
        self.mapped_windows()
    }

    fn set_popup_bounds(&self, id: WindowId, bounds: crate::surface::Rect) {
        self.set_popup_bounds(id, bounds);
    }

    fn take_gesture(&self) -> Option<(WindowId, Option<Edge>)> {
        self.take_gesture_request()
    }
}

/// §85. An interactive move or resize, run the way Windows runs `SC_MOVE` and
/// `SC_SIZE`: from the press that started it until that button comes up, the
/// pointer moves the window rather than reaching it, and the release is
/// consumed by the loop. Until §85 the stand-in had no window manager at all
/// and every such gesture was logged and dropped, which made the four WLCS
/// interactive move/resize tests "expected" failures on this leg and left the
/// two reactive-popup tests, which move their parent this way, untestable.
///
/// The edge arithmetic is `WM_SIZING`'s: a left or top edge moves the origin
/// and shrinks the window by the same amount, a right or bottom edge only
/// changes the size, and the new size is reported as `WM_SIZE` reports it.
struct Gesture {
    id: WindowId,
    key: (u32, u32),
    edge: Option<Edge>,
    pointer: (f64, f64),
    position: (i32, i32),
    size: (i32, i32),
}

/// A parsed request, and the channel its answer goes back on.
struct Job {
    line: String,
    reply: SyncSender<String>,
}

pub struct Control {
    jobs: Receiver<Job>,
    events: Sender<FromUi>,
    /// Window positions, in compositor coordinates, keyed by
    /// `(client, surface)` rather than by [`WindowId`].
    ///
    /// **Keyed by the client's names for it, on purpose.** WLCS positions a
    /// window before it has necessarily mapped, and a window that has not
    /// mapped has no id here yet; a position set against a
    /// `(client, surface)` pair survives the gap and applies the moment the
    /// window appears.
    positions: Vec<((u32, u32), (i32, i32))>,
    /// Where the pointer is, in compositor coordinates. `None` until the suite
    /// first moves it, which is not the same as (0, 0) — a pointer that has
    /// never moved must not have entered anything.
    at: Option<(f64, f64)>,
    /// The window the pointer is currently inside, so a move that leaves it
    /// produces a `leave` and a move that stays produces a `motion`.
    over: Option<WindowId>,
    /// Buttons the suite is holding down.
    ///
    /// Needed because a button held across a move must keep going to the
    /// window it was pressed on — that is `SetCapture` on Win32, and without
    /// it a drag that crosses a window edge loses its release
    /// (`platform/win32.rs` says the same thing from the other side).
    held: Vec<u32>,
    /// The window the live touch contact landed on, and where it is, in
    /// compositor coordinates.
    ///
    /// ADDED FOR §40. `None` between an up and the next down, which is the
    /// state a `wl_touch` client cannot observe and this side must still get
    /// right — a `touch-move` with no contact down is not a motion, it is a
    /// harness bug, and it is answered as one.
    ///
    /// **The window is remembered rather than re-hit-tested on every move,
    /// and that is the protocol rather than an optimisation.** `wl_touch` has
    /// no enter/leave: `down` names the surface the contact landed on and
    /// every later event for that contact belongs to that surface until it
    /// comes up. Win32 agrees — a contact is implicitly captured to the window
    /// that received its `WM_POINTERDOWN`, with no `SetCapture` — so the same
    /// rule holds on both legs, and it is the same rule `held` implements for
    /// the mouse by a different mechanism.
    touching: Option<WindowId>,
    /// Which window Windows would call the foreground one.
    ///
    /// See [`Control::follow_focus`] for why this is here and not left to the
    /// suite to ask for.
    focus: Option<WindowId>,
    /// Milliseconds, monotonic, since the control socket was bound. Every
    /// input event needs a timestamp and there is no message queue here to
    /// take one from.
    started: std::time::Instant,
    /// The one output, in compositor coordinates.
    ///
    /// ADDED FOR STAGE 38. This is the stand-in for the monitor: on Windows a
    /// popup is constrained to `GetMonitorInfo`'s `rcWork`, and here there is
    /// no monitor, no taskbar and no second screen — only the rectangle the
    /// compositor advertises as its `wl_output`, which is what WLCS positions
    /// its windows within and what its positioner tests expect a popup to be
    /// kept inside.
    output: (i32, i32),
    /// §85. The move or resize being run, if any. See [`Gesture`].
    gesture: Option<Gesture>,
}

impl Control {
    /// Bind `path` and start the thread that reads it.
    ///
    /// The listener thread is detached and lives as long as the process. It
    /// accepts one connection at a time and serves it to end of file, which is
    /// what the shim does: one connection per compositor, held for the life of
    /// the test.
    pub fn bind(path: &str, events: Sender<FromUi>, output: (i32, i32)) -> std::io::Result<Control> {
        // A stale socket from a killed run is the normal case on a machine
        // running ten worktrees, and `bind` on an existing path is
        // `EADDRINUSE` rather than anything more helpful.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        let (tx, jobs) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("wc-control".to_owned())
            .spawn(move || serve(listener, tx))?;
        println!("wc-compositor: control socket on {path}");
        let _ = std::io::stdout().flush();
        Ok(Control {
            jobs,
            events,
            positions: Vec::new(),
            at: None,
            over: None,
            held: Vec::new(),
            touching: None,
            focus: None,
            started: std::time::Instant::now(),
            output,
            gesture: None,
        })
    }

    /// Answer everything that has arrived since the last turn.
    ///
    /// Called from the Wayland loop, immediately before `pump_ui_events`, so
    /// that an event this produces is delivered to the client in the same turn
    /// the request arrived — otherwise every pointer move would cost a poll
    /// interval and a suite that moves the pointer a hundred times would spend
    /// half a second doing nothing.
    pub fn service(&mut self, windows: &impl Windows) {
        self.follow_focus(windows);
        self.publish_bounds(windows);
        if let Some((id, edge)) = windows.take_gesture() {
            self.begin_gesture(id, edge, windows);
        }
        loop {
            let job = match self.jobs.try_recv() {
                Ok(j) => j,
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => return,
            };
            let answer = self.handle(&job.line, windows);
            let _ = job.reply.send(answer);
        }
    }

    /// Give the newest mapped toplevel the activation and the keyboard focus,
    /// and take them off whatever had them.
    ///
    /// # This is not a convenience, and finding out cost a run
    ///
    /// WLCS's `ConfigurationWindow` ends its constructor with
    /// `dispatch_until([]{ return state.activated; })` and the comment *"WLCS
    /// makes the assumption that newly-mapped windows are active"*. That
    /// assumption is not in the protocol, and it is also **what Windows does**:
    /// `ShowWindow` on a `WS_OVERLAPPEDWINDOW` makes it the foreground window,
    /// which is a `WM_ACTIVATE` and a `WM_SETFOCUS`, which `win32.rs` turns
    /// into exactly the two events below. Without this, the *harness* — not
    /// the compositor — hangs every test that maps a window, for ten seconds
    /// each; a first attempt at this stage's baseline scored 63 suites as
    /// timeouts and the compositor was innocent of all of them.
    ///
    /// So it belongs here rather than in the suite's filter list: the synthetic
    /// Win32 has to do what Win32 does, and the fact that it was missing is a
    /// statement about this file, not about `state.rs`.
    ///
    /// **A popup does not take it.** `xdg_popup` is `WS_POPUP` with an owner
    /// and no activation (docs/waycompose-design.md §15.6); a menu that stole the focus
    /// would dismiss itself on the way up.
    fn follow_focus(&mut self, windows: &impl Windows) {
        let mapped = windows.mapped();
        // Topmost first, popups skipped: the foreground window is the last
        // toplevel to have appeared.
        let want = mapped.iter().rev().find(|r| r.toplevel).map(|r| r.id);
        if want == self.focus {
            return;
        }
        // `ShowWindow` sends `WM_SIZE`, and until §91 `win32.rs` turned it into
        // an `xdg_toplevel.configure` carrying the size the client had just
        // committed, so this stand-in sent a `Resized` here to match (and both
        // legs failed `XdgToplevelStableConfigurationTest.defaults`, correctly:
        // the compositor configured a size where WLCS expects none). §91 found
        // that configure locking Firefox's second window at its first, tiny
        // size: a client told its own size back has been *told* a size. Win32
        // now swallows the `WM_SIZE`s its own placement causes and says nothing
        // to a client that got what it committed, so neither does this.
        let time = self.now_ms();
        if let Some(old) = self.focus.take() {
            let _ = self.events.send(FromUi::Activated { id: old, on: false });
            let _ = self.events.send(FromUi::KeyboardFocus {
                id: old,
                on: false,
                caps: false,
                num: false,
            });
            let _ = time;
        }
        if let Some(new) = want {
            self.focus = Some(new);
            let _ = self.events.send(FromUi::Activated { id: new, on: true });
            // `caps` and `num` are both off. There is no machine here whose
            // lock keys could be on, and inventing a state would make the
            // first `wl_keyboard.modifiers` a fiction.
            let _ = self.events.send(FromUi::KeyboardFocus {
                id: new,
                on: true,
                caps: false,
                num: false,
            });
        }
    }

    /// Tell every mapped window where the screen is, from its own point of
    /// view.
    ///
    /// ADDED FOR STAGE 38. The compositor constrains a popup in its parent's
    /// coordinates, so the rectangle handed over is the output translated by
    /// minus the parent's position: a window at (5, 5) on a 1920x1080 output is
    /// told `(-5, -5, 1920, 1080)`, and a popup of that window at (-60, -40)
    /// is then visibly 55 pixels off the left edge and can be slid back on.
    ///
    /// Every turn rather than only when something moves. It is a handful of
    /// integers per window and it is the honest shape: on Win32 the answer can
    /// change without any window moving — the taskbar can be resized, a monitor
    /// can be unplugged — so a cache that is only refreshed on a move would be
    /// a promise this stand-in could not keep either.
    fn publish_bounds(&self, windows: &impl Windows) {
        let mapped = windows.mapped();
        let (ow, oh) = self.output;
        for r in &mapped {
            let (x, y) = self.origin(r, &mapped);
            windows.set_popup_bounds(r.id, crate::surface::Rect::new(-x, -y, ow, oh));
        }
    }

    fn now_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    /// §85. Start a [`Gesture`] if a button is still held over `id`, which is
    /// the condition Windows' modal loop starts under; otherwise do nothing,
    /// as `SC_MOVE` does nothing for a button that has already come up.
    fn begin_gesture(&mut self, id: WindowId, edge: Option<Edge>, windows: &impl Windows) {
        let (Some(at), false) = (self.at, self.held.is_empty()) else {
            println!("wc-compositor: [control] window {id} gesture ignored: no button held");
            return;
        };
        let mapped = windows.mapped();
        let Some(r) = mapped.iter().find(|r| r.id == id && r.toplevel) else { return };
        let key = (r.client, r.surface);
        self.gesture = Some(Gesture {
            id,
            key,
            edge,
            pointer: at,
            position: self.position_of(key.0, key.1),
            size: (r.w, r.h),
        });
        // The compositor sent `leave` when it handed the gesture over; the
        // stand-in's idea of where the pointer is must agree, so that the
        // first motion after the loop is an `enter`.
        self.over = None;
        println!(
            "wc-compositor: [control] window {id} {} started at {:.0},{:.0}",
            match edge {
                None => "move".to_owned(),
                Some(e) => format!("resize from {e:?}"),
            },
            at.0,
            at.1
        );
    }

    /// One pointer motion inside a [`Gesture`]: the window moves or resizes,
    /// and nothing reaches the client but the configure a resize implies.
    fn step_gesture(&mut self, x: f64, y: f64) {
        let Some(g) = self.gesture.as_ref() else { return };
        let (dx, dy) = ((x - g.pointer.0) as i32, (y - g.pointer.1) as i32);
        let (mut px, mut py) = g.position;
        let (mut w, mut h) = g.size;
        match g.edge {
            None => {
                px += dx;
                py += dy;
            }
            Some(e) => {
                let (left, right, top, bottom) = match e {
                    Edge::Left => (true, false, false, false),
                    Edge::Right => (false, true, false, false),
                    Edge::Top => (false, false, true, false),
                    Edge::Bottom => (false, false, false, true),
                    Edge::TopLeft => (true, false, true, false),
                    Edge::TopRight => (false, true, true, false),
                    Edge::BottomLeft => (true, false, false, true),
                    Edge::BottomRight => (false, true, false, true),
                    Edge::None => (false, false, false, false),
                };
                if left {
                    px += dx;
                    w -= dx;
                }
                if right {
                    w += dx;
                }
                if top {
                    py += dy;
                    h -= dy;
                }
                if bottom {
                    h += dy;
                }
                (w, h) = (w.max(1), h.max(1));
            }
        }
        let (id, key, resized) = (g.id, g.key, g.edge.is_some() && (w, h) != g.size);
        match self.positions.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = (px, py),
            None => self.positions.push((key, (px, py))),
        }
        if resized {
            let _ = self.events.send(FromUi::Resized { id, width: w, height: h });
        }
    }

    /// Where a window's top left corner is, in compositor coordinates.
    ///
    /// A toplevel is wherever the suite put it. **A popup is wherever the
    /// compositor put it**, which is an offset inside its parent — so the walk
    /// goes up the chain, and a submenu three levels deep resolves through its
    /// menu and its window. The recursion terminates because a popup's parent
    /// is always an older window and window ids only increase.
    fn origin(&self, r: &Rect, mapped: &[Rect]) -> (i32, i32) {
        let Some((parent, dx, dy)) = r.popup_at else {
            // The buffer origin the suite pinned, plus wherever the window's
            // top-left corner currently sits inside that buffer. See the
            // `position` handler for why the stored value is the first of those
            // and not the second.
            let (bx, by) = self.position_of(r.client, r.surface);
            return (bx + r.content.0, by + r.content.1);
        };
        match mapped.iter().find(|p| p.id == parent) {
            Some(p) => {
                let (px, py) = self.origin(p, mapped);
                (px + dx, py + dy)
            }
            // The parent is not mapped, which happens between a menu's
            // creation and its parent's first buffer. Its own offset is the
            // best available answer and is right whenever the parent is at the
            // origin, which is where an unpositioned window is.
            None => (dx, dy),
        }
    }

    fn handle(&mut self, line: &str, windows: &impl Windows) -> String {
        let mut it = line.split_whitespace();
        let Some(verb) = it.next() else {
            return "err empty\n".to_owned();
        };
        let mut num = || it.next().and_then(|s| s.parse::<f64>().ok());
        match verb {
            "ping" => "ok\n".to_owned(),
            "windows" => {
                let mut out = String::new();
                let mapped = windows.mapped();
                for r in &mapped {
                    let (x, y) = self.origin(r, &mapped);
                    out.push_str(&format!(
                        "window {} {} {} {x} {y} {} {}\n",
                        r.id, r.client, r.surface, r.w, r.h
                    ));
                }
                out.push_str("ok\n");
                out
            }
            "position" => {
                let (Some(c), Some(s), Some(x), Some(y)) = (num(), num(), num(), num()) else {
                    return "err position wants <client> <surface> <x> <y>\n".to_owned();
                };
                let key = (c as u32, s as u32);
                // **Stored as the root surface's buffer origin, not as the
                // window's top-left corner, and the difference only shows up
                // later.**
                //
                // WLCS says "put the window here", and at the moment it says so
                // the two are the same point for every surface it builds. They
                // stop being the same when the window's extent changes without
                // the surface moving — which is exactly what a `wl_subsurface`
                // hanging off the left of its parent does, since §40 made an
                // undeclared window geometry the bounds of the whole tree.
                //
                // Pinning the buffer origin is what makes the surface stay put
                // while the window grows around it, which is what every
                // compositor does and what the client is entitled to assume: a
                // subsurface appearing to the left must not drag the parent's
                // pixels rightwards across the screen.
                //
                // Subtracting the *current* inset is what converts the one to
                // the other, and it is exactly right at this instant by
                // construction — `origin` adds the inset straight back, so a
                // window whose geometry never changes is positioned precisely
                // where the suite asked, which is the case every earlier stage
                // measured.
                let inset = windows
                    .mapped()
                    .iter()
                    .find(|r| (r.client, r.surface) == key)
                    .map(|r| r.content)
                    .unwrap_or((0, 0));
                let val = (x as i32 - inset.0, y as i32 - inset.1);
                match self.positions.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => *v = val,
                    None => self.positions.push((key, val)),
                }
                // A window that moves out from under the pointer has to be
                // left, and one that moves under it has to be entered. Both
                // are what Win32 does for free when `SetWindowPos` runs.
                self.refresh(windows);
                "ok\n".to_owned()
            }
            "pointer-move" => {
                let (Some(x), Some(y)) = (num(), num()) else {
                    return "err pointer-move wants <x> <y>\n".to_owned();
                };
                self.at = Some((x, y));
                if self.gesture.is_some() {
                    self.step_gesture(x, y);
                } else {
                    self.refresh(windows);
                }
                "ok\n".to_owned()
            }
            "pointer-button" => {
                let (Some(code), Some(down)) = (num(), num()) else {
                    return "err pointer-button wants <code> <0|1>\n".to_owned();
                };
                let (code, down) = (code as u32, down != 0.0);
                // §85. Inside a gesture the loop owns the buttons: a release
                // that empties the held set ends it and reaches nobody, and the
                // pointer is re-entered wherever it now is.
                if self.gesture.is_some() {
                    if down {
                        if !self.held.contains(&code) {
                            self.held.push(code);
                        }
                    } else {
                        self.held.retain(|b| *b != code);
                    }
                    if self.held.is_empty()
                        && let Some(g) = self.gesture.take()
                    {
                        println!("wc-compositor: [control] window {} gesture ended", g.id);
                        self.refresh(windows);
                    }
                    return "ok\n".to_owned();
                }
                let Some(id) = self.over else {
                    // Not an error: WLCS is entitled to click on nothing, and
                    // a compositor that has nothing under the cursor sends
                    // nothing. Saying so is more useful than a silent ok.
                    return "ok no-window\n".to_owned();
                };
                if down {
                    if !self.held.contains(&code) {
                        self.held.push(code);
                    }
                } else {
                    self.held.retain(|b| *b != code);
                }
                let _ = self.events.send(FromUi::PointerButton {
                    id,
                    button: code,
                    pressed: down,
                    time: self.now_ms(),
                });
                "ok\n".to_owned()
            }
            // -------------------------------------------------------------
            // Touch. ADDED FOR §40.
            // -------------------------------------------------------------
            //
            // **The compositor has had touch since §20; this seam had not.**
            // `wl_seat` advertises the capability on this leg
            // (`platform::headless::has_touch`), `FromUi` has carried
            // `TouchDown`/`TouchMotion`/`TouchUp`/`TouchFrame` since §24, and
            // `State::touch_down` routes a contact through the same scene
            // lookup a click uses. The only thing missing was a way for the
            // suite to say a finger had landed — so every touch test failed,
            // and the shim's stub said "there is no touch anywhere in this
            // compositor", which had stopped being true two stages earlier.
            //
            // WLCS's touch device is a **single contact**: `touch_down`,
            // `touch_move`, `touch_up`, with no slot argument. So slot 0, and
            // the multi-contact bookkeeping in `crate::input::touch::Slots`
            // stays on the Win32 side where the `pointerId` mapping it exists
            // for actually happens. This leg cannot exercise it, and inventing
            // a second contact here would be testing the harness.
            "touch-down" => {
                let (Some(x), Some(y)) = (num(), num()) else {
                    return "err touch-down wants <x> <y>\n".to_owned();
                };
                // The between-windows hit test, done once, here — a contact is
                // captured to the window it lands on from this moment, so
                // unlike the pointer there is no `refresh` to re-run later.
                let mapped = windows.mapped();
                let hit = mapped.iter().rev().find(|r| {
                    let (wx, wy) = self.origin(r, &mapped);
                    x >= wx as f64
                        && y >= wy as f64
                        && x < (wx + r.w) as f64
                        && y < (wy + r.h) as f64
                });
                let Some(r) = hit else {
                    // A contact on the desktop. Not an error, and nothing to
                    // send: `wl_touch` has no way to say "a finger landed on
                    // nothing", so the down simply does not happen.
                    self.touching = None;
                    return "ok no-window\n".to_owned();
                };
                let (wx, wy) = self.origin(r, &mapped);
                self.touching = Some(r.id);
                let time = self.now_ms();
                let _ = self.events.send(FromUi::TouchDown {
                    id: r.id,
                    slot: 0,
                    x: x - wx as f64,
                    y: y - wy as f64,
                    // No digitiser, so no contact patch. `None` rather than a
                    // fabricated ellipse, for the reason `FromUi::TouchDown`
                    // gives: a shape event here would be a statement about
                    // hardware this leg does not have.
                    shape: None,
                    time,
                });
                let _ = self.events.send(FromUi::TouchFrame);
                "ok\n".to_owned()
            }
            "touch-move" => {
                let (Some(x), Some(y)) = (num(), num()) else {
                    return "err touch-move wants <x> <y>\n".to_owned();
                };
                let Some(id) = self.touching else {
                    return "ok no-contact\n".to_owned();
                };
                // Deliberately *not* re-hit-tested: the contact belongs to the
                // window it landed on. A move that leaves that window sends
                // coordinates outside it, which is what `wl_touch` says and
                // what Win32 does for a captured contact.
                let mapped = windows.mapped();
                let Some(r) = mapped.iter().find(|r| r.id == id) else {
                    // The window went away under the finger. `touch_up` will
                    // find nothing either; the contact is simply over.
                    self.touching = None;
                    return "ok window-gone\n".to_owned();
                };
                // §45.11: `origin`, not `position_of` — §38 made a popup's
                // position something only `origin` knows, and the touch path
                // added in §40 had been reading past it.
                let (wx, wy) = self.origin(r, &mapped);
                let time = self.now_ms();
                let _ = self.events.send(FromUi::TouchMotion {
                    id,
                    slot: 0,
                    x: x - wx as f64,
                    y: y - wy as f64,
                    shape: None,
                    time,
                });
                let _ = self.events.send(FromUi::TouchFrame);
                "ok\n".to_owned()
            }
            "touch-up" => {
                let Some(id) = self.touching.take() else {
                    return "ok no-contact\n".to_owned();
                };
                let time = self.now_ms();
                let _ = self.events.send(FromUi::TouchUp { id, slot: 0, time });
                let _ = self.events.send(FromUi::TouchFrame);
                "ok\n".to_owned()
            }
            "pointer-leave" => {
                if let Some(id) = self.over.take() {
                    let _ = self
                        .events
                        .send(FromUi::PointerLeave { id, time: self.now_ms() });
                }
                self.at = None;
                self.held.clear();
                "ok\n".to_owned()
            }
            other => format!("err unknown request {other:?}\n"),
        }
    }

    fn position_of(&self, client: u32, surface: u32) -> (i32, i32) {
        self.positions
            .iter()
            .find(|(k, _)| *k == (client, surface))
            .map(|(_, v)| *v)
            .unwrap_or((0, 0))
    }

    /// Recompute which window the pointer is over and emit whatever that
    /// implies.
    ///
    /// This is the whole of the synthetic Win32: topmost-first hit test,
    /// enter/leave when the answer changes, motion when it does not, and a
    /// held button pinning the answer the way `SetCapture` does.
    fn refresh(&mut self, windows: &impl Windows) {
        let Some((px, py)) = self.at else { return };
        let mapped = windows.mapped();
        let time = self.now_ms();

        // A held button captures: the events keep going to the window the
        // press landed on, even once the pointer has left it, and the
        // coordinates go negative or past the edge exactly as they do on
        // Win32 and as `wl_pointer` allows.
        let hit = if self.held.is_empty() {
            mapped.iter().rev().find(|r| {
                let (wx, wy) = self.origin(r, &mapped);
                px >= wx as f64
                    && py >= wy as f64
                    && px < (wx + r.w) as f64
                    && py < (wy + r.h) as f64
            })
        } else {
            self.over.and_then(|id| mapped.iter().find(|r| r.id == id))
        };

        match hit {
            Some(r) => {
                let (wx, wy) = self.origin(r, &mapped);
                let (lx, ly) = (px - wx as f64, py - wy as f64);
                if self.over != Some(r.id) {
                    if let Some(old) = self.over {
                        let _ = self.events.send(FromUi::PointerLeave { id: old, time });
                    }
                    self.over = Some(r.id);
                    let _ = self.events.send(FromUi::PointerEnter {
                        id: r.id,
                        x: lx,
                        y: ly,
                        time,
                    });
                } else {
                    let _ = self.events.send(FromUi::PointerMotion {
                        id: r.id,
                        x: lx,
                        y: ly,
                        time,
                    });
                }
            }
            None => {
                if let Some(old) = self.over.take() {
                    let _ = self.events.send(FromUi::PointerLeave { id: old, time });
                }
            }
        }
    }
}

/// The listener thread: one connection at a time, one line at a time.
fn serve(listener: UnixListener, jobs: Sender<Job>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if converse(stream, &jobs).is_err() {
            // The peer went away mid-request. Normal at the end of a test.
        }
    }
}

fn converse(stream: UnixStream, jobs: &Sender<Job>) -> std::io::Result<()> {
    let mut out = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        // A rendezvous channel, so this thread blocks until the Wayland loop
        // has *done* the thing rather than merely queued it. WLCS calls
        // `move_absolute` and then waits for the client to see the motion; a
        // reply that raced the dispatch would turn every such test into a
        // timeout on a loaded machine and a pass on an idle one.
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        if jobs.send(Job { line, reply: tx }).is_err() {
            return Ok(());
        }
        let Ok(answer) = rx.recv() else { return Ok(()) };
        out.write_all(answer.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    struct Fake(Vec<Rect>, std::cell::RefCell<Vec<(WindowId, crate::surface::Rect)>>);
    impl Fake {
        fn new(windows: Vec<Rect>) -> Fake {
            Fake(windows, std::cell::RefCell::new(Vec::new()))
        }
    }
    impl Windows for Fake {
        fn mapped(&self) -> Vec<Rect> {
            self.0.clone()
        }
        fn set_popup_bounds(&self, id: WindowId, bounds: crate::surface::Rect) {
            self.1.borrow_mut().push((id, bounds));
        }
    }

    fn control() -> (Control, Receiver<FromUi>) {
        let (tx, rx) = channel();
        let (_jt, jobs) = channel();
        (
            Control {
                jobs,
                events: tx,
                positions: Vec::new(),
                at: None,
                over: None,
                held: Vec::new(),
                touching: None,
                focus: None,
                started: std::time::Instant::now(),
                output: (800, 600),
                gesture: None,
            },
            rx,
        )
    }

    fn win(id: WindowId, client: u32, surface: u32, w: i32, h: i32) -> Rect {
        Rect { id, client, surface, w, h, toplevel: true, popup_at: None, content: (0, 0) }
    }

    /// A popup of `parent`, at `(x, y)` inside it — what the compositor's
    /// positioner decided, which is the only place this position exists.
    fn popup(id: WindowId, parent: WindowId, x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect {
            id,
            client: 1,
            surface: 100 + id as u32,
            w,
            h,
            toplevel: false,
            popup_at: Some((parent, x, y)),
            content: (0, 0),
        }
    }

    /// A window whose tree grew to the **left**, which is the case §45.11
    /// exists for.
    ///
    /// The suite positions a 100x50 window at `(200, 100)`. A subsurface then
    /// appears 40 px to its left, so `compose_tree` grows the window to 140
    /// wide and its top-left corner becomes `(160, 100)` — the surface has not
    /// moved, the window has grown around it.
    ///
    /// A click at `x = 170` is inside the overhang. Before §45.11 the hit test
    /// still looked for the window at `x >= 200` and missed it entirely.
    #[test]
    fn a_window_that_grew_leftwards_is_hit_in_the_overhang() {
        let (mut c, rx) = control();
        // Positioned while the window is still 100x50 with no overhang.
        let f = Fake::new(vec![win(1, 1, 3, 100, 50)]);
        assert_eq!(c.handle("position 1 3 200 100", &f), "ok\n");
        // The subsurface arrives: 40 px of overhang on the left.
        let mut grown = win(1, 1, 3, 140, 50);
        grown.content = (-40, 0);
        let g = Fake::new(vec![grown]);
        assert_eq!(c.handle("pointer-move 170 120", &g), "ok\n");
        match rx.try_recv() {
            Ok(FromUi::PointerEnter { id, x, y, .. }) => {
                assert_eq!(id, 1);
                // Surface-local: the window's corner is at (160, 100), so the
                // point is 10 px into it.
                assert_eq!((x, y), (10.0, 20.0));
            }
            other => panic!("expected an enter in the overhang, got {other:?}"),
        }
    }

    /// The mirror of the above: a declared window-geometry inset must **not**
    /// move the window, because the suite already compensated for it when it
    /// said where to put the window.
    ///
    /// This is the case every stage before §45.11 measured, and the one the
    /// buffer-origin bookkeeping has to leave exactly alone.
    #[test]
    fn a_declared_inset_leaves_the_window_where_it_was_put() {
        let (mut c, rx) = control();
        let mut inset = win(1, 1, 3, 100, 50);
        inset.content = (12, 5);
        let f = Fake::new(vec![inset]);
        assert_eq!(c.handle("position 1 3 200 100", &f), "ok\n");
        assert_eq!(c.handle("pointer-move 210 120", &f), "ok\n");
        match rx.try_recv() {
            Ok(FromUi::PointerEnter { id, x, y, .. }) => {
                assert_eq!(id, 1);
                assert_eq!((x, y), (10.0, 20.0));
            }
            other => panic!("expected an enter at the position given, got {other:?}"),
        }
    }

    #[test]
    fn a_move_over_a_window_enters_it_in_surface_coordinates() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 100, 50)]);
        assert_eq!(c.handle("position 1 3 200 100", &f), "ok\n");
        assert_eq!(c.handle("pointer-move 210 120", &f), "ok\n");
        match rx.try_recv() {
            Ok(FromUi::PointerEnter { id, x, y, .. }) => {
                assert_eq!(id, 1);
                assert_eq!((x, y), (10.0, 20.0));
            }
            other => panic!("expected an enter, got {other:?}"),
        }
    }

    #[test]
    fn a_second_move_inside_the_same_window_is_a_motion_not_an_enter() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 100, 50)]);
        c.handle("pointer-move 10 10", &f);
        c.handle("pointer-move 20 20", &f);
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerEnter { .. })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerMotion { x, y, .. }) if x == 20.0 && y == 20.0));
    }

    #[test]
    fn leaving_the_rectangle_leaves_the_window() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(7, 1, 3, 100, 50)]);
        c.handle("pointer-move 10 10", &f);
        c.handle("pointer-move 10 400", &f);
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerEnter { .. })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerLeave { id: 7, .. })));
    }

    #[test]
    fn the_topmost_window_wins_and_stacking_order_is_the_list_order() {
        let (mut c, rx) = control();
        // Both at the origin and overlapping; the later one is on top.
        let f = Fake::new(vec![win(1, 1, 3, 100, 100), win(2, 1, 4, 100, 100)]);
        c.handle("pointer-move 50 50", &f);
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerEnter { id: 2, .. })));
    }

    /// §85. A move started with the button down drags the window with the
    /// pointer, sends the client nothing while it runs, eats the release, and
    /// leaves the pointer entered at the same point of the window it started
    /// from — which is WLCS `surface_can_be_moved_interactively` in miniature.
    #[test]
    fn a_move_drags_the_window_and_eats_its_release() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 100, 100)]);
        c.handle("position 1 3 100 100", &f);
        c.handle("pointer-move 105 105", &f);
        c.handle("pointer-button 272 1", &f);
        while rx.try_recv().is_ok() {}
        c.begin_gesture(1, None, &f);
        c.handle("pointer-move 165 65", &f);
        assert!(rx.try_recv().is_err(), "nothing reaches the client during the loop");
        assert_eq!(c.handle("pointer-button 272 0", &f), "ok\n");
        assert_eq!(c.position_of(1, 3), (160, 60));
        // The release is consumed and the pointer re-enters where it is.
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerEnter { id: 1, x, y, .. }) if (x, y) == (5.0, 5.0)));
        assert!(rx.try_recv().is_err(), "no button event after the loop");
    }

    /// §85. A top-left resize moves the corner with the pointer and reports
    /// the smaller size, as `WM_SIZING` from that corner does.
    #[test]
    fn a_top_left_resize_moves_the_corner_and_reports_the_size() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 400, 300)]);
        c.handle("position 1 3 100 100", &f);
        c.handle("pointer-move 105 105", &f);
        c.handle("pointer-button 272 1", &f);
        while rx.try_recv().is_ok() {}
        c.begin_gesture(1, Some(Edge::TopLeft), &f);
        c.handle("pointer-move 165 65", &f);
        assert!(matches!(rx.try_recv(), Ok(FromUi::Resized { id: 1, width: 340, height: 340 })));
        c.handle("pointer-button 272 0", &f);
        assert_eq!(c.position_of(1, 3), (160, 60));
    }

    /// §85. A gesture asked for after the button came up is not started:
    /// `SC_MOVE` does nothing then either, and a stand-in that moved the
    /// window anyway would be kinder than Windows.
    #[test]
    fn a_gesture_without_a_held_button_does_nothing() {
        let (mut c, _rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 100, 100)]);
        c.handle("pointer-move 50 50", &f);
        c.begin_gesture(1, None, &f);
        assert!(c.gesture.is_none());
    }

    #[test]
    fn a_held_button_captures_the_pointer_across_the_edge() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 100, 100)]);
        c.handle("pointer-move 50 50", &f);
        c.handle("pointer-button 272 1", &f);
        // Off the right-hand edge. Win32 with `SetCapture` keeps delivering,
        // and so does this, with a coordinate past the surface.
        c.handle("pointer-move 150 50", &f);
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerEnter { .. })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerButton { pressed: true, .. })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerMotion { id: 1, x, .. }) if x == 150.0));
    }

    #[test]
    fn a_release_ends_the_capture() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 100, 100)]);
        c.handle("pointer-move 50 50", &f);
        c.handle("pointer-button 272 1", &f);
        c.handle("pointer-button 272 0", &f);
        c.handle("pointer-move 500 500", &f);
        while let Ok(e) = rx.try_recv() {
            if let FromUi::PointerLeave { id, .. } = e {
                assert_eq!(id, 1);
                return;
            }
        }
        panic!("the pointer never left after the capture ended");
    }

    #[test]
    fn a_click_with_nothing_under_the_cursor_is_answered_rather_than_sent() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 10, 10)]);
        c.handle("pointer-move 500 500", &f);
        assert_eq!(c.handle("pointer-button 272 1", &f), "ok no-window\n");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn windows_reports_the_position_that_was_set() {
        let (mut c, _rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 64, 48)]);
        c.handle("position 1 3 12 34", &f);
        assert_eq!(c.handle("windows", &f), "window 1 1 3 12 34 64 48\nok\n");
    }

    #[test]
    fn an_unmoved_window_is_at_the_origin() {
        let (mut c, _rx) = control();
        let f = Fake::new(vec![win(9, 2, 5, 1, 1)]);
        assert_eq!(c.handle("windows", &f), "window 9 2 5 0 0 1 1\nok\n");
    }

    #[test]
    fn moving_a_window_out_from_under_the_pointer_leaves_it() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(4, 1, 3, 50, 50)]);
        c.handle("pointer-move 10 10", &f);
        c.handle("position 1 3 900 900", &f);
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerEnter { .. })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerLeave { id: 4, .. })));
    }

    #[test]
    fn a_newly_mapped_toplevel_takes_the_activation_and_the_focus() {
        let (mut c, rx) = control();
        c.follow_focus(&Fake::new(vec![win(3, 1, 2, 10, 10)]));
        assert!(matches!(rx.try_recv(), Ok(FromUi::Activated { id: 3, on: true })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::KeyboardFocus { id: 3, on: true, .. })));
    }

    /// §91. Showing a window does not tell the client its own size back: a
    /// configure with a size is an instruction, and Firefox obeyed the one that
    /// echoed its first, tiny commit for the rest of the window's life.
    #[test]
    fn showing_a_window_does_not_tell_it_its_own_size_back() {
        let (mut c, rx) = control();
        c.follow_focus(&Fake::new(vec![win(1, 1, 2, 200, 320)]));
        while let Ok(m) = rx.try_recv() {
            assert!(!matches!(m, FromUi::Resized { .. }), "a configure went out at map: {m:?}");
        }
    }

    #[test]
    fn the_focus_is_not_re_sent_every_turn() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(3, 1, 2, 10, 10)]);
        c.follow_focus(&f);
        while rx.try_recv().is_ok() {}
        c.follow_focus(&f);
        assert!(rx.try_recv().is_err(), "an unchanged foreground window said so again");
    }

    #[test]
    fn a_second_toplevel_takes_it_off_the_first() {
        let (mut c, rx) = control();
        c.follow_focus(&Fake::new(vec![win(1, 1, 2, 10, 10)]));
        while rx.try_recv().is_ok() {}
        c.follow_focus(&Fake::new(vec![win(1, 1, 2, 10, 10), win(2, 1, 3, 10, 10)]));
        assert!(matches!(rx.try_recv(), Ok(FromUi::Activated { id: 1, on: false })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::KeyboardFocus { id: 1, on: false, .. })));
        assert!(matches!(rx.try_recv(), Ok(FromUi::Activated { id: 2, on: true })));
    }

    #[test]
    fn a_popup_does_not_take_the_activation() {
        let (mut c, rx) = control();
        let mut popup = win(2, 1, 3, 10, 10);
        popup.toplevel = false;
        c.follow_focus(&Fake::new(vec![win(1, 1, 2, 10, 10), popup]));
        assert!(matches!(rx.try_recv(), Ok(FromUi::Activated { id: 1, on: true })));
    }

    #[test]
    fn the_last_window_going_away_takes_the_focus_with_it() {
        let (mut c, rx) = control();
        c.follow_focus(&Fake::new(vec![win(1, 1, 2, 10, 10)]));
        while rx.try_recv().is_ok() {}
        c.follow_focus(&Fake::new(vec![]));
        assert!(matches!(rx.try_recv(), Ok(FromUi::Activated { id: 1, on: false })));
    }

    // --- popups, stage 38 --------------------------------------------------

    #[test]
    fn a_popup_is_hit_tested_where_the_compositor_put_it() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 400, 400), popup(2, 1, 30, 40, 60, 50)]);
        c.handle("position 1 3 200 100", &f);
        // 200+30+5, 100+40+5: inside the popup, and inside the parent too, so
        // this also proves the popup is on top.
        c.handle("pointer-move 235 145", &f);
        match rx.try_recv() {
            Ok(FromUi::PointerEnter { id, x, y, .. }) => {
                assert_eq!(id, 2, "the pointer entered the parent, not the menu");
                assert_eq!((x, y), (5.0, 5.0));
            }
            other => panic!("expected an enter on the popup, got {other:?}"),
        }
    }

    /// Before stage 38 every popup was at the origin here, so this is also the
    /// regression: a click at the top left of the screen must no longer land on
    /// a menu that is in the middle of it.
    #[test]
    fn a_popup_is_not_at_the_origin_merely_because_nobody_positioned_it() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 10, 10), popup(2, 1, 300, 300, 60, 50)]);
        c.handle("position 1 3 500 500", &f);
        c.handle("pointer-move 5 5", &f);
        assert!(
            rx.try_recv().is_err(),
            "something was under a cursor at the origin, with both windows far from it"
        );
    }

    #[test]
    fn a_submenu_resolves_through_the_menu_it_hangs_off() {
        let (mut c, rx) = control();
        let f = Fake::new(vec![
            win(1, 1, 3, 400, 400),
            popup(2, 1, 30, 40, 60, 50),
            popup(3, 2, 60, 10, 60, 50),
        ]);
        c.handle("position 1 3 200 100", &f);
        // 200 + 30 + 60 + 1, 100 + 40 + 10 + 1.
        c.handle("pointer-move 291 151", &f);
        assert!(matches!(rx.try_recv(), Ok(FromUi::PointerEnter { id: 3, .. })));
    }

    #[test]
    fn windows_reports_a_popup_at_its_place_on_the_screen() {
        let (mut c, _rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 400, 400), popup(2, 1, -30, 40, 60, 50)]);
        c.handle("position 1 3 200 100", &f);
        assert_eq!(
            c.handle("windows", &f),
            "window 1 1 3 200 100 400 400\nwindow 2 1 102 170 140 60 50\nok\n"
        );
    }

    /// The rectangle the compositor constrains against, in the window's own
    /// coordinates. The sign is the thing: a window at (200, 100) has the
    /// screen starting 200 pixels to its left.
    #[test]
    fn a_window_is_told_where_the_screen_is_from_where_it_stands() {
        let (mut c, _rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 400, 400)]);
        c.handle("position 1 3 200 100", &f);
        c.publish_bounds(&f);
        assert_eq!(
            f.1.borrow().last(),
            Some(&(1, crate::surface::Rect::new(-200, -100, 800, 600)))
        );
    }

    /// A menu's own submenus are constrained against the same screen, and the
    /// menu is not at the origin either.
    #[test]
    fn a_popup_is_told_where_the_screen_is_too() {
        let (mut c, _rx) = control();
        let f = Fake::new(vec![win(1, 1, 3, 400, 400), popup(2, 1, 30, 40, 60, 50)]);
        c.handle("position 1 3 200 100", &f);
        c.publish_bounds(&f);
        assert_eq!(
            f.1.borrow().last(),
            Some(&(2, crate::surface::Rect::new(-230, -140, 800, 600)))
        );
    }

    #[test]
    fn an_unknown_request_is_an_error_rather_than_a_silent_ok() {
        let (mut c, _rx) = control();
        let f = Fake::new(vec![]);
        assert!(c.handle("teleport 1 2", &f).starts_with("err "));
    }
}
