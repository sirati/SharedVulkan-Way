//! The platform for machines that are not Windows.
//!
//! # Why this exists at all, when the point of the project is Windows
//!
//! Because of docs/waycompose-design.md §7.5, which is the most expensive fact stage 2
//! established: **a stock libwayland client cannot send a buffer to the Windows
//! build.** An `h` argument travels out-of-band by `SCM_RIGHTS` on Unix and
//! in-band as a descriptor on Windows (§2.3), Wine's AF_UNIX carries no
//! ancillary data, so the first `wl_shm.create_pool` from a third-party client
//! desynchronises the Windows server's parser. Every real Wayland client in
//! existence is a stock libwayland client.
//!
//! That splits the evidence for this stage in two, and neither half is
//! sufficient alone:
//!
//! * **Linux, this file.** `weston-simple-shm` — C, libwayland, upstream
//!   xdg-shell, has never heard of us — drives the *entire* buffer path: pool,
//!   buffer, attach, damage, commit, release, frame callback. What it cannot do
//!   is produce an `HWND`, because there is no Win32 here.
//! * **Wine, `win32.rs`.** A real window with a real title bar, driven by a
//!   client that speaks the Windows dialect (`wc-shmclient`), which is ours and
//!   therefore can share our bugs.
//!
//! Together they cover it. The Linux half proves the pixels and the protocol
//! are right against a client that cannot share our bugs; the Windows half
//! proves they reach a window. Saying this out loud matters more than the code
//! below, because "we tested it with a real client" and "we tested it on the
//! real platform" are two claims and this stage can only make each of them
//! about half the path.
//!
//! # It is not a mock, and `--simulate-wm` is why
//!
//! Everything above `ToUi` is the same code on both platforms: the same commit
//! handler, the same `Shadow::blit`, the same damage clipping, the same buffer
//! release and frame-callback timing. What differs starts at `Present`.
//!
//! The half that would otherwise go untested by a real client is the *reverse*
//! direction — resize, activation and close, which on Windows originate in
//! `WM_SIZE`, `WM_ACTIVATE` and `WM_CLOSE`. [`Simulation`] generates exactly
//! those events on a schedule, so the Linux harness drives the full
//! negotiation against `weston-simple-shm`: the compositor sends a real
//! `xdg_toplevel.configure` at a new size and a real `xdg_toplevel.close`, and
//! a third-party C client either honours them or does not. That is a stronger
//! test of the negotiation than anything the Windows side can currently run,
//! because on Windows the only client available is ours.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    assemble, Commands, CursorSpec, FrameStyle, FromUi, PlatformConfig, Started, ToUi, TouchShape,
    WindowId,
};
use crate::surface::{PixelFormat, Shadow};

/// Nothing to wake: [`PumpInner::run`] blocks in `Receiver::recv_timeout`,
/// which the send itself wakes.
///
/// The Windows implementation has to do real work here, and the asymmetry is
/// the honest shape of the problem rather than an omission — a `GetMessage`
/// loop cannot also be waiting on a channel, and an `mpsc::Receiver` can.
pub(crate) struct Wake;

impl Wake {
    pub(crate) fn wake(&self) {}
}

/// What a window manager would do, on a schedule, so the reverse direction is
/// driven by something.
///
/// Each step fires on a frame count **or** a deadline since the window mapped,
/// whichever comes first.
///
/// The frame count is what makes the common case deterministic — the harness
/// can say the configure arrived after exactly so many presents, where a pure
/// timer would be flaky on a loaded machine. The deadline is what makes it work
/// at all for the clients that matter most.
///
/// **Measured, twice, and it changed this design both times.**
/// `weston-terminal` presents five frames and then stops, because a terminal
/// redraws only when its content changes; a purely frame-counted schedule never
/// reached its close, and once the close had a deadline the earlier close meant
/// its resize was never reached either. Static clients are the normal case, not
/// the corner case — a real application spends almost all of its life not
/// drawing — so any schedule that only advances on frames tests the reverse
/// direction against animations and nothing else.
#[derive(Clone, Copy)]
struct Step {
    frames: u64,
    after: Duration,
}

#[derive(Clone, Copy)]
struct Simulation {
    enabled: bool,
    activate: Step,
    /// Pointer and keyboard, as one step. See [`PumpInner::simulate_input`].
    input: Step,
    resize: Step,
    close: Step,
}

impl Simulation {
    const OFF: Simulation = Simulation {
        enabled: false,
        activate: Step { frames: u64::MAX, after: Duration::MAX },
        input: Step { frames: u64::MAX, after: Duration::MAX },
        resize: Step { frames: u64::MAX, after: Duration::MAX },
        close: Step { frames: u64::MAX, after: Duration::MAX },
    };
    const ON: Simulation = Simulation {
        enabled: true,
        activate: Step { frames: 1, after: Duration::from_millis(500) },
        // After the activation and before the resize, so the client is focused
        // when the keys arrive and has not been asked to reallocate while it is
        // being typed at. The order is the one a user produces.
        input: Step { frames: 2, after: Duration::from_millis(900) },
        resize: Step { frames: 4, after: Duration::from_millis(1500) },
        close: Step { frames: 12, after: Duration::from_millis(3500) },
    };
}

