//! waycompose, stage 3: **a Wayland surface becomes a real Windows window.**
//!
//! Stage 1 answered "does the `wayland-server` stack run as an `.exe`?"; stage
//! 2 answered "does smithay?" and got a third-party C client to
//! `xdg_toplevel`. Both stopped short of drawing anything, and the design
//! called that out: *"There is no renderer, no HWND and no output. Pixels are
//! the next stage."*
//!
//! This is that stage. One `HWND` per `xdg_toplevel`, with Windows' own
//! caption, border and buttons; the client's `wl_shm` pixels blitted into it
//! with GDI; damage honoured; buffers released; `set_title` to `SetWindowTextW`
//! and the close button back to `xdg_toplevel.close`.
//!
//! # The shape of the program
//!
//! Two threads, and the split is the interesting decision — see
//! `platform/mod.rs` for the argument. **The main thread pumps Win32
//! messages; the Wayland dispatch loop is spawned.** Every smithay object
//! lives on the spawned thread and is created inside its closure, so the
//! compiler enforces that nothing thread-affine escapes: the only things that
//! cross are the plain-data messages in `platform`, plus one
//! `Arc<Mutex<Shadow>>` of pixels per window.
//!
//! ```text
//!   main thread                      wayland thread
//!   ───────────                      ──────────────
//!   GetMessage / DispatchMessage     accept, dispatch_clients, flush
//!        │  ▲                             │  ▲
//!        │  └── ToUi ── mpsc + PostMessage┘  │
//!        └───── FromUi ── mpsc ──────────────┘
//!                        (drained each turn of the poll loop)
//! ```
//!
//! # The event loop is still a poll loop, and still deliberately
//!
//! No `calloop`. `Backend::poll_fd()` returns a wepoll handle on Windows and a
//! wepoll handle is not a socket (docs/waycompose-design.md §3.3), so the backend cannot
//! nest inside it. The Wayland thread therefore polls on a 5 ms wait, exactly
//! as stages 1 and 2 did — though on Windows it took until stage 8 for that to
//! be 5 ms rather than `Sleep`'s 15.9 ms quantum (`tick.rs`). It is a latency
//! floor on input and on how fast a `WM_CLOSE` reaches the client, and it is
//! honest about being one; it is not a throughput limit, because a single turn
//! drains every pending message.

mod clipboard;
/// The socket a conformance suite drives this compositor through, standing
/// where Windows stands. Linux only, because WLCS is. See its header, and
/// docs/waycompose-design.md §26.
#[cfg(unix)]
mod control;
mod cursor;
pub mod grow;
mod icon;
mod input;
mod platform;
/// §36: `accept()` on the ring directory, and the bridge to smithay's socket.
#[cfg(unix)]
mod ringlink;
mod state;
mod surface;
/// Which taskbar button a window belongs to. See its header, and
/// docs/waycompose-design.md §32.
mod taskbar;
mod tick;

/// How long the Wayland thread waits between turns.
///
/// 5 ms since stage 1, and unchanged — what changed in stage 8 is that it is
/// now *actually* 5 ms. See `tick.rs` and docs/waycompose-design.md §13.8: on Windows this
/// number went through `Sleep`, whose quantum is 15.6 ms, so the loop had always
/// been turning at 15.9 ms. That is a latency floor on every UI event; §13.8 is
/// also the record of this comment once claiming it was a throughput floor, and
/// of the measurement that said it was not.
const POLL: Duration = Duration::from_millis(5);

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::{Display, ListeningSocket};

use platform::{FrameStyle, PlatformConfig};
use state::{ClientState, Decorations, State};

