//! The platform for machines that are not Windows.
//!
//! # Why this exists at all, when the point of the project is Windows
//!
//! Because of docs/design.md §7.5, which is the most expensive fact stage 2
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
use std::io::Write;
use std::path::PathBuf;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{assemble, Commands, FrameStyle, FromUi, PlatformConfig, Started, ToUi, WindowId};
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
    resize: Step,
    close: Step,
}

impl Simulation {
    const OFF: Simulation = Simulation {
        enabled: false,
        activate: Step { frames: u64::MAX, after: Duration::MAX },
        resize: Step { frames: u64::MAX, after: Duration::MAX },
        close: Step { frames: u64::MAX, after: Duration::MAX },
    };
    const ON: Simulation = Simulation {
        enabled: true,
        activate: Step { frames: 1, after: Duration::from_millis(500) },
        resize: Step { frames: 4, after: Duration::from_millis(1500) },
        close: Step { frames: 12, after: Duration::from_millis(3500) },
    };
}

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
}

/// A window with no window in it.
struct Headless {
    title: String,
    app_id: String,
    shadow: Arc<Mutex<Shadow>>,
    mapped: bool,
    /// Surface size, as reported at `Map`. The *client area* size is this run
    /// through [`FrameStyle::client_size_for`], which is what a real platform
    /// would size its frame to and what a simulated resize has to speak in.
    surface: (i32, i32),
    frames: u64,
    mapped_at: Option<Instant>,
    activated: bool,
    resized: bool,
    closed: bool,
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
        },
    ))
}

impl PumpInner {
    pub(crate) fn run(self) {
        let mut windows: HashMap<WindowId, Headless> = HashMap::new();

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
            self.handle(&mut windows, cmd);
        }

        let _ = self.events.send(FromUi::Stopped);
    }

    fn handle(&self, windows: &mut HashMap<WindowId, Headless>, cmd: ToUi) {
        match cmd {
            ToUi::Create {
                id,
                title,
                app_id,
                shadow,
            } => {
                println!("wc-compositor: window {id} created (headless) title={title:?}");
                windows.insert(
                    id,
                    Headless {
                        title,
                        app_id,
                        shadow,
                        mapped: false,
                        surface: (0, 0),
                        frames: 0,
                        mapped_at: None,
                        activated: false,
                        resized: false,
                        closed: false,
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
            ToUi::AppId { id, app_id } => {
                if let Some(w) = windows.get_mut(&id) {
                    println!("wc-compositor: window {id} app_id -> {app_id:?}");
                    w.app_id = app_id;
                }
            }
            ToUi::Icon { id, icon } => match icon {
                Some(i) => println!(
                    "wc-compositor: window {id} icon -> {}x{} hash={:016x}",
                    i.width,
                    i.height,
                    fnv1a(&i.pixels)
                ),
                None => println!("wc-compositor: window {id} icon cleared"),
            },
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
            ToUi::Destroy { id } => {
                println!("wc-compositor: window {id} destroyed");
                windows.remove(&id);
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
        if !self.simulate.enabled || !w.mapped {
            return;
        }
        if !w.activated && self.simulate.activate.due(w.frames, w.mapped_at) {
            w.activated = true;
            println!("wc-compositor: [simulate-wm] window {id} activated");
            let _ = self.events.send(FromUi::Activated { id, on: true });
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
fn write_ppm(path: &std::path::Path, shadow: &Shadow) -> std::io::Result<()> {
    let (w, h) = (shadow.width(), shadow.height());
    if w <= 0 || h <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "nothing committed yet",
        ));
    }
    let src = shadow.pixels();
    let mut out = Vec::with_capacity(16 + src.len() / 4 * 3);
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for px in src.chunks_exact(PixelFormat::BYTES_PER_PIXEL) {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    std::fs::File::create(path)?.write_all(&out)
}
