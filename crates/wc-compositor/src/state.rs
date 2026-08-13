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
use smithay::utils::SERIAL_COUNTER;
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
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_seat, delegate_shm, delegate_xdg_shell};

use crate::platform::{FromUi, IconImage, ToUi, Ui, WindowId};
use crate::surface::{PixelError, PixelSource, Rect, Shadow, ShmSource};

/// One toplevel, from the Wayland side.
struct Window {
    id: WindowId,
    toplevel: ToplevelSurface,
    /// Shared with the UI thread; see `platform`'s header for why pixels live
    /// here rather than in the command queue.
    shadow: Arc<Mutex<Shadow>>,
    mapped: bool,
    title: String,
    app_id: String,
    /// Callbacks the client is waiting on before it draws its next frame.
    ///
    /// Held rather than answered immediately. Answering at once is legal and is
    /// what a naive compositor does; it also turns every client that draws in
    /// response to `frame` into a spin loop that renders as fast as the CPU
    /// allows, which is `weston-simple-shm` at several thousand frames a second
    /// and a machine that sounds broken.
    frame_callbacks: Vec<WlCallback>,
    presented: u64,
}

pub struct State {
    compositor: CompositorState,
    shm: ShmState,
    xdg_shell: XdgShellState,
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
    /// Said once, the first time a client sets a cursor. See [`SeatHandler`].
    cursor_noted: bool,

    ui: Ui,
    events: Receiver<FromUi>,
    /// Tried in order until one recognises the buffer. One entry today; see
    /// `surface.rs` for why it is a list.
    sources: Vec<Box<dyn PixelSource>>,
    windows: HashMap<WlSurface, Window>,
    next_id: WindowId,
    started: Instant,
    last_frame: Instant,
    /// Synthesise an icon at map time, so the platform's icon path is executed
    /// by something rather than merely compiled.
    debug_icon: bool,

    pub clients_seen: usize,
    pub toplevels: usize,
    pub presents: usize,
    pub stopped: bool,
}

/// Per-client data.
#[derive(Default)]
pub struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        println!("wc-compositor: client {client_id:?} initialized");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        println!("wc-compositor: client {client_id:?} disconnected: {reason:?}");
    }
}

/// How fast a held key repeats, and how long before it starts.
///
/// **The compositor does not repeat anything.** `wl_keyboard.repeat_info` is
/// advertised and the *client* runs the timer, which is how the protocol is
/// specified and is why a Wayland client's key repeat keeps working while the
/// compositor is busy. Windows repeats too — a held key produces a stream of
/// `WM_KEYDOWN` with the "previous state" bit set — and forwarding those as
/// well would give a client two repeat streams at different rates. So they are
/// dropped, in `win32.rs`, and this is the only rate anyone sees.
///
/// The numbers are Windows' own defaults expressed the way Wayland wants them:
/// `SPI_GETKEYBOARDDELAY` 1 is 500 ms, `SPI_GETKEYBOARDSPEED` is around 30/s at
/// its default. Reading the real values is a `SystemParametersInfo` call and is
/// noted in the design as not done, rather than guessed at silently.
const REPEAT_DELAY_MS: i32 = 500;
const REPEAT_RATE_HZ: i32 = 30;