struct Args {
    once: bool,
    socket: Option<String>,
    /// §36. When set, the compositor also accepts Wayland connections as ring
    /// slots in the control region it creates at this path — the `accept()` half
    /// of §36.2's table. The socket listener stays up alongside it.
    wayland_ring: Option<String>,
    /// §36, cross-VM. Publish the ring region out of the guest's own RAM over
    /// this cold path, instead of creating it as a file.
    wayland_cold: Option<String>,
    idle_timeout: Duration,
    dump_frames: Option<std::path::PathBuf>,
    debug_icon: bool,
    simulate_wm: bool,
    /// Force the poll loop back onto `Sleep`, with its 15.6 ms quantum.
    ///
    /// The control for the cadence question (docs/waycompose-design.md §13.8). One binary,
    /// one flag, everything else identical -- which is the only way to attribute
    /// a frame rate to the wait rather than to the tree, the transport or the
    /// instrument, and §13.8 is a record of getting that wrong first.
    poll_coarse: bool,
    log: Option<std::path::PathBuf>,
    /// Exit successfully only after this many frames have been presented.
    /// `--once` alone proves a window was made; this proves pixels reached it.
    expect_presents: usize,
    /// Arenas to attach before any client connects, as `id:size:path`.
    ///
    /// ADDED FOR STAGE 4. An arena is not a per-message fact and cannot be one:
    /// it is the region both sides mapped *before* any protocol traffic, so
    /// that a `wl_shm` pool can cross by arithmetic rather than by opening
    /// anything (`wc_handle::arena`). In the real deployment the attach is
    /// shared_vulkan's cold-path handshake, which has run long before a
    /// compositor starts; here it is a flag, which is the same fact supplied
    /// the same way — out of band.
    arenas: Vec<(u32, u64, String)>,
    /// DYNAMIC ARENA, fixture only: where to find regions the peer grew into.
    /// See `wc_handle::arena::set_grow_dir` for why the deployment needs none.
    arena_grow_dir: Option<String>,
    /// Arenas to **provide** rather than open, as `id:size`.
    ///
    /// ADDED FOR STAGE 7, and it is the substrate change (docs/waycompose-design.md §12).
    /// A `--arena` is a file both sides open; a `--arena-guest-ram` is memory
    /// this process commits and pins inside the virtual machine, stamped so
    /// that a process on the *host* can find those pages in QEMU's memory
    /// backend and write into them. The compositor prints the run id, which is
    /// the only thing the host needs and the only thing that crosses.
    guest_arenas: Vec<(u32, u64)>,
    /// Where to write the **page frame list** of every region this process
    /// commits, so the host can locate it by translation instead of by
    /// searching guest RAM for its stamps.
    ///
    /// # Why this is the flag that unblocks the guest harnesses
    ///
    /// The content sweep is not merely slow, it is **unreliable on a loaded
    /// machine**, and five consecutive guest runs demonstrated it: `16377 of
    /// 16384 pages`, `1 page index claimed twice`, `28 page index(es) claimed
    /// twice`. A duplicate is not the stale-stamp case — `new_run_id` hashes
    /// the millisecond with the pid, so a previous run's stamps land in
    /// `other_runs` and the tool counts them separately — it is two pages
    /// carrying *this* run's id at the same page index at two different file
    /// offsets. Whatever the cause (the flat view reaching one guest page at
    /// two offsets, or pages moving under a sweep that takes seconds), the
    /// sweep is reading **contents that are allowed to change while it reads
    /// them**, over 13 GiB, and it is a coin flip.
    ///
    /// `SuperfetchPfnQuery` reads the kernel's page-frame database rather than
    /// the extent's contents. **No stamps are read, so neither a stale stamp
    /// nor a duplicate is expressible**, and it is 0.064 s against the sweep's
    /// 2.3 s. That is the whole argument: it turns a locate from something that
    /// usually works into something that either works or says why.
    ///
    /// The file is written where *both sides can see it* — the harnesses
    /// already share a folder with the guest for `--log` — because the frame
    /// list is 16 bytes a page and a 64 MiB arena's is 256 KiB, which is a file
    /// and not a command-line argument.
    ///
    /// One file per committed region: exactly `PATH` when this process commits
    /// one region, and `PATH.<id>` for any region after the first, because a
    /// second region committed for the outbound window (§28's split) has its
    /// own run id and needs its own locate.
    arena_frames: Option<String>,
    /// Arenas to provide as a **file both sides open**, as `id:size:path`.
    ///
    /// ADDED FOR STAGE 10. `--arena` opens the region read-only, which is all a
    /// compositor reading a client's pixels needs and is why it cannot give any
    /// of the region away. This opens the same file read-write and marks it
    /// provided, which is what `--arena-outbound` requires. It is the local twin
    /// of `--arena-guest-ram`: on a machine with no hypervisor there is nothing
    /// to locate pages from outside, so the substrate is a file and everything
    /// above `wc_handle::arena` is identical.
    provide_arenas: Vec<(u32, u64, String)>,
    /// How many bytes of which arena are **this process's to hand to clients**,
    /// as `id:bytes`.
    ///
    /// ADDED FOR STAGE 10, and it is docs/waycompose-design.md §12.11 item 3. A `wl_shm`
    /// pool is created by the client and read here; a `wl_keyboard` keymap is
    /// created here and read by the client. The second direction had a wire
    /// format from stage 1 and no statement of *which bytes* were the
    /// compositor's, so it had no allocator either. This is that statement: the
    /// last `bytes` bytes of arena `id`, which the peer's allocator is told to
    /// leave alone.
    outbound: Option<(u32, u64)>,
    /// Where objects this process creates **for a client to open** are put, and
    /// how the client spells that directory.
    ///
    /// ADDED FOR STAGE 6, and it is the mirror image of stage 4's `--pool-dir`
    /// / `--peer-prefix` on the proxy. A `wl_shm` pool is created by the client
    /// and opened by the compositor; a `wl_keyboard` keymap is created by the
    /// compositor and opened by the client. The second direction has no
    /// transport at all without this: a keymap in a `Local\wc-…` section is
    /// perfectly well-formed and cannot be opened by a Linux process
    /// (docs/waycompose-design.md §9.10 item 5).
    ///
    /// Two flags rather than one `local:peer` argument, because both values are
    /// paths and one of them has a drive letter in it.
    shared_dir: Option<String>,
    shared_peer_prefix: String,
    /// Which of `input::LAYOUTS` to compile. Empty means "ask Windows", which
    /// is what a user wants and what a harness cannot depend on.
    layout: Option<String>,
    /// The size of the one `wl_output`, as `WxH`.
    ///
    /// ADDED FOR STAGE 9. A flag rather than `GetSystemMetrics` because the
    /// harness has to be able to ask for a known desktop, and because the
    /// headless platform has no desktop to ask. The Win32 platform ought to
    /// override it with the real monitor and does not yet; that is named in
    /// docs/waycompose-design.md §13.9 rather than left for someone to find.
    output: (i32, i32),
    /// One scripted click, as `X,Y[,BUTTON][,MS]`. Headless only.
    ///
    /// ADDED FOR STAGE 9, to open a menu. See `platform::headless::Click`.
    #[cfg(not(windows))]
    click: Option<platform::Click>,
    /// Which decoration mode to advertise, as `server`, `client` or `tabs`.
    ///
    /// ADDED FOR STAGE 17, and given a third value by TABS IN THE TITLE BAR. See
    /// [`Decorations`] for why it defaults to `server`, what a run that asks
    /// for `client` is for, and what `tabs` changes on the Win32 side.
    decorations: Decorations,
    /// Whether `--decorations tabs` also calls `DwmExtendFrameIntoClientArea`.
    ///
    /// ADDED FOR TABS IN THE TITLE BAR, defaulting to on, and the reason it can be turned
    /// off is in [`platform::FrameStyle`]: the DWM call is the half of the
    /// arrangement that is *not* load-bearing, and a run with it off is how
    /// that claim was measured rather than asserted.
    dwm_glass: bool,
    /// Whether `--decorations tabs` keeps Windows' own caption buttons.
    ///
    /// ADDED FOR TABS IN THE TITLE BAR, defaulting to on, and it is what makes the
    /// arrangement Windows' rather than an imitation — see
    /// [`platform::FrameStyle::caption_buttons`]. `off` is the control leg: the
    /// same window with the client's pixels over the whole caption and no
    /// buttons at all.
    caption_buttons: bool,
    /// Where to bind the conformance-suite control socket.
    ///
    /// ADDED FOR STAGE 26. `--click` is one scripted event decided before the
    /// process starts; this is the general case — a peer that positions
    /// windows and moves a pointer while the run is happening, because a test
    /// suite decides where to click based on what the client did. Linux only:
    /// see `crate::control` for why the thing it stands in for is Windows.
    control: Option<String>,
}

/// The control socket, where one can exist, and nothing at all where one
/// cannot.
///
/// A unit *struct* on the platform that has none, rather than `()`, so that the
/// plumbing which carries it is ordinary code on both: a `()` makes every
/// binding of it a `clippy::let_unit_value`, and silencing that lint at four
/// sites is worse than declaring a type. Not a *stub* of `crate::control`
/// either — that module is `AF_UNIX` and `std::os::unix::net`, and compiling a
/// do-nothing Windows version of it would be exactly the "implemented means
/// compiled" pattern docs/waycompose-design.md §14.9 is about.
#[cfg(unix)]
type ControlHook = Option<control::Control>;
#[cfg(not(unix))]
pub struct ControlHook;

