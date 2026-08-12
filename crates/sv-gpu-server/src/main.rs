//! ```text
//! sv-gpu-server serve   --socket <path> [--device N] [--arena BYTES] [--clients N]
//! sv-gpu-server attach  --socket <path> [--device N]
//! ```
//!
//! # Two subcommands, because there are two arena providers
//!
//! `serve` is the GPU-allocated path: this process opens a driver, allocates the
//! arena from it, exports a descriptor, and *is* the cold-path server — app-side
//! processes connect to it and are handed the two descriptors. Measured working
//! on RADV and NVIDIA.
//!
//! `attach` is the app-allocated path: `sv-hostd` owns a memfd, this process
//! connects as a client and imports the mapping it is given with
//! `VK_EXT_external_memory_host`. This is the topology `testhost` needs — the
//! GPU side is inside a VM and the arena is guest RAM the host located — and it
//! is the one lavapipe can serve.
//!
//! In both cases the GPU side is the **owner** of the control region in §4.3's
//! sense: it publishes the header and writes `magic` last. That is unchanged
//! between the two, which is the point — everything above the provider is the
//! same code.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!(
        "sv-gpu-server: the descriptor handoff is Linux-only. A Windows GPU side \
         keeps the ring consumers and replaces the cold path with a named pipe."
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sv-gpu-server: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use sv_gpu_server::arena;
    use sv_gpu_server::driver::Driver;

    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| -> Option<String> {
        args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned()
    };
    let mode = args.get(1).cloned().unwrap_or_default();
    let socket = arg("--socket").ok_or("--socket is required")?;
    let device: usize = arg("--device").map_or(Ok(0), |s| s.parse())?;

    let driver = Driver::open(device)?;
    let key = driver.driver_key();
    let chosen = arena::Provider::for_driver(key);
    eprintln!(
        "[gpu] {} ({key}), queue family {}, provider {chosen:?}",
        driver.name, driver.queue_family
    );

    match mode.as_str() {
        "serve" => {
            let arena_size: u64 = arg("--arena").map_or(Ok(64 << 20), |s| s.parse())?;
            let clients: usize = arg("--clients").map_or(Ok(1), |s| s.parse())?;
            serve(driver, &socket, arena_size, clients)
        }
        "attach" => attach(driver, &socket),
        _ => {
            eprintln!(
                "usage: sv-gpu-server <serve|attach> --socket <path> \
                 [--device N] [--arena BYTES] [--clients N]"
            );
            Err("no subcommand".into())
        }
    }
}

/// The GPU-allocated path: allocate, export, hand out, serve.
#[cfg(target_os = "linux")]
fn serve(
    driver: sv_gpu_server::driver::Driver,
    socket: &str,
    arena_size: u64,
    clients: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;
    use std::sync::Arc;
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{Attachment, ColdMessage, recv, send_with_fds};
    use sv_transport::control::Control;
    use sv_transport::region::{SharedRegion, memfd};

    let geometry = geometry_for(arena_size);
    let arena_size = geometry.arena_size;

    // The arena comes from the driver. The control region does not: §4.1 splits
    // the two by purpose and the control region is "the one only our code
    // touches", so it stays an ordinary memfd. That also keeps the ring atomics
    // on ordinary write-back RAM rather than on whatever a GPU's host-visible
    // heap turns out to be, which is a question nobody has measured and which
    // this design does not need to ask.
    let control_fd = memfd("sv-control", geometry.control_size)?;
    let control_region =
        Arc::new(SharedRegion::map_fd(control_fd.as_fd(), 0, geometry.control_size as usize)?);

    let arena = arena::allocate_and_export(&driver, arena_size)?;
    eprintln!(
        "[gpu] arena {} MiB from the driver, memory type {}, exported as a descriptor",
        arena.len >> 20,
        arena.memory_type
    );
    let arena_fd = arena
        .exported_fd
        .as_ref()
        .ok_or("the export path produced no descriptor")?
        .as_fd();

    let generation = 1u32;
    let control = Control::create(
        Arc::clone(&control_region),
        geometry,
        sv_gpu_server::protocol_ids(),
        generation,
    )?;
    control.stamp_epoch()?;

    let attachment = Attachment {
        control_offset: 0,
        control_len: geometry.control_size,
        // The arena is its own object here, so it begins at offset 0 of the
        // *second* descriptor. On the memfd path both regions are one file and
        // the arena begins after the control region; the client is told which by
        // how many descriptors arrive.
        arena_offset: 0,
        arena_len: arena_size,
        generation,
        run_count: 0,
    };

    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    eprintln!("[gpu] listening on {socket} for {clients} client(s)");

    for i in 0..clients {
        let (sock, _) = listener.accept()?;
        let (msg, _) = recv(&sock)?;
        match msg {
            ColdMessage::AttachRequest => {
                let mut buf = Vec::new();
                ColdMessage::Attach(attachment.clone()).encode(&mut buf)?;
                send_with_fds(&sock, &buf, &[control_fd.as_fd(), arena_fd])?;
                eprintln!("[gpu] client {i} attached (control + arena descriptors)");
            }
            other => {
                let mut buf = Vec::new();
                ColdMessage::Refuse {
                    code: 1,
                    reason: format!("expected AttachRequest, got {other:?}"),
                }
                .encode(&mut buf)?;
                send_with_fds(&sock, &buf, &[])?;
            }
        }
    }

    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    let fns = driver.load_fns();
    report_missing(&fns);
    let shared = Arc::new(Shared {
        driver,
        arena,
        objects: std::sync::Mutex::new(objects),
        fns,
    });
    let handled = session::serve(
        Arc::new(control),
        shared,
        geometry.ring_slot_count,
        std::time::Duration::from_secs(30),
    )?;
    eprintln!("[gpu] {handled} commands dispatched");
    let _ = std::fs::remove_file(socket);
    Ok(())
}

