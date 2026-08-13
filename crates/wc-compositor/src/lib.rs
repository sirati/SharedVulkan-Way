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
//! wepoll handle is not a socket (docs/design.md §3.3), so the backend cannot
//! nest inside it. The Wayland thread therefore polls on a 5 ms wait, exactly
//! as stages 1 and 2 did — though on Windows it took until stage 8 for that to
//! be 5 ms rather than `Sleep`'s 15.9 ms quantum (`tick.rs`). It is a latency
//! floor on input and on how fast a `WM_CLOSE` reaches the client, and it is
//! honest about being one; it is not a throughput limit, because a single turn
//! drains every pending message.

mod cursor;
mod icon;
mod input;
mod platform;
mod state;
mod surface;
mod tick;

/// How long the Wayland thread waits between turns.
///
/// 5 ms since stage 1, and unchanged — what changed in stage 8 is that it is
/// now *actually* 5 ms. See `tick.rs` and docs/design.md §13.8: on Windows this
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
    idle_timeout: Duration,
    dump_frames: Option<std::path::PathBuf>,
    debug_icon: bool,
    simulate_wm: bool,
    /// Force the poll loop back onto `Sleep`, with its 15.6 ms quantum.
    ///
    /// The control for the cadence question (docs/design.md §13.8). One binary,
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
    /// Arenas to **provide** rather than open, as `id:size`.
    ///
    /// ADDED FOR STAGE 7, and it is the substrate change (docs/design.md §12).
    /// A `--arena` is a file both sides open; a `--arena-guest-ram` is memory
    /// this process commits and pins inside the virtual machine, stamped so
    /// that a process on the *host* can find those pages in QEMU's memory
    /// backend and write into them. The compositor prints the run id, which is
    /// the only thing the host needs and the only thing that crosses.
    guest_arenas: Vec<(u32, u64)>,
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
    /// ADDED FOR STAGE 10, and it is docs/design.md §12.11 item 3. A `wl_shm`
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
    /// (docs/design.md §9.10 item 5).
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
    /// docs/design.md §13.9 rather than left for someone to find.
    output: (i32, i32),
    /// One scripted click, as `X,Y[,BUTTON][,MS]`. Headless only.
    ///
    /// ADDED FOR STAGE 9, to open a menu. See `platform::headless::Click`.
    #[cfg(not(windows))]
    click: Option<platform::Click>,
    /// Which decoration mode to advertise, as `server` or `client`.
    ///
    /// ADDED FOR STAGE 17. See [`Decorations`] for why it defaults to `server`
    /// and what a run that asks for `client` is for.
    decorations: Decorations,
}

fn parse_args(argv: &[String]) -> Result<Args, u8> {
    let mut args = Args {
        once: false,
        socket: None,
        idle_timeout: Duration::from_secs(30),
        dump_frames: None,
        debug_icon: false,
        simulate_wm: false,
        poll_coarse: false,
        log: None,
        expect_presents: 0,
        arenas: Vec::new(),
        guest_arenas: Vec::new(),
        provide_arenas: Vec::new(),
        outbound: None,
        shared_dir: None,
        shared_peer_prefix: String::new(),
        layout: None,
        output: (1920, 1080),
        #[cfg(not(windows))]
        click: None,
        decorations: Decorations::default(),
    };
    let mut it = argv.iter().skip(1).cloned();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--once" => args.once = true,
            "--debug-icon" => args.debug_icon = true,
            "--simulate-wm" => args.simulate_wm = true,
            "--poll-coarse" => args.poll_coarse = true,
            "--log" => args.log = it.next().map(Into::into),
            "--socket" => args.socket = it.next(),
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
                    eprintln!("wc-compositor: --decorations needs server or client");
                    return Err(2);
                };
                let Some(d) = Decorations::parse(&v) else {
                    eprintln!("wc-compositor: --decorations {v}: expected server or client");
                    return Err(2);
                };
                args.decorations = d;
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
    for (id, size, path) in &args.arenas {
        match wc_handle::arena::attach(*id, path, *size) {
            Ok(()) => println!("wc-compositor: arena {id} attached: {size} bytes at {path}"),
            Err(e) => {
                eprintln!("wc-compositor: could not attach arena {id} at {path}: {e}");
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
    for (id, size) in &args.guest_arenas {
        match wc_handle::arena::attach_guest_ram(*id, *size) {
            Ok(r) => println!(
                "wc-compositor: arena {id} provided from guest RAM: \
                 run 0x{:016x}, {} bytes, {} pages, locked={}",
                r.run_id, r.len, r.pages, r.locked
            ),
            Err(e) => {
                eprintln!("wc-compositor: could not provide arena {id} from guest RAM: {e}");
                return std::process::ExitCode::from(2);
            }
        }
    }

    // And the ones this process provides as a file. See `--arena-provide`: this
    // is `--arena` opened read-write, which is what makes an outbound window
    // possible on a machine with no hypervisor in it.
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
            Ok(w) => println!(
                "wc-compositor: arena {id} outbound window: offset {}, {} bytes \
                 (objects made for clients are arena slices; wc-proxy needs \
                 --arena-outbound {} to leave them alone)",
                w.offset, w.len, w.len
            ),
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
        // minute before any client exists. docs/design.md §14.12 item 1 said
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

    let started = match platform::start(PlatformConfig {
        events: event_tx,
        // Stage 3 gives Windows the whole frame. The field is what
        // `DwmExtendFrameIntoClientArea` will move, and every size conversion
        // already goes through it — see `platform::FrameStyle`.
        frame: FrameStyle { extend_top: 0 },
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
            let code = wayland_thread(args.once, args.socket, args.idle_timeout, args.debug_icon,
                args.expect_presents, args.poll_coarse, layout, args.output,
                args.decorations, ui, event_rx);
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
    idle_timeout: Duration,
    debug_icon: bool,
    expect_presents: usize,
    poll_coarse: bool,
    layout: String,
    output: (i32, i32),
    decorations: Decorations,
    ui: platform::Ui,
    events: std::sync::mpsc::Receiver<platform::FromUi>,
) -> u8 {
    use std::io::Write as _;

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

    loop {
        match listener.accept() {
            Ok(Some(stream)) => {
                match display
                    .handle()
                    .insert_client(stream, Arc::new(ClientState::default()))
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
        state.pump_ui_events();
        if state.stopped {
            println!("wc-compositor: the platform stopped; winding up");
            code = 0;
            break;
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
