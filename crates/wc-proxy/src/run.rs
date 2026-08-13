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

use wc_proxy::pool::{
    Arena, CopyScope, FileGrowth, PlanGrowth, Policy, PoolTransport, RegionSource, SharedFile,
};
use wc_proxy::proxy::{self, Listener};
use wc_proxy::session::Session;
use wc_proxy::spec::Spec;

struct Args {
    /// The compositor's `AF_UNIX` node. Under Wine this is a real host socket
    /// inside the prefix, which is what lets a Linux process reach a `.exe`
    /// (docs/waycompose-design.md §4.2); on the guest it is the far end of a relay.
    upstream: PathBuf,
    /// §36. The control region of the shared_vulkan session whose rings carry
    /// the Wayland stream. When set, `--upstream` is not used at all and no
    /// socket, no TCP and no ssh is in the data path.
    ///
    /// A path rather than a cold-path endpoint, deliberately and for now: this
    /// is the region as `sv-hostd` or `sv-gpu-server` published it, and opening
    /// it by name is what `Attach` would have told us to do anyway. §36.8.
    upstream_ring: Option<PathBuf>,
    /// §36, cross-VM. The `sv-hostd` socket to attach through, when the
    /// compositor published its control region out of the guest's own RAM.
    upstream_ring_attach: Option<PathBuf>,
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
    /// DYNAMIC ARENA. May the arena obtain more memory when a pool does not
    /// fit, and give it back when a region empties?
    ///
    /// **Off by default, and that is a decision rather than caution.** An arena
    /// that grew whenever it was tight would make [`Policy`] untestable — every
    /// measurement of how well runs are chosen would be contaminated by memory
    /// that was added instead of used — and `docs/waycompose-design.md` §20.4's matrix
    /// compares policies at a fixed size. Growth is what happens after the
    /// allocator has already failed, and a run has to say it wants it.
    arena_grow: bool,
    /// The base of the geometric growth series. Zero keeps
    /// [`wc_proxy::pool::GROW_STEP`] — 16 MiB, argued from the pool sizes
    /// clients actually ask for.
    ///
    /// It used to default to `--arena-size`, which meant a session started at
    /// 768 MiB grew in 768 MiB jumps. That was two unrelated quantities sharing
    /// a default rather than a number anybody chose.
    arena_grow_step: u64,
    /// How many frames a region must be empty before it is returned. Not a
    /// timer: what makes a region cold is that nothing drew in it, and a clock
    /// cannot tell a cold region from a paused video. Zero returns it as soon
    /// as it is empty, which thrashes; the default is a guard against that.
    arena_grow_idle: u64,
    scope: CopyScope,
    headroom: u64,
    trace: bool,
    /// §27. After every copy, compare the region against the client's own
    /// bytes and count the rows that differ. An instrument rather than a mode:
    /// it reads the whole extent every commit. See `Session::verify`.
    verify: bool,
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
    /// THE CLIPBOARD. Carry the clipboard across on a third connection
    /// (`crate::clip`). Off, a client that copies or pastes gets nothing —
    /// which is what the compositor did before this stage, minus the part
    /// where the connection died.
    clipboard: bool,
}