/// The app-allocated path: attach to `sv-hostd`'s memfd and import it.
#[cfg(target_os = "linux")]
fn attach(
    driver: sv_gpu_server::driver::Driver,
    socket: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use sv_gpu_server::{arena, server::Shared, session, table::Objects};
    use sv_transport::cold::{ColdMessage, recv, send};
    use sv_transport::control::Control;
    use sv_transport::region::SharedRegion;

    // The daemon may not be listening yet; bounded, because R4 applies to
    // startup too.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let sock = loop {
        match UnixStream::connect(socket) {
            Ok(s) => break s,
            Err(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(e) => return Err(Box::new(e)),
        }
    };
    send(&sock, &ColdMessage::AttachRequest)?;
    let (msg, fd) = recv(&sock)?;
    let at = match msg {
        ColdMessage::Attach(a) => a,
        ColdMessage::Refuse { code, reason } => return Err(format!("refused {code}: {reason}").into()),
        other => return Err(format!("unexpected {other:?}").into()),
    };
    let fd = fd.ok_or("sv-hostd sent no descriptor")?;

    let control_region =
        Arc::new(SharedRegion::map_fd(fd.as_fd(), at.control_offset, at.control_len as usize)?);
    let arena_region =
        Arc::new(SharedRegion::map_fd(fd.as_fd(), at.arena_offset, at.arena_len as usize)?);

    let geometry = geometry_for(at.arena_len);
    let control = Control::create(
        Arc::clone(&control_region),
        sv_transport::control::Geometry { control_size: at.control_len, ..geometry },
        sv_gpu_server::protocol_ids(),
        at.generation,
    )?;
    control.stamp_epoch()?;

    let arena = arena::import_whole(&driver, arena_region)?;
    eprintln!(
        "[gpu] arena {} MiB imported as a host pointer, memory type {}",
        arena.len >> 20,
        arena.memory_type
    );

    let mut objects = Objects::new();
    objects.set_arena(arena.memory, arena.len);
    let fns = driver.load_fns();
    report_missing(&fns);
    let shared = Arc::new(Shared {
        driver,
        arena,
        objects: std::sync::Mutex::new(objects),
        fns,
    });
    let handled = session::serve(
        Arc::new(control),
        shared,
        geometry.ring_slot_count,
        std::time::Duration::from_secs(30),
    )?;
    eprintln!("[gpu] {handled} commands dispatched");
    Ok(())
}

/// Say once, at startup, which of the profile's entry points this driver lacks.
///
/// Reported here rather than discovered per command: "this driver has no
/// `vkCmdBeginRendering`" is a fact about the configuration, and an application
/// should be told before it has drawn nothing for ten seconds. An empty list is
/// the normal case on a modern driver and prints nothing.
#[cfg(target_os = "linux")]
fn report_missing(fns: &sv_gpu_entry::DriverFns) {
    let missing = fns.missing();
    if missing.is_empty() {
        return;
    }
    eprintln!(
        "[gpu] the driver is missing {} of the profile's entry points, starting with {:?}",
        missing.len(),
        &missing[..missing.len().min(8)]
    );
}

/// The geometry both sides use — [`sv_transport::control::Geometry::for_arena`],
/// which is where it lives so that `sv-hostd` and this binary cannot disagree.
#[cfg(target_os = "linux")]
fn geometry_for(arena_size: u64) -> sv_transport::control::Geometry {
    sv_transport::control::Geometry::for_arena(arena_size)
}
