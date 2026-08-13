//! The program: argument parsing and wiring. See the crate documentation in `lib.rs` for what this is and
//! why it exists.
//!
//! ```text
//!   weston-terminal          wc-proxy                wc-compositor.exe
//!   (stock libwayland)                               (Windows)
//!        │                       │                         │
//!        │  wl_shm.create_pool   │                         │
//!        │  + SCM_RIGHTS fd  ───▶│                         │
//!        │                       │  mmap the client's fd   │
//!        │                       │  publish a region       │
//!        │                       │  create_pool + name ───▶│ opens the name
//!        │  commit ─────────────▶│  copy damage ──────────▶│ blit into an HWND
//! ```

use std::path::PathBuf;
use std::time::Duration;

use wc_proxy::pool::{Arena, CopyScope, Policy, PoolTransport, SharedFile};
use wc_proxy::proxy::{self, Listener};
use wc_proxy::session::Session;
use wc_proxy::spec::Spec;

struct Args {
    /// The compositor's `AF_UNIX` node. Under Wine this is a real host socket
    /// inside the prefix, which is what lets a Linux process reach a `.exe`
    /// (docs/design.md §4.2); on the guest it is the far end of a relay.
    upstream: PathBuf,
    runtime_dir: PathBuf,
    socket: String,
    transport: String,
    pool_dir: PathBuf,
    peer_prefix: String,
    arena_id: u32,
    arena_size: u64,
    arena_path: PathBuf,
    /// STAGE 7. The plan `wc-arena-host` wrote: an object to open and the runs
    /// of it that are the guest's arena. Set, `--transport arena` maps guest
    /// RAM; unset, it opens `--arena-path` as a file.
    arena_plan: PathBuf,
    /// STAGE 10. How many bytes at the **end** of the arena belong to the
    /// compositor rather than to this process's pool allocator — the window it
    /// cuts the objects it creates for clients out of. Zero means it has none.
    ///
    /// Only for `--arena-path`, the same-machine file case. With `--arena-plan`
    /// the number comes from the plan, because it was decided in the guest and
    /// the plan is already the description of the region that crossed from
    /// there: a number typed into two command lines is a number that can differ
    /// between them, and two allocators over the same bytes is the one failure
    /// this flag exists to prevent.
    arena_outbound: u64,
    /// STAGE 16. Which run of free arena bytes a pool takes. The default is the
    /// one this stage measured; `first-fit` is stage 12's, kept so that a live
    /// run can be compared against a synthetic one without a rebuild.
    arena_policy: Policy,
    scope: CopyScope,
    headroom: u64,
    trace: bool,
    accept_timeout: Duration,
    idle_timeout: Duration,
    /// STAGE 11. Resolve `xdg_toplevel.set_app_id` to icon pixels here, where
    /// the icon theme is, and publish them to the compositor on a second
    /// connection (`crate::iconlink`).
    icons: bool,
    /// Which icon theme to try before `hicolor`.
    icon_theme: String,
    /// STAGE 13. Extra roots to search, ahead of the XDG ones.
    ///
    /// Repeatable, because a machine can have more than one place worth
    /// searching and because the alternative -- one colon-joined string --
    /// re-invents `XDG_DATA_DIRS` badly. It exists for callers that have no
    /// session environment to inherit: a `systemd-run` transient unit, which
    /// is how this proxy runs on the VM host, gets systemd's environment and
    /// not a login shell's.
    icon_dirs: Vec<std::path::PathBuf>,
}