#[cfg(unix)]
fn open_control(
    path: Option<&str>,
    events: std::sync::mpsc::Sender<platform::FromUi>,
) -> Result<ControlHook, u8> {
    let Some(path) = path else { return Ok(None) };
    match control::Control::bind(path, events) {
        Ok(c) => Ok(Some(c)),
        Err(e) => {
            eprintln!("wc-compositor: could not bind --control {path}: {e}");
            Err(2)
        }
    }
}

/// Refuse rather than ignore. A harness that asked for a control socket and
/// silently did not get one would sit waiting for a connection that is never
/// answered, which is the least informative failure available.
#[cfg(not(unix))]
fn open_control(
    path: Option<&str>,
    _events: std::sync::mpsc::Sender<platform::FromUi>,
) -> Result<ControlHook, u8> {
    if path.is_some() {
        eprintln!("wc-compositor: --control is a Linux-only hook; there is no AF_UNIX listener here");
        return Err(2);
    }
    Ok(ControlHook)
}

fn parse_args(argv: &[String]) -> Result<Args, u8> {
    let mut args = Args {
        once: false,
        socket: None,
        wayland_ring: None,
        wayland_cold: None,
        idle_timeout: Duration::from_secs(30),
        dump_frames: None,
        debug_icon: false,
        simulate_wm: false,
        poll_coarse: false,
        log: None,
        expect_presents: 0,
        arenas: Vec::new(),
        arena_grow_dir: None,
        guest_arenas: Vec::new(),
        arena_frames: None,
        provide_arenas: Vec::new(),
        outbound: None,
        shared_dir: None,
        shared_peer_prefix: String::new(),
        layout: None,
        output: (1920, 1080),
        #[cfg(not(windows))]
        click: None,
        decorations: Decorations::default(),
        dwm_glass: true,
        caption_buttons: true,
        control: None,
    };
    let mut it = argv.iter().skip(1).cloned();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--once" => args.once = true,
            "--debug-icon" => args.debug_icon = true,
            "--simulate-wm" => args.simulate_wm = true,
            "--poll-coarse" => args.poll_coarse = true,
            "--log" => args.log = it.next().map(Into::into),
            "--arena-frames" => args.arena_frames = it.next(),
            "--socket" => args.socket = it.next(),
            "--wayland-ring" => args.wayland_ring = it.next(),
            "--wayland-cold" => args.wayland_cold = it.next(),
            "--dump-frames" => args.dump_frames = it.next().map(Into::into),
            // `id:size:path`, split from the left twice so a Windows path with
            // a drive letter in it survives — `0:67108864:Z:\tmp\wc-arena` is
            // the normal spelling and a naive split on ':' would eat it.
            "--arena" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --arena needs id:size:path");
                    return Err(2);
                };
                let mut parts = v.splitn(3, ':');
                let (Some(id), Some(size), Some(path)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    eprintln!("wc-compositor: --arena wants id:size:path, got {v:?}");
                    return Err(2);
                };
                let (Ok(id), Ok(size)) = (id.parse::<u32>(), size.parse::<u64>()) else {
                    eprintln!("wc-compositor: --arena {v:?}: id and size must be numbers");
                    return Err(2);
                };
                args.arenas.push((id, size, path.to_owned()));
            }
            // `id:size`. No path, because there is nothing to name: the region
            // is this process's own guest RAM and the host finds it by content.
            "--arena-grow-dir" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --arena-grow-dir needs a directory");
                    return Err(2);
                };
                args.arena_grow_dir = Some(v.to_owned());
            }
            "--arena-guest-ram" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --arena-guest-ram needs id:size");
                    return Err(2);
                };
                let mut p = v.splitn(2, ':');
                let (Some(id), Some(size)) = (p.next(), p.next()) else {
                    eprintln!("wc-compositor: --arena-guest-ram wants id:size, got {v:?}");
                    return Err(2);
                };
                let (Ok(id), Ok(size)) = (id.parse::<u32>(), size.parse::<u64>()) else {
                    eprintln!("wc-compositor: --arena-guest-ram {v:?}: id and size must be numbers");
                    return Err(2);
                };
                args.guest_arenas.push((id, size));
            }
            // `id:size:path`, split from the left twice for the same reason
            // `--arena` is: a Windows path has a colon two characters in.
            "--arena-provide" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --arena-provide needs id:size:path");
                    return Err(2);
                };
                let mut parts = v.splitn(3, ':');
                let (Some(id), Some(size), Some(path)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    eprintln!("wc-compositor: --arena-provide wants id:size:path, got {v:?}");
                    return Err(2);
                };
                let (Ok(id), Ok(size)) = (id.parse::<u32>(), size.parse::<u64>()) else {
                    eprintln!("wc-compositor: --arena-provide {v:?}: id and size must be numbers");
                    return Err(2);
                };
                args.provide_arenas.push((id, size, path.to_owned()));
            }
            "--arena-outbound" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --arena-outbound needs id:bytes");
                    return Err(2);
                };
                let mut p = v.splitn(2, ':');
                let (Some(id), Some(bytes)) = (p.next(), p.next()) else {
                    eprintln!("wc-compositor: --arena-outbound wants id:bytes, got {v:?}");
                    return Err(2);
                };
                let (Ok(id), Ok(bytes)) = (id.parse::<u32>(), bytes.parse::<u64>()) else {
                    eprintln!("wc-compositor: --arena-outbound {v:?}: id and bytes must be numbers");
                    return Err(2);
                };
                args.outbound = Some((id, bytes));
            }
            "--control" => args.control = it.next(),
            "--shared-dir" => args.shared_dir = it.next(),
            "--shared-peer-prefix" => args.shared_peer_prefix = it.next().unwrap_or_default(),
            "--layout" => args.layout = it.next(),
            // `X,Y[,BUTTON][,MS]`. ADDED FOR STAGE 9. Accepted and ignored on
            // Windows rather than rejected, so one harness line runs on both.
            "--click" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --click needs X,Y[,BUTTON][,MS]");
                    return Err(2);
                };
                #[cfg(not(windows))]
                {
                    let f: Vec<&str> = v.split(',').collect();
                    let (Some(Ok(x)), Some(Ok(y))) =
                        (f.first().map(|s| s.parse::<f64>()), f.get(1).map(|s| s.parse::<f64>()))
                    else {
                        eprintln!("wc-compositor: --click wants X,Y[,BUTTON][,MS], got {v:?}");
                        return Err(2);
                    };
                    args.click = Some(platform::Click {
                        x,
                        y,
                        button: f
                            .get(2)
                            .and_then(|s| s.parse::<u32>().ok())
                            .unwrap_or(input::evdev::BTN_LEFT),
                        after: Duration::from_millis(
                            f.get(3).and_then(|s| s.parse::<u64>().ok()).unwrap_or(4000),
                        ),
                    });
                }
                #[cfg(windows)]
                eprintln!(
                    "wc-compositor: --click {v:?} ignored; on Windows a click comes from \
                     SendInput in another process"
                );
            }
            // `WxH`. ADDED FOR STAGE 9.
            "--output" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --output needs WxH");
                    return Err(2);
                };
                let mut p = v.split(['x', 'X']);
                let (Some(Ok(w)), Some(Ok(h)), None) = (
                    p.next().map(str::parse::<i32>),
                    p.next().map(str::parse::<i32>),
                    p.next(),
                ) else {
                    eprintln!("wc-compositor: --output wants WxH, got {v:?}");
                    return Err(2);
                };
                if w <= 0 || h <= 0 {
                    eprintln!("wc-compositor: --output {v:?}: both sides must be positive");
                    return Err(2);
                }
                args.output = (w, h);
            }
            "--expect-presents" => {
                args.expect_presents = it.next().and_then(|s| s.parse().ok()).unwrap_or(0)
            }
            // ADDED FOR STAGE 17, and it is the flag §22.2 is about.
            "--decorations" => {
                let Some(v) = it.next() else {
                    eprintln!("wc-compositor: --decorations needs server, client or tabs");
                    return Err(2);
                };
                let Some(d) = Decorations::parse(&v) else {
                    eprintln!(
                        "wc-compositor: --decorations {v}: expected server, client or tabs"
                    );
                    return Err(2);
                };
                args.decorations = d;
            }
            // ADDED FOR TABS IN THE TITLE BAR. Only meaningful with `--decorations tabs`.
            "--dwm-glass" => {
                let v = it.next().unwrap_or_default();
                match v.as_str() {
                    "on" => args.dwm_glass = true,
                    "off" => args.dwm_glass = false,
                    _ => {
                        eprintln!("wc-compositor: --dwm-glass {v}: expected on or off");
                        return Err(2);
                    }
                }
            }
            // ADDED FOR TABS IN THE TITLE BAR. Only meaningful with `--decorations tabs`.
            "--caption-buttons" => {
                let v = it.next().unwrap_or_default();
                match v.as_str() {
                    "on" => args.caption_buttons = true,
                    "off" => args.caption_buttons = false,
                    _ => {
                        eprintln!("wc-compositor: --caption-buttons {v}: expected on or off");
                        return Err(2);
                    }
                }
            }
            "--timeout" => {
                args.idle_timeout = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(args.idle_timeout)
            }
            other => {
                eprintln!("wc-compositor: unknown argument {other}");
                return Err(2);
            }
        }
    }
    Ok(args)
}

