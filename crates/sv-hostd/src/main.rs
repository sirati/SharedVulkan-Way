//! `sv-hostd` — create the arena's backing and hand it out.
//!
//! ```text
//! sv-hostd serve --socket /run/sv/hostd.sock [--generation N] [--arena BYTES]
//! ```
//!
//! One extent, any number of clients. Each client that connects gets an
//! `Attachment` describing the two views and **the name of the object to map
//! them from**, which it opens itself — docs/design.md §1a. No descriptor crosses,
//! which is what lets the same handoff work when the peer is a VM.
//!
//! Each client replies `Accept` once it has opened the object. The daemon waits
//! for that before it counts the client served, and unlinks the name only when
//! it exits: unlinking releases the *name*, not the memory, so a client that has
//! opened is unaffected — but a client that has not opened yet would find
//! nothing, and the reply is what rules that race out.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-hostd: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// The daemon creates a `/dev/shm` object and serves a Unix socket, and neither
/// is spelled that way on Windows. design.md requires either side to be either
/// OS, and the *app* side is the one that runs `sv-hostd`; a Windows app side
/// needs a named section and a named pipe — and, since §1a, **no
/// `DuplicateHandle`**, because the peer opens the section by name.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("sv-hostd: the daemon is Linux-only; a Windows app side needs a named-section handoff");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::net::UnixListener;
    use sv_hostd::{Hostd, demo_geometry};
    use sv_transport::cold::{ColdMessage, recv, send};

    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    match args.get(1).map(String::as_str) {
        Some("serve") => {}
        Some("qemu") => return qemu_mode(&args),
        Some("qemu-serve") => return qemu_serve(&args),
        Some("ivshmem-serve") => return ivshmem_serve(&args),
        Some("bell-hub") => return bell_hub_serve(&args),
        Some("broker") => return broker_serve(&args),
        Some("broker-request") => return broker_request(&args),
        _ => {
            eprintln!(
                "usage:\n  \
                 sv-hostd serve --socket <path> [--generation <n>] [--arena <bytes>] [--clients <n>] [--live]\n  \
                 sv-hostd qemu --qmp <sock> [--vm <hint>] --run <hex> --control-pages <n> --arena-pages <n> [--verify]\n                \
                 [--locator scan|pfns|both] [--frames <guest-status.txt>]\n  \
                 sv-hostd qemu-serve [--qmp <sock>] --cold <sock|mem[:LABEL]|file:PATH> [--cold ...] --socket <path> [--vm <hint>] [--clients N] [--drop-to UID:GID]\n  \
                 sv-hostd ivshmem-serve --region <file> --cold pool [--cold-bell <hub sock>] --socket <path> [--bell <chardev sock> --bell-hub <path>] [--clients N]\n                \
                 (--cold <sock>: the TCP cold path, debug only)\n  \
                 sv-hostd bell-hub --port <chardev sock|dev:NAME>=<hub sock> [--port ...]   (§99: one per user, every session shares it)\n  \
                 sv-hostd broker --listen <sock> [--socket-gid GID] [--qmp <sock>] [--vm <hint>] [--runroot /tmp/wc-launch]\n  \
                 sv-hostd broker-request --broker <sock> --dir <session dir> --sessions N"
            );
            return Err("no subcommand".into());
        }
    }
    let path = arg("--socket").ok_or("--socket is required")?;
    let generation: u32 = arg("--generation").map_or(Ok(1), |s| s.parse())?;
    // `--live` serves the geometry a real session uses; the default stays the
    // demo one, because `sv-loopback` and the container harness are sized for
    // it and a 16 MiB control region would make them slower for no gain.
    //
    // The two must not be chosen independently. On the memfd path `sv-hostd`
    // sizes the file and `sv-gpu-server` publishes the header, so a daemon
    // serving 1 MiB to an owner that wants 16 MiB produces "the layout does not
    // fit in control_size" from the *other* process — which is how this flag
    // came to exist.
    let mut geometry = if args.iter().any(|a| a == "--live") {
        sv_transport::control::Geometry::for_arena(64 << 20)
    } else {
        demo_geometry()
    };
    if let Some(n) = arg("--arena") {
        geometry.arena_size = n.parse()?;
    }
    // Serve a fixed number of clients and exit, so the harness is a command that
    // finishes rather than a daemon a script has to remember to kill.
    let clients: usize = arg("--clients").map_or(Ok(2), |s| s.parse())?;

    // The run id names the shared object (`arena.<run id>`), created with
    // O_EXCL, so two daemons started in the same instant must not pick the
    // same one: the clock's nanoseconds alone collided under load (two
    // `two_processes` runs in 200 died with EEXIST before binding). The pid
    // makes it unique among live daemons.
    let run_id = (u64::from(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos(),
    ) ^ (u64::from(std::process::id()) << 32))
        | 1;
    let hostd = Hostd::local(geometry, run_id, generation)?;
    eprintln!(
        "sv-hostd: {} MiB control + {} MiB arena, {} run(s), generation {}",
        geometry.control_size >> 20,
        geometry.arena_size >> 20,
        hostd.runs.len(),
        generation
    );
    eprintln!("sv-hostd: shared object {} (opened by name; no descriptor is passed)", hostd.object);

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // The socket is the rendezvous, so it has to be reachable by the other
    // container. A bind mount of its directory is enough and needs no shared
    // network namespace.
    eprintln!("sv-hostd: listening on {path}");

    for i in 0..clients {
        let (sock, _) = listener.accept()?;
        let msg = recv(&sock)?;
        match msg {
            ColdMessage::AttachRequest => {
                send(&sock, &ColdMessage::Attach(hostd.attachment()))?;
                // Wait for the client to say it has the object open. Only then
                // is the name safe to unlink, and only then is "attached" true.
                // A dead client must not hang the daemon, so the wait has a
                // deadline — R4 applied to the handoff.
                sock.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
                match recv(&sock) {
                    Ok(ColdMessage::Accept { generation: g }) => {
                        eprintln!("sv-hostd: client {i} attached by name, generation {g}");
                    }
                    Ok(other) => eprintln!("sv-hostd: client {i} said {other:?} rather than Accept"),
                    Err(e) => eprintln!("sv-hostd: client {i} never acknowledged the name: {e}"),
                }
            }
            other => {
                send(
                    &sock,
                    &ColdMessage::Refuse {
                        code: 1,
                        reason: format!("expected AttachRequest, got {other:?}"),
                    },
                )?;
            }
        }
        // The client holds its own descriptor on the named object now, so
        // dropping our end of the socket costs it nothing.
    }
    // The name has done its job: every client acknowledged, and each holds its
    // own descriptor. Released here rather than only in `Hostd::drop` so that a
    // daemon killed after this point leaves nothing behind in `/dev/shm` —
    // `Drop` does not run for `SIGKILL`, and 64 MiB of tmpfs that outlives the
    // process until the next reboot is the kind of leak a memfd could not have.
    sv_transport::region::shm_unlink(&hostd.object);
    eprintln!("sv-hostd: served {clients} client(s), released the name, exiting");
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// `sv-hostd qemu` — attach to an extent the guest allocated.
///
/// This is `Mode::Qemu` as a command, and it exists to be pointed at a real VM
/// before anything depends on it. It discovers QEMU, reads the region table,
/// locates the run by content, coalesces, stitches, and reports — including the
/// numbers design.md quotes, so a regression in any of them is visible rather
/// than inferred.
///
/// `--verify` re-reads every page of the stitched view and checks the stamp's
/// page index against the extent offset it landed at. That is the §1 invariant
/// as an assertion over real guest memory: offset *n* of the extent must hold
/// the page the guest called *n*, whatever the file offsets were. It costs one
/// pass over the extent and it is the check that would catch a stitcher that
/// mapped the right pages in the wrong order — which reads as "the arena works"
/// on every test that does not look at the index.
///
/// `--locator scan|pfns|both` picks which of §5.1's locators runs, and defaults
/// to `scan` — the behaviour every measurement in the design was taken with.
/// `pfns` and `both` need `--frames <guest-status.txt>`, the file the guest's
/// `sf-probe` writes, because a run driven by hand has no cold path to carry the
/// frame list. `both` is R28's attach-time cross-check and is the mode that
/// proves the frame path: it locates the same extent twice, by two mechanisms
/// with nothing in common, and refuses if they disagree about any page.
#[cfg(target_os = "linux")]
fn qemu_mode(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use sv_hostd::{LocatorChoice, locate};
    use sv_transport::cold::{ExtentDesc, Granularity, Locator};
    use sv_transport::region::PAGE;

    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let qmp = arg("--qmp").ok_or("--qmp is required")?;
    let hint = arg("--vm").unwrap_or_default();
    let run_id =
        u64::from_str_radix(arg("--run").ok_or("--run is required")?.trim_start_matches("0x"), 16)?;
    let control_pages: u64 = arg("--control-pages").map_or(Ok(0), |s| s.parse())?;
    let arena_pages: u64 = arg("--arena-pages").ok_or("--arena-pages is required")?.parse()?;
    let large = args.iter().any(|a| a == "--large");
    let choice = match arg("--locator") {
        Some(s) => LocatorChoice::parse(&s).ok_or(format!("--locator wants scan|pfns|both, not {s:?}"))?,
        None => LocatorChoice::default(),
    };

    // The locator the descriptor carries *is* the frame list, so it is built
    // here rather than passed alongside: a descriptor that says `Pfns` and has
    // no frames is not a thing that should be representable this far in.
    let locator = match (choice, arg("--frames")) {
        (LocatorChoice::Scan, _) => Locator::Scan,
        (_, Some(path)) => {
            let frames = locate::parse_frame_list(&std::fs::read_to_string(&path)?)
                .map_err(|e| format!("{path}: {e}"))?;
            println!(
                "read {} frame(s) from {path}; first 0x{:x}",
                frames.len(),
                frames.first().copied().unwrap_or(0)
            );
            Locator::from_frames(&frames)
        }
        (_, None) => {
            return Err("--locator pfns|both needs --frames <guest-status.txt>".into());
        }
    };

    let desc = ExtentDesc {
        run_id,
        // Asserted by the *guest* and carried on the cold path; the flag here is
        // only for a run driven by hand. It selects the canary policy and
        // nothing about locating, because in v1 the guest stamps every 4 KiB
        // page whatever the allocator did (§5.1).
        granularity: if large { Granularity::Large2M } else { Granularity::Small4K },
        control_pages,
        arena_pages,
        locator,
    };

    let started = std::time::Instant::now();
    let (hostd, regions, outcome) =
        sv_hostd::Hostd::qemu_located(std::path::Path::new(&qmp), &hint, &desc, 1, choice)?;
    let located = started.elapsed();

    println!(
        "guest RAM in the flat view: {} MiB in {} alias(es)",
        regions.total_ram() >> 20,
        regions.aliases().len()
    );
    for a in regions.aliases() {
        println!(
            "  gpa 0x{:012x}..0x{:012x} -> offset 0x{:012x}",
            a.gpa,
            a.gpa + a.len - 1,
            a.file_offset
        );
    }
    println!("located by `{}` in {:.2?}", choice.as_str(), located);
    if let Some(r) = &outcome.scan {
        println!(
            "  scan: {}/{} pages (swept {} MiB; {} other-run stamps, {} outside guest RAM)",
            r.pages_found,
            r.pages_expected,
            r.bytes_swept >> 20,
            r.other_runs,
            r.outside_ram
        );
    }
    if let Some(r) = &outcome.pfns {
        println!(
            "  frame list: {} pages, {} run(s) of frames -> {} run(s) of file, first frame 0x{:x}",
            r.pages, r.frame_runs, r.file_runs, r.first_frame
        );
    }
    // R28: the frame list is believed *because* this agreed, and a run that
    // took it on trust has to look different in the log from one that checked.
    match &outcome.cross_check {
        Some(c) => println!(
            "  R28 cross-check: the scan and the frame list agree about {}/{} pages",
            c.agreed, c.pages
        ),
        None if outcome.pfns.is_some() => {
            println!("  R28 cross-check: NOT RUN — the frame list was taken on trust")
        }
        None => {}
    }
    println!(
        "coalesced into {} run(s) covering {} MiB — {:.1} pages per run",
        hostd.runs.len(),
        hostd.runs.bytes() >> 20,
        (desc.control_pages + desc.arena_pages) as f64 / hostd.runs.len() as f64
    );
    if let Some(first) = hostd.runs.runs().first() {
        println!(
            "  first run: offset 0x{:012x} (gpa 0x{:012x}), {} KiB",
            first.file_offset,
            regions.offset_to_gpa(first.file_offset).unwrap_or(0),
            first.len >> 10
        );
    }

    let attach_started = std::time::Instant::now();
    let file = {
        use std::os::fd::{AsFd, AsRawFd, FromRawFd};
        // `Hostd` owns the descriptor; borrow it as a `File` without taking it,
        // so the `File` must not close it on drop.
        let raw = hostd.fd.as_fd().as_raw_fd();
        std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(raw) })
    };
    let view = locate::attach(&file, &hostd.runs)?;
    println!(
        "stitched {} MiB into one view with {} mmap(MAP_FIXED) call(s) in {:.2?}",
        view.len() >> 20,
        view.runs(),
        attach_started.elapsed()
    );

    if args.iter().any(|a| a == "--verify") {
        let pages = view.len() / PAGE;
        let mut wrong = 0usize;
        let mut header = [0u8; 20];
        for i in 0..pages {
            view.read(i as u64 * PAGE as u64, &mut header).ok_or("short read")?;
            let magic_ok = &header[0..8] == arena_record::MAGIC;
            let run_ok = u64::from_le_bytes(header[8..16].try_into().unwrap()) == run_id;
            let index_ok = u32::from_le_bytes(header[16..20].try_into().unwrap()) == i as u32;
            if !(magic_ok && run_ok && index_ok) {
                wrong += 1;
            }
        }
        println!(
            "verify: {}/{} pages of the stitched view carry their own extent index",
            pages - wrong,
            pages
        );
        if wrong > 0 {
            return Err(format!("{wrong} page(s) are not where the extent says they are").into());
        }
    }
    Ok(())
}