fn usage() -> &'static str {
    "\
usage: wc-proxy --upstream <path> [options]

  --upstream PATH        the compositor's AF_UNIX socket (required)
  --runtime-dir DIR      where to put our own socket (default $XDG_RUNTIME_DIR)
  --socket NAME          our socket's name (default wl-proxy)

  --transport KIND       shared-file | arena          (default shared-file)
  --pool-dir DIR         shared-file: where pool files go
  --peer-prefix S        shared-file: how the peer spells --pool-dir,
                         e.g. 'Z:\\tmp\\wc-pools\\'
  --arena-path PATH      arena: the region's file on this side
  --arena-plan PATH      arena: instead of a file, the guest-RAM plan written by
                         wc-arena-host -- the object to open and the runs of it
                         that are the guest's own pinned pages
  --arena-id N           arena: the number both sides call it (default 0)
  --arena-size BYTES     arena: how big (default 64 MiB; with --arena-plan the
                         guest's allocation decides and this is ignored)
  --arena-outbound N     arena: the last N bytes belong to the compositor, which
                         cuts the objects it creates for clients -- a keymap
                         first -- out of them. Must match the compositor's
                         --arena-outbound. With --arena-plan the plan carries it
                         and this is refused rather than believed.
  --arena-policy KIND    arena: first-fit | best-fit    (default best-fit)
                         which run of free bytes a pool takes. best-fit is
                         stage 16's, measured in docs/design.md 19.4; first-fit
                         is stage 12's, kept so a live run can be compared
                         against the synthetic one without a rebuild.

  --copy pool|buffer|damage   how much to copy per commit (default damage)
  --trace                log every message in both directions\n  --pool-headroom N      over-allocate each region N-fold so resize fits
                         (default 1)

  --icon-theme NAME      the icon theme to search before hicolor
                         (default $WC_ICON_THEME, else hicolor)
  --no-icons             do not resolve app ids to icons at all: no second
                         connection to the compositor, no directory walk
  --icon-data-dir DIR    an extra XDG data directory to search first; may be
                         given more than once. The client's own package prefix
                         is always searched last, whether or not this is given
  --accept-timeout SEC   how long to wait for a client (default 20)
  --timeout SEC          exit after this long with no traffic (default 20)
"
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        upstream: PathBuf::new(),
        runtime_dir: std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir),
        socket: "wl-proxy".into(),
        transport: "shared-file".into(),
        pool_dir: PathBuf::new(),
        peer_prefix: String::new(),
        arena_id: 0,
        arena_size: 64 << 20,
        arena_path: PathBuf::new(),
        arena_plan: PathBuf::new(),
        arena_outbound: 0,
        arena_policy: Policy::default(),
        scope: CopyScope::Damage,
        headroom: 1,
        trace: false,
        accept_timeout: Duration::from_secs(20),
        idle_timeout: Duration::from_secs(20),
        icons: true,
        icon_theme: std::env::var("WC_ICON_THEME").unwrap_or_else(|_| "hicolor".into()),
        icon_dirs: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--upstream" => a.upstream = next()?.into(),
            "--runtime-dir" => a.runtime_dir = next()?.into(),
            "--socket" => a.socket = next()?,
            "--transport" => a.transport = next()?,
            "--pool-dir" => a.pool_dir = next()?.into(),
            "--peer-prefix" => a.peer_prefix = next()?,
            "--arena-path" => a.arena_path = next()?.into(),
            "--arena-plan" => a.arena_plan = next()?.into(),
            "--arena-id" => a.arena_id = next()?.parse().map_err(|e| format!("--arena-id: {e}"))?,
            "--arena-outbound" => {
                a.arena_outbound =
                    next()?.parse().map_err(|e| format!("--arena-outbound: {e}"))?
            }
            "--arena-policy" => {
                let v = next()?;
                a.arena_policy = Policy::parse(&v)
                    .ok_or_else(|| format!("--arena-policy {v}? try first-fit or best-fit"))?;
            }
            "--arena-size" => {
                a.arena_size = next()?.parse().map_err(|e| format!("--arena-size: {e}"))?
            }
            "--copy" => {
                let v = next()?;
                a.scope = CopyScope::parse(&v).ok_or_else(|| format!("--copy {v}?"))?;
            }
            "--trace" => a.trace = true,
            "--no-icons" => a.icons = false,
            "--icon-theme" => a.icon_theme = next()?,
            "--icon-data-dir" => a.icon_dirs.push(next()?.into()),
            "--pool-headroom" => {
                a.headroom = next()?.parse().map_err(|e| format!("--pool-headroom: {e}"))?
            }
            "--accept-timeout" => {
                a.accept_timeout = Duration::from_secs(
                    next()?.parse().map_err(|e| format!("--accept-timeout: {e}"))?,
                )
            }
            "--timeout" => {
                a.idle_timeout =
                    Duration::from_secs(next()?.parse().map_err(|e| format!("--timeout: {e}"))?)
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.upstream.as_os_str().is_empty() {
        return Err("--upstream is required".into());
    }
    if a.pool_dir.as_os_str().is_empty() {
        a.pool_dir = a.runtime_dir.join("wc-pools");
    }
    if a.arena_path.as_os_str().is_empty() {
        a.arena_path = a.runtime_dir.join("wc-arena");
    }
    Ok(a)
}