/// Send this process's stdout and stderr to a file.
///
/// # Why a compositor needs this and a server did not
///
/// Stages 1 and 2 were console programs whose output a shell could redirect.
/// This one puts up windows, and that changes where its output goes in two
/// ways that both bite:
///
/// * Under Wine, `wine explorer /desktop=` — which is how the harness gets a
///   virtual desktop, and therefore how the window gets a caption drawn at all
///   — launches the program as a child of `explorer.exe` and gives it a console
///   window **inside** the desktop. A shell redirect on the outside captures
///   the loader's chatter and nothing else. Measured: `scripts/wc-window.sh`
///   was asserting on an empty log while the compositor was working perfectly
///   and printing to a console visible only in a screenshot.
/// * On real Windows the same problem arrives from the other direction. A
///   process launched without a console has no `STD_OUTPUT_HANDLE` at all, and
///   `println!` goes nowhere. `scripts/wc-guest.sh` runs binaries over ssh
///   today, which supplies one; a compositor launched from a shortcut or a
///   service would not have that.
///
/// `SetStdHandle` before the first write rather than a logging framework:
/// every `println!` in this program then lands in the file with no other
/// change, and Rust's `Stdout` fetches the handle per write, so this is
/// enough. It is called first thing in `main` for that reason.
#[cfg(windows)]
fn redirect_output(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{
        SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    let file = std::fs::File::create(path)?;
    let handle = file.as_raw_handle();
    // SAFETY: a live handle to a file this process just opened. Both standard
    // handles are pointed at it; the process keeps it open for its lifetime by
    // leaking it, which is what `std::mem::forget` is doing and is correct
    // here — closing it would leave the standard handles dangling.
    unsafe {
        if SetStdHandle(STD_OUTPUT_HANDLE, handle.cast()) == 0
            || SetStdHandle(STD_ERROR_HANDLE, handle.cast()) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    std::mem::forget(file);
    Ok(())
}

#[cfg(not(windows))]
fn redirect_output(path: &std::path::Path) -> std::io::Result<()> {
    // Not needed on Unix — a shell redirect reaches this program — but
    // supported so the harness can use one command line on both platforms.
    use std::os::fd::AsRawFd;
    let file = std::fs::File::create(path)?;
    // SAFETY: `dup2` onto the two standard descriptors with a live fd. The
    // file is leaked deliberately, as on Windows.
    unsafe {
        if libc::dup2(file.as_raw_fd(), 1) < 0 || libc::dup2(file.as_raw_fd(), 2) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Write one region's page frame list where the host can read it, and say what
/// happened in a few words that fit on the region's own report line.
///
/// # Why a failure here is a note and not an exit
///
/// Because the sweep still works. A frame list makes the locate fast and
/// deterministic; its absence makes the locate slow and a coin flip, which is
/// bad and is not "the compositor cannot run". A process that refused to start
/// because it could not open the PFN database would take a machine that has
/// been working all along and stop it, which is a worse failure than the one
/// being fixed.
///
/// So the reason travels instead, on the line a harness is already grepping,
/// and `wc-arena-host` falls back to the sweep. That is the only fallback in
/// the path and it is the right one: it degrades *speed and reliability*, never
/// correctness, and it says so out loud.
fn publish_frames(args: &Args, id: u32, pages: u32, written: &mut usize) -> String {
    let Some(base) = &args.arena_frames else {
        return String::new();
    };
    // Exactly `PATH` for the first region and `PATH.<id>` after it. A second
    // region is the outbound window's own commit, which has its own run id and
    // needs its own locate — and its own file, or the two would overwrite each
    // other and the survivor would be located against the wrong run.
    let path = if *written == 0 { base.clone() } else { format!("{base}.{id}") };
    let (frames, ms) = match crate::grow::frames(id, pages) {
        Ok(v) => v,
        Err(e) => return format!(", frames=none ({e})"),
    };
    if frames.len() != pages as usize {
        return format!(
            ", frames=none (the query returned {} frames for a {pages}-page region)",
            frames.len()
        );
    }
    match std::fs::write(&path, crate::grow::frame_list_text(&frames)) {
        Ok(()) => {
            *written += 1;
            format!(", frames={path} ({} in {ms} ms)", frames.len())
        }
        Err(e) => format!(", frames=none ({path}: {e})"),
    }
}

/// What to do about the file arenas named on a command line.
///
/// Split out of [`run_from`] so it can be *tested*, which is the whole reason
/// it exists as a function. The refusal it implements was added over
/// `--arena-provide` alone while its own message named `--arena` too, and
/// `--arena <id>:<size>:<path>` therefore attached a file in silence — on the
/// two harnesses that used exactly that flag against the real guest, over the
/// export §10.3 measured incoherent for live mappings. Nothing could have
/// caught that: the check lived four statements into a function that starts a
/// compositor, so no test could reach it, and the message asserted a coverage
/// the condition did not have. §41.4.
#[derive(Debug, PartialEq, Eq)]
enum FileArenaVerdict {
    /// No `--arena` and no `--arena-provide`: nothing to decide.
    NoFileArena,
    /// A file arena, and no `SV_ALLOW_FILE_ARENA`. The run stops.
    Refuse(String),
    /// A file arena a fixture asked for by name. The run continues and says so,
    /// so that a fixture cannot be mistaken for a result in a log read later.
    Warn(String),
}

fn file_arena_verdict(file_arenas: usize, allowed: bool) -> FileArenaVerdict {
    if file_arenas == 0 {
        return FileArenaVerdict::NoFileArena;
    }
    if !allowed {
        return FileArenaVerdict::Refuse(
            "wc-compositor: the FILE arena is not a target. `--arena` and \
             `--arena-provide` open files, which is a Wine-only fixture for a machine \
             with no hypervisor. Use `--arena-guest-ram <id>:<bytes>`. Set \
             SV_ALLOW_FILE_ARENA=1 for a fixture run — never to make a guest run pass."
                .to_owned(),
        );
    }
    FileArenaVerdict::Warn(format!(
        "wc-compositor: WARNING: FILE arena in use ({file_arenas} region(s), \
         SV_ALLOW_FILE_ARENA is set). This is a fixture, not a target; guest RAM is \
         the only real substrate."
    ))
}

#[cfg(test)]
mod file_arena_tests {
    use super::*;

    /// The regression that motivated splitting this out. `--arena` and
    /// `--arena-provide` are counted together, so a run that names only the
    /// first is refused exactly like a run that names only the second — which
    /// was not true when the condition read `!args.provide_arenas.is_empty()`.
    #[test]
    fn either_file_arena_flag_is_refused_without_the_fixture_permission() {
        for n in 1..=3 {
            match file_arena_verdict(n, false) {
                FileArenaVerdict::Refuse(m) => {
                    assert!(m.contains("not a target"), "{m}");
                    // The message names both flags, and now the condition
                    // covers both. A message that claims more coverage than the
                    // code has is how the hole survived review.
                    assert!(m.contains("`--arena`"), "{m}");
                    assert!(m.contains("`--arena-provide`"), "{m}");
                    assert!(m.contains("--arena-guest-ram"), "the way out must be named: {m}");
                }
                other => panic!("{n} file arena(s) were not refused: {other:?}"),
            }
        }
    }

    /// Guest RAM is not a file arena and must never be touched by this check.
    #[test]
    fn a_run_with_no_file_arena_is_not_asked_for_permission() {
        assert_eq!(file_arena_verdict(0, false), FileArenaVerdict::NoFileArena);
        assert_eq!(file_arena_verdict(0, true), FileArenaVerdict::NoFileArena);
    }

    /// A fixture that asked for it by name runs, and **says so**: a fixture
    /// that produced a silent green log is how a file result gets quoted later
    /// as a result about the deployment.
    #[test]
    fn a_fixture_run_is_allowed_and_announces_itself() {
        match file_arena_verdict(2, true) {
            FileArenaVerdict::Warn(m) => {
                assert!(m.contains("WARNING"), "{m}");
                assert!(m.contains("not a target"), "{m}");
                assert!(m.contains("2 region(s)"), "the count is in it: {m}");
            }
            other => panic!("a permitted fixture was not allowed to run: {other:?}"),
        }
    }
}

/// Run the compositor.
///
/// This was `fn main` until the unification (one-binary.md §2.2). It is `pub`
/// so that the guest binary can be the GPU server and the compositor at once —
/// the whole point of one binary per side — while `--no-default-features
/// --features wayland` still produces exactly the executable that shipped
/// before.
pub fn run() -> std::process::ExitCode {
    run_from(std::env::args().collect())
}

/// Run the compositor with an explicit argument vector.
///
/// `argv[0]` is the program name and `argv[1..]` are the arguments, exactly as
/// `std::env::args()` yields them. This exists so that `sv-guest` can consume
/// its own half-selecting argument and hand the rest on unchanged, rather than
/// each half re-reading a process-wide `env::args` that has one extra token in
/// it. one-binary.md §2.2.
pub fn run_from(argv: Vec<String>) -> std::process::ExitCode {
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(code) => return std::process::ExitCode::from(code),
    };

    // Before anything is printed, or the first lines land in the console this
    // is trying to escape.
    if let Some(path) = &args.log
        && let Err(e) = redirect_output(path)
    {
        eprintln!("wc-compositor: could not redirect output to {}: {e}", path.display());
        return std::process::ExitCode::from(2);
    }

    // Arenas first, and before the socket exists. An arena that were attached
    // lazily — on the first `ArenaSlice` that named it — would make the first
    // client of a run take a different path from every later one, and would put
    // a filesystem open on the frame path exactly once in a way no measurement
    // would catch. Attaching here also means a wrong `--arena` is a startup
    // failure with a path in it rather than a pool that cannot be mapped.
    //
    // THE FILE ARENA IS NOT A TARGET, AND `--arena` IS ONE — which is the whole
    // reason this check is *here* rather than only over `provide_arenas` below.
    //
    // The refusal added with `--arena-provide` names both flags in its message
    // and tested only one of them, so `--arena <id>:<size>:<path>` attached a
    // file in silence. That is not an academic gap: `--arena 0:N:Z:\wc11\arena`
    // and `--arena 0:N:Z:\wc32\arena` are exactly what `wc-icon.sh` and
    // `wc-taskbar.sh` passed on every run they ever made against the real
    // guest, over the export §10.3 measured incoherent for live mappings. The
    // escape hatch had a hole in the shape of the defect it was added to close.
    //
    // Both flags open a file; they differ only in whether it is opened
    // read-write, which is what `--arena-provide` adds so that an outbound
    // window is possible on a machine with no hypervisor. Neither is guest RAM,
    // so neither is a target, and one condition covers both.
    match file_arena_verdict(
        args.arenas.len() + args.provide_arenas.len(),
        std::env::var_os("SV_ALLOW_FILE_ARENA").is_some(),
    ) {
        FileArenaVerdict::Refuse(msg) => {
            eprintln!("{msg}");
            return std::process::ExitCode::from(2);
        }
        FileArenaVerdict::Warn(msg) => eprintln!("{msg}"),
        FileArenaVerdict::NoFileArena => {}
    }
    for (id, size, path) in &args.arenas {
        match wc_handle::arena::attach(*id, path, *size) {
            Ok(()) => println!("wc-compositor: arena {id} attached: {size} bytes at {path}"),
            Err(e) => {
                eprintln!("wc-compositor: could not attach arena {id} at {path}: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    // DYNAMIC ARENA. On the file fixture the peer can grow into regions this
    // process was never told about, because `--arena` names one and there is no
    // request in the protocol for a second. This says where to find them. On
    // the guest-RAM deployment it is never needed: the regions are this
    // process's own memory and are attached before the host can locate them.
    if let Some(dir) = &args.arena_grow_dir {
        match wc_handle::arena::set_grow_dir(std::path::Path::new(dir)) {
            Ok(()) => println!("wc-compositor: regions the peer grows will be read from {dir}"),
            Err(e) => {
                eprintln!("wc-compositor: could not set the arena growth directory {dir}: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }
    // And the ones this process provides. The run id goes out on the *first*
    // line of the report because the host's locator cannot start without it and
    // a harness has to be able to grep for it; `locked=false` goes on the same
    // line because a region that is findable now and movable later is the one
    // failure mode that would look like a working arena for the first few
    // seconds. See `wc_handle::arena::attach_guest_ram`.
    let mut frames_written = 0usize;
    for (id, size) in &args.guest_arenas {
        match wc_handle::arena::attach_guest_ram(*id, *size) {
            Ok(r) => {
                // The frame list, on the same line as the run id and for the
                // same reason: the host's locator cannot start without the run
                // id, and it should not have to *guess* whether a frame list
                // exists. A harness greps one line and gets both.
                let note = publish_frames(&args, *id, r.pages, &mut frames_written);
                println!(
                    "wc-compositor: arena {id} provided from guest RAM: \
                     run 0x{:016x}, {} bytes, {} pages, locked={}{note}",
                    r.run_id, r.len, r.pages, r.locked
                );
            }
            Err(e) => {
                eprintln!("wc-compositor: could not provide arena {id} from guest RAM: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    // And the ones this process provides as a file. See `--arena-provide`: this
    // is `--arena` opened read-write, which is what makes an outbound window
    // possible on a machine with no hypervisor in it.
    //
    // THE FILE ARENA IS NOT A TARGET — guest RAM is (design.md §1a), and
    // `--arena-guest-ram` is how this process provides one. The refusal for
    // both this flag and `--arena` is above, before either is opened: it was
    // here and tested only `provide_arenas`, which let `--arena` through in
    // silence. §41.4.
    for (id, size, path) in &args.provide_arenas {
        match wc_handle::arena::provide_file(*id, path, *size) {
            Ok(()) => println!(
                "wc-compositor: arena {id} provided from a file: {size} bytes at {path}"
            ),
            Err(e) => {
                eprintln!("wc-compositor: could not provide arena {id} from {path}: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    // The window this process may hand to clients, out of an arena it provided.
    // After the arenas and before the socket, because it is a fact about the
    // region rather than about any client, and because the *number* on this
    // line is what the peer's allocator has to be told to stay out of.
    if let Some((id, bytes)) = args.outbound {
        match wc_handle::arena::reserve_outbound(id, bytes) {
            Ok(w) => {
                // DYNAMIC ARENA. A run id here means `reserve_outbound` had to
                // commit a region of its own for the window, because `id` named
                // an arena this process had not provided. The run id goes out on
                // its own line and in the same shape as the data regions',
                // because the host's locator cannot find a region it has not
                // been told the run id of — and a region nobody can locate is
                // one the compositor writes keymaps into that no client ever
                // reads.
                if let Some(run) = w.run_id {
                    let pages = (w.len / wc_handle::arena::GIFT_GRANULARITY) as u32;
                    // Its own region, so its own frame list. Locating it by
                    // sweep would be a second ~2.3 s search of the whole guest
                    // with the same chance of a duplicate, for a region whose
                    // stamps this process is about to write keymaps over.
                    let note = publish_frames(&args, id, pages, &mut frames_written);
                    println!(
                        "wc-compositor: arena {id} provided from guest RAM: \
                         run 0x{run:016x}, {} bytes, {pages} pages, locked=true{note} \
                         (the outbound window's own region)",
                        w.len,
                    );
                }
                println!(
                    "wc-compositor: arena {id} outbound window: offset {}, {} bytes{} \
                     (objects made for clients are arena slices; wc-proxy needs \
                     --arena-outbound {} to leave them alone)",
                    w.offset,
                    w.len,
                    if w.run_id.is_some() {
                        ", the whole of its own region, so every data region can be reclaimed"
                    } else {
                        ", a tail of the data region, which therefore cannot be reclaimed"
                    },
                    w.len
                );
            }
            Err(e) => {
                eprintln!("wc-compositor: could not reserve an outbound window in arena {id}: {e}");
                return std::process::ExitCode::from(2);
            }
        }

        // STAGE 12, and it is a correction rather than a feature. The window's
        // pages carry the stamps the *host* locates this arena by, and a gift
        // written into them before the host has swept takes them with it — the
        // first guest run of an outbound window reported
        // `located 16374 of 16384 pages` and wrote no plan, because smithay
        // seals the keymap in `Seat::add_keyboard`, ten lines below and a full
        // minute before any client exists. docs/waycompose-design.md §14.12 item 1 said
        // that ordering held by construction; it does not, and this is what
        // makes it hold. Only for guest RAM: a file arena has no stamps and
        // nothing is looking for it.
        if args.guest_arenas.iter().any(|(gid, _)| *gid == id)
            && let Err(e) = wc_handle::arena::defer_gifts(id)
        {
            eprintln!("wc-compositor: could not defer gifts into arena {id}: {e}");
            return std::process::ExitCode::from(2);
        } else if args.guest_arenas.iter().any(|(gid, _)| *gid == id) {
            println!(
                "wc-compositor: arena {id} objects made for clients are held in memory until the \
                 first client connects, so that the host can still find every page's stamp"
            );
        }
    }

    // Where objects made for a client go. Before the socket exists, for the
    // same reason the arenas are: a wrong path must be a startup failure with
    // the path in it, not a client that connects and then dies on its first
    // keymap.
    //
    // STAGE 10: with an outbound window this is dead weight and says so, rather
    // than being silently ignored. `Handle::create_shared` prefers the arena,
    // so a run with both flags puts nothing at all in the directory — and a
    // reader of the log who did not know that would spend the afternoon looking
    // for a keymap file that was never going to exist.
    if args.outbound.is_some() && args.shared_dir.is_some() {
        println!(
            "wc-compositor: --shared-dir is unused: with an outbound arena window, objects \
             made for clients are arena slices and no directory is opened"
        );
    }
    if let Some(dir) = &args.shared_dir {
        if args.shared_peer_prefix.is_empty() {
            eprintln!(
                "wc-compositor: --shared-dir needs --shared-peer-prefix: this process cannot \
                 guess how the client spells that directory, and a guess would be wrong on \
                 the deployment that matters"
            );
            return std::process::ExitCode::from(2);
        }
        match wc_handle::publish::set(dir, &args.shared_peer_prefix) {
            Ok(()) => println!(
                "wc-compositor: objects for clients go in {dir} ({:?} to them)",
                args.shared_peer_prefix
            ),
            Err(e) => {
                eprintln!("wc-compositor: could not use --shared-dir {dir}: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    // The two channels. `ui_events` goes UI -> Wayland; the command channel in
    // the other direction is owned by `platform::start`.
    let (event_tx, event_rx) = std::sync::mpsc::channel();

    // Before the platform, because it takes the same sender: the control
    // socket is a *second* source of `FromUi`, sitting exactly where the
    // platform sits, which is the whole of what makes it honest.
    let control = match open_control(args.control.as_deref(), event_tx.clone()) {
        Ok(c) => c,
        Err(code) => return std::process::ExitCode::from(code),
    };

    let started = match platform::start(PlatformConfig {
        events: event_tx,
        // Stage 3 gave Windows the whole frame and stage 17 kept it that way.
        // `--decorations tabs` is what moves it: the caption is folded into the
        // client area by `WM_NCCALCSIZE` so the client's own tab strip lands
        // inside the frame. See `platform::FrameStyle`, which also records why
        // the field used to be a pixel count and is now a decision.
        frame: FrameStyle {
            extend_top: args.decorations.extends_frame(),
            dwm_glass: args.dwm_glass,
            caption_buttons: args.caption_buttons,
        },
        dump_frames: args.dump_frames,
        simulate_wm: args.simulate_wm,
        #[cfg(not(windows))]
        click: args.click,
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wc-compositor: could not start the platform: {e}");
            return std::process::ExitCode::from(2);
        }
    };
    let platform::Started { ui, pump } = started;

    // Which layout the keyboard compiles. `--layout` wins so a harness is not
    // at the mercy of the machine's regional settings; otherwise ask the
    // platform, which on Windows is `GetKeyboardLayout` and everywhere else is
    // the fallback. Done after `platform::start` because on Win32 the answer
    // depends on the thread having a message queue.
    let layout = args
        .layout
        .clone()
        .unwrap_or_else(|| platform::current_layout().to_owned());

    // The Wayland side is built and run entirely inside this closure. Nothing
    // it creates is ever named on the main thread, which is what makes the
    // `!Send` smithay state a non-question rather than a fight with the
    // compiler.
    let exit = Arc::new(AtomicU8::new(1));
    let thread_exit = exit.clone();
    let wayland = std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || {
            let code = wayland_thread(args.once, args.socket, args.wayland_ring, args.wayland_cold, args.idle_timeout, args.debug_icon,
                args.expect_presents, args.poll_coarse, layout, args.output,
                args.decorations, ui, event_rx, control);
            thread_exit.store(code, Ordering::SeqCst);
        });
    let wayland = match wayland {
        Ok(h) => h,
        Err(e) => {
            eprintln!("wc-compositor: could not spawn the wayland thread: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    // Blocks until the Wayland thread sends `Quit`, or the platform stops.
    pump.run();
    let _ = wayland.join();

    std::process::ExitCode::from(exit.load(Ordering::SeqCst))
}

/// Everything Wayland, on its own thread.
///
/// Returns the process exit code: 0 success, 1 loop error, 2 setup failure,
/// 3 idle timeout, 4 the milestone was not reached.
#[allow(clippy::too_many_arguments)]
fn wayland_thread(
    once: bool,
    socket: Option<String>,
    wayland_ring: Option<String>,
    wayland_cold: Option<String>,
    idle_timeout: Duration,
    debug_icon: bool,
    expect_presents: usize,
    poll_coarse: bool,
    layout: String,
    output: (i32, i32),
    decorations: Decorations,
    ui: platform::Ui,
    events: std::sync::mpsc::Receiver<platform::FromUi>,
    control: ControlHook,
) -> u8 {
    use std::io::Write as _;

    // `mut` here rather than in the parameter list so the Windows build, where
    // the hook is a unit struct, does not carry an unused-mut.
    #[cfg(unix)]
    let mut control = control;
    #[cfg(not(unix))]
    let _ = control;

    let display: Display<State> = match Display::new() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("wc-compositor: could not create display: {e}");
            ui.send(platform::ToUi::Quit);
            return 2;
        }
    };
    let dh = display.handle();
    let mut state = State::new(&dh, ui, events, debug_icon, &layout, output, decorations);

    let listener = match socket {
        Some(name) => ListeningSocket::bind(&name),
        None => ListeningSocket::bind_auto("wayland", 1..32),
    };
    let listener = match listener {
        Ok(l) => l,
        Err(e) => {
            eprintln!("wc-compositor: could not bind socket: {e}");
            state.quit();
            return 2;
        }
    };

    // Fixed, greppable, and explicitly flushed. A Windows binary under Wine
    // with a redirected stdout is fully buffered, so a harness waiting for this
    // line would otherwise wait forever while the compositor worked perfectly.
    println!(
        "wc-compositor: listening on {}",
        listener
            .socket_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    );
    let _ = std::io::stdout().flush();

    let mut display = display;
    let deadline = Instant::now() + idle_timeout;
    let mut code = 1u8;

    // One per thread, reused every turn. See `tick.rs`: on Windows a
    // `thread::sleep(5ms)` is a 15.9 ms sleep, and 15.9 ms against
    // `tick_frames`' 16 ms gate halves the frame rate of anything that animates.
    let ticker = if poll_coarse { tick::Ticker::coarse() } else { tick::Ticker::new() };
    println!("wc-compositor: poll interval {POLL:?} using {}", ticker.describe());
    let _ = std::io::stdout().flush();

    // §36. `accept()` on the ring directory, beside `accept()` on the socket.
    // Both produce a `wc_transport::Stream` for `insert_client`; only one of
    // them involves anything crossing the VM boundary.
    #[cfg(unix)]
    let mut ring_link = {
        // Two ways to obtain the region and one directory to poll afterwards.
        // `--wayland-cold` is the cross-VM one: the region is guest RAM the host
        // locates. `--wayland-ring` is a file, for one machine.
        let made = match (wayland_cold.as_deref(), wayland_ring.as_deref()) {
            (Some(port), _) => {
                #[cfg(target_os = "linux")]
                {
                    Some(ringlink::RingLink::publish(port).map(|rl| (rl, format!("guest RAM published on {port}"))))
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = port;
                    Some(Err("--wayland-cold needs the Linux guest-RAM provider".to_string()))
                }
            }
            (None, Some(path)) => {
                Some(ringlink::RingLink::create(path).map(|rl| (rl, path.to_string())))
            }
            (None, None) => None,
        };
        match made {
            None => None,
            Some(Ok((rl, what))) => {
                println!("wc-compositor: accepting Wayland rings in {what}");
                let _ = std::io::stdout().flush();
                Some(rl)
            }
            Some(Err(e)) => {
                eprintln!("wc-compositor: could not obtain the ring region: {e}");
                state.quit();
                return 2;
            }
        }
    };

    loop {
        // The ring's accept, first, so a connection that arrived between turns
        // is dispatched in the same turn as one that arrived on the socket.
        #[cfg(unix)]
        if let Some(rl) = ring_link.as_mut() {
            for (slot, stream) in rl.accept() {
                let index = (state.clients_seen + 1) as u32;
                match display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::new(index)))
                {
                    Ok(_) => {
                        state.clients_seen += 1;
                        println!(
                            "wc-compositor: client {index} accepted on ring slot {slot} \
                             (no socket crossed the VM boundary)"
                        );
                        let _ = std::io::stdout().flush();
                    }
                    Err(e) => eprintln!("wc-compositor: ring slot {slot} not inserted: {e}"),
                }
            }
        }

        match listener.accept() {
            Ok(Some(stream)) => {
                // The ordinal is assigned *before* the insert, so it counts
                // connections in accept order and matches what a peer that
                // connected in that order believes. See `ClientState::index`.
                let index = (state.clients_seen + 1) as u32;
                match display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::new(index)))
                {
                    Ok(_) => {
                        state.clients_seen += 1;
                        // STAGE 12. The first client is the earliest moment at
                        // which anything could want an object this compositor
                        // made, and — over guest RAM — the earliest moment at
                        // which it is safe to write one: the client arrived
                        // through a proxy that was started from a plan, and the
                        // plan only exists if the host's sweep of the region
                        // succeeded. See `wc_handle::arena::defer_gifts`.
                        match wc_handle::arena::release_gifts() {
                            Ok((0, _)) => {}
                            Ok((n, bytes)) => println!(
                                "wc-compositor: client {} released {n} held object(s), \
                                 {bytes} bytes, into the arena",
                                state.clients_seen
                            ),
                            Err(e) => {
                                eprintln!("wc-compositor: could not release held objects: {e}")
                            }
                        }
                    }
                    Err(e) => eprintln!("wc-compositor: could not insert client: {e}"),
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("wc-compositor: accept failed: {e}");
                break;
            }
        }

        // Order matters. UI events first, so a resize or a close reaches the
        // client in the same turn it arrived; then dispatch, which is where
        // commits land and where `present` runs; then frame callbacks, so a
        // client is invited to draw the next frame only after this one has
        // been handed to the platform; then flush.
        // Before `pump_ui_events`, so an event the control socket produces is
        // delivered to the client in the same turn the request arrived rather
        // than one poll interval later. See `control::Control::service`.
        #[cfg(unix)]
        if let Some(c) = control.as_mut() {
            c.service(&state);
        }

        state.pump_ui_events();
        if state.stopped {
            println!("wc-compositor: the platform stopped; winding up");
            code = 0;
            break;
        }

        // §36. Before dispatch: bytes that arrived from the ring have to be in
        // the local socket *before* smithay reads it, or they wait a whole 5 ms
        // tick. `pump` also drains what smithay wrote last turn, which is why it
        // is one call and not two.
        #[cfg(unix)]
        if let Some(rl) = ring_link.as_mut() {
            rl.pump();
        }

        if let Err(e) = display.dispatch_clients(&mut state) {
            eprintln!("wc-compositor: dispatch failed: {e}");
            break;
        }
        state.tick_frames();
        // STAGE 21. After dispatch, because dispatch is where an inhibitor is
        // created, destroyed, or silently lost with the client that held it —
        // and the last of those three is the one nothing else would notice.
        state.tick_idle_inhibit();
        if let Err(e) = display.flush_clients() {
            eprintln!("wc-compositor: flush failed: {e}");
            break;
        }

        let mut live = 0;
        display
            .handle()
            .backend_handle()
            .with_all_clients(|_| live += 1);

        if once && state.clients_seen > 0 && live == 0 {
            println!(
                "wc-compositor: --once and the last client has gone; {} toplevel(s), {} frame(s) presented",
                state.toplevels, state.presents
            );
            code = if state.toplevels == 0 {
                eprintln!("wc-compositor: no xdg_toplevel was ever created");
                4
            } else if state.presents < expect_presents {
                eprintln!(
                    "wc-compositor: {} frames presented, expected at least {expect_presents}",
                    state.presents
                );
                4
            } else {
                0
            };
            break;
        }

        if Instant::now() > deadline {
            eprintln!("wc-compositor: idle timeout reached; exiting");
            code = 3;
            break;
        }

        ticker.wait(POLL);
    }

    // Said on the way out, always. "The taskbar icon is the generic one" has
    // three causes that look identical on a photograph — nobody published, the
    // publish did not match the app id, or the window was never asked — and
    // these two numbers separate the last from the first two.
    println!(
        "wc-compositor: icons: {} window(s) got one, {} had none to get{}",
        state.icons_applied,
        state.icons_missing,
        if state.icons_is_empty() {
            "; no resolver published anything this run"
        } else {
            ""
        }
    );
    let _ = std::io::stdout().flush();
    state.quit();
    code
}