/// `sv-hostd qemu-serve` — the app-side daemon for the cross-VM topology.
///
/// The whole of `Mode::Qemu`, wired end to end:
///
///  1. accept the guest's `Hello` on the **cold path** — a Unix socket that
///     QEMU's virtio-serial chardev is the other end of, so what arrives here
///     was written to `/dev/virtio-ports/...` inside the guest;
///  2. locate the extent it describes by content, translate through the region
///     table read off `info mtree -f`, coalesce, and stitch;
///  3. reply `Accept`, which is the guest's signal that its memory is reachable;
///  4. serve the stitched view to ICD processes over an ordinary Unix socket.
///
/// Step 4 is where the two topologies converge. The ICD receives an
/// `Attachment` and a *name* exactly as it does from `Mode::Local`, and it never
/// learns that the pages behind that name are another machine's RAM found by
/// sweeping thirteen gigabytes for a magic number.
///
/// **The name is QEMU's backing object, and the ICD opens it itself**
/// (`qemu::backend_name`): the `mem-path` when the guest is on
/// `memory-backend-file`, and `/proc/<qemu-pid>/fd/<n>` when it is on
/// `memory-backend-memfd`. docs/design.md §1a calls the second case legal and says
/// what it costs — it is a same-machine name, so it may only ever be given to
/// the *app* side, which is where every reader of it is. The GPU side is inside
/// the VM and never receives a name at all: it locates its own memory by content
/// (§5.1), which is the strongest possible form of "no descriptor crosses".
///
/// The extent is scattered through that object, so the ICD also needs the run
/// list — one `mmap` per run into one reservation. That is docs/design.md §3's "hands
/// **name + runs** to the ICDs", and it is a different boundary from §5.7's "the
/// run list does not cross the wire", which is about guest↔host.
#[cfg(target_os = "linux")]
fn qemu_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::net::UnixListener;
    use sv_transport::cold::{ColdMessage, recv, send};

    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    // **`--cold` may be given more than once, and each one is a whole session.**
    //
    // A cross-VM session is one pinned window in the guest, one extent for this
    // daemon to locate, and one control region — so N sessions are N guests
    // publishing on N cold paths, and this daemon locates each and hands the
    // i-th to the i-th ICD client. §0.12 made the same split on the
    // same-machine path for the same reason and it arrives here unchanged:
    // Firefox is a multi-process application, `glxtest` takes the first session
    // before the browser exists, and a second client onto the first client's
    // rings is the failure §0.11 spent a day diagnosing.
    //
    // One `--cold` behaves exactly as it did, which is what every existing
    // harness passes.
    let colds: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "--cold")
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect();
    if colds.is_empty() {
        return Err("--cold is required".into());
    }
    let qmp = arg("--qmp");
    let path = arg("--socket").ok_or("--socket is required")?;
    let hint = arg("--vm").unwrap_or_else(|| "sv-guest".into());
    // Defaulted from the number of sessions rather than to 1, because asking
    // for more clients than there are sessions is the one arrangement that
    // cannot work and should not be spellable by accident.
    let clients: usize = arg("--clients").map_or(Ok(colds.len()), |s| s.parse())?;
    if clients > colds.len() {
        return Err(format!(
            "{clients} ICD client(s) asked for and only {} session(s) offered; \
             each client needs its own control region",
            colds.len()
        )
        .into());
    }

    let drop_to = arg("--drop-to").map(|s| parse_drop_to(&s)).transpose()?;

    let mut sessions = Vec::new();
    for cold in &colds {
        sessions.push(open_session(qmp.as_deref(), &hint, cold)?);
    }

    // **Root for the locate, and not one instruction longer** (waycompose-design.md
    // §86). QMP and `/proc/<qemu-pid>/fd` are root's on a machine where QEMU is a
    // system service, so the locate needs root; serving does not. With
    // `--drop-to` this process becomes that user here, before it binds the
    // socket an application connects to, and from then on the name it hands out
    // is its *own* descriptor, `/proc/<this pid>/fd/<n>`, which the same user
    // can open once the process is dumpable again. So the ICD — Firefox — runs
    // as the user, where the harness used to run it as root because the only
    // name on offer was QEMU's.
    if let Some((uid, gid)) = drop_to {
        let me = std::process::id();
        for s in &mut sessions {
            use std::os::fd::{AsFd, AsRawFd};
            let Some(hostd) = s.hostd.as_ref() else { continue };
            let own = format!("/proc/{me}/fd/{}", hostd.fd.as_fd().as_raw_fd());
            s.attachment.object = own.clone();
            s.attachment.arena_object = own;
        }
        drop_privileges(uid, gid)?;
        // Checked rather than assumed: a name the user cannot open is a
        // Firefox that falls back to software three layers up.
        let name = &sessions[0].attachment.object;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
            .map_err(|e| format!("after dropping to {uid}:{gid}, {name} does not open: {e}"))?;
        eprintln!("sv-hostd: now uid {uid} gid {gid}; serving {name} and its siblings by this process's own descriptors");
    }

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!(
        "sv-hostd: listening on {path} for {clients} ICD client(s) over {} session(s); \
         they open {} by name",
        sessions.len(),
        sessions[0].attachment.object
    );
    // **The i-th client gets the i-th session, never a shared one.** Handing two
    // clients one attachment would put two ICDs on one control region and one
    // ring set, which is precisely the arrangement §0.11 measured: the first is
    // served, the second waits forever, and Firefox exits with `Exiting due to
    // channel error` two layers up.
    for (i, session) in sessions.iter().enumerate().take(clients) {
        let (sock, _) = listener.accept()?;
        let attachment = &session.attachment;
        let msg = recv(&sock)?;
        match msg {
            ColdMessage::AttachRequest => {
                // QEMU's object by name, not a descriptor and not a copy: the
                // ICD opens it and maps the guest's RAM directly. What it needs
                // on top is the run list, because a scattered extent has no
                // single offset whose `mmap` reproduces it.
                send(&sock, &ColdMessage::Attach(attachment.clone()))?;
                if attachment.run_count != 0 {
                    send(&sock, &ColdMessage::Runs(session.runs_for_wire.clone()))?;
                }
                // The same acknowledgement `Mode::Local` waits for. Nothing here
                // needs unlinking — the object is QEMU's — but a client that
                // could not open the name should say so *here*, where the
                // daemon can print it, rather than dying three layers up with
                // `NotConnected`.
                sock.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
                match recv(&sock) {
                    Ok(ColdMessage::Accept { .. }) => {}
                    Ok(other) => eprintln!("sv-hostd: ICD client {i} said {other:?}"),
                    Err(e) => eprintln!("sv-hostd: ICD client {i} never acknowledged: {e}"),
                }
                eprintln!(
                    "sv-hostd: ICD client {i} attached by name to session {i} ({} run(s))",
                    session.hostd.as_ref().map_or(0, |h| h.runs.len())
                );
            }
            other => {
                send(
                    &sock,
                    &ColdMessage::Refuse {
                        code: 1,
                        reason: format!("expected AttachRequest, got {other:?}"),
                    },
                )?;
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// `sv-hostd ivshmem-serve` — the app side of the cross-VM topology with the
/// GPU sessions' arena on an ivshmem device (waycompose-design.md §87, §97),
/// **as the user, from start to end**.
///
/// For every ICD that connects to `--socket`, one session of the guest's GPU
/// process, checked in `--region` (the file QEMU backs the BAR with) and
/// handed to the ICD by the file's path and offsets. The ICD opens the file
/// itself, by name (docs/design.md §1a), with the same group bit this process
/// used.
///
/// `--cold pool` (what `wc-session` passes): the cold path is the pool's
/// mailboxes (`sv_transport::poolcold`), in the header granule of the same
/// file, which is therefore opened read-write. Per ICD: claim a live offer of
/// this pool by compare-and-swap, `check_slot` it, answer in the page, ring
/// the guest, and hand the ICD its `Attach` once the guest has published the
/// control region. No socket reaches the guest. `--cold-bell <hub socket>`
/// (default: `--bell-hub`) is the doorbell hub this process connects to for
/// the length of each exchange, only to carry `park::COLD_TOKEN` both ways;
/// without it every wait is a short poll, and only while an ICD waits.
///
/// `--cold <sock>`: the TCP cold path through a socket (formerly the ssh
/// tunnel's forward of the guest's port), kept as a debug mode and announced
/// as one.
///
/// Sessions are made on demand rather than located up front, so there is no
/// count to agree with the guest: Firefox's `glxtest` takes one, ends, and its
/// place is the browser's. `--clients N` stops after N (0, the default, never).
///
/// With `--bell`, this process also holds the doorbell port's chardev and
/// serves it to every ICD on `--bell-hub` (`sv_hostd::ivshmem::bell_hub`).
#[cfg(target_os = "linux")]
fn ivshmem_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::FileExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::Arc;
    use sv_transport::cold::{ColdMessage, recv, send};
    use sv_transport::poolcold;
    use sv_transport::stream::{read_message, write_message};

    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let region = arg("--region").ok_or("--region is required")?;
    let cold = arg("--cold").ok_or("--cold is required (pool, or a socket for the TCP debug path)")?;
    let pooled_cold = cold == "pool";
    let path = arg("--socket").ok_or("--socket is required")?;
    let clients: u64 = arg("--clients").map_or(Ok(0), |s| s.parse())?;
    if unsafe { libc::geteuid() } == 0 {
        // Nothing here needs root, and the point of this mode is that nothing
        // runs as root; a root caller is a deployment mistake to refuse, not to
        // accommodate.
        return Err("ivshmem-serve runs as the user; refusing to run as root".into());
    }

    // Read-write for the mailboxes' compare-and-swap and replies; the group
    // bit that lets this user read the file lets it write too (0660).
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(pooled_cold)
        .open(&region)
        .map_err(|e| format!("{region}: {e} (is this user in the region's group?)"))?;
    let region_len = file.metadata()?.len();

    let mailboxes = if pooled_cold {
        if region_len < sv_transport::poolhdr::HEADER_LEN {
            return Err(format!("{region}: {region_len} bytes, smaller than the pool's header granule").into());
        }
        let header = sv_transport::region::SharedRegion::map_fd(
            file.as_fd(),
            0,
            sv_transport::poolhdr::HEADER_LEN as usize,
        )?;
        let mb = Arc::new(poolcold::Mailboxes::new(Arc::new(header))?);
        let live = poolcold::Liveness::new();
        poolcold::spawn_liveness(Arc::clone(&mb), Arc::clone(&live))?;
        Some((mb, live))
    } else {
        eprintln!(
            "sv-hostd: GPU COLD PATH ON TCP (debug only): --cold {cold} is a socket to the guest's TCP port; the \
             deployed cold path is --cold pool"
        );
        None
    };
    let file = Arc::new(file);

    if let Some(bell) = arg("--bell") {
        let hub = arg("--bell-hub").ok_or("--bell needs --bell-hub")?;
        match UnixStream::connect(&bell) {
            Ok(up) => {
                let _ = std::fs::remove_file(&hub);
                let l = UnixListener::bind(&hub)?;
                sv_hostd::ivshmem::bell_hub(up, l)?;
                eprintln!("sv-hostd: doorbell {bell} held, served to every ICD on {hub}");
            }
            Err(e) => eprintln!(
                "sv-hostd: no doorbell ({bell}: {e}); the ICDs' waits will run out their slice"
            ),
        }
    }
    let cold_bell = arg("--cold-bell").or_else(|| arg("--bell-hub"));

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!(
        "sv-hostd: listening on {path} (ivshmem, as uid {}): each ICD gets its own session of {region} \
         ({} MiB) through {}",
        unsafe { libc::geteuid() },
        region_len >> 20,
        if pooled_cold {
            format!(
                "the pool's mailboxes{}",
                cold_bell.as_deref().map_or(" (no doorbell: polled while an ICD waits)".to_string(), |b| format!(", woken through {b}"))
            )
        } else {
            cold.clone()
        }
    );
    let mut served = 0u64;
    for conn in listener.incoming() {
        let sock = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("sv-hostd: accept: {e}");
                continue;
            }
        };
        served += 1;
        let i = served;
        let (file, region, cold) = (Arc::clone(&file), region.clone(), cold.clone());
        let (mailboxes, cold_bell) = (mailboxes.clone(), cold_bell.clone());
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let result = (|| -> Result<String, String> {
                match recv(&sock).map_err(|e| format!("no attach request: {e}"))? {
                    ColdMessage::AttachRequest => {}
                    other => return Err(format!("expected AttachRequest, got {other:?}")),
                }
                let mut timing = String::new();
                let (at, hello) = if let Some((mb, live)) = &mailboxes {
                    // The hub connection is opened first and the mailboxes
                    // scanned after: a wake that was sent before it is
                    // covered by the scan (the reopen rule).
                    let event = poolcold::ColdEvent::new();
                    let bell = cold_bell.as_deref().and_then(|b| match poolcold::HubBell::connect(b, Arc::clone(&event)) {
                        Ok(h) => Some(h),
                        Err(e) => {
                            eprintln!("sv-hostd: ICD client {i}: no doorbell hub at {b} ({e}); polling");
                            None
                        }
                    });
                    let poke = || {
                        if let Some(b) = &bell {
                            b.poke();
                        }
                    };
                    let control_ready = |h: &sv_transport::cold::Hello| {
                        let sv_transport::cold::Locator::IvshmemPool { control_offset, .. } = h.extent.locator else {
                            return false;
                        };
                        let mut m = [0u8; 8];
                        file.read_exact_at(&mut m, control_offset + sv_transport::control::OFF_MAGIC).is_ok()
                            && u64::from_le_bytes(m) == sv_transport::control::MAGIC
                    };
                    let a = poolcold::host_attach(
                        mb,
                        live,
                        &event,
                        &poke,
                        claim_nonce(),
                        poolcold::HostTiming::default(),
                        |h| sv_hostd::ivshmem::check_slot(&file, &region, region_len, &h.extent, h.generation),
                        control_ready,
                    )?;
                    timing = format!(
                        " (mailbox {}, claimed after {:.2?}, published {:.2?} after the Accept)",
                        a.mailbox, a.claimed_after, a.published_after
                    );
                    (a.value, a.hello)
                } else {
                    tcp_cold(&cold, &file, &region, region_len)?
                };
                send(&sock, &ColdMessage::Attach(at.clone())).map_err(|e| format!("Attach: {e}"))?;
                sock.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
                match recv(&sock) {
                    Ok(ColdMessage::Accept { .. }) => {}
                    Ok(other) => return Err(format!("the ICD said {other:?}")),
                    Err(e) => return Err(format!("the ICD never acknowledged: {e}")),
                }
                // What was checked: a slot's every page, or a pooled session's
                // control region only (its arena is the whole device, §97).
                let pooled = matches!(hello.extent.locator, sv_transport::cold::Locator::IvshmemPool { .. });
                let checked = if pooled { at.control_len } else { at.control_len + at.arena_len };
                Ok(format!(
                    "{} at +{} KiB, run 0x{:016x}, {} pages checked{timing}",
                    if pooled { "pooled session's control region" } else { "slot" },
                    at.control_offset >> 10,
                    hello.extent.run_id,
                    checked / sv_transport::region::PAGE as u64
                ))
            })();
            match result {
                Ok(s) => eprintln!("sv-hostd: ICD client {i} attached: {s} in {:.2?}", t.elapsed()),
                Err(e) => {
                    let _ = send(&sock, &ColdMessage::Refuse { code: 2, reason: e.clone() });
                    eprintln!("sv-hostd: ICD client {i}: {e}");
                }
            }
        });
        if clients != 0 && served >= clients {
            break;
        }
    }
    // Let the last client's thread finish its handshake before the process —
    // and the doorbell hub with it — goes.
    if clients != 0 {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
    return Ok(());

    /// The TCP debug path's exchange: connect to `cold`, the guest's `Hello`,
    /// the check, `Accept`, and the guest's `Ping` once published.
    fn tcp_cold(
        cold: &str,
        file: &std::fs::File,
        region: &str,
        region_len: u64,
    ) -> Result<(sv_transport::cold::Attachment, sv_transport::cold::Hello), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut port = loop {
            match UnixStream::connect(cold) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(e) => return Err(format!("cannot reach the guest at {cold}: {e}")),
            }
        };
        port.set_read_timeout(Some(std::time::Duration::from_secs(30))).ok();
        let mut scratch = Vec::new();
        let hello = match read_message(&mut port, &mut scratch).map_err(|e| format!("no Hello from the guest: {e}"))? {
            ColdMessage::Hello(h) => h,
            ColdMessage::Refuse { code, reason } => return Err(format!("the guest refused {code}: {reason}")),
            other => return Err(format!("expected Hello, got {other:?}")),
        };
        let at = match sv_hostd::ivshmem::check_slot(file, region, region_len, &hello.extent, hello.generation) {
            Ok(a) => a,
            Err(e) => {
                let _ = write_message(&mut port, &ColdMessage::Refuse { code: 3, reason: e.clone() });
                return Err(e);
            }
        };
        write_message(&mut port, &ColdMessage::Accept { generation: hello.generation })
            .map_err(|e| format!("Accept: {e}"))?;
        // The guest opens its device, imports the slot's arena and publishes
        // the control region over the stamps, then says so. Only then may the
        // ICD map it: before, the control header is a stamp, which the ICD
        // refuses as a foreign magic.
        match read_message(&mut port, &mut scratch).map_err(|e| format!("the guest never published the session: {e}"))? {
            ColdMessage::Ping => {}
            ColdMessage::Refuse { code, reason } => {
                return Err(format!("the guest could not open the session ({code}): {reason}"));
            }
            other => return Err(format!("expected the guest's Ping, got {other:?}")),
        }
        Ok((at, *hello))
    }
}

