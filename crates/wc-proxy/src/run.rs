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

use wc_proxy::pool::{Arena, CopyScope, PoolTransport, SharedFile};
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
    scope: CopyScope,
    headroom: u64,
    trace: bool,
    accept_timeout: Duration,
    idle_timeout: Duration,
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

  --copy pool|buffer|damage   how much to copy per commit (default damage)
  --trace                log every message in both directions\n  --pool-headroom N      over-allocate each region N-fold so resize fits
                         (default 1)
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
        scope: CopyScope::Damage,
        headroom: 1,
        trace: false,
        accept_timeout: Duration::from_secs(20),
        idle_timeout: Duration::from_secs(20),
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
            "--arena-size" => {
                a.arena_size = next()?.parse().map_err(|e| format!("--arena-size: {e}"))?
            }
            "--copy" => {
                let v = next()?;
                a.scope = CopyScope::parse(&v).ok_or_else(|| format!("--copy {v}?"))?;
            }
            "--trace" => a.trace = true,
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

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let spec = Spec::builtin()?;
    println!("[proxy] {} interfaces from the vendored protocols", spec.len());

    // CHANGED IN STAGE 9: a **factory**, called once per client, rather than
    // one transport for the process. See the accept loop below for why there is
    // more than one client now.
    //
    // `arena` refuses the second call, and refusing loudly is the point: one
    // region, one allocator, and no protocol for handing out a second arena id
    // (docs/design.md §12.13 item 6). A silent share would give two clients
    // overlapping offsets into the same guest pages, which presents as one
    // window showing another's pixels.
    let mut arena_taken = false;
    let mut make_transport =
        || -> Result<Box<dyn PoolTransport + Send>, Box<dyn std::error::Error>> {
        Ok(match args.transport.as_str() {
        "shared-file" => {
            if args.peer_prefix.is_empty() {
                return Err(
                    "--peer-prefix is required for --transport shared-file: the proxy \
                     cannot guess how the compositor spells the pool directory, and a \
                     guess would be wrong on the deployment that matters"
                        .into(),
                );
            }
            let t = SharedFile::new(&args.pool_dir, &args.peer_prefix)?;
            println!(
                "[proxy] transport shared-file: {} here, {:?} there",
                args.pool_dir.display(),
                args.peer_prefix
            );
            Box::new(t)
        }
        // STAGE 7. The region is the guest's RAM, and the compositor is the side
        // that *provided* it — so there is no path to tell it about and nothing
        // for it to open. What it was started with is
        // `--arena-guest-ram <id>:<size>`, and what it printed is the run id
        // this plan was made from.
        "arena" if arena_taken => {
            return Err("the arena is already serving a client, and there is one arena \
                        (docs/design.md §12.13 item 6). Use --transport shared-file for a \
                        client that opens more than one connection."
                .into());
        }
        "arena" if !args.arena_plan.as_os_str().is_empty() => {
            arena_taken = true;
            let t = Arena::from_plan(args.arena_id, &args.arena_plan)?;
            println!(
                "[proxy] transport arena {}: {} bytes of {} \
                 (nothing is opened per pool, and the compositor opened nothing at all)",
                t.id(),
                t.size(),
                t.origin(),
            );
            Box::new(t)
        }
        "arena" => {
            arena_taken = true;
            let t = Arena::open_or_create(args.arena_id, &args.arena_path, args.arena_size)?;
            println!(
                "[proxy] transport arena {}: {} bytes at {} \
                 (the compositor must be started with --arena {}:<its path>:{})",
                t.id(),
                t.size(),
                args.arena_path.display(),
                args.arena_id,
                t.size(),
            );
            Box::new(t)
        }
        other => return Err(format!("--transport {other}? try shared-file or arena").into()),
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

        // A transport per session. The names `SharedFile` gives pool files
        // carry 128 bits of randomness, so two of them over one directory
        // cannot collide; an `Arena` is a single region with one allocator and
        // **cannot** be shared, which is docs/design.md §12.13 item 6 arriving
        // as a real limitation rather than a note.
        let transport = match make_transport() {
            Ok(t) => t,
            Err(e) => {
                println!("[proxy] client {n}: no transport for it: {e}");
                continue;
            }
        };

        let (spec, scope, headroom, trace, idle) =
            (spec.clone(), args.scope, args.headroom, args.trace, args.idle_timeout);
        threads.push(std::thread::spawn(move || {
            // The `+ Send` is only needed to cross this boundary; `Session`
            // itself has no opinion, and widening its signature would push the
            // bound into every test that builds one.
            let transport: Box<dyn PoolTransport> = transport;
            let mut session = Session::new(spec, transport, scope, headroom);
            session.set_trace(trace);
            let mut log = |s: &str| println!("[proxy] client {n}: {s}");
            match proxy::pump(&mut session, client, upstream, idle, &mut log) {
                Ok(why) => println!("[proxy] client {n} stopped: {why}"),
                Err(e) => println!("[proxy] client {n} failed: {e}"),
            }
            println!("[proxy] client {n} transport {}", session.transport_name());
            println!("[proxy] client {n} {}", session.stats.summary());
        }));
    }

    for t in threads {
        let _ = t.join();
    }
    println!("[proxy] stopped: {served} client(s) served");
    Ok(())
}