/// What the simulated keyboard types: `hello` in evdev keycodes, then Return.
///
/// A word rather than one key, and a word whose letters are spread across the
/// board rather than adjacent, because the failure this is here to catch is an
/// off-by-one in the keycode table — and an off-by-one is invisible if every
/// key tested is next to the one before it. `h e l l o` is scan codes 0x23,
/// 0x12, 0x26, 0x26, 0x18, which are not consecutive in either scheme.
///
/// The doubled `l` is deliberate too: a compositor that dropped repeated
/// keycodes, or that tracked "pressed" state wrongly, would deliver one.
const TYPED: &[(u16, char)] = &[
    (35, 'h'),
    (18, 'e'),
    (38, 'l'),
    (38, 'l'),
    (24, 'o'),
    (28, '\n'), // KEY_ENTER
];

/// `KEY_LEFTSHIFT`, used as a modifier rather than as a character.
///
/// The shifted key is typed as four events — shift down, key down, key up,
/// shift up — because that is what a keyboard sends and what a compositor has
/// to turn into `wl_keyboard.modifiers` around a `key`. It is the only part of
/// the keyboard path where the compositor's *own* xkb state is load-bearing:
/// the client is told which modifiers are latched, and it computes the
/// character. Sending an upper-case character instead would be making the
/// decision the keymap exists to make.
const SHIFT: u16 = 42;

/// `KEY_Z`, which must come out as `Z` while shift is held.
const SHIFTED: (u16, char) = (44, 'Z');

/// The key pressed once before the layout change and once after.
///
/// `KEY_Y` is 21, and it is the *position* of `Y` on an American board and of
/// `Z` on a German one. So the same evdev keycode must produce `y` under the
/// `us` keymap and `z` under `de` — which is a statement about the client's own
/// libxkbcommon having recompiled a keymap that arrived over the wire, and is
/// the only way to prove from outside that the second `wl_keyboard.keymap`
/// carried a different keymap rather than being re-sent or ignored.
///
/// It is also the exact pair that a compositor translating virtual-key codes
/// instead of scan codes would get wrong (see `crate::input`), which makes one
/// key carry two claims.
const LAYOUT_PROBE: u16 = 21;

impl Step {
    /// Has this step come due, by either route?
    fn due(&self, frames: u64, mapped_at: Option<Instant>) -> bool {
        frames >= self.frames || mapped_at.is_some_and(|t| t.elapsed() >= self.after)
    }
}

pub(crate) struct PumpInner {
    commands: Commands,
    events: Sender<FromUi>,
    dump_frames: Option<PathBuf>,
    frame: FrameStyle,
    simulate: Simulation,
    /// ADDED FOR STAGE 9: one scripted click, into the first window that maps.
    ///
    /// # Why a harness needs this and `--simulate-wm` was not enough
    ///
    /// The thing stage 9 has to show is a **menu**, and a menu is opened by
    /// clicking a particular pixel. `--simulate-wm` clicks the centre of the
    /// window, which in a browser is the middle of a web page and opens
    /// nothing. There is no window manager on this platform and no `SendInput`,
    /// so without a way to say *where*, the popup path can only be exercised
    /// under Wine — and Wine is a hypothesis (docs/waycompose-design.md §4.4).
    ///
    /// Deliberately a single click rather than a scripting language: the
    /// question is whether one press at a known point produces an `xdg_popup`
    /// and a second window, and anything more would be building a test
    /// framework instead of answering it.
    click: Option<Click>,
}

/// `--click X,Y[,BUTTON][,MS]`.
#[derive(Clone, Copy, Debug)]
pub struct Click {
    pub x: f64,
    pub y: f64,
    /// An evdev button code: `BTN_LEFT` 0x110, `BTN_RIGHT` 0x111.
    pub button: u32,
    /// How long after the window maps, which has to be long enough for the
    /// client to have drawn the thing being clicked on.
    pub after: Duration,
}

/// A window with no window in it.
struct Headless {
    title: String,
    app_id: String,
    /// The taskbar identity this window would be given on Windows. ADDED FOR
    /// STAGE 32; kept so that a second `AppId` for the same one is not
    /// announced twice.
    aumid: String,
    shadow: Arc<Mutex<Shadow>>,
    mapped: bool,
    /// Surface size, as reported at `Map`. The *client area* size is this run
    /// through [`FrameStyle::client_size_for`], which is what a real platform
    /// would size its frame to and what a simulated resize has to speak in.
    surface: (i32, i32),
    frames: u64,
    mapped_at: Option<Instant>,
    activated: bool,
    input: bool,
    resized: bool,
    closed: bool,
    /// ADDED FOR STAGE 9. The scripted click is delivered once, into the first
    /// window only: a popup is itself a window, and clicking into the menu that
    /// the first click opened would dismiss it.
    clicked: bool,
}

/// Which layout the keyboard compiles when nobody says.
///
/// There is no Windows here to ask, and asking the *Linux* machine's layout
/// would be answering a different question — the layout that matters is the one
/// the user is typing on, which on this platform is nobody. So it is the
/// fallback, and the harness passes `--layout` when it wants a specific one.
pub fn current_layout() -> &'static str {
    crate::input::FALLBACK
}