/// `UID:GID`, both numeric. Names are not resolved: this runs as root, and a
/// lookup through NSS is more code running as root than the two numbers need.
#[cfg(target_os = "linux")]
fn parse_drop_to(s: &str) -> Result<(u32, u32), Box<dyn std::error::Error>> {
    let (u, g) = s.split_once(':').ok_or("--drop-to wants UID:GID")?;
    let (uid, gid): (u32, u32) = (u.parse()?, g.parse()?);
    if uid == 0 || gid == 0 {
        return Err("--drop-to 0 is not a drop".into());
    }
    Ok((uid, gid))
}

/// Become `uid:gid` for good: no supplementary groups, real, effective and
/// saved ids all changed, and then **dumpable again**, because `setuid` clears
/// the flag and a non-dumpable process's `/proc/<pid>/fd` belongs to root.
/// Refuses to return if root can be taken back.
#[cfg(target_os = "linux")]
fn drop_privileges(uid: u32, gid: u32) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: plain syscalls on integers; no pointers but setgroups' null with
    // a count of zero, which is the documented way to clear the list.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 {
            return Err(format!("setgroups: {}", std::io::Error::last_os_error()).into());
        }
        if libc::setresgid(gid, gid, gid) != 0 {
            return Err(format!("setresgid {gid}: {}", std::io::Error::last_os_error()).into());
        }
        if libc::setresuid(uid, uid, uid) != 0 {
            return Err(format!("setresuid {uid}: {}", std::io::Error::last_os_error()).into());
        }
        if libc::setuid(0) == 0 {
            return Err("still able to become root after dropping privileges".into());
        }
        if libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) != 0 {
            return Err(format!("PR_SET_DUMPABLE: {}", std::io::Error::last_os_error()).into());
        }
    }
    Ok(())
}

