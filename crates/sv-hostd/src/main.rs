//! `sv-hostd` — create the arena's backing and hand it out.
//!
//! ```text
//! sv-hostd serve --socket /run/sv/hostd.sock [--generation N] [--arena BYTES]
//! ```
//!
//! One extent, any number of clients. Each client that connects gets an
//! [`Attachment`] describing the two views and the descriptor to map them from,
//! passed by `SCM_RIGHTS` — the same handoff the real deployment uses, which is
//! why the container harness exercises it rather than sharing a path.

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

/// The daemon opens a memfd and passes descriptors over a Unix socket, and
/// neither exists on Windows. design.md requires either side to be either OS,
/// and the *app* side is the one that runs `sv-hostd`; a Windows app side would
/// need a named-pipe and `DuplicateHandle` implementation of exactly this file.
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("sv-hostd: the daemon is Linux-only; a Windows app side needs a named-pipe handoff");
    std::process::ExitCode::FAILURE
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;
    use sv_hostd::{Hostd, demo_geometry};
    use sv_transport::cold::{ColdMessage, recv, send_with_fd};

    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    match args.get(1).map(String::as_str) {
        Some("serve") => {}
        Some("qemu") => return qemu_mode(&args),
        _ => {
            eprintln!(
                "usage:\n  \
                 sv-hostd serve --socket <path> [--generation <n>] [--arena <bytes>] [--clients <n>] [--live]\n  \
                 sv-hostd qemu --qmp <sock> [--vm <hint>] --run <hex> --control-pages <n> --arena-pages <n> [--verify]"
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

    let run_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .subsec_nanos() as u64
        | 1;
    let hostd = Hostd::local(geometry, run_id, generation)?;
    eprintln!(
        "sv-hostd: {} MiB control + {} MiB arena, {} run(s), generation {}",
        geometry.control_size >> 20,
        geometry.arena_size >> 20,
        hostd.runs.len(),
        generation
    );

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // The socket is the rendezvous, so it has to be reachable by the other
    // container. A bind mount of its directory is enough and needs no shared
    // network namespace.
    eprintln!("sv-hostd: listening on {path}");

    for i in 0..clients {
        let (sock, _) = listener.accept()?;
        let (msg, _) = recv(&sock)?;
        match msg {
            ColdMessage::AttachRequest => {
                let mut buf = Vec::new();
                ColdMessage::Attach(hostd.attachment()).encode(&mut buf)?;
                send_with_fd(&sock, &buf, Some(hostd.fd.as_fd()))?;
                eprintln!("sv-hostd: client {i} attached");
            }
            other => {
                let mut buf = Vec::new();
                ColdMessage::Refuse { code: 1, reason: format!("expected AttachRequest, got {other:?}") }
                    .encode(&mut buf)?;
                send_with_fd(&sock, &buf, None)?;
            }
        }
        // The client keeps the descriptor; dropping our end of the socket is
        // fine, because the memfd is refcounted by the kernel and the client's
        // copy holds it alive independently.
    }
    eprintln!("sv-hostd: served {clients} client(s), exiting");
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
#[cfg(target_os = "linux")]
fn qemu_mode(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use sv_hostd::locate;
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

    let desc = ExtentDesc {
        run_id,
        // Asserted by the *guest* and carried on the cold path; the flag here is
        // only for a run driven by hand. It selects the canary policy and
        // nothing about locating, because in v1 the guest stamps every 4 KiB
        // page whatever the allocator did (§5.1).
        granularity: if large { Granularity::Large2M } else { Granularity::Small4K },
        control_pages,
        arena_pages,
        locator: Locator::Scan,
    };

    let started = std::time::Instant::now();
    let (hostd, regions, report) =
        sv_hostd::Hostd::qemu(std::path::Path::new(&qmp), &hint, &desc, 1)?;
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
    println!(
        "located {}/{} pages in {:.2?} (swept {} MiB; {} other-run stamps, {} outside guest RAM)",
        report.pages_found,
        report.pages_expected,
        located,
        report.bytes_swept >> 20,
        report.other_runs,
        report.outside_ram
    );
    println!(
        "coalesced into {} run(s) covering {} MiB — {:.1} pages per run",
        hostd.runs.len(),
        hostd.runs.bytes() >> 20,
        report.pages_found as f64 / hostd.runs.len() as f64
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