pub fn main() -> std::process::ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("wc-proxy: {msg}");
            }
            eprint!("{}", usage());
            return std::process::ExitCode::from(2);
        }
    };

    match run(args) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wc-proxy: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Say, in the log, that this side knows about the compositor's window.
///
/// STAGE 10. It is one line and it is worth one line: the number is the only
/// thing standing between two allocators over one region, and a run where the
/// compositor reserved a window and this side did not hear about it would work
/// perfectly until the arena filled up.
fn announce_outbound(t: &Arena) {
    match t.outbound() {
        Some((off, len)) => println!(
            "[proxy] arena {}: the last {len} bytes (offset {off}) are the compositor's to \
             hand to clients; this allocator will not touch them",
            t.id()
        ),
        None => println!(
            "[proxy] arena {}: no outbound window; the compositor creates nothing here",
            t.id()
        ),
    }
    // STAGE 16. Which allocator ran is exactly the §8.4 question: two policies
    // that differ only in which free run they take produce byte-identical
    // pixels, so a run that did not say which one it used could not be compared
    // with one that used the other.
    println!("[proxy] arena {}: allocation policy {}", t.id(), t.policy().as_str());
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let spec = Spec::builtin()?;
    println!("[proxy] {} interfaces from the vendored protocols", spec.len());

    // CHANGED IN STAGE 9: a **factory**, called once per client, rather than
    // one transport for the process. See the accept loop below for why there is
    // more than one client now.
    //
    // CHANGED AGAIN IN STAGE 12: the arena is built **once, here**, before any
    // client exists, and each session gets a handle on the same region and the
    // same allocator (`Arena::share`). Stage 9 refused the second call by name —
    // one region, one allocator, no protocol for a second arena id
    // (docs/design.md §12.13 item 6) — and that refusal was the reason Firefox,
    // which opens four connections, had never touched an arena (§15.14). The
    // premise was right and the conclusion was not: what must not be duplicated
    // is the *allocator*, and one allocator serves four connections for exactly
    // the reason it serves four pools of one connection. See §16.2.
    //
    // Building it before the accept loop is a second, smaller improvement: a
    // plan that cannot be read or a window that cannot be reserved is now a
    // startup failure with the message on stderr, rather than the first client
    // being told "no transport for it" twenty seconds later.
    let shared_arena = match args.transport.as_str() {
        "arena" if !args.arena_plan.as_os_str().is_empty() => {
            if args.arena_outbound != 0 {
                return Err(
                    "--arena-outbound and --arena-plan are mutually exclusive: the window \
                     was decided in the guest and travels in the plan, so a second spelling \
                     of it on this command line could only ever disagree"
                        .into(),
                );
            }
            // STAGE 7. The region is the guest's RAM, and the compositor is the
            // side that *provided* it — so there is no path to tell it about and
            // nothing for it to open. What it was started with is
            // `--arena-guest-ram <id>:<size>`, and what it printed is the run id
            // this plan was made from.
            let mut t = Arena::from_plan(args.arena_id, &args.arena_plan)?;
            t.set_policy(args.arena_policy)?;
            println!(
                "[proxy] transport arena {}: {} bytes of {} \
                 (nothing is opened per pool, and the compositor opened nothing at all)",
                t.id(),
                t.size(),
                t.origin(),
            );
            announce_outbound(&t);
            Some(t)
        }
        "arena" => {
            let mut t = Arena::open_or_create(args.arena_id, &args.arena_path, args.arena_size)?;
            t.set_policy(args.arena_policy)?;
            if args.arena_outbound != 0 {
                t.reserve_outbound(args.arena_outbound)?;
            }
            println!(
                "[proxy] transport arena {}: {} bytes at {} \
                 (the compositor must be started with --arena {}:<its path>:{})",
                t.id(),
                t.size(),
                args.arena_path.display(),
                args.arena_id,
                t.size(),
            );
            announce_outbound(&t);
            Some(t)
        }
        "shared-file" => {
            if args.peer_prefix.is_empty() {
                return Err(
                    "--peer-prefix is required for --transport shared-file: the proxy \
                     cannot guess how the compositor spells the pool directory, and a \
                     guess would be wrong on the deployment that matters"
                        .into(),
                );
            }
            println!(
                "[proxy] transport shared-file: {} here, {:?} there",
                args.pool_dir.display(),
                args.peer_prefix
            );
            None
        }
        other => return Err(format!("--transport {other}? try shared-file or arena").into()),
    };

    let make_transport = |n: usize| -> Result<Box<dyn PoolTransport + Send>, Box<dyn std::error::Error>> {
        Ok(match &shared_arena {
            // A handle on the region, not a copy of it. `sharers()` is the count
            // §8.4's rule wants: a stage that made a second client possible with
            // no way to see that it happened would not really have made it
            // possible.
            Some(a) => {
                let t = a.share();
                println!(
                    "[proxy] client {n}: joins arena {} ({} connection(s) on it now, {} of {} bytes out)",
                    t.id(),
                    t.sharers() - 1,
                    t.published(),
                    t.size(),
                );
                Box::new(t)
            }
            // A transport per session, and here that really is a separate one:
            // the names `SharedFile` gives pool files carry 128 bits of
            // randomness, so two of them over one directory cannot collide.
            None => Box::new(SharedFile::new(&args.pool_dir, &args.peer_prefix)?),
        })
    };

    let listener = Listener::bind(&args.runtime_dir, &args.socket)?;
    println!(
        "[proxy] listening on {} (copy scope: {}, headroom: {}x)",
        listener.path().display(),
        args.scope.as_str(),
        args.headroom
    );

    // CHANGED IN STAGE 9: **the proxy serves more than one connection.**
    //
    // Until this stage it accepted exactly one client and exited when that
    // client closed, which was true of every client it had been given —
    // `weston-terminal` and `weston-presentation-shm` each open one connection
    // and keep it. Firefox does not, and the failure is total rather than
    // partial: its `WaylandProxy` opens a **throwaway connection first** to
    // check the compositor is alive, closes it, and only then connects for
    // real. Against a one-shot proxy the throwaway is the only connection that
    // is ever served; the real one finds a closed socket, and Firefox reports
    // `Wayland protocol error: Compositor () crashed` and dumps core. Measured,
    // and it is docs/design.md §13.8.
    //
    // Each client gets its **own upstream connection**, which is what a
    // compositor expects: object ids are per-connection, and multiplexing two
    // clients onto one would need an id-rewriting layer that this deliberately
    // is not (§9.3).
    let mut served: usize = 0;
    let deadline = std::time::Instant::now() + args.accept_timeout;
    let mut threads: Vec<std::thread::JoinHandle<()>> = Vec::new();
    loop {
        // The first client is waited for; later ones are only accepted while
        // something is still running, so a proxy whose client has finished does
        // not sit forever.
        let wait = if served == 0 {
            deadline.saturating_duration_since(std::time::Instant::now())
        } else {
            args.idle_timeout
        };
        let client = match listener.accept_one(wait) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(e.into()),
        };
        served += 1;
        let n = served;

        // Upstream is connected per client and **after** the accept, unlike
        // stage 4's order. The old comment said connecting first stops a client
        // being accepted into a proxy with nowhere to forward to; that is still
        // true and is now handled by failing the connection immediately, which
        // is a closed socket at the same moment either way.
        let upstream = match std::os::unix::net::UnixStream::connect(&args.upstream) {
            Ok(u) => u,
            Err(e) => {
                println!(
                    "[proxy] client {n}: cannot reach the compositor at {}: {e}",
                    args.upstream.display()
                );
                continue;
            }
        };
        println!("[proxy] client {n} connected, upstream {} connected", args.upstream.display());

        // A transport per session: a fresh `SharedFile`, or a handle on the one
        // arena. docs/design.md §16.2.
        let transport = match make_transport(n) {
            Ok(t) => t,
            Err(e) => {
                println!("[proxy] client {n}: no transport for it: {e}");
                continue;
            }
        };

        let (spec, scope, headroom, trace, idle) =
            (spec.clone(), args.scope, args.headroom, args.trace, args.idle_timeout);
        // STAGE 11. The icon feed, one per served connection.
        //
        // **Per connection rather than per process**, which is a decision and
        // not an accident of where the code sits. A shared feed would be one
        // more thing behind a mutex for no gain: an icon is published once per
        // distinct app id, `IconLink` already remembers which ones it has done,
        // and Firefox's throwaway connection (§15.2) sets no app id at all, so
        // the second connection costs a socket and nothing else. Keeping it
        // inside the thread also means a resolver that fails takes its own
        // client's icons with it and nobody else's.
        let (want_icons, icon_theme, upstream_path) =
            (args.icons, args.icon_theme.clone(), args.upstream.clone());
        // STAGE 13. Where *this* client's application is installed, asked of
        // the kernel before the socket is moved into the thread. Read here
        // rather than inside the thread only because the answer is about the
        // connection and this is where the connection still is.
        let client_dirs = wc_proxy::proxy::Listener::peer_pid(&client)
            .map(wc_proxy::icons::dirs_for_pid)
            .unwrap_or_default();
        let extra_dirs = args.icon_dirs.clone();
        threads.push(std::thread::spawn(move || {
            // The `+ Send` is only needed to cross this boundary; `Session`
            // itself has no opinion, and widening its signature would push the
            // bound into every test that builds one.
            let transport: Box<dyn PoolTransport> = transport;
            let mut icons = if want_icons {
                // STAGE 13. Three sources, in this order and for this reason:
                //
                // 1. `--icon-data-dir`, because a proxy started by a systemd
                //    transient unit has no session environment at all and a
                //    harness must be able to say what to search rather than
                //    hope. `scripts/wc-icon.sh` was already doing this by
                //    exporting `XDG_DATA_DIRS` around the process, which works
                //    for one client and not for a run with two.
                // 2. The XDG directories, which are the user's and the
                //    system's themes and must win: an icon theme the user
                //    installed is a deliberate override of what a package
                //    shipped.
                // 3. The client's own package prefix, last, because it is the
                //    guaranteed answer rather than the preferred one — the
                //    application's own pixels, found where the application is.
                let mut data_dirs = extra_dirs;
                data_dirs.extend(wc_proxy::icons::default_data_dirs());
                data_dirs.extend(client_dirs);
                let search = wc_proxy::icons::Search {
                    data_dirs,
                    theme: icon_theme,
                };
                // A failure here is logged and shrugged off. The whole feature
                // is cosmetic: a compositor too old to have the global, or a
                // socket that will not take a second connection, must cost a
                // picture rather than a window.
                match wc_proxy::iconlink::IconLink::connect(&upstream_path, &spec, search) {
                    Ok(link) => Some(link),
                    Err(e) => {
                        println!("[proxy] client {n}: no icon feed ({e}); icons will be generic");
                        None
                    }
                }
            } else {
                None
            };
            let mut session = Session::new(spec, transport, scope, headroom);
            session.set_trace(trace);
            let mut log = |s: &str| println!("[proxy] client {n}: {s}");
            match proxy::pump(&mut session, client, upstream, icons.as_mut(), idle, &mut log) {
                Ok(why) => println!("[proxy] client {n} stopped: {why}"),
                Err(e) => println!("[proxy] client {n} failed: {e}"),
            }
            println!("[proxy] client {n} transport {}", session.transport_name());
            println!("[proxy] client {n} {}", session.stats.summary());
            if let Some(link) = &icons {
                println!("[proxy] client {n} {}", link.summary());
            }
        }));
    }

    for t in threads {
        let _ = t.join();
    }
    println!("[proxy] stopped: {served} client(s) served");
    Ok(())
}