/// One located cross-VM session: a guest's extent, found in QEMU's memory and
/// stitched into a flat view, with the attachment an ICD client needs to map
/// the same bytes.
///
/// Held together rather than returned as a tuple because three of the fields
/// exist only to be kept alive: dropping `hostd` closes the descriptor the
/// attachment names, and dropping `port` closes the cold path the guest is
/// still holding its pinned window against.
#[cfg(target_os = "linux")]
struct QemuSession {
    attachment: sv_transport::cold::Attachment,
    runs_for_wire: Vec<sv_transport::cold::AttachRun>,
    /// `None` on a shared-file session, which has no QEMU backend to hold.
    hostd: Option<sv_hostd::Hostd>,
    /// The guest's cold path, kept open for the life of the daemon; `None`
    /// when the cold path is the extent's first page (§103.6).
    #[allow(dead_code)]
    port: Option<std::os::unix::net::UnixStream>,
    /// A shared-file session's file, kept open for the life of the daemon.
    _file: Option<std::fs::File>,
}

/// Connect to one cold path, take the guest's `Hello`, find its pages in QEMU's
/// memory, check the stitched view, and tell it so.
///
/// This is the whole of what used to be the first half of [`qemu_serve`], moved
/// out unchanged so that it can happen once per session rather than once per
/// daemon. Every comment in it is about a step that is per-session — the scan,
/// the stitch, the page-index check — and none of them were about the daemon.
#[cfg(target_os = "linux")]
fn qemu_session(
    qmp: &str,
    hint: &str,
    cold: &str,
) -> Result<QemuSession, Box<dyn std::error::Error>> {
    use std::os::unix::net::UnixStream;
    use sv_transport::cold::ColdMessage;
    use sv_transport::stream::{read_message, write_message};

    // The cold path. QEMU is listening on this socket as the host end of the
    // guest's virtio-serial port, so connecting to it puts us on the other end
    // of `/dev/virtio-ports/<name>` inside the guest.
    eprintln!("sv-hostd: waiting for the guest on the cold path {cold}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut port = loop {
        match UnixStream::connect(cold) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(100))
            }
            Err(e) => return Err(format!("cannot reach {cold}: {e}").into()),
        }
    };

    let mut scratch = Vec::new();
    let hello = match read_message(&mut port, &mut scratch)? {
        ColdMessage::Hello(h) => h,
        other => return Err(format!("expected Hello, got {other:?}").into()),
    };
    eprintln!(
        "sv-hostd: the guest offers run 0x{:016x}, {} control pages + {} arena pages, locator {:?}",
        hello.extent.run_id,
        hello.extent.control_pages,
        hello.extent.arena_pages,
        hello.extent.locator,
    );

    // Locate. This is the two seconds, and it is the only expensive step.
    let started = std::time::Instant::now();
    let located = sv_hostd::Hostd::qemu(
        std::path::Path::new(&qmp),
        hint,
        &hello.extent,
        hello.generation,
    );
    let (hostd, regions, report) = match located {
        Ok(v) => v,
        Err(e) => {
            // Tell the guest rather than leaving it waiting. It is holding a
            // pinned window of its own RAM and the only thing that releases it
            // is the process exiting.
            let _ = write_message(
                &mut port,
                &ColdMessage::Refuse { code: 2, reason: format!("{e}") },
            );
            return Err(Box::new(e));
        }
    };
    eprintln!(
        "sv-hostd: located {}/{} pages in {:.2?} over {} MiB of guest RAM; {} run(s), {} outside RAM",
        report.pages_found,
        report.pages_expected,
        started.elapsed(),
        regions.total_ram() >> 20,
        hostd.runs.len(),
        report.outside_ram,
    );

    let (attachment, runs_for_wire, hostd) =
        finish_located(hostd, &hello, &mut |m| write_message(&mut port, m).map_err(Into::into))?;
    Ok(QemuSession { attachment, runs_for_wire, hostd: Some(hostd), port: Some(port), _file: None })
}

