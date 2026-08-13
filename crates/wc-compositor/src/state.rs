//! The compositor's Wayland-side state, and the handlers that turn protocol
//! into window commands.
//!
//! Everything in this file runs on the Wayland thread and only there. Nothing
//! here touches Win32; the only way out is [`Ui::send`].

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::{
    wl_buffer, wl_output, wl_seat, wl_surface::WlSurface,
};
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
    seat_state: SeatState<Self>,

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

impl State {
    pub fn new(dh: &DisplayHandle, ui: Ui, events: Receiver<FromUi>, debug_icon: bool) -> Self {
        let mut state = State {
            compositor: CompositorState::new::<State>(dh),
            shm: ShmState::new::<State>(dh, Vec::new()),
            xdg_shell: XdgShellState::new::<State>(dh),
            seat_state: SeatState::new(),
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
        };
        // A seat is not needed to map a window, but a client that finds no
        // `wl_seat` may decline to draw at all. Input is stage 4 — and note
        // that `KeyboardHandle::new` still cannot succeed on Windows, because
        // `XkbConfig::compile_keymap` goes through `xkb_keymap_new_from_names`
        // (docs/design.md §7.2). Nothing here calls it.
        let _seat: Seat<State> = state.seat_state.new_wl_seat(dh, "seat0");
        state
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
            }
        }
    }

    fn by_id(&self, id: WindowId) -> Option<&Window> {
        self.windows.values().find(|w| w.id == id)
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

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
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