/// ADDED FOR STAGE 20. Whether `wl_seat` may advertise touch.
///
/// **True**, and the mirror of `win32::has_touch` rather than a copy of it. On
/// Windows the question is whether a digitiser exists, because the events come
/// from one; here they come from [`PumpInner::simulate_input`], which is a real
/// source of `wl_touch` events even though it is not a finger — so the
/// capability is honest on the same terms. A Linux run without `--simulate-wm`
/// advertises a touch that never fires, which is exactly what a Windows machine
/// with a digitiser nobody touches also does.
///
/// This leg is where a third-party client is the arbiter (docs/waycompose-design.md §24.6):
/// on Windows there is no Weston client to connect directly, and here there is.
pub fn has_touch() -> bool {
    true
}

/// What `wl_keyboard.repeat_info` advertises when there is no Windows to ask.
///
/// ADDED FOR STAGE 12, as the mirror of `win32.rs`'s `SystemParametersInfo`
/// call. **Windows' defaults, not Linux's**, and that is deliberate: this
/// platform exists so the compositor's own logic can be exercised on the
/// machine that builds it, and a headless run that advertised whatever
/// `xset q` says on the build host would make the harness's output depend on
/// the developer's desktop settings. The numbers are `SPI_GETKEYBOARDDELAY` 1
/// and `SPI_GETKEYBOARDSPEED` 31 put through the same arithmetic, which is
/// what a stock Windows install returns.
pub fn keyboard_repeat() -> (i32, i32) {
    (500, 30)
}

pub fn start(config: PlatformConfig) -> std::io::Result<Started> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(dir) = &config.dump_frames {
        std::fs::create_dir_all(dir)?;
    }
    Ok(assemble(
        tx,
        Wake,
        PumpInner {
            commands: rx,
            events: config.events,
            dump_frames: config.dump_frames,
            frame: config.frame,
            simulate: if config.simulate_wm {
                Simulation::ON
            } else {
                Simulation::OFF
            },
            click: config.click,
        },
        // STAGE 38: no monitor here, so no work area. `--output` stays the
        // whole answer, which is what the conformance leg needs.
        None,
    ))
}