/// What a session's cold path is, from one `--cold` (§103.6):
///
/// * `mem` or `mem:LABEL`: the guest's offer in its extent's first page, found
///   by sweeping QEMU's memory (`--qmp`); no socket at all;
/// * `file:PATH`: the extent is a file both sides map (role (b): a Mac's RAM
///   disk shared into this Linux VM through virtio-fs DAX), offer at offset 0;
/// * anything else: the Unix socket a guest's Hello arrives on (a QEMU
///   chardev, as before).
#[cfg(target_os = "linux")]
fn open_session(qmp: Option<&str>, hint: &str, cold: &str) -> Result<QemuSession, Box<dyn std::error::Error>> {
    let need_qmp = || qmp.ok_or_else(|| format!("--cold {cold} needs --qmp"));
    if cold == "mem" {
        return mem_session(need_qmp()?, hint, "");
    }
    if let Some(label) = cold.strip_prefix("mem:") {
        return mem_session(need_qmp()?, hint, label);
    }
    if let Some(path) = cold.strip_prefix("file:") {
        return file_session(path);
    }
    qemu_session(need_qmp()?, hint, cold)
}

/// A nonce for a claim, so a guest's log can say which host took it.
#[cfg(target_os = "linux")]
fn claim_nonce() -> u64 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    (u64::from(std::process::id()) << 40) ^ t | 1
}