fn usage() -> &'static str {
    "\
usage: wc-proxy --upstream <path> [options]

  --upstream PATH        the compositor's AF_UNIX socket (required unless
                         --upstream-ring is given)
  --upstream-ring PATH   §36: the shared_vulkan session's control region. Each
                         client claims a ring of RingKind::Wayland in it and the
                         bytes cross as shared memory -- no socket, no TCP and
                         no ssh in the data path. --upstream is then unused.
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
  --arena-grow           arena: when a pool does not fit, obtain another region
                         rather than refusing, and give a region back once it
                         has been empty for --arena-grow-idle frames. Off by
                         default so that --arena-policy stays measurable: an
                         arena that grew when tight would hide how well it
                         places what it already has.
  --arena-grow-step N    arena: base of the geometric growth series, doubling
                         to a 64 MiB cap (default 16 MiB, from the pool sizes
                         clients ask for). Growing costs a locate -- ~0.4 s on
                         the deployment -- so the number of growths is what
                         wants bounding, and the cap bounds the overshoot in
                         exchange. A pool larger than the step gets a region big
                         enough for it; a pool never straddles two.
  --arena-grow-idle N    arena: frames a region must be empty before it is
                         returned (default 120). Frames, not seconds — a clock
                         cannot tell a cold region from a paused video.
  --arena-policy KIND    arena: first-fit | best-fit    (default best-fit)
                         which run of free bytes a pool takes. best-fit is
                         stage 16's, measured in docs/waycompose-design.md 20.4; first-fit
                         is stage 12's, kept so a live run can be compared
                         against the synthetic one without a rebuild.

  --copy pool|buffer|damage   how much to copy per commit (default damage)
  --trace                log every message in both directions
  --verify-damage        after every copy, compare the region against the
                         client's own bytes and report the rows that differ.
                         Reads the whole buffer per commit, so it is for finding
                         a damage bug and never for timing one\n  --pool-headroom N      over-allocate each region N-fold so resize fits
                         (default 1)

  --icon-theme NAME      the icon theme to search before hicolor
                         (default $WC_ICON_THEME, else hicolor)
  --no-clipboard         do not carry the clipboard: no third connection to the
                         compositor, and copy and paste do nothing
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
        upstream_ring: None,
        upstream_ring_attach: None,
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
        arena_grow: false,
        arena_grow_step: 0,
        arena_grow_idle: 120,
        scope: CopyScope::Damage,
        headroom: 1,
        trace: false,
        verify: false,
        accept_timeout: Duration::from_secs(20),
        idle_timeout: Duration::from_secs(20),
        icons: true,
        icon_theme: std::env::var("WC_ICON_THEME").unwrap_or_else(|_| "hicolor".into()),
        icon_dirs: Vec::new(),
        clipboard: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--upstream" => a.upstream = next()?.into(),
            "--upstream-ring" => a.upstream_ring = Some(next()?.into()),
            "--upstream-ring-attach" => a.upstream_ring_attach = Some(next()?.into()),
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
            "--arena-grow" => a.arena_grow = true,
            "--arena-grow-step" => {
                a.arena_grow_step =
                    next()?.parse().map_err(|e| format!("--arena-grow-step: {e}"))?
            }
            "--arena-grow-idle" => {
                a.arena_grow_idle =
                    next()?.parse().map_err(|e| format!("--arena-grow-idle: {e}"))?
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
            "--verify-damage" => a.verify = true,
            "--no-icons" => a.icons = false,
            "--no-clipboard" => a.clipboard = false,
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
    if a.upstream.as_os_str().is_empty()
        && a.upstream_ring.is_none()
        && a.upstream_ring_attach.is_none()
    {
        return Err("--upstream, --upstream-ring or --upstream-ring-attach is required".into());
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
    // (docs/waycompose-design.md §12.13 item 6) — and that refusal was the reason Firefox,
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
                "[proxy] transport arena {}: {} bytes of {} in {} region(s) \
                 (nothing is opened per pool, and the compositor opened nothing at all)",
                t.id(),
                t.size(),
                t.origin(),
                t.regions(),
            );
            // DYNAMIC ARENA. Growth over guest RAM is the original handshake run
            // again — the guest commits and stamps another region and the
            // locator appends it to this same plan — so the source is the plan
            // itself, and it already knows which regions have been taken up.
            if args.arena_grow {
                let ids = t.region_ids();
                let src = PlanGrowth::new(&args.arena_plan, &ids);
                println!("[proxy] arena {} may grow: {}", t.id(), src.describe());
                t.set_source(Box::new(src))?;
                t.set_grow_step(args.arena_grow_step)?;
                t.set_reclaim_idle(args.arena_grow_idle)?;
            }
            announce_outbound(&t);
            Some(t)
        }
        "arena" => {
            // THE FILE ARENA IS NOT A TARGET. Guest RAM is (design.md §1a). This
            // branch exists so a Wine run on one Linux machine — which has no
            // QEMU and therefore no guest RAM to locate — can exercise the arena
            // path at all. It is **kept** rather than deleted, because deleting
            // it would delete the only way to run this code without a
            // hypervisor. It is **refused** rather than allowed, because a run
            // that quietly lands here is measuring the wrong substrate: §10.3
            // measured that substrate incoherent for live mappings — 65280 of
            // 65536 bytes wrong, in both directions — and it reached a user's
            // screen once as a torn window that had passed every other check.
            //
            // `SV_ALLOW_FILE_ARENA=1` is how a fixture run says it means it.
            // Any change to arena behaviour must be implemented fully and
            // correctly on guest RAM; making the fixture pass is never enough.
            if std::env::var_os("SV_ALLOW_FILE_ARENA").is_none() {
                return Err(format!(
                    "the FILE arena is not a target. `--transport arena --arena-path {}` \
                     opens a file, which is a Wine-only fixture for a machine with no \
                     hypervisor. Guest RAM is the only real substrate: pass `--arena-plan` \
                     here and `--arena-guest-ram` to the compositor. Set \
                     SV_ALLOW_FILE_ARENA=1 for a fixture run — never to make a guest run \
                     pass.",
                    args.arena_path.display()
                )
                .into());
            }
            eprintln!(
                "[proxy] WARNING: FILE arena in use (SV_ALLOW_FILE_ARENA is set). This is \
                 a fixture, not a target; guest RAM is the only real substrate."
            );
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
            // The fixture's growth, and it says so. `ProvidedRegion::file`'s
            // note applies: this is the Wine and same-machine path, and a
            // deployment reaching it would be a same-machine path wearing the
            // design's clothes (design.md §1a).
            if args.arena_grow {
                let dir = args
                    .arena_path
                    .parent()
                    .unwrap_or(std::path::Path::new("."))
                    .to_path_buf();
                let src = FileGrowth::new(&dir, 0);
                println!("[proxy] arena {} may grow: {}", t.id(), src.describe());
                t.set_source(Box::new(src))?;
                t.set_grow_step(args.arena_grow_step)?;
                t.set_reclaim_idle(args.arena_grow_idle)?;
            }
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
    // and it is docs/waycompose-design.md §13.8.
    //
    // Each client gets its **own upstream connection**, which is what a
    // compositor expects: object ids are per-connection, and multiplexing two
    // clients onto one would need an id-rewriting layer that this deliberately
    // is not (§9.3).
    // §36. The session's control region, when the Wayland stream is riding
    // shared_vulkan's rings. Attached once for the whole proxy and shared by
    // every client thread, because the *region* is one session — it is the
    // *rings* that are per connection, which is the distinction §36.3 is about.
    let ring: Option<std::sync::Arc<sv_transport::control::Control>> = match (
        &args.upstream_ring,
        &args.upstream_ring_attach,
    ) {
        // §36. The cross-VM form: the compositor published its control region
        // out of *guest RAM* and `sv-hostd` located it. This is the ICD's
        // `AttachRequest`/`Attach`/`Runs` exchange, and it is deliberately the
        // same three messages — the Wayland half and the Vulkan half attach to a
        // guest region by exactly one mechanism, which is the whole of §36.
        (_, Some(sock)) => {
            use sv_transport::cold::ColdMessage;
            let s = std::os::unix::net::UnixStream::connect(sock)?;
            sv_transport::cold::send(&s, &ColdMessage::AttachRequest)
                .map_err(|e| format!("{sock}: {e}", sock = sock.display()))?;
            let at = match sv_transport::cold::recv(&s)? {
                ColdMessage::Attach(a) => a,
                ColdMessage::Refuse { code, reason } => {
                    return Err(format!("the daemon refused {code}: {reason}").into());
                }
                other => return Err(format!("unexpected cold-path reply {other:?}").into()),
            };
            let runs = if at.run_count != 0 {
                match sv_transport::cold::recv(&s)? {
                    ColdMessage::Runs(r) => r,
                    other => return Err(format!("expected Runs, got {other:?}").into()),
                }
            } else {
                Vec::new()
            };
            // The object is QEMU's memory backend, named rather than passed —
            // §1a. A scattered extent has no single offset whose `mmap`
            // reproduces it, which is what the run list is for.
            let f = sv_transport::region::open_named(&at.object)?;
            let region = if runs.is_empty() {
                sv_transport::region::SharedRegion::map_fd(
                    std::os::fd::AsFd::as_fd(&f),
                    at.control_offset,
                    at.control_len as usize,
                )?
            } else {
                let stitch: Vec<sv_transport::region::StitchRun> = runs
                    .iter()
                    .filter(|r| r.extent_offset < at.control_len)
                    .map(|r| sv_transport::region::StitchRun {
                        file_offset: r.file_offset,
                        extent_offset: r.extent_offset,
                        len: r.len.min(at.control_len - r.extent_offset),
                    })
                    .collect();
                sv_transport::region::SharedRegion::stitch(std::os::fd::AsFd::as_fd(&f), &stitch)?
            };
            let ids = sv_transport::control::ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
            let control = sv_transport::control::Control::attach(std::sync::Arc::new(region), ids)?;
            // The acknowledgement the daemon waits for before it stops holding
            // the session open. Sent after the attach, so a failure to map is
            // reported here where the daemon can print it.
            sv_transport::cold::send(&s, &ColdMessage::Accept { generation: at.generation })?;
            println!(
                "[proxy] upstream is the guest's own RAM via {} ({} run(s), {} ring slots, \
                 generation {})",
                sock.display(),
                at.run_count,
                control.layout().geometry.ring_slot_count,
                control.generation()
            );
            Some(std::sync::Arc::new(control))
        }
        (None, None) => None,
        (Some(path), None) => {
            let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
            let len = f.metadata()?.len() as usize;
            let region = std::sync::Arc::new(sv_transport::region::SharedRegion::map_fd(
                std::os::fd::AsFd::as_fd(&f),
                0,
                len,
            )?);
            // The same `ProtocolIds` the ICD and the GPU server use. R13: two
            // builds that disagree must fail to connect rather than
            // desynchronise, and this is where that is checked for the Wayland
            // half exactly as it is for the Vulkan half.
            let ids = sv_transport::control::ProtocolIds::new(1, sv_proto::PROTOCOL_HASH);
            let control = sv_transport::control::Control::attach(region, ids)?;
            println!(
                "[proxy] upstream is the shared_vulkan session at {} \
                 ({} ring slots, generation {})",
                path.display(),
                control.layout().geometry.ring_slot_count,
                control.generation()
            );
            Some(std::sync::Arc::new(control))
        }
    };

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
        //
        // **Retried on `ECONNREFUSED`, for the reason `wc-wlcs` retries.** `bind`
        // creates the filesystem node and `listen` makes it accept; between the
        // two, `connect` is refused. Every harness that starts a compositor and
        // then a proxy waits for the *node*, which does not close that window,
        // and the symptom is one leg of a run failing with "cannot reach the
        // compositor" while an identical tree passes — a red that is not a
        // regression, which teaches everyone to re-run until green.
        //
        // Bounded and reported: the retry is printed, so a run that needed it
        // says so rather than hiding a slow compositor behind a pass. A refusal
        // that outlives the deadline is still a failure, because a compositor
        // that is not listening after five seconds is not a race.
        //
        // §36. On a ring there is nothing to refuse and nothing to race: a
        // connection is a CAS on the ring directory of a control region that is
        // already mapped. The `bind`/`listen` window this grace exists for is a
        // property of sockets, and a ring has no equivalent — which is one small
        // way the design is simpler than the thing it replaces.
        let upstream = match ring.as_ref() {
            Some(control) => match sv_transport::wlbytes::ByteChannel::claim(control, n as u64) {
                Ok(ch) => {
                    println!(
                        "[proxy] client {n} connected, upstream ring slot {} \
                         (no socket, no TCP, no ssh)",
                        ch.slot()
                    );
                    proxy::Upstream::Ring(Box::new(ch))
                }
                Err(e) => {
                    println!("[proxy] client {n}: no free Wayland ring: {e}");
                    continue;
                }
            },
            None => {
                match proxy::connect_upstream(&args.upstream, |refusals, ms| {
                    println!(
                        "[proxy] client {n}: the compositor refused {refusals} connect(s) over \
                         {ms:.0} ms before it was listening"
                    );
                }) {
                    Ok(u) => {
                        println!(
                            "[proxy] client {n} connected, upstream {} connected",
                            args.upstream.display()
                        );
                        proxy::Upstream::Socket(u)
                    }
                    Err(e) => {
                        println!(
                            "[proxy] client {n}: cannot reach the compositor at {}: {e}",
                            args.upstream.display()
                        );
                        continue;
                    }
                }
            }
        };

        // A transport per session: a fresh `SharedFile`, or a handle on the one
        // arena. docs/waycompose-design.md §16.2.
        let transport = match make_transport(n) {
            Ok(t) => t,
            Err(e) => {
                println!("[proxy] client {n}: no transport for it: {e}");
                continue;
            }
        };

        let (spec, scope, headroom, trace, idle) =
            (spec.clone(), args.scope, args.headroom, args.trace, args.idle_timeout);
        let verify = args.verify;
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
        let on_ring = ring.is_some();
        // STAGE 13. Where *this* client's application is installed, asked of
        // the kernel before the socket is moved into the thread. Read here
        // rather than inside the thread only because the answer is about the
        // connection and this is where the connection still is.
        let client_dirs = wc_proxy::proxy::Listener::peer_pid(&client)
            .map(wc_proxy::icons::dirs_for_pid)
            .unwrap_or_default();
        let extra_dirs = args.icon_dirs.clone();
        let want_clipboard = args.clipboard;
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
                //
                // §36: on a ring upstream this would be a *second* ring claim,
                // which is right and is not built. Said out loud rather than
                // skipped silently, because a generic icon looks like a bug in
                // the icon code.
                if on_ring {
                    println!(
                        "[proxy] client {n}: no icon feed on the ring upstream (§36.8); \
                         icons will be generic"
                    );
                    None
                } else {
                    match wc_proxy::iconlink::IconLink::connect(&upstream_path, &spec, search) {
                        Ok(link) => Some(link),
                        Err(e) => {
                            println!(
                                "[proxy] client {n}: no icon feed ({e}); icons will be generic"
                            );
                            None
                        }
                    }
                }
            } else {
                None
            };
            // THE CLIPBOARD. The byte path's byte path, one per served connection
            // and for the same reasons the icon feed is per connection: the
            // state is per client, a feed that fails takes only its own
            // client's clipboard with it, and a socket is cheap.
            //
            // **A separate connection from the icon feed on purpose.**
            // `--no-icons` exists, and an icon resolver that cannot reach the
            // theme must not be the reason copy and paste stop working.
            let mut clip = if want_clipboard && !on_ring {
                match wc_proxy::clip::ClipLink::connect(&upstream_path, &spec) {
                    Ok(link) => Some(link),
                    Err(e) => {
                        println!(
                            "[proxy] client {n}: no clipboard feed ({e}); copy and paste will \
                             do nothing rather than failing"
                        );
                        None
                    }
                }
            } else {
                if want_clipboard {
                    // §36.8, same as the icon feed: a third connection to the
                    // compositor is a third ring claim, and it is not built.
                    println!(
                        "[proxy] client {n}: no clipboard feed on the ring upstream (§36.8); \
                         copy and paste will do nothing rather than failing"
                    );
                }
                None
            };
            let mut session = Session::new(spec, transport, scope, headroom);
            session.set_trace(trace);
            session.set_verify(verify);
            let mut log = |s: &str| println!("[proxy] client {n}: {s}");
            match proxy::pump(
                &mut session,
                client,
                upstream,
                icons.as_mut(),
                clip.as_mut(),
                idle,
                &mut log,
            ) {
                Ok(why) => println!("[proxy] client {n} stopped: {why}"),
                Err(e) => println!("[proxy] client {n} failed: {e}"),
            }
            println!("[proxy] client {n} transport {}", session.transport_name());
            println!("[proxy] client {n} {}", session.stats.summary());
            if let Some(m) = session.transport_memory() {
                println!("[proxy] client {n} {m}");
            }
            if let Some(link) = &icons {
                println!("[proxy] client {n} {}", link.summary());
            }
            if let Some(link) = &clip {
                println!("[proxy] client {n} {}", link.summary());
            }
        }));
    }

    for t in threads {
        let _ = t.join();
    }
    // DYNAMIC ARENA. The memory figure, printed once for the process because
    // the arena is one region set shared by every connection and a per-client
    // number would be four readings of the same bytes.
    //
    // **Peak resident, not bytes copied.** Every other number this proxy prints
    // is throughput — MiB copied, per-copy microseconds — and a throughput
    // figure standing next to a memory question gets read as an answer to it.
    // A run that copied 1158 MiB held nothing like 1158 MiB; the two differ by
    // the number of frames. So the memory line says what it is, says the floor
    // release cannot go below, and says how many regions were up at the peak,
    // because "1.2 GiB in one region" and "1.2 GiB in nineteen" are different
    // facts about whether release can help at all.
    if let Some(a) = &shared_arena {
        let (peak, peak_regions) = a.peak();
        let (grown, grow_nanos, refused, released, released_bytes) = a.growth();
        let mib = |b: u64| b as f64 / (1024.0 * 1024.0);
        println!(
            "[proxy] arena {} memory: peak resident {:.1} MiB in {} region(s); \
             now {:.1} MiB in {} region(s); floor {:.1} MiB (the startup region, \
             which is never returned)",
            a.id(),
            mib(peak),
            peak_regions,
            mib(a.size()),
            a.regions(),
            mib(a.floor()),
        );
        println!(
            "[proxy] arena {} churn: grew {grown} time(s) in {:.1} ms total, \
             {refused} refused; released {released} region(s), {:.1} MiB",
            a.id(),
            grow_nanos as f64 / 1e6,
            mib(released_bytes),
        );
    }
    println!("[proxy] stopped: {served} client(s) served");
    Ok(())
}