impl PumpInner {
    pub(crate) fn run(self) {
        let mut windows: HashMap<WindowId, Headless> = HashMap::new();
        // The system clipboard, on a system that has none.
        //
        // In-process rather than a file or an X selection, and that is the
        // honest shape of this platform rather than a shortcut: the thing
        // being tested on Linux is the *path* — a client's selection reaching
        // the compositor as bytes, and coming back to another client — and the
        // only part of it that Windows owns is the last hop. Wine runs that
        // hop against the real `SetClipboardData` with a second Win32 process
        // reading the result, which is where the claim "a Windows application
        // can paste it" is made. See docs/waycompose-design.md §29.
        let mut clipboard: Vec<(crate::clipboard::SystemFormat, Vec<u8>)> = Vec::new();

        loop {
            let cmd = match self.commands.recv_timeout(Duration::from_millis(50)) {
                Ok(c) => c,
                Err(RecvTimeoutError::Timeout) => {
                    // The idle tick. A client that has stopped presenting
                    // sends nothing, so this is the only place a deadline can
                    // fire — see [`Simulation`] for why one is needed.
                    for (id, w) in windows.iter_mut() {
                        self.simulate(*id, w);
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if matches!(cmd, ToUi::Quit) {
                break;
            }
            self.handle(&mut windows, &mut clipboard, cmd);
        }

        let _ = self.events.send(FromUi::Stopped);
    }

    fn handle(
        &self,
        windows: &mut HashMap<WindowId, Headless>,
        clipboard: &mut Vec<(crate::clipboard::SystemFormat, Vec<u8>)>,
        cmd: ToUi,
    ) {
        match cmd {
            ToUi::Create {
                id,
                title,
                app_id,
                aumid,
                shadow,
                kind,
                // STAGE 38: deliberately dropped. There is no monitor here, so
                // there is no work area to answer with, and a headless run with
                // no `--control` must leave popups unconstrained rather than
                // invent a screen. Where something *is* standing in for Windows
                // — the conformance leg — `crate::control` fills the same cell
                // from the advertised output.
                bounds: _,
            } => {
                // The kind is logged rather than acted on: there is no window
                // manager here to place a popup against, and the harness reads
                // this line to know one was made.
                match kind {
                    crate::platform::WindowKind::Toplevel => println!(
                        "wc-compositor: window {id} created (headless) title={title:?}"
                    ),
                    crate::platform::WindowKind::Popup { parent, x, y } => println!(
                        "wc-compositor: window {id} created (headless popup) \
                         parent={parent} at {x},{y}"
                    ),
                }
                // ADDED FOR STAGE 32. There is no taskbar here either, so the
                // identity is *said* — which is enough for a Linux-side test to
                // assert that two clients got two of them and that two windows
                // of one client did not.
                println!("wc-compositor: window {id} taskbar identity -> {aumid:?}");
                windows.insert(
                    id,
                    Headless {
                        title,
                        app_id,
                        aumid,
                        shadow,
                        mapped: false,
                        surface: (0, 0),
                        frames: 0,
                        mapped_at: None,
                        activated: false,
                        input: false,
                        resized: false,
                        closed: false,
                        clicked: false,
                    },
                );
            }
            ToUi::Title { id, title } => {
                if let Some(w) = windows.get_mut(&id) {
                    // The Win32 arm calls `SetWindowTextW` here. Logging it is
                    // what lets the Linux harness assert the title arrived,
                    // which is the half of the claim that does not need a title
                    // bar to be true.
                    println!("wc-compositor: window {id} title -> {title:?}");
                    w.title = title;
                }
            }
            ToUi::AppId { id, app_id, aumid } => {
                if let Some(w) = windows.get_mut(&id) {
                    println!("wc-compositor: window {id} app_id -> {app_id:?}");
                    if w.aumid != aumid {
                        println!(
                            "wc-compositor: window {id} taskbar identity -> {aumid:?}"
                        );
                        w.aumid = aumid;
                    }
                    w.app_id = app_id;
                }
            }
            // There is no taskbar here, so the icon is reported rather than
            // shown — and the hash is the point: it is how a Linux-side test of
            // the resolver can assert that the *pixels* that arrived are the
            // ones the theme holds, without a Windows machine in the loop.
            ToUi::Icon { id, icons } => {
                if icons.is_empty() {
                    println!("wc-compositor: window {id} icon cleared");
                }
                for i in &icons {
                    println!(
                        "wc-compositor: window {id} icon -> {}x{} hash={:016x}",
                        i.width,
                        i.height,
                        fnv1a(&i.pixels)
                    );
                }
                // Which image each Win32 slot *would* get. The selection is the
                // one part of the icon path that is arithmetic rather than
                // Win32, so running it here means the choice is exercised on
                // the machine that runs the tests rather than only on the guest.
                for (slot, want) in [("small", 16), ("big", 32)] {
                    if let Some(i) = crate::icon::best_match(&icons, want) {
                        println!(
                            "wc-compositor: window {id} icon {slot} ({want}px) would use {}x{}",
                            i.width, i.height
                        );
                    }
                }
            }

            // ADDED FOR STAGE 12. There is no pointer here to put a cursor on,
            // so the command is reported — and reported in the terms the Win32
            // side would act on, including the `IDC_*` a named shape resolves
            // to. That table is arithmetic rather than Win32 (see
            // `crate::cursor`), so running it here means the mapping every
            // client actually depends on is exercised by `cargo test` on Linux
            // rather than only on the guest.
            ToUi::Cursor { spec } => match spec {
                CursorSpec::Hidden => {
                    println!("wc-compositor: cursor -> hidden");
                }
                CursorSpec::Named(icon) => {
                    let stock = crate::cursor::stock_for(icon);
                    println!(
                        "wc-compositor: cursor -> {} ({})",
                        icon.name(),
                        stock.name
                    );
                }
                CursorSpec::Image {
                    image,
                    hotspot_x,
                    hotspot_y,
                } => {
                    // The hash for the same reason `ToUi::Icon` carries one: it
                    // is how a test asserts that the pixels which arrived are
                    // the client's, with no Windows in the loop.
                    println!(
                        "wc-compositor: cursor -> {}x{} pixels hotspot {hotspot_x},{hotspot_y} \
                         hash={:016x}",
                        image.width,
                        image.height,
                        fnv1a(&image.pixels)
                    );
                }
            },

            // ADDED FOR STAGE 13. Printed rather than acted on: there is no
            // window manager here to move anything, and the thing worth
            // asserting is that `xdg_popup.reposition` produced a move at all,
            // which the harness can now read.
            ToUi::MovePopup { id, x, y, width, height } => println!(
                "wc-compositor: window {id} repositioned to {x},{y} {width}x{height}"
            ),

            ToUi::Map { id, width, height } => {
                if let Some(w) = windows.get_mut(&id) {
                    w.mapped = true;
                    w.surface = (width, height);
                    w.mapped_at = Some(Instant::now());
                    let (cw, ch) = self.frame.client_size_for(width, height);
                    let (ox, oy) = self.frame.content_origin();
                    println!(
                        "wc-compositor: window {id} mapped surface {width}x{height} \
                         client {cw}x{ch} content origin {ox},{oy}"
                    );
                }
            }
            ToUi::Present { id, damage } => {
                let Some(w) = windows.get_mut(&id) else { return };
                {
                    let shadow = w.shadow.lock().unwrap_or_else(|e| e.into_inner());
                    // One greppable line per frame, carrying enough for a shell
                    // script to assert on the pixels without decoding an image:
                    // size, format, damage, a hash of the whole shadow, and the
                    // centre pixel. The hash catches "the pixels never
                    // changed"; the centre pixel catches "the window is
                    // uniformly black", which a hash alone would happily pass.
                    println!(
                        "wc-compositor: present window={id} frame={} {}x{} {} rects={} damage={} hash={:016x} centre={:08x}",
                        w.frames,
                        shadow.width(),
                        shadow.height(),
                        if shadow.format().has_alpha() { "argb" } else { "xrgb" },
                        damage.len(),
                        damage
                            .iter()
                            .map(|r| format!("{},{},{},{}", r.x, r.y, r.w, r.h))
                            .collect::<Vec<_>>()
                            .join(";"),
                        fnv1a(shadow.pixels()),
                        centre_pixel(&shadow),
                    );
                    if let Some(dir) = &self.dump_frames {
                        let path = dir.join(format!("window{id}-{:04}.ppm", w.frames));
                        match write_ppm(&path, &shadow) {
                            Ok(()) => println!("wc-compositor: wrote {}", path.display()),
                            Err(e) => {
                                eprintln!("wc-compositor: could not write {}: {e}", path.display())
                            }
                        }
                    }
                }
                w.frames += 1;
                self.simulate(id, w);
            }
            ToUi::SetWindowState {
                id,
                maximized,
                minimized,
            } => {
                println!(
                    "wc-compositor: window {id} state maximized={maximized:?} minimized={minimized:?}"
                );
                // There is no window manager here to obey the request, so the
                // state is reported straight back as though it had been
                // honoured. That keeps the Wayland side on the same path it
                // takes on Windows, where the echo comes back from `WM_SIZE`.
                if let Some(on) = maximized {
                    let _ = self.events.send(FromUi::Maximized { id, on });
                }
                if let Some(on) = minimized {
                    let _ = self.events.send(FromUi::Minimized { id, on });
                }
            }
            // ADDED FOR STAGE 17. There is no window manager here to hand a
            // gesture to, and inventing one would be testing this file. Logged
            // in the same greppable shape as everything else, so that a harness
            // can assert the request *arrived* — which is the half of the path
            // that lives in `state.rs` and is the half a Linux run can check.
            ToUi::BeginMove { id } => {
                println!("wc-compositor: window {id} asked the platform to move it");
            }
            ToUi::BeginResize { id, edge } => {
                println!("wc-compositor: window {id} asked the platform to resize it from {edge:?}");
            }
            ToUi::WindowMenu { id, x, y } => {
                println!("wc-compositor: window {id} asked the platform for a window menu at {x},{y}");
            }
            // ADDED FOR STAGE 21. Both of these are Win32 calls with no
            // equivalent here — there is no display to keep awake and no
            // foreground to take — so what the headless platform owes them is
            // the line in the log, which is what the harness reads to check
            // that the Wayland half decided the right thing. Answering
            // `Activate` with a synthetic `FromUi::Activated`, the way
            // `SetWindowState` above answers itself, would be *worse*: on
            // Windows the activation may be refused, and a headless platform
            // that always succeeded would make the one interesting outcome
            // untestable.
            ToUi::IdleInhibit { on } => {
                println!(
                    "wc-compositor: idle inhibit {} (no display to keep awake here)",
                    if on { "on" } else { "off" }
                );
            }
            ToUi::Activate { id } => {
                println!("wc-compositor: window {id} asked to be activated (no foreground here)");
            }
            ToUi::Destroy { id } => {
                println!("wc-compositor: window {id} destroyed");
                windows.remove(&id);
            }

            // The clipboard, as far as this platform can carry it. The Win32
            // arm converts to `CF_UNICODETEXT` and calls `SetClipboardData`;
            // here the payloads are kept as they arrived and printed, which is
            // what lets the Linux harness assert on the *bytes a client sent*
            // without a Windows machine in the loop. The conversion itself is
            // `crate::clipboard`'s and is unit-tested on this machine anyway.
            ToUi::SetClipboard { payloads } => {
                // **The same conversion the Win32 arm performs**, and that is
                // the point of doing it here rather than storing the payloads
                // raw: `to_system` picks one text flavour out of the six a GTK
                // client offers, encodes UTF-16 with CRLF, and wraps HTML in
                // the header Windows requires. Every one of those is silently
                // wrong if it is wrong, and this is the leg that runs on the
                // machine `cargo test` runs on. What Windows adds after this
                // point is `SetClipboardData` and nothing else.
                *clipboard = crate::clipboard::to_system(&payloads);
                if clipboard.is_empty() {
                    println!("wc-compositor: clipboard cleared");
                }
                for (format, bytes) in clipboard.iter() {
                    println!(
                        "wc-compositor: clipboard set {} bytes={} hash={:016x} text={:?}",
                        format.name(),
                        bytes.len(),
                        fnv1a(bytes),
                        preview(bytes),
                    );
                }
                let formats: Vec<_> = clipboard.iter().map(|(f, _)| *f).collect();
                // The echo a real clipboard produces, so that both platforms
                // drive the same path: on Win32 this comes back as
                // `WM_CLIPBOARDUPDATE` and the reason it must not be
                // suppressed is in `win32.rs`.
                let _ = self.events.send(FromUi::ClipboardChanged {
                    mimes: crate::clipboard::mimes_for(&formats),
                });
            }
            // There is no desktop here to drag onto, so a drag ends the moment
            // it starts — and it **does** end, with `none`, which is the whole
            // point. A platform that answered nothing would leave the client's
            // `wl_data_source` waiting forever, and a drag that never ends is
            // the failure this feature exists not to build. The Win32 arm runs
            // `DoDragDrop` and answers with what the drop target did.
            ToUi::StartDrag { payloads, actions } => {
                println!(
                    "wc-compositor: [headless] a drag was started with {} mime type(s), \
                     actions {}; there is no desktop to drop onto here",
                    payloads.len(),
                    crate::platform::dnd_action::describe(actions)
                );
                let _ = self.events.send(FromUi::DragFinished {
                    action: crate::platform::dnd_action::NONE,
                });
            }
            ToUi::CancelDrag => println!("wc-compositor: [headless] the drag was withdrawn"),
            ToUi::FetchClipboard { token, from_drag, mime } => {
                if from_drag {
                    // Nothing can be dragged onto this platform, so there is
                    // never a drag to read from. Answered rather than ignored.
                    println!(
                        "wc-compositor: [headless] clipboard fetch token={token} asked for the \
                         drag, and there are no drags here"
                    );
                    let _ = self.events.send(FromUi::ClipboardData { token, data: None });
                    return;
                }
                let data = clipboard
                    .iter()
                    .find_map(|(f, b)| crate::clipboard::from_system(&mime, *f, b));
                println!(
                    "wc-compositor: clipboard fetch token={token} mime={mime:?} -> {}",
                    match &data {
                        Some(b) => format!("{} bytes {:?}", b.len(), preview(b)),
                        None => "nothing".to_owned(),
                    }
                );
                let _ = self.events.send(FromUi::ClipboardData { token, data });
            }
            ToUi::Quit => {}
        }
    }

    /// Play the window manager, on the schedule in [`Simulation`].
    ///
    /// Called both after a present (for the frame-counted triggers) and on the
    /// pump's idle tick (for the deadline), so a client that stops drawing is
    /// still closed.
    fn simulate(&self, id: WindowId, w: &mut Headless) {
        // ADDED FOR STAGE 9, and outside the `--simulate-wm` gate on purpose:
        // the two are independent, and a run that wants a menu opened does not
        // want the window resized and closed underneath it four seconds later.
        if let Some(click) = self.click
            && w.mapped
            && !w.clicked
            && id == 1
            && w.mapped_at.is_some_and(|t| t.elapsed() >= click.after)
        {
            w.clicked = true;
            self.deliver_click(id, w, click);
        }
        if !self.simulate.enabled || !w.mapped {
            return;
        }
        if !w.activated && self.simulate.activate.due(w.frames, w.mapped_at) {
            w.activated = true;
            println!("wc-compositor: [simulate-wm] window {id} activated");
            let _ = self.events.send(FromUi::Activated { id, on: true });
        }
        if !w.input && self.simulate.input.due(w.frames, w.mapped_at) {
            w.input = true;
            self.simulate_input(id, w);
        }
        if !w.resized && self.simulate.resize.due(w.frames, w.mapped_at) {
            w.resized = true;
            // A resize arrives from the platform as a *client area* size, which
            // has to go back through the frame to become a surface size. With
            // `extend_top` at 0 the two are equal, and routing it through the
            // conversion anyway is what makes this a test of the arithmetic
            // rather than of a coincidence.
            let (cw, ch) = self.frame.client_size_for(w.surface.0 + 40, w.surface.1 + 20);
            let (sw, sh) = self.frame.surface_size_for(cw, ch);
            println!("wc-compositor: [simulate-wm] window {id} resized to {sw}x{sh}");
            let _ = self.events.send(FromUi::Resized {
                id,
                width: sw,
                height: sh,
            });
        }
        if !w.closed && self.simulate.close.due(w.frames, w.mapped_at) {
            w.closed = true;
            println!(
                "wc-compositor: [simulate-wm] window {id} close button pressed \
                 (after {} frames)",
                w.frames
            );
            let _ = self.events.send(FromUi::CloseRequested { id });
        }
    }

    /// One click, at a named point, in the order a real pointer produces.
    ///
    /// ADDED FOR STAGE 9. The focus event comes first because a GTK client that
    /// has never been focused treats a button press as a click-to-focus and
    /// swallows it, which looks exactly like a compositor that failed to
    /// deliver the button.
    fn deliver_click(&self, id: WindowId, w: &Headless, click: Click) {
        let t = || {
            w.mapped_at
                .map(|m| m.elapsed().as_millis() as u32)
                .unwrap_or(0)
        };
        let send = |e: FromUi| {
            let _ = self.events.send(e);
        };
        println!(
            "wc-compositor: [click] window {id} button {:#x} at {},{}",
            click.button, click.x, click.y
        );
        send(FromUi::Activated { id, on: true });
        send(FromUi::KeyboardFocus { id, on: true, caps: false, num: false });
        send(FromUi::PointerEnter { id, x: click.x, y: click.y, time: t() });
        // A motion after the enter: GTK decides what is under the pointer from
        // the motion, and a press whose only position came from the enter
        // event lands on whatever the widget last thought was hovered.
        send(FromUi::PointerMotion { id, x: click.x, y: click.y, time: t() });
        send(FromUi::PointerButton { id, button: click.button, pressed: true, time: t() });
        send(FromUi::PointerButton { id, button: click.button, pressed: false, time: t() });
    }

    /// Drive the seat, in the units and the order a real one produces.
    ///
    /// # Why this is worth having when the whole point is Win32
    ///
    /// Because of docs/waycompose-design.md §8.5's split, which stage 6 inherits in a
    /// sharper form than stage 3 had it. On Windows the only clients that can
    /// reach the compositor are ours or come through the proxy; on Linux any
    /// Weston client connects directly. `weston-eventdemo` **prints every input
    /// event it receives, including the Unicode character its own libxkbcommon
    /// produced from the keycode and the keymap we sent it** — which is the
    /// strongest available statement that the seat is right, and it is made by
    /// a program that has never heard of this project.
    ///
    /// What this leg cannot say anything about is the translation: there is no
    /// `WM_KEYDOWN` here, so the keycodes are written down rather than
    /// converted, and `crate::input`'s table is tested by unit tests and by the
    /// Wine leg instead. Saying which half each leg carries matters more than
    /// the code, and it is the same division §8.5 drew for pixels.
    ///
    /// The sequence is deliberately a *user's* sequence and not a list of every
    /// event type: enter, move, click, release, type a word, scroll, leave. An
    /// event delivered out of that order — a button before an enter, a key
    /// before a focus — is a protocol error the client is entitled to treat as
    /// fatal, and a harness that produced them in a convenient order would not
    /// be testing that this one is correct.
    fn simulate_input(&self, id: WindowId, w: &Headless) {
        let t = || {
            w.mapped_at
                .map(|m| m.elapsed().as_millis() as u32)
                .unwrap_or(0)
        };
        let (cx, cy) = (f64::from(w.surface.0) / 2.0, f64::from(w.surface.1) / 2.0);
        let send = |e: FromUi| {
            let _ = self.events.send(e);
        };

        println!("wc-compositor: [simulate-wm] window {id} keyboard focus");
        send(FromUi::KeyboardFocus { id, on: true, caps: false, num: false });

        println!("wc-compositor: [simulate-wm] window {id} pointer enter at {cx},{cy}");
        send(FromUi::PointerEnter { id, x: cx, y: cy, time: t() });
        send(FromUi::PointerMotion { id, x: cx + 8.0, y: cy + 4.0, time: t() });
        send(FromUi::PointerButton {
            id,
            button: crate::input::evdev::BTN_LEFT,
            pressed: true,
            time: t(),
        });
        send(FromUi::PointerButton {
            id,
            button: crate::input::evdev::BTN_LEFT,
            pressed: false,
            time: t(),
        });

        let typed: String = TYPED.iter().map(|(_, c)| *c).collect();
        println!(
            "wc-compositor: [simulate-wm] window {id} typing {:?} as evdev {:?}",
            typed.trim_end(),
            TYPED.iter().map(|(k, _)| *k).collect::<Vec<_>>()
        );
        for (keycode, _) in TYPED {
            send(FromUi::Key { id, keycode: *keycode, pressed: true, time: t() });
            send(FromUi::Key { id, keycode: *keycode, pressed: false, time: t() });
        }

        println!(
            "wc-compositor: [simulate-wm] window {id} shift + evdev {} -> {:?}",
            SHIFTED.0, SHIFTED.1
        );
        send(FromUi::Key { id, keycode: SHIFT, pressed: true, time: t() });
        send(FromUi::Key { id, keycode: SHIFTED.0, pressed: true, time: t() });
        send(FromUi::Key { id, keycode: SHIFTED.0, pressed: false, time: t() });
        send(FromUi::Key { id, keycode: SHIFT, pressed: false, time: t() });

        // The layout change, which on Windows is `WM_INPUTLANGCHANGE`, with the
        // same key pressed either side of it. See [`LAYOUT_PROBE`] for why that
        // is the only client-observable proof that the second keymap crossed.
        println!("wc-compositor: [simulate-wm] window {id} probe key before the layout change");
        send(FromUi::Key { id, keycode: LAYOUT_PROBE, pressed: true, time: t() });
        send(FromUi::Key { id, keycode: LAYOUT_PROBE, pressed: false, time: t() });
        println!("wc-compositor: [simulate-wm] window {id} layout change -> de");
        send(FromUi::Layout { name: "de" });
        println!("wc-compositor: [simulate-wm] window {id} probe key after the layout change");
        send(FromUi::Key { id, keycode: LAYOUT_PROBE, pressed: true, time: t() });
        send(FromUi::Key { id, keycode: LAYOUT_PROBE, pressed: false, time: t() });

        // **One notch away from the user**, which is what a wheel does when it
        // is rolled forward and what Windows reports as `+WHEEL_DELTA`.
        //
        // Wayland's vertical axis is positive *downwards*, in the same
        // coordinate space as motion — and rolling the wheel away scrolls the
        // content up, so the value is **negative**. The numbers here are
        // therefore exactly the ones `win32.rs` produces for the same physical
        // gesture, which is the point: the two platforms feed the harness the
        // same assertion, and a sign error on either side fails it.
        println!("wc-compositor: [simulate-wm] window {id} scroll one notch away from the user");
        send(FromUi::PointerAxis {
            id,
            horizontal: 0.0,
            vertical: -10.0,
            v120_h: 0,
            v120_v: -120,
            time: t(),
        });

        println!("wc-compositor: [simulate-wm] window {id} pointer leave");
        send(FromUi::PointerLeave { id, time: t() });

        self.simulate_touch(id, w, &send, &t);
    }

    /// STAGE 20. A two-finger pinch, then both fingers up.
    ///
    /// # Why a pinch and not a tap
    ///
    /// A tap would exercise `down`, `motion`, `up` and `frame` and would say
    /// nothing about the part of `wl_touch` that is actually hard, which is
    /// that **contacts are tracked by id and the ids must stay put**. Two
    /// contacts moving in opposite directions is the smallest gesture where a
    /// swapped or renumbered id is visible in a client rather than only in a
    /// log: the two fingers converge instead of diverging.
    ///
    /// The order is a user's order and not a list of the event types, for the
    /// same reason `simulate_input` above says so: a `motion` for an id that
    /// has not been `down` is a protocol error a client may treat as fatal, and
    /// a harness that emitted them in a tidy order would not be testing that
    /// this one is right.
    ///
    /// # Both answers about shape, because there are two
    ///
    /// The pinch carries **no** `shape`, which is what a digitiser that does
    /// not report a contact area produces and is by far the commoner case; a
    /// client must see no `shape` and no `orientation` at all across it, not a
    /// default ellipse. The last contact carries **one**, so that the
    /// conditional half of `wl_touch` is executed by something rather than
    /// merely compiled — including its ordering, which the protocol constrains:
    /// `shape` and `orientation` must precede the `frame` that closes the
    /// update they belong to.
    ///
    /// The distinction that matters, and it is not the same distinction: on
    /// Windows a shape is sent **only** when `POINTER_TOUCH_INFO::touchMask`
    /// says the digitiser measured one. Inventing an ellipse *there* would tell
    /// a client something about hardware that was never asked. Writing one down
    /// *here* is the same kind of statement as the scripted keystrokes above —
    /// a harness saying what a device would send, so the code that handles it
    /// runs.
    fn simulate_touch(
        &self,
        id: WindowId,
        w: &Headless,
        send: &dyn Fn(FromUi),
        t: &dyn Fn() -> u32,
    ) {
        let (cx, cy) = (f64::from(w.surface.0) / 2.0, f64::from(w.surface.1) / 2.0);
        // Contact ids as the platform hands them out: dense, from zero.
        let (a, b) = (0, 1);

        println!("wc-compositor: [simulate-wm] window {id} two contacts down at {cx},{cy}");
        send(FromUi::TouchDown { id, slot: a, x: cx - 20.0, y: cy, shape: None, time: t() });
        send(FromUi::TouchDown { id, slot: b, x: cx + 20.0, y: cy, shape: None, time: t() });
        // One frame for both, because they landed together. This is the case a
        // client that buffers on `frame` gets wrong if the compositor forgets
        // to close it — and the case Win32 cannot produce, since it delivers
        // one message per contact (see `win32::on_pointer`).
        send(FromUi::TouchFrame);

        println!("wc-compositor: [simulate-wm] window {id} pinch out, four steps");
        for step in 1..=4 {
            let d = 20.0 + f64::from(step) * 15.0;
            send(FromUi::TouchMotion { id, slot: a, x: cx - d, y: cy, shape: None, time: t() });
            send(FromUi::TouchMotion { id, slot: b, x: cx + d, y: cy, shape: None, time: t() });
            send(FromUi::TouchFrame);
        }

        // Lifted one at a time, which is what fingers do, and which is the
        // ordering that catches an implementation that retires the wrong id.
        println!("wc-compositor: [simulate-wm] window {id} contacts up, one at a time");
        send(FromUi::TouchUp { id, slot: b, time: t() });
        send(FromUi::TouchFrame);
        send(FromUi::TouchUp { id, slot: a, time: t() });
        send(FromUi::TouchFrame);

        // And a tap the system takes away mid-gesture, which is the event a
        // compositor is most likely never to send at all: `cancel` has no
        // matching thing a user does, it comes from the platform deciding the
        // stream was a shell gesture, and a client that never sees one will
        // hold the contact for ever.
        //
        // It is also the one that carries a contact patch: 9 by 14 surface
        // units at 30 degrees clockwise of the surface's y axis, which is a
        // finger at a slight angle and is the shape `shape_from_contact` builds
        // from a Windows rectangle whose `TOUCH_MASK_ORIENTATION` bit is set.
        println!("wc-compositor: [simulate-wm] window {id} a contact the system then cancels");
        let shape = Some(TouchShape { major: 14.0, minor: 9.0, orientation: 30.0 });
        send(FromUi::TouchDown { id, slot: a, x: cx, y: cy, shape, time: t() });
        send(FromUi::TouchFrame);
        send(FromUi::TouchCancel);
    }
}

fn centre_pixel(shadow: &Shadow) -> u32 {
    if shadow.is_empty() {
        return 0;
    }
    let (w, h) = (shadow.width() as usize, shadow.height() as usize);
    let off = ((h / 2) * w + w / 2) * PixelFormat::BYTES_PER_PIXEL;
    let px = &shadow.pixels()[off..off + PixelFormat::BYTES_PER_PIXEL];
    u32::from_le_bytes([px[0], px[1], px[2], px[3]])
}

/// The first few characters of a clipboard payload, for the log.
///
/// Truncated because a selection can be four megabytes and a log line should
/// not be, and lossy because the payload is whatever a client called
/// `text/plain` — which is not required to be UTF-8 and, for `text/html` from
/// a browser, frequently is not clean.
fn preview(bytes: &[u8]) -> String {
    const N: usize = 60;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(N)]).into_owned();
    if bytes.len() > N {
        format!("{text}...")
    } else {
        text
    }
}

/// FNV-1a. Not a checksum with any cryptographic claim — it is here so a shell
/// script can tell two frames apart.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Write the shadow as a binary PPM.
///
/// PPM because it needs no library and `identify`/`convert` read it, so the
/// harness can inspect the result with tools that are not ours. The shadow is
/// BGRA and PPM is RGB, so this is the one place a channel swizzle happens —
/// and it happens on the *diagnostic* path, not the display path, which is the
/// right side for it to be on.
/// Moved to `crate::surface` in stage 4, so that the Win32 platform can dump
/// frames too — which is what lets the Wine and guest runs read their pixels
/// back with tools that are not ours, exactly as this one always could.
use crate::surface::write_ppm;