/// A session whose cold path is the extent's first page (§103.6): sweep for
/// a live offer, claim it, locate and check the extent, reply `Accept` in the
/// same page, and wait for the guest's control header before any ICD is
/// handed the region (an ICD that maps it earlier reads the stamp).
#[cfg(target_os = "linux")]
fn mem_session(qmp: &str, hint: &str, label: &str) -> Result<QemuSession, Box<dyn std::error::Error>> {
    use sv_transport::coldpage;
    eprintln!(
        "sv-hostd: sweeping guest RAM for a GPU offer{} (no socket: the cold path is the extent's first page)",
        if label.is_empty() { String::new() } else { format!(" labelled {label:?}") }
    );
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(120);
    let (hostd, regions, offered) =
        sv_hostd::Hostd::qemu_offered(std::path::Path::new(qmp), hint, label, claim_nonce(), deadline)?;
    let hello = offered.hello.clone();
    eprintln!(
        "sv-hostd: claimed run 0x{:016x} (label {:?}), {} control pages + {} arena pages, after {} sweep(s) in {:.2?}",
        hello.extent.run_id,
        offered.label,
        hello.extent.control_pages,
        hello.extent.arena_pages,
        offered.sweeps,
        started.elapsed(),
    );
    eprintln!(
        "sv-hostd: located {}/{} pages in {:.2?} (the last sweep) over {} MiB of guest RAM; {} run(s), {} outside RAM",
        offered.report.pages_found,
        offered.report.pages_expected,
        offered.last_sweep,
        regions.total_ram() >> 20,
        hostd.runs.len(),
        offered.report.outside_ram,
    );
    let page0 = offered.page0;
    let (attachment, runs_for_wire, hostd) =
        finish_located(hostd, &hello, &mut |m| coldpage::reply(&page0, m).map_err(Into::into))?;
    let t = std::time::Instant::now();
    if !coldpage::await_control(&page0, std::time::Duration::from_secs(60))? {
        return Err("the guest accepted but never published its control region".into());
    }
    eprintln!(
        "sv-hostd: the guest published its control region {:.2?} after the Accept; cold path {:.2?} in all",
        t.elapsed(),
        started.elapsed()
    );
    Ok(QemuSession { attachment, runs_for_wire, hostd: Some(hostd), port: None, _file: None })
}

/// A session over a shared file (§103.6, role (b)): the offer is at offset 0,
/// so nothing is swept, and the ICD maps the file by its path at the
/// attachment's offsets. The same mailbox and the same control region as the
/// guest-RAM path; only the memory under them differs.
#[cfg(target_os = "linux")]
fn file_session(path: &str) -> Result<QemuSession, Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use sv_transport::cold::{Attachment, ColdMessage};
    use sv_transport::coldpage;
    use sv_transport::region::{PAGE, SharedRegion};
    let started = std::time::Instant::now();
    let deadline = started + std::time::Duration::from_secs(120);
    eprintln!("sv-hostd: waiting for an offer at the start of {path}");
    let (file, page0, offer) = loop {
        let tried = (|| -> Result<Option<_>, Box<dyn std::error::Error>> {
            let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
            if file.metadata()?.len() < PAGE as u64 {
                return Ok(None);
            }
            let page0 = SharedRegion::map_fd(file.as_fd(), 0, PAGE)?;
            let Some(offer) = coldpage::read_offer(&page0)? else { return Ok(None) };
            if !coldpage::is_live(&page0, offer.heartbeat, std::time::Duration::from_millis(100))? {
                return Ok(None);
            }
            if !coldpage::claim(&page0, claim_nonce())? {
                return Ok(None);
            }
            Ok(Some((file, page0, offer)))
        })();
        match tried {
            Ok(Some(v)) => break v,
            Ok(None) | Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5))
            }
            Ok(None) => return Err(format!("no live offer at the start of {path} before the deadline").into()),
            Err(e) => return Err(format!("{path}: {e}").into()),
        }
    };
    let hello = offer.hello;
    let control_len = hello.extent.control_pages * PAGE as u64;
    let arena_len = hello.extent.arena_pages * PAGE as u64;
    let len = file.metadata()?.len();
    if len < control_len + arena_len {
        let reason = format!("{path} is {len} bytes and the offer describes {}", control_len + arena_len);
        let _ = coldpage::reply(&page0, &ColdMessage::Refuse { code: 3, reason: reason.clone() });
        return Err(reason.into());
    }
    eprintln!(
        "sv-hostd: claimed run 0x{:016x} (label {:?}) in {path} after {:.2?}",
        hello.extent.run_id,
        offer.label,
        started.elapsed()
    );
    coldpage::reply(&page0, &ColdMessage::Accept { generation: hello.generation })?;
    if !coldpage::await_control(&page0, std::time::Duration::from_secs(60))? {
        return Err("the guest accepted but never published its control region".into());
    }
    eprintln!("sv-hostd: control region published; cold path {:.2?} in all", started.elapsed());
    let object = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
    let attachment = Attachment {
        object: object.clone(),
        arena_object: object,
        control_offset: 0,
        control_len,
        arena_offset: control_len,
        arena_len,
        generation: hello.generation,
        // The file *is* the extent: offset 0 is its first byte, so the
        // shortcut is true here, as on `Mode::Local`.
        run_count: 0,
    };
    Ok(QemuSession { attachment, runs_for_wire: Vec::new(), hostd: None, port: None, _file: Some(file) })
}

