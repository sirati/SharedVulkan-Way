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
        Some("qemu-serve") => return qemu_serve(&args),
        _ => {
            eprintln!(
                "usage:\n  \
                 sv-hostd serve --socket <path> [--generation <n>] [--arena <bytes>] [--clients <n>] [--live]\n  \
                 sv-hostd qemu --qmp <sock> [--vm <hint>] --run <hex> --control-pages <n> --arena-pages <n> [--verify]\n  \
                 sv-hostd qemu-serve --qmp <sock> --cold <sock> --socket <path> [--vm <hint>] [--clients N]"
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
/// `Attachment` and a descriptor exactly as it does from `Mode::Local`, and it
/// never learns that the pages behind that descriptor are another machine's RAM
/// found by sweeping thirteen gigabytes for a magic number.
///
/// **The descriptor handed to the ICD is not the memfd.** It cannot be: the
/// extent is scattered through QEMU's backing object and the ICD needs one flat
/// region whose offsets match the guest's. So the daemon stitches the runs into
/// its own contiguous view and re-publishes *that* as a memfd of its own, which
/// is the one place this design copies rather than shares — and it does not:
/// the stitched view is `MAP_SHARED` over the same pages, so the memfd the ICD
/// maps is the guest's memory reached through a second set of page tables.
#[cfg(target_os = "linux")]
fn qemu_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::{UnixListener, UnixStream};
    use sv_hostd::locate;
    use sv_transport::cold::{Attachment, ColdMessage, recv, send_with_fd};
    use sv_transport::region::PAGE;
    use sv_transport::stream::{read_message, write_message};

    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let qmp = arg("--qmp").ok_or("--qmp is required")?;
    let cold = arg("--cold").ok_or("--cold is required")?;
    let path = arg("--socket").ok_or("--socket is required")?;
    let hint = arg("--vm").unwrap_or_else(|| "sv-guest".into());
    let clients: usize = arg("--clients").map_or(Ok(1), |s| s.parse())?;

    // The cold path. QEMU is listening on this socket as the host end of the
    // guest's virtio-serial port, so connecting to it puts us on the other end
    // of `/dev/virtio-ports/<name>` inside the guest.
    eprintln!("sv-hostd: waiting for the guest on the cold path {cold}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut port = loop {
        match UnixStream::connect(&cold) {
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
        &hint,
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
        let _ = write_message(&mut port, &ColdMessage::Refuse { code: 3, reason: reason.clone() });
        return Err(reason.into());
    }
    eprintln!(
        "sv-hostd: stitched {} MiB with {} mmap call(s); all {pages} pages carry their own index",
        view.len() >> 20,
        view.runs(),
    );

    // Accepting is the guest's signal that its memory is reachable, so it is
    // sent only after the check above has passed.
    write_message(&mut port, &ColdMessage::Accept { generation: hello.generation })?;

    // Serve the ICD. What it gets is a descriptor for the *stitched* view, not
    // for QEMU's backing object: the extent is scattered through that object and
    // an application needs one flat region whose offsets match the guest's.
    //
    // The descriptor is a memfd this daemon owns whose pages are the guest's,
    // because `attach` mapped the runs `MAP_SHARED` into it — so nothing is
    // copied and the ICD's `vkMapMemory` returns a pointer into the VM's RAM.
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

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    eprintln!("sv-hostd: listening on {path} for {clients} ICD client(s)");
    for i in 0..clients {
        let (sock, _) = listener.accept()?;
        let (msg, _) = recv(&sock)?;
        match msg {
            ColdMessage::AttachRequest => {
                let mut buf = Vec::new();
                ColdMessage::Attach(attachment.clone()).encode(&mut buf)?;
                // QEMU's own descriptor, not a copy of anything: the ICD maps
                // the guest's RAM directly. What it needs on top is the run
                // list, because a scattered extent has no single descriptor
                // whose `mmap` reproduces it — design.md §3's "hands **fd +
                // runs** to the ICDs", which is a different boundary from
                // §5.7's "the run list does not cross the wire".
                send_with_fd(&sock, &buf, Some(hostd.fd.as_fd()))?;
                if attachment.run_count != 0 {
                    let mut buf = Vec::new();
                    ColdMessage::Runs(runs_for_wire.clone()).encode(&mut buf)?;
                    send_with_fd(&sock, &buf, None)?;
                }
                eprintln!(
                    "sv-hostd: ICD client {i} attached ({} run(s))",
                    hostd.runs.len()
                );
            }
            other => {
                let mut buf = Vec::new();
                ColdMessage::Refuse { code: 1, reason: format!("expected AttachRequest, got {other:?}") }
                    .encode(&mut buf)?;
                send_with_fd(&sock, &buf, None)?;
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

