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

    let transport: Box<dyn PoolTransport> = match args.transport.as_str() {
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
        "arena" if !args.arena_plan.as_os_str().is_empty() => {
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
    };

    let listener = Listener::bind(&args.runtime_dir, &args.socket)?;
    println!(
        "[proxy] listening on {} (copy scope: {}, headroom: {}x)",
        listener.path().display(),
        args.scope.as_str(),
        args.headroom
    );

    // Connect upstream *before* accepting, so that a client is never accepted
    // into a proxy with nowhere to forward to. A client that connects and then
    // sees the socket close has no way to distinguish that from a compositor
    // crash and generally exits with a confusing message.
    let upstream = std::os::unix::net::UnixStream::connect(&args.upstream).map_err(|e| {
        format!("cannot reach the compositor at {}: {e}", args.upstream.display())
    })?;
    println!("[proxy] upstream {} connected", args.upstream.display());

    let client = listener.accept_one(args.accept_timeout)?;
    println!("[proxy] client connected");

    let mut session = Session::new(spec, transport, args.scope, args.headroom);
    session.set_trace(args.trace);
    let mut log = |s: &str| println!("[proxy] {s}");
    let why = proxy::pump(&mut session, client, upstream, args.idle_timeout, &mut log)?;

    println!("[proxy] stopped: {why}");
    println!("[proxy] transport {}", session.transport_name());
    println!("[proxy] {}", session.stats.summary());
    Ok(())
}