/// The half of a located session that does not care how the Hello came: stitch
/// and check the extent, `reply` the verdict, and build what an ICD is given.
#[cfg(target_os = "linux")]
#[allow(clippy::type_complexity)]
fn finish_located(
    hostd: sv_hostd::Hostd,
    hello: &sv_transport::cold::Hello,
    reply: &mut dyn FnMut(&sv_transport::cold::ColdMessage) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<
    (sv_transport::cold::Attachment, Vec<sv_transport::cold::AttachRun>, sv_hostd::Hostd),
    Box<dyn std::error::Error>,
> {
    use std::os::fd::AsFd;
    use sv_hostd::locate;
    use sv_transport::cold::{Attachment, ColdMessage};
    use sv_transport::region::PAGE;

    // Stitch, and check the §1 invariant before telling the guest anything. A
    // view whose pages are in the wrong order reads as a working arena to
    // everything except an application, so it is checked here, once, where the
    // stamps still exist — they are overwritten the moment the arena is used.
    let file = {
        use std::os::fd::{AsRawFd, FromRawFd};
        let raw = hostd.fd.as_fd().as_raw_fd();
        std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(raw) })
    };
    let view = locate::attach(&file, &hostd.runs)?;
    let pages = view.len() / PAGE;
    let mut header = [0u8; 20];
    let mut wrong = 0usize;
    for i in 0..pages {
        view.read(i as u64 * PAGE as u64, &mut header).ok_or("short read")?;
        if &header[0..8] != arena_record::MAGIC
            || u64::from_le_bytes(header[8..16].try_into().unwrap()) != hello.extent.run_id
            || u32::from_le_bytes(header[16..20].try_into().unwrap()) != i as u32
        {
            wrong += 1;
        }
    }
    if wrong > 0 {
        let reason = format!("{wrong} of {pages} stitched pages are not where the extent says");
        let _ = reply(&ColdMessage::Refuse { code: 3, reason: reason.clone() });
        return Err(reason.into());
    }
    eprintln!(
        "sv-hostd: stitched {} MiB with {} mmap call(s); all {pages} pages carry their own index",
        view.len() >> 20,
        view.runs(),
    );

    // Accepting is the guest's signal that its memory is reachable, so it is
    // sent only after the check above has passed.
    reply(&ColdMessage::Accept { generation: hello.generation })?;

    // Serve the ICD. What it gets is the *name* of QEMU's backing object and the
    // run list; it opens the object and stitches its own flat view, whose
    // offsets match the guest's. Nothing is copied: the runs are mapped
    // `MAP_SHARED`, so the ICD's `vkMapMemory` returns a pointer into the VM's
    // RAM.
    let runs_for_wire: Vec<sv_transport::cold::AttachRun> = hostd
        .runs
        .runs()
        .iter()
        .map(|r| sv_transport::cold::AttachRun {
            file_offset: r.file_offset,
            extent_offset: r.extent_offset,
            len: r.len,
        })
        .collect();
    if runs_for_wire.len() > sv_transport::cold::MAX_ATTACH_RUNS {
        return Err(format!(
            "{} runs will not fit in one cold-path message (max {}); \
             the guest needs huge pages for an arena this fragmented",
            runs_for_wire.len(),
            sv_transport::cold::MAX_ATTACH_RUNS
        )
        .into());
    }

    let attachment = Attachment {
        object: hostd.object.clone(),
        arena_object: hostd.object.clone(),
        control_offset: 0,
        control_len: hello.extent.control_pages * PAGE as u64,
        arena_offset: hello.extent.control_pages * PAGE as u64,
        arena_len: hello.extent.arena_pages * PAGE as u64,
        generation: hello.generation,
        // **Always the real count, never the `0` shortcut**, even when the
        // extent came back as a single run.
        //
        // `run_count == 0` means "map the fd at the attachment's offsets", and
        // that is only true where the daemon *made* the file — `Mode::Local`,
        // where offset 0 of the memfd is offset 0 of the extent. Here the file
        // is QEMU's memory backend and offset 0 of it is guest-physical zero, so
        // a client taking the shortcut would map the guest's first 48 MiB
        // instead of the arena. It would find valid, unrelated memory and report
        // no error, which is R21's failure mode arriving through the handoff
        // rather than through the translation.
        //
        // A one-run extent costs the client exactly one `mmap` either way, so
        // the shortcut buys nothing here and the two claims — "contiguous" and
        // "at these file offsets" — are kept apart.
        run_count: hostd.runs.len() as u64,
    };

    Ok((attachment, runs_for_wire, hostd))
}