impl State {
    pub fn new(
        dh: &DisplayHandle,
        ui: Ui,
        events: Receiver<FromUi>,
        debug_icon: bool,
        layout: &str,
    ) -> Self {
        let mut seat_state = SeatState::new();
        // The local is dropped at the end of this function and that is safe:
        // `SeatState` keeps every seat it makes, and `Seat` is an `Arc` handle
        // rather than the seat itself.
        let mut seat: Seat<State> = seat_state.new_wl_seat(dh, "seat0");

        // **Capabilities are advertised by adding them, and only then.**
        // smithay recomputes `wl_seat.capabilities` from what the seat has and
        // sends it to every bound `wl_seat`, so the ordering requirement — a
        // capability must not be announced before the thing it names exists —
        // is met by construction rather than by getting a sequence right. A
        // client that binds later gets the current set in its `bind`.
        let pointer = seat.add_pointer();

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
        let keyboard = chosen.and_then(|l| {
            match seat.add_keyboard(
                XkbConfig { keymap: Some(l.keymap), ..XkbConfig::default() },
                REPEAT_DELAY_MS,
                REPEAT_RATE_HZ,
            ) {
                Ok(k) => {
                    println!(
                        "wc-compositor: seat0 keyboard: layout {} ({} bytes of keymap), \
                         repeat {REPEAT_RATE_HZ}/s after {REPEAT_DELAY_MS} ms",
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
        println!("wc-compositor: seat0 capabilities: pointer{}", if keyboard.is_some() { ", keyboard" } else { "" });

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

        State {
            compositor: CompositorState::new::<State>(dh),
            shm: ShmState::new::<State>(dh, Vec::new()),
            xdg_shell: XdgShellState::new::<State>(dh),
            seat_state,
            keyboard,
            pointer,
            keyboard_focus: None,
            pointer_focus: None,
            pointer_at: (0.0, 0.0),
            cursor_noted: false,
            ui,
            events,
            sources: vec![Box::new(ShmSource)],
            windows: HashMap::new(),
            next_id: 1,
            started: Instant::now(),
            last_frame: Instant::now(),
            debug_icon,
            clients_seen: 0,
            toplevels: 0,
            presents: 0,
            stopped: false,
        }
    }

    /// Milliseconds since the compositor started, which is what a frame
    /// callback carries.
    ///
    /// Note the known wrongness, recorded in docs/design.md §7.7 item 5: the
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

    /// Drain everything the UI thread has said since the last turn.
    pub fn pump_ui_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                FromUi::Stopped => self.stopped = true,
                FromUi::Resized { id, width, height } => self.on_resized(id, width, height),
                FromUi::CloseRequested { id } => {
                    if let Some(w) = self.by_id(id) {
                        println!("wc-compositor: window {id} close requested -> xdg_toplevel.close");
                        w.toplevel.send_close();
                    }
                }
                FromUi::Maximized { id, on } => {
                    self.set_state(id, |s| toggle(s, xdg_state::Maximized, on))
                }
                FromUi::Minimized { id, on } => self.set_state(id, |s| {
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
                }),
                FromUi::Activated { id, on } => {
                    self.set_state(id, |s| toggle(s, xdg_state::Activated, on))
                }
                FromUi::KeyboardFocus { id, on } => self.set_keyboard_focus(id, on),
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
            }
        }
    }

    fn by_id(&self, id: WindowId) -> Option<&Window> {
        self.windows.values().find(|w| w.id == id)
    }

    fn surface_of(&self, id: WindowId) -> Option<WlSurface> {
        self.by_id(id).map(|w| w.toplevel.wl_surface().clone())
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
            keyboard.set_focus(self, Some(surface), SERIAL_COUNTER.next_serial());
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
            keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
        }
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
        let focus = id.and_then(|id| self.surface_of(id).map(|s| (id, s)));
        match &focus {
            Some((id, _)) => {
                self.pointer_focus = Some(*id);
                self.pointer_at = (x, y);
            }
            None => self.pointer_focus = None,
        }
        pointer.motion(
            self,
            // The second element is the surface's origin in compositor space.
            // Every window here is its own `HWND` with its own coordinate
            // system and the platform has already subtracted the frame, so the
            // surface origin *is* the compositor origin and this is `(0, 0)`.
            // It is written out rather than assumed because the moment a scene
            // graph exists — subsurfaces, popups — it stops being true, and a
            // hard-coded zero somewhere else would be found by a popup landing
            // in the wrong place.
            focus.map(|(_, s)| (s, (0.0, 0.0).into())),
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
        if self.pointer_focus != Some(id) {
            return;
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
        window.toplevel.with_pending_state(|state| {
            state.size = Some((width, height).into());
        });
        if let Some(serial) = window.toplevel.send_pending_configure() {
            println!(
                "wc-compositor: window {id} configure {width}x{height} (serial {})",
                u32::from(serial)
            );
        }
    }

    fn set_state<F: FnOnce(&mut ToplevelState)>(&mut self, id: WindowId, f: F) {
        let Some(window) = self.by_id(id) else { return };
        window.toplevel.with_pending_state(f);
        window.toplevel.send_pending_configure();
    }

    /// Copy a committed buffer into the shadow and tell the platform.
    fn present(&mut self, surface: &WlSurface, buffer: &wl_buffer::WlBuffer, damage: Vec<Rect>) {
        let Some(window) = self.windows.get(surface) else {
            return;
        };
        let id = window.id;
        let shadow = window.shadow.clone();

        // Ask each source in turn. Today there is one; the loop is what lets a
        // buffer that lives in an arena rather than a `wl_shm` pool arrive
        // without this function changing (docs/goal-firefox.md).
        let mut result: Option<(Vec<Rect>, i32, i32)> = None;
        let mut served_by = "";
        for source in &self.sources {
            let read = source.with_pixels(buffer, &mut |bytes, spec| {
                let mut shadow = shadow.lock().unwrap_or_else(|e| e.into_inner());
                result = Some((shadow.blit(bytes, spec, &damage), spec.width, spec.height));
            });
            match read {
                Ok(()) => {
                    served_by = source.name();
                    break;
                }
                Err(PixelError::UnknownSource) => continue,
                Err(e) => {
                    eprintln!("wc-compositor: window {id}: {e}");
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

        let Some((written, w, h)) = result else {
            eprintln!("wc-compositor: window {id}: no pixel source could read the committed buffer");
            return;
        };
        if written.is_empty() {
            return;
        }

        let window = self.windows.get_mut(surface).expect("checked above");
        window.presented += 1;
        let first = !window.mapped;
        window.mapped = true;
        let (title, app_id) = (window.title.clone(), window.app_id.clone());
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
            self.ui.send(ToUi::Map {
                id,
                width: w,
                height: h,
            });
            if !title.is_empty() {
                self.ui.send(ToUi::Title { id, title });
            }
            if !app_id.is_empty() {
                self.ui.send(ToUi::AppId { id, app_id });
            }
            if self.debug_icon {
                self.ui.send(ToUi::Icon {
                    id,
                    icon: Some(debug_icon(id)),
                });
            }
        }
        self.ui.send(ToUi::Present {
            id,
            damage: written,
        });
    }
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

        match buffer {
            Some(BufferAssignment::NewBuffer(buffer)) => self.present(surface, &buffer, rects),
            Some(BufferAssignment::Removed) => {
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
                }
            }
            // A commit with no new buffer: state-only, or an ack. Damage
            // without a buffer means nothing.
            None => {}
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

    /// The client wants to draw the cursor itself.
    ///
    /// **Not implemented, and the reason is worth stating rather than leaving
    /// as an empty body.** `wl_pointer.set_cursor` hands over a surface whose
    /// buffer is the cursor image, and turning that into something Win32 shows
    /// means `CreateIconIndirect` per frame plus a `SetCursor` from the window
    /// procedure's `WM_SETCURSOR` — the same shape as `set_icon` in
    /// `win32.rs`, which already exists. What is missing is not the conversion
    /// but the *policy*: hiding the system arrow while the client's cursor is
    /// up, and putting it back on leave, is state that belongs to the window
    /// and there is nothing to test it against until a client that sets one is
    /// in the harness.
    ///
    /// The visible consequence is that the arrow the user sees is Windows',
    /// which for a terminal is the same arrow the client would have drawn, and
    /// for a text field is the wrong shape. Said once so it is in the log of
    /// any run where it matters.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        if !self.cursor_noted {
            self.cursor_noted = true;
            println!(
                "wc-compositor: a client set a cursor ({image:?}); \
                 client cursors are not drawn yet, the system arrow is shown instead"
            );
        }
    }
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

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_id;
        self.next_id += 1;
        self.toplevels += 1;

        let (title, app_id) = metadata(&surface);
        let shadow = Arc::new(Mutex::new(Shadow::empty()));
        println!(
            "wc-compositor: xdg_toplevel created (#{}) -> window {id}",
            self.toplevels
        );

        self.ui.send(ToUi::Create {
            id,
            title: title.clone(),
            app_id: app_id.clone(),
            shadow: shadow.clone(),
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
                toplevel: surface,
                shadow,
                mapped: false,
                title,
                app_id,
                frame_callbacks: Vec::new(),
                presented: 0,
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
        let (id, mapped) = (window.id, window.mapped);
        if mapped {
            self.ui.send(ToUi::AppId { id, app_id });
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

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Popups are the next stage: they need a scene graph and
        // `xdg_positioner` arithmetic, and a popup rendered as its own
        // top-level `HWND` would be wrong in a way that is hard to undo. The
        // configure is sent so a client that makes one is not left hanging.
        println!("wc-compositor: xdg_popup created (not shown; stage 4)");
        let _ = surface.send_configure();
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