/// `sv-hostd broker`: the root daemon the desktop launcher asks for GPU
/// sessions (waycompose-design.md §86, `sv_hostd::broker`).
///
/// One request at a time. For each, it checks the peer's session directory and
/// cold sockets against the peer's own uid, starts
/// `sv-hostd qemu-serve … --drop-to <peer uid>:<peer gid>` as a child, relays
/// the child's lines to the peer until the child is listening (or has died),
/// and only then takes the next request — QEMU's monitor serves one client and
/// the locate is the part that uses it. The child's later lines go to this
/// daemon's own standard error, which is its unit's journal.
#[cfg(target_os = "linux")]
fn broker_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::os::unix::net::UnixListener;
    use sv_hostd::broker::{self, Kind, Seen};

    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let listen = arg("--listen").ok_or("--listen is required")?;
    let qmp = arg("--qmp").unwrap_or_else(|| "/run/windows-vm/qmp.sock".into());
    let vm = arg("--vm").unwrap_or_else(|| "windows-vm".into());
    let runroot = arg("--runroot").unwrap_or_else(|| "/tmp/wc-launch".into());
    let me = std::env::current_exe()?;

    let _ = std::fs::remove_file(&listen);
    let listener = UnixListener::bind(&listen)?;
    // Who may ask: the socket's group, read-write, nobody else. Without
    // `--socket-gid` the socket is root's alone and only root could ask.
    if let Some(gid) = arg("--socket-gid") {
        let gid: u32 = gid.parse()?;
        let c = std::ffi::CString::new(listen.as_bytes())?;
        // SAFETY: a NUL-terminated path this process just bound.
        if unsafe { libc::chown(c.as_ptr(), 0, gid) } != 0 {
            return Err(format!("chown {listen}: {}", std::io::Error::last_os_error()).into());
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&listen, std::fs::Permissions::from_mode(0o660))?;
    } else {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&listen, std::fs::Permissions::from_mode(0o600))?;
    }
    eprintln!("sv-hostd broker: listening on {listen}");

    let seen = |path: &str| -> Result<Seen, String> {
        let m = std::fs::symlink_metadata(path).map_err(|e| format!("{path}: {e}"))?;
        let ft = m.file_type();
        let kind = if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_dir() {
            Kind::Dir
        } else if ft.is_socket() {
            Kind::Socket
        } else {
            Kind::Other
        };
        Ok(Seen { kind, uid: m.uid(), mode: m.mode() })
    };

    for conn in listener.incoming() {
        let Ok(mut conn) = conn else { continue };
        let cred = match peer_cred(&conn) {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(conn, "error: no peer credentials: {e}");
                continue;
            }
        };
        let mut line = String::new();
        let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        if BufReader::new(&conn).read_line(&mut line).is_err() {
            continue;
        }
        let checked = (|| -> Result<(broker::Request, Vec<String>), String> {
            if cred.0 == 0 {
                return Err("root does not need the broker".into());
            }
            let req = broker::parse_request(&line)?;
            broker::check_dir_name(&req.dir, &runroot)?;
            broker::check_dir(seen(&req.dir)?, cred.0, &req.dir)?;
            let colds = broker::cold_paths(&req.dir, req.sessions);
            for c in &colds {
                broker::check_cold(seen(c)?, cred.0, c)?;
            }
            Ok((req, colds))
        })();
        let (req, colds) = match checked {
            Ok(v) => v,
            Err(e) => {
                eprintln!("sv-hostd broker: refused uid {}: {e}", cred.0);
                let _ = writeln!(conn, "error: {e}");
                continue;
            }
        };
        let icd = format!("{}/icd.sock", req.dir);
        let mut cmd = std::process::Command::new(&me);
        cmd.arg("qemu-serve").args(["--qmp", &qmp, "--vm", &vm]);
        for c in &colds {
            cmd.args(["--cold", c]);
        }
        cmd.args(["--socket", &icd, "--clients", &req.sessions.to_string()])
            .args(["--drop-to", &format!("{}:{}", cred.0, cred.1)])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        eprintln!("sv-hostd broker: uid {} asks for {} session(s) in {}", cred.0, req.sessions, req.dir);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = writeln!(conn, "error: could not start qemu-serve: {e}");
                continue;
            }
        };
        // stdout carries nothing; stderr is the whole account. A reader thread
        // hands lines over until this loop stops listening, and then drains
        // the rest of the child's life into this daemon's journal so the child
        // never blocks on a full pipe.
        drop(child.stdout.take());
        let err = child.stderr.take().expect("piped");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let tag = req.dir.clone();
        std::thread::spawn(move || {
            for l in BufReader::new(err).lines().map_while(Result::ok) {
                if let Err(e) = tx.send(l) {
                    eprintln!("sv-hostd broker [{tag}]: {}", e.0);
                }
            }
        });
        // A child that never listens (a guest that never says Hello) must not
        // hold the one-at-a-time broker for ever.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
        let mut outcome = "error: qemu-serve ended before it listened";
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(l) => {
                    let _ = writeln!(conn, "{l}");
                    if l.contains("listening on") {
                        outcome = "ok";
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    outcome = "error: qemu-serve did not listen within 180 s; killed";
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = writeln!(conn, "{outcome}");
        drop(conn);
        drop(rx);
        let tag = req.dir.clone();
        std::thread::spawn(move || {
            let rc = child.wait();
            eprintln!("sv-hostd broker [{tag}]: qemu-serve exited: {rc:?}");
        });
    }
    Ok(())
}

/// The peer's uid and gid, from the kernel rather than from the request.
#[cfg(target_os = "linux")]
fn peer_cred(s: &std::os::unix::net::UnixStream) -> std::io::Result<(u32, u32)> {
    use std::os::fd::AsRawFd;
    let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: a valid socket, and an out buffer of the size given.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut c as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((c.uid, c.gid))
}

/// `sv-hostd broker-request`: what `wc-session gpu` runs, as the user. Prints
/// the broker's relay of qemu-serve's account and exits 0 only on `ok`.
#[cfg(target_os = "linux")]
fn broker_request(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{BufRead, BufReader, Write};
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let sock = arg("--broker").unwrap_or_else(|| "/run/sv-hostd/broker.sock".into());
    let dir = arg("--dir").ok_or("--dir is required")?;
    let n = arg("--sessions").ok_or("--sessions is required")?;
    let mut s = std::os::unix::net::UnixStream::connect(&sock).map_err(|e| format!("{sock}: {e}"))?;
    writeln!(s, "serve {dir} {n}")?;
    let mut last = String::new();
    for l in BufReader::new(&s).lines() {
        let l = l?;
        println!("{l}");
        last = l;
    }
    if last == "ok" { Ok(()) } else { Err(format!("the broker said: {last}").into()) }
}

/// `sv-hostd bell-hub` (waycompose-design.md §99): the doorbell ports, held
/// by no session. Each `--port UPSTREAM=HUB` is QEMU's chardev socket for a
/// virtio-serial port, or `dev:NAME` for the guest's own
/// `/dev/virtio-ports/NAME` (role (b), §103.6), and the socket every session
/// connects to instead. Runs until killed; a systemd user unit
/// (`wc-bell-hub.service`) keeps it up.
#[cfg(target_os = "linux")]
fn bell_hub_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if unsafe { libc::geteuid() } == 0 {
        return Err("bell-hub runs as the user; refusing to run as root".into());
    }
    // §99.6: what a previous run of this unit left in systemd's store.
    let mut kept = sv_hostd::sdfd::take();
    let keep = std::env::var_os("NOTIFY_SOCKET").is_some();
    if !kept.is_empty() {
        let mut names: Vec<&String> = kept.keys().collect();
        names.sort();
        eprintln!("bell-hub: inherited {} descriptor(s) from the previous run: {names:?}", names.len());
    }
    let mut hubs = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if a != "--port" {
            continue;
        }
        let spec = args.get(i + 1).ok_or("--port wants UPSTREAM=HUB")?;
        let (up, at) = spec.split_once('=').ok_or("--port wants UPSTREAM=HUB")?;
        let up = sv_hostd::bellhub::Upstream::parse(up);
        let name = up.name();
        let listen_key = format!("{name}-listen");
        let l = match kept.remove(&listen_key) {
            // The same listening socket as before the restart: a client that
            // reconnects at once finds it, never ECONNREFUSED.
            Some(fd) => std::os::unix::net::UnixListener::from(fd),
            None => {
                let l = sv_hostd::bellhub::bind_private(std::path::Path::new(at))?;
                if keep {
                    sv_hostd::sdfd::store(&listen_key, &l)?;
                }
                l
            }
        };
        let inherited = kept.remove(&format!("{name}-up"));
        eprintln!("bell-hub {name}: serving {up} to every session on {at} (0600)");
        hubs.push(sv_hostd::bellhub::run_kept(&name, up, l, inherited, keep)?);
    }
    if hubs.is_empty() {
        return Err("bell-hub needs at least one --port UPSTREAM=HUB".into());
    }
    let _ = sv_hostd::sdfd::notify("READY=1");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
        for h in &hubs {
            use std::sync::atomic::Ordering::Relaxed;
            eprintln!(
                "bell-hub: {} client(s); {} byte(s) down ({} dropped), {} up ({} dropped), {} upstream connect(s)",
                h.client_count(),
                h.stats.down.load(Relaxed),
                h.stats.down_dropped.load(Relaxed),
                h.stats.up.load(Relaxed),
                h.stats.up_dropped.load(Relaxed),
                h.stats.upstream_connects.load(Relaxed)
            );
        }
    }
}
